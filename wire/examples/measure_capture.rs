//! Replays a capture through a sampler with the per-phase rates of design §3.3 and the
//! encoder, to measure real high-rate sizes and frame timing (sim-native telemetry rollout,
//! phase 1). Captures come from the X-Plane plugin's `capture-spike` feature (dataref
//! names) or the app's `msfs_capture` example (SimVar names).
//!
//! ```text
//! cargo run --release --example measure_capture -- <capture_*.blc> [--chunk-secs 5]
//!     [--keyframe-secs 60] [--level 3] [--low-hz 30] [--quanta-scale 1]
//!     [--segment <out.blsg.zst>]
//! ```
//!
//! `--segment` also writes the session as the service stores it once compacted, for tests.

use butterlog_wire::{
    compress, encode_segment, flags, Aircraft, Channel, DeltaOrder, Kind, Phase, PhaseInputs,
    RateClass, Sampler, SessionDecoder, SessionEncoder, SessionHeader, SessionOpen,
    PROTOCOL_VERSION,
};
use std::collections::BTreeMap;

fn is_xplane(name: &str) -> bool {
    name.starts_with("sim/")
}

/// Quantum and delta order per channel, in the sim's own units.
fn spec(name: &str) -> (f64, DeltaOrder) {
    if is_discrete(name) {
        (1.0, DeltaOrder::Delta)
    } else if is_xplane(name) {
        xplane_spec(leaf(name))
    } else {
        msfs_spec(name)
    }
}

fn xplane_spec(leaf: &str) -> (f64, DeltaOrder) {
    use DeltaOrder::{Delta, DeltaOfDelta};
    match leaf {
        "latitude" | "longitude" => (1e-7, DeltaOfDelta),
        "elevation" | "ellipsoid_height" | "y_agl" => (0.03, DeltaOfDelta),
        "altitude_ft_pilot" => (0.1, DeltaOfDelta),
        "pressure_altitude" | "cabin_altitude_ft" => (1.0, DeltaOfDelta),
        "theta" | "phi" | "mag_psi" | "hpath" => (0.01, DeltaOfDelta),
        "vh_ind" => (0.005, Delta),
        "g_nrml" | "g_side" => (0.001, Delta),
        "groundspeed" => (0.05, Delta),
        "indicated_airspeed" | "true_airspeed" | "wind_speed_kt" => (0.1, Delta),
        "fuel_quantity" => (0.1, Delta),
        "fuel_flow_kg_sec" => (0.0001, Delta),
        "barometer_setting" | "MP_in_hg" | "parking_brake_ratio" | "acf_gear_deploy" => {
            (0.01, Delta)
        }
        "nav1_hdef_dots_pilot" | "nav1_vdef_dots_pilot" | "gps_dme_distance_nm" => (0.01, Delta),
        "engine_speed_rpm" | "ENGHP" | "vvi_dial_fpm" | "wind_direction_degt" => (1.0, Delta),
        _ => (0.1, Delta),
    }
}

/// In the units `msfs_capture` requests them in.
fn msfs_spec(name: &str) -> (f64, DeltaOrder) {
    use DeltaOrder::{Delta, DeltaOfDelta};
    match name.split(':').next().unwrap_or(name) {
        "PLANE LATITUDE" | "PLANE LONGITUDE" => (1e-7, DeltaOfDelta),
        "INDICATED ALTITUDE" | "PLANE ALTITUDE" | "GPS POSITION ALT" | "PLANE ALT ABOVE GROUND" => {
            (0.1, DeltaOfDelta)
        }
        "PRESSURE ALTITUDE" | "DENSITY ALTITUDE" | "PRESSURIZATION CABIN ALTITUDE" => {
            (1.0, DeltaOfDelta)
        }
        "PLANE PITCH DEGREES" | "PLANE BANK DEGREES" | "PLANE HEADING DEGREES TRUE"
        | "GPS GROUND TRUE TRACK" => (0.01, DeltaOfDelta),
        "VERTICAL SPEED" | "AUTOPILOT VERTICAL HOLD VAR" | "GENERAL ENG RPM" => (1.0, Delta),
        "ACCELERATION BODY X" | "ACCELERATION BODY Z" | "G FORCE" => (0.001, Delta),
        "NAV ACTIVE FREQUENCY" | "COM ACTIVE FREQUENCY" => (0.001, Delta),
        "KOHLSMAN SETTING HG" | "ENG MANIFOLD PRESSURE" | "FUEL LEFT QUANTITY"
        | "FUEL RIGHT QUANTITY" | "GEAR TOTAL PCT EXTENDED" | "GPS WP DISTANCE" => (0.01, Delta),
        "NAV CDI" | "NAV GSI" | "AMBIENT WIND DIRECTION" => (1.0, Delta),
        _ => (0.1, Delta),
    }
}

/// "sim/cockpit2/fuel/fuel_quantity[1]" -> "fuel_quantity".
fn leaf(name: &str) -> &str {
    let base = name.split('[').next().unwrap_or(name);
    base.rsplit('/').next().unwrap_or(base)
}

/// Switches, modes and state: sampled every frame, sent on change.
fn is_discrete(name: &str) -> bool {
    if !is_xplane(name) {
        return name.starts_with("LIGHT ")
            || matches!(
                name,
                "AUTOPILOT MASTER" | "AUTOPILOT BANK HOLD" | "AUTOPILOT PITCH HOLD"
                    | "SIM ON GROUND" | "GPS DRIVES NAV1" | "BRAKE PARKING POSITION"
                    | "IS SLEW ACTIVE" | "SIMULATION RATE" | "paused"
            );
    }
    let leaf = leaf(name);
    leaf.ends_with("_on")
        || leaf.ends_with("_status")
        || leaf.ends_with("_hz")
        || leaf.ends_with("_hz_833")
        || matches!(
            leaf,
            "onground_any" | "HSI_source_select_pilot" | "paused" | "replay_mode" | "sim_speed"
                | "ground_speed"
        )
}

/// Position, attitude, VS, G, AGL and speeds sample at the per-phase frame rate; engines,
/// fuel and electrics slowly; radios, autopilot, lights, gear and state every frame, sent
/// on change.
fn rate_class(name: &str) -> RateClass {
    if is_discrete(name) {
        return RateClass::OnChange;
    }
    let (flight, systems) = if is_xplane(name) {
        (
            name.contains("/position/") || name.contains("/forces/")
                || name.contains("altitude_ft_pilot"),
            name.contains("/engine") || name.contains("/fuel/") || name.contains("/electrical/"),
        )
    } else {
        (
            name.starts_with("PLANE ") || name.starts_with("AIRSPEED ")
                || name.starts_with("ACCELERATION ")
                || matches!(
                    name,
                    "INDICATED ALTITUDE" | "GROUND VELOCITY" | "VERTICAL SPEED" | "G FORCE"
                        | "GPS GROUND TRUE TRACK" | "GPS POSITION ALT" | "PRESSURE ALTITUDE"
                ),
            name.starts_with("ENG ") || name.starts_with("GENERAL ENG ")
                || name.starts_with("FUEL ") || name.starts_with("ELECTRICAL "),
        )
    };
    if flight {
        RateClass::Frame
    } else if systems {
        RateClass::Slow
    } else {
        RateClass::OnChange
    }
}

fn phase_inputs(col: impl Fn(&str) -> Option<usize>) -> PhaseInputs {
    let id = |name: &str| col(name).unwrap_or_else(|| panic!("{name}")) as u16;
    if col("sim/flightmodel/position/y_agl").is_some() {
        PhaseInputs::xplane(
            id("sim/flightmodel/position/y_agl"),
            id("sim/flightmodel/failures/onground_any"),
            id("sim/flightmodel/position/vh_ind"),
            id("sim/flightmodel/position/phi"),
        )
    } else {
        PhaseInputs::msfs(
            id("PLANE ALT ABOVE GROUND"),
            id("SIM ON GROUND"),
            id("VERTICAL SPEED"),
            id("PLANE BANK DEGREES"),
        )
    }
}

struct Args {
    path: String,
    chunk_ms: i64,
    keyframe_ms: i64,
    level: i32,
    low_hz: f64,
    quanta_scale: f64,
    segment: Option<String>,
}

fn args() -> Args {
    let mut a = Args {
        path: String::new(),
        chunk_ms: 5_000,
        keyframe_ms: 60_000,
        level: 3,
        low_hz: 30.0,
        quanta_scale: 1.0,
        segment: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut num = || it.next().and_then(|v| v.parse::<f64>().ok()).expect("number");
        match arg.as_str() {
            "--chunk-secs" => a.chunk_ms = (num() * 1000.0) as i64,
            "--keyframe-secs" => a.keyframe_ms = (num() * 1000.0) as i64,
            "--level" => a.level = num() as i32,
            "--low-hz" => a.low_hz = num(),
            "--quanta-scale" => a.quanta_scale = num(),
            "--segment" => a.segment = it.next(),
            path => a.path = path.to_string(),
        }
    }
    assert!(!a.path.is_empty(), "usage: measure_capture <capture_*.blc> [options]");
    a
}

struct Frame {
    sim_time: f64,
    wall_ms: i64,
    counter: i64,
    values: Vec<f64>,
}

fn read_capture(path: &str) -> (Vec<String>, Vec<Frame>) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let rest = bytes.strip_prefix(b"BLCAP1\n").expect("not a capture file");
    let nl = rest.iter().position(|b| *b == b'\n').expect("header line");
    let header: serde_json::Value = serde_json::from_slice(&rest[..nl]).expect("header JSON");
    let names: Vec<String> = header["channels"]
        .as_array()
        .expect("channels")
        .iter()
        .map(|v| v.as_str().expect("channel name").to_string())
        .collect();
    let record = 24 + 8 * names.len();
    let f64_at = |r: &[u8], i: usize| f64::from_le_bytes(r[i..i + 8].try_into().unwrap());
    let i64_at = |r: &[u8], i: usize| i64::from_le_bytes(r[i..i + 8].try_into().unwrap());
    // A capture cut short mid-record (X-Plane killed) just loses that record.
    let frames = rest[nl + 1..]
        .chunks_exact(record)
        .map(|r| Frame {
            sim_time: f64_at(r, 0),
            wall_ms: i64_at(r, 8),
            counter: i64_at(r, 16),
            values: (0..names.len()).map(|i| f64_at(r, 24 + 8 * i)).collect(),
        })
        .collect();
    (names, frames)
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let a = args();
    let (names, frames) = read_capture(&a.path);
    assert!(frames.len() > 2, "capture has too few frames");
    let col = |name: &str| names.iter().position(|n| n == name);
    let p = phase_inputs(col);
    let paused = col("sim/time/paused").or(col("paused"));
    let replay = col("sim/operation/prefs/replay_mode");
    let slew = col("IS SLEW ACTIVE");
    let rates = [col("sim/time/sim_speed"), col("sim/time/ground_speed"), col("SIMULATION RATE")];

    // Frame timing, from the sim clock, over unpaused frames.
    let mut dt: Vec<f64> = frames
        .windows(2)
        .filter(|w| paused.is_none_or(|p| w[1].values[p] == 0.0))
        .map(|w| (w[1].sim_time - w[0].sim_time) * 1000.0)
        .filter(|d| *d > 0.0)
        .collect();
    dt.sort_by(f64::total_cmp);
    let skipped: i64 = frames
        .windows(2)
        .map(|w| (w[1].counter - w[0].counter - 1).max(0))
        .sum();
    let wall_s = (frames.last().unwrap().wall_ms - frames[0].wall_ms) as f64 / 1000.0;
    println!(
        "{} frames over {:.0} s wall ({:.1} fps); {} flight-loop counts not captured",
        frames.len(),
        wall_s,
        frames.len() as f64 / wall_s,
        skipped
    );
    if !dt.is_empty() {
        println!(
            "frame ms (sim clock): p1 {:.1}, p50 {:.1}, p99 {:.1}, max {:.1}",
            pct(&dt, 0.01),
            pct(&dt, 0.5),
            pct(&dt, 0.99),
            pct(&dt, 1.0)
        );
    }

    let channels: Vec<Channel> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let (quantum, delta) = spec(name);
            let discrete = is_discrete(name);
            Channel {
                id: i as u16,
                name: name.clone(),
                unit: String::new(),
                kind: if discrete { Kind::Enum } else { Kind::F64 },
                quantum: if discrete { 1.0 } else { quantum * a.quanta_scale },
                rate_class: rate_class(name),
                delta,
            }
        })
        .collect();
    let mut sampler = Sampler::new(&channels, p).with_low_hz(a.low_hz);
    let mut enc = SessionEncoder::new(channels.clone(), a.keyframe_ms);
    let mut chunks = Vec::new();
    let mut chunk_phase = Vec::new();
    let mut phase_secs: BTreeMap<Phase, f64> = BTreeMap::new();
    let mut ticks = 0u64;
    let mut current_chunk_phase = None;

    for (n, f) in frames.iter().enumerate() {
        let v = &f.values;
        for (i, value) in v.iter().enumerate() {
            sampler.set(i as u16, *value);
        }
        let phase = sampler.phase();
        if let Some(next) = frames.get(n + 1) {
            *phase_secs.entry(phase).or_default() += (next.wall_ms - f.wall_ms) as f64 / 1000.0;
        }
        let mut state = 0;
        let flag = |c: Option<usize>, test: fn(f64) -> bool| c.is_some_and(|c| test(v[c]));
        if flag(paused, |x| x != 0.0) {
            state |= flags::PAUSED;
        }
        if flag(replay, |x| x != 0.0) {
            state |= flags::REPLAY;
        }
        if flag(slew, |x| x != 0.0) {
            state |= flags::SLEW;
        }
        if rates.iter().any(|r| flag(*r, |x| x != 1.0)) {
            state |= flags::SIM_RATE;
        }
        let Some(tick) = sampler.tick(f.sim_time, f.wall_ms, state) else {
            continue;
        };
        current_chunk_phase.get_or_insert(phase);
        enc.push(tick).expect("channels come from the table");
        ticks += 1;
        if enc.buffered_span_ms() >= a.chunk_ms {
            chunks.extend(enc.flush());
            chunk_phase.push(current_chunk_phase.take().unwrap());
        }
    }
    if let Some(c) = enc.flush() {
        chunks.push(c);
        chunk_phase.push(current_chunk_phase.take().unwrap_or(Phase::Cruise));
    }

    if let Some(out) = &a.segment {
        let xplane = names.iter().any(|n| is_xplane(n));
        let open = SessionOpen {
            header: SessionHeader {
                protocol_version: PROTOCOL_VERSION,
                sim: if xplane { "xplane12" } else { "msfs2024" }.into(),
                sim_version: String::new(),
                capture: if xplane { "xplane_plugin" } else { "simconnect" }.into(),
                client_version: "measure_capture".into(),
                // The file name only: the path is local.
                aircraft: Aircraft {
                    title: std::path::Path::new(&a.path)
                        .file_name()
                        .map_or(String::new(), |n| n.to_string_lossy().into_owned()),
                    ..Default::default()
                },
            },
            channels: channels.clone(),
        };
        let raw = encode_segment(
            &open.encode(),
            chunks.iter().map(|c| (c.seq, c.payload.as_slice())),
        );
        std::fs::write(out, compress(&raw, 19)).unwrap_or_else(|e| panic!("{out}: {e}"));
    }

    let mut dec = SessionDecoder::new(channels);
    let mut segment = Vec::new();
    let mut by_phase: BTreeMap<Phase, u64> = BTreeMap::new();
    for (c, phase) in chunks.iter().zip(&chunk_phase) {
        dec.decode(&c.payload).expect("chunk decodes");
        *by_phase.entry(*phase).or_default() += compress(&c.payload, a.level).len() as u64;
        segment.extend_from_slice(&c.payload);
    }
    let sent: u64 = by_phase.values().sum();
    let hours = wall_s / 3600.0;
    println!(
        "{} channels, {} ticks, {} chunks ({} s, keyframe {} s, zstd -{}), low phase at {} Hz",
        names.len(),
        ticks,
        chunks.len(),
        a.chunk_ms / 1000,
        a.keyframe_ms / 1000,
        a.level,
        a.low_hz
    );
    println!(
        "  raw capture            {:>9.1} kB",
        (frames.len() * (24 + 8 * names.len())) as f64 / 1000.0
    );
    println!(
        "  sent, zstd per chunk   {:>9.1} kB  ({:.0} kB per hour)",
        sent as f64 / 1000.0,
        sent as f64 / 1000.0 / hours
    );
    println!(
        "  compacted, zstd -19    {:>9.1} kB",
        compress(&segment, 19).len() as f64 / 1000.0
    );
    for (phase, bytes) in &by_phase {
        let secs = phase_secs.get(phase).copied().unwrap_or(0.0);
        println!(
            "  {:<10} {:>6.1} min  {:>9.1} kB  ({:.0} kB per hour in this phase)",
            format!("{phase:?}"),
            secs / 60.0,
            *bytes as f64 / 1000.0,
            if secs > 0.0 { *bytes as f64 / 1000.0 / (secs / 3600.0) } else { 0.0 }
        );
    }
}
