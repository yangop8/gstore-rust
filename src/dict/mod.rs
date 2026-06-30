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

pub mod prefix;

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::id::{
    EntityLiteralId, PredId, INVALID_ENTITY_LITERAL_ID, INVALID_PRED_ID, LITERAL_FIRST_ID,
};
use crate::model::Term;

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
    /// `index → Some(string)`; a `None` slot is a reclaimed (freed) hole.
    backward: Vec<Option<String>>,
    /// Reclaimed indices available for reuse (gStore's `freelist`), LIFO.
    free: Vec<u32>,
}

impl Interner {
    /// Intern `s`, returning its dense index. Re-interning is idempotent;
    /// otherwise a freed index is reused before the id space is extended.
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.forward.get(s) {
            return id;
        }
        let id = if let Some(reused) = self.free.pop() {
            self.backward[reused as usize] = Some(s.to_owned());
            reused
        } else {
            let id = u32::try_from(self.backward.len())
                .expect("dictionary interner exceeded u32::MAX distinct strings");
            self.backward.push(Some(s.to_owned()));
            id
        };
        self.forward.insert(s.to_owned(), id);
        id
    }

    /// Look up the dense index for `s`, if interned.
    fn get(&self, s: &str) -> Option<u32> {
        self.forward.get(s).copied()
    }

    /// Resolve a dense index back to its string (`None` for a freed hole).
    fn resolve(&self, idx: u32) -> Option<&str> {
        self.backward.get(idx as usize).and_then(Option::as_deref)
    }

    /// Free `s`'s index for reuse, returning it if `s` was interned.
    fn free(&mut self, s: &str) -> Option<u32> {
        let idx = self.forward.remove(s)?;
        self.backward[idx as usize] = None;
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
        self.backward
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_deref().map(|s| (i as u32, s)))
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
    /// `[0, LITERAL_FIRST_ID)`.
    pub fn intern_entity(&mut self, key: &str) -> EntityLiteralId {
        self.entities.intern(key)
    }

    pub fn entity_id(&self, key: &str) -> Option<EntityLiteralId> {
        if let Some(b) = &self.backing {
            return b.entity_id(key);
        }
        self.entities.get(key)
    }

    // ---- literal space ----------------------------------------------------

    /// Intern a literal, returning its public id, offset by `LITERAL_FIRST_ID`.
    pub fn intern_literal(&mut self, key: &str) -> EntityLiteralId {
        self.literals
            .intern(key)
            .checked_add(LITERAL_FIRST_ID)
            .expect("literal id space overflowed EntityLiteralId range")
    }

    pub fn literal_id(&self, key: &str) -> Option<EntityLiteralId> {
        if let Some(b) = &self.backing {
            return b.literal_id(key);
        }
        self.literals.get(key).map(|i| {
            i.checked_add(LITERAL_FIRST_ID)
                .expect("literal id space overflowed EntityLiteralId range")
        })
    }

    // ---- predicate space --------------------------------------------------

    pub fn intern_predicate(&mut self, key: &str) -> PredId {
        self.predicates.intern(key)
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
    pub fn intern_term(&mut self, t: &Term) -> EntityLiteralId {
        let key = t.dict_key();
        if t.is_literal() {
            self.intern_literal(&key)
        } else {
            self.intern_entity(&key)
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

    // ---- prefix-compressed (front-coded) export --------------------------

    /// Every dictionary string — entities, literals, and predicates — merged,
    /// sorted, and de-duplicated. Sorting clusters shared prefixes so the set
    /// front-codes well. (Disk-backed dictionaries keep no in-memory strings, so
    /// this is empty for them.)
    pub fn all_strings_sorted(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::with_capacity(
            self.entities.len() + self.literals.len() + self.predicates.len(),
        );
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
            .map(|i| i.checked_add(LITERAL_FIRST_ID).expect("literal id overflow"))
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
            .filter(|&(i, _)| !referenced_ent_lit.contains(&(i + LITERAL_FIRST_ID)))
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
}
