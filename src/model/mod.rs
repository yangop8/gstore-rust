//! The RDF data model: terms, triples, and integer id conventions.

pub mod id;
pub mod term;

pub use id::{
    is_entity_id, is_literal_id, EntityLiteralId, ObjectKind, PredId, INVALID_ENTITY_LITERAL_ID,
    INVALID_PRED_ID, LITERAL_FIRST_ID,
};
pub use term::{ObjectType, Term, Triple};

/// A triple expressed entirely in integer ids (gStore: `ID_TUPLE`).
///
/// `sub`/`obj` live in the entity-or-literal space; `pred` in the predicate
/// space. `obj`'s kind (entity vs literal) is recoverable from its range via
/// [`ObjectKind::of`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IdTriple {
    pub sub: EntityLiteralId,
    pub pred: PredId,
    pub obj: EntityLiteralId,
}

impl IdTriple {
    pub fn new(sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> IdTriple {
        IdTriple { sub, pred, obj }
    }
}

