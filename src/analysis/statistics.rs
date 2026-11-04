//! A flight as the app's `flights.statistics` document describes it, built from the
//! telemetry instead (rollout phase 5): what the Discord post, the flight page and the feed
//! read. In shadow mode it is stored beside the app's and rendered the same way, to compare
//! (`analysis/compare.rs`); once they agree the service writes it in place of the app.
//!
//! Keys and conventions are the app's, so every reader takes it unchanged. That includes
//! the app's attitude signs: `Pitch` positive nose down, `Roll` positive left wing down.

use super::flight::{Endpoint, FlightSummary};
use super::Places;
use crate::normalise::{Field, Sample};
use butterlog_wire::SessionHeader;
use serde_json::{json, Map, Value};

/// The app's key for each field it logs, and the conversion from the canonical value.
const KEYS: &[(&str, Field, fn(f64) -> f64)] = &[
    ("Latitude", Field::Latitude, same),
    ("Longitude", Field::Longitude, same),
    ("AltB", Field::AltitudeIndicated, same),
    ("BaroA", Field::Baro, same),
    ("AltMSL", Field::AltitudeMsl, same),
    ("AltGPS", Field::AltitudeMsl, same),
    ("altitude_agl", Field::AltitudeAgl, same),
    ("pressure_altitude", Field::AltitudePressure, same),
    ("OAT", Field::Oat, same),
    ("IAS", Field::Ias, same),
    ("TAS", Field::Tas, same),
    ("GndSpd", Field::GroundSpeed, same),
    ("VSpd", Field::VerticalSpeed, same),
    ("Pitch", Field::Pitch, negated),
    ("Roll", Field::Roll, negated),
    ("LatAc", Field::GLateral, same),
    ("NormAc", Field::GNormal, same),
    ("gforce", Field::GNormal, same),
    ("HDG", Field::HeadingMagnetic, same),
    ("TRK", Field::Track, same),
    ("MagVar", Field::MagneticVariation, same),
    ("WndSpd", Field::WindSpeed, same),
    ("WndDr", Field::WindDirection, same),
    ("volt1", Field::Volts, same),
    ("amp1", Field::Amps, same),
    ("FQtyL", Field::FuelLeft, same),
    ("FQtyR", Field::FuelRight, same),
    ("E1 FFlow", Field::FuelFlow, same),
    ("E1 OilT", Field::OilTemp, same),
    ("E1 OilP", Field::OilPressure, same),
    ("E1 MAP", Field::Manifold, same),
    ("E1 RPM", Field::Rpm, same),
    ("E1 %Pwr", Field::PctRpm, same),
    ("E1 CHT1", Field::Cht, same),
    ("E1 EGT1", Field::Egt, same),
    ("COM1", Field::Com1, same),
    ("COM2", Field::Com2, same),
    ("NAV1", Field::Nav1, same),
    ("NAV2", Field::Nav2, same),
    ("AfcsOn", Field::Autopilot, same),
    ("sim_on_ground", Field::OnGround, same),
    ("xp_gear_ratio", Field::Gear, same),
    ("parking_brake", Field::ParkingBrake, same),
    ("nav_lights", Field::NavLights, same),
    ("beacon_lights", Field::BeaconLights, same),
    ("strobe_lights", Field::StrobeLights, same),
    ("taxi_lights", Field::TaxiLights, same),
    ("landing_lights", Field::LandingLights, same),
];

fn same(v: f64) -> f64 {
    v
}

fn negated(v: f64) -> f64 {
    -v
}

/// The app's timestamps: UTC, "2026-11-01 01:09:47.776".
fn app_time(ms: i64) -> Value {
    chrono::DateTime::from_timestamp_millis(ms)
        .map_or(Value::Null, |t| json!(t.format("%Y-%m-%d %H:%M:%S%.3f").to_string()))
}

/// One sample in the app's keys, the fields it has.
fn snapshot(s: &Sample) -> Map<String, Value> {
    KEYS.iter()
        .filter_map(|(key, field, convert)| Some((key.to_string(), json!(convert(s.get(*field)?)))))
        .collect()
}

/// Each field's largest value over the flight, as the app's `max_entries` (a field's
/// maximum, not its largest magnitude: `VSpd` is the best climb).
fn maxima(samples: &[&Sample]) -> Map<String, Value> {
    KEYS.iter()
        .filter_map(|(key, field, convert)| {
            let max = samples.iter().filter_map(|s| s.get(*field).map(convert)).reduce(f64::max)?;
            Some((key.to_string(), json!(max)))
        })
        .collect()
}

/// The fuel on board at the first sample with any, less that at the last, in the units of
/// `FQtyL` and `FQtyR`: the app's `fuel_consumed`, which it keeps in its log but has never
/// uploaded, so the feed's is empty for its flights. None when fuel was added on the way, by
/// a refuel or a reload, which makes the difference meaningless: the flight that began in
/// the air after an LFBO-EGJJ reload on 2026-11-02 came out at -24.2.
fn fuel_consumed(samples: &[&Sample]) -> Option<f64> {
    let totals: Vec<f64> = samples
        .iter()
        .filter_map(|s| Some(s.get(Field::FuelLeft)? + s.get(Field::FuelRight)?))
        .filter(|t| *t > 0.0)
        .collect();
    let (&first, &last) = (totals.first()?, totals.last()?);
    // More than sloshing: a quantity the gauges don't wander by.
    let added = REFUEL_FRACTION * first.max(1.0 / REFUEL_FRACTION);
    let mut lowest = first;
    for &t in &totals {
        if t > lowest + added {
            return None;
        }
        lowest = lowest.min(t);
    }
    Some(first - last)
}

/// Fuel going up by this fraction of the starting quantity (or by 1) is fuel added.
const REFUEL_FRACTION: f64 = 0.005;

/// How the app names the sim.
fn simulator(sim: &str) -> (&'static str, &'static str) {
    if sim.starts_with("xplane") { ("X-Plane", "12") } else { ("MSFS", "SimConnect") }
}

fn endpoint(e: &Endpoint, places: &Places) -> Value {
    let ident = e.to_string();
    let name = match e {
        Endpoint::Airport(id) => places.airport(id).map(|a| a.name.clone()),
        _ => None,
    };
    json!({ "icao": ident, "name": name.unwrap_or_else(|| ident.clone()) })
}

/// The statistics document for one flight of a session. `samples` are the session's.
pub fn statistics(header: &SessionHeader, f: &FlightSummary, samples: &[Sample], places: &Places) -> Value {
    let flight: Vec<&Sample> =
        samples.iter().filter(|s| (f.first_ms..=f.last_ms).contains(&s.wall_time_ms)).collect();
    let a = &header.aircraft;
    let identity = butterlog_identity::Resolver::embedded().resolve(&a.title, &a.livery);
    let (sim, sim_version) = simulator(&header.sim);

    let mut doc = json!({
        "airframe_name": a.title,
        "atc_model": a.icao,
        "atc_id": a.tail,
        "livery": a.livery,
        "resolved_icao": identity.icao.as_ref().map_or("", |m| m.icao.as_str()),
        "resolved_airline": identity.airline.as_ref().map_or("", |m| m.name.as_str()),
        "simulator": sim,
        "simulator_version": sim_version,
        "start_time": app_time(f.first_ms),
        "end_time": app_time(f.last_ms),
        "departure": endpoint(&f.departure, places),
        "takeoff_time": f.takeoff_roll_ms.map_or(Value::Null, app_time),
        "max_entries": maxima(&flight),
    });
    let doc_mut = doc.as_object_mut().expect("an object");
    // The frame the takeoff time is: the feed and the aircraft stats measure distance from it.
    if let Some(at) = f.takeoff_roll_ms.and_then(|ms| flight.iter().find(|s| s.wall_time_ms >= ms)) {
        doc_mut.insert("takeoff_snapshot".into(), Value::Object(snapshot(at)));
    }
    if let Some(burnt) = fuel_consumed(&flight) {
        doc_mut.insert("fuel_consumed".into(), json!(burnt));
    }
    if let Some(l) = &f.landing {
        let at = flight.iter().find(|s| s.wall_time_ms >= l.wall_time_ms).copied();
        let mut landing = at.map(snapshot).unwrap_or_default();
        // The touchdown as the frames show it, rather than the sample after.
        landing.insert("VSpd".into(), json!(l.touchdown_fpm));
        landing.insert("NormAc".into(), json!(l.g));
        landing.insert("gforce".into(), json!(l.g));
        let doc = doc_mut;
        doc.insert("arrival".into(), endpoint(&f.arrival, places));
        doc.insert("landing_time".into(), app_time(l.wall_time_ms));
        doc.insert("landing_snapshot".into(), Value::Object(landing));
        doc.insert("landing_score".into(), json!(l.score()));
        doc.insert("landing_offset_percent".into(), json!(l.offset_percent));
        doc.insert("landing_threshold_dist_ft".into(), json!(l.threshold_dist_ft));
        doc.insert("vs_variance".into(), json!(l.vs_variance));
        doc.insert("ias_variance".into(), json!(l.ias_variance));
    }
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::analyse;
    use crate::normalise::normalise_segment;
    use std::path::Path;

    #[test]
    fn the_pattern_reads_as_the_apps_statistics() {
        let places = Places::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("data")).unwrap();
        let path = format!("{}/tests/fixtures/xplane_pattern.blsg.zst", env!("CARGO_MANIFEST_DIR"));
        let (header, samples) = normalise_segment(&std::fs::read(path).unwrap()).unwrap();
        let f = &analyse(&samples, &places)[0];
        let s = statistics(&header, f, &samples, &places);

        assert_eq!(s["departure"], json!({ "icao": "KAST", "name": "Astoria Regional Airport" }));
        assert_eq!(s["arrival"]["icao"], "KAST");
        assert_eq!(s["simulator"], "X-Plane");
        // The app's takeoff_time is the roll passing 45 kt; its own log said 01:09:39.068.
        assert!(s["takeoff_time"].as_str().unwrap().starts_with("2026-11-01 01:09:3"), "{}", s["takeoff_time"]);
        assert!(s["landing_time"].as_str().unwrap().starts_with("2026-11-01 01:1"));

        let max = &s["max_entries"];
        assert!((max["AltMSL"].as_f64().unwrap() - f.max_altitude_ft).abs() < 1.0, "{}", max["AltMSL"]);
        assert!(max["VSpd"].as_f64().unwrap() > 300.0, "the best climb: {}", max["VSpd"]);

        let landing = &s["landing_snapshot"];
        let l = f.landing.as_ref().unwrap();
        assert_eq!(landing["VSpd"], json!(l.touchdown_fpm));
        assert_eq!(landing["NormAc"], json!(l.g));
        // The app's sign: positive nose down.
        let at = samples.iter().find(|s| s.wall_time_ms >= l.wall_time_ms).unwrap();
        assert_eq!(landing["Pitch"], json!(-at.get(Field::Pitch).unwrap()));
        assert_eq!(s["landing_score"], json!(l.score()));

        // The takeoff frame is the takeoff time's, on the runway at KAST.
        let takeoff = &s["takeoff_snapshot"];
        assert_eq!(takeoff["sim_on_ground"], json!(1.0));
        assert!((takeoff["Latitude"].as_f64().unwrap() - 46.157).abs() < 0.02, "{}", takeoff["Latitude"]);
        assert!(takeoff["IAS"].as_f64().unwrap() >= 45.0, "{}", takeoff["IAS"]);
        let burnt = s["fuel_consumed"].as_f64().unwrap();
        assert!(burnt > 0.0, "{burnt}");
    }

    #[test]
    fn fuel_added_on_the_way_is_no_fuel_used() {
        let at = |left: f64, right: f64| {
            let mut s = Sample::empty();
            s.set(Field::FuelLeft, left);
            s.set(Field::FuelRight, right);
            s
        };
        let burn = [at(100.0, 100.0), at(99.6, 100.1), at(90.0, 90.0)];
        assert_eq!(fuel_consumed(&burn.iter().collect::<Vec<_>>()), Some(20.0), "a slosh is not a refuel");
        let refuel = [at(100.0, 100.0), at(80.0, 80.0), at(110.0, 110.0), at(105.0, 105.0)];
        assert_eq!(fuel_consumed(&refuel.iter().collect::<Vec<_>>()), None);
    }
}
