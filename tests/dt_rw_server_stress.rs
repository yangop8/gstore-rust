//! Data test (Phase 6, R3-SRV): the HTTP servers under concurrent load —
//! eight readers, one writer and (on the `/api` server) a transaction cycle
//! at once, against the shared-read / exclusive-write locking of
//! [`Server`] and [`ApiServer`].
//!
//! What must hold throughout:
//!
//! * a write of two operations in one request is seen as both or neither,
//!   never one (every reader checks every pair it sees);
//! * the data of a transaction is never visible to anyone else before it
//!   commits, and the data of a rolled-back one never at all;
//! * the owner of a transaction reads its own writes;
//! * after a graceful shutdown the reopened database holds exactly the
//!   acknowledged writes (the ledger the clients kept).
//!
//! Each server runs over an in-memory (snapshot) database and over an
//! on-disk (`kvstore.kv`) one in Stream mode, whose shared reads go straight
//! to `DiskTriples` and the out-of-core dictionary.
//!
//! Each test runs `GSTORE_RW_STRESS_SECS` seconds of load (default 2) under
//! a 120 s watchdog that aborts the process if the servers wedge.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::server::Server;
use gstore::{Database, OpenOptions, QueryResult};

const BASE: &str = "<http://ex/base> <http://ex/p> <http://ex/o> .\n";
const PAIR: &str = "http://ex/pair";
const READERS: usize = 8;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// How long each test drives the server.
fn load_time() -> Duration {
    let secs = std::env::var("GSTORE_RW_STRESS_SECS")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .unwrap_or(2.0);
    Duration::from_secs_f64(secs)
}

/// Aborts the whole process when a test runs longer than 120 s (a wedged
/// server would otherwise hang the suite); disarmed when dropped.
struct Watchdog(Option<mpsc::Sender<()>>);

fn watchdog(name: &'static str) -> Watchdog {
    let (tx, rx) = mpsc::channel::<()>();
    thread::spawn(move || {
        if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(Duration::from_secs(120)) {
            eprintln!("watchdog: {name} ran longer than 120 s; aborting");
            std::process::abort();
        }
    });
    Watchdog(Some(tx))
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.take();
    }
}

/// A temp directory removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("gstore_rwstress_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One HTTP/1.1 request (`Connection: close`): `(status, body)`.
fn request(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    auth: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: x\r\n");
    if let Some(a) = auth {
        raw.push_str(&format!("Authorization: {a}\r\n"));
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

/// `Basic root:root`.
const ROOT: &str = "Basic cm9vdDpyb290";

/// The bindings of a SPARQL JSON result (the REST shape and the `/api`
/// shape agree on `results.bindings[*].<var>.value`).
fn column(body: &str, var: &str) -> Vec<String> {
    let j = json::parse(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"));
    j.get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("no bindings: {body}"))
        .iter()
        .filter_map(|b| b.get(var)?.get("value")?.as_str().map(str::to_string))
        .collect()
}

/// The pair query: every pair id with how many of its two triples it has.
const PAIRS_Q: &str = "SELECT ?o (COUNT(?s) AS ?c) WHERE { ?s <http://ex/pair> ?o } GROUP BY ?o";

/// Check a pair-query answer: each pair is whole. Returns how many pairs.
fn check_pairs(body: &str, who: &str) -> usize {
    let counts = column(body, "c");
    for c in &counts {
        assert_eq!(c, "2", "{who} saw half a two-operation write: {body}");
    }
    counts.len()
}

/// `INSERT` (or `DELETE`) pair `i` as two operations of one request.
fn pair_update(i: u64, insert: bool) -> String {
    let verb = if insert { "INSERT" } else { "DELETE" };
    format!(
        "{verb} DATA {{ <http://ex/pair/{i}/a> <{PAIR}> \"{i}\" }} ; \
         {verb} DATA {{ <http://ex/pair/{i}/b> <{PAIR}> \"{i}\" }}"
    )
}

/// The pair ids and the given predicate's object values in `db`.
fn reopened(db: &mut Database, pred: &str) -> BTreeSet<String> {
    match db
        .query(&format!("SELECT DISTINCT ?o WHERE {{ ?s <{pred}> ?o }}"))
        .unwrap()
    {
        QueryResult::Select(rs) => rs
            .rows
            .iter()
            .filter_map(|r| r[0].clone())
            .map(|v| {
                v.trim_matches('"')
                    .split('"')
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .collect(),
        other => panic!("expected Select, got {other:?}"),
    }
}

/// The writer: inserts pairs and deletes some again (two operations per
/// request) until `stop`, through `send` (which returns the HTTP status and
/// body). Returns the ledger of acknowledged pairs.
fn write_pairs(stop: &AtomicBool, send: impl Fn(&str) -> (u16, String)) -> BTreeSet<u64> {
    let mut ledger = BTreeSet::new();
    let mut next = 0u64;
    while !stop.load(Ordering::SeqCst) {
        let i = next;
        next += 1;
        let (s, b) = send(&pair_update(i, true));
        if s == 200 {
            assert!(
                b.contains("\"changed\":2") || b.contains("\"AnsNum\":2"),
                "{b}"
            );
            ledger.insert(i);
        } else {
            // Refused while a transaction is open (409): not applied.
            assert_eq!(s, 409, "insert pair {i}: {b}");
        }
        if i % 3 == 2 {
            let j = i - 2;
            if ledger.contains(&j) {
                let (s, b) = send(&pair_update(j, false));
                if s == 200 {
                    assert!(b.contains("\"changed\":2"), "{b}");
                    ledger.remove(&j);
                } else {
                    assert_eq!(s, 409, "delete pair {j}: {b}");
                }
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    ledger
}

// ---------------------------------------------------------------------------
// The single-database server
// ---------------------------------------------------------------------------

#[test]
fn single_db_server_keeps_writes_whole_under_concurrent_reads() {
    let _dog = watchdog("single_db_server_keeps_writes_whole_under_concurrent_reads");
    let dir = TmpDir::new("single");
    Database::build_from_str("stress", BASE)
        .unwrap()
        .save(&dir.0)
        .unwrap();
    single_db_stress(&dir.0, &OpenOptions::default(), "single-db");
}

/// The same over an on-disk (`kvstore.kv`) database served in Stream mode:
/// the shared-read paths run straight over `DiskTriples` and the out-of-core
/// dictionary (whose writer interns new terms under the exclusive guard).
#[test]
fn single_db_server_on_a_stream_disk_database_keeps_writes_whole() {
    let _dog = watchdog("single_db_server_on_a_stream_disk_database_keeps_writes_whole");
    let dir = TmpDir::new("single_stream");
    build_disk_base(&dir.0);
    single_db_stress(&dir.0, &OpenOptions::stream(), "single-db (stream disk)");
}

/// An on-disk (`kvstore.kv`) database holding [`BASE`] in `dir` (the bulk
/// build).
fn build_disk_base(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let nt = dir.with_extension("base.nt");
    std::fs::write(&nt, BASE).unwrap();
    Database::build_disk(dir, &[&nt]).unwrap();
    std::fs::remove_file(&nt).unwrap();
    assert!(Database::is_disk(dir));
}

/// Serve the database in `dir` (opened with `options`) on a single-database
/// server under eight readers and a writer of pairs; then check the
/// reopened database against the writer's ledger.
fn single_db_stress(dir: &Path, options: &OpenOptions, label: &str) {
    let server = Arc::new(
        Server::bind(Database::open_with(dir, options).unwrap(), "127.0.0.1:0")
            .unwrap()
            .with_db_dir(dir),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let readers: Vec<_> = (0..READERS)
        .map(|r| {
            let (stop, reads) = (Arc::clone(&stop), Arc::clone(&reads));
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let (s, b) = request(&addr, "POST", "/sparql", None, PAIRS_Q);
                    assert_eq!(s, 200, "{b}");
                    check_pairs(&b, &format!("reader {r}"));
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    let writer = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || write_pairs(&stop, |u| request(&addr, "POST", "/update", None, u)))
    };
    thread::sleep(load_time());
    stop.store(true, Ordering::SeqCst);
    for r in readers {
        r.join().unwrap();
    }
    let ledger = writer.join().unwrap();
    let reads = reads.load(Ordering::Relaxed);
    eprintln!(
        "{label}: {reads} reads, {} pairs acknowledged",
        ledger.len()
    );
    assert!(reads > 0);
    assert!(!ledger.is_empty());
    server.shutdown();
    serving.join().unwrap();
    drop(server);

    let mut db = Database::open_with(dir, options).unwrap();
    let on_disk = reopened(&mut db, PAIR);
    let expected: BTreeSet<String> = ledger.iter().map(u64::to_string).collect();
    assert_eq!(on_disk, expected, "the reopened database is the ledger");
    match db.query(PAIRS_Q).unwrap() {
        QueryResult::Select(rs) => {
            assert!(rs
                .rows
                .iter()
                .all(|r| r[1].as_deref().unwrap_or("").starts_with("\"2\"")))
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The multi-database server: readers, a writer and transactions
// ---------------------------------------------------------------------------

/// The REST and `/api` reads a reader alternates between.
fn api_read(addr: &SocketAddr, n: u64, sparql: &str) -> String {
    if n.is_multiple_of(2) {
        let (s, b) = request(addr, "POST", "/sparql?db_name=stress", Some(ROOT), sparql);
        assert_eq!(s, 200, "{b}");
        b
    } else {
        let body = Json::obj()
            .with("username", "root")
            .with("password", "root")
            .with("operation", "query")
            .with("db_name", "stress")
            .with("sparql", sparql)
            .to_string();
        let (s, b) = request(addr, "POST", "/api", None, &body);
        assert_eq!(s, 200, "{b}");
        assert!(b.contains("\"StatusCode\":0"), "{b}");
        b
    }
}

/// A transaction step through REST: `(status, body)`.
fn txn_step(addr: &SocketAddr, target: &str, body: &str) -> (u16, String) {
    request(addr, "POST", target, Some(ROOT), body)
}

/// The transaction id of a `/begin` answer.
fn tid_of(body: &str) -> u64 {
    let j = json::parse(body).unwrap_or_else(|e| panic!("{e}: {body}"));
    let tid = j.get("TID").unwrap_or_else(|| panic!("no TID: {body}"));
    tid.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| tid.as_i64().and_then(|v| u64::try_from(v).ok()))
        .unwrap_or_else(|| panic!("bad TID: {body}"))
}

#[test]
fn api_server_isolates_transactions_and_keeps_writes_whole_under_concurrent_reads() {
    let _dog = watchdog("api_server_isolates_transactions");
    let root = TmpDir::new("api");
    api_stress(&root.0, false, "api");
}

/// The same over an on-disk (`kvstore.kv`) database, which the server opens
/// in the process's disk query mode: Stream unless `GSTORE_DISK_QUERY_MODE`
/// says otherwise (this test binary installs Stream as its default; CI's
/// test job runs it in both modes). Transactions run in the store's overlay;
/// `/begin` copies the committed triples for the readers' snapshot.
#[test]
fn api_server_on_a_disk_database_isolates_transactions_and_keeps_writes_whole() {
    let _dog = watchdog("api_server_on_a_disk_database");
    gstore::set_default_open_options(OpenOptions::stream());
    let root = TmpDir::new("api_disk");
    build_disk_base(&root.0.join("stress.db"));
    api_stress(&root.0, true, "api (disk)");
}

/// Serve database `stress` from root `root` on an `ApiServer` — built there
/// with REST `/build`, or (`on_disk`) already built there and loaded —
/// under eight readers, a writer of pairs and a transaction cycle; then
/// check the reopened database against the clients' ledgers.
fn api_stress(root: &Path, on_disk: bool, label: &str) {
    let server = Arc::new(ApiServer::bind(root, "root", "127.0.0.1:0").unwrap());
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    let (s, b) = if on_disk {
        request(&addr, "POST", "/load?db_name=stress", Some(ROOT), "")
    } else {
        request(&addr, "POST", "/build?db_name=stress", Some(ROOT), BASE)
    };
    assert_eq!(s, 200, "{b}");

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    // Values of committed-transaction triples any reader saw.
    let seen_committed = Arc::new(Mutex::new(BTreeSet::<String>::new()));
    let readers: Vec<_> = (0..READERS)
        .map(|r| {
            let (stop, reads, seen) = (
                Arc::clone(&stop),
                Arc::clone(&reads),
                Arc::clone(&seen_committed),
            );
            thread::spawn(move || {
                let mut n = r as u64;
                while !stop.load(Ordering::SeqCst) {
                    n += 1;
                    let who = format!("reader {r}");
                    check_pairs(&api_read(&addr, n, PAIRS_Q), &who);
                    // Uncommitted (pending) and rolled-back data is never seen.
                    let b = api_read(
                        &addr,
                        n,
                        "SELECT ?o WHERE { { ?s <http://ex/pending> ?o } UNION \
                         { ?s <http://ex/rolledback> ?o } }",
                    );
                    assert!(
                        column(&b, "o").is_empty(),
                        "{who} saw uncommitted or rolled-back data: {b}"
                    );
                    let b = api_read(&addr, n, "SELECT ?o WHERE { ?s <http://ex/committed> ?o }");
                    seen.lock().unwrap().extend(column(&b, "o"));
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    let writer = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            write_pairs(&stop, |u| {
                request(&addr, "POST", "/update?db_name=stress", Some(ROOT), u)
            })
        })
    };
    // The transaction cycle: begin, write (a pending marker, then the final
    // triple, then remove the marker), read its own writes, then commit or
    // roll back alternately.
    let cycler = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut committed = BTreeSet::new();
            let mut cycles = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let (s, b) = txn_step(&addr, "/begin?db_name=stress", "");
                if s == 409 {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                assert_eq!(s, 200, "begin: {b}");
                let tid = tid_of(&b);
                let k = cycles;
                cycles += 1;
                let commit = k.is_multiple_of(2);
                let pred = if commit { "committed" } else { "rolledback" };
                let tq = |sparql: &str| {
                    let (s, b) =
                        txn_step(&addr, &format!("/tquery?db_name=stress&tid={tid}"), sparql);
                    assert_eq!(s, 200, "tquery {sparql}: {b}");
                    b
                };
                tq(&format!(
                    "INSERT DATA {{ <http://ex/txn/{k}> <http://ex/pending> \"{k}\" }}"
                ));
                let own = tq(&format!(
                    "ASK {{ <http://ex/txn/{k}> <http://ex/pending> \"{k}\" }}"
                ));
                assert!(own.contains("true"), "the owner reads its own write: {own}");
                thread::sleep(Duration::from_millis(10));
                tq(&format!(
                    "INSERT DATA {{ <http://ex/txn/{k}> <http://ex/{pred}> \"{k}\" }}"
                ));
                thread::sleep(Duration::from_millis(10));
                tq(&format!(
                    "DELETE DATA {{ <http://ex/txn/{k}> <http://ex/pending> \"{k}\" }}"
                ));
                let end = if commit { "commit" } else { "rollback" };
                let (s, b) = txn_step(&addr, &format!("/{end}?db_name=stress&tid={tid}"), "");
                assert_eq!(s, 200, "{end}: {b}");
                if commit {
                    committed.insert(k.to_string());
                }
                thread::sleep(Duration::from_millis(15));
            }
            (committed, cycles)
        })
    };
    thread::sleep(load_time());
    stop.store(true, Ordering::SeqCst);
    for r in readers {
        r.join().unwrap();
    }
    let ledger = writer.join().unwrap();
    let (committed, cycles) = cycler.join().unwrap();
    let reads = reads.load(Ordering::Relaxed);
    eprintln!(
        "{label}: {reads} reader rounds, {} pairs acknowledged, {cycles} transactions",
        ledger.len()
    );
    assert!(reads > 0);
    assert!(
        cycles >= 2,
        "at least one commit and one rollback ({cycles})"
    );
    let seen = seen_committed.lock().unwrap().clone();
    assert!(
        seen.is_subset(&committed),
        "a reader saw {seen:?}, committed {committed:?}"
    );
    server.shutdown();
    serving.join().unwrap();
    drop(server);

    let mut db = Database::load(root.join("stress.db")).unwrap();
    assert_eq!(Database::is_disk(root.join("stress.db")), on_disk);
    let expected: BTreeSet<String> = ledger.iter().map(u64::to_string).collect();
    assert_eq!(reopened(&mut db, PAIR), expected, "pairs: the ledger");
    assert_eq!(
        reopened(&mut db, "http://ex/committed"),
        committed,
        "committed transactions: the ledger"
    );
    assert!(reopened(&mut db, "http://ex/rolledback").is_empty());
    assert!(reopened(&mut db, "http://ex/pending").is_empty());
}
