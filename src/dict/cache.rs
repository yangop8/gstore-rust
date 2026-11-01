//! Bounded caches of an out-of-core dictionary (contract C4).

/// Counters of an out-of-core dictionary's caches (see
/// [`Dictionary::cache_stats`](super::Dictionary::cache_stats)). For an
/// in-memory dictionary `term_entries` / `term_bytes` report its strings
/// (everything is resident) and the rest is zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Id → string entries resident.
    pub term_entries: usize,
    /// Bytes charged to them (string bytes plus a fixed per-entry overhead).
    pub term_bytes: usize,
    /// The id → string cache's byte budget.
    pub term_budget: usize,
    /// Key → id entries resident.
    pub key_entries: usize,
    /// Bytes charged to them.
    pub key_bytes: usize,
    /// The key → id cache's byte budget.
    pub key_budget: usize,
    /// Look-ups answered from a cache.
    pub hits: u64,
    /// Look-ups that read the backing store.
    pub misses: u64,
    /// Entries evicted to stay within a budget.
    pub evictions: u64,
    /// Times a generation change emptied the caches.
    pub invalidations: u64,
    /// Strings pinned by the legacy `&str` API (see
    /// [`Dictionary::id_to_string`](super::Dictionary::id_to_string)).
    pub legacy_pinned: usize,
}

impl CacheStats {
    /// Bytes resident in both caches.
    pub fn resident_bytes(&self) -> usize {
        self.term_bytes + self.key_bytes
    }
}
