//! Bidirectional string↔id dictionaries.
//!
//! Corresponds to gStore's `KVstore` dictionary trees:
//! `entity2id`/`id2entity`, `literal2id`/`id2literal`, `predicate2id`/`id2predicate`.
//!
//! Three independent id spaces are maintained (see [`crate::model::id`]):
//! entities, literals, and predicates. Internally each is a vector (`id → string`)
//! plus a hash map (`string → id`). Literals are stored at internal index `i` but
//! exposed with the public id `i + LITERAL_FIRST_ID`, exactly matching gStore so
//! that an object id alone reveals whether it is an entity or a literal.
//!
//! Keys are [`Term::dict_key`] strings (full N-Triples surface syntax) so an IRI
//! `<foo>` never collides with a same-named literal `"foo"`.
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

pub mod prefix;

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{GStoreError, Result};
use crate::model::id::{
    EntityLiteralId, PredId, INVALID_ENTITY_LITERAL_ID, INVALID_PRED_ID, LITERAL_FIRST_ID,
};
use crate::model::Term;

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

/// An *out-of-core* dictionary backend: resolves str↔id against on-disk B+trees
/// on demand instead of holding every string in RAM. Implemented by the kvstore
/// (`DiskDict`) so a query over a disk-backed store needs only its touched terms
/// resident, not the whole dictionary.
///
/// Mirrors gStore reading `entity2id`/`id2entity` (etc.) trees through the buffer
/// cache. A [`Dictionary`] carrying a backing routes its lookups here; an
/// ordinary in-memory dictionary leaves it `None` and behaves exactly as before.
///
/// `id_to_string` / `predicate_to_string` return `&str` valid for the backing's
/// lifetime; an implementation that fetches lazily from disk is expected to
/// retain the materialized string (so only *looked-up* keys become resident).
/// `Send + Sync` is required because a [`Dictionary`] is shared across reader
/// threads inside a snapshot.
pub trait DiskTermSource: std::fmt::Debug + Send + Sync {
    /// Entity term key → entity id, via the on-disk `entity2id` tree.
    fn entity_id(&self, key: &str) -> Option<EntityLiteralId>;
    /// Literal term key → public literal id, via the on-disk `literal2id` tree.
    fn literal_id(&self, key: &str) -> Option<EntityLiteralId>;
    /// Predicate term key → predicate id, via the on-disk `predicate2id` tree.
    fn predicate_id(&self, key: &str) -> Option<PredId>;
    /// Entity/literal id → its surface string (materialized on demand).
    fn id_to_string(&self, id: EntityLiteralId) -> Option<&str>;
    /// Predicate id → its surface string (materialized on demand).
    fn predicate_to_string(&self, id: PredId) -> Option<&str>;
    fn entity_num(&self) -> usize;
    fn literal_num(&self) -> usize;
    fn predicate_num(&self) -> usize;
    /// Number of strings currently materialized in RAM (for tests / metrics that
    /// assert only the looked-up subset is resident).
    fn resident_string_count(&self) -> usize;
}

/// One half of a dictionary: a forward map `string → index` and a backward
/// vector `index → string`. Indices are dense and assigned in insertion order.
///
/// Freed ids are *reclaimed*: removing a string leaves a hole (`None`) in
/// `backward` and pushes the index onto a `free` stack, mirroring gStore's
/// per-space `freelist` + `limitID`. The next [`intern`](Self::intern) of a
/// brand-new string pops a freed index before extending `backward`, so ids of
/// deleted terms are reused instead of growing the id space unbounded.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Interner {
    forward: HashMap<String, u32>,
    /// `index - base → Some(string)`; a `None` slot is a reclaimed (freed) hole.
    backward: Vec<Option<String>>,
    /// Reclaimed indices available for reuse (gStore's `freelist`), LIFO.
    free: Vec<u32>,
    /// The index stored at `backward[0]`. Always `0` except in a dictionary whose
    /// counters a test advanced with [`Dictionary::debug_advance_id_counters`]
    /// to exercise the id-space bounds without allocating billions of strings.
    /// Not persisted (skipped by serde, so the on-disk format is unchanged).
    #[serde(skip)]
    base: u32,
}

impl Interner {
    /// Intern `s`, returning its dense index. Re-interning is idempotent;
    /// otherwise a freed index is reused before the id space is extended.
    /// `check` validates (and may reject) a brand-new index against the owning
    /// space's bound; a reused index is always in range.
    fn try_intern(&mut self, s: &str, check: impl Fn(u64) -> Result<u32>) -> Result<u32> {
        if let Some(&id) = self.forward.get(s) {
            return Ok(id);
        }
        let id = if let Some(reused) = self.free.pop() {
            self.backward[(reused - self.base) as usize] = Some(s.to_owned());
            reused
        } else {
            let id = check(u64::from(self.base) + self.backward.len() as u64)?;
            self.backward.push(Some(s.to_owned()));
            id
        };
        self.forward.insert(s.to_owned(), id);
        Ok(id)
    }

    /// Look up the dense index for `s`, if interned.
    fn get(&self, s: &str) -> Option<u32> {
        self.forward.get(s).copied()
    }

    /// Resolve a dense index back to its string (`None` for a freed hole).
    fn resolve(&self, idx: u32) -> Option<&str> {
        let slot = idx.checked_sub(self.base)? as usize;
        self.backward.get(slot).and_then(Option::as_deref)
    }

    /// Free `s`'s index for reuse, returning it if `s` was interned.
    fn free(&mut self, s: &str) -> Option<u32> {
        let idx = self.forward.remove(s)?;
        self.backward[(idx - self.base) as usize] = None;
        self.free.push(idx);
        Some(idx)
    }

    /// Number of *live* (non-freed) strings.
    fn len(&self) -> usize {
        self.forward.len()
    }

    /// All live interned strings (skips freed holes), in id order.
    fn strings(&self) -> impl Iterator<Item = &str> {
        self.backward.iter().filter_map(Option::as_deref)
    }

    /// Live `(index, string)` pairs (skips freed holes).
    fn entries(&self) -> impl Iterator<Item = (u32, &str)> {
        let base = self.base;
        self.backward
            .iter()
            .enumerate()
            .filter_map(move |(i, s)| s.as_deref().map(|s| (base + i as u32, s)))
    }
}

/// The full RDF dictionary: entities, literals, and predicates.
///
/// Two modes share one type so the query engine (which takes `&Dictionary`) runs
/// over either without change:
/// * *in-memory* — `backing == None`; the three [`Interner`]s hold every string.
/// * *out-of-core* — `backing == Some(_)`; the interners stay empty and every
///   lookup is served from on-disk B+trees on demand (see [`DiskTermSource`]).
///   Built by `DiskStore` so a disk query never materializes the whole
///   dictionary in RAM.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Dictionary {
    entities: Interner,
    literals: Interner,
    predicates: Interner,
    /// Out-of-core backend, when this dictionary is disk-backed. Never persisted
    /// (it is reconstructed from the on-disk trees), so it is skipped by serde
    /// and defaults to `None` on load — keeping the in-memory path byte-identical.
    #[serde(skip)]
    backing: Option<Arc<dyn DiskTermSource>>,
}

impl Dictionary {
    pub fn new() -> Dictionary {
        Dictionary::default()
    }

    /// Build a disk-backed dictionary that resolves str↔id through `backing`
    /// (the on-disk B+trees) on demand. The in-memory interners stay empty;
    /// only looked-up terms ever become resident. Used by `DiskStore::query` so
    /// a database larger than RAM is queryable without a full-dictionary load.
    pub fn from_backing(backing: Arc<dyn DiskTermSource>) -> Dictionary {
        Dictionary {
            entities: Interner::default(),
            literals: Interner::default(),
            predicates: Interner::default(),
            backing: Some(backing),
        }
    }

    /// Whether this dictionary resolves lookups from disk on demand.
    pub fn is_disk_backed(&self) -> bool {
        self.backing.is_some()
    }

    // ---- entity space -----------------------------------------------------

    /// Intern an entity (IRI or blank node), returning its entity id in
    /// `[0, LITERAL_FIRST_ID)`, or an error once the entity space is exhausted
    /// (see the module docs). Build and insert paths use this checked form.
    pub fn try_intern_entity(&mut self, key: &str) -> Result<EntityLiteralId> {
        self.entities.try_intern(key, checked_entity_id)
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

    pub fn entity_id(&self, key: &str) -> Option<EntityLiteralId> {
        if let Some(b) = &self.backing {
            return b.entity_id(key);
        }
        self.entities.get(key)
    }

    // ---- literal space ----------------------------------------------------

    /// Intern a literal, returning its public id (its dense index offset by
    /// `LITERAL_FIRST_ID`), or an error once the literal space is exhausted.
    pub fn try_intern_literal(&mut self, key: &str) -> Result<EntityLiteralId> {
        let idx = self.literals.try_intern(key, |next| {
            checked_literal_id(next).map(|id| id - LITERAL_FIRST_ID)
        })?;
        Ok(LITERAL_FIRST_ID + idx)
    }

    /// Infallible [`try_intern_literal`](Self::try_intern_literal).
    ///
    /// # Panics
    /// When the literal id space is exhausted.
    pub fn intern_literal(&mut self, key: &str) -> EntityLiteralId {
        self.try_intern_literal(key)
            .unwrap_or_else(|e| panic!("{e} (use try_intern_literal to handle this)"))
    }

    pub fn literal_id(&self, key: &str) -> Option<EntityLiteralId> {
        if let Some(b) = &self.backing {
            return b.literal_id(key);
        }
        // Checked (not `expect`): a corrupt persisted dictionary with an
        // out-of-range index reads as "absent" instead of panicking a query.
        self.literals
            .get(key)
            .and_then(|i| i.checked_add(LITERAL_FIRST_ID))
            .filter(|&id| id != INVALID_ENTITY_LITERAL_ID)
    }

    // ---- predicate space --------------------------------------------------

    /// Intern a predicate, returning its id in `[0, PREDICATE_ID_LIMIT)`, or an
    /// error once the predicate space is exhausted.
    pub fn try_intern_predicate(&mut self, key: &str) -> Result<PredId> {
        self.predicates.try_intern(key, checked_predicate_id)
    }

    /// Infallible [`try_intern_predicate`](Self::try_intern_predicate).
    ///
    /// # Panics
    /// When the predicate id space is exhausted.
    pub fn intern_predicate(&mut self, key: &str) -> PredId {
        self.try_intern_predicate(key)
            .unwrap_or_else(|e| panic!("{e} (use try_intern_predicate to handle this)"))
    }

    pub fn predicate_id(&self, key: &str) -> Option<PredId> {
        if let Some(b) = &self.backing {
            return b.predicate_id(key);
        }
        self.predicates.get(key)
    }

    // ---- generic term helpers --------------------------------------------

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
        let key = t.dict_key();
        if t.is_literal() {
            self.try_intern_literal(&key)
        } else {
            self.try_intern_entity(&key)
        }
    }

    /// Resolve a term to its existing id without interning.
    pub fn term_id(&self, t: &Term) -> Option<EntityLiteralId> {
        let key = t.dict_key();
        if t.is_literal() {
            self.literal_id(&key)
        } else {
            self.entity_id(&key)
        }
    }

    /// Resolve an entity-or-literal id back to its dictionary key string.
    /// Routes by id range, mirroring gStore's `getStringByID`.
    pub fn id_to_string(&self, id: EntityLiteralId) -> Option<&str> {
        if id == INVALID_ENTITY_LITERAL_ID {
            None
        } else if let Some(b) = &self.backing {
            b.id_to_string(id)
        } else if id >= LITERAL_FIRST_ID {
            self.literals.resolve(id - LITERAL_FIRST_ID)
        } else {
            self.entities.resolve(id)
        }
    }

    /// Resolve a predicate id back to its key string.
    pub fn predicate_to_string(&self, id: PredId) -> Option<&str> {
        if id == INVALID_PRED_ID {
            None
        } else if let Some(b) = &self.backing {
            b.predicate_to_string(id)
        } else {
            self.predicates.resolve(id)
        }
    }

    // ---- counts (mirror Database::getEntityNum etc.) ----------------------

    pub fn entity_num(&self) -> usize {
        match &self.backing {
            Some(b) => b.entity_num(),
            None => self.entities.len(),
        }
    }
    pub fn literal_num(&self) -> usize {
        match &self.backing {
            Some(b) => b.literal_num(),
            None => self.literals.len(),
        }
    }
    pub fn predicate_num(&self) -> usize {
        match &self.backing {
            Some(b) => b.predicate_num(),
            None => self.predicates.len(),
        }
    }

    /// Live `(id, string)` pairs for entities and literals — the id space that
    /// binds subject/object variables (predicates are excluded: separate space).
    /// Entity ids are their interner index; literal ids are offset by
    /// [`LITERAL_FIRST_ID`], matching [`id_to_string`](Self::id_to_string).
    ///
    /// Empty for a disk-backed dictionary (its strings live on disk, not in the
    /// interners); callers treat that as "no in-memory string index available"
    /// and fall back to a scan.
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
    /// only the subset looked up so far (used to assert out-of-core behavior).
    pub fn resident_string_count(&self) -> usize {
        match &self.backing {
            Some(b) => b.resident_string_count(),
            None => self.entities.len() + self.literals.len() + self.predicates.len(),
        }
    }

    // ---- id freelist / reclamation (gStore freeEntityID etc.) -------------

    /// Free an entity term's id for reuse, returning it if it was interned.
    /// The caller must ensure the term is no longer referenced (see
    /// [`Database::reclaim_unused`](crate::Database::reclaim_unused)).
    pub fn free_entity(&mut self, key: &str) -> Option<EntityLiteralId> {
        self.entities.free(key)
    }

    /// Free a literal term's public id for reuse, if it was interned.
    pub fn free_literal(&mut self, key: &str) -> Option<EntityLiteralId> {
        self.literals
            .free(key)
            .and_then(|i| i.checked_add(LITERAL_FIRST_ID))
    }

    /// Free a predicate term's id for reuse, if it was interned.
    pub fn free_predicate(&mut self, key: &str) -> Option<PredId> {
        self.predicates.free(key)
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
        let mut freed = 0usize;

        // Entities: dense index is the public id.
        let dead_ent: Vec<String> = self
            .entities
            .entries()
            .filter(|&(id, _)| !referenced_ent_lit.contains(&id))
            .map(|(_, s)| s.to_owned())
            .collect();
        for key in dead_ent {
            if self.entities.free(&key).is_some() {
                freed += 1;
            }
        }

        // Literals: public id = internal index + LITERAL_FIRST_ID.
        let dead_lit: Vec<String> = self
            .literals
            .entries()
            .filter(|&(i, _)| !referenced_ent_lit.contains(&i.wrapping_add(LITERAL_FIRST_ID)))
            .map(|(_, s)| s.to_owned())
            .collect();
        for key in dead_lit {
            if self.literals.free(&key).is_some() {
                freed += 1;
            }
        }

        // Predicates: dense index is the predicate id.
        let dead_pred: Vec<String> = self
            .predicates
            .entries()
            .filter(|&(id, _)| !referenced_pred.contains(&id))
            .map(|(_, s)| s.to_owned())
            .collect();
        for key in dead_pred {
            if self.predicates.free(&key).is_some() {
                freed += 1;
            }
        }

        freed
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
        let empty = |i: &Interner| i.forward.is_empty() && i.backward.is_empty();
        if self.backing.is_some()
            || !empty(&self.entities)
            || !empty(&self.literals)
            || !empty(&self.predicates)
        {
            return Err(GStoreError::Database(
                "debug_advance_id_counters needs an empty in-memory dictionary".into(),
            ));
        }
        self.entities.base = next_entity;
        self.literals.base = next_literal_index;
        self.predicates.base = next_predicate;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            forward: i.forward.clone(),
            backward: i.backward.clone(),
            free: i.free.clone(),
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
}
