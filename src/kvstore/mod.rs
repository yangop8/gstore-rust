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
//!
//! ## Deferred (task 4): differentiated block/array management
//! gStore splits storage into `SITree` (the B+tree of fixed-size signature/value
//! *blocks*), `IVArray`/`ISArray` (integer- vs string-keyed array files), and
//! VList *overflow blocks* that chain pages for values too large for one node.
//! Here a single [`bptree::BTree`] serves every index and values must fit inline
//! in a page, so [`store::DiskStore::compact`] skips any `(sub, pred)` group whose
//! VList would overflow [`pager::PAGE_SIZE`] (reads fall back to the SPO scan).
//! Introducing real overflow-block chains (so arbitrarily long posting lists
//! compress on disk) and separating integer- from string-keyed arrays is the
//! remaining gStore-faithful step; it is intentionally left out of this pass.

pub mod bptree;
pub mod disk_dict;
pub mod pager;
pub mod store;
pub mod vlist;

pub use disk_dict::DiskDict;
pub use pager::{PageId, Pager, PAGE_SIZE};
pub use store::DiskStore;
