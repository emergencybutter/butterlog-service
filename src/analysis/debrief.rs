//! What an instructor would say about a landing's circuit (`pattern.rs`): each leg's
//! figures, and findings, what went well and what to work on, each with the numbers
//! behind it. The text is plain and fixed; the numbers are what a later rewording (an LLM)
//! would work from, so every finding carries them.
//!
//! The track has no flaps, gear or slip ball, and no Vref: speeds are judged against the
//! aircraft's own final approach speed, the mean between 300 and 100 ft above the field,
//! and what is normal differs between a light aircraft and a transport, told apart by that
//! speed ([`Norms`]).
//!
//! - Downwind: circuit height held, spacing from the runway kept, how far it was extended.
//! - The turns onto base and final: the bank, and how far final was overshot.
//! - Final: from what height it was stabilised (on speed, sink under 1,000 fpm, wings
//!   nearly level, on the centreline and on a 3° path), where it was against a 3° path at
//!   1,000, 500 and 200 ft, its path angle, speed control, sink, late bank and centreline.
//! - Touchdown: the crab left in it, how far down the runway, how firm, how far off the
//!   centreline.

use super::pattern::{angle, Approach, Frame, Leg, LegKind, Pt, Side, FT_PER_NM};
use serde_json::{json, Value};

/// The glidepath judged against, and its aim point past the threshold.
const PATH_DEG: f64 = 3.0;
const AIM_FT: f64 = 1000.0;
/// A light aircraft's final approach speed is under this; a transport's, or a turboprop
/// airliner's (an ATR's is about 100 kt), over.
const TRANSPORT_KT: f64 = 90.0;
/// Stabilisation is judged down to this height: lower, a glidepath against a fixed aim
/// point says more about the aim point than the flying.
const STABLE_FLOOR_FT: f64 = 200.0;
/// The crab is only read when the final's agrees with the crosswind within this: heading
/// and track can come from the sim in different references (true and magnetic), which
/// shows as a crab the wind doesn't explain.
const CRAB_AGREE_DEG: f64 = 3.0;
/// Sink rate and speed are averaged over this many seconds either side.
const SMOOTH_S: i64 = 2;

/// What is normal for the aircraft's class.
struct Norms {
    transport: bool,
    /// Stabilised by this height above the field: 300 ft in a light aircraft's circuit,
    /// which rolls out on final at 400 to 500 ft; a transport's is the IMC gate, which
    /// airlines use for both.
    gate_ft: f64,
    /// Circuit height held within this on downwind.
    dw_height_ft: f64,
    /// Downwind spacing changing more than this from start to end is not parallel. `None`
    /// for a transport, whose downwind is ATC's vectors.
    dw_drift_nm: Option<f64>,
    /// Base turned this far past the threshold or more is an extended downwind; `None` for
    /// a transport, as above.
    dw_extended_nm: Option<f64>,
    /// Final overshot by more than this.
    overshoot_ft: f64,
    /// Off the centreline by more than this below 300 ft.
    centreline_ft: f64,
    /// Touchdown firmer than this.
    firm_fpm: f64,
    /// Touchdown further than this past the threshold, or a third of the runway.
    long_ft: f64,
}

impl Norms {
    fn for_speed(final_kt: f64) -> Self {
        if final_kt >= TRANSPORT_KT {
            Self {
                transport: true,
                gate_ft: 1000.0,
                dw_height_ft: 200.0,
                dw_drift_nm: None,
                dw_extended_nm: None,
                overshoot_ft: 600.0,
                centreline_ft: 75.0,
                firm_fpm: 400.0,
                long_ft: 3000.0,
            }
        } else {
            Self {
                transport: false,
                gate_ft: 300.0,
                dw_height_ft: 100.0,
                dw_drift_nm: Some(0.3),
                dw_extended_nm: Some(1.5),
                overshoot_ft: 300.0,
                centreline_ft: 50.0,
                firm_fpm: 400.0,
                long_ft: 1500.0,
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Stats {
    mean: f64,
    sd: f64,
    min: f64,
    max: f64,
}

fn stats(values: impl Iterator<Item = f64>) -> Option<Stats> {
    let v: Vec<f64> = values.filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
    let min = v.iter().copied().fold(f64::INFINITY, f64::min);
    let max = v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Some(Stats { mean, sd, min, max })
}

fn r0(x: f64) -> f64 {
    x.round()
}

fn r1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn stats_json(s: Option<Stats>) -> Value {
    s.map_or(Value::Null, |s| json!({ "mean": r1(s.mean), "sd": r1(s.sd), "min": r1(s.min), "max": r1(s.max) }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Caution,
    Improve,
    Good,
}

impl Severity {
    fn name(self) -> &'static str {
        match self {
            Self::Caution => "caution",
            Self::Improve => "improve",
            Self::Good => "good",
        }
    }
}

struct Finding {
    leg: &'static str,
    code: &'static str,
    severity: Severity,
    text: String,
    values: Value,
}

/// The wind's components on a course: headwind and crosswind from the right, knots.
fn components(pts: &[Pt], range: std::ops::RangeInclusive<usize>, course: f64) -> Option<(f64, f64)> {
    let (mut head, mut cross, mut n) = (0.0, 0.0, 0.0);
    for p in &pts[range] {
        if let (Some(kt), Some(from)) = (p.wind_kt, p.wind_from) {
            let rel = (from - course).to_radians();
            head += kt * rel.cos();
            cross += kt * rel.sin();
            n += 1.0;
        }
    }
    (n > 0.0).then(|| (head / n, cross / n))
}

/// The mean of `value` over [`SMOOTH_S`] either side of point `i`: a gust or one noisy
/// sample is not a sink rate or a speed an instructor would call.
fn smooth(pts: &[Pt], i: usize, value: impl Fn(&Pt) -> Option<f64>) -> Option<f64> {
    let t = pts[i].t;
    let from = pts[..i].iter().rposition(|p| p.t < t - SMOOTH_S).map_or(0, |j| j + 1);
    let to = pts[i..].iter().position(|p| p.t > t + SMOOTH_S).map_or(pts.len(), |j| i + j);
    stats(pts[from..to].iter().filter_map(value)).map(|s| s.mean)
}

fn max_bank(pts: &[Pt], range: std::ops::RangeInclusive<usize>) -> Option<f64> {
    pts[range].iter().filter_map(|p| p.roll.map(f64::abs)).reduce(f64::max)
}

/// The debrief of the landing at `touchdown`, flown into `rwy` along `legs`. `None` without
/// a final or altitudes to judge it by.
pub fn debrief(pts: &[Pt], touchdown: usize, rwy: &Approach, legs: &[Leg], landing: &Value) -> Option<Value> {
    let fin = *legs.iter().find(|l| l.kind == LegKind::Final)?;
    let base = legs.iter().find(|l| l.kind == LegKind::Base).copied();
    let down = legs.iter().find(|l| l.kind == LegKind::Downwind).copied();
    let field = pts[touchdown].alt?;
    let f = Frame::new(&pts[..=touchdown], rwy);
    let h = |i: usize| pts[i].alt.map(|a| a - field);
    let in_band = |i: usize, lo: f64, hi: f64| h(i).is_some_and(|x| x >= lo && x <= hi);
    let on_final = fin.start..=touchdown;
    let dist_ft = |i: usize| -f.along[i] * FT_PER_NM;
    let aim_ft = |i: usize| (dist_ft(i) + AIM_FT).max(0.0);
    let path_ft = |i: usize| aim_ft(i) * PATH_DEG.to_radians().tan();
    let glide_dev = |i: usize| h(i).map(|x| x - path_ft(i));
    let centre_ft = |i: usize| f.left[i] * FT_PER_NM;

    // The aircraft's own final approach speed.
    let ias_in = |lo: f64, hi: f64| stats(on_final.clone().filter(|&i| in_band(i, lo, hi)).filter_map(|i| pts[i].ias));
    let final_kt = ias_in(100.0, 300.0).or_else(|| ias_in(30.0, 500.0))?.mean;
    let norms = Norms::for_speed(final_kt);
    let mut findings: Vec<Finding> = Vec::new();
    let mut add = |leg, code, severity, text: String, values: Value| findings.push(Finding { leg, code, severity, text, values });

    // Downwind.
    let mut out_legs = serde_json::Map::new();
    if let Some(d) = down {
        let range = d.start..=d.end;
        let heights = stats(range.clone().filter_map(h));
        let dev = heights.map(|s| (s.max - s.mean).max(s.mean - s.min));
        let edge = |from: usize, to: usize| stats((from..=to).map(|i| f.left[i].abs())).map(|s| s.mean);
        let first = (d.start..=d.end).take_while(|&i| pts[i].t - pts[d.start].t <= 10).last().unwrap_or(d.start);
        let last = (d.start..=d.end).rev().take_while(|&i| pts[d.end].t - pts[i].t <= 10).last().unwrap_or(d.end);
        let (spacing_start, spacing_end) = (edge(d.start, first), edge(last, d.end));
        let drift = spacing_start.zip(spacing_end).map(|(a, b)| (b - a).abs());
        let beyond_nm = (-f.along[d.end]).max(0.0);
        let track_dev = stats(range.clone().filter_map(|i| f.rel[i].map(|r| angle(r - 180.0).abs()))).map(|s| s.mean);
        out_legs.insert(
            "downwind".into(),
            json!({
                "durationS": pts[d.end].t - pts[d.start].t,
                "heightFt": stats_json(heights),
                "heightDevFt": dev.map(r0),
                "spacingNm": { "start": spacing_start.map(r1), "end": spacing_end.map(r1) },
                "beyondThresholdNm": r1(beyond_nm),
                "trackDevDeg": track_dev.map(r1),
                "iasKt": stats_json(stats(range.clone().filter_map(|i| pts[i].ias))),
                "maxBankDeg": max_bank(pts, range.clone()).map(r0),
            }),
        );
        if let (Some(dev), Some(hs)) = (dev, heights) {
            let values = json!({ "devFt": r0(dev), "heightFt": r0(hs.mean) });
            if dev > norms.dw_height_ft {
                let sev = if dev > 2.0 * norms.dw_height_ft { Severity::Caution } else { Severity::Improve };
                add("downwind", "dw_height", sev, format!("Circuit height wandered by up to {:.0} ft on downwind, flown at about {:.0} ft above the field. Trim for level flight and keep the altimeter in your scan.", dev, hs.mean), values);
            } else if dev <= norms.dw_height_ft / 2.0 {
                add("downwind", "dw_height", Severity::Good, format!("Held circuit height within {:.0} ft on downwind.", dev), values);
            }
        }
        if let (Some(drift), Some(limit), Some(a), Some(b)) = (drift, norms.dw_drift_nm, spacing_start, spacing_end) {
            if drift > limit {
                add("downwind", "dw_parallel", Severity::Improve, format!("The downwind wasn't parallel to the runway: spacing went from {:.1} to {:.1} NM. Hold a heading that corrects for the wind and check the runway's position against a fixed point on the wing.", a, b), json!({ "startNm": r1(a), "endNm": r1(b) }));
            }
        }
        if norms.dw_extended_nm.is_some_and(|limit| beyond_nm > limit) {
            add("downwind", "dw_extended", Severity::Improve, format!("The downwind was extended: base came {:.1} NM past the threshold. Unless traffic or ATC called for it, turn base sooner so the final stays short and the runway within gliding reach.", beyond_nm), json!({ "beyondNm": r1(beyond_nm) }));
        }
    }

    // The turn onto base, and base.
    if let Some(b) = base {
        let range = b.start..=b.end;
        let turn_bank = down.and_then(|d| max_bank(pts, d.end..=(b.start + 3).min(b.end)));
        let base_course = rwy.course + if b.side == Some(Side::Right) { -90.0 } else { 90.0 };
        let wind = components(pts, range.clone(), base_course);
        out_legs.insert(
            "base".into(),
            json!({
                "durationS": pts[b.end].t - pts[b.start].t,
                "heightFt": { "start": h(b.start).map(r0), "end": h(b.end).map(r0) },
                "outNm": r1((-f.along[b.start]).max(0.0)),
                "iasKt": stats_json(stats(range.clone().filter_map(|i| pts[i].ias))),
                "vsFpm": stats_json(stats(range.clone().filter_map(|i| pts[i].vs))),
                "maxBankDeg": max_bank(pts, range.clone()).map(r0),
                "turnInBankDeg": turn_bank.map(r0),
                "tailwindKt": wind.map(|(head, _)| r0(-head)),
            }),
        );
        if let Some(bank) = turn_bank.filter(|b| b.round() > 30.0) {
            add("base", "turn_base_bank", Severity::Caution, format!("Banked {:.0}° turning base. Keep circuit turns at 30° or less.", bank), json!({ "bankDeg": r0(bank) }));
        }

        // The turn onto final.
        let end = (fin.start..=touchdown).take_while(|&i| pts[i].t - pts[fin.start].t <= 60).last().unwrap_or(fin.start);
        let bank = max_bank(pts, b.end..=(fin.start + 5).min(touchdown));
        let beyond = |i: usize| if b.side == Some(Side::Right) { centre_ft(i) } else { -centre_ft(i) };
        let overshoot = (b.end..=end).map(beyond).fold(0.0, f64::max);
        out_legs.insert("turnToFinal".into(), json!({ "maxBankDeg": bank.map(r0), "overshootFt": r0(overshoot), "rollOutFt": h(fin.start).map(r0) }));
        if let Some(bank) = bank.filter(|b| b.round() > 30.0) {
            add("final", "turn_final_bank", Severity::Caution, format!("Banked {:.0}° turning final. Keep it to 30° or less: a steep, slow turn close to the ground is where stall-spin accidents happen. If the turn won't make the centreline at 30°, go around.", bank), json!({ "bankDeg": r0(bank) }));
        }
        if overshoot > norms.overshoot_ft {
            let tail = wind.map(|(head, _)| -head).filter(|t| *t >= 5.0);
            let why = tail.map_or(String::new(), |t| format!(" There was about {:.0} kt of tailwind on base, which carries you through the centreline: start the turn earlier.", t));
            add("final", "overshoot", Severity::Improve, format!("Overshot the centreline by {:.0} ft turning final.{}", overshoot, why), json!({ "overshootFt": r0(overshoot), "baseTailwindKt": tail.map(r0) }));
        } else if overshoot <= norms.overshoot_ft / 3.0 {
            add("final", "overshoot", Severity::Good, "Rolled out of the turn onto final on the centreline.".into(), json!({ "overshootFt": r0(overshoot) }));
        }
    }

    // Final: the gates.
    let gate = |g: f64| {
        let starts_above = h(fin.start).is_some_and(|x| x > g);
        on_final.clone().find(|&i| starts_above && h(i).is_some_and(|x| x <= g))
    };
    let gate_json = |i: Option<usize>| {
        i.map_or(Value::Null, |i| {
            json!({
                "glideDevFt": glide_dev(i).map(r0),
                "centrelineFt": r0(centre_ft(i)),
                "iasKt": pts[i].ias.map(r0),
                "speedDevKt": pts[i].ias.map(|v| r0(v - final_kt)),
                "vsFpm": pts[i].vs.map(r0),
                "bankDeg": pts[i].roll.map(|r| r0(r.abs())),
            })
        })
    };
    let (g1000, g500, g200) = (gate(1000.0), gate(500.0), gate(200.0));
    let path_angle = {
        let a = g1000.unwrap_or(fin.start);
        let b = g200.or_else(|| on_final.clone().rev().find(|&i| h(i).is_some_and(|x| x >= 100.0)));
        b.and_then(|b| {
            let run = dist_ft(a) - dist_ft(b);
            let drop = h(a)? - h(b)?;
            (run > 500.0).then(|| drop.atan2(run).to_degrees())
        })
    };
    let low = |lo: f64, hi: f64| on_final.clone().filter(move |&i| in_band(i, lo, hi));
    let speed_low = stats(low(30.0, 500.0).filter_map(|i| smooth(pts, i, |p| p.ias)));
    let sink_max = low(50.0, 1000.0).filter_map(|i| smooth(pts, i, |p| p.vs)).reduce(f64::min);
    let bank_low = low(30.0, 300.0).filter_map(|i| pts[i].roll.map(f64::abs)).reduce(f64::max);
    let centre_low = low(30.0, 300.0).map(|i| centre_ft(i).abs()).reduce(f64::max);
    let crab = |range: std::ops::RangeInclusive<usize>| {
        stats(pts[range].iter().filter(|p| p.gs.unwrap_or(0.0) > 40.0).filter_map(|p| Some(angle(p.hdg? - p.trk?)))).map(|s| s.mean)
    };
    let wind = components(pts, on_final.clone(), rwy.course);
    // The crab the crosswind calls for on final, against what heading and track say.
    let final_crab = crab(on_final.clone());
    let crab_reliable = match (final_crab, wind) {
        (Some(measured), Some((_, cross))) => {
            let expected = (cross / final_kt).clamp(-1.0, 1.0).asin().to_degrees();
            (measured - expected).abs() <= CRAB_AGREE_DEG
        }
        _ => false,
    };

    // From what height every criterion held, down to STABLE_FLOOR_FT.
    let failures = |i: usize| {
        let mut why = Vec::new();
        if smooth(pts, i, |p| p.ias).is_some_and(|v| v < final_kt - 5.0 || v > final_kt + 10.0) {
            why.push("speed");
        }
        if smooth(pts, i, |p| p.vs).is_some_and(|v| v < -1000.0) {
            why.push("sink rate");
        }
        if pts[i].roll.is_some_and(|r| r.abs() > 15.0) {
            why.push("bank");
        }
        if centre_ft(i).abs() > (dist_ft(i).max(0.0) * 1f64.to_radians().tan()).max(100.0) {
            why.push("centreline");
        }
        if glide_dev(i).is_some_and(|d| d.abs() > (aim_ft(i) * 0.7f64.to_radians().tan()).max(50.0)) {
            why.push("glidepath");
        }
        why
    };
    let bottom = on_final.clone().rev().find(|&i| h(i).is_some_and(|x| x >= STABLE_FLOOR_FT));
    let (stable_from, broke_on) = match bottom {
        Some(bottom) if failures(bottom).is_empty() => {
            let mut from = bottom;
            let mut broke = Vec::new();
            for i in (fin.start..bottom).rev() {
                let why = failures(i);
                if why.is_empty() {
                    from = i;
                } else if pts[from].t - pts[i].t > 3 {
                    broke = why;
                    break;
                }
            }
            (h(from), broke)
        }
        Some(bottom) => (None, failures(bottom)),
        None => (None, Vec::new()),
    };

    out_legs.insert(
        "final".into(),
        json!({
            "lengthNm": r1((-f.along[fin.start]).max(0.0)),
            "startHeightFt": h(fin.start).map(r0),
            "finalSpeedKt": r0(final_kt),
            "stabilisedFromFt": stable_from.map(r0),
            "gates": { "1000": gate_json(g1000), "500": gate_json(g500), "200": gate_json(g200) },
            "pathAngleDeg": path_angle.map(r1),
            "iasBelow500Kt": stats_json(speed_low),
            "maxSinkFpm": sink_max.map(r0),
            "maxBankBelow300Deg": bank_low.map(r0),
            "maxCentrelineBelow300Ft": centre_low.map(r0),
            "crabDeg": final_crab.filter(|_| crab_reliable).map(r1),
            "headwindKt": wind.map(|w| r0(w.0)),
            "crosswindKt": wind.map(|w| r0(w.1)),
        }),
    );

    match stable_from {
        Some(from) if from >= norms.gate_ft => {
            add("final", "stabilised", Severity::Good, format!("Stabilised from {:.0} ft: on speed, on the centreline and on the glidepath, at a steady sink.", from), json!({ "fromFt": r0(from), "gateFt": norms.gate_ft }));
        }
        _ if bottom.is_some() => {
            let what = if broke_on.is_empty() { String::new() } else { format!(" What was off: {}.", broke_on.join(", ")) };
            let from = stable_from.map_or(format!("not even at {STABLE_FLOOR_FT:.0} ft"), |x| format!("only from {:.0} ft", x));
            add("final", "stabilised", Severity::Caution, format!("Not stabilised by {:.0} ft ({}).{} The rule is simple: not stable at the gate, go around.", norms.gate_ft, from, what), json!({ "fromFt": stable_from.map(r0), "gateFt": norms.gate_ft, "off": broke_on }));
        }
        _ => {}
    }
    if let Some(dev) = g500.and_then(glide_dev) {
        if dev > 150.0 {
            add("final", "high", Severity::Improve, format!("High at 500 ft: {:.0} ft above a 3° path. Get down to the path earlier, with power and drag, rather than diving at the runway late.", dev), json!({ "devFt": r0(dev) }));
        } else if dev < -75.0 {
            add("final", "low", Severity::Caution, format!("Low at 500 ft: {:.0} ft below a 3° path. Dragging in low leaves nothing in hand if the engine falters; add power and hold the path.", -dev), json!({ "devFt": r0(dev) }));
        }
    }
    if let Some(a) = path_angle {
        if a > 4.5 {
            add("final", "steep", Severity::Improve, format!("A steep final, about {:.1}° against 3°. It makes the flare harder to judge and the speed harder to hold.", a), json!({ "pathDeg": r1(a) }));
        } else if a < 2.0 {
            add("final", "shallow", Severity::Improve, format!("A shallow, dragged-in final, about {:.1}° against 3°.", a), json!({ "pathDeg": r1(a) }));
        }
    }
    if let Some(sink) = sink_max.filter(|s| *s < -1000.0) {
        let severity = if sink < -1200.0 { Severity::Caution } else { Severity::Improve };
        add("final", "sink", severity, format!("Sink rate reached {:.0} fpm below 1,000 ft. Over 1,000 fpm that low is unstable: correct the path early, with small changes.", -sink), json!({ "sinkFpm": r0(sink) }));
    }
    if let Some(s) = speed_low {
        let (fast, slow) = (s.max - final_kt, final_kt - s.min);
        if slow > 5.0 {
            add("final", "slow", Severity::Caution, format!("{:.0} kt below your final approach speed of {:.0} kt below 500 ft. Slow and low is the stall; keep the speed up until the flare.", slow, final_kt), json!({ "slowKt": r0(slow), "finalKt": r0(final_kt) }));
        }
        if fast > 10.0 {
            add("final", "fast", Severity::Improve, format!("Up to {:.0} kt over your final approach speed below 500 ft. Extra speed floats you down the runway; set the attitude and power early and trim.", fast), json!({ "fastKt": r0(fast), "finalKt": r0(final_kt) }));
        } else if slow <= 5.0 && s.sd <= 3.0 {
            add("final", "speed", Severity::Good, format!("Speed held within {:.0} kt below 500 ft.", s.max - s.min), json!({ "rangeKt": r0(s.max - s.min) }));
        }
    }
    if let Some(bank) = bank_low.filter(|b| b.round() > 15.0) {
        add("final", "late_bank", Severity::Improve, format!("Banked up to {:.0}° below 300 ft. Line up earlier so the last of the final needs only small corrections.", bank), json!({ "bankDeg": r0(bank) }));
    }
    // Never more than half the runway's width: past that is off the runway.
    let centreline_ft = rwy.width_ft.map_or(norms.centreline_ft, |w| norms.centreline_ft.min(w / 2.0));
    if let Some(off) = centre_low.filter(|c| *c > centreline_ft) {
        let cross = wind.map(|w| w.1.abs()).filter(|c| *c >= 5.0);
        let why = cross.map_or(String::new(), |c| format!(" With {:.0} kt of crosswind, hold the drift with a crab or a wing-low sideslip.", c));
        add("final", "centreline", Severity::Improve, format!("Drifted up to {:.0} ft off the centreline below 300 ft.{}", off, why), json!({ "offFt": r0(off), "crosswindKt": cross.map(r0) }));
    }

    // Touchdown.
    let num = |k: &str| landing.get(k).and_then(Value::as_f64);
    let fpm = num("touchdownFpm").map(f64::abs);
    let past_ft = num("thresholdDistFt");
    let offset = num("offsetPercent");
    let td_from = pts[..touchdown].iter().rposition(|p| p.t <= pts[touchdown].t - 3).unwrap_or(touchdown);
    let td_crab = crab(td_from..=touchdown).filter(|_| crab_reliable);
    out_legs.insert(
        "touchdown".into(),
        json!({
            "fpm": fpm.map(r0),
            "pastThresholdFt": past_ft.map(r0),
            "runwayLengthFt": rwy.length_ft.map(r0),
            "offsetPercent": offset.map(r1),
            "crabDeg": td_crab.map(r1),
        }),
    );
    if let Some(c) = td_crab.filter(|c| c.abs() > 5.0) {
        add("touchdown", "crab", Severity::Improve, format!("Touched down {:.0}° crabbed. In the flare, kick the crab out with rudder (or hold a wing-low sideslip) so the wheels meet the runway pointing down it.", c.abs()), json!({ "crabDeg": r1(c) }));
    }
    if let Some(past) = past_ft {
        let limit = rwy.length_ft.map_or(norms.long_ft, |l| (l / 3.0).min(norms.long_ft));
        if past > limit {
            let of = rwy.length_ft.map_or(String::new(), |l| format!(", {:.0}% of the way down the runway", past / l * 100.0));
            add("touchdown", "long", Severity::Improve, format!("Touched down {:.0} ft past the threshold{}. Aim for the touchdown zone: cross the threshold on speed and at about 50 ft, and don't hold off with excess speed.", past, of), json!({ "pastFt": r0(past), "runwayFt": rwy.length_ft.map(r0) }));
        }
    }
    if let Some(v) = fpm {
        if v > 600.0 {
            add("touchdown", "hard", Severity::Caution, format!("A hard touchdown at {:.0} fpm. Consider an inspection; and next time, a stable approach and a progressive flare.", v), json!({ "fpm": r0(v) }));
        } else if v > norms.firm_fpm {
            add("touchdown", "firm", Severity::Improve, format!("A firm touchdown at {:.0} fpm. Start the flare a little higher and hold it as the speed bleeds off.", v), json!({ "fpm": r0(v) }));
        } else if v < 200.0 {
            add("touchdown", "soft", Severity::Good, format!("A soft touchdown at {:.0} fpm.", v), json!({ "fpm": r0(v) }));
        }
    }
    if let Some(o) = offset.filter(|o| o.abs() > 50.0) {
        add("touchdown", "offset", Severity::Improve, format!("Touched down {:.0}% of the way from the centreline to the runway edge.", o.abs()), json!({ "offsetPercent": r1(o) }));
    }

    findings.sort_by_key(|f| f.severity);
    let kind = match (down, base) {
        (Some(_), _) => "circuit",
        (None, Some(_)) => "base entry",
        _ => "straight-in",
    };
    let side = base.and_then(|b| b.side).map(|s| if s == Side::Left { "left" } else { "right" });
    Some(json!({
        "landing": landing.get("timestamp").cloned().unwrap_or(Value::Null),
        "approach": kind,
        "circuit": side,
        "runwayCourse": r0(rwy.course),
        "aircraftClass": if norms.transport { "transport" } else { "light" },
        "legs": out_legs,
        "findings": findings.into_iter().map(|f| json!({
            "leg": f.leg,
            "code": f.code,
            "severity": f.severity.name(),
            "text": f.text,
            "values": f.values,
        })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::pattern::legs;

    const RWY: Approach = Approach { course: 0.0, lat: 45.0, lon: 0.0, length_ft: Some(5000.0), width_ft: Some(100.0) };

    /// A left circuit at 80 kt, NM east and north of the threshold: downwind south 1 NM
    /// west at 1,000 ft, base east descending to a 1.2 NM final, final north on a 3° path to
    /// touch down 1,000 ft past the threshold. `tweak` changes each point by its leg and
    /// second.
    fn circuit(tweak: impl Fn(&str, i64, &mut Pt)) -> Vec<Pt> {
        let kt = 80.0;
        let step = kt / 3600.0;
        let mut pts = Vec::new();
        let (mut east, mut north) = (-1.0, 1.5);
        let mut t = 0;
        let path = |north: f64| ((-north) * FT_PER_NM + AIM_FT) * PATH_DEG.to_radians().tan();
        let joins = path(-1.2);
        let (dw_secs, base_secs) = ((2.7 / step) as i64, (1.0 / step) as i64);
        let fin_secs = ((1.2 + AIM_FT / FT_PER_NM) / step) as i64;
        let legs: [(&str, f64, i64); 3] = [("downwind", 180.0, dw_secs), ("base", 90.0, base_secs), ("final", 0.0, fin_secs)];
        for (name, course, secs) in legs {
            for s in 0..secs {
                let (alt, vs) = match name {
                    "final" => (path(north), -kt * 101.27 * PATH_DEG.to_radians().tan()),
                    "base" => (1000.0 - (1000.0 - joins) * s as f64 / secs as f64, -(1000.0 - joins) / (secs as f64 / 60.0)),
                    _ => (1000.0, 0.0),
                };
                let mut p = Pt {
                    t: 1_700_000_000 + t,
                    lat: 45.0 + north / 60.0,
                    lon: east / 60.0 / 45f64.to_radians().cos(),
                    alt: Some(alt),
                    gs: Some(kt),
                    ias: Some(kt),
                    vs: Some(vs),
                    roll: Some(0.0),
                    hdg: Some(course),
                    trk: Some(course),
                    wind_kt: Some(0.0),
                    wind_from: Some(0.0),
                };
                tweak(name, s, &mut p);
                pts.push(p);
                east += step * course.to_radians().sin();
                north += step * course.to_radians().cos();
                t += 1;
            }
        }
        pts
    }

    fn run(pts: &[Pt], landing: Value) -> Value {
        let touchdown = pts.len() - 1;
        let legs = legs(pts, touchdown, &RWY);
        debrief(pts, touchdown, &RWY, &legs, &landing).expect("a debrief")
    }

    fn codes(d: &Value, severity: &str) -> Vec<String> {
        d["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["severity"] == severity)
            .map(|f| f["code"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_good_circuit_gets_good_findings_only() {
        let d = run(&circuit(|_, _, _| {}), json!({ "touchdownFpm": -120.0, "thresholdDistFt": 1000.0, "offsetPercent": 5.0 }));
        assert_eq!(d["approach"], "circuit");
        assert_eq!(d["circuit"], "left");
        assert_eq!(d["aircraftClass"], "light");
        assert!(codes(&d, "caution").is_empty() && codes(&d, "improve").is_empty(), "{d:#}");
        for good in ["dw_height", "overshoot", "stabilised", "speed", "soft"] {
            assert!(codes(&d, "good").contains(&good.to_string()), "{good}: {d:#}");
        }
        let fin = &d["legs"]["final"];
        assert!((fin["pathAngleDeg"].as_f64().unwrap() - 3.0).abs() < 0.3, "{fin}");
        assert!(fin["gates"]["200"]["glideDevFt"].as_f64().unwrap().abs() < 30.0, "{fin}");
    }

    #[test]
    fn a_sloppy_circuit_gets_its_findings() {
        let pts = circuit(|leg, s, p| match leg {
            // Climbing 450 ft over the downwind.
            "downwind" => p.alt = p.alt.map(|a| a + 450.0 * s as f64 / 121.0),
            // Steep in the turn onto final.
            "base" if s >= 40 => p.roll = Some(40.0),
            // Fast and sinking hard from about 400 down to 250 ft.
            "final" if (5..25).contains(&s) => {
                p.ias = Some(95.0);
                p.vs = Some(-1300.0);
            }
            _ => {}
        });
        let d = run(&pts, json!({ "touchdownFpm": -480.0, "thresholdDistFt": 2600.0, "offsetPercent": 10.0 }));
        let (caution, improve) = (codes(&d, "caution"), codes(&d, "improve"));
        for c in ["dw_height", "turn_final_bank", "sink", "stabilised"] {
            assert!(caution.contains(&c.to_string()), "{c}: {d:#}");
        }
        for c in ["fast", "long", "firm"] {
            assert!(improve.contains(&c.to_string()), "{c}: {d:#}");
        }
        assert_eq!(d["findings"][0]["severity"], "caution", "cautions first");
        let stab = d["findings"].as_array().unwrap().iter().find(|f| f["code"] == "stabilised").unwrap();
        assert!(stab["values"]["off"].as_array().unwrap().iter().any(|o| o == "speed" || o == "sink rate"), "{stab}");
    }

    #[test]
    fn a_crabbed_touchdown_is_called() {
        let pts = circuit(|leg, s, p| {
            if leg == "final" && s >= 55 {
                p.hdg = Some(352.0);
            }
        });
        let d = run(&pts, json!({ "touchdownFpm": -150.0, "thresholdDistFt": 1100.0 }));
        assert!(codes(&d, "improve").contains(&"crab".to_string()), "{d:#}");
        assert_eq!(d["legs"]["touchdown"]["crabDeg"], json!(-8.0));
    }

    /// Heading magnetic against a true track, 8° of variation all the way down: no crab
    /// the wind explains, so none is read.
    #[test]
    fn a_heading_in_another_reference_is_no_crab() {
        let pts = circuit(|_, _, p| p.hdg = p.hdg.map(|h| h + 8.0));
        let d = run(&pts, json!({ "touchdownFpm": -150.0, "thresholdDistFt": 1100.0 }));
        assert!(!codes(&d, "improve").contains(&"crab".to_string()), "{d:#}");
        assert_eq!(d["legs"]["touchdown"]["crabDeg"], Value::Null);
    }
}
