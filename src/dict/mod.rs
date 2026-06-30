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

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::id::{
    EntityLiteralId, PredId, INVALID_ENTITY_LITERAL_ID, INVALID_PRED_ID, LITERAL_FIRST_ID,
};
use crate::model::Term;

/// One half of a dictionary: a forward map `string → index` and a backward
/// vector `index → string`. Indices are dense and assigned in insertion order.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Interner {
    forward: HashMap<String, u32>,
    backward: Vec<String>,
}

impl Interner {
    /// Intern `s`, returning its (possibly newly allocated) dense index.
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.forward.get(s) {
            return id;
        }
        let id = u32::try_from(self.backward.len())
            .expect("dictionary interner exceeded u32::MAX distinct strings");
        self.backward.push(s.to_owned());
        self.forward.insert(s.to_owned(), id);
        id
    }

    /// Look up the dense index for `s`, if interned.
    fn get(&self, s: &str) -> Option<u32> {
        self.forward.get(s).copied()
    }

    /// Resolve a dense index back to its string.
    fn resolve(&self, idx: u32) -> Option<&str> {
        self.backward.get(idx as usize).map(String::as_str)
    }

    fn len(&self) -> usize {
        self.backward.len()
    }
}

/// The full RDF dictionary: entities, literals, and predicates.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Dictionary {
    entities: Interner,
    literals: Interner,
    predicates: Interner,
}

impl Dictionary {
    pub fn new() -> Dictionary {
        Dictionary::default()
    }

    // ---- entity space -----------------------------------------------------

    /// Intern an entity (IRI or blank node), returning its entity id in
    /// `[0, LITERAL_FIRST_ID)`.
    pub fn intern_entity(&mut self, key: &str) -> EntityLiteralId {
        self.entities.intern(key)
    }

    pub fn entity_id(&self, key: &str) -> Option<EntityLiteralId> {
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
        } else {
            self.predicates.resolve(id)
        }
    }

    // ---- counts (mirror Database::getEntityNum etc.) ----------------------

    pub fn entity_num(&self) -> usize {
        self.entities.len()
    }
    pub fn literal_num(&self) -> usize {
        self.literals.len()
    }
    pub fn predicate_num(&self) -> usize {
        self.predicates.len()
    }
}

