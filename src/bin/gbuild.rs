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
    let mut args: Vec<String> = std::env::args().collect();
    // Optional `--disk` flag selects the on-disk B+ tree KVstore backend.
    let disk = args.iter().any(|a| a == "--disk");
    args.retain(|a| a != "--disk");
    if args.len() < 3 {
        eprintln!(
            "usage: {} [--disk] <db_name> <data.nt> [more.nt ...]",
            args[0]
        );
        return ExitCode::FAILURE;
    }
    let name = &args[1];
    let files = &args[2..];
    let dir = db_dir_for(name);

    let started = Instant::now();
    if disk {
        if let Err(e) = Database::build_disk(&dir, files) {
            eprintln!("disk build failed: {e}");
            return ExitCode::FAILURE;
        }
        // Reopen to report stats.
        match Database::load_disk(&dir) {
            Ok(db) => print_stats(name, &dir, &db, started, true),
            Err(e) => {
                eprintln!("built, but failed to reopen: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        let db = match Database::build_from_files(name.clone(), files) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("build failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = db.save(&dir) {
            eprintln!("failed to save database to {dir}: {e}");
            return ExitCode::FAILURE;
        }
        print_stats(name, &dir, &db, started, false);
    }
    ExitCode::SUCCESS
}

fn print_stats(name: &str, dir: &str, db: &Database, started: Instant, disk: bool) {
    let backend = if disk { "on-disk B+ tree" } else { "in-memory" };
    println!("Built database '{name}' in {dir} ({backend})");
    println!(
        "  triples={}  entities={}  literals={}  predicates={}",
        db.triple_num(),
        db.entity_num(),
        db.literal_num(),
        db.predicate_num()
    );
    println!("  took {:.3}s", started.elapsed().as_secs_f64());
}
