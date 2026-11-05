//! Screenshots uploaded against a telemetry session, which the service places (design
//! §5.5): the app knows when a screenshot was taken and in which session, not which flight
//! it is on, since the service makes the flights. Each is placed on the user's flight whose
//! time contains it, when uploaded if that flight is made already, and again each time a
//! flight is made or updated (`place`), so a flight analysed again keeps the right ones.
//! Its position is the track's at that time: the pages place it by `created_at`, which is
//! when it was taken.

use axum::{
    extract::{Multipart, Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::error::AppError;
use crate::handlers::{get_user_id_from_session, read_screenshot, spawn_discord_sync, ScreenshotUpload};
use crate::AppState;

/// `POST /api/v0/sessions/:id/screenshots`: a multipart form with the WebP `screenshot`
/// and `taken_at`, when it was taken (RFC 3339, UTC). The session must be the user's; a
/// screenshot already uploaded, by its hash, is answered as it was.
pub async fn upload_session_screenshot_handler(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    let mine: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM telemetry_sessions WHERE id = $1 AND user_id = $2)")
        .bind(session_id)
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;
    if !mine {
        return Err(AppError::NotFound("Session not found".to_string()));
    }
    let ScreenshotUpload { bytes, hash, fields } = read_screenshot(multipart).await?;
    let taken_at = fields
        .get("taken_at")
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc))
        .ok_or_else(|| AppError::BadRequest("taken_at must be an RFC 3339 time".to_string()))?;

    let existing: Option<(String, Option<i64>)> =
        sqlx::query_as("SELECT url, flight_id FROM screenshots WHERE user_id = $1 AND hash = $2 AND session_id IS NOT NULL")
            .bind(user_id)
            .bind(&hash)
            .fetch_optional(&state.db)
            .await?;
    if let Some((url, flight_id)) = existing {
        return Ok((StatusCode::OK, Json(serde_json::json!({ "hash": hash, "url": url, "flight_id": flight_id }))));
    }

    let key = crate::handlers::screenshot_key(None, Some(session_id), &hash, ".webp");
    let url = state.r2.upload_object(&key, bytes, "image/webp").await.map_err(AppError::Storage)?;
    let flight_id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO screenshots (flight_id, hash, url, created_at, session_id, user_id, taken_at) \
         VALUES (( \
             SELECT id FROM flights \
              WHERE user_id = $5 AND service_owned AND created_at <= $4 \
                AND COALESCE(ended_at, 'infinity') >= $4 \
              ORDER BY created_at DESC LIMIT 1 \
         ), $1, $2, $4, $3, $5, $4) \
         ON CONFLICT (user_id, hash) WHERE session_id IS NOT NULL DO NOTHING \
         RETURNING flight_id",
    )
    .bind(&hash)
    .bind(&url)
    .bind(session_id)
    .bind(taken_at)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?
    .flatten();
    if let Some(flight_id) = flight_id {
        spawn_discord_sync(&state, flight_id);
    }
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "hash": hash, "url": url, "flight_id": flight_id }))))
}

/// Places the user's session screenshots taken from `from` to `to` (open-ended while the
/// flight flies) on `flight_id`, and takes off it those it had that no longer fall in it.
/// Returns whether its screenshots changed.
pub async fn place(
    tx: &mut sqlx::PgConnection,
    user_id: i64,
    flight_id: i64,
    from: DateTime<Utc>,
    to: Option<DateTime<Utc>>,
) -> Result<bool, sqlx::Error> {
    let moved = sqlx::query(
        "UPDATE screenshots \
            SET flight_id = CASE WHEN taken_at >= $3 AND taken_at <= COALESCE($4, 'infinity') THEN $2 END \
          WHERE user_id = $1 AND session_id IS NOT NULL \
            AND (flight_id = $2 OR (taken_at >= $3 AND taken_at <= COALESCE($4, 'infinity'))) \
            AND flight_id IS DISTINCT FROM CASE WHEN taken_at >= $3 AND taken_at <= COALESCE($4, 'infinity') THEN $2 END",
    )
    .bind(user_id)
    .bind(flight_id)
    .bind(from)
    .bind(to)
    .execute(&mut *tx)
    .await?;
    Ok(moved.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use crate::handlers::screenshot_key;
    use uuid::Uuid;

    /// The Discord sync, deletes and the anonymous sweep find a screenshot where its upload
    /// put it: under its session when it came with one, even once placed on a flight.
    #[test]
    fn a_session_screenshot_is_found_under_its_session() {
        let session = Uuid::nil();
        assert_eq!(
            screenshot_key(Some(564), Some(session), "h", "https://x/h.webp"),
            format!("screenshots/sessions/{session}/h.webp")
        );
        assert_eq!(screenshot_key(Some(564), None, "h", "https://x/h.webp"), "screenshots/564/h.webp");
        assert_eq!(screenshot_key(Some(7), None, "h", "https://x/h.jpg"), "screenshots/7/h.jpg");
    }
}
