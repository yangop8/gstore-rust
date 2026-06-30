//! RDFS entailment by forward-chaining materialization.
//!
//! Corresponds to gStore's `src/Reason`. Given a store that mixes data and an
//! RDFS schema, [`materialize`] computes the RDFS closure and inserts every
//! entailed triple, iterating to a fixpoint. It implements the core
//! schema-driven rules (the ones that actually change query answers):
//!
//! | rule | premises | conclusion |
//! |------|----------|------------|
//! | subclass transitivity | `c ⊑ d`, `d ⊑ e` | `c ⊑ e` |
//! | type propagation       | `x a c`, `c ⊑ d` | `x a d` |
//! | subproperty transitivity | `p ⊑ₚ q`, `q ⊑ₚ r` | `p ⊑ₚ r` |
//! | subproperty propagation  | `p ⊑ₚ q`, `x p y` | `x q y` |
//! | domain | `p domain c`, `x p y` | `x a c` |
//! | range  | `p range c`,  `x p y` | `y a c` |
//!
//! Note the id-space subtlety: a property used as the *subject* of a schema
//! triple (`p rdfs:subPropertyOf q`) is interned as an **entity**, while the same
//! property in data (`x p y`) is a **predicate** — two different id spaces sharing
//! the dictionary key. [`ent_to_pred`] bridges them through the dictionary.

use std::collections::HashMap;

use crate::dict::Dictionary;
use crate::model::id::PredId;
use crate::model::{IdTriple, Term};
use crate::parser::sparql::ast::RDF_TYPE;
use crate::store::TripleStore;

const RDFS_SUBCLASS: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
const RDFS_SUBPROP: &str = "http://www.w3.org/2000/01/rdf-schema#subPropertyOf";
const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";

/// Materialize the RDFS closure into `store`, returning the triples that were
/// added (so a caller can record them for transaction rollback). Idempotent:
/// re-running on a closed store adds nothing.
pub fn materialize(dict: &mut Dictionary, store: &mut TripleStore) -> Vec<IdTriple> {
    let key = |iri: &str| Term::iri(iri).dict_key();
    let sco_p = dict.predicate_id(&key(RDFS_SUBCLASS));
    let spo_p = dict.predicate_id(&key(RDFS_SUBPROP));
    let dom_p = dict.predicate_id(&key(RDFS_DOMAIN));
    let rng_p = dict.predicate_id(&key(RDFS_RANGE));

    // No RDFS schema vocabulary ⇒ nothing to infer.
    if sco_p.is_none() && spo_p.is_none() && dom_p.is_none() && rng_p.is_none() {
        return Vec::new();
    }
    // Subclass/domain/range rules assert `rdf:type` triples, so the type
    // predicate must exist (intern it if the data never used it directly).
    let needs_type = sco_p.is_some() || dom_p.is_some() || rng_p.is_some();
    let type_p = if needs_type {
        Some(dict.intern_predicate(&key(RDF_TYPE)))
    } else {
        dict.predicate_id(&key(RDF_TYPE))
    };

    // A subproperty target may only ever appear as a schema object (e.g.
    // `:mother rdfs:subPropertyOf :parent` with no `:parent` data triple), so it
    // has no predicate id yet. Intern a predicate id for every property named in
    // a subPropertyOf statement so [`ent_to_pred`] can resolve the target.
    if let Some(spo) = spo_p {
        let prop_ents: Vec<u32> = store
            .so_by_p(spo)
            .iter()
            .flat_map(|&(p, q)| [p, q])
            .collect();
        let keys: Vec<String> = prop_ents
            .iter()
            .filter_map(|&e| dict.id_to_string(e).map(str::to_owned))
            .collect();
        for k in keys {
            dict.intern_predicate(&k);
        }
    }

    let mut added_all: Vec<IdTriple> = Vec::new();
    loop {
        let mut new: Vec<IdTriple> = Vec::new();
        gather(dict, store, type_p, sco_p, spo_p, dom_p, rng_p, &mut new);
        let mut added = 0usize;
        for t in new {
            if store.insert(t) {
                added_all.push(t);
                added += 1;
            }
        }
        if added == 0 {
            break; // fixpoint reached
        }
    }
    added_all
}

/// One forward-chaining round: append all directly-entailed triples to `new`.
#[allow(clippy::too_many_arguments)]
fn gather(
    dict: &Dictionary,
    store: &TripleStore,
    type_p: Option<PredId>,
    sco_p: Option<PredId>,
    spo_p: Option<PredId>,
    dom_p: Option<PredId>,
    rng_p: Option<PredId>,
    new: &mut Vec<IdTriple>,
) {
    // --- subClassOf transitivity + rdf:type propagation ---
    if let Some(sco) = sco_p {
        let pairs = store.so_by_p(sco); // (c, d) meaning c ⊑ d
        let mut sub: HashMap<u32, Vec<u32>> = HashMap::new();
        for &(c, d) in pairs {
            sub.entry(c).or_default().push(d);
        }
        for &(c, d) in pairs {
            if let Some(es) = sub.get(&d) {
                for &e in es {
                    if c != e {
                        new.push(IdTriple::new(c, sco, e));
                    }
                }
            }
        }
        if let Some(tp) = type_p {
            for &(x, c) in store.so_by_p(tp) {
                if let Some(ds) = sub.get(&c) {
                    for &d in ds {
                        new.push(IdTriple::new(x, tp, d));
                    }
                }
            }
        }
    }

    // --- subPropertyOf transitivity + data propagation ---
    if let Some(spo) = spo_p {
        let pairs = store.so_by_p(spo); // (p, q) meaning p ⊑ₚ q (as entities)
        let mut sub: HashMap<u32, Vec<u32>> = HashMap::new();
        for &(p, q) in pairs {
            sub.entry(p).or_default().push(q);
        }
        for &(p, q) in pairs {
            if let Some(rs) = sub.get(&q) {
                for &r in rs {
                    if p != r {
                        new.push(IdTriple::new(p, spo, r));
                    }
                }
            }
        }
        for &(p_ent, q_ent) in pairs {
            let (Some(p_pred), Some(q_pred)) =
                (ent_to_pred(dict, p_ent), ent_to_pred(dict, q_ent))
            else {
                continue;
            };
            if p_pred == q_pred {
                continue;
            }
            for &(x, y) in store.so_by_p(p_pred) {
                new.push(IdTriple::new(x, q_pred, y));
            }
        }
    }

    // --- domain: (p domain c), (x p y) ⇒ (x a c) ---
    if let (Some(dom), Some(tp)) = (dom_p, type_p) {
        for &(p_ent, c) in store.so_by_p(dom) {
            if let Some(p_pred) = ent_to_pred(dict, p_ent) {
                for &(x, _y) in store.so_by_p(p_pred) {
                    new.push(IdTriple::new(x, tp, c));
                }
            }
        }
    }

    // --- range: (p range c), (x p y) ⇒ (y a c) ---
    if let (Some(rng), Some(tp)) = (rng_p, type_p) {
        for &(p_ent, c) in store.so_by_p(rng) {
            if let Some(p_pred) = ent_to_pred(dict, p_ent) {
                for &(_x, y) in store.so_by_p(p_pred) {
                    new.push(IdTriple::new(y, tp, c));
                }
            }
        }
    }
}

/// Map an entity id (a property used as a schema subject/object) to its
/// predicate id, via the shared dictionary key. `None` if that IRI is never used
/// as a predicate in the data.
fn ent_to_pred(dict: &Dictionary, ent: u32) -> Option<PredId> {
    let key = dict.id_to_string(ent)?;
    dict.predicate_id(key)
}

