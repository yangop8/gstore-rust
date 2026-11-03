//! Transactions' read snapshots of on-disk (Stream-mode) databases served by
//! [`ApiServer`] (Phase 6 R4-SNAP): `/begin` takes a `DiskReadView` (no
//! triple copied), non-transactional reads run on it inside the store's
//! writer gate, and the commit cancels and expires them — they re-run on
//! the committed state. The servers here open every database in Stream
//! mode ([`ApiServer::with_open_options`]), whatever the environment says.
//!
//! * readers never see an uncommitted (or rolled-back) pair, only the
//!   begin-time count, and converge on the committed one;
//! * `/commit` completes in well under a second while a long snapshot query
//!   runs (the commit cancels it, it re-runs on the live database);
//! * unloading while reads run gives clean answers (200 or 404);
//! * `/api build layout=disk` answers like `layout=snapshot`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gstore::db::OpenOptions;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};

/// A unique server root, removed with the server (also when the test fails).
fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("gstore_snapdisk_{tag}_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::canonicalize(&d).unwrap()
}

/// An `ApiServer` serving in a background thread; stopped and its root
/// removed when dropped.
struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    root: PathBuf,
    serving: Option<thread::JoinHandle<()>>,
}

impl Srv {
    fn start(tag: &str) -> Srv {
        let root = tmp_root(tag);
        let server = Arc::new(
            ApiServer::bind(&root, "root", "127.0.0.1:0")
                .expect("bind")
                .with_check_point(true)
                .with_shutdown_grace(Duration::from_secs(2))
                .with_open_options(OpenOptions::stream()),
        );
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = Some(thread::spawn(move || {
            s2.serve_forever();
        }));
        Srv {
            server,
            addr,
            root,
            serving,
        }
    }

    fn req(&self, method: &str, target: &str, body: &str) -> (u16, String) {
        request(&self.addr, method, target, body)
    }

    /// Build `name` from `data` in the disk layout through REST `/build`.
    fn build_disk(&self, name: &str, data: &str) {
        let (s, b) = self.req("POST", &format!("/build?db_name={name}&layout=disk"), data);
        assert_eq!(s, 200, "build: {b}");
        assert!(
            self.root
                .join(format!("{name}.db"))
                .join("kvstore.kv")
                .is_file(),
            "the disk layout"
        );
    }

    fn begin(&self, db: &str) -> String {
        let (s, b) = self.req("POST", &format!("/begin?db_name={db}"), "");
        assert_eq!(s, 200, "begin: {b}");
        field(&b, "TID").expect("TID").to_string()
    }

    fn tquery(&self, db: &str, tid: &str, sparql: &str) {
        let (s, b) = self.req("POST", &format!("/tquery?db_name={db}&tid={tid}"), sparql);
        assert_eq!(s, 200, "tquery: {b}");
    }

    /// The triple count through a non-transactional read.
    fn count(&self, db: &str) -> Result<u64, (u16, String)> {
        let (s, b) = self.req("POST", &format!("/sparql?db_name={db}"), COUNT);
        if s != 200 {
            return Err((s, b));
        }
        Ok(count_of(&b))
    }

    /// `POST /api` as root with `pairs`.
    fn api(&self, pairs: &[(&str, &str)]) -> Json {
        let mut body = Json::obj()
            .with("username", "root")
            .with("password", "root");
        for (k, v) in pairs {
            body.set(*k, *v);
        }
        let (s, b) = self.req("POST", "/api", &body.to_string());
        assert_eq!(s, 200, "{b}");
        json::parse(&b).unwrap_or_else(|e| panic!("not JSON ({e}): {b}"))
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(t) = self.serving.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn request(addr: &SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
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

/// A JSON string field of a flat response.
fn field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    Some(&rest[..rest.find('"')?])
}

const COUNT: &str = "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }";

/// The `?n` of a SPARQL JSON result.
fn count_of(body: &str) -> u64 {
    let i = body
        .find("\"value\":\"")
        .unwrap_or_else(|| panic!("{body}"))
        + 9;
    body[i..]
        .split('"')
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("{body}"))
}

/// `n` pairs `(s_i p o_i)`, `(o_i q s_i)` from index `from` on, as N-Triples.
fn pairs(tag: &str, from: usize, n: usize) -> String {
    (from..from + n)
        .map(|i| {
            format!(
                "<http://ex/{tag}s{i}> <http://ex/p> <http://ex/{tag}o{i}> .\n\
                 <http://ex/{tag}o{i}> <http://ex/q> <http://ex/{tag}s{i}> .\n"
            )
        })
        .collect()
}

/// While a transaction inserts pairs through `/tquery`, eight readers query
/// the database non-transactionally: they see exactly the begin-time count
/// — never an uncommitted pair, never half of one — and converge on the
/// committed count after `/commit` (snapshot reads in flight then expire and
/// re-run). A rolled-back transaction is never visible.
#[test]
fn readers_see_the_begin_time_state_and_converge_after_commit() {
    let srv = Arc::new(Srv::start("readers"));
    srv.build_disk("r", &pairs("b", 0, 300));
    let base = srv.count("r").unwrap();
    assert_eq!(base, 600);
    let tid = srv.begin("r");
    // The counts a read may answer: `lo` is the committed count once the
    // commit answered, `hi` the count it commits from the moment it is sent
    // (while it runs, either is right).
    let lo = Arc::new(AtomicU64::new(base));
    let hi = Arc::new(AtomicU64::new(base));
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let (srv, lo, hi, stop, reads) = (
                Arc::clone(&srv),
                Arc::clone(&lo),
                Arc::clone(&hi),
                Arc::clone(&stop),
                Arc::clone(&reads),
            );
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    // The oldest state the read may see is the one committed
                    // when it starts, the newest the one being committed
                    // when it ends.
                    let oldest = lo.load(Ordering::SeqCst);
                    let n = srv.count("r").expect("a reader failed");
                    let newest = hi.load(Ordering::SeqCst);
                    assert_eq!(n % 2, 0, "half a pair is visible: {n}");
                    assert!(
                        n == oldest || n == newest,
                        "saw {n}, committed {oldest} (at the start) or {newest} (at the end)"
                    );
                    reads.fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    // The owner inserts 50 pairs, one statement each.
    for i in 0..50 {
        let pair = pairs("t", i, 1);
        srv.tquery("r", &tid, &format!("INSERT DATA {{ {pair} }}"));
    }
    thread::sleep(Duration::from_millis(200));
    // While the commit runs, both counts are right; once it answered, only
    // the committed one.
    hi.store(base + 100, Ordering::SeqCst);
    let (s, b) = srv.req("POST", &format!("/commit?db_name=r&tid={tid}"), "");
    assert_eq!(s, 200, "commit: {b}");
    lo.store(base + 100, Ordering::SeqCst);
    // Converged: every reader now sees the committed count.
    let deadline = Instant::now() + Duration::from_secs(10);
    while srv.count("r").unwrap() != base + 100 {
        assert!(Instant::now() < deadline, "the readers never converged");
    }
    // A transaction that rolls back is never visible.
    let tid = srv.begin("r");
    for i in 0..20 {
        let pair = pairs("x", i, 1);
        srv.tquery("r", &tid, &format!("INSERT DATA {{ {pair} }}"));
    }
    let (s, b) = srv.req("POST", &format!("/rollback?db_name=r&tid={tid}"), "");
    assert_eq!(s, 200, "rollback: {b}");
    thread::sleep(Duration::from_millis(200));
    stop.store(true, Ordering::SeqCst);
    for r in readers {
        r.join().expect("a reader saw an uncommitted state");
    }
    assert!(reads.load(Ordering::SeqCst) > 50, "the readers ran");
    assert_eq!(srv.count("r").unwrap(), base + 100);
    let (_, b) = srv.req(
        "POST",
        "/sparql?db_name=r",
        "ASK { <http://ex/xs0> <http://ex/p> <http://ex/xo0> }",
    );
    assert!(b.contains("false"), "a rolled-back pair: {b}");
}

/// A query that streams a large cross product without materializing it and
/// never matches (each candidate row is checked against the filter, with a
/// cancel check per row): seconds to minutes on the data of
/// [`commit_does_not_wait_for_a_long_snapshot_query`].
const LONG_ASK: &str = "ASK { ?a <http://ex/p> ?x . ?b <http://ex/p> ?y . ?c <http://ex/p> ?z \
                        FILTER(STRLEN(STR(?x)) + STRLEN(STR(?y)) + STRLEN(STR(?z)) < 0) }";

/// How long [`LONG_ASK`] runs uninterrupted on the data of
/// [`commit_does_not_wait_for_a_long_snapshot_query`] (a manual check that
/// it is a long query: `cargo test --test dt_snapshot_disk -- --ignored`).
#[test]
#[ignore = "takes as long as the query runs"]
fn long_ask_runs_for_long() {
    let srv = Srv::start("longask");
    srv.build_disk("l", &pairs("b", 0, 400));
    let started = Instant::now();
    let (s, b) = srv.req("POST", "/sparql?db_name=l", LONG_ASK);
    println!("LONG_ASK: {s} {b} in {:?}", started.elapsed());
}

/// `/commit` does not wait for a long non-transactional read of the
/// transaction's snapshot: the commit's writer gate cancels it (it then
/// re-runs on the committed state), and the commit completes in under a
/// second.
#[test]
fn commit_does_not_wait_for_a_long_snapshot_query() {
    let srv = Arc::new(Srv::start("long"));
    srv.build_disk("l", &pairs("b", 0, 400));
    let tid = srv.begin("l");
    srv.tquery(
        "l",
        &tid,
        "INSERT DATA { <http://ex/n> <http://ex/p> <http://ex/m> }",
    );
    let expired_before = gstore::http_api::expired_snapshot_reads();
    let reader = {
        let srv = Arc::clone(&srv);
        thread::spawn(move || srv.req("POST", "/sparql?db_name=l", LONG_ASK))
    };
    // Wait until the read runs (it is listed as a task), and a bit more.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, list) = srv.req("GET", "/tasklist", "");
        if list.contains("STRLEN") {
            break;
        }
        assert!(Instant::now() < deadline, "the long read never started");
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(500));
    assert!(!reader.is_finished(), "the read must still run");
    let started = Instant::now();
    let (s, b) = srv.req("POST", &format!("/commit?db_name=l&tid={tid}"), "");
    let took = started.elapsed();
    assert_eq!(s, 200, "commit: {b}");
    assert!(
        took < Duration::from_secs(1),
        "the commit waited for the snapshot read: {took:?}"
    );
    // The read expired (and re-runs on the live database): cancel it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while gstore::http_api::expired_snapshot_reads() == expired_before {
        assert!(Instant::now() < deadline, "the snapshot read never expired");
        thread::sleep(Duration::from_millis(10));
    }
    let (_, list) = srv.req("GET", "/tasklist", "");
    let id = list
        .split("\"opt_id\":\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .or_else(|| {
            list.split("\"task_id\":")
                .nth(1)
                .and_then(|r| r.split([',', '}']).next())
        })
        .map(str::to_string);
    if let Some(id) = id {
        srv.req("POST", &format!("/cancel?task_id={id}&opt_id={id}"), "");
    }
    let (s, b) = reader.join().unwrap();
    assert!(
        s == 200 || b.contains("cancel"),
        "the read either answered or was cancelled: {s} {b}"
    );
    let (_, b) = srv.req(
        "POST",
        "/sparql?db_name=l",
        "ASK { <http://ex/n> <http://ex/p> <http://ex/m> }",
    );
    assert!(b.contains("true"), "committed: {b}");
}

/// Unloading (and loading again) while readers query, with and without a
/// transaction's snapshot in play: every read answers cleanly — the data
/// (200) or "not loaded" (404) — never a server error.
#[test]
fn unloading_during_reads_gives_clean_errors() {
    let srv = Arc::new(Srv::start("unload"));
    srv.build_disk("u", &pairs("b", 0, 200));
    let stop = Arc::new(AtomicBool::new(false));
    let answered = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicUsize::new(0));
    let readers: Vec<_> = (0..6)
        .map(|_| {
            let (srv, stop, answered, refused) = (
                Arc::clone(&srv),
                Arc::clone(&stop),
                Arc::clone(&answered),
                Arc::clone(&refused),
            );
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match srv.count("u") {
                        Ok(n) => {
                            assert_eq!(n, 400, "a wrong count");
                            answered.fetch_add(1, Ordering::SeqCst);
                        }
                        Err((404, _)) => {
                            refused.fetch_add(1, Ordering::SeqCst);
                        }
                        Err((s, b)) => panic!("an unclean answer: {s} {b}"),
                    }
                }
            })
        })
        .collect();
    for round in 0..6 {
        if round % 2 == 0 {
            // A transaction's snapshot serves the readers, then it ends.
            let tid = srv.begin("u");
            srv.tquery(
                "u",
                &tid,
                "INSERT DATA { <http://ex/t> <http://ex/p> <http://ex/t2> }",
            );
            thread::sleep(Duration::from_millis(50));
            let (s, b) = srv.req("POST", &format!("/rollback?db_name=u&tid={tid}"), "");
            assert_eq!(s, 200, "{b}");
        }
        let (s, b) = srv.req("POST", "/unload?db_name=u", "");
        assert_eq!(s, 200, "unload: {b}");
        thread::sleep(Duration::from_millis(30));
        let (s, b) = srv.req("POST", "/load?db_name=u", "");
        assert_eq!(s, 200, "load: {b}");
        thread::sleep(Duration::from_millis(30));
    }
    stop.store(true, Ordering::SeqCst);
    for r in readers {
        r.join().expect("a reader got an unclean answer");
    }
    assert!(answered.load(Ordering::SeqCst) > 0);
}

/// The queries `layout=disk` and `layout=snapshot` builds are compared on.
const QUERIES: &[&str] = &[
    "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o } ORDER BY ?s ?o",
    "SELECT ?o WHERE { <http://ex/bs3> ?p ?o } ORDER BY ?o",
    "SELECT ?s WHERE { ?s <http://ex/q> <http://ex/bs7> }",
    "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }",
    "SELECT ?a ?c WHERE { ?a <http://ex/p> ?b . ?b <http://ex/q> ?c } ORDER BY ?a LIMIT 20",
    "ASK { <http://ex/bs10> <http://ex/p> <http://ex/bo10> }",
];

/// `/api build` with `layout=disk` (a bulk build into the staging directory)
/// gives a database that answers exactly like the `layout=snapshot` build of
/// the same file, inside a transaction's snapshot too; an unknown layout
/// is refused.
#[test]
fn api_build_layout_disk_answers_like_snapshot() {
    let srv = Srv::start("apibuild");
    let upload = srv.root.join("upload");
    std::fs::create_dir_all(&upload).unwrap();
    let file = upload.join("d.nt");
    std::fs::write(&file, pairs("b", 0, 40)).unwrap();
    let path = file.to_string_lossy().into_owned();
    for (db, layout) in [("ondisk", "disk"), ("insnap", "snapshot")] {
        let j = srv.api(&[
            ("operation", "build"),
            ("db_name", db),
            ("db_path", &path),
            ("layout", layout),
        ]);
        assert_eq!(j.get("StatusCode").and_then(Json::as_i64), Some(0), "{j}");
        assert_eq!(j.get("success_num").and_then(Json::as_i64), Some(80), "{j}");
        let j = srv.api(&[("operation", "load"), ("db_name", db)]);
        assert_eq!(j.get("StatusCode").and_then(Json::as_i64), Some(0), "{j}");
    }
    assert!(srv.root.join("ondisk.db/kvstore.kv").is_file());
    assert!(srv.root.join("insnap.db/store.bin").is_file());
    let answers = |db: &str| -> Vec<String> {
        QUERIES
            .iter()
            .map(|q| {
                let (s, b) = srv.req("POST", &format!("/sparql?db_name={db}"), q);
                assert_eq!(s, 200, "{q}: {b}");
                b
            })
            .collect()
    };
    let snap = answers("insnap");
    assert_eq!(answers("ondisk"), snap);
    // From a transaction's (on-disk) snapshot as well.
    let tid = srv.begin("ondisk");
    srv.tquery(
        "ondisk",
        &tid,
        "INSERT DATA { <http://ex/new> <http://ex/p> <http://ex/x> }",
    );
    assert_eq!(answers("ondisk"), snap);
    let (s, _) = srv.req("POST", &format!("/rollback?db_name=ondisk&tid={tid}"), "");
    assert_eq!(s, 200);
    let j = srv.api(&[
        ("operation", "build"),
        ("db_name", "bad"),
        ("db_path", &path),
        ("layout", "papyrus"),
    ]);
    assert_eq!(
        j.get("StatusCode").and_then(Json::as_i64),
        Some(1003),
        "{j}"
    );
    assert!(!srv.root.join("bad.db").exists());
    if !cfg!(feature = "rocksdb") {
        let j = srv.api(&[
            ("operation", "build"),
            ("db_name", "rocks"),
            ("db_path", &path),
            ("layout", "rocksdb"),
        ]);
        assert_eq!(
            j.get("StatusCode").and_then(Json::as_i64),
            Some(1003),
            "{j}"
        );
        assert!(j.to_string().contains("rocksdb feature"), "{j}");
    }
}
