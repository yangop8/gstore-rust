//! Data test: the bulk build of the RocksDB layout (`gstore::bulk` with
//! `BulkTarget::RocksDb`, `gbuild --rocksdb`) against the one-write-per-
//! triple build (`Database::build_rocksdb_from_sources`) and the in-memory
//! build — every column family byte-equal, the same reads in the same order,
//! exact statistics after later updates, the old database intact after a
//! failed build or a crash before the commit point, and the CLI.
//!
//! With the feature off this file compiles to nothing.

#![cfg(feature = "rocksdb")]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use gstore::bulk::{self, BulkInput, BulkOptions, BulkReport, BulkTarget};
use gstore::model::id::LITERAL_FIRST_ID;
use gstore::parser::import::{self, BlankNodes, ImportOptions, Source};
use gstore::query::QueryResult;
use gstore::store::TripleSource;
use gstore::{Database, RocksStore};

/// Every column family of the RocksDB layout.
const CFS: [&str; 7] = ["spo", "pos", "osp", "stats", "dict", "gspo", "meta"];

/// A column family's `(key, value)` pairs, in key order.
type Pairs = Vec<(Vec<u8>, Vec<u8>)>;
/// Every column family of a store.
type Dump = Vec<(&'static str, Pairs)>;

fn lubm_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm/lubm.nt")
}

fn lubm_query(n: usize) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("testdata/lubm/lubm_q{n}.rq"));
    std::fs::read_to_string(p).unwrap()
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_dt_bulk_rocks_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Strict, with blank-node labels kept as written (deterministic ids).
fn import_opts() -> ImportOptions {
    ImportOptions {
        blank_nodes: BlankNodes::Keep,
        ..ImportOptions::strict()
    }
}

fn bulk_opts(threads: usize) -> BulkOptions {
    BulkOptions {
        memory_budget: 64 << 20,
        threads,
        ..BulkOptions::default()
    }
}

fn bulk_rocks(dir: &Path, sources: &[Source], threads: usize) -> gstore::Result<BulkReport> {
    bulk::build(
        dir,
        "t",
        BulkInput::Sources(sources),
        &import_opts(),
        &mut (),
        BulkTarget::RocksDb,
        &bulk_opts(threads),
    )
}

fn bulk_text(dir: &Path, text: &str) -> gstore::Result<BulkReport> {
    bulk::build(
        dir,
        "t",
        BulkInput::Text {
            text,
            format: import::RdfFormat::Turtle,
            name: "<string>",
        },
        &import_opts(),
        &mut (),
        BulkTarget::RocksDb,
        &bulk_opts(2),
    )
}

/// N-Quads (named graphs sharing terms with the default graph, a graph
/// named by a blank node, duplicates) and Turtle with a literal far over
/// the disk layout's key limit, blank nodes and repeated statements.
fn write_extras(dir: &Path) -> Vec<Source> {
    let mut nq = String::new();
    for g in 0..3 {
        for i in 0..150 {
            nq.push_str(&format!(
                "<http://www.Department0.University0.edu/GraduateStudent{i}> <http://ex/rated> \
                 \"{}\"^^<http://www.w3.org/2001/XMLSchema#integer> <http://ex/g{g}> .\n",
                (i * 7 + g) % 11
            ));
            nq.push_str(&format!(
                "<http://ex/only/{g}/{i}> <http://ex/p{}> <http://ex/only/{g}/{}> <http://ex/g{g}> .\n",
                i % 4,
                (i + 1) % 150
            ));
        }
    }
    nq.push_str("<http://ex/a> <http://ex/p0> <http://ex/b> _:g1 .\n");
    nq.push_str("<http://ex/a> <http://ex/p0> <http://ex/b> _:g1 .\n");
    nq.push_str("<http://ex/default> <http://ex/p9> \"in the default graph\"@en .\n");
    let long = "x".repeat(5000);
    let mut ttl = String::new();
    ttl.push_str(&format!("<http://ex/long> <http://ex/text> \"{long}\" .\n"));
    ttl.push_str("_:b0 <http://ex/knows> _:b1 .\n_:b1 <http://ex/knows> _:b0 .\n");
    ttl.push_str("<http://ex/a> <http://ex/p0> <http://ex/b> .\n<http://ex/a> <http://ex/p0> <http://ex/b> .\n");
    ttl.push_str("<http://ex/a> <http://ex/p1> \"tab\\tnew\\nline \\\"q\\\" é\" .\n");
    let (a, b) = (dir.join("extra.nq"), dir.join("extra.ttl"));
    std::fs::write(&a, nq).unwrap();
    std::fs::write(&b, ttl).unwrap();
    vec![Source::File(a), Source::File(b)]
}

/// Every column family of the store in `db_dir`, as `(key, value)` lists.
fn dump(db_dir: &Path) -> Dump {
    let store = RocksStore::open(db_dir.join("rocksdb")).unwrap();
    store.check_stats().unwrap();
    CFS.iter()
        .map(|cf| (*cf, store.debug_dump_cf(cf).unwrap()))
        .collect()
}

/// Assert that two dumps are equal, naming the first difference.
fn assert_same_store(a: &[(&'static str, Pairs)], b: &[(&'static str, Pairs)], what: &str) {
    for ((cf, x), (_, y)) in a.iter().zip(b) {
        if x == y {
            continue;
        }
        let at = x.iter().zip(y).position(|(p, q)| p != q);
        panic!(
            "{what}: column family {cf} differs ({} vs {} entries; first difference at {at:?}: \
             {:?} vs {:?})",
            x.len(),
            y.len(),
            at.map(|i| &x[i]),
            at.map(|i| &y[i]),
        );
    }
}

fn sorted_rows(r: QueryResult) -> Vec<Vec<Option<String>>> {
    match r {
        QueryResult::Select(rs) => {
            let mut rows = rs.rows;
            rows.sort();
            rows
        }
        other => panic!("SELECT expected, got {other:?}"),
    }
}

/// Every list read of `b` over the ids it uses, **in the order returned**
/// (the contract order), with the counts next to the lists they count.
fn fingerprint<B: TripleSource>(b: &B) -> String {
    let mut out = String::new();
    let all = b.iter_all();
    writeln!(out, "iter_all={all:?}").unwrap();
    writeln!(
        out,
        "globals={} {} {} {}",
        b.triple_count(),
        b.distinct_subjects(),
        b.distinct_objects(),
        b.num_predicates()
    )
    .unwrap();
    let subjects = b.subject_keys();
    let objects = b.object_keys();
    writeln!(out, "subject_keys={subjects:?}\nobject_keys={objects:?}").unwrap();
    let mut preds: Vec<u32> = all.iter().map(|t| t.pred).collect();
    preds.dedup();
    for &p in &preds {
        writeln!(
            out,
            "p{p}: so={:?} subs={:?} objs={:?} card={} ds={} do={} n={} has={}",
            b.so_by_p(p),
            b.subs_by_p(p),
            b.objs_by_p(p),
            b.pred_card(p),
            b.pred_distinct_subj(p),
            b.pred_distinct_obj(p),
            b.count_so_by_p(p),
            b.has_predicate(p)
        )
        .unwrap();
    }
    for &s in &subjects {
        let po = b.po_by_s(s);
        writeln!(
            out,
            "s{s}: po={po:?} n={} dp={} has={}",
            b.count_po_by_s(s),
            b.distinct_p_of_s(s),
            b.has_subject(s)
        )
        .unwrap();
        for &(p, o) in po.iter().take(4) {
            writeln!(
                out,
                "  sp={:?}/{} so={:?}/{} e={}",
                b.o_by_sp(s, p),
                b.count_o_by_sp(s, p),
                b.p_by_so(s, o),
                b.count_p_by_so(s, o),
                b.exists(s, p, o)
            )
            .unwrap();
        }
    }
    for &o in &objects {
        let ps = b.ps_by_o(o);
        writeln!(
            out,
            "o{o}: ps={ps:?} n={} dp={} has={}",
            b.count_ps_by_o(o),
            b.distinct_p_of_o(o),
            b.has_object(o)
        )
        .unwrap();
        for &(p, _) in ps.iter().take(4) {
            writeln!(out, "  po={:?}/{}", b.s_by_po(p, o), b.count_s_by_po(p, o)).unwrap();
        }
    }
    // Absent ids answer empty everywhere.
    for id in [9_999_999u32, LITERAL_FIRST_ID + 9_999_999] {
        writeln!(
            out,
            "absent {id}: {:?} {:?} {} {} {} {}",
            b.po_by_s(id),
            b.ps_by_o(id),
            b.has_subject(id),
            b.has_object(id),
            b.count_po_by_s(id),
            b.distinct_p_of_o(id)
        )
        .unwrap();
    }
    out
}

/// LUBM plus named graphs, blank nodes and a 5000-byte literal: every column
/// family of the bulk-built store is byte-equal to the incremental build's,
/// at one thread and at four, and the report counts what is stored.
#[test]
fn every_column_family_equals_the_incremental_build() {
    let base = tmp("cfs");
    let mut sources = vec![Source::File(lubm_nt())];
    sources.extend(write_extras(&base));
    let (inc, one, four) = (
        base.join("inc.db"),
        base.join("one.db"),
        base.join("four.db"),
    );
    let (db, report) =
        Database::build_rocksdb_from_sources("t", &inc, &sources, &import_opts(), &mut ()).unwrap();
    let (triples, quads) = (
        db.triple_num(),
        db.named_graphs()
            .values()
            .map(|g| g.triple_count())
            .sum::<u64>(),
    );
    drop(db);
    let r1 = bulk_rocks(&one, &sources, 1).unwrap();
    let r4 = bulk_rocks(&four, &sources, 4).unwrap();
    for r in [&r1, &r4] {
        assert_eq!(r.import, report);
        assert_eq!((r.triples, r.quads), (triples, quads));
        assert!(r.triples > 100_543 && r.quads > 0, "{r:?}");
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert!(r.kv_bytes > 0);
    }
    assert!(!one.join(bulk::STAGING_DIR).exists());
    let want = dump(&inc);
    assert_same_store(&want, &dump(&one), "bulk (1 thread) vs incremental");
    assert_same_store(&want, &dump(&four), "bulk (4 threads) vs incremental");
    assert!(want.iter().all(|(_, kv)| !kv.is_empty()));
    let _ = std::fs::remove_dir_all(&base);
}

/// A bulk-built store reads exactly like the in-memory build — every list
/// in the contract order, every count, every existence check — its
/// statistics are exact, and the 14 LUBM queries answer what the memory
/// build and the incremental RocksDB build answer.
#[test]
fn bulk_store_reads_like_the_memory_build() {
    let base = tmp("reads");
    let mut sources = vec![Source::File(lubm_nt())];
    sources.extend(write_extras(&base));
    let (blk, inc) = (base.join("bulk.db"), base.join("inc.db"));
    bulk_rocks(&blk, &sources, 0).unwrap();
    drop(
        Database::build_rocksdb_from_sources("t", &inc, &sources, &import_opts(), &mut ()).unwrap(),
    );
    let (mut mem, _) =
        Database::build_from_sources("m", &sources, &import_opts(), &mut ()).unwrap();
    let mut db = Database::open(&blk).unwrap();
    assert!(Database::is_rocksdb(&blk));
    assert_eq!(db.name(), "t");
    let rocks = db.backend().as_rocks().unwrap();
    rocks.check_stats().unwrap();
    assert_eq!(
        fingerprint(db.backend()),
        fingerprint(mem.store()),
        "bulk RocksDB reads differ from the in-memory store"
    );
    let mut idb = Database::open(&inc).unwrap();
    for n in 1..=14 {
        let q = lubm_query(n);
        let rows = db.select(&q).unwrap().rows;
        assert_eq!(
            rows,
            idb.select(&q).unwrap().rows,
            "Q{n} bulk vs incremental"
        );
        let mut sorted = rows.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            sorted_rows(mem.query(&q).unwrap()),
            "Q{n} vs memory"
        );
    }
    for q in [
        "SELECT ?s ?o WHERE { GRAPH <http://ex/g1> { ?s <http://ex/rated> ?o } }",
        "SELECT ?x WHERE { <http://ex/long> <http://ex/text> ?x }",
        "SELECT ?a ?b WHERE { ?a <http://ex/knows> ?b }",
        "SELECT ?s ?p WHERE { ?s ?p <http://ex/b> }",
    ] {
        assert_eq!(
            sorted_rows(db.query(q).unwrap()),
            sorted_rows(mem.query(q).unwrap()),
            "{q}"
        );
    }
    drop((db, idb));
    let _ = std::fs::remove_dir_all(&base);
}

/// Updates after a bulk build — inserts of new and known terms, deletes that
/// empty a predicate, DELETE WHERE, a rolled-back and a committed
/// transaction — keep every counter exact, before and after a reopen, and
/// the answers equal the in-memory database's after the same updates.
#[test]
fn updates_after_a_bulk_build_keep_the_stats_exact() {
    let base = tmp("updates");
    let dir = base.join("u.db");
    let data = "\
<http://ex/s0> <http://ex/p> <http://ex/o0> .
<http://ex/s0> <http://ex/p> <http://ex/o1> .
<http://ex/s1> <http://ex/p> <http://ex/o1> .
<http://ex/s1> <http://ex/q> \"one\" .
<http://ex/s2> <http://ex/lonely> <http://ex/o2> .
<http://ex/s2> <http://ex/q> \"two\" .
";
    bulk_text(&dir, data).unwrap();
    let mut db = Database::open(&dir).unwrap();
    let mut mem = Database::build_from_str("m", data).unwrap();
    let updates = [
        "INSERT DATA { <http://ex/s9> <http://ex/p> <http://ex/o0> . <http://ex/s0> <http://ex/new> \"v\" }",
        "DELETE DATA { <http://ex/s2> <http://ex/lonely> <http://ex/o2> }",
        "DELETE DATA { <http://ex/s0> <http://ex/p> <http://ex/o1> }",
        "DELETE WHERE { ?s <http://ex/q> ?o }",
        "INSERT DATA { <http://ex/s1> <http://ex/q> \"one\" . <http://ex/o1> <http://ex/p> <http://ex/s1> }",
    ];
    let check = |db: &Database, what: &str| {
        db.backend()
            .as_rocks()
            .unwrap()
            .check_stats()
            .unwrap_or_else(|e| panic!("{what}: {e}"));
    };
    for u in updates {
        db.query(u).unwrap();
        mem.query(u).unwrap();
        check(&db, u);
    }
    db.begin().unwrap();
    db.query("INSERT DATA { <http://ex/t> <http://ex/p> <http://ex/o9> }")
        .unwrap();
    db.query("DELETE DATA { <http://ex/s1> <http://ex/p> <http://ex/o1> }")
        .unwrap();
    check(&db, "inside a transaction");
    db.rollback().unwrap();
    check(&db, "after the rollback");
    db.begin().unwrap();
    db.query("INSERT DATA { <http://ex/t> <http://ex/p> <http://ex/o9> }")
        .unwrap();
    db.commit().unwrap();
    mem.query("INSERT DATA { <http://ex/t> <http://ex/p> <http://ex/o9> }")
        .unwrap();
    check(&db, "after the commit");
    let q = "SELECT ?s ?p ?o WHERE { ?s ?p ?o }";
    let want = sorted_rows(mem.query(q).unwrap());
    assert_eq!(sorted_rows(db.query(q).unwrap()), want);
    drop(db);
    let mut db = Database::open(&dir).unwrap();
    check(&db, "after a reopen");
    assert_eq!(sorted_rows(db.query(q).unwrap()), want);
    assert_eq!(db.triple_num(), mem.triple_num());
    drop(db);
    let _ = std::fs::remove_dir_all(&base);
}

/// An open database holds its directory's lock for the RocksDB layout too
/// (as for the others): released when it is dropped. A store whose
/// `rocksdb/LOCK` alone is held — what an older binary serving it holds —
/// still refuses the bulk build (Linux reads the lock from the kernel's
/// table), and the store is replaced once it is free.
#[test]
fn an_open_rocksdb_database_holds_the_directory_lock() {
    use gstore::db::dirlock::DirLock;
    let base = tmp("dirlock");
    let dir = base.join("l.db");
    bulk_text(&dir, "<http://ex/a> <http://ex/p> <http://ex/b> .\n").unwrap();
    assert!(!DirLock::held_here(&dir));
    let db = Database::open(&dir).unwrap();
    assert!(db.holds_dir_lock() && DirLock::held_here(&dir));
    drop(db);
    assert!(!DirLock::held_here(&dir));
    // The per-triple helper's database holds it as well.
    let built = Database::build_rocksdb_from_str(
        "l",
        &dir,
        "<http://ex/a> <http://ex/p> <http://ex/c> .\n",
    )
    .unwrap();
    assert!(built.holds_dir_lock() && DirLock::held_here(&dir));
    // ... and refuses to rebuild a directory a database of this process serves.
    let e = Database::build_rocksdb_from_str(
        "l",
        &dir,
        "<http://ex/x> <http://ex/p> <http://ex/y> .\n",
    )
    .err()
    .expect("refused while open")
    .to_string();
    assert!(e.contains("open in this process"), "{e}");
    drop(built);
    assert_eq!(Database::open(&dir).unwrap().triple_num(), 1);

    if cfg!(target_os = "linux") {
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("rocksdb/LOCK"))
            .unwrap();
        if holder.try_lock().is_ok() {
            let e = bulk_text(&dir, "<http://ex/x> <http://ex/p> <http://ex/y> .\n").unwrap_err();
            assert!(e.to_string().contains("in use"), "{e}");
            assert!(!dir.join(bulk::STAGING_DIR).exists());
            drop(holder);
            bulk_text(&dir, "<http://ex/x> <http://ex/p> <http://ex/y> .\n").unwrap();
            let mut db = Database::open(&dir).unwrap();
            assert!(matches!(
                db.query("ASK { <http://ex/x> <http://ex/p> <http://ex/y> }")
                    .unwrap(),
                QueryResult::Ask(true)
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// A failed bulk build — a strict import's bad statement, a database open in
/// this process — leaves the old database (RocksDB or another layout) as it
/// was, with no staging area.
#[test]
fn a_failed_build_leaves_the_old_database() {
    let base = tmp("failed");
    let dir = base.join("f.db");
    bulk_text(&dir, "<http://ex/a> <http://ex/p> <http://ex/b> .\n").unwrap();
    let ask = "ASK { <http://ex/a> <http://ex/p> <http://ex/b> }";
    let bad = base.join("bad.nt");
    std::fs::write(
        &bad,
        "<http://ex/c> <http://ex/p> <http://ex/d> .\nnot rdf at all\n",
    )
    .unwrap();
    let e = bulk_rocks(&dir, &[Source::File(bad.clone())], 2).unwrap_err();
    assert!(!e.to_string().is_empty());
    assert!(!dir.join(bulk::STAGING_DIR).exists());
    {
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(db.triple_num(), 1);
        assert!(matches!(db.query(ask).unwrap(), QueryResult::Ask(true)));
        // While it is open here (it holds the directory lock), a rebuild is
        // refused and changes nothing.
        let e = bulk_text(&dir, "<http://ex/x> <http://ex/p> <http://ex/y> .\n").unwrap_err();
        assert!(e.to_string().contains("open in this process"), "{e}");
        assert!(!dir.join(bulk::STAGING_DIR).exists());
        assert!(matches!(db.query(ask).unwrap(), QueryResult::Ask(true)));
        db.query("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/c> }")
            .unwrap();
    }
    assert_eq!(Database::open(&dir).unwrap().triple_num(), 2);

    // Over a disk-layout database.
    let disk = base.join("d.db");
    bulk::build(
        &disk,
        "d",
        BulkInput::Text {
            text: "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
            format: import::RdfFormat::Turtle,
            name: "<string>",
        },
        &import_opts(),
        &mut (),
        BulkTarget::Disk,
        &BulkOptions::default(),
    )
    .unwrap();
    bulk_rocks(&disk, &[Source::File(bad)], 1).unwrap_err();
    assert!(Database::is_disk(&disk) && !Database::is_rocksdb(&disk));
    let mut db = Database::open(&disk).unwrap();
    assert!(matches!(db.query(ask).unwrap(), QueryResult::Ask(true)));
    drop(db);
    // ... and a successful one replaces it.
    bulk_text(&disk, "<http://ex/x> <http://ex/p> <http://ex/y> .\n").unwrap();
    assert!(Database::is_rocksdb(&disk) && !disk.join("kvstore.kv").exists());
    assert_eq!(Database::open(&disk).unwrap().triple_num(), 1);
    let _ = std::fs::remove_dir_all(&base);
}

/// A crash after each install step leaves a directory that opens as the old
/// database (before the commit point) or the new one (from it on), over a
/// RocksDB and over a disk-layout database; the next build cleans up.
#[test]
fn every_crash_point_leaves_the_old_or_the_new_database() {
    let base = tmp("crash");
    let old = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";
    let new = "<http://ex/n> <http://ex/p> <http://ex/m> .\n<http://ex/n> <http://ex/p> <http://ex/k> .\n<http://ex/n> <http://ex/q> <http://ex/k> .\n";
    for (i, point) in bulk::ROCKS_CRASH_POINTS.iter().enumerate() {
        for prior in [BulkTarget::RocksDb, BulkTarget::Disk] {
            let dir = base.join(format!("c{i}-{prior:?}.db"));
            bulk::build(
                &dir,
                "t",
                BulkInput::Text {
                    text: old,
                    format: import::RdfFormat::Turtle,
                    name: "<string>",
                },
                &import_opts(),
                &mut (),
                prior,
                &BulkOptions::default(),
            )
            .unwrap();
            bulk::debug_crash_at(&dir, Some(point));
            let e = bulk_text(&dir, new).unwrap_err();
            bulk::debug_crash_at(&dir, None);
            assert!(e.to_string().contains("simulated crash"), "{point}: {e}");
            let want = if *point == "staged" { 1 } else { 3 };
            let db = Database::open(&dir).unwrap();
            assert_eq!(db.triple_num(), want, "{point} over {prior:?}");
            drop(db);
            // The next build works and leaves a clean directory.
            bulk_text(&dir, old).unwrap();
            assert_eq!(Database::open(&dir).unwrap().triple_num(), 1, "{point}");
            assert!(!dir.join(bulk::STAGING_DIR).exists());
            assert!(!dir.join("rocksdb.new").exists() && !dir.join("rocksdb.old").exists());
            assert!(!dir.join("kvstore.kv").exists());
        }
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// An empty input builds an empty, openable store with no counters.
#[test]
fn an_empty_input_builds_an_empty_store() {
    let base = tmp("empty");
    let dir = base.join("e.db");
    let r = bulk_rocks(&dir, &[], 1).unwrap();
    assert_eq!((r.triples, r.entities, r.quads), (0, 0, 0));
    let d = dump(&dir);
    for (cf, kv) in &d {
        let want = usize::from(*cf == "meta") * 2;
        assert_eq!(kv.len(), want, "{cf}: {kv:?}");
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.triple_num(), 0);
    db.query("INSERT DATA { <http://ex/a> <http://ex/p> \"v\" }")
        .unwrap();
    db.backend().as_rocks().unwrap().check_stats().unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&base);
}

/// `gbuild --rocksdb` builds in bulk (here cutting many small SST files) a
/// store byte-equal to `--rocksdb --incremental`'s; `--fill` is refused.
#[test]
fn gbuild_rocksdb_uses_the_bulk_build() {
    let base = tmp("cli");
    let gbuild = env!("CARGO_BIN_EXE_gbuild");
    let nt = lubm_nt();
    let run = |args: &[&str], split: Option<&str>| {
        let mut c = Command::new(gbuild);
        c.args(args).current_dir(&base);
        if let Some(s) = split {
            c.env("GSTORE_BULK_SST_SPLIT_BYTES", s);
        }
        c.output().unwrap()
    };
    let out = run(
        &[
            "--rocksdb",
            "--threads",
            "2",
            "--memory",
            "16",
            "b",
            nt.to_str().unwrap(),
        ],
        Some("65536"),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout.contains("RocksDB, bulk build"), "{stdout}");
    assert!(stdout.contains("triples=100543"), "{stdout}");
    assert!(stdout.contains("rocksdb/"), "{stdout}");
    let out = run(
        &["--rocksdb", "--incremental", "b", nt.to_str().unwrap()],
        None,
    );
    assert!(!out.status.success(), "an existing database needs --force");
    let out = run(
        &["--rocksdb", "--incremental", "i", nt.to_str().unwrap()],
        None,
    );
    assert!(out.status.success(), "{out:?}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("bulk build"));
    let (b, i) = (dump(&base.join("b.db")), dump(&base.join("i.db")));
    // The names differ ("b" / "i"): compare the meta CF without them.
    let names = |d: &[(&'static str, Pairs)]| -> Dump {
        d.iter()
            .map(|(cf, kv)| {
                let kv: Vec<_> = kv.iter().filter(|(k, _)| k != b"name").cloned().collect();
                (*cf, kv)
            })
            .collect::<Vec<_>>()
    };
    assert_same_store(&names(&i), &names(&b), "gbuild --rocksdb vs --incremental");
    let sst = std::fs::read_dir(base.join("b.db/rocksdb"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".sst")
        })
        .count();
    assert!(sst > 7, "many small SST files expected, found {sst}");
    // --force rebuilds over it; --fill is a --disk knob.
    let out = run(&["--rocksdb", "--force", "b", nt.to_str().unwrap()], None);
    assert!(out.status.success(), "{out:?}");
    let out = run(
        &["--rocksdb", "--fill", "70", "m", nt.to_str().unwrap()],
        None,
    );
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(!base.join("m.db").exists());
    let _ = std::fs::remove_dir_all(&base);
}
