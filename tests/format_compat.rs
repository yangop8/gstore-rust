//! Cross-version format compatibility (review gap-doc-claims#8).
//!
//! `testdata/compat/eaf5320/` holds two small databases written by the
//! **binaries of the pre-Phase-6 commit `eaf5320`** (`gbuild`, then `gquery`
//! for a named-graph `INSERT DATA` and a `DELETE DATA`), not by this code:
//! a snapshot (`snapshot/`) and an on-disk version-1 `kvstore.kv` with its
//! `vstree.kv` (`disk-v1/`). `scripts/compat-fixtures.sh` regenerates them
//! (`--generate`) and checks the other directions of the compatibility
//! matrix against the old binaries (they refuse a version-2 `kvstore.kv`,
//! read the files this code writes in version 1, and fail on the redo-log
//! records they predate); `testdata/compat/README.md` records how.
//!
//! These tests open copies of the fixtures with the current code, in every
//! mode, and check the answers (hand-written from `testdata/compat/data.nt`,
//! `named.ru` and `delete.ru`), that writes work and that a version-1
//! `kvstore.kv` stays version 1 (docs/STORAGE_BACKEND.md: old files are never
//! upgraded implicitly). `GSTORE_COMPAT_FIXTURES=<dir>` points them at
//! another fixture directory (the script's freshly generated one).
//!
//! The ignored `export_a_clear_increment_for_old_binaries` writes, into
//! `GSTORE_COMPAT_EXPORT`, a full backup and an increment holding a `CLEAR`
//! record, for the script to restore with the old library.

use std::fs;
use std::path::{Path, PathBuf};

use gstore::query::{scoped_vs_policy, VsPolicy};
use gstore::signature::{Signature, VsTree};
use gstore::{Database, DiskQueryMode, OpenOptions, QueryResult};

/// A temporary directory, removed on drop (also when an assertion fails).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        let d =
            std::env::temp_dir().join(format!("gstore_format_compat_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The fixture directory: `GSTORE_COMPAT_FIXTURES`, else the checked-in one.
fn fixtures() -> PathBuf {
    match std::env::var_os("GSTORE_COMPAT_FIXTURES") {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/compat/eaf5320"),
    }
}

/// Copy the fixture database `name` into `tmp` (the fixture is never
/// opened in place: opening may write) and return the copy.
fn copy_fixture(tmp: &TempDir, name: &str) -> PathBuf {
    let src = fixtures().join(name);
    assert!(
        src.is_dir(),
        "missing fixture {} (run scripts/compat-fixtures.sh --generate)",
        src.display()
    );
    let dst = tmp.0.join(name);
    fs::create_dir_all(&dst).unwrap();
    for e in fs::read_dir(&src).unwrap() {
        let e = e.unwrap();
        assert!(e.file_type().unwrap().is_file(), "{:?}", e.path());
        fs::copy(e.path(), dst.join(e.file_name())).unwrap();
    }
    dst
}

const EX: &str = "PREFIX ex: <http://example.org/> \
                  PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> ";

fn int(n: i64) -> String {
    format!("\"{n}\"^^<http://www.w3.org/2001/XMLSchema#integer>")
}

fn iri(local: &str) -> String {
    format!("<http://example.org/{local}>")
}

/// The rows of a SELECT, every cell bound.
fn select(db: &mut Database, q: &str) -> Vec<Vec<String>> {
    match db.query(&format!("{EX}{q}")) {
        Ok(QueryResult::Select(rs)) => rs
            .rows
            .into_iter()
            .map(|r| r.into_iter().map(|c| c.expect("bound")).collect())
            .collect(),
        other => panic!("{q}: {other:?}"),
    }
}

fn ask(db: &mut Database, q: &str) -> bool {
    match db.query(&format!("{EX}{q}")) {
        Ok(QueryResult::Ask(b)) => b,
        other => panic!("{q}: {other:?}"),
    }
}

/// The answers the eaf5320-written data must give (`data.nt` + `named.ru`
/// − `delete.ru`), plus `extra` people (added by a write test).
fn check_answers(db: &mut Database, what: &str, extra: &[&str]) {
    let people: Vec<Vec<String>> = ["alice", "bob", "carol"]
        .iter()
        .chain(extra)
        .map(|p| vec![iri(p)])
        .collect();
    assert_eq!(
        select(db, "SELECT ?p WHERE { ?p a ex:Person } ORDER BY ?p"),
        people,
        "{what}"
    );
    assert_eq!(
        select(db, "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }"),
        vec![vec![int(23 + extra.len() as i64)]],
        "{what}"
    );
    assert_eq!(
        select(
            db,
            "SELECT ?g (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g ORDER BY ?g"
        ),
        vec![
            vec![iri("graph/contacts"), int(2)],
            vec![iri("graph/hr"), int(3)]
        ],
        "{what}"
    );
    assert_eq!(
        select(
            db,
            "SELECT (SUM(?v) AS ?t) WHERE { GRAPH <http://example.org/graph/hr> { ?s ex:salary ?v } }"
        ),
        vec![vec![int(9200)]],
        "{what}"
    );
    assert_eq!(
        select(
            db,
            "SELECT ?x ?y WHERE { ?x ex:knows ?y . ?y a ex:Person } ORDER BY ?x ?y"
        ),
        vec![
            vec![iri("alice"), iri("bob")],
            vec![iri("alice"), iri("carol")],
            vec![iri("bob"), iri("carol")],
        ],
        "{what}"
    );
    assert_eq!(
        select(
            db,
            "SELECT ?s WHERE { ?s ex:age ?a FILTER(?a > 30) } ORDER BY ?s"
        ),
        vec![vec![iri("alice")], vec![iri("carol")]],
        "{what}"
    );
    assert_eq!(
        select(
            db,
            "SELECT ?n WHERE { ex:carol ex:knows ?b . ?b ex:name ?n }"
        ),
        vec![vec!["\"Anonymous\"".to_string()]],
        "{what}: the blank node"
    );
    assert_eq!(
        select(
            db,
            "SELECT ?c WHERE { ?p ex:worksFor ?c . ?p ex:name \"Alice\" }"
        ),
        vec![vec![iri("acme")]],
        "{what}"
    );
    for q in [
        "ASK { ex:carol ex:name ?n \
         FILTER(?n = \"Carol \\\"C\\\" O'Neil\\nline two \\\\ tab\\t\\u00e9\") }",
        "ASK { ex:acme ex:name \"ACME \u{682a}\u{5f0f}\u{4f1a}\u{793e}\"@ja }",
        "ASK { ex:alice ex:name \"Alicia\"@es }",
        "ASK { ex:bob ex:age ?a FILTER(?a = 27 && datatype(?a) = xsd:integer) }",
        "ASK { ex:alice ex:height ?h FILTER(?h = 1.68 && datatype(?h) = xsd:decimal) }",
        "ASK { ex:carol ex:active true }",
        "ASK { ex:alice ex:born \"1990-04-01\"^^xsd:date }",
        "ASK { ex:acme ex:note ?n \
         FILTER(STRLEN(?n) > 300 && STRSTARTS(?n, \"A long note: Lorem\")) }",
        "ASK { GRAPH <http://example.org/graph/contacts> { ex:bob ex:email <mailto:bob@example.org> } }",
    ] {
        assert!(ask(db, q), "{what}: {q}");
    }
    // The triple delete.ru removed stays removed.
    assert!(!ask(db, "ASK { ex:bob ex:score ?x }"), "{what}");
}

/// [`check_answers`] under every VS-tree policy (the tree the old binary
/// wrote is used by `Tree`; `Off` ignores it).
fn check_every_policy(db: &mut Database, what: &str, extra: &[&str]) {
    for policy in [VsPolicy::Tree, VsPolicy::Auto, VsPolicy::Off] {
        let _p = scoped_vs_policy(policy);
        check_answers(db, &format!("{what} {policy:?}"), extra);
    }
}

/// The writes the tests make: one person in (after an insert, a delete
/// and a reinsert), one triple in and out again.
const WRITES: [&str; 4] = [
    "INSERT DATA { ex:dave a ex:Person }",
    "DELETE DATA { ex:dave a ex:Person }",
    "INSERT DATA { ex:dave a ex:Person . ex:dave ex:name \"Dave\" }",
    "DELETE DATA { ex:dave ex:name \"Dave\" }",
];

fn write(db: &mut Database) {
    for w in WRITES {
        db.query(&format!("{EX}{w}"))
            .unwrap_or_else(|e| panic!("{w}: {e}"));
    }
}

#[test]
fn a_snapshot_written_by_eaf5320_opens_answers_and_takes_writes() {
    let tmp = TempDir::new("snap");
    let dir = copy_fixture(&tmp, "snapshot");
    {
        let mut db = Database::open(&dir).unwrap();
        assert!(db.stats().index_valid, "the old vstree.bin is in use");
        check_every_policy(&mut db, "snapshot", &[]);
        write(&mut db);
        check_answers(&mut db, "snapshot written", &["dave"]);
        db.save(&dir).unwrap();
    }
    let mut db = Database::open(&dir).unwrap();
    check_every_policy(&mut db, "snapshot reloaded", &["dave"]);
}

fn open_mode(dir: &Path, mode: DiskQueryMode) -> Database {
    let db = Database::open_with(dir, &OpenOptions::new().with_disk_query_mode(mode)).unwrap();
    assert_eq!(db.disk_query_mode(), Some(mode));
    db
}

/// The first 8 bytes of `kvstore.kv`: its version marker.
fn kv_magic(dir: &Path) -> Vec<u8> {
    fs::read(dir.join("kvstore.kv")).unwrap()[..8].to_vec()
}

#[test]
fn a_disk_database_written_by_eaf5320_opens_in_both_modes_and_stays_version_1() {
    let tmp = TempDir::new("disk");
    let dir = copy_fixture(&tmp, "disk-v1");
    assert_eq!(kv_magic(&dir), b"GSTOREKV", "the fixture is version 1");
    // The old binary's vstree.kv passes today's validation (review disk#4):
    // a scan of every node answers every entity. (A tree refused as corrupt
    // would only filter nothing, which the answers below cannot tell.)
    {
        let tree = VsTree::open_disk(dir.join("vstree.kv")).unwrap();
        assert!(tree.entity_count() > 0);
        assert_eq!(
            tree.candidates(&Signature::new()).map(|c| c.len()),
            Some(tree.entity_count()),
            "the eaf5320 vstree.kv validates"
        );
    }
    for mode in [DiskQueryMode::Stream, DiskQueryMode::Materialize] {
        let mut db = open_mode(&dir, mode);
        assert!(
            db.stats().index_valid,
            "{mode:?}: the old vstree.kv is in use"
        );
        check_every_policy(&mut db, &format!("disk {mode:?}"), &[]);
        // Id reclamation stays off on a pre-Phase-6 file until `gcompact
        // --enable-reclaim` upgrades it.
        assert!(!db.id_space_stats().enabled, "{mode:?}");
        assert!(db.reclaim_unused().is_err(), "{mode:?}");
    }
    // Writes, then a reopen in each mode.
    {
        let mut db = open_mode(&dir, DiskQueryMode::Stream);
        write(&mut db);
        check_answers(&mut db, "disk written", &["dave"]);
    }
    for mode in [DiskQueryMode::Stream, DiskQueryMode::Materialize] {
        let mut db = open_mode(&dir, mode);
        check_answers(&mut db, &format!("disk {mode:?} reopened"), &["dave"]);
        if !db.stats().index_valid {
            db.rebuild_index();
        }
        check_every_policy(&mut db, &format!("disk {mode:?} reindexed"), &["dave"]);
    }
    assert_eq!(
        kv_magic(&dir),
        b"GSTOREKV",
        "a version-1 kvstore.kv is never upgraded implicitly"
    );
}

/// Run by `scripts/compat-fixtures.sh`: a full backup and an increment
/// whose redo records hold a `CLEAR DEFAULT`, in `$GSTORE_COMPAT_EXPORT`,
/// for the script to restore with eaf5320's `restore_incremental` (which
/// must fail with "unknown redo.log op 'C'", docs/STORAGE_BACKEND.md).
#[test]
#[ignore = "run by scripts/compat-fixtures.sh with GSTORE_COMPAT_EXPORT set"]
fn export_a_clear_increment_for_old_binaries() {
    let out = PathBuf::from(
        std::env::var_os("GSTORE_COMPAT_EXPORT").expect("set GSTORE_COMPAT_EXPORT to a directory"),
    );
    let (base, inc) = (out.join("base"), out.join("inc1"));
    for d in [&base, &inc] {
        assert!(!d.exists(), "{} exists", d.display());
    }
    let mut db = Database::new("compat");
    db.enable_redo_log(out.join("redo.log"));
    db.query(
        "INSERT DATA { <http://example.org/a> <http://example.org/p> <http://example.org/b> }",
    )
    .unwrap();
    db.backup(&base).unwrap();
    db.query("CLEAR DEFAULT").unwrap();
    db.query(
        "INSERT DATA { <http://example.org/c> <http://example.org/p> <http://example.org/d> }",
    )
    .unwrap();
    db.backup_incremental(&inc).unwrap();
    // This version restores it: the cleared triple is gone.
    let mut r = Database::restore_incremental(&base, &[&inc]).unwrap();
    assert!(!ask(&mut r, "ASK { ex:a ex:p ex:b }"));
    assert!(ask(&mut r, "ASK { ex:c ex:p ex:d }"));
}
