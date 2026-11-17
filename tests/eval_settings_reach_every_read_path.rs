//! Every read and UPDATE path applies the database's evaluation settings
//! (review design#9).
//!
//! The evaluator's configuration — `SERVICE` policy, integer division,
//! Top-K options, graph-function budget, PFN caller and limits, user
//! functions, and the CSR / substring-index caches — used to be copied by
//! hand at six construction sites (`Database::read_as`, the UPDATE WHERE
//! clause, an HTTP transaction's read snapshot, `ConcurrentDb` snapshots and
//! version views), and the snapshot paths silently left some out (the
//! substring-index cache everywhere, the CSR cache on version views; a
//! transaction read once missed the `SERVICE` policy). Each site now applies
//! one [`EvalSettings`] value ([`Database::eval_settings`]) through
//! [`Evaluator::with_settings`]. These tests watch what every evaluation was
//! configured with ([`watch_evaluator_settings`]) and compare it with the
//! database's settings, path by path.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gstore::db::{LoadPolicy, QuerySettings, ServicePolicy};
use gstore::http_api::ApiServer;
use gstore::pfn::{PfnCaller, PfnLimits, PfnRegistry};
use gstore::query::engine::{
    dictionary_string_index, watch_evaluator_settings, EvalSettings, SeenSettings, StringIndexCell,
};
use gstore::query::{DpbMode, Evaluator, IntegerDivision, TopKOptions};
use gstore::{ConcurrentDb, Database, QueryResult};

/// A temp directory removed when dropped (also when the test fails).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gstore_evalsettings_{tag}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Settings unlike every default, so a site that leaves one out shows.
const TOPK: TopKOptions = TopKOptions {
    early_termination: true,
    bounded_heap: false,
    dpb: DpbMode::Off,
};

fn limits() -> PfnLimits {
    PfnLimits {
        fuel_per_call: 777_777,
        ..PfnLimits::default()
    }
}

/// An in-memory database configured with non-default settings: truncating
/// division, unusual Top-K options and graph-function bound, a restricted
/// `SERVICE` policy, PFN limits and an installed PFN caller, and one user
/// function.
fn configured(pfn_dir: &TempDir) -> Database {
    let mut db = Database::build_from_str("settings", "").unwrap();
    db.query(&format!("INSERT DATA {{ {} }}", trig_body()))
        .unwrap();
    db.set_query_cache_size(0);
    db.set_integer_division(IntegerDivision::Truncate);
    db.set_topk_options(TOPK);
    db.set_gfun_max_output_bytes(12_345);
    db.set_load_policy(LoadPolicy::restricted());
    db.set_pfn_limits(limits());
    db.set_pfn_caller(Some(PfnCaller::new(
        Arc::new(PfnRegistry::new(pfn_dir.0.join("pfn"))),
        "alice",
    )));
    db.register_function("http://ex/fn", |_| None);
    db
}

/// `DATA` as the body of an `INSERT DATA` (a named graph included).
fn trig_body() -> String {
    "<http://ex/a> <http://ex/p> <http://ex/b> . <http://ex/a> <http://ex/name> \"alpha\" . \
     <http://ex/b> <http://ex/p> <http://ex/c> . <http://ex/b> <http://ex/name> \"beta\" . \
     <http://ex/c> <http://ex/p> <http://ex/a> . \
     GRAPH <http://ex/g> { <http://ex/a> <http://ex/q> <http://ex/b> }"
        .to_string()
}

/// What `run` evaluated, one entry per evaluation.
fn seen(run: impl FnOnce()) -> Vec<SeenSettings> {
    let watch = watch_evaluator_settings();
    run();
    watch.take()
}

#[test]
fn the_default_settings_are_a_new_evaluators() {
    let dict = gstore::dict::Dictionary::new();
    let store = gstore::store::TripleStore::new();
    let q = gstore::parser::sparql::parse("ASK {}").unwrap();
    let got = seen(|| {
        Evaluator::new(&dict, &store).evaluate(&q).unwrap();
    });
    assert_eq!(
        got,
        vec![SeenSettings::of(&EvalSettings::default(), false, false)]
    );
}

#[test]
fn database_reads_and_updates_apply_the_settings() {
    let pfn_dir = TempDir::new("db");
    let mut db = configured(&pfn_dir);
    let settings = db.eval_settings();
    assert_eq!(settings.service_policy, ServicePolicy::Denied);
    assert_eq!(settings.integer_division, IntegerDivision::Truncate);
    let both = SeenSettings::of(&settings, true, true);
    // The substring index covers the dictionary every graph shares; the
    // CSR covers the default graph only, so a WITH / USING clause's
    // evaluator builds its own over its store.
    let dictionary_only = SeenSettings::of(&settings, false, true);

    let read = seen(|| {
        db.query("SELECT ?s WHERE { ?s <http://ex/p> ?o FILTER(CONTAINS(STR(?o), \"ex\")) }")
            .unwrap();
    });
    assert_eq!(read, vec![both.clone()], "Database::read_as");

    let update = seen(|| {
        db.query("INSERT { ?s <http://ex/r> ?o } WHERE { ?s <http://ex/p> ?o }")
            .unwrap();
    });
    assert_eq!(update, vec![both], "UPDATE WHERE on the default graph");

    let with = seen(|| {
        db.query(
            "WITH <http://ex/g> INSERT { ?s <http://ex/r2> ?o } WHERE { ?s <http://ex/q> ?o }",
        )
        .unwrap();
    });
    assert_eq!(with, vec![dictionary_only.clone()], "UPDATE WITH <g>");

    let using = seen(|| {
        db.query(
            "INSERT { ?s <http://ex/r3> ?o } USING <http://ex/g> WHERE { ?s <http://ex/q> ?o }",
        )
        .unwrap();
    });
    assert_eq!(using, vec![dictionary_only], "UPDATE … USING <g>");
}

#[test]
fn concurrent_db_snapshots_and_version_views_apply_the_settings() {
    let pfn_dir = TempDir::new("cdb");
    let db = configured(&pfn_dir);
    let settings = db.eval_settings();
    let c = ConcurrentDb::new(db);
    let q = "SELECT ?s WHERE { ?s <http://ex/name> ?n FILTER(STRSTARTS(?n, \"al\")) }";

    let snapshot = seen(|| {
        c.query(q).unwrap();
    });
    assert_eq!(
        snapshot,
        vec![SeenSettings::of(&settings, true, true)],
        "ConcurrentDb snapshot reads"
    );

    // A view of the published state: the base snapshot's caches serve.
    let view = c.version_view();
    let at_base = seen(|| {
        view.query(q).unwrap();
    });
    assert_eq!(
        at_base,
        vec![SeenSettings::of(&settings, true, true)],
        "a version view of the base state"
    );
    drop(view);

    // A view over a state with a newer write: the evaluator builds its own
    // CSR over the materialized store; the settings are the same.
    let mut txn = c.begin();
    txn.insert(gstore::Triple::new(
        gstore::Term::iri("http://ex/d"),
        gstore::Term::iri("http://ex/name"),
        gstore::Term::plain_literal("alder"),
    ));
    txn.commit().unwrap();
    let view = c.version_view();
    let mut rows = 0;
    let after_write = seen(|| {
        if let QueryResult::Select(rs) = view.query(q).unwrap() {
            rows = rs.row_count();
        }
    });
    assert_eq!(rows, 2, "alpha and alder");
    assert_eq!(
        after_write,
        vec![SeenSettings::of(&settings, false, true)],
        "a version view of a newer state"
    );
}

// ---- an HTTP transaction's read snapshot -------------------------------

/// `method target` with `body` as root (password `root`); status and body.
fn request(addr: &SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    // A hang guard only.
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Basic cm9vdDpyb290\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status in {resp:?}"));
    let body = resp.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, body.to_string())
}

/// Stops the server (and waits for it) when dropped — also on failure.
struct Serving {
    server: Arc<ApiServer>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[test]
fn a_transaction_read_applies_the_databases_settings() {
    let root = TempDir::new("http");
    let settings = QuerySettings {
        integer_division: IntegerDivision::Truncate,
        query_cache_bytes: 0,
        topk: TOPK,
        gfun_max_output_bytes: 12_345,
        pfn: limits(),
    };
    let server = Arc::new(
        ApiServer::bind(&root.0, "root", "127.0.0.1:0")
            .expect("bind")
            .with_query_settings(settings)
            .with_shutdown_grace(Duration::from_secs(2)),
    );
    let addr = server.local_addr().unwrap();
    let serving = Serving {
        server: Arc::clone(&server),
        thread: Some(std::thread::spawn({
            let server = Arc::clone(&server);
            move || {
                server.serve_forever();
            }
        })),
    };
    let data = "<http://ex/a> <http://ex/name> \"alpha\" .\n\
                <http://ex/b> <http://ex/name> \"beta\" .\n";
    let (s, b) = request(&addr, "POST", "/build?db_name=t", data);
    assert_eq!(s, 200, "build: {b}");
    let q = "SELECT ?s WHERE { ?s <http://ex/name> ?n FILTER(CONTAINS(?n, \"lph\")) }";

    let outside = seen(|| {
        let (s, b) = request(&addr, "POST", "/sparql?db_name=t", q);
        assert_eq!(s, 200, "read: {b}");
    });
    let (s, b) = request(&addr, "POST", "/begin?db_name=t", "");
    assert_eq!(s, 200, "begin: {b}");
    let tid = b
        .split("\"TID\":\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .unwrap_or_else(|| panic!("no TID in {b}"))
        .to_string();
    // While the transaction is open, a reader outside it reads the
    // transaction's begin-time snapshot (ReadSnapshot)…
    let snapshot = seen(|| {
        let (s, b) = request(&addr, "POST", "/sparql?db_name=t", q);
        assert_eq!(s, 200, "read during the transaction: {b}");
    });
    // … and the transaction's own reads the live database.
    let inside = seen(|| {
        let (s, b) = request(&addr, "POST", &format!("/tquery?db_name=t&tid={tid}"), q);
        assert_eq!(s, 200, "tquery: {b}");
    });
    let (s, b) = request(&addr, "POST", &format!("/rollback?db_name=t&tid={tid}"), "");
    assert_eq!(s, 200, "rollback: {b}");
    drop(serving);

    assert_eq!(outside.len(), 1, "{outside:?}");
    let expected = &outside[0];
    assert_eq!(expected.integer_division, IntegerDivision::Truncate);
    assert_eq!(expected.topk, TOPK);
    assert_eq!(expected.gfun_max_output_bytes, 12_345);
    assert!(expected.csr_provider && expected.string_index_provider);
    assert_eq!(
        snapshot,
        vec![expected.clone()],
        "a read of the transaction's snapshot is configured as a read of the database"
    );
    assert_eq!(
        inside,
        vec![expected.clone()],
        "a read inside the transaction"
    );
}

#[test]
fn a_snapshots_string_index_serves_its_own_dictionary_state() {
    // The cell a read snapshot keeps: seeded with the database's index, it
    // hands out that index (no rebuild) for the dictionary state it was
    // taken at, and builds — without keeping — one for any other state.
    let mut dict = gstore::dict::Dictionary::new();
    for i in 0..100 {
        dict.intern_term(&gstore::Term::plain_literal(format!("term number {i}")));
    }
    let index = dictionary_string_index(&dict).expect("an index for 100 terms");
    let cell = StringIndexCell::seeded(dict.generation(), Arc::clone(&index));
    let got = cell.get(&dict, None).expect("the seeded index");
    assert!(Arc::ptr_eq(&got, &index), "the seeded index is served");
    let mut later = dict.clone();
    later.intern_term(&gstore::Term::plain_literal("one more"));
    let other = cell
        .get(&later, None)
        .expect("an index for the newer state");
    assert!(!Arc::ptr_eq(&other, &index));
    let again = cell.get(&dict, None).expect("still the seeded index");
    assert!(
        Arc::ptr_eq(&again, &index),
        "the newer state's index was not kept"
    );
    // An empty cell builds on first use and keeps what it built.
    let lazy = StringIndexCell::new();
    let first = lazy.get(&dict, None).expect("built");
    let second = lazy.get(&dict, None).expect("kept");
    assert!(Arc::ptr_eq(&first, &second));
}
