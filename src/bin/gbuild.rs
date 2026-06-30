//! `gbuild` — build a database from RDF (N-Triples) files.
//!
//! Mirrors gStore's `gbuild`:
//!
//! ```text
//! gbuild <db_name> <data.nt> [more.nt ...]
//! ```
//!
//! Produces a `<db_name>.db` directory holding the dictionary and indexes.

use std::process::ExitCode;
use std::time::Instant;

use gstore::{db_dir_for, Database};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: {} <db_name> <data.nt> [more.nt ...]", args[0]);
        return ExitCode::FAILURE;
    }
    let name = &args[1];
    let files = &args[2..];

    let started = Instant::now();
    let db = match Database::build_from_files(name.clone(), files) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("build failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let dir = db_dir_for(name);
    if let Err(e) = db.save(&dir) {
        eprintln!("failed to save database to {dir}: {e}");
        return ExitCode::FAILURE;
    }

    let elapsed = started.elapsed();
    println!("Built database '{name}' in {dir}");
    println!(
        "  triples={}  entities={}  literals={}  predicates={}",
        db.triple_num(),
        db.entity_num(),
        db.literal_num(),
        db.predicate_num()
    );
    println!("  took {:.3}s", elapsed.as_secs_f64());
    ExitCode::SUCCESS
}
