//! Data test: the three Phase 6 round-2 packages together — a `kvstore.kv`
//! written by the bulk build (R2-BULK, contract C6), read through the
//! out-of-core dictionary (R2-DICT, C4) and through `DiskTriples` /
//! `DiskReadView` / `DiskSource` (R2-DISKLIB, C5).
//!
//! - The bulk build's ids are the in-memory build's: the out-of-core
//!   dictionary over a bulk-built store resolves every id to the memory
//!   build's term, and materializes to a dictionary whose `dict.bin` encoding
//!   is byte-identical to the memory build's.
//! - Q1–Q14 over `DiskTriples` with the out-of-core dictionary answer the
//!   memory build's rows in the same order (also through `DiskStore::query`
//!   and a `DiskReadView` snapshot).
//! - Transactions over a bulk-built store: read-your-writes through the
//!   out-of-core dictionary, nothing on disk before commit, snapshot views
//!   expire on commit, and the commit survives a reopen.
//! - `DiskStore::query` fails on a read error of the triple indexes or of the
//!   out-of-core dictionary instead of answering partially (C2).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gstore::bulk::{self, BulkInput, BulkOptions, BulkTarget};
use gstore::dict::{Dictionary, Space};
use gstore::kvstore::{DiskDict, DiskSource, DiskStore, DiskTriples, ViewError};
use gstore::parser::import::{ImportOptions, RdfFormat, Source};
use gstore::parser::sparql;
use gstore::query::Evaluator;
use gstore::store::read_scope::ReadScope;
use gstore::store::TripleSource;
use gstore::{Database, IdTriple, QueryResult};

const UB: &str = "http://www.lehigh.edu/~zhp2/2004/0401/univ-bench.owl#";
const RDF_TYPE: &str = "<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>";
const COURSE0: &str = "<http://www.Department0.University0.edu/GraduateCourse0>";

/// Header root slots (`kvstore::format`, crate-private): the POS tree and the
/// entity id → key array.
const SLOT_POS: usize = 1;
const SLOT_ID2ENTITY: usize = 6;

fn lubm_nt() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm/lubm.nt")
}

fn lubm_queries() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm");
    (1..=14)
        .map(|i| {
            let text = std::fs::read_to_string(dir.join(format!("lubm_q{i}.rq"))).unwrap();
            (format!("Q{i}"), text)
        })
        .collect()
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_dt_r2_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

type Rows = Vec<Vec<Option<String>>>;

fn rows(r: QueryResult) -> Rows {
    match r {
        QueryResult::Select(rs) => rs.rows,
        QueryResult::Ask(b) => vec![vec![Some(b.to_string())]],
        other => panic!("unexpected result {other:?}"),
    }
}

/// Evaluate `q` over `store` with `dict` in a read scope that must stay clean.
fn eval<S: TripleSource>(dict: &Dictionary, store: &S, q: &str) -> Rows {
    let parsed = sparql::parse(q).unwrap();
    let scope = ReadScope::enter();
    let out = rows(Evaluator::new(dict, store).evaluate(&parsed).unwrap());
    assert_eq!(scope.take_error(), None, "no read error expected");
    out
}

/// Bulk-build `sources` into the database directory `db`; returns the path
/// of its `kvstore.kv`.
fn bulk_build(db: &Path, sources: &[Source]) -> PathBuf {
    let r = bulk::build(
        db,
        "r2",
        BulkInput::Sources(sources),
        &ImportOptions::strict(),
        &mut (),
        BulkTarget::Disk,
        &BulkOptions::default(),
    )
    .unwrap();
    assert_eq!(r.import.failed, 0);
    db.join("kvstore.kv")
}

/// The out-of-core dictionary over `pager` (no materialized strings).
fn lazy_dict(pager: Arc<std::sync::RwLock<gstore::kvstore::Pager>>) -> Dictionary {
    Dictionary::from_backing(Arc::new(DiskDict::open_shared(pager).unwrap()))
}

#[test]
fn bulk_built_store_serves_the_out_of_core_dictionary_and_stream_queries() {
    let base = tmp("serve");
    let sources = vec![Source::File(lubm_nt())];
    let kv = bulk_build(&base.join("bulk.db"), &sources);
    let (mem, _) =
        Database::build_from_sources("m", &sources, &ImportOptions::strict(), &mut ()).unwrap();
    let mdict = mem.dict();

    let dt = DiskTriples::open(&kv, 4096).unwrap();
    let lazy = lazy_dict(dt.shared_pager().unwrap());
    assert!(lazy.is_disk_backed());

    // Every id of every space names the memory build's term, both ways.
    let mut seen = 0usize;
    for space in Space::ALL {
        assert_eq!(lazy.id_limit(space), mdict.id_limit(space), "{space:?}");
        let ids: Vec<u32> = (space.first_id()..lazy.id_limit(space)).collect();
        let batch = lazy.resolve_many(space, &ids).unwrap();
        for (&id, got) in ids.iter().zip(&batch) {
            let want = mdict.resolve(space, id);
            assert_eq!(got.as_deref(), want.as_deref(), "{space:?} {id}");
            let key = got.as_deref().expect("dense ids");
            assert_eq!(lazy.id_of(space, key), Some(id), "{space:?} {key}");
            seen += 1;
        }
    }
    assert!(seen > 10_000, "{seen} ids");
    // Materialized, it is the memory build's dictionary to the byte: the
    // persisted `dict.bin` encoding is identical.
    let materialized = lazy.materialize().unwrap();
    assert!(!materialized.is_disk_backed());
    assert!(
        bincode::serialize(&materialized).unwrap() == bincode::serialize(mdict).unwrap(),
        "dict.bin encoding of the bulk-built dictionary differs from the memory build's"
    );

    // Q1–Q14: DiskTriples (and a snapshot view over a DiskSource, and the
    // DiskStore::query convenience path) with the out-of-core dictionary
    // answer the memory build's rows in the same order.
    let view = dt.read_view();
    let mut total = 0usize;
    for (name, q) in lubm_queries() {
        let want = eval(mdict, mem.store(), &q);
        let got = eval(&lazy, &dt, &q);
        assert_eq!(got, want, "{name} DiskTriples vs memory");
        let snap = view
            .with_base(|ds, _cancel| eval(&lazy, &DiskSource::new(ds), &q))
            .unwrap();
        assert_eq!(snap, want, "{name} DiskReadView vs memory");
        assert_eq!(
            rows(dt.disk_store().unwrap().query(&q).unwrap()),
            want,
            "{name} DiskStore::query vs memory"
        );
        total += want.len();
    }
    assert!(total > 1000, "the queries have answers ({total} rows)");
    let st = lazy.cache_stats();
    assert!(
        st.resident_bytes() > 0 && st.resident_bytes() <= 64 << 20,
        "{st:?}"
    );

    drop((view, lazy));
    dt.close().unwrap();
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn transactions_over_a_bulk_built_store_with_the_out_of_core_dictionary() {
    let base = tmp("txn");
    let sources = vec![Source::File(lubm_nt())];
    let kv = bulk_build(&base.join("bulk.db"), &sources);
    let q1 = &lubm_queries()[0].1;
    let all_grads = format!("SELECT ?X WHERE {{ ?X {RDF_TYPE} <{UB}GraduateStudent> }}");

    let mut dt = DiskTriples::open(&kv, 4096).unwrap();
    let lazy = lazy_dict(dt.shared_pager().unwrap());
    let before = eval(&lazy, &dt, q1);
    assert!(!before.is_empty());
    // A graduate student who does not take GraduateCourse0 yet.
    let student = eval(&lazy, &dt, &all_grads)
        .into_iter()
        .find(|r| !before.contains(r))
        .expect("some graduate student skips GraduateCourse0")[0]
        .clone()
        .unwrap();
    let t = IdTriple {
        sub: lazy.entity_id(&student).expect("the student's id"),
        pred: lazy
            .predicate_id(&format!("<{UB}takesCourse>"))
            .expect("takesCourse"),
        obj: lazy.entity_id(COURSE0).expect("GraduateCourse0"),
    };
    let pages = std::fs::metadata(&kv).unwrap().len();
    let view = dt.read_view();

    // Rolled back: read-your-writes inside, nothing left after.
    dt.begin().unwrap();
    assert!(dt.insert_triple(t).unwrap());
    let inside = eval(&lazy, &dt, q1);
    assert_eq!(inside.len(), before.len() + 1);
    assert!(inside.contains(&vec![Some(student.clone())]));
    // No triple reaches the pager before commit: the snapshot (and the
    // committed store) still answer the old rows.
    let snap = view
        .with_base(|ds, _| eval(&lazy, &DiskSource::new(ds), q1))
        .unwrap();
    assert_eq!(snap, before);
    assert_eq!(rows(dt.disk_store().unwrap().query(q1).unwrap()), before);
    assert!(dt.rollback());
    assert_eq!(eval(&lazy, &dt, q1), before);
    assert_eq!(std::fs::metadata(&kv).unwrap().len(), pages);
    assert!(view.is_current(), "a rollback moves no epoch");

    // Committed: visible everywhere, the old snapshot expires.
    dt.begin().unwrap();
    assert!(dt.insert_triple(t).unwrap());
    assert!(!dt.insert_triple(t).unwrap(), "already in the transaction");
    dt.commit().unwrap();
    let after = eval(&lazy, &dt, q1);
    assert_eq!(after, inside);
    assert!(matches!(view.with_base(|_, _| ()), Err(ViewError::Expired)));
    let fresh = dt
        .read_view()
        .with_base(|ds, _| eval(&lazy, &DiskSource::new(ds), q1))
        .unwrap();
    assert_eq!(fresh, after);
    dt.commit_point().unwrap();
    drop(lazy);
    drop(dt);

    // Reopened: the commit is durable, and the out-of-core dictionary still
    // names every id (the commit added no term).
    let ds = DiskStore::open(&kv, 1024).unwrap();
    assert_eq!(rows(ds.query(q1).unwrap()), after);
    let lazy = ds.lazy_dictionary();
    assert_eq!(lazy.term(t.sub).as_deref(), Some(student.as_str()));
    drop((lazy, ds));
    let _ = std::fs::remove_dir_all(&base);
}

/// Overwrite the root page of header slot `slot` with garbage.
fn corrupt_root(path: &Path, slot: usize) {
    let mut bytes = std::fs::read(path).unwrap();
    let off = 16 + slot * 8;
    let page = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap()) as usize;
    assert!(page > 0, "slot {slot} has a root page");
    bytes[page * 4096..(page + 1) * 4096].fill(0xFF);
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn disk_store_query_fails_on_a_read_error() {
    let base = tmp("readerr");
    let mut text = String::new();
    for i in 0..300 {
        text.push_str(&format!(
            "<http://ex/s{i}> <http://ex/p{}> <http://ex/o{i}> .\n",
            i % 3
        ));
        text.push_str(&format!("<http://ex/s{i}> <http://ex/name> \"n{i}\" .\n"));
    }
    let build = |name: &str| -> PathBuf {
        let db = base.join(name);
        bulk::build(
            &db,
            name,
            BulkInput::Text {
                text: &text,
                format: RdfFormat::Turtle,
                name: "<string>",
            },
            &ImportOptions::strict(),
            &mut (),
            BulkTarget::Disk,
            &BulkOptions::default(),
        )
        .unwrap();
        db.join("kvstore.kv")
    };
    let by_p = "SELECT ?s ?o WHERE { ?s <http://ex/p1> ?o }";
    let by_s = "SELECT ?p ?o WHERE { <http://ex/s7> ?p ?o }";

    // Healthy: both queries answer.
    let kv = build("ok.db");
    let ds = DiskStore::open(&kv, 64).unwrap();
    assert_eq!(rows(ds.query(by_p).unwrap()).len(), 100);
    assert_eq!(rows(ds.query(by_s).unwrap()).len(), 2);
    drop(ds);

    // A broken triple index fails the query that reads it.
    let kv = build("pos.db");
    corrupt_root(&kv, SLOT_POS);
    let ds = DiskStore::open(&kv, 64).unwrap();
    let err = ds.query(by_p).unwrap_err().to_string();
    assert!(err.contains("read from the store failed"), "{err}");
    drop(ds);

    // A broken out-of-core dictionary fails the query whose rows it names.
    let kv = build("dict.db");
    corrupt_root(&kv, SLOT_ID2ENTITY);
    let ds = DiskStore::open(&kv, 64).unwrap();
    let err = ds.query(by_p).unwrap_err().to_string();
    assert!(err.contains("read from the store failed"), "{err}");
    // The error belonged to that statement only: a later query reading no
    // broken page answers (a literal-only projection).
    let lits = rows(
        ds.query("SELECT ?n WHERE { <http://ex/s7> <http://ex/name> ?n }")
            .unwrap(),
    );
    assert_eq!(lits, vec![vec![Some("\"n7\"".to_string())]]);
    drop(ds);
    let _ = std::fs::remove_dir_all(&base);
}
