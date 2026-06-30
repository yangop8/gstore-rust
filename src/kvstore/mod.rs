//! On-disk storage: a paged file with an LRU cache and B+ trees over it,
//! plus a disk-backed triple store and dictionary.
//!
//! This is the faithful counterpart to gStore's `KVstore` (fixed-block B+ tree
//! files + buffer cache + VLists). See `docs/REFACTOR_BACKLOG.md` item A.
//!
//! Layout: [`pager::Pager`] gives fixed [`pager::PAGE_SIZE`] pages; [`bptree::BTree`]
//! builds ordered B+ trees on them; [`store::DiskStore`] composes those into the
//! six-way triple index (three SPO/POS/OSP orderings) plus the string↔id
//! dictionary — enough to build, persist, reopen, and query a database entirely
//! from disk.
//!
//! Space/compaction features layered on top:
//! * [`disk_dict::DiskDict`] backs an *out-of-core* dictionary: a disk query
//!   resolves str↔id from the B+trees on demand instead of loading every string
//!   (only looked-up terms become resident).
//! * [`vlist`] is the compact delta+varint posting-list codec; `DiskStore::compact`
//!   wires it into a `(sub, pred) → objects` value index.
//! * [`overflow`] chains pager pages for VList values too large to fit inline in
//!   a B+tree leaf, mirroring gStore's separate large-block VList file: a
//!   `(sub, pred)` group with an arbitrarily long posting list is written across
//!   a linked list of pages and referenced from the tree by just its head id.
//!
//! ## Differentiated block/array management (gStore IVArray/ISArray)
//! gStore does not store its *id-keyed* tables in a B+ tree: it keeps an array of
//! entries indexed directly by the id, each pointing into a block-managed value
//! file (`IVArray`/`ISArray` + their block managers). [`ivarray::IvArray`] ports
//! that **integer-keyed dense array** and now backs the `id2entity`/`id2literal`/
//! `id2predicate` reverse-dictionary stores (see [`store::DiskStore`]). The
//! *string*-keyed forward dictionary (`entity2id`/…) keeps the variable-key
//! [`bptree::BTree`] — already the right structure for byte keys — and the
//! composite-key triple indexes (SPO/POS/OSP) stay on ordered B+ trees because
//! they need prefix range scans an array can't provide. (gStore's single-id
//! `IVArray` posting-list layout for the triple values is a deeper refactor; the
//! [`vlist`]-compressed `sp2o_vlist` index is the closest analogue here.)

pub mod bptree;
pub mod disk_dict;
pub mod ivarray;
pub mod overflow;
pub mod pager;
pub mod store;
pub mod string_index;
pub mod vlist;

pub use disk_dict::DiskDict;
pub use ivarray::IvArray;
pub use pager::{PageId, Pager, PAGE_SIZE};
pub use store::DiskStore;
pub use string_index::StringIndex;
