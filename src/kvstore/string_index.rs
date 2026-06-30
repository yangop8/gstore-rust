//! A secondary string index over dictionary strings, for accelerating
//! `FILTER(CONTAINS(...))`, regex, and prefix lookups.
//!
//! gStore keeps a `StringIndex` (`src/StringIndex/`) so the engine can resolve
//! and match dictionary strings without rescanning every term. This is the same
//! intent, specialised for *substring / prefix* acceleration: instead of the
//! query engine scanning all N dictionary strings for a `CONTAINS` / regex /
//! `STRSTARTS` filter (O(N · |string|)), it asks this index for a small
//! candidate set first.
//!
//! Two complementary structures over `(id, string)` pairs:
//!
//! * **Trigram inverted index** — every 3-byte window of a string posts that
//!   string's id under that trigram. A `CONTAINS(needle)` becomes the
//!   *intersection* of the postings of `needle`'s trigrams: any string holding
//!   the needle must hold all of its trigrams, so the intersection is a **sound
//!   superset** of the true matches. [`search_contains`](StringIndex::search_contains)
//!   then verifies each candidate exactly; [`contains_candidates`](StringIndex::contains_candidates)
//!   exposes the unverified superset for a query planner to intersect into a join.
//! * **Id list ordered by string** — supports exact, sound `STRSTARTS` /
//!   prefix-range lookups by binary search ([`prefix`](StringIndex::prefix)).
//!
//! Matching is byte-exact (case-sensitive), as SPARQL `CONTAINS`/`STRSTARTS`
//! are by default; a case-insensitive or regex filter can extract its mandatory
//! literal substrings and use [`contains_candidates`](StringIndex::contains_candidates)
//! on each, then verify with the real matcher.
//!
//! ## Wiring (follow-up)
//! This provides and tests the index structure only; hooking it into the query
//! planner (so a `FILTER` over a dictionary-backed variable consults the index
//! instead of scanning) is left as a follow-up, since the planner lives outside
//! the storage engine this module belongs to.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Substring length used for the trigram inverted index.
const GRAM: usize = 3;

/// A secondary index over `(id, string)` pairs for substring / prefix lookups.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StringIndex {
    /// `id → string`, for exact verification and id→string resolution.
    by_id: HashMap<u32, String>,
    /// Packed trigram (`b0<<16 | b1<<8 | b2`) → sorted, unique ids whose string
    /// contains that 3-byte window.
    trigrams: HashMap<u32, Vec<u32>>,
    /// All ids, kept ordered by their string, for prefix / range queries.
    ordered: Vec<u32>,
}

impl StringIndex {
    pub fn new() -> StringIndex {
        StringIndex::default()
    }

    /// Build an index from `(id, string)` pairs in one pass (ids should be
    /// unique). Cheaper than repeated [`insert`](Self::insert) because the
    /// ordered id list is sorted once at the end.
    pub fn build<I, S>(pairs: I) -> StringIndex
    where
        I: IntoIterator<Item = (u32, S)>,
        S: AsRef<str>,
    {
        let mut idx = StringIndex::new();
        for (id, s) in pairs {
            idx.index_grams(id, s.as_ref());
            idx.by_id.insert(id, s.as_ref().to_string());
            idx.ordered.push(id);
        }
        idx.sort_ordered();
        idx
    }

    /// Add one `(id, string)` pair, keeping every structure current. Re-adding
    /// an existing id overwrites its string but does not remove its old trigram
    /// postings, so prefer [`build`](Self::build) for a fresh index.
    pub fn insert(&mut self, id: u32, s: &str) {
        self.index_grams(id, s);
        let existed = self.by_id.insert(id, s.to_string()).is_some();
        if !existed {
            let key = s.to_string();
            let pos = self
                .ordered
                .partition_point(|oid| self.by_id[oid].as_str() < key.as_str());
            self.ordered.insert(pos, id);
        } else {
            // String may have changed; re-sort to restore the ordering invariant.
            self.sort_ordered();
        }
    }

    /// Number of indexed strings.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// The string for `id`, if indexed.
    pub fn string_of(&self, id: u32) -> Option<&str> {
        self.by_id.get(&id).map(String::as_str)
    }

    /// A **sound superset** of the ids whose string contains `needle`, via
    /// trigram-posting intersection. For a `needle` shorter than [`GRAM`], the
    /// trigram index cannot prune, so every id is returned (still a superset).
    /// The result is sorted and de-duplicated. Use this when a caller wants to
    /// intersect candidates into a join and verify later; for verified results
    /// use [`search_contains`](Self::search_contains).
    pub fn contains_candidates(&self, needle: &str) -> Vec<u32> {
        let nb = needle.as_bytes();
        if nb.len() < GRAM {
            return self.all_ids();
        }
        // Distinct trigrams of the needle, rarest posting list first so the
        // running intersection shrinks fast.
        let mut grams: Vec<u32> = Vec::new();
        for w in nb.windows(GRAM) {
            grams.push(pack(w));
        }
        grams.sort_unstable();
        grams.dedup();

        let mut postings: Vec<&Vec<u32>> = Vec::with_capacity(grams.len());
        for g in &grams {
            match self.trigrams.get(g) {
                Some(list) => postings.push(list),
                None => return Vec::new(), // a needle trigram is absent ⇒ no match
            }
        }
        postings.sort_by_key(|p| p.len());

        let mut acc: Vec<u32> = postings[0].clone();
        for p in &postings[1..] {
            acc = intersect_sorted(&acc, p);
            if acc.is_empty() {
                break;
            }
        }
        acc
    }

    /// The ids whose string actually contains `needle` (trigram-pruned, then
    /// verified). Sorted. This is the accelerated form of a `CONTAINS` filter.
    pub fn search_contains(&self, needle: &str) -> Vec<u32> {
        self.contains_candidates(needle)
            .into_iter()
            .filter(|id| {
                self.by_id
                    .get(id)
                    .is_some_and(|s| s.contains(needle))
            })
            .collect()
    }

    /// The ids whose string starts with `prefix`, by binary search over the
    /// string-ordered id list. Exact (not just a candidate set) and sorted by
    /// id. This is the accelerated form of a `STRSTARTS` / prefix filter.
    pub fn prefix(&self, prefix: &str) -> Vec<u32> {
        let lo = self
            .ordered
            .partition_point(|id| self.by_id[id].as_str() < prefix);
        let mut out = Vec::new();
        for &id in &self.ordered[lo..] {
            let s = &self.by_id[&id];
            if s.starts_with(prefix) {
                out.push(id);
            } else {
                break; // ordered: first non-match ends the prefix range
            }
        }
        out.sort_unstable();
        out
    }

    // ---- internals --------------------------------------------------------

    fn index_grams(&mut self, id: u32, s: &str) {
        let b = s.as_bytes();
        if b.len() < GRAM {
            return;
        }
        for w in b.windows(GRAM) {
            let list = self.trigrams.entry(pack(w)).or_default();
            if let Err(pos) = list.binary_search(&id) {
                list.insert(pos, id);
            }
        }
    }

    fn sort_ordered(&mut self) {
        // Sort ids by their string (then id) so the order is total and stable.
        let by_id = &self.by_id;
        self.ordered.sort_by(|a, b| {
            by_id[a].as_str().cmp(by_id[b].as_str()).then(a.cmp(b))
        });
    }

    fn all_ids(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.by_id.keys().copied().collect();
        v.sort_unstable();
        v
    }
}

/// Pack a 3-byte window into a `u32` trigram key.
fn pack(w: &[u8]) -> u32 {
    ((w[0] as u32) << 16) | ((w[1] as u32) << 8) | (w[2] as u32)
}

/// Intersect two ascending, de-duplicated id lists.
fn intersect_sorted(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StringIndex {
        StringIndex::build([
            (0u32, "<http://example.org/alice>"),
            (1, "<http://example.org/alicia>"),
            (2, "<http://example.org/bob>"),
            (3, "\"Alice in Wonderland\""),
            (4, "\"a\""),
            (5, "<http://other.test/carol>"),
        ])
    }

    #[test]
    fn contains_is_exact_and_sound() {
        let idx = sample();
        // "alic" matches alice(0), alicia(1) — entities, lowercase.
        let mut hit = idx.search_contains("alic");
        hit.sort_unstable();
        assert_eq!(hit, vec![0, 1]);

        // Candidates are a superset of the verified hits.
        let cands = idx.contains_candidates("alic");
        for id in &hit {
            assert!(cands.contains(id));
        }

        // Case-sensitive: capital "Alice" only matches the literal (id 3).
        assert_eq!(idx.search_contains("Alice"), vec![3]);

        // A needle present nowhere yields nothing.
        assert!(idx.search_contains("zzzzz").is_empty());
    }

    #[test]
    fn trigram_index_actually_prunes() {
        // A selective needle's candidate set is far smaller than the whole index.
        let mut idx = StringIndex::new();
        for i in 0..1000u32 {
            idx.insert(i, &format!("<http://ex/node{i:05}>"));
        }
        idx.insert(2000, "<http://ex/UNIQUEMARKER>");
        let cands = idx.contains_candidates("UNIQUEMARKER");
        assert_eq!(cands, vec![2000]);
        assert!(
            cands.len() < idx.len(),
            "trigram filter must prune below the full set"
        );
        assert_eq!(idx.search_contains("UNIQUEMARKER"), vec![2000]);
    }

    #[test]
    fn short_needle_returns_sound_superset() {
        let idx = sample();
        // "a" is shorter than a trigram: the index can't prune, so every id is a
        // candidate (a sound superset) and verification still gives exact hits.
        let cands = idx.contains_candidates("a");
        assert_eq!(cands.len(), idx.len());
        // Verified: strings actually containing "a".
        let hit = idx.search_contains("a");
        for id in &hit {
            assert!(idx.string_of(*id).unwrap().contains('a'));
        }
        // ids 0,1,2,3,4,5: alice,alicia,bob,Alice in Wonderland,"a",carol all contain 'a'.
        assert!(hit.contains(&4));
    }

    #[test]
    fn prefix_is_exact_and_ordered() {
        let idx = sample();
        let mut p = idx.prefix("<http://example.org/");
        p.sort_unstable();
        assert_eq!(p, vec![0, 1, 2]);

        assert_eq!(idx.prefix("<http://other"), vec![5]);
        assert!(idx.prefix("<http://nope").is_empty());
        // Whole-string prefix.
        assert_eq!(idx.prefix("\"Alice in Wonderland\""), vec![3]);
    }

    #[test]
    fn insert_matches_build() {
        let built = sample();
        let mut inc = StringIndex::new();
        for (id, s) in [
            (0u32, "<http://example.org/alice>"),
            (1, "<http://example.org/alicia>"),
            (2, "<http://example.org/bob>"),
            (3, "\"Alice in Wonderland\""),
            (4, "\"a\""),
            (5, "<http://other.test/carol>"),
        ] {
            inc.insert(id, s);
        }
        assert_eq!(inc.search_contains("alic"), built.search_contains("alic"));
        assert_eq!(inc.prefix("<http://example.org/"), built.prefix("<http://example.org/"));
    }

    #[test]
    fn serde_roundtrip_preserves_lookups() {
        let idx = sample();
        let bytes = bincode::serialize(&idx).unwrap();
        let back: StringIndex = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.search_contains("alic"), idx.search_contains("alic"));
        assert_eq!(back.prefix("<http://example.org/"), idx.prefix("<http://example.org/"));
        assert_eq!(back.len(), idx.len());
    }

    #[test]
    fn unicode_substring_bytewise() {
        // Byte-exact substring semantics over multibyte UTF-8.
        let idx = StringIndex::build([(0u32, "café société"), (1, "naïve")]);
        assert_eq!(idx.search_contains("café"), vec![0]);
        assert_eq!(idx.search_contains("ïve"), vec![1]);
        assert!(idx.search_contains("xyz").is_empty());
    }
}
