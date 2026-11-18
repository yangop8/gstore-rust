//! `gbuild` — build a database from RDF files.
//!
//! Mirrors gStore's `gbuild`:
//!
//! ```text
//! gbuild [--layout snapshot|disk|rocksdb | --disk | --rocksdb] [--strict] [--force] <db_name> [<source> ...]
//! gbuild -db <db_name> [-f <source>] [--layout L | --disk | --rocksdb] [--strict] [--force]
//! gbuild --disk [--memory MB] [--threads N] [--tmp-dir DIR] [--fill PCT] [--incremental] …
//! gbuild --rocksdb [--memory MB] [--threads N] [--tmp-dir DIR] [--incremental] …
//! ```
//!
//! (the second form is upstream 1.3's: `-db`/`--database`, `-f`/`--file`;
//! further positional sources may follow it).
//!
//! Produces a `<db_name>.db` directory holding the dictionary and indexes.
//! `--layout` chooses its layout: `snapshot` (in memory, saved as a bincode
//! snapshot), `disk` (the on-disk B+ tree KVstore; also `--disk`) or
//! `rocksdb` (the persistent RocksDB backend, which requires building with
//! `--features rocksdb`; also `--rocksdb`). Without any of them the layout is
//! `[storage] default_layout` of `./conf/conf.ini` when it is set, else
//! `snapshot` — the layout the servers' builds (REST `/build`, `/api build`)
//! default to as well. Without
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
//! N` change them, `0` = no limit) — into a new scratch directory beside the
//! database (`<db_name>.db.unzip-tmp-<pid>-<random>`, created exclusively
//! once the database directory is claimed, removed when `gbuild` ends) and
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
//! `./conf/conf.ini` when it exists (its warnings — unknown keys, values
//! that do not parse, settings that cannot take effect — are printed on
//! standard error, as `gserver` logs them) — `id_reclaim` among them: with
//! `id_reclaim=off` a new disk database is created version 1 (no free-id
//! list), which binaries older than Phase 6 can open; `GSTORE_ID_RECLAIM`
//! overrides it, as for the server.
//!
//! `--rocksdb` builds in bulk too: the same sorted streams are written as
//! SST files, one set per column family, ingested into a fresh store and
//! installed crash-safely (the RocksDB layout `"1"`, every key and value as
//! the one-write-per-triple path stores them). `--memory`, `--threads` and
//! `--tmp-dir` apply as for `--disk` (`--fill` does not), and
//! `--incremental` selects the previous path (one write batch per triple,
//! then a reopen to print the counts).
//!
//! Like upstream, a statement that does not parse is skipped, counted and
//! logged to `<db_name>.db/parse_error.log` (see
//! [`gstore::parser::import`]), and the build goes on. `--strict` stops at the
//! first such statement instead (exit status 1, nothing built).
//!
//! `gbuild` claims the database directory (its inter-process lock) before
//! it writes anything into or beside it — the parse error log, a zip
//! package's scratch directory — and keeps the claim until it ends: another
//! process building, loading or dropping the same database meanwhile is
//! refused, and `gbuild` fails at once (status 1) when another process holds
//! the directory. Without `--force` a directory that was new when `gbuild`
//! started is checked again once claimed: a database another process
//! created and finished in between is refused like one that was there
//! before, and left alone. A build that fails removes the database directory it
//! created (upstream's `Util::remove_path` of a failed build) — under that
//! claim, and only while it holds no database — so the same `gbuild` can
//! simply be run again; a failed `--force` rebuild leaves the directory as
//! it was. A build that succeeded keeps its database even when its summary
//! cannot be printed (a full disk behind standard output: status 1). A
//! bulk build whose install put the new database in place but could not
//! finish cleaning up (an old layout, the staging area) says so — "built,
//! but …" — and exits with status 1; the new database is the one served.
//!
//! Exit status: 0 built, 1 failed, 2 invalid arguments, 3 built but
//! statements were skipped (tolerant mode: the database holds every other
//! statement, and `parse_error.log` names the skipped ones — so a script
//! running `gexport` then `gbuild` notices a statement that did not load
//! back); `-h`/`--help`.

#[path = "common/cli.rs"]
mod cli;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use gstore::bulk::{BulkInput, BulkOptions, BulkReport, BulkTarget};
use gstore::config::GStoreConfig;
use gstore::db::dirlock::BuildGuard;
use gstore::parser::import::{self, ImportOptions, ImportReport, ParseErrorLog, Source};
use gstore::zipio::ZipLimits;
use gstore::{db_dir_for, Database};

const TOOL: cli::Tool = cli::Tool {
    name: "gbuild",
    usage: "gbuild [--layout snapshot|disk|rocksdb | --disk | --rocksdb] [--strict] [--force] <db_name> [<file | dir | zip | url> ...]\n\
            gbuild -db <db_name> [-f <file | dir | zip | url>] [--layout L | --disk | --rocksdb] [--strict] [--force]\n\
            \t[--unzip-max-size BYTES] [--unzip-max-entries N]\n\
            gbuild --disk [--memory MB] [--threads N] [--tmp-dir DIR] [--fill PCT] [--incremental] ...\n\
            gbuild --rocksdb [--memory MB] [--threads N] [--tmp-dir DIR] [--incremental] ...",
    help: "build a database from RDF files\n\
           \n\
           Options:\n\
           \t-h,--help\t\tDisplay this message.\n\
           \t-db,--database NAME\tThe database name (or give it as the first argument).\n\
           \t-f,--file PATH\t\tAn RDF file, a directory of RDF files, a zip package of\n\
           \t\t\t\tRDF files (feature `zip`) or an http(s) URL.\n\
           \t\t\t\tWithout any source an empty database is built.\n\
           \t--layout L\t\tThe layout: snapshot, disk or rocksdb (default: conf.ini\n\
           \t\t\t\t[storage] default_layout, else snapshot).\n\
           \t--disk\t\t\tStore the database in the on-disk B+ tree KVstore (built in\n\
           \t\t\t\tbulk: sorted, bottom-up, installed crash-safely).\n\
           \t--memory MB\t\tSort memory of a --disk / --rocksdb build (default:\n\
           \t\t\t\tconf.ini build_memory_mb, else 1024); the dictionary comes\n\
           \t\t\t\ton top.\n\
           \t--threads N\t\tThreads of a --disk / --rocksdb build (default: conf.ini\n\
           \t\t\t\tbuild_threads; 0 = every core).\n\
           \t--tmp-dir DIR\t\tWhere a --disk / --rocksdb build spills sorted runs\n\
           \t\t\t\t(default: inside the database directory).\n\
           \t--fill PCT\t\tB+ tree node fill of the --disk build, 50-100 (default 90).\n\
           \t--incremental\t\tBuild --disk / --rocksdb the previous way, one insert at\n\
           \t\t\t\ta time.\n\
           \t--rocksdb\t\tStore the database in RocksDB (feature `rocksdb`; built in\n\
           \t\t\t\tbulk: SST files ingested, installed crash-safely).\n\
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
/// `--layout snapshot|disk|rocksdb`: the database's layout.
const LAYOUT: cli::Opt = cli::Opt {
    short: "",
    long: "layout",
};

/// The layout `gbuild` builds in: `--layout`, else `--disk` / `--rocksdb`,
/// else `conf.ini`'s `[storage] default_layout`, else the snapshot layout.
fn resolve_layout(
    cfg: &GStoreConfig,
    layout: Option<&str>,
    disk: bool,
    rocksdb: bool,
) -> Result<gstore::StorageKind, String> {
    use gstore::StorageKind;
    if disk && rocksdb {
        return Err("choose at most one of --disk / --rocksdb".into());
    }
    let flag = if disk {
        Some(StorageKind::Disk)
    } else if rocksdb {
        Some(StorageKind::RocksDb)
    } else {
        None
    };
    let Some(v) = layout else {
        return Ok(flag.or(cfg.default_layout).unwrap_or(StorageKind::Snapshot));
    };
    let chosen = match v.to_ascii_lowercase().as_str() {
        "snapshot" | "memory" => StorageKind::Snapshot,
        "disk" | "kvstore" => StorageKind::Disk,
        "rocksdb" | "rocks" => StorageKind::RocksDb,
        _ => {
            return Err(format!(
                "--layout takes snapshot, disk or rocksdb, not '{v}'"
            ))
        }
    };
    match flag {
        Some(f) if f != chosen => Err(format!("--layout {v} contradicts --disk / --rocksdb")),
        _ => Ok(chosen),
    }
}

/// The configuration: `./conf/conf.ini` when it exists, else the built-in
/// defaults. Its warnings — unknown keys, values that do not parse (the
/// default is kept), settings that cannot take effect — go to standard
/// error, as `gserver` logs them; a file that cannot be read is reported and
/// the defaults are used, as `gserver` does (review bulkcli#7: both were
/// dropped silently).
fn load_config() -> GStoreConfig {
    let path = Path::new("conf/conf.ini");
    if !path.is_file() {
        return GStoreConfig::default();
    }
    match GStoreConfig::load(path) {
        Ok(c) => {
            for w in c.warnings() {
                eprintln!("gbuild: conf.ini warning: {w}");
            }
            c
        }
        Err(e) => {
            eprintln!(
                "gbuild: conf.ini warning: cannot read '{}': {e}; using the built-in defaults",
                path.display()
            );
            GStoreConfig::default()
        }
    }
}

/// The bulk options: `conf.ini`'s `build_*` keys, then the command line
/// (options 4–7 of `args`).
fn bulk_options(cfg: &GStoreConfig, args: &cli::Args) -> Result<BulkOptions, String> {
    let mut bulk = cfg.bulk_options();
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
            LAYOUT,
        ],
    ) {
        Ok(cli::Parsed::Help) => {
            TOOL.print_help();
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Args(a)) => a,
        Err(e) => return TOOL.usage_error(&e),
    };
    let cfg = load_config();
    // `[system] id_reclaim` (which file version a new disk database gets)
    // and the RocksDB tuning keys, as the server installs them.
    cfg.install_storage_defaults();
    // Optional flags select an alternative storage backend / strict parsing.
    let disk = args.take_flag(&["--disk"]);
    let rocksdb = args.take_flag(&["--rocksdb"]);
    let strict = args.take_flag(&["--strict"]);
    let force = args.take_flag(&["--force"]);
    let incremental = args.take_flag(&["--incremental"]);
    let layout = match resolve_layout(&cfg, args.value(8), disk, rocksdb) {
        Ok(l) => l,
        Err(e) => return TOOL.usage_error(&e),
    };
    let disk = layout == gstore::StorageKind::Disk;
    let rocksdb = layout == gstore::StorageKind::RocksDb;
    let bulk_flags = (4..7).any(|i| args.value(i).is_some()) || incremental;
    if bulk_flags && !disk && !rocksdb {
        return TOOL.usage_error(
            "--memory, --threads, --tmp-dir and --incremental apply to --disk and --rocksdb builds",
        );
    }
    if args.value(7).is_some() && !disk {
        return TOOL.usage_error("--fill applies to --disk builds");
    }
    let bulk = match bulk_options(&cfg, &args) {
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
    let refuse_existing = || {
        eprintln!(
            "gbuild: the database path '{dir}' already exists. Please check database {name}, \
             or use gdrop to remove it at first (or rebuild it with --force)."
        );
        ExitCode::FAILURE
    };
    if !force && existed {
        return refuse_existing();
    }
    park_before_claim();
    // Claim the directory before anything is written into or beside it —
    // the parse error log, a zip package's scratch directory — and hold the
    // claim through the build (review bulkcli#2, #6): another process
    // building, loading or dropping the same database is refused from here
    // on (the builds below nest in this claim), and a failed build removes
    // the directory only while it holds it. The directory and any ancestor
    // the claim creates are made durable in their parents (round-5 review
    // follow-up of tests-b#3: a plain `create_dir_all` here left the
    // library builds, which sync only the directories they create, nothing
    // to sync).
    let claim = match BuildGuard::create_and_lock(Path::new(&dir)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gbuild: cannot build '{dir}': {e}");
            return ExitCode::FAILURE;
        }
    };
    // The directory was new when checked above; another process may have
    // created, built and released it in the moment before the claim (review
    // of bulkcli#2's fix). Under the claim nothing else can, so look again:
    // anything there besides the lock file is refused as above, and nothing
    // is removed (the claim is simply released).
    if !force && !existed && holds_more_than_its_lock(Path::new(&dir)) {
        drop(claim);
        return refuse_existing();
    }
    // A failed build removes what it created; one over an existing
    // database (`--force`) leaves the directory as it is.
    let mut created = CreatedDir {
        path: (!existed).then(|| PathBuf::from(&dir)),
        claim: Some(claim),
    };
    let mut out = Summary::default();
    if paths.is_empty() {
        out.line(format_args!("will build an empty database"));
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
    // A fresh build starts a fresh parse error log (under the claim: never
    // another build's log).
    let log_path = Path::new(&dir).join(import::PARSE_ERROR_LOG);
    let _ = std::fs::remove_file(&log_path);
    let mut log = match ParseErrorLog::open(&log_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("gbuild: cannot create {}: {e}", log_path.display());
            return ExitCode::FAILURE;
        }
    };
    if rocksdb && (incremental || !cfg!(feature = "rocksdb")) {
        let Some(report) = build_rocksdb(name, &dir, &sources, &opts, &mut log) else {
            return ExitCode::FAILURE;
        };
        // Built: whatever happens from here on, the database stays (review
        // bulkcli#3: a failed summary print removed it).
        created.keep();
        if existed && drop_old_rules(&dir) != ExitCode::SUCCESS {
            return ExitCode::FAILURE;
        }
        match Database::open(&dir) {
            Ok(db) => print_stats(&mut out, name, &dir, &db, started, "RocksDB"),
            Err(e) => {
                eprintln!("built, but failed to reopen: {e}");
                return ExitCode::FAILURE;
            }
        }
        print_report(&mut out, &report, log.path());
        if let Some(code) = out.failed(&dir) {
            return code;
        }
        return built_exit(&report, log.path());
    }
    let mut bulk_installed = false;
    let mut cleanup_incomplete = false;
    let report = if (disk || rocksdb) && !incremental {
        let target = if rocksdb {
            BulkTarget::RocksDb
        } else {
            BulkTarget::Disk
        };
        let built = gstore::bulk::build(
            Path::new(&dir),
            name,
            BulkInput::Sources(&sources),
            &opts,
            &mut log,
            target,
            &bulk,
        );
        match built {
            Ok(r) => {
                // Installed: kept before anything is printed (bulkcli#3).
                created.keep();
                print_bulk(&mut out, name, &dir, &r, target, started);
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
                let what = if rocksdb { "rocksdb" } else { "disk" };
                eprintln!("{what} build failed: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else if disk {
        let report =
            match Database::build_disk_from_sources_incremental(&dir, &sources, &opts, &mut log) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("disk build failed: {e}");
                    return ExitCode::FAILURE;
                }
            };
        created.keep();
        // Reopen to report stats.
        match Database::open(&dir) {
            Ok(db) => print_stats(&mut out, name, &dir, &db, started, "on-disk B+ tree"),
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
        created.keep();
        print_stats(&mut out, name, &dir, &db, started, "in-memory");
        report
    };
    print_report(&mut out, &report, &log_path);
    if cleanup_incomplete {
        return ExitCode::FAILURE;
    }
    // The bulk install removed the replaced database's rules itself, right
    // after its commit point.
    if existed && !bulk_installed && drop_old_rules(&dir) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }
    if let Some(code) = out.failed(&dir) {
        return code;
    }
    built_exit(&report, &log_path)
}

/// gbuild's standard output: the summary of a build, written line by line;
/// a write error (other than a reader gone away, which ends the output
/// quietly) is remembered instead of panicking, so a full disk or a failing
/// terminal never unwinds into the cleanup of a database that was built
/// (review bulkcli#3).
#[derive(Default)]
struct Summary {
    error: Option<std::io::Error>,
}

impl Summary {
    fn line(&mut self, args: std::fmt::Arguments<'_>) {
        if self.error.is_some() || gstore::stdout::is_closed() {
            return;
        }
        let r = writeln!(std::io::stdout().lock(), "{args}");
        if let Err(e) = gstore::stdout::quiet_broken_pipe(r) {
            self.error = Some(e);
        }
    }

    /// The exit status when the summary of a database built in `dir`
    /// could not be printed (reported on standard error), else `None`.
    fn failed(&self, dir: &str) -> Option<ExitCode> {
        let e = self.error.as_ref()?;
        eprintln!("gbuild: built '{dir}', but its summary could not be printed: {e}");
        Some(ExitCode::FAILURE)
    }
}

/// The exit status of a build that completed: 0, or [`SKIPPED`] when the
/// tolerant import skipped statements (review sparqlconf#12: a skipped
/// statement — a literal the loader refuses, say — must not pass for a
/// complete build in a script; the database is built all the same).
fn built_exit(report: &ImportReport, log: &Path) -> ExitCode {
    if report.failed == 0 {
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "gbuild: built, but {} statement(s) were skipped (listed in {}); exit status {SKIPPED}",
        report.failed,
        log.display()
    );
    ExitCode::from(SKIPPED)
}

/// The exit status of a build that skipped statements (see [`built_exit`]).
const SKIPPED: u8 = 3;

/// After a `--force` rebuild: remove the replaced database's reasoning rules
/// (`reason_rule_files/`), as dropping it would — executed, their ledgers
/// would name triples the new data holds as asserted ones.
fn drop_old_rules(dir: &str) -> ExitCode {
    let rules = Path::new(dir).join(gstore::reason::rules::RULE_DIR);
    // Under the rule lock, which finishes a replacement of the rules a
    // crash interrupted first (its staged copy would otherwise come back as
    // the rebuilt database's rules), and renamed aside before the delete
    // (review durability#11).
    match gstore::reason::store::remove_rule_dir(Path::new(dir)) {
        Ok(()) => ExitCode::SUCCESS,
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
/// build succeeded ([`CreatedDir::keep`]) — only under the build's claim on
/// it, taken before anything was written there, and only while it holds no
/// database (review bulkcli#2: the cleanup used to remove the directory
/// without its lock, deleting another process's build). The claim is
/// released after the cleanup.
struct CreatedDir {
    path: Option<PathBuf>,
    claim: Option<(BuildGuard, gstore::db::dirlock::DirLock)>,
}

impl CreatedDir {
    fn keep(&mut self) {
        self.path = None;
    }
}

impl Drop for CreatedDir {
    fn drop(&mut self) {
        if let (Some(p), Some(_)) = (self.path.take(), &self.claim) {
            if Database::storage_kind(&p).is_none() {
                let _ = std::fs::remove_dir_all(p);
            }
        }
        self.claim = None;
    }
}

/// Build a RocksDB-backed database (each file parsed by its extension). Gated on
/// the `rocksdb` feature. The import report of a build that completed (the
/// store closed again); `None` (the error printed) when it failed.
#[cfg(feature = "rocksdb")]
fn build_rocksdb(
    name: &str,
    dir: &str,
    sources: &[Source],
    opts: &ImportOptions,
    log: &mut ParseErrorLog,
) -> Option<ImportReport> {
    // Close the freshly built store before reopening it (RocksDB takes a lock).
    match Database::build_rocksdb_from_sources(name.to_string(), dir, sources, opts, log) {
        Ok((db, report)) => {
            drop(db);
            Some(report)
        }
        Err(e) => {
            eprintln!("rocksdb build failed: {e}");
            None
        }
    }
}

#[cfg(not(feature = "rocksdb"))]
fn build_rocksdb(
    _name: &str,
    _dir: &str,
    _sources: &[Source],
    _opts: &ImportOptions,
    _log: &mut ParseErrorLog,
) -> Option<ImportReport> {
    eprintln!("gbuild: --rocksdb requires building with `--features rocksdb`");
    None
}

/// Whether `path` exists as anything but an empty directory.
fn holds_data(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => path.exists(),
    }
}

/// [`holds_data`] for a directory this process has claimed: its lock file
/// (which the claim created) does not count.
fn holds_more_than_its_lock(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Ok(entries) => entries
            .flatten()
            .any(|e| e.file_name() != gstore::db::dirlock::LOCK_FILE),
        Err(_) => true,
    }
}

/// A test hook of debug builds (round-4 review of bulkcli#2's fix): with
/// `GSTORE_GBUILD_PARK_BEFORE_CLAIM=<path>` set, `gbuild` reads `<path>` to
/// its end between its "is the directory new?" check and its claim — a
/// FIFO parks it there until the test closes the FIFO's other end, so the
/// test can build the same name in that gap deterministically.
fn park_before_claim() {
    #[cfg(debug_assertions)]
    if let Some(p) = std::env::var_os("GSTORE_GBUILD_PARK_BEFORE_CLAIM") {
        let _ = std::fs::read(p);
    }
}

fn print_stats(
    out: &mut Summary,
    name: &str,
    dir: &str,
    db: &Database,
    started: Instant,
    backend: &str,
) {
    out.line(format_args!("Built database '{name}' in {dir} ({backend})"));
    out.line(format_args!(
        "  triples={}  entities={}  literals={}  predicates={}",
        db.triple_num(),
        db.entity_num(),
        db.literal_num(),
        db.predicate_num()
    ));
    let quads: u64 = db.named_graphs().values().map(|g| g.triple_count()).sum();
    if quads > 0 {
        out.line(format_args!(
            "  named graphs={}  named-graph quads={quads}",
            db.named_graphs().len()
        ));
    }
    out.line(format_args!(
        "  took {:.3}s",
        started.elapsed().as_secs_f64()
    ));
}

fn print_bulk(
    out: &mut Summary,
    name: &str,
    dir: &str,
    r: &BulkReport,
    target: BulkTarget,
    started: Instant,
) {
    let backend = match target {
        BulkTarget::Disk => "on-disk B+ tree",
        BulkTarget::RocksDb => "RocksDB",
    };
    out.line(format_args!(
        "Built database '{name}' in {dir} ({backend}, bulk build)"
    ));
    out.line(format_args!("{}", r.summary_for(target)));
    out.line(format_args!(
        "  took {:.3}s",
        started.elapsed().as_secs_f64()
    ));
}

fn print_report(out: &mut Summary, report: &ImportReport, log: &Path) {
    out.line(format_args!(
        "  read {} statement(s) from {} source(s)",
        report.statements, report.documents
    ));
    if report.failed > 0 {
        out.line(format_args!(
            "  RDF parse error num {}; see {}",
            report.failed,
            log.display()
        ));
    }
}
