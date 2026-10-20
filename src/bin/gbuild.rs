//! `gbuild` — build a database from RDF files.
//!
//! Mirrors gStore's `gbuild`:
//!
//! ```text
//! gbuild [--disk | --rocksdb] <db_name> <data.nt> [more.ttl ...]
//! ```
//!
//! Produces a `<db_name>.db` directory holding the dictionary and indexes.
//! `--disk` selects the on-disk B+ tree KVstore; `--rocksdb` (requires building
//! with `--features rocksdb`) selects the persistent RocksDB backend. With every
//! backend the format of each input file is chosen by its extension (`.nt`,
//! `.ttl`, `.nq`, `.trig`, `.rdf`/`.xml`/`.owl`, `.jsonld`/`.json`,
//! `.html`/`.xhtml`; anything else is parsed as Turtle). Building replaces any
//! database previously stored in the directory, whatever its backend.

use std::process::ExitCode;
use std::time::Instant;

use gstore::{db_dir_for, Database};

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    // Optional flags select an alternative storage backend.
    let disk = args.iter().any(|a| a == "--disk");
    let rocksdb = args.iter().any(|a| a == "--rocksdb");
    args.retain(|a| a != "--disk" && a != "--rocksdb");
    if disk && rocksdb {
        eprintln!("gbuild: choose at most one of --disk / --rocksdb");
        return ExitCode::FAILURE;
    }
    if args.len() < 3 {
        eprintln!(
            "usage: {} [--disk | --rocksdb] <db_name> <data.nt> [more.nt ...]",
            args[0]
        );
        return ExitCode::FAILURE;
    }
    let name = &args[1];
    let files = &args[2..];
    let dir = db_dir_for(name);

    let started = Instant::now();
    if rocksdb {
        return build_rocksdb(name, &dir, files, started);
    }
    if disk {
        if let Err(e) = Database::build_disk(&dir, files) {
            eprintln!("disk build failed: {e}");
            return ExitCode::FAILURE;
        }
        // Reopen to report stats.
        match Database::open(&dir) {
            Ok(db) => print_stats(name, &dir, &db, started, "on-disk B+ tree"),
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
        print_stats(name, &dir, &db, started, "in-memory");
    }
    ExitCode::SUCCESS
}

/// Build a RocksDB-backed database (each file parsed by its extension). Gated on
/// the `rocksdb` feature.
#[cfg(feature = "rocksdb")]
fn build_rocksdb(name: &str, dir: &str, files: &[String], started: Instant) -> ExitCode {
    // Close the freshly built store before reopening it (RocksDB takes a lock).
    if let Err(e) = Database::build_rocksdb_from_files(name.to_string(), dir, files) {
        eprintln!("rocksdb build failed: {e}");
        return ExitCode::FAILURE;
    }
    match Database::open(dir) {
        Ok(db) => print_stats(name, dir, &db, started, "RocksDB"),
        Err(e) => {
            eprintln!("built, but failed to reopen: {e}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

#[cfg(not(feature = "rocksdb"))]
fn build_rocksdb(_name: &str, _dir: &str, _files: &[String], _started: Instant) -> ExitCode {
    eprintln!("gbuild: --rocksdb requires building with `--features rocksdb`");
    ExitCode::FAILURE
}

fn print_stats(name: &str, dir: &str, db: &Database, started: Instant, backend: &str) {
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
