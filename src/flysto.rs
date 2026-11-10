//! Uploading a flight to FlySto: the same Garmin CSV the Export button saves (`export.rs`),
//! zipped, as FlySto wants its logs, and posted to its public API
//! (www.flysto.net/public-api/swagger) with the pilot's own API token
//! (`users.flysto_token`), which they make at www.flysto.net/account/api and paste in
//! Settings. The token is checked against FlySto before it is kept, and never sent back
//! to the browser. Flights that end after the pilot connected go by themselves
//! ([`spawn_auto`]), and the Discord post links each once FlySto has processed it.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Write;

use crate::error::AppError;
use crate::AppState;

const API: &str = "https://www.flysto.net/public-api";

/// One file in a zip, deflated: what FlySto takes a log as.
pub fn zip_one(name: &str, data: &[u8]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file(name, options).expect("a file in a zip in memory");
    zip.write_all(data).expect("writing to memory");
    zip.finish().expect("a zip in memory").into_inner()
}

/// What FlySto's refusal says, short enough to show.
async fn refusal(r: reqwest::Response) -> String {
    let status = r.status();
    let body = r.text().await.unwrap_or_default();
    let body = body.trim();
    if body.is_empty() {
        format!("FlySto answered {status}")
    } else {
        format!("FlySto answered {status}: {}", body.chars().take(200).collect::<String>())
    }
}

async fn signed_in(state: &AppState, headers: &HeaderMap) -> Result<i64, AppError> {
    crate::handlers::get_user_id_from_session(&state.db, headers)
        .await
        .map_err(|_| AppError::Auth("Sign in first".to_string()))
}

/// Whether the pilot has connected FlySto, for the pages to offer the upload.
pub async fn connected(db: &sqlx::PgPool, user_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT flysto_token IS NOT NULL FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(db)
        .await
        .map(|c| c.unwrap_or(false))
}

#[derive(Deserialize)]
pub struct TokenBody {
    token: String,
}

/// `PUT /api/v0/me/flysto`: keeps the pilot's FlySto API token, once FlySto takes it.
pub async fn put_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TokenBody>,
) -> Result<Json<Value>, AppError> {
    let user_id = signed_in(&state, &headers).await?;
    let token = body.token.trim();
    if token.is_empty() || token.len() > 4096 || token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(AppError::BadRequest("That doesn't look like a FlySto API token".to_string()));
    }
    // Listing the pilot's newest log is the cheapest call the token must be good for.
    let r = state
        .http_client
        .get(format!("{API}/logs"))
        .query(&[("type", "all"), ("limit", "1")])
        .bearer_auth(token)
        .send()
        .await?;
    // 401 is a token FlySto doesn't know. 403 is one it knows but that may not list logs,
    // which is not what it is kept for: uploading is, so it is kept.
    match r.status().as_u16() {
        200..=299 => {}
        403 => tracing::warn!(user_id, "FlySto knows the token but won't list logs with it: kept for uploads"),
        401 => {
            tracing::info!(user_id, "FlySto refused a token: {}", refusal(r).await);
            return Err(AppError::BadRequest(
                "FlySto doesn't recognise this token: copy it again from your FlySto account's API page".to_string(),
            ));
        }
        _ => return Err(AppError::Upstream(refusal(r).await)),
    }
    // Flights ended from now go by themselves; a replaced token keeps the date.
    sqlx::query("UPDATE users SET flysto_token = $1, flysto_since = COALESCE(flysto_since, NOW()) WHERE id = $2")
        .bind(token)
        .bind(user_id)
        .execute(&state.db)
        .await?;
    Ok(Json(json!({ "connected": true })))
}

/// `DELETE /api/v0/me/flysto`: forgets the pilot's FlySto API token.
pub async fn delete_token(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Value>, AppError> {
    let user_id = signed_in(&state, &headers).await?;
    sqlx::query("UPDATE users SET flysto_token = NULL, flysto_since = NULL WHERE id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await?;
    Ok(Json(json!({ "connected": false })))
}

/// `POST /content/flights/:id/upload/flysto`: the flight's owner sends it to their
/// FlySto, as the CSV the Export button saves.
pub async fn upload_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user_id = signed_in(&state, &headers).await?;
    let token: Option<String> = sqlx::query_scalar("SELECT flysto_token FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;
    let Some(token) = token else {
        return Err(AppError::Conflict("Connect FlySto in Settings first".to_string()));
    };
    upload(&state, flight_id, user_id, &token).await.map(Json)
}

/// Sends the pilot's flight to their FlySto, keeps where it went, and says where it is
/// now, with whether FlySto already had it (`duplicate`).
async fn upload(state: &AppState, flight_id: i64, user_id: i64, token: &str) -> Result<Value, AppError> {
    let csv = crate::export::owned_csv(state, flight_id, Some(user_id)).await?;
    let upload_id = format!("butterlog-{flight_id}");
    let name = format!("{upload_id}.csv");
    let body = tokio::task::spawn_blocking(move || zip_one(&name, csv.as_bytes()))
        .await
        .map_err(|e| AppError::Storage(e.to_string()))?;
    let r = state
        .http_client
        .post(format!("{API}/log-upload"))
        // Names the upload in FlySto's own logs only.
        .query(&[("id", &upload_id)])
        .bearer_auth(token)
        .header(reqwest::header::CONTENT_TYPE, "application/zip")
        .timeout(std::time::Duration::from_secs(120))
        .body(body)
        .send()
        .await?;
    // Both answer `{"fileId": ...}`, as text: the new file's, or, for a log FlySto
    // already has (the same flight uploaded again), the existing one's.
    let status = r.status().as_u16();
    if status == 200 || status == 409 {
        let duplicate = status == 409;
        let Some(file_id) = file_id(&r.text().await.unwrap_or_default()) else {
            return Err(AppError::Upstream("FlySto took the flight but didn't say where it put it".to_string()));
        };
        tracing::info!(flight_id, user_id, duplicate, file_id, "uploaded to FlySto");
        // The same file again keeps what is known of it; another starts over.
        sqlx::query(
            "INSERT INTO flysto_uploads (flight_id, user_id, file_id) VALUES ($1, $2, $3) \
             ON CONFLICT (flight_id) DO UPDATE SET \
               user_id = EXCLUDED.user_id, file_id = EXCLUDED.file_id, uploaded_at = NOW(), \
               processed = flysto_uploads.processed AND flysto_uploads.file_id = EXCLUDED.file_id, \
               log_ids = CASE WHEN flysto_uploads.file_id = EXCLUDED.file_id THEN flysto_uploads.log_ids ELSE '{}' END",
        )
        .bind(flight_id)
        .bind(user_id)
        .bind(&file_id)
        .execute(&state.db)
        .await?;
        let mut answer = where_it_is(state, flight_id, token).await?;
        answer["duplicate"] = json!(duplicate);
        return Ok(answer);
    }
    match status {
        401 | 403 => Err(AppError::Upstream(
            "FlySto no longer accepts your API token: update it in Settings".to_string(),
        )),
        _ => Err(AppError::Upstream(refusal(r).await)),
    }
}

/// Where a flight is on FlySto, as the pages show it: `none` (never uploaded),
/// `processing`, `done` with its log's `url`, or `empty` when FlySto found no flight in it.
#[derive(Debug, Clone, PartialEq)]
pub enum Pointer {
    None,
    Processing,
    Done(String),
    Empty,
}

impl Pointer {
    fn of(processed: bool, log_ids: &[String]) -> Self {
        match (processed, log_ids.first()) {
            (false, _) => Pointer::Processing,
            (true, Some(id)) => Pointer::Done(log_url(id)),
            (true, None) => Pointer::Empty,
        }
    }

    pub fn state(&self) -> &'static str {
        match self {
            Pointer::None => "none",
            Pointer::Processing => "processing",
            Pointer::Done(_) => "done",
            Pointer::Empty => "empty",
        }
    }

    pub fn url(&self) -> &str {
        if let Pointer::Done(url) = self { url } else { "" }
    }

    fn json(&self) -> Value {
        json!({ "state": self.state(), "url": self.url() })
    }
}

/// A FlySto log's page.
fn log_url(id: &str) -> String {
    format!("https://www.flysto.net/logs/{}", id.trim())
}

/// What is known of the flight on FlySto, without asking it.
pub async fn pointer(db: &sqlx::PgPool, flight_id: i64) -> Result<Pointer, sqlx::Error> {
    let row: Option<(bool, Vec<String>)> =
        sqlx::query_as("SELECT processed, log_ids FROM flysto_uploads WHERE flight_id = $1")
            .bind(flight_id)
            .fetch_optional(db)
            .await?;
    Ok(row.map_or(Pointer::None, |(processed, logs)| Pointer::of(processed, &logs)))
}

/// Where the flight is on FlySto, asking FlySto while it is still processing the file
/// (`GET /log-files/{fileId}`: `{"processed": ..., "logs": [...]}`), and keeping its answer.
async fn where_it_is(state: &AppState, flight_id: i64, token: &str) -> Result<Value, AppError> {
    let row: Option<(String, bool, Vec<String>)> =
        sqlx::query_as("SELECT file_id, processed, log_ids FROM flysto_uploads WHERE flight_id = $1")
            .bind(flight_id)
            .fetch_optional(&state.db)
            .await?;
    let Some((file_id, processed, logs)) = row else { return Ok(Pointer::None.json()) };
    if processed {
        return Ok(Pointer::of(true, &logs).json());
    }
    let r = state
        .http_client
        .get(format!("{API}/log-files/{file_id}"))
        .bearer_auth(token)
        .send()
        .await?;
    match r.status().as_u16() {
        200 => {}
        // Deleted on FlySto: it is no longer there to point at.
        404 => {
            sqlx::query("DELETE FROM flysto_uploads WHERE flight_id = $1")
                .bind(flight_id)
                .execute(&state.db)
                .await?;
            return Ok(Pointer::None.json());
        }
        401 | 403 => {
            return Err(AppError::Upstream(
                "FlySto no longer accepts your API token: update it in Settings".to_string(),
            ));
        }
        _ => return Err(AppError::Upstream(refusal(r).await)),
    }
    let (processed, logs) = file_state(&r.text().await.unwrap_or_default());
    sqlx::query("UPDATE flysto_uploads SET processed = $2, log_ids = $3, checked_at = NOW() WHERE flight_id = $1")
        .bind(flight_id)
        .bind(processed)
        .bind(&logs)
        .execute(&state.db)
        .await?;
    // Its Discord post links it from now: synced with the next minute's (`sessions.rs`).
    if processed && !logs.is_empty() {
        sqlx::query("UPDATE flights SET discord_sync_due = TRUE WHERE id = $1")
            .bind(flight_id)
            .execute(&state.db)
            .await?;
    }
    Ok(Pointer::of(processed, &logs).json())
}

/// `GET /content/flights/:id/upload/flysto`: where the owner's flight is on FlySto, which
/// the page asks until FlySto has processed it.
pub async fn status_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let user_id = signed_in(&state, &headers).await?;
    let owner: Option<i64> = sqlx::query_scalar("SELECT user_id FROM flights WHERE id = $1")
        .bind(flight_id)
        .fetch_optional(&state.db)
        .await?;
    if owner != Some(user_id) {
        return Err(AppError::NotFound("Flight not found".to_string()));
    }
    let token: Option<String> = sqlx::query_scalar("SELECT flysto_token FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&state.db)
        .await?;
    match token {
        Some(token) => where_it_is(&state, flight_id, &token).await.map(Json),
        // Without a token, only what is already known.
        None => Ok(Json(pointer(&state.db, flight_id).await?.json())),
    }
}

/// How often ended flights go to FlySto, and FlySto is asked about those processing.
const AUTO_EVERY_SECS: u64 = 60;

/// Sends each ended flight of the pilots who connected FlySto, ended since they did, once
/// its telemetry is analysed (the export is made from it); then, every minute, asks FlySto
/// where those it is still processing went, which the Discord post then links.
pub fn spawn_auto(state: AppState) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(AUTO_EVERY_SECS));
        loop {
            ticker.tick().await;
            match upload_ended(&state, 5).await {
                Ok(n) if n > 0 => tracing::info!(count = n, "sent ended flights to FlySto"),
                Err(e) => tracing::error!("automatic FlySto uploads failed: {e}"),
                _ => {}
            }
            if let Err(e) = follow_processing(&state).await {
                tracing::error!("FlySto processing not followed: {e}");
            }
        }
    });
}

/// Sends up to `limit` ended flights waiting to go to FlySto. One FlySto refused is tried
/// again 10 minutes later, then 20, and so on, 5 times at most. Returns how many went.
async fn upload_ended(state: &AppState, limit: i64) -> Result<usize, AppError> {
    let due: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT f.id, f.user_id, u.flysto_token FROM flights f JOIN users u ON u.id = f.user_id \
          WHERE u.flysto_token IS NOT NULL AND f.status = 'ended' \
            AND f.ended_at >= u.flysto_since AND f.ended_at > NOW() - INTERVAL '2 days' \
            AND EXISTS (SELECT 1 FROM telemetry_flights t WHERE t.flight_id = f.id) \
            AND NOT EXISTS (SELECT 1 FROM flysto_uploads x WHERE x.flight_id = f.id) \
            AND NOT EXISTS (SELECT 1 FROM flysto_auto_attempts a WHERE a.flight_id = f.id \
                  AND (a.attempts >= 5 OR a.tried_at > NOW() - a.attempts * INTERVAL '10 minutes')) \
          ORDER BY f.ended_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    let mut sent = 0;
    for (flight_id, user_id, token) in due {
        match upload(state, flight_id, user_id, &token).await {
            Ok(_) => {
                sent += 1;
                sqlx::query("DELETE FROM flysto_auto_attempts WHERE flight_id = $1")
                    .bind(flight_id)
                    .execute(&state.db)
                    .await?;
            }
            Err(e) => {
                tracing::warn!(flight_id, user_id, "automatic FlySto upload failed: {e}");
                sqlx::query(
                    "INSERT INTO flysto_auto_attempts (flight_id, attempts, last_error) VALUES ($1, 1, $2) \
                     ON CONFLICT (flight_id) DO UPDATE SET \
                       attempts = flysto_auto_attempts.attempts + 1, last_error = $2, tried_at = NOW()",
                )
                .bind(flight_id)
                .bind(e.to_string())
                .execute(&state.db)
                .await?;
            }
        }
    }
    Ok(sent)
}

/// Asks FlySto where the files it is still processing went, those uploaded in the last
/// day: the button's page may be closed, and the Discord post waits for the link.
async fn follow_processing(state: &AppState) -> Result<(), AppError> {
    let processing: Vec<(i64, String)> = sqlx::query_as(
        "SELECT x.flight_id, u.flysto_token FROM flysto_uploads x JOIN users u ON u.id = x.user_id \
          WHERE NOT x.processed AND u.flysto_token IS NOT NULL AND x.uploaded_at > NOW() - INTERVAL '1 day'",
    )
    .fetch_all(&state.db)
    .await?;
    for (flight_id, token) in processing {
        if let Err(e) = where_it_is(state, flight_id, &token).await {
            tracing::warn!(flight_id, "FlySto processing not known: {e}");
        }
    }
    Ok(())
}

/// FlySto's state of an uploaded file, `{"processed": ..., "logs": [...]}` as text:
/// whether it is processed, and the logs it made.
fn file_state(body: &str) -> (bool, Vec<String>) {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let logs = v["logs"]
        .as_array()
        .map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    (v["processed"].as_bool().unwrap_or(false), logs)
}

/// The `fileId` of FlySto's answer to an upload, which it sends as text.
fn file_id(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    match &v["fileId"] {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn a_processed_file_points_at_its_log() {
        // As FlySto answered for the KAST circuit uploaded on 2026-11-10.
        assert_eq!(file_state(r#"{"processed":false}"#), (false, vec![]));
        let (processed, logs) = file_state(r#"{"processed":true,"logs":["y6wg6irz"]}"#);
        assert_eq!(Pointer::of(processed, &logs), Pointer::Done("https://www.flysto.net/logs/y6wg6irz".into()));
        assert_eq!(Pointer::of(false, &[]).state(), "processing");
        assert_eq!(Pointer::of(true, &[]).state(), "empty");
        assert_eq!(file_state("not json"), (false, vec![]));
    }

    #[test]
    fn the_uploaded_files_id_is_read_from_flystos_text() {
        assert_eq!(file_id(r#"{"fileId": "a1b2"}"#).as_deref(), Some("a1b2"));
        assert_eq!(file_id(r#"{"fileId": 42}"#).as_deref(), Some("42"));
        assert_eq!(file_id("not json"), None);
    }

    #[test]
    fn a_log_goes_to_flysto_as_the_one_file_of_a_zip() {
        let csv = "Lcl Date,Lcl Time\n2026-11-09,22:02:01\n".repeat(50);
        let zipped = zip_one("butterlog-598.csv", csv.as_bytes());
        assert!(zipped.starts_with(b"PK\x03\x04"));
        assert!(zipped.len() < csv.len(), "deflated");
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zipped)).unwrap();
        assert_eq!(archive.len(), 1);
        let mut file = archive.by_index(0).unwrap();
        assert_eq!(file.name(), "butterlog-598.csv");
        let mut back = String::new();
        file.read_to_string(&mut back).unwrap();
        assert_eq!(back, csv);
    }
}
