//! A forced rebuild removes the replaced database's reasoning rules — also
//! the staged copy an interrupted rule-directory swap left behind.
//!
//! Round 4 met two changes here: r4-ext-docs stages every replacement of
//! `reason_rule_files/` and swaps it in (`reason_rule_files.new` →
//! `reason_rule_files`; review durability#11), finishing a swap a crash
//! interrupted at the next operation that takes the directory's rule lock;
//! `gbuild --force` (r4-cli-config's tool) deleted only
//! `reason_rule_files/`, without that lock. A crash after the swap moved
//! the old directory aside leaves just the complete `.new`, which the
//! rebuild left in place — and the rebuilt database's first rule operation
//! then installed it: the replaced database's rules, with ledgers naming
//! triples of the old data, came back. Every path that replaces a database
//! (gbuild, the bulk install, REST rebuild) now removes the rules through
//! `gstore::reason::store::remove_rule_dir[_locked]`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::reason::rules::RULE_DIR;
use gstore::reason::store::RuleStore;

const OLD: &str = "<http://e/s> <http://e/p> <http://e/old> .\n";
const NEW: &str = "<http://e/s> <http://e/p> <http://e/new> .\n";

/// A temp directory (canonical path) removed when dropped, also when the
/// test fails.
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_rule_dir_replacement_{tag}_{}_{n}",
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

fn gbuild(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gbuild"))
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The rules a database directory shows to a rule operation (read under
/// its rule lock, which finishes an interrupted swap first).
fn shown(db_dir: &Path) -> Vec<String> {
    let store = RuleStore::dir(db_dir);
    let _lock = store.lock();
    store.names().unwrap()
}

/// Build `name` from `old.nt` with `layout`, give it a rule with its
/// ledger, leave the state a crash inside a rule swap leaves (the old
/// directory gone, only the complete `reason_rule_files.new`), and rebuild
/// it with `--force` from `new.nt`: the rebuilt database has no rules.
fn rebuild_after_an_interrupted_rule_swap(layout: &[&str], name: &str) {
    let base = TmpDir::new(name);
    fs::write(base.0.join("old.nt"), OLD).unwrap();
    fs::write(base.0.join("new.nt"), NEW).unwrap();
    let mut args: Vec<&str> = layout.to_vec();
    args.extend([name, "old.nt"]);
    let out = gbuild(&args, &base.0);
    assert!(out.status.success(), "{}", text(&out));
    let db = base.0.join(format!("{name}.db"));
    assert!(db.is_dir(), "{}", text(&out));

    let mut store = RuleStore::dir(&db);
    assert!(store.create("r", "{\"rulename\":\"r\"}").unwrap());
    let ledger = ["<http://e/s> <http://e/p> <http://e/old> .".to_string()].into();
    store.set_ledger("r", Some(&ledger)).unwrap();
    assert_eq!(shown(&db), ["r"]);
    let rules = db.join(RULE_DIR);
    let staged = db.join(format!("{RULE_DIR}.new"));
    fs::rename(&rules, &staged).unwrap();

    let mut args: Vec<&str> = vec!["--force"];
    args.extend(layout);
    args.extend([name, "new.nt"]);
    let out = gbuild(&args, &base.0);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        !staged.exists(),
        "the replaced database's staged rules were left behind: {}",
        text(&out)
    );
    assert_eq!(
        shown(&db),
        Vec::<String>::new(),
        "the replaced database's rules came back"
    );
}

#[test]
fn a_forced_snapshot_rebuild_removes_an_interrupted_rule_swap() {
    rebuild_after_an_interrupted_rule_swap(&[], "snap");
}

/// The bulk install removed the rules under the rule lock already (which
/// finished the swap first); kept as a guard for the shared helper.
#[test]
fn a_forced_bulk_rebuild_removes_an_interrupted_rule_swap() {
    rebuild_after_an_interrupted_rule_swap(&["--disk"], "bulk");
}
