//! Data test: asynchronous operations and the task registry of the
//! upstream-compatible `/api` protocol ([`gstore::gapi::tasks`]).
//!
//! `async=true` answers an `opt_id` at once and runs the operation on the
//! task manager's runner threads; `checkOperationState` reports its state,
//! a `callback` URL receives its result, and `operationTaskList` /
//! `operationTaskCancel` (and the REST `/tasklist` / `/cancel`) list and
//! cancel running SPARQL statements and asynchronous operations alike. The
//! generic machinery is driven through the hidden test-only operation
//! `__asyncTest` and through `query` (upstream 1.4's asynchronous query).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use gstore::gapi::tasks::{self, TaskConfig, TaskSpec, TaskState};
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};

/// A unique, empty server root (a leftover of an earlier run with the same
/// pid is removed first: pids are reused).
fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("gstore_gtasks_{tag}_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn serve(server: ApiServer) -> (Arc<ApiServer>, SocketAddr, thread::JoinHandle<()>) {
    tasks::enable_test_ops(true);
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let h = thread::spawn(move || {
        s2.serve_forever();
    });
    (server, addr, h)
}

fn start(tag: &str) -> (Arc<ApiServer>, SocketAddr, PathBuf) {
    start_with(tag, TaskConfig::default())
}

fn start_with(tag: &str, config: TaskConfig) -> (Arc<ApiServer>, SocketAddr, PathBuf) {
    let root = tmp_root(tag);
    let server = ApiServer::bind(&root, "root", "127.0.0.1:0")
        .unwrap()
        .with_task_config(config);
    let (srv, addr, _h) = serve(server);
    (srv, addr, root)
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

fn send(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let mut bytes = raw.into_bytes();
    bytes.extend_from_slice(body);
    s.write_all(&bytes).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let split = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header end");
    let head = String::from_utf8_lossy(&resp[..split]).into_owned();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status");
    Reply {
        status,
        body: String::from_utf8_lossy(&resp[split + 4..]).into_owned(),
    }
}

/// `POST /api` with a JSON object body; the answer must be HTTP 200 JSON.
fn api_body(addr: &SocketAddr, body: &Json) -> Json {
    let r = send(
        addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        body.to_string().as_bytes(),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()
}

/// `POST /api` as `user` (password `pw`, or `root` for root) with string
/// parameters.
fn api_as(addr: &SocketAddr, user: &str, pairs: &[(&str, &str)]) -> Json {
    let pass = if user == "root" { "root" } else { "pw" };
    let mut body = Json::obj().with("username", user).with("password", pass);
    for (k, v) in pairs {
        body.set(*k, *v);
    }
    api_body(addr, &body)
}

fn api(addr: &SocketAddr, pairs: &[(&str, &str)]) -> Json {
    api_as(addr, "root", pairs)
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .expect("StatusCode")
}

fn msg(j: &Json) -> String {
    j.get("StatusMsg")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

fn int(j: &Json, key: &str) -> i64 {
    j.get(key)
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no integer {key} in {j}"))
}

fn opt_id_of(j: &Json) -> String {
    let id = j
        .get("opt_id")
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no opt_id string in {j}"))
        .to_string();
    assert!(
        id.parse::<u64>().is_ok(),
        "opt_id {id:?} is a decimal number"
    );
    id
}

fn basic(user: &str, pass: &str) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let input = format!("{user}:{pass}");
    let mut out = String::new();
    for chunk in input.as_bytes().chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let v = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[((v >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    format!("Basic {out}")
}

fn rest_as(addr: &SocketAddr, user: &str, method: &str, target: &str, body: &str) -> Reply {
    let pass = if user == "root" { "root" } else { "pw" };
    send(
        addr,
        method,
        target,
        &[("Authorization", &basic(user, pass))],
        body.as_bytes(),
    )
}

fn rest(addr: &SocketAddr, method: &str, target: &str, body: &str) -> Reply {
    rest_as(addr, "root", method, target, body)
}

fn build(addr: &SocketAddr, db: &str, data: &str) {
    let r = rest(addr, "POST", &format!("/build?db_name={db}"), data);
    assert_eq!(r.status, 200, "build: {}", r.body);
}

fn add_user(addr: &SocketAddr, name: &str, privileges: &str) {
    let r = rest(
        addr,
        "POST",
        &format!("/user/create?op_username={name}&op_password=pw&privileges={privileges}"),
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
}

/// Poll `checkOperationState` (as `user`) until the operation has ended.
fn wait_done_as(addr: &SocketAddr, user: &str, opt_id: &str) -> Json {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let j = api_as(
            addr,
            user,
            &[("operation", "checkOperationState"), ("opt_id", opt_id)],
        );
        assert_eq!(code(&j), 0, "{j}");
        if int(&j, "state") != 0 {
            return j;
        }
        assert!(Instant::now() < until, "operation {opt_id} never finished");
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_done(addr: &SocketAddr, opt_id: &str) -> Json {
    wait_done_as(addr, "root", opt_id)
}

/// `n` triples `:s<i> :p :o<i>`.
fn chain(n: usize) -> String {
    let mut ttl = String::from("@prefix : <http://ex/> .\n");
    for i in 0..n {
        ttl.push_str(&format!(":s{i} :p :o{i} .\n"));
    }
    ttl
}

/// A query that runs for a long time (a REGEX FILTER over a cross product,
/// checked for cancellation per row) unless cancelled.
const SLOW: &str = "SELECT (COUNT(*) AS ?n) WHERE { ?a ?p1 ?b . ?c ?p2 ?d \
                    FILTER(REGEX(CONCAT(STR(?a), STR(?d), STR(?b), STR(?c)), \"(x+x+)+y\")) }";

/// The running tasks `operationTaskList` shows `user` (status 0).
fn running_tasks_as(addr: &SocketAddr, user: &str) -> Vec<Json> {
    let j = api_as(addr, user, &[("operation", "operationTaskList")]);
    assert_eq!(code(&j), 0, "{j}");
    j.get("list").and_then(Json::as_array).unwrap().to_vec()
}

/// Wait until `operationTaskList` shows a running `query` task; its opt_id.
fn wait_running_query(addr: &SocketAddr) -> String {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(t) = running_tasks_as(addr, "root")
            .into_iter()
            .find(|t| t.get("operation").and_then(Json::as_str) == Some("query"))
        {
            return t.get("opt_id").and_then(Json::as_str).unwrap().to_string();
        }
        assert!(
            Instant::now() < until,
            "the query never showed up as running"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn async_test_operation_lifecycle() {
    let (srv, addr, _root) = start("lifecycle");
    // Synchronously, the test operation simply answers.
    let j = api(&addr, &[("operation", "__asyncTest"), ("success_num", "2")]);
    assert_eq!((code(&j), msg(&j)), (0, "test operation done".to_string()));
    assert!(j.get("opt_id").is_none(), "{j}");

    // Asynchronously: an opt_id at once (upstream's answer), the result later.
    let started = Instant::now();
    let j = api_body(
        &addr,
        &Json::obj()
            .with("operation", "__asyncTest")
            .with("username", "root")
            .with("password", "root")
            .with("async", true)
            .with("sleep_ms", 3000)
            .with("success_num", 5)
            .with("failed_num", 2),
    );
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "answered at once"
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "Operation Success.".to_string()),
        "{j}"
    );
    let opt_id = opt_id_of(&j);
    let id: u64 = opt_id.parse().unwrap();
    // The upstream id layout: a millisecond timestamp since 2024-03-12.
    let t = tasks::opt_id_time_ms(id);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!(t <= now + 5 && now - t < 60_000, "{t} vs {now}");

    // While it runs: state 0 with its progress (or the initial message).
    let j = api(
        &addr,
        &[("operation", "checkOperationState"), ("opt_id", &opt_id)],
    );
    assert_eq!((code(&j), int(&j, "state")), (0, 0), "{j}");
    assert!(
        ["test operation running", "Operation success"].contains(&msg(&j).as_str()),
        "{j}"
    );
    let listed = running_tasks_as(&addr, "root");
    let item = listed
        .iter()
        .find(|t| t.get("opt_id").and_then(Json::as_str) == Some(opt_id.as_str()))
        .unwrap_or_else(|| panic!("{opt_id} not in {listed:?}"));
    assert_eq!(
        item.get("operation").and_then(Json::as_str),
        Some("__asyncTest")
    );
    assert_eq!(int(item, "async"), 1);
    assert_eq!(int(item, "status"), 0);
    assert_eq!(int(item, "endTime"), 0);
    // REST /tasklist lists it as well.
    let r = rest(&addr, "GET", "/tasklist", "");
    assert!(
        r.body.contains(&format!("\"opt_id\":\"{opt_id}\"")),
        "{}",
        r.body
    );

    // Done: state 1, the final message and the counts.
    let j = wait_done(&addr, &opt_id);
    assert_eq!(int(&j, "state"), 1, "{j}");
    assert_eq!(msg(&j), "test operation done");
    assert_eq!((int(&j, "success_num"), int(&j, "failed_num")), (5, 2));
    let info = srv.task_manager().get(id).unwrap();
    assert_eq!(info.state, TaskState::Succeeded);
    assert!(info.end_ms >= info.start_ms + 2900, "{info:?}");
    assert_eq!(info.user, "root");
    // The finished operation is listed under status 1, not 0.
    let j = api(
        &addr,
        &[("operation", "operationTaskList"), ("status", "1")],
    );
    let list = j.get("list").and_then(Json::as_array).unwrap();
    let item = list
        .iter()
        .find(|t| t.get("opt_id").and_then(Json::as_str) == Some(opt_id.as_str()))
        .expect("listed as finished");
    assert!(int(item, "endTime") >= int(item, "startTime"));
    assert!(int(item, "queryTime") >= 2900);
    assert!(!running_tasks_as(&addr, "root")
        .iter()
        .any(|t| t.get("opt_id").and_then(Json::as_str) == Some(opt_id.as_str())));
}

#[test]
fn check_operation_state_answers_failures_and_errors() {
    let (_srv, addr, _root) = start("states");
    // Upstream's parameter errors.
    let j = api(&addr, &[("operation", "checkOperationState")]);
    assert_eq!(code(&j), 1005);
    assert_eq!(msg(&j), "the value of opt_id can not be empty!");
    for bad in ["123456789", "not-a-number"] {
        let j = api(
            &addr,
            &[("operation", "checkOperationState"), ("opt_id", bad)],
        );
        assert_eq!((code(&j), msg(&j)), (1005, "opt_id not found.".to_string()));
    }
    // A failing and a panicking operation: state -1 with their message.
    let failing = opt_id_of(&api(
        &addr,
        &[
            ("operation", "__asyncTest"),
            ("async", "true"),
            ("fail", "1"),
        ],
    ));
    let panicking = opt_id_of(&api(
        &addr,
        &[
            ("operation", "__asyncTest"),
            ("async", "1"),
            ("panic", "true"),
        ],
    ));
    let j = wait_done(&addr, &failing);
    assert_eq!((code(&j), int(&j, "state")), (0, -1), "{j}");
    assert_eq!(msg(&j), "test operation failed");
    let j = wait_done(&addr, &panicking);
    assert_eq!(int(&j, "state"), -1, "{j}");
    assert!(msg(&j).contains("panicked"), "{j}");
    // The server and its runners survive: another operation succeeds.
    let ok = opt_id_of(&api(
        &addr,
        &[("operation", "__asyncTest"), ("async", "true")],
    ));
    assert_eq!(int(&wait_done(&addr, &ok), "state"), 1);
    // async on an operation without asynchronous support is ignored.
    let j = api(&addr, &[("operation", "testConnect"), ("async", "true")]);
    assert_eq!(code(&j), 0);
    assert!(j.get("opt_id").is_none(), "{j}");
    // A bad callback address is refused up front.
    let j = api(
        &addr,
        &[
            ("operation", "__asyncTest"),
            ("async", "true"),
            ("callback", "ftp://example.org/x"),
        ],
    );
    assert_eq!(code(&j), 1003, "{j}");
    assert!(msg(&j).contains("callback"), "{j}");
    // The dispatcher's checks still come first: no credentials, no task.
    let j = api_body(
        &addr,
        &Json::obj()
            .with("operation", "__asyncTest")
            .with("async", true),
    );
    assert_eq!(code(&j), 1003, "{j}");
}

#[test]
fn async_query_result_is_fetched_with_check_operation_state() {
    let (_srv, addr, root) = start("aquery");
    build(&addr, "aqdb", &chain(3));
    let j = api(
        &addr,
        &[
            ("operation", "query"),
            ("db_name", "aqdb"),
            (
                "sparql",
                "SELECT ?s WHERE { ?s <http://ex/p> ?o } ORDER BY ?s",
            ),
            ("async", "true"),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "Operation Success.".to_string()),
        "{j}"
    );
    assert!(j.get("results").is_none(), "only the opt_id at first: {j}");
    let opt_id = opt_id_of(&j);
    // Finished: upstream answers the query result itself, plus "state": 1.
    let j = wait_done(&addr, &opt_id);
    assert_eq!(int(&j, "state"), 1, "{j}");
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!(int(&j, "AnsNum"), 3, "{j}");
    let bindings = j
        .get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("no results in {j}"));
    assert_eq!(bindings.len(), 3);
    // Written to <root>/tasks/<opt_id>.json — not to the export directory
    // (upstream's place), which anyone may download from.
    let file = root.join("tasks").join(format!("{opt_id}.json"));
    assert!(!root.join("export").join(format!("{opt_id}.json")).exists());
    let saved = json::parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(saved.get("AnsNum").and_then(Json::as_i64), Some(3));
    let j = api(
        &addr,
        &[("operation", "operationTaskList"), ("status", "1")],
    );
    let item = j
        .get("list")
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .find(|t| t.get("opt_id").and_then(Json::as_str) == Some(opt_id.as_str()))
        .cloned()
        .expect("listed");
    assert_eq!(
        item.get("resultFile").and_then(Json::as_str),
        Some(format!("{opt_id}.json").as_str())
    );
    assert_eq!(item.get("database").and_then(Json::as_str), Some("aqdb"));
    assert!(item
        .get("sparql")
        .and_then(Json::as_str)
        .unwrap()
        .starts_with("SELECT ?s"));
    // A query that fails asynchronously: state -1 with its error message.
    let j = api(
        &addr,
        &[
            ("operation", "query"),
            ("db_name", "aqdb"),
            ("sparql", "SELECT WHERE {"),
            ("async", "true"),
        ],
    );
    let j = wait_done(&addr, &opt_id_of(&j));
    assert_eq!(int(&j, "state"), -1, "{j}");
    assert!(!msg(&j).is_empty());
    // The result file gone: upstream's read error.
    std::fs::remove_file(&file).unwrap();
    let j = api(
        &addr,
        &[("operation", "checkOperationState"), ("opt_id", &opt_id)],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (9, "Read result file failed.".to_string())
    );
}

/// Accept one HTTP request on `listener`, answer `status`, and send the
/// request line, headers and body back.
fn capture_one(listener: TcpListener, status: u16) -> mpsc::Receiver<(String, String)> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let (head, body) = loop {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "callback connection closed early");
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw).into_owned();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                if body.len() >= len {
                    break (head.to_string(), body.to_string());
                }
            }
        };
        let reply = format!("HTTP/1.1 {status} X\r\nContent-Length: 2\r\n\r\nok");
        let _ = s.write_all(reply.as_bytes());
        tx.send((head, body)).unwrap();
    });
    rx
}

#[test]
fn callback_receives_the_result_and_failures_are_tolerated() {
    // The callbacks go to listeners on the loopback interface.
    let (srv, addr, _root) = start_with(
        "callback",
        TaskConfig {
            callback_allow_local: true,
            ..TaskConfig::default()
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook?src=gstore", listener.local_addr().unwrap());
    let got = capture_one(listener, 200);
    let j = api(
        &addr,
        &[
            ("operation", "__asyncTest"),
            ("async", "true"),
            ("success_num", "9"),
            ("callback", &url),
        ],
    );
    let opt_id = opt_id_of(&j);
    let (head, body) = got
        .recv_timeout(Duration::from_secs(30))
        .expect("the callback was delivered");
    assert!(head.starts_with("POST /hook?src=gstore HTTP/1.1"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/json"),
        "{head}"
    );
    let pushed = json::parse(&body).unwrap();
    assert_eq!(
        pushed.get("opt_id").and_then(Json::as_str),
        Some(opt_id.as_str())
    );
    assert_eq!(code(&pushed), 0);
    assert_eq!(msg(&pushed), "test operation done");
    assert_eq!(int(&pushed, "success_num"), 9);
    // By the time the callback arrives the state already reads finished.
    let j = api(
        &addr,
        &[("operation", "checkOperationState"), ("opt_id", &opt_id)],
    );
    assert_eq!(int(&j, "state"), 1, "{j}");
    srv.task_manager().wait_async_idle(Duration::from_secs(10));
    let info = srv.task_manager().get(opt_id.parse().unwrap()).unwrap();
    assert_eq!(
        info.callback_status.as_deref(),
        Some("delivered (HTTP 200)")
    );

    // An unreachable callback does not fail the operation.
    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let j = api(
        &addr,
        &[
            ("operation", "__asyncTest"),
            ("async", "true"),
            ("callback", &format!("http://127.0.0.1:{dead_port}/cb")),
        ],
    );
    let dead = opt_id_of(&j);
    assert_eq!(int(&wait_done(&addr, &dead), "state"), 1);
    assert!(srv.task_manager().wait_async_idle(Duration::from_secs(30)));
    let info = srv.task_manager().get(dead.parse().unwrap()).unwrap();
    assert!(
        info.callback_status
            .as_deref()
            .unwrap()
            .starts_with("failed:"),
        "{info:?}"
    );
    // A failed operation is pushed too (an HTTP error answer is tolerated).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/cb", listener.local_addr().unwrap());
    let got = capture_one(listener, 500);
    let j = api(
        &addr,
        &[
            ("operation", "__asyncTest"),
            ("async", "true"),
            ("fail", "true"),
            ("callback", &url),
        ],
    );
    let failed = opt_id_of(&j);
    let (_, body) = got.recv_timeout(Duration::from_secs(30)).unwrap();
    let pushed = json::parse(&body).unwrap();
    assert_eq!(
        (code(&pushed), msg(&pushed)),
        (1005, "test operation failed".to_string())
    );
    assert_eq!(
        pushed.get("opt_id").and_then(Json::as_str),
        Some(failed.as_str())
    );
    assert!(srv.task_manager().wait_async_idle(Duration::from_secs(30)));
    let info = srv.task_manager().get(failed.parse().unwrap()).unwrap();
    assert_eq!(info.callback_status.as_deref(), Some("failed: HTTP 500"));
}

#[test]
fn a_running_query_is_cancelled_through_the_upstream_operation() {
    let (srv, addr, _root) = start("cancel_up");
    build(&addr, "slow", &chain(1200));
    let addr_q = addr;
    let query = thread::spawn(move || {
        api(
            &addr_q,
            &[
                ("operation", "query"),
                ("db_name", "slow"),
                ("sparql", SLOW),
            ],
        )
    });
    let opt_id = wait_running_query(&addr);
    let item = running_tasks_as(&addr, "root")
        .into_iter()
        .find(|t| t.get("opt_id").and_then(Json::as_str) == Some(opt_id.as_str()))
        .unwrap();
    assert_eq!(int(&item, "async"), 0);
    assert_eq!(item.get("database").and_then(Json::as_str), Some("slow"));
    assert_eq!(item.get("sparql").and_then(Json::as_str), Some(SLOW));
    // A synchronous statement has no checkOperationState record (upstream).
    let j = api(
        &addr,
        &[("operation", "checkOperationState"), ("opt_id", &opt_id)],
    );
    assert_eq!(code(&j), 1005);

    let j = api(
        &addr,
        &[("operation", "operationTaskCancel"), ("opt_id", &opt_id)],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "query cancel successfully".to_string()),
        "{j}"
    );
    let answer = query.join().unwrap();
    assert_ne!(code(&answer), 0, "the cancelled query fails: {answer}");
    assert!(msg(&answer).to_lowercase().contains("cancel"), "{answer}");
    // Upstream's answers for a task that is no longer running.
    let j = api(
        &addr,
        &[("operation", "operationTaskCancel"), ("opt_id", &opt_id)],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (1005, "task already cancel".to_string())
    );
    let j = api(
        &addr,
        &[("operation", "operationTaskList"), ("status", "-1")],
    );
    assert!(j
        .get("list")
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .any(|t| t.get("opt_id").and_then(Json::as_str) == Some(opt_id.as_str())));
    assert_eq!(
        srv.task_manager()
            .get(opt_id.parse().unwrap())
            .unwrap()
            .state,
        TaskState::Cancelled
    );
    for bad in ["", "999", "abc"] {
        let j = api(
            &addr,
            &[("operation", "operationTaskCancel"), ("opt_id", bad)],
        );
        assert_eq!(code(&j), 1005, "{bad:?}: {j}");
    }
    // A finished query: "task already finish".
    let j = api(
        &addr,
        &[
            ("operation", "query"),
            ("db_name", "slow"),
            ("sparql", "ASK { ?s ?p ?o }"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    let j = api(
        &addr,
        &[("operation", "operationTaskList"), ("status", "1")],
    );
    let done = j.get("list").and_then(Json::as_array).unwrap();
    let last = done.last().expect("the ASK is in the history");
    let last_id = last.get("opt_id").and_then(Json::as_str).unwrap();
    let j = api(
        &addr,
        &[("operation", "operationTaskCancel"), ("opt_id", last_id)],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (1005, "task already finish, do not cancel".to_string())
    );
}

#[test]
fn queries_are_cancelled_across_the_rest_and_upstream_apis() {
    let (_srv, addr, _root) = start("cancel_cross");
    build(&addr, "slow", &chain(1200));
    // An /api query, cancelled through REST /cancel.
    let addr_q = addr;
    let query = thread::spawn(move || {
        api(
            &addr_q,
            &[
                ("operation", "query"),
                ("db_name", "slow"),
                ("sparql", SLOW),
            ],
        )
    });
    let opt_id = wait_running_query(&addr);
    let r = rest(&addr, "GET", "/tasklist", "");
    assert!(r.body.contains(&format!("\"id\":{opt_id},")), "{}", r.body);
    let r = rest(&addr, "POST", &format!("/cancel?id={opt_id}"), "");
    assert_eq!(r.status, 200, "{}", r.body);
    let answer = query.join().unwrap();
    assert!(msg(&answer).to_lowercase().contains("cancel"), "{answer}");
    let r = rest(&addr, "POST", &format!("/cancel?id={opt_id}"), "");
    assert_eq!(r.status, 404, "no longer running: {}", r.body);

    // A REST query, cancelled through operationTaskCancel.
    let addr_q = addr;
    let query = thread::spawn(move || rest(&addr_q, "POST", "/sparql?db_name=slow", SLOW));
    let opt_id = wait_running_query(&addr);
    let j = api(
        &addr,
        &[("operation", "operationTaskCancel"), ("opt_id", &opt_id)],
    );
    assert_eq!(code(&j), 0, "{j}");
    let r = query.join().unwrap();
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.to_lowercase().contains("cancel"), "{}", r.body);

    // An asynchronous query: one task, cancelled by its opt_id.
    let j = api(
        &addr,
        &[
            ("operation", "query"),
            ("db_name", "slow"),
            ("sparql", SLOW),
            ("async", "true"),
        ],
    );
    let opt_id = opt_id_of(&j);
    assert_eq!(
        wait_running_query(&addr),
        opt_id,
        "the statement runs under the operation's id"
    );
    let running = running_tasks_as(&addr, "root");
    assert_eq!(running.len(), 1, "{running:?}");
    assert_eq!(int(&running[0], "async"), 1);
    let j = api(
        &addr,
        &[("operation", "operationTaskCancel"), ("opt_id", &opt_id)],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "query cancel successfully".to_string()),
        "{j}"
    );
    let j = wait_done(&addr, &opt_id);
    assert_eq!(int(&j, "state"), -1, "{j}");
    assert!(msg(&j).to_lowercase().contains("cancel"), "{j}");
}

/// A graph-function query that runs until cancelled: PageRank with a
/// negative tolerance never converges, and `maxIter` is two billion sweeps.
/// The PR loop polls the query's cancel flag every sweep (and every 1024
/// vertices).
const SLOW_GFUN: &str = "SELECT (PR(true, {}, 3, alpha=0.85, maxIter=2000000000, tol=-1) AS ?pr) \
                         WHERE {}";

/// Graph functions honour the same cancellation as every other query: an
/// `/api` query (synchronous and `async=true`) cancelled through
/// `operationTaskCancel` (the async one then reported by
/// `checkOperationState`), and a REST query cancelled through `/cancel`.
#[test]
fn graph_function_queries_are_cancelled_like_any_query() {
    let (_srv, addr, _root) = start("cancel_gfun");
    build(&addr, "slow", &chain(300));
    let gfun_args: [(&str, &str); 3] = [
        ("operation", "query"),
        ("db_name", "slow"),
        ("sparql", SLOW_GFUN),
    ];

    // A synchronous /api query, cancelled through operationTaskCancel.
    let addr_q = addr;
    let query = thread::spawn(move || api(&addr_q, &gfun_args));
    let opt_id = wait_running_query(&addr);
    let j = api(
        &addr,
        &[("operation", "operationTaskCancel"), ("opt_id", &opt_id)],
    );
    assert_eq!(code(&j), 0, "{j}");
    let answer = query.join().unwrap();
    assert_ne!(code(&answer), 0, "the cancelled query fails: {answer}");
    assert!(msg(&answer).to_lowercase().contains("cancel"), "{answer}");

    // An asynchronous /api query: checkOperationState reports the cancel.
    let mut async_args = gfun_args.to_vec();
    async_args.push(("async", "true"));
    let opt_id = opt_id_of(&api(&addr, &async_args));
    assert_eq!(wait_running_query(&addr), opt_id);
    let j = api(
        &addr,
        &[("operation", "operationTaskCancel"), ("opt_id", &opt_id)],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "query cancel successfully".to_string()),
        "{j}"
    );
    let j = wait_done(&addr, &opt_id);
    assert_eq!(int(&j, "state"), -1, "{j}");
    assert!(msg(&j).to_lowercase().contains("cancel"), "{j}");

    // A REST query, cancelled through REST /cancel.
    let addr_q = addr;
    let query = thread::spawn(move || rest(&addr_q, "POST", "/sparql?db_name=slow", SLOW_GFUN));
    let opt_id = wait_running_query(&addr);
    let r = rest(&addr, "POST", &format!("/cancel?id={opt_id}"), "");
    assert_eq!(r.status, 200, "{}", r.body);
    let r = query.join().unwrap();
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.to_lowercase().contains("cancel"), "{}", r.body);

    // The database still answers graph-function queries afterwards.
    let j = api(
        &addr,
        &[
            ("operation", "query"),
            ("db_name", "slow"),
            ("sparql", "SELECT (PR(true, {}, 1, alpha=0.85, maxIter=5, tol=0.1) AS ?pr) WHERE {}"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
}

/// A graceful shutdown cancels a graph-function query still running past
/// the grace period, as it does any other query, and returns in bounded
/// time.
#[test]
fn shutdown_cancels_a_running_graph_function_query() {
    let root = tmp_root("shutdown_gfun");
    let server = ApiServer::bind(&root, "root", "127.0.0.1:0")
        .unwrap()
        .with_shutdown_grace(Duration::from_millis(300));
    let (srv, addr, serving) = serve(server);
    build(&addr, "slow", &chain(300));
    let addr_q = addr;
    let query = thread::spawn(move || rest(&addr_q, "POST", "/sparql?db_name=slow", SLOW_GFUN));
    wait_running_query(&addr);
    let started = Instant::now();
    srv.shutdown();
    serving.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the shutdown waited {:?}",
        started.elapsed()
    );
    let r = query.join().unwrap();
    assert!(r.body.to_lowercase().contains("cancel"), "{} {}", r.status, r.body);
}

/// An UPDATE whose WHERE clause runs that same graph function (in a
/// sub-SELECT) is cancelled like a query — through `operationTaskCancel`,
/// REST `/cancel`, and a shutdown past its grace period — applies nothing,
/// and releases the database: neither the next request nor the shutdown's
/// final save waits for the function to finish.
#[test]
fn an_update_running_a_graph_function_is_cancelled_like_a_query() {
    let root = tmp_root("cancel_gfun_update");
    let server = ApiServer::bind(&root, "root", "127.0.0.1:0")
        .unwrap()
        .with_check_point(false)
        .with_shutdown_grace(Duration::from_millis(300));
    let (srv, addr, serving) = serve(server);
    build(&addr, "slow", &chain(300));
    let update = "INSERT { <http://ex/x> <http://ex/pr> ?pr } WHERE { { SELECT (PR(true, {}, 3, \
                  alpha=0.85, maxIter=2000000000, tol=-1) AS ?pr) WHERE {} } }";
    let inserted = |addr: &SocketAddr| {
        let j = api(
            addr,
            &[
                ("operation", "query"),
                ("db_name", "slow"),
                ("sparql", "SELECT ?o WHERE { <http://ex/x> ?p ?o }"),
            ],
        );
        assert_eq!(code(&j), 0, "{j}");
        int(&j, "AnsNum") > 0
    };

    // A synchronous /api update, cancelled through operationTaskCancel.
    let addr_q = addr;
    let query = thread::spawn(move || {
        api(
            &addr_q,
            &[
                ("operation", "query"),
                ("db_name", "slow"),
                ("sparql", update),
            ],
        )
    });
    let opt_id = wait_running_query(&addr);
    let j = api(
        &addr,
        &[("operation", "operationTaskCancel"), ("opt_id", &opt_id)],
    );
    assert_eq!(code(&j), 0, "{j}");
    let answer = query.join().unwrap();
    assert_ne!(code(&answer), 0, "the cancelled update fails: {answer}");
    assert!(msg(&answer).to_lowercase().contains("cancel"), "{answer}");
    assert!(!inserted(&addr), "a cancelled update applies nothing");

    // A REST update, cancelled through REST /cancel.
    let addr_q = addr;
    let query = thread::spawn(move || rest(&addr_q, "POST", "/update?db_name=slow", update));
    let opt_id = wait_running_query(&addr);
    let r = rest(&addr, "POST", &format!("/cancel?id={opt_id}"), "");
    assert_eq!(r.status, 200, "{}", r.body);
    let r = query.join().unwrap();
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(r.body.to_lowercase().contains("cancel"), "{}", r.body);
    assert!(!inserted(&addr));

    // A graceful shutdown with the database dirty (check_point off) and
    // the update still running: it is cancelled, and the final save runs.
    let j = api(
        &addr,
        &[
            ("operation", "query"),
            ("db_name", "slow"),
            (
                "sparql",
                "INSERT DATA { <http://ex/y> <http://ex/p> <http://ex/z> }",
            ),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    let addr_q = addr;
    let query = thread::spawn(move || rest(&addr_q, "POST", "/update?db_name=slow", update));
    wait_running_query(&addr);
    let started = Instant::now();
    srv.shutdown();
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        serving.join().unwrap();
        let _ = done_tx.send(());
    });
    assert!(
        done_rx.recv_timeout(Duration::from_secs(30)).is_ok(),
        "the shutdown never finished: the update kept the database locked"
    );
    assert!(started.elapsed() < Duration::from_secs(30));
    let r = query.join().unwrap();
    assert!(
        r.body.to_lowercase().contains("cancel"),
        "{} {}",
        r.status,
        r.body
    );
}

/// Graph functions stay within the server's output bound (`conf.ini`
/// `[query] gfun_max_output_bytes`): a call whose JSON would exceed it fails
/// the query with an error instead of building it, and the server goes on
/// answering. (The row cap `max_output_size` caps rows as for any query: a
/// graph function's result is one row.)
#[test]
fn graph_function_output_is_bounded_by_the_server_setting() {
    let root = tmp_root("gfun_bound");
    let settings = gstore::QuerySettings {
        gfun_max_output_bytes: 64,
        ..gstore::QuerySettings::default()
    };
    let server = ApiServer::bind(&root, "root", "127.0.0.1:0")
        .unwrap()
        .with_query_settings(settings);
    let (_srv, addr, _h) = serve(server);
    build(&addr, "big", &chain(300));
    let all = "SELECT (kHopNeighbor(<http://ex/s1>, false, 2, {}) AS ?n) WHERE {}";
    let paths = "SELECT (PR(true, {}, -1, alpha=0.85, maxIter=5, tol=0.1) AS ?pr) WHERE {}";
    for q in [all, paths] {
        let j = api(&addr, &[("operation", "query"), ("db_name", "big"), ("sparql", q)]);
        if q == paths {
            assert_ne!(code(&j), 0, "300 vertices' scores exceed 64 bytes: {j}");
            assert!(msg(&j).contains("bytes"), "{j}");
        } else {
            assert_eq!(code(&j), 0, "a small result fits: {j}");
        }
        let r = rest(&addr, "POST", "/sparql?db_name=big", q);
        if q == paths {
            assert_eq!(r.status, 400, "{}", r.body);
        } else {
            assert_eq!(r.status, 200, "{}", r.body);
        }
    }
    let j = api(&addr, &[("operation", "query"), ("db_name", "big"), ("sparql", "ASK { ?s ?p ?o }")]);
    assert_eq!(code(&j), 0, "{j}");
}

#[test]
fn users_see_and_cancel_only_their_own_tasks() {
    let (_srv, addr, _root) = start("owners");
    build(&addr, "slow", &chain(1200));
    add_user(&addr, "alice", "query");
    add_user(&addr, "bob", "query,update");
    add_user(&addr, "admin2", "admin");
    // alice's asynchronous query.
    let j = api_as(
        &addr,
        "alice",
        &[
            ("operation", "query"),
            ("db_name", "slow"),
            ("sparql", SLOW),
            ("async", "true"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    let opt_id = opt_id_of(&j);
    let _ = wait_running_query(&addr);
    let ids = |user: &str| -> Vec<String> {
        running_tasks_as(&addr, user)
            .iter()
            .map(|t| t.get("opt_id").and_then(Json::as_str).unwrap().to_string())
            .collect()
    };
    assert_eq!(ids("alice"), vec![opt_id.clone()]);
    assert!(ids("bob").is_empty(), "bob does not see alice's task");
    assert_eq!(
        ids("admin2"),
        vec![opt_id.clone()],
        "an admin sees every task"
    );
    // bob can neither read nor cancel it.
    let j = api_as(
        &addr,
        "bob",
        &[("operation", "checkOperationState"), ("opt_id", &opt_id)],
    );
    assert_eq!((code(&j), msg(&j)), (1005, "opt_id not found.".to_string()));
    let j = api_as(
        &addr,
        "bob",
        &[("operation", "operationTaskCancel"), ("opt_id", &opt_id)],
    );
    assert_eq!((code(&j), msg(&j)), (1005, "opt_id not found".to_string()));
    let r = rest_as(&addr, "bob", "POST", &format!("/cancel?id={opt_id}"), "");
    assert_eq!(r.status, 403, "{}", r.body);
    // alice polls and cancels her own.
    let j = api_as(
        &addr,
        "alice",
        &[("operation", "checkOperationState"), ("opt_id", &opt_id)],
    );
    assert_eq!((code(&j), int(&j, "state")), (0, 0), "{j}");
    let j = api_as(
        &addr,
        "alice",
        &[("operation", "operationTaskCancel"), ("opt_id", &opt_id)],
    );
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!(int(&wait_done_as(&addr, "alice", &opt_id), "state"), -1);
    // The test operation stays root-only.
    let j = api_as(&addr, "admin2", &[("operation", "__asyncTest")]);
    assert_eq!(code(&j), 0, "{j}");
    let j = api_as(
        &addr,
        "alice",
        &[("operation", "__asyncTest"), ("async", "true")],
    );
    assert_eq!(code(&j), 1004, "{j}");
}

#[test]
fn the_task_list_pages_like_upstream() {
    let (srv, addr, _root) = start("paging");
    let mut ids = Vec::new();
    for _ in 0..5 {
        let j = api(&addr, &[("operation", "__asyncTest"), ("async", "true")]);
        ids.push(opt_id_of(&j));
    }
    assert!(srv.task_manager().wait_async_idle(Duration::from_secs(30)));
    let page = |no: &str, size: &str| {
        api(
            &addr,
            &[
                ("operation", "operationTaskList"),
                ("status", "1"),
                ("pageNo", no),
                ("pageSize", size),
            ],
        )
    };
    let j = page("2", "2");
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!(
        (
            int(&j, "pageNo"),
            int(&j, "pageSize"),
            int(&j, "totalPage"),
            int(&j, "totalSize")
        ),
        (2, 2, 3, 5)
    );
    let got: Vec<&str> = j
        .get("list")
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .map(|t| t.get("opt_id").and_then(Json::as_str).unwrap())
        .collect();
    assert_eq!(
        got,
        vec![ids[2].as_str(), ids[3].as_str()],
        "ordered by opt_id"
    );
    // Defaults: page 1 of 10; status 0 (running) by default; any other
    // status lists everything.
    let j = page("", "");
    assert_eq!((int(&j, "pageNo"), int(&j, "pageSize")), (1, 10));
    let j = api(&addr, &[("operation", "operationTaskList")]);
    assert_eq!(int(&j, "totalSize"), 0, "nothing running: {j}");
    let j = api(
        &addr,
        &[("operation", "operationTaskList"), ("status", "2")],
    );
    assert_eq!(int(&j, "totalSize"), 5);
    let j = page("x", "2");
    assert_eq!(code(&j), 1003, "{j}");
    // Upstream field types.
    let item = j_first(&api(
        &addr,
        &[("operation", "operationTaskList"), ("status", "9")],
    ));
    for key in ["opt_id", "operation", "database", "sparql", "resultFile"] {
        assert!(
            item.get(key).and_then(Json::as_str).is_some(),
            "{key}: {item}"
        );
    }
    for key in ["async", "status", "startTime", "endTime", "queryTime"] {
        assert!(
            item.get(key).and_then(Json::as_i64).is_some(),
            "{key}: {item}"
        );
    }
}

fn j_first(j: &Json) -> Json {
    j.get("list").and_then(Json::as_array).unwrap()[0].clone()
}

#[test]
fn finished_results_are_retained_within_bounds() {
    let (_srv, addr, _root) = start_with(
        "retention",
        TaskConfig {
            max_finished: 2,
            ..TaskConfig::default()
        },
    );
    let mut ids = Vec::new();
    for _ in 0..4 {
        let j = api(&addr, &[("operation", "__asyncTest"), ("async", "true")]);
        let id = opt_id_of(&j);
        wait_done(&addr, &id);
        ids.push(id);
    }
    for (i, id) in ids.iter().enumerate() {
        let j = api(
            &addr,
            &[("operation", "checkOperationState"), ("opt_id", id)],
        );
        if i < 2 {
            assert_eq!(msg(&j), "opt_id not found.", "{i}: {j}");
        } else {
            assert_eq!(int(&j, "state"), 1, "{i}: {j}");
        }
    }
    let j = api(
        &addr,
        &[("operation", "operationTaskList"), ("status", "1")],
    );
    assert_eq!(int(&j, "totalSize"), 2, "{j}");

    // And for a limited time.
    let (_srv, addr, _root) = start_with(
        "ttl",
        TaskConfig {
            retention: Duration::from_millis(300),
            ..TaskConfig::default()
        },
    );
    let id = opt_id_of(&api(
        &addr,
        &[("operation", "__asyncTest"), ("async", "true")],
    ));
    assert_eq!(int(&wait_done(&addr, &id), "state"), 1);
    thread::sleep(Duration::from_millis(400));
    let j = api(
        &addr,
        &[("operation", "checkOperationState"), ("opt_id", &id)],
    );
    assert_eq!(msg(&j), "opt_id not found.", "{j}");
}

#[test]
fn shutdown_cancels_running_operations_and_waits_bounded() {
    let root = tmp_root("shutdown");
    let server = ApiServer::bind(&root, "root", "127.0.0.1:0")
        .unwrap()
        .with_shutdown_grace(Duration::from_millis(300))
        .with_task_config(TaskConfig {
            shutdown_wait: Duration::from_millis(1500),
            ..TaskConfig::default()
        });
    let (srv, addr, serving) = serve(server);
    // One operation that stops when cancelled, one that ignores it.
    let polite = opt_id_of(&api(
        &addr,
        &[
            ("operation", "__asyncTest"),
            ("async", "true"),
            ("sleep_ms", "60000"),
        ],
    ));
    let stubborn = opt_id_of(&api(
        &addr,
        &[
            ("operation", "__asyncTest"),
            ("async", "true"),
            ("sleep_ms", "8000"),
            ("ignore_cancel", "true"),
        ],
    ));
    // Wait until both have started (they report progress first thing).
    let until = Instant::now() + Duration::from_secs(30);
    for id in [&polite, &stubborn] {
        while srv
            .task_manager()
            .get(id.parse().unwrap())
            .unwrap()
            .status_msg
            != "test operation running"
        {
            assert!(Instant::now() < until, "operation {id} never started");
            thread::sleep(Duration::from_millis(5));
        }
    }
    let started = Instant::now();
    srv.shutdown();
    serving.join().unwrap();
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(6),
        "the shutdown waited {took:?} — not bounded by the grace + shutdown_wait"
    );
    let tm = srv.task_manager();
    let info = tm.get(polite.parse().unwrap()).unwrap();
    assert_eq!(info.state, TaskState::Cancelled, "{info:?}");
    assert!(info.cancel_requested);
    let info = tm.get(stubborn.parse().unwrap()).unwrap();
    assert_eq!(
        info.state,
        TaskState::Running,
        "still running past the bound: {info:?} after {took:?}"
    );
    assert!(info.cancel_requested);
    // No new asynchronous operation once shut down.
    let err = tm
        .submit(TaskSpec::default(), Box::new(|_| Json::obj()))
        .unwrap_err();
    assert!(err.contains("shutting down"), "{err}");
}

#[test]
fn embedders_can_submit_background_work() {
    let (srv, addr, _root) = start("embed");
    let id = srv
        .task_manager()
        .submit(
            TaskSpec {
                operation: "reindex".into(),
                db_name: "x".into(),
                user: "root".into(),
                ..TaskSpec::default()
            },
            Box::new(|h| {
                h.set_progress("halfway", 50);
                Json::obj()
                    .with("StatusCode", 0)
                    .with("StatusMsg", "reindexed")
                    .with("success_num", 100)
            }),
        )
        .unwrap();
    let j = wait_done(&addr, &id.to_string());
    assert_eq!((int(&j, "state"), msg(&j)), (1, "reindexed".to_string()));
    assert_eq!(int(&j, "success_num"), 100);
}

/// With the `tls` feature a callback may be an `https://` URL, verified
/// against `TaskConfig::callback_ca_file` (else the system bundle).
#[cfg(feature = "tls")]
#[test]
fn callbacks_are_delivered_over_https_with_the_tls_feature() {
    // A self-signed EC end-entity cert (CN=localhost, SAN DNS:localhost + IP
    // 127.0.0.1) and its key — the pair of the `server.rs` TLS tests.
    const CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBvTCCAWOgAwIBAgIUKHclqAxNOvVOQ3kWyfFT+rCJUHwwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDcwMjIwMjkyMFoXDTM2MDYyOTIw
MjkyMFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0D
AQcDQgAEYHEfrA2tZnM3sdpdua9aH0oSv8G2EmRrc10JlY7xgVO8+6VoyIJqtvH5
AxyPvu6UfMZMa+/UFLAmk+WCYEKCi6OBkjCBjzAdBgNVHQ4EFgQUJOZ8AYKC5S5i
QJhbdtMpHgmjlaowHwYDVR0jBBgwFoAUJOZ8AYKC5S5iQJhbdtMpHgmjlaowGgYD
VR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMAwGA1UdEwEB/wQCMAAwDgYDVR0PAQH/
BAQDAgeAMBMGA1UdJQQMMAoGCCsGAQUFBwMBMAoGCCqGSM49BAMCA0gAMEUCIQCo
ndoh3KosHMemvMMbjxtaNbbXmxGjfsgme3rBkLuziQIgFeUx/cU+Fpw/Dl5McuKP
cCWfronEM/ASghi4/9ldMUI=
-----END CERTIFICATE-----
";
    const KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgUBOdoa9/azwl851s
NPlNYkOXVnBaUKf3fk8J38AOaYehRANCAARgcR+sDa1mczex2l25r1ofShK/wbYS
ZGtzXQmVjvGBU7z7pWjIgmq28fkDHI++7pR8xkxr79QUsCaT5YJgQoKL
-----END PRIVATE KEY-----
";
    let dir = tmp_root("https_cb_certs");
    std::fs::create_dir_all(&dir).unwrap();
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    std::fs::write(&cert, CERT_PEM).unwrap();
    std::fs::write(&key, KEY_PEM).unwrap();
    let tls_config = gstore::server::load_tls_config(&cert, &key).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let conn = rustls::ServerConnection::new(tls_config).unwrap();
        let mut tls = rustls::StreamOwned::new(conn, sock);
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        while !req.ends_with(b"}") {
            match tls.read(&mut buf) {
                Ok(n) if n > 0 => req.extend_from_slice(&buf[..n]),
                _ => break,
            }
        }
        let _ = tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        tls.conn.send_close_notify();
        let _ = tls.flush();
        tx.send(String::from_utf8_lossy(&req).into_owned()).unwrap();
    });
    let (srv, addr, _root) = start_with(
        "https_cb",
        TaskConfig {
            callback_ca_file: Some(cert),
            // The hook listens on the loopback interface.
            callback_allow_local: true,
            ..TaskConfig::default()
        },
    );
    let j = api(
        &addr,
        &[
            ("operation", "__asyncTest"),
            ("async", "true"),
            ("callback", &format!("https://localhost:{port}/secure-hook")),
        ],
    );
    let opt_id = opt_id_of(&j);
    let req = rx.recv_timeout(Duration::from_secs(30)).unwrap();
    assert!(req.starts_with("POST /secure-hook HTTP/1.1\r\n"), "{req}");
    let body = req.split_once("\r\n\r\n").unwrap().1;
    let pushed = json::parse(body).unwrap();
    assert_eq!(
        pushed.get("opt_id").and_then(Json::as_str),
        Some(opt_id.as_str())
    );
    assert!(srv.task_manager().wait_async_idle(Duration::from_secs(30)));
    let info = srv.task_manager().get(opt_id.parse().unwrap()).unwrap();
    assert_eq!(
        info.callback_status.as_deref(),
        Some("delivered (HTTP 200)")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without the `tls` feature an `https://` callback is refused up front.
#[cfg(not(feature = "tls"))]
#[test]
fn https_callbacks_need_the_tls_feature() {
    let (_srv, addr, _root) = start("https_cb_off");
    let j = api(
        &addr,
        &[
            ("operation", "__asyncTest"),
            ("async", "true"),
            ("callback", "https://localhost:1/hook"),
        ],
    );
    assert_eq!(code(&j), 1003, "{j}");
    assert!(msg(&j).contains("tls"), "{j}");
}
