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
//! The index is a **trigram inverted index** over `(id, string)` pairs: every
//! 3-byte window of a string posts that string's id under that trigram. A
//! `CONTAINS(needle)` becomes the *intersection* of the postings of
//! `needle`'s trigrams: any string holding the needle must hold all of its
//! trigrams, so the intersection is a **sound superset** of the true matches
//! ([`contains_candidates`](StringIndex::contains_candidates)). An id may be
//! posted under more than one string (the candidates are then a superset for
//! each), and postings of a string that is gone are harmless (one more
//! candidate the exact check drops).
//!
//! It comes in two kinds:
//!
//! * **With strings** ([`new`](StringIndex::new), [`build`](StringIndex::build),
//!   [`insert`](StringIndex::insert)) — it also keeps `id → string`, so
//!   [`search_contains`](StringIndex::search_contains) and
//!   [`prefix`](StringIndex::prefix) answer exactly (candidates, then a check
//!   of each string) and [`string_of`](StringIndex::string_of) resolves.
//! * **Candidates only** ([`candidates_only`](StringIndex::candidates_only),
//!   [`build_candidates`](StringIndex::build_candidates)) — postings and the id
//!   list, no strings: what the query engine prunes with, because it checks
//!   every candidate with the real `FILTER` anyway (and resolves strings from
//!   its dictionary). Keeping a second copy of every term string there was
//!   about a third of the index (review gap-accel-soundness#3).
//!
//! Matching is byte-exact (case-sensitive), as SPARQL `CONTAINS`/`STRSTARTS`
//! are by default; a case-insensitive or regex filter can extract its mandatory
//! literal substrings and use [`contains_candidates`](StringIndex::contains_candidates)
//! on each, then verify with the real matcher.
//!
//! ## Wiring
//! The query engine uses it: `query::engine::dictionary_string_index` builds a
//! candidates-only index of an in-memory dictionary (each term under the
//! strings a substring filter may compare), a [`Database`](crate::Database)
//! caches it per dictionary state (extending it when terms are added), and
//! the BGP planner intersects [`contains_candidates`](StringIndex::contains_candidates)
//! of every mandatory needle into the variable's candidate set
//! (`query::candidates`). An out-of-core dictionary has no index (its filters
//! scan, still exactly).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Substring length used for the trigram inverted index.
const GRAM: usize = 3;

/// A secondary index over `(id, string)` pairs for substring / prefix lookups
/// (see the module docs for its two kinds).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StringIndex {
    /// `id → string`, for exact verification and id→string resolution; empty
    /// for a candidates-only index (see [`keeps_strings`](Self::keeps_strings)).
    by_id: HashMap<u32, String>,
    /// Packed trigram (`b0<<16 | b1<<8 | b2`) → sorted, unique ids whose string
    /// contains that 3-byte window.
    trigrams: HashMap<u32, Vec<u32>>,
    /// Every indexed id, ascending and unique (the superset a needle shorter
    /// than a trigram gets).
    ids: Vec<u32>,
    /// Whether `by_id` holds the strings.
    keeps_strings: bool,
}

impl Default for StringIndex {
    /// An empty index that keeps its strings (as [`new`](Self::new)).
    fn default() -> StringIndex {
        StringIndex {
            by_id: HashMap::new(),
            trigrams: HashMap::new(),
            ids: Vec::new(),
            keeps_strings: true,
        }
    }
}

impl StringIndex {
    /// An empty index that keeps its strings.
    pub fn new() -> StringIndex {
        StringIndex::default()
    }

    /// An empty candidates-only index: postings and ids, no strings (see the
    /// module docs).
    pub fn candidates_only() -> StringIndex {
        StringIndex {
            keeps_strings: false,
            ..StringIndex::default()
        }
    }

    /// Build an index that keeps its strings from `(id, string)` pairs in one
    /// pass (ids should be unique). Cheaper than repeated
    /// [`insert`](Self::insert) because the id list is sorted once at the end.
    pub fn build<I, S>(pairs: I) -> StringIndex
    where
        I: IntoIterator<Item = (u32, S)>,
        S: AsRef<str>,
    {
        let mut idx = StringIndex::new();
        let mut ids = Vec::new();
        for (id, s) in pairs {
            idx.index_grams(id, s.as_ref());
            idx.by_id.insert(id, s.as_ref().to_string());
            ids.push(id);
        }
        idx.add_ids(ids);
        idx.shrink();
        idx
    }

    /// Build a candidates-only index from `(id, string)` pairs in one pass:
    /// each string is read once and not kept (an id may come more than once,
    /// with different strings — it is posted under each).
    pub fn build_candidates<I, S>(pairs: I) -> StringIndex
    where
        I: IntoIterator<Item = (u32, S)>,
        S: AsRef<str>,
    {
        let mut idx = StringIndex::candidates_only();
        let mut ids = Vec::new();
        for (id, s) in pairs {
            idx.index_grams(id, s.as_ref());
            ids.push(id);
        }
        idx.add_ids(ids);
        idx.shrink();
        idx
    }

    /// Add one `(id, string)` pair, keeping every structure current. Re-adding
    /// an existing id keeps its old trigram postings (a superset still): an
    /// index with strings then checks against the new string only, so prefer
    /// [`build`](Self::build) for a fresh index.
    pub fn insert(&mut self, id: u32, s: &str) {
        self.index_grams(id, s);
        if self.keeps_strings {
            self.by_id.insert(id, s.to_string());
        }
        if let Err(pos) = self.ids.binary_search(&id) {
            self.ids.insert(pos, id);
        }
    }

    /// Post `id` under every trigram of `s`, without touching the id list or
    /// the strings (with [`add_ids`](Self::add_ids): a build or an extension
    /// in batches).
    pub(crate) fn post(&mut self, id: u32, s: &str) {
        self.index_grams(id, s);
    }

    /// Merge `ids` into the id list (any order, duplicates allowed): one sort
    /// of the new ids and one merge, however many there are.
    pub(crate) fn add_ids(&mut self, mut ids: Vec<u32>) {
        if ids.is_empty() {
            return;
        }
        ids.sort_unstable();
        ids.dedup();
        if self.ids.last().is_none_or(|&last| last < ids[0]) {
            self.ids.extend_from_slice(&ids);
            return;
        }
        let old = std::mem::take(&mut self.ids);
        let mut merged = Vec::with_capacity(old.len() + ids.len());
        let (mut i, mut j) = (0, 0);
        while i < old.len() && j < ids.len() {
            match old[i].cmp(&ids[j]) {
                std::cmp::Ordering::Less => {
                    merged.push(old[i]);
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    merged.push(ids[j]);
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    merged.push(old[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        merged.extend_from_slice(&old[i..]);
        merged.extend_from_slice(&ids[j..]);
        self.ids = merged;
    }

    /// Give back the growth slack of every list (a built index is kept: a
    /// database caches it).
    pub(crate) fn shrink(&mut self) {
        for list in self.trigrams.values_mut() {
            list.shrink_to_fit();
        }
        self.trigrams.shrink_to_fit();
        self.by_id.shrink_to_fit();
        self.ids.shrink_to_fit();
    }

    /// Whether this index keeps its strings ([`new`](Self::new) /
    /// [`build`](Self::build)) or holds candidates only
    /// ([`candidates_only`](Self::candidates_only) /
    /// [`build_candidates`](Self::build_candidates)).
    pub fn keeps_strings(&self) -> bool {
        self.keeps_strings
    }

    /// Number of indexed ids.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// An estimate of the heap bytes the index holds (map buckets at their
    /// capacity, strings and lists at theirs).
    pub fn heap_bytes(&self) -> usize {
        let bucket = |entry: usize| entry + 1; // one control byte per bucket
        self.by_id.capacity() * bucket(std::mem::size_of::<(u32, String)>())
            + self.by_id.values().map(String::capacity).sum::<usize>()
            + self.trigrams.capacity() * bucket(std::mem::size_of::<(u32, Vec<u32>)>())
            + self
                .trigrams
                .values()
                .map(|v| v.capacity() * 4)
                .sum::<usize>()
            + self.ids.capacity() * 4
    }

    /// The string for `id`, if indexed — `None` always for a candidates-only
    /// index.
    pub fn string_of(&self, id: u32) -> Option<&str> {
        self.by_id.get(&id).map(String::as_str)
    }

    /// A **sound superset** of the ids whose string contains `needle`, via
    /// trigram-posting intersection. For a `needle` shorter than `GRAM`, the
    /// trigram index cannot prune, so every id is returned (still a superset).
    /// The result is sorted and de-duplicated. Use this when a caller wants to
    /// intersect candidates into a join and verify later; for verified results
    /// use [`search_contains`](Self::search_contains).
    pub fn contains_candidates(&self, needle: &str) -> Vec<u32> {
        let nb = needle.as_bytes();
        if nb.len() < GRAM {
            return self.ids.clone();
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
    /// A candidates-only index has no strings to verify against: it returns
    /// the [candidates](Self::contains_candidates).
    pub fn search_contains(&self, needle: &str) -> Vec<u32> {
        let candidates = self.contains_candidates(needle);
        if !self.keeps_strings {
            return candidates;
        }
        candidates
            .into_iter()
            .filter(|id| self.by_id.get(id).is_some_and(|s| s.contains(needle)))
            .collect()
    }

    /// The ids whose string starts with `prefix`, sorted by id — exact: the
    /// trigram candidates of the prefix (every id for a prefix shorter than a
    /// trigram), each checked against its string. This is the accelerated form
    /// of a `STRSTARTS` / prefix filter. A candidates-only index has no
    /// strings to check: it returns the candidates.
    pub fn prefix(&self, prefix: &str) -> Vec<u32> {
        let candidates = self.contains_candidates(prefix);
        if !self.keeps_strings {
            return candidates;
        }
        candidates
            .into_iter()
            .filter(|id| self.by_id.get(id).is_some_and(|s| s.starts_with(prefix)))
            .collect()
    }

    // ---- internals --------------------------------------------------------

    fn index_grams(&mut self, id: u32, s: &str) {
        let b = s.as_bytes();
        if b.len() < GRAM {
            return;
        }
        for w in b.windows(GRAM) {
            let list = self.trigrams.entry(pack(w)).or_default();
            // Ids mostly come in ascending order: the push is the fast path.
            match list.last() {
                Some(&last) if last >= id => {
                    if let Err(pos) = list.binary_search(&id) {
                        list.insert(pos, id);
                    }
                }
                _ => list.push(id),
            }
        }
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
        assert_eq!(
            inc.prefix("<http://example.org/"),
            built.prefix("<http://example.org/")
        );
    }

    #[test]
    fn serde_roundtrip_preserves_lookups() {
        let idx = sample();
        let bytes = bincode::serialize(&idx).unwrap();
        let back: StringIndex = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.search_contains("alic"), idx.search_contains("alic"));
        assert_eq!(
            back.prefix("<http://example.org/"),
            idx.prefix("<http://example.org/")
        );
        assert_eq!(back.len(), idx.len());
    }

    /// review gap-accel-soundness#3: a candidates-only index answers the
    /// same candidates as one with strings, without a copy of the strings.
    #[test]
    fn a_candidates_only_index_prunes_alike_without_the_strings() {
        let pairs: Vec<(u32, String)> = (0..2000u32)
            .map(|i| (i, format!("<http://example.org/resource/node{i:05}>")))
            .collect();
        let full = StringIndex::build(pairs.iter().map(|(i, s)| (*i, s.as_str())));
        let lean = StringIndex::build_candidates(pairs.iter().map(|(i, s)| (*i, s.as_str())));
        assert!(full.keeps_strings() && !lean.keeps_strings());
        assert_eq!(lean.len(), full.len());
        for needle in ["node0", "e01", "01234", "zzz", "ab", "resource/node00042"] {
            assert_eq!(
                lean.contains_candidates(needle),
                full.contains_candidates(needle),
                "{needle}"
            );
        }
        assert_eq!(lean.string_of(7), None);
        assert_eq!(
            full.string_of(7),
            Some("<http://example.org/resource/node00007>")
        );
        // The strings (and the string-ordered id list) were a large share.
        assert!(
            lean.heap_bytes() * 4 < full.heap_bytes() * 3,
            "lean {} vs full {}",
            lean.heap_bytes(),
            full.heap_bytes()
        );
    }

    /// An id posted under two strings is a candidate for a needle of either
    /// (the query engine posts a typed literal under its lexical and its
    /// canonical form); extending an index merges the new ids in order.
    #[test]
    fn posting_more_strings_and_ids_keeps_the_superset() {
        let mut idx = StringIndex::build_candidates([(10u32, "12"), (20, "abc")]);
        idx.post(10, "0012");
        idx.post(5, "xyz5");
        idx.add_ids(vec![30, 5, 20]);
        idx.post(30, "0012");
        assert_eq!(idx.contains_candidates("001"), vec![10, 30]);
        assert_eq!(idx.contains_candidates("yz5"), vec![5]);
        // Short needles: every id, ascending and unique.
        assert_eq!(idx.contains_candidates("1"), vec![5, 10, 20, 30]);
        assert_eq!(idx.len(), 4);
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
