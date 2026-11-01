//! Analyses telemetry sessions as they end, before compaction moves their chunks out of
//! Postgres: their flights go to `telemetry_flights`, each beside the same flight as the
//! app uploaded it the old way and how the two compare (rollout phase 3's parity check).
//!
//! A session is analysed once per [`ANALYSER_VERSION`]. A failure is recorded with the
//! version too, so it doesn't hold compaction back; bumping the version retries it.

use super::flight::Endpoint;
use super::{analyse, FlightSummary, Places};
use crate::error::AppError;
use crate::normalise::normalise;
use butterlog_wire::{decompress, SessionDecoder, SessionOpen};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::OnceLock;
use uuid::Uuid;

/// Bump when the analysis changes, to analyse sessions again.
pub const ANALYSER_VERSION: i32 = 1;

/// Old-path flights within this of a flight's takeoff (or start, with no takeoff) are
/// the same flight.
const SAME_FLIGHT_MS: i64 = 5 * 60_000;

/// Airports and runways, loaded on first use from `BUTTERLOG_DATA_DIR` (default `data`,
/// beside the binary in the image). `None` if they don't load.
fn places() -> Option<&'static Places> {
    static PLACES: OnceLock<Option<Places>> = OnceLock::new();
    PLACES
        .get_or_init(|| {
            let dir = PathBuf::from(std::env::var("BUTTERLOG_DATA_DIR").unwrap_or_else(|_| "data".into()));
            match Places::load(&dir) {
                Ok(places) => Some(places),
                Err(e) => {
                    tracing::error!(dir = %dir.display(), "airport data did not load: {e}");
                    None
                }
            }
        })
        .as_ref()
}

/// Decodes a session's chunk payloads (zstd, in order) and analyses its flights, each as
/// stored.
fn analyse_session(open: &[u8], chunks: &[Vec<u8>]) -> Result<Vec<(FlightSummary, Value)>, String> {
    let places = places().ok_or("airport data is not loaded")?;
    let (open, _) = SessionOpen::decode(open).map_err(|e| e.to_string())?;
    let mut decoder = SessionDecoder::new(open.channels.clone());
    let mut ticks = Vec::new();
    for data in chunks {
        let payload = decompress(data).map_err(|e| e.to_string())?;
        ticks.extend(decoder.decode(&payload).map_err(|e| e.to_string())?.ticks);
    }
    let samples = normalise(&open, ticks).map_err(|e| e.to_string())?;
    Ok(analyse(&samples, places).into_iter().map(|f| {
        let json = summary_json(&f, places);
        (f, json)
    }).collect())
}

/// The app's timestamps: UTC, "2026-11-01 01:09:47.776".
fn app_time(v: &Value) -> Option<i64> {
    let s = v.as_str()?;
    let t = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").ok()?;
    Some(t.and_utc().timestamp_millis())
}

fn timestamp(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

/// The old-path flight this one is, by takeoff time, or start time with no takeoff.
fn same_flight<'a>(f: &FlightSummary, candidates: &'a [(i64, Value)]) -> Option<&'a (i64, Value)> {
    let (ours, key) = match f.takeoff_ms {
        Some(t) => (t, "takeoff_time"),
        None => (f.first_ms, "start_time"),
    };
    candidates
        .iter()
        .filter_map(|c| Some((c, (app_time(&c.1[key])? - ours).abs())))
        .filter(|(_, gap)| *gap <= SAME_FLIGHT_MS)
        .min_by_key(|(_, gap)| *gap)
        .map(|(c, _)| c)
}

/// What the service made of a flight beside what the app did: each entry is
/// `{service, app}`, with the difference in seconds for times.
fn parity(f: &FlightSummary, app: &Value) -> Value {
    let pair = |service: Value, app: &Value| json!({ "service": service, "app": app });
    let seconds = |ours: Option<i64>, theirs: &Value| match (ours, app_time(theirs)) {
        (Some(a), Some(b)) => json!((a - b) as f64 / 1000.0),
        _ => Value::Null,
    };
    let landing = f.landing.as_ref();
    let max_alt = &app["max_entries"]["AltMSL"];
    json!({
        "departure": pair(json!(f.departure.to_string()), &app["departure"]["icao"]),
        "arrival": pair(json!(f.arrival.to_string()), &app["arrival"]["icao"]),
        "takeoff_s": seconds(f.takeoff_ms, &app["takeoff_time"]),
        "landing_s": seconds(landing.map(|l| l.wall_time_ms), &app["landing_time"]),
        "max_altitude_ft": pair(json!(f.max_altitude_ft), max_alt),
        "landing_score": pair(json!(landing.map(|l| l.score())), &app["landing_score"]),
        "landing_offset_percent": pair(json!(landing.and_then(|l| l.offset_percent)), &app["landing_offset_percent"]),
        "landing_threshold_dist_ft": pair(json!(landing.and_then(|l| l.threshold_dist_ft)), &app["landing_threshold_dist_ft"]),
        "vs_variance": pair(json!(landing.and_then(|l| l.vs_variance)), &app["vs_variance"]),
        "ias_variance": pair(json!(landing.and_then(|l| l.ias_variance)), &app["ias_variance"]),
    })
}

/// The summary as stored, with what is looked up or computed: the endpoints' names and
/// the landing's score.
fn summary_json(f: &FlightSummary, places: &Places) -> Value {
    let mut v = serde_json::to_value(f).unwrap_or(Value::Null);
    let name = |e: &Endpoint| match e {
        Endpoint::Airport(ident) => places.airport(ident).map(|a| json!(a.name)),
        _ => None,
    };
    v["departure_name"] = name(&f.departure).unwrap_or(Value::Null);
    v["arrival_name"] = name(&f.arrival).unwrap_or(Value::Null);
    if let (Some(landing), Some(obj)) = (&f.landing, v["landing"].as_object_mut()) {
        obj.insert("score".into(), json!(landing.score()));
    }
    v
}

/// Analyses up to `limit` ended sessions whose chunks are all in and decoded, and that
/// this analyser version hasn't seen.
pub async fn analyse_ended(db: &PgPool, limit: i64) -> Result<usize, AppError> {
    let ready: Vec<(Uuid, i64, Vec<u8>)> = sqlx::query_as(
        "SELECT id, user_id, open_blob FROM telemetry_sessions \
          WHERE status = 'ended' AND invalid_seq IS NULL AND next_seq = chunk_count \
            AND analyser_version IS DISTINCT FROM $1 \
          ORDER BY ended_at LIMIT $2",
    )
    .bind(ANALYSER_VERSION)
    .bind(limit)
    .fetch_all(db)
    .await?;

    for (id, user_id, open) in &ready {
        let chunks: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT data FROM telemetry_chunks WHERE session_id = $1 ORDER BY seq")
                .bind(id)
                .fetch_all(db)
                .await?;
        let open = open.clone();
        let analysed = tokio::task::spawn_blocking(move || analyse_session(&open, &chunks))
            .await
            .map_err(|e| AppError::Storage(e.to_string()))?;

        let mut tx = db.begin().await?;
        sqlx::query("DELETE FROM telemetry_flights WHERE session_id = $1").bind(id).execute(&mut *tx).await?;
        let error = match analysed {
            Ok(flights) => {
                for (idx, (f, summary)) in flights.iter().enumerate() {
                    // The app's flights of the day around it.
                    let candidates: Vec<(i64, Value)> = sqlx::query_as(
                        "SELECT id, statistics FROM flights \
                          WHERE user_id = $1 AND created_at BETWEEN $2 - INTERVAL '1 day' AND $3 + INTERVAL '1 day'",
                    )
                    .bind(user_id)
                    .bind(timestamp(f.first_ms))
                    .bind(timestamp(f.last_ms))
                    .fetch_all(&mut *tx)
                    .await?;
                    let app = same_flight(f, &candidates);
                    sqlx::query(
                        "INSERT INTO telemetry_flights \
                           (session_id, idx, analyser_version, first_at, last_at, takeoff_at, \
                            departure, arrival, summary, flight_id, parity) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                    )
                    .bind(id)
                    .bind(idx as i32)
                    .bind(ANALYSER_VERSION)
                    .bind(timestamp(f.first_ms))
                    .bind(timestamp(f.last_ms))
                    .bind(f.takeoff_ms.map(timestamp))
                    .bind(f.departure.to_string())
                    .bind(f.arrival.to_string())
                    .bind(summary)
                    .bind(app.map(|a| a.0))
                    .bind(app.map(|a| parity(f, &a.1)))
                    .execute(&mut *tx)
                    .await?;
                }
                None
            }
            Err(e) => {
                tracing::error!(session = %id, "telemetry session analysis failed: {e}");
                Some(e)
            }
        };
        sqlx::query(
            "UPDATE telemetry_sessions \
                SET analyser_version = $2, analysis_error = $3, analysed_at = NOW() WHERE id = $1",
        )
        .bind(id)
        .bind(ANALYSER_VERSION)
        .bind(error)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok(ready.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flight(takeoff: Option<i64>, first: i64) -> FlightSummary {
        FlightSummary {
            departure: Endpoint::Airport("KAST".into()),
            arrival: Endpoint::Airport("KAST".into()),
            first_ms: first,
            last_ms: first + 600_000,
            takeoff_ms: takeoff,
            max_altitude_ft: 1000.0,
            max_ground_speed_kt: 100.0,
            fuel_consumed_gal: 1.0,
            events: vec![],
            landing: None,
        }
    }

    #[test]
    fn matches_the_old_path_flight_by_takeoff() {
        let t = app_time(&json!("2026-11-01 01:09:47.776")).unwrap();
        let candidates = vec![
            (1, json!({ "takeoff_time": "2026-10-31 18:00:00.000" })),
            (2, json!({ "takeoff_time": "2026-11-01 01:09:47.776", "start_time": "2026-11-01 01:08:56.650" })),
            (3, json!({ "start_time": "2026-11-01 01:10:00" })),
        ];
        assert_eq!(same_flight(&flight(Some(t + 400), t - 50_000), &candidates).unwrap().0, 2);
        // No takeoff: by start, within five minutes.
        assert_eq!(same_flight(&flight(None, t + 60_000), &candidates).unwrap().0, 3);
        assert!(same_flight(&flight(Some(t + 3_600_000), t), &candidates).is_none());
    }

    #[test]
    fn parity_pairs_each_figure() {
        let t = app_time(&json!("2026-11-01 01:09:47.776")).unwrap();
        let app = json!({
            "departure": { "icao": "KAST" },
            "arrival": { "icao": "KAST" },
            "takeoff_time": "2026-11-01 01:09:47.776",
            "max_entries": { "AltMSL": 918.26 },
        });
        let p = parity(&flight(Some(t + 500), t), &app);
        assert_eq!(p["departure"], json!({ "service": "KAST", "app": "KAST" }));
        assert_eq!(p["takeoff_s"], json!(0.5));
        assert_eq!(p["max_altitude_ft"]["app"], json!(918.26));
        assert_eq!(p["landing_s"], Value::Null);
    }

    /// Against a real Postgres, given by `TEST_DATABASE_URL` (skipped without it).
    #[tokio::test]
    async fn analyses_an_ended_session_beside_the_apps_flight() {
        use crate::sessions::{end_session, open_session, store_chunk};
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return };
        let db = PgPool::connect(&url).await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        let user: i64 = sqlx::query_scalar(
            "INSERT INTO users (discord_id, username) VALUES ($1, 'test') RETURNING id",
        )
        .bind(Uuid::new_v4().to_string())
        .fetch_one(&db)
        .await
        .unwrap();

        // The X-Plane pattern, uploaded as the app would, and the app's old-path flight.
        let path = format!("{}/tests/fixtures/xplane_pattern.blsg.zst", env!("CARGO_MANIFEST_DIR"));
        let segment = butterlog_wire::decode_segment(&decompress(&std::fs::read(path).unwrap()).unwrap()).unwrap();
        let id = Uuid::new_v4();
        open_session(&db, user, id, &segment.open).await.unwrap();
        for (seq, payload) in &segment.chunks {
            store_chunk(&db, user, id, *seq, &butterlog_wire::compress(payload, 3)).await.unwrap();
        }
        end_session(&db, user, id).await.unwrap();
        let flight: i64 = sqlx::query_scalar(
            "INSERT INTO flights (user_id, departure, arrival, statistics, created_at) \
             VALUES ($1, 'KAST', 'KAST', $2, '2026-11-01 01:09:00+00') RETURNING id",
        )
        .bind(user)
        .bind(json!({
            "departure": { "icao": "KAST" },
            "arrival": { "icao": "KAST" },
            "takeoff_time": "2026-11-01 01:09:47.776",
            "landing_score": -142.24,
        }))
        .fetch_one(&db)
        .await
        .unwrap();

        // Other tests' sessions may be ready too.
        let version = || {
            sqlx::query_as::<_, (Option<i32>, Option<String>)>(
                "SELECT analyser_version, analysis_error FROM telemetry_sessions WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&db)
        };
        while version().await.unwrap().0.is_none() {
            assert!(analyse_ended(&db, 100).await.unwrap() > 0);
        }
        assert_eq!(version().await.unwrap(), (Some(ANALYSER_VERSION), None));

        let (departure, summary, linked, parity): (String, Value, Option<i64>, Value) = sqlx::query_as(
            "SELECT departure, summary, flight_id, parity FROM telemetry_flights WHERE session_id = $1",
        )
        .bind(id)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(departure, "KAST");
        assert_eq!(summary["departure_name"], json!("Astoria Regional Airport"));
        assert!(summary["landing"]["score"].is_number());
        assert_eq!(linked, Some(flight));
        assert!(parity["takeoff_s"].as_f64().unwrap().abs() < 1.5);
        assert_eq!(parity["landing_score"]["app"], json!(-142.24));

        // Analysed once per version.
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM telemetry_sessions WHERE id = $1 AND analysed_at IS NOT NULL")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap(),
            1
        );
    }
}
