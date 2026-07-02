//! `gadd` — batch-insert RDF data into an existing database.
//!
//! Mirrors gStore's `gadd` (load triples from a file, add them, persist):
//!
//! ```text
//! gadd <db_name> <rdf_file>
//! ```
//!
//! The input format is chosen by file extension: `.nt` (N-Triples), `.ttl`
//! (Turtle), `.nq` (N-Quads), `.trig` (TriG), `.rdf`/`.xml`/`.owl` (RDF/XML),
//! `.jsonld`/`.json` (JSON-LD), `.html`/`.xhtml` (RDFa); anything else is parsed
//! as Turtle (a superset of N-Triples). Quad formats (N-Quads / TriG) are
//! flattened into the default graph.

use std::process::ExitCode;

use gstore::model::Triple;
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

/// Read triples from an RDF file, dispatching on the file extension (quad
/// formats flattened to the default graph). See [`gstore::parser::parse_file_by_ext`].
fn read_triples(path: &str) -> Result<Vec<Triple>> {
    gstore::parser::parse_file_by_ext(path)
}
