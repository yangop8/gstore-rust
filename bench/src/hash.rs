//! Content hashes: FNV-1a 64 for byte streams (data files, query texts,
//! response bodies) and [`rows_fnv64`], an order-independent multiset hash of
//! a result table.
//!
//! Hashes are written into result files as 16 lowercase hex digits
//! ([`hex`]), so they survive JSON tools that read numbers as doubles.

use std::io::{self, Write};

/// The FNV-1a 64 offset basis.
pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// The FNV-1a 64 prime.
pub const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A streaming FNV-1a 64 hasher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fnv64(u64);

impl Default for Fnv64 {
    fn default() -> Self {
        Fnv64(FNV_OFFSET)
    }
}

impl Fnv64 {
    pub fn new() -> Fnv64 {
        Fnv64::default()
    }

    #[inline]
    pub fn update(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(FNV_PRIME);
        }
        self.0 = h;
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

/// FNV-1a 64 of `bytes`.
pub fn fnv64(bytes: &[u8]) -> u64 {
    let mut h = Fnv64::new();
    h.update(bytes);
    h.finish()
}

/// `h` as 16 lowercase hex digits.
pub fn hex(h: u64) -> String {
    format!("{h:016x}")
}

/// Parse [`hex`]'s output back.
pub fn parse_hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s, 16).ok()
}

/// The hash of one row: every cell is tagged, so an unbound cell (`None`)
/// differs from a bound empty string, and cell boundaries are length-prefixed.
pub fn row_fnv64(row: &[Option<String>]) -> u64 {
    let mut h = Fnv64::new();
    h.update(&(row.len() as u64).to_le_bytes());
    for cell in row {
        match cell {
            None => h.update(&[0]),
            Some(s) => {
                h.update(&[1]);
                h.update(&(s.len() as u64).to_le_bytes());
                h.update(s.as_bytes());
            }
        }
    }
    h.finish()
}

/// An order-independent multiset hash of a result table: the wrapping sum of
/// the mixed hashes of its rows, so row order does not matter but duplicate
/// rows do. The row count is mixed in as well.
pub fn rows_fnv64(rows: &[Vec<Option<String>>]) -> u64 {
    let mut sum = 0u64;
    for row in rows {
        sum = sum.wrapping_add(crate::rng::mix64(row_fnv64(row)));
    }
    crate::rng::mix64(sum ^ (rows.len() as u64).wrapping_mul(crate::rng::GOLDEN_GAMMA))
}

/// A writer that hashes and counts everything written through it.
pub struct HashingWriter<W: Write> {
    inner: W,
    hash: Fnv64,
    bytes: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        HashingWriter {
            inner,
            hash: Fnv64::new(),
            bytes: 0,
        }
    }

    /// (hash, bytes) of everything written so far.
    pub fn digest(&self) -> (u64, u64) {
        (self.hash.finish(), self.bytes)
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// FNV-1a 64 and length of a file's bytes.
pub fn file_fnv64(path: &std::path::Path) -> io::Result<(u64, u64)> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut h = Fnv64::new();
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((h.finish(), n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_reference_values() {
        // Published FNV-1a 64 test vectors.
        assert_eq!(fnv64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn rows_hash_is_a_multiset_hash() {
        let a = vec![Some("x".to_string()), None];
        let b = vec![Some("y".to_string()), Some(String::new())];
        let h1 = rows_fnv64(&[a.clone(), b.clone()]);
        let h2 = rows_fnv64(&[b.clone(), a.clone()]);
        assert_eq!(h1, h2, "order does not matter");
        assert_ne!(h1, rows_fnv64(&[a.clone(), b.clone(), a.clone()]), "duplicates count");
        assert_ne!(h1, rows_fnv64(&[a.clone()]));
        // Unbound differs from the empty string.
        let unbound = vec![None];
        let empty = vec![Some(String::new())];
        assert_ne!(rows_fnv64(&[unbound]), rows_fnv64(&[empty]));
        // Cell boundaries matter.
        let ab = vec![Some("ab".to_string()), Some("c".to_string())];
        let a_bc = vec![Some("a".to_string()), Some("bc".to_string())];
        assert_ne!(rows_fnv64(&[ab]), rows_fnv64(&[a_bc]));
        assert_ne!(rows_fnv64(&[]), rows_fnv64(&[vec![]]));
    }

    #[test]
    fn hashing_writer_matches_fnv64() {
        let mut w = HashingWriter::new(Vec::new());
        w.write_all(b"foo").unwrap();
        w.write_all(b"bar").unwrap();
        assert_eq!(w.digest(), (fnv64(b"foobar"), 6));
        assert_eq!(parse_hex(&hex(0x1234)), Some(0x1234));
        assert_eq!(hex(1), "0000000000000001");
    }
}
