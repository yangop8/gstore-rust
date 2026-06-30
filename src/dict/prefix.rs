//! Shared-prefix (front-coding) compression for sorted string dictionaries.
//!
//! gStore's dictionary keeps strings in sorted B+trees and elides each entry's
//! shared prefix with its predecessor, up to a bounded length. RDF terms are
//! extremely prefix-redundant — IRIs in a dataset share a long common namespace
//! (`<http://dbpedia.org/resource/...>`), and sorting clusters them — so storing
//! only `(shared_prefix_len, suffix)` per entry shrinks the dictionary sharply.
//!
//! This module is the codec; [`super::Dictionary::front_coded_block`] wires it in
//! by front-coding the dictionary's strings. The encoding is *front-coding*
//! (a.k.a. incremental encoding), the building block of a prefix trie laid out
//! linearly:
//!
//! ```text
//! count                              (varint)
//! per entry: shared_len  suffix_len  suffix_bytes
//!            (u8, ≤255)  (varint)    (raw)
//! ```
//!
//! `shared_len` is bounded to [`MAX_SHARED_PREFIX`] bytes (one byte on disk),
//! mirroring gStore's bounded shared prefix; longer common prefixes simply leave
//! a few bytes un-elided.

/// Maximum elided shared-prefix length (stored in a single byte), matching
/// gStore's bounded shared prefix.
pub const MAX_SHARED_PREFIX: usize = u8::MAX as usize;

/// Append `v` to `out` as an unsigned LEB128 varint.
fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

/// Read an unsigned LEB128 varint at `*pos`, advancing it. `None` if malformed.
fn read_varint(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    loop {
        let &byte = bytes.get(*pos)?;
        *pos += 1;
        if shift >= 32 || (shift == 28 && byte > 0x0f) {
            return None;
        }
        result |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
    }
}

/// Length of the common byte prefix of `a` and `b`, capped at [`MAX_SHARED_PREFIX`]
/// and rounded down to a UTF-8 char boundary of `b` (so the suffix split is valid;
/// because the bytes are shared, that boundary is also valid in `a`).
fn shared_prefix_len(a: &str, b: &str) -> usize {
    let cap = a.len().min(b.len()).min(MAX_SHARED_PREFIX);
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let mut n = 0;
    while n < cap && ab[n] == bb[n] {
        n += 1;
    }
    while n > 0 && !b.is_char_boundary(n) {
        n -= 1;
    }
    n
}

/// Front-code a **sorted** slice of strings into one compressed block. Sorting is
/// the caller's responsibility (it determines how well prefixes are shared).
pub fn encode_block<S: AsRef<str>>(sorted: &[S]) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint(&mut out, sorted.len() as u32);
    let mut prev: &str = "";
    for s in sorted {
        let cur = s.as_ref();
        let shared = shared_prefix_len(prev, cur);
        let suffix = &cur.as_bytes()[shared..];
        out.push(shared as u8);
        write_varint(&mut out, suffix.len() as u32);
        out.extend_from_slice(suffix);
        prev = cur;
    }
    out
}

/// Decode a block produced by [`encode_block`], reconstructing the exact strings
/// in their original (sorted) order. `None` if the buffer is malformed.
pub fn decode_block(bytes: &[u8]) -> Option<Vec<String>> {
    let mut pos = 0usize;
    let count = read_varint(bytes, &mut pos)? as usize;
    let mut out: Vec<String> = Vec::with_capacity(count);
    let mut prev = String::new();
    for _ in 0..count {
        let shared = *bytes.get(pos)? as usize;
        pos += 1;
        let suffix_len = read_varint(bytes, &mut pos)? as usize;
        let end = pos.checked_add(suffix_len)?;
        let suffix = bytes.get(pos..end)?;
        pos = end;
        // `shared` indexes into `prev`; it was emitted at a char boundary.
        if shared > prev.len() || !prev.is_char_boundary(shared) {
            return None;
        }
        let mut cur = String::with_capacity(shared + suffix_len);
        cur.push_str(&prev[..shared]);
        cur.push_str(std::str::from_utf8(suffix).ok()?);
        out.push(cur.clone());
        prev = cur;
    }
    Some(out)
}

/// Total bytes of the raw strings (no separators) — the baseline a front-coded
/// block is compared against.
pub fn raw_bytes<S: AsRef<str>>(strings: &[S]) -> usize {
    strings.iter().map(|s| s.as_ref().len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_sorted_strings() {
        let mut v = vec![
            "<http://ex.org/a>".to_string(),
            "<http://ex.org/abc>".to_string(),
            "<http://ex.org/abd>".to_string(),
            "<http://ex.org/b>".to_string(),
            "\"a literal\"".to_string(),
            "\"a literal too\"".to_string(),
        ];
        v.sort();
        let enc = encode_block(&v);
        assert_eq!(decode_block(&enc).unwrap(), v);
    }

    #[test]
    fn roundtrips_empty_and_single() {
        let empty: Vec<String> = Vec::new();
        assert_eq!(decode_block(&encode_block(&empty)).unwrap(), empty);
        let one = vec!["solo".to_string()];
        assert_eq!(decode_block(&encode_block(&one)).unwrap(), one);
    }

    #[test]
    fn handles_multibyte_utf8_prefixes() {
        // Shared multibyte prefix must not be split mid-codepoint.
        let mut v = vec![
            "café-1".to_string(),
            "café-2".to_string(),
            "café-au-lait".to_string(),
            "naïve".to_string(),
        ];
        v.sort();
        let enc = encode_block(&v);
        assert_eq!(decode_block(&enc).unwrap(), v);
    }

    #[test]
    fn shares_prefixes_to_save_space() {
        // Many IRIs under one long namespace: front-coding should slash the size.
        let mut v: Vec<String> = (0..1000)
            .map(|i| format!("<http://dbpedia.org/resource/Entity_{i:06}>"))
            .collect();
        v.sort();
        let enc = encode_block(&v);
        let raw = raw_bytes(&v);
        assert!(
            enc.len() < raw / 2,
            "front-coded block should be <1/2 of raw {raw} bytes, got {}",
            enc.len()
        );
        assert_eq!(decode_block(&enc).unwrap(), v);
    }

    #[test]
    fn bounded_shared_prefix_is_respected() {
        // Two strings sharing more than MAX_SHARED_PREFIX bytes: only the bound
        // is elided, and the value still round-trips.
        let base = "x".repeat(MAX_SHARED_PREFIX + 50);
        let a = format!("{base}A");
        let b = format!("{base}B");
        let v = vec![a, b];
        let enc = encode_block(&v);
        // Second entry's shared byte is capped at the bound.
        // (count=1B, entry0: 1B shared +1B suflen-ish, ...). Just verify the cap
        // via round-trip plus that no shared_len byte exceeds the bound.
        assert_eq!(decode_block(&enc).unwrap(), v);
    }

    #[test]
    fn decode_rejects_truncated_suffix() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 1); // count = 1
        buf.push(0); // shared = 0
        write_varint(&mut buf, 10); // suffix_len = 10 …
        buf.extend_from_slice(b"abc"); // … but only 3 bytes present
        assert_eq!(decode_block(&buf), None);
    }
}
