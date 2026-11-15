//! A write that was applied in memory but could not be saved is never
//! acknowledged (review durability#2 / H-7, and the save-failure gap of
//! durability#18).
//!
//! A real `gserver` runs with `RLIMIT_FSIZE` below the size of its snapshot
//! files (set in `pre_exec`, `SIGXFSZ` ignored), so every check_point save
//! fails with `EFBIG` — what a full disk does — while reads keep working.
//! Every write path must then answer a failure: the REST endpoints and the
//! single-database server HTTP 500, the upstream `/api` protocol HTTP 200
//! with `StatusCode` 1005 (the code `/api` `batchInsert` answers for a
//! failed save), each saying `"applied in memory but not durable: <cause>"`.
//! After `SIGKILL` and a restart the database holds exactly what was
//! acknowledged. Before the fix these writes answered HTTP 200 /
//! `StatusCode` 0 with a `"warning"` field and were lost on the kill.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use gstore::json::{self, Json};
use gstore::{Database, QueryResult};

/// The `RLIMIT_FSIZE` of the failing server: above every small file it
/// writes (logs, lock and system files), below its snapshot files.
const FSIZE_LIMIT: u64 = 64 * 1024;
/// `StatusCode` of a failed `/api` operation (`batchInsert`'s failed save).
const API_FAILED: i64 = 1005;
const NOT_DURABLE: &str = "applied in memory but not durable";
/// `Basic root:pw`.
const ROOT: &str = "Basic cm9vdDpwdw==";

/// A temp directory removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_save_failure_ack_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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

/// A `gserver` child, killed (`SIGKILL`) and reaped when dropped.
struct Gserver {
    child: Child,
    addr: SocketAddr,
}

impl Gserver {
    /// Spawn `gserver args` in `cwd` (no `conf/conf.ini` there), with
    /// `RLIMIT_FSIZE` = `fsize` when given.
    fn spawn(cwd: &Path, args: &[&str], fsize: Option<u64>) -> Gserver {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gserver"));
        cmd.args(args)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(limit) = fsize {
            let rl = RLimit {
                cur: limit,
                max: limit,
            };
            // SAFETY: setrlimit(2) and signal(2) are async-signal-safe; the
            // closure only calls them on values it owns.
            unsafe {
                cmd.pre_exec(move || {
                    if setrlimit(RLIMIT_FSIZE, &rl) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // A write past the limit then fails with EFBIG instead
                    // of killing the process.
                    signal(SIGXFSZ, SIG_IGN);
                    Ok(())
                });
            }
        }
        let mut child = cmd.spawn().expect("spawn gserver");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(rest) = line.split("http://").nth(1) {
                    let addr = rest.split_whitespace().next().unwrap_or("").to_string();
                    let _ = tx.send(addr);
                }
            }
        });
        let addr = match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(a) => a.parse().expect("socket address"),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("gserver did not print its address");
            }
        };
        Gserver { child, addr }
    }

    /// `SIGKILL`, and wait for it to be gone.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// One HTTP/1.1 request: `(status, body)`.
    fn request(&self, method: &str, target: &str, auth: Option<&str>, body: &str) -> (u16, String) {
        let mut s = TcpStream::connect(self.addr).expect("connect to gserver");
        // A hang guard only; no assertion depends on time.
        s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
        let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: {}\r\n", self.addr);
        if let Some(a) = auth {
            raw.push_str(&format!("Authorization: {a}\r\n"));
        }
        if target.starts_with("/api") {
            raw.push_str("Content-Type: application/json\r\n");
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
            .unwrap_or_else(|| panic!("no status line: {resp:?}"));
        let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
        (status, body.to_string())
    }

    /// `POST /api` as root with string parameters: HTTP 200 and the envelope.
    fn api(&self, pairs: &[(&str, &str)]) -> Json {
        let mut items = vec![
            format!("{}: {}", json::quote("username"), json::quote("root")),
            format!("{}: {}", json::quote("password"), json::quote("pw")),
        ];
        items.extend(
            pairs
                .iter()
                .map(|(k, v)| format!("{}: {}", json::quote(k), json::quote(v))),
        );
        let (st, body) = self.request("POST", "/api", None, &format!("{{{}}}", items.join(", ")));
        assert_eq!(st, 200, "/api always answers HTTP 200: {body}");
        parse(&body)
    }
}

impl Drop for Gserver {
    fn drop(&mut self) {
        self.kill();
    }
}

fn parse(body: &str) -> Json {
    json::parse(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

fn int(j: &Json, key: &str) -> i64 {
    j.get(key)
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no integer {key} in {j}"))
}

fn text<'a>(j: &'a Json, key: &str) -> &'a str {
    j.get(key)
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no string {key} in {j}"))
}

/// A base data set whose snapshot files are larger than [`FSIZE_LIMIT`].
fn base_nt() -> String {
    let mut s = String::new();
    for i in 0..3000 {
        s.push_str(&format!(
            "<http://ex/base/{i}> <http://ex/value> \"base literal number {i} with some padding\" .\n"
        ));
    }
    s.push_str("<http://ex/kid> <http://ex/parent> <http://ex/mum> .\n");
    s
}

fn build(dir: &Path) {
    Database::build_from_str("acked", &base_nt())
        .unwrap()
        .save(dir)
        .unwrap();
    let store = std::fs::metadata(dir.join("store.bin")).unwrap().len();
    assert!(
        store > FSIZE_LIMIT,
        "store.bin ({store} bytes) must exceed the limit"
    );
}

fn insert(subject: &str) -> String {
    format!("INSERT DATA {{ <http://ex/{subject}> <http://ex/p> <http://ex/o> }}")
}

/// An UPDATE whose INSERT DATA is applied, then fails (LOAD of a missing
/// file): an update that fails part-way.
fn partial(subject: &str) -> String {
    format!(
        "INSERT DATA {{ <http://ex/{subject}> <http://ex/p> <http://ex/o> }} ; \
         LOAD <file:///nonexistent/gstore-save-failure-ack.ttl>"
    )
}

fn nt(subject: &str) -> String {
    format!("<http://ex/{subject}> <http://ex/p> <http://ex/o> .\n")
}

/// Which of `subjects` the database at `dir` holds (reopened: recovery
/// included).
fn present(dir: &Path, subjects: &[&str]) -> Vec<String> {
    let mut db = Database::open(dir).unwrap_or_else(|e| panic!("reopen {}: {e}", dir.display()));
    subjects
        .iter()
        .filter(|s| {
            db.query(&format!(
                "ASK {{ <http://ex/{s}> <http://ex/p> <http://ex/o> }}"
            ))
            .unwrap()
                == QueryResult::Ask(true)
        })
        .map(|s| s.to_string())
        .collect()
}

/// The `statusCode` of every query-log record (`<root>/logs/endpoint/`, the
/// default `querylog_path`) of statement `sparql`.
fn logged_status(root: &Path, sparql: &str) -> Vec<i64> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root.join("logs").join("endpoint"))
        .expect("the query log directory")
        .flatten()
    {
        let text = std::fs::read_to_string(entry.path()).unwrap();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let rec = parse(line);
            if rec.get("sparql").and_then(Json::as_str) == Some(sparql) {
                out.push(int(&rec, "statusCode"));
            }
        }
    }
    out
}

/// A REST failure envelope of a write that is not durable.
fn assert_rest_not_durable(what: &str, (st, body): (u16, String)) -> Json {
    assert_eq!(
        st, 500,
        "{what}: a write that was not saved must fail: {body}"
    );
    let j = parse(&body);
    assert_eq!(int(&j, "StatusCode"), 500, "{what}: {body}");
    assert!(
        text(&j, "StatusMsg").starts_with(NOT_DURABLE),
        "{what}: {body}"
    );
    j
}

/// An `/api` envelope of a write that is not durable.
fn assert_api_not_durable(what: &str, j: &Json) {
    assert_eq!(int(j, "StatusCode"), API_FAILED, "{what}: {j}");
    assert!(text(j, "StatusMsg").contains(NOT_DURABLE), "{what}: {j}");
}

#[test]
fn single_db_gserver_does_not_acknowledge_a_write_it_could_not_save() {
    let tmp = TmpDir::new("single");
    let dir = tmp.0.join("acked.db");
    build(&dir);
    let dir_s = dir.to_string_lossy().into_owned();

    // A durable write is acknowledged (200, no warning).
    let mut ok = Gserver::spawn(&tmp.0, &[&dir_s, "--port", "0"], None);
    let (st, body) = ok.request("POST", "/update", None, &insert("acked"));
    assert_eq!(st, 200, "{body}");
    let j = parse(&body);
    assert_eq!(int(&j, "changed"), 1);
    assert!(
        j.get("warning").is_none() && j.get("error").is_none(),
        "{body}"
    );
    ok.kill();

    // Every save fails from now on.
    let mut failing = Gserver::spawn(&tmp.0, &[&dir_s, "--port", "0"], Some(FSIZE_LIMIT));
    for (what, target, sparql) in [
        ("POST /update", "/update", insert("u1")),
        ("POST /sparql UPDATE", "/sparql", insert("u2")),
    ] {
        let (st, body) = failing.request("POST", target, None, &sparql);
        assert_eq!(
            st, 500,
            "{what}: a write that was not saved must fail: {body}"
        );
        let j = parse(&body);
        assert!(text(&j, "error").starts_with(NOT_DURABLE), "{what}: {body}");
        assert!(text(&j, "warning").contains("saving to"), "{what}: {body}");
        assert_eq!(int(&j, "changed"), 1, "{what}: {body}");
    }
    let (st, body) = failing.request("POST", "/update", None, &partial("u3"));
    assert_eq!(st, 500, "partial update: {body}");
    let j = parse(&body);
    let err = text(&j, "error");
    assert!(err.contains(NOT_DURABLE), "partial update: {body}");
    assert!(
        err.contains("before the failing one were applied"),
        "{body}"
    );
    // The writes are applied in memory (that is what the message says).
    let (st, body) = failing.request(
        "POST",
        "/sparql",
        None,
        "ASK { <http://ex/u1> <http://ex/p> <http://ex/o> }",
    );
    assert_eq!(st, 200, "{body}");
    assert!(body.contains("true"), "{body}");
    failing.kill();

    // After SIGKILL only the acknowledged write is there.
    assert_eq!(present(&dir, &["acked", "u1", "u2", "u3"]), ["acked"]);
}

#[test]
fn api_mode_gserver_does_not_acknowledge_a_write_it_could_not_save() {
    let tmp = TmpDir::new("api");
    let root = tmp.0.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let dir = root.join("acked.db");
    build(&dir);
    let root_s = root.to_string_lossy().into_owned();
    let args = [root_s.as_str(), "--api", "--port", "0", "--password", "pw"];
    let load = |s: &Gserver| {
        let (st, body) = s.request("POST", "/load?db_name=acked", Some(ROOT), "");
        assert_eq!(st, 200, "load: {body}");
    };

    // Durable writes are acknowledged.
    let mut ok = Gserver::spawn(&tmp.0, &args, None);
    load(&ok);
    let (st, body) = ok.request(
        "POST",
        "/update?db_name=acked",
        Some(ROOT),
        &insert("acked"),
    );
    assert_eq!(st, 200, "{body}");
    let j = parse(&body);
    assert_eq!(int(&j, "StatusCode"), 0);
    assert!(j.get("warning").is_none(), "{body}");
    let j = ok.api(&[
        ("operation", "query"),
        ("db_name", "acked"),
        ("sparql", &insert("acked_api")),
    ]);
    assert_eq!(int(&j, "StatusCode"), 0, "{j}");
    assert!(j.get("warning").is_none(), "{j}");
    ok.kill();

    let mut failing = Gserver::spawn(&tmp.0, &args, Some(FSIZE_LIMIT));
    load(&failing);
    // REST: HTTP 500.
    let j = assert_rest_not_durable(
        "REST /update",
        failing.request("POST", "/update?db_name=acked", Some(ROOT), &insert("r1")),
    );
    assert_eq!(int(&j, "changed"), 1);
    assert!(text(&j, "warning").starts_with(NOT_DURABLE), "{j}");
    assert_rest_not_durable(
        "REST /sparql UPDATE",
        failing.request("POST", "/sparql?db_name=acked", Some(ROOT), &insert("r2")),
    );
    let j = assert_rest_not_durable(
        "REST /upload",
        failing.request(
            "POST",
            "/upload?db_name=acked&format=ntriples",
            Some(ROOT),
            &nt("r3"),
        ),
    );
    assert_eq!(int(&j, "triples_inserted"), 1, "{j}");
    let j = assert_rest_not_durable(
        "REST /batchInsert",
        failing.request(
            "POST",
            "/batchInsert?db_name=acked&format=nt",
            Some(ROOT),
            &nt("r4"),
        ),
    );
    assert_eq!(int(&j, "applied"), 1, "{j}");
    // The base triple is removed in memory, but that is not durable either.
    let j = assert_rest_not_durable(
        "REST /batchRemove",
        failing.request(
            "POST",
            "/batchRemove?db_name=acked&format=nt",
            Some(ROOT),
            "<http://ex/kid> <http://ex/parent> <http://ex/mum> .\n",
        ),
    );
    assert_eq!(int(&j, "applied"), 1, "{j}");
    let (st, body) = failing.request("POST", "/begin?db_name=acked", Some(ROOT), "");
    assert_eq!(st, 200, "begin: {body}");
    let tid = text(&parse(&body), "TID").to_string();
    let (st, body) = failing.request(
        "POST",
        &format!("/tquery?tid={tid}"),
        Some(ROOT),
        &insert("r5"),
    );
    assert_eq!(st, 200, "tquery: {body}");
    assert_rest_not_durable(
        "REST /commit",
        failing.request("POST", &format!("/commit?tid={tid}"), Some(ROOT), ""),
    );
    let j = assert_rest_not_durable(
        "REST partial update",
        failing.request("POST", "/update?db_name=acked", Some(ROOT), &partial("r6")),
    );
    assert!(
        text(&j, "StatusMsg").contains("before the failing one were applied"),
        "{j}"
    );

    // /api: HTTP 200, StatusCode 1005.
    let j = failing.api(&[
        ("operation", "query"),
        ("db_name", "acked"),
        ("sparql", &insert("a1")),
    ]);
    assert_api_not_durable("/api query update", &j);
    assert_eq!(int(&j, "AnsNum"), 1, "{j}");
    assert!(text(&j, "warning").starts_with(NOT_DURABLE), "{j}");
    let j = failing.api(&[
        ("operation", "query"),
        ("db_name", "acked"),
        ("sparql", &partial("a2")),
    ]);
    assert_api_not_durable("/api partial update", &j);
    let j = failing.api(&[("operation", "begin"), ("db_name", "acked")]);
    assert_eq!(int(&j, "StatusCode"), 0, "{j}");
    let tid = text(&j, "TID").to_string();
    let j = failing.api(&[
        ("operation", "tquery"),
        ("db_name", "acked"),
        ("tid", &tid),
        ("sparql", &insert("a3")),
    ]);
    assert_eq!(int(&j, "StatusCode"), 0, "{j}");
    let j = failing.api(&[("operation", "commit"), ("db_name", "acked"), ("tid", &tid)]);
    assert_api_not_durable("/api commit", &j);
    assert!(text(&j, "warning").starts_with(NOT_DURABLE), "{j}");
    // reasonManage execute: the inferred triples are a write like any other.
    let rule = r#"{"rulename": "rel", "description": "d", "isenable": 1, "type": 1, "logic": 1,
        "conditions": [{"patterns": [{"subject": "?x", "predicate": "<http://ex/parent>",
        "object": "?y"}], "filters": [], "count_info": {}}],
        "return": {"source": "?y", "target": "?x", "label": "child"}}"#;
    let (st, body) = failing.request(
        "POST",
        "/api",
        None,
        &format!(
            "{{\"username\": \"root\", \"password\": \"pw\", \"operation\": \"reasonManage\", \
             \"db_name\": \"acked\", \"type\": \"1\", \"ruleinfo\": {rule}}}"
        ),
    );
    assert_eq!(st, 200, "{body}");
    assert_eq!(int(&parse(&body), "StatusCode"), 0, "add rule: {body}");
    // (batchRemove above took the only <parent> triple out in memory; put
    // it back so the rule infers something.)
    failing.request(
        "POST",
        "/batchInsert?db_name=acked&format=nt",
        Some(ROOT),
        "<http://ex/kid> <http://ex/parent> <http://ex/mum> .\n",
    );
    for ty in ["3", "4"] {
        let j = failing.api(&[
            ("operation", "reasonManage"),
            ("db_name", "acked"),
            ("type", ty),
            ("rulename", "rel"),
        ]);
        if ty == "3" {
            assert_eq!(int(&j, "StatusCode"), 0, "compile: {j}");
        } else {
            assert_api_not_durable("/api reasonManage execute", &j);
            assert!(text(&j, "warning").starts_with(NOT_DURABLE), "{j}");
        }
    }
    // The query log records each write with the outcome it was answered
    // with: the durable one as a success, a write that was not saved as the
    // failure it was answered as — REST 500, `/api` 1005 — never as 0.
    assert_eq!(logged_status(&root, &insert("acked")), [0]);
    assert_eq!(logged_status(&root, &insert("acked_api")), [0]);
    assert_eq!(logged_status(&root, &insert("r1")), [500], "REST /update");
    assert_eq!(logged_status(&root, &insert("r2")), [500], "REST /sparql");
    assert_eq!(
        logged_status(&root, &insert("a1")),
        [API_FAILED],
        "/api query"
    );
    failing.kill();

    // After SIGKILL exactly the acknowledged writes are there.
    assert_eq!(
        present(
            &dir,
            &[
                "acked",
                "acked_api",
                "r1",
                "r2",
                "r3",
                "r4",
                "r5",
                "r6",
                "a1",
                "a2",
                "a3"
            ]
        ),
        ["acked", "acked_api"]
    );
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(
        db.query("ASK { <http://ex/kid> <http://ex/parent> <http://ex/mum> }")
            .unwrap(),
        QueryResult::Ask(true),
        "the unacknowledged batchRemove is not durable"
    );
    assert_eq!(
        db.query("ASK { <http://ex/mum> ?p <http://ex/kid> }")
            .unwrap(),
        QueryResult::Ask(false),
        "the unacknowledged inference is not durable"
    );
}
