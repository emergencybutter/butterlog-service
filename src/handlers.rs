use axum::{
    extract::{Path, State, Multipart},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use crate::AppState;
use sha2::{Digest, Sha256};
use std::io::Cursor;

#[derive(Deserialize)]
pub struct CreateFlightRequest {
    pub departure: String,
    pub statistics: serde_json::Value,
}

#[derive(Deserialize)]
pub struct UpdateFlightRequest {
    pub arrival: Option<String>,
    pub statistics: serde_json::Value,
}

#[derive(Serialize)]
pub struct FlightResponse {
    pub id: i64,
    pub user_id: i64,
    pub departure: String,
    pub arrival: Option<String>,
    pub statistics: serde_json::Value,
    pub screenshots: Vec<String>,
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
    };

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

    let response = FlightResponse {
        id: flight_id,
        user_id,
        departure,
        arrival,
        statistics,
        screenshots,
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

    // Calculate SHA-256 hash of the upload
    let mut hasher = Sha256::new();
    hasher.update(&raw_bytes);
    let hash = format!("{:x}", hasher.finalize());

    // Process image: resize to 1600px width if larger, compress to JPEG
    let img = image::load_from_memory(&raw_bytes).map_err(|e| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": format!("Invalid image format: {}", e) })))
    })?;

    let resized_img = if img.width() > 1600 {
        let aspect_ratio = img.height() as f32 / img.width() as f32;
        let new_height = (1600.0 * aspect_ratio) as u32;
        img.resize_exact(1600, new_height, image::imageops::FilterType::Triangle)
    } else {
        img
    };

    // Encode as optimized JPEG
    let mut jpeg_bytes = Vec::new();
    let mut cursor = Cursor::new(&mut jpeg_bytes);
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, 80);
    encoder.encode_image(&resized_img).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": format!("Failed to compress image: {}", e) })))
    })?;

    // Upload to Cloudflare R2
    let key = format!("screenshots/{}/{}.jpg", flight_id, hash);
    let url = state.r2.upload_object(&key, jpeg_bytes, "image/jpeg").await.map_err(|e| {
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

    Ok((StatusCode::CREATED, Json(serde_json::json!({ "hash": hash }))))
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

    // Delete from R2
    let key = format!("screenshots/{}/{}.jpg", flight_id, hash);
    let _ = state.r2.delete_object(&key).await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e })))
    })?;

    // Delete from DB
    sqlx::query("DELETE FROM screenshots WHERE flight_id = $1 AND hash = $2")
        .bind(flight_id)
        .bind(&hash)
        .execute(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))))?;

    Ok(StatusCode::NO_CONTENT)
}
