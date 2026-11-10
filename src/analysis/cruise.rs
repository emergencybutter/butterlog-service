//! The altitude a flight cruised at, from its track (`pattern::points`), for its Discord
//! post: the level the aircraft held longest in the air, high in the flight.

use super::pattern::Pt;

/// Flown level: climbing or descending slower than this (fpm).
const LEVEL_FPM: f64 = 300.0;
/// In the air: faster than this (kt), so taxiing and holding short don't count.
const AIRBORNE_KT: f64 = 40.0;
/// Bins of this many feet.
const BIN_FT: f64 = 100.0;
/// Of the levels held, only those at least this share of the highest: a long hold or a
/// circuit below the cruise is not it.
const HIGH_SHARE: f64 = 0.6;
/// The level must be held this long (s) to be a cruise at all.
const MIN_HELD_S: i64 = 60;
/// A gap longer than this between two points (s) isn't counted as held.
const MAX_GAP_S: i64 = 60;

/// The cruise altitude in feet, before rounding; `None` when nothing was held level long
/// enough in the air.
pub fn cruise_altitude(pts: &[Pt]) -> Option<f64> {
    let mut held: std::collections::BTreeMap<i64, i64> = std::collections::BTreeMap::new();
    for w in pts.windows(2) {
        let (p, next) = (&w[0], &w[1]);
        let dt = next.t - p.t;
        let (Some(alt), Some(vs), Some(ias)) = (p.alt, p.vs, p.ias) else { continue };
        if dt <= 0 || dt > MAX_GAP_S || vs.abs() > LEVEL_FPM || ias < AIRBORNE_KT {
            continue;
        }
        *held.entry((alt / BIN_FT).round() as i64).or_default() += dt;
    }
    // A level and its neighbours: a cruise wanders a bin either way.
    let around = |bin: i64| (bin - 1..=bin + 1).filter_map(|b| held.get(&b)).sum::<i64>();
    let highest = *held.iter().filter(|(_, s)| **s >= MIN_HELD_S / 3).map(|(b, _)| b).max()?;
    let floor = (highest as f64 * HIGH_SHARE).floor() as i64;
    let (bin, secs) = held
        .keys()
        .filter(|&&b| b >= floor)
        .map(|&b| (b, around(b)))
        .max_by_key(|&(b, s)| (s, b))?;
    (secs >= MIN_HELD_S).then_some(bin as f64 * BIN_FT)
}

/// A cruise altitude as a pilot says it: a flight level from 18,000 ft, to the nearest
/// thousand feet; below, to the nearest 500 ft from 5,000 ft, 100 ft lower down.
pub fn format_cruise(ft: f64) -> String {
    if ft >= 18_000.0 {
        format!("FL{:03}", ((ft / 1000.0).round() * 10.0) as i64)
    } else {
        let step = if ft >= 5_000.0 { 500.0 } else { 100.0 };
        let rounded = ((ft / step).round() * step) as i64;
        format!("{} ft", thousands(rounded))
    }
}

fn thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One point a second through each `(seconds, altitude, vs, ias)` stretch.
    fn flight(stretches: &[(i64, f64, f64, f64)]) -> Vec<Pt> {
        let mut t = 0;
        let mut out = Vec::new();
        for &(secs, alt, vs, ias) in stretches {
            for i in 0..secs {
                out.push(Pt {
                    t,
                    lat: 0.0,
                    lon: 0.0,
                    alt: Some(alt + vs / 60.0 * i as f64),
                    gs: Some(ias),
                    ias: Some(ias),
                    vs: Some(vs),
                    roll: None,
                    pitch: None,
                    hdg: None,
                    trk: None,
                    wind_kt: None,
                    wind_from: None,
                });
                t += 1;
            }
        }
        out
    }

    #[test]
    fn the_cruise_is_the_level_held_longest_high_up() {
        let pts = flight(&[
            (600, 400.0, 0.0, 10.0),      // taxi, long, on the ground
            (300, 400.0, 1500.0, 140.0),   // climb to 7,900
            (1200, 34_980.0, 0.0, 250.0),  // cruise, as if after a fast climb
            (300, 24_000.0, 0.0, 250.0),   // a step held lower
            (900, 3_000.0, 0.0, 180.0),    // a long hold low down
        ]);
        assert_eq!(cruise_altitude(&pts), Some(35_000.0));
        assert_eq!(format_cruise(35_000.0), "FL350");
    }

    #[test]
    fn a_circuit_cruises_at_its_pattern_altitude() {
        let pts = flight(&[(120, 50.0, 700.0, 70.0), (240, 1_050.0, 0.0, 90.0), (120, 1_050.0, -500.0, 70.0)]);
        assert_eq!(cruise_altitude(&pts), Some(1_100.0));
        assert_eq!(format_cruise(1_049.0), "1,000 ft");
    }

    #[test]
    fn nothing_held_is_no_cruise() {
        let pts = flight(&[(600, 400.0, 0.0, 10.0), (300, 400.0, 800.0, 90.0), (300, 4_400.0, -800.0, 90.0)]);
        assert_eq!(cruise_altitude(&pts), None);
    }

    #[test]
    fn altitudes_round_as_pilots_say_them() {
        assert_eq!(format_cruise(18_400.0), "FL180");
        assert_eq!(format_cruise(17_800.0), "18,000 ft");
        assert_eq!(format_cruise(8_740.0), "8,500 ft");
        assert_eq!(format_cruise(4_460.0), "4,500 ft");
        assert_eq!(format_cruise(2_430.0), "2,400 ft");
    }
}
