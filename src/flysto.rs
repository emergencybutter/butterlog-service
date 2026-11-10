//! Uploading a flight to FlySto: the same Garmin CSV the Export button saves (`export.rs`),
//! zipped, as FlySto wants its logs, and posted to its public API
//! (www.flysto.net/public-api/swagger) with the pilot's own API token
//! (`users.flysto_token`), which they make at www.flysto.net/account/api and paste in
//! Settings. The token is checked against FlySto before it is kept, and never sent back
//! to the browser.

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
    sqlx::query("UPDATE users SET flysto_token = $1 WHERE id = $2")
        .bind(token)
        .bind(user_id)
        .execute(&state.db)
        .await?;
    Ok(Json(json!({ "connected": true })))
}

/// `DELETE /api/v0/me/flysto`: forgets the pilot's FlySto API token.
pub async fn delete_token(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Value>, AppError> {
    let user_id = signed_in(&state, &headers).await?;
    sqlx::query("UPDATE users SET flysto_token = NULL WHERE id = $1")
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
    let csv = crate::export::owned_csv(&state, flight_id, Some(user_id)).await?;
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
        .bearer_auth(&token)
        .header(reqwest::header::CONTENT_TYPE, "application/zip")
        .timeout(std::time::Duration::from_secs(120))
        .body(body)
        .send()
        .await?;
    // Both answer `{"fileId": ...}`, as text: the new file's, or, for a log FlySto
    // already has (the same flight uploaded again), the existing one's.
    let status = r.status().as_u16();
    if status == 200 || status == 409 {
        let file_id = file_id(&r.text().await.unwrap_or_default());
        let duplicate = status == 409;
        tracing::info!(flight_id, user_id, duplicate, file_id = file_id.as_deref(), "uploaded to FlySto");
        return Ok(Json(json!({ "uploaded": true, "duplicate": duplicate, "fileId": file_id })));
    }
    match status {
        401 | 403 => Err(AppError::Upstream(
            "FlySto no longer accepts your API token: update it in Settings".to_string(),
        )),
        _ => Err(AppError::Upstream(refusal(r).await)),
    }
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
