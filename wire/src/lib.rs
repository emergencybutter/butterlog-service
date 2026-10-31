//! The sim-native telemetry wire format (`docs/sim-native-telemetry.md` §3), shared by the
//! app, which encodes, and the service, which decodes. Also home to `TrafficState` (§6.1).
//!
//! This crate holds the format only: no normalisation, segmentation or analysis, which are
//! service-only.

mod chunk;
mod quantise;
mod schema;
mod varint;

pub use chunk::{
    flags, DecodeError, DecodedChunk, EncodeError, EncodedChunk, SessionDecoder, SessionEncoder,
    Tick, Value,
};
#[cfg(feature = "zstd")]
pub use chunk::{compress, decompress};
pub use quantise::ChangeFilter;
pub use schema::{Channel, DeltaOrder, Kind, RateClass};

/// Version of the wire format, sent in every session header and plugin handshake.
pub const PROTOCOL_VERSION: u16 = 1;
