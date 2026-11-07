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
use butterlog_wire::{flags, SimRunways};
use serde::Serialize;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Takeoff,
    Landing,
    TopOfClimb,
    TopOfDescent,
    Parked,
    AutopilotOn,
    AutopilotOff,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FlightEvent {
    pub kind: EventKind,
    pub wall_time_ms: i64,
    pub latitude: f64,
    pub longitude: f64,
    /// True heading, for landings and parking.
    pub heading: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
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

impl Serialize for Endpoint {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FlightSummary {
    pub departure: Endpoint,
    pub arrival: Endpoint,
    pub first_ms: i64,
    pub last_ms: i64,
    /// When the wheels first left the ground; a touch-and-go doesn't move it.
    pub takeoff_ms: Option<i64>,
    /// When the first takeoff roll passed [`TAKEOFF_IAS_KT`] on the ground: the app's
    /// `takeoff_time`. A speed, not a standstill, so a rolling takeoff has one too. `None`
    /// for a flight that started in the air.
    pub takeoff_roll_ms: Option<i64>,
    pub max_altitude_ft: f64,
    pub max_ground_speed_kt: f64,
    pub fuel_consumed_gal: f64,
    pub events: Vec<FlightEvent>,
    /// The first landing.
    pub landing: Option<Landing>,
}

/// How long the app's 300-sample endpoint windows lasted at its ~1 Hz.
const ENDPOINT_WINDOW_MS: i64 = 300_000;
/// Endpoint positions are taken at the app's rate, so a fast stretch doesn't outvote a
/// slow one.
const ENDPOINT_SPACING_MS: i64 = 1_000;
/// How long a climb must level off before it counts.
const STABLE_MS: i64 = 10_000;
/// A descent is a descent once the aircraft has dropped this far below where it started,
/// so a bump, a dip or a small correction at cruise is no top of descent. Lower down, half
/// its height above the field it last left, so a circuit still has one, but no less than
/// [`DESCENT_CONFIRM_MIN_FT`].
const DESCENT_CONFIRM_FT: f64 = 1_000.0;
const DESCENT_CONFIRM_MIN_FT: f64 = 300.0;
/// Vertical speed at which a descent may be starting.
const DESCENT_START_FPM: f64 = -400.0;
/// After a top of descent, how far the aircraft must climb back above its lowest point
/// before another descent gets one: a go-around does, a step-down on the approach doesn't.
const DESCENT_REARM_FT: f64 = 1_000.0;
/// Indicated airspeed on the ground at which taxiing becomes the takeoff roll.
pub const TAKEOFF_IAS_KT: f64 = 45.0;
/// How long after touchdown the landing's peak G is taken over.
const TOUCHDOWN_G_MS: i64 = 1_000;
const G_AVERAGE_MS: i64 = 200;
/// How long ground contact, or its absence, must last to count.
pub const GROUND_HOLD_MS: i64 = 500;
/// A flight starts when the aircraft first moves faster than this, as the app's logs do.
const MOVING_KT: f64 = 10.0;
/// A stop at least this long after the last touchdown ends the flight; the next move
/// starts another, as the app ends its log once the aircraft has landed and stopped.
const STOP_AFTER_TOUCHDOWN_MS: i64 = 10_000;
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

/// How much of the latest stretch the analyser keeps: enough for the minute before a
/// touchdown that the landing's statistics read.
const KEEP_RECENT_MS: i64 = 65_000;
/// How long after the first touchdown its points are kept: the second its peak G is
/// taken over, and the average that reaches past it.
const KEEP_AFTER_TOUCHDOWN_MS: i64 = TOUCHDOWN_G_MS + G_AVERAGE_MS;

/// Where a descent that hasn't yet dropped [`DESCENT_CONFIRM_FT`] started.
#[derive(Clone, Copy)]
struct PendingDescent {
    t: i64,
    lat: f64,
    lon: f64,
    alt: f64,
    /// Since when it has been level, if it is.
    level_since: Option<i64>,
}

#[derive(Clone)]
struct Analyser {
    phase: Phase,
    /// The first and the latest point's times.
    first_t: Option<i64>,
    last_t: i64,
    /// The latest [`KEEP_RECENT_MS`] of points.
    recent: VecDeque<Point>,
    /// The points around the first touchdown, which the landing's statistics read: those
    /// from [`KEEP_RECENT_MS`] before it to [`KEEP_AFTER_TOUCHDOWN_MS`] after.
    landing_points: Vec<Point>,
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
    takeoff_roll_ms: Option<i64>,
    pending_toc: Option<(i64, f64, f64)>,
    pending_tod: Option<PendingDescent>,
    /// The lowest altitude since the latest top of descent, until the aircraft has
    /// climbed [`DESCENT_REARM_FT`] above it; `None` while a descent would get one.
    tod_low: Option<f64>,
    /// The altitude the aircraft was last on the ground at.
    ground_alt: Option<f64>,
    landing: Option<Landing>,
    /// The latest touchdown, a touch-and-go's included.
    last_touchdown_ms: Option<i64>,
    /// Landed and stopped: the flight is over, and the next move starts another.
    stopped: bool,
}

impl Analyser {
    fn new() -> Self {
        Self {
            phase: Phase::Parked,
            first_t: None,
            last_t: 0,
            recent: VecDeque::new(),
            landing_points: Vec::new(),
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
            takeoff_roll_ms: None,
            pending_toc: None,
            pending_tod: None,
            tod_low: None,
            ground_alt: None,
            landing: None,
            last_touchdown_ms: None,
            stopped: false,
        }
    }

    fn event(&mut self, kind: EventKind, t: i64, lat: f64, lon: f64, heading: Option<f64>) {
        self.events.push(FlightEvent { kind, wall_time_ms: t, latitude: lat, longitude: lon, heading });
    }

    fn add_position(&mut self, p: &Point) {
        let first = self.first_t.unwrap_or(p.t);
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

    /// Follows a possible descent; returns where it started once it has dropped far enough
    /// (see [`DESCENT_CONFIRM_FT`]). Climbing back, or levelling off for [`STABLE_MS`], drops it.
    fn track_descent(&mut self, p: &Point) -> Option<PendingDescent> {
        let Some(mut d) = self.pending_tod else {
            if p.vs < DESCENT_START_FPM {
                self.pending_tod =
                    Some(PendingDescent { t: p.t, lat: p.lat, lon: p.lon, alt: p.alt_msl, level_since: None });
            }
            return None;
        };
        if p.alt_msl > d.alt + 100.0 || p.vs > 300.0 {
            self.pending_tod = None;
            return None;
        }
        if p.vs.abs() < 200.0 {
            let since = *d.level_since.get_or_insert(p.t);
            if p.t - since >= STABLE_MS {
                self.pending_tod = None;
                return None;
            }
        } else {
            d.level_since = None;
        }
        let needed = self.ground_alt.map_or(DESCENT_CONFIRM_FT, |g| {
            ((d.alt - g) / 2.0).clamp(DESCENT_CONFIRM_MIN_FT, DESCENT_CONFIRM_FT)
        });
        if d.alt - p.alt_msl >= needed {
            self.pending_tod = None;
            return Some(d);
        }
        self.pending_tod = Some(d);
        None
    }

    /// Records a confirmed descent's top, unless it continues one already recorded.
    fn top_of_descent(&mut self, d: PendingDescent, p: &Point) {
        self.phase = Phase::Descent;
        if self.tod_low.is_none() {
            self.event(EventKind::TopOfDescent, d.t, d.lat, d.lon, None);
        }
        self.tod_low = Some(self.tod_low.unwrap_or(p.alt_msl).min(p.alt_msl));
    }

    fn touchdown(&mut self, p: &Point) {
        self.phase = Phase::Landing;
        self.last_touchdown_ms = Some(p.t);
        self.pending_toc = None;
        self.pending_tod = None;
        self.event(EventKind::Landing, p.t, p.lat, p.lon, Some(p.heading));
        if self.landing.is_none() {
            self.landing_points = self.recent.iter().copied().collect();
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
        self.first_t.get_or_insert(p.t);
        self.last_t = p.t;
        self.recent.push_back(p);
        while self.recent.front().is_some_and(|f| p.t - f.t > KEEP_RECENT_MS) {
            self.recent.pop_front();
        }
        if self.landing.as_ref().is_some_and(|l| p.t <= l.wall_time_ms + KEEP_AFTER_TOUCHDOWN_MS) {
            self.landing_points.push(p);
        }
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
        if ground {
            self.ground_alt = Some(p.alt_msl);
        }
        if let Some(low) = self.tod_low {
            self.tod_low = (p.alt_msl < low + DESCENT_REARM_FT).then_some(low.min(p.alt_msl));
        }
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
                if ground && p.ias > TAKEOFF_IAS_KT {
                    self.phase = Phase::Takeoff;
                    self.takeoff_roll_ms.get_or_insert(p.t);
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
                } else if let Some(d) = self.track_descent(&p) {
                    // Straight from the climb into a descent: both tops are where it turned.
                    self.event(EventKind::TopOfClimb, d.t, d.lat, d.lon, None);
                    self.top_of_descent(d, &p);
                    self.pending_toc = None;
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
                } else {
                    self.pending_toc = None;
                }
            }
            Phase::Cruise => {
                if ground {
                    self.touchdown(&p);
                } else if let Some(d) = self.track_descent(&p) {
                    self.top_of_descent(d, &p);
                } else if p.vs > 500.0 {
                    self.phase = Phase::Climb;
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
                    // As the app closes its log: landed, then stopped once the rollout is
                    // well over, so a stop then another takeoff is two flights.
                    self.stopped = self.landed && self.last_touchdown_ms.is_some_and(|t| p.t - t >= STOP_AFTER_TOUCHDOWN_MS);
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

    /// `sim_runways` are those the sim sent, drawn as it draws them; the touchdown is
    /// measured against them first, then against OurAirports'.
    fn finish(mut self, places: &Places, sim_runways: &[Runway]) -> Option<FlightSummary> {
        let (first, last) = (self.first_t?, self.last_t);
        let departure = self.departure(places);
        let arrival = self.arrival(places);
        if let (Some(landing), Endpoint::Airport(ident)) = (self.landing.as_mut(), &arrival) {
            let (lat, lon, heading) = (landing.latitude, landing.longitude, landing.heading);
            if let Some(m) = best_runway_match(lat, lon, heading, sim_runways)
                .or_else(|| best_runway_match(lat, lon, heading, places.runways(ident)))
            {
                landing.runway = m.ident;
                landing.threshold_dist_ft = m.along_ft;
                landing.offset_percent = m.offset_percent;
            }
            let t = landing.wall_time_ms;
            let points = &self.landing_points;
            if let Some(g) = peak_average_g(points, t) {
                landing.g = g;
            }
            let stat = |from, to, value: fn(&Point) -> f64| weighted_stats(points, from, to, value);
            if let (Some(vs), Some(ias)) =
                (stat(t - 60_000, t, |p| p.vs), stat(t - 60_000, t, |p| p.ias))
            {
                landing.vs_variance = Some(vs.1);
                landing.ias_variance = Some(ias.1);
            }
            landing.approach_stability = approach_stability(points, t);
        }
        Some(FlightSummary {
            departure,
            arrival,
            first_ms: first,
            last_ms: last,
            takeoff_ms: self.takeoff_ms,
            takeoff_roll_ms: self.takeoff_roll_ms,
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

/// Where on a runway the touchdown was. A measure that puts a touchdown on the ground off
/// the pavement, beside the runway or before its end, says the runway is not where the
/// sim has it, so it is `None`, not a penalty in the score.
struct RunwayMatch {
    ident: Option<String>,
    along_ft: Option<f64>,
    offset_percent: Option<f64>,
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
                let displaced = displaced.unwrap_or(0) as f64;
                let m = RunwayMatch {
                    ident: ident.clone(),
                    along_ft: Some(along).filter(|a| *a >= -displaced && (length <= 0.0 || *a <= length - displaced)),
                    offset_percent: Some(lateral / half_width * 100.0).filter(|o| o.abs() <= 100.0),
                };
                best = Some((score, m));
            }
        }
    }
    best.map(|(_, m)| m)
}

/// The runways the sim sent in the samples' events, as OurAirports rows.
fn sim_runways(samples: &[Sample]) -> Vec<Runway> {
    samples
        .iter()
        .flat_map(|s| &s.events)
        .filter(|e| e.kind == butterlog_wire::EventKind::Runways)
        .filter_map(|e| SimRunways::decode(&e.detail))
        .flat_map(|sent| {
            sent.runways.into_iter().map(move |r| {
                let [le, he] = r.ends;
                Runway {
                    airport_ident: sent.airport.clone(),
                    length_ft: Some(r.length_ft.round() as i32),
                    width_ft: Some(r.width_ft.round() as i32),
                    le_ident: Some(le.ident),
                    le_latitude_deg: Some(le.latitude),
                    le_longitude_deg: Some(le.longitude),
                    le_heading_degt: Some(le.heading),
                    le_displaced_threshold_ft: Some(le.displaced_ft.round() as i32),
                    he_ident: Some(he.ident),
                    he_latitude_deg: Some(he.latitude),
                    he_longitude_deg: Some(he.longitude),
                    he_heading_degt: Some(he.heading),
                    he_displaced_threshold_ft: Some(he.displaced_ft.round() as i32),
                }
            })
        })
        .collect()
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
/// and one that lasts takes effect from its first sample. The whole-session form of
/// [`Debounce`], which the tests hold it to.
#[cfg(test)]
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
/// aircraft. The whole-session form of [`FlightStream`]'s, which the tests hold it to.
#[cfg(test)]
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
/// after a teleport, a stop after landing, or the session's start, to the next of these.
pub fn analyse(samples: &[Sample], places: &Places) -> Vec<FlightSummary> {
    let mut stream = FlightStream::<()>::new();
    samples.iter().for_each(|s| stream.push(s));
    stream.flights(places).into_iter().map(|(f, _)| f).collect()
}

/// What the analysis knows of a flight while its samples go by, for a [`FlightSink`].
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub takeoff_roll_ms: Option<i64>,
    pub landing_ms: Option<i64>,
}

/// Whatever else is built from a flight's samples as they go by: each sample from its
/// first point's to its last's, in order, as [`analyse`]'s flights span them.
pub trait FlightSink: Clone + Default {
    fn sample(&mut self, s: &Sample, progress: Progress);
}

impl FlightSink for () {
    fn sample(&mut self, _: &Sample, _: Progress) {}
}

/// How long engine 1 must stay shut down, parked, for a flight to be over.
const ENGINE_OFF_MS: i64 = 10_000;

/// Ground contact as it held, a point at a time: [`GROUND_HOLD_MS`]'s rule, a change kept
/// back until it has lasted or given way.
#[derive(Clone, Default)]
struct Debounce {
    state: Option<bool>,
    /// Points since the ground contact last differed from `state`, not yet decided.
    run: Vec<Point>,
}

impl Debounce {
    fn push(&mut self, p: Point, out: &mut Vec<Point>) {
        let Some(state) = self.state else {
            self.state = Some(p.on_ground);
            out.push(p);
            return;
        };
        if self.run.is_empty() {
            if p.on_ground == state {
                out.push(p);
            } else {
                self.run.push(p);
            }
            return;
        }
        if p.on_ground != state {
            self.run.push(p);
            // Already as long as it must be, however it ends.
            if p.t - self.run[0].t >= GROUND_HOLD_MS {
                self.state = Some(!state);
                out.append(&mut self.run);
            }
            return;
        }
        if p.t - self.run[0].t >= GROUND_HOLD_MS {
            self.state = Some(!state);
            out.append(&mut self.run);
            self.run.push(p);
        } else {
            out.extend(self.run.drain(..).map(|r| Point { on_ground: state, ..r }));
            out.push(p);
        }
    }

    /// At the end of the samples: a change still holding counts.
    fn flush(&mut self, out: &mut Vec<Point>) {
        if !self.run.is_empty() {
            self.state = self.state.map(|s| !s);
            out.append(&mut self.run);
        }
    }
}

/// When engine 1 was shut down, parked, after a landing: engine 1's fuel flow off, on the
/// ground and stopped, for [`ENGINE_OFF_MS`]. Both sims report the fuel flow, a piston's or
/// a turbine's, and cut it with the engine.
#[derive(Clone)]
struct EngineOff {
    after_ms: i64,
    since: Option<i64>,
    off: bool,
}

impl EngineOff {
    fn sample(&mut self, s: &Sample) {
        if self.off || s.wall_time_ms < self.after_ms {
            return;
        }
        let off = s.get(Field::OnGround).is_some_and(|g| g > 0.5)
            && s.get(Field::GroundSpeed).is_some_and(|v| v < 1.0)
            && s.get(Field::FuelFlow).is_some_and(|f| f < 0.1);
        match self.since {
            _ if !off => self.since = None,
            None => self.since = Some(s.wall_time_ms),
            Some(t) if s.wall_time_ms - t >= ENGINE_OFF_MS => self.off = true,
            Some(_) => {}
        }
    }
}

/// One flight of a stream: its analysis, what its sink built, and whether its engine was
/// shut down after it landed.
#[derive(Clone)]
struct StreamFlight<S> {
    analyser: Analyser,
    sink: S,
    engine: Option<EngineOff>,
}

impl<S: FlightSink> StreamFlight<S> {
    fn progress(&self) -> Progress {
        Progress {
            takeoff_roll_ms: self.analyser.takeoff_roll_ms,
            landing_ms: self.analyser.landing.as_ref().map(|l| l.wall_time_ms),
        }
    }
}

/// [`analyse`] a sample at a time, for a session still arriving: what it keeps is bounded
/// by the flights, not by how long they last. Its flights at any point are [`analyse`]'s
/// of the samples so far.
#[derive(Clone)]
pub struct FlightStream<S> {
    runways: Vec<Runway>,
    skipped: bool,
    ground: Debounce,
    last: Option<Point>,
    flights: Vec<StreamFlight<S>>,
    /// Whether the last of `flights` is still being flown.
    flying: bool,
    /// Samples later than the last point, not yet known to be in a flight or not.
    queue: VecDeque<Sample>,
    /// Those given out last, at the last one's time: a flight starting at that time takes
    /// them too.
    same_time: Vec<Sample>,
}

impl<S: FlightSink> FlightStream<S> {
    pub fn new() -> Self {
        Self {
            runways: Vec::new(),
            skipped: false,
            ground: Debounce::default(),
            last: None,
            flights: Vec::new(),
            flying: false,
            queue: VecDeque::new(),
            same_time: Vec::new(),
        }
    }

    pub fn push(&mut self, s: &Sample) {
        self.runways.extend(sim_runways(std::slice::from_ref(s)));
        let point = if s.flags & SKIPPED != 0 {
            self.skipped = true;
            None
        } else {
            Point::of(s).map(|p| Point { resumed: std::mem::take(&mut self.skipped), ..p })
        };
        self.queue.push_back(s.clone());
        if let Some(p) = point {
            let mut settled = Vec::new();
            self.ground.push(p, &mut settled);
            settled.into_iter().for_each(|p| self.point(p));
        }
    }

    /// A point, its ground contact settled: as [`analyse`] went through them.
    fn point(&mut self, p: Point) {
        if let Some(last) = self.last {
            let hours = (p.t - last.t).max(0) as f64 / 3_600_000.0;
            let plausible = last.gs.max(0.0) * hours * TELEPORT_SPEED_FACTOR + TELEPORT_MARGIN_NM;
            if distance_nm(&last, &p) > plausible {
                self.flying = false;
            }
        }
        self.last = Some(p);
        if !self.flying && p.gs > MOVING_KT {
            self.flights.push(StreamFlight { analyser: Analyser::new(), sink: S::default(), engine: None });
            self.flying = true;
            // The samples at its first point's time, given out already.
            let flight = self.flights.last_mut().expect("just pushed");
            flight.analyser.update(p);
            let progress = flight.progress();
            for s in self.same_time.iter().filter(|s| s.wall_time_ms >= p.t) {
                flight.sink.sample(s, progress);
            }
        } else if self.flying {
            self.flights.last_mut().expect("flying").analyser.update(p);
        }
        if let Some(flight) = self.flights.last_mut().filter(|_| self.flying) {
            if flight.engine.is_none() {
                flight.engine = flight.analyser.landing.as_ref().map(|l| EngineOff { after_ms: l.wall_time_ms, since: None, off: false });
            }
        }
        self.give_out(p.t);
        if self.flying && self.flights.last().is_some_and(|f| f.analyser.stopped) {
            self.flying = false;
        }
    }

    /// Gives the samples up to `t` to the flight flying and to the engine watches.
    fn give_out(&mut self, t: i64) {
        while self.queue.front().is_some_and(|s| s.wall_time_ms <= t) {
            let s = self.queue.pop_front().expect("not empty");
            self.watch(&s);
            if let Some(flight) = self.flights.last_mut().filter(|_| self.flying) {
                if flight.analyser.first_t.is_some_and(|f| s.wall_time_ms >= f) {
                    let progress = flight.progress();
                    flight.sink.sample(&s, progress);
                }
            }
            if self.same_time.first().is_some_and(|f| f.wall_time_ms != s.wall_time_ms) {
                self.same_time.clear();
            }
            self.same_time.push(s);
        }
    }

    fn watch(&mut self, s: &Sample) {
        self.flights.iter_mut().filter_map(|f| f.engine.as_mut()).for_each(|e| e.sample(s));
    }

    /// The samples so far as if they ended here: the change of ground contact still
    /// holding counts, and the samples after the last point are in no flight.
    fn ended(&self) -> Self {
        let mut end = self.clone();
        let mut settled = Vec::new();
        end.ground.flush(&mut settled);
        settled.into_iter().for_each(|p| end.point(p));
        for s in std::mem::take(&mut end.queue) {
            end.watch(&s);
        }
        end
    }

    /// The flights so far, each with what its sink built.
    pub fn flights(&self, places: &Places) -> Vec<(FlightSummary, S)> {
        self.snapshot(places).0
    }

    /// How many of the flights so far, first to last, are over: each landed and shut its
    /// engine down, or a later flight followed it.
    #[cfg(test)]
    pub fn over(&self) -> usize {
        self.snapshot_over(&self.ended())
    }

    fn snapshot_over(&self, end: &Self) -> usize {
        let shut_down = |f: &StreamFlight<S>| f.engine.as_ref().is_some_and(|e| e.off);
        match end.flights.iter().rposition(shut_down) {
            Some(last) => last + 1,
            None => end.flights.len().saturating_sub(1),
        }
    }

    /// The flights so far, and how many of them are over.
    pub fn snapshot(&self, places: &Places) -> (Vec<(FlightSummary, S)>, usize) {
        let end = self.ended();
        let over = self.snapshot_over(&end);
        let flights = end
            .flights
            .into_iter()
            .filter_map(|f| Some((f.analyser.finish(places, &end.runways)?, f.sink)))
            .collect();
        (flights, over)
    }
}

impl<S: FlightSink> Default for FlightStream<S> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use butterlog_wire::{RunwayEnd, SimRunway};
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
        assert_eq!(a.takeoff_roll_ms, Some(2_000));
        p.on_ground = false;
        p.vs = 500.0;
        a.update(Point { t: 3_000, ..p });
        assert_eq!(a.phase, Phase::Climb);

        // Level for 10 s: top of climb, dated when it levelled.
        (p.vs, p.ias, p.alt_msl) = (50.0, 100.0, 5000.0);
        a.update(Point { t: 4_000, ..p });
        a.update(Point { t: 13_999, ..p });
        assert_eq!(a.phase, Phase::Climb);
        a.update(Point { t: 14_000, ..p });
        assert_eq!(a.phase, Phase::Cruise);

        p.vs = -600.0;
        a.update(Point { t: 15_000, ..p });
        a.update(Point { t: 25_000, alt_msl: 4100.0, ..p });
        assert_eq!(a.phase, Phase::Cruise);
        a.update(Point { t: 26_000, alt_msl: 4000.0, ..p });
        assert_eq!(a.phase, Phase::Descent);
        (p.alt_msl, p.vs) = (2500.0, -300.0);
        a.update(Point { t: 27_000, ..p });
        assert_eq!(a.phase, Phase::Approach);
        p.on_ground = true;
        a.update(Point { t: 28_000, ..p });
        assert_eq!(a.phase, Phase::Landing);
        assert_eq!(a.landing.as_ref().unwrap().touchdown_fpm, -300.0);
        p.gs = 10.0;
        a.update(Point { t: 29_000, ..p });
        assert_eq!(a.phase, Phase::TaxiIn);
        assert_eq!(kinds(&a), [Takeoff, TopOfClimb, TopOfDescent, Landing]);
        assert_eq!(a.events[1].wall_time_ms, 4_000);
        // Dated where the descent started, not where it was confirmed.
        assert_eq!(a.events[2].wall_time_ms, 15_000);
    }

    /// Airborne at `alt` feet and `vs` fpm, `secs` into the flight.
    fn flying(secs: f64, alt: f64, vs: f64) -> Point {
        Point { on_ground: false, alt_msl: alt, vs, ias: 120.0, gs: 120.0, ..point(secs) }
    }

    fn tops_of_descent(a: &Analyser) -> Vec<i64> {
        a.events.iter().filter(|e| e.kind == EventKind::TopOfDescent).map(|e| e.wall_time_ms).collect()
    }

    #[test]
    fn a_dip_at_cruise_is_no_top_of_descent() {
        let mut a = Analyser::new();
        a.phase = Phase::Cruise;
        a.update(flying(0.0, 8000.0, 0.0));
        // Down 600 ft at 800 fpm for 45 s, then back up: never 1000 ft below.
        for i in 1..=45 {
            a.update(flying(i as f64, 8000.0 - 13.0 * i as f64, -800.0));
        }
        a.update(flying(46.0, 7500.0, 600.0));
        a.update(flying(60.0, 8000.0, 0.0));
        assert!(tops_of_descent(&a).is_empty());
        // Down 600 ft and levelled there for good: dropped, and the real descent later
        // is dated from where it left that level.
        for i in 61..=100 {
            a.update(flying(i as f64, 7400.0, if i < 64 { -800.0 } else { 0.0 }));
        }
        a.update(flying(120.0, 7400.0, -1000.0));
        a.update(flying(170.0, 6300.0, -1000.0));
        assert_eq!(tops_of_descent(&a), [120_000]);
    }

    #[test]
    fn a_dip_in_the_climb_is_no_top_of_descent() {
        use EventKind::*;
        let mut a = Analyser::new();
        a.phase = Phase::Climb;
        a.update(flying(0.0, 4000.0, 1500.0));
        a.update(flying(1.0, 4000.0, -600.0));
        a.update(flying(2.0, 3950.0, -500.0));
        a.update(flying(3.0, 4000.0, 1500.0));
        a.update(flying(60.0, 9000.0, 0.0));
        a.update(flying(70.0, 9000.0, 0.0));
        assert_eq!(kinds(&a), [TopOfClimb]);
        assert_eq!(a.events[0].wall_time_ms, 60_000);
    }

    #[test]
    fn step_downs_keep_one_top_of_descent_and_a_go_around_gets_another() {
        let mut a = Analyser::new();
        a.phase = Phase::Cruise;
        a.update(flying(0.0, 9000.0, 0.0));
        a.update(flying(10.0, 9000.0, -1000.0));
        a.update(flying(80.0, 7800.0, -1000.0));
        // Level at 6000 for a while, then down again: the same descent.
        a.update(flying(200.0, 6000.0, 0.0));
        a.update(flying(260.0, 6000.0, 0.0));
        a.update(flying(270.0, 6000.0, -800.0));
        a.update(flying(370.0, 4500.0, -800.0));
        assert_eq!(tops_of_descent(&a), [10_000]);
        // A go-around from 3500 back up to 6000, level, and down again.
        a.update(flying(400.0, 3500.0, -600.0));
        a.update(flying(410.0, 3600.0, 1500.0));
        a.update(flying(500.0, 6000.0, 0.0));
        a.update(flying(510.0, 6000.0, 0.0));
        a.update(flying(520.0, 6000.0, -900.0));
        a.update(flying(600.0, 4800.0, -900.0));
        assert_eq!(tops_of_descent(&a), [10_000, 520_000]);
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

    /// A circuit as samples, one a second from `t0`: a taxi, the roll, a minute airborne,
    /// the touchdown and rollout, then stopped for 30 s.
    fn circuit(t0: i64) -> Vec<Sample> {
        let mut out = Vec::new();
        let mut at = |secs: i64, ground: bool, gs: f64, ias: f64| {
            let mut s = Sample::empty();
            s.wall_time_ms = (t0 + secs) * 1_000;
            s.set(Field::Latitude, 45.0);
            s.set(Field::Longitude, 45.0);
            s.set(Field::AltitudeIndicated, if ground { 1000.0 } else { 2000.0 });
            s.set(Field::AltitudeMsl, if ground { 1000.0 } else { 2000.0 });
            s.set(Field::OnGround, if ground { 1.0 } else { 0.0 });
            s.set(Field::GroundSpeed, gs);
            s.set(Field::Ias, ias);
            s.set(Field::GNormal, 1.0);
            out.push(s);
        };
        (0..10).for_each(|i| at(i, true, 15.0, 15.0));
        (10..20).for_each(|i| at(i, true, 60.0, 60.0));
        (20..80).for_each(|i| at(i, false, 90.0, 90.0));
        (80..90).for_each(|i| at(i, true, 50.0, 50.0));
        (90..100).for_each(|i| at(i, true, 10.0, 10.0));
        (100..130).for_each(|i| at(i, true, 0.0, 0.0));
        out
    }

    /// [`analyse`] as it was, on the whole session at once: what the stream must find at
    /// every point.
    fn analyse_whole(samples: &[Sample], places: &Places) -> Vec<FlightSummary> {
        let sim_runways = sim_runways(samples);
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
        let mut flights = Vec::new();
        for part in split_at_teleports(&points) {
            let mut rest = part;
            while let Some(moving) = rest.iter().position(|p| p.gs > MOVING_KT) {
                let mut analyser = Analyser::new();
                let mut used = rest.len();
                for (i, p) in rest.iter().enumerate().skip(moving) {
                    analyser.update(*p);
                    if analyser.stopped {
                        used = i + 1;
                        break;
                    }
                }
                flights.extend(analyser.finish(places, &sim_runways));
                rest = &rest[used..];
            }
        }
        flights
    }

    /// How many flights were over, as it was: landed and shut down, or followed.
    fn over_whole(flights: &[FlightSummary], samples: &[Sample]) -> usize {
        let shut_down = |f: &FlightSummary| {
            f.landing.as_ref().is_some_and(|l| {
                let mut e = EngineOff { after_ms: l.wall_time_ms, since: None, off: false };
                samples.iter().for_each(|s| e.sample(s));
                e.off
            })
        };
        match flights.iter().rposition(shut_down) {
            Some(last) => last + 1,
            None => flights.len().saturating_sub(1),
        }
    }

    /// The stream, fed a sample at a time, finds at every point what the whole analysis
    /// finds of the samples so far.
    fn holds_to_the_whole(samples: &[Sample], places: &Places, every: usize) -> usize {
        let mut stream = FlightStream::<()>::new();
        for (i, s) in samples.iter().enumerate() {
            stream.push(s);
            if i % every == 0 || i + 1 == samples.len() {
                let so_far = &samples[..=i];
                let whole = analyse_whole(so_far, places);
                let streamed: Vec<FlightSummary> = stream.flights(places).into_iter().map(|(f, _)| f).collect();
                assert_eq!(streamed, whole, "after {} of {} samples", i + 1, samples.len());
                assert_eq!(stream.over(), over_whole(&whole, so_far), "over, after {} samples", i + 1);
            }
        }
        stream.flights(places).len()
    }

    fn fixture(name: &str) -> Vec<Sample> {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        crate::normalise::normalise_segment(&std::fs::read(path).unwrap()).unwrap().1
    }

    #[test]
    fn the_stream_finds_what_the_whole_analysis_finds() {
        let real = Places::load(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data")).unwrap();
        for name in ["msfs_pattern.blsg.zst", "xplane_pattern.blsg.zst"] {
            let samples = fixture(name);
            assert_eq!(holds_to_the_whole(&samples, &real, 97), 1, "{name}");
            // Twice in a row, the second moved away: a teleport, then another flight.
            let mut twice = samples.clone();
            let shift = samples.last().unwrap().wall_time_ms - samples[0].wall_time_ms + 1_000;
            twice.extend(samples.iter().cloned().map(|mut s| {
                s.wall_time_ms += shift;
                if let Some(lat) = s.get(Field::Latitude) {
                    s.set(Field::Latitude, lat + 1.0);
                }
                s
            }));
            assert_eq!(holds_to_the_whole(&twice, &real, 211), 2, "{name} twice");
        }
        // Circuits with a stop between, and a flicker of ground contact in the air.
        let airports = places(&[("XX", 45.0, 45.0)]);
        let mut circuits = circuit(0);
        circuits.extend(circuit(130));
        circuits[40].set(Field::OnGround, 1.0);
        assert_eq!(holds_to_the_whole(&circuits, &airports, 1), 2);
    }

    #[test]
    fn a_stop_after_landing_ends_the_flight() {
        let places = places(&[("XX", 45.0, 45.0)]);
        // Two circuits, back to back in one session: two flights, as the app makes.
        let mut samples = circuit(0);
        samples.extend(circuit(130));
        let flights = analyse(&samples, &places);
        assert_eq!(flights.len(), 2, "{flights:?}");
        assert_eq!(flights[0].takeoff_ms, Some(20_000));
        assert_eq!(flights[1].takeoff_ms, Some(150_000));
        assert!(flights[0].last_ms <= 130_000 && flights[0].landing.is_some());
        assert_eq!(flights[1].first_ms, 130_000);
        // A touch-and-go stops nowhere: one flight.
        let mut touch_and_go = circuit(0);
        touch_and_go.truncate(85);
        touch_and_go.extend(circuit(85).into_iter().skip(10));
        assert_eq!(analyse(&touch_and_go, &places).len(), 1);
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
        let m = best_runway_match(45.0 + 1000.0 / 364_000.0, 5.0, 2.0, &[runway.clone()]).unwrap();
        assert_eq!(m.ident.as_deref(), Some("36"));
        assert!((m.along_ft.unwrap() - 1000.0).abs() < 10.0, "{:?}", m.along_ft);
        assert!(m.offset_percent.unwrap().abs() < 1.0);

        // 70 ft left of a 100 ft runway's centreline, as at PHJH: off the pavement, so the
        // runway isn't where the sim has it, and there is no offset to score.
        let left = 70.0 / (364_000.0 * 45f64.to_radians().cos());
        let m = best_runway_match(45.0 + 1000.0 / 364_000.0, 5.0 - left, 2.0, &[runway.clone()]).unwrap();
        assert_eq!(m.ident.as_deref(), Some("36"));
        assert!(m.along_ft.is_some());
        assert_eq!(m.offset_percent, None);
        // Before the threshold: on the ground short of the pavement.
        let m = best_runway_match(45.0 - 200.0 / 364_000.0, 5.0, 2.0, &[runway]).unwrap();
        assert_eq!(m.along_ft, None);
        assert!(m.offset_percent.is_some());
    }

    #[test]
    fn the_sims_runways_come_first() {
        let ours = Runway {
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
        // The sim's runway is 70 ft east of ours, and the aircraft lands on its centreline.
        let east = 70.0 / (364_000.0 * 45f64.to_radians().cos());
        let sent = SimRunways {
            airport: "X".into(),
            runways: vec![SimRunway {
                width_ft: 100.0,
                length_ft: 5000.0,
                ends: [
                    RunwayEnd { ident: "36".into(), latitude: 45.0, longitude: 5.0 + east, heading: 0.0, displaced_ft: 0.0 },
                    RunwayEnd {
                        ident: "18".into(),
                        latitude: 45.0 + 5000.0 / 364_000.0,
                        longitude: 5.0 + east,
                        heading: 180.0,
                        displaced_ft: 0.0,
                    },
                ],
            }],
        };
        let mut s = Sample::empty();
        s.events.push(butterlog_wire::Event { kind: butterlog_wire::EventKind::Runways, detail: sent.encode() });
        let sim = sim_runways(&[s]);
        assert_eq!(sim.len(), 1);
        let (lat, lon) = (45.0 + 1000.0 / 364_000.0, 5.0 + east);
        let m = best_runway_match(lat, lon, 2.0, &sim).unwrap();
        assert!(m.offset_percent.unwrap().abs() < 1.0, "{:?}", m.offset_percent);
        assert_eq!(best_runway_match(lat, lon, 2.0, &[ours]).unwrap().offset_percent, None);
    }
}
