//! Parsing: RDF input (N-Triples) and SPARQL queries.
//!
//! Corresponds to gStore's `src/Parser`. The C++ side bundles a full SPARQL 1.1
//! grammar; the Rust trunk implements N-Triples plus the SPARQL subset the query
//! engine supports (see `docs/REFACTOR_BACKLOG.md` item D for what is deferred).

pub mod ntriples;
pub mod sparql;
pub mod turtle;
