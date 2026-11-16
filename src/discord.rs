use serenity::async_trait;
use serenity::prelude::*;
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, MessageId, RoleId, GuildId, UserId};
use serenity::model::channel::{Channel, ChannelType, PermissionOverwrite, PermissionOverwriteType};
use serenity::model::permissions::Permissions;
use serenity::builder::{CreateMessage, EditMessage, CreateEmbed, CreateEmbedFooter, CreateEmbedAuthor, CreateAttachment};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use serde_json::Value;

use crate::r2::R2Client;
use crate::telemetry;

struct Handler {
    is_ready: Arc<AtomicBool>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _: Context, ready: Ready) {
        tracing::info!("Discord bot {} is connected and ready", ready.user.name);
        self.is_ready.store(true, Ordering::Relaxed);
    }
}

pub async fn start_discord_bot(token: &str) -> Result<Arc<serenity::http::Http>, Box<dyn std::error::Error>> {
    let is_ready = Arc::new(AtomicBool::new(false));
    let handler = Handler { is_ready: is_ready.clone() };

    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES;
    let mut client = Client::builder(token, intents)
        .event_handler(handler)
        .await?;

    let http_client = client.http.clone();

    // Spawn the client in a background task
    tokio::spawn(async move {
        if let Err(why) = client.start().await {
            tracing::error!("Discord bot client error: {:?}", why);
        }
    });

    // Wait for the bot to be ready, but never hang startup forever (e.g. on a
    // revoked token) — Cloud Run would kill the instance without a useful log.
    tracing::info!("Waiting for Discord bot to be ready...");
    let wait = async {
        while !is_ready.load(Ordering::Relaxed) {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
    };
    tokio::time::timeout(tokio::time::Duration::from_secs(30), wait)
        .await
        .map_err(|_| "Discord bot did not become ready within 30s (bad token or gateway issue?)")?;
    tracing::info!("Discord bot is ready!");

    Ok(http_client)
}

/// Discord's JSON error code for a channel that doesn't exist: deleted, or never there.
const UNKNOWN_CHANNEL: isize = 10003;

fn is_unknown_channel(e: &serenity::Error) -> bool {
    matches!(
        e,
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(r)) if r.error.code == UNKNOWN_CHANNEL
    )
}

/// Drops a channel Discord says no longer exists, as an admin removing it would
/// (`handlers.rs`), and the record of the posts made there: otherwise every flight of
/// every member tries it again, and the hourly refresh puts it back on their lists.
async fn forget_channel(db: &PgPool, channel: &str) {
    let forgotten = async {
        let mut tx = db.begin().await?;
        sqlx::query("DELETE FROM discord_notification_channels WHERE channel_id = $1")
            .bind(channel)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM flight_discord_messages WHERE discord_channel_id = $1")
            .bind(channel)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM discord_channel_optouts WHERE channel_id = $1")
            .bind(channel)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM allowlisted_channels WHERE channel_id = $1")
            .bind(channel)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    };
    match forgotten.await {
        Ok(()) => tracing::warn!("Discord channel {channel} no longer exists; removed it"),
        Err(e) => tracing::error!("Failed to remove deleted Discord channel {channel}: {e}"),
    }
}

/// What the bot needs in a channel it posts flights to: to see it, to write in it, to
/// embed a flight's card, and to attach its screenshots. The invite link asks for exactly
/// these, and a channel is only allowlisted once the bot holds them there.
pub const POSTING_PERMISSIONS: Permissions = Permissions::VIEW_CHANNEL
    .union(Permissions::SEND_MESSAGES)
    .union(Permissions::EMBED_LINKS)
    .union(Permissions::ATTACH_FILES);

/// The link that adds the bot to a server, with the permissions it posts with. Only a
/// server's owner or someone with Manage Server can complete it, and only while the
/// application is a Public Bot in Discord's developer portal.
pub fn invite_url(client_id: &str) -> String {
    let permissions = POSTING_PERMISSIONS.bits().to_string();
    reqwest::Url::parse_with_params(
        "https://discord.com/oauth2/authorize",
        &[("client_id", client_id), ("scope", "bot"), ("permissions", permissions.as_str())],
    )
    .expect("the invite URL is well formed")
    .to_string()
}

/// Whether flights can be posted in a channel of this kind: a text channel, or an
/// announcement channel (whose posts members may also follow from other servers).
pub fn is_postable(kind: ChannelType) -> bool {
    matches!(kind, ChannelType::Text | ChannelType::News)
}

/// Whether the bot can post flights in a channel: it must exist, be a text or
/// announcement channel of a server the bot is in, and grant the bot `POSTING_PERMISSIONS`.
/// The error says what is wrong, for the admin who ticked the channel.
pub async fn validate_discord_channel(http: &serenity::http::Http, channel_id: u64) -> Result<(), String> {
    let chan_id = ChannelId::new(channel_id);
    let channel = chan_id.to_channel(http).await
        .map_err(|e| format!("Channel not found on Discord: {}", e))?;

    let guild_channel = match channel {
        Channel::Guild(c) => c,
        _ => return Err("Not a server channel".to_string()),
    };

    if !is_postable(guild_channel.kind) {
        return Err("Flights can only be posted in a text or announcement channel".to_string());
    }

    // Fetch the bot's own user info to get its user ID
    let bot_user = http.get_current_user().await
        .map_err(|e| format!("Failed to fetch current bot user: {}", e))?;

    // Fetch the bot member in the guild
    let member = http.get_member(guild_channel.guild_id, bot_user.id).await
        .map_err(|e| format!("Failed to fetch bot member in guild: {}", e))?;

    // Fetch guild roles to get permissions
    let roles = http.get_guild_roles(guild_channel.guild_id).await
        .map_err(|e| format!("Failed to fetch guild roles: {}", e))?;
    let roles: HashMap<RoleId, Permissions> = roles.into_iter().map(|r| (r.id, r.permissions)).collect();

    let permissions = channel_permissions(
        guild_channel.guild_id,
        bot_user.id,
        &member.roles,
        &roles,
        &guild_channel.permission_overwrites,
    );
    missing_for_posting(permissions)
}

/// Ok when `permissions` cover `POSTING_PERMISSIONS`, else which are missing, by name.
fn missing_for_posting(permissions: Permissions) -> Result<(), String> {
    let missing = POSTING_PERMISSIONS - permissions;
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "ButterLog can't post in this channel: it lacks {} there. Grant them to its role, \
         or to it in the channel's permissions, and tick the channel again.",
        missing.get_permission_names().join(", "),
    ))
}

/// A member's permissions across a server: what @everyone grants (the role whose id is
/// the server's), and each of their roles. Channel overwrites are `channel_permissions`'.
fn guild_permissions(guild_id: GuildId, member_roles: &[RoleId], roles: &HashMap<RoleId, Permissions>) -> Permissions {
    let everyone = roles.get(&RoleId::new(guild_id.get())).copied().unwrap_or_else(Permissions::empty);
    member_roles.iter().filter_map(|r| roles.get(r)).fold(everyone, |all, p| all | *p)
}

/// A member's permissions in one channel, in Discord's order: the server's, then the
/// channel's overwrite for @everyone, then its overwrites for the member's roles taken
/// together, then its overwrite for the member. Administrator overrides every overwrite.
fn channel_permissions(
    guild_id: GuildId,
    user: UserId,
    member_roles: &[RoleId],
    roles: &HashMap<RoleId, Permissions>,
    overwrites: &[PermissionOverwrite],
) -> Permissions {
    let mut permissions = guild_permissions(guild_id, member_roles, roles);
    if permissions.administrator() {
        return Permissions::all();
    }

    let everyone = RoleId::new(guild_id.get());
    let for_role = |role: RoleId| {
        overwrites.iter().find(|o| matches!(o.kind, PermissionOverwriteType::Role(id) if id == role))
    };

    if let Some(o) = for_role(everyone) {
        permissions = (permissions & !o.deny) | o.allow;
    }

    let (mut allow, mut deny) = (Permissions::empty(), Permissions::empty());
    for o in member_roles.iter().filter_map(|r| for_role(*r)) {
        allow |= o.allow;
        deny |= o.deny;
    }
    permissions = (permissions & !deny) | allow;

    if let Some(o) = overwrites.iter().find(|o| matches!(o.kind, PermissionOverwriteType::Member(id) if id == user)) {
        permissions = (permissions & !o.deny) | o.allow;
    }

    permissions
}

/// Render a snapshot's fields for a category as Discord markdown lines
/// (`**Field**: value unit`). The field table and value formatting are shared
/// with the web flight-detail page — see `crate::telemetry`.
fn get_formatted_fields_for_category(snapshot: &Value, category: &str) -> String {
    telemetry::labeled_values(snapshot, category)
        .into_iter()
        .map(|(name, value)| format!("**{}**: {}", name, value))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The in-flight post's one block of numbers: the indicated altitude with the
/// altimeter setting it reads against, the speeds, the wind and the fuel used so far.
fn in_flight_lines(snapshot: &Value, fuel_consumed: Option<&Value>) -> String {
    let num = |v: &Value| v.as_f64().or_else(|| v.as_str()?.parse().ok());
    let mut lines = Vec::new();
    if let Some(alt) = snapshot.get("AltB").and_then(num) {
        let baro = snapshot.get("BaroA").and_then(num).filter(|b| *b > 0.0);
        lines.push(match baro {
            Some(b) => format!("**Altitude**: {alt:.0} ft ({b:.2} inHg)"),
            None => format!("**Altitude**: {alt:.0} ft"),
        });
    }
    for (name, value) in telemetry::labeled_values_for_keys(snapshot, &["IAS", "GndSpd", "VSpd", "OAT", "WndSpd", "WndDr"]) {
        lines.push(format!("**{name}**: {value}"));
    }
    if let Some(gal) = fuel_consumed.and_then(num).filter(|g| *g >= 0.0) {
        lines.push(format!("**Fuel Used**: {gal:.1} Gal"));
    }
    lines.join("\n")
}

fn format_timestamp_to_discord(iso_str: &str) -> String {
    // Timestamps arrive either as RFC3339 or as the app's UTC log format
    // ("%Y-%m-%d %H:%M:%S%.3f", no 'T' and no timezone). Try RFC3339 first, then
    // fall back to the log format, treating it as UTC.
    let dt_utc = chrono::DateTime::parse_from_rfc3339(iso_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(iso_str, "%Y-%m-%d %H:%M:%S%.f")
                .map(|naive| naive.and_utc())
        });

    if let Ok(dt) = dt_utc {
        let epoch = dt.timestamp();
        // Discord's <t:...> tags always render in each viewer's local timezone, so
        // the UTC/Zulu value is printed literally and the tag supplies local time.
        let zulu = dt.format("%H%MZ");
        format!("{} (<t:{}:F> local, <t:{}:R>)", zulu, epoch, epoch)
    } else {
        iso_str.to_string()
    }
}

pub struct DiscordUserInfo {
    pub discord_id: String,
    pub username: String,
    pub global_name: Option<String>,
    pub avatar: Option<String>,
}

pub async fn maybe_update_user_notification_channels(
    db: &PgPool,
    http: &serenity::http::Http,
    user_id: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Fetch user's discord_id and last update timestamp
    let row: Option<(String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT discord_id, discord_notification_channels_updated_at FROM users \
          WHERE id = $1 AND discord_id IS NOT NULL"
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;

    let (discord_id_str, last_updated) = match row {
        Some(r) => r,
        None => return Ok(()),
    };

    let user_discord_id = match discord_id_str.parse::<u64>() {
        Ok(id) => id,
        Err(_) => return Ok(()),
    };

    // Check if updated in the last hour
    let now = chrono::Utc::now();
    if let Some(last) = last_updated {
        if now.signed_duration_since(last).num_hours() < 1 {
            return Ok(());
        }
    }

    // 2. The servers with an allowlisted channel, and of them, those the pilot is in.
    let guild_ids: Vec<String> = sqlx::query_scalar("SELECT DISTINCT guild_id FROM allowlisted_channels")
        .fetch_all(db)
        .await?;
    let mut member_of = Vec::new();
    for guild_id_str in guild_ids {
        if let Ok(guild_id_u64) = guild_id_str.parse::<u64>() {
            let g_id = serenity::model::id::GuildId::new(guild_id_u64);
            let u_id = serenity::model::id::UserId::new(user_discord_id);
            if http.get_member(g_id, u_id).await.is_ok() {
                member_of.push(guild_id_str);
            }
        }
    }

    store_notification_channels(db, user_id, &member_of, now).await?;
    Ok(())
}

/// Point a pilot's notifications at every allowlisted channel of the servers they are in
/// (`member_of`, Discord guild ids), and at no other, and note when, so the next hour's
/// syncs don't ask Discord again.
pub async fn store_notification_channels(
    db: &PgPool,
    user_id: i64,
    member_of: &[String],
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    sqlx::query(
        "DELETE FROM discord_notification_channels n WHERE n.user_id = $1 AND NOT EXISTS \
           (SELECT 1 FROM allowlisted_channels a WHERE a.channel_id = n.channel_id AND a.guild_id = ANY($2))",
    )
    .bind(user_id)
    .bind(member_of)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO discord_notification_channels (user_id, channel_id) \
         SELECT $1, channel_id FROM allowlisted_channels WHERE guild_id = ANY($2) \
         ON CONFLICT (user_id, channel_id) DO NOTHING",
    )
    .bind(user_id)
    .bind(member_of)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE users SET discord_notification_channels_updated_at = $1 WHERE id = $2")
        .bind(now)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// Allowlist a channel, or rename it, and have every pilot's channels looked up again on
/// their next flight or settings visit, so the members of its server are posted there
/// from then on rather than after their hour-long cache runs out.
pub async fn allowlist_channel(db: &PgPool, channel_id: &str, channel_name: &str, guild_id: &str) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    let added = sqlx::query(
        "INSERT INTO allowlisted_channels (channel_id, channel_name, guild_id) VALUES ($1, $2, $3) \
         ON CONFLICT (channel_id) DO UPDATE SET channel_name = EXCLUDED.channel_name \
         RETURNING (xmax = 0)",
    )
    .bind(channel_id)
    .bind(channel_name)
    .bind(guild_id)
    .fetch_one(&mut *tx)
    .await?;
    let added: bool = sqlx::Row::get(&added, 0);
    if added {
        sqlx::query(
            "UPDATE users SET discord_notification_channels_updated_at = NULL \
             WHERE discord_notification_channels_updated_at IS NOT NULL",
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// The channels a pilot's flights are posted in: the chosen channels of their servers,
/// less those they opted out of.
pub async fn posted_channels(db: &PgPool, user_id: i64) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT n.channel_id FROM discord_notification_channels n \
         WHERE n.user_id = $1 AND NOT EXISTS \
           (SELECT 1 FROM discord_channel_optouts o WHERE o.user_id = n.user_id AND o.channel_id = n.channel_id) \
         ORDER BY n.id",
    )
    .bind(user_id)
    .fetch_all(db)
    .await
}

/// Have a pilot's flights posted in one of their servers' chosen channels again, or no
/// longer. False when the channel is not one of theirs, and nothing changes.
pub async fn set_channel_posting(db: &PgPool, user_id: i64, channel_id: &str, posted: bool) -> Result<bool, sqlx::Error> {
    let theirs: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM discord_notification_channels WHERE user_id = $1 AND channel_id = $2)",
    )
    .bind(user_id)
    .bind(channel_id)
    .fetch_one(db)
    .await?;
    if !theirs {
        return Ok(false);
    }
    let query = if posted {
        "DELETE FROM discord_channel_optouts WHERE user_id = $1 AND channel_id = $2"
    } else {
        "INSERT INTO discord_channel_optouts (user_id, channel_id) VALUES ($1, $2) ON CONFLICT DO NOTHING"
    };
    sqlx::query(query).bind(user_id).bind(channel_id).execute(db).await?;
    Ok(true)
}

/// All the flight + user state needed to render and throttle a Discord sync.
#[derive(sqlx::FromRow)]
struct FlightSyncRow {
    user_id: i64,
    statistics: Value,
    discord_id: String,
    username: String,
    global_name: Option<String>,
    avatar: Option<String>,
    discord_screenshot_sig: Option<String>,
    notes: Option<String>,
}

/// Whether the flight's departure is `Airborne`: it began in the air.
fn begun_airborne(statistics: &Value) -> bool {
    statistics["departure"]["icao"].as_str() == Some("Airborne")
}

pub async fn sync_flight_discord(
    db: &PgPool,
    r2: &R2Client,
    http: &serenity::http::Http,
    flight_id: i64,
    base_url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Fetch flight info, user info, and the Discord sync state used for throttling.
    let row: Option<FlightSyncRow> = sqlx::query_as(
        "SELECT f.user_id, f.statistics, u.discord_id, u.username, u.global_name, u.avatar, f.discord_screenshot_sig, f.notes \
         FROM flights f \
         JOIN users u ON f.user_id = u.id \
         WHERE f.id = $1 AND u.discord_id IS NOT NULL"
    )
    .bind(flight_id)
    .fetch_optional(db)
    .await?;

    let row = match row {
        Some(r) => r,
        None => return Ok(()),
    };
    let user_id = row.user_id;
    let statistics = row.statistics;
    // A flight begun in the air (a sim loaded mid-flight, a session resumed without the
    // stream before it) has no departure to tell, and is not posted.
    if begun_airborne(&statistics) {
        return Ok(());
    }
    let stored_sig = row.discord_screenshot_sig;
    let notes = row.notes;

    // Update user notification channels before notifying
    if let Err(e) = maybe_update_user_notification_channels(db, http, user_id).await {
        tracing::error!("Failed to update user notification channels: {}", e);
    }

    let user_info = DiscordUserInfo {
        discord_id: row.discord_id,
        username: row.username,
        global_name: row.global_name,
        avatar: row.avatar,
    };

    // Expire stale 'pending' claims left behind by a crashed sync so the
    // channel does not get stuck without a message forever.
    sqlx::query(
        "DELETE FROM flight_discord_messages \
         WHERE flight_id = $1 AND discord_message_id = 'pending' \
           AND created_at < NOW() - INTERVAL '5 minutes'"
    )
    .bind(flight_id)
    .execute(db)
    .await?;

    // 2. The channels this pilot's flights go to. A channel they opted out of gets no new
    // post, and a post already made there is left as it was.
    let channels = posted_channels(db, user_id).await?;

    if channels.is_empty() {
        return Ok(());
    }

    // 3. Fetch screenshots (cheap, no R2 yet) and compute a signature of the ordered set.
    let screenshot_rows: Vec<(String, String, Option<uuid::Uuid>)> = sqlx::query_as(
        "SELECT hash, url, session_id FROM screenshots WHERE flight_id = $1 ORDER BY id LIMIT 9"
    )
    .bind(flight_id)
    .fetch_all(db)
    .await?;
    // A screenshot uploaded against a session is marked, so the signature of a flight whose
    // session screenshots were recorded before their key was found here differs, and they
    // are downloaded again.
    let screenshot_sig = screenshot_rows
        .iter()
        .map(|(h, _, session)| if session.is_some() { format!("s:{h}") } else { h.clone() })
        .collect::<Vec<_>>()
        .join(",");
    let screenshot_count = screenshot_rows.len();

    // 4. Existing Discord messages for this flight (one query instead of one per channel).
    let existing_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT discord_channel_id, discord_message_id FROM flight_discord_messages WHERE flight_id = $1"
    )
    .bind(flight_id)
    .fetch_all(db)
    .await?;
    let existing_msgs: std::collections::HashMap<String, String> = existing_rows.into_iter().collect();

    // 5. Decide whether to sync now and whether screenshots must be (re)downloaded.
    let is_landed = statistics.get("landing_time").and_then(|t| t.as_str()).is_some();
    let screenshots_changed = stored_sig.as_deref() != Some(screenshot_sig.as_str());
    let need_send = channels.iter().any(|c| !existing_msgs.contains_key(c));
    let download_needed = screenshots_changed || need_send;
    // Push first posts, new channels, landings, and screenshot changes immediately; otherwise
    // throttle telemetry-only updates to at most once per minute to spare R2 + Discord.
    // The throttle check-and-claim is a single conditional UPDATE so concurrent
    // syncs (other tasks or other instances) cannot both pass the window.
    let force = download_needed || is_landed;
    let claimed: Option<i64> = sqlx::query_scalar(
        "UPDATE flights SET discord_last_synced_at = NOW() \
         WHERE id = $1 AND ($2 OR discord_last_synced_at IS NULL \
               OR discord_last_synced_at < NOW() - INTERVAL '60 seconds') \
         RETURNING id"
    )
    .bind(flight_id)
    .bind(force)
    .fetch_optional(db)
    .await?;
    if claimed.is_none() {
        tracing::debug!("Skipping Discord sync for flight {} (throttled, <60s since last)", flight_id);
        return Ok(());
    }

    // 6-7. Assemble the primary embed. Every embed in the message must share one
    // url so Discord merges the screenshot images into the primary embed's
    // gallery instead of rendering them as a separate trailing block: the
    // flight's page, which shows all of it.
    let gallery_url = format!("{}/content/flights/{}", base_url, flight_id);
    // Where it is on FlySto, once processed there (`flysto.rs`).
    let flysto = crate::flysto::pointer(db, flight_id).await?;
    let flysto_url = Some(flysto.url()).filter(|u| !u.is_empty());
    // Landed: the cruise and the debriefs, from the flight's page document.
    let landed = if is_landed { landed_extras(db, r2, flight_id).await } else { Landed::default() };
    let (embeds, _) = assemble_embeds(&statistics, &user_info, base_url, &gallery_url, notes.as_deref(), flysto_url, &landed)?;

    // 8. Build attachments + helper embeds. Only re-download from R2 when the screenshot set
    // changed or a brand-new message must be sent; otherwise reuse the already-attached files
    // by referencing them by their original filenames.
    let mut attachments = Vec::new();
    let mut helper_embeds = Vec::new();
    let mut all_downloaded = true;
    if download_needed {
        for (index, (hash, url, session)) in screenshot_rows.into_iter().enumerate() {
            let key = crate::handlers::screenshot_key(Some(flight_id), session, &hash, &url);
            match r2.download_object(&key).await {
                Ok(bytes) => {
                    let filename = format!("screenshot-{}.jpg", index);
                    attachments.push(CreateAttachment::bytes(bytes, &filename));
                    helper_embeds.push(
                        CreateEmbed::new()
                            .url(&gallery_url)
                            .image(format!("attachment://{}", filename)),
                    );
                }
                Err(e) => {
                    tracing::error!("Failed to download screenshot {} from R2: {:?}", hash, e);
                    all_downloaded = false;
                }
            }
        }
    } else {
        for index in 0..screenshot_count {
            let filename = format!("screenshot-{}.jpg", index);
            helper_embeds.push(
                CreateEmbed::new()
                    .url(&gallery_url)
                    .image(format!("attachment://{}", filename)),
            );
        }
    }

    // Combine primary and helper embeds
    let mut all_embeds = embeds;
    all_embeds.extend(helper_embeds);

    // 9. Synchronize message state for each channel.
    for channel_str in &channels {
        let channel_id = match channel_str.parse::<u64>() {
            Ok(c) => ChannelId::new(c),
            Err(_) => continue,
        };

        if let Some(msg_id_str) = existing_msgs.get(channel_str) {
            // A 'pending' placeholder (non-numeric) means another sync is
            // mid-send for this channel; leave it alone.
            let msg_id_val = match msg_id_str.parse::<u64>() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let msg_id = MessageId::new(msg_id_val);

            let mut builder = EditMessage::new().embeds(all_embeds.clone());
            // Only replace attachments when we actually re-downloaded; otherwise omit the field
            // so Discord keeps the existing files.
            if download_needed {
                let mut edit_attachments = serenity::builder::EditAttachments::new();
                for attachment in attachments.clone() {
                    edit_attachments = edit_attachments.add(attachment);
                }
                builder = builder.attachments(edit_attachments);
            }

            tracing::info!("[Outgoing Request] Discord EDIT message {} in channel {}", msg_id, channel_id);
            match channel_id.edit_message(http, msg_id, builder).await {
                Ok(_) => tracing::info!("[Outgoing Response] Edited message {} successfully", msg_id),
                Err(e) if is_unknown_channel(&e) => forget_channel(db, channel_str).await,
                Err(e) => tracing::error!("Failed to edit Discord message {} in channel {}: {:?}", msg_id, channel_id, e),
            }
        } else {
            // Claim the (flight, channel) slot with a placeholder row before
            // sending. The unique index makes this race-safe: if another sync
            // (or another instance) claimed it first, rows_affected is 0 and
            // we skip instead of posting a duplicate message.
            let claim = sqlx::query(
                "INSERT INTO flight_discord_messages (flight_id, discord_message_id, discord_channel_id) \
                 VALUES ($1, 'pending', $2) \
                 ON CONFLICT (flight_id, discord_channel_id) DO NOTHING"
            )
            .bind(flight_id)
            .bind(channel_str)
            .execute(db)
            .await;
            match claim {
                Ok(res) if res.rows_affected() == 1 => {}
                Ok(_) => continue, // someone else owns this channel slot
                Err(e) => {
                    tracing::error!("Failed to claim Discord message slot for flight {} channel {}: {:?}", flight_id, channel_str, e);
                    continue;
                }
            }

            let builder = CreateMessage::new()
                .embeds(all_embeds.clone())
                .files(attachments.clone());

            tracing::info!("[Outgoing Request] Discord SEND message in channel {}", channel_id);
            match channel_id.send_message(http, builder).await {
                Ok(msg) => {
                    tracing::info!("[Outgoing Response] Sent message {} successfully", msg.id);
                    let _ = sqlx::query(
                        "UPDATE flight_discord_messages SET discord_message_id = $1 \
                         WHERE flight_id = $2 AND discord_channel_id = $3"
                    )
                    .bind(msg.id.to_string())
                    .bind(flight_id)
                    .bind(channel_str)
                    .execute(db)
                    .await;
                }
                Err(e) if is_unknown_channel(&e) => forget_channel(db, channel_str).await,
                Err(e) => {
                    tracing::error!("Failed to send Discord message in channel {}: {:?}", channel_id, e);
                    // Release the claim so a later sync can retry.
                    let _ = sqlx::query(
                        "DELETE FROM flight_discord_messages \
                         WHERE flight_id = $1 AND discord_channel_id = $2 AND discord_message_id = 'pending'"
                    )
                    .bind(flight_id)
                    .bind(channel_str)
                    .execute(db)
                    .await;
                }
            }
        }
    }

    // 10. Record the screenshot signature for change detection (the throttle
    // timestamp was already claimed atomically above). Not when a screenshot failed to
    // download: the next sync tries again.
    if !all_downloaded {
        return Ok(());
    }
    let _ = sqlx::query(
        "UPDATE flights SET discord_screenshot_sig = $1 WHERE id = $2"
    )
    .bind(&screenshot_sig)
    .bind(flight_id)
    .execute(db)
    .await;

    Ok(())
}

/// What a landed post adds from the flight's track: the altitude it cruised at, and the
/// debrief of each takeoff and approach, as the page shows them.
#[derive(Default)]
pub struct Landed {
    pub cruise_ft: Option<f64>,
    pub debriefs: Vec<Value>,
}

async fn landed_extras(db: &PgPool, r2: &R2Client, flight_id: i64) -> Landed {
    match crate::flight_doc::document(db, r2, flight_id).await {
        Ok(Some(doc)) => {
            let simulator = doc.get("simulator").and_then(Value::as_str).unwrap_or_default();
            let pts = crate::analysis::pattern::points(&doc["transposedData"], simulator);
            Landed {
                cruise_ft: crate::analysis::cruise::cruise_altitude(&pts),
                debriefs: doc.get("debriefs").and_then(Value::as_array).cloned().unwrap_or_default(),
            }
        }
        Ok(None) => Landed::default(),
        Err(e) => {
            tracing::warn!(flight_id, "no cruise or debrief for the Discord post: {e:?}");
            Landed::default()
        }
    }
}

/// Debriefs a post shows; the rest are on the flight's page.
const MAX_POSTED_DEBRIEFS: usize = 3;
/// Discord's limit on a field's text.
const FIELD_MAX_CHARS: usize = 1024;

/// A whole number with thousands separators.
fn grouped(n: f64) -> String {
    let s = (n.round().abs() as i64).to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0.0 { format!("-{out}") } else { out }
}

/// A debrief as a post's field, `(name, text)`: what it was, its key figures, what to work
/// on and what went well, as the page's `debriefHead` (`_flight_render.html`) puts them.
fn debrief_field(db: &Value, at: Option<&str>) -> (String, String) {
    let num = |path: &str| db.pointer(path).and_then(Value::as_f64);
    let wind = |leg: &str| match (num(&format!("/legs/{leg}/headwindKt")), num(&format!("/legs/{leg}/crosswindKt"))) {
        (Some(h), Some(c)) => Some(format!(
            "{:.0} kt {} · {:.0} kt from the {}",
            h.abs(),
            if h >= 0.0 { "head" } else { "tail" },
            c.abs(),
            if c >= 0.0 { "right" } else { "left" }
        )),
        _ => None,
    };
    let fig = |label: &str, v: Option<String>| v.map(|v| format!("{label} **{v}**"));
    let (title, sub, figs, none) = if db.get("phase").and_then(Value::as_str) == Some("takeoff") {
        let rolling = db.pointer("/legs/roll/rolling").and_then(Value::as_bool).unwrap_or(false);
        let runway_matches = db.get("runwayMatches").and_then(Value::as_bool).unwrap_or(false);
        let sub = vec![
            Some(if rolling { "Rolling takeoff" } else { "From a standstill" }.to_string()),
            num("/legs/roll/usedFt").filter(|_| runway_matches).map(|f| format!("{} ft of runway", grouped(f))),
            num("/legs/climb/levelledOffFt").map(|f| format!("levelled at {} ft", grouped(f))),
        ];
        let figs = vec![
            fig("Rotated at", num("/legs/rotation/rotateKt").map(|v| format!("{v:.0} kt"))),
            fig("lifted off at", num("/legs/rotation/liftoffKt").map(|v| format!("{v:.0} kt"))),
            fig("pitch rate", num("/legs/rotation/pitchRateDps").map(|v| format!("{v:.1}°/s"))),
            fig("climb", num("/legs/climb/climbFpm").map(|v| format!("{} fpm", grouped(v)))),
            fig("wind on the roll", wind("roll")),
        ];
        ("Takeoff debrief", sub, figs, "Nothing to work on in this takeoff.")
    } else {
        let right = db.get("circuit").and_then(Value::as_str) == Some("right");
        let kind = match db.get("approach").and_then(Value::as_str) {
            Some("circuit") => format!("{} circuit", if right { "Right" } else { "Left" }),
            Some("base entry") => format!("Joined on {} base", if right { "right" } else { "left" }),
            _ => "Straight-in".to_string(),
        };
        let sub = vec![
            Some(kind),
            num("/legs/final/lengthNm").map(|v| format!("{v:.1} NM final")),
            num("/legs/final/startHeightFt").map(|v| format!("from {} ft", grouped(v))),
        ];
        let stable = num("/legs/final/stabilisedFromFt").map_or("not stable".to_string(), |v| format!("{} ft", grouped(v)));
        let figs = vec![
            Some(format!("Stabilised from **{stable}**")),
            fig("path", num("/legs/final/pathAngleDeg").map(|v| format!("{v:.1}°"))),
            fig("final speed", num("/legs/final/finalSpeedKt").map(|v| format!("{v:.0} kt"))),
            fig("max sink", num("/legs/final/maxSinkFpm").map(|v| format!("{} fpm", grouped(v.abs())))),
            fig("wind on final", wind("final")),
        ];
        ("Approach debrief", sub, figs, "Nothing to work on in this approach.")
    };

    let mut text = sub.into_iter().flatten().collect::<Vec<_>>().join(" · ");
    if let Some(at) = at {
        text.push_str(&format!(" · {at}Z"));
    }
    let figs: Vec<String> = figs.into_iter().flatten().collect();
    if !figs.is_empty() {
        text.push_str(&format!("\n{}", figs.join(", ")));
    }
    let findings: Vec<&Value> = db.get("findings").and_then(Value::as_array).map(|f| f.iter().collect()).unwrap_or_default();
    let severity = |f: &Value| f.get("severity").and_then(Value::as_str).unwrap_or_default().to_string();
    let line = |f: &&Value| {
        let mark = match severity(f).as_str() {
            "good" => "✅",
            "caution" => "⚠️",
            _ => "▫️",
        };
        format!("{mark} {}", f.get("text").and_then(Value::as_str).unwrap_or_default())
    };
    let (good, work): (Vec<&Value>, Vec<&Value>) = findings.into_iter().partition(|f| severity(f) == "good");
    text.push_str("\n**To work on**\n");
    if work.is_empty() {
        text.push_str(none);
    } else {
        text.push_str(&work.iter().map(line).collect::<Vec<_>>().join("\n"));
    }
    if !good.is_empty() {
        text.push_str("\n**Done well**\n");
        text.push_str(&good.iter().map(line).collect::<Vec<_>>().join("\n"));
    }
    (title.to_string(), truncate(&text, FIELD_MAX_CHARS))
}

/// At most `max` characters, cut at a line where it can be, with an ellipsis.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max - 2).collect();
    match cut.rfind('\n') {
        Some(i) if i > cut.len() / 2 => format!("{}\n…", &cut[..i]),
        _ => format!("{cut}…"),
    }
}

struct LocalField {
    name: String,
    value: String,
    inline: bool,
}

fn assemble_embeds(
    statistics: &Value,
    user_info: &DiscordUserInfo,
    base_url: &str,
    gallery_url: &str,
    notes: Option<&str>,
    flysto_url: Option<&str>,
    landed: &Landed,
) -> Result<(Vec<CreateEmbed>, Vec<String>), Box<dyn std::error::Error>> {
    let departure_icao = statistics.get("departure").and_then(|d| d.get("icao")).and_then(|v| v.as_str()).unwrap_or("Unknown");
    let departure_name = statistics.get("departure").and_then(|d| d.get("name")).and_then(|v| v.as_str()).unwrap_or("Unknown");

    let arrival_icao = statistics.get("arrival").and_then(|a| d_or_null(a, "icao")).and_then(|v| v.as_str());
    let arrival_name = statistics.get("arrival").and_then(|a| d_or_null(a, "name")).and_then(|v| v.as_str());

    let takeoff_time = statistics.get("takeoff_time").and_then(|t| t.as_str());
    let landing_time = statistics.get("landing_time").and_then(|t| t.as_str());

    let is_landed = landing_time.is_some();

    // 1. Embed Title
    let title = if let Some(arr_icao) = arrival_icao {
        format!("Flight {} ✈ {}", departure_icao, arr_icao)
    } else {
        format!("Flight {} ✈ In Progress", departure_icao)
    };

    // 2. Embed Description
    let mut description = String::new();
    if let Some(t_time) = takeoff_time {
        description.push_str(&format!("Departed **{}** at {}\n", departure_name, format_timestamp_to_discord(t_time)));
    }
    if let Some(l_time) = landing_time {
        let when = format_timestamp_to_discord(l_time);
        if arrival_icao == Some("Crashed") {
            // A crash site is no airport: near the one it was, if any.
            let near = statistics.pointer("/arrival/near/name").and_then(|v| v.as_str());
            match near {
                Some(name) => description.push_str(&format!("Crashed near **{}** at {}\n", name, when)),
                None => description.push_str(&format!("Crashed at {}\n", when)),
            }
        } else {
            let arr_name = arrival_name.unwrap_or("Destination");
            description.push_str(&format!("Landed in **{}** at {}\n", arr_name, when));
        }
    }

    // 3. Embed Color & Autopilot Check
    let current_snapshot = statistics.get("current_snapshot");
    let max_entries = statistics.get("max_entries");

    let color = if is_landed {
        0x00FFFF // Cyan
    } else {
        // Autopilot check
        let mut afcs_on = false;
        if let Some(curr) = current_snapshot {
            if let Some(val) = curr.get("AfcsOn") {
                if let Some(b) = val.as_bool() {
                    afcs_on = b;
                } else if let Some(n) = val.as_f64() {
                    afcs_on = n > 0.5;
                } else if let Some(s) = val.as_str() {
                    afcs_on = s.parse::<f64>().map(|n| n > 0.5).unwrap_or(false) || s.to_lowercase() == "true";
                }
            }
        }
        if afcs_on {
            0xFF00FF // Autopilot active -> Magenta
        } else {
            0x00FF00 // Autopilot inactive -> Green
        }
    };

    // 4. Author Details
    let author_avatar = if let Some(ref hash) = user_info.avatar {
        format!("https://cdn.discordapp.com/avatars/{}/{}.png", user_info.discord_id, hash)
    } else {
        "https://cdn.discordapp.com/embed/avatars/0.png".to_string()
    };
    let author_name = user_info.global_name.as_ref().unwrap_or(&user_info.username);
    let author = CreateEmbedAuthor::new(author_name)
        .icon_url(author_avatar);

    // 5. Flight Info Field (Airframe name, simulator)
    let airframe_name = statistics.get("airframe_name").and_then(|v| v.as_str()).unwrap_or("Unknown Aircraft");
    let mut flight_info_val = format!("Flying {}", airframe_name);

    // Livery (MSFS LIVERY NAME / X-Plane acf_livery_path); may be absent or empty.
    // X-Plane sends a path, so show only its final segment for readability.
    let livery = statistics.get("livery").and_then(|v| v.as_str()).unwrap_or("").trim();
    if !livery.is_empty() {
        let livery_display = livery
            .rsplit(|c| c == '/' || c == '\\')
            .find(|s| !s.is_empty())
            .unwrap_or(livery);
        flight_info_val.push_str(&format!(" — {}", livery_display));
    }

    // ICAO type resolved from the title/livery by the client's word index; may be absent.
    let resolved_icao = statistics.get("resolved_icao").and_then(|v| v.as_str()).unwrap_or("").trim();
    if !resolved_icao.is_empty() {
        flight_info_val.push_str(&format!(" ({})", resolved_icao));
    }

    // Operating airline identified from the title/livery; may be absent.
    let resolved_airline = statistics.get("resolved_airline").and_then(|v| v.as_str()).unwrap_or("").trim();
    if !resolved_airline.is_empty() {
        flight_info_val.push_str(&format!("\nOperated by {}", resolved_airline));
    }

    // Simulator brand and version details from root of statistics
    let simulator = statistics.get("simulator").and_then(|v| v.as_str())
        .or_else(|| max_entries.and_then(|m| m.get("Simulator")).and_then(|v| v.as_str()));
    let simulator_version = statistics.get("simulator_version").and_then(|v| v.as_str())
        .or_else(|| max_entries.and_then(|m| m.get("SimulatorVersion")).and_then(|v| v.as_str()));

    if let Some(sim) = simulator {
        if let Some(ver) = simulator_version {
            flight_info_val.push_str(&format!("\nSimulator: {} {}", sim, ver));
        } else {
            flight_info_val.push_str(&format!("\nSimulator: {}", sim));
        }
    }

    // The level flown, once landed (`analysis/cruise.rs`).
    if let Some(ft) = landed.cruise_ft.filter(|_| is_landed) {
        flight_info_val.push_str(&format!("\nCruised at {}", crate::analysis::cruise::format_cruise(ft)));
    }

    // Distance calculation if cruising (read directly from client payload)
    if !is_landed {
        if let Some(closest) = statistics.get("closest_airport") {
            if !closest.is_null() {
                let icao = closest.get("icao").and_then(|v| v.as_str());
                let name = closest.get("name").and_then(|v| v.as_str());
                let dist = closest.get("distance").and_then(|v| v.as_f64());
                if let (Some(code), Some(n_str), Some(d_val)) = (icao, name, dist) {
                    flight_info_val.push_str(&format!(
                        "\nCurrently {:.1} nautical miles from {} ({})",
                        d_val, n_str, code
                    ));
                }
            }
        }
    }

    let mut fields = vec![
        LocalField {
            name: "Flight Info".to_string(),
            value: flight_info_val,
            inline: false,
        }
    ];

    // 6. Dynamic Fields by Phase
    if !is_landed {
        if let Some(curr) = current_snapshot {
            let in_flight_stats = in_flight_lines(curr, statistics.get("fuel_consumed"));
            if !in_flight_stats.is_empty() {
                fields.push(LocalField {
                    name: "Currently In Flight".to_string(),
                    value: in_flight_stats,
                    inline: true,
                });
            }
        }
    } else {
        // Each takeoff's and approach's debrief, as on the page. The times say which is
        // which when there are several.
        let many = landed.debriefs.len() > 2;
        for db in landed.debriefs.iter().take(MAX_POSTED_DEBRIEFS) {
            let at = db.get("at").and_then(Value::as_str).filter(|_| many).and_then(|a| a.get(11..16));
            let (name, value) = debrief_field(db, at);
            fields.push(LocalField { name, value, inline: false });
        }
        if landed.debriefs.len() > MAX_POSTED_DEBRIEFS {
            fields.push(LocalField {
                name: "More debriefs".to_string(),
                value: format!(
                    "{} more on [the flight's page]({gallery_url}#debrief)",
                    landed.debriefs.len() - MAX_POSTED_DEBRIEFS
                ),
                inline: false,
            });
        }
    }

    // Pilot-provided notes (capped at 500 chars upstream, well under Discord's field limit).
    if let Some(text) = notes.map(str::trim).filter(|n| !n.is_empty()) {
        fields.push(LocalField {
            name: "Notes".to_string(),
            value: text.to_string(),
            inline: false,
        });
    }

    // The flight on the pilot's FlySto, once there.
    if let Some(url) = flysto_url {
        fields.push(LocalField {
            name: "FlySto".to_string(),
            value: format!("[Open this flight on FlySto]({url})"),
            inline: false,
        });
    }

    // Footer text is plain (no markdown/<t:> support), so the Zulu time is
    // printed literally; the embed's native timestamp below renders alongside it
    // in each viewer's local timezone.
    let now = chrono::Utc::now();
    // The site serves no apple-touch-icon - nothing links one - so the logo the
    // page itself uses is the icon that actually exists. Discord caches a fetch
    // failure, so a 404 here is a blank square until its cache turns over.
    let footer = CreateEmbedFooter::new(format!("ButterLog • Updated {}", now.format("%H%MZ")))
        .icon_url(format!("{}/images/icon.png", base_url));

    // The shared url is what lets Discord group the screenshot image embeds into
    // this embed's gallery; it doubles as the title link.
    let primary_embed = CreateEmbed::new()
        .title(title)
        .url(gallery_url)
        .thumbnail(format!("{}/images/icon.png", base_url))
        .color(color)
        .author(author)
        .description(description)
        .timestamp(serenity::model::Timestamp::now())
        .footer(footer);

    // Apply fields to primary embed
    let mut final_embed = primary_embed;
    for f in fields {
        final_embed = final_embed.field(f.name, f.value, f.inline);
    }

    Ok((vec![final_embed], Vec::new()))
}

/// The landed or in-flight post a statistics document makes, as `(label, text)` lines:
/// the title, each line of the description, and each line of each field, labelled by the
/// field and, for `**Name**: value` lines, the name. For comparing the service's document
/// with the app's (`analysis/compare.rs`, rollout phase 5): what differs between two posts,
/// without the parts that are the same for both (author, links, footer, notes).
pub fn post_lines(statistics: &Value) -> Vec<(String, String)> {
    let user = DiscordUserInfo { discord_id: "0".into(), username: "pilot".into(), global_name: None, avatar: None };
    let Ok((embeds, _)) = assemble_embeds(statistics, &user, "", "", None, None, &Landed::default()) else {
        return Vec::new();
    };
    let embed = serde_json::to_value(&embeds[0]).unwrap_or(Value::Null);
    let mut lines = vec![("Title".to_string(), embed["title"].as_str().unwrap_or_default().to_string())];
    for line in embed["description"].as_str().unwrap_or_default().lines() {
        let line = without_discord_times(line);
        let label = line.split_whitespace().next().unwrap_or("Description").to_string();
        lines.push((label, line));
    }
    for field in embed["fields"].as_array().into_iter().flatten() {
        let name = field["name"].as_str().unwrap_or_default();
        for (i, line) in field["value"].as_str().unwrap_or_default().lines().enumerate() {
            match line.strip_prefix("**").and_then(|l| l.split_once("**: ")) {
                Some((label, value)) => lines.push((format!("{name} · {label}"), value.to_string())),
                None => lines.push((format!("{name} · line {}", i + 1), line.to_string())),
            }
        }
    }
    lines
}

/// "Departed **X** at 0209Z (<t:…:F> local, <t:…:R>)" without the viewer-local parts.
fn without_discord_times(line: &str) -> String {
    match (line.find(" (<t:"), line.rfind(":R>)")) {
        (Some(start), Some(end)) if end > start => format!("{}{}", &line[..start], &line[end + 4..]),
        _ => line.to_string(),
    }
}

fn d_or_null<'a>(val: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(v) = val.get(key) {
        if v.is_null() {
            None
        } else {
            Some(v)
        }
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct BotGuildInfo {
    pub id: String,
    pub name: String,
    pub channels: Vec<(String, String)>, // (channel_id, channel_name)
    pub is_user_admin: bool,
}

/// Whether a user may choose a server's channels: its owner, or a member holding
/// Administrator through @everyone or a role.
pub async fn is_user_admin_in_guild(
    http: &serenity::http::Http,
    guild_id: u64,
    user_id: u64,
) -> bool {
    let g_id = GuildId::new(guild_id);
    let u_id = UserId::new(user_id);
    // The partial guild carries both the owner and the roles.
    let Ok(guild) = g_id.to_partial_guild(http).await else { return false };
    let member_roles = if guild.owner_id == u_id {
        Vec::new()
    } else {
        match http.get_member(g_id, u_id).await {
            Ok(member) => member.roles,
            Err(_) => return false,
        }
    };
    let roles: HashMap<RoleId, Permissions> = guild.roles.values().map(|r| (r.id, r.permissions)).collect();
    may_administer(g_id, guild.owner_id, u_id, &member_roles, &roles)
}

/// The rule `is_user_admin_in_guild` applies to what Discord told it.
fn may_administer(
    guild_id: GuildId,
    owner: UserId,
    user: UserId,
    member_roles: &[RoleId],
    roles: &HashMap<RoleId, Permissions>,
) -> bool {
    owner == user || guild_permissions(guild_id, member_roles, roles).administrator()
}

pub async fn get_bot_guilds_and_channels(
    http: &serenity::http::Http,
    user_discord_id: Option<u64>,
) -> Result<Vec<BotGuildInfo>, String> {
    let guilds = http.get_guilds(None, None).await
        .map_err(|e| format!("Failed to fetch bot guilds: {}", e))?;

    let mut list = Vec::new();
    for guild in guilds {
        let is_user_admin = if let Some(u_id) = user_discord_id {
            is_user_admin_in_guild(http, guild.id.get(), u_id).await
        } else {
            false
        };

        let mut channels_list = Vec::new();
        if let Ok(channels) = http.get_channels(guild.id).await {
            for channel in channels {
                if is_postable(channel.kind) {
                    channels_list.push((channel.id.to_string(), channel.name.clone()));
                }
            }
        }

        list.push(BotGuildInfo {
            id: guild.id.to_string(),
            name: guild.name.clone(),
            channels: channels_list,
            is_user_admin,
        });
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a real Postgres, given by `TEST_DATABASE_URL` (skipped without it).
    #[tokio::test]
    async fn a_deleted_channel_is_forgotten_and_no_other() {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return };
        let db = PgPool::connect(&url).await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        let user: i64 = sqlx::query_scalar("INSERT INTO users (discord_id, username) VALUES ($1, 'test') RETURNING id")
            .bind(uuid::Uuid::new_v4().to_string())
            .fetch_one(&db)
            .await
            .unwrap();
        let flight: i64 =
            sqlx::query_scalar("INSERT INTO flights (user_id, departure, arrival, statistics) VALUES ($1, 'KAST', 'KAST', '{}') RETURNING id")
                .bind(user)
                .fetch_one(&db)
                .await
                .unwrap();
        let (gone, kept) = (uuid::Uuid::new_v4().to_string(), uuid::Uuid::new_v4().to_string());
        for channel in [&gone, &kept] {
            sqlx::query("INSERT INTO allowlisted_channels (channel_id, channel_name, guild_id) VALUES ($1, 'flightlogs', '1')")
                .bind(channel)
                .execute(&db)
                .await
                .unwrap();
            sqlx::query("INSERT INTO discord_notification_channels (user_id, channel_id) VALUES ($1, $2)")
                .bind(user)
                .bind(channel)
                .execute(&db)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO flight_discord_messages (flight_id, discord_message_id, discord_channel_id) VALUES ($1, '1', $2)",
            )
            .bind(flight)
            .bind(channel)
            .execute(&db)
            .await
            .unwrap();
        }

        forget_channel(&db, &gone).await;

        for (channel, expected) in [(&gone, 0), (&kept, 1)] {
            let counts: (i64, i64, i64) = sqlx::query_as(
                "SELECT (SELECT COUNT(*) FROM allowlisted_channels WHERE channel_id = $1), \
                        (SELECT COUNT(*) FROM discord_notification_channels WHERE channel_id = $1), \
                        (SELECT COUNT(*) FROM flight_discord_messages WHERE discord_channel_id = $1)",
            )
            .bind(channel)
            .fetch_one(&db)
            .await
            .unwrap();
            assert_eq!(counts, (expected, expected, expected), "channel {channel}");
        }
    }

    #[test]
    fn format_timestamp_handles_rfc3339_and_log_format() {
        // RFC3339
        let out = format_timestamp_to_discord("2026-06-09T14:30:00Z");
        assert!(out.starts_with("1430Z"), "got: {}", out);
        assert!(out.contains("<t:"));

        // App log format (UTC, no timezone)
        let out = format_timestamp_to_discord("2026-06-09 14:30:00.123");
        assert!(out.starts_with("1430Z"), "got: {}", out);

        // Unparseable input is passed through untouched
        assert_eq!(format_timestamp_to_discord("not a date"), "not a date");
    }

    #[test]
    fn telemetry_formatting_by_category() {
        let snapshot = serde_json::json!({
            "IAS": 123.456,
            "VSpd": "-700.5",
            "AfcsOn": 1.0,
            "E1 RPM": 2400.0,
            "Unknown": 1.0
        });

        let normal = get_formatted_fields_for_category(&snapshot, "normal");
        assert!(normal.contains("**Indicated Airspeed**: 123.46 kts"));
        assert!(normal.contains("**Vertical Speed**: -700.50 fpm"));
        assert!(!normal.contains("RPM"));

        let instruments = get_formatted_fields_for_category(&snapshot, "instruments");
        assert!(instruments.contains("**Autopilot**: On"));

        let engine = get_formatted_fields_for_category(&snapshot, "engine");
        assert!(engine.contains("**Engine 1 RPM**: 2400.00 rpm"));
    }

    #[test]
    fn the_in_flight_block_is_trimmed() {
        let snapshot = serde_json::json!({
            "AltB": 5512.4, "BaroA": 29.92, "AltGPS": 5600.0, "TAS": 130.0, "IAS": 120.0,
            "NormAc": 1.0, "E1 RPM": 2400.0, "COM1": 121.5, "AfcsOn": 1.0,
        });
        let lines = in_flight_lines(&snapshot, Some(&serde_json::json!(4.25)));
        assert!(lines.contains("**Altitude**: 5512 ft (29.92 inHg)"));
        assert!(lines.contains("**Indicated Airspeed**: 120.00 kts"));
        assert!(lines.contains("**Fuel Used**: 4.2 Gal") || lines.contains("**Fuel Used**: 4.3 Gal"));
        for gone in ["GPS", "True Airspeed", "Normal Acceleration", "RPM", "COM1", "Autopilot"] {
            assert!(!lines.contains(gone), "{gone} in {lines}");
        }
        assert!(!in_flight_lines(&snapshot, Some(&serde_json::json!(-1.0))).contains("Fuel"));
    }

    #[test]
    fn a_landed_post_reads_as_lines() {
        let lines = post_lines(&serde_json::json!({
            "departure": { "icao": "PHOG", "name": "Kahului" },
            "arrival": { "icao": "PHOG", "name": "Kahului" },
            "takeoff_time": "2026-11-02 02:05:39.519",
            "landing_time": "2026-11-02 02:06:02.519",
            "airframe_name": "Cessna Skyhawk",
            "resolved_icao": "C172",
            "simulator": "X-Plane",
            "simulator_version": "12",
            "landing_snapshot": { "VSpd": -382.56 },
            "max_entries": { "IAS": 81.24 },
        }));
        let get = |label: &str| lines.iter().find(|(l, _)| l == label).map(|(_, v)| v.as_str());
        assert_eq!(get("Title"), Some("Flight PHOG ✈ PHOG"));
        assert_eq!(get("Departed"), Some("Departed **Kahului** at 0205Z"));
        assert_eq!(get("Flight Info · line 1"), Some("Flying Cessna Skyhawk (C172)"));
        // No more figures than the page has: the max and touchdown stats are gone.
        assert!(lines.iter().all(|(l, _)| !l.starts_with("Landing Stats") && !l.starts_with("Max Stats")), "{lines:?}");
    }

    #[test]
    fn a_crash_is_no_arrival_at_an_airport() {
        let lines = post_lines(&serde_json::json!({
            "departure": { "icao": "BGSF", "name": "Kangerlussuaq" },
            "arrival": { "icao": "Crashed", "name": "Crash site", "near": { "icao": "CYFB", "name": "Iqaluit Airport" } },
            "takeoff_time": "2026-11-12 22:52:58.948",
            "landing_time": "2026-11-13 02:09:50.051",
            "airframe_name": "ATR 72-600",
        }));
        let get = |label: &str| lines.iter().find(|(l, _)| l == label).map(|(_, v)| v.as_str());
        assert_eq!(get("Title"), Some("Flight BGSF ✈ Crashed"));
        assert!(lines.iter().any(|(_, v)| v.contains("Crashed near **Iqaluit Airport** at 0209Z")), "{lines:?}");
        assert!(lines.iter().all(|(_, v)| !v.contains("Landed in")), "{lines:?}");
    }

    #[test]
    fn a_landed_post_says_the_cruise_and_the_debriefs() {
        let statistics = serde_json::json!({
            "departure": { "icao": "KPAE", "name": "Paine" },
            "arrival": { "icao": "KSEA", "name": "Seattle" },
            "landing_time": "2026-11-10 10:00:00.000",
            "airframe_name": "Boeing 737-800",
        });
        let landed = Landed {
            cruise_ft: Some(11_480.0),
            debriefs: vec![
                serde_json::json!({ "phase": "takeoff", "at": "2026-11-10 09:20:00", "runwayMatches": true,
                    "legs": { "roll": { "rolling": false, "usedFt": 4210.0 }, "rotation": { "rotateKt": 148.0, "liftoffKt": 152.4 }, "climb": { "climbFpm": 2100.0 } },
                    "findings": [{ "severity": "good", "leg": "rotation", "text": "Rotated at a steady rate." }] }),
                serde_json::json!({ "phase": "approach", "approach": "straight-in", "at": "2026-11-10 09:58:00",
                    "legs": { "final": { "lengthNm": 6.2, "stabilisedFromFt": 1000.0, "pathAngleDeg": 3.04, "finalSpeedKt": 141.0, "maxSinkFpm": -980.0 } },
                    "findings": [{ "severity": "caution", "leg": "final", "text": "Sink rate over 1,000 fpm below 500 ft." }] }),
            ],
        };
        let user = DiscordUserInfo { discord_id: "0".into(), username: "pilot".into(), global_name: None, avatar: None };
        let (embeds, _) = assemble_embeds(&statistics, &user, "", "https://b/content/flights/1", None, None, &landed).unwrap();
        let embed = serde_json::to_value(&embeds[0]).unwrap();
        let fields = embed["fields"].as_array().unwrap();
        let field = |name: &str| fields.iter().find(|f| f["name"] == name).and_then(|f| f["value"].as_str()).unwrap_or_default().to_string();
        assert!(field("Flight Info").contains("Cruised at 11,500 ft"), "{}", field("Flight Info"));
        let takeoff = field("Takeoff debrief");
        assert!(takeoff.starts_with("From a standstill · 4,210 ft of runway"), "{takeoff}");
        assert!(takeoff.contains("Rotated at **148 kt**, lifted off at **152 kt**"), "{takeoff}");
        assert!(takeoff.contains("Nothing to work on in this takeoff.") && takeoff.contains("✅ Rotated at a steady rate."), "{takeoff}");
        let approach = field("Approach debrief");
        assert!(approach.starts_with("Straight-in · 6.2 NM final"), "{approach}");
        assert!(approach.contains("max sink **980 fpm**") && approach.contains("⚠️ Sink rate over 1,000 fpm"), "{approach}");

        // In the air, neither.
        let flying = serde_json::json!({ "departure": { "icao": "KPAE", "name": "Paine" } });
        let (embeds, _) = assemble_embeds(&flying, &user, "", "", None, None, &landed).unwrap();
        let text = serde_json::to_string(&embeds[0]).unwrap();
        assert!(!text.contains("Cruised") && !text.contains("debrief"), "{text}");
    }

    #[test]
    fn a_long_debrief_is_cut_at_a_line() {
        let text = (0..200).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let cut = truncate(&text, FIELD_MAX_CHARS);
        assert!(cut.chars().count() <= FIELD_MAX_CHARS && cut.ends_with("\n…"), "{cut}");
    }


    #[test]
    fn a_flight_on_flysto_links_it_in_its_post() {
        let statistics = serde_json::json!({
            "departure": { "icao": "KAST", "name": "Astoria" },
            "arrival": { "icao": "KAST", "name": "Astoria" },
            "landing_time": "2026-11-01 02:02:00.000",
        });
        let user = DiscordUserInfo { discord_id: "0".into(), username: "pilot".into(), global_name: None, avatar: None };
        let post = |flysto| {
            let (embeds, _) = assemble_embeds(&statistics, &user, "", "", None, flysto, &Landed::default()).unwrap();
            serde_json::to_string(&embeds[0]).unwrap()
        };
        let linked = post(Some("https://www.flysto.net/logs/y6wg6irz"));
        assert!(linked.contains("[Open this flight on FlySto](https://www.flysto.net/logs/y6wg6irz)"), "{linked}");
        assert!(!post(None).contains("FlySto"));
    }

    #[test]
    fn a_flight_begun_airborne_is_not_posted() {
        assert!(begun_airborne(&serde_json::json!({ "departure": { "icao": "Airborne", "name": "Airborne" } })));
        assert!(!begun_airborne(&serde_json::json!({ "departure": { "icao": "TKPK", "name": "Bradshaw" } })));
        assert!(!begun_airborne(&serde_json::json!({})));
    }

    #[test]
    fn telemetry_skips_null_and_non_numeric() {
        let snapshot = serde_json::json!({ "IAS": null, "TAS": {} });
        assert_eq!(get_formatted_fields_for_category(&snapshot, "normal"), "");
    }

    // -- Who may choose a server's channels, and whether the bot can post in one --

    const GUILD: u64 = 100;
    const BOT: u64 = 7;

    fn roles(list: &[(u64, Permissions)]) -> HashMap<RoleId, Permissions> {
        list.iter().map(|(id, p)| (RoleId::new(*id), *p)).collect()
    }

    fn role_overwrite(role: u64, allow: Permissions, deny: Permissions) -> PermissionOverwrite {
        PermissionOverwrite { allow, deny, kind: PermissionOverwriteType::Role(RoleId::new(role)) }
    }

    fn in_channel(member_roles: &[u64], roles: &HashMap<RoleId, Permissions>, overwrites: &[PermissionOverwrite]) -> Permissions {
        let member_roles: Vec<RoleId> = member_roles.iter().map(|r| RoleId::new(*r)).collect();
        channel_permissions(GuildId::new(GUILD), UserId::new(BOT), &member_roles, roles, overwrites)
    }

    #[test]
    fn the_invite_asks_for_what_posting_needs() {
        let url = reqwest::Url::parse(&invite_url("42")).unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("discord.com"));
        assert_eq!(query["client_id"], "42");
        assert_eq!(query["scope"], "bot");
        // View Channel, Send Messages, Embed Links, Attach Files.
        assert_eq!(query["permissions"], "52224");
    }

    #[test]
    fn the_owner_and_administrators_may_choose_channels_and_no_one_else() {
        let (guild, owner, user) = (GuildId::new(GUILD), UserId::new(1), UserId::new(2));
        let server = roles(&[
            (GUILD, Permissions::SEND_MESSAGES),
            (10, Permissions::ADMINISTRATOR),
            (11, Permissions::MANAGE_GUILD | Permissions::MANAGE_CHANNELS),
        ]);
        assert!(may_administer(guild, owner, owner, &[], &server));
        assert!(may_administer(guild, owner, user, &[RoleId::new(10)], &server));
        assert!(!may_administer(guild, owner, user, &[RoleId::new(11)], &server), "Manage Server is not enough");
        assert!(!may_administer(guild, owner, user, &[], &server));
        // @everyone holding Administrator makes everyone an administrator.
        let open = roles(&[(GUILD, Permissions::ADMINISTRATOR)]);
        assert!(may_administer(guild, owner, user, &[], &open));
    }

    #[test]
    fn the_bot_posts_where_its_roles_let_it() {
        let server = roles(&[(GUILD, Permissions::empty()), (20, POSTING_PERMISSIONS)]);
        assert!(missing_for_posting(in_channel(&[20], &server, &[])).is_ok());
        assert!(missing_for_posting(in_channel(&[], &server, &[])).is_err());
    }

    #[test]
    fn a_channel_hidden_from_everyone_hides_it_from_the_bot_too() {
        let server = roles(&[(GUILD, POSTING_PERMISSIONS), (20, Permissions::empty())]);
        let hidden = role_overwrite(GUILD, Permissions::empty(), Permissions::VIEW_CHANNEL);
        let err = missing_for_posting(in_channel(&[20], &server, &[hidden.clone()])).unwrap_err();
        assert!(err.contains("View Channel"), "{err}");
        // Until the channel lets the bot's role back in.
        let let_in = role_overwrite(20, Permissions::VIEW_CHANNEL, Permissions::empty());
        assert!(missing_for_posting(in_channel(&[20], &server, &[hidden, let_in])).is_ok());
    }

    #[test]
    fn a_role_allow_beats_a_role_deny_and_the_member_overwrite_beats_both() {
        let server = roles(&[(GUILD, POSTING_PERMISSIONS), (20, Permissions::empty()), (21, Permissions::empty())]);
        let deny = role_overwrite(20, Permissions::empty(), Permissions::ATTACH_FILES);
        let allow = role_overwrite(21, Permissions::ATTACH_FILES, Permissions::empty());
        assert!(in_channel(&[20, 21], &server, &[deny.clone(), allow.clone()]).contains(Permissions::ATTACH_FILES));
        assert!(!in_channel(&[20], &server, &[deny.clone(), allow.clone()]).contains(Permissions::ATTACH_FILES));
        let member = PermissionOverwrite {
            allow: Permissions::empty(),
            deny: Permissions::ATTACH_FILES,
            kind: PermissionOverwriteType::Member(UserId::new(BOT)),
        };
        assert!(!in_channel(&[20, 21], &server, &[deny, allow, member]).contains(Permissions::ATTACH_FILES));
    }

    #[test]
    fn administrator_ignores_every_overwrite() {
        let server = roles(&[(GUILD, Permissions::empty()), (20, Permissions::ADMINISTRATOR)]);
        let denied = [role_overwrite(GUILD, Permissions::empty(), Permissions::all())];
        assert!(missing_for_posting(in_channel(&[20], &server, &denied)).is_ok());
    }

    #[test]
    fn a_missing_attach_files_is_named() {
        let err = missing_for_posting(POSTING_PERMISSIONS - Permissions::ATTACH_FILES).unwrap_err();
        assert!(err.contains("Attach Files") && !err.contains("Send Messages"), "{err}");
        assert!(!err.contains("  "), "{err}");
    }

    #[test]
    fn flights_go_to_text_and_announcement_channels_only() {
        assert!(is_postable(ChannelType::Text));
        assert!(is_postable(ChannelType::News));
        for kind in [ChannelType::Voice, ChannelType::Forum, ChannelType::Category, ChannelType::Stage] {
            assert!(!is_postable(kind), "{kind:?}");
        }
    }

    // -- Which channels a pilot's flights go to, against a real Postgres --

    async fn test_db() -> Option<PgPool> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let db = PgPool::connect(&url).await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        Some(db)
    }

    async fn pilot(db: &PgPool) -> i64 {
        sqlx::query_scalar("INSERT INTO users (discord_id, username) VALUES ($1, 'pilot') RETURNING id")
            .bind(uuid::Uuid::new_v4().to_string())
            .fetch_one(db)
            .await
            .unwrap()
    }

    async fn channels_of(db: &PgPool, user: i64) -> Vec<String> {
        sqlx::query_scalar("SELECT channel_id FROM discord_notification_channels WHERE user_id = $1 ORDER BY channel_id")
            .bind(user)
            .fetch_all(db)
            .await
            .unwrap()
    }

    async fn updated_at(db: &PgPool, user: i64) -> Option<chrono::DateTime<chrono::Utc>> {
        sqlx::query_scalar("SELECT discord_notification_channels_updated_at FROM users WHERE id = $1")
            .bind(user)
            .fetch_one(db)
            .await
            .unwrap()
    }

    fn fresh_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    #[tokio::test]
    async fn a_pilot_is_posted_in_every_chosen_channel_of_their_servers_and_no_other() {
        let Some(db) = test_db().await else { return };
        let (theirs, other) = (fresh_id(), fresh_id());
        let (a, b, elsewhere) = (format!("a-{}", fresh_id()), format!("b-{}", fresh_id()), fresh_id());
        allowlist_channel(&db, &a, "flight-log", &theirs).await.unwrap();
        allowlist_channel(&db, &b, "landings", &theirs).await.unwrap();
        allowlist_channel(&db, &elsewhere, "flight-log", &other).await.unwrap();
        let user = pilot(&db).await;
        // A channel left over from a server they have since left.
        sqlx::query("INSERT INTO discord_notification_channels (user_id, channel_id) VALUES ($1, $2)")
            .bind(user)
            .bind(&elsewhere)
            .execute(&db)
            .await
            .unwrap();

        store_notification_channels(&db, user, &[theirs.clone()], chrono::Utc::now()).await.unwrap();
        assert_eq!(channels_of(&db, user).await, vec![a.clone(), b.clone()]);
        assert!(updated_at(&db, user).await.is_some());

        // Storing again changes nothing; leaving every server clears them all.
        store_notification_channels(&db, user, &[theirs], chrono::Utc::now()).await.unwrap();
        assert_eq!(channels_of(&db, user).await, vec![a, b]);
        store_notification_channels(&db, user, &[], chrono::Utc::now()).await.unwrap();
        assert!(channels_of(&db, user).await.is_empty());
    }

    #[tokio::test]
    async fn a_newly_chosen_channel_is_looked_up_on_the_pilots_next_flight() {
        let Some(db) = test_db().await else { return };
        let user = pilot(&db).await;
        let (guild, channel) = (fresh_id(), fresh_id());
        allowlist_channel(&db, &channel, "flight-log", &guild).await.unwrap();
        store_notification_channels(&db, user, &[], chrono::Utc::now()).await.unwrap();
        assert!(updated_at(&db, user).await.is_some());

        // Renaming a channel already chosen keeps every pilot's cache.
        allowlist_channel(&db, &channel, "flight-log-2", &guild).await.unwrap();
        assert!(updated_at(&db, user).await.is_some());
        let name: String = sqlx::query_scalar("SELECT channel_name FROM allowlisted_channels WHERE channel_id = $1")
            .bind(&channel)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(name, "flight-log-2");

        // A new one drops it, so the next flight asks Discord again.
        allowlist_channel(&db, &fresh_id(), "landings", &guild).await.unwrap();
        assert!(updated_at(&db, user).await.is_none());
    }

    #[tokio::test]
    async fn a_pilot_opted_out_of_a_channel_is_not_posted_there_until_they_opt_back_in() {
        let Some(db) = test_db().await else { return };
        let (guild, other) = (fresh_id(), fresh_id());
        let (log, landings, elsewhere) = (format!("a-{}", fresh_id()), format!("b-{}", fresh_id()), fresh_id());
        allowlist_channel(&db, &log, "flight-log", &guild).await.unwrap();
        allowlist_channel(&db, &landings, "landings", &guild).await.unwrap();
        allowlist_channel(&db, &elsewhere, "flight-log", &other).await.unwrap();
        let (user, neighbour) = (pilot(&db).await, pilot(&db).await);
        for who in [user, neighbour] {
            store_notification_channels(&db, who, &[guild.clone()], chrono::Utc::now()).await.unwrap();
        }

        assert!(set_channel_posting(&db, user, &log, false).await.unwrap());
        assert_eq!(posted_channels(&db, user).await.unwrap(), vec![landings.clone()]);
        // Only for them.
        assert_eq!(posted_channels(&db, neighbour).await.unwrap().len(), 2);
        // The hourly refresh, or leaving the server and joining again, keeps it.
        store_notification_channels(&db, user, &[], chrono::Utc::now()).await.unwrap();
        store_notification_channels(&db, user, &[guild.clone()], chrono::Utc::now()).await.unwrap();
        assert_eq!(posted_channels(&db, user).await.unwrap(), vec![landings.clone()]);
        // Saying it twice is no error.
        assert!(set_channel_posting(&db, user, &log, false).await.unwrap());

        // A channel of a server they are not in is not theirs to switch.
        assert!(!set_channel_posting(&db, user, &elsewhere, false).await.unwrap());
        assert!(!set_channel_posting(&db, user, &elsewhere, true).await.unwrap());

        assert!(set_channel_posting(&db, user, &log, true).await.unwrap());
        let mut posted = posted_channels(&db, user).await.unwrap();
        posted.sort();
        assert_eq!(posted, vec![log.clone(), landings]);

        // A channel Discord deleted takes its opt-outs with it.
        set_channel_posting(&db, user, &log, false).await.unwrap();
        forget_channel(&db, &log).await;
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM discord_channel_optouts WHERE channel_id = $1")
            .bind(&log)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }
}
