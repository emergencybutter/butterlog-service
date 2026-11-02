//! What one ButterLog pilot sends another for traffic injection (design §6.1): where the
//! aircraft is, how it is drawn, and what it is. Sent peer to peer over UDP, a few times
//! a second.
//!
//! Sim-neutral: SI-ish units and the usual aviation signs, so an MSFS pilot and an X-Plane
//! pilot see each other. Each sim adapter converts its own values in and out.
//!
//! Later versions only append fields. A decoder reads the fields it knows and skips the
//! rest, so a peer on an older version still draws a newer one.

use crate::chunk::DecodeError;
use crate::varint::{self, Reader};

const MAGIC: &[u8; 4] = b"BLT1";

/// The version this crate writes.
pub const TRAFFIC_VERSION: u16 = 1;

/// Longest text field accepted, in bytes.
const MAX_FIELD_BYTES: usize = 256;

/// Light switches, as bits of [`TrafficState::lights`].
pub mod lights {
    pub const NAV: u8 = 1;
    pub const BEACON: u8 = 1 << 1;
    pub const STROBE: u8 = 1 << 2;
    pub const TAXI: u8 = 1 << 3;
    pub const LANDING: u8 = 1 << 4;
}

/// What the aircraft is, as the sender knows it. The receiver picks a local model from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrafficIdentity {
    /// The sim's aircraft title (MSFS `TITLE`, X-Plane `acf_ui_name`).
    pub title: String,
    /// The sim's own type claim (MSFS `ATC MODEL`, X-Plane `acf_ICAO`); often empty.
    pub atc_model: String,
    /// ICAO type designator, resolved by the service (or the sender, until it answers).
    pub icao: String,
    /// ICAO airline designator; the receiver maps it to a name in its own table.
    pub airline_icao: String,
    pub livery: String,
    /// "airplane" or "helicopter", as far as the sim says.
    pub object_class: String,
    pub category: String,
    pub engine_type: String,
    pub engine_count: u8,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrafficState {
    /// Version the sender wrote. Fields after those this crate knows are skipped.
    pub version: u16,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_msl_m: f32,
    /// True heading, 0–360.
    pub heading_deg: f32,
    /// True ground track, 0–360.
    pub track_deg: f32,
    /// Positive nose up.
    pub pitch_deg: f32,
    /// Positive right wing down.
    pub bank_deg: f32,
    pub ground_speed_kt: f32,
    pub indicated_airspeed_kt: f32,
    pub on_ground: bool,
    /// 0 up, 1 down.
    pub gear: f32,
    /// [`lights`] bits.
    pub lights: u8,
    pub identity: TrafficIdentity,
}

impl TrafficState {
    /// Whether a datagram is a `TrafficState` rather than the JSON packet older apps send.
    pub fn is_traffic_state(bytes: &[u8]) -> bool {
        bytes.starts_with(MAGIC)
    }

    /// ```text
    /// "BLT1" | version varint | latitude, longitude f64 LE | altitude, heading, track,
    /// pitch, bank, ground speed, airspeed f32 LE | on ground u8 | gear f32 LE | lights u8
    /// | title, ATC model, ICAO, airline ICAO, livery, object class, category, engine type:
    /// length varint + UTF-8 | engine count u8
    /// ```
    /// Always written as [`TRAFFIC_VERSION`], whatever `version` says.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        varint::write_u64(&mut out, TRAFFIC_VERSION.into());
        out.extend_from_slice(&self.latitude_deg.to_le_bytes());
        out.extend_from_slice(&self.longitude_deg.to_le_bytes());
        for v in [
            self.altitude_msl_m,
            self.heading_deg,
            self.track_deg,
            self.pitch_deg,
            self.bank_deg,
            self.ground_speed_kt,
            self.indicated_airspeed_kt,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.push(self.on_ground.into());
        out.extend_from_slice(&self.gear.to_le_bytes());
        out.push(self.lights);
        let id = &self.identity;
        for s in [
            &id.title,
            &id.atc_model,
            &id.icao,
            &id.airline_icao,
            &id.livery,
            &id.object_class,
            &id.category,
            &id.engine_type,
        ] {
            let b = s.as_bytes();
            varint::write_bytes(&mut out, &b[..floor_char_boundary(s, MAX_FIELD_BYTES)]);
        }
        out.push(id.engine_count);
        out
    }

    /// Decodes a datagram from a peer. Rejects anything malformed: a peer is not trusted.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        use DecodeError::{BadString, BadValue, Truncated};
        let mut r = Reader::new(bytes);
        if r.bytes(MAGIC.len()) != Some(MAGIC.as_slice()) {
            return Err(BadValue("traffic state"));
        }
        let version = u16::try_from(r.u64().ok_or(Truncated)?).map_err(|_| BadValue("version"))?;
        if version == 0 {
            return Err(BadValue("version"));
        }
        let f64_ = |r: &mut Reader| -> Result<f64, DecodeError> {
            let b = r.bytes(8).ok_or(Truncated)?;
            finite(f64::from_le_bytes(b.try_into().unwrap()))
        };
        let latitude_deg = f64_(&mut r)?;
        let longitude_deg = f64_(&mut r)?;
        let f32_ = |r: &mut Reader| -> Result<f32, DecodeError> {
            let b = r.bytes(4).ok_or(Truncated)?;
            let v = f32::from_le_bytes(b.try_into().unwrap());
            if v.is_finite() { Ok(v) } else { Err(BadValue("number")) }
        };
        let altitude_msl_m = f32_(&mut r)?;
        let heading_deg = f32_(&mut r)?;
        let track_deg = f32_(&mut r)?;
        let pitch_deg = f32_(&mut r)?;
        let bank_deg = f32_(&mut r)?;
        let ground_speed_kt = f32_(&mut r)?;
        let indicated_airspeed_kt = f32_(&mut r)?;
        let on_ground = r.u8().ok_or(Truncated)? != 0;
        let gear = f32_(&mut r)?;
        let lights = r.u8().ok_or(Truncated)?;
        let mut text = || -> Result<String, DecodeError> {
            let b = r.len_prefixed().ok_or(Truncated)?;
            if b.len() > MAX_FIELD_BYTES {
                return Err(BadValue("text field"));
            }
            std::str::from_utf8(b).map(str::to_owned).map_err(|_| BadString)
        };
        let (title, atc_model, icao, airline_icao) = (text()?, text()?, text()?, text()?);
        let (livery, object_class, category, engine_type) = (text()?, text()?, text()?, text()?);
        let engine_count = r.u8().ok_or(Truncated)?;
        if !(-90.0..=90.0).contains(&latitude_deg) || !(-180.0..=180.0).contains(&longitude_deg) {
            return Err(BadValue("position"));
        }
        // Anything after is a newer version's fields.
        Ok(TrafficState {
            version,
            latitude_deg,
            longitude_deg,
            altitude_msl_m,
            heading_deg,
            track_deg,
            pitch_deg,
            bank_deg,
            ground_speed_kt,
            indicated_airspeed_kt,
            on_ground,
            gear,
            lights,
            identity: TrafficIdentity {
                title,
                atc_model,
                icao,
                airline_icao,
                livery,
                object_class,
                category,
                engine_type,
                engine_count,
            },
        })
    }
}

fn finite(v: f64) -> Result<f64, DecodeError> {
    if v.is_finite() { Ok(v) } else { Err(DecodeError::BadValue("number")) }
}

/// The largest index up to `max` that falls on a character boundary of `s`.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if s.len() <= max {
        return s.len();
    }
    (0..=max).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> TrafficState {
        TrafficState {
            version: TRAFFIC_VERSION,
            latitude_deg: 47.449_012_3,
            longitude_deg: -122.309_876_5,
            altitude_msl_m: 1_234.5,
            heading_deg: 163.2,
            track_deg: 161.0,
            pitch_deg: 7.5,
            bank_deg: -12.0,
            ground_speed_kt: 182.0,
            indicated_airspeed_kt: 170.0,
            on_ground: false,
            gear: 1.0,
            lights: lights::NAV | lights::BEACON | lights::LANDING,
            identity: TrafficIdentity {
                title: "Boeing 737-800 Ryanair".into(),
                atc_model: "B738".into(),
                icao: "B738".into(),
                airline_icao: "RYR".into(),
                livery: "Ryanair".into(),
                object_class: "airplane".into(),
                category: "Airplane".into(),
                engine_type: "jet".into(),
                engine_count: 2,
            },
        }
    }

    #[test]
    fn round_trips() {
        let bytes = state().encode();
        assert!(TrafficState::is_traffic_state(&bytes));
        assert_eq!(TrafficState::decode(&bytes).unwrap(), state());
    }

    #[test]
    fn is_small() {
        // The JSON packet it replaces is 1.3 kB with every value zero.
        assert!(state().encode().len() < 150, "{}", state().encode().len());
    }

    #[test]
    fn rejects_every_truncation() {
        let bytes = state().encode();
        for len in 0..bytes.len() {
            assert!(TrafficState::decode(&bytes[..len]).is_err(), "length {len}");
        }
    }

    #[test]
    fn a_newer_version_with_more_fields_still_decodes() {
        let mut bytes = state().encode();
        bytes[4] = 2; // version varint
        bytes.extend_from_slice(b"a field from version 2");
        let decoded = TrafficState::decode(&bytes).unwrap();
        assert_eq!(decoded.version, 2);
        assert_eq!(decoded.identity, state().identity);
    }

    #[test]
    fn json_is_not_a_traffic_state() {
        assert!(!TrafficState::is_traffic_state(br#"{"aircraft":"C172"}"#));
        assert!(TrafficState::decode(br#"{"aircraft":"C172"}"#).is_err());
    }

    #[test]
    fn rejects_nonsense_numbers() {
        let mut s = state();
        s.latitude_deg = 91.0;
        assert!(TrafficState::decode(&s.encode()).is_err());
        let mut s = state();
        s.pitch_deg = f32::NAN;
        assert!(TrafficState::decode(&s.encode()).is_err());
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary() {
        let mut s = state();
        s.identity.title = "é".repeat(200); // 400 bytes
        let decoded = TrafficState::decode(&s.encode()).unwrap();
        assert_eq!(decoded.identity.title, "é".repeat(128));
    }
}
