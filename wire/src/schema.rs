//! The channel table a session opens with (design §3.1).

/// What a channel carries. Every kind but `String` travels as a quantised integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    F64,
    I64,
    Bool,
    Enum,
    String,
}

/// How often the sampler reads a channel (design §3.3); the rates per class are tunables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateClass {
    Frame,
    Fast,
    Slow,
    OnChange,
}

/// How a channel's values are differenced within a chunk. Smoothly varying channels
/// (position, attitude) compress best as delta-of-delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaOrder {
    Delta,
    DeltaOfDelta,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub id: u16,
    /// SimVar or dataref path, plus index for arrays.
    pub name: String,
    /// Sim-native unit.
    pub unit: String,
    pub kind: Kind,
    /// Quantisation step, e.g. 1e-7 for lat/lon, 0.1 for feet. Ignored for `String`.
    pub quantum: f64,
    pub rate_class: RateClass,
    pub delta: DeltaOrder,
}

impl Channel {
    /// Quantises a sim value onto this channel's integer grid.
    pub fn quantise(&self, v: f64) -> i64 {
        (v / self.quantum).round() as i64
    }

    /// The value a quantised integer stands for.
    pub fn dequantise(&self, q: i64) -> f64 {
        q as f64 * self.quantum
    }
}

use crate::chunk::DecodeError;
use crate::varint::{self, Reader};

const TABLE_FORMAT: u8 = 1;
/// Limits on a table received from a client.
const MAX_CHANNELS: usize = 4096;
const MAX_NAME_BYTES: usize = 256;

impl Kind {
    fn code(self) -> u8 {
        match self {
            Kind::F64 => 0,
            Kind::I64 => 1,
            Kind::Bool => 2,
            Kind::Enum => 3,
            Kind::String => 4,
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        [Kind::F64, Kind::I64, Kind::Bool, Kind::Enum, Kind::String]
            .into_iter()
            .find(|k| k.code() == c)
    }
}

impl RateClass {
    fn code(self) -> u8 {
        match self {
            RateClass::Frame => 0,
            RateClass::Fast => 1,
            RateClass::Slow => 2,
            RateClass::OnChange => 3,
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        [RateClass::Frame, RateClass::Fast, RateClass::Slow, RateClass::OnChange]
            .into_iter()
            .find(|r| r.code() == c)
    }
}

impl DeltaOrder {
    fn code(self) -> u8 {
        match self {
            DeltaOrder::Delta => 1,
            DeltaOrder::DeltaOfDelta => 2,
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        [DeltaOrder::Delta, DeltaOrder::DeltaOfDelta]
            .into_iter()
            .find(|d| d.code() == c)
    }
}

/// The channel table as bytes. The service stores it under the hash of these exact
/// bytes ([`crate::schema_hash`]), so a table is only ever encoded once per session.
///
/// ```text
/// format u8 | n_channels varint, then per channel:
///   id varint | name, unit: length varint + UTF-8 | kind u8 | quantum f64 LE
///   | rate class u8 | delta order u8
/// ```
pub fn encode_channels(channels: &[Channel]) -> Vec<u8> {
    let mut out = vec![TABLE_FORMAT];
    varint::write_u64(&mut out, channels.len() as u64);
    for c in channels {
        varint::write_u64(&mut out, c.id.into());
        varint::write_bytes(&mut out, c.name.as_bytes());
        varint::write_bytes(&mut out, c.unit.as_bytes());
        out.push(c.kind.code());
        out.extend_from_slice(&c.quantum.to_le_bytes());
        out.push(c.rate_class.code());
        out.push(c.delta.code());
    }
    out
}

/// Decodes and validates a channel table received from a client.
pub fn decode_channels(bytes: &[u8]) -> Result<Vec<Channel>, DecodeError> {
    let mut r = Reader::new(bytes);
    let channels = decode_table(&mut r)?;
    if !r.is_empty() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(channels)
}

pub(crate) fn decode_table(r: &mut Reader) -> Result<Vec<Channel>, DecodeError> {
    use DecodeError::{BadString, BadValue, Truncated};
    match r.u8().ok_or(Truncated)? {
        TABLE_FORMAT => {}
        other => return Err(DecodeError::UnknownFormat(other)),
    }
    let n = usize::try_from(r.u64().ok_or(Truncated)?).map_err(|_| BadValue("channel count"))?;
    if n > MAX_CHANNELS {
        return Err(BadValue("channel count"));
    }
    let mut channels: Vec<Channel> = Vec::with_capacity(n);
    for _ in 0..n {
        let id = u16::try_from(r.u64().ok_or(Truncated)?).map_err(|_| BadValue("channel id"))?;
        if channels.iter().any(|c| c.id == id) {
            return Err(BadValue("channel id"));
        }
        let mut text = |what| -> Result<String, DecodeError> {
            let b = r.len_prefixed().ok_or(Truncated)?;
            if b.len() > MAX_NAME_BYTES {
                return Err(BadValue(what));
            }
            std::str::from_utf8(b).map(str::to_owned).map_err(|_| BadString)
        };
        let name = text("channel name")?;
        let unit = text("channel unit")?;
        if name.is_empty() {
            return Err(BadValue("channel name"));
        }
        let kind = Kind::from_code(r.u8().ok_or(Truncated)?).ok_or(BadValue("channel kind"))?;
        let quantum = f64::from_le_bytes(r.bytes(8).ok_or(Truncated)?.try_into().unwrap());
        if !(quantum.is_finite() && quantum > 0.0) {
            return Err(BadValue("channel quantum"));
        }
        let rate_class =
            RateClass::from_code(r.u8().ok_or(Truncated)?).ok_or(BadValue("channel rate class"))?;
        let delta =
            DeltaOrder::from_code(r.u8().ok_or(Truncated)?).ok_or(BadValue("channel delta order"))?;
        channels.push(Channel { id, name, unit, kind, quantum, rate_class, delta });
    }
    Ok(channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Channel> {
        vec![
            Channel {
                id: 0,
                name: "PLANE LATITUDE".into(),
                unit: "degrees".into(),
                kind: Kind::F64,
                quantum: 1e-7,
                rate_class: RateClass::Frame,
                delta: DeltaOrder::DeltaOfDelta,
            },
            Channel {
                id: 7,
                name: "sim/aircraft/view/acf_ICAO".into(),
                unit: "".into(),
                kind: Kind::String,
                quantum: 1.0,
                rate_class: RateClass::OnChange,
                delta: DeltaOrder::Delta,
            },
        ]
    }

    #[test]
    fn round_trips() {
        assert_eq!(decode_channels(&encode_channels(&sample())).unwrap(), sample());
    }

    #[test]
    fn rejects_invalid_tables() {
        let mut dup = sample();
        dup[1].id = 0;
        assert_eq!(
            decode_channels(&encode_channels(&dup)),
            Err(DecodeError::BadValue("channel id"))
        );
        let mut zero = sample();
        zero[0].quantum = 0.0;
        assert_eq!(
            decode_channels(&encode_channels(&zero)),
            Err(DecodeError::BadValue("channel quantum"))
        );
        let bytes = encode_channels(&sample());
        for len in 0..bytes.len() {
            assert!(decode_channels(&bytes[..len]).is_err(), "length {len}");
        }
    }
}
