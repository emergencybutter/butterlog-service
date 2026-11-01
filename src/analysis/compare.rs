//! The comparison page's data (`/content/telemetry`): a pilot's telemetry flights, each
//! measure as the service and the app saw it, and whether the difference is beyond what
//! the parity check allows (the tolerances of the fixture tests in `analysis/mod.rs`).

use crate::error::AppError;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

pub struct Measure {
    pub label: &'static str,
    pub service: String,
    pub app: String,
    pub difference: String,
    /// Beyond the tolerance.
    pub off: bool,
}

pub struct FlightRow {
    pub when: String,
    pub sim: String,
    pub aircraft: String,
    pub route: String,
    /// The app's flight, when the service found it.
    pub app_flight_id: Option<i64>,
    pub analyser_version: i32,
    pub measures: Vec<Measure>,
    pub off_count: usize,
}

pub struct FailedSession {
    pub when: String,
    pub sim: String,
    pub aircraft: String,
    pub error: String,
}

/// How a difference is judged.
enum Tolerance {
    /// Must be equal.
    Same,
    Absolute(f64),
    /// A fraction of the app's value.
    Relative(f64),
}

fn number(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

fn shown(v: Option<f64>, decimals: usize) -> String {
    v.map_or("—".into(), |x| format!("{x:.decimals$}"))
}

/// With its sign, except a difference that rounds to nothing: "0", not "-0".
fn signed(d: f64, decimals: usize) -> String {
    let s = format!("{d:+.decimals$}");
    if s[1..].chars().all(|c| c == '0' || c == '.') { "0".into() } else { s }
}

/// A `{service, app}` pair from the parity document.
fn pair(label: &'static str, p: &Value, decimals: usize, tolerance: Tolerance) -> Measure {
    if let Tolerance::Same = tolerance {
        let (s, a) = (text(&p["service"]), text(&p["app"]));
        let off = matches!((&s, &a), (Some(s), Some(a)) if s != a);
        return Measure {
            label,
            service: s.unwrap_or("—".into()),
            app: a.unwrap_or("—".into()),
            difference: if off { "differs".into() } else { String::new() },
            off,
        };
    }
    let (s, a) = (number(&p["service"]), number(&p["app"]));
    let (difference, off) = match (s, a) {
        (Some(s), Some(a)) => {
            let d = s - a;
            let off = match tolerance {
                Tolerance::Absolute(t) => d.abs() > t,
                Tolerance::Relative(r) => d.abs() > a.abs() * r,
                Tolerance::Same => false,
            };
            (signed(d, decimals), off)
        }
        _ => (String::new(), false),
    };
    Measure { label, service: shown(s, decimals), app: shown(a, decimals), difference, off }
}

/// A time difference in seconds, service minus app.
fn seconds(label: &'static str, v: &Value, tolerance: f64) -> Measure {
    let d = number(v);
    Measure {
        label,
        service: String::new(),
        app: String::new(),
        difference: d.map_or("—".into(), |d| format!("{d:+.1} s")),
        off: d.is_some_and(|d| d.abs() > tolerance),
    }
}

/// The measures of one flight's parity document, in reading order.
pub fn measures(parity: &Value) -> Vec<Measure> {
    use Tolerance::*;
    vec![
        pair("Departure", &parity["departure"], 0, Same),
        pair("Arrival", &parity["arrival"], 0, Same),
        seconds("Takeoff roll (45 kt)", &parity["takeoff_s"], 1.5),
        seconds("Touchdown", &parity["landing_s"], 1.5),
        pair("Max altitude, ft", &parity["max_altitude_ft"], 0, Absolute(5.0)),
        pair("Touchdown distance, ft", &parity["landing_threshold_dist_ft"], 0, Absolute(150.0)),
        pair("Centreline offset, %", &parity["landing_offset_percent"], 1, Absolute(10.0)),
        pair("VS variance", &parity["vs_variance"], 0, Relative(0.15)),
        pair("IAS variance", &parity["ias_variance"], 1, Relative(0.15)),
        // Not judged: the service's landing G is the impact the frames show, the app's
        // whatever its next sample caught (see `Landing::g`), so scores are expected to
        // differ on firm landings.
        Measure { off: false, ..pair("Landing score", &parity["landing_score"], 0, Absolute(f64::INFINITY)) },
    ]
}

fn utc(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// The pilot's latest telemetry flights, newest first.
pub async fn flights(db: &PgPool, user_id: i64, limit: i64) -> Result<Vec<FlightRow>, AppError> {
    let rows: Vec<(DateTime<Utc>, String, Value, String, String, Option<i64>, i32, Option<Value>)> = sqlx::query_as(
        "SELECT f.first_at, s.sim, s.aircraft, f.departure, f.arrival, f.flight_id, \
                f.analyser_version, f.parity \
           FROM telemetry_flights f JOIN telemetry_sessions s ON s.id = f.session_id \
          WHERE s.user_id = $1 \
          ORDER BY f.first_at DESC LIMIT $2",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(first, sim, aircraft, departure, arrival, app_flight_id, analyser_version, parity)| {
            let measures = parity.as_ref().map(measures).unwrap_or_default();
            FlightRow {
                when: utc(first),
                sim,
                aircraft: aircraft["title"].as_str().unwrap_or_default().to_string(),
                route: format!("{departure} → {arrival}"),
                app_flight_id,
                analyser_version,
                off_count: measures.iter().filter(|m| m.off).count(),
                measures,
            }
        })
        .collect())
}

/// The pilot's sessions whose analysis failed, newest first.
pub async fn failures(db: &PgPool, user_id: i64, limit: i64) -> Result<Vec<FailedSession>, AppError> {
    let rows: Vec<(DateTime<Utc>, String, Value, String)> = sqlx::query_as(
        "SELECT started_at, sim, aircraft, analysis_error FROM telemetry_sessions \
          WHERE user_id = $1 AND analysis_error IS NOT NULL \
          ORDER BY started_at DESC LIMIT $2",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(started, sim, aircraft, error)| FailedSession {
            when: utc(started),
            sim,
            aircraft: aircraft["title"].as_str().unwrap_or_default().to_string(),
            error,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn by_label<'a>(ms: &'a [Measure], label: &str) -> &'a Measure {
        ms.iter().find(|m| m.label == label).unwrap()
    }

    #[test]
    fn differences_beyond_tolerance_are_off() {
        // The ATR flight of 2026-11-01, analyser version 2.
        let parity = json!({
            "departure": { "service": "CYZF", "app": "CYZF" },
            "arrival": { "service": "CYZF", "app": "CYZC" },
            "takeoff_s": -0.603,
            "landing_s": 2.4,
            "max_altitude_ft": { "service": 1734.94, "app": 1734.98 },
            "landing_threshold_dist_ft": { "service": 3796.2, "app": 3869.0 },
            "landing_offset_percent": { "service": -0.08, "app": -4.34 },
            "vs_variance": { "service": 2_598_698.4, "app": 2_527_526.8 },
            "ias_variance": { "service": 611.8, "app": 800.0 },
            "landing_score": { "service": -538.5, "app": -443.4 },
        });
        let ms = measures(&parity);
        let off: Vec<_> = ms.iter().filter(|m| m.off).map(|m| m.label).collect();
        assert_eq!(off, ["Arrival", "Touchdown", "IAS variance"]);
        assert_eq!(by_label(&ms, "Takeoff roll (45 kt)").difference, "-0.6 s");
        assert_eq!(by_label(&ms, "Max altitude, ft").difference, "0");
        assert_eq!(by_label(&ms, "Landing score").difference, "-95");
    }

    #[test]
    fn the_page_renders() {
        use askama::Template;
        let parity = json!({
            "departure": { "service": "CYZF", "app": "CYZF" },
            "arrival": { "service": "CYZF", "app": "CYZF" },
            "takeoff_s": -0.603,
            "landing_s": 2.4,
            "max_altitude_ft": { "service": 1734.94, "app": 1734.98 },
            "landing_threshold_dist_ft": { "service": 3796.2, "app": 3869.0 },
            "landing_offset_percent": { "service": -0.08, "app": -4.34 },
            "vs_variance": { "service": 2_598_698.4, "app": 2_527_526.8 },
            "ias_variance": { "service": 611.8, "app": 636.2 },
            "landing_score": { "service": -538.5, "app": -443.4 },
        });
        let ms = measures(&parity);
        let page = crate::templates::TelemetryPage {
            nav: crate::templates::Nav::anonymous("flights", "Telemetry"),
            flights: vec![FlightRow {
                when: "2026-11-01 14:51 UTC".into(),
                sim: "xplane12".into(),
                aircraft: "ATR 72-600".into(),
                route: "CYZF → CYZF".into(),
                app_flight_id: Some(548),
                analyser_version: 2,
                off_count: ms.iter().filter(|m| m.off).count(),
                measures: ms,
            }],
            failures: vec![FailedSession {
                when: "2026-11-01 15:02 UTC".into(),
                sim: "msfs2024".into(),
                aircraft: "C172SP <G1000>".into(),
                error: "chunk 3 is truncated".into(),
            }],
        };
        let html = page.render().unwrap();
        assert!(html.contains("CYZF → CYZF") && html.contains("/content/flights/548"));
        assert!(html.contains("C172SP &lt;G1000&gt;"), "escaped");
        if let Ok(out) = std::env::var("RENDER_TELEMETRY_PAGE") {
            std::fs::write(out, html).unwrap();
        }
    }

    #[test]
    fn a_missing_app_value_is_not_off() {
        let ms = measures(&json!({ "landing_offset_percent": { "service": 3.0, "app": null }, "takeoff_s": null }));
        let offset = by_label(&ms, "Centreline offset, %");
        assert_eq!((offset.service.as_str(), offset.app.as_str(), offset.off), ("3.0", "—", false));
        assert!(!by_label(&ms, "Takeoff roll (45 kt)").off);
    }
}
