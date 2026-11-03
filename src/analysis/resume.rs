//! A session that picks a flight up where the user's previous one left it, as the app
//! resumes its flight log after a restart (`flight_log_manager.rs`): the previous
//! session's last flight ended in the air, in the same aircraft, and this session starts
//! within [`RESUME_NM`] and [`RESUME_FT`] of where that one stopped. There is no limit on
//! the time between: a paused sim leaves the aircraft where it was. The sessions are then
//! analysed as one stream, and their flights are one. A reload that puts the aircraft
//! further away is a new flight, as in the app.

use super::flight::{Endpoint, FlightSummary};
use super::places::haversine_km;
use crate::normalise::{Field, Sample};
use butterlog_wire::flags;

pub const RESUME_NM: f64 = 10.0;
pub const RESUME_FT: f64 = 1000.0;
/// How many of the user's previous sessions are looked at, as the app looks at its three
/// most recent logs.
pub const CANDIDATES: i64 = 3;

fn position(s: &Sample) -> Option<(f64, f64, f64)> {
    Some((s.get(Field::Latitude)?, s.get(Field::Longitude)?, s.get(Field::AltitudeMsl)?))
}

/// Whether a session whose samples are `after` continues the stream `before`, whose
/// flights are `flights`.
pub fn continues(before: &[Sample], flights: &[FlightSummary], after: &[Sample]) -> bool {
    if !flights.last().is_some_and(|f| f.arrival == Endpoint::Airborne) {
        return false;
    }
    let (Some(a), Some(b)) = (before.iter().rev().find_map(position), after.iter().find_map(position)) else {
        return false;
    };
    haversine_km(a.0, a.1, b.0, b.1) / 1.852 <= RESUME_NM && (a.2 - b.2).abs() <= RESUME_FT
}

/// Appends a session's samples to the stream it continues, after a pause: the time
/// between the two, the app restarting, is not flying.
pub fn join(stream: &mut Vec<Sample>, mut next: Vec<Sample>) {
    if let (false, Some(first)) = (stream.is_empty(), next.first_mut()) {
        first.flags |= flags::PAUSED;
    }
    stream.append(&mut next);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{analyse, Places};
    use crate::normalise::normalise_segment;
    use std::path::Path;

    fn places() -> Places {
        Places::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("data")).unwrap()
    }

    /// The X-Plane pattern, cut in two halfway through its time in the air.
    fn halves() -> (Vec<Sample>, Vec<Sample>) {
        let path = format!("{}/tests/fixtures/xplane_pattern.blsg.zst", env!("CARGO_MANIFEST_DIR"));
        let (_, samples) = normalise_segment(&std::fs::read(path).unwrap()).unwrap();
        let airborne: Vec<usize> =
            (0..samples.len()).filter(|&i| samples[i].get(Field::OnGround) == Some(0.0)).collect();
        let cut = airborne[airborne.len() / 2];
        let mut first = samples;
        let second = first.split_off(cut);
        (first, second)
    }

    #[test]
    fn a_restart_in_the_air_is_the_same_flight() {
        let places = places();
        let (first, second) = halves();
        let flights = analyse(&first, &places);
        assert_eq!(flights.last().unwrap().arrival, Endpoint::Airborne);
        assert!(continues(&first, &flights, &second));

        let mut stream = first;
        join(&mut stream, second);
        let joined = analyse(&stream, &places);
        assert_eq!(joined.len(), 1);
        assert_eq!(joined[0].departure, Endpoint::Airport("KAST".into()));
        assert_eq!(joined[0].arrival, Endpoint::Airport("KAST".into()));
        assert!(joined[0].landing.is_some());
    }

    #[test]
    fn a_session_elsewhere_or_after_landing_is_not() {
        let places = places();
        let (first, mut second) = halves();
        let flights = analyse(&first, &places);
        // Put back 12 nm on: a reload, a new flight.
        for s in &mut second {
            if let Some(lat) = s.get(Field::Latitude) {
                s.set(Field::Latitude, lat + 0.2);
            }
        }
        assert!(!continues(&first, &flights, &second));

        // A flight that landed is not picked up again.
        let (mut whole, second) = halves();
        whole.extend(second);
        let landed = analyse(&whole, &places);
        assert!(!continues(&whole, &landed, &whole));
    }
}
