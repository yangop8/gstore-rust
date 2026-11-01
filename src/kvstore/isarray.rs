//! `IsArray` — a **string-keyed** (variable byte-key) block-managed array
//! (gStore's `ISArray`).
//!
//! gStore pairs its integer-keyed [`IvArray`](super::ivarray) with a *string*-keyed
//! array (`ISArray`) for the byte-key stores its dictionary needs, keeping the
//! keys/values packed in block-managed value blocks rather than in a node-based
//! tree. This is the Rust counterpart: a hash-directory over the shared
//! [`Pager`], purpose-built for **point** lookups on arbitrary byte keys
//! (`get`/`insert`/`delete` — no ordered range scan), the access pattern of a
//! forward dictionary (`entity2id`/`literal2id`/`predicate2id`). Where a caller
//! needs ordered prefix range scans it should keep the variable-key
//! [`BTree`](super::bptree) instead; where it only needs exact-key resolution
//! this trades the tree's `O(log n)` descent for an `O(1)`-expected bucket probe
//! with byte-compact storage.
//!
//! ## Layout
//! The array is a fixed set of [`NUM_BUCKETS`] hash buckets. A key hashes
//! (FNV-1a) to a bucket; each **bucket's whole entry list is one value** stored
//! in an internal [`IvArray`] keyed by the bucket index. That reuse is the point:
//! [`IvArray`] already does the block management — a small bucket lives inline in
//! its directory slot, a large one (many colliding keys, or a big value) spills
//! into an [`overflow`](super::overflow) page chain automatically — so `IsArray`
//! is a thin, correctness-focused hash layer and never re-implements paging.
//!
//! A bucket payload is a self-describing sequence of entries:
//! ```text
//! [key_len: varint][key bytes][val_len: varint][val bytes] …
//! ```
//! (LEB128 varints, reusing the [`vlist`](super::vlist) codec.) Lookups scan the
//! (typically length-1) bucket list comparing keys byte-exactly, so collisions
//! and duplicate-key overwrites are always correct.
//!
//! ## Sizing (no rehash)
//! [`NUM_BUCKETS`] is fixed, so there is **no resize/rehash** — a design choice
//! that keeps the structure simple and unconditionally correct: an oversubscribed
//! bucket merely holds a longer entry list (still handled by [`IvArray`]'s
//! overflow chains), degrading lookup speed but never behaviour. Pick a bucket
//! count comfortably above the expected key count for `O(1)`-expected probes.
//! This mirrors [`IvArray`]'s own documented capacity note: correctness first.

use crate::error::Result;

use super::ivarray::IvArray;
use super::pager::Pager;
use super::vlist::{read_varint, write_varint};

/// Number of hash buckets. A power of two so `hash % NUM_BUCKETS` is a mask; sized
/// so working-set dictionaries keep short (usually singleton) bucket lists. Fixed
/// for the life of the array (see the module "Sizing" note).
const NUM_BUCKETS: u32 = 1 << 16; // 65_536

/// A handle to one string-keyed array, rooted at a [`Pager`] header slot (the
/// bucket directory's [`IvArray`] root). Like [`IvArray`]/[`BTree`](super::bptree)
/// it is `Copy` — the structure lives in pager pages, the handle is just the slot.
#[derive(Clone, Copy)]
pub struct IsArray {
    /// Bucket directory: bucket index → packed entry list, block-managed.
    dir: IvArray,
}

impl IsArray {
    /// A string-keyed array rooted at header slot `root_slot`.
    pub fn new(root_slot: usize) -> IsArray {
        IsArray {
            dir: IvArray::new(root_slot),
        }
    }

    /// The bucket a key hashes into (FNV-1a, masked to [`NUM_BUCKETS`]).
    fn bucket_of(key: &[u8]) -> u32 {
        fnv1a(key) % NUM_BUCKETS
    }

    /// Look up the value for byte-key `key`. Read-only (`&Pager`), so it runs
    /// under a shared read guard concurrently with other readers.
    pub fn get(&self, pager: &Pager, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let bucket = Self::bucket_of(key);
        let Some(payload) = self.dir.get(pager, bucket)? else {
            return Ok(None);
        };
        Ok(find_entry(&payload, key))
    }

    /// Insert or replace `key → val`. Rewrites the whole bucket payload (through
    /// [`IvArray`], which re-does the inline/overflow decision), so an existing
    /// key is overwritten in place rather than duplicated.
    pub fn insert(&self, pager: &mut Pager, key: &[u8], val: &[u8]) -> Result<()> {
        let bucket = Self::bucket_of(key);
        let payload = self.dir.get(pager, bucket)?.unwrap_or_default();
        let updated = upsert_entry(&payload, key, val);
        self.dir.insert(pager, bucket, &updated)
    }

    /// Remove `key`'s entry if present; returns `true` if it existed. An emptied
    /// bucket is deleted from the directory (freeing any overflow chain), so the
    /// store does not accumulate empty buckets.
    pub fn delete(&self, pager: &mut Pager, key: &[u8]) -> Result<bool> {
        let bucket = Self::bucket_of(key);
        let Some(payload) = self.dir.get(pager, bucket)? else {
            return Ok(false);
        };
        match remove_entry(&payload, key) {
            None => Ok(false),
            Some(rest) => {
                if rest.is_empty() {
                    self.dir.delete(pager, bucket)?;
                } else {
                    self.dir.insert(pager, bucket, &rest)?;
                }
                Ok(true)
            }
        }
    }
}

// ---- bucket payload codec -------------------------------------------------

/// Iterate `(key, val)` byte-slice views over a bucket payload, yielding the
/// byte range each entry occupies so callers can rebuild the payload cheaply.
/// Returns `None` mid-iteration on a malformed (truncated) payload — which a
/// well-formed store never produces, but keeps parsing panic-free.
fn entries(payload: &[u8]) -> impl Iterator<Item = (&[u8], &[u8], usize)> + '_ {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        if pos >= payload.len() {
            return None;
        }
        let start = pos;
        let klen = read_varint(payload, &mut pos)? as usize;
        let k = payload.get(pos..pos + klen)?;
        pos += klen;
        let vlen = read_varint(payload, &mut pos)? as usize;
        let v = payload.get(pos..pos + vlen)?;
        pos += vlen;
        Some((k, v, start))
    })
}

/// Append one `[klen][key][vlen][val]` record to `out`.
fn push_entry(out: &mut Vec<u8>, key: &[u8], val: &[u8]) {
    write_varint(out, key.len() as u32);
    out.extend_from_slice(key);
    write_varint(out, val.len() as u32);
    out.extend_from_slice(val);
}

/// The value for `key` in a bucket payload, if present.
fn find_entry(payload: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    entries(payload).find(|&(k, _, _)| k == key).map(|(_, v, _)| v.to_vec())
}

/// Rebuild a bucket payload with `key → val` inserted or overwritten, preserving
/// every other entry (order-independent, so an overwrite keeps the bucket compact
/// with no duplicate keys).
fn upsert_entry(payload: &[u8], key: &[u8], val: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + key.len() + val.len() + 4);
    for (k, v, _) in entries(payload) {
        if k != key {
            push_entry(&mut out, k, v);
        }
    }
    push_entry(&mut out, key, val);
    out
}

/// Rebuild a bucket payload with `key`'s entry removed. `None` if `key` is
/// absent (so the caller can report "not found" without a rewrite).
fn remove_entry(payload: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    let mut found = false;
    let mut out = Vec::with_capacity(payload.len());
    for (k, v, _) in entries(payload) {
        if k == key {
            found = true;
        } else {
            push_entry(&mut out, k, v);
        }
    }
    found.then_some(out)
}

/// FNV-1a 32-bit hash — dependency-free, well-distributed for short byte keys.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kvstore::pager::Pager;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gstore_isarray_{tag}_{}.kv", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("kv.wal"));
        p
    }

    #[test]
    fn insert_get_basic_and_missing() {
        let path = tmp("basic");
        let mut pg = Pager::open(&path, 64).unwrap();
        let a = IsArray::new(0);
        a.insert(&mut pg, b"<alice>", &1u32.to_le_bytes()).unwrap();
        a.insert(&mut pg, b"<bob>", &2u32.to_le_bytes()).unwrap();
        a.insert(&mut pg, b"\"a literal\"", &3u32.to_le_bytes()).unwrap();
        assert_eq!(a.get(&pg, b"<alice>").unwrap().unwrap(), 1u32.to_le_bytes());
        assert_eq!(a.get(&pg, b"<bob>").unwrap().unwrap(), 2u32.to_le_bytes());
        assert_eq!(
            a.get(&pg, b"\"a literal\"").unwrap().unwrap(),
            3u32.to_le_bytes()
        );
        assert_eq!(a.get(&pg, b"<carol>").unwrap(), None);
        assert_eq!(a.get(&pg, b"").unwrap(), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn overwrite_keeps_single_entry() {
        let path = tmp("overwrite");
        let mut pg = Pager::open(&path, 64).unwrap();
        let a = IsArray::new(0);
        a.insert(&mut pg, b"k", b"one").unwrap();
        a.insert(&mut pg, b"k", b"two").unwrap();
        a.insert(&mut pg, b"k", b"three").unwrap();
        assert_eq!(a.get(&pg, b"k").unwrap().as_deref(), Some(&b"three"[..]));
        // Overwrite must not duplicate: deleting once removes it entirely.
        assert!(a.delete(&mut pg, b"k").unwrap());
        assert_eq!(a.get(&pg, b"k").unwrap(), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn delete_reports_presence_and_clears() {
        let path = tmp("delete");
        let mut pg = Pager::open(&path, 64).unwrap();
        let a = IsArray::new(0);
        a.insert(&mut pg, b"keep", b"1").unwrap();
        a.insert(&mut pg, b"drop", b"2").unwrap();
        assert!(a.delete(&mut pg, b"drop").unwrap());
        assert!(!a.delete(&mut pg, b"drop").unwrap()); // already gone
        assert_eq!(a.get(&pg, b"drop").unwrap(), None);
        assert_eq!(a.get(&pg, b"keep").unwrap().as_deref(), Some(&b"1"[..]));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn many_keys_including_collisions_roundtrip() {
        // Far more keys than fit in one bucket-worth of distinct hashes ⇒ exercises
        // colliding entry lists; every key must resolve to its own value.
        let path = tmp("many");
        let mut pg = Pager::open(&path, 256).unwrap();
        let a = IsArray::new(0);
        let n = 5000u32;
        for i in 0..n {
            a.insert(&mut pg, format!("<http://ex/node{i}>").as_bytes(), &i.to_le_bytes())
                .unwrap();
        }
        for i in 0..n {
            assert_eq!(
                a.get(&pg, format!("<http://ex/node{i}>").as_bytes())
                    .unwrap()
                    .unwrap(),
                i.to_le_bytes(),
                "key {i}"
            );
        }
        // A never-inserted key is absent even if it collides with a live bucket.
        assert_eq!(a.get(&pg, b"<http://ex/absent>").unwrap(), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn large_value_roundtrips_via_overflow() {
        // A value far larger than the IvArray inline slot forces the backing
        // bucket into an overflow chain; it must round-trip byte-exactly.
        let path = tmp("large");
        let mut pg = Pager::open(&path, 64).unwrap();
        let a = IsArray::new(0);
        let big = vec![0xabu8; 4096 * 3 + 17];
        a.insert(&mut pg, b"huge", &big).unwrap();
        a.insert(&mut pg, b"small", b"x").unwrap();
        assert_eq!(a.get(&pg, b"huge").unwrap().unwrap(), big);
        assert_eq!(a.get(&pg, b"small").unwrap().as_deref(), Some(&b"x"[..]));
        // Shrinking back to a small value reclaims the overflow chain.
        a.insert(&mut pg, b"huge", b"tiny").unwrap();
        assert_eq!(a.get(&pg, b"huge").unwrap().as_deref(), Some(&b"tiny"[..]));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn persists_across_reopen() {
        let path = tmp("persist");
        {
            let mut pg = Pager::open(&path, 64).unwrap();
            let a = IsArray::new(0);
            for i in 0..1000u32 {
                a.insert(&mut pg, format!("k{i}").as_bytes(), &i.to_le_bytes())
                    .unwrap();
            }
            a.insert(&mut pg, b"big", &vec![7u8; 4096 * 2]).unwrap();
            pg.flush().unwrap();
        }
        let pg = Pager::open(&path, 64).unwrap();
        let a = IsArray::new(0);
        assert_eq!(a.get(&pg, b"k500").unwrap().unwrap(), 500u32.to_le_bytes());
        assert_eq!(a.get(&pg, b"big").unwrap().unwrap(), vec![7u8; 4096 * 2]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn two_arrays_share_a_pager_via_distinct_root_slots() {
        let path = tmp("two");
        let mut pg = Pager::open(&path, 64).unwrap();
        let ent = IsArray::new(0);
        let pred = IsArray::new(1);
        ent.insert(&mut pg, b"<e>", b"10").unwrap();
        pred.insert(&mut pg, b"<e>", b"20").unwrap(); // same key, independent array
        assert_eq!(ent.get(&pg, b"<e>").unwrap().as_deref(), Some(&b"10"[..]));
        assert_eq!(pred.get(&pg, b"<e>").unwrap().as_deref(), Some(&b"20"[..]));
        std::fs::remove_file(&path).ok();
    }
}
