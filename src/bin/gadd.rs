//! `gadd` — batch-insert RDF data into an existing database.
//!
//! Mirrors gStore's `gadd` (load triples from a file, add them, persist):
//!
//! ```text
//! gadd [--strict] <db_name> <source> [more sources ...]
//! gadd -db <db_name> (-f <file> | -dir <directory>) [--strict]   # upstream 1.3
//! ```
//!
//! A source is a file, a directory (every RDF file below it, recursively,
//! sorted) or an `http(s)://` URL. The input format is chosen by file
//! extension: `.nt` (N-Triples), `.ttl` (Turtle), `.nq` (N-Quads), `.trig`
//! (TriG), `.rdf`/`.xml`/`.owl` (RDF/XML), `.jsonld`/`.json` (JSON-LD),
//! `.html`/`.xhtml` (RDFa); anything else is parsed as Turtle (a superset of
//! N-Triples). The files stream in; N-Quads / TriG statements go to their
//! named graphs; every file is its own blank-node scope (a `_:b` of this call
//! never merges with one already stored).
//!
//! Like upstream's batch insert, a statement that does not parse is skipped,
//! counted and logged to `<db_name>.db/parse_error.log`; `--strict` stops at
//! the first one instead (what was inserted before it is kept and saved).
//!
//! Any database layout is accepted ([`Database::open`] detects it): for an
//! in-memory snapshot the database is re-saved; for an on-disk (`gbuild --disk`)
//! or RocksDB database the change is written through to its store.
//!
//! Exit status: 0 inserted, 1 failed, 2 invalid arguments; `-h`/`--help`.

#[path = "common/cli.rs"]
mod cli;

use std::process::ExitCode;

use gstore::parser::import::{self, ImportOptions, ParseErrorLog};
use gstore::{db_dir_for, Database};

const TOOL: cli::Tool = cli::Tool {
    name: "gadd",
    usage: "gadd [--strict] <db_name> <file | dir | url> [more ...]\n\
            gadd -db <db_name> (-f <file> | -dir <directory>) [--strict]",
    help: "insert the triples of RDF files into a database\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database name (or give it as the first argument).\n\
           \t-f,--file PATH\t\tThe RDF file (or directory, or http(s) URL) to insert.\n\
           \t-dir,--directory DIR\tA directory of RDF files to insert.\n\
           \t--strict\t\tStop at the first statement that does not parse.",
};

const FILE: cli::Opt = cli::Opt {
    short: "f",
    long: "file",
};
const DIRECTORY: cli::Opt = cli::Opt {
    short: "dir",
    long: "directory",
};

fn main() -> ExitCode {
    let mut args = match cli::parse(&cli::argv(), &[cli::DB, FILE, DIRECTORY]) {
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
    for v in [args.value(2), args.value(1)].into_iter().flatten() {
        paths.insert(0, v.to_string());
    }
    if paths.is_empty() {
        return TOOL.usage_error("nothing to insert (give a file, a directory or a URL)");
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
    };
    let mut log = match ParseErrorLog::in_dir(std::path::Path::new(&dir)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot open the parse error log of '{dir}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut added = 0u64;
    let report = match db.insert_sources(&sources, &opts, &mut log, &mut added) {
        Ok(r) => r,
        Err(e) => {
            // Persist what was inserted so far, then report the failure.
            let saved = match db.save(&dir) {
                Ok(()) => String::new(),
                Err(e) => format!(" (saving failed: {e})"),
            };
            eprintln!("inserted {added} triple(s), then failed: {e}{saved}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = db.save(&dir) {
        eprintln!("inserted {added} triple(s) but saving failed: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "Added {added} new triple(s) to '{dir}' ({} read; {} now total)",
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
