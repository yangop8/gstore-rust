//! Bidirectional string↔id dictionaries.
//!
//! Corresponds to gStore's `KVstore` dictionary trees:
//! `entity2id`/`id2entity`, `literal2id`/`id2literal`, `predicate2id`/`id2predicate`.
//!
//! Three independent id spaces are maintained (see [`crate::model::id`] and
//! [`Space`]): entities, literals, and predicates. Literals are stored at
//! internal index `i` but exposed with the public id `i + LITERAL_FIRST_ID`,
//! exactly matching gStore so that an object id alone reveals whether it is an
//! entity or a literal.
//!
//! Keys are [`Term::dict_key`] strings (full N-Triples surface syntax) so an IRI
//! `<foo>` never collides with a same-named literal `"foo"`.
//!
//! ## Two modes, one API (contract C4)
//!
//! * **In memory** — the three interners hold every string. Each string is
//!   stored **once**, as an `Arc<str>` the id → string vector owns and the
//!   string → id table points at by id. The interners are copy-on-write
//!   (`Arc<InternerData>`): cloning a dictionary is O(1), and the first
//!   mutation of a shared clone copies only the interner it changes. Interning
//!   an existing term never copies.
//! * **Out of core** — a [`DiskTermSource`] backend (the kvstore's `DiskDict`)
//!   resolves both directions on demand; the interners stay empty.
//!
//! The id → string API returns a [`TermStr`]: a borrowed `&str` for an
//! in-memory dictionary (no copy), a shared `Arc<str>` from a backend.
//! [`term`](Dictionary::term) / [`pred`](Dictionary::pred) (and the fallible
//! `try_*` forms) work for both modes; batch callers first call
//! [`prefetch`](Dictionary::prefetch) or [`resolve_many`](Dictionary::resolve_many)
//! so a backend reads its pages in id order. The legacy
//! [`id_to_string`](Dictionary::id_to_string) /
//! [`predicate_to_string`](Dictionary::predicate_to_string) `&str` forms are
//! for in-memory dictionaries; see their docs for the out-of-core behaviour.
//!
//! Read errors of a backend reach the running statement through
//! [`note_read_error`] (contract C2) when the method cannot return them.
//!
//! ## Persistence
//!
//! `dict.bin` keeps its bincode layout exactly: per interner a map
//! `string → index`, the `index → Option<string>` vector (holes are freed ids)
//! and the free stack. The map is now written in ascending id order, so the
//! same dictionary always encodes to the same bytes (it used to follow a
//! `HashMap`'s random order); older and newer binaries read each other's files
//! into equal dictionaries (see the mirror tests).
//!
//! ## Id-space bounds
//!
//! Every space is bounded exactly like gStore's `allocEntityID` /
//! `allocLiteralID` / `allocPredicateID`: at most [`LITERAL_FIRST_ID`]
//! (2,000,000,000) ids each, i.e. entity ids in `[0, LITERAL_FIRST_ID)`,
//! literal ids in `[LITERAL_FIRST_ID, 2 * LITERAL_FIRST_ID)` and predicate ids in
//! `[0, LITERAL_FIRST_ID)`. This keeps the entity/literal classification by id
//! range sound (the 2,000,000,000th entity would otherwise get a literal-range
//! id) and keeps every id clear of the `u32::MAX` sentinels and of the query
//! engine's synthetic-id range above `4_200_000_000`. The checked
//! [`try_intern_entity`](Dictionary::try_intern_entity) /
//! [`try_intern_literal`](Dictionary::try_intern_literal) /
//! [`try_intern_predicate`](Dictionary::try_intern_predicate) /
//! [`try_intern_term`](Dictionary::try_intern_term) return
//! [`GStoreError::Database`] once a space is exhausted (freed ids are still
//! reused); the infallible `intern_*` forms panic instead of silently
//! corrupting the classification. Out-of-core dictionaries that allocate ids
//! themselves can apply the same bounds with [`checked_entity_id`],
//! [`checked_literal_id`] and [`checked_predicate_id`].

pub mod cache;
pub mod prefix;

use std::borrow::Borrow;
use std::collections::hash_map::RandomState;
use std::collections::BTreeSet;
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher};
use std::ops::{ControlFlow, Deref};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use cache::CacheStats;

use crate::error::{GStoreError, Result};
use crate::model::id::{
    is_literal_id, EntityLiteralId, PredId, INVALID_ENTITY_LITERAL_ID, INVALID_PRED_ID,
    LITERAL_FIRST_ID,
};
use crate::model::Term;
use crate::store::read_scope::note_read_error;

/// Exclusive upper bound of the entity id space: entity ids are
/// `[0, ENTITY_ID_LIMIT)` (gStore: `limitID_entity < LITERAL_FIRST_ID`).
pub const ENTITY_ID_LIMIT: EntityLiteralId = LITERAL_FIRST_ID;

/// Exclusive upper bound of the literal *index* space: public literal ids are
/// `LITERAL_FIRST_ID + index` for `index` in `[0, LITERAL_INDEX_LIMIT)`, i.e.
/// `[2_000_000_000, 4_000_000_000)` (gStore: `limitID_literal < LITERAL_FIRST_ID`).
pub const LITERAL_INDEX_LIMIT: u32 = LITERAL_FIRST_ID;

/// Exclusive upper bound of the predicate id space: predicate ids are
/// `[0, PREDICATE_ID_LIMIT)` (gStore: `limitID_predicate < LITERAL_FIRST_ID`).
pub const PREDICATE_ID_LIMIT: PredId = LITERAL_FIRST_ID;

/// Validate a newly allocated entity id `next` (the allocator's counter).
/// `Err` once the entity space `[0, ENTITY_ID_LIMIT)` is exhausted.
pub fn checked_entity_id(next: u64) -> Result<EntityLiteralId> {
    if next < u64::from(ENTITY_ID_LIMIT) {
        Ok(next as EntityLiteralId)
    } else {
        Err(GStoreError::Database(format!(
            "entity id space exhausted: at most {ENTITY_ID_LIMIT} distinct entities \
             (IRIs / blank nodes) are supported (ids [0, {LITERAL_FIRST_ID}))"
        )))
    }
}

/// Validate a newly allocated literal *index* `next_index` and return its public
/// id (`LITERAL_FIRST_ID + next_index`). `Err` once the literal space is exhausted.
pub fn checked_literal_id(next_index: u64) -> Result<EntityLiteralId> {
    if next_index < u64::from(LITERAL_INDEX_LIMIT) {
        Ok(LITERAL_FIRST_ID + next_index as u32)
    } else {
        Err(GStoreError::Database(format!(
            "literal id space exhausted: at most {LITERAL_INDEX_LIMIT} distinct literals \
             are supported (ids [{LITERAL_FIRST_ID}, {}))",
            u64::from(LITERAL_FIRST_ID) + u64::from(LITERAL_INDEX_LIMIT)
        )))
    }
}

/// Validate a newly allocated predicate id `next`. `Err` once the predicate
/// space `[0, PREDICATE_ID_LIMIT)` is exhausted.
pub fn checked_predicate_id(next: u64) -> Result<PredId> {
    if next < u64::from(PREDICATE_ID_LIMIT) {
        Ok(next as PredId)
    } else {
        Err(GStoreError::Database(format!(
            "predicate id space exhausted: at most {PREDICATE_ID_LIMIT} distinct \
             predicates are supported"
        )))
    }
}

// ---- id spaces ----------------------------------------------------------------

/// One of the dictionary's three id spaces. Entity and literal ids share one
/// numbering (a literal's public id is offset by [`LITERAL_FIRST_ID`]);
/// predicates have their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Space {
    /// IRIs and blank nodes (ids `< LITERAL_FIRST_ID`).
    Entity,
    /// Literals (public ids `>= LITERAL_FIRST_ID`).
    Literal,
    /// Predicates (their own id space).
    Predicate,
}

impl Space {
    /// Every space, in a fixed order.
    pub const ALL: [Space; 3] = [Space::Entity, Space::Literal, Space::Predicate];

    /// The space of a subject/object id, by its range.
    pub fn of_id(id: EntityLiteralId) -> Space {
        if is_literal_id(id) {
            Space::Literal
        } else {
            Space::Entity
        }
    }

    /// The space a term goes to as a subject/object.
    pub fn of_term(t: &Term) -> Space {
        if t.is_literal() {
            Space::Literal
        } else {
            Space::Entity
        }
    }

    /// The first public id of the space (`LITERAL_FIRST_ID` for literals).
    pub fn first_id(self) -> u32 {
        match self {
            Space::Literal => LITERAL_FIRST_ID,
            Space::Entity | Space::Predicate => 0,
        }
    }

    /// A short lower-case name, for messages.
    pub fn name(self) -> &'static str {
        match self {
            Space::Entity => "entity",
            Space::Literal => "literal",
            Space::Predicate => "predicate",
        }
    }
}

// ---- TermStr ------------------------------------------------------------------

/// A dictionary string handed out by [`Dictionary::term`] /
/// [`Dictionary::pred`]: borrowed from an in-memory dictionary (no copy), or a
/// shared `Arc<str>` from an out-of-core backend (no `&str` can outlive the
/// backend's cache entry, and nothing is leaked to fake one).
///
/// It dereferences to `str`, compares, orders and hashes as its text, and
/// converts with [`into_string`](Self::into_string).
#[derive(Clone)]
pub enum TermStr<'a> {
    /// Borrowed from an in-memory dictionary.
    Borrowed(&'a str),
    /// Shared with an out-of-core backend's cache.
    Shared(Arc<str>),
}

impl<'a> TermStr<'a> {
    /// The text.
    #[inline]
    pub fn as_str(&self) -> &str {
        match self {
            TermStr::Borrowed(s) => s,
            TermStr::Shared(s) => s,
        }
    }

    /// The text as an owned `String` (one copy).
    #[inline]
    pub fn into_string(self) -> String {
        self.as_str().to_owned()
    }

    /// The text as a shared `Arc<str>` (a copy only for a borrowed string).
    pub fn into_shared(self) -> Arc<str> {
        match self {
            TermStr::Borrowed(s) => Arc::from(s),
            TermStr::Shared(s) => s,
        }
    }

    /// Detach from the dictionary's lifetime (a copy only for a borrowed string).
    pub fn into_owned(self) -> TermStr<'static> {
        TermStr::Shared(self.into_shared())
    }
}

impl Deref for TermStr<'_> {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for TermStr<'_> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for TermStr<'_> {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for TermStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for TermStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl PartialEq for TermStr<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for TermStr<'_> {}

impl PartialEq<str> for TermStr<'_> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for TermStr<'_> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for TermStr<'_> {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialOrd for TermStr<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TermStr<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for TermStr<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

impl<'a> From<&'a str> for TermStr<'a> {
    fn from(s: &'a str) -> TermStr<'a> {
        TermStr::Borrowed(s)
    }
}

impl From<Arc<str>> for TermStr<'_> {
    fn from(s: Arc<str>) -> Self {
        TermStr::Shared(s)
    }
}

impl From<TermStr<'_>> for String {
    fn from(t: TermStr<'_>) -> String {
        t.into_string()
    }
}

/// A borrowed string stays borrowed; a shared one is copied once.
impl<'a> From<TermStr<'a>> for std::borrow::Cow<'a, str> {
    fn from(t: TermStr<'a>) -> std::borrow::Cow<'a, str> {
        match t {
            TermStr::Borrowed(s) => std::borrow::Cow::Borrowed(s),
            TermStr::Shared(s) => std::borrow::Cow::Owned(s.to_string()),
        }
    }
}

// ---- out-of-core backend --------------------------------------------------------

/// An *out-of-core* dictionary backend: resolves str↔id against on-disk
/// structures on demand instead of holding every string in RAM (contract C4).
/// Implemented by the kvstore's `DiskDict` over `kvstore.kv`'s `*2id` B+trees
/// and `id2*` arrays, through a bounded string cache.
///
/// Every method takes `&self` and reports failures as `Err` — the
/// [`Dictionary`] front end turns an error it cannot return into
/// [`note_read_error`]. Ids are **public** ids: a literal id is offset by
/// [`LITERAL_FIRST_ID`], as everywhere else. `Send + Sync`, because a
/// dictionary is shared by reader threads.
pub trait DiskTermSource: fmt::Debug + Send + Sync {
    /// Term key → id in `space` (`None` if the key is not interned).
    fn lookup(&self, space: Space, key: &str) -> Result<Option<u32>>;

    /// Id → its key string in `space` (`None` for an id that names no term,
    /// e.g. a freed hole or one past the end).
    fn resolve(&self, space: Space, id: u32) -> Result<Option<Arc<str>>>;

    /// Intern `key` in `space`, returning its id (existing or new). A
    /// read-only backend refuses (the default).
    fn intern(&self, space: Space, key: &str) -> Result<u32> {
        let _ = key;
        Err(GStoreError::Database(format!(
            "this out-of-core dictionary is read-only: cannot intern a new {} term",
            space.name()
        )))
    }

    /// Free `id` in `space` for reuse (`Ok(false)` if it named no term). A
    /// read-only backend refuses (the default).
    fn free(&self, space: Space, id: u32) -> Result<bool> {
        let _ = id;
        Err(GStoreError::Database(format!(
            "this out-of-core dictionary is read-only: cannot free a {} id",
            space.name()
        )))
    }

    /// Resolve a batch: `ids` are ascending, deduplicated and all in
    /// `space`; the answer is aligned with them. A backend reads what it does
    /// not hold in id order (the default resolves one by one).
    fn resolve_sorted(&self, space: Space, ids: &[u32]) -> Result<Vec<Option<Arc<str>>>> {
        ids.iter().map(|&id| self.resolve(space, id)).collect()
    }

    /// Make the strings of `ids` (ascending, deduplicated, all in `space`)
    /// cheap to [`resolve`](Self::resolve) next — e.g. read them in id order
    /// into a cache. Advisory: the default does nothing.
    fn prefetch(&self, space: Space, ids: &[u32]) -> Result<()> {
        let _ = (space, ids);
        Ok(())
    }

    /// Visit every term of `space` with id `>= from` in ascending id order:
    /// `f(id, key)`, which may stop the walk with [`ControlFlow::Break`].
    fn for_each(
        &self,
        space: Space,
        from: u32,
        f: &mut dyn FnMut(u32, &str) -> ControlFlow<()>,
    ) -> Result<()>;

    /// Number of live terms in `space`.
    fn count(&self, space: Space) -> usize;

    /// Exclusive upper bound of the public ids allocated in `space` (holes
    /// included): `first_id(space) + allocated`.
    fn id_limit(&self, space: Space) -> u32;

    /// A counter that changes whenever an id of this backend may have changed
    /// meaning (a term interned or freed). Caches keyed by id or key are
    /// valid for one generation.
    fn generation(&self) -> u64 {
        0
    }

    /// Make the backend's own writes durable (default: nothing to do).
    fn sync(&self) -> Result<()> {
        Ok(())
    }

    /// The backend's cache counters (default: no cache).
    fn cache_stats(&self) -> CacheStats {
        CacheStats::default()
    }

    /// Live entity count (see [`count`](Self::count)).
    fn entity_num(&self) -> usize {
        self.count(Space::Entity)
    }

    /// Live literal count (see [`count`](Self::count)).
    fn literal_num(&self) -> usize {
        self.count(Space::Literal)
    }

    /// Live predicate count (see [`count`](Self::count)).
    fn predicate_num(&self) -> usize {
        self.count(Space::Predicate)
    }

    /// Number of id → string entries currently resident in RAM (for tests and
    /// metrics that assert only the looked-up subset is resident).
    fn resident_string_count(&self) -> usize {
        self.cache_stats().term_entries
    }
}

// ---- the in-memory interner ----------------------------------------------------

/// The string → index table of an interner: open addressing with linear
/// probing over `u64` slots, each `hash32 << 32 | (slot + 1)` (`0` = empty).
/// It stores no string: a probe compares the 32-bit hash first, then the
/// string the interner's vector holds at that slot — so every string lives
/// exactly once (in the vector), and the table costs 8 bytes per slot.
/// Deletion shifts the following run back (no tombstones). The hash is
/// SipHash with per-interner random keys, like the `HashMap` it replaces.
#[derive(Clone, Default)]
struct IdTable {
    slots: Vec<u64>,
    len: usize,
    hasher: RandomState,
}

/// The table grows when it would be more than 7/8 full.
const TABLE_LOAD_NUM: usize = 7;
const TABLE_LOAD_DEN: usize = 8;

impl IdTable {
    /// A table sized for `n` entries without growing.
    fn with_capacity(n: usize) -> IdTable {
        let mut t = IdTable::default();
        if n > 0 {
            t.slots = vec![0; Self::capacity_for(n)];
        }
        t
    }

    fn capacity_for(n: usize) -> usize {
        (n * TABLE_LOAD_DEN / TABLE_LOAD_NUM + 1)
            .next_power_of_two()
            .max(16)
    }

    fn hash(&self, s: &str) -> u32 {
        (self.hasher.hash_one(s) >> 32) as u32
    }

    fn mask(&self) -> usize {
        self.slots.len() - 1
    }

    /// Position of `s` in the table, and its vector slot.
    fn position(&self, s: &str, strings: &[Option<Arc<str>>]) -> Option<(usize, u32)> {
        if self.slots.is_empty() {
            return None;
        }
        let h = self.hash(s);
        let mask = self.mask();
        let mut i = h as usize & mask;
        loop {
            let e = self.slots[i];
            if e == 0 {
                return None;
            }
            if (e >> 32) as u32 == h {
                let slot = (e as u32).wrapping_sub(1);
                if strings.get(slot as usize).and_then(Option::as_deref) == Some(s) {
                    return Some((i, slot));
                }
            }
            i = (i + 1) & mask;
        }
    }

    fn find(&self, s: &str, strings: &[Option<Arc<str>>]) -> Option<u32> {
        self.position(s, strings).map(|(_, slot)| slot)
    }

    /// Add `s` (known to be absent) at vector slot `slot`.
    fn insert_new(&mut self, s: &str, slot: u32) {
        if (self.len + 1) * TABLE_LOAD_DEN > self.slots.len() * TABLE_LOAD_NUM {
            self.grow();
        }
        let h = self.hash(s);
        self.place(h, slot);
        self.len += 1;
    }

    /// Add `s` at vector slot `slot` unless an equal string is already
    /// indexed (`false` then): one probe of the run does both.
    fn insert_unique(&mut self, s: &str, slot: u32, strings: &[Option<Arc<str>>]) -> bool {
        if (self.len + 1) * TABLE_LOAD_DEN > self.slots.len() * TABLE_LOAD_NUM {
            self.grow();
        }
        let h = self.hash(s);
        let mask = self.mask();
        let mut i = h as usize & mask;
        loop {
            let e = self.slots[i];
            if e == 0 {
                self.slots[i] = (u64::from(h) << 32) | (u64::from(slot) + 1);
                self.len += 1;
                return true;
            }
            if (e >> 32) as u32 == h {
                let other = (e as u32).wrapping_sub(1);
                if strings.get(other as usize).and_then(Option::as_deref) == Some(s) {
                    return false;
                }
            }
            i = (i + 1) & mask;
        }
    }

    fn place(&mut self, h: u32, slot: u32) {
        let mask = self.mask();
        let mut i = h as usize & mask;
        while self.slots[i] != 0 {
            i = (i + 1) & mask;
        }
        self.slots[i] = (u64::from(h) << 32) | (u64::from(slot) + 1);
    }

    fn grow(&mut self) {
        let cap = (self.slots.len() * 2).max(Self::capacity_for(self.len + 1));
        let old = std::mem::replace(&mut self.slots, vec![0; cap]);
        for e in old.into_iter().filter(|&e| e != 0) {
            self.place((e >> 32) as u32, (e as u32) - 1);
        }
    }

    /// Remove `s`, returning its vector slot.
    fn remove(&mut self, s: &str, strings: &[Option<Arc<str>>]) -> Option<u32> {
        let (mut i, slot) = self.position(s, strings)?;
        // Knuth's algorithm R: shift every later entry of the run whose home
        // position does not lie cyclically in (i, j] back into the hole.
        let mask = self.mask();
        let mut j = i;
        loop {
            j = (j + 1) & mask;
            let e = self.slots[j];
            if e == 0 {
                break;
            }
            let home = (e >> 32) as usize & mask;
            let stays = if i <= j {
                i < home && home <= j
            } else {
                i < home || home <= j
            };
            if !stays {
                self.slots[i] = e;
                i = j;
            }
        }
        self.slots[i] = 0;
        self.len -= 1;
        Some(slot)
    }

    /// Bytes held by the table.
    fn heap_bytes(&self) -> usize {
        self.slots.capacity() * 8
    }
}

/// The shared state of one interner (see [`Interner`]).
#[derive(Clone, Default)]
struct InternerData {
    /// `index - base → Some(string)`; a `None` slot is a freed (or retired)
    /// hole. Never ends in a free hole: freeing the last index trims the
    /// vector (and every free hole that becomes last with it).
    strings: Vec<Option<Arc<str>>>,
    /// `string → index - base`, by reference into `strings`.
    table: IdTable,
    /// Reclaimed indices available for reuse (gStore's `freelist`), reused
    /// **lowest first**. Persisted as the same `seq` of `u32` the former LIFO
    /// stack used (ascending now).
    free: BTreeSet<u32>,
    /// Indices whose term was removed but which may not be reused yet: a
    /// [`DictPin`] older than the retirement still holds them (see
    /// [`Dictionary::retire`]). `(index, retire stamp)`. Not persisted as
    /// such: a saved dictionary lists them with the free indices (no pin
    /// survives a reopen).
    retired: Vec<(u32, u64)>,
    /// The index stored at `strings[0]`. Always `0` except in a dictionary whose
    /// counters a test advanced with [`Dictionary::debug_advance_id_counters`]
    /// to exercise the id-space bounds without allocating billions of strings.
    /// Not persisted (so the on-disk format is unchanged).
    base: u32,
}

impl InternerData {
    #[inline]
    fn get(&self, s: &str) -> Option<u32> {
        self.table
            .find(s, &self.strings)
            .map(|slot| self.base + slot)
    }

    #[inline]
    fn resolve(&self, idx: u32) -> Option<&Arc<str>> {
        let slot = idx.checked_sub(self.base)? as usize;
        self.strings.get(slot).and_then(Option::as_ref)
    }

    /// Intern `s` (known to be absent): reuse the lowest freed index, else
    /// extend.
    fn insert_new(&mut self, s: &str, check: impl Fn(u64) -> Result<u32>) -> Result<u32> {
        // Every free index names a hole below the end (the invariant the
        // normalization at load and `release` keep); one that does not is
        // dropped rather than overwriting a term.
        while let Some(reused) = self.free.pop_first() {
            let Some(slot) = reused.checked_sub(self.base) else {
                continue;
            };
            if matches!(self.strings.get(slot as usize), Some(None)) {
                self.strings[slot as usize] = Some(Arc::from(s));
                self.table.insert_new(s, slot);
                return Ok(reused);
            }
        }
        let id = check(u64::from(self.base) + self.strings.len() as u64)?;
        let slot = id - self.base;
        self.strings.push(Some(Arc::from(s)));
        self.table.insert_new(s, slot);
        Ok(id)
    }

    /// Remove the term at `idx` and park the index as retired (stamp
    /// `stamp`); `false` if `idx` names no term.
    fn retire(&mut self, idx: u32, stamp: u64) -> bool {
        let Some(slot) = idx.checked_sub(self.base) else {
            return false;
        };
        let Some(s) = self.strings.get(slot as usize).cloned().flatten() else {
            return false;
        };
        self.table.remove(&s, &self.strings);
        self.strings[slot as usize] = None;
        self.retired.push((idx, stamp));
        true
    }

    /// Move every retired index no pin holds (`oldest_pin`: the stamp of the
    /// oldest live pin) to the free set, trimming the end. Returns how many.
    fn release(&mut self, oldest_pin: Option<u64>) -> usize {
        let before = self.retired.len();
        let mut freed = Vec::new();
        self.retired.retain(|&(idx, stamp)| {
            if oldest_pin.is_some_and(|p| p < stamp) {
                return true;
            }
            freed.push(idx);
            false
        });
        for idx in freed {
            self.free.insert(idx);
        }
        self.trim();
        before - self.retired.len()
    }

    /// Drop trailing free holes (the cascade: each trimmed hole may expose
    /// another free one). A retired hole stops it: its index is still held.
    fn trim(&mut self) {
        while let Some(None) = self.strings.last() {
            let idx = self.base + (self.strings.len() as u32 - 1);
            if !self.free.remove(&idx) {
                break;
            }
            self.strings.pop();
        }
    }

    /// Live `(index, string)` pairs (skips freed holes), in id order.
    fn entries(&self) -> impl Iterator<Item = (u32, &Arc<str>)> {
        let base = self.base;
        self.strings
            .iter()
            .enumerate()
            .filter_map(move |(i, s)| s.as_ref().map(|s| (base + i as u32, s)))
    }

    /// The persisted free list: the free and the retired indices, ascending.
    fn persisted_free(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.free.iter().copied().collect();
        if !self.retired.is_empty() {
            v.extend(self.retired.iter().map(|&(i, _)| i));
            v.sort_unstable();
        }
        v
    }

    /// Bytes this interner holds on the heap (strings counted with their
    /// `Arc` header).
    fn heap_bytes(&self) -> usize {
        self.strings.capacity() * std::mem::size_of::<Option<Arc<str>>>()
            + self.table.heap_bytes()
            + self.free.len() * 8
            + self.retired.capacity() * 16
            + self
                .strings
                .iter()
                .flatten()
                .map(|s| s.len() + 16)
                .sum::<usize>()
    }
}

/// One space of an in-memory dictionary: `string → index` and `index →
/// string`, each string stored once.
///
/// Freed ids are *reclaimed*: removing a string leaves a hole (`None`) and
/// puts the index in a free set, mirroring gStore's per-space `freelist` +
/// `limitID`. The next [`try_intern`](Self::try_intern) of a brand-new
/// string takes the **lowest** freed index before extending; freeing the
/// highest index shrinks the space instead (with every free hole below it
/// that becomes the end). A removed term's index may first be *retired*:
/// held until no [`DictPin`] older than the removal remains.
///
/// Copy-on-write: clones share one `InternerData`; a mutation of a shared
/// interner copies it first (`Arc::make_mut`). Look-ups and re-interning an
/// existing string never copy.
#[derive(Clone, Default)]
struct Interner {
    data: Arc<InternerData>,
}

impl Interner {
    /// Intern `s`, returning its index and whether it is new.
    /// Re-interning is idempotent (and never copies a shared interner);
    /// otherwise the lowest freed index is reused before the id space is
    /// extended. `check` validates (and may reject) a brand-new index against
    /// the owning space's bound; a reused index is always in range.
    fn try_intern(&mut self, s: &str, check: impl Fn(u64) -> Result<u32>) -> Result<(u32, bool)> {
        if let Some(id) = self.data.get(s) {
            return Ok((id, false));
        }
        let id = Arc::make_mut(&mut self.data).insert_new(s, check)?;
        Ok((id, true))
    }

    /// Look up the index for `s`, if interned.
    #[inline]
    fn get(&self, s: &str) -> Option<u32> {
        self.data.get(s)
    }

    /// Resolve an index back to its string (`None` for a hole).
    #[inline]
    fn resolve(&self, idx: u32) -> Option<&str> {
        self.data.resolve(idx).map(|s| &**s)
    }

    /// Retire index `idx` (see [`InternerData::retire`]). A shared interner
    /// is copied only when `idx` names a term.
    fn retire(&mut self, idx: u32, stamp: u64) -> bool {
        if self.data.resolve(idx).is_none() {
            return false;
        }
        Arc::make_mut(&mut self.data).retire(idx, stamp)
    }

    /// Release the retired indices no pin holds; copies a shared interner
    /// only when one is releasable.
    fn release(&mut self, oldest_pin: Option<u64>) -> usize {
        let releasable = self
            .data
            .retired
            .iter()
            .any(|&(_, stamp)| oldest_pin.is_none_or(|p| p >= stamp));
        if !releasable {
            return 0;
        }
        Arc::make_mut(&mut self.data).release(oldest_pin)
    }

    /// Number of *live* (non-freed) strings.
    fn len(&self) -> usize {
        self.data.table.len
    }

    /// Number of free (reusable) indices.
    fn free_len(&self) -> usize {
        self.data.free.len()
    }

    /// Number of retired indices (waiting for a pin).
    fn retired_len(&self) -> usize {
        self.data.retired.len()
    }

    /// All live interned strings (skips freed holes), in id order.
    fn strings(&self) -> impl Iterator<Item = &str> {
        self.data.strings.iter().filter_map(Option::as_deref)
    }

    /// Live `(index, string)` pairs (skips freed holes), in id order.
    fn entries(&self) -> impl Iterator<Item = (u32, &str)> {
        self.data.entries().map(|(i, s)| (i, &**s))
    }

    /// One past the highest index allocated (interior holes included).
    fn index_limit(&self) -> u64 {
        u64::from(self.data.base) + self.data.strings.len() as u64
    }

    /// Visit live `(index, string)` pairs with index `>= from`, in order.
    fn for_each_from(&self, from: u32, f: &mut dyn FnMut(u32, &str) -> ControlFlow<()>) {
        let d = &*self.data;
        let start = from.saturating_sub(d.base) as usize;
        for (i, s) in d.strings.iter().enumerate().skip(start) {
            if let Some(s) = s {
                if f(d.base + i as u32, s).is_break() {
                    return;
                }
            }
        }
    }

    /// Build an interner holding exactly `slots` (index → term; `None` is a
    /// hole): holes become free, trailing holes are trimmed, a term stored
    /// twice is refused.
    fn from_slots<S: AsRef<str>>(slots: Vec<Option<S>>) -> std::result::Result<Interner, String> {
        let mut strings: Vec<Option<Arc<str>>> = Vec::with_capacity(slots.len());
        let mut live = 0usize;
        for s in slots {
            if s.is_some() {
                live += 1;
            }
            strings.push(s.map(|s| Arc::from(s.as_ref())));
        }
        Interner::assemble(strings, live, &[])
    }

    /// The common tail of loading: index the strings, check them and the
    /// persisted free list, and normalize (every hole free, trailing holes
    /// trimmed).
    fn assemble(
        mut strings: Vec<Option<Arc<str>>>,
        live: usize,
        persisted_free: &[u32],
    ) -> std::result::Result<Interner, String> {
        // A free list naming an index twice is corrupt (no writer ever
        // produced one); entries naming a live index or one past the end
        // are ignored, as the former reader skipped them.
        if !persisted_free.windows(2).all(|w| w[0] < w[1]) {
            let mut sorted = persisted_free.to_vec();
            sorted.sort_unstable();
            if let Some(w) = sorted.windows(2).find(|w| w[0] == w[1]) {
                return Err(format!(
                    "corrupt dictionary: index {} is listed as free twice",
                    w[0]
                ));
            }
        }
        while let Some(None) = strings.last() {
            strings.pop();
        }
        strings.shrink_to_fit();
        let mut table = IdTable::with_capacity(live);
        let mut free = BTreeSet::new();
        for (slot, s) in strings.iter().enumerate() {
            match s {
                Some(s) => {
                    if !table.insert_unique(s, slot as u32, &strings) {
                        return Err(format!(
                            "corrupt dictionary: term {s:?} is stored under two ids"
                        ));
                    }
                }
                None => {
                    free.insert(slot as u32);
                }
            }
        }
        Ok(Interner {
            data: Arc::new(InternerData {
                strings,
                table,
                free,
                retired: Vec::new(),
                base: 0,
            }),
        })
    }

    /// Whether two interners share their data (no copy happened).
    #[cfg(test)]
    fn shares_with(&self, other: &Interner) -> bool {
        Arc::ptr_eq(&self.data, &other.data)
    }
}

impl fmt::Debug for Interner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Interner")
            .field("live", &self.len())
            .field("allocated", &self.data.strings.len())
            .field("free", &self.data.free.len())
            .field("retired", &self.data.retired.len())
            .finish()
    }
}

// ---- dict.bin encoding (hand-written, byte-identical to the derived one) ------

/// Field names of the persisted interner, in their persisted order.
const INTERNER_FIELDS: &[&str] = &["forward", "backward", "free"];

impl Serialize for Interner {
    /// The derived encoding of the former `{ forward: HashMap<String, u32>,
    /// backward: Vec<Option<String>>, free: Vec<u32> }`, with the map's
    /// entries in ascending id order (the derived map followed hash order).
    fn serialize<S: Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        struct Forward<'a>(&'a InternerData);
        impl Serialize for Forward<'_> {
            fn serialize<S: Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
                let mut m = ser.serialize_map(Some(self.0.table.len))?;
                for (id, s) in self.0.entries() {
                    m.serialize_entry(&**s, &id)?;
                }
                m.end()
            }
        }
        struct Backward<'a>(&'a InternerData);
        impl Serialize for Backward<'_> {
            fn serialize<S: Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
                let mut q = ser.serialize_seq(Some(self.0.strings.len()))?;
                for s in &self.0.strings {
                    q.serialize_element(&s.as_deref())?;
                }
                q.end()
            }
        }
        let d = &*self.data;
        let mut st = ser.serialize_struct("Interner", 3)?;
        st.serialize_field("forward", &Forward(d))?;
        st.serialize_field("backward", &Backward(d))?;
        st.serialize_field("free", &d.persisted_free())?;
        st.end()
    }
}

/// Assembles an interner from the persisted forward map and backward vector,
/// sharing one `Arc<str>` per string. The forward entries are checked against
/// the backward vector: a file whose two halves disagree is refused.
struct Assembler {
    /// Forward entries `(string, index)`, sorted by index.
    forward: Vec<(Option<Arc<str>>, u32)>,
    pos: usize,
    strings: Vec<Option<Arc<str>>>,
}

impl Assembler {
    fn new(mut forward: Vec<(Option<Arc<str>>, u32)>) -> std::result::Result<Assembler, String> {
        if !forward.windows(2).all(|w| w[0].1 < w[1].1) {
            forward.sort_unstable_by_key(|e| e.1);
            if let Some(w) = forward.windows(2).find(|w| w[0].1 == w[1].1) {
                return Err(format!(
                    "corrupt dictionary: two terms map to index {}",
                    w[0].1
                ));
            }
        }
        Ok(Assembler {
            forward,
            pos: 0,
            strings: Vec::new(),
        })
    }

    fn reserve(&mut self, hint: Option<usize>) {
        // Trust the length prefix only as far as the forward map vouches for
        // it (a corrupt prefix must not ask for exabytes).
        let cap = hint
            .unwrap_or(0)
            .min(self.forward.len().saturating_mul(2) + 1024);
        self.strings.reserve_exact(cap);
    }

    fn push(&mut self, s: Option<&str>) -> std::result::Result<(), String> {
        let i = self.strings.len() as u64;
        let fwd = self
            .forward
            .get_mut(self.pos)
            .filter(|(_, id)| u64::from(*id) == i);
        match (s, fwd) {
            (Some(s), Some((arc, _))) if arc.as_deref() == Some(s) => {
                self.strings.push(arc.take());
                self.pos += 1;
                Ok(())
            }
            (None, None) => {
                self.strings.push(None);
                Ok(())
            }
            _ => Err(format!(
                "corrupt dictionary: forward and backward maps disagree at index {i}"
            )),
        }
    }

    /// Normalize on the way in (`serde(from)` semantics): every hole is free
    /// whatever the persisted list says, trailing holes are trimmed, a free
    /// list naming an index twice or a term stored under two indices is
    /// refused.
    fn finish(self, free: Vec<u32>) -> std::result::Result<Interner, String> {
        if self.pos != self.forward.len() {
            return Err(format!(
                "corrupt dictionary: index {} is mapped but past the end ({})",
                self.forward[self.pos].1,
                self.strings.len()
            ));
        }
        let live = self.forward.len();
        Interner::assemble(self.strings, live, &free)
    }
}

/// Deserializes a string straight into an `Arc<str>` (one allocation).
struct ArcStrSeed;

impl<'de> DeserializeSeed<'de> for ArcStrSeed {
    type Value = Arc<str>;
    fn deserialize<D: Deserializer<'de>>(self, de: D) -> std::result::Result<Arc<str>, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Arc<str>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<Arc<str>, E> {
                Ok(Arc::from(s))
            }
        }
        de.deserialize_str(V)
    }
}

/// The persisted forward map, as `(string, index)` entries.
struct ForwardSeed;

impl<'de> DeserializeSeed<'de> for ForwardSeed {
    type Value = Vec<(Option<Arc<str>>, u32)>;
    fn deserialize<D: Deserializer<'de>>(
        self,
        de: D,
    ) -> std::result::Result<Self::Value, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Vec<(Option<Arc<str>>, u32)>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map from dictionary key to index")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut m: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut out = Vec::with_capacity(m.size_hint().unwrap_or(0).min(1 << 16));
                while let Some(k) = m.next_key_seed(ArcStrSeed)? {
                    let id: u32 = m.next_value()?;
                    out.push((Some(k), id));
                }
                Ok(out)
            }
        }
        de.deserialize_map(V)
    }
}

/// The persisted backward vector, streamed into an [`Assembler`] (each string
/// is compared with the forward map's copy, which is kept; nothing else is
/// allocated per string).
struct BackwardSeed<'a>(&'a mut Assembler);

impl<'de> DeserializeSeed<'de> for BackwardSeed<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, de: D) -> std::result::Result<(), D::Error> {
        struct Elem<'a>(&'a mut Assembler);
        impl<'de> DeserializeSeed<'de> for Elem<'_> {
            type Value = ();
            fn deserialize<D: Deserializer<'de>>(self, de: D) -> std::result::Result<(), D::Error> {
                de.deserialize_option(self)
            }
        }
        impl<'de> Visitor<'de> for Elem<'_> {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an optional dictionary key")
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<(), E> {
                self.0.push(None).map_err(E::custom)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<(), E> {
                self.visit_none()
            }
            fn visit_some<D: Deserializer<'de>>(self, de: D) -> std::result::Result<(), D::Error> {
                de.deserialize_str(self)
            }
            fn visit_str<E: de::Error>(self, s: &str) -> std::result::Result<(), E> {
                self.0.push(Some(s)).map_err(E::custom)
            }
        }
        struct V<'a>(&'a mut Assembler);
        impl<'de> Visitor<'de> for V<'_> {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a sequence of optional dictionary keys")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut q: A) -> std::result::Result<(), A::Error> {
                self.0.reserve(q.size_hint());
                while q.next_element_seed(Elem(&mut *self.0))?.is_some() {}
                Ok(())
            }
        }
        de.deserialize_seq(V(self.0))
    }
}

impl<'de> Deserialize<'de> for Interner {
    fn deserialize<D: Deserializer<'de>>(de: D) -> std::result::Result<Interner, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Interner;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a dictionary interner")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut q: A,
            ) -> std::result::Result<Interner, A::Error> {
                let forward = q
                    .next_element_seed(ForwardSeed)?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let mut asm = Assembler::new(forward).map_err(de::Error::custom)?;
                q.next_element_seed(BackwardSeed(&mut asm))?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let free: Vec<u32> = q
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;
                asm.finish(free).map_err(de::Error::custom)
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut m: A,
            ) -> std::result::Result<Interner, A::Error> {
                // Self-describing formats: the fields may come in any order.
                let mut forward = None;
                let mut backward: Option<Vec<Option<String>>> = None;
                let mut free: Option<Vec<u32>> = None;
                while let Some(k) = m.next_key::<String>()? {
                    match k.as_str() {
                        "forward" => forward = Some(m.next_value_seed(ForwardSeed)?),
                        "backward" => backward = Some(m.next_value()?),
                        "free" => free = Some(m.next_value()?),
                        _ => {
                            m.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                let forward = forward.ok_or_else(|| de::Error::missing_field("forward"))?;
                let backward = backward.ok_or_else(|| de::Error::missing_field("backward"))?;
                let mut asm = Assembler::new(forward).map_err(de::Error::custom)?;
                asm.reserve(Some(backward.len()));
                for s in &backward {
                    asm.push(s.as_deref()).map_err(de::Error::custom)?;
                }
                asm.finish(free.unwrap_or_default())
                    .map_err(de::Error::custom)
            }
        }
        de.deserialize_struct("Interner", INTERNER_FIELDS, V)
    }
}

// ---- generations ----------------------------------------------------------------

/// Process-wide source of dictionary generations: every mutation of an
/// in-memory dictionary takes a fresh value, so equal generations mean equal
/// contents (clones share it; a fresh empty dictionary has generation 0).
/// Pin and retirement stamps are drawn from the same clock, so they order
/// with each other.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}

// ---- term ids and pins ----------------------------------------------------------

/// A dictionary id together with its space (literal ids are public ids,
/// offset by [`LITERAL_FIRST_ID`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TermId {
    /// The id space.
    pub space: Space,
    /// The public id within it.
    pub id: u32,
}

impl TermId {
    /// An entity or literal id (the space follows from the id's range).
    pub fn node(id: EntityLiteralId) -> TermId {
        TermId {
            space: Space::of_id(id),
            id,
        }
    }

    /// A predicate id.
    pub fn predicate(id: PredId) -> TermId {
        TermId {
            space: Space::Predicate,
            id,
        }
    }
}

/// The live pins of one dictionary and its clones: pin stamp → count.
#[derive(Default)]
struct PinSet(std::sync::Mutex<std::collections::BTreeMap<u64, usize>>);

impl PinSet {
    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<u64, usize>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn oldest(&self) -> Option<u64> {
        self.lock().keys().next().copied()
    }
}

impl fmt::Debug for PinSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PinSet({} live)", self.lock().values().sum::<usize>())
    }
}

/// A reader's hold on the meaning of every id of a dictionary (see
/// [`Dictionary::pin`]): while it lives, no id retired after it was taken is
/// handed out again, so an id the reader saw keeps resolving to the same
/// term — or to none — for as long as it reads. Dropping it releases the
/// hold (the ids wait for the next [`Dictionary::release`]).
#[derive(Debug)]
pub struct DictPin {
    pins: Arc<PinSet>,
    stamp: u64,
}

impl DictPin {
    fn new(pins: &Arc<PinSet>) -> DictPin {
        let stamp = next_generation();
        *pins.lock().entry(stamp).or_insert(0) += 1;
        DictPin {
            pins: Arc::clone(pins),
            stamp,
        }
    }
}

impl Drop for DictPin {
    fn drop(&mut self) {
        let mut g = self.pins.lock();
        if let Some(n) = g.get_mut(&self.stamp) {
            *n -= 1;
            if *n == 0 {
                g.remove(&self.stamp);
            }
        }
    }
}

// ---- the dictionary --------------------------------------------------------------

/// The full RDF dictionary: entities, literals, and predicates.
///
/// Two modes share one type so the query engine (which takes `&Dictionary`) runs
/// over either without change:
/// * *in-memory* — `backing == None`; the three copy-on-write interners hold
///   every string.
/// * *out-of-core* — `backing == Some(_)`; the interners stay empty and every
///   lookup is served by the [`DiskTermSource`] on demand. Built by
///   `DiskStore` so a disk query never materializes the whole dictionary.
///
/// `Send + Sync` and cheap to clone (the interners are shared until written).
///
/// ## Reclamation and pins
///
/// A removed term's id is first *retired* ([`retire`](Self::retire)): the
/// term no longer resolves either way, but the id is not reused until
/// [`release`](Self::release) finds no [`DictPin`] older than the retirement.
/// Pins are shared by a dictionary and its clones. A clone of an
/// **out-of-core** dictionary holds a pin of its own for as long as it lives
/// (it shares the backend, so an id reused through the original would change
/// meaning under it); a clone of an in-memory dictionary does not need one
/// (it owns its copy-on-write version of every id).
#[derive(Default, Serialize, Deserialize)]
pub struct Dictionary {
    entities: Interner,
    literals: Interner,
    predicates: Interner,
    /// Out-of-core backend, when this dictionary is disk-backed. Never persisted
    /// (it is reconstructed from the on-disk trees), so it is skipped by serde
    /// and defaults to `None` on load — keeping the in-memory path byte-identical.
    #[serde(skip)]
    backing: Option<Arc<dyn DiskTermSource>>,
    /// Content generation (see [`generation`](Self::generation)); not persisted.
    #[serde(skip, default = "next_generation")]
    generation: u64,
    /// The pins of this dictionary and its clones (see [`pin`](Self::pin)).
    #[serde(skip)]
    pins: Arc<PinSet>,
    /// The pin a clone of an out-of-core dictionary holds while it lives
    /// (held for its `Drop`).
    #[serde(skip)]
    #[allow(dead_code)]
    clone_pin: Option<Arc<DictPin>>,
    /// Ids interned new while tracking is on (see
    /// [`track_allocations`](Self::track_allocations)).
    #[serde(skip)]
    allocations: Option<Vec<TermId>>,
}

impl Clone for Dictionary {
    fn clone(&self) -> Dictionary {
        Dictionary {
            entities: self.entities.clone(),
            literals: self.literals.clone(),
            predicates: self.predicates.clone(),
            backing: self.backing.clone(),
            generation: self.generation,
            pins: Arc::clone(&self.pins),
            // A clone sharing a backend pins it (see the type docs).
            clone_pin: self
                .backing
                .as_ref()
                .map(|_| Arc::new(DictPin::new(&self.pins))),
            allocations: None,
        }
    }
}

impl fmt::Debug for Dictionary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Dictionary");
        match &self.backing {
            Some(b) => d.field("backing", b),
            None => d
                .field("entities", &self.entities)
                .field("literals", &self.literals)
                .field("predicates", &self.predicates),
        };
        d.finish()
    }
}

impl Dictionary {
    pub fn new() -> Dictionary {
        Dictionary::default()
    }

    /// Build a disk-backed dictionary that resolves str↔id through `backing`
    /// on demand. The in-memory interners stay empty; only looked-up terms
    /// ever become resident (in the backend's bounded cache).
    pub fn from_backing(backing: Arc<dyn DiskTermSource>) -> Dictionary {
        Dictionary {
            backing: Some(backing),
            ..Dictionary::default()
        }
    }

    /// An in-memory dictionary holding exactly the given terms at their ids:
    /// `ent[i]` is entity `i`, `lit[i]` literal `LITERAL_FIRST_ID + i`,
    /// `pred[i]` predicate `i`; `None` is a hole. Holes become free ids
    /// (reused lowest first), trailing holes are dropped, and a term listed
    /// twice in one space is refused. The loader of every layout that keeps
    /// ids with gaps (RocksDB's `dict` CF, a `kvstore.kv` with freed ids).
    pub fn from_slots<S: AsRef<str>>(
        ent: Vec<Option<S>>,
        lit: Vec<Option<S>>,
        pred: Vec<Option<S>>,
    ) -> Result<Dictionary> {
        let bound = |space: Space, n: usize| -> Result<()> {
            let ok = match space {
                Space::Entity => n as u64 <= u64::from(ENTITY_ID_LIMIT),
                Space::Literal => n as u64 <= u64::from(LITERAL_INDEX_LIMIT),
                Space::Predicate => n as u64 <= u64::from(PREDICATE_ID_LIMIT),
            };
            if ok {
                Ok(())
            } else {
                Err(GStoreError::Database(format!(
                    "{} id space exhausted: {n} ids",
                    space.name()
                )))
            }
        };
        bound(Space::Entity, ent.len())?;
        bound(Space::Literal, lit.len())?;
        bound(Space::Predicate, pred.len())?;
        let build =
            |slots: Vec<Option<S>>| Interner::from_slots(slots).map_err(GStoreError::Database);
        let mut d = Dictionary {
            entities: build(ent)?,
            literals: build(lit)?,
            predicates: build(pred)?,
            ..Dictionary::default()
        };
        d.touched();
        Ok(d)
    }

    /// Whether this dictionary resolves lookups from disk on demand.
    pub fn is_disk_backed(&self) -> bool {
        self.backing.is_some()
    }

    /// The out-of-core backend, if any.
    pub fn backing(&self) -> Option<&Arc<dyn DiskTermSource>> {
        self.backing.as_ref()
    }

    /// A number that changes whenever this dictionary's contents may have
    /// changed: equal values (for one dictionary and its clones) mean equal
    /// contents. In-memory dictionaries draw it from a process-wide counter on
    /// every intern of a new term and every free; a backed dictionary reports
    /// its backend's [`generation`](DiskTermSource::generation).
    pub fn generation(&self) -> u64 {
        match &self.backing {
            Some(b) => b.generation(),
            None => self.generation,
        }
    }

    fn touched(&mut self) {
        self.generation = next_generation();
    }

    fn interner(&self, space: Space) -> &Interner {
        match space {
            Space::Entity => &self.entities,
            Space::Literal => &self.literals,
            Space::Predicate => &self.predicates,
        }
    }

    fn interner_mut(&mut self, space: Space) -> &mut Interner {
        match space {
            Space::Entity => &mut self.entities,
            Space::Literal => &mut self.literals,
            Space::Predicate => &mut self.predicates,
        }
    }

    /// The interner index of public id `id` of `space`.
    fn index_of(space: Space, id: u32) -> Option<u32> {
        id.checked_sub(space.first_id())
    }

    // ---- pins and reclamation ---------------------------------------------------

    /// Pin the current meaning of every id (see [`DictPin`]): ids retired
    /// from now on are not reused until the pin is dropped. Shared with every
    /// clone of this dictionary.
    pub fn pin(&self) -> DictPin {
        DictPin::new(&self.pins)
    }

    /// Whether any pin of this dictionary (or of a clone) is alive — an
    /// out-of-core dictionary's clones hold one each.
    pub fn is_pinned(&self) -> bool {
        self.pins.oldest().is_some()
    }

    /// Remove the term of `id` (in `space`) from an in-memory dictionary:
    /// neither its key nor its id resolves any more, and the id waits to be
    /// [released](Self::release) for reuse. `false` if `id` names no term
    /// (or the dictionary is out-of-core: its backend frees ids itself).
    /// The caller guarantees the term is unreferenced.
    pub fn retire(&mut self, space: Space, id: u32) -> bool {
        if self.backing.is_some() {
            return false;
        }
        let Some(idx) = Self::index_of(space, id) else {
            return false;
        };
        let stamp = next_generation();
        let done = self.interner_mut(space).retire(idx, stamp);
        if done {
            self.touched();
        }
        done
    }

    /// Make every retired id that no pin older than its retirement holds
    /// free for reuse (lowest first; freeing the highest ids shrinks the
    /// space). Returns how many ids were released.
    pub fn release(&mut self) -> usize {
        if self.backing.is_some() {
            return 0;
        }
        let oldest = self.pins.oldest();
        let n = self.entities.release(oldest)
            + self.literals.release(oldest)
            + self.predicates.release(oldest);
        if n > 0 {
            self.touched();
        }
        n
    }

    /// Retire `id` and release what no pin holds — the id is reusable at once
    /// unless pinned. `false` if `id` named no term.
    pub fn free_id(&mut self, space: Space, id: u32) -> bool {
        let done = self.retire(space, id);
        if done {
            self.release();
        }
        done
    }

    /// Ids retired but still held by a pin, per space `[entity, literal,
    /// predicate]`.
    pub fn retired_counts(&self) -> [usize; 3] {
        [
            self.entities.retired_len(),
            self.literals.retired_len(),
            self.predicates.retired_len(),
        ]
    }

    /// Free (reusable) ids below the id limits, per space `[entity,
    /// literal, predicate]`. In memory only (0 for an out-of-core
    /// dictionary, whose backend reports its own).
    pub fn free_counts(&self) -> [usize; 3] {
        [
            self.entities.free_len(),
            self.literals.free_len(),
            self.predicates.free_len(),
        ]
    }

    /// [`id_limit`](Self::id_limit) of each space `[entity, literal,
    /// predicate]`.
    pub fn id_limits(&self) -> [u32; 3] {
        [
            self.id_limit(Space::Entity),
            self.id_limit(Space::Literal),
            self.id_limit(Space::Predicate),
        ]
    }

    /// Start (`true`) or stop recording the ids this dictionary interns new
    /// (a transaction's minted terms, freed again at its rollback). Starting
    /// clears the record.
    pub fn track_allocations(&mut self, on: bool) {
        self.allocations = on.then(Vec::new);
    }

    /// The ids interned new since tracking started or since the last call
    /// (empty when tracking is off). Tracking stays on.
    pub fn take_allocations(&mut self) -> Vec<TermId> {
        match self.allocations.as_mut() {
            Some(v) => std::mem::take(v),
            None => Vec::new(),
        }
    }

    // ---- interning ----------------------------------------------------------

    /// Intern `key` in `space`, returning its id and whether it is new:
    /// through the backend for a backed dictionary (a read-only backend
    /// refuses), else in memory with the space's bound. Releases retired ids
    /// first when some wait (so a dropped pin is honoured by the next
    /// intern).
    pub fn try_intern_new(&mut self, space: Space, key: &str) -> Result<(u32, bool)> {
        if let Some(b) = &self.backing {
            // A backend's generation moves exactly when it writes a new term
            // (the dictionary has one writer: the `&mut` caller).
            let before = b.generation();
            let id = b.intern(space, key)?;
            let new = b.generation() != before;
            if let (true, Some(log)) = (new, self.allocations.as_mut()) {
                log.push(TermId { space, id });
            }
            return Ok((id, new));
        }
        if self.interner(space).retired_len() > 0 && self.interner(space).get(key).is_none() {
            self.release();
        }
        let (id, new) = match space {
            Space::Entity => self.entities.try_intern(key, checked_entity_id)?,
            Space::Literal => {
                let (idx, new) = self.literals.try_intern(key, |next| {
                    checked_literal_id(next).map(|id| id - LITERAL_FIRST_ID)
                })?;
                (LITERAL_FIRST_ID + idx, new)
            }
            Space::Predicate => self.predicates.try_intern(key, checked_predicate_id)?,
        };
        if new {
            self.touched();
            if let Some(log) = self.allocations.as_mut() {
                log.push(TermId { space, id });
            }
        }
        Ok((id, new))
    }

    /// Intern `key` in `space`: through the backend for a backed dictionary
    /// (a read-only backend refuses), else in memory with the space's bound.
    #[inline]
    fn try_intern_in(&mut self, space: Space, key: &str) -> Result<u32> {
        self.try_intern_new(space, key).map(|(id, _)| id)
    }
    /// Intern an entity (IRI or blank node), returning its entity id in
    /// `[0, LITERAL_FIRST_ID)`, or an error once the entity space is exhausted
    /// (see the module docs). Build and insert paths use this checked form.
    /// On a backed dictionary the backend interns (a read-only one refuses).
    pub fn try_intern_entity(&mut self, key: &str) -> Result<EntityLiteralId> {
        self.try_intern_in(Space::Entity, key)
    }

    /// Infallible [`try_intern_entity`](Self::try_intern_entity).
    ///
    /// # Panics
    /// When the entity id space is exhausted — rather than handing out an id in
    /// the literal range, which would silently corrupt the store.
    pub fn intern_entity(&mut self, key: &str) -> EntityLiteralId {
        self.try_intern_entity(key)
            .unwrap_or_else(|e| panic!("{e} (use try_intern_entity to handle this)"))
    }

    /// Intern a literal, returning its public id (its dense index offset by
    /// `LITERAL_FIRST_ID`), or an error once the literal space is exhausted.
    pub fn try_intern_literal(&mut self, key: &str) -> Result<EntityLiteralId> {
        self.try_intern_in(Space::Literal, key)
    }

    /// Infallible [`try_intern_literal`](Self::try_intern_literal).
    ///
    /// # Panics
    /// When the literal id space is exhausted.
    pub fn intern_literal(&mut self, key: &str) -> EntityLiteralId {
        self.try_intern_literal(key)
            .unwrap_or_else(|e| panic!("{e} (use try_intern_literal to handle this)"))
    }

    /// Intern a predicate, returning its id in `[0, PREDICATE_ID_LIMIT)`, or an
    /// error once the predicate space is exhausted.
    pub fn try_intern_predicate(&mut self, key: &str) -> Result<PredId> {
        self.try_intern_in(Space::Predicate, key)
    }

    /// Infallible [`try_intern_predicate`](Self::try_intern_predicate).
    ///
    /// # Panics
    /// When the predicate id space is exhausted.
    pub fn intern_predicate(&mut self, key: &str) -> PredId {
        self.try_intern_predicate(key)
            .unwrap_or_else(|e| panic!("{e} (use try_intern_predicate to handle this)"))
    }

    /// Intern a term in the appropriate object/subject space: literals go to the
    /// literal space, everything else (IRI / blank node) to the entity space.
    ///
    /// # Panics
    /// When the target id space is exhausted; see
    /// [`try_intern_term`](Self::try_intern_term).
    pub fn intern_term(&mut self, t: &Term) -> EntityLiteralId {
        let key = t.dict_key();
        if t.is_literal() {
            self.intern_literal(&key)
        } else {
            self.intern_entity(&key)
        }
    }

    /// Checked [`intern_term`](Self::intern_term): `Err` when the term's id
    /// space (entity or literal) is exhausted.
    pub fn try_intern_term(&mut self, t: &Term) -> Result<EntityLiteralId> {
        self.try_intern_in(Space::of_term(t), &t.dict_key())
    }

    // ---- key → id -------------------------------------------------------------

    /// Key → id in `space`, with a backend's read error returned.
    pub fn try_id(&self, space: Space, key: &str) -> Result<Option<u32>> {
        if let Some(b) = &self.backing {
            return b.lookup(space, key);
        }
        Ok(match space {
            Space::Entity => self.entities.get(key),
            // Checked (not `expect`): a corrupt persisted dictionary with an
            // out-of-range index reads as "absent" instead of panicking a query.
            Space::Literal => self
                .literals
                .get(key)
                .and_then(|i| i.checked_add(LITERAL_FIRST_ID))
                .filter(|&id| id != INVALID_ENTITY_LITERAL_ID),
            Space::Predicate => self.predicates.get(key),
        })
    }

    /// Key → id in `space`; a backend's read error is noted (C2) and reads as
    /// absent.
    pub fn id_of(&self, space: Space, key: &str) -> Option<u32> {
        self.try_id(space, key).unwrap_or_else(|e| {
            note_read_error(format_args!("dictionary lookup ({}): {e}", space.name()));
            None
        })
    }

    /// Entity key → id, with a backend's read error returned.
    pub fn try_entity_id(&self, key: &str) -> Result<Option<EntityLiteralId>> {
        self.try_id(Space::Entity, key)
    }

    /// Literal key → public id, with a backend's read error returned.
    pub fn try_literal_id(&self, key: &str) -> Result<Option<EntityLiteralId>> {
        self.try_id(Space::Literal, key)
    }

    /// Predicate key → id, with a backend's read error returned.
    pub fn try_predicate_id(&self, key: &str) -> Result<Option<PredId>> {
        self.try_id(Space::Predicate, key)
    }

    pub fn entity_id(&self, key: &str) -> Option<EntityLiteralId> {
        self.id_of(Space::Entity, key)
    }

    pub fn literal_id(&self, key: &str) -> Option<EntityLiteralId> {
        self.id_of(Space::Literal, key)
    }

    pub fn predicate_id(&self, key: &str) -> Option<PredId> {
        self.id_of(Space::Predicate, key)
    }

    /// Resolve a term to its existing id without interning.
    pub fn term_id(&self, t: &Term) -> Option<EntityLiteralId> {
        self.id_of(Space::of_term(t), &t.dict_key())
    }

    /// [`term_id`](Self::term_id), with a backend's read error returned.
    pub fn try_term_id(&self, t: &Term) -> Result<Option<EntityLiteralId>> {
        self.try_id(Space::of_term(t), &t.dict_key())
    }

    // ---- id → string ------------------------------------------------------------

    /// The in-memory string of `id` in `space` (public id).
    #[inline]
    fn mem_resolve(&self, space: Space, id: u32) -> Option<&str> {
        match space {
            Space::Entity | Space::Predicate => self.interner(space).resolve(id),
            Space::Literal => self.literals.resolve(id.checked_sub(LITERAL_FIRST_ID)?),
        }
    }

    /// Id → string in `space` (public id), with a backend's read error
    /// returned. `Ok(None)` for an id that names no term.
    pub fn try_resolve(&self, space: Space, id: u32) -> Result<Option<TermStr<'_>>> {
        if id == INVALID_ENTITY_LITERAL_ID {
            return Ok(None);
        }
        match &self.backing {
            Some(b) => Ok(b.resolve(space, id)?.map(TermStr::Shared)),
            None => Ok(self.mem_resolve(space, id).map(TermStr::Borrowed)),
        }
    }

    /// Id → string in `space`; a backend's read error is noted (C2) and reads
    /// as absent.
    #[inline]
    pub fn resolve(&self, space: Space, id: u32) -> Option<TermStr<'_>> {
        if self.backing.is_none() {
            // The in-memory fast path, inlined into the evaluator's per-cell
            // resolution.
            return self.mem_resolve(space, id).map(TermStr::Borrowed);
        }
        self.resolve_backed(space, id)
    }

    /// [`resolve`](Self::resolve) for a backed dictionary (out of line).
    #[inline(never)]
    fn resolve_backed(&self, space: Space, id: u32) -> Option<TermStr<'_>> {
        self.try_resolve(space, id).unwrap_or_else(|e| {
            note_read_error(format_args!("dictionary {} id {id}: {e}", space.name()));
            None
        })
    }

    /// Entity-or-literal id → its dictionary key (routed by id range, like
    /// gStore's `getStringByID`). A backend's read error is noted (C2) and
    /// reads as absent.
    #[inline]
    pub fn term(&self, id: EntityLiteralId) -> Option<TermStr<'_>> {
        self.resolve(Space::of_id(id), id)
    }

    /// Predicate id → its key. A backend's read error is noted (C2).
    #[inline]
    pub fn pred(&self, id: PredId) -> Option<TermStr<'_>> {
        if id == INVALID_PRED_ID {
            return None;
        }
        self.resolve(Space::Predicate, id)
    }

    /// [`term`](Self::term), with a backend's read error returned.
    pub fn try_term(&self, id: EntityLiteralId) -> Result<Option<TermStr<'_>>> {
        self.try_resolve(Space::of_id(id), id)
    }

    /// [`pred`](Self::pred), with a backend's read error returned.
    pub fn try_pred(&self, id: PredId) -> Result<Option<TermStr<'_>>> {
        if id == INVALID_PRED_ID {
            return Ok(None);
        }
        self.try_resolve(Space::Predicate, id)
    }

    /// The legacy `&str` form of [`term`](Self::term).
    ///
    /// For an in-memory dictionary it borrows the string (no copy). An
    /// out-of-core dictionary has no string a `&str` could borrow for as long
    /// as `&self` (the backend's cache may evict it), so there the call fails
    /// closed (contract C4): `None` plus a read error (C2), never a leaked or
    /// wrong string. Production code uses [`term`](Self::term) /
    /// [`pred`](Self::pred), which serve both kinds.
    pub fn id_to_string(&self, id: EntityLiteralId) -> Option<&str> {
        if id == INVALID_ENTITY_LITERAL_ID {
            return None;
        }
        if self.backing.is_none() {
            return self.mem_resolve(Space::of_id(id), id);
        }
        Self::legacy_on_backed(Space::of_id(id), id)
    }

    /// The legacy `&str` form of [`pred`](Self::pred); see
    /// [`id_to_string`](Self::id_to_string) for the out-of-core behaviour.
    pub fn predicate_to_string(&self, id: PredId) -> Option<&str> {
        if id == INVALID_PRED_ID {
            return None;
        }
        if self.backing.is_none() {
            return self.predicates.resolve(id);
        }
        Self::legacy_on_backed(Space::Predicate, id)
    }

    /// The legacy `&str` API on an out-of-core dictionary: `None` plus a
    /// read error (C4 fails closed; C2 makes the statement fail).
    #[cold]
    fn legacy_on_backed<'a>(space: Space, id: u32) -> Option<&'a str> {
        note_read_error(format_args!(
            "dictionary {} id {id}: the legacy &str dictionary API cannot serve an \
             out-of-core dictionary; use Dictionary::term / pred",
            space.name()
        ));
        None
    }

    // ---- batches ------------------------------------------------------------------

    /// Prepare a batch of look-ups: `ids` are entity/literal ids, `pids`
    /// predicate ids, in any order with repeats. A backed dictionary sorts
    /// them per space and has its backend read them in id order; an in-memory
    /// one has nothing to do. Advisory: a read error is noted (C2).
    pub fn prefetch(&self, ids: &[EntityLiteralId], pids: &[PredId]) {
        let Some(b) = &self.backing else {
            return;
        };
        let mut ents = Vec::new();
        let mut lits = Vec::new();
        for &id in ids {
            if id == INVALID_ENTITY_LITERAL_ID {
                continue;
            }
            if is_literal_id(id) {
                lits.push(id);
            } else {
                ents.push(id);
            }
        }
        let mut preds: Vec<u32> = pids
            .iter()
            .copied()
            .filter(|&p| p != INVALID_PRED_ID)
            .collect();
        for (space, v) in [
            (Space::Entity, &mut ents),
            (Space::Literal, &mut lits),
            (Space::Predicate, &mut preds),
        ] {
            if v.is_empty() {
                continue;
            }
            v.sort_unstable();
            v.dedup();
            if let Err(e) = b.prefetch(space, v) {
                note_read_error(format_args!("dictionary prefetch ({}): {e}", space.name()));
            }
        }
    }

    /// Resolve `ids` — ascending, deduplicated, all of `space` — aligned with
    /// them: one batch for a backed dictionary (it reads what its cache does
    /// not hold in id order), direct borrows in memory.
    pub fn resolve_sorted(&self, space: Space, ids: &[u32]) -> Result<Vec<Option<TermStr<'_>>>> {
        match &self.backing {
            Some(b) => Ok(b
                .resolve_sorted(space, ids)?
                .into_iter()
                .map(|s| s.map(TermStr::Shared))
                .collect()),
            None => Ok(ids
                .iter()
                .map(|&id| self.mem_resolve(space, id).map(TermStr::Borrowed))
                .collect()),
        }
    }

    /// Resolve a batch of ids of one `space`, in the order given. A backed
    /// dictionary reads them in ascending id order (each distinct id once).
    pub fn resolve_many(&self, space: Space, ids: &[u32]) -> Result<Vec<Option<TermStr<'_>>>> {
        let Some(b) = &self.backing else {
            return Ok(ids
                .iter()
                .map(|&id| {
                    if id == INVALID_ENTITY_LITERAL_ID {
                        None
                    } else {
                        self.mem_resolve(space, id).map(TermStr::Borrowed)
                    }
                })
                .collect());
        };
        let mut order: Vec<u32> = ids
            .iter()
            .copied()
            .filter(|&id| id != INVALID_ENTITY_LITERAL_ID)
            .collect();
        order.sort_unstable();
        order.dedup();
        let found = b.resolve_sorted(space, &order)?;
        Ok(ids
            .iter()
            .map(|id| {
                order
                    .binary_search(id)
                    .ok()
                    .and_then(|i| found[i].clone())
                    .map(TermStr::Shared)
            })
            .collect())
    }

    /// Visit every term of `space` with public id `>= from`, in ascending id
    /// order: `f(id, key)`, which may stop the walk. Works for both modes.
    pub fn for_each_term(
        &self,
        space: Space,
        from: u32,
        mut f: impl FnMut(u32, &str) -> ControlFlow<()>,
    ) -> Result<()> {
        if let Some(b) = &self.backing {
            return b.for_each(space, from, &mut f);
        }
        match space {
            Space::Literal => {
                self.literals
                    .for_each_from(from.saturating_sub(LITERAL_FIRST_ID), &mut |i, s| match i
                        .checked_add(LITERAL_FIRST_ID)
                    {
                        Some(id) if id != INVALID_ENTITY_LITERAL_ID => f(id, s),
                        _ => ControlFlow::Break(()),
                    })
            }
            _ => self.interner(space).for_each_from(from, &mut f),
        }
        Ok(())
    }

    /// An in-memory copy: a clone for an in-memory dictionary (O(1), the
    /// interners are shared until written); for a backed one, every term read
    /// from the backend with its id kept (gaps become freed holes).
    pub fn materialize(&self) -> Result<Dictionary> {
        if self.backing.is_none() {
            return Ok(self.clone());
        }
        let mut out = Dictionary::new();
        for space in Space::ALL {
            let mut strings: Vec<Option<Arc<str>>> = Vec::new();
            let mut live = 0usize;
            let first = space.first_id();
            let mut failed = None;
            self.for_each_term(space, first, |id, key| {
                let idx = id - first;
                if (strings.len() as u32) > idx {
                    failed = Some(id);
                    return ControlFlow::Break(());
                }
                strings.resize(idx as usize, None);
                strings.push(Some(Arc::from(key)));
                live += 1;
                ControlFlow::Continue(())
            })?;
            if let Some(id) = failed {
                return Err(GStoreError::Database(format!(
                    "out-of-core dictionary listed {} id {id} out of order",
                    space.name()
                )));
            }
            let interner = Interner::assemble(strings, live, &[]).map_err(GStoreError::Database)?;
            match space {
                Space::Entity => out.entities = interner,
                Space::Literal => out.literals = interner,
                Space::Predicate => out.predicates = interner,
            }
        }
        out.touched();
        Ok(out)
    }

    /// Exclusive upper bound of the public ids allocated in `space` (freed
    /// holes included): ids of `space` lie in `[space.first_id(), id_limit)`.
    pub fn id_limit(&self, space: Space) -> u32 {
        match &self.backing {
            Some(b) => b.id_limit(space),
            None => {
                let limit = self.interner(space).index_limit() + u64::from(space.first_id());
                limit.min(u64::from(u32::MAX)) as u32
            }
        }
    }

    /// Cache counters of a backed dictionary; for an in-memory one, its
    /// string bytes (everything is resident).
    pub fn cache_stats(&self) -> CacheStats {
        match &self.backing {
            Some(b) => b.cache_stats(),
            None => CacheStats {
                term_entries: self.resident_string_count(),
                term_bytes: self.heap_bytes(),
                ..CacheStats::default()
            },
        }
    }

    /// Heap bytes the in-memory interners hold (0 for a backed dictionary).
    pub fn heap_bytes(&self) -> usize {
        if self.backing.is_some() {
            return 0;
        }
        Space::ALL
            .iter()
            .map(|&s| self.interner(s).data.heap_bytes())
            .sum()
    }

    // ---- counts (mirror Database::getEntityNum etc.) ----------------------

    pub fn entity_num(&self) -> usize {
        match &self.backing {
            Some(b) => b.count(Space::Entity),
            None => self.entities.len(),
        }
    }
    pub fn literal_num(&self) -> usize {
        match &self.backing {
            Some(b) => b.count(Space::Literal),
            None => self.literals.len(),
        }
    }
    pub fn predicate_num(&self) -> usize {
        match &self.backing {
            Some(b) => b.count(Space::Predicate),
            None => self.predicates.len(),
        }
    }

    /// Live `(id, string)` pairs for entities and literals — the id space that
    /// binds subject/object variables (predicates are excluded: separate space).
    /// Entity ids are their interner index; literal ids are offset by
    /// [`LITERAL_FIRST_ID`], matching [`term`](Self::term).
    ///
    /// In-memory dictionaries only: empty for a disk-backed dictionary (its
    /// strings live on disk); [`for_each_term`](Self::for_each_term) walks
    /// either kind.
    pub fn entity_literal_entries(&self) -> Vec<(EntityLiteralId, &str)> {
        if self.backing.is_some() {
            return Vec::new();
        }
        let mut v = Vec::with_capacity(self.entities.len() + self.literals.len());
        v.extend(self.entities.entries());
        v.extend(
            self.literals
                .entries()
                .filter_map(|(i, s)| i.checked_add(LITERAL_FIRST_ID).map(|id| (id, s))),
        );
        v
    }

    /// Live `(id, key)` pairs of one space in id order, borrowed — in-memory
    /// dictionaries only (empty for a disk-backed one; see
    /// [`for_each_term`](Self::for_each_term)).
    pub fn entries_in(&self, space: Space) -> Vec<(u32, &str)> {
        if self.backing.is_some() {
            return Vec::new();
        }
        let first = space.first_id();
        let it = self.interner(space).entries();
        let mut v = Vec::with_capacity(self.interner(space).len());
        v.extend(it.filter_map(|(i, s)| {
            i.checked_add(first)
                .filter(|&id| id != INVALID_ENTITY_LITERAL_ID)
                .map(|id| (id, s))
        }));
        v
    }

    // ---- prefix-compressed (front-coded) export --------------------------

    /// Every dictionary string — entities, literals, and predicates — merged,
    /// sorted, and de-duplicated. Sorting clusters shared prefixes so the set
    /// front-codes well. (Disk-backed dictionaries keep no in-memory strings, so
    /// this is empty for them.)
    pub fn all_strings_sorted(&self) -> Vec<String> {
        let mut v: Vec<String> =
            Vec::with_capacity(self.entities.len() + self.literals.len() + self.predicates.len());
        v.extend(self.entities.strings().map(str::to_owned));
        v.extend(self.literals.strings().map(str::to_owned));
        v.extend(self.predicates.strings().map(str::to_owned));
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Front-code (shared-prefix compress) this dictionary's string set into one
    /// compact block, realizing gStore's bounded-shared-prefix dictionary
    /// storage. RDF term sets are highly prefix-redundant, so the block is far
    /// smaller than the concatenated raw strings. Round-trips via
    /// [`prefix::decode_block`], which returns the strings of
    /// [`all_strings_sorted`](Self::all_strings_sorted).
    pub fn front_coded_block(&self) -> Vec<u8> {
        prefix::encode_block(&self.all_strings_sorted())
    }

    /// Number of dictionary strings currently materialized in RAM. For an
    /// in-memory dictionary this is the full count; for a disk-backed one it is
    /// only the subset its backend holds (used to assert out-of-core behavior).
    pub fn resident_string_count(&self) -> usize {
        match &self.backing {
            Some(b) => b.resident_string_count(),
            None => self.entities.len() + self.literals.len() + self.predicates.len(),
        }
    }

    // ---- id freelist / reclamation (gStore freeEntityID etc.) -------------

    /// Free `key`'s id in `space` (backed: through the backend, which a
    /// read-only one refuses — noted as a read error and read as `None`).
    fn free_in(&mut self, space: Space, key: &str) -> Option<u32> {
        if let Some(b) = &self.backing {
            let freed = b.lookup(space, key).and_then(|id| match id {
                Some(id) => b.free(space, id).map(|ok| ok.then_some(id)),
                None => Ok(None),
            });
            return freed.unwrap_or_else(|e| {
                note_read_error(format_args!("dictionary free ({}): {e}", space.name()));
                None
            });
        }
        let id = self.id_of(space, key)?;
        self.free_id(space, id).then_some(id)
    }

    /// Free an entity term's id for reuse, returning it if it was interned.
    /// The caller must ensure the term is no longer referenced (see
    /// [`Database::reclaim_unused`](crate::Database::reclaim_unused)). The id
    /// is reused at once unless a [pin](Self::pin) holds it.
    pub fn free_entity(&mut self, key: &str) -> Option<EntityLiteralId> {
        self.free_in(Space::Entity, key)
    }

    /// Free a literal term's public id for reuse, if it was interned.
    pub fn free_literal(&mut self, key: &str) -> Option<EntityLiteralId> {
        self.free_in(Space::Literal, key)
    }

    /// Free a predicate term's id for reuse, if it was interned.
    pub fn free_predicate(&mut self, key: &str) -> Option<PredId> {
        self.free_in(Space::Predicate, key)
    }

    /// Conservative reclamation pass (gStore's `freelist` rebuild): free every
    /// interned id that is **not** present in the supplied referenced-id sets, so
    /// those ids can be reused by future interns. `referenced_ent_lit` holds the
    /// still-live entity *and* literal ids (object ids, by range); `referenced_pred`
    /// the still-live predicate ids. Returns the number of ids reclaimed.
    ///
    /// Reclamation never touches an id that is still referenced, so the store
    /// stays internally consistent. No-op on a disk-backed dictionary.
    pub fn reclaim_unused(
        &mut self,
        referenced_ent_lit: &std::collections::HashSet<EntityLiteralId>,
        referenced_pred: &std::collections::HashSet<PredId>,
    ) -> usize {
        if self.backing.is_some() {
            return 0;
        }
        let dead = self.unreferenced(|t| match t.space {
            Space::Predicate => referenced_pred.contains(&t.id),
            _ => referenced_ent_lit.contains(&t.id),
        });
        let freed = dead.iter().filter(|t| self.retire(t.space, t.id)).count();
        self.release();
        freed
    }

    /// Every live term id of an in-memory dictionary for which `referenced`
    /// answers `false` (empty for an out-of-core dictionary).
    pub fn unreferenced(&self, mut referenced: impl FnMut(TermId) -> bool) -> Vec<TermId> {
        if self.backing.is_some() {
            return Vec::new();
        }
        let mut dead = Vec::new();
        for space in Space::ALL {
            let first = space.first_id();
            for (i, _) in self.interner(space).entries() {
                let Some(id) = i.checked_add(first) else {
                    continue;
                };
                let t = TermId { space, id };
                if !referenced(t) {
                    dead.push(t);
                }
            }
        }
        dead
    }

    /// **Testing aid — not for production use.** Move each id space's
    /// allocation counter of an *empty* in-memory dictionary to the given next
    /// id (`next_literal_index` is the literal's dense index, i.e. its public id
    /// minus `LITERAL_FIRST_ID`), so the id-space bounds can be exercised without
    /// allocating billions of terms. The skipped ids are simply never used. The
    /// advanced offsets are not persisted, so such a dictionary must not be
    /// saved and reloaded.
    #[doc(hidden)]
    pub fn debug_advance_id_counters(
        &mut self,
        next_entity: EntityLiteralId,
        next_literal_index: u32,
        next_predicate: PredId,
    ) -> Result<()> {
        let empty = |i: &Interner| i.len() == 0 && i.data.strings.is_empty();
        if self.backing.is_some()
            || !empty(&self.entities)
            || !empty(&self.literals)
            || !empty(&self.predicates)
        {
            return Err(GStoreError::Database(
                "debug_advance_id_counters needs an empty in-memory dictionary".into(),
            ));
        }
        Arc::make_mut(&mut self.entities.data).base = next_entity;
        Arc::make_mut(&mut self.literals.data).base = next_literal_index;
        Arc::make_mut(&mut self.predicates.data).base = next_predicate;
        self.touched();
        Ok(())
    }
}

/// Compile-time check: the dictionary is shared across threads.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Dictionary>();
};
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn entity_ids_start_at_zero_and_are_stable() {
        let mut d = Dictionary::new();
        let a = d.intern_entity("<a>");
        let b = d.intern_entity("<b>");
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        // re-interning is idempotent
        assert_eq!(d.intern_entity("<a>"), 0);
        assert_eq!(d.entity_id("<a>"), Some(0));
        assert_eq!(d.entity_id("<missing>"), None);
    }

    #[test]
    fn literal_ids_are_offset_by_literal_first_id() {
        let mut d = Dictionary::new();
        let l = d.intern_literal("\"hi\"");
        assert_eq!(l, LITERAL_FIRST_ID);
        assert_eq!(d.intern_literal("\"hi\""), LITERAL_FIRST_ID);
        assert_eq!(d.intern_literal("\"bye\""), LITERAL_FIRST_ID + 1);
        assert_eq!(d.literal_id("\"hi\""), Some(LITERAL_FIRST_ID));
    }

    #[test]
    fn entity_and_literal_spaces_do_not_collide() {
        // Same dense index 0, but distinct public ids.
        let mut d = Dictionary::new();
        let e = d.intern_entity("<foo>");
        let l = d.intern_literal("\"foo\"");
        assert_ne!(e, l);
    }

    #[test]
    fn predicates_have_their_own_space() {
        let mut d = Dictionary::new();
        let p = d.intern_predicate("<knows>");
        assert_eq!(p, 0);
        // predicate id 0 coexists with entity id 0
        assert_eq!(d.intern_entity("<x>"), 0);
        assert_eq!(d.predicate_to_string(0), Some("<knows>"));
    }

    #[test]
    fn id_to_string_roundtrips_across_spaces() {
        let mut d = Dictionary::new();
        let e = d.intern_entity("<a>");
        let l = d.intern_literal("\"v\"");
        assert_eq!(d.id_to_string(e), Some("<a>"));
        assert_eq!(d.id_to_string(l), Some("\"v\""));
        assert_eq!(d.id_to_string(INVALID_ENTITY_LITERAL_ID), None);
    }

    #[test]
    fn intern_term_routes_by_kind() {
        let mut d = Dictionary::new();
        let e = d.intern_term(&Term::iri("a"));
        let l = d.intern_term(&Term::plain_literal("a"));
        assert!(e < LITERAL_FIRST_ID);
        assert!(l >= LITERAL_FIRST_ID);
        assert_eq!(d.term_id(&Term::iri("a")), Some(e));
        assert_eq!(d.term_id(&Term::iri("missing")), None);
    }

    #[test]
    fn counts_track_interned_terms() {
        let mut d = Dictionary::new();
        d.intern_entity("<a>");
        d.intern_entity("<b>");
        d.intern_literal("\"x\"");
        d.intern_predicate("<p>");
        assert_eq!(d.entity_num(), 2);
        assert_eq!(d.literal_num(), 1);
        assert_eq!(d.predicate_num(), 1);
    }

    #[test]
    fn freed_entity_id_is_reused_by_next_intern() {
        let mut d = Dictionary::new();
        let a = d.intern_entity("<a>");
        let b = d.intern_entity("<b>");
        let c = d.intern_entity("<c>");
        assert_eq!((a, b, c), (0, 1, 2));
        assert_eq!(d.entity_num(), 3);

        // Free <b>'s id; the count drops and the id becomes unresolvable.
        assert_eq!(d.free_entity("<b>"), Some(1));
        assert_eq!(d.entity_num(), 2);
        assert_eq!(d.entity_id("<b>"), None);
        assert_eq!(d.id_to_string(1), None);

        // A brand-new term reuses the freed id rather than allocating id 3.
        let dnew = d.intern_entity("<d>");
        assert_eq!(dnew, 1, "freed id must be reused");
        assert_eq!(d.id_to_string(1), Some("<d>"));
        assert_eq!(d.entity_num(), 3);
        // The next new term extends the id space again.
        assert_eq!(d.intern_entity("<e>"), 3);
    }

    #[test]
    fn reclaim_unused_frees_only_unreferenced_ids() {
        use std::collections::HashSet;
        let mut d = Dictionary::new();
        let a = d.intern_entity("<a>");
        let b = d.intern_entity("<b>");
        let lit = d.intern_literal("\"v\"");
        let p = d.intern_predicate("<p>");
        let _q = d.intern_predicate("<q>");

        // Only <a>, "v", and <p> are still referenced.
        let ent_lit: HashSet<EntityLiteralId> = [a, lit].into_iter().collect();
        let preds: HashSet<PredId> = [p].into_iter().collect();
        let freed = d.reclaim_unused(&ent_lit, &preds);
        assert_eq!(freed, 2, "<b> and <q> must be reclaimed");

        assert_eq!(d.entity_id("<a>"), Some(a));
        assert_eq!(d.entity_id("<b>"), None);
        assert_eq!(d.literal_id("\"v\""), Some(lit));
        assert_eq!(d.predicate_id("<p>"), Some(p));
        assert_eq!(d.predicate_id("<q>"), None);
        assert_eq!(d.entity_num(), 1);
        assert_eq!(d.predicate_num(), 1);

        // The reclaimed entity id is reused.
        assert_eq!(d.intern_entity("<z>"), b);
    }

    #[test]
    fn front_coded_block_roundtrips_and_compresses() {
        let mut d = Dictionary::new();
        // Prefix-heavy IRIs under a shared namespace, plus a predicate.
        for i in 0..500 {
            d.intern_entity(&format!("<http://example.org/resource/item_{i:05}>"));
        }
        d.intern_predicate("<http://example.org/ns#partOf>");
        d.intern_literal("\"some literal value\"");

        let sorted = d.all_strings_sorted();
        let block = d.front_coded_block();

        // Round-trips exactly to the sorted string set.
        assert_eq!(prefix::decode_block(&block).unwrap(), sorted);

        // Shared prefixes make the block much smaller than the raw strings.
        let raw = prefix::raw_bytes(&sorted);
        assert!(
            block.len() < raw / 2,
            "front-coded dictionary should be <1/2 of raw {raw} bytes, got {}",
            block.len()
        );
    }

    /// Regression for the unbounded entity space: the 2,000,000,000th entity
    /// used to receive id `LITERAL_FIRST_ID`, i.e. be classified as a literal.
    #[test]
    fn entity_space_is_bounded_below_literal_first_id() {
        use crate::model::id::{is_entity_id, is_literal_id};
        let mut d = Dictionary::new();
        d.debug_advance_id_counters(ENTITY_ID_LIMIT - 2, 0, 0)
            .unwrap();
        let a = d.try_intern_entity("<a>").unwrap();
        let b = d.try_intern_entity("<b>").unwrap();
        assert_eq!((a, b), (LITERAL_FIRST_ID - 2, LITERAL_FIRST_ID - 1));
        assert!(is_entity_id(b) && !is_literal_id(b));
        assert_eq!(d.id_to_string(b), Some("<b>"));

        // The next brand-new entity is refused — not given a literal-range id.
        let err = d.try_intern_entity("<c>").unwrap_err();
        assert!(
            err.to_string().contains("entity id space exhausted"),
            "{err}"
        );
        assert_eq!(d.entity_id("<c>"), None);
        assert_eq!(d.entity_num(), 2);
        // Existing entities still resolve; a term routed to the full space errs.
        assert_eq!(d.try_intern_entity("<a>").unwrap(), a);
        assert!(d.try_intern_term(&Term::iri("c")).is_err());
        // Literals are a separate space and unaffected.
        assert_eq!(d.try_intern_literal("\"v\"").unwrap(), LITERAL_FIRST_ID);

        // A freed id is reused even when the space is otherwise full.
        assert_eq!(d.free_entity("<a>"), Some(a));
        assert_eq!(d.try_intern_entity("<c>").unwrap(), a);
        assert!(d.try_intern_entity("<d>").is_err());
    }

    #[test]
    fn literal_space_is_bounded_to_twice_literal_first_id() {
        let mut d = Dictionary::new();
        d.debug_advance_id_counters(0, LITERAL_INDEX_LIMIT - 1, 0)
            .unwrap();
        let last = d.try_intern_literal("\"last\"").unwrap();
        assert_eq!(last, 3_999_999_999);
        assert_eq!(d.literal_id("\"last\""), Some(last));
        assert_eq!(d.id_to_string(last), Some("\"last\""));
        assert_eq!(d.entity_literal_entries(), vec![(last, "\"last\"")]);
        let err = d.try_intern_literal("\"one more\"").unwrap_err();
        assert!(
            err.to_string().contains("literal id space exhausted"),
            "{err}"
        );
        assert!(d.try_intern_term(&Term::plain_literal("x")).is_err());
        assert_eq!(d.try_intern_entity("<e>").unwrap(), 0);
    }

    #[test]
    fn predicate_space_is_bounded() {
        let mut d = Dictionary::new();
        d.debug_advance_id_counters(0, 0, PREDICATE_ID_LIMIT - 1)
            .unwrap();
        assert_eq!(
            d.try_intern_predicate("<p>").unwrap(),
            PREDICATE_ID_LIMIT - 1
        );
        assert_eq!(d.predicate_to_string(PREDICATE_ID_LIMIT - 1), Some("<p>"));
        let err = d.try_intern_predicate("<q>").unwrap_err();
        assert!(
            err.to_string().contains("predicate id space exhausted"),
            "{err}"
        );
    }

    #[test]
    #[should_panic(expected = "entity id space exhausted")]
    fn infallible_intern_panics_instead_of_corrupting() {
        let mut d = Dictionary::new();
        d.debug_advance_id_counters(ENTITY_ID_LIMIT, 0, 0).unwrap();
        d.intern_entity("<x>");
    }

    #[test]
    fn advancing_counters_requires_an_empty_dictionary() {
        let mut d = Dictionary::new();
        d.intern_entity("<a>");
        assert!(d.debug_advance_id_counters(5, 5, 5).is_err());
    }

    #[test]
    fn checked_id_helpers_enforce_the_bounds() {
        assert_eq!(checked_entity_id(0).unwrap(), 0);
        assert_eq!(checked_entity_id(1_999_999_999).unwrap(), 1_999_999_999);
        assert!(checked_entity_id(2_000_000_000).is_err());
        assert!(checked_entity_id(u64::MAX).is_err());
        assert_eq!(checked_literal_id(0).unwrap(), LITERAL_FIRST_ID);
        assert_eq!(checked_literal_id(1_999_999_999).unwrap(), 3_999_999_999);
        assert!(checked_literal_id(2_000_000_000).is_err());
        assert!(checked_predicate_id(1_999_999_999).is_ok());
        assert!(checked_predicate_id(2_000_000_000).is_err());
    }

    /// The new (serde-skipped) interner offset must not change the persisted
    /// `dict.bin` encoding: bytes equal those of the pre-change layout.
    #[test]
    fn persisted_encoding_is_unchanged() {
        #[derive(Serialize)]
        struct OldInterner {
            forward: HashMap<String, u32>,
            backward: Vec<Option<String>>,
            free: Vec<u32>,
        }
        #[derive(Serialize)]
        struct OldDictionary {
            entities: OldInterner,
            literals: OldInterner,
            predicates: OldInterner,
        }
        let mut d = Dictionary::new();
        d.intern_entity("<a>");
        d.intern_entity("<b>");
        d.free_entity("<a>");
        d.intern_literal("\"x\"");
        d.intern_predicate("<p>");
        let old = |i: &Interner| OldInterner {
            forward: i.forward_map(),
            backward: i.backward_vec(),
            free: i.free_vec(),
        };
        let mirror = OldDictionary {
            entities: old(&d.entities),
            literals: old(&d.literals),
            predicates: old(&d.predicates),
        };
        let new_bytes = bincode::serialize(&d).unwrap();
        assert_eq!(new_bytes, bincode::serialize(&mirror).unwrap());
        let back: Dictionary = bincode::deserialize(&new_bytes).unwrap();
        assert_eq!(back.entity_id("<b>"), Some(1));
        assert_eq!(back.literal_id("\"x\""), Some(LITERAL_FIRST_ID));
        assert_eq!(back.predicate_id("<p>"), Some(0));
    }

    // ---- test-only views of an interner, in the former field layout --------

    impl Interner {
        fn forward_map(&self) -> HashMap<String, u32> {
            self.entries().map(|(i, s)| (s.to_owned(), i)).collect()
        }
        fn backward_vec(&self) -> Vec<Option<String>> {
            self.data
                .strings
                .iter()
                .map(|s| s.as_deref().map(str::to_owned))
                .collect()
        }
        fn free_vec(&self) -> Vec<u32> {
            self.data.persisted_free()
        }
    }

    /// SplitMix64, for seeded model tests.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// The interner in its persisted field layout: its derived serde *is* the
    /// pre-Phase-6 `dict.bin` encoding. `intern` / `free` model the Phase 6
    /// reclamation (lowest free index first; freeing the last index trims
    /// the vector and every free hole that becomes last; the free list kept
    /// ascending), which the new code is checked against;
    /// [`legacy_intern`](Self::legacy_intern) / [`legacy_free`](Self::legacy_free)
    /// are the former LIFO algorithm (the files older binaries wrote).
    #[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
    struct ModelInterner {
        forward: HashMap<String, u32>,
        backward: Vec<Option<String>>,
        free: Vec<u32>,
    }

    impl ModelInterner {
        fn intern(&mut self, s: &str) -> u32 {
            if let Some(&id) = self.forward.get(s) {
                return id;
            }
            let id = if self.free.is_empty() {
                self.backward.push(Some(s.to_owned()));
                self.backward.len() as u32 - 1
            } else {
                let reused = self.free.remove(0);
                self.backward[reused as usize] = Some(s.to_owned());
                reused
            };
            self.forward.insert(s.to_owned(), id);
            id
        }
        fn free(&mut self, s: &str) -> Option<u32> {
            let idx = self.forward.remove(s)?;
            self.backward[idx as usize] = None;
            let at = self.free.binary_search(&idx).unwrap_err();
            self.free.insert(at, idx);
            // Trim the end: the last hole and every free hole before it.
            while let Some(None) = self.backward.last() {
                let last = self.backward.len() as u32 - 1;
                match self.free.binary_search(&last) {
                    Ok(at) => {
                        self.free.remove(at);
                        self.backward.pop();
                    }
                    Err(_) => break,
                }
            }
            Some(idx)
        }
        fn legacy_intern(&mut self, s: &str) -> u32 {
            if let Some(&id) = self.forward.get(s) {
                return id;
            }
            let id = if let Some(reused) = self.free.pop() {
                self.backward[reused as usize] = Some(s.to_owned());
                reused
            } else {
                self.backward.push(Some(s.to_owned()));
                self.backward.len() as u32 - 1
            };
            self.forward.insert(s.to_owned(), id);
            id
        }
        fn legacy_free(&mut self, s: &str) -> Option<u32> {
            let idx = self.forward.remove(s)?;
            self.backward[idx as usize] = None;
            self.free.push(idx);
            Some(idx)
        }
    }

    #[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
    struct ModelDictionary {
        entities: ModelInterner,
        literals: ModelInterner,
        predicates: ModelInterner,
    }

    fn mirror(d: &Dictionary) -> ModelDictionary {
        let old = |i: &Interner| ModelInterner {
            forward: i.forward_map(),
            backward: i.backward_vec(),
            free: i.free_vec(),
        };
        ModelDictionary {
            entities: old(&d.entities),
            literals: old(&d.literals),
            predicates: old(&d.predicates),
        }
    }

    /// Apply the same random interns and frees (holes, a non-trivial free
    /// stack, reuse) to the old model and to a new dictionary; every returned
    /// id must agree.
    fn random_pair(seed: u64, ops: usize) -> (ModelDictionary, Dictionary) {
        let mut rng = Rng(seed);
        let mut old = ModelDictionary::default();
        let mut new = Dictionary::new();
        for _ in 0..ops {
            let space = rng.below(3);
            let key = match space {
                0 => format!("<http://ex/e{}>", rng.below(200)),
                1 => format!("\"lit {}\"", rng.below(150)),
                _ => format!("<http://ex/p{}>", rng.below(20)),
            };
            if rng.below(4) == 0 {
                let (o, n) = match space {
                    0 => (old.entities.free(&key), new.free_entity(&key)),
                    1 => (
                        old.literals.free(&key).map(|i| i + LITERAL_FIRST_ID),
                        new.free_literal(&key),
                    ),
                    _ => (old.predicates.free(&key), new.free_predicate(&key)),
                };
                assert_eq!(o, n, "free {key}");
            } else {
                let (o, n) = match space {
                    0 => (old.entities.intern(&key), new.intern_entity(&key)),
                    1 => (
                        old.literals.intern(&key) + LITERAL_FIRST_ID,
                        new.intern_literal(&key),
                    ),
                    _ => (old.predicates.intern(&key), new.intern_predicate(&key)),
                };
                assert_eq!(o, n, "intern {key}");
            }
        }
        // End with frees only, so holes and a free stack remain.
        for _ in 0..40 {
            let key = format!("<http://ex/e{}>", rng.below(200));
            assert_eq!(old.entities.free(&key), new.free_entity(&key));
            let key = format!("\"lit {}\"", rng.below(150));
            assert_eq!(
                old.literals.free(&key).map(|i| i + LITERAL_FIRST_ID),
                new.free_literal(&key)
            );
        }
        (old, new)
    }

    #[test]
    fn new_encoding_decodes_with_the_old_layout() {
        let mut with_holes = 0;
        for seed in 0..20 {
            let (old, new) = random_pair(seed, 600);
            if old.entities.backward.iter().any(Option::is_none) && !old.entities.free.is_empty() {
                with_holes += 1;
            }
            let bytes = bincode::serialize(&new).unwrap();
            let decoded: ModelDictionary = bincode::deserialize(&bytes).unwrap();
            assert_eq!(decoded, old, "seed {seed}");
            assert_eq!(mirror(&new), old, "seed {seed}");
        }
        assert!(
            with_holes >= 10,
            "the model runs must exercise holes and the free stack"
        );
    }

    #[test]
    fn old_encoding_decodes_with_the_new_code() {
        for seed in 0..20 {
            let (old, new) = random_pair(seed, 600);
            let bytes = bincode::serialize(&old).unwrap();
            let mut decoded: Dictionary = bincode::deserialize(&bytes).unwrap();
            assert_eq!(mirror(&decoded), old, "seed {seed}");
            // Same contents ⇒ same bytes as the dictionary built directly.
            assert_eq!(
                bincode::serialize(&decoded).unwrap(),
                bincode::serialize(&new).unwrap()
            );
            // Every string resolves both ways.
            for (k, &i) in &old.entities.forward {
                assert_eq!(decoded.entity_id(k), Some(i));
                assert_eq!(decoded.term(i).as_deref(), Some(k.as_str()));
            }
            // The free stack decodes in order: the next interns reuse the same
            // ids as the old model would.
            let mut model = old.clone();
            for i in 0..30 {
                let key = format!("<http://ex/fresh{i}>");
                assert_eq!(decoded.intern_entity(&key), model.entities.intern(&key));
                let lit = format!("\"fresh {i}\"");
                assert_eq!(
                    decoded.intern_literal(&lit),
                    model.literals.intern(&lit) + LITERAL_FIRST_ID
                );
            }
        }
    }

    #[test]
    fn encoding_is_reproducible() {
        let (_, a) = random_pair(7, 800);
        let (_, b) = random_pair(7, 800);
        let bytes = bincode::serialize(&a).unwrap();
        assert_eq!(bytes, bincode::serialize(&b).unwrap());
        assert_eq!(bytes, bincode::serialize(&a.clone()).unwrap());
        let back: Dictionary = bincode::deserialize(&bytes).unwrap();
        assert_eq!(bytes, bincode::serialize(&back).unwrap());
    }

    #[test]
    fn inconsistent_encoding_is_refused_not_panicking() {
        let mut bad = ModelDictionary::default();
        bad.entities.intern("<a>");
        bad.entities.intern("<b>");
        // forward says <b> is 0, backward says 0 is <a>.
        bad.entities.forward.insert("<b>".into(), 0);
        let bytes = bincode::serialize(&bad).unwrap();
        assert!(bincode::deserialize::<Dictionary>(&bytes).is_err());
        // A forward index past the end of the backward vector.
        let mut bad = ModelDictionary::default();
        bad.literals.forward.insert("\"x\"".into(), 5);
        let bytes = bincode::serialize(&bad).unwrap();
        assert!(bincode::deserialize::<Dictionary>(&bytes).is_err());
        // Truncated input.
        let (_, d) = random_pair(3, 100);
        let bytes = bincode::serialize(&d).unwrap();
        for cut in [1, bytes.len() / 2, bytes.len() - 1] {
            assert!(bincode::deserialize::<Dictionary>(&bytes[..cut]).is_err());
        }
    }

    #[test]
    fn id_table_matches_a_hash_map_model() {
        let mut rng = Rng(99);
        let mut strings: Vec<Option<Arc<str>>> = Vec::new();
        let mut table = IdTable::default();
        let mut model: HashMap<String, u32> = HashMap::new();
        for _ in 0..20_000 {
            let key = format!("k{}", rng.below(3000));
            if rng.below(3) == 0 {
                let got = table.remove(&key, &strings);
                let want = model.remove(&key);
                assert_eq!(got, want);
                if let Some(slot) = got {
                    strings[slot as usize] = None;
                }
            } else if let std::collections::hash_map::Entry::Vacant(e) = model.entry(key) {
                let slot = strings.len() as u32;
                strings.push(Some(Arc::from(e.key().as_str())));
                table.insert_new(e.key(), slot);
                e.insert(slot);
            }
            assert_eq!(table.len, model.len());
        }
        for i in 0..3000 {
            let key = format!("k{i}");
            assert_eq!(
                table.find(&key, &strings),
                model.get(&key).copied(),
                "{key}"
            );
        }
    }

    #[test]
    fn clones_are_copy_on_write() {
        let mut a = Dictionary::new();
        a.intern_entity("<a>");
        a.intern_literal("\"x\"");
        a.intern_predicate("<p>");
        let b = a.clone();
        assert!(a.entities.shares_with(&b.entities));
        // Re-interning an existing term, look-ups and freeing an absent key
        // never copy.
        assert_eq!(a.intern_entity("<a>"), 0);
        assert_eq!(a.entity_id("<a>"), Some(0));
        assert_eq!(a.free_entity("<absent>"), None);
        assert!(a.entities.shares_with(&b.entities));
        // A new term copies only the interner it changes.
        let c = a.intern_entity("<c>");
        assert!(!a.entities.shares_with(&b.entities));
        assert!(a.literals.shares_with(&b.literals));
        assert!(a.predicates.shares_with(&b.predicates));
        assert_eq!(a.entity_id("<c>"), Some(c));
        assert_eq!(b.entity_id("<c>"), None, "the clone is isolated");
        assert_eq!(b.entity_num(), 1);
        // Freeing in one clone leaves the other intact.
        let mut d = b.clone();
        assert_eq!(d.free_entity("<a>"), Some(0));
        assert_eq!(b.term(0).as_deref(), Some("<a>"));
        assert_eq!(d.term(0), None);
    }

    #[test]
    fn generation_tracks_content_changes() {
        let mut d = Dictionary::new();
        assert_eq!(d.generation(), 0);
        d.intern_entity("<a>");
        let g1 = d.generation();
        assert_ne!(g1, 0);
        d.intern_entity("<a>");
        let _ = d.entity_id("<a>");
        assert_eq!(d.generation(), g1, "no change, same generation");
        let c = d.clone();
        assert_eq!(c.generation(), g1);
        d.intern_literal("\"v\"");
        let g2 = d.generation();
        assert_ne!(g2, g1);
        d.free_literal("\"v\"");
        assert_ne!(d.generation(), g2);
        assert_eq!(c.generation(), g1);
        let back: Dictionary = bincode::deserialize(&bincode::serialize(&d).unwrap()).unwrap();
        assert_ne!(
            back.generation(),
            d.generation(),
            "a decoded copy is a new state"
        );
    }

    #[test]
    fn term_str_behaves_like_its_text() {
        use std::collections::HashSet;
        let mut d = Dictionary::new();
        let id = d.intern_entity("<a>");
        let t = d.term(id).unwrap();
        assert!(matches!(t, TermStr::Borrowed(_)));
        assert_eq!(t, "<a>");
        assert_eq!(t, *"<a>");
        assert_eq!(t, String::from("<a>"));
        assert_eq!(&*t, "<a>");
        assert_eq!(t.len(), 3);
        assert_eq!(t.to_string(), "<a>");
        assert_eq!(format!("{t:?}"), "\"<a>\"");
        let shared: TermStr<'static> = TermStr::Shared(Arc::from("<a>"));
        assert_eq!(t, shared, "borrowed and shared compare by text");
        let mut set = HashSet::new();
        set.insert(shared.clone());
        assert!(set.contains("<a>"));
        assert!(set.contains(&t));
        assert!(t < TermStr::Borrowed("<b>"));
        assert_eq!(t.clone().into_string(), "<a>");
        assert_eq!(&*t.clone().into_shared(), "<a>");
        let owned = t.into_owned();
        drop(d);
        assert_eq!(owned, "<a>");
        let s: String = owned.into();
        assert_eq!(s, "<a>");
    }

    #[test]
    fn new_api_matches_the_legacy_api_in_memory() {
        let (_, d) = random_pair(11, 500);
        for id in 0..d.id_limit(Space::Entity) {
            assert_eq!(d.term(id).as_deref(), d.id_to_string(id));
            assert_eq!(d.try_term(id).unwrap().as_deref(), d.id_to_string(id));
        }
        let lits = LITERAL_FIRST_ID..d.id_limit(Space::Literal);
        for id in lits.clone() {
            assert_eq!(d.term(id).as_deref(), d.id_to_string(id));
        }
        for id in 0..d.id_limit(Space::Predicate) {
            assert_eq!(d.pred(id).as_deref(), d.predicate_to_string(id));
        }
        assert_eq!(d.pred(INVALID_PRED_ID), None);
        assert_eq!(d.term(INVALID_ENTITY_LITERAL_ID), None);
        // for_each_term lists exactly the live terms, in id order.
        let mut seen = Vec::new();
        d.for_each_term(Space::Literal, 0, |id, s| {
            seen.push((id, s.to_owned()));
            ControlFlow::Continue(())
        })
        .unwrap();
        let want: Vec<(u32, String)> = lits
            .filter_map(|id| d.id_to_string(id).map(|s| (id, s.to_owned())))
            .collect();
        assert_eq!(seen, want);
        // resolve_many keeps the input order (repeats included).
        let ids: Vec<u32> = vec![3, 1, 3, 0, 4_000_000_000];
        let got = d.resolve_many(Space::Entity, &ids).unwrap();
        for (id, t) in ids.iter().zip(&got) {
            assert_eq!(t.as_deref(), d.id_to_string(*id));
        }
        // materialize of an in-memory dictionary is a shared clone.
        let m = d.materialize().unwrap();
        assert!(m.entities.shares_with(&d.entities));
    }

    #[test]
    fn heap_bytes_counts_each_string_once() {
        let mut d = Dictionary::new();
        for i in 0..1000 {
            d.intern_entity(&format!("<http://example.org/a/long/prefix/{i:06}>"));
        }
        let strings: usize = (0..1000).map(|i| d.term(i).unwrap().len()).sum();
        let bytes = d.heap_bytes();
        assert!(bytes >= strings);
        // One copy of the strings plus Arc headers, the id vector and the table.
        assert!(bytes < strings + 1000 * (16 + 16) + 4096 * 8, "{bytes}");
    }

    // ---- Phase 6 R4: reclamation ------------------------------------------

    #[test]
    fn freed_ids_are_reused_lowest_first() {
        let mut d = Dictionary::new();
        for i in 0..6 {
            assert_eq!(d.intern_entity(&format!("<e{i}>")), i);
        }
        // Free 4, 1, 3 (in that order): reuse is 1, 3, 4 — not LIFO.
        for k in ["<e4>", "<e1>", "<e3>"] {
            assert!(d.free_entity(k).is_some());
        }
        assert_eq!(d.free_counts()[0], 3);
        assert_eq!(d.intern_entity("<n0>"), 1);
        assert_eq!(d.intern_entity("<n1>"), 3);
        assert_eq!(d.intern_entity("<n2>"), 4);
        assert_eq!(d.intern_entity("<n3>"), 6, "then the space extends");
        // Literals reuse by public id as well.
        let a = d.intern_literal("\"a\"");
        let b = d.intern_literal("\"b\"");
        let _c = d.intern_literal("\"c\"");
        d.free_literal("\"b\"");
        d.free_literal("\"a\"");
        assert_eq!(d.intern_literal("\"x\""), a);
        assert_eq!(d.intern_literal("\"y\""), b);
    }

    #[test]
    fn freeing_the_end_trims_with_a_cascade() {
        let mut d = Dictionary::new();
        for i in 0..5 {
            d.intern_entity(&format!("<e{i}>"));
        }
        d.free_entity("<e1>");
        d.free_entity("<e3>");
        assert_eq!(d.id_limit(Space::Entity), 5);
        // Freeing the last id trims it and the free hole before it (3), but
        // stops at the live 2.
        d.free_entity("<e4>");
        assert_eq!(d.id_limit(Space::Entity), 3);
        assert_eq!(d.free_counts()[0], 1, "only 1 stays free");
        d.free_entity("<e2>");
        assert_eq!(d.id_limit(Space::Entity), 1, "2 then 1 trimmed");
        assert_eq!(d.free_counts()[0], 0);
        assert_eq!(d.intern_entity("<z>"), 1);
        // A trimmed dictionary still encodes as the model does.
        let bytes = bincode::serialize(&d).unwrap();
        let back: Dictionary = bincode::deserialize(&bytes).unwrap();
        assert_eq!(bincode::serialize(&back).unwrap(), bytes);
    }

    #[test]
    fn a_pin_holds_retired_ids_until_released() {
        let mut d = Dictionary::new();
        for i in 0..4 {
            d.intern_entity(&format!("<e{i}>"));
        }
        let early = d.pin();
        assert!(d.retire(Space::Entity, 1));
        // Retired: gone both ways, but not reusable while the pin lives.
        assert_eq!(d.entity_id("<e1>"), None);
        assert_eq!(d.term(1), None);
        assert_eq!(d.entity_num(), 3);
        assert_eq!(d.release(), 0);
        assert_eq!(d.retired_counts(), [1, 0, 0]);
        assert_eq!(d.intern_entity("<new>"), 4, "the pinned id is not reused");
        // A pin taken after the retirement does not hold it.
        let late = d.pin();
        drop(early);
        assert_eq!(d.release(), 1);
        assert_eq!(d.retired_counts(), [0, 0, 0]);
        assert_eq!(d.intern_entity("<again>"), 1);
        drop(late);
        // The next intern releases what a dropped pin held.
        let p = d.pin();
        d.retire(Space::Entity, 2);
        drop(p);
        assert_eq!(d.intern_entity("<third>"), 2);
        // A retired id is saved as free (no pin survives a reopen).
        let p = d.pin();
        d.retire(Space::Entity, 0);
        let back: Dictionary = bincode::deserialize(&bincode::serialize(&d).unwrap()).unwrap();
        assert_eq!(back.free_counts()[0], 1);
        assert_eq!(back.retired_counts(), [0, 0, 0]);
        drop(p);
    }

    #[test]
    fn retire_on_a_shared_clone_copies_and_keeps_the_clone() {
        let mut a = Dictionary::new();
        a.intern_entity("<x>");
        a.intern_entity("<y>");
        let b = a.clone();
        assert!(a.free_id(Space::Entity, 0));
        assert_eq!(a.entity_id("<x>"), None);
        assert_eq!(b.entity_id("<x>"), Some(0), "the clone keeps its version");
        assert_eq!(a.intern_entity("<z>"), 0);
        assert_eq!(b.term(0).as_deref(), Some("<x>"));
        // Retiring an id that names nothing copies nothing.
        let c = a.clone();
        assert!(!a.retire(Space::Entity, 99));
        assert!(a.entities.shares_with(&c.entities));
    }

    #[test]
    fn from_slots_keeps_ids_and_normalizes() {
        let some = |s: &str| Some(s.to_string());
        let d = Dictionary::from_slots(
            vec![some("<a>"), None, some("<c>"), None, None],
            vec![None, some("\"l\"")],
            vec![some("<p>")],
        )
        .unwrap();
        assert_eq!(d.entity_id("<c>"), Some(2));
        assert_eq!(d.literal_id("\"l\""), Some(LITERAL_FIRST_ID + 1));
        assert_eq!(
            d.id_limits(),
            [3, LITERAL_FIRST_ID + 2, 1],
            "trailing holes trimmed"
        );
        assert_eq!(d.free_counts(), [1, 1, 0]);
        assert_eq!(
            (d.entity_num(), d.literal_num(), d.predicate_num()),
            (2, 1, 1)
        );
        let mut d = d;
        assert_eq!(d.intern_entity("<b>"), 1);
        assert_eq!(d.intern_literal("\"k\""), LITERAL_FIRST_ID);
        // A term stored twice is refused.
        assert!(Dictionary::from_slots(vec![some("<a>"), some("<a>")], vec![], vec![]).is_err());
    }

    #[test]
    fn legacy_files_are_normalized_on_load() {
        // An older binary's file: LIFO free stack, trailing holes, and a hole
        // the free stack does not name.
        let mut old = ModelDictionary::default();
        for i in 0..8 {
            old.entities.legacy_intern(&format!("<e{i}>"));
        }
        for k in ["<e2>", "<e6>", "<e7>", "<e4>"] {
            old.entities.legacy_free(k);
        }
        old.entities.backward.push(None); // an unlisted trailing hole
        old.entities.free.retain(|&i| i != 4); // an unlisted interior hole
        let bytes = bincode::serialize(&old).unwrap();
        let mut d: Dictionary = bincode::deserialize(&bytes).unwrap();
        // Holes 2 and 4 are free, 6.. are trimmed.
        assert_eq!(d.id_limit(Space::Entity), 6);
        assert_eq!(d.free_counts()[0], 2);
        assert_eq!(d.entity_num(), 4);
        assert_eq!(d.intern_entity("<n0>"), 2);
        assert_eq!(d.intern_entity("<n1>"), 4);
        assert_eq!(d.intern_entity("<n2>"), 6);
        // A free list naming an index twice is refused.
        let mut dup = ModelDictionary::default();
        dup.entities.legacy_intern("<a>");
        dup.entities.legacy_intern("<b>");
        dup.entities.legacy_free("<a>");
        dup.entities.free.push(0);
        assert!(bincode::deserialize::<Dictionary>(&bincode::serialize(&dup).unwrap()).is_err());
    }

    #[test]
    fn allocations_are_tracked_on_demand() {
        let mut d = Dictionary::new();
        d.intern_entity("<old>");
        assert!(d.take_allocations().is_empty(), "off by default");
        d.track_allocations(true);
        let a = d.intern_entity("<a>");
        d.intern_entity("<old>");
        let p = d.intern_predicate("<p>");
        assert_eq!(
            d.take_allocations(),
            vec![TermId::node(a), TermId::predicate(p)]
        );
        assert!(d.take_allocations().is_empty());
        d.track_allocations(false);
        d.intern_entity("<b>");
        assert!(d.take_allocations().is_empty());
        // `try_intern_new` tells new from known.
        assert!(!d.try_intern_new(Space::Entity, "<b>").unwrap().1);
        assert!(d.try_intern_new(Space::Entity, "<c>").unwrap().1);
    }

    #[test]
    fn unreferenced_lists_dead_terms_by_space() {
        let mut d = Dictionary::new();
        let a = d.intern_entity("<a>");
        let _b = d.intern_entity("<b>");
        let l = d.intern_literal("\"l\"");
        let p = d.intern_predicate("<p>");
        let keep = [TermId::node(a), TermId::node(l)];
        let dead = d.unreferenced(|t| keep.contains(&t));
        assert_eq!(dead, vec![TermId::node(1), TermId::predicate(p)]);
    }
}
