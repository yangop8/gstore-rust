//! Request-path hygiene of the database server (review round 4,
//! `r4-api-hygiene`):
//!
//! * Creating a served database's handle — loading, building, uploading
//!   into a new name, restoring, on the REST endpoints and through `/api` —
//!   locks its directory and measures its size (a walk of every file) with
//!   the server-wide state lock released, so `/status`, `/cancel` and every
//!   other request go on meanwhile (review http#13, conc#7).
//!
//! * The API logs: a torn last line (a crash, a full disk) never swallows
//!   the record appended after it (review durability#13); the transaction
//!   log rotates at [`TXN_LOG_MAX_BYTES`] and a `txnlog` read parses only
//!   what was appended since the last one (review gapi#18).
//!
//! * `monitor` and `schema` of an unloaded database read its files once and
//!   answer later requests from statistics cached until the files change,
//!   instead of opening it on every request (review gapi#17).
//!
//! * A `gserver` built without the `tls` feature refuses `--tls-cert` /
//!   `--tls-key` as an argument error instead of serving plaintext HTTP
//!   (review http#19).
//!
//! * One form / URL decoder and one base64 decoder serve the REST
//!   endpoints, the `/api` layer and PFN, under each caller's rules
//!   (`gstore::util`); `/api` `show` lists the database directories the
//!   system registry scans (review gapi#15, robust#16, design#13).
//!
//! Every server here is in-process on `127.0.0.1:0` in a temp directory
//! removed when the test ends (also when it fails). Synchronisation is by
//! test hooks that park a step; a timeout only guards against a hang.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::gapi::apilog::{self, AccessLogRecord, ApiLogs, TxnState, TXN_LOG_MAX_BYTES};
use gstore::http_api::{debug_park_dir_size, ApiServer};
use gstore::json::{self, Json};
use gstore::server::ShutdownReport;

/// Bounds every wait (a hang guard, never what a test asserts).
const HANG_GUARD: Duration = Duration::from_secs(30);
const ROOT: (&str, &str) = ("root", "root");
const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/a> <http://ex/q> \"x\" .\n";

// ---- plumbing ---------------------------------------------------------------

/// A temp directory removed on drop (also while a failing test unwinds).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "gstore_api_hygiene_{tag}_{}_{}",
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
    body: String,
}

impl Reply {
    fn json(&self) -> Json {
        json::parse(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

/// One request on a fresh connection; the reply is awaited at most
/// `HANG_GUARD` (a read past it panics: the request hung).
fn send(addr: SocketAddr, method: &str, target: &str, ctype: Option<&str>, body: &str) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let auth = base64(&format!("{}:{}", ROOT.0, ROOT.1));
    let ctype = ctype
        .map(|c| format!("Content-Type: {c}\r\n"))
        .unwrap_or_default();
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {auth}\r\n\
         {ctype}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp)
        .unwrap_or_else(|e| panic!("{method} {target}: no answer within the hang guard: {e}"));
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((&resp, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {resp:?}"));
    Reply {
        status,
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
        let root = TmpDir::new(tag);
        let server = ApiServer::bind(&root.0, ROOT.1, "127.0.0.1:0")
            .expect("bind")
            .with_shutdown_grace(Duration::from_millis(100));
        let server = Arc::new(server);
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
        send(self.addr, method, target, None, body)
    }

    /// A REST request that must succeed.
    fn ok(&self, method: &str, target: &str, body: &str) -> Reply {
        let r = self.rest(method, target, body);
        assert_eq!(r.status, 200, "{method} {target}: {}", r.body);
        r
    }

    fn api(&self, pairs: &[(&str, &str)]) -> Json {
        let mut obj = Json::obj()
            .with("username", ROOT.0)
            .with("password", ROOT.1);
        for (k, v) in pairs {
            obj.set(*k, *v);
        }
        let r = send(self.addr, "POST", "/api", None, &obj.to_string());
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

// ---- handle creation off the state lock (http#13, conc#7) -------------------

/// Run `op` (which creates the handle of the database at `dir`) on its own
/// thread with the measurement of `dir` parked; once it is parked there,
/// `/status` and an `/api` `show` must be answered — they wait for the
/// state lock — and then `op` finishes when the walk is let go.
fn handle_creation_does_not_block_the_server<T: Send + 'static>(
    srv: &Srv,
    dir: &Path,
    what: &str,
    op: impl FnOnce(SocketAddr) -> T + Send + 'static,
) -> T {
    let park = debug_park_dir_size(dir);
    let addr = srv.addr;
    let worker = thread::spawn(move || op(addr));
    assert!(
        park.wait_parked(HANG_GUARD),
        "{what}: the directory of the new handle was never measured"
    );
    // The state lock is free while the walk is parked: before the fix
    // these waited for the walk (and ran into the hang guard).
    let status = srv.rest("GET", "/status", "");
    assert_eq!(status.status, 200, "{what}: /status: {}", status.body);
    let show = srv.api(&[("operation", "show")]);
    assert_eq!(code(&show), 0, "{what}: /api show: {show}");
    park.release();
    worker.join().expect("the request thread")
}

#[test]
fn rest_load_measures_the_directory_with_the_state_lock_released() {
    let srv = Srv::start("restload");
    srv.ok("POST", "/build?db_name=big", DATA);
    srv.ok("POST", "/unload?db_name=big", "");
    let r = handle_creation_does_not_block_the_server(&srv, &srv.db_dir("big"), "/load", |a| {
        send(a, "POST", "/load?db_name=big", None, "")
    });
    assert_eq!(r.status, 200, "/load: {}", r.body);
    // The size measured off the lock is the one monitoring reports.
    let j = srv.api_ok(&[("operation", "monitor"), ("db_name", "big")]);
    assert!(j.get("diskUsed").is_some(), "{j}");
}

#[test]
fn api_load_measures_the_directory_with_the_state_lock_released() {
    let srv = Srv::start("apiload");
    srv.ok("POST", "/build?db_name=big", DATA);
    srv.ok("POST", "/unload?db_name=big", "");
    let body = Json::obj()
        .with("username", ROOT.0)
        .with("password", ROOT.1)
        .with("operation", "load")
        .with("db_name", "big")
        .to_string();
    let r = handle_creation_does_not_block_the_server(&srv, &srv.db_dir("big"), "/api load", {
        move |a| send(a, "POST", "/api", None, &body)
    });
    assert_eq!(code(&r.json()), 0, "/api load: {}", r.body);
}

#[test]
fn rest_build_measures_the_directory_with_the_state_lock_released() {
    let srv = Srv::start("restbuild");
    let r = handle_creation_does_not_block_the_server(&srv, &srv.db_dir("big"), "/build", |a| {
        send(a, "POST", "/build?db_name=big", None, DATA)
    });
    assert_eq!(r.status, 200, "/build: {}", r.body);
}

#[test]
fn rest_upload_into_an_unloaded_database_measures_it_with_the_state_lock_released() {
    let srv = Srv::start("upload");
    srv.ok("POST", "/build?db_name=big", DATA);
    srv.ok("POST", "/unload?db_name=big", "");
    let r = handle_creation_does_not_block_the_server(&srv, &srv.db_dir("big"), "/upload", |a| {
        send(
            a,
            "POST",
            "/upload?db_name=big&format=ntriples",
            None,
            "<http://ex/c> <http://ex/p> <http://ex/d> .\n",
        )
    });
    assert_eq!(r.status, 200, "/upload: {}", r.body);
}

#[test]
fn restores_measure_the_directory_with_the_state_lock_released() {
    let srv = Srv::start("restore");
    srv.ok("POST", "/build?db_name=big", DATA);
    let j = srv.api_ok(&[("operation", "backup"), ("db_name", "big")]);
    let shown = j.get("backupfilepath").and_then(Json::as_str).unwrap();
    let backup = srv
        .root
        .0
        .join("backups")
        .join(Path::new(shown).file_name().unwrap());
    let target = format!(
        "/restore?db_name=big&path={}",
        enc(&backup.to_string_lossy())
    );
    let r = handle_creation_does_not_block_the_server(&srv, &srv.db_dir("big"), "/restore", {
        move |a| send(a, "POST", &target, None, "")
    });
    assert_eq!(r.status, 200, "/restore: {}", r.body);
    // `/api` `restore` over the loaded database serves the restored copy.
    let body = Json::obj()
        .with("username", ROOT.0)
        .with("password", ROOT.1)
        .with("operation", "restore")
        .with("db_name", "big")
        .with("backup_path", backup.to_string_lossy().as_ref())
        .to_string();
    let r = handle_creation_does_not_block_the_server(&srv, &srv.db_dir("big"), "/api restore", {
        move |a| send(a, "POST", "/api", None, &body)
    });
    assert_eq!(code(&r.json()), 0, "/api restore: {}", r.body);
}

// ---- the API logs (durability#13, gapi#18) ----------------------------------

/// Append raw bytes to `path` (what a crash or a full disk leaves).
fn append_raw(path: &Path, bytes: &[u8]) {
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(bytes).unwrap();
}

/// The `TID`s of the transaction log's records (one per transaction).
fn txn_ids(root: &Path) -> Vec<String> {
    apilog::txn_records(root)
        .iter()
        .map(|r| r.get("TID").and_then(Json::as_str).unwrap().to_string())
        .collect()
}

#[test]
fn a_torn_last_line_of_the_transaction_log_does_not_swallow_the_next_record() {
    let root = TmpDir::new("torn_txnlog");
    let logs = ApiLogs::new();
    logs.txn_begin(&root.0, "db", 1, "root");
    // A record cut short by a crash or a full disk: no newline.
    append_raw(&apilog::txnlog_path(&root.0), b"{\"TID\":\"1_2\",\"db_na");
    logs.txn_begin(&root.0, "db", 3, "root");
    let ids = txn_ids(&root.0);
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(
        ids[1].ends_with("_3"),
        "the record after the torn line: {ids:?}"
    );
    let page = logs.txn_page(&root.0, 1, 10).unwrap();
    assert_eq!(page.total_size, 2, "{:?}", page.items);
    // A zero-filled tail (power loss) likewise.
    append_raw(&apilog::txnlog_path(&root.0), &[0; 16]);
    logs.txn_end(&root.0, "db", 3, "root", TxnState::Committed);
    let recs = apilog::txn_records(&root.0);
    let last = recs.last().unwrap();
    assert_eq!(
        last.get("state").and_then(Json::as_str),
        Some("COMMITED"),
        "{recs:?}"
    );
}

#[test]
fn a_torn_last_line_of_a_daily_log_does_not_swallow_the_next_record() {
    let root = TmpDir::new("torn_access");
    let mut config = GStoreConfig::default();
    config.accesslog_mode = true;
    let logs = ApiLogs::new();
    let rec = |msg: &str| AccessLogRecord {
        ip: "127.0.0.1".into(),
        operation: "show".into(),
        msg: msg.into(),
        createtime: "2026-01-02 03:04:05".into(),
        ..AccessLogRecord::default()
    };
    logs.write_access(&config, &root.0, &rec("first"));
    let dir = apilog::accesslog_dir(&config, &root.0);
    let day = apilog::log_dates(&dir).remove(0);
    let path = dir.join(format!("{day}.log"));
    append_raw(&path, b"{\"code\":0,\"ms");
    logs.write_access(&config, &root.0, &rec("second"));
    let msgs: Vec<String> = apilog::read_records(&path)
        .iter()
        .map(|r| r.get("msg").and_then(Json::as_str).unwrap().to_string())
        .collect();
    assert_eq!(msgs, ["first", "second"]);
}

#[test]
fn the_transaction_log_rotates_at_its_size_cap_and_keeps_open_transactions() {
    let root = TmpDir::new("rotate");
    let logs = ApiLogs::new();
    // Open across the rotation.
    logs.txn_begin(&root.0, "db", 1, "root");
    let path = apilog::txnlog_path(&root.0);
    {
        // Finished transactions, as the server writes them, up to the cap.
        let mut w =
            std::io::BufWriter::new(fs::OpenOptions::new().append(true).open(&path).unwrap());
        let mut len = fs::metadata(&path).unwrap().len();
        let mut i = 0u64;
        while len + 300 < TXN_LOG_MAX_BYTES {
            let line = format!(
                "{{\"TID\":\"5_{i}\",\"begin_time\":\"5\",\"db_name\":\"old\",\"end_time\":\"6\",\
                 \"state\":\"COMMITED\",\"user\":\"root\"}}\n"
            );
            w.write_all(line.as_bytes()).unwrap();
            len += line.len() as u64;
            i += 1;
        }
    }
    let before = fs::metadata(&path).unwrap().len();
    logs.txn_begin(&root.0, "db", 2, "root");
    logs.txn_end(&root.0, "db", 2, "root", TxnState::Committed);
    let after = fs::metadata(&path).unwrap().len();
    let rotated = path.with_file_name("transaction.json.1");
    assert!(
        after <= TXN_LOG_MAX_BYTES,
        "the log grew past its cap: {after} bytes"
    );
    assert!(
        after < before,
        "the log was not rotated at its cap ({before} -> {after} bytes)"
    );
    assert!(
        fs::metadata(&rotated).map(|m| m.len()).unwrap_or(0) >= before,
        "the previous generation is kept as {}",
        rotated.display()
    );
    // The open transaction is still listed, and its end lands on it.
    logs.txn_end(&root.0, "db", 1, "root", TxnState::RolledBack);
    let recs = apilog::txn_records(&root.0);
    let states: Vec<(String, String)> = recs
        .iter()
        .map(|r| {
            let f = |k: &str| r.get(k).and_then(Json::as_str).unwrap().to_string();
            (f("db_name"), f("state"))
        })
        .collect();
    assert_eq!(
        states,
        [
            ("db".to_string(), "ROLLBACK".to_string()),
            ("db".to_string(), "COMMITED".to_string())
        ],
        "only the current generation's transactions, the open one carried over"
    );
    let page = logs.txn_page(&root.0, 1, 10).unwrap();
    assert_eq!(page.total_size, 2);
}

#[test]
fn txnlog_reads_parse_only_what_was_appended_since_the_last_read() {
    let root = TmpDir::new("incremental");
    let logs = ApiLogs::new();
    for tid in 1..=200 {
        logs.txn_begin(&root.0, "db", tid, "root");
        logs.txn_end(&root.0, "db", tid, "root", TxnState::Committed);
    }
    let path = apilog::txnlog_path(&root.0);
    let size = fs::metadata(&path).unwrap().len();
    let page = logs.txn_page(&root.0, 1, 10).unwrap();
    assert_eq!((page.total_size, page.items.len()), (200, 10));
    let first = logs.txn_log_bytes_parsed();
    // Nothing appended: nothing parsed.
    let again = logs.txn_page(&root.0, 2, 10).unwrap();
    assert_eq!(again.total_size, 200);
    assert_eq!(
        logs.txn_log_bytes_parsed(),
        first,
        "a read with nothing new"
    );
    // One record appended: only it is parsed.
    logs.txn_begin(&root.0, "db", 201, "root");
    let grown = fs::metadata(&path).unwrap().len() - size;
    let page = logs.txn_page(&root.0, 1, 10).unwrap();
    assert_eq!(page.total_size, 201);
    let newest = &page.items[0];
    assert!(
        newest
            .get("TID")
            .and_then(Json::as_str)
            .unwrap()
            .ends_with("_201"),
        "{newest}"
    );
    assert_eq!(newest.get("state").and_then(Json::as_str), Some("RUNNING"));
    let parsed = logs.txn_log_bytes_parsed() - first;
    assert!(
        parsed <= grown,
        "{parsed} bytes parsed for a {grown}-byte append to a {size}-byte log"
    );
    // A log replaced behind the reader's back (removed here) is read anew.
    fs::remove_file(&path).unwrap();
    logs.txn_begin(&root.0, "db", 202, "root");
    let page = logs.txn_page(&root.0, 1, 10).unwrap();
    assert_eq!(page.total_size, 1, "{:?}", page.items);
}

// ---- statistics of an unloaded database (gapi#17) ---------------------------

/// `monitor` and `schema` of an unloaded database in `layout`: read from
/// its files once, then answered from the statistics cached for its files
/// until they change.
fn unloaded_statistics_are_read_once(layout: &str) {
    let srv = Srv::start(&format!("unloaded_{layout}"));
    srv.ok(
        "POST",
        &format!("/build?db_name=cold&layout={layout}"),
        DATA,
    );
    srv.ok("POST", "/unload?db_name=cold", "");
    let dir = srv.db_dir("cold");
    let reads = || gstore::gapi::unloaded_stats_reads(&dir);
    let start = reads();
    let monitor = || srv.api_ok(&[("operation", "monitor"), ("db_name", "cold")]);
    let first = monitor();
    assert_eq!(
        first.get("tripleNum").and_then(Json::as_str),
        Some("2"),
        "{first}"
    );
    assert_eq!(reads() - start, 1, "the first monitor reads the database");
    for _ in 0..3 {
        let again = monitor();
        assert_eq!(again.get("tripleNum"), first.get("tripleNum"), "{again}");
        assert_eq!(
            again.get("subjectList"),
            first.get("subjectList"),
            "{again}"
        );
        assert_eq!(again.get("diskUsed"), first.get("diskUsed"), "{again}");
        let schema = srv.api_ok(&[("operation", "schema"), ("db_name", "cold")]);
        assert_eq!(schema.get("ResponseBody"), first.get("schema"), "{schema}");
    }
    assert_eq!(
        reads() - start,
        1,
        "monitor / schema of an unchanged unloaded database read it again"
    );
    // Its files change (loaded, written, unloaded): read again, once.
    srv.ok("POST", "/load?db_name=cold", "");
    srv.ok(
        "POST",
        "/update?db_name=cold",
        "INSERT DATA { <http://ex/c> a <http://ex/K> }",
    );
    srv.ok("POST", "/unload?db_name=cold", "");
    let changed = reads();
    let after = monitor();
    assert_eq!(
        after.get("tripleNum").and_then(Json::as_str),
        Some("3"),
        "{after}"
    );
    monitor();
    assert_eq!(reads() - changed, 1, "read once after the change");
}

#[test]
fn unloaded_snapshot_statistics_are_read_once_until_the_files_change() {
    unloaded_statistics_are_read_once("snapshot");
}

#[test]
fn unloaded_disk_statistics_are_read_once_until_the_files_change() {
    unloaded_statistics_are_read_once("disk");
}

#[cfg(feature = "rocksdb")]
#[test]
fn unloaded_rocksdb_statistics_are_read_once_until_the_files_change() {
    unloaded_statistics_are_read_once("rocksdb");
}

/// Set the modification time of every file and directory under `dir`
/// (`dir` included) to `t`.
fn set_mtimes(dir: &Path, t: std::time::SystemTime) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                fs::File::options()
                    .write(true)
                    .open(e.path())
                    .unwrap()
                    .set_modified(t)
                    .unwrap();
            }
        }
        fs::File::open(&d).unwrap().set_modified(t).unwrap();
    }
}

/// The relative paths and sizes of the files under `dir`, sorted.
fn file_sizes(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                let rel = e.path().strip_prefix(dir).unwrap().to_path_buf();
                out.push((rel, e.metadata().unwrap().len()));
            }
        }
    }
    out.sort();
    out
}

/// The cached statistics of an unloaded database are keyed by its files
/// themselves, not only by what their count, sizes and times add up to: a
/// directory swapped for a look-alike — the same file names and sizes, the
/// same timestamps (a copy that kept them, or a file system whose
/// timestamps are too coarse to tell two saves apart) — is read anew
/// instead of answered with the other database's statistics (review
/// gapi#17).
#[cfg(unix)]
#[test]
fn unloaded_statistics_are_read_again_for_a_look_alike_directory() {
    let srv = Srv::start("lookalike");
    // Two databases whose files have the same names and sizes.
    srv.ok(
        "POST",
        "/build?db_name=cold",
        "<http://ex/a> a <http://ex/K1> .\n",
    );
    srv.ok(
        "POST",
        "/build?db_name=twin",
        "<http://ex/a> a <http://ex/K2> .\n",
    );
    srv.ok("POST", "/unload?db_name=cold", "");
    srv.ok("POST", "/unload?db_name=twin", "");
    let (cold, twin) = (srv.db_dir("cold"), srv.db_dir("twin"));
    assert_eq!(file_sizes(&cold), file_sizes(&twin), "look-alike files");
    let t = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    set_mtimes(&cold, t);
    set_mtimes(&twin, t);
    let classes = || {
        let j = srv.api_ok(&[("operation", "monitor"), ("db_name", "cold")]);
        j.get("subjectList")
            .map(Json::to_string)
            .unwrap_or_default()
    };
    assert!(classes().contains("K1"), "cold's own classes");
    // `twin`'s directory takes `cold`'s place, timestamps and all.
    fs::rename(&cold, srv.root.0.join("cold.moved")).unwrap();
    fs::rename(&twin, &cold).unwrap();
    set_mtimes(&cold, t);
    // The read count is taken once `cold` exists again: between the two
    // renames there is no directory `cold`, and a `monitor` / `schema` of
    // any unloaded database in this process (another test's server, run in
    // parallel) drops the counts of directories that are gone, so a count
    // taken before the swap could be reset under the test (a full workspace
    // run saw "read anew" fail with 0 for 1). No read of `cold` happens in
    // between, so the count says the same as before.
    let reads = gstore::gapi::unloaded_stats_reads(&cold);
    let after = classes();
    assert!(
        after.contains("K2") && !after.contains("K1"),
        "the statistics of the directory now in place: {after}"
    );
    assert_eq!(
        gstore::gapi::unloaded_stats_reads(&cold) - reads,
        1,
        "read anew"
    );
}

// ---- gserver without the tls feature (http#19) ------------------------------

/// A `gserver` built without the `tls` feature cannot serve HTTPS: given
/// `--tls-cert` / `--tls-key` it refuses them as an argument error instead
/// of warning and then serving plaintext HTTP.
#[cfg(not(feature = "tls"))]
#[test]
fn a_gserver_without_tls_refuses_the_tls_options() {
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    let home = TmpDir::new("notls");
    let child = Command::new(env!("CARGO_BIN_EXE_gserver"))
        .args(["-p", "0", "--addr", "127.0.0.1", "--password", "pw"])
        .arg("--db-home")
        .arg(&home.0)
        .arg("--tls-cert")
        .arg(home.0.join("cert.pem"))
        .arg("--tls-key")
        .arg(home.0.join("key.pem"))
        .arg("--api")
        .arg(&home.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let out = match rx.recv_timeout(HANG_GUARD) {
        Ok(out) => out.unwrap(),
        Err(_) => {
            // Still serving (plaintext): stop it, then fail.
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
            let _ = waiter.join();
            panic!("gserver without tls kept serving with --tls-cert/--tls-key");
        }
    };
    let _ = waiter.join();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // An argument error: exit status 2, like every other bad argument
    // (review bulkcli#18, which landed in the same round).
    assert_eq!(
        out.status.code(),
        Some(2),
        "exit status {:?}; stderr: {stderr}",
        out.status
    );
    assert!(stderr.contains("--tls-cert"), "stderr: {stderr}");
    assert!(stderr.contains("`tls` feature"), "stderr: {stderr}");
    assert!(!stdout.contains("http://"), "it served: {stdout}");
}

// ---- one database-directory scan (gapi#15) ----------------------------------

/// The names in an `/api` `show` answer.
fn api_show_names(srv: &Srv) -> Vec<String> {
    let j = srv.api_ok(&[("operation", "show")]);
    j.get("ResponseBody")
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("{j}"))
        .iter()
        .map(|d| {
            d.get("database")
                .and_then(Json::as_str)
                .unwrap()
                .to_string()
        })
        .collect()
}

/// `/api` `show` scans the database directories with the system
/// registry's scan and name rule, so it lists every database REST `/show`
/// and the registry know — before, a name shorter than three characters
/// (which REST `/build` and `gbuild` accept) was missing from it.
#[test]
fn api_show_lists_the_databases_the_registry_scan_finds() {
    let srv = Srv::start("scan");
    srv.ok("POST", "/build?db_name=ab", DATA);
    srv.ok("POST", "/build?db_name=abc", DATA);
    srv.ok("POST", "/unload?db_name=ab", "");
    srv.ok("POST", "/unload?db_name=abc", "");
    // A directory holding no database is no database.
    fs::create_dir_all(srv.db_dir("empty")).unwrap();
    let rest = srv.ok("GET", "/show", "").body;
    let api = api_show_names(&srv);
    for name in ["ab", "abc"] {
        assert!(rest.contains(&format!("\"{name}\"")), "REST /show: {rest}");
        assert!(api.iter().any(|n| n == name), "/api show: {api:?}");
    }
    assert!(!api.iter().any(|n| n == "empty"), "/api show: {api:?}");
}

// ---- the shared decoders (gapi#15, robust#16, design#13) ---------------------

#[test]
fn every_layer_percent_decodes_alike() {
    use gstore::util::form;
    let cases = [
        ("a%20b+c", "a b c"),
        ("%41", "A"),
        ("100%", "100%"),
        ("%4", "%4"),
        ("%", "%"),
        ("%zz%4", "%zz%4"),
        ("a+b%2B%41%zz%", "a b+A%zz%"),
        ("%E4%B8%AD", "中"),
        ("%e4%b8%ad", "中"),
        ("%ff", "\u{FFFD}"),
        ("", ""),
    ];
    for (enc, want) in cases {
        assert_eq!(form::url_decode(enc), want, "util {enc:?}");
        assert_eq!(
            form::url_decode_bytes(enc.as_bytes()),
            want,
            "bytes {enc:?}"
        );
        assert_eq!(gstore::gapi::params::url_decode(enc), want, "/api {enc:?}");
        assert_eq!(gstore::pfn::url_decode(enc), want, "pfn {enc:?}");
    }
}

#[test]
fn a_form_is_listed_for_the_api_layer_and_looked_up_by_name_for_rest() {
    use gstore::util::form;
    let pairs = |v: &[(&str, &str)]| -> Vec<(String, String)> {
        v.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    // Every pair; a bare name has an empty value; an empty name is dropped.
    let want = pairs(&[("a", ""), ("b", ""), ("c", "1=2"), ("d e", "f+g")]);
    let form_text = "a&=x&b=&c=1=2&&d+e=f%2Bg";
    assert_eq!(form::parse_form(form_text), want);
    assert_eq!(gstore::gapi::params::parse_form(form_text), want);
    // By name: the first pair with an `=`, names decoded as they are
    // compared.
    let get = |f: &str, k: &str| form::form_lookup(f.as_bytes(), k);
    assert_eq!(get("q=1&q=2", "q").as_deref(), Some("1"));
    assert_eq!(get("stream&x=1", "stream"), None, "a bare name is no value");
    assert_eq!(get("%71uery=A+B%21", "query").as_deref(), Some("A B!"));
    assert_eq!(get("query%3D=x", "query"), None, "the name is `query=`");
    assert_eq!(get("query=", "query").as_deref(), Some(""));
    assert_eq!(get("", "query"), None);
}

#[test]
fn base64_is_decoded_by_one_decoder_under_each_callers_rules() {
    use gstore::util::base64::{decode, Base64Error, BASIC_AUTH, LENIENT};
    // HTTP Basic credentials: standard alphabet, whitespace skipped, the
    // first `=` ends the token, a partial group is ignored.
    assert_eq!(decode("dXNlcjpwYXNz", BASIC_AUTH).unwrap(), b"user:pass");
    assert_eq!(
        decode(" dXNl\r\ncjpw\tYXNz ", BASIC_AUTH).unwrap(),
        b"user:pass"
    );
    assert_eq!(
        decode("dXNlcjpwYXNz=*junk", BASIC_AUTH).unwrap(),
        b"user:pass"
    );
    assert_eq!(decode("YQ==", BASIC_AUTH).unwrap(), b"a");
    assert_eq!(decode("YWJjZA", BASIC_AUTH).unwrap(), b"abcd");
    assert_eq!(decode("A", BASIC_AUTH).unwrap(), b"");
    assert_eq!(
        decode("-_8=", BASIC_AUTH),
        Err(Base64Error::InvalidCharacter(0))
    );
    assert_eq!(
        decode("not*base64", BASIC_AUTH),
        Err(Base64Error::InvalidCharacter(3))
    );
    // PFN modules: both alphabets, a space is `+`, padding optional but
    // final, a truncated group refused.
    assert_eq!(decode("AGFzbQEAAAA=", LENIENT).unwrap(), b"\0asm\x01\0\0\0");
    assert_eq!(decode("AGFzbQEAAAA", LENIENT).unwrap(), b"\0asm\x01\0\0\0");
    assert_eq!(decode("-_8=", LENIENT).unwrap(), [0xfb, 0xff]);
    assert_eq!(decode("+/8=", LENIENT).unwrap(), [0xfb, 0xff]);
    assert_eq!(decode(" /8=", LENIENT).unwrap(), [0xfb, 0xff]);
    assert_eq!(decode("YQ==\n", LENIENT).unwrap(), b"a");
    assert_eq!(decode("A", LENIENT), Err(Base64Error::Truncated));
    assert_eq!(
        decode("AA=A", LENIENT),
        Err(Base64Error::DataAfterPadding(3))
    );
    assert_eq!(decode("A*", LENIENT), Err(Base64Error::InvalidCharacter(1)));
    // PFN's own entry point keeps its messages.
    assert_eq!(
        gstore::pfn::base64_decode("A*").unwrap_err(),
        "invalid base64 character at offset 1"
    );
    assert_eq!(
        gstore::pfn::base64_decode("AA=A").unwrap_err(),
        "base64 data after padding at offset 3"
    );
    assert_eq!(
        gstore::pfn::base64_decode("A").unwrap_err(),
        "truncated base64 data"
    );
}
