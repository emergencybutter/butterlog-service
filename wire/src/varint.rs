//! LEB128 varints and zigzag mapping, the integer packing used throughout a chunk.

pub fn write_u64(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn write_i64(out: &mut Vec<u8>, v: i64) {
    write_u64(out, zigzag(v));
}

/// Length-prefixed bytes, for strings and nested blobs.
pub fn write_bytes(out: &mut Vec<u8>, b: &[u8]) {
    write_u64(out, b.len() as u64);
    out.extend_from_slice(b);
}

pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

pub fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// A cursor over an encoded chunk. Every read fails cleanly on truncated input.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn u8(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    pub fn u64(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    pub fn i64(&mut self) -> Option<i64> {
        self.u64().map(unzigzag)
    }

    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let b = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(b)
    }

    /// Bytes written by [`write_bytes`].
    pub fn len_prefixed(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.u64()?).ok()?;
        self.bytes(n)
    }

    pub fn is_empty(&self) -> bool {
        self.pos == self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_extremes() {
        for v in [0, 1, -1, 63, -64, 64, i64::MAX, i64::MIN] {
            let mut out = Vec::new();
            write_i64(&mut out, v);
            assert_eq!(Reader::new(&out).i64(), Some(v));
        }
    }

    #[test]
    fn small_magnitudes_take_one_byte() {
        let mut out = Vec::new();
        write_i64(&mut out, -64);
        write_i64(&mut out, 63);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn truncated_input_is_none() {
        assert_eq!(Reader::new(&[0x80]).u64(), None);
    }
}
