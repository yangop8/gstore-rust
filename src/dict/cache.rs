//! Bounded caches of an out-of-core dictionary (contract C4).
//!
//! An out-of-core dictionary reads `id → string` and `string → id` from disk.
//! Both directions are cached here, each within a **byte budget**:
//!
//! * [`TermCache`]: `(space, id) → Arc<str>`, 64 MiB by default;
//! * [`KeyCache`]: `(space, key) → id`, 16 MiB by default.
//!
//! Each is split into 16 shards (one mutex each) and evicts with CLOCK (a
//! second chance for every entry used since the hand last passed). An entry
//! is charged its string's bytes plus a fixed overhead ([`ENTRY_OVERHEAD`]),
//! so the charged bytes bound what the cache keeps resident. Both caches
//! belong to one backend *generation* ([`DiskTermSource::generation`]): the
//! first access under a new generation empties them, since an id or a key may
//! have changed meaning (an intern, a free).
//!
//! ## Generations and racing reads
//! A reader that misses reads the backing store and then caches what it
//! read; a free may run in between. So a value is cached *at the generation
//! it was read under* ([`ClockCache::insert_at`]): every shard records the
//! generation its entries belong to, and an access first brings the shard to
//! the cache's newest generation (emptying it if it is older) under the
//! shard's lock. An insert carrying an older generation than the shard's is
//! dropped there, under the same lock, so a value read before a free can
//! never be cached after it — whatever order the free, the reader and the
//! readers that move the cache to the new generation run in (review disk#6,
//! conc#12, db#20). Generations passed to the `*_at` methods only grow.
//!
//! [`DiskTermSource::generation`]: super::DiskTermSource::generation

use std::borrow::Borrow;
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use super::Space;
use crate::query::engine::FxBuildHasher;

/// Default byte budget of the `id → string` cache.
pub const DEFAULT_TERM_CACHE_BYTES: usize = 64 << 20;
/// Default byte budget of the `key → id` cache.
pub const DEFAULT_KEY_CACHE_BYTES: usize = 16 << 20;
/// Bytes charged per entry on top of its string: the `Arc` header (16), the
/// map bucket (16), the CLOCK slot (32) and the allocator's rounding.
pub const ENTRY_OVERHEAD: usize = 72;
/// Shards per cache.
const SHARDS: usize = 16;

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
}

impl CacheStats {
    /// Bytes resident in both caches.
    pub fn resident_bytes(&self) -> usize {
        self.term_bytes + self.key_bytes
    }
}

/// One CLOCK slot.
struct Slot<K, V> {
    key: K,
    value: V,
    cost: u32,
    referenced: bool,
}

/// One shard: a map from key to ring position and the CLOCK ring.
struct Shard<K, V, S> {
    map: HashMap<K, usize, S>,
    ring: Vec<Option<Slot<K, V>>>,
    /// Empty ring positions, reused before the ring grows.
    holes: Vec<usize>,
    hand: usize,
    bytes: usize,
    /// The generation the entries belong to (see the module docs).
    generation: u64,
}

impl<K: Hash + Eq + Clone, V: Clone, S: BuildHasher + Default> Shard<K, V, S> {
    fn new() -> Shard<K, V, S> {
        Shard {
            map: HashMap::with_hasher(S::default()),
            ring: Vec::new(),
            holes: Vec::new(),
            hand: 0,
            bytes: 0,
            generation: 0,
        }
    }

    /// Drop every entry (the generation stays).
    fn clear(&mut self) {
        let generation = self.generation;
        *self = Shard::new();
        self.generation = generation;
    }

    /// Evict one entry by CLOCK, returning its cost; `None` if the shard is
    /// empty.
    fn evict_one(&mut self) -> Option<usize> {
        if self.map.is_empty() {
            return None;
        }
        loop {
            if self.hand >= self.ring.len() {
                self.hand = 0;
            }
            let pos = self.hand;
            self.hand += 1;
            match &mut self.ring[pos] {
                None => {}
                Some(slot) if slot.referenced => slot.referenced = false,
                Some(_) => {
                    let slot = self.ring[pos].take().expect("checked above");
                    self.map.remove(&slot.key);
                    self.bytes -= slot.cost as usize;
                    self.holes.push(pos);
                    return Some(slot.cost as usize);
                }
            }
        }
    }
}

/// A sharded, byte-bounded CLOCK cache. Values are cloned out (use cheap
/// clones: `Arc<str>`, ids). `S` hashes the keys (shard choice and the
/// shards' maps).
pub struct ClockCache<K, V, S = RandomState> {
    shards: Box<[Mutex<Shard<K, V, S>>]>,
    shard_budget: usize,
    hasher: S,
    generation: AtomicU64,
    entries: AtomicUsize,
    bytes: AtomicUsize,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    invalidations: AtomicU64,
}

impl<K: Hash + Eq + Clone, V: Clone, S: BuildHasher + Default> ClockCache<K, V, S> {
    /// A cache keeping at most `budget` charged bytes (split evenly between
    /// its shards; an entry larger than a shard's share is never cached).
    pub fn new(budget: usize) -> ClockCache<K, V, S> {
        ClockCache {
            shards: (0..SHARDS).map(|_| Mutex::new(Shard::new())).collect(),
            shard_budget: budget / SHARDS,
            hasher: S::default(),
            generation: AtomicU64::new(0),
            entries: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            invalidations: AtomicU64::new(0),
        }
    }

    /// The byte budget.
    pub fn budget(&self) -> usize {
        self.shard_budget * SHARDS
    }

    /// Lock shard `i`, brought to the cache's current generation: a shard
    /// whose entries belong to another generation is emptied first (under
    /// its lock, see the module docs).
    fn lock_shard(&self, i: usize) -> MutexGuard<'_, Shard<K, V, S>> {
        // A panic while a shard was locked leaves it consistent enough to
        // empty: recover by clearing it (it only holds copies).
        let mut g = match self.shards[i].lock() {
            Ok(g) => g,
            Err(poisoned) => {
                self.shards[i].clear_poison();
                let mut g = poisoned.into_inner();
                self.drop_entries(&mut g);
                g
            }
        };
        // Read under the shard's lock: a generation change stores the new
        // generation before it empties the shards, so an entry of an older
        // generation is never visible past this point.
        let current = self.generation.load(Ordering::Acquire);
        if g.generation != current {
            self.drop_entries(&mut g);
            g.generation = current;
        }
        g
    }

    /// Empty a locked shard, keeping the totals in step.
    fn drop_entries(&self, g: &mut Shard<K, V, S>) {
        self.entries.fetch_sub(g.map.len(), Ordering::Relaxed);
        self.bytes.fetch_sub(g.bytes, Ordering::Relaxed);
        g.clear();
    }

    fn shard<Q: Hash + ?Sized>(&self, key: &Q) -> MutexGuard<'_, Shard<K, V, S>> {
        self.lock_shard((self.hasher.hash_one(key) >> 40) as usize % SHARDS)
    }

    /// Bring every shard to the current generation (emptying the ones of
    /// another), so a generation change frees the memory at once.
    fn sync_all(&self) {
        for i in 0..SHARDS {
            drop(self.lock_shard(i));
        }
    }

    /// The generation the cache's entries belong to now.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Adopt `generation` if it is newer than the cache's (emptying it);
    /// an older one changes nothing. What the `*_at` methods do first.
    pub fn advance(&self, generation: u64) {
        // A plain load first: the common case (no change) writes nothing,
        // so concurrent readers do not contend on the counter's cache line.
        if self.generation.load(Ordering::Acquire) >= generation {
            return;
        }
        let previous = self.generation.fetch_max(generation, Ordering::AcqRel);
        if previous < generation {
            self.invalidations.fetch_add(1, Ordering::Relaxed);
            self.sync_all();
        }
    }

    /// Empty the cache if `generation` differs from the one its entries
    /// belong to (and adopt it, also when it is older). A cache whose
    /// readers race with changes of meaning uses the `*_at` methods instead
    /// (see the module docs).
    pub fn check_generation(&self, generation: u64) {
        if self.generation.swap(generation, Ordering::AcqRel) == generation {
            return;
        }
        self.invalidations.fetch_add(1, Ordering::Relaxed);
        self.sync_all();
    }

    /// Drop every entry.
    pub fn clear(&self) {
        for i in 0..SHARDS {
            let mut g = self.lock_shard(i);
            self.drop_entries(&mut g);
        }
    }

    /// Whether `key` is cached (neither marks it used nor counts as a hit or
    /// a miss).
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.shard(key).map.contains_key(key)
    }

    /// The cached value of `key` (marking it used).
    pub fn get<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let g = self.shard(key);
        self.lookup(g, key)
    }

    /// [`get`](Self::get) for a reader of `generation` (adopted first if
    /// newer): `None` when the cache has moved past it — what that reader
    /// may know is not what the cache holds.
    pub fn get_at<Q>(&self, key: &Q, generation: u64) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.advance(generation);
        let g = self.shard(key);
        if g.generation != generation {
            drop(g);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        self.lookup(g, key)
    }

    /// [`contains`](Self::contains) at `generation` (see
    /// [`get_at`](Self::get_at)).
    pub fn contains_at<Q>(&self, key: &Q, generation: u64) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.advance(generation);
        let g = self.shard(key);
        g.generation == generation && g.map.contains_key(key)
    }

    /// The value of `key` in the locked shard `g` (marking it used),
    /// counted as a hit or a miss.
    fn lookup<Q>(&self, mut g: MutexGuard<'_, Shard<K, V, S>>, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hit = match g.map.get(key) {
            Some(&pos) => {
                let slot = g.ring[pos].as_mut().expect("mapped slot is live");
                slot.referenced = true;
                Some(slot.value.clone())
            }
            None => None,
        };
        drop(g);
        let counter = if hit.is_some() {
            &self.hits
        } else {
            &self.misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
        hit
    }

    /// Cache `key → value`, charged `cost` bytes, evicting by CLOCK to stay
    /// within the budget. A value already cached is kept as is.
    pub fn insert(&self, key: K, value: V, cost: usize) {
        if cost > self.shard_budget || cost > u32::MAX as usize {
            return;
        }
        let g = self.shard(&key);
        self.insert_locked(g, key, value, cost);
    }

    /// [`insert`](Self::insert) a value read at `generation` (adopted first
    /// if newer): dropped when the cache has moved past it, checked under
    /// the shard's lock — so a value read before a change of meaning is
    /// never cached after it (see the module docs).
    pub fn insert_at(&self, key: K, value: V, cost: usize, generation: u64) {
        if cost > self.shard_budget || cost > u32::MAX as usize {
            return;
        }
        self.advance(generation);
        let g = self.shard(&key);
        if g.generation != generation {
            return;
        }
        self.insert_locked(g, key, value, cost);
    }

    fn insert_locked(&self, mut g: MutexGuard<'_, Shard<K, V, S>>, key: K, value: V, cost: usize) {
        if g.map.contains_key(&key) {
            return;
        }
        let (mut evicted, mut freed) = (0u64, 0usize);
        while g.bytes + cost > self.shard_budget {
            match g.evict_one() {
                Some(c) => {
                    evicted += 1;
                    freed += c;
                }
                None => break,
            }
        }
        let slot = Slot {
            key: key.clone(),
            value,
            cost: cost as u32,
            // A new entry gets no second chance until it is used again, so a
            // one-pass scan cannot flush the entries that are re-used.
            referenced: false,
        };
        let pos = match g.holes.pop() {
            Some(pos) => {
                g.ring[pos] = Some(slot);
                pos
            }
            None => {
                g.ring.push(Some(slot));
                g.ring.len() - 1
            }
        };
        g.map.insert(key, pos);
        g.bytes += cost;
        // The totals change under the shard's lock, so a concurrent clear of
        // this shard never subtracts more than was added.
        self.entries.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(cost, Ordering::Relaxed);
        if evicted > 0 {
            self.entries.fetch_sub(evicted as usize, Ordering::Relaxed);
            self.bytes.fetch_sub(freed, Ordering::Relaxed);
        }
        drop(g);
        if evicted > 0 {
            self.evictions.fetch_add(evicted, Ordering::Relaxed);
        }
    }

    /// Entries resident.
    pub fn len(&self) -> usize {
        self.entries.load(Ordering::Relaxed)
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes charged to the resident entries.
    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// `(hits, misses, evictions, invalidations)`.
    pub fn counters(&self) -> (u64, u64, u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
            self.evictions.load(Ordering::Relaxed),
            self.invalidations.load(Ordering::Relaxed),
        )
    }
}

/// `(space, id) → Arc<str>`: the out-of-core dictionary's id → string cache.
/// Its keys are ids, so it hashes with the engine's fast FxHash.
pub struct TermCache {
    inner: ClockCache<u64, Arc<str>, FxBuildHasher>,
}

fn term_key(space: Space, id: u32) -> u64 {
    (space as u64) << 32 | u64::from(id)
}

impl TermCache {
    /// A cache of at most `budget` charged bytes.
    pub fn new(budget: usize) -> TermCache {
        TermCache {
            inner: ClockCache::new(budget),
        }
    }

    /// See [`ClockCache::check_generation`].
    pub fn check_generation(&self, generation: u64) {
        self.inner.check_generation(generation)
    }

    /// The cached string of `id` in `space`.
    pub fn get(&self, space: Space, id: u32) -> Option<Arc<str>> {
        self.inner.get(&term_key(space, id))
    }

    /// Whether the string of `id` in `space` is cached (no counters move).
    pub fn contains(&self, space: Space, id: u32) -> bool {
        self.inner.contains(&term_key(space, id))
    }

    /// Cache the string of `id` in `space`.
    pub fn insert(&self, space: Space, id: u32, s: Arc<str>) {
        let cost = s.len() + ENTRY_OVERHEAD;
        self.inner.insert(term_key(space, id), s, cost)
    }

    /// See [`ClockCache::get_at`].
    pub fn get_at(&self, space: Space, id: u32, generation: u64) -> Option<Arc<str>> {
        self.inner.get_at(&term_key(space, id), generation)
    }

    /// See [`ClockCache::contains_at`].
    pub fn contains_at(&self, space: Space, id: u32, generation: u64) -> bool {
        self.inner.contains_at(&term_key(space, id), generation)
    }

    /// Cache the string of `id` in `space`, read at `generation` (see
    /// [`ClockCache::insert_at`]).
    pub fn insert_at(&self, space: Space, id: u32, s: Arc<str>, generation: u64) {
        let cost = s.len() + ENTRY_OVERHEAD;
        self.inner
            .insert_at(term_key(space, id), s, cost, generation)
    }

    /// The underlying cache.
    pub(crate) fn cache(&self) -> &ClockCache<u64, Arc<str>, FxBuildHasher> {
        &self.inner
    }

    /// Entries resident.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Bytes charged.
    pub fn bytes(&self) -> usize {
        self.inner.bytes()
    }

    /// The byte budget.
    pub fn budget(&self) -> usize {
        self.inner.budget()
    }

    /// `(hits, misses, evictions, invalidations)`.
    pub fn counters(&self) -> (u64, u64, u64, u64) {
        self.inner.counters()
    }
}

/// `(space, key) → id`: the out-of-core dictionary's key → id cache (one
/// cache per space, each with a third of the budget).
pub struct KeyCache {
    spaces: [ClockCache<Arc<str>, u32>; 3],
}

impl KeyCache {
    /// A cache of at most `budget` charged bytes.
    pub fn new(budget: usize) -> KeyCache {
        KeyCache {
            spaces: [
                ClockCache::new(budget / 3),
                ClockCache::new(budget / 3),
                ClockCache::new(budget / 3),
            ],
        }
    }

    fn of(&self, space: Space) -> &ClockCache<Arc<str>, u32> {
        &self.spaces[space as usize]
    }

    /// See [`ClockCache::check_generation`].
    pub fn check_generation(&self, generation: u64) {
        for c in &self.spaces {
            c.check_generation(generation);
        }
    }

    /// The cached id of `key` in `space`.
    pub fn get(&self, space: Space, key: &str) -> Option<u32> {
        self.of(space).get(key)
    }

    /// Cache `key → id` in `space`.
    pub fn insert(&self, space: Space, key: &str, id: u32) {
        let cost = key.len() + ENTRY_OVERHEAD;
        self.of(space).insert(Arc::from(key), id, cost)
    }

    /// See [`ClockCache::get_at`].
    pub fn get_at(&self, space: Space, key: &str, generation: u64) -> Option<u32> {
        self.of(space).get_at(key, generation)
    }

    /// Cache `key → id` in `space`, read at `generation` (see
    /// [`ClockCache::insert_at`]).
    pub fn insert_at(&self, space: Space, key: &str, id: u32, generation: u64) {
        let cost = key.len() + ENTRY_OVERHEAD;
        self.of(space)
            .insert_at(Arc::from(key), id, cost, generation)
    }

    /// Entries resident.
    pub fn len(&self) -> usize {
        self.spaces.iter().map(ClockCache::len).sum()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes charged.
    pub fn bytes(&self) -> usize {
        self.spaces.iter().map(ClockCache::bytes).sum()
    }

    /// The byte budget.
    pub fn budget(&self) -> usize {
        self.spaces.iter().map(ClockCache::budget).sum()
    }

    /// `(hits, misses, evictions, invalidations)` summed over the spaces
    /// (an invalidation counts once).
    pub fn counters(&self) -> (u64, u64, u64, u64) {
        let mut out = (0, 0, 0, 0);
        for c in &self.spaces {
            let (h, m, e, i) = c.counters();
            out.0 += h;
            out.1 += m;
            out.2 += e;
            out.3 = out.3.max(i);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stays_within_its_budget_and_keeps_hot_entries() {
        let budget = 1 << 16;
        let c: ClockCache<u64, Arc<str>> = ClockCache::new(budget);
        let hot: Arc<str> = Arc::from("hot".repeat(10));
        c.insert(0, Arc::clone(&hot), 30 + ENTRY_OVERHEAD);
        for i in 1..10_000u64 {
            // Touch the hot entry every round: CLOCK keeps it.
            assert_eq!(c.get(&0).as_deref(), Some(&*hot), "round {i}");
            c.insert(i, Arc::from(format!("value {i}")), 100);
            assert!(c.bytes() <= budget, "{} > {budget}", c.bytes());
        }
        let (_, _, evictions, _) = c.counters();
        assert!(evictions > 0);
        assert!(!c.is_empty() && c.len() < 10_000);
    }

    #[test]
    fn oversized_entries_are_not_cached() {
        let c: ClockCache<u64, u32> = ClockCache::new(SHARDS * 100);
        c.insert(1, 1, 101);
        assert_eq!(c.get(&1), None);
        c.insert(2, 2, 100);
        assert_eq!(c.get(&2), Some(2));
    }

    #[test]
    fn a_new_generation_empties_the_cache() {
        let t = TermCache::new(1 << 20);
        t.check_generation(1);
        t.insert(Space::Entity, 5, Arc::from("<e>"));
        t.insert(Space::Predicate, 5, Arc::from("<p>"));
        assert_eq!(t.get(Space::Entity, 5).as_deref(), Some("<e>"));
        assert_eq!(t.get(Space::Predicate, 5).as_deref(), Some("<p>"));
        t.check_generation(1);
        assert_eq!(t.cache().len(), 2);
        t.check_generation(2);
        assert_eq!(t.get(Space::Entity, 5), None);
        assert_eq!(t.cache().len(), 0);
        assert_eq!(t.cache().bytes(), 0);
        let k = KeyCache::new(1 << 20);
        k.insert(Space::Literal, "\"x\"", 7);
        assert_eq!(k.get(Space::Literal, "\"x\""), Some(7));
        assert_eq!(k.get(Space::Entity, "\"x\""), None, "spaces are separate");
        k.check_generation(9);
        assert!(k.is_empty());
    }

    /// Review disk#6 / db#20: a value read at an older generation is not
    /// cached once the cache moved on — whichever of the two happens first
    /// — and a reader of an older generation gets no answer from a newer one.
    #[test]
    fn a_value_read_at_an_older_generation_is_not_cached() {
        let c: ClockCache<u64, u32> = ClockCache::new(1 << 16);
        c.insert_at(1, 10, 8, 3);
        assert_eq!(c.get_at(&1, 3), Some(10));
        // The cache moves to generation 4 (a free), then the late insert of
        // a value read at 3 arrives: dropped.
        c.advance(4);
        assert!(c.is_empty());
        c.insert_at(2, 20, 8, 3);
        assert_eq!(c.get_at(&2, 4), None, "a stale value was cached");
        assert!(!c.contains_at(&2, 4));
        // A reader still at 3 gets no answer from generation 4's entries.
        c.insert_at(5, 50, 8, 4);
        assert_eq!(c.get_at(&5, 3), None);
        assert_eq!(c.get_at(&5, 4), Some(50));
        // An insert carrying a newer generation moves the cache there.
        c.insert_at(6, 60, 8, 7);
        assert_eq!(c.generation(), 7);
        assert_eq!(c.get_at(&5, 7), None, "emptied by the move");
        assert_eq!(c.get_at(&6, 7), Some(60));
        assert_eq!(c.len(), 1);
        assert_eq!(c.bytes(), 8);
        assert_eq!(c.counters().3, 3, "generation changes: 0 to 3, to 4, to 7");
    }

    #[test]
    fn concurrent_use_is_consistent() {
        let c = Arc::new(TermCache::new(1 << 14));
        let threads: Vec<_> = (0..4)
            .map(|t| {
                let c = Arc::clone(&c);
                std::thread::spawn(move || {
                    for i in 0..5_000u32 {
                        let id = (i * 7 + t) % 997;
                        match c.get(Space::Entity, id) {
                            Some(s) => assert_eq!(&*s, format!("<e{id}>")),
                            None => c.insert(Space::Entity, id, Arc::from(format!("<e{id}>"))),
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(c.cache().bytes() <= 1 << 14);
    }
}
