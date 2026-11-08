//! The legs of the circuit flown into each landing, from the stored track: final, and
//! before it base, and before that downwind, the last two only when they were flown. A
//! straight-in approach has a final only, one joined on base a base and a final.
//!
//! Each landing's touchdown is placed on the runway landed on, whose course and threshold
//! frame the approach; with no runway known, the touchdown and the course flown into it do.
//! Going back from the touchdown, each leg is the longest stretch whose ground track keeps
//! to the leg's course relative to the runway, with breaks under [`BREAK_S`] kept in it
//! (a correction, an S-turn), and the turn between two legs under [`TURN_S`]:
//!
//! - final: within [`FINAL_DEG`] of the runway course, near the extended centreline;
//! - base: within [`BASE_DEG`] of square to it, flying towards the centreline;
//! - downwind: within [`DOWNWIND_DEG`] of the reciprocal, beside the runway on the side the
//!   base came from, roughly level, no further out than [`DOWNWIND_MAX_S`] at its speed.
//!
//! Both turns go the same way, so a base and downwind make a left or a right circuit. The
//! search stops at the stretch before the approach: a go-around, a touch-and-go or a low
//! pass there, which belongs to another landing.

use super::flight::project_onto_runway;
use super::places::Runway;
use serde_json::{json, Value};

/// Ground track within this of the runway course is on final.
const FINAL_DEG: f64 = 20.0;
/// On final, no further from the extended centreline than this, plus [`FINAL_SPLAY`] of the
/// distance out.
const FINAL_OFF_NM: f64 = 0.3;
const FINAL_SPLAY: f64 = 0.05;
const BASE_DEG: f64 = 35.0;
const DOWNWIND_DEG: f64 = 30.0;
/// A leg is at least this long; a final may be shorter.
const LEG_MIN_S: i64 = 15;
/// Off a leg's course for less than this keeps the leg going.
const BREAK_S: i64 = 10;
/// The longest turn between two legs.
const TURN_S: i64 = 60;
/// How far beside the runway a downwind may be: this long at its ground speed, and no
/// closer than [`DOWNWIND_MIN_NM`].
const DOWNWIND_MAX_S: f64 = 150.0;
const DOWNWIND_MIN_NM: f64 = 0.25;
/// A downwind that climbs or descends faster than this on average is something else.
const DOWNWIND_LEVEL_FPM: f64 = 500.0;
/// How far back from the touchdown the approach is looked for.
const LOOKBACK_S: i64 = 20 * 60;
/// Lower than this above the touchdown, further back than [`LOW_AFTER_S`], is the stretch
/// before the approach: the previous touchdown or a low pass.
const LOW_FT: f64 = 100.0;
const LOW_AFTER_S: i64 = 60;
/// Ground track is taken over this many seconds either side of a point.
const TRACK_HALF_S: i64 = 2;
pub(crate) const FT_PER_NM: f64 = 6076.12;

/// One stored track point.
#[derive(Debug, Clone, Copy, Default)]
pub struct Pt {
    /// Unix seconds.
    pub t: i64,
    pub lat: f64,
    pub lon: f64,
    /// Indicated, feet.
    pub alt: Option<f64>,
    pub gs: Option<f64>,
    pub ias: Option<f64>,
    /// Feet a minute.
    pub vs: Option<f64>,
    /// Bank, degrees, in the sim's own sign: only its size is read.
    pub roll: Option<f64>,
    /// Heading and ground track as the sim reported them, in the same reference, so their
    /// difference is the crab whatever that reference is.
    pub hdg: Option<f64>,
    pub trk: Option<f64>,
    pub wind_kt: Option<f64>,
    /// Where the wind blows from, true.
    pub wind_from: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegKind {
    Downwind,
    Base,
    Final,
}

impl LegKind {
    fn name(self) -> &'static str {
        match self {
            Self::Downwind => "downwind",
            Self::Base => "base",
            Self::Final => "final",
        }
    }
}

/// Which way the circuit turned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// A leg, by its first and last point's index.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Leg {
    pub kind: LegKind,
    pub start: usize,
    pub end: usize,
    pub side: Option<Side>,
}

/// The runway's frame: its course, true, and its threshold.
#[derive(Debug, Clone, Copy)]
pub struct Approach {
    pub course: f64,
    pub lat: f64,
    pub lon: f64,
    /// The runway's, when it is known.
    pub length_ft: Option<f64>,
    pub width_ft: Option<f64>,
}

/// The runway end landed on: the end whose centreline the touchdown is nearest, within
/// 300 ft, its heading within 30° of the touchdown's.
pub fn runway_landed_on(lat: f64, lon: f64, heading: f64, runways: &[Runway]) -> Option<Approach> {
    runways
        .iter()
        .flat_map(|rw| {
            [
                (rw.le_latitude_deg, rw.le_longitude_deg, rw.le_heading_degt, rw.length_ft, rw.width_ft),
                (rw.he_latitude_deg, rw.he_longitude_deg, rw.he_heading_degt, rw.length_ft, rw.width_ft),
            ]
        })
        .filter_map(|(lat0, lon0, hdg, len, width)| {
            let (lat0, lon0, hdg) = (lat0?, lon0?, hdg?);
            let (lateral, along) = project_onto_runway(lat, lon, lat0, lon0, hdg, 0.0);
            let length_ft = len.filter(|l| *l > 0).map(f64::from);
            let fits = lateral.abs() < 300.0 && along > -1000.0 && length_ft.is_none_or(|l| along < l + 1000.0);
            let width_ft = width.filter(|w| *w > 0).map(f64::from);
            let approach = Approach { course: hdg, lat: lat0, lon: lon0, length_ft, width_ft };
            (fits && angle(heading - hdg).abs() <= 30.0).then_some((lateral.abs(), approach))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, a)| a)
}

/// `d` wrapped into (-180, 180].
pub(crate) fn angle(d: f64) -> f64 {
    let d = d.rem_euclid(360.0);
    if d > 180.0 { d - 360.0 } else { d }
}

fn bearing(a: &Pt, b: &Pt) -> Option<f64> {
    let (la1, la2) = (a.lat.to_radians(), b.lat.to_radians());
    let dl = (b.lon - a.lon).to_radians();
    let y = dl.sin() * la2.cos();
    let x = la1.cos() * la2.sin() - la1.sin() * la2.cos() * dl.cos();
    // Under ~20 m apart the bearing is noise.
    let moved = super::places::haversine_km(a.lat, a.lon, b.lat, b.lon) > 0.02;
    moved.then(|| y.atan2(x).to_degrees().rem_euclid(360.0))
}

/// Each point's ground track, over [`TRACK_HALF_S`] either side.
fn tracks(pts: &[Pt]) -> Vec<Option<f64>> {
    (0..pts.len())
        .map(|i| {
            let t = pts[i].t;
            let a = pts[..i].iter().rposition(|p| p.t <= t - TRACK_HALF_S).unwrap_or(0);
            let b = pts[i..].iter().position(|p| p.t >= t + TRACK_HALF_S).map_or(pts.len() - 1, |j| i + j);
            if a == b { None } else { bearing(&pts[a], &pts[b]) }
        })
        .collect()
}

/// Per point, in the runway's frame: along the course past the threshold and left of it,
/// in NM, and the ground track relative to the course.
pub(crate) struct Frame {
    pub(crate) along: Vec<f64>,
    pub(crate) left: Vec<f64>,
    pub(crate) rel: Vec<Option<f64>>,
}

impl Frame {
    pub(crate) fn new(pts: &[Pt], rwy: &Approach) -> Self {
        let (mut along, mut left) = (Vec::with_capacity(pts.len()), Vec::with_capacity(pts.len()));
        for p in pts {
            let (l, a) = project_onto_runway(p.lat, p.lon, rwy.lat, rwy.lon, rwy.course, 0.0);
            along.push(a / FT_PER_NM);
            left.push(l / FT_PER_NM);
        }
        let rel = tracks(pts).into_iter().map(|t| t.map(|t| angle(t - rwy.course))).collect();
        Self { along, left, rel }
    }

    fn final_(&self, i: usize) -> bool {
        let out = (-self.along[i]).max(0.0);
        self.rel[i].is_some_and(|r| r.abs() <= FINAL_DEG)
            && self.left[i].abs() <= FINAL_OFF_NM + FINAL_SPLAY * out
            && self.along[i] < 0.5
    }

    /// A left circuit's base flies right of the runway course, towards the centreline from
    /// its left; a right circuit's the mirror.
    fn base(&self, i: usize, side: Side) -> bool {
        let (course, beside) = match side {
            Side::Left => (90.0, self.left[i] > 0.0),
            Side::Right => (-90.0, self.left[i] < 0.0),
        };
        beside && self.rel[i].is_some_and(|r| angle(r - course).abs() <= BASE_DEG)
    }

    fn downwind(&self, i: usize, side: Side) -> bool {
        let beside = match side {
            Side::Left => self.left[i] > 0.0,
            Side::Right => self.left[i] < 0.0,
        };
        beside && self.rel[i].is_some_and(|r| angle(r - 180.0).abs() <= DOWNWIND_DEG)
    }
}

/// The stretch ending at or before `from` that keeps to `on`: its last point no more than
/// `within` seconds before `from`'s, and breaks shorter than [`BREAK_S`] kept. `floor` is
/// the earliest index looked at.
fn run_back(pts: &[Pt], from: usize, floor: usize, within: i64, on: impl Fn(usize) -> bool) -> Option<(usize, usize)> {
    let t0 = pts[from].t;
    let end = (floor..=from).rev().take_while(|&i| t0 - pts[i].t <= within).find(|&i| on(i))?;
    let mut start = end;
    for i in (floor..end).rev() {
        if on(i) {
            start = i;
        } else if pts[start].t - pts[i].t >= BREAK_S {
            break;
        }
    }
    Some((start, end))
}

fn span_s(pts: &[Pt], (a, b): (usize, usize)) -> i64 {
    pts[b].t - pts[a].t
}

/// The legs flown into the touchdown at `touchdown`, in order: downwind, base, final, the
/// first two only when flown.
pub fn legs(pts: &[Pt], touchdown: usize, rwy: &Approach) -> Vec<Leg> {
    if touchdown >= pts.len() || touchdown == 0 {
        return Vec::new();
    }
    let t_down = pts[touchdown].t;
    // The approach starts after the stretch before it, if it is in the track.
    let ground = pts[touchdown].alt;
    let floor = (0..touchdown)
        .rev()
        .take_while(|&i| t_down - pts[i].t <= LOOKBACK_S)
        .find(|&i| {
            t_down - pts[i].t > LOW_AFTER_S
                && matches!((pts[i].alt, ground), (Some(a), Some(g)) if a < g + LOW_FT)
        })
        .map_or_else(
            || (0..touchdown).rev().take_while(|&i| t_down - pts[i].t <= LOOKBACK_S).last().unwrap_or(touchdown),
            |i| i + 1,
        );
    let f = Frame::new(&pts[..=touchdown], rwy);

    let Some(fin) = run_back(pts, touchdown, floor, 30, |i| f.final_(i)) else {
        return Vec::new();
    };
    let mut out = vec![Leg { kind: LegKind::Final, start: fin.0, end: fin.1, side: None }];
    if fin.0 <= floor {
        return out;
    }

    // Base: which side is the first point square to the runway before the final on.
    let side_at = |i: usize| [Side::Left, Side::Right].into_iter().find(|&s| f.base(i, s));
    let Some(side) = (floor..fin.0).rev().take_while(|&i| pts[fin.0].t - pts[i].t <= TURN_S).find_map(side_at) else {
        return out;
    };
    let Some(base) = run_back(pts, fin.0 - 1, floor, TURN_S, |i| f.base(i, side)).filter(|b| span_s(pts, *b) >= LEG_MIN_S)
    else {
        return out;
    };
    let speed = |(a, b): (usize, usize)| {
        let gs: Vec<f64> = pts[a..=b].iter().filter_map(|p| p.gs).collect();
        if gs.is_empty() { 120.0 } else { gs.iter().sum::<f64>() / gs.len() as f64 }
    };
    let max_out = |leg| (speed(leg) * DOWNWIND_MAX_S / 3600.0).max(1.0);
    if f.left[base.0].abs() > max_out(base) * 1.5 {
        return out;
    }
    out.insert(0, Leg { kind: LegKind::Base, start: base.0, end: base.1, side: Some(side) });
    if base.0 <= floor {
        return out;
    }

    let Some(down) = run_back(pts, base.0 - 1, floor, TURN_S, |i| f.downwind(i, side))
        .filter(|d| span_s(pts, *d) >= LEG_MIN_S)
    else {
        return out;
    };
    let mut offsets: Vec<f64> = (down.0..=down.1).map(|i| f.left[i].abs()).collect();
    offsets.sort_by(f64::total_cmp);
    let offset = offsets[offsets.len() / 2];
    let level = match (pts[down.0].alt, pts[down.1].alt) {
        (Some(a), Some(b)) => ((b - a) / (span_s(pts, down) as f64 / 60.0)).abs() <= DOWNWIND_LEVEL_FPM,
        _ => true,
    };
    if level && offset >= DOWNWIND_MIN_NM && offset <= max_out(down) {
        out.insert(0, Leg { kind: LegKind::Downwind, start: down.0, end: down.1, side: Some(side) });
    }
    out
}

/// What a final adds: its length from the threshold and its height above the touchdown.
fn final_figures(pts: &[Pt], leg: &Leg, touchdown: usize, rwy: &Approach) -> (f64, Option<f64>) {
    let p = &pts[leg.start];
    let (_, along) = project_onto_runway(p.lat, p.lon, rwy.lat, rwy.lon, rwy.course, 0.0);
    let height = match (p.alt, pts[touchdown].alt) {
        (Some(a), Some(g)) => Some(a - g),
        _ => None,
    };
    ((-along / FT_PER_NM).max(0.0), height)
}

fn timestamp(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y-%m-%d %H:%M:%S%.3f").to_string()).unwrap_or_default()
}

/// What the circuits into a flight's landings add to its page.
#[derive(Debug, Default)]
pub struct Circuits {
    /// The legs, as events of their own: `downwind`, `base` and `final`, at each leg's
    /// start.
    pub events: Vec<Value>,
    /// Each landing's debrief (`debrief.rs`).
    pub debriefs: Vec<Value>,
}

/// The circuit flown into each landing among `events` (the page's). `runways` are the
/// arrival's. The track is the page's columnar one.
pub fn circuits(events: &[Value], track: &Value, runways: &[Runway]) -> Circuits {
    let col = |k: &str| track.get(k).and_then(Value::as_array);
    let Some(ts) = col("timestamps") else { return Circuits::default() };
    let num = |k: &str, i: usize| col(k).and_then(|c| c.get(i)).and_then(Value::as_f64);
    // The first timestamp is absolute and the rest are deltas, as the renderer reads them.
    let mut at = 0i64;
    let times: Vec<i64> = ts
        .iter()
        .map(|v| {
            at += v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).unwrap_or(0);
            at
        })
        .collect();
    // Older tracks are in milliseconds.
    let ms = times.first().is_some_and(|&t| t > 100_000_000_000);
    let pts: Vec<Pt> = (0..ts.len())
        .filter_map(|i| {
            Some(Pt {
                t: if ms { times[i] / 1000 } else { times[i] },
                lat: num("latitudes", i)?,
                lon: num("longitudes", i)?,
                alt: num("altitudes", i),
                gs: num("ground_speed", i),
                ias: num("ias", i),
                vs: num("vspeed", i),
                roll: num("roll", i),
                hdg: num("heading", i),
                trk: num("track", i),
                wind_kt: num("wind_speed", i),
                wind_from: num("wind_dir", i),
            })
        })
        .collect();
    if pts.len() < 2 {
        return Circuits::default();
    }
    let mut out = Circuits::default();
    for e in events {
        if e.get("eventType").and_then(Value::as_str) != Some("landing") {
            continue;
        }
        let Some(t) = e
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| chrono::NaiveDateTime::parse_from_str(s.split('.').next().unwrap_or(s), "%Y-%m-%d %H:%M:%S").ok())
            .map(|d| d.and_utc().timestamp())
        else {
            continue;
        };
        let touchdown = pts.partition_point(|p| p.t < t).min(pts.len() - 1);
        if (pts[touchdown].t - t).abs() > 30 || touchdown < 2 {
            continue;
        }
        let at = &pts[touchdown];
        let (lat, lon) = (
            e.get("latitude").and_then(Value::as_f64).unwrap_or(at.lat),
            e.get("longitude").and_then(Value::as_f64).unwrap_or(at.lon),
        );
        // The course flown into the touchdown, over the 10 s before it.
        let before = pts[..touchdown].iter().rposition(|p| p.t <= at.t - 10).unwrap_or(0);
        let flown = bearing(&pts[before], at);
        let heading = e.get("heading").and_then(Value::as_f64).or(flown);
        let rwy = heading
            .and_then(|h| runway_landed_on(lat, lon, h, runways))
            .or_else(|| flown.map(|course| Approach { course, lat, lon, length_ft: None, width_ft: None }));
        let Some(rwy) = rwy else { continue };
        let legs = legs(&pts, touchdown, &rwy);
        if let Some(debrief) = super::debrief::debrief(&pts, touchdown, &rwy, &legs, e) {
            out.debriefs.push(debrief);
        }
        for leg in legs {
            let p = &pts[leg.start];
            let mut ev = json!({
                "eventType": leg.kind.name(),
                "timestamp": timestamp(p.t),
                "latitude": p.lat,
                "longitude": p.lon,
                "heading": Value::Null,
            });
            if let Some(side) = leg.side {
                ev["circuit"] = json!(if side == Side::Left { "left" } else { "right" });
            }
            if leg.kind == LegKind::Final {
                let (nm, height) = final_figures(&pts, &leg, touchdown, &rwy);
                ev["finalNm"] = json!((nm * 10.0).round() / 10.0);
                ev["heightFt"] = json!(height.map(f64::round));
            }
            out.events.push(ev);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A track flown from `start` (NM east and north of the threshold at 45,0, runway course
    /// 360) along `legs` of (course, seconds), at `kt`, a point a second, level at 1000 ft
    /// then descending on the last leg to the field at 0 ft.
    fn fly(start: (f64, f64), legs: &[(f64, i64)], kt: f64) -> Vec<Pt> {
        let (mut east, mut north) = start;
        let mut pts = Vec::new();
        let total: i64 = legs.iter().map(|l| l.1).sum();
        let last = legs.last().unwrap().1;
        let mut t = 0;
        for &(course, secs) in legs {
            for _ in 0..secs {
                let alt = if t >= total - last { 1000.0 * (total - t) as f64 / last as f64 } else { 1000.0 };
                let descending = t >= total - last;
                pts.push(Pt {
                    t: 1_700_000_000 + t,
                    lat: 45.0 + north / 60.0,
                    lon: east / 60.0 / 45f64.to_radians().cos(),
                    alt: Some(alt),
                    gs: Some(kt),
                    ias: Some(kt),
                    vs: Some(if descending { -1000.0 * 60.0 / last as f64 } else { 0.0 }),
                    roll: Some(0.0),
                    hdg: Some(course),
                    trk: Some(course),
                    ..Default::default()
                });
                let step = kt / 3600.0;
                east += step * course.to_radians().sin();
                north += step * course.to_radians().cos();
                t += 1;
            }
        }
        pts
    }

    const RWY: Approach = Approach { course: 0.0, lat: 45.0, lon: 0.0, length_ft: None, width_ft: None };

    fn kinds(legs: &[Leg]) -> Vec<LegKind> {
        legs.iter().map(|l| l.kind).collect()
    }

    #[test]
    fn a_left_circuit_has_its_three_legs() {
        use LegKind::*;
        // Downwind south 1 NM west of the runway, base east, final north to the threshold.
        // 90 kt: 1 NM in 40 s.
        let pts = fly((-1.0, 1.0), &[(180.0, 80), (90.0, 40), (0.0, 40)], 90.0);
        let legs = legs(&pts, pts.len() - 1, &RWY);
        assert_eq!(kinds(&legs), [Downwind, Base, Final]);
        assert!(legs.iter().take(2).all(|l| l.side == Some(Side::Left)));
        // Each leg starts within a few seconds of where it was flown.
        let starts: Vec<i64> = legs.iter().map(|l| pts[l.start].t - pts[0].t).collect();
        assert!(starts[0] <= 3 && (78..=84).contains(&starts[1]) && (118..=124).contains(&starts[2]), "{starts:?}");
    }

    #[test]
    fn a_right_circuit_mirrors_it() {
        use LegKind::*;
        let pts = fly((1.0, 1.0), &[(180.0, 80), (270.0, 40), (0.0, 40)], 90.0);
        let legs = legs(&pts, pts.len() - 1, &RWY);
        assert_eq!(kinds(&legs), [Downwind, Base, Final]);
        assert!(legs.iter().take(2).all(|l| l.side == Some(Side::Right)));
    }

    #[test]
    fn a_straight_in_is_a_final_only_and_a_base_entry_has_no_downwind() {
        use LegKind::*;
        let straight = fly((0.0, -4.0), &[(0.0, 160)], 90.0);
        assert_eq!(kinds(&legs(&straight, straight.len() - 1, &RWY)), [Final]);
        // Joined on base from the east, after flying in from the north-east.
        let base_entry = fly((2.0, 0.0), &[(225.0, 40), (270.0, 50), (0.0, 40)], 90.0);
        assert_eq!(kinds(&legs(&base_entry, base_entry.len() - 1, &RWY)), [Base, Final]);
    }

    #[test]
    fn an_s_turn_on_final_keeps_the_final() {
        let pts = fly((0.0, -3.0), &[(0.0, 40), (30.0, 4), (330.0, 4), (0.0, 60)], 90.0);
        let legs = legs(&pts, pts.len() - 1, &RWY);
        assert_eq!(kinds(&legs), [LegKind::Final]);
        assert_eq!(legs[0].start, 0);
    }

    #[test]
    fn a_leg_flown_the_wrong_way_or_too_far_out_is_none() {
        use LegKind::*;
        // Flown north beside the runway, the way it lands, before the base: no downwind.
        let wrong = fly((-1.0, -2.0), &[(0.0, 40), (90.0, 40), (0.0, 40)], 90.0);
        assert_eq!(kinds(&legs(&wrong, wrong.len() - 1, &RWY)), [Base, Final]);
        // 10 NM out at 90 kt is no downwind (2.5 min is 3.75 NM), nor such a base.
        let far = fly((-10.0, 1.0), &[(180.0, 80), (90.0, 400), (0.0, 40)], 90.0);
        assert_eq!(kinds(&legs(&far, far.len() - 1, &RWY)), [Final]);
    }

    #[test]
    fn the_approach_before_a_touch_and_go_is_not_this_landings() {
        use LegKind::*;
        // A first circuit down to the field, then a straight-in to land: the second landing
        // has a final only.
        let mut pts = fly((-1.0, 1.0), &[(180.0, 80), (90.0, 40), (0.0, 40)], 90.0);
        let mut again = fly((0.0, -3.0), &[(0.0, 120)], 90.0);
        let t0 = pts.last().unwrap().t + 120;
        for (i, p) in again.iter_mut().enumerate() {
            p.t = t0 + i as i64;
        }
        pts.extend(again);
        assert_eq!(kinds(&legs(&pts, pts.len() - 1, &RWY)), [Final]);
    }

    #[test]
    fn the_runway_landed_on_is_the_end_flown_into() {
        let rw = Runway {
            airport_ident: "TEST".into(),
            length_ft: Some(6000),
            width_ft: Some(150),
            le_ident: Some("36".into()),
            le_latitude_deg: Some(45.0),
            le_longitude_deg: Some(0.0),
            le_heading_degt: Some(0.0),
            le_displaced_threshold_ft: None,
            he_ident: Some("18".into()),
            he_latitude_deg: Some(45.0 + 6000.0 / FT_PER_NM / 60.0),
            he_longitude_deg: Some(0.0),
            he_heading_degt: Some(180.0),
            he_displaced_threshold_ft: None,
        };
        let a = runway_landed_on(45.003, 0.0, 2.0, std::slice::from_ref(&rw)).unwrap();
        assert_eq!((a.course, a.lat), (0.0, 45.0));
        let b = runway_landed_on(45.003, 0.0, 178.0, std::slice::from_ref(&rw)).unwrap();
        assert_eq!(b.course, 180.0);
        assert!(runway_landed_on(45.003, 0.0, 90.0, &[rw]).is_none());
    }

    #[test]
    fn circuits_read_the_page_track() {
        let pts = fly((-1.0, 1.0), &[(180.0, 80), (90.0, 40), (0.0, 40)], 90.0);
        let track = json!({
            "timestamps": pts.iter().enumerate().map(|(i, p)| if i == 0 { p.t } else { p.t - pts[i - 1].t }).collect::<Vec<_>>(),
            "latitudes": pts.iter().map(|p| p.lat).collect::<Vec<_>>(),
            "longitudes": pts.iter().map(|p| p.lon).collect::<Vec<_>>(),
            "altitudes": pts.iter().map(|p| p.alt.unwrap()).collect::<Vec<_>>(),
            "ground_speed": pts.iter().map(|p| p.gs.unwrap()).collect::<Vec<_>>(),
        });
        let last = pts.last().unwrap();
        let landing = json!({ "eventType": "landing", "timestamp": timestamp(last.t), "latitude": last.lat, "longitude": last.lon });
        let evs = circuits(&[landing], &track, &[]).events;
        let types: Vec<&str> = evs.iter().map(|e| e["eventType"].as_str().unwrap()).collect();
        assert_eq!(types, ["downwind", "base", "final"]);
        assert_eq!(evs[0]["circuit"], json!("left"));
        let nm = evs[2]["finalNm"].as_f64().unwrap();
        assert!((0.8..=1.2).contains(&nm), "{nm}");
        assert!(evs[2]["heightFt"].as_f64().unwrap() > 900.0);
    }
}
