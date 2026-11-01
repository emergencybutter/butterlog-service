//! The sim-native telemetry wire format (`docs/sim-native-telemetry.md` §3), shared by the
//! app, which encodes, and the service, which decodes. Also home to `TrafficState` (§6.1).
//!
//! This crate holds the format only: no normalisation, segmentation or analysis, which are
//! service-only.

mod chunk;
mod quantise;
mod sampler;
mod schema;
mod segment;
mod session;
mod varint;

pub use chunk::{
    flags, peek_chunk, DecodeError, DecodedChunk, EncodeError, EncodedChunk, SessionDecoder,
    SessionEncoder, Tick, Value,
};
#[cfg(feature = "zstd")]
pub use chunk::{compress, decompress};
pub use quantise::ChangeFilter;
pub use sampler::{period, Phase, PhaseInputs, Sampler, DEFAULT_LOW_HZ};
pub use schema::{decode_channels, encode_channels, Channel, DeltaOrder, Kind, RateClass};
pub use segment::{decode_segment, encode_segment, Segment};
pub use session::{schema_hash, Aircraft, SessionHeader, SessionOpen};

/// Version of the wire format, sent in every session header and plugin handshake.
pub const PROTOCOL_VERSION: u16 = 1;
