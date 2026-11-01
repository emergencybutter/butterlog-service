//! Server-side flight analysis (docs/sim-native-telemetry.md §5.2, rollout phase 3):
//! normalised samples in, the flight summary the app computes today out.

// Wired into ingest once the parity check passes on real sessions.
#![allow(dead_code, unused_imports)]

pub mod flight;
pub mod places;

pub use flight::{analyse, Endpoint, EventKind, FlightSummary};
pub use places::Places;

#[cfg(test)]
mod parity {
    //! The pattern fixtures against what the app's analyser made of the same flights,
    //! from its local logs (`butterlog_xp_20261101_010856.db`,
    //! `butterlog_20261101_015533.db`).

    use super::*;
    use crate::normalise::normalise_segment;
    use std::path::Path;
    use std::sync::OnceLock;

    fn places() -> &'static Places {
        static PLACES: OnceLock<Places> = OnceLock::new();
        PLACES.get_or_init(|| Places::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("data")).unwrap())
    }

    fn summary(fixture: &str) -> FlightSummary {
        let path = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
        let (_, samples) = normalise_segment(&std::fs::read(path).unwrap()).unwrap();
        let flights = analyse(&samples, places());
        assert_eq!(flights.len(), 1, "one flight");
        flights.into_iter().next().unwrap()
    }

    fn utc(ms: i64) -> String {
        chrono::DateTime::from_timestamp_millis(ms).unwrap().format("%H:%M:%S%.3f").to_string()
    }

    fn ms(hms: &str) -> i64 {
        let t = chrono::NaiveDateTime::parse_from_str(&format!("2026-11-01 {hms}"), "%Y-%m-%d %H:%M:%S%.3f");
        t.unwrap().and_utc().timestamp_millis()
    }

    fn near(what: &str, ours: f64, app: f64, tolerance: f64) {
        assert!((ours - app).abs() <= tolerance, "{what}: {ours} here, {app} in the app");
    }

    fn kinds(s: &FlightSummary) -> Vec<EventKind> {
        s.events.iter().map(|e| e.kind).collect()
    }

    /// What both flights share with the app, and the differences both have:
    /// - takeoff within 1.5 s: the app's samples were about a second apart;
    /// - touchdown rate within 50 fpm, from a frame before contact rather than up to a
    ///   second, and touchdown distance within 150 ft, the app's sample landing up to a
    ///   second (100 ft at 60 kt) late;
    /// - approach statistics within 15%, from every frame weighted by time;
    /// - landing G not compared: the app's was its first sample's on the ground, up to a
    ///   second after the impact the frames show (see `Landing::g`).
    fn check(s: &FlightSummary, app: &App) {
        assert_eq!(s.departure, Endpoint::Airport("KAST".into()));
        assert_eq!(s.arrival, Endpoint::Airport("KAST".into()));
        near("takeoff", s.takeoff_ms.unwrap() as f64, ms(app.takeoff) as f64, 1_500.0);
        near("max altitude", s.max_altitude_ft, app.max_altitude, 5.0);
        near("max ground speed", s.max_ground_speed_kt, app.max_ground_speed, 1.0);
        near("fuel", s.fuel_consumed_gal, app.fuel, 0.05);
        let l = s.landing.as_ref().unwrap();
        assert_eq!(l.runway.as_deref(), Some("32"));
        near("touchdown fpm", l.touchdown_fpm, app.touchdown_fpm, 50.0);
        near("offset", l.offset_percent.unwrap(), app.offset_pct, 10.0);
        near("threshold distance", l.threshold_dist_ft.unwrap(), app.dist_ft, 150.0);
        near("VS variance", l.vs_variance.unwrap(), app.vs_variance, app.vs_variance * 0.15);
        near("IAS variance", l.ias_variance.unwrap(), app.ias_variance, app.ias_variance * 0.15);
        near("stability", l.approach_stability.unwrap(), app.stability, 5.0);
        assert!((1.0..2.5).contains(&l.g), "landing G {}", l.g);
    }

    /// The app's summary of the same flight.
    struct App {
        takeoff: &'static str,
        max_altitude: f64,
        max_ground_speed: f64,
        fuel: f64,
        touchdown_fpm: f64,
        offset_pct: f64,
        dist_ft: f64,
        vs_variance: f64,
        ias_variance: f64,
        stability: f64,
    }

    #[test]
    fn xplane_pattern() {
        let s = summary("xplane_pattern.blsg.zst");
        check(&s, &App {
            takeoff: "01:09:47.776",
            max_altitude: 918.26,
            max_ground_speed: 117.77,
            fuel: 0.534,
            touchdown_fpm: -289.3,
            offset_pct: -95.16,
            dist_ft: 770.8,
            vs_variance: 420_836.0,
            ias_variance: 29.96,
            stability: 44.00,
        });
        // The app's events, and the parking it stopped logging 0.14 s before. The level-off
        // at 01:11 was paused past the 10 s a top of climb needs, in both.
        use EventKind::*;
        assert_eq!(kinds(&s), [Takeoff, TopOfClimb, TopOfDescent, Landing, Parked]);
        near("top of descent", s.events[2].wall_time_ms as f64, ms("01:12:42.687") as f64, 1_500.0);
        // True now; the app logged X-Plane's magnetic heading, 317.7.
        near("landing heading", s.events[3].heading.unwrap(), 332.8, 1.0);
    }

    #[test]
    fn msfs_pattern() {
        let s = summary("msfs_pattern.blsg.zst");
        // The session starts while MSFS loads, at another airport: the flight is what
        // follows the teleport to KAST, from the first move, as the app's log was.
        assert!(s.first_ms > ms("01:57:00.000"));
        // The app's takeoff is the first time the wheels left; this one ignores the
        // flicker of `SIM ON GROUND` at liftoff, 0.8 s.
        check(&s, &App {
            takeoff: "01:58:02.305",
            max_altitude: 1177.11,
            max_ground_speed: 118.86,
            fuel: 0.7565,
            touchdown_fpm: -171.9,
            offset_pct: -100.32,
            dist_ft: 856.4,
            vs_variance: 70_802.0,
            ias_variance: 22.31,
            stability: 54.12,
        });
        use EventKind::*;
        assert_eq!(kinds(&s), [Takeoff, TopOfClimb, TopOfDescent, Landing, Parked]);
        // Level for 10.8 s from 02:00:00.8. The app read 9 s, its timestamps cut to the
        // second, and its next sample was descending: it put both at the descent.
        near("top of climb", s.events[1].wall_time_ms as f64, ms("02:00:00.840") as f64, 100.0);
        near("top of descent", s.events[2].wall_time_ms as f64, ms("02:00:13.224") as f64, 1_500.0);
    }
}
