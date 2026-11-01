//! Data test: the command-line tools' compatibility with upstream gStore
//! 1.3's spellings (`docs/GAP_PLAN.md` §5 "老工具兼容") — the `-db`/`-f`/…
//! option aliases, `grestore`'s argument order, `gshow` listing every
//! database, `gbuild` building an empty database and refusing to overwrite
//! one, and the shared `--help` / exit-status conventions (0 ok, 1 failed,
//! 2 invalid arguments).
//!
//! Every test runs the real binaries in a fresh working directory (the tools
//! resolve `<name>` to `./<name>.db`, as upstream's do under its root).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

fn small_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/small/small.nt")
}

fn workdir(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!(
        "gstore_dt_cli_compat_{tag}_{}_{}",
        std::process::id(),
        CTR.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

impl std::fmt::Debug for Run {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exit {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.out, self.err
        )
    }
}

fn run(bin: &str, args: &[&str], cwd: &Path) -> Run {
    let o: Output = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("run {bin}: {e}"));
    Run {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn ok(bin: &str, args: &[&str], cwd: &Path) -> Run {
    let r = run(bin, args, cwd);
    assert_eq!(r.code, 0, "{bin} {args:?}: {r:?}");
    r
}

/// The triple count `gshow <db>` reports.
fn triples(db: &str, cwd: &Path) -> u64 {
    let r = ok(env!("CARGO_BIN_EXE_gshow"), &[db], cwd);
    r.out
        .lines()
        .find_map(|l| l.strip_prefix("triples   : "))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("no triple count: {r:?}"))
}

const GBUILD: &str = env!("CARGO_BIN_EXE_gbuild");
const GQUERY: &str = env!("CARGO_BIN_EXE_gquery");
const GADD: &str = env!("CARGO_BIN_EXE_gadd");
const GSUB: &str = env!("CARGO_BIN_EXE_gsub");
const GDROP: &str = env!("CARGO_BIN_EXE_gdrop");
const GEXPORT: &str = env!("CARGO_BIN_EXE_gexport");
const GBACKUP: &str = env!("CARGO_BIN_EXE_gbackup");
const GRESTORE: &str = env!("CARGO_BIN_EXE_grestore");
const GSHOW: &str = env!("CARGO_BIN_EXE_gshow");
const GMONITOR: &str = env!("CARGO_BIN_EXE_gmonitor");

const ALL: [&str; 10] = [
    GBUILD, GQUERY, GADD, GSUB, GDROP, GEXPORT, GBACKUP, GRESTORE, GSHOW, GMONITOR,
];

// ---- the -db / -f aliases ---------------------------------------------------------

#[test]
fn upstream_option_spellings_work_for_every_tool() {
    let dir = workdir("aliases");
    let nt = small_nt();
    let nt = nt.to_str().unwrap();
    // gbuild -db NAME -f FILE (upstream's QUICK_START: bin/gbuild -db lubm -f …)
    let r = ok(GBUILD, &["-db", "lubm", "-f", nt], &dir);
    assert!(r.out.contains("triples=25"), "{r:?}");
    assert!(dir.join("lubm.db").is_dir());

    // gquery -db NAME -q FILE, and the long spellings with -f RESULTFILE.
    fs::write(dir.join("q.sql"), "SELECT ?o WHERE { <root> <contain> ?o }").unwrap();
    let r = ok(GQUERY, &["-db", "lubm", "-q", "q.sql"], &dir);
    assert!(
        r.out.contains("<node0>") && r.err.contains("5 row(s)"),
        "{r:?}"
    );
    let r = ok(
        GQUERY,
        &[
            "--database",
            "lubm",
            "--queryfile",
            "q.sql",
            "-f",
            "result.txt",
        ],
        &dir,
    );
    assert!(r.out.is_empty(), "the result went to the file: {r:?}");
    assert!(r.err.contains("5 row(s)"), "{r:?}");
    let result = fs::read_to_string(dir.join("result.txt")).unwrap();
    assert!(result.contains("<node4>"), "{result}");
    // -e still works with -db; and the positional form is unchanged.
    let r = ok(
        GQUERY,
        &["-db", "lubm", "-e", "ASK { <root> <contain> <node0> }"],
        &dir,
    );
    assert_eq!(r.out.trim(), "true");
    let r = ok(GQUERY, &["lubm", "q.sql"], &dir);
    assert!(r.out.contains("<node0>"), "{r:?}");

    // gadd -db NAME -f FILE / -dir DIR; gsub -db NAME --file FILE.
    fs::write(dir.join("add.nt"), "<x1> <p> <y1> .\n").unwrap();
    fs::create_dir(dir.join("more")).unwrap();
    fs::write(
        dir.join("more").join("m.nt"),
        "<x2> <p> <y2> .\n<x3> <p> <y3> .\n",
    )
    .unwrap();
    ok(GADD, &["-db", "lubm", "-f", "add.nt"], &dir);
    assert_eq!(triples("lubm", &dir), 26);
    ok(GADD, &["--database", "lubm", "-dir", "more"], &dir);
    assert_eq!(triples("lubm", &dir), 28);
    ok(GSUB, &["-db", "lubm", "--file", "add.nt"], &dir);
    assert_eq!(triples("lubm", &dir), 27);
    ok(GSUB, &["lubm", "more"], &dir);
    assert_eq!(triples("lubm", &dir), 25);

    // gmonitor -db / gshow -db.
    let r = ok(GMONITOR, &["-db", "lubm"], &dir);
    assert!(
        r.out.contains("triple_num") && r.out.contains("25"),
        "{r:?}"
    );
    let r = ok(GSHOW, &["--database", "lubm"], &dir);
    assert!(r.out.contains("triples   : 25"), "{r:?}");

    // gdrop -db: the confirmation is still required.
    let r = run(GDROP, &["-db", "lubm"], &dir);
    assert_eq!(r.code, 1, "{r:?}");
    assert!(dir.join("lubm.db").is_dir());
    ok(GDROP, &["-db", "lubm", "--yes"], &dir);
    assert!(!dir.join("lubm.db").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn gexport_writes_where_upstream_does() {
    let dir = workdir("gexport");
    ok(GBUILD, &["x", small_nt().to_str().unwrap()], &dir);
    let nt_files = |d: &Path| -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".nt"))
            .collect();
        v.sort();
        v
    };
    let is_stamped = |n: &str| {
        n.strip_prefix("x_")
            .and_then(|r| r.strip_suffix(".nt"))
            .is_some_and(|ts| ts.len() >= 13 && ts.bytes().all(|b| b.is_ascii_digit()))
    };
    // No -f: <db>_<timestamp>.nt in the current directory.
    let r = ok(GEXPORT, &["-db", "x"], &dir);
    assert!(r.out.contains("Exported 25 triple(s)"), "{r:?}");
    let files = nt_files(&dir);
    assert!(files.len() == 1 && is_stamped(&files[0]), "{files:?}");
    // -f DIR (created when missing): the same name inside it.
    ok(GEXPORT, &["--database", "x", "-f", "exports"], &dir);
    let files = nt_files(&dir.join("exports"));
    assert!(files.len() == 1 && is_stamped(&files[0]), "{files:?}");
    let text = fs::read_to_string(dir.join("exports").join(&files[0])).unwrap();
    assert_eq!(text.lines().count(), 25);
    // -f FILE.nt: that file.
    ok(GEXPORT, &["-db", "x", "-f", "dump.nt"], &dir);
    assert_eq!(
        fs::read_to_string(dir.join("dump.nt"))
            .unwrap()
            .lines()
            .count(),
        25
    );
    // The positional form is unchanged.
    ok(GEXPORT, &["x", "plain.nt"], &dir);
    assert!(dir.join("plain.nt").is_file());
    let _ = fs::remove_dir_all(&dir);
}

// ---- grestore: the database first --------------------------------------------------

#[test]
fn grestore_takes_the_database_then_the_backup() {
    let dir = workdir("grestore");
    ok(GBUILD, &["src", small_nt().to_str().unwrap()], &dir);
    ok(GBACKUP, &["src", "bak"], &dir);

    // Upstream's order: grestore <db_name> <backup_dir>.
    let r = ok(GRESTORE, &["dst", "bak"], &dir);
    assert!(r.out.contains("Restored 'bak' into 'dst.db'"), "{r:?}");
    assert_eq!(triples("dst", &dir), 25);

    // The old order is refused, with a hint, and restores nothing.
    let r = run(GRESTORE, &["bak", "dst2"], &dir);
    assert_eq!(r.code, 1, "{r:?}");
    assert!(
        r.err.contains("backup directory 'dst2' does not exist"),
        "{r:?}"
    );
    assert!(r.err.contains("grestore <db_name> <backup_dir>"), "{r:?}");
    assert!(!dir.join("bak.db").exists() && !dir.join("dst2.db").exists());

    // gbackup -db NAME: a timestamped backup under ./backups/; grestore -l
    // lists it, grestore -db NAME -p PATH restores it.
    let r = ok(GBACKUP, &["-db", "src"], &dir);
    assert!(r.out.contains("backups"), "{r:?}");
    let backups: Vec<String> = fs::read_dir(dir.join("backups"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert!(backups[0].starts_with("src.db_"), "{backups:?}");
    let r = ok(GRESTORE, &["-l", "src"], &dir);
    assert!(r.out.starts_with("Backup Path\n"), "{r:?}");
    assert!(r.out.contains(&backups[0]), "{r:?}");
    let r = ok(GRESTORE, &["--list", "nothing"], &dir);
    assert_eq!(r.out.trim(), "Backup Path", "{r:?}");
    let path = format!("./backups/{}", backups[0]);
    ok(GRESTORE, &["-db", "dst3", "-p", &path], &dir);
    assert_eq!(triples("dst3", &dir), 25);
    // gbackup -db NAME -p DIR.
    ok(GBACKUP, &["--database", "src", "--path", "elsewhere"], &dir);
    let n = fs::read_dir(dir.join("elsewhere")).unwrap().count();
    assert_eq!(n, 1);
    let _ = fs::remove_dir_all(&dir);
}

// ---- gshow lists every database ----------------------------------------------------

fn table_rows(out: &str) -> Vec<Vec<String>> {
    out.lines()
        .filter(|l| l.starts_with('|'))
        .map(|l| {
            l.trim_matches('|')
                .split('|')
                .map(|c| c.trim().to_string())
                .collect()
        })
        .collect()
}

#[test]
fn gshow_lists_every_database() {
    let dir = workdir("gshow");
    let nt = small_nt();
    ok(GBUILD, &["alpha", nt.to_str().unwrap()], &dir);
    ok(GBUILD, &["--disk", "beta", nt.to_str().unwrap()], &dir);
    ok(GBUILD, &["gamma"], &dir);
    // Not databases: an empty `.db` directory, a `.db` file, another directory.
    fs::create_dir(dir.join("junk.db")).unwrap();
    fs::write(dir.join("users.db"), "not a database\n").unwrap();
    fs::create_dir(dir.join("other")).unwrap();

    let r = ok(GSHOW, &[], &dir);
    let rows = table_rows(&r.out);
    assert_eq!(rows[0], ["database", "creator", "built_time"], "{r:?}");
    let names: Vec<&str> = rows[1..].iter().map(|r| r[0].as_str()).collect();
    assert_eq!(names, ["alpha", "beta", "gamma"], "{r:?}");
    for row in &rows[1..] {
        assert_eq!(row[1], "root");
        assert_eq!(row[2].len(), 19, "YYYY-MM-DD HH:MM:SS: {row:?}");
    }
    assert!(r.out.starts_with("+-"), "upstream's table: {r:?}");

    // The server registry, when present, supplies creator and build time.
    fs::write(
        dir.join("system.json"),
        "{\"format\":1,\"databases\":[{\"database\":\"alpha\",\"creator\":\"alice\",\
         \"built_time\":\"2021-08-22 11:08:57\"}]}\n",
    )
    .unwrap();
    let rows = table_rows(&ok(GSHOW, &[], &dir).out);
    assert_eq!(rows[1], ["alpha", "alice", "2021-08-22 11:08:57"]);
    assert_eq!(rows[2][1], "root");

    // --db-home DIR lists another directory; an empty one gives just headers.
    let r = ok(GSHOW, &["--db-home", dir.to_str().unwrap()], Path::new("/"));
    assert_eq!(table_rows(&r.out).len(), 4, "{r:?}");
    let r = ok(GSHOW, &["--db-home", "other"], &dir);
    assert_eq!(table_rows(&r.out).len(), 1, "{r:?}");
    let r = run(GSHOW, &["--db-home", "missing"], &dir);
    assert_eq!(r.code, 1, "{r:?}");
    // A database name still shows that database's counts.
    assert_eq!(triples("beta", &dir), 25);
    let _ = fs::remove_dir_all(&dir);
}

// ---- gbuild: empty databases, no silent overwrite ------------------------------------

#[test]
fn gbuild_builds_empty_databases_and_never_overwrites_silently() {
    let dir = workdir("gbuild");
    let nt = small_nt();
    let nt = nt.to_str().unwrap();

    // No source: an empty database (upstream: "will build an empty database").
    let r = ok(GBUILD, &["e"], &dir);
    assert!(r.out.contains("will build an empty database"), "{r:?}");
    assert!(r.out.contains("triples=0"), "{r:?}");
    assert_eq!(triples("e", &dir), 0);
    ok(GBUILD, &["-db", "e2", "--disk"], &dir);
    assert_eq!(triples("e2", &dir), 0);
    let r = ok(GSHOW, &["e2"], &dir);
    assert!(r.out.contains("on-disk"), "{r:?}");
    // An empty database takes data like any other.
    ok(GADD, &["e", nt], &dir);
    assert_eq!(triples("e", &dir), 25);

    // An existing database is not replaced …
    let r = run(GBUILD, &["e2", nt], &dir);
    assert_eq!(r.code, 1, "{r:?}");
    assert!(r.err.contains("already exists"), "{r:?}");
    assert!(
        r.err.contains("gdrop") && r.err.contains("--force"),
        "{r:?}"
    );
    assert_eq!(triples("e2", &dir), 0, "untouched");
    let r = run(GBUILD, &["-db", "e", "-f", nt], &dir);
    assert_eq!(r.code, 1, "{r:?}");
    assert_eq!(triples("e", &dir), 25);
    // … unless --force; then the build replaces it, whatever its layout —
    // and the old database's reasoning rules go with it.
    let rules = dir.join("e2.db/reason_rule_files");
    fs::create_dir_all(rules.join("inferred")).unwrap();
    fs::write(rules.join("r.json"), "{}").unwrap();
    fs::write(rules.join("inferred/r.nt"), "<a>\t<Rule:r>\t<b>\t.\n").unwrap();
    let r = ok(GBUILD, &["--force", "e2", nt], &dir);
    assert!(r.out.contains("triples=25"), "{r:?}");
    assert!(!rules.exists(), "{r:?}");
    let r = ok(GSHOW, &["e2"], &dir);
    assert!(
        r.out.contains("in-memory") || r.out.contains("snapshot"),
        "{r:?}"
    );
    // A directory that exists but is empty is not a database: built into.
    fs::create_dir(dir.join("fresh.db")).unwrap();
    ok(GBUILD, &["fresh", nt], &dir);
    assert_eq!(triples("fresh", &dir), 25);
    // So is a stray file in its place refused.
    fs::write(dir.join("file.db"), "x").unwrap();
    assert_eq!(run(GBUILD, &["file"], &dir).code, 1);
    let _ = fs::remove_dir_all(&dir);
}

// ---- help and exit statuses -------------------------------------------------------

#[test]
fn every_tool_has_help_and_the_same_exit_statuses() {
    let dir = workdir("exit");
    for bin in ALL {
        for flag in ["-h", "--help"] {
            let r = run(bin, &[flag], &dir);
            assert_eq!(r.code, 0, "{bin} {flag}: {r:?}");
            assert!(r.out.contains("Usage:"), "{bin} {flag}: {r:?}");
            assert!(r.out.contains("-db,--database"), "{bin} {flag}: {r:?}");
            assert!(r.err.is_empty(), "{bin} {flag}: {r:?}");
        }
        // An unknown option, and an option without its value: status 2.
        let r = run(bin, &["--no-such-option", "x"], &dir);
        assert_eq!(r.code, 2, "{bin}: {r:?}");
        assert!(r.err.contains("usage:"), "{bin}: {r:?}");
        assert!(r.err.contains(" -h\" for help"), "{bin}: {r:?}");
        let r = run(bin, &["-db"], &dir);
        assert_eq!(r.code, 2, "{bin}: {r:?}");
        assert!(r.err.contains("-db needs a value"), "{bin}: {r:?}");
        let r = run(bin, &["-db", "a", "--database", "b"], &dir);
        assert_eq!(r.code, 2, "{bin}: {r:?}");
        // No arguments: invalid for all but gshow (which lists the databases).
        let r = run(bin, &[], &dir);
        if bin == GSHOW {
            assert_eq!(r.code, 0, "{r:?}");
        } else {
            assert_eq!(r.code, 2, "{bin}: {r:?}");
            assert!(r.err.contains("usage:"), "{bin}: {r:?}");
        }
    }
    // A database that does not exist is an operation failure: status 1.
    for (bin, args) in [
        (GQUERY, vec!["nope", "-e", "ASK {}"]),
        (GADD, vec!["nope", "x.nt"]),
        (GSUB, vec!["nope", "x.nt"]),
        (GDROP, vec!["nope", "--yes"]),
        (GEXPORT, vec!["nope", "out.nt"]),
        (GBACKUP, vec!["nope", "bak"]),
        (GRESTORE, vec!["nope", "no_backup"]),
        (GSHOW, vec!["nope"]),
        (GMONITOR, vec!["nope"]),
    ] {
        let r = run(bin, &args, &dir);
        assert_eq!(r.code, 1, "{bin} {args:?}: {r:?}");
    }
    // gquery without a query (upstream would open its console).
    let r = run(GQUERY, &["-db", "nope"], &dir);
    assert_eq!(r.code, 2, "{r:?}");
    assert!(r.err.contains("no query given"), "{r:?}");
    let _ = fs::remove_dir_all(&dir);
}
