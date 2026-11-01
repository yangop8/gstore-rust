//! A compact string → dense id map for one dictionary space.
//!
//! Ids are handed out sequentially from 0 in first-seen order, so the map is
//! also the id → string table: the key bytes of all terms are appended to
//! one arena, `offsets[id] .. offsets[id + 1]` delimit term `id`, and an
//! open-addressing table (linear probing, one `u64` per slot: a 32-bit hash
//! fingerprint and `id + 1`) finds a key's id. That is about `L + 8` bytes
//! per term plus 8–16 bytes of table: roughly `L + 26` bytes per term at
//! worst, against several pointers and a heap block per term for a
//! `HashMap<String, u32>`.

/// A key hash: 8 bytes at a time through a folded 64×64→128 multiply
/// (wyhash-style), finished with the murmur3 avalanche.
#[inline]
pub(crate) fn hash_key(b: &[u8]) -> u64 {
    const P0: u64 = 0xa076_1d64_78bd_642f;
    const P1: u64 = 0xe703_7ed1_a0b4_28db;
    #[inline]
    fn fold(a: u64, b: u64) -> u64 {
        let m = (a as u128).wrapping_mul(b as u128);
        (m as u64) ^ ((m >> 64) as u64)
    }
    let mut h = (b.len() as u64) ^ P0;
    let mut chunks = b.chunks_exact(16);
    for c in &mut chunks {
        let w0 = u64::from_le_bytes(c[0..8].try_into().unwrap());
        let w1 = u64::from_le_bytes(c[8..16].try_into().unwrap());
        h = fold(w0 ^ P1 ^ h, w1 ^ P0);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut t = [0u8; 16];
        t[..rem.len()].copy_from_slice(rem);
        let w0 = u64::from_le_bytes(t[0..8].try_into().unwrap());
        let w1 = u64::from_le_bytes(t[8..16].try_into().unwrap());
        h = fold(w0 ^ P1 ^ h, w1 ^ P0);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

/// The string → id map of one space (see the module docs).
#[derive(Debug)]
pub(crate) struct TermMap {
    arena: Vec<u8>,
    /// `offsets[id]` is where term `id` starts; one more entry than terms.
    offsets: Vec<u64>,
    /// Slots: `0` = empty, else `(fingerprint << 32) | (id + 1)`.
    table: Vec<u64>,
    mask: usize,
}

impl Default for TermMap {
    fn default() -> Self {
        TermMap::new()
    }
}

impl TermMap {
    pub(crate) fn new() -> TermMap {
        TermMap {
            arena: Vec::new(),
            offsets: vec![0],
            table: vec![0; 1024],
            mask: 1023,
        }
    }

    /// Number of terms.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    /// The key of term `id`.
    #[inline]
    pub(crate) fn key(&self, id: u32) -> &[u8] {
        let i = id as usize;
        &self.arena[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }

    /// The byte length of term `id`'s key.
    #[inline]
    pub(crate) fn key_len(&self, id: u32) -> usize {
        let i = id as usize;
        (self.offsets[i + 1] - self.offsets[i]) as usize
    }

    /// Heap bytes held (arena, offsets, table).
    pub(crate) fn heap_bytes(&self) -> usize {
        self.arena.capacity() + self.offsets.capacity() * 8 + self.table.capacity() * 8
    }

    /// The id of `key` (hashed as `h` by [`hash_key`]), if present.
    #[inline]
    pub(crate) fn find(&self, key: &[u8], h: u64) -> Option<u32> {
        let fp = h >> 32;
        let mut i = h as usize & self.mask;
        loop {
            let slot = self.table[i];
            if slot == 0 {
                return None;
            }
            if slot >> 32 == fp {
                let id = (slot as u32) - 1;
                if self.key(id) == key {
                    return Some(id);
                }
            }
            i = (i + 1) & self.mask;
        }
    }

    /// The id of `key`, inserting it with the next id when absent. Returns
    /// `(id, newly inserted)`.
    #[inline]
    pub(crate) fn intern(&mut self, key: &[u8], h: u64) -> (u32, bool) {
        let fp = h >> 32;
        let mut i = h as usize & self.mask;
        loop {
            let slot = self.table[i];
            if slot == 0 {
                break;
            }
            if slot >> 32 == fp {
                let id = (slot as u32) - 1;
                if self.key(id) == key {
                    return (id, false);
                }
            }
            i = (i + 1) & self.mask;
        }
        let id = self.len() as u32;
        self.arena.extend_from_slice(key);
        self.offsets.push(self.arena.len() as u64);
        self.table[i] = (fp << 32) | (u64::from(id) + 1);
        // Keep the load at most 3/4.
        if self.len() * 4 > self.table.len() * 3 {
            self.grow();
        }
        (id, true)
    }

    /// Double the table, re-hashing every key from the arena.
    #[cold]
    fn grow(&mut self) {
        let size = self.table.len() * 2;
        let mut table = vec![0u64; size];
        let mask = size - 1;
        for id in 0..self.len() as u32 {
            let h = hash_key(self.key(id));
            let mut i = h as usize & mask;
            while table[i] != 0 {
                i = (i + 1) & mask;
            }
            table[i] = ((h >> 32) << 32) | (u64::from(id) + 1);
        }
        self.table = table;
        self.mask = mask;
    }

    /// Release the lookup table (the id → key table stays): after encoding,
    /// only the keys are needed.
    pub(crate) fn drop_table(&mut self) {
        self.table = Vec::new();
        self.mask = 0;
    }

    /// The ids sorted by key bytes (the order of the `*2id` B+ trees).
    pub(crate) fn sorted_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = (0..self.len() as u32).collect();
        ids.sort_unstable_by(|&a, &b| self.key(a).cmp(self.key(b)));
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn ids_are_first_seen_and_lookups_agree_with_a_hashmap() {
        let mut m = TermMap::new();
        let mut reference: HashMap<Vec<u8>, u32> = HashMap::new();
        let mut order = Vec::new();
        for i in 0..20_000u32 {
            let key = format!("<http://example.org/{}>", (i * 7919) % 6001).into_bytes();
            let h = hash_key(&key);
            let (id, new) = m.intern(&key, h);
            let next = reference.len() as u32;
            let want = *reference.entry(key.clone()).or_insert(next);
            assert_eq!(id, want);
            assert_eq!(new, want == next);
            if new {
                order.push(key.clone());
            }
            assert_eq!(m.find(&key, h), Some(id));
        }
        assert_eq!(m.len(), reference.len());
        for (id, k) in order.iter().enumerate() {
            assert_eq!(m.key(id as u32), &k[..]);
            assert_eq!(m.key_len(id as u32), k.len());
        }
        assert_eq!(m.find(b"<absent>", hash_key(b"<absent>")), None);
        let sorted = m.sorted_ids();
        assert!(sorted.windows(2).all(|w| m.key(w[0]) < m.key(w[1])));
        // The empty key is a key like any other.
        let (e, new) = m.intern(b"", hash_key(b""));
        assert!(new);
        assert_eq!(m.key(e), b"");
        assert!(m.heap_bytes() > 0);
    }

    #[test]
    fn hash_spreads_common_prefixes() {
        let mut fps = std::collections::HashSet::new();
        let mut buckets = vec![0u32; 1024];
        let n = 100_000u32;
        for i in 0..n {
            let k = format!(
                "<http://www.Department{}.University{}.edu/Course{}>",
                i % 20,
                i / 20,
                i
            );
            let h = hash_key(k.as_bytes());
            fps.insert(h >> 32);
            buckets[(h & 1023) as usize] += 1;
        }
        assert!(
            fps.len() as u32 >= n - 10,
            "fingerprints collide: {}",
            fps.len()
        );
        let avg = n / 1024;
        assert!(
            buckets.iter().all(|&b| b > avg / 2 && b < avg * 2),
            "{buckets:?}"
        );
    }
}
