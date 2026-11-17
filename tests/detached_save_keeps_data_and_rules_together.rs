//! A detached database's save writes its snapshot and its in-memory
//! reasoning rules into the destination as a pair, or neither.
//!
//! Round-4 review (durability): `Database::save` of a database detached
//! from its directory (`into_memory`) installed the snapshot first and only
//! then wrote the rules, whose write takes the destination's rule lock
//! without waiting and is refused while another operation holds it. The
//! save then reported the refusal — with the new data already installed
//! beside the directory's old rules and their ledgers, which describe
//! triples of the old data. The rule lock is now taken before anything is
//! written, so a refused save leaves the destination as it was.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::reason::store::RuleStore;
use gstore::Database;

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_detached_save_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TmpDir(fs::canonicalize(&p).unwrap())
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A snapshot database at `dir` holding `triple`, with one rule `rule` and
/// a ledger naming that triple.
fn database_with_a_rule(dir: &std::path::Path, triple: &str, rule: &str) {
    let mut db = Database::new("d");
    db.query(&format!("INSERT DATA {{ {triple} }}")).unwrap();
    db.save(dir).unwrap();
    drop(db);
    let mut store = RuleStore::dir(dir);
    assert!(store
        .create(rule, &format!("{{\"rulename\":\"{rule}\"}}"))
        .unwrap());
    let ledger = [format!("{triple} .")].into();
    store.set_ledger(rule, Some(&ledger)).unwrap();
}

fn rules(dir: &std::path::Path) -> Vec<String> {
    let store = RuleStore::dir(dir);
    let _lock = store.lock();
    store.names().unwrap()
}

#[test]
fn a_save_refused_for_its_rules_leaves_the_destination_as_it_was() {
    let base = TmpDir::new("refused");
    let src = base.0.join("src.db");
    let dst = base.0.join("dst.db");
    database_with_a_rule(&src, "<http://e/s> <http://e/p> <http://e/src>", "srcrule");
    database_with_a_rule(&dst, "<http://e/s> <http://e/p> <http://e/dst>", "oldrule");

    let mut detached = Database::open(&src).unwrap().into_memory().unwrap();
    detached
        .query("INSERT DATA { <http://e/s> <http://e/p> <http://e/more> }")
        .unwrap();
    assert_eq!(detached.triple_num(), 2);

    // Another operation of this process is changing dst's rules.
    let holder = RuleStore::dir(&dst);
    let held = holder.lock();
    let e = detached.save(&dst).unwrap_err();
    assert!(
        e.to_string()
            .contains("are being changed by another operation"),
        "{e}"
    );
    drop(held);

    // Nothing of the refused save reached dst: its own triple, its own rule.
    let reopened = Database::open(&dst).unwrap();
    assert_eq!(
        reopened.triple_num(),
        1,
        "the refused save installed its snapshot beside the old rules"
    );
    drop(reopened);
    assert_eq!(rules(&dst), ["oldrule"]);

    // Once the lock is free the save writes both.
    detached.save(&dst).unwrap();
    let reopened = Database::open(&dst).unwrap();
    assert_eq!(reopened.triple_num(), 2);
    drop(reopened);
    assert_eq!(rules(&dst), ["srcrule"]);
}
