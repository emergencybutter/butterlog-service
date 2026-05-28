use serenity::async_trait;
use serenity::prelude::*;
use serenity::model::gateway::Ready;
use serenity::model::id::{ChannelId, MessageId, RoleId, GuildId, UserId};
use serenity::model::channel::{Channel, ChannelType, GuildChannel};
use serenity::model::guild::Member;
use serenity::model::permissions::Permissions;
use serenity::builder::{CreateMessage, EditMessage, CreateEmbed, CreateEmbedFooter, CreateEmbedAuthor, CreateAttachment};
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use serde_json::Value;

use crate::r2::R2Client;

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

    // Wait for the bot to be ready
    tracing::info!("Waiting for Discord bot to be ready...");
    while !is_ready.load(Ordering::Relaxed) {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    tracing::info!("Discord bot is ready!");

    Ok(http_client)
}

pub async fn validate_discord_channel(http: &serenity::http::Http, channel_id: u64) -> Result<(), String> {
    let chan_id = ChannelId::new(channel_id);
    let channel = chan_id.to_channel(http).await
        .map_err(|e| format!("Channel not found on Discord: {}", e))?;

    let guild_channel = match channel {
        Channel::Guild(c) => c,
        _ => return Err("Not a guild text channel".to_string()),
    };

    if guild_channel.kind != ChannelType::Text {
        return Err("Channel is not a text channel".to_string());
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

    let roles_map: std::collections::HashMap<_, _> = roles.into_iter().map(|r| (r.id, r)).collect();

    let permissions = calculate_bot_permissions(&guild_channel, &member, &roles_map);

    let required = Permissions::SEND_MESSAGES | Permissions::EMBED_LINKS;
    if !permissions.contains(required) {
        return Err("Bot lacks SEND_MESSAGES or EMBED_LINKS permission in this channel".to_string());
    }

    Ok(())
}

fn calculate_bot_permissions(
    channel: &GuildChannel,
    member: &Member,
    roles: &std::collections::HashMap<serenity::model::id::RoleId, serenity::model::guild::Role>,
) -> Permissions {
    // 1. Get @everyone role permissions
    let everyone_role_id = RoleId::new(channel.guild_id.get());
    let mut permissions = match roles.get(&everyone_role_id) {
        Some(role) => role.permissions,
        None => Permissions::empty(),
    };

    // 2. Apply member roles permissions
    for role_id in &member.roles {
        if let Some(role) = roles.get(role_id) {
            permissions |= role.permissions;
        }
    }

    // If ADMINISTRATOR is set, they have all permissions
    if permissions.administrator() {
        return Permissions::all();
    }

    // 3. Apply channel overwrites for @everyone
    if let Some(overwrite) = channel.permission_overwrites.iter().find(|o| {
        match o.kind {
            serenity::model::channel::PermissionOverwriteType::Role(id) => id.get() == everyone_role_id.get(),
            _ => false,
        }
    }) {
        permissions = (permissions & !overwrite.deny) | overwrite.allow;
    }

    // 4. Apply channel overwrites for member roles
    let mut role_allow = Permissions::empty();
    let mut role_deny = Permissions::empty();
    for role_id in &member.roles {
        if let Some(overwrite) = channel.permission_overwrites.iter().find(|o| {
            match o.kind {
                serenity::model::channel::PermissionOverwriteType::Role(id) => id.get() == role_id.get(),
                _ => false,
            }
        }) {
            role_allow |= overwrite.allow;
            role_deny |= overwrite.deny;
        }
    }
    permissions = (permissions & !role_deny) | role_allow;

    // 5. Apply channel overwrites for the member itself
    if let Some(overwrite) = channel.permission_overwrites.iter().find(|o| {
        match o.kind {
            serenity::model::channel::PermissionOverwriteType::Member(id) => id.get() == member.user.id.get(),
            _ => false,
        }
    }) {
        permissions = (permissions & !overwrite.deny) | overwrite.allow;
    }

    permissions
}

struct TelemetryField {
    key: &'static str,
    friendly_name: &'static str,
    unit: &'static str,
    digits: usize,
    categories: &'static [&'static str],
}

const TELEMETRY_FIELDS: &[TelemetryField] = &[
    TelemetryField { key: "AltB", friendly_name: "Altitude (Barometric)", unit: "ft", digits: 2, categories: &["normal", "max"] },
    TelemetryField { key: "BaroA", friendly_name: "Kohlsmann Setting", unit: "inHg", digits: 2, categories: &["instruments"] },
    TelemetryField { key: "AltGPS", friendly_name: "Altitude (GPS)", unit: "ft", digits: 2, categories: &["normal", "max"] },
    TelemetryField { key: "OAT", friendly_name: "Outside Air Temperature", unit: "C", digits: 2, categories: &["normal", "max"] },
    TelemetryField { key: "IAS", friendly_name: "Indicated Airspeed", unit: "kts", digits: 2, categories: &["normal", "max"] },
    TelemetryField { key: "TAS", friendly_name: "True Airspeed", unit: "kts", digits: 2, categories: &["normal", "max"] },
    TelemetryField { key: "GndSpd", friendly_name: "Ground Speed", unit: "kts", digits: 2, categories: &["normal", "max"] },
    TelemetryField { key: "VSpd", friendly_name: "Vertical Speed", unit: "fpm", digits: 2, categories: &["normal", "max", "landing"] },
    TelemetryField { key: "Pitch", friendly_name: "Pitch", unit: "deg", digits: 2, categories: &["landing"] },
    TelemetryField { key: "Roll", friendly_name: "Roll", unit: "deg", digits: 2, categories: &["landing"] },
    TelemetryField { key: "NormAc", friendly_name: "Normal Acceleration", unit: "G", digits: 2, categories: &["normal", "max", "landing"] },
    TelemetryField { key: "volt1", friendly_name: "Voltage 1", unit: "V", digits: 2, categories: &["engine"] },
    TelemetryField { key: "volt2", friendly_name: "Voltage 2", unit: "V", digits: 2, categories: &["engine"] },
    TelemetryField { key: "amp1", friendly_name: "Amperage 1", unit: "A", digits: 2, categories: &["engine"] },
    TelemetryField { key: "FQtyL", friendly_name: "Fuel Quantity Left", unit: "Gal", digits: 1, categories: &["engine"] },
    TelemetryField { key: "FQtyR", friendly_name: "Fuel Quantity Right", unit: "Gal", digits: 1, categories: &["engine"] },
    TelemetryField { key: "E1 FFlow", friendly_name: "Engine 1 Fuel Flow", unit: "Gal/h", digits: 2, categories: &["engine"] },
    TelemetryField { key: "E1 OilT", friendly_name: "Engine 1 Oil Temperature", unit: "F", digits: 2, categories: &["engine", "max"] },
    TelemetryField { key: "E1 OilP", friendly_name: "Engine 1 Oil Pressure", unit: "psi", digits: 2, categories: &["engine", "max"] },
    TelemetryField { key: "E1 MAP", friendly_name: "Engine 1 Manifold Pressure", unit: "inHg", digits: 2, categories: &["engine"] },
    TelemetryField { key: "E1 RPM", friendly_name: "Engine 1 RPM", unit: "rpm", digits: 2, categories: &["engine", "max"] },
    TelemetryField { key: "E1 %Pwr", friendly_name: "Engine 1 Percent Power", unit: "%", digits: 2, categories: &["engine"] },
    TelemetryField { key: "E1 CHT1", friendly_name: "Engine 1 Cylinder Head Temp 1", unit: "F", digits: 0, categories: &["engine", "max"] },
    TelemetryField { key: "E1 EGT1", friendly_name: "Engine 1 Exhaust Gas Temp 1", unit: "F", digits: 0, categories: &["engine", "max"] },
    TelemetryField { key: "E1 TIT1", friendly_name: "Engine 1 Turbine Inlet Temp 1", unit: "F", digits: 0, categories: &["engine"] },
    TelemetryField { key: "E1 TIT2", friendly_name: "Engine 1 Turbine Inlet Temp 2", unit: "F", digits: 0, categories: &["engine"] },
    TelemetryField { key: "COM1", friendly_name: "COM1", unit: "MHz", digits: 3, categories: &["instruments"] },
    TelemetryField { key: "COM2", friendly_name: "COM2", unit: "MHz", digits: 3, categories: &["instruments"] },
    TelemetryField { key: "WndSpd", friendly_name: "Wind Speed", unit: "kts", digits: 2, categories: &["normal", "max", "landing"] },
    TelemetryField { key: "WndDr", friendly_name: "Wind Direction", unit: "deg", digits: 2, categories: &["normal"] },
    TelemetryField { key: "AfcsOn", friendly_name: "Autopilot", unit: "", digits: 0, categories: &["instruments"] },
];

fn format_telemetry_value(field: &TelemetryField, value: &Value) -> Option<String> {
    match value {
        Value::Bool(b) => {
            let state = if *b { "On" } else { "Off" };
            Some(format!("**{}**: {}", field.friendly_name, state))
        }
        Value::Number(num) => {
            if let Some(val) = num.as_f64() {
                if field.key == "AfcsOn" {
                    let state = if val > 0.5 { "On" } else { "Off" };
                    return Some(format!("**{}**: {}", field.friendly_name, state));
                }
                let formatted = format!("{:.*}", field.digits, val);
                let unit_suffix = if field.unit.is_empty() { "".to_string() } else { format!(" {}", field.unit) };
                Some(format!("**{}**: {}{}", field.friendly_name, formatted, unit_suffix))
            } else {
                None
            }
        }
        Value::String(s) => {
            if let Ok(val) = s.parse::<f64>() {
                if field.key == "AfcsOn" {
                    let state = if val > 0.5 { "On" } else { "Off" };
                    return Some(format!("**{}**: {}", field.friendly_name, state));
                }
                let formatted = format!("{:.*}", field.digits, val);
                let unit_suffix = if field.unit.is_empty() { "".to_string() } else { format!(" {}", field.unit) };
                Some(format!("**{}**: {}{}", field.friendly_name, formatted, unit_suffix))
            } else if s.to_lowercase() == "true" || s.to_lowercase() == "false" {
                let state = if s.to_lowercase() == "true" { "On" } else { "Off" };
                Some(format!("**{}**: {}", field.friendly_name, state))
            } else {
                Some(format!("**{}**: {}", field.friendly_name, s))
            }
        }
        _ => None,
    }
}

fn get_formatted_fields_for_category(snapshot: &Value, category: &str) -> String {
    let mut lines = Vec::new();
    for field in TELEMETRY_FIELDS {
        if field.categories.contains(&category) {
            if let Some(val) = snapshot.get(field.key) {
                if !val.is_null() {
                    if let Some(formatted) = format_telemetry_value(field, val) {
                        lines.push(formatted);
                    }
                }
            }
        }
    }
    lines.join("\n")
}

fn format_timestamp_to_discord(iso_str: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(iso_str) {
        let epoch = dt.timestamp();
        format!("<t:{}:F> (<t:{}:R>)", epoch, epoch)
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
        "SELECT discord_id, discord_notification_channels_updated_at FROM users WHERE id = $1"
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

    // 2. Fetch all allowlisted channels to know what guilds we care about
    let allowlisted: Vec<(String, String)> = sqlx::query_as(
        "SELECT channel_id, guild_id FROM allowlisted_channels"
    )
    .fetch_all(db)
    .await?;

    // Determine distinct guilds
    let mut guild_ids = std::collections::HashSet::new();
    for (_, guild_id) in &allowlisted {
        guild_ids.insert(guild_id.clone());
    }

    // 3. For each guild, check if user belongs to it
    let mut user_guilds = std::collections::HashSet::new();
    for guild_id_str in guild_ids {
        if let Ok(guild_id_u64) = guild_id_str.parse::<u64>() {
            let g_id = serenity::model::id::GuildId::new(guild_id_u64);
            let u_id = serenity::model::id::UserId::new(user_discord_id);
            if http.get_member(g_id, u_id).await.is_ok() {
                user_guilds.insert(guild_id_str);
            }
        }
    }

    // 4. Collect all allowlisted channel IDs that belong to the user's guilds
    let mut target_channels = Vec::new();
    for (channel_id, guild_id) in allowlisted {
        if user_guilds.contains(&guild_id) {
            target_channels.push(channel_id);
        }
    }

    // 5. Update database inside a transaction
    let mut tx = db.begin().await?;

    // Delete channels not in the target list anymore
    if target_channels.is_empty() {
        sqlx::query("DELETE FROM discord_notification_channels WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query(
            "DELETE FROM discord_notification_channels WHERE user_id = $1 AND NOT (channel_id = ANY($2))"
        )
        .bind(user_id)
        .bind(&target_channels)
        .execute(&mut *tx)
        .await?;

        // Insert new ones
        for channel_id in &target_channels {
            sqlx::query(
                "INSERT INTO discord_notification_channels (user_id, channel_id) VALUES ($1, $2) \
                 ON CONFLICT (user_id, channel_id) DO NOTHING"
            )
            .bind(user_id)
            .bind(channel_id)
            .execute(&mut *tx)
            .await?;
        }
    }

    // Update timestamp
    sqlx::query(
        "UPDATE users SET discord_notification_channels_updated_at = $1 WHERE id = $2"
    )
    .bind(now)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(())
}

pub async fn sync_flight_discord(
    db: &PgPool,
    r2: &R2Client,
    http: &serenity::http::Http,
    flight_id: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Fetch flight information and user information
    let row: Option<(i64, i64, String, Option<String>, Value, String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT f.id, f.user_id, f.departure, f.arrival, f.statistics, u.discord_id, u.username, u.global_name, u.avatar \
         FROM flights f \
         JOIN users u ON f.user_id = u.id \
         WHERE f.id = $1"
    )
    .bind(flight_id)
    .fetch_optional(db)
    .await?;

    let (_, user_id, _departure, _arrival, statistics, discord_id, username, global_name, avatar) = match row {
        Some(r) => r,
        None => return Ok(()),
    };

    // Update user notification channels before notifying
    if let Err(e) = maybe_update_user_notification_channels(db, http, user_id).await {
        tracing::error!("Failed to update user notification channels: {}", e);
    }

    let user_info = DiscordUserInfo {
        discord_id,
        username,
        global_name,
        avatar,
    };

    // 2. Fetch the target channels registered by this user
    let channels: Vec<String> = sqlx::query_scalar(
        "SELECT channel_id FROM discord_notification_channels \
         WHERE user_id = (SELECT user_id FROM flights WHERE id = $1)"
    )
    .bind(flight_id)
    .fetch_all(db)
    .await?;

    if channels.is_empty() {
        return Ok(());
    }

    // 3. Assemble primary embed & helper embeds
    let (embeds, _) = assemble_embeds(&statistics, &user_info)?;

    // 4. Fetch screenshots associated with this flight from DB
    // Limit to up to 9 screenshots as specified
    let screenshot_rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT hash, url FROM screenshots WHERE flight_id = $1 ORDER BY id LIMIT 9"
    )
    .bind(flight_id)
    .fetch_all(db)
    .await?;

    // 5. Download screenshot bytes from R2 for attachments
    let mut attachments = Vec::new();
    let mut helper_embeds = Vec::new();

    for (index, (hash, _url)) in screenshot_rows.into_iter().enumerate() {
        let key = format!("screenshots/{}/{}.webp", flight_id, hash);
        match r2.download_object(&key).await {
            Ok(bytes) => {
                let filename = format!("screenshot-{}.jpg", index);
                attachments.push(CreateAttachment::bytes(bytes, &filename));

                let helper = CreateEmbed::new()
                    .url("https://butterlog.flyvoyager.net/")
                    .image(format!("attachment://{}", filename));
                helper_embeds.push(helper);
            }
            Err(e) => {
                tracing::error!("Failed to download screenshot {} from R2: {:?}", hash, e);
            }
        }
    }

    // Combine primary and helper embeds
    let mut all_embeds = embeds;
    all_embeds.extend(helper_embeds);

    // 6. Process message state synchronization for each channel
    for channel_str in channels {
        let channel_id = match channel_str.parse::<u64>() {
            Ok(c) => ChannelId::new(c),
            Err(_) => continue,
        };

        // Query if message has already been sent to this channel
        let message_id_str: Option<String> = sqlx::query_scalar(
            "SELECT discord_message_id FROM flight_discord_messages \
             WHERE flight_id = $1 AND discord_channel_id = $2"
        )
        .bind(flight_id)
        .bind(&channel_str)
        .fetch_optional(db)
        .await?;

        if let Some(msg_id_str) = message_id_str {
            if let Ok(msg_id_val) = msg_id_str.parse::<u64>() {
                let msg_id = MessageId::new(msg_id_val);

                // Edit existing message using attachments replacements
                let mut edit_attachments = serenity::builder::EditAttachments::new();
                for attachment in attachments.clone() {
                    edit_attachments = edit_attachments.add(attachment);
                }

                let builder = EditMessage::new()
                    .embeds(all_embeds.clone())
                    .attachments(edit_attachments);

                tracing::info!("[Outgoing Request] Discord EDIT message {} in channel {}", msg_id, channel_id);
                match channel_id.edit_message(http, msg_id, builder).await {
                    Ok(_) => {
                        tracing::info!("[Outgoing Response] Edited message {} successfully", msg_id);
                    }
                    Err(e) => {
                        tracing::error!("Failed to edit Discord message {} in channel {}: {:?}", msg_id, channel_id, e);
                    }
                }
            }
        } else {
            // Send new message
            let builder = CreateMessage::new()
                .embeds(all_embeds.clone())
                .files(attachments.clone());

            tracing::info!("[Outgoing Request] Discord SEND message in channel {}", channel_id);
            match channel_id.send_message(http, builder).await {
                Ok(msg) => {
                    tracing::info!("[Outgoing Response] Sent message {} successfully", msg.id);
                    // Record message in database
                    let msg_id_str = msg.id.to_string();
                    let _ = sqlx::query(
                        "INSERT INTO flight_discord_messages (flight_id, discord_message_id, discord_channel_id) \
                         VALUES ($1, $2, $3)"
                    )
                    .bind(flight_id)
                    .bind(&msg_id_str)
                    .bind(&channel_str)
                    .execute(db)
                    .await;
                }
                Err(e) => {
                    tracing::error!("Failed to send Discord message in channel {}: {:?}", channel_id, e);
                }
            }
        }
    }

    Ok(())
}

struct LocalField {
    name: String,
    value: String,
    inline: bool,
}

fn assemble_embeds(
    statistics: &Value,
    user_info: &DiscordUserInfo,
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
        let arr_name = arrival_name.unwrap_or("Destination");
        description.push_str(&format!("Landed in **{}** at {}\n", arr_name, format_timestamp_to_discord(l_time)));
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
            // Cruising fields
            let in_flight_stats = get_formatted_fields_for_category(curr, "normal");
            if !in_flight_stats.is_empty() {
                fields.push(LocalField {
                    name: "Currently In Flight".to_string(),
                    value: in_flight_stats,
                    inline: true,
                });
            }

            let instrument_stats = get_formatted_fields_for_category(curr, "instruments");
            if !instrument_stats.is_empty() {
                fields.push(LocalField {
                    name: "Instruments".to_string(),
                    value: instrument_stats,
                    inline: true,
                });
            }

            let engine_stats = get_formatted_fields_for_category(curr, "engine");
            if !engine_stats.is_empty() {
                fields.push(LocalField {
                    name: "Engine Details".to_string(),
                    value: engine_stats,
                    inline: true,
                });
            }
        }
    } else {
        // Landed stats
        if let Some(landing) = statistics.get("landing_snapshot") {
            let landing_stats = get_formatted_fields_for_category(landing, "landing");
            if !landing_stats.is_empty() {
                fields.push(LocalField {
                    name: "Landing Stats".to_string(),
                    value: landing_stats,
                    inline: true,
                });
            }
        }

        if let Some(max_e) = max_entries {
            let max_stats = get_formatted_fields_for_category(max_e, "normal");
            if !max_stats.is_empty() {
                fields.push(LocalField {
                    name: "Max Stats".to_string(),
                    value: max_stats,
                    inline: true,
                });
            }
        }
    }

    let footer = CreateEmbedFooter::new("ButterLog")
        .icon_url("https://butterlog.flyvoyager.net/apple-touch-icon.png");

    let primary_embed = CreateEmbed::new()
        .title(title)
        .url("https://butterlog.flyvoyager.net/")
        .thumbnail("https://butterlog.flyvoyager.net/apple-touch-icon.png")
        .color(color)
        .author(author)
        .description(description)
        .footer(footer);

    // Apply fields to primary embed
    let mut final_embed = primary_embed;
    for f in fields {
        final_embed = final_embed.field(f.name, f.value, f.inline);
    }

    Ok((vec![final_embed], Vec::new()))
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

#[allow(dead_code)]
pub async fn get_bot_channels(
    http: &serenity::http::Http,
    predetermined_env: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let mut channels_list = Vec::new();

    // 1. Parse predetermined channels from env if present
    if let Some(raw) = predetermined_env {
        for item in raw.split(',') {
            let parts: Vec<&str> = item.trim().split(':').collect();
            if parts.len() == 2 {
                channels_list.push((parts[0].to_string(), parts[1].to_string()));
            } else if !item.trim().is_empty() {
                channels_list.push((item.trim().to_string(), format!("Channel {}", item.trim())));
            }
        }
    }

    // 2. Fetch channels from all guilds the bot is in
    match http.get_guilds(None, None).await {
        Ok(guilds) => {
            for guild in guilds {
                if let Ok(channels) = http.get_channels(guild.id).await {
                    for channel in channels {
                        if channel.kind == ChannelType::Text {
                            let label = format!("{} - #{}", guild.name, channel.name);
                            if !channels_list.iter().any(|(id, _)| id == &channel.id.to_string()) {
                                channels_list.push((channel.id.to_string(), label));
                            }
                        }
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to fetch bot guilds dynamically: {:?}", e);
        }
    }

    // 3. Fallback default channel if list is empty
    if channels_list.is_empty() {
        channels_list.push(("1462209019740426452".to_string(), "Default Voyager Channel".to_string()));
    }

    Ok(channels_list)
}

#[derive(Debug, Clone)]
pub struct BotGuildInfo {
    pub id: String,
    pub name: String,
    pub channels: Vec<(String, String)>, // (channel_id, channel_name)
    pub is_user_admin: bool,
}

pub async fn is_user_admin_in_guild(
    http: &serenity::http::Http,
    guild_id: u64,
    user_id: u64,
) -> bool {
    let g_id = GuildId::new(guild_id);
    let u_id = UserId::new(user_id);

    // 1. Check if the user is the owner of the guild
    if let Ok(guild) = g_id.to_partial_guild(http).await {
        if guild.owner_id == u_id {
            return true;
        }
    }

    // 2. Fetch member and check permissions based on roles
    if let Ok(member) = http.get_member(g_id, u_id).await {
        if let Ok(roles) = http.get_guild_roles(g_id).await {
            let roles_map: std::collections::HashMap<_, _> = roles.into_iter().map(|r| (r.id, r)).collect();
            let everyone_role_id = RoleId::new(guild_id);
            let mut permissions = match roles_map.get(&everyone_role_id) {
                Some(role) => role.permissions,
                None => Permissions::empty(),
            };

            for role_id in &member.roles {
                if let Some(role) = roles_map.get(role_id) {
                    permissions |= role.permissions;
                }
            }

            return permissions.administrator();
        }
    }

    false
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
                if channel.kind == ChannelType::Text {
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
