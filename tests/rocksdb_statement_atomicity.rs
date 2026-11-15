//! RocksDB: auto-commit statements are crash-atomic, and no crash leaves an
//! orphan dictionary term (review H-11 / gap-rocksdb-exec#1, #4, #8).
//!
//! Every scenario runs in a child process — this test binary re-executed —
//! on a fresh copy of a base database. The child installs a write hook on
//! the RocksDB store (`RocksStore::debug_set_write_hook`) that, after the
//! k-th write batch of the statement, prints `STOPPED` and parks; the parent
//! then kills it with SIGKILL. Stop points are spread over the statement's
//! writes (counted by a calibration run that is not stopped), so the kills
//! land at exact, repeatable points: deterministic, no timing involved.
//!
//! After every kill the parent reopens the database and checks that
//!
//! * its contents are those of a statement boundary — the state before the
//!   statement or after it, never a prefix of it (a DELETE/INSERT rename
//!   that only deleted, half a LOAD, …), and
//! * every live dictionary term is referenced by the data (live counts equal
//!   the distinct terms of the contents): the terms a statement or a
//!   rolled-back transaction wrote are freed again, not left as orphans —
//!   also when the kill falls between a new term's write and its triple's,
//!   or inside a rollback.

#![cfg(feature = "rocksdb")]

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use gstore::{Database, LoadPolicy, QueryResult};

const CHILD_ENV: &str = "GSTORE_ROCKS_ATOMIC_CHILD";
const DIR_ENV: &str = "GSTORE_ROCKS_ATOMIC_DIR";
const SCENARIO_ENV: &str = "GSTORE_ROCKS_ATOMIC_SCENARIO";
const STOP_ENV: &str = "GSTORE_ROCKS_ATOMIC_STOP";

/// A temp directory removed on drop (also when a test panics).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d =
            std::env::temp_dir().join(format!("gstore_rocks_atomic_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn copy_dir(from: &Path, to: &Path) {
    let _ = std::fs::remove_dir_all(to);
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

// ---- the scenarios ------------------------------------------------------------

const SUBJECTS: usize = 120;
const LOADED: usize = 150;
const TXN_INSERTS: usize = 60;

/// The base database: subjects with a status and a name, and a named graph.
fn base_nt() -> String {
    let mut text = String::new();
    for i in 0..SUBJECTS {
        text.push_str(&format!(
            "<http://ex/s{i}> <http://ex/status> \"st{}\" .\n\
             <http://ex/s{i}> <http://ex/name> \"n{i}\" .\n",
            i % 7
        ));
    }
    text
}

fn base_quads() -> String {
    let mut q = String::from("INSERT DATA { GRAPH <http://ex/g> { ");
    for i in 0..SUBJECTS / 2 {
        q.push_str(&format!("<http://ex/s{i}> <http://ex/in> \"q{i}\" . "));
    }
    q.push_str("} }");
    q
}

/// The N-Triples file the LOAD / `insert_sources` scenarios read: fresh
/// subjects and literals (each statement mints two terms), and a fresh
/// predicate.
fn load_nt() -> String {
    (0..LOADED)
        .map(|i| format!("<http://ex/new{i}> <http://ex/loaded> \"fresh {i}\" .\n"))
        .collect()
}

/// The file the `remove_sources` scenario reads: half the base's names.
fn remove_nt() -> String {
    (0..SUBJECTS)
        .step_by(2)
        .map(|i| format!("<http://ex/s{i}> <http://ex/name> \"n{i}\" .\n"))
        .collect()
}

/// The statement of an auto-commit scenario (`None`: one that is not a
/// SPARQL UPDATE).
fn statement(scenario: &str, dir: &Path) -> Option<String> {
    Some(match scenario {
        // A predicate rename: deletes and inserts in one request.
        "rename" => "DELETE { ?s <http://ex/status> ?o } INSERT { ?s <http://ex/state> ?o } \
                     WHERE { ?s <http://ex/status> ?o }"
            .to_string(),
        "insert_data" => {
            let mut q = String::from("INSERT DATA { ");
            for i in 0..LOADED {
                q.push_str(&format!("<http://ex/d{i}> <http://ex/data> \"d{i}\" . "));
            }
            q.push('}');
            q
        }
        "load" => format!("LOAD <file://{}>", dir.join("load.nt").display()),
        "clear_graph" => "CLEAR GRAPH <http://ex/g>".to_string(),
        _ => return None,
    })
}

/// Apply scenario `scenario` to `db` (the oracle, or the child's database).
fn run(scenario: &str, db: &mut Database, files: &Path) {
    db.set_load_policy(LoadPolicy::permissive());
    let sources =
        |name: &str| gstore::parser::import::sources_from_paths(&[files.join(name)]).unwrap();
    match scenario {
        "insert_sources" => {
            let mut applied = 0;
            db.insert_sources(
                &sources("load.nt"),
                &gstore::parser::import::ImportOptions::strict(),
                &mut (),
                &mut applied,
            )
            .unwrap();
            db.sync().unwrap();
        }
        "remove_sources" => {
            let mut applied = 0;
            db.remove_sources(
                &sources("remove.nt"),
                &gstore::parser::import::ImportOptions::strict().for_removal(),
                &mut (),
                &mut applied,
            )
            .unwrap();
            db.sync().unwrap();
        }
        // The transaction itself is set up by `prepare`; its rollback is
        // the scenario.
        "rollback" => db.rollback().unwrap(),
        _ => {
            let q = statement(scenario, files).unwrap();
            db.query(&q).unwrap_or_else(|e| panic!("{scenario}: {e}"));
        }
    }
}

/// What a scenario does before its stop points count: the rollback
/// scenario opens a transaction with fresh triples (fresh terms each).
fn prepare(scenario: &str, db: &mut Database) {
    if scenario == "rollback" {
        db.begin().unwrap();
        let mut q = String::from("INSERT DATA { ");
        for i in 0..TXN_INSERTS {
            q.push_str(&format!(
                "<http://ex/t{i}> <http://ex/tp{}> \"tv{i}\" . ",
                i % 3
            ));
        }
        q.push('}');
        db.query(&q).unwrap();
    }
}

// ---- the child --------------------------------------------------------------

/// The child: open the database, prepare, install the stop hook, run the
/// scenario. Prints `STOPPED <k>` and parks at the stop point, or prints
/// `FINISHED <writes>` when the scenario ends first.
#[test]
fn crash_child() {
    if std::env::var(CHILD_ENV).is_err() {
        return; // not the child
    }
    let dir = PathBuf::from(std::env::var(DIR_ENV).unwrap());
    let scenario = std::env::var(SCENARIO_ENV).unwrap();
    let stop: u64 = std::env::var(STOP_ENV).unwrap().parse().unwrap();
    let mut db = Database::open(dir.join("db")).unwrap();
    prepare(&scenario, &mut db);
    let writes = Arc::new(AtomicU64::new(0));
    let seen = Arc::clone(&writes);
    db.backend()
        .as_rocks()
        .expect("a RocksDB database")
        .debug_set_write_hook(Some(Arc::new(move |n| {
            seen.store(n, Ordering::SeqCst);
            if n == stop {
                let mut out = std::io::stdout().lock();
                writeln!(out, "STOPPED {n}").unwrap();
                out.flush().unwrap();
                loop {
                    std::thread::park();
                }
            }
        })));
    run(&scenario, &mut db, &dir);
    let mut out = std::io::stdout().lock();
    writeln!(out, "FINISHED {}", writes.load(Ordering::SeqCst)).unwrap();
    out.flush().unwrap();
}

/// How a child run ended.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// Stopped (and then killed) after this many write batches.
    Stopped(u64),
    /// Ran to its end, after this many write batches.
    Finished(u64),
}

/// Run `scenario` in a child on `dir`, killing it at write `stop`.
fn run_child(dir: &Path, scenario: &str, stop: u64) -> Outcome {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .env(DIR_ENV, dir)
        .env(SCENARIO_ENV, scenario)
        .env(STOP_ENV, stop.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let errors = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    // The marker line, read on a thread so that a hung child cannot hang
    // the test (the timeout is only a hang guard).
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let marker = line
                .find("STOPPED ")
                .map(|at| Outcome::Stopped(line[at + 8..].trim().parse().unwrap()))
                .or_else(|| {
                    line.find("FINISHED ")
                        .map(|at| Outcome::Finished(line[at + 9..].trim().parse().unwrap()))
                });
            if let Some(m) = marker {
                let _ = tx.send(m);
                return;
            }
        }
    });
    let outcome = rx.recv_timeout(Duration::from_secs(300));
    let _ = child.kill(); // SIGKILL (a no-op once it exited)
    let status = child.wait().unwrap();
    let errors = errors.join().unwrap();
    match outcome {
        Ok(o) => o,
        Err(_) => panic!(
            "{scenario} (stop {stop}): the child neither stopped nor finished ({status}): {errors}"
        ),
    }
}

// ---- checks -------------------------------------------------------------------

/// Every default-graph triple and named-graph quad, as sorted strings.
fn contents(db: &mut Database) -> Vec<String> {
    let q = "SELECT ?g ?s ?p ?o WHERE { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }";
    match db.query(q).unwrap() {
        QueryResult::Select(rs) => {
            let mut v: Vec<String> = rs
                .rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|c| c.clone().unwrap_or_else(|| "-".into()))
                        .collect::<Vec<_>>()
                        .join("\t")
                })
                .collect();
            v.sort();
            v
        }
        other => panic!("{other:?}"),
    }
}

/// The distinct terms `contents` references: nodes (subjects, objects,
/// graph IRIs) plus predicates.
fn referenced(contents: &[String]) -> usize {
    let mut nodes = BTreeSet::new();
    let mut preds = BTreeSet::new();
    for row in contents {
        let c: Vec<&str> = row.split('\t').collect();
        if c[0] != "-" {
            nodes.insert(c[0].to_string());
        }
        nodes.insert(c[1].to_string());
        preds.insert(c[2].to_string());
        nodes.insert(c[3].to_string());
    }
    nodes.len() + preds.len()
}

fn live_terms(db: &Database) -> usize {
    db.entity_num() + db.literal_num() + db.predicate_num()
}

/// The states before and after `scenario`, from an in-memory oracle.
fn oracle(scenario: &str, files: &Path) -> (Vec<String>, Vec<String>) {
    let mut db = Database::build_from_str("oracle", &base_nt()).unwrap();
    db.query(&base_quads()).unwrap();
    let before = contents(&mut db);
    prepare(scenario, &mut db);
    run(scenario, &mut db, files);
    (before, contents(&mut db))
}

/// The stop points of a statement of `writes` write batches: its first
/// ones, a spread over the middle, and its last ones.
fn stop_points(writes: u64) -> Vec<u64> {
    let mut v: BTreeSet<u64> = [1, 2, 3, writes / 4, writes / 2, writes * 3 / 4]
        .into_iter()
        .collect();
    for d in 0..3 {
        v.insert(writes.saturating_sub(d));
    }
    v.into_iter().filter(|&k| k >= 1 && k <= writes).collect()
}

/// Kill `scenario` at every stop point; every reopened state must be a
/// statement boundary without orphan terms.
fn battery(scenario: &str) {
    let tmp = TempDir::new(scenario);
    let base = tmp.0.join("base");
    std::fs::create_dir_all(&base).unwrap();
    {
        let mut db =
            Database::build_rocksdb_from_str("atomic", base.join("db"), &base_nt()).unwrap();
        db.query(&base_quads()).unwrap();
    }
    std::fs::write(base.join("load.nt"), load_nt()).unwrap();
    std::fs::write(base.join("remove.nt"), remove_nt()).unwrap();
    let (before, after) = oracle(scenario, &base);
    // A rollback ends where it began; every other scenario changes data.
    assert_eq!(before == after, scenario == "rollback", "{scenario}");
    let work = tmp.0.join("work");

    // Calibration: how many write batches the statement makes.
    copy_dir(&base, &work);
    let Outcome::Finished(writes) = run_child(&work, scenario, u64::MAX) else {
        panic!("{scenario}: the calibration run stopped");
    };
    assert!(writes > 4, "{scenario}: only {writes} write batches");
    let mut db = Database::open(work.join("db")).unwrap();
    assert_eq!(contents(&mut db), after, "{scenario}: not stopped");
    drop(db);

    for stop in stop_points(writes) {
        copy_dir(&base, &work);
        let outcome = run_child(&work, scenario, stop);
        assert_eq!(
            outcome,
            Outcome::Stopped(stop),
            "{scenario}: the child must reach write {stop} before it is killed"
        );
        let mut db = Database::open(work.join("db")).unwrap();
        db.verify_integrity()
            .unwrap_or_else(|e| panic!("{scenario} killed after write {stop}: {e}"));
        let got = contents(&mut db);
        let boundary = if got == before {
            "before"
        } else if got == after {
            "after"
        } else {
            panic!(
                "{scenario} killed after write {stop} of {writes}: the reopened store is no \
                 statement boundary ({} rows; before {}, after {}): missing {:?}, extra {:?}",
                got.len(),
                before.len(),
                after.len(),
                before
                    .iter()
                    .filter(|r| !got.contains(r))
                    .take(3)
                    .collect::<Vec<_>>(),
                got.iter()
                    .filter(|r| !before.contains(r))
                    .take(3)
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            live_terms(&db),
            referenced(&got),
            "{scenario} killed after write {stop} of {writes} (state {boundary}): orphan terms"
        );
        drop(db);
    }
}

#[test]
fn a_killed_rename_recovers_to_a_statement_boundary() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    battery("rename");
}

#[test]
fn a_killed_insert_data_recovers_to_a_statement_boundary() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    battery("insert_data");
}

#[test]
fn a_killed_load_recovers_to_a_statement_boundary() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    battery("load");
}

#[test]
fn a_killed_clear_graph_recovers_to_a_statement_boundary() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    battery("clear_graph");
}

#[test]
fn a_killed_insert_sources_recovers_to_a_statement_boundary() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    battery("insert_sources");
}

#[test]
fn a_killed_remove_sources_recovers_to_a_statement_boundary() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    battery("remove_sources");
}

/// A rollback killed at any point — while its journal is replayed, or
/// between the replay and the deletion of the journal — leaves the
/// begin-time data and frees every term the transaction minted.
#[test]
fn a_killed_rollback_leaves_no_orphan_terms() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    battery("rollback");
}
