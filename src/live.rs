//! Public read model for an in-progress flight.
//!
//! The document is deliberately shaped like the app's `FlightDetailShare`, so
//! the share page's renderer draws a live flight and a finished one with the
//! same code. The only real translation is the summary adapter below: a live
//! flight carries `WebhookFlightSummary`-shaped statistics, while the share
//! renderer expects the app's `FlightSummary`.

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::track::{STALE_AFTER_SECS, TransposedPoints};
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct LiveQuery {
    /// Return only samples newer than this epoch. Omit for the whole track.
    pub since: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Pilot {
    pub user_id: i64,
    pub name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrentPosition {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude: f64,
    pub heading: f64,
    pub speed: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareScreenshot {
    pub timestamp: i64,
    pub url: String,
}

/// The app's `FlightSummary`, rebuilt from a live flight's statistics.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSummary {
    pub filename: String,
    pub start_icao: String,
    pub start_airport_name: String,
    pub end_icao: String,
    pub end_airport_name: String,
    pub start_time: String,
    pub end_time: String,
    pub duration_minutes: i64,
    pub file_size_bytes: u64,
    pub aircraft_title: String,
    pub livery: String,
    pub resolved_icao: String,
    pub resolved_airline: String,
    pub atc_model: String,
    pub atc_id: String,
    pub max_altitude: f64,
    pub max_ground_speed: f64,
    pub fuel_consumed: f64,
    pub events: Vec<serde_json::Value>,
    pub screenshot_count: usize,
    pub notes: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveDocument {
    pub status: String,
    pub last_epoch: Option<i64>,
    pub updated_ago_secs: i64,
    pub share_id: Option<String>,
    pub track_points: i64,
    /// Command types the connected sim currently advertises. Empty when the
    /// pilot has remote control switched off, which is the default.
    pub commands: Vec<String>,
    pub pilot: Pilot,
    pub current: Option<CurrentPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<LiveSummary>,
    /// Set on a delta response whose summary and screenshots did not change, so
    /// the client keeps what it already has.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub summary_unchanged: bool,
    pub transposed_data: TransposedPoints,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshots: Option<Vec<ShareScreenshot>>,
    pub remote_flight_id: i64,
}

fn str_at(v: &serde_json::Value, path: &[&str]) -> String {
    let mut cur = v;
    for key in path {
        match cur.get(*key) {
            Some(next) => cur = next,
            None => return String::new(),
        }
    }
    cur.as_str().unwrap_or("").to_string()
}

fn num_at(v: &serde_json::Value, path: &[&str]) -> f64 {
    let mut cur = v;
    for key in path {
        match cur.get(*key) {
            Some(next) => cur = next,
            None => return 0.0,
        }
    }
    cur.as_f64().unwrap_or(0.0)
}

/// Parse the app's ISO-ish timestamps. It emits both `%Y-%m-%d %H:%M:%S%.f` and
/// RFC3339 depending on the field, so accept either.
fn parse_time(ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if ts.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    let head = ts.split('.').next().unwrap_or(ts);
    chrono::NaiveDateTime::parse_from_str(head, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|dt| dt.and_utc())
}

/// Map a live flight's `WebhookFlightSummary`-shaped statistics onto the
/// `FlightSummary` the share renderer reads.
pub fn adapt_summary(
    statistics: &serde_json::Value,
    notes: Option<&str>,
    screenshot_count: usize,
    ended: bool,
) -> LiveSummary {
    let start_time = str_at(statistics, &["start_time"]);
    let end_time = str_at(statistics, &["end_time"]);

    // While airborne there is no end_time, so the duration has to run to now —
    // otherwise a live flight would show as zero minutes long for its whole
    // duration. Prefer takeoff/landing over block times when present.
    let from = parse_time(&str_at(statistics, &["takeoff_time"]))
        .or_else(|| parse_time(&start_time));
    let to = parse_time(&str_at(statistics, &["landing_time"]))
        .or_else(|| parse_time(&end_time))
        .or(if ended { None } else { Some(chrono::Utc::now()) });
    let duration_minutes = match (from, to) {
        (Some(a), Some(b)) if b > a => b.signed_duration_since(a).num_minutes(),
        _ => 0,
    };

    // Falling back to the closest airport keeps the header meaningful for a
    // flight still en route, which has no arrival yet.
    let mut end_icao = str_at(statistics, &["arrival", "icao"]);
    let mut end_airport_name = str_at(statistics, &["arrival", "name"]);
    if end_icao.is_empty() {
        end_icao = str_at(statistics, &["closest_airport", "icao"]);
        end_airport_name = str_at(statistics, &["closest_airport", "name"]);
    }

    LiveSummary {
        filename: String::new(),
        start_icao: str_at(statistics, &["departure", "icao"]),
        start_airport_name: str_at(statistics, &["departure", "name"]),
        end_icao,
        end_airport_name,
        start_time,
        end_time,
        duration_minutes,
        file_size_bytes: 0,
        aircraft_title: str_at(statistics, &["airframe_name"]),
        livery: str_at(statistics, &["livery"]),
        resolved_icao: str_at(statistics, &["resolved_icao"]),
        resolved_airline: str_at(statistics, &["resolved_airline"]),
        atc_model: str_at(statistics, &["atc_model"]),
        atc_id: str_at(statistics, &["atc_id"]),
        max_altitude: num_at(statistics, &["max_entries", "AltMSL"]),
        max_ground_speed: num_at(statistics, &["max_entries", "GndSpd"]),
        fuel_consumed: num_at(statistics, &["fuel_consumed"]),
        events: statistics
            .get("events")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        screenshot_count,
        notes: notes.unwrap_or("").to_string(),
    }
}

fn extract_current(statistics: &serde_json::Value) -> Option<CurrentPosition> {
    let snapshot = statistics.get("current_snapshot")?;
    let num = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| snapshot.get(*k).and_then(|v| v.as_f64()))
    };
    Some(CurrentPosition {
        latitude: num(&["Latitude", "latitude"])?,
        longitude: num(&["Longitude", "longitude"])?,
        altitude: num(&["AltMSL", "gps_altitude_msl", "AltB"]).unwrap_or(0.0),
        heading: num(&["HDG", "heading"]).unwrap_or(0.0),
        speed: num(&["GndSpd", "ground_speed"]).unwrap_or(0.0),
        phase: statistics
            .get("flight_phase")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

/// `active` while reporting, `stale` once it goes quiet, `ended` when the app
/// said so or the reaper gave up. The distinction lets a viewer tell a frozen
/// track from a moving one instead of watching a stopped aeroplane.
fn derive_status(stored: &str, updated_ago_secs: i64) -> &'static str {
    if stored == "ended" {
        "ended"
    } else if updated_ago_secs > STALE_AFTER_SECS {
        "stale"
    } else {
        "active"
    }
}

type FlightRow = (
    i64,
    i64,
    serde_json::Value,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
    String,
    Option<String>,
    Option<String>,
    String,
    i32,
);

/// Public: the live map and flight history pages are already open to anyone, so
/// the in-progress view of the same flight is too.
pub async fn live_flight_handler(
    State(state): State<AppState>,
    Path(flight_id): Path<i64>,
    Query(q): Query<LiveQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let row: Option<FlightRow> = sqlx::query_as(
        "SELECT f.id, f.user_id, f.statistics, f.notes, f.updated_at, u.username, u.global_name, \
                f.share_id, f.status, f.track_points \
           FROM flights f JOIN users u ON f.user_id = u.id \
          WHERE f.id = $1",
    )
    .bind(flight_id)
    .fetch_optional(&state.db)
    .await?;

    let (
        id,
        user_id,
        statistics,
        notes,
        updated_at,
        username,
        global_name,
        share_id,
        stored_status,
        track_points,
    ) = row.ok_or_else(|| AppError::NotFound("Flight not found".to_string()))?;

    let updated_ago_secs = chrono::Utc::now()
        .signed_duration_since(updated_at)
        .num_seconds()
        .max(0);
    let status = derive_status(&stored_status, updated_ago_secs);

    let last_epoch: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(sample_epoch) FROM flight_track_points WHERE flight_id = $1",
    )
    .bind(flight_id)
    .fetch_one(&state.db)
    .await?;

    // A poll that would return nothing new costs one round trip and no body.
    let etag = format!("\"{}-{}\"", last_epoch.unwrap_or(0), updated_at.timestamp());
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == etag)
        .unwrap_or(false)
    {
        return Ok((StatusCode::NOT_MODIFIED, cache_headers(status, &etag)).into_response());
    }

    let points: Vec<(i64, f32, f32, Option<f32>, Option<f32>, Option<f32>, Option<f32>, Option<f32>)> =
        sqlx::query_as(
            "SELECT sample_epoch, latitude, longitude, altitude, ias, vspeed, pitch, roll \
               FROM flight_track_points \
              WHERE flight_id = $1 AND sample_epoch > $2 \
              ORDER BY sample_epoch",
        )
        .bind(flight_id)
        .bind(q.since.unwrap_or(i64::MIN))
        .fetch_all(&state.db)
        .await?;

    // Transpose back into the columnar wire format the renderer decodes. The
    // first timestamp is absolute and the rest are deltas, matching the share.
    let mut transposed = TransposedPoints::default();
    let mut prev = 0i64;
    for (i, p) in points.iter().enumerate() {
        transposed
            .timestamps
            .push(if i == 0 { p.0 } else { p.0 - prev });
        prev = p.0;
        transposed.latitudes.push(p.1);
        transposed.longitudes.push(p.2);
        transposed.altitudes.push(p.3.unwrap_or(0.0));
        transposed.ias.push(p.4.unwrap_or(0.0));
        transposed.vspeed.push(p.5.unwrap_or(0.0));
        transposed.pitch.push(p.6.unwrap_or(0.0));
        transposed.roll.push(p.7.unwrap_or(0.0));
    }

    // On a delta the client already has the summary and gallery; sending them
    // again on every 10s poll is most of the payload.
    let is_delta = q.since.is_some();
    let (summary, screenshots) = if is_delta {
        (None, None)
    } else {
        let shots: Vec<(String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "SELECT url, created_at FROM screenshots WHERE flight_id = $1 ORDER BY id",
        )
        .bind(flight_id)
        .fetch_all(&state.db)
        .await?;

        let summary = adapt_summary(
            &statistics,
            notes.as_deref(),
            shots.len(),
            status == "ended",
        );
        let shots = shots
            .into_iter()
            .map(|(url, created_at)| ShareScreenshot {
                timestamp: created_at.timestamp(),
                url,
            })
            .collect();
        (Some(summary), Some(shots))
    };

    let doc = LiveDocument {
        status: status.to_string(),
        last_epoch,
        updated_ago_secs,
        share_id,
        track_points: track_points as i64,
        commands: crate::commands::advertised_commands(&statistics),
        pilot: Pilot {
            user_id,
            name: global_name.unwrap_or(username),
        },
        current: extract_current(&statistics),
        summary,
        summary_unchanged: is_delta,
        transposed_data: transposed,
        screenshots,
        remote_flight_id: id,
    };

    Ok((StatusCode::OK, cache_headers(status, &etag), Json(doc)).into_response())
}

/// A finished flight never changes, so it caches hard; a live one must not.
fn cache_headers(status: &str, etag: &str) -> [(header::HeaderName, String); 3] {
    let cache = if status == "ended" {
        "public, max-age=86400"
    } else {
        "no-store"
    };
    [
        (header::CACHE_CONTROL, cache.to_string()),
        (header::ETAG, etag.to_string()),
        (
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            "*".to_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_distinguishes_active_stale_and_ended() {
        assert_eq!(derive_status("active", 5), "active");
        assert_eq!(derive_status("active", STALE_AFTER_SECS + 1), "stale");
        // An ended flight stays ended however recently it was touched.
        assert_eq!(derive_status("ended", 1), "ended");
    }

    #[test]
    fn arrival_falls_back_to_the_closest_airport_while_en_route() {
        let stats = json!({
            "departure": { "icao": "KLAX", "name": "Los Angeles Intl" },
            "closest_airport": { "icao": "KSFO", "name": "San Francisco Intl" }
        });
        let s = adapt_summary(&stats, None, 0, false);
        assert_eq!(s.start_icao, "KLAX");
        assert_eq!(s.end_icao, "KSFO");
        assert_eq!(s.end_airport_name, "San Francisco Intl");
    }

    #[test]
    fn a_real_arrival_beats_the_closest_airport() {
        let stats = json!({
            "arrival": { "icao": "KJFK", "name": "Kennedy" },
            "closest_airport": { "icao": "KSFO", "name": "San Francisco Intl" }
        });
        assert_eq!(adapt_summary(&stats, None, 0, false).end_icao, "KJFK");
    }

    #[test]
    fn an_airborne_flight_has_a_running_duration() {
        let takeoff = chrono::Utc::now() - chrono::Duration::minutes(42);
        let stats = json!({
            "takeoff_time": takeoff.format("%Y-%m-%d %H:%M:%S").to_string(),
        });
        // No landing_time yet: the duration has to run to now, or a live flight
        // reads as zero minutes long for its entire duration.
        let s = adapt_summary(&stats, None, 0, false);
        assert!((41..=43).contains(&s.duration_minutes), "got {}", s.duration_minutes);
    }

    #[test]
    fn an_ended_flight_without_a_landing_time_does_not_keep_counting() {
        let takeoff = chrono::Utc::now() - chrono::Duration::minutes(42);
        let stats = json!({
            "takeoff_time": takeoff.format("%Y-%m-%d %H:%M:%S").to_string(),
        });
        assert_eq!(adapt_summary(&stats, None, 0, true).duration_minutes, 0);
    }

    #[test]
    fn duration_spans_takeoff_to_landing_when_both_are_known() {
        let stats = json!({
            "takeoff_time": "2026-01-01 10:00:00",
            "landing_time": "2026-01-01 12:30:00",
        });
        assert_eq!(adapt_summary(&stats, None, 0, true).duration_minutes, 150);
    }

    #[test]
    fn max_entries_drive_the_peak_stats() {
        let stats = json!({ "max_entries": { "AltMSL": 37000.0, "GndSpd": 488.0 } });
        let s = adapt_summary(&stats, None, 0, false);
        assert_eq!(s.max_altitude, 37000.0);
        assert_eq!(s.max_ground_speed, 488.0);
    }

    #[test]
    fn missing_statistics_fields_degrade_to_empty_rather_than_failing() {
        let s = adapt_summary(&json!({}), None, 0, false);
        assert_eq!(s.start_icao, "");
        assert_eq!(s.max_altitude, 0.0);
        assert!(s.events.is_empty());
    }

    #[test]
    fn timestamps_parse_in_both_formats_the_app_emits() {
        assert!(parse_time("2026-01-01 10:00:00").is_some());
        assert!(parse_time("2026-01-01 10:00:00.250").is_some());
        assert!(parse_time("2026-01-01T10:00:00Z").is_some());
        assert!(parse_time("").is_none());
    }

    #[test]
    fn current_position_needs_a_fix_but_tolerates_missing_extras() {
        let stats = json!({ "current_snapshot": { "Latitude": 1.0, "Longitude": 2.0 } });
        let pos = extract_current(&stats).unwrap();
        assert_eq!(pos.latitude, 1.0);
        assert_eq!(pos.altitude, 0.0);
        assert!(extract_current(&json!({ "current_snapshot": {} })).is_none());
    }
}
