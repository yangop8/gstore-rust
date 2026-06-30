//! Persistent storage backends (feature-gated).
//!
//! These backends implement the same [`TripleSource`](crate::store::TripleSource)
//! + [`MutableStore`](crate::store::MutableStore) seam as the in-memory
//! [`TripleStore`](crate::store::TripleStore), so the query engine, optimizer,
//! VS-tree, and analytics run unchanged on top of them. Compiled only when the
//! `rocksdb` Cargo feature is enabled.

pub mod rocks;
