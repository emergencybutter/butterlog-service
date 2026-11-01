//! Ticks grouped into chunks (design §3.2, §3.4).
//!
//! A chunk payload, before compression:
//!
//! ```text
//! format u8 | seq varint | keyframe u8 | base_sim_time f64 LE | base_wall_ms zigzag
//! n_ticks varint
//! sim time offsets    n_ticks × zigzag, delta-of-delta, in SIM_TIME_QUANTUM steps
//! wall time offsets   n_ticks × zigzag, delta-of-delta, in ms
//! state flags         n_runs varint, then (flags u8, run length varint) per run
//! n_columns varint, then per column, in channel id order:
//!   channel id varint | n_values << 1 | is_bitmap, varint
//!   presence          bitmap: ⌈n_ticks / 8⌉ bytes, bit i set for tick i, or
//!                     gaps:   n_values × varint, the first absolute, then gaps (≥ 1)
//!                     whichever is smaller
//!   values            numeric: n_values × zigzag, delta or delta-of-delta; the first is a
//!                     delta against the channel's last value (absolute in a keyframe)
//!                     string:  n_values × (length varint, UTF-8 bytes)
//! n_events varint, then per event, in tick order:
//!   tick index varint, the first absolute, then gaps (≥ 0) | kind u8
//!   detail            length varint, UTF-8 bytes
//! ```
//!
//! A chunk continues the previous one: each column starts as a delta against the channel's
//! last value in the session, so decoding needs every chunk since the last keyframe, in
//! order. A keyframe starts from absolute values and also carries, at its first tick, the
//! last value of every channel seen so far, so a reader starting there needs nothing older.
//! Restarting every chunk from absolute values instead would cost more than the deltas
//! themselves at low rates (rollout phase 1 measurements).

use crate::schema::{Channel, DeltaOrder, Kind};
use crate::varint::{self, Reader};
use std::collections::BTreeMap;
use std::fmt;

const FORMAT: u8 = 2;

/// Sim time is carried in 0.1 ms steps: finer than any frame, coarse enough to pack small.
const SIM_TIME_QUANTUM: f64 = 1e-4;

/// State flags carried by every tick.
pub mod flags {
    pub const PAUSED: u8 = 1;
    pub const REPLAY: u8 = 2;
    pub const SLEW: u8 = 4;
    /// Sim rate other than 1×.
    pub const SIM_RATE: u8 = 8;
}

/// A channel value as it travels: quantised for every kind but `String`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Str(String),
}

/// What kind of thing an [`Event`] is. Codes are fixed on the wire; a code this build
/// doesn't know decodes as `Other`, so a newer client's events don't fail an older reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    SimStart,
    SimStop,
    Paused,
    Unpaused,
    Crashed,
    CrashReset,
    /// Detail: the flight file loaded.
    FlightLoaded,
    /// Detail: the aircraft file loaded.
    AircraftLoaded,
    /// The aircraft was moved by hand, through a menu or a map.
    PositionChanged,
    ReplayStarted,
    ReplayEnded,
    Other(u8),
}

impl EventKind {
    const KNOWN: [EventKind; 11] = [
        Self::SimStart,
        Self::SimStop,
        Self::Paused,
        Self::Unpaused,
        Self::Crashed,
        Self::CrashReset,
        Self::FlightLoaded,
        Self::AircraftLoaded,
        Self::PositionChanged,
        Self::ReplayStarted,
        Self::ReplayEnded,
    ];

    pub fn code(self) -> u8 {
        match self {
            Self::Other(code) => code,
            known => Self::KNOWN.iter().position(|k| *k == known).unwrap() as u8 + 1,
        }
    }

    pub fn from_code(code: u8) -> Self {
        code.checked_sub(1)
            .and_then(|i| Self::KNOWN.get(usize::from(i)).copied())
            .unwrap_or(Self::Other(code))
    }
}

/// Something the sim reported happening (design §3.2), at the tick that carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub kind: EventKind,
    /// Kind-specific, often empty.
    pub detail: String,
}

impl Event {
    pub fn new(kind: EventKind) -> Self {
        Self { kind, detail: String::new() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tick {
    /// Sim clock, in seconds.
    pub sim_time: f64,
    /// UTC, in ms since the Unix epoch.
    pub wall_time_ms: i64,
    /// See [`flags`].
    pub flags: u8,
    /// Only the channels whose quantised value changed (see [`crate::ChangeFilter`]).
    pub values: Vec<(u16, Value)>,
    /// What the sim reported happening since the previous tick.
    pub events: Vec<Event>,
}

/// An uncompressed chunk payload, ready for [`compress`] and the spool.
#[derive(Debug, Clone)]
pub struct EncodedChunk {
    pub seq: u32,
    pub keyframe: bool,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    UnknownChannel(u16),
    /// A `Str` value for a numeric channel, or an `Int` for a string one.
    KindMismatch(u16),
    /// The same channel twice in one tick.
    DuplicateChannel(u16),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownChannel(id) => write!(f, "channel {id} is not in the session's table"),
            Self::KindMismatch(id) => write!(f, "value does not match the kind of channel {id}"),
            Self::DuplicateChannel(id) => write!(f, "channel {id} appears twice in one tick"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// Builds the chunks of one session. The caller pushes ticks and flushes a chunk every
/// 2–5 s of wall time (the live latency budget).
pub struct SessionEncoder {
    channels: BTreeMap<u16, Channel>,
    keyframe_interval_ms: i64,
    seq: u32,
    last_keyframe_wall_ms: Option<i64>,
    /// Last value of every channel, as of the end of the buffered ticks.
    last: BTreeMap<u16, Value>,
    /// Last value of every channel, as of the start of the buffered ticks.
    at_chunk_start: BTreeMap<u16, Value>,
    ticks: Vec<Tick>,
}

impl SessionEncoder {
    /// `keyframe_interval_ms` is how often a chunk restates every channel (design: ~60 s).
    pub fn new(channels: Vec<Channel>, keyframe_interval_ms: i64) -> Self {
        Self {
            channels: channels.into_iter().map(|c| (c.id, c)).collect(),
            keyframe_interval_ms,
            seq: 0,
            last_keyframe_wall_ms: None,
            last: BTreeMap::new(),
            at_chunk_start: BTreeMap::new(),
            ticks: Vec::new(),
        }
    }

    pub fn push(&mut self, tick: Tick) -> Result<(), EncodeError> {
        for (n, (id, value)) in tick.values.iter().enumerate() {
            let channel = self.channels.get(id).ok_or(EncodeError::UnknownChannel(*id))?;
            let is_string = channel.kind == Kind::String;
            if is_string != matches!(value, Value::Str(_)) {
                return Err(EncodeError::KindMismatch(*id));
            }
            if tick.values[..n].iter().any(|(other, _)| other == id) {
                return Err(EncodeError::DuplicateChannel(*id));
            }
        }
        if self.ticks.is_empty() {
            self.at_chunk_start = self.last.clone();
        }
        for (id, value) in &tick.values {
            self.last.insert(*id, value.clone());
        }
        self.ticks.push(tick);
        Ok(())
    }

    /// Wall time covered by the buffered ticks, to decide when to flush.
    pub fn buffered_span_ms(&self) -> i64 {
        match (self.ticks.first(), self.ticks.last()) {
            (Some(first), Some(last)) => last.wall_time_ms - first.wall_time_ms,
            _ => 0,
        }
    }

    /// Encodes the buffered ticks as the next chunk, or `None` if there are none.
    pub fn flush(&mut self) -> Option<EncodedChunk> {
        let first_wall = self.ticks.first()?.wall_time_ms;
        let keyframe = self
            .last_keyframe_wall_ms
            .is_none_or(|k| first_wall - k >= self.keyframe_interval_ms);
        let mut ticks = std::mem::take(&mut self.ticks);
        if keyframe {
            self.last_keyframe_wall_ms = Some(first_wall);
            let first = &mut ticks[0];
            for (id, value) in &self.at_chunk_start {
                if !first.values.iter().any(|(v_id, _)| v_id == id) {
                    first.values.push((*id, value.clone()));
                }
            }
        }
        let chunk = EncodedChunk {
            seq: self.seq,
            keyframe,
            payload: encode(
                self.seq,
                keyframe,
                &ticks,
                &self.channels,
                (!keyframe).then_some(&self.at_chunk_start),
            ),
        };
        self.seq += 1;
        Some(chunk)
    }
}

/// `reference` holds each channel's last value before this chunk, `None` for a keyframe.
fn encode(
    seq: u32,
    keyframe: bool,
    ticks: &[Tick],
    channels: &BTreeMap<u16, Channel>,
    reference: Option<&BTreeMap<u16, Value>>,
) -> Vec<u8> {
    let mut out = vec![FORMAT];
    varint::write_u64(&mut out, seq.into());
    out.push(keyframe.into());
    let base_sim = ticks[0].sim_time;
    let base_wall = ticks[0].wall_time_ms;
    out.extend_from_slice(&base_sim.to_le_bytes());
    varint::write_i64(&mut out, base_wall);
    varint::write_u64(&mut out, ticks.len() as u64);

    let sim: Vec<i64> = ticks
        .iter()
        .map(|t| ((t.sim_time - base_sim) / SIM_TIME_QUANTUM).round() as i64)
        .collect();
    write_differenced(&mut out, &sim, DeltaOrder::DeltaOfDelta, 0);
    let wall: Vec<i64> = ticks.iter().map(|t| t.wall_time_ms - base_wall).collect();
    write_differenced(&mut out, &wall, DeltaOrder::DeltaOfDelta, 0);

    let mut runs: Vec<(u8, u64)> = Vec::new();
    for t in ticks {
        match runs.last_mut() {
            Some((f, n)) if *f == t.flags => *n += 1,
            _ => runs.push((t.flags, 1)),
        }
    }
    varint::write_u64(&mut out, runs.len() as u64);
    for (f, n) in runs {
        out.push(f);
        varint::write_u64(&mut out, n);
    }

    let mut columns: BTreeMap<u16, Vec<(usize, &Value)>> = BTreeMap::new();
    for (i, t) in ticks.iter().enumerate() {
        for (id, value) in &t.values {
            columns.entry(*id).or_default().push((i, value));
        }
    }
    varint::write_u64(&mut out, columns.len() as u64);
    for (id, column) in columns {
        varint::write_u64(&mut out, id.into());
        let indices: Vec<usize> = column.iter().map(|(i, _)| *i).collect();
        write_presence(&mut out, &indices, ticks.len());
        // `push` checked every value against its channel's kind.
        if channels[&id].kind == Kind::String {
            for (_, value) in &column {
                if let Value::Str(s) = value {
                    varint::write_u64(&mut out, s.len() as u64);
                    out.extend_from_slice(s.as_bytes());
                }
            }
        } else {
            let ints: Vec<i64> = column
                .iter()
                .filter_map(|(_, v)| match v {
                    Value::Int(q) => Some(*q),
                    Value::Str(_) => None,
                })
                .collect();
            let base = int_reference(reference, id);
            write_differenced(&mut out, &ints, channels[&id].delta, base);
        }
    }

    let events: Vec<(usize, &Event)> =
        ticks.iter().enumerate().flat_map(|(i, t)| t.events.iter().map(move |e| (i, e))).collect();
    varint::write_u64(&mut out, events.len() as u64);
    let mut prev = 0;
    for (i, event) in events {
        varint::write_u64(&mut out, (i - prev) as u64);
        prev = i;
        out.push(event.kind.code());
        varint::write_bytes(&mut out, event.detail.as_bytes());
    }
    out
}

/// Which ticks a column has values for, as a bitmap or as index gaps, whichever is
/// smaller: a bitmap suits a channel present at most ticks, gaps one that rarely changes.
/// The header varint is the value count, shifted left, with the low bit set for a bitmap.
fn write_presence(out: &mut Vec<u8>, indices: &[usize], n_ticks: usize) {
    let mut gaps = Vec::new();
    let mut prev = 0;
    for (n, &i) in indices.iter().enumerate() {
        varint::write_u64(&mut gaps, if n == 0 { i } else { i - prev } as u64);
        prev = i;
    }
    let count = (indices.len() as u64) << 1;
    let bitmap_len = n_ticks.div_ceil(8);
    if bitmap_len < gaps.len() {
        varint::write_u64(out, count | 1);
        let mut bitmap = vec![0u8; bitmap_len];
        for &i in indices {
            bitmap[i / 8] |= 1 << (i % 8);
        }
        out.extend_from_slice(&bitmap);
    } else {
        varint::write_u64(out, count);
        out.extend_from_slice(&gaps);
    }
}

fn read_presence(r: &mut Reader, n_ticks: usize) -> Result<Vec<usize>, DecodeError> {
    use DecodeError::{BadTickIndex, Truncated};
    let header = r.u64().ok_or(Truncated)?;
    let count = usize::try_from(header >> 1).map_err(|_| BadTickIndex)?;
    if count > n_ticks {
        return Err(BadTickIndex);
    }
    let mut indices = Vec::with_capacity(count);
    if header & 1 == 1 {
        let bitmap = r.bytes(n_ticks.div_ceil(8)).ok_or(Truncated)?;
        for (byte, bits) in bitmap.iter().enumerate() {
            for bit in 0..8 {
                if bits & (1 << bit) != 0 {
                    indices.push(byte * 8 + bit);
                }
            }
        }
        if indices.len() != count || indices.last().is_some_and(|&i| i >= n_ticks) {
            return Err(BadTickIndex);
        }
    } else {
        for _ in 0..count {
            let step = usize::try_from(r.u64().ok_or(Truncated)?).map_err(|_| Truncated)?;
            let i = match indices.last() {
                None => step,
                Some(_) if step == 0 => return Err(BadTickIndex),
                Some(prev) => prev.checked_add(step).ok_or(BadTickIndex)?,
            };
            if i >= n_ticks {
                return Err(BadTickIndex);
            }
            indices.push(i);
        }
    }
    Ok(indices)
}

/// A channel's last numeric value before a chunk, which its column starts from; 0 when
/// there is none, as in a keyframe.
fn int_reference(reference: Option<&BTreeMap<u16, Value>>, id: u16) -> i64 {
    match reference.and_then(|r| r.get(&id)) {
        Some(Value::Int(q)) => *q,
        _ => 0,
    }
}

/// The first value as a delta against `base`, then deltas, or deltas of deltas. Wrapping
/// arithmetic keeps extreme values round-tripping instead of overflowing.
fn write_differenced(out: &mut Vec<u8>, xs: &[i64], order: DeltaOrder, base: i64) {
    let (mut prev, mut prev_delta) = (base, 0i64);
    for (n, &x) in xs.iter().enumerate() {
        let delta = x.wrapping_sub(prev);
        let coded = match order {
            _ if n == 0 => delta,
            DeltaOrder::Delta => delta,
            DeltaOrder::DeltaOfDelta if n == 1 => delta,
            DeltaOrder::DeltaOfDelta => delta.wrapping_sub(prev_delta),
        };
        varint::write_i64(out, coded);
        prev = x;
        prev_delta = delta;
    }
}

fn read_differenced(r: &mut Reader, n: usize, order: DeltaOrder, base: i64) -> Option<Vec<i64>> {
    let mut xs = Vec::with_capacity(n.min(4096));
    let (mut prev, mut prev_delta) = (base, 0i64);
    for i in 0..n {
        let coded = r.i64()?;
        let x = match order {
            _ if i == 0 => prev.wrapping_add(coded),
            DeltaOrder::Delta => prev.wrapping_add(coded),
            DeltaOrder::DeltaOfDelta if i == 1 => prev.wrapping_add(coded),
            DeltaOrder::DeltaOfDelta => prev.wrapping_add(prev_delta.wrapping_add(coded)),
        };
        prev_delta = x.wrapping_sub(prev);
        prev = x;
        xs.push(x);
    }
    Some(xs)
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedChunk {
    pub seq: u32,
    pub keyframe: bool,
    /// Each tick's values are in channel id order.
    pub ticks: Vec<Tick>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    UnknownFormat(u8),
    UnknownChannel(u16),
    /// Tick indices out of range or not strictly increasing.
    BadTickIndex,
    BadString,
    TrailingBytes,
    /// A field outside its allowed values, in a channel table, header or segment.
    BadValue(&'static str),
    /// A chunk other than a keyframe whose predecessor was not decoded first.
    MissingPredecessor { expected: Option<u32>, got: u32 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "chunk is truncated"),
            Self::UnknownFormat(v) => write!(f, "unknown chunk format {v}"),
            Self::UnknownChannel(id) => write!(f, "channel {id} is not in the session's table"),
            Self::BadTickIndex => write!(f, "tick indices out of range or out of order"),
            Self::BadString => write!(f, "string value is not UTF-8"),
            Self::TrailingBytes => write!(f, "bytes left after the last field"),
            Self::BadValue(what) => write!(f, "invalid {what}"),
            Self::MissingPredecessor { expected: Some(e), got } => {
                write!(f, "chunk {got} arrived while chunk {e} was expected")
            }
            Self::MissingPredecessor { expected: None, got } => {
                write!(f, "chunk {got} is not a keyframe and none came before it")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

/// A chunk's sequence number and keyframe flag, read from its first bytes without decoding
/// the rest. Lets a receiver file a chunk that arrived before its predecessors.
pub fn peek_chunk(payload: &[u8]) -> Result<(u32, bool), DecodeError> {
    use DecodeError::Truncated;
    let mut r = Reader::new(payload);
    let format = r.u8().ok_or(Truncated)?;
    if format != FORMAT {
        return Err(DecodeError::UnknownFormat(format));
    }
    let seq = u32::try_from(r.u64().ok_or(Truncated)?).map_err(|_| Truncated)?;
    Ok((seq, r.u8().ok_or(Truncated)? != 0))
}

/// Decodes the chunks of one session, in order from a keyframe. Payloads come from
/// clients, so every length is checked against the input rather than trusted, and a chunk
/// that fails to decode leaves the decoder as it was.
pub struct SessionDecoder {
    channels: BTreeMap<u16, Channel>,
    /// Last value of every channel, as of the end of the last decoded chunk.
    last: BTreeMap<u16, Value>,
    next_seq: Option<u32>,
}

impl SessionDecoder {
    pub fn new(channels: Vec<Channel>) -> Self {
        Self {
            channels: channels.into_iter().map(|c| (c.id, c)).collect(),
            last: BTreeMap::new(),
            next_seq: None,
        }
    }

    pub fn decode(&mut self, payload: &[u8]) -> Result<DecodedChunk, DecodeError> {
        let chunk = decode(payload, &self.channels, self.next_seq, &self.last)?;
        if chunk.keyframe {
            self.last.clear();
        }
        for tick in &chunk.ticks {
            for (id, value) in &tick.values {
                self.last.insert(*id, value.clone());
            }
        }
        self.next_seq = chunk.seq.checked_add(1);
        Ok(chunk)
    }
}

fn decode(
    payload: &[u8],
    channels: &BTreeMap<u16, Channel>,
    next_seq: Option<u32>,
    last: &BTreeMap<u16, Value>,
) -> Result<DecodedChunk, DecodeError> {
    use DecodeError::Truncated;
    let mut r = Reader::new(payload);
    let format = r.u8().ok_or(Truncated)?;
    if format != FORMAT {
        return Err(DecodeError::UnknownFormat(format));
    }
    let seq = u32::try_from(r.u64().ok_or(Truncated)?).map_err(|_| Truncated)?;
    let keyframe = r.u8().ok_or(Truncated)? != 0;
    if !keyframe && next_seq != Some(seq) {
        return Err(DecodeError::MissingPredecessor { expected: next_seq, got: seq });
    }
    let reference = (!keyframe).then_some(last);
    let base_sim = f64::from_le_bytes(r.bytes(8).ok_or(Truncated)?.try_into().unwrap());
    let base_wall = r.i64().ok_or(Truncated)?;
    let n = usize::try_from(r.u64().ok_or(Truncated)?).map_err(|_| Truncated)?;

    let sim = read_differenced(&mut r, n, DeltaOrder::DeltaOfDelta, 0).ok_or(Truncated)?;
    let wall = read_differenced(&mut r, n, DeltaOrder::DeltaOfDelta, 0).ok_or(Truncated)?;
    let mut ticks: Vec<Tick> = sim
        .iter()
        .zip(&wall)
        .map(|(s, w)| Tick {
            sim_time: base_sim + *s as f64 * SIM_TIME_QUANTUM,
            wall_time_ms: base_wall.wrapping_add(*w),
            flags: 0,
            values: Vec::new(),
            events: Vec::new(),
        })
        .collect();

    let n_runs = r.u64().ok_or(Truncated)?;
    let mut at = 0usize;
    for _ in 0..n_runs {
        let f = r.u8().ok_or(Truncated)?;
        let len = usize::try_from(r.u64().ok_or(Truncated)?).map_err(|_| Truncated)?;
        let end = at.checked_add(len).filter(|e| *e <= n).ok_or(DecodeError::BadTickIndex)?;
        ticks[at..end].iter_mut().for_each(|t| t.flags = f);
        at = end;
    }

    let n_columns = r.u64().ok_or(Truncated)?;
    for _ in 0..n_columns {
        let id = u16::try_from(r.u64().ok_or(Truncated)?).map_err(|_| Truncated)?;
        let channel = channels.get(&id).ok_or(DecodeError::UnknownChannel(id))?;
        let indices = read_presence(&mut r, n)?;
        let count = indices.len();
        if channel.kind == Kind::String {
            for i in indices {
                let len = usize::try_from(r.u64().ok_or(Truncated)?).map_err(|_| Truncated)?;
                let bytes = r.bytes(len).ok_or(Truncated)?;
                let s = std::str::from_utf8(bytes).map_err(|_| DecodeError::BadString)?;
                ticks[i].values.push((id, Value::Str(s.to_owned())));
            }
        } else {
            let base = int_reference(reference, id);
            let ints = read_differenced(&mut r, count, channel.delta, base).ok_or(Truncated)?;
            for (i, q) in indices.into_iter().zip(ints) {
                ticks[i].values.push((id, Value::Int(q)));
            }
        }
    }

    let n_events = r.u64().ok_or(Truncated)?;
    let mut at = 0usize;
    for _ in 0..n_events {
        let step = usize::try_from(r.u64().ok_or(Truncated)?).map_err(|_| Truncated)?;
        at = at.checked_add(step).filter(|i| *i < n).ok_or(DecodeError::BadTickIndex)?;
        let kind = EventKind::from_code(r.u8().ok_or(Truncated)?);
        let detail = std::str::from_utf8(r.len_prefixed().ok_or(Truncated)?)
            .map_err(|_| DecodeError::BadString)?;
        ticks[at].events.push(Event { kind, detail: detail.to_owned() });
    }
    if !r.is_empty() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(DecodedChunk { seq, keyframe, ticks })
}

/// Chunks are zstd-compressed as a whole for the spool and the wire.
#[cfg(feature = "zstd")]
pub fn compress(payload: &[u8], level: i32) -> Vec<u8> {
    zstd::bulk::compress(payload, level).expect("zstd compression of an in-memory buffer")
}

/// Largest decompressed chunk accepted; a 5 s chunk at 30 Hz is a small fraction of this.
#[cfg(feature = "zstd")]
const MAX_PAYLOAD: usize = 16 << 20;

#[cfg(feature = "zstd")]
pub fn decompress(compressed: &[u8]) -> std::io::Result<Vec<u8>> {
    zstd::bulk::decompress(compressed, MAX_PAYLOAD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::RateClass;

    fn channel(id: u16, kind: Kind, delta: DeltaOrder) -> Channel {
        Channel {
            id,
            name: format!("CHANNEL {id}"),
            unit: "unit".into(),
            kind,
            quantum: 0.1,
            rate_class: RateClass::Frame,
            delta,
        }
    }

    fn channels() -> Vec<Channel> {
        vec![
            channel(1, Kind::F64, DeltaOrder::DeltaOfDelta),
            channel(2, Kind::I64, DeltaOrder::Delta),
            channel(3, Kind::String, DeltaOrder::Delta),
        ]
    }

    fn tick(i: i64, values: Vec<(u16, Value)>) -> Tick {
        Tick {
            sim_time: 1000.0 + i as f64 / 30.0,
            wall_time_ms: 1_790_000_000_000 + i * 33,
            flags: if i == 2 { flags::PAUSED } else { 0 },
            values,
            events: Vec::new(),
        }
    }

    fn sorted(mut values: Vec<(u16, Value)>) -> Vec<(u16, Value)> {
        values.sort_by_key(|(id, _)| *id);
        values
    }

    #[test]
    fn round_trips_ticks() {
        let mut enc = SessionEncoder::new(channels(), 60_000);
        let input = vec![
            tick(0, vec![(1, Value::Int(100)), (3, Value::Str("A320".into()))]),
            tick(1, vec![(1, Value::Int(103)), (2, Value::Int(-7))]),
            tick(2, vec![]),
            tick(3, vec![(1, Value::Int(i64::MIN)), (2, Value::Int(i64::MAX))]),
        ];
        for t in input.clone() {
            enc.push(t).unwrap();
        }
        let chunk = enc.flush().unwrap();
        let decoded = SessionDecoder::new(channels()).decode(&chunk.payload).unwrap();
        assert_eq!(decoded.seq, 0);
        assert!(decoded.keyframe);
        for (got, want) in decoded.ticks.iter().zip(&input) {
            assert!((got.sim_time - want.sim_time).abs() < SIM_TIME_QUANTUM);
            assert_eq!(got.wall_time_ms, want.wall_time_ms);
            assert_eq!(got.flags, want.flags);
            assert_eq!(got.values, sorted(want.values.clone()));
        }
    }

    #[test]
    fn keyframes_restate_every_channel() {
        let mut enc = SessionEncoder::new(channels(), 60_000);
        let mut dec = SessionDecoder::new(channels());
        enc.push(tick(0, vec![(1, Value::Int(1)), (2, Value::Int(2))])).unwrap();
        let chunk = enc.flush().unwrap();
        assert!(chunk.keyframe);
        dec.decode(&chunk.payload).unwrap();

        // Not yet due: only what changed.
        enc.push(tick(30, vec![(1, Value::Int(5))])).unwrap();
        let chunk = enc.flush().unwrap();
        assert!(!chunk.keyframe);
        let decoded = dec.decode(&chunk.payload).unwrap();
        assert_eq!(decoded.ticks[0].values, vec![(1, Value::Int(5))]);

        // Due: channel 2 is restated at the chunk's first tick, channel 1 keeps its new value,
        // and a fresh reader can start here.
        enc.push(tick(2000, vec![(1, Value::Int(6))])).unwrap();
        let chunk = enc.flush().unwrap();
        assert!(chunk.keyframe);
        assert_eq!(chunk.seq, 2);
        let decoded = SessionDecoder::new(channels()).decode(&chunk.payload).unwrap();
        assert_eq!(decoded.ticks[0].values, vec![(1, Value::Int(6)), (2, Value::Int(2))]);
    }

    #[test]
    fn chunks_continue_from_the_previous_one() {
        let mut enc = SessionEncoder::new(channels(), 60_000);
        let mut chunks = Vec::new();
        for i in 0..6 {
            enc.push(tick(i, vec![(1, Value::Int(1_000_000 + i)), (2, Value::Int(-i))])).unwrap();
            chunks.push(enc.flush().unwrap());
        }

        let mut dec = SessionDecoder::new(channels());
        for (i, chunk) in chunks.iter().enumerate() {
            let decoded = dec.decode(&chunk.payload).unwrap();
            let i = i as i64;
            assert_eq!(
                decoded.ticks[0].values,
                vec![(1, Value::Int(1_000_000 + i)), (2, Value::Int(-i))]
            );
        }

        // Starting mid-session, or skipping a chunk, is refused rather than decoded wrong.
        assert_eq!(
            SessionDecoder::new(channels()).decode(&chunks[3].payload),
            Err(DecodeError::MissingPredecessor { expected: None, got: 3 })
        );
        let mut dec = SessionDecoder::new(channels());
        dec.decode(&chunks[0].payload).unwrap();
        assert_eq!(
            dec.decode(&chunks[2].payload),
            Err(DecodeError::MissingPredecessor { expected: Some(1), got: 2 })
        );
        // A refused chunk leaves the decoder where it was.
        dec.decode(&chunks[1].payload).unwrap();
    }

    #[test]
    fn rejects_values_that_do_not_fit_the_table() {
        let mut enc = SessionEncoder::new(channels(), 60_000);
        assert_eq!(
            enc.push(tick(0, vec![(9, Value::Int(1))])),
            Err(EncodeError::UnknownChannel(9))
        );
        assert_eq!(
            enc.push(tick(0, vec![(3, Value::Int(1))])),
            Err(EncodeError::KindMismatch(3))
        );
        assert_eq!(
            enc.push(tick(0, vec![(1, Value::Int(1)), (1, Value::Int(2))])),
            Err(EncodeError::DuplicateChannel(1))
        );
        assert!(enc.flush().is_none());
    }

    #[test]
    fn dense_and_sparse_columns_round_trip() {
        // 150 ticks, as in 5 s at 30 Hz: channel 1 at every tick but a few (bitmap),
        // channel 2 twice (gaps).
        let mut enc = SessionEncoder::new(channels(), 60_000);
        let input: Vec<Tick> = (0..150)
            .map(|i| {
                let mut values = Vec::new();
                if i % 50 != 7 {
                    values.push((1, Value::Int(i * i)));
                }
                if i == 3 || i == 149 {
                    values.push((2, Value::Int(i)));
                }
                tick(i, values)
            })
            .collect();
        for t in input.clone() {
            enc.push(t).unwrap();
        }
        let chunk = enc.flush().unwrap();
        let decoded = SessionDecoder::new(channels()).decode(&chunk.payload).unwrap();
        for (got, want) in decoded.ticks.iter().zip(&input) {
            assert_eq!(got.values, want.values);
        }
    }

    #[test]
    fn events_round_trip_at_their_ticks() {
        let mut enc = SessionEncoder::new(channels(), 60_000);
        let crash = Event { kind: EventKind::Crashed, detail: String::new() };
        let loaded = Event { kind: EventKind::FlightLoaded, detail: "Flights/pattern.FLT".into() };
        let future = Event { kind: EventKind::Other(200), detail: "x".into() };
        let mut input = vec![
            tick(0, vec![(1, Value::Int(1))]),
            tick(1, vec![]),
            tick(2, vec![(1, Value::Int(2))]),
            tick(3, vec![]),
        ];
        input[1].events = vec![crash.clone(), loaded.clone()];
        input[3].events = vec![future.clone()];
        for t in input.clone() {
            enc.push(t).unwrap();
        }
        let decoded = SessionDecoder::new(channels()).decode(&enc.flush().unwrap().payload).unwrap();
        let events: Vec<_> = decoded.ticks.iter().map(|t| t.events.clone()).collect();
        assert_eq!(events, vec![vec![], vec![crash, loaded], vec![], vec![future]]);
    }

    #[test]
    fn event_kinds_keep_their_codes() {
        for code in 0..=u8::MAX {
            assert_eq!(EventKind::from_code(code).code(), code);
        }
        assert_eq!(EventKind::SimStart.code(), 1);
        assert_eq!(EventKind::ReplayEnded.code(), 11);
        assert_eq!(EventKind::from_code(12), EventKind::Other(12));
    }

    #[test]
    fn rejects_every_truncation() {
        let mut enc = SessionEncoder::new(channels(), 60_000);
        enc.push(tick(0, vec![(1, Value::Int(1)), (3, Value::Str("x".into()))])).unwrap();
        let mut with_event = tick(1, vec![(2, Value::Int(2))]);
        with_event.events.push(Event { kind: EventKind::Paused, detail: "d".into() });
        enc.push(with_event).unwrap();
        let payload = enc.flush().unwrap().payload;
        for len in 0..payload.len() {
            let decoded = SessionDecoder::new(channels()).decode(&payload[..len]);
            assert!(decoded.is_err(), "length {len}");
        }
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn compression_round_trips() {
        let payload = vec![7u8; 10_000];
        let compressed = compress(&payload, 3);
        assert!(compressed.len() < 100);
        assert_eq!(decompress(&compressed).unwrap(), payload);
    }
}
