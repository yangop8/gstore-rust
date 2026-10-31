//! `gsub` — batch-delete RDF data from a database.
//!
//! Mirrors gStore's `gsub` (load triples from a file, remove them, persist):
//!
//! ```text
//! gsub [--strict] <db_name> <source> [more sources ...]
//! gsub -db <db_name> -f <file> [--strict]   # upstream 1.3
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
//!
//! Exit status: 0 removed, 1 failed, 2 invalid arguments; `-h`/`--help`.

#[path = "common/cli.rs"]
mod cli;

use std::process::ExitCode;

use gstore::parser::import::{self, ImportOptions, ParseErrorLog};
use gstore::{db_dir_for, Database};

const TOOL: cli::Tool = cli::Tool {
    name: "gsub",
    usage: "gsub [--strict] <db_name> <file | dir | url> [more ...]\n\
            gsub -db <db_name> -f <file> [--strict]",
    help: "remove the triples of RDF files from a database\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database name (or give it as the first argument).\n\
           \t-f,--file PATH\t\tThe RDF file (or directory, or http(s) URL) to remove.\n\
           \t--strict\t\tStop at the first statement that does not parse.",
};

const FILE: cli::Opt = cli::Opt {
    short: "f",
    long: "file",
};

fn main() -> ExitCode {
    let mut args = match cli::parse(&cli::argv(), &[cli::DB, FILE]) {
        Ok(cli::Parsed::Help) => {
            TOOL.print_help();
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Args(a)) => a,
        Err(e) => return TOOL.usage_error(&e),
    };
    let strict = args.take_flag(&["--strict"]);
    if let Some(o) = args.unknown_option() {
        return TOOL.usage_error(&format!("unknown option '{o}'"));
    }
    let (name, mut paths) = match args.value(0) {
        Some(db) => (db.to_string(), args.rest.clone()),
        None => match args.rest.split_first() {
            Some((db, more)) => (db.clone(), more.to_vec()),
            None => return TOOL.usage_error("missing the database name"),
        },
    };
    if let Some(f) = args.value(1) {
        paths.insert(0, f.to_string());
    }
    if paths.is_empty() {
        return TOOL.usage_error("nothing to remove (give a file, a directory or a URL)");
    }
    let dir = db_dir_for(&name);

    let mut db = match Database::open(&dir) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("cannot load database '{dir}': {e}");
            return ExitCode::FAILURE;
        }
    };
    let sources = match import::sources_from_paths(&paths) {
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
