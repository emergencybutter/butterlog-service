//! Replays the app's per-flight SQLite logs through the encoder to measure sizes and tune
//! quanta (sim-native telemetry rollout, phase 1).
//!
//! ```text
//! cargo run --release --example measure_logs -- [flightlogs dir] [--chunk-secs 5]
//!     [--keyframe-secs 60] [--level 3] [--quanta-scale 1]
//! ```
//!
//! The logs are 1 Hz and already normalised, so this measures the codec's overhead and the
//! quanta, not high-rate capture: each `metrics` column stands in for a sim channel.

use butterlog_wire::{
    compress, Channel, ChangeFilter, DeltaOrder, Kind, RateClass, SessionDecoder, SessionEncoder,
    Tick, Value,
};
use rusqlite::{Connection, OpenFlags};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Quantum and delta order per `metrics` column, in the log's own units.
fn column_spec(name: &str) -> (Kind, f64, DeltaOrder) {
    use DeltaOrder::{Delta, DeltaOfDelta};
    match name {
        "latitude" | "longitude" => (Kind::F64, 1e-7, DeltaOfDelta),
        "indicated_altitude" | "gps_altitude_msl" | "gps_altitude_wgs84" | "altitude_agl" => {
            (Kind::F64, 0.1, DeltaOfDelta)
        }
        "pressure_altitude" | "density_altitude" | "pressurization_cabin_altitude" => {
            (Kind::F64, 1.0, DeltaOfDelta)
        }
        "pitch_angle" | "roll_angle" => (Kind::F64, 0.01, DeltaOfDelta),
        "heading" | "track" => (Kind::F64, 0.01, DeltaOfDelta),
        "vertical_speed" | "vertical_speed_target" => (Kind::F64, 1.0, Delta),
        "lateral_acceleration" | "normal_acceleration" | "gforce" => (Kind::F64, 0.001, Delta),
        "indicated_airspeed" | "ground_speed" | "true_airspeed" | "wind_speed" => {
            (Kind::F64, 0.1, Delta)
        }
        "altimeter_setting" => (Kind::F64, 0.01, Delta),
        "engine_1_rpm" | "xp_prop_rpm" => (Kind::F64, 1.0, Delta),
        "engine_1_manifold_pressure" => (Kind::F64, 0.01, Delta),
        "nav_1_frequency" | "nav_2_frequency" | "com_1_frequency" | "com_2_frequency" => {
            (Kind::F64, 0.001, Delta)
        }
        "horizontal_cdi" | "vertical_cdi" | "xp_gear_ratio" => (Kind::F64, 0.01, Delta),
        "hsi_source" | "autopilot_active" | "roll_mode" | "pitch_mode" | "is_on_ground" => {
            (Kind::Enum, 1.0, Delta)
        }
        _ => (Kind::F64, 0.1, Delta),
    }
}

/// "YYYY-MM-DD HH:MM:SS[.mmm]" as ms since the epoch. The logs are local time; only
/// differences matter here.
fn parse_ms(ts: &str) -> Option<i64> {
    let (date, time) = ts.split_once(' ')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let (hms, ms) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':').map(|p| p.parse::<i64>());
    let (hh, mm, ss) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    let ms: i64 = format!("{ms:0<3}")[..3].parse().ok()?;
    // Days from civil (Howard Hinnant).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((((days * 24 + hh) * 60 + mm) * 60 + ss) * 1000 + ms)
}

fn varint_len(v: i64) -> u64 {
    let z = ((v << 1) ^ (v >> 63)) as u64;
    (64 - z.leading_zeros() as u64).max(1).div_ceil(7)
}

#[derive(Default)]
struct ChannelCost {
    values: u64,
    bytes: u64,
}

#[derive(Default)]
struct Totals {
    flights: u64,
    hours: f64,
    rows: u64,
    sqlite: u64,
    raw_f64: u64,
    payload: u64,
    chunks: u64,
    chunk_zstd: u64,
    segment_zstd: u64,
    per_hour: Vec<f64>,
    cost: BTreeMap<String, ChannelCost>,
}

struct Args {
    dir: PathBuf,
    chunk_ms: i64,
    keyframe_ms: i64,
    level: i32,
    /// Multiplies every quantum, to see how much the quanta matter overall.
    quanta_scale: f64,
}

fn args() -> Args {
    let mut a = Args {
        dir: std::env::var_os("APPDATA")
            .map(|d| PathBuf::from(d).join("net.flyvoyager.butterlog").join("flightlogs"))
            .unwrap_or_default(),
        chunk_ms: 5_000,
        keyframe_ms: 60_000,
        level: 3,
        quanta_scale: 1.0,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut num = || it.next().and_then(|v| v.parse::<f64>().ok()).expect("number");
        match arg.as_str() {
            "--chunk-secs" => a.chunk_ms = (num() * 1000.0) as i64,
            "--keyframe-secs" => a.keyframe_ms = (num() * 1000.0) as i64,
            "--level" => a.level = num() as i32,
            "--quanta-scale" => a.quanta_scale = num(),
            dir => a.dir = PathBuf::from(dir),
        }
    }
    a
}

fn measure(path: &PathBuf, a: &Args, totals: &mut Totals) -> rusqlite::Result<()> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let names: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('metrics') WHERE name != 'timestamp'")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if names.is_empty() {
        return Ok(());
    }
    let channels: Vec<Channel> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let (kind, quantum, delta) = column_spec(name);
            Channel {
                id: i as u16,
                name: name.clone(),
                unit: String::new(),
                kind,
                quantum: quantum * a.quanta_scale,
                rate_class: RateClass::Fast,
                delta,
            }
        })
        .collect();

    let sql = format!(
        "SELECT timestamp, {} FROM metrics ORDER BY timestamp",
        names.join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut enc = SessionEncoder::new(channels.clone(), a.keyframe_ms);
    let mut filter = ChangeFilter::new();
    let mut chunks = Vec::new();
    let mut prev: Vec<(i64, i64)> = vec![(0, 0); channels.len()];
    let mut cost: Vec<ChannelCost> = channels.iter().map(|_| ChannelCost::default()).collect();
    let (mut first_ms, mut last_ms, mut n_rows) = (None, 0i64, 0u64);

    while let Some(row) = rows.next()? {
        let Some(wall) = parse_ms(&row.get::<_, String>(0)?) else { continue };
        let first = *first_ms.get_or_insert(wall);
        last_ms = wall;
        n_rows += 1;
        let mut values = Vec::new();
        for (i, ch) in channels.iter().enumerate() {
            let Some(v) = row.get::<_, Option<f64>>(i + 1)? else { continue };
            let q = ch.quantise(v);
            if filter.filter(ch.id, Value::Int(q)).is_some() {
                // Per-channel cost, as if one long column: close enough to rank channels.
                let (p, pd) = prev[i];
                let d = q.wrapping_sub(p);
                let coded = match ch.delta {
                    DeltaOrder::Delta => d,
                    DeltaOrder::DeltaOfDelta => d.wrapping_sub(pd),
                };
                prev[i] = (q, d);
                cost[i].values += 1;
                cost[i].bytes += varint_len(coded);
                values.push((ch.id, Value::Int(q)));
            }
        }
        let tick = Tick {
            sim_time: (wall - first) as f64 / 1000.0,
            wall_time_ms: wall,
            flags: 0,
            values,
        };
        enc.push(tick).expect("channels come from the table");
        if enc.buffered_span_ms() >= a.chunk_ms {
            chunks.extend(enc.flush());
        }
    }
    chunks.extend(enc.flush());

    let hours = (last_ms - first_ms.unwrap_or(last_ms)) as f64 / 3_600_000.0;
    if n_rows < 60 || hours <= 0.0 {
        return Ok(());
    }
    let mut segment = Vec::new();
    let mut chunk_zstd = 0;
    let mut dec = SessionDecoder::new(channels.clone());
    for c in &chunks {
        // Every chunk must decode: the measurement is only as good as the round trip.
        let decoded = dec.decode(&c.payload).expect("chunk decodes");
        assert_eq!(decoded.seq, c.seq);
        chunk_zstd += compress(&c.payload, a.level).len() as u64;
        segment.extend_from_slice(&c.payload);
    }
    let segment_zstd = compress(&segment, 19).len() as u64;

    totals.flights += 1;
    totals.hours += hours;
    totals.rows += n_rows;
    totals.sqlite += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    totals.raw_f64 += n_rows * channels.len() as u64 * 8;
    totals.payload += segment.len() as u64;
    totals.chunks += chunks.len() as u64;
    totals.chunk_zstd += chunk_zstd;
    totals.segment_zstd += segment_zstd;
    if hours >= 0.25 {
        totals.per_hour.push(chunk_zstd as f64 / hours);
    }
    for (ch, c) in channels.iter().zip(cost) {
        let t = totals.cost.entry(ch.name.clone()).or_default();
        t.values += c.values;
        t.bytes += c.bytes;
    }
    Ok(())
}

fn mb(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1e6)
}

fn main() {
    let a = args();
    let mut totals = Totals::default();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&a.dir)
        .unwrap_or_else(|e| panic!("{}: {e}", a.dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "db"))
        .collect();
    paths.sort();
    for p in &paths {
        if let Err(e) = measure(p, &a, &mut totals) {
            eprintln!("skipped {}: {e}", p.display());
        }
    }

    let t = &totals;
    println!(
        "{} flights, {:.1} h, {} rows ({:.2} rows/s); chunks {} s, keyframe {} s, zstd -{}",
        t.flights,
        t.hours,
        t.rows,
        t.rows as f64 / (t.hours * 3600.0),
        a.chunk_ms / 1000,
        a.keyframe_ms / 1000,
        a.level
    );
    println!("  SQLite files          {:>10}", mb(t.sqlite));
    println!("  raw f64 (rows × cols) {:>10}", mb(t.raw_f64));
    println!("  chunk payloads        {:>10}", mb(t.payload));
    println!("  chunks, zstd each     {:>10}  ({} chunks)", mb(t.chunk_zstd), t.chunks);
    println!("  segments, zstd -19    {:>10}  (compacted per session)", mb(t.segment_zstd));
    let mut per_hour = t.per_hour.clone();
    per_hour.sort_by(f64::total_cmp);
    if !per_hour.is_empty() {
        let pct = |p: f64| per_hour[((per_hour.len() - 1) as f64 * p) as usize] / 1000.0;
        println!(
            "  chunked zstd per flight-hour (flights ≥ 15 min): median {:.0} kB, p90 {:.0} kB, max {:.0} kB",
            pct(0.5),
            pct(0.9),
            pct(1.0)
        );
    }
    println!("\n  heaviest channels (estimated uncompressed value bytes, presence excluded; share of rows that changed):");
    let mut cost: Vec<_> = t.cost.iter().collect();
    cost.sort_by_key(|(_, c)| std::cmp::Reverse(c.bytes));
    for (name, c) in cost.iter().take(20) {
        println!(
            "  {:<32} {:>10}  {:>5.1}%",
            name,
            mb(c.bytes),
            100.0 * c.values as f64 / t.rows as f64
        );
    }
}
