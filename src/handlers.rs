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

/// Maximum decompressed size of a flight-share JSON document. Caps gzip
/// expansion so a small malicious upload cannot decompress into gigabytes.
pub const MAX_SHARE_DECOMPRESSED: u64 = 32 * 1024 * 1024;

/// Maximum accepted screenshot upload size (pre-decode, on the wire).
pub const MAX_SCREENSHOT_UPLOAD: usize = 15 * 1024 * 1024;

/// Decompress a gzip payload, refusing anything that expands past `max` bytes.
pub fn decompress_gzip_capped(data: &[u8], max: u64) -> Result<String, String> {
    let decoder = GzDecoder::new(data);
    let mut out = String::new();
    decoder
        .take(max + 1)
        .read_to_string(&mut out)
        .map_err(|e| format!("Failed to decompress: {}", e))?;
    if out.len() as u64 > max {
        return Err(format!("Decompressed payload exceeds {} byte limit", max));
    }
    Ok(out)
}

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

/// Peer presence lives in the `multiplayer_peers` table (not process memory) so
/// it works when the service runs as multiple instances. A peer is considered
/// active when seen within the last 120 seconds.
async fn update_and_get_peers(
    state: &crate::AppState,
    user_id: i64,
    multiplayer_enabled: Option<bool>,
    udp_address: Option<String>,
    local_udp_address: Option<String>,
) -> Result<Option<Vec<PeerDetail>>, AppError> {
    // 1. Update or remove the current peer's presence row
    if multiplayer_enabled.unwrap_or(false) {
        if let Some(ref addr) = udp_address {
            if !addr.trim().is_empty() {
                // COALESCE keeps a previously published local address when a caller
                // (e.g. flight create/update) doesn't supply one.
                sqlx::query(
                    "INSERT INTO multiplayer_peers (user_id, udp_address, local_udp_address, last_seen) \
                     VALUES ($1, $2, $3, CURRENT_TIMESTAMP) \
                     ON CONFLICT (user_id) DO UPDATE SET \
                         udp_address = EXCLUDED.udp_address, \
                         local_udp_address = COALESCE(EXCLUDED.local_udp_address, multiplayer_peers.local_udp_address), \
                         last_seen = EXCLUDED.last_seen"
                )
                .bind(user_id)
                .bind(addr)
                .bind(local_udp_address.as_deref().filter(|a| !a.trim().is_empty()))
                .execute(&state.db)
                .await?;
            }
        }
    } else if let Some(false) = multiplayer_enabled {
        sqlx::query("DELETE FROM multiplayer_peers WHERE user_id = $1")
            .bind(user_id)
            .execute(&state.db)
            .await?;
        return Ok(None);
    }

    if !multiplayer_enabled.unwrap_or(false) {
        return Ok(None);
    }

    // 2. Prune stale peers, then collect the other active ones
    sqlx::query("DELETE FROM multiplayer_peers WHERE last_seen < CURRENT_TIMESTAMP - INTERVAL '120 seconds'")
        .execute(&state.db)
        .await?;

    // Join the owning user so the client can label each peer by name (the debug
    // window shows usernames rather than raw UDP addresses). Prefer the Discord
    // global_name, falling back to username.
    let other_peers: Vec<PeerDetail> = sqlx::query_as::<_, (String, Option<String>, String)>(
        "SELECT mp.udp_address, mp.local_udp_address, COALESCE(NULLIF(u.global_name, ''), u.username) \
         FROM multiplayer_peers mp \
         JOIN users u ON u.id = mp.user_id \
         WHERE mp.user_id <> $1"
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|(udp_address, local_udp_address, username)| PeerDetail { udp_address, local_udp_address, username })
    .collect();

    Ok(Some(other_peers))
}

/// Fire-and-forget Discord synchronization for a flight.
fn spawn_discord_sync(state: &crate::AppState, flight_id: i64) {
    let db = state.db.clone();
    let r2 = state.r2.clone();
    let http = state.discord_http.clone();
    let base_url = state.config.public_base_url.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::discord::sync_flight_discord(&db, &r2, &http, flight_id, &base_url).await {
            tracing::error!("Discord sync failed for flight {}: {:?}", flight_id, e);
        }
    });
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct AddChannelRequest {
    #[serde(rename = "channelId", alias = "channel_id")]
    pub channel_id: String,
}

/// Resolve a raw API token to a user id. Tokens are stored hashed; the lookup
/// also stamps `last_used_at` so idle tokens can be pruned.
async fn authenticate_user(
    db_pool: &PgPool,
    raw_token: &str,
) -> Result<i64, AppError> {
    let user_id: Option<i64> = sqlx::query_scalar(
        "UPDATE api_tokens SET last_used_at = NOW() WHERE token_hash = $1 RETURNING user_id"
    )
    .bind(crate::auth::hash_token(raw_token))
    .fetch_optional(db_pool)
    .await?;

    user_id.ok_or_else(|| AppError::Auth("Invalid token".to_string()))
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

    authenticate_user(db_pool, &token_str).await
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

/// Legacy route with the token embedded in the path. Prefer the header-
/// authenticated `/api/v0/flights` routes; these remain for old clients.
pub async fn create_flight_handler(
    State(state): State<AppState>,
    Path(webhook_token): Path<String>,
    Json(payload): Json<CreateFlightRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;
    create_flight_core(state, user_id, payload).await
}

pub async fn create_flight_bearer_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateFlightRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    create_flight_core(state, user_id, payload).await
}

async fn create_flight_core(
    state: AppState,
    user_id: i64,
    payload: CreateFlightRequest,
) -> Result<(StatusCode, Json<FlightResponse>), AppError> {
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
        peers: update_and_get_peers(&state, user_id, payload.multiplayer_enabled, payload.udp_address, None)
            .await?
            .map(|peers| peers.into_iter().map(|p| p.udp_address).collect()),
    };

    // Trigger Discord Sync in background
    spawn_discord_sync(&state, response.id);

    Ok((StatusCode::CREATED, Json(response)))
}

pub async fn update_flight_handler(
    State(state): State<AppState>,
    Path((webhook_token, flight_id)): Path<(String, i64)>,
    Json(payload): Json<UpdateFlightRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;
    update_flight_core(state, user_id, flight_id, payload).await
}

pub async fn update_flight_bearer_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    headers: HeaderMap,
    Json(payload): Json<UpdateFlightRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    update_flight_core(state, user_id, flight_id, payload).await
}

async fn update_flight_core(
    state: AppState,
    user_id: i64,
    flight_id: i64,
    payload: UpdateFlightRequest,
) -> Result<(StatusCode, Json<FlightResponse>), AppError> {
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
    spawn_discord_sync(&state, flight_id);

    let response = FlightResponse {
        id: flight_id,
        user_id,
        departure,
        arrival,
        statistics,
        screenshots,
        notes,
        peers: update_and_get_peers(&state, user_id, payload.multiplayer_enabled, payload.udp_address, None)
            .await?
            .map(|peers| peers.into_iter().map(|p| p.udp_address).collect()),
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
    update_flight_notes_core(state, user_id, flight_id, payload).await
}

pub async fn update_flight_notes_bearer_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    headers: HeaderMap,
    Json(payload): Json<UpdateNotesRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    update_flight_notes_core(state, user_id, flight_id, payload).await
}

async fn update_flight_notes_core(
    state: AppState,
    user_id: i64,
    flight_id: i64,
    payload: UpdateNotesRequest,
) -> Result<StatusCode, AppError> {
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
    get_flight_core(state, user_id, flight_id).await
}

pub async fn get_flight_bearer_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    get_flight_core(state, user_id, flight_id).await
}

async fn get_flight_core(
    state: AppState,
    user_id: i64,
    flight_id: i64,
) -> Result<(StatusCode, Json<FlightResponse>), AppError> {
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
    multipart: Multipart,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;
    upload_screenshot_core(state, user_id, flight_id, multipart).await
}

pub async fn upload_screenshot_bearer_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    upload_screenshot_core(state, user_id, flight_id, multipart).await
}

async fn upload_screenshot_core(
    state: AppState,
    user_id: i64,
    flight_id: i64,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
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

    // The on-the-wire size is enforced by the per-route DefaultBodyLimit
    // (MAX_SCREENSHOT_UPLOAD) configured in main.rs.

    // Verify format is WebP
    let format = image::guess_format(&raw_bytes).map_err(|e| {
        AppError::BadRequest(format!("Could not identify image format: {}", e))
    })?;
    if format != image::ImageFormat::WebP {
        return Err(AppError::BadRequest("Only WebP images are allowed".to_string()));
    }

    // Decode with dimension/memory limits enforced *during* decode so an
    // oversized or decompression-bomb image fails before allocating.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(1600);
    limits.max_image_height = Some(1600);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(&raw_bytes));
    reader.set_format(image::ImageFormat::WebP);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| {
        AppError::BadRequest(format!("Invalid image data (must be WebP, max 1600px): {}", e))
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
    spawn_discord_sync(&state, flight_id);

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "hash": hash, "url": url }))))
}

pub async fn delete_screenshot_handler(
    State(state): State<AppState>,
    Path((webhook_token, flight_id, hash)): Path<(String, i64, String)>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;
    delete_screenshot_core(state, user_id, flight_id, hash).await
}

pub async fn delete_screenshot_bearer_handler(
    State(state): State<AppState>,
    Path((flight_id, hash)): Path<(i64, String)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    delete_screenshot_core(state, user_id, flight_id, hash).await
}

async fn delete_screenshot_core(
    state: AppState,
    user_id: i64,
    flight_id: i64,
    hash: String,
) -> Result<StatusCode, AppError> {
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

    // R2 keys are namespaced per flight (screenshots/{flight_id}/{hash}), so
    // this flight's object is never shared with other flights — always delete it.
    let key = match url {
        Some(url_str) if !url_str.ends_with(".webp") => {
            format!("screenshots/{}/{}.jpg", flight_id, hash)
        }
        _ => format!("screenshots/{}/{}.webp", flight_id, hash),
    };
    state.r2.delete_object(&key).await
        .map_err(AppError::Storage)?;

    // Trigger Discord Sync in background
    spawn_discord_sync(&state, flight_id);

    Ok(StatusCode::NO_CONTENT)
}

pub async fn upload_flight_share_handler(
    State(state): State<AppState>,
    Path(webhook_token): Path<String>,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;
    upload_flight_share_core(state, user_id, body).await
}

pub async fn upload_flight_share_bearer_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    upload_flight_share_core(state, user_id, body).await
}

async fn upload_flight_share_core(
    state: AppState,
    user_id: i64,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    if body.is_empty() {
        return Err(AppError::BadRequest("Empty body".to_string()));
    }

    // Decompress only to validate and extract remote_flight_id for the DB record.
    // Capped to guard against gzip decompression bombs.
    let json_str = decompress_gzip_capped(body.as_ref(), MAX_SHARE_DECOMPRESSED)
        .map_err(AppError::BadRequest)?;

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
        spawn_discord_sync(&state, flight_id);
    }

    let share_url = format!("{}/content/flights/share/{}", state.config.public_base_url, share_id);
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

    let json_str = decompress_gzip_capped(compressed.as_slice(), MAX_SHARE_DECOMPRESSED)
        .map_err(AppError::Storage)?;

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
    /// LAN address for peers behind the same NAT (optional; older clients omit it).
    #[serde(default)]
    pub local_udp_address: Option<String>,
}

/// A peer's reachable address(es) plus the name to label it with in the UI.
#[derive(Serialize)]
pub struct PeerDetail {
    pub udp_address: String,
    /// LAN address, used by clients behind the same NAT (may be null).
    pub local_udp_address: Option<String>,
    pub username: String,
}

#[derive(Serialize)]
pub struct MultiplayerPingResponse {
    /// Bare addresses, kept for older clients (e.g. the traffic simulator).
    pub peers: Vec<String>,
    /// Address + username pairs for clients that label peers by name.
    pub peer_details: Vec<PeerDetail>,
}

pub async fn multiplayer_ping_handler(
    State(state): State<AppState>,
    Path(webhook_token): Path<String>,
    Json(payload): Json<MultiplayerPingRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;
    multiplayer_ping_core(state, user_id, payload).await
}

pub async fn multiplayer_ping_bearer_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<MultiplayerPingRequest>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    multiplayer_ping_core(state, user_id, payload).await
}

async fn multiplayer_ping_core(
    state: AppState,
    user_id: i64,
    payload: MultiplayerPingRequest,
) -> Result<(StatusCode, Json<MultiplayerPingResponse>), AppError> {
    let has_udp = payload.udp_address.as_ref().map(|a| !a.trim().is_empty()).unwrap_or(false);

    let peer_details = update_and_get_peers(
        &state,
        user_id,
        Some(has_udp),
        payload.udp_address,
        payload.local_udp_address,
    )
    .await?
    .unwrap_or_default();

    let peers = peer_details.iter().map(|p| p.udp_address.clone()).collect();

    Ok((StatusCode::OK, Json(MultiplayerPingResponse { peers, peer_details })))
}
