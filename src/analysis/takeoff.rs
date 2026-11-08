//! What an instructor would say about each takeoff, from the stored track: the roll, the
//! rotation and the initial climb, each with its figures and findings, as `debrief.rs`
//! does for an approach.
//!
//! - Roll: from brake release (or, for a rolling takeoff, where the roll got going) to
//!   liftoff: how straight it was, how much runway it took and what was left. Straight is
//!   against the roll's own line, not the published centreline: public runway positions
//!   can be 100 ft from where the sim draws them, and the sim's own runways only come with
//!   a touchdown.
//! - Rotation: where the nose came up, at what speed and how fast it was pitched, and the
//!   attitude at liftoff.
//! - Initial climb, to 1,000 ft above the runway (1,500 ft in a transport) or the level-off
//!   if sooner: no sink after liftoff, the speed held, no turn and little bank below
//!   400 ft, and the climb rate.
//!
//! The track has no V-speeds, flaps or on-ground flag: the ground is the altitude on the
//! roll, and speeds are judged against the aircraft's own liftoff speed. Light aircraft and
//! transports are told apart by that speed ([`TRANSPORT_KT`]).

use super::debrief::{components, findings_json, max_bank, r0, r1, smooth, stats, stats_json, Finding, Severity};
use super::pattern::{bearing, event_time, runway_landed_on, Approach, Frame, Pt, FT_PER_NM};
use super::places::Runway;
use serde_json::{json, Value};

/// A liftoff at this speed or faster is a transport's.
const TRANSPORT_KT: f64 = 100.0;
/// The roll proper: from this ground speed.
const ROLL_KT: f64 = 30.0;
/// Standing still, lined up, under this.
const STOPPED_KT: f64 = 3.0;
/// Airborne: this high above the roll, and still there [`AIRBORNE_S`] later.
const AIRBORNE_FT: f64 = 15.0;
const AIRBORNE_S: i64 = 10;
/// The nose has come up once the pitch is this far above the roll's.
const ROTATE_DEG: f64 = 2.0;
/// The takeoff is looked for this far either side of its event.
const BEFORE_S: i64 = 180;
const AFTER_S: i64 = 120;
/// The initial climb is judged up to here at most.
const CLIMB_MAX_S: i64 = 300;
/// No turn, and little bank, below this.
const TURN_FT: f64 = 400.0;
/// The published runway agrees with the roll: its course within this, the roll this close
/// to its centreline on average.
const MATCH_DEG: f64 = 3.0;
const MATCH_FT: f64 = 40.0;
/// Levelled off: climbing slower than this for [`LEVEL_S`].
const LEVEL_FPM: f64 = 200.0;
const LEVEL_S: i64 = 10;

/// What is normal for the aircraft's class.
struct Norms {
    transport: bool,
    /// The initial climb ends this high above the runway.
    gate_ft: f64,
    /// Weaving more than this either side of the roll's line.
    weave_ft: f64,
    /// Pitched up faster than this in the rotation, degrees a second. A transport's is 2 to
    /// 3: faster, and the tail meets the runway. `None` for a light aircraft.
    pitch_rate: Option<f64>,
    /// A climb attitude this much steeper than the attitude on the roll is close to the
    /// stall.
    pitch_max: f64,
}

impl Norms {
    fn for_speed(liftoff_kt: f64) -> Self {
        if liftoff_kt >= TRANSPORT_KT {
            Self { transport: true, gate_ft: 1500.0, weave_ft: 15.0, pitch_rate: Some(4.0), pitch_max: 20.0 }
        } else {
            Self { transport: false, gate_ft: 1000.0, weave_ft: 20.0, pitch_rate: None, pitch_max: 20.0 }
        }
    }
}

/// Each point's distance from the straight line fitted through `xs` (along) and `ys`
/// (across), the same units.
fn off_line(xs: &[f64], ys: &[f64]) -> Vec<f64> {
    let n = xs.len() as f64;
    if xs.len() < 3 {
        return vec![0.0; xs.len()];
    }
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let sxx: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    let sxy: f64 = xs.iter().zip(ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let slope = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    xs.iter().zip(ys).map(|(x, y)| y - (my + slope * (x - mx))).collect()
}

/// When `value` first reaches `level` from `from` on, between the samples either side.
fn crossing(pts: &[Pt], from: usize, to: usize, level: f64, value: impl Fn(&Pt) -> Option<f64>) -> Option<f64> {
    let mut prev: Option<(i64, f64)> = None;
    for p in &pts[from..=to] {
        let Some(v) = value(p) else { continue };
        if v >= level {
            return Some(match prev {
                Some((t0, v0)) if v > v0 => t0 as f64 + (level - v0) / (v - v0) * (p.t - t0) as f64,
                _ => p.t as f64,
            });
        }
        prev = Some((p.t, v));
    }
    None
}

/// The debrief of each takeoff among `events` (the page's), along the track's `pts`.
/// `runways` are the departure's.
pub fn takeoffs(events: &[Value], pts: &[Pt], runways: &[Runway]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut last_liftoff = i64::MIN;
    for e in events {
        if e.get("eventType").and_then(Value::as_str) != Some("takeoff") {
            continue;
        }
        let Some(t) = event_time(e) else { continue };
        if let Some(d) = takeoff(pts, t, runways) {
            // Two events for one takeoff (a roll and a liftoff) are one debrief.
            let lifted = d["liftoffT"].as_i64().unwrap_or_default();
            if lifted != last_liftoff {
                last_liftoff = lifted;
                out.push(d);
            }
        }
    }
    for d in &mut out {
        if let Some(o) = d.as_object_mut() {
            o.remove("liftoffT");
        }
    }
    out
}

/// The takeoff whose event was at `t`.
fn takeoff(all: &[Pt], t: i64, runways: &[Runway]) -> Option<Value> {
    let a = all.partition_point(|p| p.t < t - BEFORE_S);
    let b = all.partition_point(|p| p.t <= t + AFTER_S + CLIMB_MAX_S);
    let pts = &all[a..b];
    let near = |i: usize| (pts[i].t - t).abs() <= BEFORE_S.max(AFTER_S);
    let gs = |i: usize| pts[i].gs.unwrap_or(0.0);

    // The roll: the last time the ground speed came up through ROLL_KT before the event's
    // window ends, so a taxi before it doesn't count.
    let fast = (1..pts.len())
        .filter(|&i| near(i) && pts[i].t <= t + AFTER_S && gs(i) >= ROLL_KT && gs(i - 1) < ROLL_KT)
        .last()?;
    // Brake release: standing within a minute before it; else a rolling takeoff, from
    // where it was last at taxi speed.
    let start = (0..fast)
        .rev()
        .take_while(|&i| pts[fast].t - pts[i].t <= 60)
        .find(|&i| gs(i) < STOPPED_KT)
        .or_else(|| (0..fast).rev().take_while(|&i| pts[fast].t - pts[i].t <= 60).find(|&i| gs(i) < 15.0))
        .unwrap_or(fast);
    let ground = stats(pts[start..=fast].iter().filter_map(|p| p.alt))?.mean;
    let h = |i: usize| pts[i].alt.map(|x| x - ground);
    let liftoff = (fast..pts.len()).find(|&i| {
        h(i).is_some_and(|x| x > AIRBORNE_FT)
            && pts[i..].iter().take_while(|p| p.t - pts[i].t <= AIRBORNE_S).all(|p| p.alt.is_some_and(|x| x - ground > AIRBORNE_FT))
    })?;
    // Back on the ground in the meantime: not a takeoff this track can judge.
    if pts[liftoff].t - pts[fast].t > 120 {
        return None;
    }

    // The runway: the one under the roll, its course the roll's.
    let course = bearing(&pts[start.min(fast)], &pts[liftoff]).or_else(|| bearing(&pts[fast], &pts[liftoff]))?;
    let mid = (fast + liftoff) / 2;
    let known = runway_landed_on(pts[mid].lat, pts[mid].lon, course, runways);
    let rwy = known.unwrap_or(Approach { course, lat: pts[start].lat, lon: pts[start].lon, length_ft: None, width_ft: None });
    let f = Frame::new(pts, &rwy);
    let along_ft = |i: usize| f.along[i] * FT_PER_NM;
    let centre_ft = |i: usize| f.left[i] * FT_PER_NM;

    let liftoff_kt = pts[liftoff].ias.or(pts[liftoff].gs)?;
    let norms = Norms::for_speed(liftoff_kt);
    let mut findings: Vec<Finding> = Vec::new();
    let mut add = |leg, code, severity, text: String, values: Value| findings.push(Finding { leg, code, severity, text, values });
    let mut legs = serde_json::Map::new();

    // The roll.
    let on_roll = fast..=liftoff;
    let (xs, ys): (Vec<f64>, Vec<f64>) = on_roll.clone().map(|i| (along_ft(i), centre_ft(i))).unzip();
    let weave = off_line(&xs, &ys).into_iter().map(f64::abs).reduce(f64::max);
    let centre = stats(on_roll.clone().map(|i| centre_ft(i).abs()));
    let wind = components(pts, start..=liftoff, rwy.course);
    let used_ft = along_ft(liftoff) - along_ft(start);
    // The published runway is only trusted where the roll followed it: public positions and
    // headings can differ from the sim's scenery (KAST's 32 is drawn 8 to 10° off).
    let matches = known.is_some()
        && super::pattern::angle(course - rwy.course).abs() <= MATCH_DEG
        && centre.is_some_and(|c| c.mean <= MATCH_FT);
    let left_ft = rwy.length_ft.filter(|_| matches).map(|l| l - along_ft(liftoff));
    legs.insert(
        "roll".into(),
        json!({
            "durationS": pts[liftoff].t - pts[start].t,
            "rolling": gs(start) >= STOPPED_KT,
            "usedFt": r0(used_ft),
            "runwayLengthFt": rwy.length_ft.filter(|_| matches).map(r0),
            "remainingFt": left_ft.map(r0),
            "weaveFt": weave.map(r0),
            // Against the published centreline, which may not be the sim's.
            "publishedCentrelineFt": centre.filter(|_| known.is_some()).map(|s| r0(s.mean)),
            "headwindKt": wind.map(|w| r0(w.0)),
            "crosswindKt": wind.map(|w| r0(w.1)),
        }),
    );
    if let Some(w) = weave {
        let cross = wind.map(|w| w.1.abs()).filter(|c| *c >= 5.0);
        let why = cross.map_or(String::new(), |c| format!(" With {:.0} kt of crosswind, hold aileron into the wind, more at the start and less as the speed builds, and keep straight with rudder.", c));
        if w > 2.0 * norms.weave_ft {
            add("roll", "straight", Severity::Caution, format!("Weaved up to {:.0} ft either side of your line on the roll.{} Look at the far end of the runway, not over the nose, and use small, early rudder.", w, why), json!({ "weaveFt": r0(w), "crosswindKt": cross.map(r0) }));
        } else if w > norms.weave_ft {
            add("roll", "straight", Severity::Improve, format!("Wandered up to {:.0} ft off a straight line on the roll.{} Look at the far end of the runway and keep straight with small rudder inputs.", w, why), json!({ "weaveFt": r0(w), "crosswindKt": cross.map(r0) }));
        } else if w <= norms.weave_ft / 2.0 {
            add("roll", "straight", Severity::Good, format!("A straight roll, within {:.0} ft of a line.", w), json!({ "weaveFt": r0(w) }));
        }
    }
    if let (Some(left), Some(len)) = (left_ft, rwy.length_ft) {
        if left < len * 0.2 {
            add("roll", "runway_left", Severity::Caution, format!("Lifted off with {:.0} ft of runway left, {:.0}% of it. Check the takeoff distance before departure, and know where you will abort if the speed isn't there.", left.max(0.0), (left / len * 100.0).max(0.0)), json!({ "remainingFt": r0(left), "runwayFt": r0(len) }));
        }
    }

    // The rotation: the nose coming up from the roll's attitude.
    let roll_pitch = stats(pts[fast..liftoff].iter().filter(|p| p.ias.is_some_and(|v| v < liftoff_kt - 10.0)).filter_map(|p| p.pitch));
    let rotation = roll_pitch.and_then(|base| {
        let base = base.mean;
        let look = (liftoff..pts.len()).take_while(|&i| pts[i].t - pts[liftoff].t <= 15).last().unwrap_or(liftoff);
        let rot = (fast..=look).find(|&i| pts[i].pitch.is_some_and(|p| p >= base + ROTATE_DEG))?;
        // The attitude it was rotated to: the most within 10 s of the rotation.
        let end = (rot..pts.len()).take_while(|&i| pts[i].t - pts[rot].t <= 10).last().unwrap_or(rot);
        let top = pts[rot..=end].iter().filter_map(|p| p.pitch).fold(f64::NEG_INFINITY, f64::max);
        let from = rot.saturating_sub(2).max(fast);
        let t0 = crossing(pts, from, end, base + ROTATE_DEG, |p| p.pitch)?;
        let target = base + 0.9 * (top - base);
        let t1 = crossing(pts, from, end, target, |p| p.pitch)?;
        let rate = (t1 > t0 && top - base > ROTATE_DEG + 1.0).then(|| (target - base - ROTATE_DEG) / (t1 - t0));
        Some((rot, base, top, rate))
    });
    let max_pitch = pts[liftoff..].iter().take_while(|p| p.t - pts[liftoff].t <= 60).filter_map(|p| p.pitch).reduce(f64::max);
    legs.insert(
        "rotation".into(),
        json!({
            "rotateKt": rotation.and_then(|(i, ..)| pts[i].ias).map(r0),
            "liftoffKt": r0(liftoff_kt),
            "rollPitchDeg": rotation.map(|r| r1(r.1)),
            "pitchRateDps": rotation.and_then(|r| r.3).map(r1),
            "liftoffPitchDeg": pts[liftoff].pitch.map(r1),
            "maxPitchDeg": max_pitch.map(r1),
        }),
    );
    if let (Some(rate), Some(limit)) = (rotation.and_then(|r| r.3), norms.pitch_rate) {
        if rate > limit {
            add("rotation", "pitch_rate", Severity::Improve, format!("Rotated at about {:.1}° a second. Aim for 2 to 3°: a fast rotation risks a tail strike, and the speed has no time to build.", rate), json!({ "rateDps": r1(rate) }));
        } else if rate >= 1.5 {
            add("rotation", "pitch_rate", Severity::Good, format!("A smooth rotation, at about {:.1}° a second.", rate), json!({ "rateDps": r1(rate) }));
        }
    }
    // Above the attitude on the roll, which is a taildragger's or a nose-high type's.
    let ground_pitch = roll_pitch.map_or(0.0, |s| s.mean.max(0.0));
    if let Some(p) = max_pitch.filter(|p| *p - ground_pitch > norms.pitch_max) {
        add("rotation", "pitch_high", Severity::Caution, format!("Pitched up to {:.0}° after liftoff. That close to the ground, so steep an attitude leaves little margin over the stall: fly the climb attitude and let the speed settle.", p), json!({ "pitchDeg": r0(p) }));
    }

    // The initial climb.
    let within = (liftoff..pts.len()).take_while(|&i| pts[i].t - pts[liftoff].t <= CLIMB_MAX_S).last().unwrap_or(liftoff);
    let gate = (liftoff..=within).find(|&i| h(i).is_some_and(|x| x >= norms.gate_ft));
    // Levelled off (or going down) before the gate, as on a circuit's crosswind leg: the
    // climb ends there.
    let climbing = |i: usize| smooth(pts, i, |p| p.vs).is_none_or(|v| v >= LEVEL_FPM);
    let level = (liftoff..=gate.unwrap_or(within)).filter(|&i| pts[i].t - pts[liftoff].t >= 20).find(|&i| {
        (i..=within).take_while(|&k| pts[k].t - pts[i].t <= LEVEL_S).all(|k| !climbing(k))
    });
    let end = level.or(gate).unwrap_or(within);
    let climb = liftoff..=end;
    // Below 400 ft: until the climb first passes it.
    let past_400 = climb.clone().find(|&i| h(i).is_some_and(|x| x >= TURN_FT)).unwrap_or(end);
    // Settling: losing height in the first 30 s, or below 400 ft.
    let early = climb.clone().filter(|&i| pts[i].t - pts[liftoff].t <= 30 || h(i).is_some_and(|x| x < TURN_FT));
    let min_vs = early.filter_map(|i| smooth(pts, i, |p| p.vs)).reduce(f64::min);
    let speed = stats(climb.clone().filter(|&i| h(i).is_some_and(|x| x >= 50.0)).filter_map(|i| smooth(pts, i, |p| p.ias)));
    let turn_low = (liftoff..=past_400).filter(|&i| pts[i].t - pts[liftoff].t >= 3).filter_map(|i| f.rel[i].map(f64::abs)).reduce(f64::max);
    let bank_low = (liftoff..=past_400).filter_map(|i| pts[i].roll.map(f64::abs)).reduce(f64::max);
    let bank = max_bank(pts, climb.clone());
    let rate = {
        let (dh, dt) = (h(end).zip(h(liftoff)).map(|(a, b)| a - b), (pts[end].t - pts[liftoff].t) as f64 / 60.0);
        dh.filter(|_| dt >= 0.25).map(|dh| dh / dt)
    };
    legs.insert(
        "climb".into(),
        json!({
            "toFt": norms.gate_ft,
            "reached": gate.is_some() && level.is_none(),
            "levelledOffFt": level.and_then(h).map(r0),
            "durationS": pts[end].t - pts[liftoff].t,
            "climbFpm": rate.map(r0),
            "minVsFpm": min_vs.map(r0),
            "iasKt": stats_json(speed),
            "maxTurnBelow400Deg": turn_low.map(r0),
            "maxBankBelow400Deg": bank_low.map(r0),
            "maxBankDeg": bank.map(r0),
        }),
    );
    match min_vs {
        Some(v) if v < -100.0 => {
            add("climb", "sink", Severity::Caution, format!("Sank after liftoff, at up to {:.0} fpm. Hold the climb attitude and keep a positive rate before anything else: gear and flaps only once the climb is established.", -v), json!({ "minVsFpm": r0(v) }));
        }
        Some(_) if gate.is_some() || level.is_some() => {
            add("climb", "sink", Severity::Good, "Climbed away without settling back.".into(), json!({ "minVsFpm": min_vs.map(r0) }));
        }
        _ => {}
    }
    // Slower than the rotation speed, near the speed it first flew at; without a rotation,
    // 10 kt under the liftoff speed.
    let floor_kt = rotation.and_then(|(i, ..)| pts[i].ias).map_or(liftoff_kt - 10.0, |v| v - 5.0);
    if let Some(s) = speed {
        if s.min < floor_kt {
            let severity = if s.min < floor_kt - 5.0 { Severity::Caution } else { Severity::Improve };
            add("climb", "slow", severity, format!("The speed fell to {:.0} kt in the climb, below the speed you rotated at. Lower the nose: too steep an attitude low down trades away the speed you need.", s.min), json!({ "minKt": r0(s.min), "floorKt": r0(floor_kt) }));
        } else if s.sd <= 4.0 && (gate.is_some() || level.is_some()) {
            add("climb", "speed", Severity::Good, format!("Held the climb speed steady, within {:.0} kt.", s.max - s.min), json!({ "rangeKt": r0(s.max - s.min) }));
        }
    }
    if let Some(turn) = turn_low.filter(|t| *t > 15.0) {
        add("climb", "turn_low", Severity::Improve, format!("Turned {:.0}° off the runway heading below {:.0} ft. Unless the departure calls for it, climb straight ahead to {:.0} ft first.", turn, TURN_FT, TURN_FT), json!({ "turnDeg": r0(turn) }));
    }
    if let Some(b) = bank_low.filter(|b| b.round() > 15.0) {
        add("climb", "bank_low", Severity::Improve, format!("Banked {:.0}° below {:.0} ft. Keep the wings nearly level until the climb is established.", b, TURN_FT), json!({ "bankDeg": r0(b) }));
    } else if let Some(b) = bank.filter(|b| b.round() > 30.0) {
        add("climb", "bank", Severity::Caution, format!("Banked {:.0}° in the initial climb. Keep turns at 30° or less while slow and low.", b), json!({ "bankDeg": r0(b) }));
    }

    Some(json!({
        "phase": "takeoff",
        "at": super::pattern::timestamp(pts[liftoff].t),
        "liftoffT": pts[liftoff].t,
        "runwayCourse": r0(rwy.course),
        "runwayKnown": known.is_some(),
        "runwayMatches": matches,
        "aircraftClass": if norms.transport { "transport" } else { "light" },
        "legs": legs,
        "findings": findings_json(findings),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: i64 = 1_700_000_000;

    /// A takeoff north from 45,0 at 0 ft: 10 s standing, a roll accelerating 3 kt a second,
    /// the nose up from `rotate_kt` at `rate`° a second to 8°, liftoff 3 s later, then a
    /// climb at 700 fpm and the liftoff speed plus 10 kt for 2 minutes. `tweak` changes
    /// each point by its second from brake release.
    fn takeoff_track(rotate_kt: f64, rate: f64, tweak: impl Fn(i64, &mut Pt)) -> Vec<Pt> {
        let mut pts = Vec::new();
        let (mut north, mut kt, mut alt, mut pitch) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        let mut rotated: Option<i64> = None;
        for s in -10..200 {
            if s >= 0 {
                match rotated {
                    None if kt >= rotate_kt => rotated = Some(s),
                    _ => {}
                }
                let since = rotated.map(|r| s - r);
                if since.is_none_or(|x| x < 6) {
                    kt += 3.0;
                }
                if since.is_some() {
                    pitch = (pitch + rate).min(8.0);
                }
                if since.is_some_and(|x| x >= 3) {
                    alt += 700.0 / 60.0;
                }
            }
            let mut p = Pt {
                t: START + s,
                lat: 45.0 + north / 60.0,
                lon: 0.0,
                alt: Some(alt),
                gs: Some(kt),
                ias: Some(kt),
                vs: Some(if alt > 0.0 { 700.0 } else { 0.0 }),
                roll: Some(0.0),
                pitch: Some(pitch),
                hdg: Some(0.0),
                trk: Some(0.0),
                wind_kt: Some(0.0),
                wind_from: Some(0.0),
            };
            tweak(s, &mut p);
            pts.push(p);
            north += kt / 3600.0;
        }
        pts
    }

    fn runway() -> Vec<Runway> {
        vec![Runway {
            airport_ident: "TEST".into(),
            le_ident: Some("36".into()),
            le_latitude_deg: Some(45.0),
            le_longitude_deg: Some(0.0),
            le_heading_degt: Some(0.0),
            le_displaced_threshold_ft: None,
            he_ident: Some("18".into()),
            he_latitude_deg: Some(45.0 + 12000.0 / FT_PER_NM / 60.0),
            he_longitude_deg: Some(0.0),
            he_heading_degt: Some(180.0),
            he_displaced_threshold_ft: None,
            length_ft: Some(12000),
            width_ft: Some(100),
        }]
    }

    fn run(pts: &[Pt], liftoff_after_s: i64) -> Value {
        let ev = json!({ "eventType": "takeoff", "timestamp": super::super::pattern::timestamp(START + liftoff_after_s) });
        let all = takeoffs(&[ev], pts, &runway());
        assert_eq!(all.len(), 1, "one takeoff");
        all.into_iter().next().unwrap()
    }

    fn codes(d: &Value, severity: &str) -> Vec<String> {
        d["findings"].as_array().unwrap().iter().filter(|f| f["severity"] == severity).map(|f| f["code"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn a_good_transport_takeoff_gets_good_findings_only() {
        let d = run(&takeoff_track(130.0, 2.5, |_, _| {}), 50);
        assert_eq!(d["aircraftClass"], "transport", "{d:#}");
        assert_eq!(d["runwayKnown"], true, "{d:#}");
        assert!(codes(&d, "caution").is_empty() && codes(&d, "improve").is_empty(), "{d:#}");
        for good in ["straight", "pitch_rate", "sink", "speed"] {
            assert!(codes(&d, "good").contains(&good.to_string()), "{good}: {d:#}");
        }
        let rot = &d["legs"]["rotation"];
        assert_eq!(rot["rotateKt"], json!(135.0), "{rot}");
        assert!((rot["pitchRateDps"].as_f64().unwrap() - 2.5).abs() <= 0.6, "{rot}");
        assert_eq!(d["legs"]["roll"]["rolling"], false);
        assert!(d.get("liftoffT").is_none());
    }

    #[test]
    fn a_sloppy_takeoff_gets_its_findings() {
        let pts = takeoff_track(130.0, 8.0, |s, p| {
            // Weaving 30 ft right and back on the roll.
            if (20..46).contains(&s) {
                p.lon += 30.0 * (std::f64::consts::PI * (s - 20) as f64 / 26.0).sin() / FT_PER_NM / 60.0 / 45f64.to_radians().cos();
            }
            // Settling after liftoff, then banking low.
            if (52..58).contains(&s) {
                p.vs = Some(-300.0);
            }
            if (60..70).contains(&s) {
                p.roll = Some(25.0);
            }
        });
        let d = run(&pts, 50);
        let (caution, improve) = (codes(&d, "caution"), codes(&d, "improve"));
        assert!(caution.contains(&"sink".to_string()), "{d:#}");
        for c in ["straight", "pitch_rate", "bank_low"] {
            assert!(improve.contains(&c.to_string()), "{c}: {d:#}");
        }
        assert_eq!(d["findings"][0]["severity"], "caution", "cautions first");
    }

    #[test]
    fn a_light_aircraft_has_no_pitch_rate_norm() {
        let d = run(&takeoff_track(55.0, 4.0, |_, _| {}), 25);
        assert_eq!(d["aircraftClass"], "light", "{d:#}");
        assert!(!codes(&d, "improve").contains(&"pitch_rate".to_string()), "{d:#}");
        assert_eq!(d["legs"]["climb"]["toFt"], json!(1000.0));
    }
}
