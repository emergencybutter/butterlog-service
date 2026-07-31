//! Live flight track ingestion.
//!
//! The app streams telemetry batches here while a flight is in progress. This
//! endpoint is deliberately separate from `PUT /flights/:id`: that one fires a
//! Discord notification sync and so cannot be sped up, while this one is a pure
//! append that also serves as the flight's liveness heartbeat.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::error::AppError;
use crate::handlers::{decompress_gzip_capped, get_user_id_from_session};
use crate::AppState;

/// Maximum decompressed size of a single track batch.
pub const MAX_TRACK_DECOMPRESSED: u64 = 256 * 1024;

/// Maximum samples accepted in one batch.
pub const MAX_POINTS_PER_BATCH: usize = 2000;

/// Ceiling on stored samples for one flight, past which the oldest hour is
/// thinned. ~14 hours of continuously hand-flown 1 Hz samples.
pub const MAX_POINTS_PER_FLIGHT: i64 = 50_000;

/// A flight with no update within this window is shown as `stale` rather than
/// live, so a viewer can tell a frozen track from a moving one.
pub const STALE_AFTER_SECS: i64 = 120;

/// A flight with no update within this window is ended by the reaper.
pub const REAP_AFTER_SECS: i64 = 30 * 60;

/// Columnar telemetry, matching the app's `TransposedFlightData` so the app's
/// existing transposition code and the share renderer's decoder both apply
/// unchanged. This is a transport shape only — storage is one row per sample.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct TransposedPoints {
    #[serde(default)]
    pub timestamps: Vec<i64>,
    #[serde(default)]
    pub latitudes: Vec<f32>,
    #[serde(default)]
    pub longitudes: Vec<f32>,
    #[serde(default)]
    pub altitudes: Vec<f32>,
    #[serde(default)]
    pub ias: Vec<f32>,
    #[serde(default)]
    pub vspeed: Vec<f32>,
    #[serde(default)]
    pub pitch: Vec<f32>,
    #[serde(default)]
    pub roll: Vec<f32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackBatch {
    /// Absolute unix seconds of the first sample in the batch.
    pub start_epoch: i64,
    #[serde(default)]
    pub points: TransposedPoints,
    /// Flight events observed so far, repeated here so a new takeoff/landing
    /// lands online without waiting for the next `PUT /flights/:id`.
    #[serde(default)]
    pub events: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackAccepted {
    pub last_epoch: Option<i64>,
    pub accepted: usize,
    pub duplicates: usize,
    pub total_points: i64,
    pub status: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackCursor {
    pub last_epoch: Option<i64>,
    pub total_points: i64,
    pub status: String,
}

/// One decoded sample.
pub struct DecodedPoint {
    pub epoch: i64,
    pub latitude: f32,
    pub longitude: f32,
    pub altitude: Option<f32>,
    pub ias: Option<f32>,
    pub vspeed: Option<f32>,
    pub pitch: Option<f32>,
    pub roll: Option<f32>,
}

/// Expand the columnar batch into absolute-timestamped samples.
///
/// `timestamps` are deltas with a leading zero, so sample *i* sits at
/// `start_epoch + sum(timestamps[0..=i])`. Optional columns may be absent
/// entirely (older clients, or channels a sim does not provide) but if present
/// must match the timestamp count — a short column would silently misalign
/// every sample after the gap.
pub fn decode_batch(batch: &TrackBatch) -> Result<Vec<DecodedPoint>, String> {
    let p = &batch.points;
    let n = p.timestamps.len();

    if n == 0 {
        return Ok(Vec::new());
    }
    if n > MAX_POINTS_PER_BATCH {
        return Err(format!(
            "Batch holds {} samples, limit is {}",
            n, MAX_POINTS_PER_BATCH
        ));
    }
    if p.latitudes.len() != n || p.longitudes.len() != n {
        return Err("latitudes and longitudes must match the timestamp count".to_string());
    }
    for (name, len) in [
        ("altitudes", p.altitudes.len()),
        ("ias", p.ias.len()),
        ("vspeed", p.vspeed.len()),
        ("pitch", p.pitch.len()),
        ("roll", p.roll.len()),
    ] {
        if len != 0 && len != n {
            return Err(format!(
                "column `{}` has {} values but the batch has {} samples",
                name, len, n
            ));
        }
    }

    let at = |col: &Vec<f32>, i: usize| -> Option<f32> {
        col.get(i).copied().filter(|v| v.is_finite())
    };

    let mut out = Vec::with_capacity(n);
    let mut epoch = batch.start_epoch;
    for i in 0..n {
        epoch += p.timestamps[i];
        let lat = p.latitudes[i];
        let lon = p.longitudes[i];
        // A sample without a usable fix is worse than no sample: it draws the
        // track through null island.
        if !lat.is_finite() || !lon.is_finite() || (lat == 0.0 && lon == 0.0) {
            continue;
        }
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            continue;
        }
        out.push(DecodedPoint {
            epoch,
            latitude: lat,
            longitude: lon,
            altitude: at(&p.altitudes, i),
            ias: at(&p.ias, i),
            vspeed: at(&p.vspeed, i),
            pitch: at(&p.pitch, i),
            roll: at(&p.roll, i),
        });
    }
    Ok(out)
}

/// Flight ownership plus lifecycle in one lookup.
async fn owned_active_flight(
    db: &PgPool,
    flight_id: i64,
    user_id: i64,
) -> Result<String, AppError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT status FROM flights WHERE id = $1 AND user_id = $2")
            .bind(flight_id)
            .bind(user_id)
            .fetch_optional(db)
            .await?;

    let status = row
        .ok_or_else(|| AppError::NotFound("Flight not found".to_string()))?
        .0;

    if status == "ended" {
        return Err(AppError::Gone("Flight has ended".to_string()));
    }
    Ok(status)
}

/// Identity of a flight event. The app has emitted both casings over time, so
/// accept either rather than letting a rename duplicate every event.
fn event_key(e: &serde_json::Value) -> (String, String) {
    (
        e.get("event_type")
            .or_else(|| e.get("eventType"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        e.get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    )
}

/// Union of the two event lists, keyed by `(event_type, timestamp)`.
///
/// Returns `None` when `incoming` adds nothing, which is the *common* case: the
/// app re-sends the whole cumulative list with every 20s batch, so from the
/// first takeoff onward almost every merge is a no-op. The caller uses this to
/// skip the write entirely — see `merge_events`.
fn merged_events(
    existing: &[serde_json::Value],
    incoming: &[serde_json::Value],
) -> Option<Vec<serde_json::Value>> {
    let mut seen: std::collections::HashSet<(String, String)> =
        existing.iter().map(event_key).collect();
    let mut merged = existing.to_vec();
    for ev in incoming {
        if seen.insert(event_key(ev)) {
            merged.push(ev.clone());
        }
    }
    (merged.len() != existing.len()).then_some(merged)
}

/// Merge newly observed flight events into `statistics.events`. Runs inside the
/// caller's transaction against a locked row so it cannot race a concurrent
/// `PUT /flights/:id`.
async fn merge_events(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    flight_id: i64,
    incoming: &[serde_json::Value],
) -> Result<(), AppError> {
    if incoming.is_empty() {
        return Ok(());
    }

    let mut stats: serde_json::Value =
        sqlx::query_scalar("SELECT statistics FROM flights WHERE id = $1 FOR UPDATE")
            .bind(flight_id)
            .fetch_one(&mut **tx)
            .await?;

    let existing = stats
        .get("events")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // Nothing new: leave the row alone. Writing an identical statistics blob
    // back on every batch would rewrite a multi-kilobyte JSONB (and its TOAST
    // chunks) roughly 500 times over a three-hour flight, for no change.
    let Some(merged) = merged_events(&existing, incoming) else {
        return Ok(());
    };

    if let Some(obj) = stats.as_object_mut() {
        obj.insert("events".to_string(), serde_json::Value::Array(merged));
    }

    sqlx::query("UPDATE flights SET statistics = $1 WHERE id = $2")
        .bind(&stats)
        .bind(flight_id)
        .execute(&mut **tx)
        .await?;

    Ok(())
}

/// Thin the oldest samples once a flight passes the per-flight ceiling: keep one
/// per minute outside a +/-60s window around any flight event, so touchdown
/// detail survives and only the cruise is coarsened.
async fn thin_if_over_ceiling(
    db: &PgPool,
    flight_id: i64,
    total: i64,
) -> Result<i64, AppError> {
    if total <= MAX_POINTS_PER_FLIGHT {
        return Ok(total);
    }

    // Event epochs are parsed from statistics; an empty list simply means no
    // window is protected.
    let stats: serde_json::Value =
        sqlx::query_scalar("SELECT statistics FROM flights WHERE id = $1")
            .bind(flight_id)
            .fetch_one(db)
            .await?;

    let event_epochs: Vec<i64> = stats
        .get("events")
        .and_then(|v| v.as_array())
        .map(|events| {
            events
                .iter()
                .filter_map(|e| e.get("timestamp").and_then(|t| t.as_str()))
                .filter_map(parse_event_epoch)
                .collect()
        })
        .unwrap_or_default();

    // Cut off at the oldest hour of the flight's samples.
    let cutoff: Option<i64> = sqlx::query_scalar(
        "SELECT MIN(sample_epoch) + 3600 FROM flight_track_points WHERE flight_id = $1",
    )
    .bind(flight_id)
    .fetch_one(db)
    .await?;

    let Some(cutoff) = cutoff else {
        return Ok(total);
    };

    let deleted = sqlx::query(
        "DELETE FROM flight_track_points p \
          WHERE p.flight_id = $1 \
            AND p.sample_epoch < $2 \
            AND p.sample_epoch % 60 <> 0 \
            AND NOT EXISTS (SELECT 1 FROM unnest($3::bigint[]) e WHERE abs(p.sample_epoch - e) <= 60)",
    )
    .bind(flight_id)
    .bind(cutoff)
    .bind(&event_epochs)
    .execute(db)
    .await?
    .rows_affected() as i64;

    let remaining = total - deleted;
    sqlx::query("UPDATE flights SET track_points = $1 WHERE id = $2")
        .bind(remaining as i32)
        .bind(flight_id)
        .execute(db)
        .await?;

    tracing::info!(
        flight_id,
        deleted,
        remaining,
        "thinned track past the per-flight ceiling"
    );
    Ok(remaining)
}

/// Parse the app's `%Y-%m-%d %H:%M:%S[.fff]` event timestamps (UTC) to epoch
/// seconds.
fn parse_event_epoch(ts: &str) -> Option<i64> {
    let head = ts.split('.').next().unwrap_or(ts);
    chrono::NaiveDateTime::parse_from_str(head, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|dt| dt.and_utc().timestamp())
}

pub async fn upload_track_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    let status = owned_active_flight(&state.db, flight_id, user_id).await?;

    let json = decompress_gzip_capped(&body, MAX_TRACK_DECOMPRESSED)
        .map_err(AppError::PayloadTooLarge)?;
    let batch: TrackBatch = serde_json::from_str(&json)
        .map_err(|e| AppError::BadRequest(format!("Invalid track batch: {}", e)))?;

    let points = decode_batch(&batch).map_err(AppError::BadRequest)?;
    let submitted = points.len();

    // Explode the columnar batch into rows. ON CONFLICT DO NOTHING is what makes
    // the endpoint idempotent: a replayed batch inserts nothing and still
    // reports success.
    let mut epochs: Vec<i64> = Vec::with_capacity(submitted);
    let mut lats: Vec<f32> = Vec::with_capacity(submitted);
    let mut lons: Vec<f32> = Vec::with_capacity(submitted);
    let mut alts: Vec<Option<f32>> = Vec::with_capacity(submitted);
    let mut ias: Vec<Option<f32>> = Vec::with_capacity(submitted);
    let mut vs: Vec<Option<f32>> = Vec::with_capacity(submitted);
    let mut pitch: Vec<Option<f32>> = Vec::with_capacity(submitted);
    let mut roll: Vec<Option<f32>> = Vec::with_capacity(submitted);
    for p in &points {
        epochs.push(p.epoch);
        lats.push(p.latitude);
        lons.push(p.longitude);
        alts.push(p.altitude);
        ias.push(p.ias);
        vs.push(p.vspeed);
        pitch.push(p.pitch);
        roll.push(p.roll);
    }

    let mut tx = state.db.begin().await?;

    let accepted = if submitted == 0 {
        0
    } else {
        sqlx::query(
            "INSERT INTO flight_track_points \
                 (flight_id, sample_epoch, latitude, longitude, altitude, ias, vspeed, pitch, roll) \
             SELECT $1, * FROM UNNEST($2::bigint[], $3::real[], $4::real[], $5::real[], \
                                      $6::real[], $7::real[], $8::real[], $9::real[]) \
             ON CONFLICT (flight_id, sample_epoch) DO NOTHING",
        )
        .bind(flight_id)
        .bind(&epochs)
        .bind(&lats)
        .bind(&lons)
        .bind(&alts)
        .bind(&ias)
        .bind(&vs)
        .bind(&pitch)
        .bind(&roll)
        .execute(&mut *tx)
        .await?
        .rows_affected() as usize
    };

    merge_events(&mut tx, flight_id, &batch.events).await?;

    // Bumping updated_at is what makes this endpoint the liveness heartbeat, so
    // an empty batch during cruise still keeps the flight showing as active.
    let total: i32 = sqlx::query_scalar(
        "UPDATE flights SET track_points = track_points + $1, updated_at = CURRENT_TIMESTAMP \
         WHERE id = $2 RETURNING track_points",
    )
    .bind(accepted as i32)
    .bind(flight_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;

    let total = thin_if_over_ceiling(&state.db, flight_id, total as i64).await?;

    let last_epoch: Option<i64> =
        sqlx::query_scalar("SELECT MAX(sample_epoch) FROM flight_track_points WHERE flight_id = $1")
            .bind(flight_id)
            .fetch_one(&state.db)
            .await?;

    Ok((
        StatusCode::OK,
        Json(TrackAccepted {
            last_epoch,
            accepted,
            duplicates: submitted - accepted,
            total_points: total,
            status,
        }),
    ))
}

/// Where the server thinks this flight's track ends. The app keeps its own
/// cursor in SQLite; this is the recovery path when that is lost, so it can
/// resume instead of replaying hours of samples.
pub async fn track_cursor_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;

    let row: Option<(String, i32)> =
        sqlx::query_as("SELECT status, track_points FROM flights WHERE id = $1 AND user_id = $2")
            .bind(flight_id)
            .bind(user_id)
            .fetch_optional(&state.db)
            .await?;
    let (status, total) = row.ok_or_else(|| AppError::NotFound("Flight not found".to_string()))?;

    let last_epoch: Option<i64> =
        sqlx::query_scalar("SELECT MAX(sample_epoch) FROM flight_track_points WHERE flight_id = $1")
            .bind(flight_id)
            .fetch_one(&state.db)
            .await?;

    Ok((
        StatusCode::OK,
        Json(TrackCursor {
            last_epoch,
            total_points: total as i64,
            status,
        }),
    ))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndFlightRequest {
    /// `landed`, `sim_closed` or `abandoned`.
    #[serde(default)]
    pub reason: Option<String>,
    /// Set when the app has already uploaded the permanent share, so the live
    /// page can hand off to it rather than showing a frozen track.
    #[serde(default)]
    pub share_id: Option<String>,
}

pub async fn end_flight_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    headers: HeaderMap,
    body: Option<Json<EndFlightRequest>>,
) -> Result<impl IntoResponse, AppError> {
    let user_id = get_user_id_from_session(&state.db, &headers).await?;
    let payload = body.map(|Json(b)| b).unwrap_or(EndFlightRequest {
        reason: None,
        share_id: None,
    });

    let reason = payload.reason.unwrap_or_else(|| "landed".to_string());
    if !matches!(reason.as_str(), "landed" | "sim_closed" | "abandoned") {
        return Err(AppError::Unprocessable(format!(
            "Unknown end reason `{}`",
            reason
        )));
    }

    // Ending twice is not an error — the app may retry, and the reaper may have
    // got there first. COALESCE keeps the original ending intact.
    let updated = sqlx::query(
        "UPDATE flights \
            SET status = 'ended', \
                ended_at = COALESCE(ended_at, NOW()), \
                end_reason = COALESCE(end_reason, $1), \
                share_id = COALESCE($2, share_id) \
          WHERE id = $3 AND user_id = $4",
    )
    .bind(&reason)
    .bind(&payload.share_id)
    .bind(flight_id)
    .bind(user_id)
    .execute(&state.db)
    .await?;

    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound("Flight not found".to_string()));
    }

    Ok(StatusCode::NO_CONTENT)
}

/// Background sweep: end flights that stopped reporting, and drop track points
/// for flights whose retention window has passed.
pub fn spawn_reaper(db: PgPool) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            ticker.tick().await;

            let ended = sqlx::query(
                "UPDATE flights \
                    SET status = 'ended', ended_at = NOW(), end_reason = 'abandoned' \
                  WHERE status = 'active' \
                    AND updated_at < NOW() - make_interval(secs => $1)",
            )
            .bind(REAP_AFTER_SECS as f64)
            .execute(&db)
            .await;

            match ended {
                Ok(r) if r.rows_affected() > 0 => {
                    tracing::info!(count = r.rows_affected(), "reaped abandoned flights");
                }
                Err(e) => tracing::error!("flight reaper failed: {:?}", e),
                _ => {}
            }

            // Retention: the share is the durable copy, so a shared flight keeps
            // its live track for a week and an unshared one for a month.
            let pruned = sqlx::query(
                "DELETE FROM flight_track_points p \
                  USING flights f \
                  WHERE p.flight_id = f.id \
                    AND f.status = 'ended' \
                    AND f.ended_at < NOW() - CASE WHEN f.share_id IS NULL \
                                                  THEN INTERVAL '30 days' \
                                                  ELSE INTERVAL '7 days' END",
            )
            .execute(&db)
            .await;

            match pruned {
                Ok(r) if r.rows_affected() > 0 => {
                    tracing::info!(points = r.rows_affected(), "pruned expired track points");
                }
                Err(e) => tracing::error!("track retention sweep failed: {:?}", e),
                _ => {}
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(start: i64, deltas: Vec<i64>, lat: Vec<f32>, lon: Vec<f32>) -> TrackBatch {
        TrackBatch {
            start_epoch: start,
            points: TransposedPoints {
                timestamps: deltas,
                latitudes: lat,
                longitudes: lon,
                ..Default::default()
            },
            events: Vec::new(),
        }
    }

    #[test]
    fn deltas_accumulate_from_start_epoch() {
        let b = batch(
            1000,
            vec![0, 10, 20],
            vec![10.0, 10.1, 10.2],
            vec![20.0, 20.1, 20.2],
        );
        let pts = decode_batch(&b).unwrap();
        assert_eq!(
            pts.iter().map(|p| p.epoch).collect::<Vec<_>>(),
            vec![1000, 1010, 1030]
        );
    }

    #[test]
    fn optional_columns_may_be_absent_entirely() {
        let b = batch(0, vec![0], vec![1.0], vec![2.0]);
        let pts = decode_batch(&b).unwrap();
        assert_eq!(pts.len(), 1);
        assert!(pts[0].altitude.is_none());
    }

    #[test]
    fn a_short_optional_column_is_rejected() {
        // Accepting this would misalign every sample after the gap.
        let mut b = batch(0, vec![0, 1], vec![1.0, 1.0], vec![2.0, 2.0]);
        b.points.altitudes = vec![100.0];
        assert!(decode_batch(&b).is_err());
    }

    #[test]
    fn null_island_and_out_of_range_fixes_are_dropped() {
        let b = batch(
            0,
            vec![0, 1, 1, 1],
            vec![0.0, 91.0, 45.0, 10.0],
            vec![0.0, 10.0, 200.0, 20.0],
        );
        let pts = decode_batch(&b).unwrap();
        assert_eq!(pts.len(), 1, "only the one valid fix survives");
        assert_eq!(pts[0].latitude, 10.0);
    }

    #[test]
    fn dropped_fixes_do_not_shift_later_timestamps() {
        // The epoch accumulator must advance for skipped samples too, otherwise
        // a bad fix early in the batch drags the rest of the track backwards.
        let b = batch(
            100,
            vec![0, 5, 5],
            vec![0.0, 0.0, 45.0],
            vec![0.0, 0.0, 45.0],
        );
        let pts = decode_batch(&b).unwrap();
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].epoch, 110);
    }

    #[test]
    fn an_empty_batch_is_valid() {
        let b = batch(0, vec![], vec![], vec![]);
        assert_eq!(decode_batch(&b).unwrap().len(), 0);
    }

    #[test]
    fn oversized_batches_are_rejected() {
        let n = MAX_POINTS_PER_BATCH + 1;
        let b = batch(0, vec![1; n], vec![1.0; n], vec![2.0; n]);
        assert!(decode_batch(&b).is_err());
    }

    fn ev(kind: &str, ts: &str) -> serde_json::Value {
        serde_json::json!({ "event_type": kind, "timestamp": ts })
    }

    #[test]
    fn resending_the_same_events_asks_for_no_write() {
        // The app re-sends its whole cumulative event list on every 20s batch,
        // so this is the steady state for most of a flight. Returning None is
        // what stops it rewriting the statistics blob 500 times per flight.
        let existing = vec![ev("takeoff", "2026-01-01 10:00:00")];
        assert!(merged_events(&existing, &existing).is_none());
    }

    #[test]
    fn a_genuinely_new_event_is_appended() {
        let existing = vec![ev("takeoff", "2026-01-01 10:00:00")];
        let incoming = vec![
            ev("takeoff", "2026-01-01 10:00:00"),
            ev("top_of_climb", "2026-01-01 10:12:00"),
        ];
        let merged = merged_events(&existing, &incoming).expect("should ask for a write");
        assert_eq!(merged.len(), 2);
        assert_eq!(event_key(&merged[1]).0, "top_of_climb");
    }

    #[test]
    fn the_first_event_of_a_flight_writes() {
        let merged = merged_events(&[], &[ev("takeoff", "2026-01-01 10:00:00")]).unwrap();
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn both_key_casings_identify_the_same_event() {
        // The app has emitted both over time; treating them as distinct would
        // duplicate every event on the flight the casing changed.
        let existing = vec![serde_json::json!({
            "eventType": "landing", "timestamp": "2026-01-01 11:30:00"
        })];
        let incoming = vec![ev("landing", "2026-01-01 11:30:00")];
        assert!(merged_events(&existing, &incoming).is_none());
    }

    #[test]
    fn same_type_at_a_different_time_is_a_different_event() {
        // Touch-and-goes produce several landings; keying on type alone would
        // collapse them into one.
        let existing = vec![ev("landing", "2026-01-01 11:00:00")];
        let incoming = vec![ev("landing", "2026-01-01 11:30:00")];
        assert_eq!(merged_events(&existing, &incoming).unwrap().len(), 2);
    }

    #[test]
    fn event_timestamps_parse_with_and_without_fractions() {
        assert_eq!(parse_event_epoch("1970-01-01 00:00:10"), Some(10));
        assert_eq!(parse_event_epoch("1970-01-01 00:00:10.500"), Some(10));
        assert_eq!(parse_event_epoch("not a date"), None);
    }
}
