//! Data test: durability, graceful shutdown, transaction isolation and batch
//! safety of the bundled HTTP servers ([`gstore::server::Server`] and
//! [`gstore::http_api::ApiServer`]).
//!
//! Each test boots a server on `127.0.0.1:0` in a background thread and drives
//! it with a hand-rolled `std::net::TcpStream` HTTP/1.1 client; the on-disk
//! state is checked by re-opening the database directory with
//! [`Database::load`]. The last tests run the real `gserver` binary and stop it
//! with `SIGTERM` (unix only).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gstore::http_api::ApiServer;
use gstore::server::{Server, ShutdownReport};
use gstore::{Database, QueryResult};

const DATA: &str = "@prefix : <http://ex/> .\n:a :p :b .\n:a :name \"Alice\" .\n";
const NEW_TRIPLE: &str = "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }";

/// A unique temp directory per test.
fn tmp_dir(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "gstore_durability_{tag}_{}_{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// Send one HTTP/1.1 request and return `(status_code, body)`.
fn request(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    auth: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
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
    split_response(&resp)
}

fn split_response(resp: &str) -> (u16, String) {
    let status = resp
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status code in response: {resp:?}"));
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

/// RFC 4648 base64 (client side of the server's decoder).
fn base64(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn basic(user: &str, pass: &str) -> String {
    format!("Basic {}", base64(format!("{user}:{pass}").as_bytes()))
}

/// Extract a JSON string field's value (naive — fine for flat responses).
fn json_field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    Some(&rest[..rest.find('"')?])
}

/// Whether the database saved at `dir` contains `<s> <p> <o>` (full IRIs).
fn on_disk_has(dir: &Path, s: &str, p: &str, o: &str) -> bool {
    let mut db = Database::load(dir).expect("load saved database");
    match db.query(&format!("ASK {{ <{s}> <{p}> <{o}> }}")).unwrap() {
        QueryResult::Ask(b) => b,
        other => panic!("unexpected {other:?}"),
    }
}

fn on_disk_has_new_triple(dir: &Path) -> bool {
    on_disk_has(dir, "http://ex/c", "http://ex/p", "http://ex/d")
}

// ---- single-database Server --------------------------------------------------

/// Build `DATA` into a fresh directory and serve it from there.
fn start_single(
    tag: &str,
    check_point: bool,
) -> (
    Arc<Server>,
    SocketAddr,
    PathBuf,
    thread::JoinHandle<ShutdownReport>,
) {
    let dir = tmp_dir(tag);
    Database::build_from_str("single", DATA)
        .unwrap()
        .save(&dir)
        .unwrap();
    let server = Server::bind(Database::load(&dir).unwrap(), "127.0.0.1:0")
        .unwrap()
        .with_db_dir(&dir)
        .with_check_point(check_point);
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    (server, addr, dir, serving)
}

#[test]
fn single_server_update_survives_shutdown_and_reload() {
    // check_point off: the update stays in memory until the graceful shutdown.
    let (server, addr, dir, serving) = start_single("single_off", false);
    assert_eq!(server.db_dir(), Some(dir.as_path()));
    let (s, body) = request(&addr, "POST", "/update", None, NEW_TRIPLE);
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("\"changed\":1"), "{body}");
    assert!(
        !on_disk_has_new_triple(&dir),
        "check_point off: nothing is saved before shutdown"
    );
    let (_, status) = request(&addr, "GET", "/status", None, "");
    assert!(status.contains("\"dirty\":true"), "{status}");

    server.shutdown();
    let report = serving
        .join()
        .expect("serve_forever returns after shutdown");
    assert!(report.drained, "{report:?}");
    assert_eq!(report.saved, vec![dir.clone()], "{report:?}");
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(
        on_disk_has_new_triple(&dir),
        "the update must be on disk after a graceful shutdown"
    );
    // The original data is intact too.
    assert!(on_disk_has(
        &dir,
        "http://ex/a",
        "http://ex/p",
        "http://ex/b"
    ));
    // The listener is closed: new connections are refused.
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_err(),
        "a shut-down server must not accept connections"
    );
}

#[test]
fn single_server_check_point_saves_every_update() {
    let (server, addr, dir, serving) = start_single("single_on", true);
    assert!(server.check_point());
    let (s, body) = request(&addr, "POST", "/update", None, NEW_TRIPLE);
    assert_eq!(s, 200, "{body}");
    assert!(
        on_disk_has_new_triple(&dir),
        "check_point on: the update is on disk before it is acknowledged"
    );
    // An update via /sparql is persisted the same way.
    let (s, _) = request(
        &addr,
        "POST",
        "/sparql",
        None,
        "DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }",
    );
    assert_eq!(s, 200);
    assert!(!on_disk_has(
        &dir,
        "http://ex/a",
        "http://ex/p",
        "http://ex/b"
    ));
    let (_, status) = request(&addr, "GET", "/status", None, "");
    assert!(status.contains("\"dirty\":false"), "{status}");
    server.shutdown();
    let report = serving.join().unwrap();
    assert!(report.saved.is_empty(), "nothing left to save: {report:?}");
}

#[test]
fn single_server_without_a_directory_stays_in_memory() {
    let server = Arc::new(
        Server::bind(
            Database::build_from_str("mem", DATA).unwrap(),
            "127.0.0.1:0",
        )
        .unwrap(),
    );
    assert!(server.db_dir().is_none());
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    assert_eq!(request(&addr, "POST", "/update", None, NEW_TRIPLE).0, 200);
    server.shutdown_handle().shutdown();
    let report = serving.join().unwrap();
    assert!(
        report.saved.is_empty() && report.failed.is_empty(),
        "{report:?}"
    );
}

#[test]
fn single_server_shutdown_lets_an_in_flight_request_finish() {
    let (server, addr, dir, serving) = start_single("single_inflight", false);
    // A request whose body arrives only after the shutdown was requested: the
    // connection is already accepted (in flight), so it must still be served.
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "POST /update HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        NEW_TRIPLE.len()
    )
    .unwrap();
    s.flush().unwrap();
    thread::sleep(Duration::from_millis(500)); // let the accept loop hand it off
    server.shutdown();
    thread::sleep(Duration::from_millis(200));
    assert!(
        !serving.is_finished(),
        "serve_forever must wait for the in-flight request"
    );
    s.write_all(NEW_TRIPLE.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let (status, body) = split_response(&resp);
    assert_eq!(status, 200, "{body}");
    let report = serving.join().unwrap();
    assert!(report.drained, "{report:?}");
    assert!(
        on_disk_has_new_triple(&dir),
        "the in-flight update is saved by the shutdown"
    );
}

#[test]
fn single_server_shutdown_wait_is_bounded() {
    let dir = tmp_dir("single_bounded");
    let server = Arc::new(
        Server::bind(Database::build_from_str("b", DATA).unwrap(), "127.0.0.1:0")
            .unwrap()
            .with_db_dir(&dir)
            .with_shutdown_grace(Duration::from_millis(300)),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    // A client that connects and never sends a request pins a worker.
    let _idle = TcpStream::connect(addr).unwrap();
    thread::sleep(Duration::from_millis(500));
    let t0 = Instant::now();
    server.shutdown();
    let report = serving.join().unwrap();
    assert!(
        !report.drained,
        "the idle connection is still open: {report:?}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "shutdown must not wait longer than (about) the grace period"
    );
}

// ---- multi-database ApiServer ----------------------------------------------

struct Api {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    root_dir: PathBuf,
    serving: Option<thread::JoinHandle<ShutdownReport>>,
}

impl Api {
    fn start(tag: &str, check_point: bool) -> Api {
        let root_dir = tmp_dir(tag);
        let server = Arc::new(
            ApiServer::bind(&root_dir, "root", "127.0.0.1:0")
                .expect("bind")
                .with_check_point(check_point),
        );
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = Some(thread::spawn(move || s2.serve_forever()));
        Api {
            server,
            addr,
            root_dir,
            serving,
        }
    }

    fn root(&self, method: &str, target: &str, body: &str) -> (u16, String) {
        request(
            &self.addr,
            method,
            target,
            Some(&basic("root", "root")),
            body,
        )
    }

    fn as_user(&self, user: &str, method: &str, target: &str, body: &str) -> (u16, String) {
        request(&self.addr, method, target, Some(&basic(user, "pw")), body)
    }

    fn db_dir(&self, name: &str) -> PathBuf {
        self.root_dir.join(format!("{name}.db"))
    }

    /// Build database `name` from `DATA`.
    fn build(&self, name: &str) {
        let (s, body) = self.root("POST", &format!("/build?db_name={name}"), DATA);
        assert_eq!(s, 200, "build: {body}");
    }

    fn add_user(&self, name: &str, privileges: &str) {
        let (s, body) = self.root(
            "POST",
            &format!("/user/create?op_username={name}&op_password=pw&privileges={privileges}"),
            "",
        );
        assert_eq!(s, 200, "user/create: {body}");
    }

    fn begin(&self, user: &str, db: &str) -> String {
        let (s, body) = self.as_user_or_root(user, "POST", &format!("/begin?db_name={db}"), "");
        assert_eq!(s, 200, "begin: {body}");
        json_field(&body, "TID").expect("TID").to_string()
    }

    fn as_user_or_root(&self, user: &str, method: &str, target: &str, body: &str) -> (u16, String) {
        if user == "root" {
            self.root(method, target, body)
        } else {
            self.as_user(user, method, target, body)
        }
    }

    fn shutdown(&mut self) -> ShutdownReport {
        self.server.shutdown();
        self.serving.take().unwrap().join().unwrap()
    }
}

#[test]
fn api_unload_saves_a_dirty_database() {
    let api = Api::start("api_unload", false);
    api.build("u");
    assert_eq!(api.root("POST", "/update?db_name=u", NEW_TRIPLE).0, 200);
    assert!(!on_disk_has_new_triple(&api.db_dir("u")), "check_point off");
    let (s, body) = api.root("GET", "/show", "");
    assert_eq!(s, 200);
    assert!(body.contains("\"dirty\":true"), "{body}");

    let (s, body) = api.root("POST", "/unload?db_name=u", "");
    assert_eq!(s, 200, "unload: {body}");
    assert!(body.contains("saved"), "{body}");
    assert!(
        on_disk_has_new_triple(&api.db_dir("u")),
        "/unload must persist unsaved changes"
    );
    // Loading it back serves the saved change.
    assert_eq!(api.root("POST", "/load?db_name=u", "").0, 200);
    let (_, body) = api.root(
        "POST",
        "/sparql?db_name=u",
        "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/d> }",
    );
    assert!(body.contains("http://ex/c"), "{body}");
}

#[test]
fn api_check_point_off_saves_only_on_checkpoint() {
    let api = Api::start("api_cp_off", false);
    assert!(!api.server.check_point());
    api.build("k");
    assert_eq!(api.root("POST", "/update?db_name=k", NEW_TRIPLE).0, 200);
    let batch = "<http://ex/x> <http://ex/p> <http://ex/y> .\n";
    assert_eq!(api.root("POST", "/batchInsert?db_name=k", batch).0, 200);
    assert!(!on_disk_has_new_triple(&api.db_dir("k")));
    assert!(!on_disk_has(
        &api.db_dir("k"),
        "http://ex/x",
        "http://ex/p",
        "http://ex/y"
    ));

    let (s, body) = api.root("POST", "/checkpoint?db_name=k", "");
    assert_eq!(s, 200, "checkpoint: {body}");
    assert!(on_disk_has_new_triple(&api.db_dir("k")));
    assert!(on_disk_has(
        &api.db_dir("k"),
        "http://ex/x",
        "http://ex/p",
        "http://ex/y"
    ));
}

#[test]
fn api_check_point_on_saves_every_write() {
    let api = Api::start("api_cp_on", true);
    assert!(api.server.check_point());
    api.build("w");
    let dir = api.db_dir("w");

    assert_eq!(api.root("POST", "/update?db_name=w", NEW_TRIPLE).0, 200);
    assert!(on_disk_has_new_triple(&dir), "/update is saved");

    let (s, body) = api.root(
        "POST",
        "/batchInsert?db_name=w",
        "<http://ex/b1> <http://ex/p> <http://ex/o1> .\n",
    );
    assert_eq!(s, 200, "{body}");
    assert!(
        on_disk_has(&dir, "http://ex/b1", "http://ex/p", "http://ex/o1"),
        "batch is saved"
    );

    let (s, body) = api.root(
        "POST",
        "/upload?db_name=w&format=ntriples",
        "<http://ex/u1> <http://ex/p> <http://ex/o2> .\n",
    );
    assert_eq!(s, 200, "{body}");
    assert!(
        on_disk_has(&dir, "http://ex/u1", "http://ex/p", "http://ex/o2"),
        "upload is saved"
    );

    let tid = api.begin("root", "w");
    let (s, _) = api.root(
        "POST",
        &format!("/tquery?tid={tid}"),
        "INSERT DATA { <http://ex/t1> <http://ex/p> <http://ex/o3> }",
    );
    assert_eq!(s, 200);
    assert!(
        !on_disk_has(&dir, "http://ex/t1", "http://ex/p", "http://ex/o3"),
        "an uncommitted transactional write is never saved"
    );
    assert_eq!(api.root("POST", &format!("/commit?tid={tid}"), "").0, 200);
    assert!(
        on_disk_has(&dir, "http://ex/t1", "http://ex/p", "http://ex/o3"),
        "commit is saved"
    );
}

#[test]
fn api_shutdown_saves_dirty_databases_and_rolls_back_open_transactions() {
    let mut api = Api::start("api_shutdown", false);
    api.build("s");
    api.build("clean");
    assert_eq!(api.root("POST", "/update?db_name=s", NEW_TRIPLE).0, 200);
    let tid = api.begin("root", "s");
    let (s, _) = api.root(
        "POST",
        &format!("/tquery?tid={tid}"),
        "INSERT DATA { <http://ex/uncommitted> <http://ex/p> <http://ex/o> }",
    );
    assert_eq!(s, 200);

    let report = api.shutdown();
    assert!(report.drained, "{report:?}");
    assert_eq!(report.rolled_back, 1, "{report:?}");
    assert_eq!(
        report.saved,
        vec![api.db_dir("s")],
        "only the dirty db is saved: {report:?}"
    );
    let dir = api.db_dir("s");
    assert!(
        on_disk_has_new_triple(&dir),
        "the committed update is saved on shutdown"
    );
    assert!(
        !on_disk_has(&dir, "http://ex/uncommitted", "http://ex/p", "http://ex/o"),
        "an open transaction is rolled back, never persisted"
    );
    assert!(TcpStream::connect_timeout(&api.addr, Duration::from_secs(2)).is_err());
}

#[test]
fn api_transaction_is_owned_by_its_user() {
    let api = Api::start("api_owner", true);
    api.build("o");
    api.add_user("alice", "query,update");
    api.add_user("bob", "query,update");
    let tid = api.begin("alice", "o");
    let insert = "INSERT DATA { <http://ex/al> <http://ex/p> <http://ex/o> }";

    for path in ["/tquery", "/commit", "/rollback"] {
        let (s, body) = api.as_user("bob", "POST", &format!("{path}?tid={tid}"), insert);
        assert_eq!(
            s, 403,
            "bob must not touch alice's transaction via {path}: {body}"
        );
        assert!(body.contains("another user"), "{body}");
    }
    // The owner may use it; root may end it.
    assert_eq!(
        api.as_user("alice", "POST", &format!("/tquery?tid={tid}"), insert)
            .0,
        200
    );
    let (s, body) = api.root("POST", &format!("/commit?tid={tid}"), "");
    assert_eq!(s, 200, "root may commit any transaction: {body}");
    let (_, body) = api.as_user(
        "bob",
        "POST",
        "/sparql?db_name=o",
        "ASK { <http://ex/al> <http://ex/p> <http://ex/o> }",
    );
    assert!(body.contains("true"), "committed data is visible: {body}");
}

#[test]
fn api_non_transactional_writes_are_rejected_while_a_transaction_is_open() {
    let api = Api::start("api_reject", true);
    api.build("r");
    let tid = api.begin("root", "r");
    let batch = "<http://ex/x> <http://ex/p> <http://ex/y> .\n";

    let rejected = [
        ("/update?db_name=r", NEW_TRIPLE),
        ("/sparql?db_name=r", NEW_TRIPLE),
        ("/batchInsert?db_name=r", batch),
        (
            "/batchRemove?db_name=r",
            "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
        ),
        ("/upload?db_name=r&format=ntriples", batch),
        ("/checkpoint?db_name=r", ""),
    ];
    for (target, body) in rejected {
        let (s, resp) = api.root("POST", target, body);
        assert_eq!(s, 409, "{target} must be rejected during the txn: {resp}");
        assert!(resp.contains("transaction"), "{target}: {resp}");
    }
    // None of them leaked into the transaction: rolling back changes nothing,
    // and nothing was applied outside it either.
    assert_eq!(api.root("POST", &format!("/rollback?tid={tid}"), "").0, 200);
    let (_, body) = api.root(
        "POST",
        "/sparql?db_name=r",
        "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o }",
    );
    assert!(
        !body.contains("http://ex/c") && !body.contains("http://ex/x"),
        "{body}"
    );
    assert!(
        body.contains("http://ex/b"),
        "the original data is intact: {body}"
    );
    // Once the transaction is over, writes are accepted again.
    assert_eq!(api.root("POST", "/update?db_name=r", NEW_TRIPLE).0, 200);
}

#[test]
fn api_other_clients_never_read_uncommitted_data() {
    let api = Api::start("api_dirty", true);
    api.build("d");
    api.add_user("alice", "query,update");
    api.add_user("bob", "query,export");
    let tid = api.begin("alice", "d");
    let (s, _) = api.as_user(
        "alice",
        "POST",
        &format!("/tquery?tid={tid}"),
        "INSERT DATA { <http://ex/dirty> <http://ex/p> <http://ex/o> . \
                       <http://ex/dirty> <http://ex/p> <http://ex/o2> } ; \
         DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }",
    );
    assert_eq!(s, 200);

    let ask_new = "ASK { <http://ex/dirty> <http://ex/p> <http://ex/o> }";
    let ask_old = "ASK { <http://ex/a> <http://ex/p> <http://ex/b> }";
    // The owner sees its own writes inside the transaction...
    let (_, body) = api.as_user("alice", "POST", &format!("/tquery?tid={tid}"), ask_new);
    assert!(body.contains("true"), "read-your-writes in the txn: {body}");
    // ...but no one else does — neither the insert nor the delete.
    let (s, body) = api.as_user("bob", "POST", "/sparql?db_name=d", ask_new);
    assert_eq!(s, 200);
    assert!(body.contains("false"), "dirty read of an insert: {body}");
    let (_, body) = api.as_user("bob", "POST", "/sparql?db_name=d", ask_old);
    assert!(body.contains("true"), "dirty read of a delete: {body}");
    let (s, body) = api.as_user("bob", "GET", "/export?db_name=d", "");
    assert_eq!(s, 200);
    assert!(
        !body.contains("http://ex/dirty") && body.contains("http://ex/b"),
        "{body}"
    );
    // The live store holds 3 triples (2 + 2 inserted - 1 deleted), but only
    // the committed 2 are reported while the transaction is open.
    let (_, body) = api.root("GET", "/show", "");
    assert!(
        body.contains("\"triples\":2"),
        "committed count while txn open: {body}"
    );
    let (_, body) = api.root("GET", "/status", "");
    assert!(body.contains("\"total_triples\":2"), "{body}");

    assert_eq!(
        api.as_user("alice", "POST", &format!("/commit?tid={tid}"), "")
            .0,
        200
    );
    let (_, body) = api.as_user("bob", "POST", "/sparql?db_name=d", ask_new);
    assert!(body.contains("true"), "visible once committed: {body}");
    let (_, body) = api.as_user("bob", "POST", "/sparql?db_name=d", ask_old);
    assert!(
        body.contains("false"),
        "the delete is visible once committed: {body}"
    );
    let (_, body) = api.root("GET", "/show", "");
    assert!(body.contains("\"triples\":3"), "{body}");
}

#[test]
fn api_rolled_back_writes_are_never_visible() {
    let api = Api::start("api_rb", true);
    api.build("b");
    let tid = api.begin("root", "b");
    let (s, _) = api.root(
        "POST",
        &format!("/tquery?tid={tid}"),
        "INSERT DATA { <http://ex/rb> <http://ex/p> <http://ex/o> }",
    );
    assert_eq!(s, 200);
    let ask = "ASK { <http://ex/rb> <http://ex/p> <http://ex/o> }";
    assert!(api
        .root("POST", "/sparql?db_name=b", ask)
        .1
        .contains("false"));
    assert_eq!(api.root("POST", &format!("/rollback?tid={tid}"), "").0, 200);
    assert!(api
        .root("POST", "/sparql?db_name=b", ask)
        .1
        .contains("false"));
    assert!(!on_disk_has(
        &api.db_dir("b"),
        "http://ex/rb",
        "http://ex/p",
        "http://ex/o"
    ));
}

#[test]
fn api_isolevel_is_validated_and_reported() {
    let api = Api::start("api_iso", true);
    api.build("i");
    // Default: SERIALIZABLE (1), upstream's Txn_manager::Begin default.
    let (s, body) = api.root("POST", "/begin?db_name=i", "");
    assert_eq!(s, 200, "{body}");
    assert!(
        body.contains("\"isolevel\":1") && body.contains("SERIALIZABLE"),
        "{body}"
    );
    let tid = json_field(&body, "TID").unwrap().to_string();
    assert_eq!(api.root("POST", &format!("/rollback?tid={tid}"), "").0, 200);

    for (level, code, name) in [
        ("2", 2, "SNAPSHOT"),
        ("3", 3, "READ_COMMITTED"),
        ("serializable", 1, "SERIALIZABLE"),
    ] {
        let (s, body) = api.root("POST", &format!("/begin?db_name=i&isolevel={level}"), "");
        assert_eq!(s, 200, "{body}");
        assert!(body.contains(&format!("\"isolevel\":{code}")), "{body}");
        assert!(body.contains(name), "{body}");
        let tid = json_field(&body, "TID").unwrap().to_string();
        assert_eq!(api.root("POST", &format!("/commit?tid={tid}"), "").0, 200);
    }
    for bad in ["0", "4", "abc"] {
        let (s, body) = api.root("POST", &format!("/begin?db_name=i&isolevel={bad}"), "");
        assert_eq!(s, 400, "isolevel={bad} must be rejected: {body}");
        assert!(body.contains("1"), "{body}");
    }
    // A rejected begin opened nothing.
    let (_, body) = api.root("GET", "/status", "");
    assert!(body.contains("\"open_transactions\":0"), "{body}");
}

#[test]
fn api_unknown_and_finished_transactions_are_clean_errors() {
    let api = Api::start("api_tid", true);
    api.build("t");
    let (s, body) = api.root("POST", "/commit?tid=123456789", "");
    assert_eq!(s, 404, "{body}");
    assert_eq!(api.root("POST", "/rollback?tid=abc", "").0, 400);
    assert_eq!(
        api.root("POST", "/tquery?tid=987", "ASK { ?s ?p ?o }").0,
        404
    );

    let tid = api.begin("root", "t");
    assert_eq!(api.root("POST", &format!("/commit?tid={tid}"), "").0, 200);
    let (s, body) = api.root("POST", &format!("/commit?tid={tid}"), "");
    assert_eq!(s, 409, "double commit: {body}");
    assert!(body.contains("already committed"), "{body}");
    let (s, body) = api.root("POST", &format!("/rollback?tid={tid}"), "");
    assert_eq!(s, 409, "rollback after commit: {body}");
    let (s, _) = api.root("POST", &format!("/tquery?tid={tid}"), "ASK { ?s ?p ?o }");
    assert_eq!(s, 409, "tquery after commit");

    // The Workbench's `beginTime_tid` form addresses the same transaction.
    let tid = api.begin("root", "t");
    let (s, body) = api.root("POST", &format!("/rollback?tid=1700000000_{tid}"), "");
    assert_eq!(s, 200, "{body}");
    let (s, body) = api.root("POST", &format!("/rollback?tid={tid}"), "");
    assert_eq!(s, 409, "{body}");
    assert!(body.contains("already rolled back"), "{body}");
}

#[test]
fn api_batch_injection_payload_is_a_parse_error_and_data_is_intact() {
    let api = Api::start("api_inject", true);
    api.build("j");
    let payload = "<a> <b> <c> } ; DROP ALL ; INSERT DATA { <x> <y> <z>";
    for op in ["/batchInsert", "/batchRemove"] {
        let (s, body) = api.root("POST", &format!("{op}?db_name=j"), payload);
        assert_eq!(s, 400, "{op} must reject the payload: {body}");
        assert!(body.contains("not valid RDF"), "{body}");
        // Also via the `triples=` form parameter.
        let form = format!("triples={}", payload.replace(' ', "+").replace(';', "%3B"));
        let (s, _) = api.root("POST", &format!("{op}?db_name=j"), &form);
        assert_eq!(s, 400);
    }
    let (s, body) = api.root(
        "POST",
        "/sparql?db_name=j",
        "SELECT ?s ?p ?o WHERE { ?s ?p ?o }",
    );
    assert_eq!(s, 200);
    assert!(
        body.contains("http://ex/b") && body.contains("Alice"),
        "data intact: {body}"
    );
    assert!(!body.contains("\"x\""), "nothing inserted: {body}");
    let (_, show) = api.root("GET", "/show", "");
    assert!(show.contains("\"triples\":2"), "{show}");
    assert!(on_disk_has(
        &api.db_dir("j"),
        "http://ex/a",
        "http://ex/p",
        "http://ex/b"
    ));
}

#[test]
fn api_batch_reports_counts_and_accepts_turtle() {
    let api = Api::start("api_batch", true);
    api.build("c");
    // One new triple and one that already exists.
    let nt = "<http://ex/n1> <http://ex/p> <http://ex/o1> .\n\
              <http://ex/a> <http://ex/p> <http://ex/b> .\n";
    let (s, body) = api.root("POST", "/batchInsert?db_name=c", nt);
    assert_eq!(s, 200, "{body}");
    for want in [
        "\"format\":\"ntriples\"",
        "\"parsed\":2",
        "\"applied\":1",
        "\"failed\":1",
        "\"changed\":1",
    ] {
        assert!(body.contains(want), "{want} in {body}");
    }
    // Turtle, detected from @prefix.
    let ttl = "@prefix : <http://ex/> .\n:t1 :p :o1 , :o2 .\n";
    let (s, body) = api.root("POST", "/batchInsert?db_name=c", ttl);
    assert_eq!(s, 200, "{body}");
    assert!(
        body.contains("\"format\":\"turtle\"") && body.contains("\"applied\":2"),
        "{body}"
    );
    // Turtle via format=ttl, without prefixes.
    let (s, body) = api.root(
        "POST",
        "/batchInsert?db_name=c&format=ttl",
        "<http://ex/t2> <http://ex/p> \"x\" ; <http://ex/q> \"y\" .",
    );
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("\"applied\":2"), "{body}");
    // An explicit N-Triples format is strict: Turtle syntax is a parse error.
    let (s, _) = api.root("POST", "/batchInsert?db_name=c&format=nt", ttl);
    assert_eq!(s, 400);
    // Removing a present and an absent triple.
    let rm = "<http://ex/n1> <http://ex/p> <http://ex/o1> .\n\
              <http://ex/nope> <http://ex/p> <http://ex/o1> .\n";
    let (s, body) = api.root("POST", "/batchRemove?db_name=c", rm);
    assert_eq!(s, 200, "{body}");
    assert!(
        body.contains("\"applied\":1") && body.contains("\"failed\":1"),
        "{body}"
    );
    assert!(!on_disk_has(
        &api.db_dir("c"),
        "http://ex/n1",
        "http://ex/p",
        "http://ex/o1"
    ));
    assert!(on_disk_has(
        &api.db_dir("c"),
        "http://ex/t1",
        "http://ex/p",
        "http://ex/o2"
    ));
}

#[test]
fn api_load_of_a_loaded_database_keeps_unsaved_changes() {
    let api = Api::start("api_reload", false);
    api.build("l");
    assert_eq!(api.root("POST", "/update?db_name=l", NEW_TRIPLE).0, 200);
    let (s, body) = api.root("POST", "/load?db_name=l", "");
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("already loaded"), "{body}");
    let (_, body) = api.root(
        "POST",
        "/sparql?db_name=l",
        "ASK { <http://ex/c> <http://ex/p> <http://ex/d> }",
    );
    assert!(
        body.contains("true"),
        "the unsaved update survives /load: {body}"
    );
}

// ---- the real gserver binary, stopped with SIGTERM ------------------------

/// Spawn `gserver` with `args`, wait for its "listening on http://ADDR" line,
/// and return the child plus the address. Remaining stdout lines are collected
/// by a background thread into the returned handle.
#[cfg(unix)]
fn spawn_gserver(
    args: &[&str],
) -> (
    std::process::Child,
    SocketAddr,
    thread::JoinHandle<Vec<String>>,
) {
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    let mut child = Command::new(env!("CARGO_BIN_EXE_gserver"))
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn gserver");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(rest) = line.split("http://").nth(1) {
                let addr = rest.split_whitespace().next().unwrap_or("").to_string();
                let _ = tx.send(addr);
            }
            lines.push(line);
        }
        lines
    });
    let addr = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("gserver printed its address");
    (child, addr.parse().expect("socket address"), reader)
}

/// Send `SIGTERM` to `child` and wait (bounded) for it to exit successfully.
#[cfg(unix)]
fn terminate(child: &mut std::process::Child) {
    let status = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("run kill");
    assert!(status.success());
    let t0 = Instant::now();
    loop {
        if let Some(code) = child.try_wait().unwrap() {
            assert!(code.success(), "gserver exited with {code}");
            return;
        }
        if t0.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!("gserver did not exit after SIGTERM");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[test]
fn gserver_sigterm_saves_single_database() {
    let dir = tmp_dir("gserver_single");
    Database::build_from_str("gs", DATA)
        .unwrap()
        .save(&dir)
        .unwrap();
    let dir_s = dir.to_string_lossy().into_owned();
    let (mut child, addr, reader) = spawn_gserver(&[&dir_s, "--port", "0", "--no-checkpoint"]);
    let (s, body) = request(&addr, "POST", "/update", None, NEW_TRIPLE);
    assert_eq!(s, 200, "{body}");
    assert!(
        !on_disk_has_new_triple(&dir),
        "--no-checkpoint: not saved yet"
    );
    terminate(&mut child);
    let out = reader.join().unwrap().join("\n");
    assert!(
        on_disk_has_new_triple(&dir),
        "SIGTERM must save the update; output:\n{out}"
    );
    assert!(out.contains("saved database to"), "{out}");
    assert!(out.contains(&dir_s), "{out}");
}

#[cfg(unix)]
#[test]
fn gserver_sigterm_saves_api_databases() {
    let root = tmp_dir("gserver_api");
    let root_s = root.to_string_lossy().into_owned();
    let (mut child, addr, reader) = spawn_gserver(&[
        &root_s,
        "--api",
        "--port",
        "0",
        "--password",
        "pw",
        "--no-checkpoint",
    ]);
    let auth = basic("root", "pw");
    let (s, body) = request(&addr, "POST", "/build?db_name=g", Some(&auth), DATA);
    assert_eq!(s, 200, "{body}");
    let (s, body) = request(&addr, "POST", "/update?db_name=g", Some(&auth), NEW_TRIPLE);
    assert_eq!(s, 200, "{body}");
    let dir = root.join("g.db");
    assert!(
        !on_disk_has_new_triple(&dir),
        "--no-checkpoint: not saved yet"
    );
    terminate(&mut child);
    let out = reader.join().unwrap().join("\n");
    assert!(
        on_disk_has_new_triple(&dir),
        "SIGTERM must save the update; output:\n{out}"
    );
    assert!(out.contains("saved database to"), "{out}");
}
