//! A flight's page document: its track, summary and screenshots, in the shape the flight
//! renderer reads (the app's `FlightDetailShare`). Every flight has one: there are no
//! shares any more, the page of a flight shows all the service has of it.
//!
//! The track comes from the flight's points while it has them, then from the copy saved to
//! R2 before they are pruned (`archive`), or, for a flight pruned before that existed, from
//! the share the app once uploaded of it. The summary and screenshots are always the
//! flight's own, so an edit after the track is archived still shows.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    Json,
};
use flate2::{write::GzEncoder, Compression};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::io::Write;

use crate::error::AppError;
use crate::r2::R2Client;
use crate::AppState;

/// How long after a flight ends its points are kept, before only the archive is.
pub const KEEP_POINTS: &str = "7 days";

fn gzip(value: &Value) -> Result<Vec<u8>, AppError> {
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(value.to_string().as_bytes())
        .and_then(|_| gz.finish())
        .map_err(|e| AppError::Storage(e.to_string()))
}

async fn download_json(r2: &R2Client, key: &str) -> Option<Value> {
    let gz = r2.download_object(key).await.ok()?;
    let text = crate::handlers::decompress_gzip_capped(gz.as_slice(), crate::handlers::MAX_SHARE_DECOMPRESSED).ok()?;
    serde_json::from_str(&text).ok()
}

fn has_points(track: &Value) -> bool {
    track.get("timestamps").and_then(Value::as_array).is_some_and(|t| !t.is_empty())
}

/// The flight's track in the renderer's columnar shape, from wherever it now is.
pub async fn track(db: &PgPool, r2: &R2Client, flight_id: i64) -> Result<Option<Value>, AppError> {
    let points = crate::live::load_track(db, flight_id, None).await?;
    if !points.timestamps.is_empty() {
        return Ok(Some(serde_json::to_value(points).map_err(|e| AppError::Storage(e.to_string()))?));
    }
    let (archived, share): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT f.track_key, \
                (SELECT s.r2_key FROM flight_shares s WHERE s.remote_flight_id = f.id ORDER BY s.created_at DESC LIMIT 1) \
           FROM flights f WHERE f.id = $1",
    )
    .bind(flight_id)
    .fetch_optional(db)
    .await?
    .unwrap_or((None, None));
    if let Some(key) = archived {
        if let Some(track) = download_json(r2, &key).await.filter(has_points) {
            return Ok(Some(track));
        }
    }
    if let Some(key) = share {
        let doc = download_json(r2, &key).await;
        if let Some(track) = doc
            .and_then(|d| d.get("transposedData").or_else(|| d.get("transposed_data")).cloned())
            .filter(has_points)
        {
            return Ok(Some(track));
        }
    }
    Ok(None)
}

/// The circuit flown into each landing (`analysis/pattern.rs`), worked out from the track
/// as the page is made: its legs among the summary's events, and each landing's debrief
/// (`analysis/debrief.rs`), returned.
fn add_circuits(summary: &mut crate::live::LiveSummary, statistics: &Value, track: &Value) -> Vec<Value> {
    let arrival = statistics.pointer("/arrival/icao").and_then(Value::as_str).unwrap_or_default();
    let runways = crate::analysis::ingest::places().map_or(&[][..], |p| p.runways(arrival));
    let circuits = crate::analysis::pattern::circuits(&summary.events, track, runways);
    if !circuits.events.is_empty() {
        summary.events.extend(circuits.events);
        let at = |e: &Value| e.get("timestamp").and_then(Value::as_str).unwrap_or_default().to_string();
        summary.events.sort_by_key(at);
    }
    circuits.debriefs
}

/// The flight's page document; `None` when no track of it is left anywhere.
pub async fn document(db: &PgPool, r2: &R2Client, flight_id: i64) -> Result<Option<Value>, AppError> {
    let row: Option<(Value, Option<String>, String)> =
        sqlx::query_as("SELECT statistics, notes, status FROM flights WHERE id = $1")
            .bind(flight_id)
            .fetch_optional(db)
            .await?;
    let Some((statistics, notes, status)) = row else { return Ok(None) };
    let Some(track) = track(db, r2, flight_id).await? else { return Ok(None) };
    let screenshots = crate::live::load_screenshots(db, flight_id).await?;
    let mut summary = crate::live::adapt_summary(&statistics, notes.as_deref(), screenshots.len(), status == "ended");
    let debriefs = add_circuits(&mut summary, &statistics, &track);
    Ok(Some(json!({
        "transposedData": track,
        "summary": summary,
        "debriefs": debriefs,
        "screenshots": screenshots,
        "remoteFlightId": flight_id,
        // The renderer reads the track's raw pitch and roll through it.
        "simulator": statistics.get("simulator").and_then(Value::as_str).unwrap_or_default(),
    })))
}

/// Saves the flight's track to R2 and records where; `false` when it has no points.
pub async fn archive(db: &PgPool, r2: &R2Client, flight_id: i64) -> Result<bool, AppError> {
    let points = crate::live::load_track(db, flight_id, None).await?;
    if points.timestamps.is_empty() {
        return Ok(false);
    }
    let track = serde_json::to_value(points).map_err(|e| AppError::Storage(e.to_string()))?;
    let key = format!("tracks/{flight_id}.json.gz");
    r2.upload_object(&key, gzip(&track)?, "application/octet-stream")
        .await
        .map_err(AppError::Storage)?;
    sqlx::query("UPDATE flights SET track_key = $1 WHERE id = $2")
        .bind(&key)
        .bind(flight_id)
        .execute(db)
        .await?;
    Ok(true)
}

/// Archives then prunes the points of flights ended more than `KEEP_POINTS` ago, up to
/// `limit` of them. Points are only deleted once saved: a flight whose archive fails keeps
/// them, to be tried again. Archived each time, so points a re-analysis wrote again after
/// an earlier archive are saved too. Returns the points deleted.
pub async fn prune(db: &PgPool, r2: &R2Client, limit: i64) -> Result<u64, AppError> {
    let due: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT f.id FROM flights f \
          WHERE f.status = 'ended' AND f.ended_at < NOW() - INTERVAL '{KEEP_POINTS}' \
            AND EXISTS (SELECT 1 FROM flight_track_points p WHERE p.flight_id = f.id) \
          ORDER BY f.ended_at LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(db)
    .await?;
    let mut deleted = 0;
    for id in due {
        match archive(db, r2, id).await {
            Ok(_) => {
                deleted += sqlx::query("DELETE FROM flight_track_points WHERE flight_id = $1")
                    .bind(id)
                    .execute(db)
                    .await?
                    .rows_affected();
            }
            Err(e) => tracing::error!(flight_id = id, "archiving the track failed, its points kept: {e:?}"),
        }
    }
    Ok(deleted)
}

/// The user's flight the service owns that a span of time (epoch seconds) covers most,
/// by the telemetry it was made from: how the app, which has no id for a flight the
/// service made, finds it from its own log. The telemetry's span outlives the track's
/// points.
pub async fn covering(db: &PgPool, user_id: i64, from: i64, to: i64) -> Result<Option<i64>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT f.id FROM telemetry_flights t JOIN flights f ON f.id = t.flight_id \
          WHERE f.user_id = $1 AND f.service_owned \
            AND t.first_at < to_timestamp($3) AND t.last_at > to_timestamp($2) \
          ORDER BY LEAST(t.last_at, to_timestamp($3)) - GREATEST(t.first_at, to_timestamp($2)) DESC, \
                   t.analyser_version DESC \
          LIMIT 1",
    )
    .bind(user_id)
    .bind(from as f64)
    .bind(to as f64)
    .fetch_optional(db)
    .await?)
}

#[derive(serde::Deserialize)]
pub struct Span {
    pub from: i64,
    pub to: i64,
}

/// `GET /api/v0/flights/covering?from=&to=`: the signed-in user's flight the service
/// made over that span of their log, with its page and export, for the app.
pub async fn covering_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(span): Query<Span>,
) -> Result<Response, AppError> {
    let user_id = crate::handlers::get_user_id_from_session(&state.db, &headers).await?;
    let Some(id) = covering(&state.db, user_id, span.from, span.to).await? else {
        return Err(AppError::NotFound("No flight the service made covers this".to_string()));
    };
    let base = &state.config.public_base_url;
    Ok(Json(json!({
        "id": id,
        "url": format!("{base}/content/flights/{id}"),
        "export_url": format!("{base}/content/flights/{id}/export/flysto.csv"),
    }))
    .into_response())
}

/// Against a real Postgres, given by `TEST_DATABASE_URL` (skipped without it), with an R2
/// that refuses every connection.
#[cfg(test)]
mod tests {
    use super::*;

    fn unreachable_r2() -> R2Client {
        R2Client::new(&crate::config::Config {
            database_url: String::new(),
            discord_client_id: String::new(),
            discord_client_secret: String::new(),
            discord_redirect_uri: String::new(),
            discord_bot_token: String::new(),
            port: 0,
            r2_bucket: "test".into(),
            r2_endpoint: "http://127.0.0.1:9".into(),
            r2_access_key_id: "test".into(),
            r2_secret_access_key: "test".into(),
            r2_public_url: String::new(),
            r2_telemetry_bucket: None,
            public_base_url: String::new(),
            predetermined_channels: None,
        })
    }

    #[tokio::test]
    async fn a_flight_shows_its_track_and_keeps_its_points_until_they_are_saved() {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return };
        let db = PgPool::connect(&url).await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        let r2 = unreachable_r2();
        let user: i64 = sqlx::query_scalar("INSERT INTO users (discord_id, username) VALUES ($1, 'test') RETURNING id")
            .bind(uuid::Uuid::new_v4().to_string())
            .fetch_one(&db)
            .await
            .unwrap();
        let flight: i64 = sqlx::query_scalar(
            "INSERT INTO flights (user_id, departure, statistics, status, ended_at) \
             VALUES ($1, 'EKVG', $2, 'ended', NOW() - INTERVAL '8 days') RETURNING id",
        )
        .bind(user)
        .bind(json!({ "start_time": "x", "simulator": "MSFS" }))
        .fetch_one(&db)
        .await
        .unwrap();
        let t0 = chrono::Utc::now().timestamp() - 8 * 86_400;
        for epoch in [t0, t0 + 60, t0 + 120] {
            sqlx::query("INSERT INTO flight_track_points (flight_id, sample_epoch, latitude, longitude) VALUES ($1, $2, 62.0, -7.3)")
                .bind(flight)
                .bind(epoch)
                .execute(&db)
                .await
                .unwrap();
        }

        // Its page document: its track, and its own summary.
        let doc = document(&db, &r2, flight).await.unwrap().unwrap();
        assert_eq!(crate::shares::span(&doc), Some((t0, t0 + 120)));
        assert_eq!(doc["remoteFlightId"], json!(flight));
        assert_eq!(doc["simulator"], json!("MSFS"));

        // Past its week, but R2 is unreachable: the track can't be saved, so its points stay.
        assert_eq!(prune(&db, &r2, 1_000).await.unwrap(), 0);
        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM flight_track_points WHERE flight_id = $1")
            .bind(flight)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(kept, 3);

        // No points, no saved track, no share: nothing to show it with.
        sqlx::query("DELETE FROM flight_track_points WHERE flight_id = $1").bind(flight).execute(&db).await.unwrap();
        assert!(document(&db, &r2, flight).await.unwrap().is_none());
        // A saved track that can't be read is no track either, not an error.
        sqlx::query("UPDATE flights SET track_key = 'tracks/x.json.gz' WHERE id = $1").bind(flight).execute(&db).await.unwrap();
        assert!(document(&db, &r2, flight).await.unwrap().is_none());
    }
}
