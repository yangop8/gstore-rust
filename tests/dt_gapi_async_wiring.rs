//! Data test: how the asynchronous operations of the upstream `/api`
//! protocol ([`gstore::gapi::tasks`]) meet the system store and the API logs
//! ([`gstore::sysdb`], `gapi::apilog`, `gapi::ipfilter`):
//!
//! * `checkOperationState` / `operationTaskCancel` / `operationTaskList` show
//!   a user their own tasks only while they hold, on the task's database, the
//!   per-database privilege its operation needs; `root` sees every task;
//! * an asynchronous submission is recorded in the access log under its
//!   `opt_id` (`state` 0) and its outcome replaces that record once it ends;
//! * the IP allow / deny lists and the failed-login lockout refuse an
//!   asynchronous submission before any task exists.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gstore::config::GStoreConfig;
use gstore::gapi::tasks::TaskState;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};

const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
<http://ex/a> <http://ex/p> <http://ex/c> .\n";

const SELECT: &str = "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }";

// ---- harness ------------------------------------------------------------------

fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "gstore_gapi_async_wiring_{tag}_{}_{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// A served `ApiServer` (root password `root`) in `root` with `config`.
fn start_with(root: &Path, config: GStoreConfig) -> (Arc<ApiServer>, SocketAddr) {
    let server = Arc::new(
        ApiServer::bind(root, "root", "127.0.0.1:0")
            .unwrap()
            .with_config(config),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    (server, addr)
}

fn start(tag: &str) -> (Arc<ApiServer>, SocketAddr, PathBuf) {
    let root = tmp_root(tag);
    let (srv, addr) = start_with(&root, GStoreConfig::default());
    (srv, addr, root)
}

/// One HTTP exchange: `(status, body)`.
fn send(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let text = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("header end");
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status");
    (status, body.to_string())
}

/// Operation `op` as `user` (password `pw`, or `root` for root).
fn as_user(addr: &SocketAddr, user: &str, op: &str, extra: &[(&str, &str)]) -> Json {
    let pw = if user == "root" { "root" } else { "pw" };
    let mut body = Json::obj()
        .with("username", user)
        .with("password", pw)
        .with("operation", op);
    for (k, v) in extra {
        body.set(*k, *v);
    }
    let (status, text) = send(
        addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        &body.to_string(),
    );
    assert_eq!(status, 200, "{text}");
    json::parse(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn root_api(addr: &SocketAddr, op: &str, extra: &[(&str, &str)]) -> Json {
    as_user(addr, "root", op, extra)
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .expect("StatusCode")
}

fn msg(j: &Json) -> &str {
    j.get("StatusMsg")
        .and_then(Json::as_str)
        .expect("StatusMsg")
}

fn list(j: &Json) -> &[Json] {
    j.get("list")
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("no list in {j}"))
}

fn opt_id_of(j: &Json) -> String {
    j.get("opt_id")
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no opt_id in {j}"))
        .to_string()
}

/// Build database `db` as root (REST, data in the body).
fn build(addr: &SocketAddr, db: &str) {
    let auth = "Basic cm9vdDpyb290"; // root:root
    let (status, body) = send(
        addr,
        "POST",
        &format!("/build?db_name={db}"),
        &[("Authorization", auth)],
        DATA,
    );
    assert_eq!(status, 200, "build {db}: {body}");
}

fn add_user(addr: &SocketAddr, name: &str) {
    let j = root_api(
        addr,
        "usermanage",
        &[("type", "1"), ("op_username", name), ("op_password", "pw")],
    );
    assert_eq!(code(&j), 0, "{j}");
}

/// Grant (`type` 1) or revoke (`type` 2) privilege codes on `db`.
fn privileges(addr: &SocketAddr, kind: &str, user: &str, db: &str, codes: &str) {
    let j = root_api(
        addr,
        "userprivilegemanage",
        &[
            ("type", kind),
            ("op_username", user),
            ("db_name", db),
            ("privileges", codes),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
}

/// Submit an asynchronous query of `db` as `user`; wait until it has ended.
fn async_query(srv: &ApiServer, addr: &SocketAddr, user: &str, db: &str) -> String {
    let j = as_user(
        addr,
        user,
        "query",
        &[("db_name", db), ("sparql", SELECT), ("async", "true")],
    );
    assert_eq!((code(&j), msg(&j)), (0, "Operation Success."), "{j}");
    let opt_id = opt_id_of(&j);
    let state = srv
        .task_manager()
        .wait_finished(opt_id.parse().unwrap(), Duration::from_secs(60));
    assert_eq!(state, Some(TaskState::Succeeded), "{opt_id}");
    opt_id
}

fn check_state(addr: &SocketAddr, user: &str, opt_id: &str) -> Json {
    as_user(addr, user, "checkOperationState", &[("opt_id", opt_id)])
}

/// The `opt_id`s `user` sees in `operationTaskList` (every status).
fn listed(addr: &SocketAddr, user: &str) -> Vec<String> {
    let j = as_user(
        addr,
        user,
        "operationTaskList",
        &[("status", "2"), ("pageSize", "1000")],
    );
    assert_eq!(code(&j), 0, "{j}");
    list(&j)
        .iter()
        .filter_map(|t| t.get("opt_id").and_then(Json::as_str).map(str::to_string))
        .collect()
}

fn not_found(j: &Json) {
    assert_eq!((code(j), msg(j)), (1005, "opt_id not found."), "{j}");
}

// ---- per-database task visibility -------------------------------------------

#[test]
fn async_tasks_are_visible_to_their_owner_while_the_database_privilege_holds() {
    let (srv, addr, _root) = start("visibility");
    build(&addr, "alpha");
    build(&addr, "beta");
    add_user(&addr, "alice");
    add_user(&addr, "bob");
    privileges(&addr, "1", "alice", "alpha", "1");
    privileges(&addr, "1", "bob", "alpha", "1");

    let opt_id = async_query(&srv, &addr, "alice", "alpha");
    let j = check_state(&addr, "alice", &opt_id);
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!(j.get("state").and_then(Json::as_i64), Some(1), "{j}");
    assert!(listed(&addr, "alice").contains(&opt_id));

    // Another user — even one holding query on the same database — does not
    // see it; root does.
    not_found(&check_state(&addr, "bob", &opt_id));
    assert!(!listed(&addr, "bob").contains(&opt_id));
    assert_eq!(code(&check_state(&addr, "root", &opt_id)), 0);
    assert!(listed(&addr, "root").contains(&opt_id));

    // A privilege on another database does not count.
    privileges(&addr, "2", "alice", "alpha", "1");
    privileges(&addr, "1", "alice", "beta", "1");
    not_found(&check_state(&addr, "alice", &opt_id));
    assert!(!listed(&addr, "alice").contains(&opt_id));
    let j = as_user(
        &addr,
        "alice",
        "operationTaskCancel",
        &[("opt_id", &opt_id)],
    );
    assert_eq!((code(&j), msg(&j)), (1005, "opt_id not found"), "{j}");
    // Root still sees it (and is told it already finished).
    assert_eq!(code(&check_state(&addr, "root", &opt_id)), 0);
    let j = root_api(&addr, "operationTaskCancel", &[("opt_id", &opt_id)]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1005, "task already finish, do not cancel"),
        "{j}"
    );

    // Granted again (update suffices for a statement), it is visible again.
    privileges(&addr, "1", "alice", "alpha", "4");
    let j = check_state(&addr, "alice", &opt_id);
    assert_eq!(j.get("state").and_then(Json::as_i64), Some(1), "{j}");
    assert!(listed(&addr, "alice").contains(&opt_id));

    // An admin-privileged user sees every task, as root.
    let (status, body) = send(
        &addr,
        "POST",
        "/user/create?op_username=carol&op_password=pw&privileges=admin",
        &[("Authorization", "Basic cm9vdDpyb290")],
        "",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(code(&check_state(&addr, "carol", &opt_id)), 0);
    assert!(listed(&addr, "carol").contains(&opt_id));
}

// ---- the access log ---------------------------------------------------------

/// The access-log records of today (a fresh server root: one day's file).
fn access_records(addr: &SocketAddr) -> Vec<Json> {
    let j = root_api(addr, "accesslogdate", &[]);
    let dates: Vec<String> = list(&j)
        .iter()
        .filter_map(Json::as_str)
        .map(str::to_string)
        .collect();
    let mut out = Vec::new();
    for date in dates {
        let j = root_api(
            addr,
            "accesslog",
            &[("date", &date), ("pageNo", "1"), ("pageSize", "1000")],
        );
        assert_eq!(code(&j), 0, "{j}");
        out.extend(list(&j).iter().cloned());
    }
    out
}

fn with_opt_id<'a>(records: &'a [Json], opt_id: &str) -> Vec<&'a Json> {
    records
        .iter()
        .filter(|r| r.get("opt_id").and_then(Json::as_str) == Some(opt_id))
        .collect()
}

#[test]
fn async_submissions_and_their_outcomes_are_in_the_access_log() {
    let (srv, addr, root) = start("accesslog");
    build(&addr, "alpha");
    let opt_id = async_query(&srv, &addr, "root", "alpha");

    // The final record is written on the runner once the task has ended;
    // wait for it to reach the file.
    let until = Instant::now() + Duration::from_secs(30);
    let rec = loop {
        let records = access_records(&addr);
        let mine = with_opt_id(&records, &opt_id);
        assert!(mine.len() <= 1, "one record per opt_id: {mine:?}");
        if let Some(r) = mine
            .first()
            .filter(|r| r.get("state").and_then(Json::as_i64) == Some(1))
        {
            break (*r).clone();
        }
        assert!(
            Instant::now() < until,
            "no final record for {opt_id}: {records:?}"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(rec.get("operation").and_then(Json::as_str), Some("query"));
    assert_eq!(rec.get("dbname").and_then(Json::as_str), Some("alpha"));
    assert_eq!(rec.get("code").and_then(Json::as_i64), Some(0));
    assert_eq!(rec.get("ip").and_then(Json::as_str), Some("127.0.0.1"));
    let file = rec
        .get("filepath")
        .and_then(Json::as_str)
        .unwrap_or_default();
    assert!(file.ends_with(&format!("{opt_id}.json")), "{rec}");

    // On disk: the submission (state 0) and the outcome (state 1).
    let dir = root.join("logs").join("ipaccess");
    let text: String = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect();
    let states: Vec<i64> = text
        .lines()
        .filter_map(|l| json::parse(l).ok())
        .filter(|r| r.get("opt_id").and_then(Json::as_str) == Some(opt_id.as_str()))
        .filter_map(|r| r.get("state").and_then(Json::as_i64))
        .collect();
    let mut sorted = states.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![0, 1], "{text}");

    // A refused submission (a malformed callback) is an ordinary record.
    let j = root_api(
        &addr,
        "query",
        &[
            ("db_name", "alpha"),
            ("sparql", SELECT),
            ("async", "true"),
            ("callback", "not a url"),
        ],
    );
    assert_eq!(code(&j), 1003, "{j}");
    let records = access_records(&addr);
    let refused = records
        .iter()
        .find(|r| r.get("code").and_then(Json::as_i64) == Some(1003))
        .unwrap_or_else(|| panic!("{records:?}"));
    assert!(refused.get("opt_id").is_none(), "{refused}");
    assert_eq!(refused.get("state").and_then(Json::as_i64), Some(1));
}

#[test]
fn a_failed_async_operation_is_logged_with_state_minus_one() {
    let (srv, addr, _root) = start("accesslog_fail");
    build(&addr, "alpha");
    let j = root_api(
        &addr,
        "query",
        &[
            ("db_name", "alpha"),
            ("sparql", "SELECT WHERE {"),
            ("async", "true"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    let opt_id = opt_id_of(&j);
    let state = srv
        .task_manager()
        .wait_finished(opt_id.parse().unwrap(), Duration::from_secs(60));
    assert_eq!(state, Some(TaskState::Failed));
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let records = access_records(&addr);
        let mine = with_opt_id(&records, &opt_id);
        if let Some(r) = mine
            .first()
            .filter(|r| r.get("state").and_then(Json::as_i64) == Some(-1))
        {
            assert_ne!(r.get("code").and_then(Json::as_i64), Some(0), "{r}");
            break;
        }
        assert!(
            Instant::now() < until,
            "no final record for {opt_id}: {records:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

// ---- IP checks come first ---------------------------------------------------

#[test]
fn blocked_addresses_cannot_submit_asynchronous_operations() {
    let root = tmp_root("ipdeny");
    let deny = root.join("ipDeny.config");
    std::fs::write(&deny, "127.0.0.1\n").unwrap();
    let mut cfg = GStoreConfig::default();
    cfg.ip_deny_path = Some(deny);
    let (srv, addr) = start_with(&root, cfg);
    let j = root_api(
        &addr,
        "query",
        &[("db_name", "alpha"), ("sparql", SELECT), ("async", "true")],
    );
    assert_eq!((code(&j), msg(&j)), (1101, "IP Blocked!"), "{j}");
    assert!(j.get("opt_id").is_none(), "{j}");
    assert!(srv.task_manager().list().is_empty(), "no task was created");
}

#[test]
fn a_locked_out_address_cannot_submit_asynchronous_operations() {
    let (srv, addr, _root) = start("lockout");
    build(&addr, "alpha");
    for _ in 0..5 {
        let j = as_user(&addr, "root", "login", &[("password", "wrong")]);
        assert_ne!(code(&j), 0, "{j}");
    }
    let before = srv.task_manager().list().len();
    let j = root_api(
        &addr,
        "query",
        &[("db_name", "alpha"), ("sparql", SELECT), ("async", "true")],
    );
    assert_eq!(code(&j), 1101, "{j}");
    assert!(j.get("opt_id").is_none(), "{j}");
    assert_eq!(
        srv.task_manager().list().len(),
        before,
        "no task was created"
    );
    // A successful login lifts the lockout; the submission then goes through.
    assert_eq!(code(&root_api(&addr, "login", &[])), 0);
    async_query(&srv, &addr, "root", "alpha");
}
