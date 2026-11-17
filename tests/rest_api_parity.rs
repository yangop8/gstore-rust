//! The REST endpoints and the upstream `/api` layer run the same lifecycle
//! operations — build, load, unload, backup, restore, drop, export,
//! batchInsert / batchRemove, users — with the same effects; only the way an
//! answer is rendered differs (REST: HTTP status; `/api`: HTTP 200 and an
//! upstream `StatusCode`). Review design#4: the two copies had diverged, and
//! each test here pins one operation on both surfaces:
//!
//! - REST `/export` wrote only the default graph (`/api` writes N-Quads when
//!   named graphs exist), held the whole database in memory, and could not
//!   be listed or cancelled (http#11);
//! - REST `/upload` creating a database neither registered it nor forgot a
//!   vanished name's grants, as `/build` and `/restore` do (http#16);
//! - REST `/build` (snapshot layout) deleted the old database's rules and
//!   ledgers before saving, so a failed save served it again without them
//!   (http#10);
//! - `/api` `load` answered "does not exist" for a database a crash left
//!   mid-restore, which REST `/load` recovers;
//! - REST `/backup` recorded its directory as a backup before writing it, so
//!   a failed backup left a directory claimed as one (`/api` records only a
//!   complete backup and removes a failed one);
//! - REST `/restore` registered a database it created at "now", `/api` at
//!   the backup's time.
//!
//! Deliberate differences stay (documented on the handlers): REST `/build`
//! rebuilds an existing database, REST `/drop` always deletes, REST
//! `/restore` needs a path and leaves the database loaded, REST `/export`
//! of a database in a transaction writes its committed state (`/api`
//! refuses, as upstream).
//!
//! Synchronisation is by test hooks (a parked statement), never by sleeping.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::http_api::{debug_park_txn, ApiServer, TxnParkPoint};
use gstore::json::{self, Json};
use gstore::server::ShutdownReport;

const HANG_GUARD: Duration = Duration::from_secs(60);
const ROOT: (&str, &str) = ("root", "root");
const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/a> <http://ex/q> \"x\" .\n";
const NAMED: &str = "<http://ex/x> <http://ex/p> <http://ex/y> <http://ex/g1> .";

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "gstore_rest_api_parity_{tag}_{}_{}",
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

fn base64(s: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n = chunk.iter().fold(0u32, |a, &x| (a << 8) | u32::from(x)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
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

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    }

    fn json(&self) -> Json {
        json::parse(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

fn send(addr: SocketAddr, user: (&str, &str), method: &str, target: &str, body: &str) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let auth = base64(&format!("{}:{}", user.0, user.1));
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {auth}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((&resp, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {resp:?}"));
    Reply {
        status,
        head: head.to_string(),
        body: body.to_string(),
    }
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

    fn rest(&self, method: &str, target: &str, body: &str) -> Reply {
        send(self.addr, ROOT, method, target, body)
    }

    /// A REST request that must succeed.
    fn ok(&self, method: &str, target: &str, body: &str) -> Reply {
        let r = self.rest(method, target, body);
        assert_eq!(r.status, 200, "{method} {target}: {}", r.body);
        r
    }

    fn api(&self, pairs: &[(&str, &str)]) -> Json {
        let mut obj = Json::obj()
            .with("username", "root")
            .with("password", "root");
        for (k, v) in pairs {
            obj.set(*k, *v);
        }
        let r = send(self.addr, ROOT, "POST", "/api", &obj.to_string());
        assert_eq!(r.status, 200, "/api answers HTTP 200: {}", r.body);
        r.json()
    }

    /// An `/api` operation that must succeed.
    fn api_ok(&self, pairs: &[(&str, &str)]) -> Json {
        let j = self.api(pairs);
        assert_eq!(code(&j), 0, "{pairs:?}: {j}");
        j
    }

    fn db_dir(&self, db: &str) -> PathBuf {
        self.root.0.join(format!("{db}.db"))
    }

    fn export_dir(&self) -> PathBuf {
        self.root.0.join("export")
    }

    /// A file in the upload directory (readable by `/api` `build` /
    /// `batchInsert`): its absolute path.
    fn upload_file(&self, name: &str, content: &str) -> String {
        let dir = self.root.0.join("upload");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p.to_string_lossy().into_owned()
    }

    /// The rows of `SELECT ?s ?p ?o` over every graph of `db`, as N-Quads-ish
    /// lines (REST).
    fn statements(&self, db: &str) -> BTreeSet<String> {
        let q = "SELECT ?s ?p ?o ?g WHERE { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }";
        let r = self.ok(
            "GET",
            &format!("/sparql?db_name={db}&format=json&sparql={}", enc(q)),
            "",
        );
        let j = r.json();
        let rows = j
            .get("results")
            .and_then(|r| r.get("bindings"))
            .and_then(Json::as_array)
            .unwrap_or_else(|| panic!("{}", r.body));
        rows.iter()
            .map(|b| {
                ["s", "p", "o", "g"]
                    .iter()
                    .map(|v| {
                        b.get(v)
                            .and_then(|t| t.get("value"))
                            .and_then(Json::as_str)
                            .unwrap_or("-")
                            .to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    /// The registry record of `db` (`system.json`): (creator, built time).
    fn registered(&self, db: &str) -> Option<(String, String)> {
        let text = fs::read_to_string(self.root.0.join("system.json")).ok()?;
        let j = json::parse(&text).unwrap();
        let list = j.get("databases").and_then(Json::as_array)?;
        list.iter()
            .find(|r| r.get("database").and_then(Json::as_str) == Some(db))
            .map(|r| {
                (
                    r.get("creator").and_then(Json::as_str).unwrap().to_string(),
                    r.get("built_time")
                        .and_then(Json::as_str)
                        .unwrap()
                        .to_string(),
                )
            })
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

fn code(j: &Json) -> i64 {
    j.get("StatusCode").and_then(Json::as_i64).unwrap()
}

fn lines(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Entries of the export directory other than `/api` exports' files.
fn scratch_files(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default()
}

/// The content of the file an `/api` `export` answered as `filepath`.
fn api_export_text(srv: &Srv, j: &Json) -> String {
    let shown = j.get("filepath").and_then(Json::as_str).unwrap();
    let name = Path::new(shown).file_name().unwrap();
    fs::read_to_string(srv.export_dir().join(name)).unwrap()
}

/// A database `db` with two default-graph triples and one named-graph quad.
fn with_named_graph(srv: &Srv, db: &str) {
    srv.ok("POST", &format!("/build?db_name={db}"), DATA);
    srv.ok(
        "POST",
        &format!("/update?db_name={db}"),
        "INSERT DATA { GRAPH <http://ex/g1> { <http://ex/x> <http://ex/p> <http://ex/y> } }",
    );
}

// ---------------------------------------------------------------------------
// export (http#11)
// ---------------------------------------------------------------------------

#[test]
fn export_writes_every_quad_on_both_surfaces() {
    let srv = Srv::start("export");
    with_named_graph(&srv, "expdb");
    let rest = srv.ok("GET", "/export?db_name=expdb", "");
    assert_eq!(
        rest.header("content-type").as_deref(),
        Some("application/n-quads"),
        "{}",
        rest.head
    );
    assert!(
        rest.header("content-disposition")
            .is_some_and(|d| d.contains("filename=\"expdb.nq\"")),
        "{}",
        rest.head
    );
    // Streamed with its length, and not left behind.
    assert_eq!(
        rest.header("content-length").as_deref(),
        Some(rest.body.len().to_string().as_str()),
        "{}",
        rest.head
    );
    assert!(scratch_files(&srv.export_dir()).is_empty());
    let got = lines(&rest.body);
    assert!(
        got.contains(NAMED),
        "the named graph is exported: {}",
        rest.body
    );
    assert!(got.contains("<http://ex/a> <http://ex/p> <http://ex/b> ."));
    assert_eq!(got.len(), 3, "{}", rest.body);
    // The `/api` export writes the same statements.
    let j = srv.api_ok(&[("operation", "export"), ("db_name", "expdb")]);
    assert_eq!(lines(&api_export_text(&srv, &j)), got);
    // `format` picks N-Triples (the default graph) on both.
    let rest = srv.ok("GET", "/export?db_name=expdb&format=nt", "");
    assert_eq!(
        rest.header("content-type").as_deref(),
        Some("application/n-triples")
    );
    let nt = lines(&rest.body);
    assert_eq!(nt.len(), 2, "{}", rest.body);
    let j = srv.api_ok(&[
        ("operation", "export"),
        ("db_name", "expdb"),
        ("format", "nt"),
    ]);
    assert_eq!(lines(&api_export_text(&srv, &j)), nt);
    // An unknown format is refused on both.
    assert_eq!(
        srv.rest("GET", "/export?db_name=expdb&format=xml", "")
            .status,
        400
    );
    let j = srv.api(&[
        ("operation", "export"),
        ("db_name", "expdb"),
        ("format", "xml"),
    ]);
    assert_eq!(code(&j), 1003, "{j}");
    // What REST exported builds the same database again.
    let file = srv.upload_file(
        "reload.nq",
        &srv.ok("GET", "/export?db_name=expdb", "").body,
    );
    srv.api_ok(&[
        ("operation", "build"),
        ("db_name", "reload"),
        ("db_path", &file),
    ]);
    srv.api_ok(&[("operation", "load"), ("db_name", "reload")]);
    assert_eq!(srv.statements("reload"), srv.statements("expdb"));
}

#[test]
fn export_in_a_transaction_writes_the_committed_state() {
    let srv = Srv::start("exptxn");
    with_named_graph(&srv, "txndb");
    export_in_a_transaction(&srv, "txndb");
}

/// The same for an on-disk database served in Stream mode, whose
/// transaction reads a view of the store's committed pages
/// (`SnapStore::Disk`) rather than an in-memory copy.
#[test]
fn export_in_a_transaction_of_a_stream_mode_disk_database_writes_the_committed_state() {
    let srv = Srv::start_with("exptxndisk", |s| {
        s.with_open_options(gstore::db::OpenOptions {
            disk_query_mode: gstore::db::DiskQueryMode::Stream,
            ..gstore::db::OpenOptions::default()
        })
    });
    srv.ok("POST", "/build?db_name=dtxn&layout=disk", DATA);
    srv.ok(
        "POST",
        "/update?db_name=dtxn",
        "INSERT DATA { GRAPH <http://ex/g1> { <http://ex/x> <http://ex/p> <http://ex/y> } }",
    );
    export_in_a_transaction(&srv, "dtxn");
}

/// REST `/export` of `db` (two default-graph triples and [`NAMED`]) while a
/// transaction that wrote to another graph is open, then after its commit.
fn export_in_a_transaction(srv: &Srv, db: &str) {
    let begin = srv.ok("POST", &format!("/begin?db_name={db}"), "");
    let tid = begin
        .json()
        .get("TID")
        .map(|t| t.as_str().map_or_else(|| t.to_string(), str::to_string))
        .unwrap();
    srv.ok(
        "POST",
        &format!("/tquery?tid={tid}"),
        "INSERT DATA { GRAPH <http://ex/g2> { <http://ex/u> <http://ex/p> <http://ex/v> } }",
    );
    let rest = srv.ok("GET", &format!("/export?db_name={db}"), "");
    let got = lines(&rest.body);
    assert!(got.contains(NAMED), "{}", rest.body);
    assert!(
        !rest.body.contains("http://ex/g2"),
        "uncommitted: {}",
        rest.body
    );
    assert_eq!(got.len(), 3, "{}", rest.body);
    // `/api` refuses while the transaction is open (upstream).
    let j = srv.api(&[("operation", "export"), ("db_name", db)]);
    assert_eq!(code(&j), 1004, "{j}");
    srv.ok("POST", &format!("/commit?tid={tid}"), "");
    let rest = srv.ok("GET", &format!("/export?db_name={db}"), "");
    assert!(rest.body.contains("<http://ex/g2> ."), "{}", rest.body);
}

/// The id of the running task of `db` whose operation is `op`.
fn running_task(srv: &Srv, db: &str, op: &str) -> u64 {
    let list = srv.ok("GET", "/tasklist", "").body;
    let entry = list
        .split("{\"id\":")
        .skip(1)
        .find(|e| e.contains(&format!("\"db\":\"{db}\"")) && e.contains(&format!("\"{op}\"")))
        .unwrap_or_else(|| panic!("no running {op} task of {db}: {list}"));
    entry
        .split([',', '}'])
        .next()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| panic!("{list}"))
}

#[test]
fn rest_export_is_a_task_that_cancel_stops() {
    let srv = Srv::start("expcancel");
    with_named_graph(&srv, "canceldb");
    // The export holds the database's guard and waits here until released.
    let park = debug_park_txn("canceldb", TxnParkPoint::StatementHolding);
    let addr = srv.addr;
    let export = thread::spawn(move || send(addr, ROOT, "GET", "/export?db_name=canceldb", ""));
    assert!(park.wait_parked(HANG_GUARD), "the export never started");
    let id = running_task(&srv, "canceldb", "export");
    srv.ok("POST", &format!("/cancel?id={id}"), "");
    park.release();
    let r = export.join().unwrap();
    assert_eq!(r.status, 503, "a cancelled export is refused: {}", r.body);
    assert!(r.body.contains("cancelled"), "{}", r.body);
    assert!(!srv.ok("GET", "/tasklist", "").body.contains("\"export\""));
    assert!(scratch_files(&srv.export_dir()).is_empty());
    drop(park);
    // The next export runs to the end.
    assert_eq!(
        lines(&srv.ok("GET", "/export?db_name=canceldb", "").body).len(),
        3
    );
}

// ---------------------------------------------------------------------------
// upload (http#16)
// ---------------------------------------------------------------------------

#[test]
fn upload_that_creates_a_database_registers_it_and_forgets_stale_grants() {
    let srv = Srv::start("upload");
    let alice = ("alice", "Alice-pass-123");
    srv.ok("POST", "/build?db_name=foo", DATA);
    srv.ok(
        "POST",
        "/user/create?op_username=alice&op_password=Alice-pass-123&privileges=query&db_name=foo",
        "",
    );
    let ask = "/sparql?db_name=foo&sparql=ASK%7B%3Fs%20%3Fp%20%3Fo%7D";
    assert_eq!(send(srv.addr, alice, "GET", ask, "").status, 200);
    // `foo` is removed outside the server (`gdrop`) while still registered.
    srv.ok("POST", "/unload?db_name=foo", "");
    fs::remove_dir_all(srv.db_dir("foo")).unwrap();
    // Root re-creates it by uploading new data: alice's grant on the old
    // `foo` does not reach the new data, as after `/build`.
    srv.ok(
        "POST",
        "/upload?db_name=foo&format=ntriples",
        "<http://ex/secret> <http://ex/p> \"new-data\" .\n",
    );
    let r = send(srv.addr, alice, "GET", ask, "");
    assert_eq!(
        r.status, 403,
        "a vanished database's grant was kept: {}",
        r.body
    );
    assert_eq!(
        srv.registered("foo").map(|(creator, _)| creator).as_deref(),
        Some("root")
    );
    // A brand-new name is registered for its creator, as `/build` does.
    srv.ok(
        "POST",
        "/user/create?op_username=bob&op_password=Bob-pass-1234&privileges=admin",
        "",
    );
    let bob = ("bob", "Bob-pass-1234");
    let r = send(
        srv.addr,
        bob,
        "POST",
        "/upload?db_name=bar&format=ntriples",
        DATA,
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        srv.registered("bar").map(|(creator, _)| creator).as_deref(),
        Some("bob"),
        "the uploaded database is not in the registry"
    );
    srv.ok("POST", "/build?db_name=baz", DATA);
    assert!(srv.registered("baz").is_some());
}

// ---------------------------------------------------------------------------
// build (http#10)
// ---------------------------------------------------------------------------

const RULE: &str = r#"{
    "rulename": "r1",
    "description": "a link is a relation",
    "isenable": 1,
    "type": 1,
    "logic": 1,
    "conditions": [{
        "patterns": [{"subject": "?x", "predicate": "<http://ex/p>", "object": "?y"}],
        "filters": [],
        "count_info": {}
    }],
    "return": {"source": "?x", "target": "?y", "label": "linked"}
}"#;

/// Every file under `dir`, relative to it.
fn tree(dir: &Path) -> BTreeSet<String> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            out.insert(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            if p.is_dir() {
                walk(base, &p, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(dir, dir, &mut out);
    out
}

fn rule_names(srv: &Srv, db: &str) -> Vec<String> {
    let j = srv.api_ok(&[
        ("operation", "reasonManage"),
        ("db_name", db),
        ("type", "2"),
    ]);
    j.get("list")
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .map(|r| {
            r.get("rulename")
                .and_then(Json::as_str)
                .unwrap()
                .to_string()
        })
        .collect()
}

#[test]
fn a_failed_rebuild_keeps_the_rules_and_ledgers_of_the_database_it_serves_again() {
    let srv = Srv::start("rebuild");
    srv.ok("POST", "/build?db_name=fam&layout=snapshot", DATA);
    let mut obj = Json::obj()
        .with("username", "root")
        .with("password", "root")
        .with("operation", "reasonManage")
        .with("db_name", "fam")
        .with("type", "1");
    obj.set("ruleinfo", json::parse(RULE).unwrap());
    let r = send(srv.addr, ROOT, "POST", "/api", &obj.to_string());
    assert_eq!(code(&r.json()), 0, "{}", r.body);
    // Compiled and executed: its ledger names the triples it inferred.
    for ty in ["3", "4"] {
        srv.api_ok(&[
            ("operation", "reasonManage"),
            ("db_name", "fam"),
            ("type", ty),
            ("rulename", "r1"),
        ]);
    }
    let rules_dir = srv.db_dir("fam").join("reason_rule_files");
    let before = tree(&rules_dir);
    assert!(before.contains("r1.json"), "{before:?}");
    assert!(before.len() > 2, "a ledger was written: {before:?}");
    // The save of the rebuild fails (a directory stands where its staging
    // file goes: what an I/O error does).
    let blocker = srv.db_dir("fam").join("dict.bin.tmp");
    fs::create_dir_all(blocker.join("blocker")).unwrap();
    let r = srv.rest(
        "POST",
        "/build?db_name=fam&layout=snapshot",
        "<http://ex/other> <http://ex/p> <http://ex/data> .\n",
    );
    assert_eq!(r.status, 500, "{}", r.body);
    // The previous database is served again — with its rules and ledgers.
    assert_eq!(tree(&rules_dir), before, "the rules were deleted");
    assert_eq!(rule_names(&srv, "fam"), ["r1"]);
    let ask = "ASK { <http://ex/a> <http://ex/p> <http://ex/b> }";
    let r = srv.ok(
        "GET",
        &format!("/sparql?db_name=fam&sparql={}", enc(ask)),
        "",
    );
    assert!(r.body.contains("true"), "{}", r.body);
    // A rebuild that succeeds replaces the database and its rules.
    fs::remove_dir_all(&blocker).unwrap();
    srv.ok("POST", "/build?db_name=fam&layout=snapshot", DATA);
    assert!(!rules_dir.exists());
    assert!(rule_names(&srv, "fam").is_empty());
}

// ---------------------------------------------------------------------------
// load / unload, build, drop
// ---------------------------------------------------------------------------

#[test]
fn build_load_unload_and_drop_have_the_same_effects_on_both_surfaces() {
    let srv = Srv::start("lifecycle");
    // build: both serve the data and register the database for its creator.
    srv.ok("POST", "/build?db_name=lrest", DATA);
    let path = srv.upload_file("lapi.nt", DATA);
    srv.api_ok(&[
        ("operation", "build"),
        ("db_name", "lapi"),
        ("db_path", &path),
    ]);
    srv.api_ok(&[("operation", "load"), ("db_name", "lapi")]);
    assert_eq!(srv.statements("lrest"), srv.statements("lapi"));
    for db in ["lrest", "lapi"] {
        assert_eq!(
            srv.registered(db).map(|(c, _)| c).as_deref(),
            Some("root"),
            "{db}"
        );
    }
    // unload / load: the same data back.
    srv.ok("POST", "/unload?db_name=lrest", "");
    srv.api_ok(&[("operation", "unload"), ("db_name", "lapi")]);
    srv.api_ok(&[("operation", "load"), ("db_name", "lrest")]);
    srv.ok("POST", "/load?db_name=lapi", "");
    assert_eq!(srv.statements("lrest"), srv.statements("lapi"));
    // load of an unknown database: REST 404, `/api` 1005.
    assert_eq!(srv.rest("POST", "/load?db_name=ghost", "").status, 404);
    let j = srv.api(&[("operation", "load"), ("db_name", "ghost")]);
    assert_eq!(code(&j), 1005, "{j}");
    // drop (`/api` with is_backup=false, REST's only form): directory and
    // registry entry gone on both.
    srv.ok("POST", "/drop?db_name=lrest", "");
    srv.api_ok(&[
        ("operation", "drop"),
        ("db_name", "lapi"),
        ("is_backup", "false"),
    ]);
    for db in ["lrest", "lapi"] {
        assert!(!srv.db_dir(db).exists(), "{db}");
        assert_eq!(srv.registered(db), None, "{db}");
    }
}

/// What a crash between the two renames of a restore's swap leaves: the
/// database moved aside, its replacement staged beside it.
fn crash_mid_restore(dir: &Path) {
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    let sibling = |suffix: &str| dir.with_file_name(format!("{name}{suffix}"));
    let copy = sibling(".restore-new");
    fs::create_dir_all(&copy).unwrap();
    for e in fs::read_dir(dir).unwrap().flatten() {
        if e.file_type().unwrap().is_file() && e.file_name() != "LOCK" {
            fs::copy(e.path(), copy.join(e.file_name())).unwrap();
        }
    }
    fs::rename(dir, sibling(".restore-old")).unwrap();
}

#[test]
fn load_finishes_an_interrupted_restore_on_both_surfaces() {
    let srv = Srv::start("midrestore");
    for db in ["mrest", "mapi"] {
        srv.ok("POST", &format!("/build?db_name={db}"), DATA);
        srv.ok("POST", &format!("/unload?db_name={db}"), "");
        crash_mid_restore(&srv.db_dir(db));
        assert!(!srv.db_dir(db).exists());
    }
    srv.ok("POST", "/load?db_name=mrest", "");
    let j = srv.api(&[("operation", "load"), ("db_name", "mapi")]);
    assert_eq!(code(&j), 0, "the database left mid-restore loads: {j}");
    assert_eq!(srv.statements("mrest"), srv.statements("mapi"));
    for db in ["mrest", "mapi"] {
        assert!(srv.db_dir(db).is_dir(), "{db}");
        assert!(!srv.root.0.join(format!("{db}.db.restore-old")).exists());
        assert!(!srv.root.0.join(format!("{db}.db.restore-new")).exists());
    }
}

// ---------------------------------------------------------------------------
// backup / restore
// ---------------------------------------------------------------------------

/// A failed REST `/backup` over an earlier backup of the database keeps the
/// directory recorded as the database's, so the next backup there is
/// allowed: dropping its record would orphan it, and every later `/backup`
/// into the directory — the default `<db>.backup` is reused by every REST
/// backup of the database — would be refused for good as "data that is not
/// a backup" (`409`).
///
/// Review (round 3, REST `/backup` in-progress marker): this test asserted
/// that the earlier backup stays restorable after the failed replacement
/// ("each store is replaced as a whole, so the directory still holds the
/// earlier backup"). `Database::backup` copies the reasoning rules and
/// ledgers before the store (conc#13), so the directory then pairs the new
/// rules with the earlier store — no complete backup of either moment. It
/// is marked incomplete (`BACKUP_OF.partial`) and refused by `/restore`
/// (`409`, for its database's restorers and root alike) until a backup into
/// it completes, which the retry below does.
#[test]
fn a_failed_backup_over_an_earlier_one_keeps_it_and_can_be_retried() {
    let srv = Srv::start("backupfail");
    srv.ok("POST", "/build?db_name=bkp", DATA);
    let r = srv.ok("POST", "/backup?db_name=bkp", "");
    let target = PathBuf::from(r.json().get("path").and_then(Json::as_str).unwrap());
    assert_eq!(fs::read_to_string(target.join("BACKUP_OF")).unwrap(), "bkp");
    let insert = "INSERT DATA { <http://ex/n> <http://ex/p> <http://ex/m> }";
    srv.ok("POST", "/update?db_name=bkp", insert);
    let current = srv.statements("bkp");
    // The next backup there fails (a directory stands where its staging file
    // goes: what an I/O error does).
    let blocker = target.join("dict.bin.tmp");
    fs::create_dir_all(blocker.join("blocker")).unwrap();
    let r = srv.rest("POST", "/backup?db_name=bkp", "");
    assert_eq!(r.status, 500, "{}", r.body);
    assert_eq!(
        fs::read_to_string(target.join("BACKUP_OF")).unwrap(),
        "bkp",
        "the directory is no longer recorded as the database's"
    );
    assert!(
        target.join("BACKUP_OF.partial").exists(),
        "the partly replaced backup is not marked incomplete"
    );
    fs::remove_dir_all(&blocker).unwrap();
    // A user who may restore `bkp` from its own backups only (root and admin
    // users may restore from anything) is told it is incomplete; so is root.
    srv.ok(
        "POST",
        "/user/create?op_username=carol&op_password=Carol-pass-123&privileges=restore&db_name=bkp",
        "",
    );
    let carol = ("carol", "Carol-pass-123");
    let restore = format!(
        "/restore?db_name=bkp&path={}",
        enc(&target.to_string_lossy())
    );
    let r = send(srv.addr, carol, "POST", &restore, "");
    assert_eq!(r.status, 409, "{}", r.body);
    assert!(r.body.contains("incomplete"), "{}", r.body);
    let r = srv.rest("POST", &restore, "");
    assert_eq!(r.status, 409, "{}", r.body);
    assert_eq!(
        srv.statements("bkp"),
        current,
        "a refused restore changed it"
    );
    // The fault gone, the next backup into the same directory succeeds and
    // completes it: restorable again, by the database's restorers too.
    let r = srv.rest("POST", "/backup?db_name=bkp", "");
    assert_eq!(r.status, 200, "a retry is refused: {}", r.body);
    assert_eq!(fs::read_to_string(target.join("BACKUP_OF")).unwrap(), "bkp");
    assert!(!target.join("BACKUP_OF.partial").exists());
    srv.ok(
        "POST",
        "/update?db_name=bkp",
        "DELETE DATA { <http://ex/n> <http://ex/p> <http://ex/m> }",
    );
    let r = send(srv.addr, carol, "POST", &restore, "");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(srv.statements("bkp"), current);
    // The `/api` backup, which records only a complete backup, agrees on a
    // good one; and a good REST backup is recorded.
    let j = srv.api_ok(&[("operation", "backup"), ("db_name", "bkp")]);
    let shown = j.get("backupfilepath").and_then(Json::as_str).unwrap();
    let api_backup = srv
        .root
        .0
        .join("backups")
        .join(Path::new(shown).file_name().unwrap());
    assert_eq!(
        fs::read_to_string(api_backup.join("BACKUP_OF")).unwrap(),
        "bkp"
    );
    let r = srv.ok("POST", "/backup?db_name=bkp&path=fresh", "");
    let fresh = PathBuf::from(r.json().get("path").and_then(Json::as_str).unwrap());
    assert_eq!(fs::read_to_string(fresh.join("BACKUP_OF")).unwrap(), "bkp");
}

#[test]
fn restore_registers_a_database_it_creates_at_the_backups_time() {
    let srv = Srv::start("restoretime");
    srv.ok("POST", "/build?db_name=rsrc", DATA);
    let j = srv.api_ok(&[("operation", "backup"), ("db_name", "rsrc")]);
    let shown = j.get("backupfilepath").and_then(Json::as_str).unwrap();
    let backups = srv.root.0.join("backups");
    let made = backups.join(Path::new(shown).file_name().unwrap());
    // An older backup of `rsrc` (its name records its time).
    let old = backups.join("rsrc.db_20200102030405");
    fs::rename(&made, &old).unwrap();
    for (db, via_rest) in [("rnewrest", true), ("rnewapi", false)] {
        if via_rest {
            srv.ok(
                "POST",
                &format!("/restore?db_name={db}&path={}", enc(&old.to_string_lossy())),
                "",
            );
        } else {
            srv.api_ok(&[
                ("operation", "restore"),
                ("db_name", db),
                ("backup_path", &old.to_string_lossy()),
            ]);
            srv.api_ok(&[("operation", "load"), ("db_name", db)]);
        }
        assert_eq!(
            srv.registered(db),
            Some(("root".to_string(), "2020-01-02 03:04:05".to_string())),
            "{db}"
        );
        assert_eq!(srv.statements(db), srv.statements("rsrc"), "{db}");
    }
}

// ---------------------------------------------------------------------------
// batchInsert / batchRemove, users
// ---------------------------------------------------------------------------

#[test]
fn batch_insert_and_remove_have_the_same_effects_on_both_surfaces() {
    let srv = Srv::start("batch");
    srv.ok("POST", "/build?db_name=brest", DATA);
    srv.ok("POST", "/build?db_name=bapi", DATA);
    let more = "<http://ex/c> <http://ex/p> <http://ex/d> .\n\
                <http://ex/a> <http://ex/p> <http://ex/b> .\n";
    let r = srv.ok("POST", "/batchInsert?db_name=brest", more);
    let rest = r.json();
    let file = srv.upload_file("more.nt", more);
    let api = srv.api_ok(&[
        ("operation", "batchInsert"),
        ("db_name", "bapi"),
        ("file", &file),
    ]);
    for key in ["success_num", "failed_num"] {
        assert_eq!(rest.get(key), api.get(key), "{key}: {rest} vs {api}");
    }
    assert_eq!(srv.statements("brest"), srv.statements("bapi"));
    let r = srv.ok("POST", "/batchRemove?db_name=brest", more);
    let rest = r.json();
    let api = srv.api_ok(&[
        ("operation", "batchRemove"),
        ("db_name", "bapi"),
        ("file", &file),
    ]);
    for key in ["success_num", "failed_num"] {
        assert_eq!(rest.get(key), api.get(key), "{key}: {rest} vs {api}");
    }
    assert_eq!(srv.statements("brest"), srv.statements("bapi"));
    assert_eq!(srv.statements("brest").len(), 1);
    // Not loaded: REST 404, `/api` 1004.
    srv.ok("POST", "/unload?db_name=brest", "");
    srv.api_ok(&[("operation", "unload"), ("db_name", "bapi")]);
    assert_eq!(
        srv.rest("POST", "/batchInsert?db_name=brest", more).status,
        404
    );
    let j = srv.api(&[
        ("operation", "batchInsert"),
        ("db_name", "bapi"),
        ("file", &file),
    ]);
    assert_eq!(code(&j), 1004, "{j}");
}

#[test]
fn users_created_and_granted_on_either_surface_behave_alike() {
    let srv = Srv::start("users");
    srv.ok("POST", "/build?db_name=udb", DATA);
    srv.ok(
        "POST",
        "/user/create?op_username=urest&op_password=Urest-pass-123",
        "",
    );
    srv.ok(
        "POST",
        "/user/grant?op_username=urest&privileges=query&db_name=udb",
        "",
    );
    srv.api_ok(&[
        ("operation", "usermanage"),
        ("type", "1"),
        ("op_username", "uapi"),
        ("op_password", "Uapi-pass-1234"),
    ]);
    srv.api_ok(&[
        ("operation", "userprivilegemanage"),
        ("type", "1"),
        ("op_username", "uapi"),
        ("privileges", "1"),
        ("db_name", "udb"),
    ]);
    let ask = "/sparql?db_name=udb&sparql=ASK%7B%3Fs%20%3Fp%20%3Fo%7D";
    for user in [("urest", "Urest-pass-123"), ("uapi", "Uapi-pass-1234")] {
        assert_eq!(send(srv.addr, user, "GET", ask, "").status, 200, "{user:?}");
        let r = send(
            srv.addr,
            user,
            "POST",
            "/update?db_name=udb",
            "INSERT DATA { <a> <b> <c> }",
        );
        assert_eq!(r.status, 403, "{user:?}: {}", r.body);
    }
    // Dropped on either surface, neither can log in.
    srv.ok("POST", "/user/drop?op_username=urest", "");
    srv.api_ok(&[
        ("operation", "usermanage"),
        ("type", "2"),
        ("op_username", "uapi"),
    ]);
    for user in [("urest", "Urest-pass-123"), ("uapi", "Uapi-pass-1234")] {
        assert_eq!(send(srv.addr, user, "GET", ask, "").status, 401, "{user:?}");
    }
}

/// A backup into a fresh directory that fails part-way — `gserver` runs
/// with `RLIMIT_FSIZE` below the size of the database's snapshot files
/// (`SIGXFSZ` ignored), so every large write fails with `EFBIG`, what a full
/// disk does — leaves no directory behind on either surface (the REST
/// `/backup` left it, recorded as a backup).
#[cfg(target_os = "linux")]
#[test]
fn a_backup_that_fails_part_way_leaves_no_directory_on_either_surface() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[repr(C)]
    struct RLimit {
        cur: u64,
        max: u64,
    }
    extern "C" {
        fn setrlimit(resource: i32, rlim: *const RLimit) -> i32;
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const RLIMIT_FSIZE: i32 = 1;
    const SIGXFSZ: i32 = 25;
    const SIG_IGN: usize = 1;
    const LIMIT: u64 = 64 * 1024;

    /// The `gserver` child, killed and reaped when dropped.
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let tmp = TmpDir::new("backupfsize");
    let root = tmp.0.join("root");
    let mut nt = String::new();
    for i in 0..3000 {
        nt.push_str(&format!(
            "<http://ex/s/{i}> <http://ex/value> \"literal number {i} with some padding\" .\n"
        ));
    }
    let dir = root.join("bigdb.db");
    gstore::Database::build_from_str("bigdb", &nt)
        .unwrap()
        .save(&dir)
        .unwrap();
    assert!(fs::metadata(dir.join("store.bin")).unwrap().len() > LIMIT);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gserver"));
    cmd.args([
        root.to_str().unwrap(),
        "--api",
        "--port",
        "0",
        "--password",
        "root",
    ])
    .current_dir(&tmp.0)
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    let rl = RLimit {
        cur: LIMIT,
        max: LIMIT,
    };
    // SAFETY: setrlimit(2) and signal(2) are async-signal-safe; the closure
    // only calls them on values it owns.
    unsafe {
        cmd.pre_exec(move || {
            if setrlimit(RLIMIT_FSIZE, &rl) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            signal(SIGXFSZ, SIG_IGN);
            Ok(())
        });
    }
    let mut child = Child(cmd.spawn().expect("spawn gserver"));
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(rest) = line.split("http://").nth(1) {
                let _ = tx.send(rest.split_whitespace().next().unwrap_or("").to_string());
            }
        }
    });
    let addr: SocketAddr = rx
        .recv_timeout(HANG_GUARD)
        .expect("gserver printed its address")
        .parse()
        .unwrap();
    let r = send(addr, ROOT, "POST", "/load?db_name=bigdb", "");
    assert_eq!(r.status, 200, "{}", r.body);
    let backups = root.join("backups");

    let r = send(addr, ROOT, "POST", "/backup?db_name=bigdb&path=fresh", "");
    assert_eq!(r.status, 500, "{}", r.body);
    assert!(
        !backups.join("fresh").exists(),
        "a failed backup left its directory"
    );
    // Into an empty directory made beforehand (a mount point, say): left
    // empty again — neither recorded as a backup nor holding data a later
    // `/backup` there would refuse as "not a backup" (409).
    let empty = backups.join("empty");
    fs::create_dir(&empty).unwrap();
    let r = send(addr, ROOT, "POST", "/backup?db_name=bigdb&path=empty", "");
    assert_eq!(r.status, 500, "{}", r.body);
    let held: Vec<_> = fs::read_dir(&empty)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(held.is_empty(), "a failed backup left {held:?}");
    fs::remove_dir(&empty).unwrap();

    let body = Json::obj()
        .with("username", "root")
        .with("password", "root")
        .with("operation", "backup")
        .with("db_name", "bigdb")
        .to_string();
    let r = send(addr, ROOT, "POST", "/api", &body);
    assert_eq!(code(&r.json()), 1005, "{}", r.body);
    let left: Vec<_> = fs::read_dir(&backups)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}
