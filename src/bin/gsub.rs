//! `gsub` — batch-delete RDF data from a database.
//!
//! Mirrors gStore's `gsub` (load triples from a file, remove them, persist):
//!
//! ```text
//! gsub <db_name> <rdf_file>
//! ```
//!
//! The input format is chosen by file extension, exactly as `gadd`: `.nt`,
//! `.ttl`, `.nq`, `.trig`, `.rdf`/`.xml`/`.owl`, `.jsonld`/`.json`,
//! `.html`/`.xhtml`; anything else is parsed as Turtle. Quad formats are
//! flattened to the default graph.
//!
//! Any database layout is accepted ([`Database::open`] detects it): for an
//! in-memory snapshot the database is re-saved; for an on-disk (`gbuild --disk`)
//! or RocksDB database the change is written through to its store.

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

    let mut db = match Database::open(&dir) {
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

    let removed = triples.iter().filter(|t| db.remove_triple(t)).count();

    if let Err(e) = db.save(&dir) {
        eprintln!("removed {removed} triple(s) but saving failed: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "Removed {removed} triple(s) from '{dir}' ({} read; {} now total)",
        triples.len(),
        db.triple_num()
    );
    ExitCode::SUCCESS
}

/// Read triples from an RDF file, dispatching on the file extension (quad
/// formats flattened to the default graph). See [`gstore::parser::parse_file_by_ext`].
fn read_triples(path: &str) -> Result<Vec<Triple>> {
    gstore::parser::parse_file_by_ext(path)
}
