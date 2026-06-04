use axum::{
    extract::{Path, State, Multipart},
    http::{StatusCode, HeaderMap},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use crate::AppState;
use crate::error::AppError;
use sha2::{Digest, Sha256};
use flate2::read::GzDecoder;
use std::io::Read;

/// Maximum allowed length (in characters) of a user-provided flight note.
const MAX_NOTES_LEN: usize = 500;

/// Validate an optional notes value against the length limit.
fn validate_notes(notes: &Option<String>) -> Result<(), AppError> {
    if let Some(text) = notes {
        if text.chars().count() > MAX_NOTES_LEN {
            return Err(AppError::BadRequest(format!(
                "Notes must be {} characters or fewer",
                MAX_NOTES_LEN
            )));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct CreateFlightRequest {
    pub departure: String,
    pub statistics: serde_json::Value,
    pub multiplayer_enabled: Option<bool>,
    pub udp_address: Option<String>,
    pub notes: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateFlightRequest {
    pub arrival: Option<String>,
    pub statistics: serde_json::Value,
    pub multiplayer_enabled: Option<bool>,
    pub udp_address: Option<String>,
    pub notes: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateNotesRequest {
    pub notes: String,
}

#[derive(Serialize)]
pub struct FlightResponse {
    pub id: i64,
    pub user_id: i64,
    pub departure: String,
    pub arrival: Option<String>,
    pub statistics: serde_json::Value,
    pub screenshots: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peers: Option<Vec<String>>,
}

fn update_and_get_peers(
    state: &crate::AppState,
    user_id: i64,
    multiplayer_enabled: Option<bool>,
    udp_address: Option<String>,
) -> Option<Vec<String>> {
    let mut peers_lock = state.peers.lock().ok()?;
    let now = std::time::Instant::now();

    // 1. Update or remove current peer
    if multiplayer_enabled.unwrap_or(false) {
        if let Some(ref addr) = udp_address {
            if !addr.trim().is_empty() {
                peers_lock.insert(user_id, (addr.clone(), now));
            }
        }
    } else if let Some(false) = multiplayer_enabled {
        peers_lock.remove(&user_id);
    }

    // 2. Prune stale peers (older than 120s)
    peers_lock.retain(|_, (_, last_seen)| {
        now.duration_since(*last_seen) < std::time::Duration::from_secs(120)
    });

    // 3. Collect other active peers
    let mut other_peers = Vec::new();
    for (&peer_user_id, (addr, _)) in peers_lock.iter() {
        if peer_user_id != user_id {
            other_peers.push(addr.clone());
        }
    }

    if multiplayer_enabled.unwrap_or(false) {
        Some(other_peers)
    } else {
        None
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct AddChannelRequest {
    #[serde(rename = "channelId", alias = "channel_id")]
    pub channel_id: String,
}

async fn authenticate_user(
    db_pool: &PgPool,
    webhook_token: &str,
) -> Result<i64, AppError> {
    let user_id: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE api_token = $1")
        .bind(webhook_token)
        .fetch_optional(db_pool)
        .await?;

    user_id.ok_or_else(|| AppError::Auth("Invalid webhook token".to_string()))
}

/// Returns whether the given flight exists and is owned by the given user.
async fn flight_belongs_to_user(
    db_pool: &PgPool,
    flight_id: i64,
    user_id: i64,
) -> Result<bool, AppError> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM flights WHERE id = $1 AND user_id = $2)"
    )
    .bind(flight_id)
    .bind(user_id)
    .fetch_one(db_pool)
    .await?;
    Ok(exists)
}

pub async fn get_user_id_from_session(
    db_pool: &PgPool,
    headers: &HeaderMap,
) -> Result<i64, AppError> {
    // 1. Check Authorization header
    let mut token = None;
    if let Some(auth_val) = headers.get("Authorization").and_then(|v| v.to_str().ok()) {
        if auth_val.starts_with("Bearer ") {
            token = Some(auth_val[7..].to_string());
        } else {
            token = Some(auth_val.to_string());
        }
    }

    // 2. Check Cookie header
    if token.is_none() {
        if let Some(cookie_val) = headers.get("Cookie").and_then(|v| v.to_str().ok()) {
            for cookie in cookie_val.split(';') {
                let parts: Vec<&str> = cookie.trim().split('=').collect();
                if parts.len() == 2 && (parts[0] == "token" || parts[0] == "session") {
                    token = Some(parts[1].to_string());
                    break;
                }
            }
        }
    }

    let token_str = token.ok_or_else(|| AppError::Auth("Unauthorized".to_string()))?;

    let user_id: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE api_token = $1")
        .bind(&token_str)
        .fetch_optional(db_pool)
        .await?;

    user_id.ok_or_else(|| AppError::Auth("Invalid session token".to_string()))
}

pub async fn get_discord_channels_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;

    let channels: Vec<String> = sqlx::query_scalar(
        "SELECT channel_id FROM discord_notification_channels WHERE user_id = $1 ORDER BY id"
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?;

    Ok((StatusCode::OK, Json(channels)))
}

pub async fn add_discord_channel_handler(
    State(_state): State<AppState>,
    _headers: HeaderMap,
    Json(_payload): Json<AddChannelRequest>,
) -> Result<StatusCode, AppError> {
    Err(AppError::Forbidden(
        "Direct mutation of notification channels is disabled. They are automatically managed."
            .to_string(),
    ))
}

pub async fn delete_discord_channel_handler(
    State(_state): State<AppState>,
    Path(_channel_id): Path<String>,
    _headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    Err(AppError::Forbidden(
        "Direct mutation of notification channels is disabled. They are automatically managed."
            .to_string(),
    ))
}

pub async fn create_flight_handler(
    State(state): State<AppState>,
    Path(webhook_token): Path<String>,
    Json(payload): Json<CreateFlightRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;
    validate_notes(&payload.notes)?;

    let row: (i64, String, Option<String>, serde_json::Value, Option<String>) = sqlx::query_as(
        "INSERT INTO flights (user_id, departure, statistics, notes) VALUES ($1, $2, $3, $4) RETURNING id, departure, arrival, statistics, notes"
    )
    .bind(user_id)
    .bind(&payload.departure)
    .bind(&payload.statistics)
    .bind(&payload.notes)
    .fetch_one(&state.db)
    .await?;

    let response = FlightResponse {
        id: row.0,
        user_id,
        departure: row.1,
        arrival: row.2,
        statistics: row.3,
        screenshots: Vec::new(),
        notes: row.4,
        peers: update_and_get_peers(&state, user_id, payload.multiplayer_enabled, payload.udp_address),
    };

    // Trigger Discord Sync in background
    let db_clone = state.db.clone();
    let r2_clone = state.r2.clone();
    let discord_http_clone = state.discord_http.clone();
    let flight_id = response.id;
    tokio::spawn(async move {
        if let Err(e) = crate::discord::sync_flight_discord(&db_clone, &r2_clone, &discord_http_clone, flight_id).await {
            tracing::error!("Discord sync failed for flight {}: {:?}", flight_id, e);
        }
    });

    Ok((StatusCode::CREATED, Json(response)))
}

pub async fn update_flight_handler(
    State(state): State<AppState>,
    Path((webhook_token, flight_id)): Path<(String, i64)>,
    Json(payload): Json<UpdateFlightRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;
    validate_notes(&payload.notes)?;

    if !flight_belongs_to_user(&state.db, flight_id, user_id).await? {
        return Err(AppError::NotFound("Flight not found".to_string()));
    }

    // Perform update; COALESCE keeps the existing value when the payload omits it.
    let (departure, arrival, statistics, notes): (String, Option<String>, serde_json::Value, Option<String>) = sqlx::query_as(
        "UPDATE flights SET arrival = COALESCE($1, arrival), statistics = $2, notes = COALESCE($3, notes), updated_at = CURRENT_TIMESTAMP \
         WHERE id = $4 RETURNING departure, arrival, statistics, notes"
    )
    .bind(&payload.arrival)
    .bind(&payload.statistics)
    .bind(&payload.notes)
    .bind(flight_id)
    .fetch_one(&state.db)
    .await?;

    // Fetch screenshots for response
    let screenshots = sqlx::query_scalar::<_, String>(
        "SELECT hash FROM screenshots WHERE flight_id = $1 ORDER BY id"
    )
    .bind(flight_id)
    .fetch_all(&state.db)
    .await?;

    // Trigger Discord Sync in background
    let db_clone = state.db.clone();
    let r2_clone = state.r2.clone();
    let discord_http_clone = state.discord_http.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::discord::sync_flight_discord(&db_clone, &r2_clone, &discord_http_clone, flight_id).await {
            tracing::error!("Discord sync failed for flight {}: {:?}", flight_id, e);
        }
    });

    let response = FlightResponse {
        id: flight_id,
        user_id,
        departure,
        arrival,
        statistics,
        screenshots,
        notes,
        peers: update_and_get_peers(&state, user_id, payload.multiplayer_enabled, payload.udp_address),
    };

    Ok((StatusCode::OK, Json(response)))
}

/// Update only the notes for a flight, enforcing the 500-character limit.
pub async fn update_flight_notes_handler(
    State(state): State<AppState>,
    Path((webhook_token, flight_id)): Path<(String, i64)>,
    Json(payload): Json<UpdateNotesRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    if payload.notes.chars().count() > MAX_NOTES_LEN {
        return Err(AppError::BadRequest(format!(
            "Notes must be {} characters or fewer",
            MAX_NOTES_LEN
        )));
    }

    if !flight_belongs_to_user(&state.db, flight_id, user_id).await? {
        return Err(AppError::NotFound("Flight not found".to_string()));
    }

    sqlx::query(
        "UPDATE flights SET notes = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2"
    )
    .bind(&payload.notes)
    .bind(flight_id)
    .execute(&state.db)
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_flight_handler(
    State(state): State<AppState>,
    Path((webhook_token, flight_id)): Path<(String, i64)>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    let flight_row: Option<(String, Option<String>, serde_json::Value, Option<String>)> = sqlx::query_as(
        "SELECT departure, arrival, statistics, notes FROM flights WHERE id = $1 AND user_id = $2"
    )
    .bind(flight_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    let (departure, arrival, statistics, notes) = flight_row
        .ok_or_else(|| AppError::NotFound("Flight not found".to_string()))?;

    let screenshots = sqlx::query_scalar::<_, String>(
        "SELECT hash FROM screenshots WHERE flight_id = $1 ORDER BY id"
    )
    .bind(flight_id)
    .fetch_all(&state.db)
    .await?;

    let response = FlightResponse {
        id: flight_id,
        user_id,
        departure,
        arrival,
        statistics,
        screenshots,
        notes,
        peers: None,
    };

    Ok((StatusCode::OK, Json(response)))
}

pub async fn upload_screenshot_handler(
    State(state): State<AppState>,
    Path((webhook_token, flight_id)): Path<(String, i64)>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    if !flight_belongs_to_user(&state.db, flight_id, user_id).await? {
        return Err(AppError::NotFound("Flight not found".to_string()));
    }

    // Extract image field
    let mut field_bytes = None;
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        AppError::BadRequest(format!("Failed to read multipart field: {}", e))
    })? {
        if field.name() == Some("screenshot") {
            let bytes = field.bytes().await.map_err(|e| {
                AppError::BadRequest(format!("Failed to read field bytes: {}", e))
            })?;
            field_bytes = Some(bytes.to_vec());
            break;
        }
    }

    let raw_bytes = field_bytes
        .ok_or_else(|| AppError::BadRequest("No screenshot field found".to_string()))?;

    // Refuse upload if file is larger than 100MB
    if raw_bytes.len() > 100 * 1024 * 1024 {
        return Err(AppError::BadRequest("File size exceeds 100MB limit".to_string()));
    }

    // Verify format is WebP
    let format = image::guess_format(&raw_bytes).map_err(|e| {
        AppError::BadRequest(format!("Could not identify image format: {}", e))
    })?;
    if format != image::ImageFormat::WebP {
        return Err(AppError::BadRequest("Only WebP images are allowed".to_string()));
    }

    // Load and verify dimensions (max width and height of 1600px)
    let img = image::load_from_memory(&raw_bytes).map_err(|e| {
        AppError::BadRequest(format!("Invalid image data: {}", e))
    })?;
    if img.width() > 1600 || img.height() > 1600 {
        return Err(AppError::BadRequest("Image dimensions exceed 1600px limit".to_string()));
    }

    // Calculate SHA-256 hash of the upload
    let mut hasher = Sha256::new();
    hasher.update(&raw_bytes);
    let hash = format!("{:x}", hasher.finalize());

    // Upload to Cloudflare R2
    let key = format!("screenshots/{}/{}.webp", flight_id, hash);
    let url = state.r2.upload_object(&key, raw_bytes, "image/webp").await
        .map_err(AppError::Storage)?;

    // Save/update to DB (avoid duplicate check via UNIQUE constraint)
    sqlx::query(
        "INSERT INTO screenshots (flight_id, hash, url) VALUES ($1, $2, $3) ON CONFLICT (flight_id, hash) DO NOTHING"
    )
    .bind(flight_id)
    .bind(&hash)
    .bind(&url)
    .execute(&state.db)
    .await?;

    // Trigger Discord Sync in background
    let db_clone = state.db.clone();
    let r2_clone = state.r2.clone();
    let discord_http_clone = state.discord_http.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::discord::sync_flight_discord(&db_clone, &r2_clone, &discord_http_clone, flight_id).await {
            tracing::error!("Discord sync failed for flight {}: {:?}", flight_id, e);
        }
    });

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "hash": hash, "url": url }))))
}

pub async fn delete_screenshot_handler(
    State(state): State<AppState>,
    Path((webhook_token, flight_id, hash)): Path<(String, i64, String)>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    if !flight_belongs_to_user(&state.db, flight_id, user_id).await? {
        return Err(AppError::NotFound("Flight not found".to_string()));
    }

    // Query URL from DB to determine file extension for R2 key
    let url: Option<String> = sqlx::query_scalar("SELECT url FROM screenshots WHERE flight_id = $1 AND hash = $2")
        .bind(flight_id)
        .bind(&hash)
        .fetch_optional(&state.db)
        .await?;

    // Delete from DB first
    sqlx::query("DELETE FROM screenshots WHERE flight_id = $1 AND hash = $2")
        .bind(flight_id)
        .bind(&hash)
        .execute(&state.db)
        .await?;

    // Check if any other flight references this hash
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM screenshots WHERE hash = $1")
        .bind(&hash)
        .fetch_one(&state.db)
        .await?;

    if count == 0 {
        let key = if let Some(url_str) = url {
            if url_str.ends_with(".webp") {
                format!("screenshots/{}/{}.webp", flight_id, hash)
            } else {
                format!("screenshots/{}/{}.jpg", flight_id, hash)
            }
        } else {
            format!("screenshots/{}/{}.webp", flight_id, hash)
        };

        // Delete from R2
        state.r2.delete_object(&key).await
            .map_err(AppError::Storage)?;
    }

    // Trigger Discord Sync in background
    let db_clone = state.db.clone();
    let r2_clone = state.r2.clone();
    let discord_http_clone = state.discord_http.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::discord::sync_flight_discord(&db_clone, &r2_clone, &discord_http_clone, flight_id).await {
            tracing::error!("Discord sync failed for flight {}: {:?}", flight_id, e);
        }
    });

    Ok(StatusCode::NO_CONTENT)
}

pub async fn upload_flight_share_handler(
    State(state): State<AppState>,
    Path(webhook_token): Path<String>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    if body.is_empty() {
        return Err(AppError::BadRequest("Empty body".to_string()));
    }

    // Decompress only to validate and extract remote_flight_id for the DB record
    let mut decoder = GzDecoder::new(body.as_ref());
    let mut json_str = String::new();
    decoder.read_to_string(&mut json_str)
        .map_err(|e| AppError::BadRequest(format!("Failed to decompress: {}", e)))?;

    let share: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| AppError::BadRequest(format!("Invalid JSON: {}", e)))?;

    let remote_flight_id = share.get("remoteFlightId").and_then(|v| v.as_i64())
        .or_else(|| share.get("remote_flight_id").and_then(|v| v.as_i64()));

    // Store the original compressed bytes as-is
    let share_id = uuid::Uuid::new_v4().to_string();
    let r2_key = format!("shares/{}.json.gz", share_id);
    state.r2.upload_object(&r2_key, body.to_vec(), "application/octet-stream")
        .await
        .map_err(AppError::Storage)?;

    sqlx::query(
        "INSERT INTO flight_shares (id, user_id, remote_flight_id, r2_key) VALUES ($1, $2, $3, $4)"
    )
    .bind(&share_id)
    .bind(user_id)
    .bind(remote_flight_id)
    .bind(&r2_key)
    .execute(&state.db)
    .await?;

    // Update Discord notification with share link if a message exists for this flight
    if let Some(flight_id) = remote_flight_id {
        let db_clone = state.db.clone();
        let r2_clone = state.r2.clone();
        let discord_http_clone = state.discord_http.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::discord::sync_flight_discord(&db_clone, &r2_clone, &discord_http_clone, flight_id).await {
                tracing::warn!("Failed to update Discord notification after share: {:?}", e);
            }
        });
    }

    let share_url = format!("https://butterlog.flyvoyager.net/content/flights/share/{}", share_id);
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "url": share_url, "id": share_id }))))
}

pub async fn delete_flight_share_handler(
    State(state): State<AppState>,
    Path((webhook_token, share_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT fs.r2_key, fs.remote_flight_id FROM flight_shares fs \
         WHERE fs.id = $1 AND fs.user_id = $2"
    )
    .bind(&share_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    let (r2_key, _) = row.ok_or_else(|| AppError::NotFound("Share not found".to_string()))?;

    // Delete from R2
    state.r2.delete_object(&r2_key).await
        .map_err(AppError::Storage)?;

    // Delete from DB
    sqlx::query("DELETE FROM flight_shares WHERE id = $1")
        .bind(&share_id)
        .execute(&state.db)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_flight_share_session_handler(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;

    let r2_key: Option<String> = sqlx::query_scalar(
        "SELECT r2_key FROM flight_shares WHERE id = $1 AND user_id = $2"
    )
    .bind(&share_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;

    let r2_key = r2_key.ok_or_else(|| AppError::NotFound("Share not found".to_string()))?;

    state.r2.delete_object(&r2_key).await
        .map_err(AppError::Storage)?;

    sqlx::query("DELETE FROM flight_shares WHERE id = $1")
        .bind(&share_id)
        .execute(&state.db)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_flight_share_json_handler(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let r2_key: Option<String> = sqlx::query_scalar(
        "SELECT r2_key FROM flight_shares WHERE id = $1"
    )
    .bind(&share_id)
    .fetch_optional(&state.db)
    .await?;

    let key = r2_key.ok_or_else(|| AppError::NotFound("Share not found".to_string()))?;

    let compressed = state.r2.download_object(&key)
        .await
        .map_err(AppError::Storage)?;

    let mut decoder = GzDecoder::new(compressed.as_slice());
    let mut json_str = String::new();
    decoder.read_to_string(&mut json_str)
        .map_err(|e| AppError::Storage(format!("Decompression failed: {}", e)))?;

    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "application/json"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=86400"),
            (axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        json_str,
    ))
}

#[derive(Deserialize)]
pub struct AllowlistChannelRequest {
    #[serde(rename = "channelId", alias = "channel_id")]
    pub channel_id: String,
    #[serde(rename = "guildId", alias = "guild_id")]
    pub guild_id: String,
    #[serde(rename = "channelName", alias = "channel_name")]
    pub channel_name: String,
}

pub async fn add_allowlist_channel_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AllowlistChannelRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;

    let user_discord_id: String = sqlx::query_scalar("SELECT discord_id FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;

    let user_discord_id_u64 = user_discord_id.parse::<u64>()
        .map_err(|_| AppError::BadRequest("Invalid user Discord ID".to_string()))?;

    let guild_id_u64 = payload.guild_id.parse::<u64>()
        .map_err(|_| AppError::BadRequest("Invalid guild ID".to_string()))?;

    let is_admin = crate::discord::is_user_admin_in_guild(&state.discord_http, guild_id_u64, user_discord_id_u64).await;
    if !is_admin {
        return Err(AppError::Forbidden("You are not an administrator of this server".to_string()));
    }

    // Verify channel with serenity
    let channel_id_u64 = payload.channel_id.parse::<u64>()
        .map_err(|_| AppError::BadRequest("Invalid channel ID format. Must be numeric.".to_string()))?;

    crate::discord::validate_discord_channel(&state.discord_http, channel_id_u64).await
        .map_err(AppError::BadRequest)?;

    // Insert into allowlisted_channels
    sqlx::query(
        "INSERT INTO allowlisted_channels (channel_id, channel_name, guild_id) VALUES ($1, $2, $3) \
         ON CONFLICT (channel_id) DO UPDATE SET channel_name = EXCLUDED.channel_name"
    )
    .bind(&payload.channel_id)
    .bind(&payload.channel_name)
    .bind(&payload.guild_id)
    .execute(&state.db)
    .await?;

    Ok(StatusCode::CREATED)
}

pub async fn delete_allowlist_channel_handler(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;

    let user_discord_id: String = sqlx::query_scalar("SELECT discord_id FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;

    let user_discord_id_u64 = user_discord_id.parse::<u64>()
        .map_err(|_| AppError::BadRequest("Invalid user Discord ID".to_string()))?;

    // Fetch the guild_id of the channel first
    let guild_id: Option<String> = sqlx::query_scalar("SELECT guild_id FROM allowlisted_channels WHERE channel_id = $1")
        .bind(&channel_id)
        .fetch_optional(&state.db)
        .await?;

    let guild_id_str = match guild_id {
        Some(g) => g,
        None => return Ok(StatusCode::NO_CONTENT), // Already deleted or not found
    };

    let guild_id_u64 = guild_id_str.parse::<u64>()
        .map_err(|_| AppError::BadRequest("Invalid guild ID format in database".to_string()))?;

    let is_admin = crate::discord::is_user_admin_in_guild(&state.discord_http, guild_id_u64, user_discord_id_u64).await;
    if !is_admin {
        return Err(AppError::Forbidden("You are not an administrator of this server".to_string()));
    }

    // Delete from allowlisted_channels
    sqlx::query("DELETE FROM allowlisted_channels WHERE channel_id = $1")
        .bind(&channel_id)
        .execute(&state.db)
        .await?;

    // Also clean up any user-registered notification channels mapped to this channel ID
    sqlx::query("DELETE FROM discord_notification_channels WHERE channel_id = $1")
        .bind(&channel_id)
        .execute(&state.db)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct MultiplayerPingRequest {
    pub udp_address: Option<String>,
}

#[derive(Serialize)]
pub struct MultiplayerPingResponse {
    pub peers: Vec<String>,
}

pub async fn multiplayer_ping_handler(
    State(state): State<AppState>,
    Path(webhook_token): Path<String>,
    Json(payload): Json<MultiplayerPingRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    let has_udp = payload.udp_address.as_ref().map(|a| !a.trim().is_empty()).unwrap_or(false);

    let peers = update_and_get_peers(
        &state,
        user_id,
        Some(has_udp),
        payload.udp_address,
    ).unwrap_or_default();

    Ok((StatusCode::OK, Json(MultiplayerPingResponse { peers })))
}
