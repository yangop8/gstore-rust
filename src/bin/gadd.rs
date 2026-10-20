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

    let mut added = 0usize;
    for t in &triples {
        match db.try_insert_triple(t) {
            Ok(true) => added += 1,
            Ok(false) => {}
            Err(e) => {
                // Persist what was inserted so far, then report the failure.
                let saved = match db.save(&dir) {
                    Ok(()) => String::new(),
                    Err(e) => format!(" (saving failed: {e})"),
                };
                eprintln!("inserted {added} triple(s), then failed: {e}{saved}");
                return ExitCode::FAILURE;
            }
        }
    }

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

/// Read triples from an RDF file, dispatching on the file extension (quad
/// formats flattened to the default graph). See [`gstore::parser::parse_file_by_ext`].
fn read_triples(path: &str) -> Result<Vec<Triple>> {
    gstore::parser::parse_file_by_ext(path)
}
