//! The comparison page's data (`/content/telemetry`): a pilot's telemetry flights, each
//! measure as the service and the app saw it, and whether the difference is beyond what
//! the parity check allows (the tolerances of the fixture tests in `analysis/mod.rs`).
//! Below each, the Discord post each side's statistics document makes (rollout phase 5's
//! shadow mode): the service's is never posted, only compared.

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
    /// The two posts line by line; empty without both documents.
    pub post: Vec<PostLine>,
    pub post_off_count: usize,
    /// The flight page, the feed and the aircraft stats, as each side's statistics make them.
    pub pages: Vec<PostLine>,
    pub pages_off_count: usize,
}

/// One line of the Discord post, as each side's statistics make it.
pub struct PostLine {
    pub label: String,
    pub service: String,
    pub app: String,
    /// Different beyond rounding: a number off by more than 5% (and 1 unit), or anything
    /// else that isn't equal.
    pub off: bool,
}

/// The first number in a line ("-382.56 fpm", "Currently 0.1 nautical miles...").
fn first_number(s: &str) -> Option<f64> {
    let start = s.find(|c: char| c.is_ascii_digit() || c == '-')?;
    let rest = &s[start..];
    let end = rest[1..].find(|c: char| !(c.is_ascii_digit() || c == '.')).map_or(rest.len(), |i| i + 1);
    rest[..end].parse().ok()
}

/// The line without its numbers, to tell a different number from different words.
fn words(s: &str) -> String {
    s.replace(|c: char| c.is_ascii_digit() || c == '.' || c == '-', "")
}

fn close(service: &str, app: &str) -> bool {
    if service == app {
        return true;
    }
    match (first_number(service), first_number(app)) {
        (Some(s), Some(a)) if words(service) == words(app) => (s - a).abs() <= (a.abs() * 0.05).max(1.0),
        _ => false,
    }
}

/// The two posts, line by line: the service's lines in order, then those only the app's has.
pub fn post_lines(service: &Value, app: &Value) -> Vec<PostLine> {
    let ours = crate::discord::post_lines(service);
    let theirs = crate::discord::post_lines(app);
    let find = |lines: &[(String, String)], label: &str| {
        lines.iter().find(|(l, _)| l == label).map(|(_, v)| v.clone())
    };
    let mut out: Vec<PostLine> = ours
        .iter()
        .map(|(label, s)| {
            let a = find(&theirs, label);
            PostLine {
                label: label.clone(),
                service: s.clone(),
                off: a.as_deref().is_none_or(|a| !close(s, a)),
                app: a.unwrap_or_default(),
            }
        })
        .collect();
    for (label, a) in &theirs {
        if find(&ours, label).is_none() {
            out.push(PostLine { label: label.clone(), service: String::new(), app: a.clone(), off: true });
        }
    }
    out
}

/// One figure a page shows from a statistics document, with how far two may differ.
struct Reading {
    label: String,
    text: String,
    /// Compared with its tolerance; without, the texts are, as a post's lines are.
    value: Option<(f64, Tolerance)>,
}

fn reading(label: &str, text: String) -> Reading {
    Reading { label: label.to_string(), text, value: None }
}

/// Seconds either side for a time: the takeoff and landing are found to the frame.
const TIME_S: f64 = 2.0;

/// What the flight page and the flight list, the feed and the aircraft stats show of a
/// statistics document.
fn readings(doc: &Value) -> Vec<Reading> {
    let mut out = Vec::new();
    let landing = &doc["landing_snapshot"];
    if let Some(vspd) = landing["VSpd"].as_f64() {
        out.push(reading("Flight page · Badge", crate::landing_rating(vspd).1.to_string()));
    }
    for (label, value) in crate::telemetry::labeled_values(landing, "landing") {
        out.push(reading(&format!("Flight page · Touchdown · {label}"), value));
    }
    for (label, value) in crate::telemetry::labeled_values_for_keys(&doc["max_entries"], crate::PEAK_KEYS) {
        out.push(reading(&format!("Flight page · Peak · {label}"), value));
    }

    let text = |key: &str| doc[key].as_str().map(str::to_string);
    for (label, key) in [("Start", "start_time"), ("Takeoff", "takeoff_time"), ("Landing", "landing_time"), ("End", "end_time")] {
        if let Some(t) = text(key).as_deref().and_then(crate::live::parse_time) {
            out.push(Reading {
                label: format!("Feed · {label}"),
                text: t.format("%H:%M:%S").to_string(),
                value: Some((t.timestamp_millis() as f64 / 1000.0, Tolerance::Absolute(TIME_S))),
            });
        }
    }
    let span = crate::flight_span_seconds(&text("takeoff_time"), &text("landing_time"), &text("start_time"), &text("end_time"));
    if let Some(s) = span {
        out.push(Reading {
            label: "Feed and aircraft stats · Flight time".into(),
            text: format!("{}h {:02}m {:02}s", s / 3600, s / 60 % 60, s % 60),
            value: Some((s as f64, Tolerance::Absolute(2.0 * TIME_S))),
        });
    }
    let fix = |snapshot: &Value| Some((snapshot["Latitude"].as_f64()?, snapshot["Longitude"].as_f64()?));
    if let (Some(a), Some(b)) = (fix(&doc["takeoff_snapshot"]), fix(landing)) {
        let nm = crate::haversine_nm(a.0, a.1, b.0, b.1);
        out.push(Reading {
            label: "Feed and aircraft stats · Distance".into(),
            text: format!("{nm:.1} nm"),
            value: Some((nm, Tolerance::Absolute(0.5))),
        });
    }
    if let Some(burnt) = doc["fuel_consumed"].as_f64().or_else(|| text("fuel_consumed")?.parse().ok()) {
        out.push(Reading {
            label: "Feed · Fuel used".into(),
            text: format!("{burnt:.1}"),
            value: Some((burnt, Tolerance::Relative(0.02))),
        });
    }
    out
}

fn agrees(service: &Reading, app: &Reading) -> bool {
    match (&service.value, &app.value) {
        (Some((s, Tolerance::Absolute(t))), Some((a, _))) => (s - a).abs() <= *t,
        (Some((s, Tolerance::Relative(r))), Some((a, _))) => (s - a).abs() <= a.abs() * r,
        _ => close(&service.text, &app.text),
    }
}

/// The other pages, line by line. A figure only the service has is not off: the app
/// never sent it (fuel used), and the service filling it in is the point.
pub fn page_lines(service: &Value, app: &Value) -> Vec<PostLine> {
    let (ours, theirs) = (readings(service), readings(app));
    let mut out: Vec<PostLine> = ours
        .iter()
        .map(|s| {
            let a = theirs.iter().find(|a| a.label == s.label);
            PostLine {
                label: s.label.clone(),
                service: s.text.clone(),
                app: a.map_or(String::new(), |a| a.text.clone()),
                off: a.is_some_and(|a| !agrees(s, a)),
            }
        })
        .collect();
    for a in &theirs {
        if !ours.iter().any(|s| s.label == a.label) {
            out.push(PostLine { label: a.label.clone(), service: String::new(), app: a.text.clone(), off: true });
        }
    }
    out
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
    type Row =
        (DateTime<Utc>, String, Value, String, String, Option<i64>, i32, Option<Value>, Option<Value>, Option<Value>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT f.first_at, s.sim, s.aircraft, f.departure, f.arrival, f.flight_id, \
                f.analyser_version, f.parity, f.statistics, a.statistics \
           FROM telemetry_flights f JOIN telemetry_sessions s ON s.id = f.session_id \
           LEFT JOIN flights a ON a.id = f.flight_id \
          WHERE s.user_id = $1 \
          ORDER BY f.first_at DESC LIMIT $2",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(first, sim, aircraft, departure, arrival, app_flight_id, analyser_version, parity, ours, theirs)| {
            let measures = parity.as_ref().map(measures).unwrap_or_default();
            let (post, pages) = match (&ours, &theirs) {
                (Some(ours), Some(theirs)) => (post_lines(ours, theirs), page_lines(ours, theirs)),
                _ => (Vec::new(), Vec::new()),
            };
            FlightRow {
                when: utc(first),
                sim,
                aircraft: aircraft["title"].as_str().unwrap_or_default().to_string(),
                route: format!("{departure} → {arrival}"),
                app_flight_id,
                analyser_version,
                off_count: measures.iter().filter(|m| m.off).count(),
                measures,
                post_off_count: post.iter().filter(|l| l.off).count(),
                post,
                pages_off_count: pages.iter().filter(|l| l.off).count(),
                pages,
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
                post_off_count: 1,
                post: vec![PostLine {
                    label: "Landing Stats · Vertical Speed".into(),
                    service: "-171.90 fpm".into(),
                    app: "-212.40 fpm".into(),
                    off: true,
                }],
                pages_off_count: 0,
                pages: vec![PostLine {
                    label: "Feed and aircraft stats · Distance".into(),
                    service: "112.4 nm".into(),
                    app: "112.3 nm".into(),
                    off: false,
                }],
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
        assert!(html.contains("-212.40 fpm"));
        assert!(html.contains("112.4 nm"));
        if let Ok(out) = std::env::var("RENDER_TELEMETRY_PAGE") {
            std::fs::write(out, html).unwrap();
        }
    }

    #[test]
    fn post_lines_differ_beyond_rounding() {
        let doc = |vspd: f64, ias: f64, name: &str| json!({
            "departure": { "icao": "KAST", "name": name },
            "arrival": { "icao": "KAST", "name": name },
            "landing_time": "2026-11-01 01:14:00.000",
            "airframe_name": "Cessna Skyhawk",
            "landing_snapshot": { "VSpd": vspd },
            "max_entries": { "IAS": ias },
        });
        let lines = post_lines(&doc(-171.9, 117.0, "Astoria Regional Airport"), &doc(-289.3, 116.0, "Astoria"));
        let off: Vec<_> = lines.iter().filter(|l| l.off).map(|l| l.label.as_str()).collect();
        assert_eq!(off, ["Landed", "Landing Stats · Vertical Speed"]);
        assert_eq!(first_number("Currently 0.1 nautical miles"), Some(0.1));
        assert_eq!(first_number("-382.56 fpm"), Some(-382.56));
    }

    #[test]
    fn the_pages_differ_beyond_their_tolerances() {
        // EGJJ-EGPH of 2026-11-03: the app's takeoff time is 19 s after its takeoff frame.
        let doc = |takeoff: &str, to_lat: Option<f64>, fuel: Option<f64>| {
            let mut d = json!({
                "start_time": "2026-11-03 18:01:31.139",
                "takeoff_time": takeoff,
                "landing_time": "2026-11-03 21:29:31.619",
                "end_time": "2026-11-03 21:30:52.812",
                "landing_snapshot": { "VSpd": -480.3, "NormAc": 2.01, "Latitude": 55.9471, "Longitude": -3.3812 },
                "max_entries": { "AltB": 20376.4 },
            });
            if let Some(lat) = to_lat {
                d["takeoff_snapshot"] = json!({ "Latitude": lat, "Longitude": -2.1943 });
            }
            if let Some(f) = fuel {
                d["fuel_consumed"] = json!(f);
            }
            d
        };
        let service = doc("2026-11-03 18:03:30.161", Some(49.2072), Some(217.4));
        let lines = page_lines(&service, &doc("2026-11-03 18:03:49.892", Some(49.2080), None));
        let off: Vec<_> = lines.iter().filter(|l| l.off).map(|l| l.label.as_str()).collect();
        assert_eq!(off, ["Feed · Takeoff", "Feed and aircraft stats · Flight time"]);
        let line = |label: &str| lines.iter().find(|l| l.label == label).unwrap();
        assert_eq!(line("Flight page · Badge").service, "HARD");
        assert_eq!(line("Feed · Fuel used").app, "", "the app never sends it");

        // The service without a takeoff frame or fuel: what only the app has is off.
        let lines = page_lines(&doc("2026-11-03 18:03:30.161", None, None), &service);
        let off: Vec<_> = lines.iter().filter(|l| l.off).map(|l| l.label.as_str()).collect();
        assert_eq!(off, ["Feed and aircraft stats · Distance", "Feed · Fuel used"]);
    }

    #[test]
    fn a_missing_app_value_is_not_off() {
        let ms = measures(&json!({ "landing_offset_percent": { "service": 3.0, "app": null }, "takeoff_s": null }));
        let offset = by_label(&ms, "Centreline offset, %");
        assert_eq!((offset.service.as_str(), offset.app.as_str(), offset.off), ("3.0", "—", false));
        assert!(!by_label(&ms, "Takeoff roll (45 kt)").off);
    }
}
