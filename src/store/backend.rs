//! The runtime-selectable storage backend a [`Database`](crate::Database)
//! holds, and its static dispatch over the [`TripleSource`] /
//! [`MutableStore`] seam (moved here from `store/mod.rs`, unchanged; re-exported
//! as `crate::store::Backend`).

use crate::model::id::{EntityLiteralId, PredId};
use crate::model::IdTriple;

use super::{MutableStore, TripleSource, TripleStore};

/// The runtime-selectable storage backend the [`Database`](crate::Database)
/// holds (DESIGN §"Phase 2", route A). It is a [`StorageBackend`](super::StorageBackend)
/// in its own right: it implements [`TripleSource`] + [`MutableStore`] by
/// **statically** dispatching to the active variant (a branch, not a vtable),
/// so the query engine / optimizer / VS-tree / analytics run over it unchanged
/// while the physical store is chosen at construction time.
///
/// The default is `Memory` — every existing constructor wraps a fresh
/// [`TripleStore`], so behaviour with the `rocksdb` feature *off* is identical.
/// An on-disk (`kvstore.kv`) database also evaluates over a `Memory` working
/// set: the `Database` facade writes every committed mutation through to its
/// [`DiskStore`](crate::kvstore::DiskStore), which stays the source of truth.
#[derive(Debug)]
pub enum Backend {
    /// In-memory triple store (the default; three sorted adjacency maps).
    Memory(TripleStore),
    /// Persistent RocksDB triple store (feature `rocksdb`).
    #[cfg(feature = "rocksdb")]
    Rocks(crate::backend::rocks::RocksStore),
}

/// Run `$body` against the inner store of whichever [`Backend`] variant is
/// active, binding it to `$inner`. Pure static dispatch — no trait object.
macro_rules! backend_dispatch {
    ($self:expr, $inner:ident, $body:expr) => {
        match $self {
            Backend::Memory($inner) => $body,
            #[cfg(feature = "rocksdb")]
            Backend::Rocks($inner) => $body,
        }
    };
}

impl Backend {
    /// The in-memory store, if this is the `Memory` variant. Lets components
    /// that are intrinsically in-memory (snapshot MVCC, named-graph clones)
    /// borrow the concrete store without a downcast.
    pub fn as_memory(&self) -> Option<&TripleStore> {
        match self {
            Backend::Memory(s) => Some(s),
            #[cfg(feature = "rocksdb")]
            Backend::Rocks(_) => None,
        }
    }

    /// The RocksDB store, if this is the `Rocks` variant.
    #[cfg(feature = "rocksdb")]
    pub fn as_rocks(&self) -> Option<&crate::backend::rocks::RocksStore> {
        match self {
            Backend::Rocks(r) => Some(r),
            Backend::Memory(_) => None,
        }
    }

    /// Mutable access to the RocksDB store, if this is the `Rocks` variant.
    #[cfg(feature = "rocksdb")]
    pub fn as_rocks_mut(&mut self) -> Option<&mut crate::backend::rocks::RocksStore> {
        match self {
            Backend::Rocks(r) => Some(r),
            Backend::Memory(_) => None,
        }
    }

    /// Collect (and clear) a write error a persistent variant recorded because
    /// [`MutableStore`]'s `bool` results cannot carry one. Always `None` for the
    /// in-memory store, whose writes cannot fail.
    pub fn take_write_error(&self) -> Option<String> {
        match self {
            Backend::Memory(_) => None,
            #[cfg(feature = "rocksdb")]
            Backend::Rocks(r) => r.take_error(),
        }
    }

    /// All predicate ids present in the store (sorted). Not part of
    /// [`TripleSource`] (the engine never needs it), but the schema extractor
    /// does, so it is dispatched here rather than special-cased on the caller.
    pub fn predicates(&self) -> Vec<PredId> {
        match self {
            Backend::Memory(s) => s.predicates().collect(),
            #[cfg(feature = "rocksdb")]
            Backend::Rocks(r) => r.predicates(),
        }
    }
}

impl TripleSource for Backend {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        backend_dispatch!(self, s, TripleSource::exists(s, sub, pred, obj))
    }
    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        backend_dispatch!(self, s, TripleSource::po_by_s(s, sub))
    }
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        backend_dispatch!(self, s, TripleSource::o_by_sp(s, sub, pred))
    }
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        backend_dispatch!(self, s, TripleSource::p_by_so(s, sub, obj))
    }
    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        backend_dispatch!(self, s, TripleSource::ps_by_o(s, obj))
    }
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        backend_dispatch!(self, s, TripleSource::s_by_po(s, pred, obj))
    }
    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        backend_dispatch!(self, s, TripleSource::so_by_p(s, pred))
    }
    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        backend_dispatch!(self, s, TripleSource::subs_by_p(s, pred))
    }
    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        backend_dispatch!(self, s, TripleSource::objs_by_p(s, pred))
    }
    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        backend_dispatch!(self, s, TripleSource::subject_keys(s))
    }
    fn object_keys(&self) -> Vec<EntityLiteralId> {
        backend_dispatch!(self, s, TripleSource::object_keys(s))
    }
    fn triple_count(&self) -> u64 {
        backend_dispatch!(self, s, TripleSource::triple_count(s))
    }
    fn distinct_subjects(&self) -> usize {
        backend_dispatch!(self, s, TripleSource::distinct_subjects(s))
    }
    fn distinct_objects(&self) -> usize {
        backend_dispatch!(self, s, TripleSource::distinct_objects(s))
    }
    fn num_predicates(&self) -> usize {
        backend_dispatch!(self, s, TripleSource::num_predicates(s))
    }
    fn pred_card(&self, pred: PredId) -> usize {
        backend_dispatch!(self, s, TripleSource::pred_card(s, pred))
    }
    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        backend_dispatch!(self, s, TripleSource::pred_distinct_subj(s, pred))
    }
    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        backend_dispatch!(self, s, TripleSource::pred_distinct_obj(s, pred))
    }
    fn iter_all(&self) -> Vec<IdTriple> {
        backend_dispatch!(self, s, TripleSource::iter_all(s))
    }

    // ---- C1 provided methods: dispatched, so a variant's override is used --

    fn count_po_by_s(&self, sub: EntityLiteralId) -> u64 {
        backend_dispatch!(self, s, TripleSource::count_po_by_s(s, sub))
    }
    fn count_o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> u64 {
        backend_dispatch!(self, s, TripleSource::count_o_by_sp(s, sub, pred))
    }
    fn count_p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> u64 {
        backend_dispatch!(self, s, TripleSource::count_p_by_so(s, sub, obj))
    }
    fn count_ps_by_o(&self, obj: EntityLiteralId) -> u64 {
        backend_dispatch!(self, s, TripleSource::count_ps_by_o(s, obj))
    }
    fn count_s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> u64 {
        backend_dispatch!(self, s, TripleSource::count_s_by_po(s, pred, obj))
    }
    fn count_so_by_p(&self, pred: PredId) -> u64 {
        backend_dispatch!(self, s, TripleSource::count_so_by_p(s, pred))
    }
    fn distinct_p_of_s(&self, sub: EntityLiteralId) -> u64 {
        backend_dispatch!(self, s, TripleSource::distinct_p_of_s(s, sub))
    }
    fn distinct_p_of_o(&self, obj: EntityLiteralId) -> u64 {
        backend_dispatch!(self, s, TripleSource::distinct_p_of_o(s, obj))
    }
    fn has_subject(&self, sub: EntityLiteralId) -> bool {
        backend_dispatch!(self, s, TripleSource::has_subject(s, sub))
    }
    fn has_object(&self, obj: EntityLiteralId) -> bool {
        backend_dispatch!(self, s, TripleSource::has_object(s, obj))
    }
    fn has_predicate(&self, pred: PredId) -> bool {
        backend_dispatch!(self, s, TripleSource::has_predicate(s, pred))
    }
}

impl MutableStore for Backend {
    fn insert(&mut self, t: IdTriple) -> bool {
        backend_dispatch!(self, s, MutableStore::insert(s, t))
    }
    fn remove(&mut self, t: IdTriple) -> bool {
        backend_dispatch!(self, s, MutableStore::remove(s, t))
    }
    fn bulk_load(&mut self, triples: Vec<IdTriple>) {
        backend_dispatch!(self, s, MutableStore::bulk_load(s, triples))
    }
}

impl Default for Backend {
    fn default() -> Backend {
        Backend::Memory(TripleStore::new())
    }
}
