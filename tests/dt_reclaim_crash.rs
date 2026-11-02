//! Phase 6 R4-RECLAIM: id reclamation survives a crash.
//!
//! The test re-executes its own binary as a child that applies a churn of
//! inserts and deletes (each delete frees ids; inserts reuse them) through
//! auto-commit SPARQL updates and transactions, printing an acknowledgement
//! after each one returned. The parent kills it with SIGKILL at a random
//! moment, 20 times per layout, then reopens the database and checks that
//! `verify_integrity` holds, every acknowledged operation is there (the one
//! in flight may or may not be), at most 4 terms are orphans (interned or
//! kept by an interrupted operation), and `reclaim_unused` removes them.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use gstore::db::OpenOptions;
use gstore::{Database, QueryResult};

const CHILD_ENV: &str = "GSTORE_RECLAIM_CRASH_CHILD";
const DIR_ENV: &str = "GSTORE_RECLAIM_CRASH_DIR";
const LAYOUT_ENV: &str = "GSTORE_RECLAIM_CRASH_LAYOUT";
const START_ENV: &str = "GSTORE_RECLAIM_CRASH_START";

/// A temp directory removed on drop (also when a test panics).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d = std::env::temp_dir().join(format!(
            "gstore_dt_reclaim_crash_{tag}_{}",
            std::process::id()
        ));
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

/// SplitMix64.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// The triple of operation `i`: fresh subject and object terms each time
/// (so a delete leaves them unreferenced), a few shared predicates.
fn triple(i: u64) -> String {
    format!("<http://ex/s{i}> <http://ex/p{}> \"o{i}\"", i % 7)
}

/// The operations, deterministic: `(insert?, i)` groups; each group is one
/// acknowledged statement (a transaction for a group of several).
fn ops(start: u64) -> impl Iterator<Item = Vec<(bool, u64)>> {
    let mut rng = Rng(start.wrapping_mul(7919) + 1);
    let mut next = start;
    let mut live: Vec<u64> = Vec::new();
    std::iter::from_fn(move || {
        let n = if rng.below(5) == 0 {
            2 + rng.below(3)
        } else {
            1
        };
        let mut group = Vec::new();
        for _ in 0..n {
            if !live.is_empty() && rng.below(2) == 0 {
                let k = rng.below(live.len() as u64) as usize;
                group.push((false, live.swap_remove(k)));
            } else {
                group.push((true, next));
                live.push(next);
                next += 1;
            }
        }
        Some(group)
    })
}

fn apply(db: &mut Database, group: &[(bool, u64)]) {
    let stmt = |&(ins, i): &(bool, u64)| {
        format!(
            "{} DATA {{ {} }}",
            if ins { "INSERT" } else { "DELETE" },
            triple(i)
        )
    };
    if group.len() == 1 {
        db.query(&stmt(&group[0])).unwrap();
    } else {
        db.begin().unwrap();
        for op in group {
            db.query(&stmt(op)).unwrap();
        }
        db.commit().unwrap();
    }
}

fn options(layout: &str) -> OpenOptions {
    match layout {
        "materialize" => OpenOptions::materialize(),
        _ => OpenOptions::stream(),
    }
}

fn open(layout: &str, dir: &Path) -> Database {
    match layout {
        #[cfg(feature = "rocksdb")]
        "rocksdb" => Database::open(dir).unwrap(),
        _ => Database::open_with(dir, &options(layout)).unwrap(),
    }
}

/// The child: apply operations forever (until killed), acknowledging each.
#[test]
fn crash_child() {
    let Ok(dir) = std::env::var(DIR_ENV) else {
        return; // not the child
    };
    if std::env::var(CHILD_ENV).is_err() {
        return;
    }
    let layout = std::env::var(LAYOUT_ENV).unwrap();
    let start: u64 = std::env::var(START_ENV).unwrap().parse().unwrap();
    let mut db = open(&layout, Path::new(&dir));
    let out = std::io::stdout();
    for (n, group) in ops(start).enumerate() {
        apply(&mut db, &group);
        let mut o = out.lock();
        writeln!(o, "ACK {n}").unwrap();
        o.flush().unwrap();
    }
}

/// The model after the first `n` groups: the triples present.
fn model(start: u64, n: usize) -> BTreeSet<u64> {
    let mut present = BTreeSet::new();
    for group in ops(start).take(n) {
        for (ins, i) in group {
            if ins {
                present.insert(i);
            } else {
                present.remove(&i);
            }
        }
    }
    present
}

fn present(db: &mut Database) -> BTreeSet<u64> {
    match db.query("SELECT ?s WHERE { ?s ?p ?o }").unwrap() {
        QueryResult::Select(rs) => rs
            .rows
            .iter()
            .filter_map(|r| r[0].as_deref())
            .filter_map(|s| {
                s.trim_start_matches("<http://ex/s")
                    .trim_end_matches('>')
                    .parse()
                    .ok()
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn live_terms(db: &Database) -> usize {
    db.entity_num() + db.literal_num() + db.predicate_num()
}

/// Terms the data references: 2 per triple plus its predicate.
fn referenced(triples: &BTreeSet<u64>) -> usize {
    let preds: BTreeSet<u64> = triples.iter().map(|i| i % 7).collect();
    triples.len() * 2 + preds.len()
}

fn crash_iterations(layout: &str, iterations: u64) {
    let tmp = TempDir::new(layout);
    let dir = tmp.0.join("db");
    // Base database.
    match layout {
        #[cfg(feature = "rocksdb")]
        "rocksdb" => {
            Database::build_rocksdb_from_str(
                "crash",
                &dir,
                "<http://ex/base> <http://ex/p0> \"b\" .\n",
            )
            .unwrap();
        }
        _ => {
            let src = tmp.0.join("base.nt");
            std::fs::write(&src, "<http://ex/base> <http://ex/p0> \"b\" .\n").unwrap();
            Database::build_disk(&dir, &[&src]).unwrap();
        }
    }
    let exe = std::env::current_exe().unwrap();
    let mut rng = Rng(0x5eed ^ layout.len() as u64);
    // Each iteration's operations use their own id range.
    let mut state: BTreeSet<u64> = BTreeSet::new();
    for it in 0..iterations {
        let start = 1_000_000 * (it + 1);
        let mut child = Command::new(&exe)
            .args(["--exact", "crash_child", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .env(DIR_ENV, &dir)
            .env(LAYOUT_ENV, layout)
            .env(START_ENV, start.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(30 + rng.below(120)));
        child.kill().unwrap(); // SIGKILL
        let out = child.stdout.take().unwrap();
        // The highest acknowledged group (libtest may print its own
        // `test crash_child ... ` on the first line, before an ACK).
        let acked = BufReader::new(out)
            .lines()
            .map_while(Result::ok)
            .filter_map(|l| {
                let at = l.find("ACK ")?;
                l[at + 4..].trim().parse::<usize>().ok()
            })
            .max()
            .map_or(0, |n| n + 1);
        child.wait().unwrap();

        let mut db = open(layout, &dir);
        db.verify_integrity()
            .unwrap_or_else(|e| panic!("{layout} iteration {it}: {e}"));
        let got: BTreeSet<u64> = present(&mut db)
            .into_iter()
            .filter(|&i| i >= start)
            .collect();
        let want = model(start, acked);
        let in_flight = model(start, acked + 1);
        assert!(
            got == want || got == in_flight,
            "{layout} iteration {it}: {acked} acknowledged, store differs from the model: \
             missing {:?}, extra {:?}; matches model after {:?} groups",
            want.difference(&got).take(10).collect::<Vec<_>>(),
            got.difference(&in_flight).take(10).collect::<Vec<_>>(),
            (0..acked + 50).find(|&k| model(start, k) == got)
        );
        // The earlier iterations' triples are untouched.
        let older: BTreeSet<u64> = present(&mut db)
            .into_iter()
            .filter(|&i| i < start)
            .collect();
        assert_eq!(older, state, "{layout} iteration {it}");
        state.extend(got.iter().copied());
        // The base triple's <base>, "b" and <p0> (which others may use too).
        let base_terms = 3;
        let refs = referenced(&state) + base_terms - usize::from(state.iter().any(|i| i % 7 == 0));
        let orphans = live_terms(&db).saturating_sub(refs);
        assert!(
            orphans <= 4,
            "{layout} iteration {it}: {orphans} orphan terms ({} live, {refs} referenced)",
            live_terms(&db)
        );
        let freed = db.reclaim_unused().unwrap();
        assert_eq!(freed, orphans, "{layout} iteration {it}");
        assert_eq!(live_terms(&db), refs, "{layout} iteration {it}");
        db.verify_integrity().unwrap();
        drop(db);
    }
}

#[test]
fn stream_disk_survives_kills() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    crash_iterations("stream", 20);
}

#[test]
fn materialize_disk_survives_kills() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    crash_iterations("materialize", 20);
}

#[cfg(feature = "rocksdb")]
#[test]
fn rocksdb_survives_kills() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    crash_iterations("rocksdb", 20);
}

/// The same operations without a crash: the store follows the model after
/// every group, and after a reopen.
#[test]
fn the_workload_without_crashes_matches_the_model() {
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    for layout in ["stream", "materialize"] {
        let tmp = TempDir::new(&format!("nocrash_{layout}"));
        let dir = tmp.0.join("db");
        let src = tmp.0.join("base.nt");
        std::fs::write(&src, "<http://ex/base> <http://ex/p0> \"b\" .\n").unwrap();
        Database::build_disk(&dir, &[&src]).unwrap();
        for it in 0..3u64 {
            let start = 1_000_000 * (it + 1);
            let mut db = open(layout, &dir);
            for (n, group) in ops(start).take(120).enumerate() {
                apply(&mut db, &group);
                let got: BTreeSet<u64> = present(&mut db)
                    .into_iter()
                    .filter(|&i| i >= start)
                    .collect();
                let want = model(start, n + 1);
                assert_eq!(got, want, "{layout} it {it} group {n}: {group:?}");
            }
            drop(db);
            let mut db = open(layout, &dir);
            let got: BTreeSet<u64> = present(&mut db)
                .into_iter()
                .filter(|&i| i >= start)
                .collect();
            assert_eq!(got, model(start, 120), "{layout} it {it} after reopen");
        }
    }
}
