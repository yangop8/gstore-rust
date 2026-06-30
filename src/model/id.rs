//! Integer identifiers and ID-space conventions.
//!
//! Mirrors gStore's `GlobalTypedef.h`. gStore encodes every RDF term as an
//! integer and distinguishes *what* a term is purely by its numeric range:
//!
//! * entities (IRIs, blank nodes)  → `[0, LITERAL_FIRST_ID)`
//! * literals                      → `[LITERAL_FIRST_ID, 2 * LITERAL_FIRST_ID)`
//! * predicates                    → a *separate* space `[0, …)`
//!
//! Because subjects are always entities and an object may be either an entity
//! or a literal, the single `u32` "entity-or-literal" id is enough at the
//! object position: `id >= LITERAL_FIRST_ID` means "this object is a literal".

use serde::{Deserialize, Serialize};

/// Type of an entity or literal id (gStore: `TYPE_ENTITY_LITERAL_ID`, `unsigned`).
pub type EntityLiteralId = u32;

/// Type of a predicate id (gStore: `TYPE_PREDICATE_ID`, `int`). We use `u32`:
/// predicates are always allocated non-negative, and [`INVALID_PRED_ID`] stands
/// in for the C++ sentinel `-1`.
pub type PredId = u32;

/// Sentinel for "no such entity/literal" (gStore: `INVALID_ENTITY_LITERAL_ID`).
pub const INVALID_ENTITY_LITERAL_ID: EntityLiteralId = u32::MAX;

/// Sentinel for "no such predicate" (gStore: `INVALID_PREDICATE_ID == -1`).
pub const INVALID_PRED_ID: PredId = u32::MAX;

/// First literal id. Object ids `>= LITERAL_FIRST_ID` denote literals; smaller
/// ids denote entities. Identical to gStore's `LITERAL_FIRST_ID`.
pub const LITERAL_FIRST_ID: EntityLiteralId = 2_000_000_000;

/// Is this entity-or-literal id an entity (IRI / blank node)?
#[inline]
pub fn is_entity_id(id: EntityLiteralId) -> bool {
    id < LITERAL_FIRST_ID
}

/// Is this entity-or-literal id a literal?
#[inline]
pub fn is_literal_id(id: EntityLiteralId) -> bool {
    id >= LITERAL_FIRST_ID && id != INVALID_ENTITY_LITERAL_ID
}

/// What an object id refers to. Used to reconstruct the right RDF term and to
/// route inserts into the entity- vs literal-side object index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectKind {
    Entity,
    Literal,
}

impl ObjectKind {
    /// Classify an object id by its range.
    #[inline]
    pub fn of(id: EntityLiteralId) -> ObjectKind {
        if is_literal_id(id) {
            ObjectKind::Literal
        } else {
            ObjectKind::Entity
        }
    }
}

