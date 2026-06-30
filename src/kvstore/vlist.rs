//! Compact variable-length encoding for multi-value posting lists ("VList").
//!
//! gStore stores, per key, a packed list of ids (e.g. `subID → values`). Storing
//! those as a raw fixed-width `u32` array costs 4 bytes per id regardless of
//! magnitude. Because the lists are **sorted and de-duplicated**, we can do much
//! better with the classic *delta + varint* scheme used by inverted indexes:
//!
//! * **delta**: store each id as its difference from the previous one. A dense,
//!   monotonically-increasing id list turns into a run of small numbers.
//! * **varint** (LEB128, little-endian base-128): write 7 payload bits per byte
//!   with the high bit as a continuation flag, so small numbers take 1 byte,
//!   values `< 2^14` take 2, etc. Most deltas in a real graph are tiny.
//!
//! A list of `n` ids spanning a small range thus compresses from `4n` bytes
//! toward `~n` bytes. The format is self-describing (it stores the element
//! count), so [`decode_u32s`] reconstructs the exact original list.
//!
//! This is the codec; [`super::store`] wires it into the disk store's value
//! (de)serialization (see `DiskStore::compact`). Values larger than a page are
//! gStore's "VList overflow block" case — bounded here by the caller (see the
//! REFACTOR backlog / task 4 note).

/// Append `v` to `out` as an unsigned LEB128 varint.
pub fn write_varint(out: &mut Vec<u8>, mut v: u32) {
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

/// Read an unsigned LEB128 varint starting at `*pos`, advancing `*pos`. Returns
/// `None` on a truncated or overlong (> 5-byte) encoding.
pub fn read_varint(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    loop {
        let &byte = bytes.get(*pos)?;
        *pos += 1;
        // The 5th byte may only contribute the top 4 bits of a u32.
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

/// Encode a **sorted, de-duplicated** id list as a VList (count, then deltas).
///
/// The caller must pass strictly-ascending ids; that is the shape of every
/// posting list in the store. Encoding is delta + varint, so the byte length is
/// driven by the *gaps* between ids, not their absolute size.
pub fn encode_u32s(ids: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ids.len() + 2);
    write_varint(&mut out, ids.len() as u32);
    let mut prev: u32 = 0;
    for &id in ids {
        // Gaps are non-negative for a sorted list; the first delta is `id - 0`.
        let delta = id - prev;
        write_varint(&mut out, delta);
        prev = id;
    }
    out
}

/// Decode a VList produced by [`encode_u32s`] back into the original id list.
/// Returns `None` if the buffer is malformed (truncated / overlong varint).
pub fn decode_u32s(bytes: &[u8]) -> Option<Vec<u32>> {
    let mut pos = 0usize;
    let count = read_varint(bytes, &mut pos)? as usize;
    let mut out = Vec::with_capacity(count);
    let mut prev: u32 = 0;
    for _ in 0..count {
        let delta = read_varint(bytes, &mut pos)?;
        let id = prev.checked_add(delta)?;
        out.push(id);
        prev = id;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrips_boundaries() {
        for v in [0u32, 1, 127, 128, 16_383, 16_384, 1 << 21, u32::MAX] {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            let mut pos = 0;
            assert_eq!(read_varint(&buf, &mut pos), Some(v), "value {v}");
            assert_eq!(pos, buf.len(), "consumed all bytes for {v}");
        }
    }

    #[test]
    fn varint_byte_widths_match_leb128() {
        let width = |v| {
            let mut b = Vec::new();
            write_varint(&mut b, v);
            b.len()
        };
        assert_eq!(width(0), 1);
        assert_eq!(width(127), 1);
        assert_eq!(width(128), 2);
        assert_eq!(width(16_383), 2);
        assert_eq!(width(16_384), 3);
        assert_eq!(width(u32::MAX), 5);
    }

    #[test]
    fn read_varint_rejects_truncated_and_overlong() {
        // Continuation bit set but no following byte.
        let mut pos = 0;
        assert_eq!(read_varint(&[0x80], &mut pos), None);
        // Six bytes (overlong for a u32).
        let mut pos = 0;
        assert_eq!(read_varint(&[0xff, 0xff, 0xff, 0xff, 0xff, 0x01], &mut pos), None);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let cases: Vec<Vec<u32>> = vec![
            vec![],
            vec![0],
            vec![5],
            vec![0, 1, 2, 3, 4],
            vec![10, 20, 30, 40],
            vec![1, 1_000, 1_000_000, 2_000_000_000],
            (0..1000u32).map(|i| i * 3).collect(),
            vec![u32::MAX - 1, u32::MAX],
        ];
        for c in cases {
            let enc = encode_u32s(&c);
            assert_eq!(decode_u32s(&enc).unwrap(), c, "roundtrip {c:?}");
        }
    }

    #[test]
    fn dense_list_is_far_smaller_than_raw() {
        // 1000 consecutive ids: raw = 4000 bytes; deltas are all 1 → ~1 byte each.
        let ids: Vec<u32> = (0..1000).collect();
        let raw = ids.len() * 4;
        let enc = encode_u32s(&ids);
        assert!(
            enc.len() < raw / 3,
            "dense vlist should be <1/3 of raw: {} vs {raw}",
            enc.len()
        );
        // A consecutive run is ~1 byte/id plus the count prefix.
        assert!(enc.len() <= ids.len() + 2);
    }

    #[test]
    fn sparse_list_still_no_worse_than_raw_plus_count() {
        // Large gaps: each delta needs up to 5 bytes, but never more than raw + a
        // small overhead. (Worst case for varint vs fixed 4-byte is bounded.)
        let ids: Vec<u32> = vec![0, 1 << 28, 1 << 29, u32::MAX];
        let enc = encode_u32s(&ids);
        let decoded = decode_u32s(&enc).unwrap();
        assert_eq!(decoded, ids);
    }

    #[test]
    fn decode_rejects_garbage() {
        // Claims 5 elements but provides no delta bytes.
        let mut buf = Vec::new();
        write_varint(&mut buf, 5);
        assert_eq!(decode_u32s(&buf), None);
    }
}
