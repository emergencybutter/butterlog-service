//! What a session opens with: a header and the channel table (design §3.1).

use crate::chunk::DecodeError;
use crate::schema::{decode_table, encode_channels, Channel};
use crate::varint::{self, Reader};
use sha2::{Digest, Sha256};

const OPEN_MAGIC: &[u8; 4] = b"BLS1";
const MAX_FIELD_BYTES: usize = 512;

/// The aircraft as the sim reports it; the service resolves type and operator from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Aircraft {
    pub title: String,
    pub icao: String,
    pub tail: String,
    pub livery: String,
    pub engine_type: String,
    pub engine_count: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionHeader {
    pub protocol_version: u16,
    /// "msfs2020", "msfs2024" or "xplane12".
    pub sim: String,
    pub sim_version: String,
    /// "simconnect", "xp_plugin" or "xp_webapi": tells the service how far to trust timing.
    pub capture: String,
    pub client_version: String,
    pub aircraft: Aircraft,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionOpen {
    pub header: SessionHeader,
    pub channels: Vec<Channel>,
}

/// The hash a channel table is stored under: SHA-256 of its encoded bytes.
pub fn schema_hash(encoded_channels: &[u8]) -> [u8; 32] {
    Sha256::digest(encoded_channels).into()
}

impl SessionOpen {
    /// ```text
    /// "BLS1" | protocol version varint | sim, sim version, capture, client version,
    /// aircraft title, ICAO, tail, livery, engine type: length varint + UTF-8
    /// | engine count u8 | channel table: length varint + bytes (see `encode_channels`)
    /// ```
    pub fn encode(&self) -> Vec<u8> {
        let h = &self.header;
        let mut out = OPEN_MAGIC.to_vec();
        varint::write_u64(&mut out, h.protocol_version.into());
        for s in [
            &h.sim,
            &h.sim_version,
            &h.capture,
            &h.client_version,
            &h.aircraft.title,
            &h.aircraft.icao,
            &h.aircraft.tail,
            &h.aircraft.livery,
            &h.aircraft.engine_type,
        ] {
            varint::write_bytes(&mut out, s.as_bytes());
        }
        out.push(h.aircraft.engine_count);
        varint::write_bytes(&mut out, &encode_channels(&self.channels));
        out
    }

    /// Decodes and validates a session opening received from a client. Also returns the
    /// channel table's bytes, to store and hash as received.
    pub fn decode(bytes: &[u8]) -> Result<(Self, Vec<u8>), DecodeError> {
        use DecodeError::{BadString, BadValue, Truncated};
        let mut r = Reader::new(bytes);
        if r.bytes(OPEN_MAGIC.len()) != Some(OPEN_MAGIC.as_slice()) {
            return Err(BadValue("session opening"));
        }
        let protocol_version =
            u16::try_from(r.u64().ok_or(Truncated)?).map_err(|_| BadValue("protocol version"))?;
        let mut text = || -> Result<String, DecodeError> {
            let b = r.len_prefixed().ok_or(Truncated)?;
            if b.len() > MAX_FIELD_BYTES {
                return Err(BadValue("header field"));
            }
            std::str::from_utf8(b).map(str::to_owned).map_err(|_| BadString)
        };
        let (sim, sim_version, capture, client_version) = (text()?, text()?, text()?, text()?);
        let (title, icao, tail, livery, engine_type) =
            (text()?, text()?, text()?, text()?, text()?);
        let engine_count = r.u8().ok_or(Truncated)?;
        let table = r.len_prefixed().ok_or(Truncated)?;
        let mut table_reader = Reader::new(table);
        let channels = decode_table(&mut table_reader)?;
        if !table_reader.is_empty() || !r.is_empty() {
            return Err(DecodeError::TrailingBytes);
        }
        let header = SessionHeader {
            protocol_version,
            sim,
            sim_version,
            capture,
            client_version,
            aircraft: Aircraft { title, icao, tail, livery, engine_type, engine_count },
        };
        Ok((Self { header, channels }, table.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{DeltaOrder, Kind, RateClass};

    fn open() -> SessionOpen {
        SessionOpen {
            header: SessionHeader {
                protocol_version: crate::PROTOCOL_VERSION,
                sim: "xplane12".into(),
                sim_version: "12.3.1".into(),
                capture: "xp_plugin".into(),
                client_version: "0.4.0".into(),
                aircraft: Aircraft {
                    title: "Cessna 172 SP".into(),
                    icao: "C172".into(),
                    tail: "N172SP".into(),
                    livery: "".into(),
                    engine_type: "recip".into(),
                    engine_count: 1,
                },
            },
            channels: vec![Channel {
                id: 0,
                name: "sim/flightmodel/position/latitude".into(),
                unit: "degrees".into(),
                kind: Kind::F64,
                quantum: 1e-7,
                rate_class: RateClass::Frame,
                delta: DeltaOrder::DeltaOfDelta,
            }],
        }
    }

    #[test]
    fn round_trips_and_returns_the_table_as_sent() {
        let bytes = open().encode();
        let (decoded, table) = SessionOpen::decode(&bytes).unwrap();
        assert_eq!(decoded, open());
        assert_eq!(table, encode_channels(&open().channels));
    }

    #[test]
    fn rejects_every_truncation() {
        let bytes = open().encode();
        for len in 0..bytes.len() {
            assert!(SessionOpen::decode(&bytes[..len]).is_err(), "length {len}");
        }
    }
}
