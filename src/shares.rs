//! Shares of the flights the service owns (rollout phase 5). The app still decides when a
//! flight is shared, and uploads its own document, but the share of a flight the service
//! owns is the service's: its document is written again from the flight, its statistics,
//! track and screenshots, each time the flight changes, so the share page shows what the
//! flight page and the Discord post show. Same R2 key, same link.
//!
//! A flight the service made has no id in the app, so its share arrives with none; such a
//! share takes the flight it covers most, by the time span of its track, once one exists.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
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

/// How long after its upload a share without a flight is still matched to one.
const ADOPT_WITHIN: &str = "2 days";

/// The first and last epoch (seconds) of a share document's track: the first timestamp is
/// absolute, the rest deltas.
pub fn span(doc: &Value) -> Option<(i64, i64)> {
    let stamps = doc
        .get("transposedData")
        .or_else(|| doc.get("transposed_data"))?
        .get("timestamps")?
        .as_array()?;
    let first = stamps.first()?.as_i64()?;
    let last = stamps.iter().skip(1).filter_map(Value::as_i64).fold(first, |t, d| t + d);
    Some((first, last))
}

/// The share document of a flight, gzipped JSON in the app's `FlightDetailShare` shape;
/// `None` while the flight has no track, its points pruned past retention, so a share
/// already written is not replaced with an empty one.
pub async fn document(db: &PgPool, flight_id: i64) -> Result<Option<Vec<u8>>, AppError> {
    let row: Option<(Value, Option<String>)> =
        sqlx::query_as("SELECT statistics, notes FROM flights WHERE id = $1")
            .bind(flight_id)
            .fetch_optional(db)
            .await?;
    let Some((statistics, notes)) = row else { return Ok(None) };
    let track = crate::live::load_track(db, flight_id, None).await?;
    if track.timestamps.is_empty() {
        return Ok(None);
    }
    let screenshots = crate::live::load_screenshots(db, flight_id).await?;
    let summary = crate::live::adapt_summary(&statistics, notes.as_deref(), screenshots.len(), true);
    let doc = json!({
        "transposedData": track,
        "summary": summary,
        "screenshots": screenshots,
        "remoteFlightId": flight_id,
    });
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(doc.to_string().as_bytes())
        .and_then(|_| gz.finish())
        .map(Some)
        .map_err(|e| AppError::Storage(e.to_string()))
}

/// Writes the shares of a flight the service owns again from the flight. Returns how many.
pub async fn refresh(db: &PgPool, r2: &R2Client, flight_id: i64) -> Result<usize, AppError> {
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT s.r2_key FROM flight_shares s JOIN flights f ON f.id = s.remote_flight_id \
          WHERE f.id = $1 AND f.service_owned",
    )
    .bind(flight_id)
    .fetch_all(db)
    .await?;
    if keys.is_empty() {
        return Ok(0);
    }
    let Some(doc) = document(db, flight_id).await? else { return Ok(0) };
    for key in &keys {
        r2.upload_object(key, doc.clone(), "application/octet-stream")
            .await
            .map_err(AppError::Storage)?;
    }
    Ok(keys.len())
}

/// Reads the time span of shares uploaded without one, which only shares from before the
/// span was recorded lack, so they can be matched. Returns how many.
pub async fn fill_spans(db: &PgPool, r2: &R2Client, limit: i64) -> Result<usize, AppError> {
    let pending: Vec<(String, String)> = sqlx::query_as(&format!(
        "SELECT s.id, s.r2_key FROM flight_shares s JOIN users u ON u.id = s.user_id \
          WHERE s.remote_flight_id IS NULL AND s.starts_at IS NULL \
            AND u.service_flights_since IS NOT NULL \
            AND s.created_at > NOW() - INTERVAL '{ADOPT_WITHIN}' \
          LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(db)
    .await?;
    for (id, key) in &pending {
        let doc = r2
            .download_object(key)
            .await
            .ok()
            .and_then(|gz| crate::handlers::decompress_gzip_capped(gz.as_slice(), crate::handlers::MAX_SHARE_DECOMPRESSED).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        // One that can't be read is given an empty span at its upload, so it is not read
        // again each minute, and matches nothing.
        let (from, to) = doc.as_ref().and_then(span).map_or((None, None), |(a, b)| (Some(a), Some(b)));
        sqlx::query(
            "UPDATE flight_shares \
                SET starts_at = COALESCE(to_timestamp($2), created_at), \
                    ends_at = COALESCE(to_timestamp($3), created_at) \
              WHERE id = $1",
        )
        .bind(id)
        .bind(from.map(|t| t as f64))
        .bind(to.map(|t| t as f64))
        .execute(db)
        .await?;
    }
    Ok(pending.len())
}

/// Gives each recent share without a flight the user's flight the service owns that its
/// track overlaps most, if that flight has no share yet. Returns the flights given one,
/// whose shares are then to be written and Discord posts synced.
pub async fn adopt(db: &PgPool) -> Result<Vec<i64>, AppError> {
    let matched: Vec<(String, i64)> = sqlx::query_as(&format!(
        "SELECT DISTINCT ON (s.id) s.id, f.id \
           FROM flight_shares s \
           JOIN flights f ON f.user_id = s.user_id AND f.service_owned \
           CROSS JOIN LATERAL ( \
             SELECT to_timestamp(MIN(sample_epoch)) AS lo, to_timestamp(MAX(sample_epoch)) AS hi \
               FROM flight_track_points WHERE flight_id = f.id) t \
          WHERE s.remote_flight_id IS NULL AND s.starts_at IS NOT NULL \
            AND s.created_at > NOW() - INTERVAL '{ADOPT_WITHIN}' \
            AND f.created_at > s.starts_at - INTERVAL '1 day' \
            AND t.lo < s.ends_at AND t.hi > s.starts_at \
            AND NOT EXISTS (SELECT 1 FROM flight_shares o WHERE o.remote_flight_id = f.id) \
          ORDER BY s.id, LEAST(s.ends_at, t.hi) - GREATEST(s.starts_at, t.lo) DESC"
    ))
    .fetch_all(db)
    .await?;
    let mut adopted = Vec::new();
    for (share, flight) in matched {
        // Two shares overlapping one flight: the first takes it, the other stays as it is.
        if adopted.contains(&flight) {
            continue;
        }
        let mut tx = db.begin().await?;
        let taken = sqlx::query("UPDATE flight_shares SET remote_flight_id = $2 WHERE id = $1 AND remote_flight_id IS NULL")
            .bind(&share)
            .bind(flight)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if taken == 1 {
            sqlx::query("UPDATE flights SET share_id = COALESCE(share_id, $1) WHERE id = $2")
                .bind(&share)
                .bind(flight)
                .execute(&mut *tx)
                .await?;
            adopted.push(flight);
        }
        tx.commit().await?;
    }
    Ok(adopted)
}

/// The user's flight the service owns that a span of time (epoch seconds) covers most,
/// by the telemetry it was made from: how the app, which has no id for a flight the
/// service made, finds it from its own log. The telemetry's span outlives the track's
/// points, pruned past retention.
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

/// The id of a flight's share, its latest.
pub async fn of_flight(db: &PgPool, flight_id: i64) -> Result<Option<String>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM flight_shares WHERE remote_flight_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(flight_id)
    .fetch_optional(db)
    .await?)
}

/// Shares a flight the service owns from its own document, or gives back the share it
/// already has. `None` while the flight has no track to share.
pub async fn create(db: &PgPool, r2: &R2Client, user_id: i64, flight_id: i64) -> Result<Option<String>, AppError> {
    if let Some(id) = of_flight(db, flight_id).await? {
        return Ok(Some(id));
    }
    let Some(doc) = document(db, flight_id).await? else { return Ok(None) };
    let (from, to): (Option<f64>, Option<f64>) = sqlx::query_as(
        "SELECT MIN(sample_epoch)::float8, MAX(sample_epoch)::float8 FROM flight_track_points WHERE flight_id = $1",
    )
    .bind(flight_id)
    .fetch_one(db)
    .await?;
    let id = uuid::Uuid::new_v4().to_string();
    let key = format!("shares/{id}.json.gz");
    r2.upload_object(&key, doc, "application/octet-stream")
        .await
        .map_err(AppError::Storage)?;
    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO flight_shares (id, user_id, remote_flight_id, r2_key, starts_at, ends_at) \
         VALUES ($1, $2, $3, $4, COALESCE(to_timestamp($5), NOW()), COALESCE(to_timestamp($6), NOW()))",
    )
    .bind(&id)
    .bind(user_id)
    .bind(flight_id)
    .bind(&key)
    .bind(from)
    .bind(to)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE flights SET share_id = COALESCE(share_id, $1) WHERE id = $2")
        .bind(&id)
        .bind(flight_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(id))
}

#[derive(serde::Deserialize)]
pub struct Span {
    pub from: i64,
    pub to: i64,
}

/// `GET /api/v0/flights/covering?from=&to=`: the signed-in user's flight the service
/// made over that span of their log, with its share and export, for the app's buttons.
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
    let share_url = of_flight(&state.db, id).await?.map(|s| format!("{base}/content/flights/share/{s}"));
    Ok(Json(json!({
        "id": id,
        "share_url": share_url,
        "export_url": format!("{base}/content/flights/{id}/export/flysto.csv"),
    }))
    .into_response())
}

/// `POST /api/v0/flights/:id/share`: shares the signed-in user's flight the service made,
/// from the service's own document; the share it has if it has one.
pub async fn share_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(flight_id): Path<i64>,
) -> Result<Response, AppError> {
    let user_id = crate::handlers::get_user_id_from_session(&state.db, &headers).await?;
    let owned: Option<bool> = sqlx::query_scalar("SELECT service_owned FROM flights WHERE id = $1 AND user_id = $2")
        .bind(flight_id)
        .bind(user_id)
        .fetch_optional(&state.db)
        .await?;
    if owned != Some(true) {
        return Err(AppError::NotFound("Flight not found".to_string()));
    }
    let had = of_flight(&state.db, flight_id).await?.is_some();
    let Some(id) = create(&state.db, &state.r2, user_id, flight_id).await? else {
        return Err(AppError::NotFound("The flight has no track to share".to_string()));
    };
    if !had {
        crate::handlers::spawn_discord_sync(&state, flight_id);
    }
    let url = format!("{}/content/flights/share/{id}", state.config.public_base_url);
    let status = if had { StatusCode::OK } else { StatusCode::CREATED };
    Ok((status, Json(json!({ "url": url, "id": id }))).into_response())
}

/// Against a real Postgres, given by `TEST_DATABASE_URL` (skipped without it).
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn a_shares_span_adds_up_its_deltas() {
        let doc = json!({ "transposedData": { "timestamps": [1_000, 1, 1, 298] } });
        assert_eq!(span(&doc), Some((1_000, 1_300)));
        assert_eq!(span(&json!({ "summary": {} })), None);
    }

    async fn db() -> Option<PgPool> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        let db = PgPool::connect(&url).await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        Some(db)
    }

    #[tokio::test]
    async fn a_share_without_a_flight_takes_the_service_flight_it_covers() {
        let Some(db) = db().await else { return };
        let user: i64 = sqlx::query_scalar(
            "INSERT INTO users (discord_id, username, service_flights_since) \
             VALUES ($1, 'test', NOW() - INTERVAL '1 day') RETURNING id",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .fetch_one(&db)
        .await
        .unwrap();
        let t0 = chrono::Utc::now().timestamp() - 3_600;
        // Two circuits, a minute apart; the share's track covers most of the second.
        let mut flights = Vec::new();
        for (from, to) in [(t0, t0 + 600), (t0 + 660, t0 + 1_260)] {
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO flights (user_id, departure, statistics, created_at, service_owned, service_made, status) \
                 VALUES ($1, 'PHJH', $2, to_timestamp($3), TRUE, TRUE, 'ended') RETURNING id",
            )
            .bind(user)
            .bind(json!({ "start_time": "x", "simulator": "xplane" }))
            .bind(from as f64)
            .fetch_one(&db)
            .await
            .unwrap();
            for epoch in [from, (from + to) / 2, to] {
                sqlx::query("INSERT INTO flight_track_points (flight_id, sample_epoch, latitude, longitude) VALUES ($1, $2, 20.9, -156.6)")
                    .bind(id)
                    .bind(epoch)
                    .execute(&db)
                    .await
                    .unwrap();
            }
            flights.push(id);
        }
        let share = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO flight_shares (id, user_id, r2_key, starts_at, ends_at) \
             VALUES ($1, $2, 'shares/x.json.gz', to_timestamp($3), to_timestamp($4))",
        )
        .bind(&share)
        .bind(user)
        .bind((t0 + 500) as f64)
        .bind((t0 + 1_300) as f64)
        .execute(&db)
        .await
        .unwrap();

        let adopted = adopt(&db).await.unwrap();
        assert!(adopted.contains(&flights[1]), "{adopted:?}");
        assert!(!adopted.contains(&flights[0]));
        let (linked, on_flight): (Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT s.remote_flight_id, f.share_id FROM flight_shares s, flights f WHERE s.id = $1 AND f.id = $2",
        )
        .bind(&share)
        .bind(flights[1])
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!((linked, on_flight.as_deref()), (Some(flights[1]), Some(share.as_str())));
        // Once taken, nothing more to adopt.
        assert!(!adopt(&db).await.unwrap().contains(&flights[1]));

        // Its document is the flight's own.
        let gz = document(&db, flights[1]).await.unwrap().unwrap();
        let mut text = String::new();
        flate2::read::GzDecoder::new(gz.as_slice()).read_to_string(&mut text).unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(span(&doc), Some((t0 + 660, t0 + 1_260)));
        assert_eq!(doc["remoteFlightId"], json!(flights[1]));
        assert_eq!(doc["summary"]["startIcao"], json!(""));
        sqlx::query("DELETE FROM flight_track_points WHERE flight_id = $1").bind(flights[1]).execute(&db).await.unwrap();
        assert!(document(&db, flights[1]).await.unwrap().is_none());
    }
}
