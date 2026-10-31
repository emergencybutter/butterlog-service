//! A finished session compacted into one object (design §5.1): its opening and every chunk
//! payload, in order. The caller compresses the whole segment, which compresses far better
//! than chunks one by one (rollout phase 1 measurements).

use crate::chunk::DecodeError;
use crate::varint::{self, Reader};

const SEGMENT_MAGIC: &[u8; 5] = b"BLSG1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The session opening, as received (see [`crate::SessionOpen`]).
    pub open: Vec<u8>,
    /// Uncompressed chunk payloads by sequence number.
    pub chunks: Vec<(u32, Vec<u8>)>,
}

/// ```text
/// "BLSG1" | opening: length varint + bytes | n_chunks varint,
/// then per chunk: seq varint | payload: length varint + bytes
/// ```
pub fn encode_segment<'a>(
    open: &[u8],
    chunks: impl ExactSizeIterator<Item = (u32, &'a [u8])>,
) -> Vec<u8> {
    let mut out = SEGMENT_MAGIC.to_vec();
    varint::write_bytes(&mut out, open);
    varint::write_u64(&mut out, chunks.len() as u64);
    for (seq, payload) in chunks {
        varint::write_u64(&mut out, seq.into());
        varint::write_bytes(&mut out, payload);
    }
    out
}

pub fn decode_segment(bytes: &[u8]) -> Result<Segment, DecodeError> {
    use DecodeError::{BadValue, Truncated};
    let mut r = Reader::new(bytes);
    if r.bytes(SEGMENT_MAGIC.len()) != Some(SEGMENT_MAGIC.as_slice()) {
        return Err(BadValue("segment"));
    }
    let open = r.len_prefixed().ok_or(Truncated)?.to_vec();
    let n = r.u64().ok_or(Truncated)?;
    let mut chunks = Vec::new();
    for _ in 0..n {
        let seq = u32::try_from(r.u64().ok_or(Truncated)?).map_err(|_| BadValue("chunk seq"))?;
        chunks.push((seq, r.len_prefixed().ok_or(Truncated)?.to_vec()));
    }
    if !r.is_empty() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(Segment { open, chunks })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let chunks: Vec<(u32, Vec<u8>)> = vec![(0, vec![1, 2, 3]), (1, vec![]), (2, vec![9; 300])];
        let bytes = encode_segment(b"open", chunks.iter().map(|(s, p)| (*s, p.as_slice())));
        assert_eq!(decode_segment(&bytes).unwrap(), Segment { open: b"open".to_vec(), chunks });
        for len in 0..bytes.len() {
            assert!(decode_segment(&bytes[..len]).is_err(), "length {len}");
        }
    }
}
