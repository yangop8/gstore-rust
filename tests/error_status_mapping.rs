//! The HTTP status of a failure follows its kind, the same on every
//! endpoint (review design#3): `GStoreError` was a string to the protocol
//! layers, which classified it by the endpoint that saw it — a store read
//! error answered `400 Bad Request` (a client error), the same lock
//! conflict was `500` on `/load` and `/upload` but `409` on `/drop`,
//! `/build` and `/restore`, an I/O error on the lock file was a `409`
//! conflict there, and a cancelled statement was a `400` query error.
//!
//! Now a lock another process holds is `409` on every REST endpoint, an I/O
//! error taking the lock (the lock file cannot be opened: a directory stands
//! where it should be, as EACCES would when not running as root) is `500`,
//! a store read error `500`, a cancelled statement `503`, a syntax error
//! still `400`; the single-database server answers the same; the upstream
//! `/api` keeps HTTP 200 and its `StatusCode`s.
//!
//! "Another process" is an open file description of `<db>/LOCK` that this
//! test locks itself: the advisory lock (`flock`) excludes every other
//! description of the file, in this process or another, and the server's
//! registry of its own locks does not know it. Synchronisation is by test
//! hooks (a parked statement), never by sleeping.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::bulk::{self, BulkInput, BulkOptions, BulkTarget};
use gstore::db::{DiskQueryMode, OpenOptions as DbOpenOptions};
use gstore::http_api::{debug_park_txn, ApiServer, TxnParkPoint};
use gstore::json::{self, Json};
use gstore::parser::import::{ImportOptions, RdfFormat};
use gstore::server::{Server, ShutdownReport};
use gstore::Database;

const HANG_GUARD: Duration = Duration::from_secs(60);
const ROOT_BASIC: &str = "Basic cm9vdDpyb290"; // root:root
const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/a> <http://ex/q> \"x\" .\n";

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "gstore_err_status_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request(addr: SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nAuthorization: {ROOT_BASIC}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {resp:?}"));
    let body = resp.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, body.to_string())
}

fn api(addr: SocketAddr, pairs: &[(&str, &str)]) -> Json {
    let mut items = vec![
        format!("{}: {}", json::quote("username"), json::quote("root")),
        format!("{}: {}", json::quote("password"), json::quote("root")),
    ];
    items.extend(
        pairs
            .iter()
            .map(|(k, v)| format!("{}: {}", json::quote(k), json::quote(v))),
    );
    let (st, body) = request(addr, "POST", "/api", &format!("{{{}}}", items.join(", ")));
    assert_eq!(st, 200, "/api keeps HTTP 200: {body}");
    json::parse(&body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode").and_then(Json::as_i64).unwrap()
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    serving: Option<thread::JoinHandle<ShutdownReport>>,
    root: TmpDir,
}

impl Srv {
    fn start(tag: &str) -> Srv {
        Srv::start_with(tag, |s| s)
    }

    fn start_with(tag: &str, f: impl FnOnce(ApiServer) -> ApiServer) -> Srv {
        let root = TmpDir::new(tag);
        Srv::serve(root, f)
    }

    fn serve(root: TmpDir, f: impl FnOnce(ApiServer) -> ApiServer) -> Srv {
        let server = ApiServer::bind(&root.0, "root", "127.0.0.1:0")
            .expect("bind")
            .with_shutdown_grace(Duration::from_millis(100));
        let server = Arc::new(f(server));
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = Some(thread::spawn(move || s2.serve_forever()));
        Srv {
            server,
            addr,
            serving,
            root,
        }
    }

    fn db_dir(&self, db: &str) -> PathBuf {
        self.root.0.join(format!("{db}.db"))
    }

    fn req(&self, method: &str, target: &str, body: &str) -> (u16, String) {
        request(self.addr, method, target, body)
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(h) = self.serving.take() {
            let _ = h.join();
        }
    }
}

/// `<dir>/LOCK`, locked through a description of its own — what another
/// process holding the database looks like. Released when dropped.
struct ForeignLock(File);

impl ForeignLock {
    fn take(dir: &Path) -> ForeignLock {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("LOCK"))
            .unwrap();
        f.try_lock().expect("the test locks the database directory");
        ForeignLock(f)
    }
}

impl Drop for ForeignLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// A built, backed-up and unloaded database `db`: its backup's path.
fn built_unloaded(srv: &Srv, db: &str) -> String {
    let (st, body) = srv.req("POST", &format!("/build?db_name={db}"), DATA);
    assert_eq!(st, 200, "build: {body}");
    let (st, body) = srv.req("POST", &format!("/backup?db_name={db}"), "");
    assert_eq!(st, 200, "backup: {body}");
    let path = json::parse(&body)
        .unwrap()
        .get("path")
        .and_then(Json::as_str)
        .unwrap()
        .to_string();
    let (st, body) = srv.req("POST", &format!("/unload?db_name={db}"), "");
    assert_eq!(st, 200, "unload: {body}");
    path
}

/// Every REST endpoint that opens, rewrites, swaps or deletes the directory
/// of unloaded database `db` (`backup`: a backup of it): (endpoint, status,
/// body).
fn lifecycle_answers(srv: &Srv, db: &str, backup: &str) -> Vec<(String, u16, String)> {
    let mut out = Vec::new();
    for (target, body) in [
        (format!("/load?db_name={db}"), ""),
        (format!("/upload?db_name={db}&format=ntriples"), DATA),
        (format!("/build?db_name={db}"), DATA),
        (format!("/build?db_name={db}&layout=disk"), DATA),
        (format!("/restore?db_name={db}&path={}", enc(backup)), ""),
        (format!("/drop?db_name={db}"), ""),
    ] {
        let (st, text) = srv.req("POST", &target, body);
        out.push((target, st, text));
    }
    out
}

#[test]
fn a_lock_another_process_holds_is_409_on_every_rest_endpoint() {
    let srv = Srv::start("locked");
    let backup = built_unloaded(&srv, "lockdb");
    let held = ForeignLock::take(&srv.db_dir("lockdb"));
    for (target, st, body) in lifecycle_answers(&srv, "lockdb", &backup) {
        assert_eq!(st, 409, "{target}: a lock conflict is a conflict: {body}");
        assert!(
            body.contains("locked by another process"),
            "{target}: {body}"
        );
    }
    // Nothing changed meanwhile.
    assert!(srv.db_dir("lockdb").join("LOCK").is_file());
    // `/api` keeps its `StatusCode`s (upstream's 1005 for a failed load or
    // drop), with HTTP 200.
    let j = api(srv.addr, &[("operation", "load"), ("db_name", "lockdb")]);
    assert_eq!(code(&j), 1005, "{j}");
    let j = api(
        srv.addr,
        &[
            ("operation", "drop"),
            ("db_name", "lockdb"),
            ("is_backup", "false"),
        ],
    );
    assert_eq!(code(&j), 1005, "{j}");
    // Released, the database loads and holds its data.
    drop(held);
    let (st, body) = srv.req("POST", "/load?db_name=lockdb", "");
    assert_eq!(st, 200, "{body}");
    let (st, body) = srv.req(
        "GET",
        "/sparql?db_name=lockdb&sparql=ASK%7B%3Fs%20%3Fp%20%3Fo%7D",
        "",
    );
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("true"), "{body}");
}

#[test]
fn an_io_error_taking_the_lock_is_500_not_409() {
    let srv = Srv::start("lockio");
    let backup = built_unloaded(&srv, "lockio");
    // The lock file cannot be opened for writing: a directory stands in its
    // place (the stand-in for EACCES, which root never gets).
    let lock = srv.db_dir("lockio").join("LOCK");
    fs::remove_file(&lock).unwrap();
    fs::create_dir(&lock).unwrap();
    for (target, st, body) in lifecycle_answers(&srv, "lockio", &backup) {
        assert_eq!(
            st, 500,
            "{target}: an I/O error is the server's fault, not a conflict: {body}"
        );
        assert!(
            !body.contains("locked by another process"),
            "{target}: {body}"
        );
    }
    let j = api(srv.addr, &[("operation", "load"), ("db_name", "lockio")]);
    assert_eq!(code(&j), 1005, "{j}");
    fs::remove_dir(&lock).unwrap();
    let (st, body) = srv.req("POST", "/load?db_name=lockio", "");
    assert_eq!(st, 200, "{body}");
}

/// Header root slot of the POS tree in `kvstore.kv` (`kvstore::format`).
const SLOT_POS: usize = 1;

/// A Stream-layout database `<parent>/<name>.db` whose POS index root page
/// is overwritten with garbage: a query by predicate fails reading it.
fn broken_disk_db(parent: &Path, name: &str) -> PathBuf {
    let mut text = String::new();
    for i in 0..300 {
        text.push_str(&format!(
            "<http://ex/s{i}> <http://ex/p{}> <http://ex/o{i}> .\n",
            i % 3
        ));
    }
    let db = parent.join(format!("{name}.db"));
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
    let kv = db.join("kvstore.kv");
    let mut bytes = fs::read(&kv).unwrap();
    let off = 16 + SLOT_POS * 8;
    let page = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap()) as usize;
    assert!(page > 0, "the POS tree has a root page");
    bytes[page * 4096..(page + 1) * 4096].fill(0xFF);
    fs::write(&kv, bytes).unwrap();
    db
}

fn stream_mode() -> DbOpenOptions {
    DbOpenOptions {
        disk_query_mode: DiskQueryMode::Stream,
        ..DbOpenOptions::default()
    }
}

const BY_PREDICATE: &str = "SELECT ?s ?o WHERE { ?s <http://ex/p1> ?o }";

#[test]
fn a_store_read_error_is_500_not_400() {
    let root = TmpDir::new("readerr");
    broken_disk_db(&root.0, "readfail");
    let srv = Srv::serve(root, |s| s.with_open_options(stream_mode()));
    let (st, body) = srv.req("POST", "/load?db_name=readfail", "");
    assert_eq!(st, 200, "{body}");
    let (st, body) = srv.req(
        "GET",
        &format!("/sparql?db_name=readfail&sparql={}", enc(BY_PREDICATE)),
        "",
    );
    assert_eq!(
        st, 500,
        "a store read error is the server's fault, not a bad request: {body}"
    );
    assert!(body.contains("read from the store failed"), "{body}");
    // `/api` answers it as before: 1005, HTTP 200.
    let j = api(
        srv.addr,
        &[
            ("operation", "query"),
            ("db_name", "readfail"),
            ("format", "json"),
            ("sparql", BY_PREDICATE),
        ],
    );
    assert_eq!(code(&j), 1005, "{j}");
}

#[test]
fn the_single_database_server_maps_a_store_read_error_to_500() {
    let root = TmpDir::new("single");
    let dir = broken_disk_db(&root.0, "sf");
    let db = Database::open_with(&dir, &stream_mode()).unwrap();
    let server = Arc::new(
        Server::bind(db, "127.0.0.1:0")
            .unwrap()
            .with_shutdown_grace(Duration::from_millis(100)),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    let (st, body) = request(
        addr,
        "GET",
        &format!("/sparql?query={}", enc(BY_PREDICATE)),
        "",
    );
    assert_eq!(st, 500, "{body}");
    assert!(body.contains("read from the store failed"), "{body}");
    // A syntax error stays a client error.
    let (st, body) = request(addr, "GET", "/sparql?query=SELECT%20WHERE", "");
    assert_eq!(st, 400, "{body}");
    let (st, body) = request(addr, "POST", "/update", "INSERT DATA { <a> }");
    assert_eq!(st, 400, "{body}");
    server.shutdown();
    let _ = serving.join();
}

/// The id of the running task of `db` in `/tasklist`.
fn running_task(srv: &Srv, db: &str) -> u64 {
    let (st, list) = srv.req("GET", "/tasklist", "");
    assert_eq!(st, 200, "{list}");
    let entry = list
        .split("{\"id\":")
        .skip(1)
        .find(|e| e.contains(&format!("\"db\":\"{db}\"")))
        .unwrap_or_else(|| panic!("no running task of {db}: {list}"));
    entry
        .split([',', '}'])
        .next()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| panic!("{list}"))
}

#[test]
fn a_cancelled_statement_is_503_not_400() {
    let srv = Srv::start("cancel");
    let (st, body) = srv.req("POST", "/build?db_name=cancelq", DATA);
    assert_eq!(st, 200, "{body}");
    // The statement holds its database and waits here until released.
    let park = debug_park_txn("cancelq", TxnParkPoint::StatementHolding);
    let addr = srv.addr;
    let query = thread::spawn(move || {
        request(
            addr,
            "GET",
            "/sparql?db_name=cancelq&sparql=SELECT%20*%20WHERE%20%7B%3Fs%20%3Fp%20%3Fo%7D",
            "",
        )
    });
    assert!(park.wait_parked(HANG_GUARD), "the query never ran");
    let id = running_task(&srv, "cancelq");
    let (st, body) = srv.req("POST", &format!("/cancel?id={id}"), "");
    assert_eq!(st, 200, "{body}");
    park.release();
    let (st, body) = query.join().unwrap();
    assert_eq!(
        st, 503,
        "a cancelled statement is a refused request, not a bad one: {body}"
    );
    assert!(body.contains("cancelled"), "{body}");
}

#[test]
fn syntax_errors_stay_400_and_api_codes_are_unchanged() {
    let srv = Srv::start("syntax");
    let (st, body) = srv.req("POST", "/build?db_name=syntax", DATA);
    assert_eq!(st, 200, "{body}");
    let (st, body) = srv.req("GET", "/sparql?db_name=syntax&sparql=SELECT%20WHERE", "");
    assert_eq!(st, 400, "{body}");
    let (st, body) = srv.req("POST", "/update?db_name=syntax", "INSERT DATA { <a> }");
    assert_eq!(st, 400, "{body}");
    let (st, body) = srv.req("POST", "/build?db_name=syntaxbad", "<a> <b> .");
    assert_eq!(st, 400, "an RDF syntax error is a bad request: {body}");
    let j = api(
        srv.addr,
        &[
            ("operation", "query"),
            ("db_name", "syntax"),
            ("sparql", "SELECT WHERE"),
        ],
    );
    assert_eq!(code(&j), 1005, "{j}");
}

/// The lock primitives type their failures (review design#3): a lock held
/// elsewhere is `Locked` (a conflict), a lock that cannot be taken at all is
/// an I/O error (a storage failure).
#[test]
fn the_directory_locks_type_their_errors() {
    use gstore::db::dirlock::{BuildGuard, DirLock};
    use gstore::error::ErrorKind;
    let root = TmpDir::new("lockkinds");
    let dir = root.0.join("k.db");
    fs::create_dir_all(&dir).unwrap();
    // Held by "another process" (a description of its own).
    let held = ForeignLock::take(&dir);
    let e = DirLock::acquire(&dir).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Locked, "{e}");
    assert!(e.to_string().contains("locked by another process"), "{e}");
    drop(held);
    drop(DirLock::acquire(&dir).unwrap());
    // The lock file cannot be opened: an I/O error.
    fs::remove_file(dir.join("LOCK")).unwrap();
    fs::create_dir(dir.join("LOCK")).unwrap();
    let e = DirLock::acquire(&dir).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Storage, "{e}");
    assert!(
        e.to_string().contains("cannot lock database directory"),
        "{e}"
    );
    // A directory that is not there cannot be locked either.
    let e = DirLock::acquire(&root.0.join("missing.db")).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Storage, "{e}");
    // A second bulk build of the same directory in this process.
    let guard = BuildGuard::enter(&dir).unwrap();
    let e = BuildGuard::enter(&dir).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Locked, "{e}");
    drop(guard);
    drop(BuildGuard::enter(&dir).unwrap());
}
