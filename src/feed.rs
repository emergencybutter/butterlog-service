//! Public feed of finished flights, for services that keep a logbook of their
//! own (vhangar, first) and need to page through everything butterlog has
//! recorded rather than catch each flight live off the map.
//!
//! Ordered by `(ended_at, id)` and paged by an opaque cursor over that pair, so
//! a consumer that polls every few minutes sees each flight exactly once, and
//! one that was down for a day picks up where it stopped.
//!
//! Public, like the live map and the flight history pages: nothing here is not
//! already on `/content`. The Discord id rides along because it is how a
//! consumer that signs its users in with Discord knows whose flight it is.

use axum::{
    extract::{Query, State},
    http::header,
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::live::parse_time;
use crate::AppState;

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;

/// How long a flight has to have been ended before it is listed.
///
/// `ended_at` is `NOW()` inside the ending transaction, which is the start of
/// that transaction, not its commit. Two flights ending a moment apart can
/// commit in the other order, and a consumer whose cursor had already passed
/// the earlier timestamp would never see it. Holding back the last few seconds
/// gives every such transaction time to land first.
const SETTLE_SECS: f64 = 10.0;

#[derive(Debug, Deserialize)]
pub struct EndedQuery {
    /// The `next` of the previous page.
    pub after: Option<String>,
    /// Where to start when there is no cursor yet, as RFC 3339. Omit both to
    /// start from the first flight ever recorded.
    pub since: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct EndedPage {
    pub data: Vec<EndedFlight>,
    /// Always set: the position after the last flight returned, or the
    /// position asked for when there was nothing new. Pass it back as `after`.
    pub next: String,
    /// Whether a full page came back, so there may be more right now.
    pub has_more: bool,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct EndedFlight {
    pub id: i64,
    pub user_id: i64,
    pub discord_id: String,
    pub pilot_name: String,
    pub departure: String,
    pub arrival: Option<String>,
    /// ICAO type designator, as resolved by the app (e.g. `C172`).
    pub aircraft_icao: Option<String>,
    /// The simulator's title for the airframe, free text.
    pub aircraft_title: Option<String>,
    /// The registration painted on the aircraft.
    pub tail_number: Option<String>,
    pub simulator: Option<String>,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub takeoff_time: Option<DateTime<Utc>>,
    pub landing_time: Option<DateTime<Utc>>,
    /// Takeoff to landing, falling back to the block times.
    pub flight_seconds: Option<i64>,
    /// Great circle between the takeoff and landing fixes, not the path flown.
    pub distance_nm: Option<f64>,
    pub fuel_consumed: Option<f64>,
    /// Vertical speed at touchdown, feet per minute (negative is down).
    pub landing_vs_fpm: Option<f64>,
    /// `landed`, `sim_closed` or `abandoned`. A client too old to end its own
    /// flights is ended by the reaper, which says `abandoned` whether or not
    /// the aeroplane landed — `landing_time` is the better witness.
    pub end_reason: Option<String>,
    pub ended_at: DateTime<Utc>,
}

/// A position in the feed: just after the flight that ended at `ended_at` with
/// id `id`. Opaque to clients, which only ever hand back what they were given.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cursor {
    ended_at: DateTime<Utc>,
    id: i64,
}

impl Cursor {
    fn encode(&self) -> String {
        format!("{}_{}", self.ended_at.timestamp_micros(), self.id)
    }

    fn decode(s: &str) -> Option<Cursor> {
        let (micros, id) = s.split_once('_')?;
        Some(Cursor {
            ended_at: DateTime::from_timestamp_micros(micros.parse().ok()?)?,
            id: id.parse().ok()?,
        })
    }

    /// Where to begin: the cursor if there is one, else the start of `since`,
    /// else the beginning of time.
    fn start(q: &EndedQuery) -> Result<Cursor, AppError> {
        if let Some(after) = &q.after {
            return Cursor::decode(after)
                .ok_or_else(|| AppError::BadRequest("Malformed `after` cursor".to_string()));
        }
        let ended_at = match &q.since {
            Some(since) => DateTime::parse_from_rfc3339(since)
                .map_err(|_| AppError::BadRequest("`since` must be RFC 3339".to_string()))?
                .with_timezone(&Utc),
            None => DateTime::<Utc>::UNIX_EPOCH,
        };
        // Id 0 sorts before every real flight at that instant.
        Ok(Cursor { ended_at, id: 0 })
    }
}

/// The columns the feed needs, projected out of the statistics blob in SQL so
/// the snapshot arrays never cross the wire. Numbers come back as text and are
/// parsed here, the way the aircraft stats do it, so one malformed value in one
/// flight costs that field rather than the page.
#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    user_id: i64,
    discord_id: String,
    username: String,
    global_name: Option<String>,
    departure: String,
    arrival: Option<String>,
    ended_at: DateTime<Utc>,
    end_reason: Option<String>,
    resolved_icao: Option<String>,
    airframe_name: Option<String>,
    atc_id: Option<String>,
    simulator: Option<String>,
    start_time: Option<String>,
    end_time: Option<String>,
    takeoff_time: Option<String>,
    landing_time: Option<String>,
    to_lat: Option<String>,
    to_lon: Option<String>,
    ld_lat: Option<String>,
    ld_lon: Option<String>,
    fuel_consumed: Option<String>,
    landing_vs: Option<String>,
}

/// Blank is absent: the app writes empty strings for what it does not know.
fn present(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn number(s: &Option<String>) -> Option<f64> {
    s.as_deref()?.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

impl From<Row> for EndedFlight {
    fn from(r: Row) -> Self {
        let flight_seconds =
            crate::flight_span_seconds(&r.takeoff_time, &r.landing_time, &r.start_time, &r.end_time);
        let distance_nm = match (number(&r.to_lat), number(&r.to_lon), number(&r.ld_lat), number(&r.ld_lon)) {
            (Some(a), Some(b), Some(c), Some(d)) => {
                Some((crate::haversine_nm(a, b, c, d) * 10.0).round() / 10.0)
            }
            _ => None,
        };
        let time = |s: &Option<String>| s.as_deref().and_then(parse_time);

        EndedFlight {
            id: r.id,
            user_id: r.user_id,
            discord_id: r.discord_id,
            pilot_name: r.global_name.unwrap_or(r.username),
            departure: r.departure,
            arrival: present(r.arrival),
            aircraft_icao: present(r.resolved_icao),
            aircraft_title: present(r.airframe_name),
            tail_number: present(r.atc_id),
            simulator: present(r.simulator),
            start_time: time(&r.start_time),
            end_time: time(&r.end_time),
            takeoff_time: time(&r.takeoff_time),
            landing_time: time(&r.landing_time),
            flight_seconds,
            distance_nm,
            fuel_consumed: number(&r.fuel_consumed),
            landing_vs_fpm: number(&r.landing_vs),
            end_reason: r.end_reason,
            ended_at: r.ended_at,
        }
    }
}

/// Public: every flight that has ended, oldest ending first.
pub async fn ended_flights_handler(
    State(state): State<AppState>,
    Query(q): Query<EndedQuery>,
) -> Result<impl IntoResponse, AppError> {
    let start = Cursor::start(&q)?;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let rows: Vec<Row> = sqlx::query_as(
        "SELECT f.id, f.user_id, u.discord_id, u.username, u.global_name, \
                f.departure, f.arrival, f.ended_at, f.end_reason, \
                f.statistics->>'resolved_icao' AS resolved_icao, \
                f.statistics->>'airframe_name' AS airframe_name, \
                f.statistics->>'atc_id' AS atc_id, \
                f.statistics->>'simulator' AS simulator, \
                f.statistics->>'start_time' AS start_time, \
                f.statistics->>'end_time' AS end_time, \
                f.statistics->>'takeoff_time' AS takeoff_time, \
                f.statistics->>'landing_time' AS landing_time, \
                f.statistics->'takeoff_snapshot'->>'Latitude' AS to_lat, \
                f.statistics->'takeoff_snapshot'->>'Longitude' AS to_lon, \
                f.statistics->'landing_snapshot'->>'Latitude' AS ld_lat, \
                f.statistics->'landing_snapshot'->>'Longitude' AS ld_lon, \
                f.statistics->>'fuel_consumed' AS fuel_consumed, \
                f.statistics->'landing_snapshot'->>'VSpd' AS landing_vs \
           FROM flights f JOIN users u ON u.id = f.user_id \
          WHERE f.status = 'ended' \
            AND f.ended_at IS NOT NULL \
            AND f.ended_at < NOW() - make_interval(secs => $1) \
            AND (f.ended_at, f.id) > ($2, $3) \
          ORDER BY f.ended_at, f.id \
          LIMIT $4",
    )
    .bind(SETTLE_SECS)
    .bind(start.ended_at)
    .bind(start.id)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;

    let has_more = rows.len() as i64 == limit;
    let next = rows
        .last()
        .map(|r| Cursor { ended_at: r.ended_at, id: r.id })
        .unwrap_or(start)
        .encode();
    let page = EndedPage {
        data: rows.into_iter().map(EndedFlight::from).collect(),
        next,
        has_more,
    };

    Ok((
        [
            // A page with a cursor is not stable until the settle window has
            // passed, and the head of the feed changes constantly.
            (header::CACHE_CONTROL, "no-store"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        Json(page),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(after: Option<&str>, since: Option<&str>) -> EndedQuery {
        EndedQuery {
            after: after.map(str::to_string),
            since: since.map(str::to_string),
            limit: None,
        }
    }

    fn row() -> Row {
        Row {
            id: 7,
            user_id: 3,
            discord_id: "123456789012345678".into(),
            username: "pilot".into(),
            global_name: Some("Pilot Name".into()),
            departure: "LFPG".into(),
            arrival: Some("EGLL".into()),
            ended_at: DateTime::from_timestamp(1_760_000_000, 0).unwrap(),
            end_reason: Some("landed".into()),
            resolved_icao: Some("A320".into()),
            airframe_name: Some("Airbus A320neo".into()),
            atc_id: Some("F-HBNA".into()),
            simulator: Some("MSFS".into()),
            start_time: Some("2026-01-01 10:00:00".into()),
            end_time: Some("2026-01-01 11:30:00".into()),
            takeoff_time: Some("2026-01-01 10:15:00".into()),
            landing_time: Some("2026-01-01 11:20:00".into()),
            to_lat: Some("49.0097".into()),
            to_lon: Some("2.5479".into()),
            ld_lat: Some("51.4700".into()),
            ld_lon: Some("-0.4543".into()),
            fuel_consumed: Some("2450.5".into()),
            landing_vs: Some("-142.0".into()),
        }
    }

    #[test]
    fn a_cursor_survives_the_round_trip_to_the_microsecond() {
        let c = Cursor {
            ended_at: DateTime::from_timestamp_micros(1_760_000_000_123_456).unwrap(),
            id: 42,
        };
        assert_eq!(Cursor::decode(&c.encode()), Some(c));
    }

    #[test]
    fn a_malformed_cursor_is_refused_rather_than_restarting_the_feed() {
        // Starting over from the epoch would replay every flight to a consumer
        // that only mangled its cursor.
        for bad in ["", "abc", "123", "123_", "_4", "x_4"] {
            assert!(Cursor::start(&query(Some(bad), None)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_cursor_wins_over_since_and_since_over_the_beginning() {
        let c = Cursor { ended_at: DateTime::from_timestamp(1_760_000_000, 0).unwrap(), id: 9 };
        let enc = c.encode();
        assert_eq!(Cursor::start(&query(Some(&enc), Some("2020-01-01T00:00:00Z"))).unwrap(), c);

        let s = Cursor::start(&query(None, Some("2026-10-01T00:00:00Z"))).unwrap();
        assert_eq!(s.ended_at, DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z").unwrap());
        assert_eq!(s.id, 0);

        assert_eq!(Cursor::start(&query(None, None)).unwrap().ended_at, DateTime::<Utc>::UNIX_EPOCH);
        assert!(Cursor::start(&query(None, Some("yesterday"))).is_err());
    }

    #[test]
    fn a_row_maps_to_the_fields_a_logbook_needs() {
        let f = EndedFlight::from(row());
        assert_eq!(f.pilot_name, "Pilot Name");
        assert_eq!(f.aircraft_icao.as_deref(), Some("A320"));
        assert_eq!(f.tail_number.as_deref(), Some("F-HBNA"));
        // Takeoff to landing, not the block times.
        assert_eq!(f.flight_seconds, Some(65 * 60));
        assert_eq!(f.start_time, parse_time("2026-01-01 10:00:00"));
        assert_eq!(f.fuel_consumed, Some(2450.5));
        assert_eq!(f.landing_vs_fpm, Some(-142.0));
        // CDG to LHR is about 188 nm.
        let d = f.distance_nm.unwrap();
        assert!((185.0..192.0).contains(&d), "got {d}");
    }

    #[test]
    fn what_the_app_left_blank_or_mangled_comes_back_absent() {
        let f = EndedFlight::from(Row {
            global_name: None,
            arrival: Some("  ".into()),
            resolved_icao: Some("".into()),
            atc_id: None,
            takeoff_time: None,
            landing_time: Some("not a time".into()),
            ld_lat: Some("NaN".into()),
            fuel_consumed: Some("lots".into()),
            ..row()
        });
        assert_eq!(f.pilot_name, "pilot");
        assert_eq!(f.arrival, None);
        assert_eq!(f.aircraft_icao, None);
        assert_eq!(f.tail_number, None);
        assert_eq!(f.landing_time, None);
        assert_eq!(f.distance_nm, None);
        assert_eq!(f.fuel_consumed, None);
        // No takeoff, so the span falls back to the block times.
        assert_eq!(f.flight_seconds, Some(90 * 60));
    }
}
