//! `gsub` — batch-delete RDF data from a database.
//!
//! Mirrors gStore's `gsub` (load triples from a file, remove them, persist):
//!
//! ```text
//! gsub [--strict] <db_name> <source> [more sources ...]
//! ```
//!
//! Sources and formats exactly as `gadd` (files by extension, directories,
//! `http(s)://` URLs); N-Quads / TriG statements are removed from their named
//! graphs. Blank-node labels are read verbatim, so a file naming stored blank
//! nodes (e.g. one written by `gexport`) removes them; anonymous `[]` nodes
//! match nothing.
//!
//! A statement that does not parse is skipped, counted and logged to
//! `<db_name>.db/parse_error.log`; `--strict` stops at the first one instead
//! (what was removed before it stays removed and is saved).
//!
//! Any database layout is accepted ([`Database::open`] detects it): for an
//! in-memory snapshot the database is re-saved; for an on-disk (`gbuild --disk`)
//! or RocksDB database the change is written through to its store.

use std::process::ExitCode;

use gstore::parser::import::{self, ImportOptions, ParseErrorLog};
use gstore::{db_dir_for, Database};

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().collect();
    let strict = args.iter().any(|a| a == "--strict");
    args.retain(|a| a != "--strict");
    if args.len() < 3 {
        eprintln!(
            "usage: {} [--strict] <db_name> <file | dir | url> [more ...]",
            args[0]
        );
        return ExitCode::FAILURE;
    }
    let dir = db_dir_for(&args[1]);

    let mut db = match Database::open(&dir) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot load database '{dir}': {e}");
            return ExitCode::FAILURE;
        }
    };
    let sources = match import::sources_from_paths(&args[2..]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read the sources: {e}");
            return ExitCode::FAILURE;
        }
    };
    let opts = if strict {
        ImportOptions::strict()
    } else {
        ImportOptions::tolerant()
    }
    .for_removal();
    let mut log = match ParseErrorLog::in_dir(std::path::Path::new(&dir)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot open the parse error log of '{dir}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut removed = 0u64;
    let result = db.remove_sources(&sources, &opts, &mut log, &mut removed);
    if let Err(e) = db.save(&dir) {
        eprintln!("removed {removed} triple(s) but saving failed: {e}");
        return ExitCode::FAILURE;
    }
    let report = match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("removed {removed} triple(s), then failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "Removed {removed} triple(s) from '{dir}' ({} read; {} now total)",
        report.statements,
        db.triple_num()
    );
    if report.failed > 0 {
        println!(
            "Skipped {} statement(s) that could not be read; see {}",
            report.failed,
            log.path().display()
        );
    }
    ExitCode::SUCCESS
}
