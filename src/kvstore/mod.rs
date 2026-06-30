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

pub mod bptree;
pub mod pager;
pub mod store;

pub use pager::{PageId, Pager, PAGE_SIZE};
pub use store::DiskStore;
