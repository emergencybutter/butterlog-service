//! Analyses telemetry sessions as they end, before compaction moves their chunks out of
//! Postgres: their flights go to `telemetry_flights`, each beside the same flight as the
//! app uploaded it the old way and how the two compare (rollout phase 3's parity check).
//!
//! A session is analysed once per [`ANALYSER_VERSION`]. A failure is recorded with the
//! version too, so it doesn't hold compaction back; bumping the version retries it.
//! Sessions already compacted are analysed again from their segment in R2, a few at a time
//! ([`analyse_compacted`]).
//!
//! A session that picks up the flight of the one before it, after a client restart
//! (`analysis/resume.rs`), is analysed with it as one stream, and the stream's flights are
//! stored under its first session.

use super::flight::Endpoint;
use super::{analyse, FlightSummary, Places};
use crate::error::AppError;
use crate::normalise::{normalise, Sample};
use crate::sessions::SegmentStore;
use butterlog_wire::{decode_segment, decompress, SessionDecoder, SessionHeader, SessionOpen};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::OnceLock;
use uuid::Uuid;

/// Bump when the analysis changes, to analyse sessions again.
pub const ANALYSER_VERSION: i32 = 6;

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

/// Each flight, its summary as stored, and its statistics document
/// (`analysis/statistics.rs`).
type Analysed = Vec<(FlightSummary, Value, Value)>;

/// One session, decoded.
struct Part {
    header: SessionHeader,
    samples: Vec<Sample>,
}

/// A session's chunks as uploaded (zstd each, in order), decoded.
fn decode_chunks(open: &[u8], chunks: &[Vec<u8>]) -> Result<Part, String> {
    let payloads = chunks.iter().map(|c| decompress(c).map_err(|e| e.to_string())).collect::<Result<Vec<_>, _>>()?;
    decode_payloads(open, &payloads)
}

/// A compacted session's segment, as stored in R2, decoded.
fn decode_stored_segment(compressed: &[u8]) -> Result<Part, String> {
    let raw = decompress(compressed).map_err(|e| e.to_string())?;
    let segment = decode_segment(&raw).map_err(|e| e.to_string())?;
    let payloads: Vec<Vec<u8>> = segment.chunks.into_iter().map(|(_, p)| p).collect();
    decode_payloads(&segment.open, &payloads)
}

/// Decodes a session's chunk payloads, in order, into samples.
fn decode_payloads(open: &[u8], payloads: &[Vec<u8>]) -> Result<Part, String> {
    let (open, _) = SessionOpen::decode(open).map_err(|e| e.to_string())?;
    let mut decoder = SessionDecoder::new(open.channels.clone());
    let mut ticks = Vec::new();
    for payload in payloads {
        ticks.extend(decoder.decode(payload).map_err(|e| e.to_string())?.ticks);
    }
    let samples = normalise(&open, ticks).map_err(|e| e.to_string())?;
    Ok(Part { header: open.header, samples })
}

/// The flights of a stream of samples, each as stored. `header` is its first session's.
fn analyse_samples(header: &SessionHeader, samples: &[Sample]) -> Result<Analysed, String> {
    let places = places().ok_or("airport data is not loaded")?;
    Ok(analyse(samples, places)
        .into_iter()
        .map(|f| {
            let json = summary_json(&f, places);
            let statistics = super::statistics::statistics(header, &f, samples, places);
            (f, json, statistics)
        })
        .collect())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, AppError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| AppError::Storage(e.to_string()))
}

/// A session's samples, from its chunks in Postgres or, once compacted, its segment. An
/// `Err` inside is the session's own problem, not the database's.
async fn load(db: &PgPool, segments: &impl SegmentStore, id: Uuid) -> Result<Result<Part, String>, AppError> {
    let (status, open, key): (String, Vec<u8>, Option<String>) =
        sqlx::query_as("SELECT status, open_blob, segment_key FROM telemetry_sessions WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await?;
    if status != "compacted" {
        let chunks: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT data FROM telemetry_chunks WHERE session_id = $1 ORDER BY seq")
                .bind(id)
                .fetch_all(db)
                .await?;
        return blocking(move || decode_chunks(&open, &chunks)).await;
    }
    match key {
        // Compacted with no chunks: no segment.
        None => blocking(move || decode_payloads(&open, &[])).await,
        Some(key) => match segments.get(&key).await {
            Ok(bytes) => blocking(move || decode_stored_segment(&bytes)).await,
            Err(e) => Ok(Err(e)),
        },
    }
}

/// The longest chain of sessions analysed as one stream.
const MAX_CHAIN: usize = 50;

/// The sessions `id` continues, first to last, ending with `id`.
async fn chain_to(db: &PgPool, id: Uuid) -> Result<Vec<Uuid>, AppError> {
    let mut chain = vec![id];
    loop {
        let at = *chain.last().expect("not empty");
        let previous: Option<Uuid> = sqlx::query_scalar("SELECT continues FROM telemetry_sessions WHERE id = $1")
            .bind(at)
            .fetch_one(db)
            .await?;
        match previous {
            Some(p) if !chain.contains(&p) && chain.len() < MAX_CHAIN => chain.push(p),
            _ => break,
        }
    }
    chain.reverse();
    Ok(chain)
}

/// Sessions analysed as one: their ids, first to last, their samples joined, and the
/// first one's header.
struct Stream {
    ids: Vec<Uuid>,
    samples: Vec<Sample>,
    header: SessionHeader,
}

/// The stream session `id` continues, if it does (`analysis/resume.rs`): among the
/// user's latest sessions before it that no other session continues, the first in the
/// same aircraft whose stream ends in the air where this one starts.
async fn resumed(db: &PgPool, segments: &impl SegmentStore, id: Uuid, session: &Part) -> Result<Option<Stream>, AppError> {
    let candidates: Vec<(Uuid, Value)> = sqlx::query_as(
        "SELECT s.id, s.aircraft FROM telemetry_sessions s JOIN telemetry_sessions x ON x.id = $1 \
          WHERE s.user_id = x.user_id AND s.started_at < x.started_at AND s.id <> x.id \
            AND s.chunk_count > 0 AND s.status IN ('ended', 'compacted') \
            AND s.invalid_seq IS NULL AND s.next_seq = s.chunk_count \
            AND NOT EXISTS (SELECT 1 FROM telemetry_sessions c WHERE c.continues = s.id AND c.id <> x.id) \
          ORDER BY s.started_at DESC LIMIT $2",
    )
    .bind(id)
    .bind(super::resume::CANDIDATES)
    .fetch_all(db)
    .await?;
    let Some(places) = places() else { return Ok(None) };
    'candidates: for (candidate, aircraft) in candidates {
        if aircraft["title"].as_str() != Some(session.header.aircraft.title.as_str()) {
            continue;
        }
        let ids = chain_to(db, candidate).await?;
        let mut samples = Vec::new();
        let mut header = None;
        for &part in &ids {
            match load(db, segments, part).await? {
                Ok(p) => {
                    header.get_or_insert(p.header);
                    super::resume::join(&mut samples, p.samples);
                }
                Err(e) => {
                    tracing::warn!(session = %part, "telemetry session not loaded to resume from: {e}");
                    continue 'candidates;
                }
            }
        }
        let (samples, flights) = blocking(move || {
            let flights = analyse(&samples, places);
            (samples, flights)
        })
        .await?;
        if super::resume::continues(&samples, &flights, &session.samples) {
            let header = header.expect("a chain has a session");
            return Ok(Some(Stream { ids, samples, header }));
        }
    }
    Ok(None)
}

/// Analyses a session as part of its stream: the sessions it continues, itself, and those
/// that continue it. The stream's flights are stored under its first session.
async fn analyse_session(db: &PgPool, segments: &impl SegmentStore, id: Uuid, user_id: i64) -> Result<(), AppError> {
    let session = match load(db, segments, id).await? {
        Ok(p) => p,
        Err(e) => return store(db, id, &[id], None, user_id, Err(e)).await,
    };
    let mut stream = match resumed(db, segments, id, &session).await? {
        Some(s) => s,
        None => Stream { ids: Vec::new(), samples: Vec::new(), header: session.header.clone() },
    };
    let continues = stream.ids.last().copied();
    stream.ids.push(id);
    super::resume::join(&mut stream.samples, session.samples);
    // Those that continue it, as they were linked.
    loop {
        let tail = *stream.ids.last().expect("the session itself");
        let next: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM telemetry_sessions \
              WHERE continues = $1 AND status IN ('ended', 'compacted') \
                AND invalid_seq IS NULL AND next_seq = chunk_count \
              ORDER BY started_at LIMIT 1",
        )
        .bind(tail)
        .fetch_optional(db)
        .await?;
        let Some(next) = next.filter(|n| !stream.ids.contains(n) && stream.ids.len() < MAX_CHAIN) else { break };
        match load(db, segments, next).await? {
            Ok(p) => {
                super::resume::join(&mut stream.samples, p.samples);
                stream.ids.push(next);
            }
            Err(e) => {
                tracing::warn!(session = %next, "telemetry session not loaded to continue: {e}");
                break;
            }
        }
    }
    let Stream { ids, samples, header } = stream;
    let analysed = blocking(move || analyse_samples(&header, &samples)).await?;
    store(db, id, &ids, continues, user_id, analysed).await
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

/// The old-path flight this one is, by takeoff time (the app's is the roll's), or start
/// time with no takeoff.
fn same_flight<'a>(f: &FlightSummary, candidates: &'a [(i64, Value)]) -> Option<&'a (i64, Value)> {
    let (ours, key) = match f.takeoff_roll_ms.or(f.takeoff_ms) {
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
/// `{service, app}`, with the difference in seconds for times. The app's `takeoff_time` is
/// when its takeoff roll passed 45 kt, not liftoff, so it is compared with
/// `takeoff_roll_ms`.
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
        "takeoff_s": seconds(f.takeoff_roll_ms, &app["takeoff_time"]),
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
pub async fn analyse_ended(db: &PgPool, segments: &impl SegmentStore, limit: i64) -> Result<usize, AppError> {
    let ready: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, user_id FROM telemetry_sessions \
          WHERE status = 'ended' AND invalid_seq IS NULL AND next_seq = chunk_count \
            AND analyser_version IS DISTINCT FROM $1 \
          ORDER BY ended_at LIMIT $2",
    )
    .bind(ANALYSER_VERSION)
    .bind(limit)
    .fetch_all(db)
    .await?;

    for (id, user_id) in &ready {
        analyse_session(db, segments, *id, *user_id).await?;
    }
    Ok(ready.len())
}

/// Analyses up to `limit` compacted sessions again from their segments when the analyser
/// version changed since, oldest first: a session resumes the stream of those before it,
/// whose links must be made first. A session compacted with no chunks has no segment and
/// no flights.
pub async fn analyse_compacted(db: &PgPool, segments: &impl SegmentStore, limit: i64) -> Result<usize, AppError> {
    let stale: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT id, user_id FROM telemetry_sessions \
          WHERE status = 'compacted' AND analyser_version IS DISTINCT FROM $1 \
          ORDER BY started_at LIMIT $2",
    )
    .bind(ANALYSER_VERSION)
    .bind(limit)
    .fetch_all(db)
    .await?;

    for (id, user_id) in &stale {
        // Analysed since the batch was read, with another session's stream.
        let version: Option<i32> = sqlx::query_scalar("SELECT analyser_version FROM telemetry_sessions WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await?;
        if version != Some(ANALYSER_VERSION) {
            analyse_session(db, segments, *id, *user_id).await?;
        }
    }
    Ok(stale.len())
}

/// Replaces the flights of a stream of sessions, `ids` first to last, with this analysis,
/// stored under the first; or records why it failed. `session` is the one analysed, which
/// `continues` the stream before it.
async fn store(
    db: &PgPool,
    session: Uuid,
    ids: &[Uuid],
    continues: Option<Uuid>,
    user_id: i64,
    analysed: Result<Analysed, String>,
) -> Result<(), AppError> {
    let id = ids[0];
    let mut tx = db.begin().await?;
    // One analysis of these sessions at a time stores its flights.
    sqlx::query("SELECT id FROM telemetry_sessions WHERE id = ANY($1) ORDER BY id FOR UPDATE")
        .bind(ids)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM telemetry_flights WHERE session_id = ANY($1)").bind(ids).execute(&mut *tx).await?;
    sqlx::query("UPDATE telemetry_sessions SET continues = $2 WHERE id = $1")
        .bind(session)
        .bind(continues)
        .execute(&mut *tx)
        .await?;
    let error = match analysed {
        Ok(flights) => {
            for (idx, (f, summary, statistics)) in flights.iter().enumerate() {
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
                        departure, arrival, summary, flight_id, parity, statistics) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
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
                .bind(statistics)
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
            SET analyser_version = $2, analysis_error = $3, analysed_at = NOW() WHERE id = ANY($1)",
    )
    .bind(ids)
    .bind(ANALYSER_VERSION)
    .bind(error)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::TestSegments;

    fn flight(takeoff: Option<i64>, first: i64) -> FlightSummary {
        FlightSummary {
            departure: Endpoint::Airport("KAST".into()),
            arrival: Endpoint::Airport("KAST".into()),
            first_ms: first,
            last_ms: first + 600_000,
            takeoff_ms: takeoff,
            takeoff_roll_ms: takeoff.map(|t| t - 20_000),
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
        // The roll passed 45 kt 20 s before liftoff; the app's takeoff_time is that.
        let p = parity(&flight(Some(t + 20_500), t), &app);
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
            "takeoff_time": "2026-11-01 01:09:39.068",
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
            assert!(analyse_ended(&db, &TestSegments, 100).await.unwrap() > 0);
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
        let statistics: Value =
            sqlx::query_scalar("SELECT statistics FROM telemetry_flights WHERE session_id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(statistics["arrival"]["name"], json!("Astoria Regional Airport"));
        assert_eq!(linked, Some(flight));
        assert!(parity["takeoff_s"].as_f64().unwrap().abs() < 1.5);
        assert_eq!(parity["landing_score"]["app"], json!(-142.24));

        // Compacted, then analysed by an older version: analysed again from its segment.
        let segments = TestSegments;
        while sqlx::query_scalar::<_, String>("SELECT status FROM telemetry_sessions WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap()
            != "compacted"
        {
            assert!(crate::sessions::compact_ready(&db, &segments, 100).await.unwrap() > 0);
        }
        sqlx::query("UPDATE telemetry_sessions SET analyser_version = $2 WHERE id = $1")
            .bind(id)
            .bind(ANALYSER_VERSION - 1)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM telemetry_flights WHERE session_id = $1").bind(id).execute(&db).await.unwrap();
        while version().await.unwrap().0 != Some(ANALYSER_VERSION) {
            assert!(analyse_compacted(&db, &segments, 100).await.unwrap() > 0);
        }
        assert_eq!(version().await.unwrap().1, None);
        let again: (String, Option<i64>) =
            sqlx::query_as("SELECT departure, flight_id FROM telemetry_flights WHERE session_id = $1")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(again, ("KAST".to_string(), Some(flight)));
    }

    /// A chunk payload with its sequence number changed (`format u8 | seq varint | ...`),
    /// and whether it is a keyframe.
    fn renumbered(payload: &[u8], seq: u32) -> (Vec<u8>, bool) {
        let mut at = 1;
        while payload[at] & 0x80 != 0 {
            at += 1;
        }
        at += 1;
        let mut out = vec![payload[0]];
        let mut v = seq;
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        out.extend_from_slice(&payload[at..]);
        (out, payload[at] == 1)
    }

    /// The X-Plane pattern, as if the app restarted in the air: one session up to a
    /// keyframe halfway through, the next from it. One flight, under the first.
    #[tokio::test]
    async fn a_session_after_a_restart_in_the_air_continues_the_flight() {
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

        let path = format!("{}/tests/fixtures/xplane_pattern.blsg.zst", env!("CARGO_MANIFEST_DIR"));
        let segment = butterlog_wire::decode_segment(&decompress(&std::fs::read(path).unwrap()).unwrap()).unwrap();
        let keyframes: Vec<usize> =
            (1..segment.chunks.len()).filter(|&i| renumbered(&segment.chunks[i].1, 0).1).collect();
        let cut = keyframes[keyframes.len() / 2];
        let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
        open_session(&db, user, first, &segment.open).await.unwrap();
        for (seq, payload) in &segment.chunks[..cut] {
            store_chunk(&db, user, first, *seq, &butterlog_wire::compress(payload, 3)).await.unwrap();
        }
        end_session(&db, user, first).await.unwrap();
        open_session(&db, user, second, &segment.open).await.unwrap();
        for (seq, (_, payload)) in segment.chunks[cut..].iter().enumerate() {
            let (payload, _) = renumbered(payload, seq as u32);
            store_chunk(&db, user, second, seq as u32, &butterlog_wire::compress(&payload, 3)).await.unwrap();
        }
        end_session(&db, user, second).await.unwrap();

        let segments = TestSegments;
        let analysed = |id: Uuid| {
            sqlx::query_scalar::<_, Option<i32>>("SELECT analyser_version FROM telemetry_sessions WHERE id = $1")
                .bind(id)
                .fetch_one(&db)
        };
        while analysed(second).await.unwrap() != Some(ANALYSER_VERSION) {
            assert!(analyse_ended(&db, &segments, 100).await.unwrap() > 0);
        }
        let continues: Option<Uuid> = sqlx::query_scalar("SELECT continues FROM telemetry_sessions WHERE id = $1")
            .bind(second)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(continues, Some(first));
        let flights = || {
            sqlx::query_as::<_, (Uuid, String, String)>(
                "SELECT session_id, departure, arrival FROM telemetry_flights WHERE session_id = ANY($1)",
            )
            .bind([first, second])
            .fetch_all(&db)
        };
        assert_eq!(flights().await.unwrap(), [(first, "KAST".to_string(), "KAST".to_string())]);

        // Compacted, and analysed again from the segments by the later session: the same.
        while sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM telemetry_sessions WHERE id = ANY($1) AND status <> 'compacted'",
        )
        .bind([first, second])
        .fetch_one(&db)
        .await
        .unwrap()
            > 0
        {
            assert!(crate::sessions::compact_ready(&db, &segments, 100).await.unwrap() > 0);
        }
        sqlx::query("UPDATE telemetry_sessions SET analyser_version = $2 WHERE id = ANY($1)")
            .bind([first, second])
            .bind(ANALYSER_VERSION - 1)
            .execute(&db)
            .await
            .unwrap();
        analyse_session(&db, &segments, second, user).await.unwrap();
        assert_eq!(analysed(first).await.unwrap(), Some(ANALYSER_VERSION));
        assert_eq!(flights().await.unwrap(), [(first, "KAST".to_string(), "KAST".to_string())]);
    }
}
