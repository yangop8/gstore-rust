//! The layout-matrix guard (reviews db#6, design#5, gap-doc-claims#7).
//!
//! A database's layout is not one type: it is a `Backend` variant plus a
//! Materialize write-through sink, a dictionary backing and a VS-tree kind,
//! and the [`Database`] facade dispatches on it at about seventy places in
//! `db/mod.rs`, `persist.rs` and `reclaim.rs` (persistence, journaling,
//! transactions, reclamation, inference). A missed arm fails silently — an
//! `if let Some(..) = self.store.as_disk()` simply does not fire — and a new
//! layout has no exhaustiveness check. Until that dispatch is redesigned
//! (one `Layout` enum / layout driver; recorded for docs/REFACTOR_BACKLOG.md
//! with these review ids), this guard runs every public write and
//! maintenance operation of [`Database`] on every layout and requires the
//! same observable results:
//!
//! * the layouts are every [`StorageKind`] — with both [`DiskQueryMode`]s for
//!   the disk layout, and RocksDB under the `rocksdb` feature — listed
//!   through exhaustive matches, so a new `StorageKind` or `DiskQueryMode`
//!   variant does not compile until it joins the matrix;
//! * the operations: `INSERT DATA` / `DELETE DATA` (default and named
//!   graphs), `DELETE`/`INSERT … WHERE`, `LOAD` (into the default and a named
//!   graph), `CLEAR` / `DROP` / `COPY` / `MOVE` / `ADD`, the direct API
//!   (`insert_triple`, `remove_triple`, `insert_data`, `delete_data`),
//!   transactions (`begin` / `commit` / `rollback`), `save` (into another
//!   directory) / `checkpoint` / `sync`, `backup` / `restore` /
//!   `restore_into`, `reclaim_unused`, and inference (`materialize_rdfs`,
//!   `run_rules`, the upstream `reason_*` rule lifecycle);
//! * after every step: the step's own result (counts, success), the full
//!   content (default graph and every named graph, through SPARQL),
//!   `triple_num` and `verify_integrity` must equal the snapshot layout's;
//!   and the step must be durable — after a `checkpoint` the database is
//!   dropped and reopened from its directory, and the reopened content must
//!   equal the live one (a missed persistence arm loses data here).
//!
//! The check was proven sensitive by removing one dispatch arm at a time
//! locally: dropping the write-through of a cleared named graph
//! (`persist_cleared_graph`'s `persist_mutation`) fails it for
//! `Disk(Materialize)` at "copy named to named"; dropping the Stream arm of
//! `persist_mutation` (`named_op`) fails it for `Disk(Stream)` at the first
//! named-graph insert; dropping the RocksDB arm of `rocks_rollback_journal`
//! fails it for `Rocks` at "transaction rolled back".

use std::fs;
use std::path::{Path, PathBuf};

use super::{Database, DiskQueryMode, OpenOptions, StorageKind};
use crate::model::{Term, Triple};
use crate::query::QueryResult;

/// Every [`StorageKind`]. Exhaustive on purpose: a new variant fails to
/// compile in [`kind_index`] until it is listed here and given a row in
/// [`layouts`].
const ALL_KINDS: [StorageKind; 3] = [
    StorageKind::Snapshot,
    StorageKind::Disk,
    StorageKind::RocksDb,
];

/// Every [`DiskQueryMode`] (see [`ALL_KINDS`]).
const ALL_DISK_MODES: [DiskQueryMode; 2] = [DiskQueryMode::Materialize, DiskQueryMode::Stream];

const fn kind_index(kind: StorageKind) -> usize {
    match kind {
        StorageKind::Snapshot => 0,
        StorageKind::Disk => 1,
        StorageKind::RocksDb => 2,
    }
}

const fn mode_index(mode: DiskQueryMode) -> usize {
    match mode {
        DiskQueryMode::Materialize => 0,
        DiskQueryMode::Stream => 1,
    }
}

/// One row of the matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    Snapshot,
    Disk(DiskQueryMode),
    #[cfg(feature = "rocksdb")]
    Rocks,
}

impl Layout {
    fn options(self) -> OpenOptions {
        match self {
            Layout::Disk(mode) => OpenOptions::new().with_disk_query_mode(mode),
            _ => OpenOptions::new(),
        }
    }

    fn kind(self) -> StorageKind {
        match self {
            Layout::Snapshot => StorageKind::Snapshot,
            Layout::Disk(_) => StorageKind::Disk,
            #[cfg(feature = "rocksdb")]
            Layout::Rocks => StorageKind::RocksDb,
        }
    }
}

/// The rows: every kind, every disk mode, RocksDB when compiled in.
fn layouts() -> Vec<Layout> {
    let mut rows = Vec::new();
    for (i, kind) in ALL_KINDS.into_iter().enumerate() {
        assert_eq!(kind_index(kind), i, "ALL_KINDS lists every kind once");
        match kind {
            StorageKind::Snapshot => rows.push(Layout::Snapshot),
            StorageKind::Disk => {
                for (j, mode) in ALL_DISK_MODES.into_iter().enumerate() {
                    assert_eq!(mode_index(mode), j, "ALL_DISK_MODES lists every mode once");
                    rows.push(Layout::Disk(mode));
                }
            }
            StorageKind::RocksDb => {
                #[cfg(feature = "rocksdb")]
                rows.push(Layout::Rocks);
            }
        }
    }
    rows
}

/// A temp directory removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_layout_matrix_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const BASE: &str = "\
<http://ex/s0> <http://ex/p> <http://ex/o0> .
<http://ex/s0> <http://ex/name> \"zero\" .
<http://ex/s1> <http://ex/p> <http://ex/o1> .
<http://ex/s2> <http://ex/p> <http://ex/o2> .
<http://ex/s2> <http://ex/age> \"42\"^^<http://www.w3.org/2001/XMLSchema#integer> .
<http://ex/Dog> <http://www.w3.org/2000/01/rdf-schema#subClassOf> <http://ex/Animal> .
<http://ex/rex> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/Dog> .
";

const EXTRA: &str = "\
<http://ex/l1> <http://ex/p> <http://ex/m1> .
<http://ex/l2> <http://ex/q> \"loaded\"@en .
";

const PARENT_RULE: &str = r#"{"rulename":"R","type":1,"logic":1,
    "conditions":[{"patterns":[{"subject":"?x","predicate":"<http://ex/p>","object":"?y"}]}],
    "return":{"source":"?y","target":"?x","label":"R"}}"#;

/// Everything a step can observe of a database.
#[derive(Debug, Clone, PartialEq)]
struct State {
    default: Vec<Vec<Option<String>>>,
    named: Vec<Vec<Option<String>>>,
    triple_num: u64,
    integrity: std::result::Result<(), String>,
}

fn rows(db: &mut Database, sparql: &str) -> Vec<Vec<Option<String>>> {
    let mut r = db
        .select(sparql)
        .unwrap_or_else(|e| panic!("{sparql}: {e}"))
        .rows;
    r.sort();
    r
}

fn state(db: &mut Database) -> State {
    State {
        default: rows(db, "SELECT ?s ?p ?o WHERE { ?s ?p ?o }"),
        named: rows(db, "SELECT ?g ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } }"),
        triple_num: db.triple_num(),
        integrity: db.verify_integrity().map_err(|e| e.to_string()),
    }
}

/// A step's own result, comparable across layouts.
fn outcome(r: crate::Result<QueryResult>) -> String {
    match r {
        Ok(QueryResult::Update { changed }) => format!("changed {changed}"),
        Ok(other) => format!("{other:?}"),
        Err(e) => format!("error: {e}"),
    }
}

fn iri(s: &str) -> Term {
    Term::iri(format!("http://ex/{s}"))
}

/// What a step gets: the database (replaced when a step reopens it), its
/// layout and directory, and a scratch directory for files and copies.
struct Ctx<'a> {
    layout: Layout,
    home: &'a Path,
    scratch: &'a Path,
}

type Step = (&'static str, fn(&mut Database, &Ctx) -> String);

fn update(db: &mut Database, sparql: &str) -> String {
    outcome(db.query(sparql))
}

/// The script every layout runs.
fn steps() -> Vec<Step> {
    vec![
        ("insert data, default and named", |db, _| {
            update(
                db,
                "INSERT DATA { <http://ex/s3> <http://ex/p> <http://ex/o3> . \
                 <http://ex/s3> <http://ex/name> \"three\"@en . \
                 GRAPH <http://ex/g1> { <http://ex/a> <http://ex/p> <http://ex/b> . \
                 <http://ex/a> <http://ex/q> \"x\" . <http://ex/c> <http://ex/p> <http://ex/a> } }",
            )
        }),
        ("delete data, default and named", |db, _| {
            update(
                db,
                "DELETE DATA { <http://ex/s1> <http://ex/p> <http://ex/o1> . \
                 GRAPH <http://ex/g1> { <http://ex/a> <http://ex/q> \"x\" } }",
            )
        }),
        ("delete/insert where", |db, _| {
            update(
                db,
                "DELETE { ?s <http://ex/p> ?o } INSERT { ?s <http://ex/p2> ?o . \
                 GRAPH <http://ex/g2> { ?s <http://ex/p> ?o } } \
                 WHERE { ?s <http://ex/p> ?o FILTER(?s = <http://ex/s2>) }",
            )
        }),
        ("insert where into a named graph", |db, _| {
            update(
                db,
                "INSERT { GRAPH <http://ex/g3> { ?s <http://ex/name> ?n } } \
                 WHERE { ?s <http://ex/name> ?n }",
            )
        }),
        ("load into the default graph", |db, ctx| {
            let file = ctx.scratch.join("extra.nt");
            fs::write(&file, EXTRA).unwrap();
            update(db, &format!("LOAD <file://{}>", file.display()))
        }),
        ("load into a named graph", |db, ctx| {
            let file = ctx.scratch.join("extra.nt");
            fs::write(&file, EXTRA).unwrap();
            update(
                db,
                &format!("LOAD <file://{}> INTO GRAPH <http://ex/g4>", file.display()),
            )
        }),
        ("add", |db, _| {
            update(db, "ADD <http://ex/g1> TO <http://ex/g5>")
        }),
        ("copy named to named", |db, _| {
            update(db, "COPY <http://ex/g4> TO <http://ex/g3>")
        }),
        ("move named to named", |db, _| {
            update(db, "MOVE <http://ex/g5> TO <http://ex/g6>")
        }),
        ("copy default to named", |db, _| {
            update(db, "COPY DEFAULT TO <http://ex/g7>")
        }),
        ("add named to default", |db, _| {
            update(db, "ADD <http://ex/g6> TO DEFAULT")
        }),
        ("clear graph", |db, _| {
            update(db, "CLEAR GRAPH <http://ex/g3>")
        }),
        ("drop graph", |db, _| {
            update(db, "DROP GRAPH <http://ex/g4>")
        }),
        ("clear default", |db, _| update(db, "CLEAR DEFAULT")),
        ("move named to default", |db, _| {
            update(db, "MOVE <http://ex/g7> TO DEFAULT")
        }),
        ("clear named", |db, _| update(db, "CLEAR NAMED")),
        ("insert data again", |db, _| {
            update(
                db,
                "INSERT DATA { GRAPH <http://ex/g8> { <http://ex/a> <http://ex/p> <http://ex/b> } \
                 GRAPH <http://ex/g9> { <http://ex/b> <http://ex/p> <http://ex/c> } }",
            )
        }),
        ("drop all", |db, _| {
            let dropped = update(db, "DROP ALL");
            let back = update(
                db,
                &format!(
                    "INSERT DATA {{ {} GRAPH <http://ex/g1> {{ <http://ex/a> <http://ex/p> <http://ex/b> }} }}",
                    BASE
                ),
            );
            format!("{dropped}; {back}")
        }),
        ("direct api", |db, _| {
            let t1 = Triple::new(iri("d1"), iri("p"), iri("e1"));
            let t2 = Triple::new(iri("d2"), iri("p"), Term::plain_literal("direct"));
            let a = db.try_insert_triple(&t1).map_err(|e| e.to_string());
            let b = db.insert_triple(&t2);
            let c = db.insert_triple(&t2);
            let d = db.remove_triple(&Triple::new(iri("s0"), iri("p"), iri("o0")));
            let e = db.remove_triple(&Triple::new(iri("nope"), iri("p"), iri("o0")));
            let sync = db.sync().map_err(|e| e.to_string());
            format!("{a:?} {b} {c} {d} {e} {sync:?}")
        }),
        ("transaction committed", |db, _| {
            let begin = db.begin().map_err(|e| e.to_string());
            let a = update(
                db,
                "INSERT DATA { <http://ex/t1> <http://ex/p> <http://ex/u1> . \
                 GRAPH <http://ex/gt> { <http://ex/t1> <http://ex/p> <http://ex/u1> } }",
            );
            let b = update(
                db,
                "DELETE DATA { <http://ex/s2> <http://ex/name> \"zero\" }",
            );
            let c = db.insert_triple(&Triple::new(iri("t2"), iri("p"), iri("u2")));
            let d = db.remove_triple(&Triple::new(
                iri("s0"),
                iri("name"),
                Term::plain_literal("zero"),
            ));
            let inside = db.in_transaction();
            let commit = db.commit().map_err(|e| e.to_string());
            format!("{begin:?} {a} {b} {c} {d} {inside} {commit:?}")
        }),
        ("transaction rolled back", |db, _| {
            let before = state(db);
            let begin = db.begin().map_err(|e| e.to_string());
            let a = update(
                db,
                "INSERT DATA { <http://ex/r1> <http://ex/p> <http://ex/v1> . \
                 GRAPH <http://ex/gr> { <http://ex/r1> <http://ex/p> <http://ex/v1> } }",
            );
            let b = update(db, "DELETE WHERE { <http://ex/t1> ?p ?o }");
            let c = update(db, "CLEAR GRAPH <http://ex/gt>");
            let d = db.insert_triple(&Triple::new(
                iri("r2"),
                iri("p"),
                Term::plain_literal("gone"),
            ));
            let rollback = db.rollback().map_err(|e| e.to_string());
            let same = state(db) == before;
            format!("{begin:?} {a} {b} {c} {d} {rollback:?} restored={same}")
        }),
        ("insert_data / delete_data", |db, _| {
            use crate::parser::sparql::ast::GroundTriple;
            let gt = |s: &str, o: &str| GroundTriple {
                subject: iri(s),
                predicate: iri("p"),
                object: iri(o),
                graph: None,
            };
            let a = db.insert_data(&[gt("i1", "j1"), gt("i2", "j2")]);
            let b = db.delete_data(&[gt("i1", "j1"), gt("nope", "j1")]);
            format!("{a} {b}")
        }),
        ("save, checkpoint, sync", |db, ctx| {
            let copy = ctx.scratch.join("saved_copy");
            let saved = db.save(&copy).map_err(|e| e.to_string());
            let checkpoint = db.checkpoint().map_err(|e| e.to_string());
            let sync = db.sync().map_err(|e| e.to_string());
            let live = state(db);
            let copied = match Database::open_with(&copy, &ctx.layout.options()) {
                Ok(mut c) => (c.storage() == ctx.layout.kind(), state(&mut c) == live),
                Err(e) => panic!("{:?}: open the saved copy: {e}", ctx.layout),
            };
            format!("{saved:?} {checkpoint:?} {sync:?} copy(kind, same)={copied:?}")
        }),
        ("backup, restore, restore_into", |db, ctx| {
            let backup = ctx.scratch.join("backup");
            let target = ctx.scratch.join("restored");
            let b = db.backup(&backup).map_err(|e| e.to_string());
            let live = state(db);
            let restored = {
                let mut r = Database::restore(&backup).unwrap();
                state(&mut r) == live
            };
            let into = Database::restore_into(&backup, &target).map_err(|e| e.to_string());
            let opened = {
                let mut r = Database::open_with(&target, &ctx.layout.options()).unwrap();
                (r.storage() == ctx.layout.kind(), state(&mut r) == live)
            };
            format!("{b:?} restore={restored} {into:?} restore_into(kind, same)={opened:?}")
        }),
        ("reclaim unused ids", |db, _| {
            let a = update(
                db,
                "DELETE WHERE { <http://ex/d1> ?p ?o } ; \
                 DELETE DATA { <http://ex/d2> <http://ex/p> \"direct\" }",
            );
            let n = db.reclaim_unused().map_err(|e| e.to_string());
            let again = db.reclaim_unused().map_err(|e| e.to_string());
            format!("{a} first={n:?} second={again:?}")
        }),
        ("materialize rdfs", |db, _| {
            let n = db.materialize_rdfs();
            let again = db.materialize_rdfs();
            let sync = db.sync().map_err(|e| e.to_string());
            format!("{n} {again} {sync:?}")
        }),
        ("run user rules", |db, _| {
            let add = db
                .add_rule("inv", "?x <http://ex/p> ?y => ?y <http://ex/inv> ?x")
                .map_err(|e| e.to_string());
            let n = db.run_rules();
            let again = db.run_rules();
            let count = db.rule_effect_count("inv");
            let sync = db.sync().map_err(|e| e.to_string());
            format!("{add:?} {n} {again} {count:?} {sync:?}")
        }),
        ("upstream rule lifecycle", |db, _| {
            let rule = crate::json::parse(PARENT_RULE).unwrap();
            let add = db.reason_add(&rule).map(|_| ()).map_err(|e| e.to_string());
            let compile = db
                .reason_compile("R")
                .map(|_| ())
                .map_err(|e| e.to_string());
            let exec = db
                .reason_execute("R")
                .map(|x| (x.inserted, x.owned))
                .map_err(|e| e.to_string());
            let mid = rows(db, "SELECT ?s ?o WHERE { ?s <Rule:R> ?o }").len();
            let disable = db
                .reason_disable("R")
                .map(|d| d.removed)
                .map_err(|e| e.to_string());
            let after = rows(db, "SELECT ?s ?o WHERE { ?s <Rule:R> ?o }").len();
            let delete = db.reason_delete("R").map(|_| ()).map_err(|e| e.to_string());
            format!("{add:?} {compile:?} {exec:?} {mid} {disable:?} {after} {delete:?}")
        }),
    ]
}

/// Build the base database in `home` as `layout` and open it.
fn create(layout: Layout, home: &Path, scratch: &Path) -> Database {
    let nt = scratch.join("base.nt");
    fs::write(&nt, BASE).unwrap();
    match layout {
        Layout::Snapshot => {
            let db = Database::build_from_str("matrix", BASE).unwrap();
            db.save(home).unwrap();
        }
        Layout::Disk(_) => Database::build_disk(home, &[&nt]).unwrap(),
        #[cfg(feature = "rocksdb")]
        Layout::Rocks => drop(Database::build_rocksdb_from_str("matrix", home, BASE).unwrap()),
    }
    reopen(layout, home)
}

fn reopen(layout: Layout, home: &Path) -> Database {
    let db = Database::open_with(home, &layout.options())
        .unwrap_or_else(|e| panic!("{layout:?}: open {}: {e}", home.display()));
    assert_eq!(db.storage(), layout.kind(), "{layout:?}");
    if let Layout::Disk(mode) = layout {
        assert_eq!(db.disk_query_mode(), Some(mode));
    }
    db
}

/// One layout's run: per step, (name, outcome, live state).
type Trace = Vec<(&'static str, String, State)>;

fn run(layout: Layout) -> Trace {
    let tmp = TmpDir::new(&format!("{layout:?}").replace(['(', ')'], "_"));
    let home = tmp.0.join("db");
    let scratch = tmp.0.join("scratch");
    fs::create_dir_all(&scratch).unwrap();
    let mut db = create(layout, &home, &scratch);
    let mut trace = vec![("open", String::new(), state(&mut db))];
    for (i, (name, step)) in steps().into_iter().enumerate() {
        let step_scratch = scratch.join(format!("step{i}"));
        fs::create_dir_all(&step_scratch).unwrap();
        let ctx = Ctx {
            layout,
            home: &home,
            scratch: &step_scratch,
        };
        let result = step(&mut db, &ctx);
        let live = state(&mut db);
        // Durable: a checkpoint, then a reopen from the directory, gives the
        // same content.
        db.checkpoint()
            .unwrap_or_else(|e| panic!("{layout:?} after '{name}': checkpoint: {e}"));
        drop(db);
        db = reopen(layout, ctx.home);
        let reopened = state(&mut db);
        assert_eq!(
            reopened, live,
            "{layout:?}: '{name}' did not persist (reopened vs live)"
        );
        trace.push((name, result, live));
    }
    trace
}

#[test]
fn every_write_and_maintenance_operation_behaves_alike_on_every_layout() {
    let rows = layouts();
    assert!(rows.len() >= 3, "snapshot and both disk modes at least");
    let reference = run(Layout::Snapshot);
    for (name, result, st) in &reference {
        assert_eq!(st.integrity, Ok(()), "snapshot after '{name}' ({result})");
        assert!(
            !result.contains("error"),
            "snapshot: '{name}' failed: {result}"
        );
    }
    for layout in rows.into_iter().filter(|l| *l != Layout::Snapshot) {
        let trace = run(layout);
        assert_eq!(trace.len(), reference.len());
        for ((name, want, want_state), (_, got, got_state)) in reference.iter().zip(&trace) {
            assert_eq!(got, want, "{layout:?}: result of '{name}'");
            assert_eq!(
                got_state, want_state,
                "{layout:?}: state after '{name}' differs from the snapshot layout's"
            );
        }
    }
}
