//! `gadd` — batch-insert RDF data into an existing database.
//!
//! Mirrors gStore's `gadd` (load triples from a file, add them, persist):
//!
//! ```text
//! gadd <db_name> <rdf_file>
//! ```
//!
//! The input format is chosen by file extension: `.nt` (N-Triples), `.ttl`
//! (Turtle), `.nq` (N-Quads), `.trig` (TriG), `.rdf`/`.xml`/`.owl` (RDF/XML);
//! anything else is parsed as Turtle (a superset of N-Triples). Quad formats
//! (N-Quads / TriG) are flattened into the default graph.

use std::path::Path;
use std::process::ExitCode;

use gstore::model::Triple;
use gstore::parser::{nquads, ntriples, rdfxml, trig, turtle};
use gstore::{db_dir_for, Database, Result};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: {} <db_name> <rdf_file>", args[0]);
        return ExitCode::FAILURE;
    }
    let dir = db_dir_for(&args[1]);
    let file = &args[2];

    let mut db = match load_any(&dir) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot load database '{dir}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let triples = match read_triples(file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot read '{file}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let added = triples.iter().filter(|t| db.insert_triple(t)).count();

    if let Err(e) = db.save(&dir) {
        eprintln!("inserted {added} triple(s) but saving failed: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "Added {added} new triple(s) to '{dir}' ({} read; {} now total)",
        triples.len(),
        db.triple_num()
    );
    ExitCode::SUCCESS
}

/// Load a database, auto-detecting the on-disk vs in-memory backend (as gquery).
fn load_any(dir: &str) -> Result<Database> {
    if Database::is_disk(dir) {
        Database::load_disk(dir)
    } else {
        Database::load(dir)
    }
}

/// Read triples from an RDF file, dispatching on the file extension. Quad
/// formats are flattened to their triple part (default graph).
fn read_triples(path: &str) -> Result<Vec<Triple>> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "nt" => ntriples::parse_file(path),
        "ttl" | "turtle" => turtle::parse_file(path),
        "nq" | "nquads" => Ok(nquads::parse_file(path)?.iter().map(|q| q.to_triple()).collect()),
        "trig" => Ok(trig::parse_file(path)?.iter().map(|q| q.to_triple()).collect()),
        "rdf" | "xml" | "owl" | "rdfs" => rdfxml::parse_file(path),
        // Default: Turtle is a superset of N-Triples.
        _ => turtle::parse_file(path),
    }
}
