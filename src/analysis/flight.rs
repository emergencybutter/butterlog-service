//! Flight analysis on canonical samples, ported from the app's `flight_analyzer.rs`
//! (design §5.2): phases, takeoff and landing, top of climb and descent, endpoints and the
//! landing scorecard.
//!
//! The rules are the app's. What changes with the input:
//! - Samples arrive at up to 30 Hz and only when something changed, where the app logged
//!   about one a second. Windows the app counted in samples are in seconds here, endpoint
//!   positions are taken at most once a second, and the approach statistics weight each
//!   sample by how long it held.
//! - Samples paused, in replay or in slew are skipped: a replay replays the takeoff and
//!   teleports the aircraft. The top of climb and descent timers start over after them.
//! - Ground contact must hold for [`GROUND_HOLD_MS`] to count, from its first sample:
//!   MSFS flickers it at liftoff, which at frame rate reads as touch-and-goes.
//! - A session is split into flights as the app splits its logs: at a teleport, such as
//!   a flight being loaded, and from the first time the aircraft moves.
//! - Touchdown vertical speed is the last airborne sample's, now a frame before the
//!   wheels touch rather than up to a second.

use super::places::{Places, Runway};
use crate::normalise::{Field, Sample};
use butterlog_wire::flags;
use std::collections::{HashMap, VecDeque};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Parked,
    TaxiOut,
    Takeoff,
    Climb,
    Cruise,
    Descent,
    Approach,
    Landing,
    TaxiIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Takeoff,
    Landing,
    TopOfClimb,
    TopOfDescent,
    Parked,
    AutopilotOn,
    AutopilotOff,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlightEvent {
    pub kind: EventKind,
    pub wall_time_ms: i64,
    pub latitude: f64,
    pub longitude: f64,
    /// True heading, for landings and parking.
    pub heading: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Landing {
    pub wall_time_ms: i64,
    pub latitude: f64,
    pub longitude: f64,
    pub heading: f64,
    /// Feet per minute, negative descending.
    pub touchdown_fpm: f64,
    /// Peak G in the second after touchdown, averaged over [`G_AVERAGE_MS`]: a frame's G
    /// spikes as the gear takes the load. The app took the G of its first sample on the
    /// ground, up to a second after the wheels touched.
    pub g: f64,
    /// The runway end landed on, e.g. "26".
    pub runway: Option<String>,
    /// Lateral offset from the centreline, percent of half the width, right positive.
    pub offset_percent: Option<f64>,
    /// Touchdown distance past the threshold, feet.
    pub threshold_dist_ft: Option<f64>,
    /// Over the minute before touchdown.
    pub vs_variance: Option<f64>,
    pub ias_variance: Option<f64>,
    /// 0-100, higher steadier: G, roll and speed from 60 to 5 s before touchdown.
    pub approach_stability: Option<f64>,
}

impl Landing {
    /// The app's landing score: 0 for a perfect landing, minus points for centreline
    /// offset, distance from the 300 ft mark, G above 1.2 and sink rate above 300 fpm.
    pub fn score(&self) -> f64 {
        let mut score = 0.0;
        if let Some(offset) = self.offset_percent {
            score -= offset.abs();
        }
        if let Some(dist) = self.threshold_dist_ft {
            score -= (dist - 300.0).abs() / 10.0;
        }
        if self.g > 1.2 {
            score -= (self.g - 1.2) * 50.0;
        }
        let rate = self.touchdown_fpm.abs();
        if rate > 300.0 {
            score -= (rate.min(500.0) - 300.0) * 0.05;
            if rate > 500.0 {
                score -= (rate - 500.0) * 0.2;
            }
        }
        score
    }
}

/// A flight's departure or arrival.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Airport(String),
    /// Started in the air, or not yet landed.
    Airborne,
    /// No takeoff yet, so no arrival.
    Unknown,
    /// No airport anywhere near.
    NotFound,
}

impl fmt::Display for Endpoint {
    /// As the app's summaries spell them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Airport(ident) => f.write_str(ident),
            Self::Airborne => f.write_str("Airborne"),
            Self::Unknown => f.write_str("Unknown"),
            Self::NotFound => f.write_str("XXXX"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlightSummary {
    pub departure: Endpoint,
    pub arrival: Endpoint,
    pub first_ms: i64,
    pub last_ms: i64,
    /// When the wheels first left the ground; a touch-and-go doesn't move it.
    pub takeoff_ms: Option<i64>,
    pub max_altitude_ft: f64,
    pub max_ground_speed_kt: f64,
    pub fuel_consumed_gal: f64,
    pub events: Vec<FlightEvent>,
    /// The first landing.
    pub landing: Option<Landing>,
}

impl FlightSummary {
    pub fn duration_minutes(&self) -> i64 {
        (self.last_ms - self.first_ms) / 60_000
    }
}

/// How long the app's 300-sample endpoint windows lasted at its ~1 Hz.
const ENDPOINT_WINDOW_MS: i64 = 300_000;
/// Endpoint positions are taken at the app's rate, so a fast stretch doesn't outvote a
/// slow one.
const ENDPOINT_SPACING_MS: i64 = 1_000;
/// How long a climb or descent must level off or start before it counts.
const STABLE_MS: i64 = 10_000;
/// How long after touchdown the landing's peak G is taken over.
const TOUCHDOWN_G_MS: i64 = 1_000;
const G_AVERAGE_MS: i64 = 200;
/// How long ground contact, or its absence, must last to count.
pub const GROUND_HOLD_MS: i64 = 500;
/// A flight starts when the aircraft first moves faster than this, as the app's logs do.
const MOVING_KT: f64 = 10.0;
/// A position change beyond what the ground speed allows, times this, plus
/// [`TELEPORT_MARGIN_NM`], is a teleport (the app's X-Plane rule).
const TELEPORT_SPEED_FACTOR: f64 = 8.0;
const TELEPORT_MARGIN_NM: f64 = 3.0;
const SKIPPED: u8 = flags::PAUSED | flags::REPLAY | flags::SLEW;

/// What the analysis reads from one sample.
#[derive(Debug, Clone, Copy)]
struct Point {
    t: i64,
    lat: f64,
    lon: f64,
    on_ground: bool,
    alt_msl: f64,
    gs: f64,
    ias: f64,
    vs: f64,
    g: f64,
    roll: f64,
    heading: f64,
    fuel: f64,
    autopilot: bool,
    /// The first sample after skipped ones (paused, replay, slew).
    resumed: bool,
}

impl Point {
    /// `None` for a sample with no real position yet (null island while MSFS loads, a
    /// garbage indicated altitude) or no ground contact reported.
    fn of(s: &Sample) -> Option<Self> {
        let v = |f| s.get(f).unwrap_or(0.0);
        let (lat, lon) = (s.get(Field::Latitude)?, s.get(Field::Longitude)?);
        if (lat.abs() < 0.01 && lon.abs() < 0.01) || v(Field::AltitudeIndicated) < -5000.0 {
            return None;
        }
        Some(Self {
            t: s.wall_time_ms,
            lat,
            lon,
            on_ground: s.get(Field::OnGround)? > 0.5,
            alt_msl: v(Field::AltitudeMsl),
            gs: v(Field::GroundSpeed),
            ias: v(Field::Ias),
            vs: v(Field::VerticalSpeed),
            g: v(Field::GNormal),
            roll: v(Field::Roll),
            heading: v(Field::HeadingTrue),
            fuel: v(Field::FuelLeft) + v(Field::FuelRight),
            autopilot: v(Field::Autopilot) > 0.5,
            resumed: false,
        })
    }
}

struct Analyser {
    phase: Phase,
    points: Vec<Point>,
    start_coords: Vec<(i64, f64, f64)>,
    /// The latest stretch on the ground, from touchdown through the taxi in.
    end_coords: VecDeque<(i64, f64, f64)>,
    max_alt: f64,
    max_gs: f64,
    initial_fuel: f64,
    final_fuel: f64,
    events: Vec<FlightEvent>,
    landed: bool,
    airborne_start: bool,
    last_on_ground: bool,
    last_autopilot: bool,
    last_vs: f64,
    takeoff_ms: Option<i64>,
    pending_toc: Option<(i64, f64, f64)>,
    pending_tod: Option<(i64, f64, f64)>,
    landing: Option<Landing>,
}

impl Analyser {
    fn new() -> Self {
        Self {
            phase: Phase::Parked,
            points: Vec::new(),
            start_coords: Vec::new(),
            end_coords: VecDeque::new(),
            max_alt: 0.0,
            max_gs: 0.0,
            initial_fuel: 0.0,
            final_fuel: 0.0,
            events: Vec::new(),
            landed: false,
            airborne_start: false,
            last_on_ground: true,
            last_autopilot: false,
            last_vs: 0.0,
            takeoff_ms: None,
            pending_toc: None,
            pending_tod: None,
            landing: None,
        }
    }

    fn event(&mut self, kind: EventKind, t: i64, lat: f64, lon: f64, heading: Option<f64>) {
        self.events.push(FlightEvent { kind, wall_time_ms: t, latitude: lat, longitude: lon, heading });
    }

    fn add_position(&mut self, p: &Point) {
        let first = self.points.first().map_or(p.t, |f| f.t);
        let spaced = |last: Option<&(i64, f64, f64)>| last.is_none_or(|l| p.t - l.0 >= ENDPOINT_SPACING_MS);
        if p.t - first < ENDPOINT_WINDOW_MS && spaced(self.start_coords.last()) {
            self.start_coords.push((p.t, p.lat, p.lon));
        }
        if !p.on_ground {
            return;
        }
        // A touchdown starts the stretch over, so a touch-and-go's field gives way.
        if !self.last_on_ground {
            self.end_coords.clear();
        }
        if spaced(self.end_coords.back()) {
            self.end_coords.push_back((p.t, p.lat, p.lon));
        }
        while self.end_coords.front().is_some_and(|f| p.t - f.0 > ENDPOINT_WINDOW_MS) {
            self.end_coords.pop_front();
        }
    }

    fn touchdown(&mut self, p: &Point) {
        self.phase = Phase::Landing;
        self.pending_toc = None;
        self.pending_tod = None;
        self.event(EventKind::Landing, p.t, p.lat, p.lon, Some(p.heading));
        if self.landing.is_none() {
            self.landing = Some(Landing {
                wall_time_ms: p.t,
                latitude: p.lat,
                longitude: p.lon,
                heading: p.heading,
                touchdown_fpm: self.last_vs,
                g: p.g,
                runway: None,
                offset_percent: None,
                threshold_dist_ft: None,
                vs_variance: None,
                ias_variance: None,
                approach_stability: None,
            });
        }
    }

    fn update(&mut self, p: Point) {
        if p.resumed {
            self.pending_toc = None;
            self.pending_tod = None;
        }
        self.points.push(p);
        self.add_position(&p);
        self.max_alt = self.max_alt.max(p.alt_msl);
        self.max_gs = self.max_gs.max(p.gs);
        if self.initial_fuel <= 0.0 && p.fuel > 0.0 {
            self.initial_fuel = p.fuel;
        }
        self.final_fuel = p.fuel;

        if p.autopilot != self.last_autopilot {
            let kind = if p.autopilot { EventKind::AutopilotOn } else { EventKind::AutopilotOff };
            self.event(kind, p.t, p.lat, p.lon, None);
            self.last_autopilot = p.autopilot;
        }

        let ground = p.on_ground;
        match self.phase {
            Phase::Parked => {
                if ground && p.gs > 2.0 {
                    self.phase = Phase::TaxiOut;
                } else if !ground {
                    self.phase = Phase::Climb;
                    self.takeoff_ms.get_or_insert(p.t);
                    self.airborne_start = true;
                }
            }
            Phase::TaxiOut => {
                if ground && p.ias > 45.0 {
                    self.phase = Phase::Takeoff;
                } else if !ground {
                    self.phase = Phase::Climb;
                    self.takeoff_ms.get_or_insert(p.t);
                }
            }
            // A bounce, touch-and-go or go-around comes back through here.
            Phase::Takeoff => {
                if !ground {
                    self.phase = Phase::Climb;
                    self.takeoff_ms.get_or_insert(p.t);
                    self.event(EventKind::Takeoff, p.t, p.lat, p.lon, None);
                }
            }
            Phase::Climb => {
                if ground {
                    self.touchdown(&p);
                } else if p.vs.abs() < 200.0 && p.ias > 60.0 {
                    match self.pending_toc {
                        Some((t, lat, lon)) if p.t - t >= STABLE_MS => {
                            self.phase = Phase::Cruise;
                            self.event(EventKind::TopOfClimb, t, lat, lon, None);
                            self.pending_toc = None;
                        }
                        Some(_) => {}
                        None => self.pending_toc = Some((p.t, p.lat, p.lon)),
                    }
                } else if p.vs < -400.0 {
                    self.phase = Phase::Descent;
                    self.event(EventKind::TopOfClimb, p.t, p.lat, p.lon, None);
                    self.event(EventKind::TopOfDescent, p.t, p.lat, p.lon, None);
                    self.pending_toc = None;
                } else {
                    self.pending_toc = None;
                }
            }
            Phase::Cruise => {
                if ground {
                    self.touchdown(&p);
                } else if p.vs < -500.0 {
                    match self.pending_tod {
                        Some((t, lat, lon)) if p.t - t >= STABLE_MS => {
                            self.phase = Phase::Descent;
                            self.event(EventKind::TopOfDescent, t, lat, lon, None);
                            self.pending_tod = None;
                        }
                        Some(_) => {}
                        None => self.pending_tod = Some((p.t, p.lat, p.lon)),
                    }
                } else if p.vs > 500.0 {
                    self.phase = Phase::Climb;
                    self.pending_tod = None;
                } else {
                    self.pending_tod = None;
                }
            }
            Phase::Descent => {
                if ground {
                    self.touchdown(&p);
                } else {
                    if p.alt_msl < 3000.0 && p.vs < -200.0 {
                        self.phase = Phase::Approach;
                    } else if p.vs > 300.0 {
                        self.phase = Phase::Climb;
                    } else if p.vs.abs() < 200.0 {
                        self.phase = Phase::Cruise;
                    }
                    self.pending_toc = None;
                    self.pending_tod = None;
                }
            }
            Phase::Approach => {
                if ground {
                    self.touchdown(&p);
                } else if p.vs > 500.0 {
                    self.phase = Phase::Climb;
                    self.pending_toc = None;
                    self.pending_tod = None;
                }
            }
            Phase::Landing => {
                if ground && p.gs < 30.0 {
                    self.phase = Phase::TaxiIn;
                    self.landed = true;
                } else if !ground {
                    self.phase = Phase::Takeoff;
                }
            }
            Phase::TaxiIn => {
                if p.gs < 1.0 {
                    self.phase = Phase::Parked;
                    self.event(EventKind::Parked, p.t, p.lat, p.lon, Some(p.heading));
                } else if !ground {
                    self.phase = Phase::Climb;
                }
            }
        }
        self.last_on_ground = ground;
        self.last_vs = p.vs;
    }

    fn departure(&self, places: &Places) -> Endpoint {
        if self.airborne_start {
            return Endpoint::Airborne;
        }
        dominant_airport(self.start_coords.iter(), places)
    }

    /// Where the latest stretch on the ground was, after takeoff.
    fn arrival(&self, places: &Places) -> Endpoint {
        if self.takeoff_ms.is_none() {
            return Endpoint::Unknown;
        }
        if !self.last_on_ground && !self.landed && !matches!(self.phase, Phase::Parked | Phase::TaxiOut) {
            return Endpoint::Airborne;
        }
        dominant_airport(self.end_coords.iter(), places)
    }

    fn finish(mut self, places: &Places) -> Option<FlightSummary> {
        let (first, last) = (self.points.first()?.t, self.points.last()?.t);
        let departure = self.departure(places);
        let arrival = self.arrival(places);
        if let (Some(landing), Endpoint::Airport(ident)) = (self.landing.as_mut(), &arrival) {
            if let Some(m) = best_runway_match(landing.latitude, landing.longitude, landing.heading, places.runways(ident)) {
                landing.runway = m.ident;
                landing.threshold_dist_ft = Some(m.along_ft);
                landing.offset_percent = Some(m.offset_percent);
            }
            let t = landing.wall_time_ms;
            if let Some(g) = peak_average_g(&self.points, t) {
                landing.g = g;
            }
            let stat = |from, to, value: fn(&Point) -> f64| {
                weighted_stats(&self.points, from, to, value)
            };
            if let (Some(vs), Some(ias)) =
                (stat(t - 60_000, t, |p| p.vs), stat(t - 60_000, t, |p| p.ias))
            {
                landing.vs_variance = Some(vs.1);
                landing.ias_variance = Some(ias.1);
            }
            landing.approach_stability = approach_stability(&self.points, t);
        }
        Some(FlightSummary {
            departure,
            arrival,
            first_ms: first,
            last_ms: last,
            takeoff_ms: self.takeoff_ms,
            max_altitude_ft: self.max_alt,
            max_ground_speed_kt: self.max_gs,
            fuel_consumed_gal: self.initial_fuel - self.final_fuel,
            events: self.events,
            landing: self.landing,
        })
    }
}

/// The highest [`G_AVERAGE_MS`] average of G starting in the second after touchdown.
fn peak_average_g(points: &[Point], touchdown: i64) -> Option<f64> {
    let start = points.partition_point(|p| p.t < touchdown);
    points[start..]
        .iter()
        .take_while(|p| p.t <= touchdown + TOUCHDOWN_G_MS)
        .filter_map(|p| weighted_stats_from(points, p.t, p.t + G_AVERAGE_MS, |p| p.g, 1).map(|(mean, _)| mean))
        .reduce(f64::max)
}

fn dominant_airport<'a>(coords: impl Iterator<Item = &'a (i64, f64, f64)>, places: &Places) -> Endpoint {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (_, lat, lon) in coords {
        if let Some(airport) = places.best_airport(*lat, *lon) {
            *counts.entry(&airport.ident).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(ident, count)| (*count, std::cmp::Reverse(*ident)))
        .map_or(Endpoint::NotFound, |(ident, _)| Endpoint::Airport(ident.to_string()))
}

/// Mean and variance of `value` over `[from, to]` ms, each sample weighted by how long it
/// held: samples come when something changed, not at a fixed rate. `None` with fewer than
/// five samples, as the app required.
fn weighted_stats(points: &[Point], from: i64, to: i64, value: fn(&Point) -> f64) -> Option<(f64, f64)> {
    weighted_stats_from(points, from, to, value, 5)
}

fn weighted_stats_from(
    points: &[Point],
    from: i64,
    to: i64,
    value: fn(&Point) -> f64,
    min_samples: usize,
) -> Option<(f64, f64)> {
    let start = points.partition_point(|p| p.t < from);
    let end = points.partition_point(|p| p.t <= to);
    let window = &points[start..end];
    if window.len() < min_samples {
        return None;
    }
    let weights: Vec<f64> = window
        .iter()
        .enumerate()
        .map(|(i, p)| (window.get(i + 1).map_or(to, |n| n.t) - p.t).max(0) as f64)
        .collect();
    let total: f64 = weights.iter().sum();
    if total <= 0.0 {
        return None;
    }
    let mean = window.iter().zip(&weights).map(|(p, w)| value(p) * w).sum::<f64>() / total;
    let var = window.iter().zip(&weights).map(|(p, w)| (value(p) - mean).powi(2) * w).sum::<f64>() / total;
    Some((mean, var))
}

/// The app's approach stability: standard deviations of G, roll and IAS from 60 to 5 s
/// before touchdown against what a steady approach allows, mapped so steady is 100 and
/// at tolerance on average is 50.
fn approach_stability(points: &[Point], touchdown: i64) -> Option<f64> {
    const G_TOLERANCE: f64 = 0.10;
    const ROLL_TOLERANCE_DEG: f64 = 5.0;
    const SPEED_TOLERANCE_KT: f64 = 5.0;
    let (from, to) = (touchdown - 60_000, touchdown - 5_000);
    let sd = |value: fn(&Point) -> f64| weighted_stats(points, from, to, value).map(|(_, v)| v.sqrt());
    let instability = (sd(|p| p.g)? / G_TOLERANCE
        + sd(|p| p.roll)? / ROLL_TOLERANCE_DEG
        + sd(|p| p.ias)? / SPEED_TOLERANCE_KT)
        / 3.0;
    Some(100.0 / (1.0 + instability))
}

struct RunwayMatch {
    ident: Option<String>,
    along_ft: f64,
    offset_percent: f64,
}

/// The runway end whose centreline the touchdown is nearest, within 200 ft and a runway
/// length (plus 500 ft either side), heading within 30° preferred.
fn best_runway_match(lat: f64, lon: f64, heading: f64, runways: &[Runway]) -> Option<RunwayMatch> {
    let mut best: Option<(f64, RunwayMatch)> = None;
    for rw in runways {
        let ends = [
            (&rw.le_ident, rw.le_latitude_deg, rw.le_longitude_deg, rw.le_heading_degt, rw.le_displaced_threshold_ft),
            (&rw.he_ident, rw.he_latitude_deg, rw.he_longitude_deg, rw.he_heading_degt, rw.he_displaced_threshold_ft),
        ];
        for (ident, end_lat, end_lon, end_hdg, displaced) in ends {
            let (Some(end_lat), Some(end_lon), Some(end_hdg)) = (end_lat, end_lon, end_hdg) else {
                continue;
            };
            let (lateral, along) = project_onto_runway(lat, lon, end_lat, end_lon, end_hdg, displaced.unwrap_or(0) as f64);
            let length = rw.length_ft.unwrap_or(0) as f64;
            if lateral.abs() >= 200.0 || along <= -500.0 || along >= length + 500.0 {
                continue;
            }
            let half_width = rw.width_ft.unwrap_or(150) as f64 / 2.0;
            let off_heading = {
                let d = (heading - end_hdg).abs() % 360.0;
                if d > 180.0 { 360.0 - d } else { d }
            };
            let score = lateral.abs() + if off_heading > 30.0 { 10_000.0 } else { 0.0 };
            if best.as_ref().is_none_or(|(s, _)| score < *s) {
                let m = RunwayMatch { ident: ident.clone(), along_ft: along, offset_percent: lateral / half_width * 100.0 };
                best = Some((score, m));
            }
        }
    }
    best.map(|(_, m)| m)
}

/// The touchdown in the runway's frame, flat-earth at runway scale: lateral offset from
/// the centreline, left of the runway heading positive as the app had it, and distance
/// along it past the (displaced) threshold, both in feet.
fn project_onto_runway(lat: f64, lon: f64, thr_lat: f64, thr_lon: f64, hdg: f64, displaced: f64) -> (f64, f64) {
    const EARTH_RADIUS_FT: f64 = 20_925_646.3;
    let y = (lat - thr_lat).to_radians() * EARTH_RADIUS_FT;
    let x = (lon - thr_lon).to_radians() * EARTH_RADIUS_FT * thr_lat.to_radians().cos();
    let angle = (90.0 - hdg).to_radians();
    let along = x * angle.cos() + y * angle.sin();
    let lateral = -x * angle.sin() + y * angle.cos();
    (lateral, along - displaced)
}

/// Ground contact as it held: a change lasting less than [`GROUND_HOLD_MS`] is dropped,
/// and one that lasts takes effect from its first sample.
fn debounce_ground(points: &mut [Point]) {
    let Some(first) = points.first() else { return };
    let mut state = first.on_ground;
    let mut i = 0;
    while i < points.len() {
        if points[i].on_ground == state {
            i += 1;
            continue;
        }
        let run_end = points[i..].iter().position(|p| p.on_ground == state).map_or(points.len(), |n| i + n);
        let lasted = points.get(run_end).map_or(i64::MAX, |p| p.t) - points[i].t;
        if lasted >= GROUND_HOLD_MS {
            state = !state;
            i = run_end;
        } else {
            points[i..run_end].iter_mut().for_each(|p| p.on_ground = state);
            i = run_end;
        }
    }
}

fn distance_nm(a: &Point, b: &Point) -> f64 {
    super::places::haversine_km(a.lat, a.lon, b.lat, b.lon) / 1.852
}

/// Splits at teleports: a move further than the ground speed could have carried the
/// aircraft.
fn split_at_teleports(points: &[Point]) -> Vec<&[Point]> {
    let mut parts = Vec::new();
    let mut start = 0;
    for i in 1..points.len() {
        let (a, b) = (&points[i - 1], &points[i]);
        let hours = (b.t - a.t).max(0) as f64 / 3_600_000.0;
        let plausible = a.gs.max(0.0) * hours * TELEPORT_SPEED_FACTOR + TELEPORT_MARGIN_NM;
        if distance_nm(a, b) > plausible {
            parts.push(&points[start..i]);
            start = i;
        }
    }
    parts.push(&points[start..]);
    parts
}

/// The flights in a session's samples, in order: each from the aircraft's first move
/// after a teleport, or after the session started.
pub fn analyse(samples: &[Sample], places: &Places) -> Vec<FlightSummary> {
    let mut points = Vec::new();
    let mut skipped = false;
    for s in samples {
        if s.flags & SKIPPED != 0 {
            skipped = true;
        } else if let Some(p) = Point::of(s) {
            points.push(Point { resumed: std::mem::take(&mut skipped), ..p });
        }
    }
    debounce_ground(&mut points);
    split_at_teleports(&points)
        .into_iter()
        .filter_map(|part| {
            let moving = part.iter().position(|p| p.gs > MOVING_KT)?;
            let mut analyser = Analyser::new();
            part[moving..].iter().for_each(|p| analyser.update(*p));
            analyser.finish(places)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::places::mock_airport;

    /// Parked on the ground at 45,45 with full tanks, `secs` into the flight.
    fn point(secs: f64) -> Point {
        Point {
            t: (secs * 1000.0) as i64,
            lat: 45.0,
            lon: 45.0,
            on_ground: true,
            alt_msl: 1000.0,
            gs: 0.0,
            ias: 0.0,
            vs: 0.0,
            g: 1.0,
            roll: 0.0,
            heading: 0.0,
            fuel: 40.0,
            autopilot: false,
            resumed: false,
        }
    }

    fn places(airports: &[(&str, f64, f64)]) -> Places {
        Places::new(airports.iter().map(|(i, lat, lon)| mock_airport(i, *lat, *lon)).collect(), HashMap::new())
    }

    fn kinds(a: &Analyser) -> Vec<EventKind> {
        a.events.iter().map(|e| e.kind).collect()
    }

    #[test]
    fn phases_follow_the_app() {
        use EventKind::*;
        let mut a = Analyser::new();
        let mut p = point(1.0);
        p.gs = 5.0;
        a.update(p);
        assert_eq!(a.phase, Phase::TaxiOut);
        p.ias = 50.0;
        a.update(Point { t: 2_000, ..p });
        assert_eq!(a.phase, Phase::Takeoff);
        p.on_ground = false;
        p.vs = 500.0;
        a.update(Point { t: 3_000, ..p });
        assert_eq!(a.phase, Phase::Climb);

        // Level for 10 s: top of climb, dated when it levelled.
        (p.vs, p.ias) = (50.0, 100.0);
        a.update(Point { t: 4_000, ..p });
        a.update(Point { t: 13_999, ..p });
        assert_eq!(a.phase, Phase::Climb);
        a.update(Point { t: 14_000, ..p });
        assert_eq!(a.phase, Phase::Cruise);

        p.vs = -600.0;
        a.update(Point { t: 15_000, ..p });
        a.update(Point { t: 25_000, ..p });
        assert_eq!(a.phase, Phase::Descent);
        (p.alt_msl, p.vs) = (2500.0, -300.0);
        a.update(Point { t: 26_000, ..p });
        assert_eq!(a.phase, Phase::Approach);
        p.on_ground = true;
        a.update(Point { t: 27_000, ..p });
        assert_eq!(a.phase, Phase::Landing);
        assert_eq!(a.landing.as_ref().unwrap().touchdown_fpm, -300.0);
        p.gs = 10.0;
        a.update(Point { t: 28_000, ..p });
        assert_eq!(a.phase, Phase::TaxiIn);
        assert_eq!(kinds(&a), [Takeoff, TopOfClimb, TopOfDescent, Landing]);
        assert_eq!(a.events[1].wall_time_ms, 4_000);
    }

    #[test]
    fn a_pause_restarts_the_top_of_climb_timer() {
        let mut a = Analyser::new();
        a.phase = Phase::Climb;
        let mut p = point(0.0);
        (p.on_ground, p.ias, p.vs) = (false, 100.0, 0.0);
        a.update(p);
        // Paused for a minute, then level for 5 s: not yet.
        a.update(Point { t: 60_000, resumed: true, ..p });
        a.update(Point { t: 65_000, ..p });
        assert_eq!(a.phase, Phase::Climb);
        a.update(Point { t: 70_000, ..p });
        assert_eq!(a.phase, Phase::Cruise);
        assert_eq!(a.events[0].wall_time_ms, 60_000);
    }

    #[test]
    fn a_touch_and_go_keeps_the_first_takeoff_and_gives_way_to_the_next_field() {
        let places = places(&[("A1", 40.0, 5.0), ("A2", 41.0, 5.0), ("A3", 42.0, 5.0)]);
        let mut a = Analyser::new();
        let mut p = point(0.0);
        (p.lat, p.lon, p.gs) = (40.0, 5.0, 5.0);
        a.update(p);
        p.ias = 50.0;
        a.update(Point { t: 1_000, ..p });
        p.on_ground = false;
        a.update(Point { t: 2_000, ..p });
        assert_eq!(a.takeoff_ms, Some(2_000));

        // A long touch-and-go at A2, then off again.
        (p.lat, p.on_ground, p.gs) = (41.0, true, 65.0);
        for i in 0..100 {
            a.update(Point { t: 1_200_000 + i * 1000, ..p });
        }
        assert_eq!(a.arrival(&places), Endpoint::Airport("A2".into()));
        p.on_ground = false;
        a.update(Point { t: 1_320_000, ..p });
        a.update(Point { t: 1_321_000, ..p });
        assert_eq!(a.phase, Phase::Climb);
        assert_eq!(a.takeoff_ms, Some(2_000));

        // A brief stop at A3 outweighs the hundred seconds at A2.
        (p.lat, p.on_ground) = (42.0, true);
        for i in 0..5 {
            a.update(Point { t: 2_400_000 + i * 1000, ..p });
        }
        assert_eq!(a.arrival(&places), Endpoint::Airport("A3".into()));
    }

    #[test]
    fn the_arrival_is_where_it_rolled_out_not_what_it_flew_over() {
        // KINT, and the closed strip east of it that a long final passes nearer.
        let places = places(&[("KCVG", 39.0488, -84.6678), ("KINT", 36.1337, -80.2220), ("NC63", 36.1351, -80.1848)]);
        let mut a = Analyser::new();
        let mut p = point(0.0);
        (p.lat, p.lon, p.gs) = (39.0488, -84.6678, 5.0);
        a.update(p);
        p.ias = 50.0;
        a.update(Point { t: 180_000, ..p });
        (p.on_ground, p.vs) = (false, 2000.0);
        a.update(Point { t: 181_000, ..p });
        assert_eq!(a.arrival(&places), Endpoint::Airborne);

        // Final, nearer NC63 all the way down, at 30 Hz.
        p.vs = -700.0;
        for i in 0..7500 {
            let f = i as f64 / 30.0;
            (p.lat, p.lon) = (36.08 + f * 0.0002, -80.17 - f * 0.0001);
            a.update(Point { t: 3_300_000 + i * 33, ..p });
        }
        (p.on_ground, p.gs) = (true, 120.0);
        for i in 0..1200 {
            let f = i as f64 / 30.0;
            (p.lat, p.lon) = (36.127 + f * 0.0002, -80.216 - f * 0.0001);
            a.update(Point { t: 3_600_000 + i * 33, ..p });
        }
        assert_eq!(a.arrival(&places), Endpoint::Airport("KINT".into()));
    }

    #[test]
    fn autopilot_events() {
        let mut a = Analyser::new();
        let mut p = point(0.0);
        p.autopilot = true;
        a.update(p);
        p.autopilot = false;
        a.update(Point { t: 1000, ..p });
        assert_eq!(kinds(&a), [EventKind::AutopilotOn, EventKind::AutopilotOff]);
    }

    #[test]
    fn ground_contact_must_hold() {
        let mut points: Vec<Point> = (0..40).map(|i| point(i as f64 * 0.05)).collect();
        // Off for 0.1 s at 0.5 s, then off for good from 1.0 s.
        for p in &mut points[10..12] {
            p.on_ground = false;
        }
        for p in &mut points[20..] {
            p.on_ground = false;
        }
        debounce_ground(&mut points);
        assert_eq!(points.iter().position(|p| !p.on_ground), Some(20));
        assert!(points[20..].iter().all(|p| !p.on_ground));
    }

    #[test]
    fn a_teleport_splits_the_session() {
        // Taxiing at 10 kt, then 60 nm away in a second.
        let mut points: Vec<Point> = (0..10).map(|i| Point { gs: 10.0, ..point(i as f64) }).collect();
        for p in &mut points[6..] {
            p.lat += 1.0;
        }
        let parts = split_at_teleports(&points);
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [6, 4]);
    }

    #[test]
    fn landing_score_is_the_apps() {
        let landing = Landing {
            wall_time_ms: 0,
            latitude: 0.0,
            longitude: 0.0,
            heading: 0.0,
            touchdown_fpm: -600.0,
            g: 1.4,
            runway: None,
            offset_percent: Some(-10.0),
            threshold_dist_ft: Some(800.0),
            vs_variance: None,
            ias_variance: None,
            approach_stability: None,
        };
        // 10 for offset, 50 for distance, 10 for G, 10 + 20 for sink rate.
        assert!((landing.score() + 100.0).abs() < 1e-9);
    }

    #[test]
    fn a_touchdown_on_the_centreline_matches_the_runway() {
        // A north-south runway: "36" from the south threshold, "18" from the north.
        let runway = Runway {
            airport_ident: "X".into(),
            length_ft: Some(5000),
            width_ft: Some(100),
            le_ident: Some("36".into()),
            le_latitude_deg: Some(45.0),
            le_longitude_deg: Some(5.0),
            le_heading_degt: Some(0.0),
            le_displaced_threshold_ft: None,
            he_ident: Some("18".into()),
            he_latitude_deg: Some(45.0 + 5000.0 / 364_000.0),
            he_longitude_deg: Some(5.0),
            he_heading_degt: Some(180.0),
            he_displaced_threshold_ft: None,
        };
        // 1000 ft past the south threshold, heading north.
        let m = best_runway_match(45.0 + 1000.0 / 364_000.0, 5.0, 2.0, &[runway]).unwrap();
        assert_eq!(m.ident.as_deref(), Some("36"));
        assert!((m.along_ft - 1000.0).abs() < 10.0, "{}", m.along_ft);
        assert!(m.offset_percent.abs() < 1.0);
    }
}
