use axum::{
    extract::{Path, State, Multipart},
    http::{StatusCode, HeaderMap},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use crate::AppState;
use sha2::{Digest, Sha256};
use flate2::read::GzDecoder;
use std::io::Read;

#[derive(Deserialize)]
pub struct CreateFlightRequest {
    pub departure: String,
    pub statistics: serde_json::Value,
    pub multiplayer_enabled: Option<bool>,
    pub udp_address: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateFlightRequest {
    pub arrival: Option<String>,
    pub statistics: serde_json::Value,
    pub multiplayer_enabled: Option<bool>,
    pub udp_address: Option<String>,
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
) -> Result<i64, (StatusCode, Json<serde_json::Value>)> {
    let user_id: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE api_token = $1")
        .bind(webhook_token)
        .fetch_optional(db_pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    match user_id {
        Some(id) => Ok(id),
        None => Err((StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Invalid webhook token" })))),
    }
}

pub async fn get_user_id_from_session(
    db_pool: &PgPool,
    headers: &HeaderMap,
) -> Result<i64, (StatusCode, Json<serde_json::Value>)> {
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

    let token_str = match token {
        Some(t) => t,
        None => return Err((StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Unauthorized" })))),
    };

    let user_id: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE api_token = $1")
        .bind(&token_str)
        .fetch_optional(db_pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    match user_id {
        Some(id) => Ok(id),
        None => Err((StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Invalid session token" })))),
    }
}

pub async fn get_discord_channels_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;

    let channels: Vec<String> = sqlx::query_scalar(
        "SELECT channel_id FROM discord_notification_channels WHERE user_id = $1 ORDER BY id"
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    Ok((StatusCode::OK, Json(channels)))
}

pub async fn add_discord_channel_handler(
    State(_state): State<AppState>,
    _headers: HeaderMap,
    Json(_payload): Json<AddChannelRequest>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    Err((
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": "Direct mutation of notification channels is disabled. They are automatically managed."
        }))
    ))
}

pub async fn delete_discord_channel_handler(
    State(_state): State<AppState>,
    Path(_channel_id): Path<String>,
    _headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    Err((
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": "Direct mutation of notification channels is disabled. They are automatically managed."
        }))
    ))
}

pub async fn create_flight_handler(
    State(state): State<AppState>,
    Path(webhook_token): Path<String>,
    Json(payload): Json<CreateFlightRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    let row: (i64, String, Option<String>, serde_json::Value) = sqlx::query_as(
        "INSERT INTO flights (user_id, departure, statistics) VALUES ($1, $2, $3) RETURNING id, departure, arrival, statistics"
    )
    .bind(user_id)
    .bind(&payload.departure)
    .bind(&payload.statistics)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    let response = FlightResponse {
        id: row.0,
        user_id,
        departure: row.1,
        arrival: row.2,
        statistics: row.3,
        screenshots: Vec::new(),
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
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    // Check if the flight exists and belongs to the user
    let flight_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM flights WHERE id = $1 AND user_id = $2)"
    )
    .bind(flight_id)
    .bind(user_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    if !flight_exists {
        return Err((StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Flight not found" }))));
    }

    // Perform update
    let (departure, arrival, statistics): (String, Option<String>, serde_json::Value) = if let Some(ref arr) = payload.arrival {
        sqlx::query_as(
            "UPDATE flights SET arrival = $1, statistics = $2, updated_at = CURRENT_TIMESTAMP WHERE id = $3 RETURNING departure, arrival, statistics"
        )
        .bind(arr)
        .bind(&payload.statistics)
        .bind(flight_id)
        .fetch_one(&state.db)
        .await
    } else {
        sqlx::query_as(
            "UPDATE flights SET statistics = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2 RETURNING departure, arrival, statistics"
        )
        .bind(&payload.statistics)
        .bind(flight_id)
        .fetch_one(&state.db)
        .await
    }
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    // Fetch screenshots for response
    let screenshots = sqlx::query_scalar::<_, String>(
        "SELECT hash FROM screenshots WHERE flight_id = $1 ORDER BY id"
    )
    .bind(flight_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

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
        peers: update_and_get_peers(&state, user_id, payload.multiplayer_enabled, payload.udp_address),
    };

    Ok((StatusCode::OK, Json(response)))
}

pub async fn get_flight_handler(
    State(state): State<AppState>,
    Path((webhook_token, flight_id)): Path<(String, i64)>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    let flight_row: Option<(String, Option<String>, serde_json::Value)> = sqlx::query_as(
        "SELECT departure, arrival, statistics FROM flights WHERE id = $1 AND user_id = $2"
    )
    .bind(flight_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    let (departure, arrival, statistics) = match flight_row {
        Some(row) => row,
        None => return Err((StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Flight not found" })))),
    };

    let screenshots = sqlx::query_scalar::<_, String>(
        "SELECT hash FROM screenshots WHERE flight_id = $1 ORDER BY id"
    )
    .bind(flight_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    let response = FlightResponse {
        id: flight_id,
        user_id,
        departure,
        arrival,
        statistics,
        screenshots,
        peers: None,
    };

    Ok((StatusCode::OK, Json(response)))
}

pub async fn upload_screenshot_handler(
    State(state): State<AppState>,
    Path((webhook_token, flight_id)): Path<(String, i64)>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    // Check if the flight exists and belongs to the user
    let flight_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM flights WHERE id = $1 AND user_id = $2)"
    )
    .bind(flight_id)
    .bind(user_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    if !flight_exists {
        return Err((StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Flight not found" }))));
    }

    // Extract image field
    let mut field_bytes = None;
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": format!("Failed to read multipart field: {}", e) })))
    })? {
        if field.name() == Some("screenshot") {
            let bytes = field.bytes().await.map_err(|e| {
                (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": format!("Failed to read field bytes: {}", e) })))
            })?;
            field_bytes = Some(bytes.to_vec());
            break;
        }
    }

    let raw_bytes = match field_bytes {
        Some(b) => b,
        None => return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "No screenshot field found" })))),
    };

    // Refuse upload if file is larger than 100MB
    if raw_bytes.len() > 100 * 1024 * 1024 {
        return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "File size exceeds 100MB limit" }))));
    }

    // Verify format is WebP
    let format = image::guess_format(&raw_bytes).map_err(|e| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": format!("Could not identify image format: {}", e) })))
    })?;
    if format != image::ImageFormat::WebP {
        return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Only WebP images are allowed" }))));
    }

    // Load and verify dimensions (max width and height of 1600px)
    let img = image::load_from_memory(&raw_bytes).map_err(|e| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": format!("Invalid image data: {}", e) })))
    })?;
    if img.width() > 1600 || img.height() > 1600 {
        return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Image dimensions exceed 1600px limit" }))));
    }

    // Calculate SHA-256 hash of the upload
    let mut hasher = Sha256::new();
    hasher.update(&raw_bytes);
    let hash = format!("{:x}", hasher.finalize());

    // Upload to Cloudflare R2
    let key = format!("screenshots/{}/{}.webp", flight_id, hash);
    let url = state.r2.upload_object(&key, raw_bytes, "image/webp").await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e })))
    })?;

    // Save/update to DB (avoid duplicate check via UNIQUE constraint)
    sqlx::query(
        "INSERT INTO screenshots (flight_id, hash, url) VALUES ($1, $2, $3) ON CONFLICT (flight_id, hash) DO NOTHING"
    )
    .bind(flight_id)
    .bind(&hash)
    .bind(&url)
    .execute(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

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
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    // Verify ownership of the flight
    let flight_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM flights WHERE id = $1 AND user_id = $2)"
    )
    .bind(flight_id)
    .bind(user_id)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    if !flight_exists {
        return Err((StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Flight not found" }))));
    }

    // Query URL from DB to determine file extension for R2 key
    let url: Option<String> = sqlx::query_scalar("SELECT url FROM screenshots WHERE flight_id = $1 AND hash = $2")
        .bind(flight_id)
        .bind(&hash)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    // Delete from DB first
    sqlx::query("DELETE FROM screenshots WHERE flight_id = $1 AND hash = $2")
        .bind(flight_id)
        .bind(&hash)
        .execute(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    // Check if any other flight references this hash
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM screenshots WHERE hash = $1")
        .bind(&hash)
        .fetch_one(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

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
        let _ = state.r2.delete_object(&key).await.map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e })))
        })?;
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
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    if body.is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Empty body" }))));
    }

    // Decompress only to validate and extract remote_flight_id for the DB record
    let mut decoder = GzDecoder::new(body.as_ref());
    let mut json_str = String::new();
    decoder.read_to_string(&mut json_str)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": format!("Failed to decompress: {}", e) }))))?;

    let share: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": format!("Invalid JSON: {}", e) }))))?;

    let remote_flight_id = share.get("remoteFlightId").and_then(|v| v.as_i64())
        .or_else(|| share.get("remote_flight_id").and_then(|v| v.as_i64()));

    // Store the original compressed bytes as-is
    let share_id = uuid::Uuid::new_v4().to_string();
    let r2_key = format!("shares/{}.json.gz", share_id);
    state.r2.upload_object(&r2_key, body.to_vec(), "application/octet-stream")
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))))?;

    sqlx::query(
        "INSERT INTO flight_shares (id, user_id, remote_flight_id, r2_key) VALUES ($1, $2, $3, $4)"
    )
    .bind(&share_id)
    .bind(user_id)
    .bind(remote_flight_id)
    .bind(&r2_key)
    .execute(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    let share_url = format!("https://butterlog.flyvoyager.net/content/flights/share/{}", share_id);
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "url": share_url, "id": share_id }))))
}

pub async fn delete_flight_share_handler(
    State(state): State<AppState>,
    Path((webhook_token, share_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT fs.r2_key, fs.remote_flight_id FROM flight_shares fs \
         WHERE fs.id = $1 AND fs.user_id = $2"
    )
    .bind(&share_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    let (r2_key, _) = row.ok_or_else(|| (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Share not found" }))))?;

    // Delete from R2
    state.r2.delete_object(&r2_key).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))))?;

    // Delete from DB
    sqlx::query("DELETE FROM flight_shares WHERE id = $1")
        .bind(&share_id)
        .execute(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_flight_share_json_handler(
    State(state): State<AppState>,
    Path(share_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let r2_key: Option<String> = sqlx::query_scalar(
        "SELECT r2_key FROM flight_shares WHERE id = $1"
    )
    .bind(&share_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    let key = r2_key.ok_or_else(|| (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Share not found" }))))?;

    let compressed = state.r2.download_object(&key)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))))?;

    let mut decoder = GzDecoder::new(compressed.as_slice());
    let mut json_str = String::new();
    decoder.read_to_string(&mut json_str)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("Decompression failed: {}", e) }))))?;

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
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;

    let user_discord_id: String = sqlx::query_scalar("SELECT discord_id FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    let user_discord_id_u64 = user_discord_id.parse::<u64>().map_err(|_| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "Invalid user Discord ID" })))
    })?;

    let guild_id_u64 = payload.guild_id.parse::<u64>().map_err(|_| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Invalid guild ID" })))
    })?;

    let is_admin = crate::discord::is_user_admin_in_guild(&state.discord_http, guild_id_u64, user_discord_id_u64).await;
    if !is_admin {
        return Err((StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": "You are not an administrator of this server" }))));
    }

    // Verify channel with serenity
    let channel_id_u64 = payload.channel_id.parse::<u64>().map_err(|_| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "Invalid channel ID format. Must be numeric." })))
    })?;

    crate::discord::validate_discord_channel(&state.discord_http, channel_id_u64).await
        .map_err(|err| (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": err }))))?;

    // Insert into allowlisted_channels
    sqlx::query(
        "INSERT INTO allowlisted_channels (channel_id, channel_name, guild_id) VALUES ($1, $2, $3) \
         ON CONFLICT (channel_id) DO UPDATE SET channel_name = EXCLUDED.channel_name"
    )
    .bind(&payload.channel_id)
    .bind(&payload.channel_name)
    .bind(&payload.guild_id)
    .execute(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    Ok(StatusCode::CREATED)
}

pub async fn delete_allowlist_channel_handler(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;

    let user_discord_id: String = sqlx::query_scalar("SELECT discord_id FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    let user_discord_id_u64 = user_discord_id.parse::<u64>().map_err(|_| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "Invalid user Discord ID" })))
    })?;

    // Fetch the guild_id of the channel first
    let guild_id: Option<String> = sqlx::query_scalar("SELECT guild_id FROM allowlisted_channels WHERE channel_id = $1")
        .bind(&channel_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    let guild_id_str = match guild_id {
        Some(g) => g,
        None => return Ok(StatusCode::NO_CONTENT), // Already deleted or not found
    };

    let guild_id_u64 = guild_id_str.parse::<u64>().map_err(|_| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "Invalid guild ID format in database" })))
    })?;

    let is_admin = crate::discord::is_user_admin_in_guild(&state.discord_http, guild_id_u64, user_discord_id_u64).await;
    if !is_admin {
        return Err((StatusCode::FORBIDDEN, Json(serde_json::json!({ "error": "You are not an administrator of this server" }))));
    }

    // Delete from allowlisted_channels
    sqlx::query("DELETE FROM allowlisted_channels WHERE channel_id = $1")
        .bind(&channel_id)
        .execute(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    // Also clean up any user-registered notification channels mapped to this channel ID
    sqlx::query("DELETE FROM discord_notification_channels WHERE channel_id = $1")
        .bind(&channel_id)
        .execute(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

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
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let user_id = authenticate_user(&state.db, &webhook_token).await?;

    let has_udp = payload.udp_address.is_some() && !payload.udp_address.as_ref().unwrap().trim().is_empty();

    let peers = update_and_get_peers(
        &state,
        user_id,
        Some(has_udp),
        payload.udp_address,
    ).unwrap_or_default();

    Ok((StatusCode::OK, Json(MultiplayerPingResponse { peers })))
}
