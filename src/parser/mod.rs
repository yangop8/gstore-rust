//! Parsing: RDF input (N-Triples) and SPARQL queries.
//!
//! Corresponds to gStore's `src/Parser`. The C++ side bundles a full SPARQL 1.1
//! grammar; the Rust trunk implements N-Triples plus the SPARQL subset the query
//! engine supports (see `docs/REFACTOR_BACKLOG.md` item D for what is deferred).

pub mod jsonld;
pub mod nquads;
pub mod ntriples;
pub mod rdfa;
pub mod rdfxml;
pub mod sparql;
pub mod trig;
pub mod turtle;

use std::path::Path;

use crate::error::Result;
use crate::model::Triple;

/// Parse an RDF file into triples, dispatching on the file extension. Quad
/// formats (N-Quads / TriG) are flattened to their default-graph triple. An
/// unknown extension falls back to Turtle (a superset of N-Triples). This is the
/// single place file-format detection lives — shared by `Database::build_from_files`
/// (gbuild), `gadd`, and `gsub`.
pub fn parse_file_by_ext<P: AsRef<Path>>(path: P) -> Result<Vec<Triple>> {
    let p = path.as_ref();
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        // Turtle is a superset of N-Triples; use it for both so `.nt` files that
        // include Turtle conveniences (e.g. `@prefix`) still load — this matches
        // the long-standing `Database::build_from_files` behavior.
        "nt" | "ntriples" | "ttl" | "turtle" => turtle::parse_file(p),
        "nq" | "nquads" => Ok(nquads::parse_file(p)?.iter().map(|q| q.to_triple()).collect()),
        "trig" => Ok(trig::parse_file(p)?.iter().map(|q| q.to_triple()).collect()),
        "rdf" | "xml" | "owl" | "rdfs" => rdfxml::parse_file(p),
        "jsonld" | "json" => jsonld::parse_file(p),
        "html" | "xhtml" | "htm" => rdfa::parse_file(p),
        // Default: Turtle is a superset of N-Triples.
        _ => turtle::parse_file(p),
    }
}
