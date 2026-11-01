//! Per-sim normalisers (docs/sim-native-telemetry.md §5.2): a session's raw channels, in
//! each sim's own names, units and conventions, become one canonical sample stream.
//!
//! Canonical conventions, checked against rollout phase 1 captures of both sims:
//! - pitch is positive nose up and roll positive right wing down. MSFS reports both the
//!   other way round; X-Plane already matches.
//! - headings and tracks are true. X-Plane's `mag_psi` is magnetic.
//! - magnetic variation is positive east. X-Plane's `magnetic_variation` is positive west.
//! - frequencies are in MHz. X-Plane gives COM in kHz and NAV in tens of kHz.
//! - otherwise the units of the app's flight logs: feet, knots, feet per minute, G, inHg,
//!   °C for the air and °F for engine temperatures, US gallons (X-Plane's kilograms as
//!   avgas).
//!
//! Channels report only changes, so each sample carries the latest value of every field;
//! `None` until a field's first value.

// Read by the server analysis that follows in rollout phase 3.
#![allow(dead_code)]

use butterlog_wire::{
    decode_segment, decompress, Channel, DecodeError, Event, SessionDecoder, SessionHeader,
    SessionOpen, Tick, Value,
};
use std::fmt;

macro_rules! fields {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Field { $($(#[$doc])* $name),* }

        impl Field {
            pub const ALL: &'static [Field] = &[$(Field::$name),*];
        }
    };
}

fields! {
    /// Degrees.
    Latitude,
    Longitude,
    /// True altitude above mean sea level, feet.
    AltitudeMsl,
    /// What the altimeter shows, feet.
    AltitudeIndicated,
    AltitudeAgl,
    AltitudePressure,
    /// Altimeter setting, inHg.
    Baro,
    /// Knots.
    Ias,
    Tas,
    GroundSpeed,
    /// Feet per minute.
    VerticalSpeed,
    /// Degrees, nose up positive.
    Pitch,
    /// Degrees, right wing down positive.
    Roll,
    /// Degrees.
    HeadingTrue,
    HeadingMagnetic,
    /// Ground track, degrees true.
    Track,
    /// Degrees, east positive.
    MagneticVariation,
    /// G.
    GNormal,
    GLateral,
    /// 1 on the ground, 0 airborne.
    OnGround,
    /// Outside air, °C.
    Oat,
    WindSpeed,
    /// Where the wind blows from, degrees true.
    WindDirection,
    /// US gallons.
    FuelLeft,
    FuelRight,
    /// Engine 1, US gallons per hour.
    FuelFlow,
    /// Engine 1, °F.
    OilTemp,
    /// Engine 1, psi.
    OilPressure,
    /// Engine 1, inHg.
    Manifold,
    Rpm,
    /// Engine 1, percent of maximum RPM.
    PctRpm,
    /// Engine 1, cylinder 1, °F.
    Cht,
    Egt,
    /// Main bus 1.
    Volts,
    Amps,
    /// 1 engaged, 0 off.
    Autopilot,
    /// 0 retracted to 1 extended.
    Gear,
    ParkingBrake,
    NavLights,
    BeaconLights,
    StrobeLights,
    TaxiLights,
    LandingLights,
    /// MHz.
    Com1,
    Com2,
    Nav1,
    Nav2,
}

const FIELDS: usize = Field::ALL.len();

impl Field {
    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub sim_time: f64,
    pub wall_time_ms: i64,
    /// See [`butterlog_wire::flags`].
    pub flags: u8,
    pub events: Vec<Event>,
    values: [Option<f64>; FIELDS],
}

impl Sample {
    pub fn get(&self, field: Field) -> Option<f64> {
        self.values[field.index()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sim {
    Msfs,
    XPlane,
}

impl Sim {
    fn of(header: &SessionHeader) -> Option<Self> {
        if header.sim.starts_with("msfs") {
            Some(Self::Msfs)
        } else if header.sim.starts_with("xplane") {
            Some(Self::XPlane)
        } else {
            None
        }
    }
}

const FT_PER_M: f64 = 3.28084;
const KT_PER_MS: f64 = 1.943_84;
const FPM_PER_MS: f64 = 196.850;
const LB_PER_KG: f64 = 2.204_62;
/// US gallons per pound of avgas, as the app's X-Plane logs assume.
const GAL_PER_LB: f64 = 0.1498;

fn f_from_c(c: f64) -> f64 {
    c * 1.8 + 32.0
}

fn gal_from_kg(kg: f64) -> f64 {
    kg * LB_PER_KG * GAL_PER_LB
}

/// One raw channel's place in the canonical model.
struct Mapping {
    field: Field,
    /// The unit the channel must be in, when its table names one.
    unit: &'static str,
    convert: fn(f64) -> f64,
}

fn same(v: f64) -> f64 {
    v
}

fn negated(v: f64) -> f64 {
    -v
}

fn flag(v: f64) -> f64 {
    if v > 0.5 { 1.0 } else { 0.0 }
}

/// SimVars in the units the app requests them in (`telemetry/msfs.rs`).
fn msfs(name: &str) -> Option<Mapping> {
    use Field::*;
    let m = |field, unit, convert| Some(Mapping { field, unit, convert });
    match name {
        "PLANE LATITUDE" => m(Latitude, "degrees", same),
        "PLANE LONGITUDE" => m(Longitude, "degrees", same),
        "PLANE ALTITUDE" => m(AltitudeMsl, "feet", same),
        "INDICATED ALTITUDE" => m(AltitudeIndicated, "feet", same),
        "PLANE ALT ABOVE GROUND" => m(AltitudeAgl, "feet", same),
        "PRESSURE ALTITUDE" => m(AltitudePressure, "feet", same),
        "KOHLSMAN SETTING HG" => m(Baro, "inHg", same),
        "AIRSPEED INDICATED" => m(Ias, "knots", same),
        "AIRSPEED TRUE" => m(Tas, "knots", same),
        "GROUND VELOCITY" => m(GroundSpeed, "knots", same),
        "VERTICAL SPEED" => m(VerticalSpeed, "feet per minute", same),
        "PLANE PITCH DEGREES" => m(Pitch, "degrees", negated),
        "PLANE BANK DEGREES" => m(Roll, "degrees", negated),
        "PLANE HEADING DEGREES TRUE" => m(HeadingTrue, "degrees", same),
        "GPS GROUND TRUE TRACK" => m(Track, "degrees", same),
        "MAGVAR" => m(MagneticVariation, "degrees", same),
        "G FORCE" => m(GNormal, "gforce", same),
        "ACCELERATION BODY X" => m(GLateral, "G force", same),
        "SIM ON GROUND" => m(OnGround, "bool", flag),
        "AMBIENT TEMPERATURE" => m(Oat, "celsius", same),
        "AMBIENT WIND VELOCITY" => m(WindSpeed, "knots", same),
        "AMBIENT WIND DIRECTION" => m(WindDirection, "degrees", same),
        "FUEL LEFT QUANTITY" => m(FuelLeft, "gallons", same),
        "FUEL RIGHT QUANTITY" => m(FuelRight, "gallons", same),
        "ENG FUEL FLOW GPH:1" => m(FuelFlow, "gallons per hour", same),
        "ENG OIL TEMPERATURE:1" => m(OilTemp, "farenheit", same),
        "ENG OIL PRESSURE:1" => m(OilPressure, "psi", same),
        "ENG MANIFOLD PRESSURE:1" => m(Manifold, "inHg", same),
        "GENERAL ENG RPM:1" => m(Rpm, "rpm", same),
        "GENERAL ENG PCT MAX RPM:1" => m(PctRpm, "percent", same),
        "ENG CYLINDER HEAD TEMPERATURE:1" => m(Cht, "farenheit", same),
        "ENG EXHAUST GAS TEMPERATURE:1" => m(Egt, "farenheit", same),
        "ELECTRICAL MAIN BUS VOLTAGE:1" => m(Volts, "volts", same),
        "ELECTRICAL MAIN BUS AMPS:1" => m(Amps, "amps", same),
        "AUTOPILOT MASTER" => m(Autopilot, "bool", flag),
        "GEAR TOTAL PCT EXTENDED" => m(Gear, "percent over 100", same),
        "BRAKE PARKING POSITION" => m(ParkingBrake, "bool", flag),
        "LIGHT NAV" => m(NavLights, "bool", flag),
        "LIGHT BEACON" => m(BeaconLights, "bool", flag),
        "LIGHT STROBE" => m(StrobeLights, "bool", flag),
        "LIGHT TAXI" => m(TaxiLights, "bool", flag),
        "LIGHT LANDING" => m(LandingLights, "bool", flag),
        "COM ACTIVE FREQUENCY:1" => m(Com1, "MHz", same),
        "COM ACTIVE FREQUENCY:2" => m(Com2, "MHz", same),
        "NAV ACTIVE FREQUENCY:1" => m(Nav1, "MHz", same),
        "NAV ACTIVE FREQUENCY:2" => m(Nav2, "MHz", same),
        _ => None,
    }
}

/// Datarefs, whose units are fixed by X-Plane; arrays by element, engine and bus 1 only.
fn xplane(name: &str) -> Option<Mapping> {
    use Field::*;
    let m = |field, convert| Some(Mapping { field, unit: "", convert });
    let name = name.strip_prefix("sim/")?;
    match name {
        "flightmodel/position/latitude" => m(Latitude, same),
        "flightmodel/position/longitude" => m(Longitude, same),
        "flightmodel/position/elevation" => m(AltitudeMsl, |v| v * FT_PER_M),
        "cockpit2/gauges/indicators/altitude_ft_pilot" => m(AltitudeIndicated, same),
        "flightmodel/position/y_agl" => m(AltitudeAgl, |v| v * FT_PER_M),
        "flightmodel2/position/pressure_altitude" => m(AltitudePressure, same),
        "cockpit/misc/barometer_setting" => m(Baro, same),
        "flightmodel/position/indicated_airspeed" => m(Ias, same),
        "flightmodel/position/true_airspeed" => m(Tas, |v| v * KT_PER_MS),
        "flightmodel/position/groundspeed" => m(GroundSpeed, |v| v * KT_PER_MS),
        "flightmodel/position/vh_ind" => m(VerticalSpeed, |v| v * FPM_PER_MS),
        "flightmodel/position/theta" => m(Pitch, same),
        "flightmodel/position/phi" => m(Roll, same),
        "flightmodel/position/psi" => m(HeadingTrue, same),
        "flightmodel/position/mag_psi" => m(HeadingMagnetic, same),
        "flightmodel/position/hpath" => m(Track, same),
        "flightmodel/position/magnetic_variation" => m(MagneticVariation, negated),
        "flightmodel/forces/g_nrml" => m(GNormal, same),
        "flightmodel/forces/g_side" => m(GLateral, same),
        "flightmodel/failures/onground_any" => m(OnGround, flag),
        "weather/temperature_ambient_c" => m(Oat, same),
        "weather/wind_speed_kt" => m(WindSpeed, same),
        "weather/wind_direction_degt" => m(WindDirection, same),
        "cockpit2/fuel/fuel_quantity[0]" => m(FuelLeft, gal_from_kg),
        "cockpit2/fuel/fuel_quantity[1]" => m(FuelRight, gal_from_kg),
        "cockpit2/engine/indicators/fuel_flow_kg_sec[0]" => m(FuelFlow, |v| gal_from_kg(v) * 3600.0),
        "cockpit2/engine/indicators/oil_temp_deg_C[0]" => m(OilTemp, f_from_c),
        "cockpit2/engine/indicators/oil_pressure_psi[0]" => m(OilPressure, same),
        "cockpit2/engine/indicators/MP_in_hg[0]" => m(Manifold, same),
        "cockpit2/engine/indicators/engine_speed_rpm[0]" => m(Rpm, same),
        "cockpit2/engine/indicators/CHT_deg_C[0]" => m(Cht, f_from_c),
        "cockpit2/engine/indicators/EGT_deg_C[0]" => m(Egt, f_from_c),
        "cockpit2/electrical/bus_volts[0]" => m(Volts, same),
        "cockpit2/electrical/generator_amps[0]" => m(Amps, same),
        "cockpit2/autopilot/autopilot_on" => m(Autopilot, flag),
        "aircraft/parts/acf_gear_deploy[0]" => m(Gear, same),
        "cockpit2/controls/parking_brake_ratio" => m(ParkingBrake, flag),
        "cockpit2/switches/navigation_lights_on" => m(NavLights, flag),
        "cockpit2/switches/beacon_on" => m(BeaconLights, flag),
        "cockpit2/switches/strobe_lights_on" => m(StrobeLights, flag),
        "cockpit2/switches/taxi_light_on" => m(TaxiLights, flag),
        "cockpit2/switches/landing_lights_on" => m(LandingLights, flag),
        "cockpit2/radios/actuators/com1_frequency_hz_833" => m(Com1, |v| v / 1000.0),
        "cockpit2/radios/actuators/com2_frequency_hz_833" => m(Com2, |v| v / 1000.0),
        "cockpit2/radios/actuators/nav1_frequency_hz" => m(Nav1, |v| v / 100.0),
        "cockpit2/radios/actuators/nav2_frequency_hz" => m(Nav2, |v| v / 100.0),
        _ => None,
    }
}

/// Where each channel id goes, for one session's table. Channels the normaliser doesn't
/// know, or in a unit other than the one it expects, are left out.
fn mappings(sim: Sim, channels: &[Channel]) -> Vec<(u16, Mapping)> {
    channels
        .iter()
        .filter_map(|c| {
            let mapping = match sim {
                Sim::Msfs => msfs(&c.name),
                Sim::XPlane => xplane(&c.name),
            }?;
            if !c.unit.is_empty() && !mapping.unit.is_empty() && c.unit != mapping.unit {
                tracing::warn!(channel = %c.name, unit = %c.unit, "unexpected unit; channel left out");
                return None;
            }
            Some((c.id, mapping))
        })
        .collect()
}

fn wrap_degrees(d: f64) -> f64 {
    d.rem_euclid(360.0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormaliseError {
    Decode(DecodeError),
    Decompress(String),
    UnknownSim(String),
}

impl fmt::Display for NormaliseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "{e}"),
            Self::Decompress(e) => write!(f, "segment does not decompress: {e}"),
            Self::UnknownSim(sim) => write!(f, "no normaliser for sim {sim:?}"),
        }
    }
}

impl std::error::Error for NormaliseError {}

impl From<DecodeError> for NormaliseError {
    fn from(e: DecodeError) -> Self {
        Self::Decode(e)
    }
}

/// One canonical sample per tick, in order.
pub fn normalise(
    open: &SessionOpen,
    ticks: impl IntoIterator<Item = Tick>,
) -> Result<Vec<Sample>, NormaliseError> {
    let sim =
        Sim::of(&open.header).ok_or_else(|| NormaliseError::UnknownSim(open.header.sim.clone()))?;
    let channels: std::collections::HashMap<u16, &Channel> =
        open.channels.iter().map(|c| (c.id, c)).collect();
    let mappings: std::collections::HashMap<u16, Mapping> =
        mappings(sim, &open.channels).into_iter().collect();
    let mut state = [None; FIELDS];
    let mut samples = Vec::new();
    for tick in ticks {
        for (id, value) in &tick.values {
            let (Some(mapping), Value::Int(q)) = (mappings.get(id), value) else {
                continue;
            };
            state[mapping.field.index()] = Some((mapping.convert)(channels[id].dequantise(*q)));
        }
        let mut values = state;
        derive_headings(&mut values);
        samples.push(Sample {
            sim_time: tick.sim_time,
            wall_time_ms: tick.wall_time_ms,
            flags: tick.flags,
            events: tick.events,
            values,
        });
    }
    Ok(samples)
}

/// Fills a true or magnetic heading the sim doesn't report from the other one and the
/// variation.
fn derive_headings(values: &mut [Option<f64>; FIELDS]) {
    let get = |f: Field| values[f.index()];
    let variation = get(Field::MagneticVariation);
    match (get(Field::HeadingTrue), get(Field::HeadingMagnetic), variation) {
        (None, Some(magnetic), Some(v)) => {
            values[Field::HeadingTrue.index()] = Some(wrap_degrees(magnetic + v));
        }
        (Some(heading), None, Some(v)) => {
            values[Field::HeadingMagnetic.index()] = Some(wrap_degrees(heading - v));
        }
        _ => {}
    }
}

/// A compacted session as the service stores it (`sessions/{user}/{id}.blsg.zst`).
pub fn normalise_segment(compressed: &[u8]) -> Result<(SessionHeader, Vec<Sample>), NormaliseError> {
    let raw = decompress(compressed).map_err(|e| NormaliseError::Decompress(e.to_string()))?;
    let segment = decode_segment(&raw)?;
    let (open, _) = SessionOpen::decode(&segment.open)?;
    let mut decoder = SessionDecoder::new(open.channels.clone());
    let mut ticks = Vec::new();
    for (_, payload) in &segment.chunks {
        ticks.extend(decoder.decode(payload)?.ticks);
    }
    let samples = normalise(&open, ticks)?;
    Ok((open.header, samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> (SessionHeader, Vec<Sample>) {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        normalise_segment(&std::fs::read(&path).unwrap()).unwrap()
    }

    fn median(mut xs: Vec<f64>) -> f64 {
        assert!(!xs.is_empty());
        xs.sort_by(f64::total_cmp);
        xs[xs.len() / 2]
    }

    fn angle_diff(a: f64, b: f64) -> f64 {
        ((a - b + 540.0).rem_euclid(360.0) - 180.0).abs()
    }

    fn airborne(s: &Sample) -> bool {
        s.get(Field::OnGround) == Some(0.0)
    }

    /// Fraction of airborne frames banked beyond 15° whose heading moves the way the bank
    /// says: right wing down, heading increasing.
    fn bank_agrees_with_turn(samples: &[Sample]) -> f64 {
        let mut agree = (0, 0);
        for w in samples.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            let dt = b.sim_time - a.sim_time;
            let (Some(roll), Some(h0), Some(h1)) =
                (a.get(Field::Roll), a.get(Field::HeadingTrue), b.get(Field::HeadingTrue))
            else {
                continue;
            };
            if !airborne(a) || roll.abs() < 15.0 || !(0.0..0.5).contains(&dt) || dt == 0.0 {
                continue;
            }
            // Ticks carry only what changed: heading may not have moved between two.
            let turn = (h1 - h0 + 540.0).rem_euclid(360.0) - 180.0;
            if turn == 0.0 {
                continue;
            }
            agree.1 += 1;
            if roll * turn > 0.0 {
                agree.0 += 1;
            }
        }
        assert!(agree.1 > 100, "too few banked frames");
        agree.0 as f64 / agree.1 as f64
    }

    /// Both captures: a traffic pattern in a single-engine piston.
    fn check_pattern(samples: &[Sample]) {
        let climbing: Vec<&Sample> = samples
            .iter()
            .filter(|s| airborne(s) && s.get(Field::VerticalSpeed).is_some_and(|v| v > 500.0))
            .collect();
        assert!(climbing.len() > 100);
        let pitch = median(climbing.iter().filter_map(|s| s.get(Field::Pitch)).collect());
        assert!((2.0..20.0).contains(&pitch), "climbing pitch {pitch}");
        let agree = bank_agrees_with_turn(samples);
        assert!(agree > 0.95, "bank agrees with the turn {agree}");

        // Low and slow: true airspeed within a few percent of indicated, and heading
        // true within a few degrees of the track.
        let ias = median(climbing.iter().filter_map(|s| s.get(Field::Ias)).collect());
        let tas = median(climbing.iter().filter_map(|s| s.get(Field::Tas)).collect());
        assert!((1.0..1.15).contains(&(tas / ias)), "TAS {tas} for IAS {ias}");
        let off_track = median(
            climbing
                .iter()
                .filter_map(|s| Some(angle_diff(s.get(Field::HeadingTrue)?, s.get(Field::Track)?)))
                .collect(),
        );
        assert!(off_track < 5.0, "heading {off_track}° off the track");

        let last = samples.last().unwrap();
        let com1 = last.get(Field::Com1).unwrap();
        assert!((118.0..137.0).contains(&com1), "COM1 {com1}");
        let nav1 = last.get(Field::Nav1).unwrap();
        assert!((108.0..118.0).contains(&nav1), "NAV1 {nav1}");
        let gal = last.get(Field::FuelLeft).unwrap();
        assert!((1.0..60.0).contains(&gal), "left tank {gal} gal");
        assert!(samples.iter().any(|s| s.get(Field::OnGround) == Some(1.0)));
        let variation = last.get(Field::MagneticVariation).unwrap();
        assert!((10.0..20.0).contains(&variation), "variation {variation}, east in the US northwest");
    }

    #[test]
    fn msfs_pattern() {
        let (header, samples) = fixture("msfs_pattern.blsg.zst");
        assert_eq!(header.sim, "msfs2024");
        check_pattern(&samples);
    }

    #[test]
    fn xplane_pattern() {
        let (header, samples) = fixture("xplane_pattern.blsg.zst");
        assert_eq!(header.sim, "xplane12");
        check_pattern(&samples);
        // True heading comes from the magnetic one and the variation.
        let s = samples.iter().rev().find(|s| s.get(Field::HeadingMagnetic).is_some()).unwrap();
        let expected = s.get(Field::HeadingMagnetic).unwrap() + s.get(Field::MagneticVariation).unwrap();
        assert!(angle_diff(s.get(Field::HeadingTrue).unwrap(), expected) < 1e-6);
    }

    #[test]
    fn fields_are_unknown_until_reported() {
        let open = SessionOpen {
            header: SessionHeader { sim: "msfs2024".into(), ..Default::default() },
            channels: vec![Channel {
                id: 0,
                name: "PLANE PITCH DEGREES".into(),
                unit: "degrees".into(),
                kind: butterlog_wire::Kind::F64,
                quantum: 0.01,
                rate_class: butterlog_wire::RateClass::Frame,
                delta: butterlog_wire::DeltaOrder::Delta,
            }],
        };
        let tick = |t: f64, values| Tick { sim_time: t, wall_time_ms: 0, flags: 0, values, events: vec![] };
        let samples = normalise(
            &open,
            [tick(0.0, vec![]), tick(1.0, vec![(0, Value::Int(-500))]), tick(2.0, vec![])],
        )
        .unwrap();
        assert_eq!(samples[0].get(Field::Pitch), None);
        assert_eq!(samples[1].get(Field::Pitch), Some(5.0));
        assert_eq!(samples[2].get(Field::Pitch), Some(5.0), "carried forward");
        assert_eq!(samples[2].get(Field::Latitude), None);
    }

    #[test]
    fn a_unit_other_than_expected_is_left_out() {
        let open = SessionOpen {
            header: SessionHeader { sim: "msfs2020".into(), ..Default::default() },
            channels: vec![Channel {
                id: 0,
                name: "PLANE ALTITUDE".into(),
                unit: "meters".into(),
                kind: butterlog_wire::Kind::F64,
                quantum: 0.1,
                rate_class: butterlog_wire::RateClass::Frame,
                delta: butterlog_wire::DeltaOrder::Delta,
            }],
        };
        let tick = Tick { sim_time: 0.0, wall_time_ms: 0, flags: 0, values: vec![(0, Value::Int(10))], events: vec![] };
        assert_eq!(normalise(&open, [tick]).unwrap()[0].get(Field::AltitudeMsl), None);
    }
}
