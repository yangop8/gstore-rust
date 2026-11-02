//! `gbuild` — build a database from RDF files.
//!
//! Mirrors gStore's `gbuild`:
//!
//! ```text
//! gbuild [--disk | --rocksdb] [--strict] [--force] <db_name> [<source> ...]
//! gbuild -db <db_name> [-f <source>] [--disk | --rocksdb] [--strict] [--force]
//! gbuild --disk [--memory MB] [--threads N] [--tmp-dir DIR] [--fill PCT] [--incremental] …
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
//! stored there, whatever its backend, and its reasoning rules go with it
//! (as when it is dropped and built).
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
//! A local `.zip` source (upstream 1.3's `gbuild -f x.zip`; feature `zip`)
//! is unpacked — refusing unsafe entry names, under the default zip limits
//! (10 GiB, 10000 entries; `--unzip-max-size BYTES` / `--unzip-max-entries
//! N` change them, `0` = no limit) — into a scratch directory beside the
//! database (`<db_name>.db.unzip-tmp-N`, removed when `gbuild` ends) and
//! read like a directory source.
//!
//! `--disk` builds the indexes **in bulk** ([`gstore::bulk`]): the input is
//! encoded, sorted (spilling sorted runs to disk beyond the memory budget)
//! and the B+ trees, dictionary arrays and VS-tree are written bottom-up,
//! then installed crash-safely over whatever the directory held. Its knobs:
//! `--memory MB` (the sort budget, default `build_memory_mb` of
//! `conf/conf.ini`, else 1024; the dictionary under construction comes on
//! top), `--threads N` (`build_threads`, `0` = every core;
//! `build_multi_thread=off` means 1), `--tmp-dir DIR` (`build_tmp_dir`,
//! where runs spill; default inside the database directory) and `--fill
//! PCT` (`build_fill_factor`, 50–100, default 90). Instead of reopening the
//! database it prints the counts and a phase / memory / spill summary.
//! `--incremental` selects the previous path (one B+ tree insert per index
//! and term), for comparisons. The `[system]` keys are read from
//! `./conf/conf.ini` when it exists.
//!
//! Like upstream, a statement that does not parse is skipped, counted and
//! logged to `<db_name>.db/parse_error.log` (see
//! [`gstore::parser::import`]), and the build goes on. `--strict` stops at the
//! first such statement instead (exit status 1, nothing built).
//!
//! A build that fails removes the database directory it created (upstream's
//! `Util::remove_path` of a failed build), so the same `gbuild` can simply be
//! run again; a failed `--force` rebuild leaves the directory as it was. A
//! bulk build whose install put the new database in place but could not
//! finish cleaning up (an old layout, the staging area) says so — "built,
//! but …" — and exits with status 1; the new database is the one served.
//!
//! Exit status: 0 built, 1 failed, 2 invalid arguments; `-h`/`--help`.

#[path = "common/cli.rs"]
mod cli;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use gstore::bulk::{BulkInput, BulkOptions, BulkReport, BulkTarget};
use gstore::config::GStoreConfig;
use gstore::parser::import::{self, ImportOptions, ImportReport, ParseErrorLog, Source};
use gstore::zipio::ZipLimits;
use gstore::{db_dir_for, Database};

const TOOL: cli::Tool = cli::Tool {
    name: "gbuild",
    usage: "gbuild [--disk | --rocksdb] [--strict] [--force] <db_name> [<file | dir | zip | url> ...]\n\
            gbuild -db <db_name> [-f <file | dir | zip | url>] [--disk | --rocksdb] [--strict] [--force]\n\
            \t[--unzip-max-size BYTES] [--unzip-max-entries N]\n\
            gbuild --disk [--memory MB] [--threads N] [--tmp-dir DIR] [--fill PCT] [--incremental] ...",
    help: "build a database from RDF files\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database name (or give it as the first argument).\n\
           \t-f,--file PATH\t\tAn RDF file, a directory of RDF files, a zip package of\n\
           \t\t\t\tRDF files (feature `zip`) or an http(s) URL.\n\
           \t\t\t\tWithout any source an empty database is built.\n\
           \t--disk\t\t\tStore the database in the on-disk B+ tree KVstore (built in\n\
           \t\t\t\tbulk: sorted, bottom-up, installed crash-safely).\n\
           \t--memory MB\t\tSort memory of the --disk build (default: conf.ini\n\
           \t\t\t\tbuild_memory_mb, else 1024); the dictionary comes on top.\n\
           \t--threads N\t\tThreads of the --disk build (default: conf.ini\n\
           \t\t\t\tbuild_threads; 0 = every core).\n\
           \t--tmp-dir DIR\t\tWhere the --disk build spills sorted runs (default:\n\
           \t\t\t\tinside the database directory).\n\
           \t--fill PCT\t\tB+ tree node fill of the --disk build, 50-100 (default 90).\n\
           \t--incremental\t\tBuild --disk the previous way, one insert at a time.\n\
           \t--rocksdb\t\tStore the database in RocksDB (feature `rocksdb`).\n\
           \t--strict\t\tStop at the first statement that does not parse.\n\
           \t--force\t\t\tReplace an existing database (else gbuild refuses).\n\
           \t--unzip-max-size BYTES\tThe most a zip package may unpack to (default\n\
           \t\t\t\t10737418240; 0 = no limit).\n\
           \t--unzip-max-entries N\tThe most entries a zip package may hold (default\n\
           \t\t\t\t10000; 0 = no limit).",
};

/// `-f` / `--file`.
const FILE: cli::Opt = cli::Opt {
    short: "f",
    long: "file",
};
/// `--memory MB`: the bulk build's memory budget.
const MEMORY: cli::Opt = cli::Opt {
    short: "",
    long: "memory",
};
/// `--threads N`: the bulk build's threads.
const THREADS: cli::Opt = cli::Opt {
    short: "",
    long: "threads",
};
/// `--tmp-dir DIR`: where the bulk build spills.
const TMP_DIR: cli::Opt = cli::Opt {
    short: "",
    long: "tmp-dir",
};
/// `--fill PCT`: the bulk build's B+ tree fill factor.
const FILL: cli::Opt = cli::Opt {
    short: "",
    long: "fill",
};

/// The `[system]` configuration: `./conf/conf.ini` when it exists.
fn load_config() -> GStoreConfig {
    let path = Path::new("conf/conf.ini");
    if path.is_file() {
        if let Ok(c) = GStoreConfig::load(path) {
            return c;
        }
    }
    GStoreConfig::default()
}

/// The bulk options: `conf.ini`'s `build_*` keys, then the command line
/// (options 4–7 of `args`).
fn bulk_options(args: &cli::Args) -> Result<BulkOptions, String> {
    let mut bulk = load_config().bulk_options();
    if let Some(v) = args.value(4) {
        let mb: u64 = v
            .parse()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| format!("--memory takes a number of MiB, not '{v}'"))?;
        bulk.memory_budget = usize::try_from(mb.saturating_mul(1 << 20)).unwrap_or(usize::MAX);
    }
    if let Some(v) = args.value(5) {
        bulk.threads = v
            .parse()
            .map_err(|_| format!("--threads takes a number of threads, not '{v}'"))?;
    }
    if let Some(v) = args.value(6) {
        bulk.temp_dir = Some(PathBuf::from(v));
    }
    if let Some(v) = args.value(7) {
        bulk.fill_percent = v
            .trim_end_matches('%')
            .parse()
            .ok()
            .filter(|n| (50..=100).contains(n))
            .ok_or_else(|| format!("--fill takes a percentage from 50 to 100, not '{v}'"))?;
    }
    Ok(bulk)
}

fn main() -> ExitCode {
    let mut args = match cli::parse(
        &cli::argv(),
        &[
            cli::DB,
            FILE,
            cli::UNZIP_MAX_SIZE,
            cli::UNZIP_MAX_ENTRIES,
            MEMORY,
            THREADS,
            TMP_DIR,
            FILL,
        ],
    ) {
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
    let incremental = args.take_flag(&["--incremental"]);
    if disk && rocksdb {
        return TOOL.usage_error("choose at most one of --disk / --rocksdb");
    }
    let bulk_flags = (4..8).any(|i| args.value(i).is_some()) || incremental;
    if bulk_flags && !disk {
        return TOOL.usage_error(
            "--memory, --threads, --tmp-dir, --fill and --incremental apply to --disk builds",
        );
    }
    let bulk = match bulk_options(&args) {
        Ok(b) => b,
        Err(e) => return TOOL.usage_error(&e),
    };
    if let Some(o) = args.unknown_option() {
        return TOOL.usage_error(&format!("unknown option '{o}'"));
    }
    let limits = match cli::zip_limits(&args, 2, 3, ZipLimits::default()) {
        Ok(l) => l,
        Err(e) => return TOOL.usage_error(&e),
    };
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
    let existed = holds_data(Path::new(&dir));
    if !force && existed {
        eprintln!(
            "gbuild: the database path '{dir}' already exists. Please check database {name}, \
             or use gdrop to remove it at first (or rebuild it with --force)."
        );
        return ExitCode::FAILURE;
    }
    // A failed build removes what it created; one over an existing
    // database (`--force`) leaves the directory as it is.
    let mut created = CreatedDir {
        path: (!existed).then(|| PathBuf::from(&dir)),
    };
    if paths.is_empty() {
        gstore::outln!("will build an empty database");
    }
    // Zip packages are unpacked first and read as directories; the scratch
    // directories go when `_unpacked` is dropped, whatever the outcome.
    let _unpacked = match cli::unpack_packages(&dir, &mut paths, &limits) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("gbuild: {e}");
            return ExitCode::FAILURE;
        }
    };
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
        let code = build_rocksdb(name, &dir, &sources, &opts, &mut log, started);
        if code == ExitCode::SUCCESS {
            created.keep();
            if existed {
                return drop_old_rules(&dir);
            }
        }
        return code;
    }
    let mut bulk_installed = false;
    let mut cleanup_incomplete = false;
    let report = if disk && !incremental {
        let built = gstore::bulk::build(
            Path::new(&dir),
            name,
            BulkInput::Sources(&sources),
            &opts,
            &mut log,
            BulkTarget::Disk,
            &bulk,
        );
        match built {
            Ok(r) => {
                print_bulk(name, &dir, &r, started);
                bulk_installed = true;
                for w in &r.warnings {
                    eprintln!(
                        "gbuild: built (the new database is in place), but its install \
                         did not finish: {w}"
                    );
                    cleanup_incomplete = true;
                }
                r.import
            }
            Err(e) => {
                eprintln!("disk build failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else if disk {
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
        let (db, report) =
            match Database::build_from_sources(name.clone(), &sources, &opts, &mut log) {
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
    created.keep();
    if cleanup_incomplete {
        return ExitCode::FAILURE;
    }
    // The bulk install removed the replaced database's rules itself, right
    // after its commit point.
    if existed && !bulk_installed {
        return drop_old_rules(&dir);
    }
    ExitCode::SUCCESS
}

/// After a `--force` rebuild: remove the replaced database's reasoning rules
/// (`reason_rule_files/`), as dropping it would — executed, their ledgers
/// would name triples the new data holds as asserted ones.
fn drop_old_rules(dir: &str) -> ExitCode {
    let rules = Path::new(dir).join(gstore::reason::rules::RULE_DIR);
    match std::fs::remove_dir_all(&rules) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "gbuild: built, but the replaced database's reasoning rules could not be \
                 removed ({}): {e}",
                rules.display()
            );
            ExitCode::FAILURE
        }
    }
}

/// The database directory a build created, removed when dropped unless the
/// build succeeded ([`CreatedDir::keep`]).
struct CreatedDir {
    path: Option<PathBuf>,
}

impl CreatedDir {
    fn keep(&mut self) {
        self.path = None;
    }
}

impl Drop for CreatedDir {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            let _ = std::fs::remove_dir_all(p);
        }
    }
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
    let report =
        match Database::build_rocksdb_from_sources(name.to_string(), dir, sources, opts, log) {
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
    gstore::outln!("Built database '{name}' in {dir} ({backend})");
    gstore::outln!(
        "  triples={}  entities={}  literals={}  predicates={}",
        db.triple_num(),
        db.entity_num(),
        db.literal_num(),
        db.predicate_num()
    );
    let quads: u64 = db.named_graphs().values().map(|g| g.triple_count()).sum();
    if quads > 0 {
        gstore::outln!(
            "  named graphs={}  named-graph quads={quads}",
            db.named_graphs().len()
        );
    }
    gstore::outln!("  took {:.3}s", started.elapsed().as_secs_f64());
}

fn print_bulk(name: &str, dir: &str, r: &BulkReport, started: Instant) {
    gstore::outln!("Built database '{name}' in {dir} (on-disk B+ tree, bulk build)");
    gstore::outln!("{}", r.summary());
    gstore::outln!("  took {:.3}s", started.elapsed().as_secs_f64());
}

fn print_report(report: &ImportReport, log: &Path) {
    gstore::outln!(
        "  read {} statement(s) from {} source(s)",
        report.statements,
        report.documents
    );
    if report.failed > 0 {
        gstore::outln!(
            "  RDF parse error num {}; see {}",
            report.failed,
            log.display()
        );
    }
}
