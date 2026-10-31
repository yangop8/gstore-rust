//! `gbuild` — build a database from RDF files.
//!
//! Mirrors gStore's `gbuild`:
//!
//! ```text
//! gbuild [--disk | --rocksdb] [--strict] [--force] <db_name> [<source> ...]
//! gbuild -db <db_name> [-f <source>] [--disk | --rocksdb] [--strict] [--force]
//! ```
//!
//! (the second form is upstream 1.3's: `-db`/`--database`, `-f`/`--file`;
//! further positional sources may follow it).
//!
//! Produces a `<db_name>.db` directory holding the dictionary and indexes.
//! `--disk` selects the on-disk B+ tree KVstore; `--rocksdb` (requires building
//! with `--features rocksdb`) selects the persistent RocksDB backend. Without
//! a source an empty database is built (upstream: "will build an empty
//! database"). Like upstream, `gbuild` refuses to build over an existing
//! database directory (exit status 1; remove it with `gdrop` first) unless
//! `--force` is given — then the build replaces the database previously
//! stored there, whatever its backend.
//!
//! A source is a file, a directory (every RDF file below it, recursively,
//! sorted by path; hidden entries skipped) or an `http://` / `https://` URL
//! (`https` needs the `tls` feature). With every backend the format of each
//! file is chosen by its extension (`.nt`, `.ttl`, `.nq`, `.trig`,
//! `.rdf`/`.xml`/`.owl`, `.jsonld`/`.json`, `.html`/`.xhtml`; anything else is
//! parsed as Turtle; `x.nt.gz` with the `gzip` feature). The files stream in
//! with bounded memory; N-Quads / TriG statements keep their named graph;
//! each file is its own blank-node scope.
//!
//! Like upstream, a statement that does not parse is skipped, counted and
//! logged to `<db_name>.db/parse_error.log` (see
//! [`gstore::parser::import`]), and the build goes on. `--strict` stops at the
//! first such statement instead (exit status 1, nothing built).
//!
//! Exit status: 0 built, 1 failed, 2 invalid arguments; `-h`/`--help`.

#[path = "common/cli.rs"]
mod cli;

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use gstore::parser::import::{self, ImportOptions, ImportReport, ParseErrorLog, Source};
use gstore::{db_dir_for, Database};

const TOOL: cli::Tool = cli::Tool {
    name: "gbuild",
    usage: "gbuild [--disk | --rocksdb] [--strict] [--force] <db_name> [<file | dir | url> ...]\n\
            gbuild -db <db_name> [-f <file | dir | url>] [--disk | --rocksdb] [--strict] [--force]",
    help: "build a database from RDF files\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database name (or give it as the first argument).\n\
           \t-f,--file PATH\t\tAn RDF file, a directory of RDF files or an http(s) URL.\n\
           \t\t\t\tWithout any source an empty database is built.\n\
           \t--disk\t\t\tStore the database in the on-disk B+ tree KVstore.\n\
           \t--rocksdb\t\tStore the database in RocksDB (feature `rocksdb`).\n\
           \t--strict\t\tStop at the first statement that does not parse.\n\
           \t--force\t\t\tReplace an existing database (else gbuild refuses).",
};

/// `-f` / `--file`.
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
    // Optional flags select an alternative storage backend / strict parsing.
    let disk = args.take_flag(&["--disk"]);
    let rocksdb = args.take_flag(&["--rocksdb"]);
    let strict = args.take_flag(&["--strict"]);
    let force = args.take_flag(&["--force"]);
    if disk && rocksdb {
        return TOOL.usage_error("choose at most one of --disk / --rocksdb");
    }
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
    if name.trim().is_empty() {
        return TOOL.usage_error("the database name is empty");
    }
    let name = &name;
    let dir = db_dir_for(name);
    if !force && holds_data(Path::new(&dir)) {
        eprintln!(
            "gbuild: the database path '{dir}' already exists. Please check database {name}, \
             or use gdrop to remove it at first (or rebuild it with --force)."
        );
        return ExitCode::FAILURE;
    }
    if paths.is_empty() {
        println!("will build an empty database");
    }
    let sources = match import::sources_from_paths(&paths) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("gbuild: cannot read the sources: {e}");
            return ExitCode::FAILURE;
        }
    };
    let opts = if strict {
        ImportOptions::strict()
    } else {
        ImportOptions::tolerant()
    };

    let started = Instant::now();
    // A fresh build starts a fresh parse error log.
    let log_path = Path::new(&dir).join(import::PARSE_ERROR_LOG);
    let _ = std::fs::remove_file(&log_path);
    let mut log = match ParseErrorLog::open(&log_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("gbuild: cannot create {}: {e}", log_path.display());
            return ExitCode::FAILURE;
        }
    };
    if rocksdb {
        return build_rocksdb(name, &dir, &sources, &opts, &mut log, started);
    }
    let report = if disk {
        let report = match Database::build_disk_from_sources(&dir, &sources, &opts, &mut log) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("disk build failed: {e}");
                return ExitCode::FAILURE;
            }
        };
        // Reopen to report stats.
        match Database::open(&dir) {
            Ok(db) => print_stats(name, &dir, &db, started, "on-disk B+ tree"),
            Err(e) => {
                eprintln!("built, but failed to reopen: {e}");
                return ExitCode::FAILURE;
            }
        }
        report
    } else {
        let (db, report) = match Database::build_from_sources(name.clone(), &sources, &opts, &mut log)
        {
            Ok(b) => b,
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
        report
    };
    print_report(&report, &log_path);
    ExitCode::SUCCESS
}

/// Build a RocksDB-backed database (each file parsed by its extension). Gated on
/// the `rocksdb` feature.
#[cfg(feature = "rocksdb")]
fn build_rocksdb(
    name: &str,
    dir: &str,
    sources: &[Source],
    opts: &ImportOptions,
    log: &mut ParseErrorLog,
    started: Instant,
) -> ExitCode {
    // Close the freshly built store before reopening it (RocksDB takes a lock).
    let report = match Database::build_rocksdb_from_sources(name.to_string(), dir, sources, opts, log)
    {
        Ok((db, report)) => {
            drop(db);
            report
        }
        Err(e) => {
            eprintln!("rocksdb build failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    match Database::open(dir) {
        Ok(db) => print_stats(name, dir, &db, started, "RocksDB"),
        Err(e) => {
            eprintln!("built, but failed to reopen: {e}");
            return ExitCode::FAILURE;
        }
    }
    print_report(&report, log.path());
    ExitCode::SUCCESS
}

#[cfg(not(feature = "rocksdb"))]
fn build_rocksdb(
    _name: &str,
    _dir: &str,
    _sources: &[Source],
    _opts: &ImportOptions,
    _log: &mut ParseErrorLog,
    _started: Instant,
) -> ExitCode {
    eprintln!("gbuild: --rocksdb requires building with `--features rocksdb`");
    ExitCode::FAILURE
}

/// Whether `path` exists as anything but an empty directory.
fn holds_data(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => path.exists(),
    }
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
    let quads: u64 = db.named_graphs().values().map(|g| g.triple_count()).sum();
    if quads > 0 {
        println!(
            "  named graphs={}  named-graph quads={quads}",
            db.named_graphs().len()
        );
    }
    println!("  took {:.3}s", started.elapsed().as_secs_f64());
}

fn print_report(report: &ImportReport, log: &Path) {
    println!(
        "  read {} statement(s) from {} source(s)",
        report.statements, report.documents
    );
    if report.failed > 0 {
        println!(
            "  RDF parse error num {}; see {}",
            report.failed,
            log.display()
        );
    }
}
