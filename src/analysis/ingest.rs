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

use super::flight::{Endpoint, FlightStream};
use super::statistics::FlightStats;
use super::resume::{position, Position};
use super::{FlightSummary, Places};
use crate::error::AppError;
use crate::normalise::{normalise, Normaliser, Sample};
use crate::sessions::SegmentStore;
use crate::track::DecodedPoint;
use butterlog_wire::{decode_segment, decompress, SessionDecoder, SessionHeader, SessionOpen};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::OnceLock;
use uuid::Uuid;

/// Bump when the analysis changes, to analyse sessions again.
pub const ANALYSER_VERSION: i32 = 11;

/// Old-path flights within this of a flight's takeoff (or start, with no takeoff) are
/// the same flight.
const SAME_FLIGHT_MS: i64 = 5 * 60_000;

/// A flight the service owns holds its own analysis's times, so the same flight analysed
/// again lands within this; another takeoff minutes later, a new session after a reload
/// or a second circuit, is another flight.
const SAME_ANALYSIS_MS: i64 = 5_000;

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

/// Each flight, its summary as stored, its statistics document
/// (`analysis/statistics.rs`) and its track.
type Analysed = Vec<(FlightSummary, Value, Value, Vec<DecodedPoint>)>;

/// A few of a session's chunks as uploaded (zstd each, in order), decoded: those the tail
/// adds to a flight. A whole session is decoded a chunk at a time ([`each_sample`]).
fn decode_chunks(open: &[u8], chunks: &[Vec<u8>]) -> Result<Vec<Sample>, String> {
    let payloads = chunks.iter().map(|c| decompress(c).map_err(|e| e.to_string())).collect::<Result<Vec<_>, _>>()?;
    decode_payloads(open, &payloads)
}

/// Decodes a session's chunk payloads, in order, into samples.
fn decode_payloads(open: &[u8], payloads: &[Vec<u8>]) -> Result<Vec<Sample>, String> {
    let (open, _) = SessionOpen::decode(open).map_err(|e| e.to_string())?;
    let mut decoder = SessionDecoder::new(open.channels.clone());
    let mut ticks = Vec::new();
    for payload in payloads {
        ticks.extend(decoder.decode(payload).map_err(|e| e.to_string())?.ticks);
    }
    normalise(&open, ticks).map_err(|e| e.to_string())
}

/// Flights as stored, from what the stream gathered of each.
fn analysed(header: &SessionHeader, flights: Vec<(FlightSummary, FlightStats)>, places: &Places) -> Analysed {
    flights
        .into_iter()
        .map(|(f, stats)| {
            let json = summary_json(&f, places);
            let statistics = super::statistics::document(header, &f, &stats, places);
            (f, json, statistics, stats.track)
        })
        .collect()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, AppError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| AppError::Storage(e.to_string()))
}

/// A session's telemetry as stored, still compressed: its open blob and chunks while they
/// are in Postgres, or its segment once compacted. Small beside its samples, which are
/// decoded from it a chunk at a time ([`each_sample`]).
enum Stored {
    Chunks { open: Vec<u8>, chunks: Vec<Vec<u8>> },
    Segment(Vec<u8>),
}

/// A session's telemetry, from its chunks in Postgres or, once compacted, its segment. An
/// `Err` inside is the session's own problem, not the database's.
async fn fetch(db: &PgPool, segments: &impl SegmentStore, id: Uuid) -> Result<Result<Stored, String>, AppError> {
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
        return Ok(Ok(Stored::Chunks { open, chunks }));
    }
    match key {
        // Compacted with no chunks: no segment.
        None => Ok(Ok(Stored::Chunks { open, chunks: Vec::new() })),
        Some(key) => Ok(segments.get(&key).await.map(Stored::Segment)),
    }
}

/// Decodes a stored session a chunk at a time, handing `each` its samples in order until
/// it breaks, and returns the session's header. Only one chunk's ticks are held at once.
fn each_sample(stored: &Stored, mut each: impl FnMut(Sample) -> ControlFlow<()>) -> Result<SessionHeader, String> {
    fn feed(
        open: &[u8],
        payloads: impl Iterator<Item = Result<Vec<u8>, String>>,
        each: &mut impl FnMut(Sample) -> ControlFlow<()>,
    ) -> Result<SessionHeader, String> {
        let (open, _) = SessionOpen::decode(open).map_err(|e| e.to_string())?;
        let mut decoder = SessionDecoder::new(open.channels.clone());
        let mut normaliser = Normaliser::new(&open).map_err(|e| e.to_string())?;
        'chunks: for payload in payloads {
            for tick in decoder.decode(&payload?).map_err(|e| e.to_string())?.ticks {
                if each(normaliser.sample(tick)).is_break() {
                    break 'chunks;
                }
            }
        }
        Ok(open.header)
    }
    match stored {
        Stored::Chunks { open, chunks } => {
            feed(open, chunks.iter().map(|c| decompress(c).map_err(|e| e.to_string())), &mut each)
        }
        Stored::Segment(compressed) => {
            let raw = decompress(compressed).map_err(|e| e.to_string())?;
            let segment = decode_segment(&raw).map_err(|e| e.to_string())?;
            feed(&segment.open, segment.chunks.into_iter().map(|(_, p)| Ok(p)), &mut each)
        }
    }
}

/// A stored session's header and where its first sample with a position has the aircraft,
/// decoding no further than that.
fn first_position(stored: &Stored) -> Result<(SessionHeader, Option<Position>), String> {
    let mut first = None;
    let header = each_sample(stored, |s| match position(&s) {
        Some(p) => {
            first = Some(p);
            ControlFlow::Break(())
        }
        None => ControlFlow::Continue(()),
    })?;
    Ok((header, first))
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

/// Sessions analysed as one, as they are added: their ids, first to last, the first one's
/// header, the stream's flights so far and where it last had the aircraft. Bounded by the
/// flights, not by how long they fly.
#[derive(Clone, Default)]
struct Stream {
    ids: Vec<Uuid>,
    header: Option<SessionHeader>,
    flights: FlightStream<FlightStats>,
    /// Whether any sample is in, and so whether the next session's first is the pause
    /// between the two (`resume::join`).
    started: bool,
    last_position: Option<Position>,
}

impl Stream {
    /// Adds session `id`'s samples, after a pause if the stream has some already: the
    /// time between the two, the app restarting, is not flying. All or nothing: a session
    /// that doesn't decode leaves the stream as it was.
    fn add(&mut self, id: Uuid, stored: &Stored) -> Result<(), String> {
        let mut next = self.clone();
        let mut pause = next.started;
        let header = each_sample(stored, |mut s| {
            if pause {
                s.flags |= butterlog_wire::flags::PAUSED;
                pause = false;
            }
            next.started = true;
            if let Some(p) = position(&s) {
                next.last_position = Some(p);
            }
            next.flights.push(&s);
            ControlFlow::Continue(())
        })?;
        next.header.get_or_insert(header);
        next.ids.push(id);
        *self = next;
        Ok(())
    }

    /// The stream's flights, each as stored.
    fn analysed(&self) -> Result<Analysed, String> {
        let places = places().ok_or("airport data is not loaded")?;
        let header = self.header.as_ref().ok_or("no session in the stream")?;
        Ok(analysed(header, self.flights.flights(places), places))
    }
}

/// Adds a session to `stream` off the async threads; the stream comes back either way.
async fn add(mut stream: Stream, id: Uuid, stored: Stored) -> Result<(Stream, Result<(), String>), AppError> {
    blocking(move || {
        let added = stream.add(id, &stored);
        (stream, added)
    })
    .await
}

/// The sessions `ids`, first to last, as one stream; or why one of them didn't load.
async fn stream_of(db: &PgPool, segments: &impl SegmentStore, ids: &[Uuid]) -> Result<Result<Stream, String>, AppError> {
    let mut stream = Stream::default();
    for &part in ids {
        let stored = match fetch(db, segments, part).await? {
            Ok(s) => s,
            Err(e) => return Ok(Err(e)),
        };
        let (next, added) = add(stream, part, stored).await?;
        if let Err(e) = added {
            return Ok(Err(e));
        }
        stream = next;
    }
    Ok(Ok(stream))
}

/// The stream session `id` continues, if it does (`analysis/resume.rs`): among the
/// user's latest sessions before it that no other session continues, the first in the
/// same aircraft whose stream ends in the air where this one starts, `first`.
async fn resumed(
    db: &PgPool,
    segments: &impl SegmentStore,
    id: Uuid,
    header: &SessionHeader,
    first: Option<Position>,
) -> Result<Option<Stream>, AppError> {
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
    for (candidate, aircraft) in candidates {
        if aircraft["title"].as_str() != Some(header.aircraft.title.as_str()) {
            continue;
        }
        let ids = chain_to(db, candidate).await?;
        let stream = match stream_of(db, segments, &ids).await? {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(session = %candidate, "telemetry session not loaded to resume from: {e}");
                continue;
            }
        };
        let (stream, continued) = blocking(move || {
            let flights: Vec<FlightSummary> = stream.flights.flights(places).into_iter().map(|(f, _)| f).collect();
            let continued = super::resume::continues(stream.last_position, &flights, first);
            (stream, continued)
        })
        .await?;
        if continued {
            return Ok(Some(stream));
        }
    }
    Ok(None)
}

/// Analyses a session as part of its stream: the sessions it continues, itself, and those
/// that continue it. The stream's flights are stored under its first session. Each
/// session is decoded a chunk at a time into the stream, so a long flight costs its
/// flights' statistics, not its samples.
async fn analyse_session(db: &PgPool, segments: &impl SegmentStore, id: Uuid, user_id: i64) -> Result<(), AppError> {
    let stored = match fetch(db, segments, id).await? {
        Ok(s) => s,
        Err(e) => return store(db, id, &[id], None, user_id, Err(e)).await,
    };
    let (stored, first) = blocking(move || {
        let first = first_position(&stored);
        (stored, first)
    })
    .await?;
    let (header, first) = match first {
        Ok(f) => f,
        Err(e) => return store(db, id, &[id], None, user_id, Err(e)).await,
    };
    let stream = resumed(db, segments, id, &header, first).await?.unwrap_or_default();
    let continues = stream.ids.last().copied();
    let (mut stream, added) = add(stream, id, stored).await?;
    if let Err(e) = added {
        return store(db, id, &[id], None, user_id, Err(e)).await;
    }
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
        let added = match fetch(db, segments, next).await? {
            Ok(stored) => {
                let (s, added) = add(stream, next, stored).await?;
                stream = s;
                added
            }
            Err(e) => Err(e),
        };
        if let Err(e) = added {
            tracing::warn!(session = %next, "telemetry session not loaded to continue: {e}");
            break;
        }
    }
    let ids = stream.ids.clone();
    let analysed = blocking(move || stream.analysed()).await?;
    store(db, id, &ids, continues, user_id, analysed).await
}

/// Makes a flight the analysis found the user's on the service (rollout phase 5): the
/// app's flight it matched, `linked`, takes the service's document, the app's kept aside
/// the first time; with none, the flight is created, with its track. Either way its
/// Discord post is due a sync. A flight not `over` yet, still flying in an open session,
/// is the service's own active flight, its track added to as it goes. Returns the flight.
async fn take_over(
    tx: &mut sqlx::PgConnection,
    user_id: i64,
    linked: Option<i64>,
    f: &FlightSummary,
    statistics: &Value,
    track: &[DecodedPoint],
    over: bool,
) -> Result<i64, AppError> {
    let arrival = f.landing.as_ref().map(|_| f.arrival.to_string());
    let (status, ended_at, end_reason) = match (over, f.landing.is_some()) {
        (false, _) => ("active", None, None),
        (true, true) => ("ended", Some(timestamp(f.last_ms)), Some("landed")),
        (true, false) => ("ended", Some(timestamp(f.last_ms)), Some("sim_closed")),
    };
    let (id, service_made): (i64, bool) = match linked {
        // How the app's flight ended is the app's; the service's own ends with its
        // analysis, which a later session continuing it can extend to a landing.
        Some(id) => sqlx::query_as(
            "UPDATE flights \
                SET app_statistics = CASE WHEN service_owned THEN app_statistics ELSE statistics END, \
                    statistics = $2, arrival = COALESCE($3, arrival), service_owned = TRUE, \
                    status = CASE WHEN service_made THEN $4 ELSE status END, \
                    ended_at = CASE WHEN service_made THEN $5 ELSE ended_at END, \
                    end_reason = CASE WHEN service_made THEN $6 ELSE end_reason END, \
                    created_at = CASE WHEN service_made THEN $7 ELSE created_at END, \
                    discord_sync_due = TRUE, updated_at = CURRENT_TIMESTAMP \
              WHERE id = $1 RETURNING id, service_made",
        )
        .bind(id)
        .bind(statistics)
        .bind(&arrival)
        .bind(status)
        .bind(ended_at)
        .bind(end_reason)
        .bind(timestamp(f.first_ms))
        .fetch_one(&mut *tx)
        .await?,
        None => sqlx::query_as(
            "INSERT INTO flights \
               (user_id, departure, arrival, statistics, created_at, updated_at, status, ended_at, \
                end_reason, service_owned, service_made, discord_sync_due) \
             VALUES ($1, $2, $3, $4, $5, CURRENT_TIMESTAMP, $6, $7, $8, TRUE, TRUE, TRUE) \
             RETURNING id, service_made",
        )
        .bind(user_id)
        .bind(f.departure.to_string())
        .bind(&arrival)
        .bind(statistics)
        .bind(timestamp(f.first_ms))
        .bind(status)
        .bind(ended_at)
        .bind(end_reason)
        .fetch_one(&mut *tx)
        .await?,
    };
    // The app's track, for a flight it made, stays; the service's own is the analysis's,
    // added to while it flies. Once it is over, the points stored past its end, while it
    // flew, go; only those missing are sent, not the whole track again.
    if service_made {
        if over {
            let (first, last) = (track.first().map_or(i64::MAX, |p| p.epoch), track.last().map_or(i64::MIN, |p| p.epoch));
            sqlx::query("DELETE FROM flight_track_points WHERE flight_id = $1 AND NOT (sample_epoch BETWEEN $2 AND $3)")
                .bind(id)
                .bind(first)
                .bind(last)
                .execute(&mut *tx)
                .await?;
        }
        let (lo, hi): (Option<i64>, Option<i64>) =
            sqlx::query_as("SELECT MIN(sample_epoch), MAX(sample_epoch) FROM flight_track_points WHERE flight_id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        let (before, after) = match (lo, hi) {
            (Some(lo), Some(hi)) => (track.partition_point(|p| p.epoch < lo), track.partition_point(|p| p.epoch <= hi)),
            _ => (0, 0),
        };
        if over {
            crate::track::insert_points(&mut *tx, id, &track[..before]).await?;
        }
        crate::track::insert_points(&mut *tx, id, &track[after..]).await?;
        sqlx::query(
            "UPDATE flights SET track_points = (SELECT COUNT(*) FROM flight_track_points WHERE flight_id = $1) \
              WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    // Its screenshots uploaded against the session: those taken while it flew.
    crate::screenshots::place(&mut *tx, user_id, id, timestamp(f.first_ms), ended_at).await?;
    Ok(id)
}

/// The user's flight an analysed one is, and what the app made of it; the flight made the
/// service's, or created, when the user's flights are the service's since before it began
/// (`since`, rollout phase 5).
async fn link(
    tx: &mut sqlx::PgConnection,
    user_id: i64,
    since: Option<DateTime<Utc>>,
    f: &FlightSummary,
    statistics: &Value,
    track: &[DecodedPoint],
    over: bool,
) -> Result<(Option<i64>, Option<Value>), AppError> {
    // The user's flights of the day around it, with what the app made of each.
    let rows: Vec<(i64, Value, bool, Option<Value>)> = sqlx::query_as(
        "SELECT id, statistics, service_owned, app_statistics FROM flights \
          WHERE user_id = $1 AND created_at BETWEEN $2 - INTERVAL '1 day' AND $3 + INTERVAL '1 day'",
    )
    .bind(user_id)
    .bind(timestamp(f.first_ms))
    .bind(timestamp(f.last_ms))
    .fetch_all(&mut *tx)
    .await?;
    let candidates: Vec<(i64, Value, bool)> = rows.iter().map(|r| (r.0, r.1.clone(), r.2)).collect();
    let linked = same_flight(f, &candidates).map(|c| c.0);
    // The app's document: its own, kept aside on a flight the service owns.
    let app_doc = linked
        .and_then(|id| rows.iter().find(|r| r.0 == id))
        .and_then(|r| if r.2 { r.3.clone() } else { Some(r.1.clone()) });
    let takes_over = since.is_some_and(|t| f.first_ms >= t.timestamp_millis())
        && (f.takeoff_ms.is_some() || f.landing.is_some());
    let flight_id = if takes_over {
        Some(take_over(&mut *tx, user_id, linked, f, statistics, track, over).await?)
    } else {
        linked
    };
    Ok((flight_id, app_doc))
}

/// How long engine 1 must stay shut down, parked, for a flight to be over (the stream's,
/// `flight.rs`, which the tests check against).
#[cfg(test)]
const ENGINE_OFF_MS: i64 = 10_000;

/// When the engine was shut down after `after_ms`, parked: engine 1's fuel flow off, on
/// the ground and stopped, for [`ENGINE_OFF_MS`]. Both sims report the fuel flow, a piston's
/// or a turbine's, and cut it with the engine.
#[cfg(test)]
fn engine_off(samples: &[Sample], after_ms: i64) -> Option<i64> {
    use crate::normalise::Field;
    let mut since = None;
    for s in samples.iter().filter(|s| s.wall_time_ms >= after_ms) {
        let off = s.get(Field::OnGround).is_some_and(|g| g > 0.5)
            && s.get(Field::GroundSpeed).is_some_and(|v| v < 1.0)
            && s.get(Field::FuelFlow).is_some_and(|f| f < 0.1);
        match since {
            _ if !off => since = None,
            None => since = Some(s.wall_time_ms),
            Some(t) if s.wall_time_ms - t >= ENGINE_OFF_MS => return Some(t),
            Some(_) => {}
        }
    }
    None
}

/// Makes the flights of up to `limit` open sessions as they fly, for users whose flights
/// the service makes (rollout phase 5): a flight is created, active, once it takes off,
/// its document, track and Discord post kept current each pass, and ended once it is over
/// (landed and shut down), not when the sim closes. Only sessions with chunks since they
/// were last looked at. The flights aren't stored in `telemetry_flights`: the session is
/// still analysed when it ends, which updates the flights made here rather than adding
/// to them. Returns how many flights were made or updated.
///
/// Each session's analysis is kept between passes ([`OpenAnalysis`]), so a pass decodes
/// and analyses only the chunks since the last: its cost is a minute of flight, not the
/// whole of it. A restart builds it again from the session, once.
pub async fn analyse_open(db: &PgPool, segments: &impl SegmentStore, limit: i64) -> Result<usize, AppError> {
    forget_closed(db).await?;
    let ready: Vec<(Uuid, i64, i32, bool)> = sqlx::query_as(
        "SELECT s.id, s.user_id, s.chunk_count, s.open_analysed_chunks IS NULL \
           FROM telemetry_sessions s JOIN users u ON u.id = s.user_id \
          WHERE s.status = 'open' AND u.service_flights_since <= NOW() \
            AND s.chunk_count > 0 AND s.invalid_seq IS NULL AND s.next_seq = s.chunk_count \
            AND s.open_analysed_chunks IS DISTINCT FROM s.chunk_count \
          ORDER BY s.last_chunk_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(db)
    .await?;
    let mut made = 0;
    for (id, user_id, chunks, first) in &ready {
        made += analyse_open_session(db, segments, *id, *user_id, *chunks, *first).await?;
    }
    Ok(made)
}

/// An open session's analysis, kept from one pass to the next: its decoder and
/// normaliser where its chunks left them, and the stream's flights so far. Bounded by the
/// flights, not by how long they fly.
struct OpenAnalysis {
    /// How many of the session's chunks it has.
    chunks: i32,
    decoder: SessionDecoder,
    normaliser: Normaliser,
    /// The stream's first session's.
    header: SessionHeader,
    /// Whether the session continues another's stream, and so whether its first sample
    /// is the pause between the two (`resume::join`).
    continues: bool,
    started: bool,
    stream: FlightStream<FlightStats>,
    used: std::time::Instant,
}

impl OpenAnalysis {
    /// Adds the session's next chunks, in order.
    fn add(&mut self, chunks: &[Vec<u8>]) -> Result<(), String> {
        for chunk in chunks {
            let payload = decompress(chunk).map_err(|e| e.to_string())?;
            for tick in self.decoder.decode(&payload).map_err(|e| e.to_string())?.ticks {
                let sample = self.normaliser.sample(tick);
                self.push(sample);
            }
            self.chunks += 1;
        }
        Ok(())
    }

    fn push(&mut self, mut sample: Sample) {
        if self.continues && !self.started {
            sample.flags |= butterlog_wire::flags::PAUSED;
        }
        self.started = true;
        self.stream.push(&sample);
    }
}

type OpenAnalyses = std::sync::Mutex<std::collections::HashMap<Uuid, OpenAnalysis>>;

/// The open sessions' analyses, by session.
fn open_analyses() -> &'static OpenAnalyses {
    static OPEN: OnceLock<OpenAnalyses> = OnceLock::new();
    OPEN.get_or_init(Default::default)
}

/// An open session's analysis left unused this long is dropped; it is built again if the
/// session sends more.
const FORGET_AFTER: std::time::Duration = std::time::Duration::from_secs(3_600);

/// Drops the analyses of sessions no longer open, or idle past [`FORGET_AFTER`].
async fn forget_closed(db: &PgPool) -> Result<(), AppError> {
    let ids: Vec<Uuid> = open_analyses().lock().expect("not poisoned").keys().copied().collect();
    if ids.is_empty() {
        return Ok(());
    }
    let open: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM telemetry_sessions WHERE id = ANY($1) AND status = 'open'")
        .bind(&ids)
        .fetch_all(db)
        .await?;
    open_analyses()
        .lock()
        .expect("not poisoned")
        .retain(|id, a| !ids.contains(id) || (open.contains(id) && a.used.elapsed() < FORGET_AFTER));
    Ok(())
}

/// An open session's chunks from `from` up to `to`, as uploaded.
async fn open_chunks(db: &PgPool, id: Uuid, from: i32, to: i32) -> Result<Vec<Vec<u8>>, AppError> {
    Ok(sqlx::query_scalar(
        "SELECT data FROM telemetry_chunks WHERE session_id = $1 AND seq >= $2 AND seq < $3 ORDER BY seq",
    )
    .bind(id)
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await?)
}

/// Builds an open session's analysis from the start: the stream it continues, then its
/// chunks up to `chunks`. Whether it continues one is settled on its `first` look, and
/// kept in `continues`: the stream before it is over, and checking again each pass would
/// load it again each time.
async fn build_open(
    db: &PgPool,
    segments: &impl SegmentStore,
    id: Uuid,
    chunks: i32,
    first: bool,
) -> Result<Result<OpenAnalysis, String>, AppError> {
    let blob: Vec<u8> = sqlx::query_scalar("SELECT open_blob FROM telemetry_sessions WHERE id = $1")
        .bind(id)
        .fetch_one(db)
        .await?;
    let stored = Stored::Chunks { open: blob, chunks: open_chunks(db, id, 0, chunks).await? };
    let (stored, found) = blocking(move || {
        let found = first_position(&stored);
        (stored, found)
    })
    .await?;
    let (header, first_at) = match found {
        Ok(f) => f,
        Err(e) => return Ok(Err(e)),
    };
    let before = if first {
        let before = resumed(db, segments, id, &header, first_at).await?;
        sqlx::query("UPDATE telemetry_sessions SET continues = $2 WHERE id = $1")
            .bind(id)
            .bind(before.as_ref().and_then(|s| s.ids.last().copied()))
            .execute(db)
            .await?;
        before
    } else {
        let continues: Option<Uuid> = sqlx::query_scalar("SELECT continues FROM telemetry_sessions WHERE id = $1")
            .bind(id)
            .fetch_one(db)
            .await?;
        match continues {
            None => None,
            Some(previous) => {
                let ids = chain_to(db, previous).await?;
                match stream_of(db, segments, &ids).await? {
                    Ok(s) => Some(s),
                    Err(e) => return Ok(Err(e)),
                }
            }
        }
    };
    blocking(move || -> Result<OpenAnalysis, String> {
        let Stored::Chunks { open: blob, chunks: data } = stored else { unreachable!("an open session is in Postgres") };
        let (open, _) = SessionOpen::decode(&blob).map_err(|e| e.to_string())?;
        let continues = before.is_some();
        let (stream, header) = match before {
            Some(before) => (before.flights, before.header.unwrap_or(header)),
            None => (FlightStream::new(), header),
        };
        let mut analysis = OpenAnalysis {
            chunks: 0,
            decoder: SessionDecoder::new(open.channels.clone()),
            normaliser: Normaliser::new(&open).map_err(|e| e.to_string())?,
            header,
            continues,
            started: false,
            stream,
            used: std::time::Instant::now(),
        };
        analysis.add(&data)?;
        analysis.chunks = chunks;
        Ok(analysis)
    })
    .await
}

/// The flights of an open session's stream it makes: those over, and the one flying
/// after them, if it has taken off.
struct OpenFlights {
    over: Analysed,
    flying: Option<(FlightSummary, Value, Value, Vec<DecodedPoint>)>,
}

/// Makes the flights of an open session, and of the stream it continues: those over and
/// not yet made, ended, and the one flying, active. Returns how many it made or updated.
/// `chunks` is how many it was analysed with; `first`, whether it is its first look.
async fn analyse_open_session(
    db: &PgPool,
    segments: &impl SegmentStore,
    id: Uuid,
    user_id: i64,
    chunks: i32,
    first: bool,
) -> Result<usize, AppError> {
    let kept = open_analyses()
        .lock()
        .expect("not poisoned")
        .remove(&id)
        .filter(|a| !first && a.chunks <= chunks);
    let analysis = match kept {
        Some(mut analysis) => {
            let data = open_chunks(db, id, analysis.chunks, chunks).await?;
            blocking(move || analysis.add(&data).map(|_| analysis)).await?
        }
        None => build_open(db, segments, id, chunks, first).await?,
    };
    let new = match analysis {
        Ok(analysis) => {
            let (flights, mut analysis) = blocking(move || {
                let flights = places().ok_or_else(|| "airport data is not loaded".to_string()).map(|places| {
                    let (flights, over) = analysis.stream.snapshot(places);
                    let mut flights = analysed(&analysis.header, flights, places);
                    let flying = flights.drain(over.min(flights.len())..).next().filter(|f| f.0.takeoff_ms.is_some());
                    OpenFlights { over: flights, flying }
                });
                (flights, analysis)
            })
            .await?;
            analysis.used = std::time::Instant::now();
            if flights.is_ok() {
                open_analyses().lock().expect("not poisoned").insert(id, analysis);
            }
            flights
        }
        Err(e) => Err(e),
    };
    let mut tx = db.begin().await?;
    let (status, made): (String, i32) =
        sqlx::query_as("SELECT status, open_flights_made FROM telemetry_sessions WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let mut count = 0;
    let mut flying_id = None;
    let over = match new {
        // Ended since: the analysis at its end makes them.
        _ if status != "open" => made as usize,
        Ok(OpenFlights { over, flying }) => {
            let since: Option<DateTime<Utc>> =
                sqlx::query_scalar("SELECT service_flights_since FROM users WHERE id = $1")
                    .bind(user_id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .flatten();
            for (f, _, statistics, track) in over.iter().skip(made as usize) {
                link(&mut tx, user_id, since, f, statistics, track, true).await?;
                count += 1;
            }
            if let Some((f, _, statistics, track)) = &flying {
                flying_id = link(&mut tx, user_id, since, f, statistics, track, false).await?.0;
                count += 1;
            }
            over.len().max(made as usize)
        }
        Err(e) => {
            tracing::warn!(session = %id, "open telemetry session not analysed: {e}");
            made as usize
        }
    };
    sqlx::query(
        "UPDATE telemetry_sessions \
            SET open_analysed_chunks = $2, open_flights_made = $3, open_flight_id = $4, open_tailed_chunks = $2 \
          WHERE id = $1",
    )
    .bind(id)
    .bind(chunks)
    .bind(over as i32)
    .bind(flying_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(count)
}

/// Adds the chunks open sessions received since their last analysis to the flight each is
/// flying, up to `limit` sessions: its track, its position (`current_snapshot`) and
/// `updated_at`, which the live page polls, so it moves with the chunks rather than with
/// the analysis each minute. Only the chunks since the last keyframe before them are
/// decoded. The document and the Discord post wait for the analysis. Returns how many
/// flights moved.
pub async fn tail_open(db: &PgPool, limit: i64) -> Result<usize, AppError> {
    let ready: Vec<(Uuid, i64, i32, i32, Vec<u8>)> = sqlx::query_as(
        "SELECT s.id, s.open_flight_id, s.open_tailed_chunks, s.chunk_count, s.open_blob \
           FROM telemetry_sessions s JOIN flights f ON f.id = s.open_flight_id \
          WHERE s.status = 'open' AND f.status = 'active' AND f.service_made \
            AND s.invalid_seq IS NULL AND s.next_seq = s.chunk_count \
            AND s.open_tailed_chunks < s.chunk_count \
          ORDER BY s.last_chunk_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(db)
    .await?;
    let mut moved = 0;
    for (id, flight_id, tailed, chunks, open) in ready {
        if tail_session(db, id, flight_id, tailed, chunks, open).await? {
            moved += 1;
        }
    }
    Ok(moved)
}

/// Adds chunks `tailed..chunks` of session `id` to `flight_id`; whether any point was new.
async fn tail_session(
    db: &PgPool,
    id: Uuid,
    flight_id: i64,
    tailed: i32,
    chunks: i32,
    open: Vec<u8>,
) -> Result<bool, AppError> {
    let data: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT data FROM telemetry_chunks \
          WHERE session_id = $1 AND seq < $3 AND seq >= \
                (SELECT MAX(seq) FROM telemetry_chunks WHERE session_id = $1 AND keyframe AND seq <= $2) \
          ORDER BY seq",
    )
    .bind(id)
    .bind(tailed)
    .bind(chunks)
    .fetch_all(db)
    .await?;
    let samples = match blocking(move || decode_chunks(&open, &data)).await? {
        Ok(samples) => samples,
        Err(e) => {
            tracing::warn!(session = %id, "open telemetry session not tailed: {e}");
            Vec::new()
        }
    };
    let mut tx = db.begin().await?;
    // The analysis may have moved on meanwhile: then it has these chunks.
    let current: Option<(i32,)> = sqlx::query_as(
        "SELECT open_tailed_chunks FROM telemetry_sessions \
          WHERE id = $1 AND status = 'open' AND open_flight_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(flight_id)
    .fetch_optional(&mut *tx)
    .await?;
    if current.is_none_or(|(t,)| t != tailed) {
        return Ok(false);
    }
    let last: Option<i64> = sqlx::query_scalar("SELECT MAX(sample_epoch) FROM flight_track_points WHERE flight_id = $1")
        .bind(flight_id)
        .fetch_one(&mut *tx)
        .await?;
    let new = super::statistics::points(
        samples.iter().filter(|s| last.is_none_or(|l| s.wall_time_ms.div_euclid(1000) > l)),
    );
    if let Some(latest) = samples.last().filter(|_| !new.is_empty()) {
        let added = crate::track::insert_points(&mut tx, flight_id, &new).await?;
        sqlx::query(
            "UPDATE flights \
                SET statistics = jsonb_set(statistics, '{current_snapshot}', $2), \
                    track_points = track_points + $3, updated_at = CURRENT_TIMESTAMP \
              WHERE id = $1 AND status = 'active'",
        )
        .bind(flight_id)
        .bind(super::statistics::current_snapshot(latest))
        .bind(added as i32)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE telemetry_sessions SET open_tailed_chunks = $2 WHERE id = $1")
        .bind(id)
        .bind(chunks)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(!new.is_empty())
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

/// The flight this one is, by takeoff time (the app's is the roll's), or start time with
/// no takeoff: an old-path flight within [`SAME_FLIGHT_MS`], or one the service owns, its
/// document the service's, within [`SAME_ANALYSIS_MS`]. Candidates are `(id, statistics,
/// service_owned)`.
fn same_flight<'a>(f: &FlightSummary, candidates: &'a [(i64, Value, bool)]) -> Option<&'a (i64, Value, bool)> {
    let (ours, key) = match f.takeoff_roll_ms.or(f.takeoff_ms) {
        Some(t) => (t, "takeoff_time"),
        None => (f.first_ms, "start_time"),
    };
    candidates
        .iter()
        .filter_map(|c| Some((c, (app_time(&c.1[key])? - ours).abs())))
        .filter(|(c, gap)| *gap <= if c.2 { SAME_ANALYSIS_MS } else { SAME_FLIGHT_MS })
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

/// The samples of a flight the analysis found, from the first to the last, decoded again
/// from the stream of sessions it was found in, with the stream's aircraft title. `None`
/// when no analysis found it: a flight the app made with no telemetry, or one whose
/// sessions have not been analysed yet.
pub async fn flight_samples(
    db: &PgPool,
    segments: &impl SegmentStore,
    flight_id: i64,
) -> Result<Option<(String, Vec<Sample>)>, AppError> {
    let found: Option<(Uuid, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT session_id, first_at, last_at FROM telemetry_flights \
          WHERE flight_id = $1 ORDER BY analyser_version DESC, last_at DESC LIMIT 1",
    )
    .bind(flight_id)
    .fetch_optional(db)
    .await?;
    let Some((first, from, to)) = found else { return Ok(None) };
    let (from, to) = (from.timestamp_millis(), to.timestamp_millis());
    let mut ids = vec![first];
    // The flight's samples only, each session decoded a chunk at a time; a session's first
    // after another is the pause between them, as the analysis had it (`Stream::add`).
    let mut samples: Vec<Sample> = Vec::new();
    let mut seen = false;
    let mut last_ms = None;
    let mut title = None;
    loop {
        let at = *ids.last().expect("not empty");
        let stored = match fetch(db, segments, at).await? {
            Ok(s) => s,
            Err(e) => return Err(AppError::Storage(format!("session {at} not loaded: {e}"))),
        };
        let decoded = blocking(move || {
            let mut pause = seen;
            let header = each_sample(&stored, |mut s| {
                if pause {
                    s.flags |= butterlog_wire::flags::PAUSED;
                    pause = false;
                }
                seen = true;
                last_ms = Some(s.wall_time_ms);
                if (from..=to).contains(&s.wall_time_ms) {
                    samples.push(s);
                }
                ControlFlow::Continue(())
            });
            (header, samples, seen, last_ms)
        })
        .await?;
        let header;
        (header, samples, seen, last_ms) = decoded;
        match header {
            Ok(h) => title.get_or_insert(h.aircraft.title),
            Err(e) => return Err(AppError::Storage(format!("session {at} not loaded: {e}"))),
        };
        if last_ms.is_some_and(|t| t > to) {
            break;
        }
        let next: Option<Uuid> = sqlx::query_scalar("SELECT id FROM telemetry_sessions WHERE continues = $1 ORDER BY started_at LIMIT 1")
            .bind(at)
            .fetch_optional(db)
            .await?;
        match next.filter(|n| !ids.contains(n) && ids.len() < MAX_CHAIN) {
            Some(n) => ids.push(n),
            None => break,
        }
    }
    Ok(Some((title.unwrap_or_default(), samples)))
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
    let since: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT service_flights_since FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
    let error = match analysed {
        Ok(flights) => {
            for (idx, (f, summary, statistics, track)) in flights.iter().enumerate() {
                let (flight_id, app_doc) = link(&mut tx, user_id, since, f, statistics, track, true).await?;
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
                .bind(flight_id)
                .bind(app_doc.as_ref().map(|a| parity(f, a)))
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
    use crate::analysis::analyse;
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
            (1, json!({ "takeoff_time": "2026-10-31 18:00:00.000" }), false),
            (2, json!({ "takeoff_time": "2026-11-01 01:09:47.776", "start_time": "2026-11-01 01:08:56.650" }), false),
            (3, json!({ "start_time": "2026-11-01 01:10:00" }), false),
        ];
        assert_eq!(same_flight(&flight(Some(t + 400), t - 50_000), &candidates).unwrap().0, 2);
        // No takeoff: by start, within five minutes.
        assert_eq!(same_flight(&flight(None, t + 60_000), &candidates).unwrap().0, 3);
        assert!(same_flight(&flight(Some(t + 3_600_000), t), &candidates).is_none());

        // The service's own flight is the one it analysed, not a second circuit minutes
        // later (flight 564 took two sessions' circuits, 4.5 minutes apart, as one).
        let owned = vec![(4, json!({ "takeoff_time": "2026-11-01 01:09:47.776" }), true)];
        assert_eq!(same_flight(&flight(Some(t + 20_000), t - 50_000), &owned).unwrap().0, 4);
        assert!(same_flight(&flight(Some(t + 270_000), t + 200_000), &owned).is_none());
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
        // Its flights gone first: another test's re-analysis may pick it up once it is stale.
        sqlx::query("DELETE FROM telemetry_flights WHERE session_id = $1").bind(id).execute(&db).await.unwrap();
        sqlx::query("UPDATE telemetry_sessions SET analyser_version = $2 WHERE id = $1")
            .bind(id)
            .bind(ANALYSER_VERSION - 1)
            .execute(&db)
            .await
            .unwrap();
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

    /// For a user the service makes the flights of: the pattern, once with no flight from
    /// the app, which the service creates, and once with the app's, which it takes over.
    #[tokio::test]
    async fn the_service_makes_the_flights_of_a_user_it_owns() {
        use crate::sessions::{end_session, open_session, store_chunk};
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return };
        let db = PgPool::connect(&url).await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        let path = format!("{}/tests/fixtures/xplane_pattern.blsg.zst", env!("CARGO_MANIFEST_DIR"));
        let segment = butterlog_wire::decode_segment(&decompress(&std::fs::read(path).unwrap()).unwrap()).unwrap();
        let segments = TestSegments;

        // A user it owns, the pattern uploaded, and analysed.
        let fly = |app_flight: bool| {
            let (db, segment) = (&db, &segment);
            async move {
                let user: i64 = sqlx::query_scalar(
                    "INSERT INTO users (discord_id, username, service_flights_since) \
                     VALUES ($1, 'test', '2026-01-01') RETURNING id",
                )
                .bind(Uuid::new_v4().to_string())
                .fetch_one(db)
                .await
                .unwrap();
                let app = if app_flight {
                    Some(
                        sqlx::query_scalar::<_, i64>(
                            "INSERT INTO flights (user_id, departure, statistics, track_points, created_at) \
                             VALUES ($1, 'KAST', $2, 7, '2026-11-01 01:09:00+00') RETURNING id",
                        )
                        .bind(user)
                        .bind(json!({ "takeoff_time": "2026-11-01 01:09:39.068", "landing_score": -142.24 }))
                        .fetch_one(db)
                        .await
                        .unwrap(),
                    )
                } else {
                    None
                };
                let id = Uuid::new_v4();
                open_session(db, user, id, &segment.open).await.unwrap();
                for (seq, payload) in &segment.chunks {
                    store_chunk(db, user, id, *seq, &butterlog_wire::compress(payload, 3)).await.unwrap();
                }
                end_session(db, user, id).await.unwrap();
                analyse_session(db, &TestSegments, id, user).await.unwrap();
                (user, id, app)
            }
        };
        type Flight = (i64, String, Option<String>, Value, Option<Value>, bool, bool, bool, String, i32);
        let flights_of = |user: i64| {
            sqlx::query_as::<_, Flight>(
                "SELECT id, departure, arrival, statistics, app_statistics, service_owned, service_made, \
                        discord_sync_due, status, track_points FROM flights WHERE user_id = $1",
            )
            .bind(user)
            .fetch_all(&db)
        };
        let linked = |session: Uuid| {
            sqlx::query_as::<_, (Option<i64>, Option<Value>)>(
                "SELECT flight_id, parity FROM telemetry_flights WHERE session_id = $1",
            )
            .bind(session)
            .fetch_one(&db)
        };

        // No flight from the app: the service's, ended, with its track, its post due.
        let (user, session, _) = fly(false).await;
        let made = flights_of(user).await.unwrap();
        assert_eq!(made.len(), 1);
        let (id, departure, arrival, statistics, app_statistics, owned, service_made, due, status, points) =
            made[0].clone();
        assert_eq!((departure.as_str(), arrival.as_deref(), status.as_str()), ("KAST", Some("KAST"), "ended"));
        assert_eq!(statistics["arrival"]["icao"], json!("KAST"));
        assert!(statistics["current_snapshot"]["Latitude"].is_number());
        assert_eq!((app_statistics, owned, service_made, due), (None, true, true, true));
        assert!(points > 100, "{points} track points");
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM flight_track_points WHERE flight_id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(stored, points as i64);
        assert_eq!(linked(session).await.unwrap(), (Some(id), None));

        // Its export: the flight's samples from its session, a row a second.
        let (title, samples) = flight_samples(&db, &segments, id).await.unwrap().unwrap();
        assert!(!title.is_empty());
        let csv = crate::export::flysto_csv(&title, &samples);
        let rows = csv.lines().count() - 3;
        let seconds = (samples.last().unwrap().wall_time_ms - samples[0].wall_time_ms) / 1_000;
        // Every second but those of a silence too long to carry over, a pause in the fixture.
        let silent: i64 = samples
            .windows(2)
            .map(|w| (w[1].wall_time_ms - w[0].wall_time_ms) / 1_000)
            .filter(|g| *g > 10)
            .sum();
        assert!((rows as i64 - (seconds - silent)).abs() <= 2, "{rows} rows over {seconds} s, {silent} silent");
        assert!(csv.lines().nth(3).unwrap().contains(", 46.1"), "{}", csv.lines().nth(3).unwrap());
        assert!(flight_samples(&db, &segments, -1).await.unwrap().is_none());

        // The app finds it from its log's span, which has no id for it; a span before it, nothing.
        let (from, to) = (samples[0].wall_time_ms / 1_000, samples.last().unwrap().wall_time_ms / 1_000);
        assert_eq!(crate::flight_doc::covering(&db, user, from + 60, to + 600).await.unwrap(), Some(id));
        assert_eq!(crate::flight_doc::covering(&db, user, from - 7_200, from - 3_600).await.unwrap(), None);
        assert_eq!(crate::flight_doc::covering(&db, user + 1_000_000, from, to).await.unwrap(), None);

        // Analysed again: the same flight, not another.
        sqlx::query("UPDATE flights SET discord_sync_due = FALSE WHERE id = $1").bind(id).execute(&db).await.unwrap();
        analyse_session(&db, &segments, session, user).await.unwrap();
        let again = flights_of(user).await.unwrap();
        assert_eq!(again.len(), 1);
        assert_eq!((again[0].0, again[0].7, again[0].9), (id, true, points));

        // The app's flight: the service's document, the app's kept, the app's track.
        let (user, session, app) = fly(true).await;
        let taken = flights_of(user).await.unwrap();
        assert_eq!(taken.len(), 1);
        let (id, _, arrival, statistics, app_statistics, owned, service_made, due, _, points) = taken[0].clone();
        assert_eq!(Some(id), app);
        assert_eq!(arrival.as_deref(), Some("KAST"));
        assert_eq!(statistics["arrival"]["icao"], json!("KAST"));
        assert_eq!(app_statistics.as_ref().unwrap()["landing_score"], json!(-142.24));
        assert_eq!((owned, service_made, due, points), (true, false, true, 7));
        let (flight, parity) = linked(session).await.unwrap();
        assert_eq!(flight, app);
        assert_eq!(parity.unwrap()["landing_score"]["app"], json!(-142.24));
        // Analysed again, the app's document is still the one kept.
        analyse_session(&db, &segments, session, user).await.unwrap();
        let kept = flights_of(user).await.unwrap();
        assert_eq!(kept[0].4.as_ref().unwrap()["landing_score"], json!(-142.24));
    }

    fn parked(t: i64, fuel_flow: f64) -> Sample {
        use crate::normalise::Field;
        let mut s = Sample::empty();
        s.wall_time_ms = t;
        s.set(Field::OnGround, 1.0);
        s.set(Field::GroundSpeed, 0.0);
        s.set(Field::FuelFlow, fuel_flow);
        s
    }

    /// A session decoded a chunk at a time into the stream is analysed as its samples were
    /// all at once: whole, from its segment or its chunks, and cut in two in the air and
    /// resumed, the second's first sample the pause (`resume::join`). A session that
    /// doesn't decode leaves the stream as it was.
    #[test]
    fn a_stream_fed_a_chunk_at_a_time_is_analysed_as_its_samples_were() {
        let places = places().unwrap();
        let compressed = |payloads: &[Vec<u8>]| payloads.iter().map(|p| butterlog_wire::compress(p, 3)).collect::<Vec<_>>();
        for name in ["msfs_pattern.blsg.zst", "xplane_pattern.blsg.zst"] {
            let bytes = std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap();
            let segment = decode_segment(&decompress(&bytes).unwrap()).unwrap();
            let header = SessionOpen::decode(&segment.open).unwrap().0.header;
            let at_once = |samples: &[Sample]| {
                let mut stream = FlightStream::<FlightStats>::new();
                samples.iter().for_each(|s| stream.push(s));
                analysed(&header, stream.flights(places), places)
            };
            let payloads: Vec<Vec<u8>> = segment.chunks.iter().map(|(_, p)| p.clone()).collect();
            let samples = decode_payloads(&segment.open, &payloads).unwrap();
            let expected = at_once(&samples);
            assert!(!expected.is_empty(), "{name}");

            let chunks = Stored::Chunks { open: segment.open.clone(), chunks: compressed(&payloads) };
            for stored in [Stored::Segment(bytes.clone()), chunks] {
                let mut stream = Stream::default();
                stream.add(Uuid::nil(), &stored).unwrap();
                assert_eq!(stream.analysed().unwrap(), expected, "{name}");
                assert_eq!(stream.last_position, samples.iter().rev().find_map(position), "{name}");
                assert_eq!(first_position(&stored).unwrap().1, samples.iter().find_map(position), "{name}");
            }

            // Cut at a keyframe halfway, the second session numbered from 0.
            let keyframes: Vec<usize> = (1..payloads.len()).filter(|&i| renumbered(&payloads[i], 0).1).collect();
            let cut = keyframes[keyframes.len() / 2];
            let second: Vec<Vec<u8>> =
                payloads[cut..].iter().enumerate().map(|(seq, p)| renumbered(p, seq as u32).0).collect();
            let mut joined = decode_payloads(&segment.open, &payloads[..cut]).unwrap();
            super::super::resume::join(&mut joined, decode_payloads(&segment.open, &second).unwrap());
            let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
            let mut stream = Stream::default();
            stream.add(a, &Stored::Chunks { open: segment.open.clone(), chunks: compressed(&payloads[..cut]) }).unwrap();
            stream.add(b, &Stored::Chunks { open: segment.open.clone(), chunks: compressed(&second) }).unwrap();
            assert_eq!(stream.ids, [a, b]);
            assert_eq!(stream.analysed().unwrap(), at_once(&joined), "{name} resumed");

            let before = stream.analysed().unwrap();
            assert!(stream.add(Uuid::new_v4(), &Stored::Segment(vec![1, 2, 3])).is_err());
            assert_eq!(stream.ids, [a, b]);
            assert_eq!(stream.analysed().unwrap(), before);
        }
    }

    #[test]
    fn the_engine_is_off_once_its_fuel_flow_stays_cut() {
        // Idling, then cut for 5 s, running again, then cut for good.
        let mut samples: Vec<Sample> = (0..10).map(|i| parked(i * 1000, 2.0)).collect();
        samples.extend((10..15).map(|i| parked(i * 1000, 0.0)));
        samples.extend((15..20).map(|i| parked(i * 1000, 2.0)));
        samples.extend((20..40).map(|i| parked(i * 1000, 0.0)));
        assert_eq!(engine_off(&samples, 0), Some(20_000));
        assert_eq!(engine_off(&samples, 35_000), None);
        // Rolling with the fuel cut isn't parked.
        let mut rolling = samples.clone();
        rolling.iter_mut().for_each(|s| s.set(crate::normalise::Field::GroundSpeed, 5.0));
        assert_eq!(engine_off(&rolling, 0), None);
    }

    /// The pattern in a session still open: active once it takes off, its track added to,
    /// ended once its engine is shut down, the same flight throughout; the analysis at the
    /// session's end updates it.
    #[tokio::test]
    async fn a_flight_is_made_as_it_flies() {
        use crate::sessions::{end_session, open_session, store_chunk};
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return };
        let db = PgPool::connect(&url).await.unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        let path = format!("{}/tests/fixtures/xplane_pattern.blsg.zst", env!("CARGO_MANIFEST_DIR"));
        let segment = butterlog_wire::decode_segment(&decompress(&std::fs::read(path).unwrap()).unwrap()).unwrap();
        let payloads: Vec<Vec<u8>> = segment.chunks.iter().map(|c| c.1.clone()).collect();
        let samples = decode_payloads(&segment.open, &payloads).unwrap();
        let flight = &analyse(&samples, places().unwrap())[0];
        let takeoff = flight.takeoff_ms.unwrap();
        let off = engine_off(&samples, flight.landing.as_ref().unwrap().wall_time_ms)
            .expect("the pattern ends with the engine shut down");
        // The first chunks that hold `t`.
        let upto = |t: i64| {
            let ks: Vec<usize> = (1..=payloads.len()).collect();
            ks[ks.partition_point(|&k| {
                !decode_payloads(&segment.open, &payloads[..k])
                    .is_ok_and(|p| p.last().is_some_and(|s| s.wall_time_ms >= t))
            })]
        };
        let (airborne, shut_down) = (upto(takeoff + 60_000), upto(off + ENGINE_OFF_MS));

        let user: i64 = sqlx::query_scalar(
            "INSERT INTO users (discord_id, username, service_flights_since) VALUES ($1, 'test', '2026-01-01') RETURNING id",
        )
        .bind(Uuid::new_v4().to_string())
        .fetch_one(&db)
        .await
        .unwrap();
        let id = Uuid::new_v4();
        let upload = |chunks: &[(u32, Vec<u8>)]| {
            let db = &db;
            let chunks = chunks.to_vec();
            async move {
                for (seq, payload) in &chunks {
                    store_chunk(db, user, id, *seq, &butterlog_wire::compress(payload, 3)).await.unwrap();
                }
            }
        };
        type Row = (i64, Option<String>, String, Option<String>, i32, bool, Value);
        let flights = || {
            sqlx::query_as::<_, Row>(
                "SELECT id, arrival, status, end_reason, track_points, discord_sync_due, statistics \
                   FROM flights WHERE user_id = $1",
            )
            .bind(user)
            .fetch_all(&db)
        };
        let due_no_more = || sqlx::query("UPDATE flights SET discord_sync_due = FALSE WHERE user_id = $1").bind(user).execute(&db);

        // Screenshots uploaded against the session: one in the air, one after shutdown.
        let shot = |hash: &'static str, at: i64| {
            let db = &db;
            async move {
                sqlx::query(
                    "INSERT INTO screenshots (hash, url, created_at, session_id, user_id, taken_at) \
                     VALUES ($1, 'u.webp', $2, $3, $4, $2)",
                )
                .bind(hash)
                .bind(timestamp(at))
                .bind(id)
                .bind(user)
                .execute(db)
                .await
                .unwrap();
            }
        };
        let placed = || {
            sqlx::query_as::<_, (String, Option<i64>)>(
                "SELECT hash, flight_id FROM screenshots WHERE user_id = $1 ORDER BY hash",
            )
            .bind(user)
            .fetch_all(&db)
        };

        // A minute after takeoff: active, its takeoff marked, a track so far, its post due.
        open_session(&db, user, id, &segment.open).await.unwrap();
        shot("a-air", takeoff + 30_000).await;
        shot("b-parked", off + ENGINE_OFF_MS + 2_000).await;
        upload(&segment.chunks[..airborne]).await;
        assert_eq!(analyse_open_session(&db, &TestSegments, id, user, airborne as i32, true).await.unwrap(), 1);
        let flying = flights().await.unwrap();
        assert_eq!(flying.len(), 1);
        let (flight_id, arrival, status, end_reason, points, due, statistics) = flying[0].clone();
        assert_eq!((arrival, status.as_str(), end_reason, due), (None, "active", None, true));
        assert!(points > 30, "{points} track points");
        assert_eq!(statistics["events"][0]["eventType"], json!("takeoff"));
        // While it flies, every screenshot since it began is its own.
        let on = placed().await.unwrap();
        assert_eq!(on, vec![("a-air".into(), Some(flight_id)), ("b-parked".into(), Some(flight_id))]);
        due_no_more().await.unwrap();

        // Between analyses, the newest chunks move it: its track and position, not its
        // post.
        let tail = airborne + 6;
        upload(&segment.chunks[airborne..tail]).await;
        tail_open(&db, 20).await.unwrap();
        let tailed = flights().await.unwrap();
        let (_, _, status, _, tailed_points, due, tailed_statistics) = tailed[0].clone();
        assert_eq!((status.as_str(), due), ("active", false));
        assert!(tailed_points > points + 20, "{points} then {tailed_points} track points");
        assert_ne!(tailed_statistics["current_snapshot"], statistics["current_snapshot"]);
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM flight_track_points WHERE flight_id = $1")
            .bind(flight_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(stored, tailed_points as i64);
        let to: i32 = sqlx::query_scalar("SELECT open_tailed_chunks FROM telemetry_sessions WHERE id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(to, tail as i32);
        let points = tailed_points;

        // Landed, the engine still running: the same flight, active, its track longer.
        upload(&segment.chunks[tail..shut_down - 1]).await;
        assert_eq!(analyse_open_session(&db, &TestSegments, id, user, shut_down as i32 - 1, false).await.unwrap(), 1);
        let landed = flights().await.unwrap();
        assert_eq!(landed.len(), 1);
        assert_eq!((landed[0].0, landed[0].2.as_str(), landed[0].5), (flight_id, "active", true));
        assert!(landed[0].4 > points);
        due_no_more().await.unwrap();

        // Shut down: ended, landed, its whole track.
        upload(&segment.chunks[shut_down - 1..]).await;
        let all = segment.chunks.len() as i32;
        assert_eq!(analyse_open_session(&db, &TestSegments, id, user, all, false).await.unwrap(), 1);
        let ended = flights().await.unwrap();
        assert_eq!(ended.len(), 1);
        let (ended_id, arrival, status, end_reason, points, due, statistics) = ended[0].clone();
        assert_eq!(
            (ended_id, arrival.as_deref(), status.as_str(), end_reason.as_deref(), due),
            (flight_id, Some("KAST"), "ended", Some("landed"), true)
        );
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM flight_track_points WHERE flight_id = $1")
            .bind(flight_id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(stored, points as i64);
        // Once it is over, the one taken after it ended is no longer its.
        assert_eq!(placed().await.unwrap(), vec![("a-air".into(), Some(flight_id)), ("b-parked".into(), None)]);
        let landing = statistics["events"].as_array().unwrap().iter().find(|e| e["eventType"] == "landing").cloned();
        assert!(landing.unwrap()["touchdownFpm"].is_number());
        // The passes after the first added their chunks to the analysis kept from the one
        // before, which finds what one built from the whole session does.
        let kept = open_analyses().lock().unwrap().remove(&id).expect("kept between passes");
        assert_eq!(kept.chunks, all);
        let built = build_open(&db, &TestSegments, id, all, false).await.unwrap().unwrap();
        let p = places().unwrap();
        assert_eq!(
            analysed(&kept.header, kept.stream.snapshot(p).0, p),
            analysed(&built.header, built.stream.snapshot(p).0, p)
        );
        open_analyses().lock().unwrap().insert(id, built);
        // Looked at again with no new chunks, or with them: nothing more.
        assert_eq!(analyse_open_session(&db, &TestSegments, id, user, all, false).await.unwrap(), 0);

        // Ended: the same flight.
        end_session(&db, user, id).await.unwrap();
        analyse_session(&db, &TestSegments, id, user).await.unwrap();
        let after = flights().await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!((after[0].0, after[0].2.as_str()), (flight_id, "ended"));
        let linked: Option<i64> = sqlx::query_scalar("SELECT flight_id FROM telemetry_flights WHERE session_id = $1")
            .bind(id)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(linked, Some(flight_id));
    }
}
