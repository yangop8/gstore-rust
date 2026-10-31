//! Data test: the system store behind the upstream `/api` protocol — users,
//! per-database privileges and the database registry ([`gstore::sysdb`]),
//! the user operations (`showuser`, `usermanage`, `userprivilegemanage`,
//! `userpassword`), IP allow / deny lists with the failed-login lockout and
//! `ipmanage`, and the API logs with their paging operations (`querylog`,
//! `accesslog`, `txnlog`, `…date`).
//!
//! Each test boots an `ApiServer` (root password `root`) on `127.0.0.1:0` and
//! speaks the upstream protocol (JSON bodies as `GstoreConnector.py` sends
//! them) and, where the same rules apply, the Rust-only REST endpoints.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::http_users::{Privilege, UserStore};
use gstore::json::{self, Json};
use gstore::query::hash::{hex_encode, pbkdf2_hmac_sha256};
use gstore::sysdb;

const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
<http://ex/a> <http://ex/p> <http://ex/c> .\n";

const ASK: &str = "ASK { <http://ex/a> <http://ex/p> <http://ex/b> }";
const INSERT: &str = "INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> }";

// ---- harness ------------------------------------------------------------------

fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "gstore_gapi_users_{tag}_{}_{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn serve(server: ApiServer) -> (Arc<ApiServer>, SocketAddr) {
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    (server, addr)
}

fn start(tag: &str) -> (Arc<ApiServer>, SocketAddr, PathBuf) {
    let root = tmp_root(tag);
    let (srv, addr) = serve(ApiServer::bind(&root, "root", "127.0.0.1:0").unwrap());
    (srv, addr, root)
}

/// A server whose `conf.ini` settings are `config` (bound in `root`).
fn start_with(root: &Path, config: GStoreConfig) -> (Arc<ApiServer>, SocketAddr) {
    serve(
        ApiServer::bind(root, "root", "127.0.0.1:0")
            .unwrap()
            .with_config(config),
    )
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
    body: &str,
) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
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
    Reply {
        status,
        body: body.to_string(),
    }
}

fn py_json(pairs: &[(&str, &str)]) -> String {
    let items: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}: {}", json::quote(k), json::quote(v)))
        .collect();
    format!("{{{}}}", items.join(", "))
}

/// `POST /api` with a JSON body; always HTTP 200.
fn api(addr: &SocketAddr, pairs: &[(&str, &str)]) -> Json {
    let r = send(
        addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        &py_json(pairs),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()
}

/// Operation `op` as `user` (password `pw`, or `root` for root).
fn as_user(addr: &SocketAddr, user: &str, op: &str, extra: &[(&str, &str)]) -> Json {
    let pw = if user == "root" { "root" } else { "pw" };
    let mut pairs = vec![("username", user), ("password", pw), ("operation", op)];
    pairs.extend_from_slice(extra);
    api(addr, &pairs)
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

fn s<'a>(j: &'a Json, key: &str) -> &'a str {
    j.get(key)
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no string {key} in {j}"))
}

fn n(j: &Json, key: &str) -> i64 {
    j.get(key)
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no number {key} in {j}"))
}

fn list(j: &Json) -> &[Json] {
    j.get("list")
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("no list in {j}"))
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

/// A REST request with Basic auth (`pw`, or `root` for root).
fn rest(addr: &SocketAddr, user: &str, method: &str, target: &str, body: &str) -> Reply {
    let pw = if user == "root" { "root" } else { "pw" };
    send(
        addr,
        method,
        target,
        &[("Authorization", &basic(user, pw))],
        body,
    )
}

fn build(addr: &SocketAddr, db: &str) {
    let r = rest(addr, "root", "POST", &format!("/build?db_name={db}"), DATA);
    assert_eq!(r.status, 200, "build {db}: {}", r.body);
}

fn add_user(addr: &SocketAddr, name: &str) {
    let j = root_api(
        addr,
        "usermanage",
        &[("type", "1"), ("op_username", name), ("op_password", "pw")],
    );
    assert_eq!((code(&j), msg(&j)), (0, "Add user done."), "{j}");
}

fn grant(addr: &SocketAddr, user: &str, db: &str, privileges: &str) {
    let j = root_api(
        addr,
        "userprivilegemanage",
        &[
            ("type", "1"),
            ("op_username", user),
            ("db_name", db),
            ("privileges", privileges),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
}

fn no_privilege(j: &Json, op: &str) {
    assert_eq!(
        (code(j), msg(j)),
        (1004, format!("You have no {op} privilege").as_str()),
        "{j}"
    );
}

// ---- per-database privileges ------------------------------------------------

#[test]
fn query_and_update_privileges_are_per_database() {
    let (_srv, addr, _root) = start("perdb");
    build(&addr, "alpha");
    build(&addr, "beta");
    add_user(&addr, "u");
    // A new user may log in and use the open operations, nothing else.
    assert_eq!(code(&as_user(&addr, "u", "login", &[])), 0);
    let j = as_user(
        &addr,
        "u",
        "query",
        &[("db_name", "alpha"), ("sparql", ASK)],
    );
    no_privilege(&j, "query");

    grant(&addr, "u", "alpha", "1");
    let j = as_user(
        &addr,
        "u",
        "query",
        &[("db_name", "alpha"), ("sparql", ASK)],
    );
    assert_eq!(code(&j), 0, "{j}");
    let j = as_user(&addr, "u", "query", &[("db_name", "beta"), ("sparql", ASK)]);
    no_privilege(&j, "query");
    // An update through `query` needs the update privilege on that database.
    let j = as_user(
        &addr,
        "u",
        "query",
        &[("db_name", "alpha"), ("sparql", INSERT)],
    );
    no_privilege(&j, "update");
    for op in ["begin", "checkpoint"] {
        no_privilege(&as_user(&addr, "u", op, &[("db_name", "alpha")]), op);
    }

    grant(&addr, "u", "alpha", "4");
    let j = as_user(
        &addr,
        "u",
        "query",
        &[("db_name", "alpha"), ("sparql", INSERT)],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "update query returns true."),
        "{j}"
    );
    let j = as_user(&addr, "u", "begin", &[("db_name", "alpha")]);
    assert_eq!(code(&j), 0, "{j}");
    let tid = s(&j, "TID").to_string();
    let j = as_user(
        &addr,
        "u",
        "rollback",
        &[("db_name", "alpha"), ("tid", &tid)],
    );
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!(
        code(&as_user(&addr, "u", "checkpoint", &[("db_name", "alpha")])),
        0
    );
    no_privilege(
        &as_user(&addr, "u", "begin", &[("db_name", "beta")]),
        "begin",
    );
    // Nobody — root included — may target the system database.
    let j = root_api(&addr, "query", &[("db_name", "system"), ("sparql", ASK)]);
    assert_eq!(code(&j), 1003, "{j}");

    // type 2 revokes on that database; type 3 clears everything.
    let j = root_api(
        &addr,
        "userprivilegemanage",
        &[
            ("type", "2"),
            ("op_username", "u"),
            ("db_name", "alpha"),
            ("privileges", "4"),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "delete privilege update successfully."),
        "{j}"
    );
    let j = as_user(
        &addr,
        "u",
        "query",
        &[("db_name", "alpha"), ("sparql", INSERT)],
    );
    no_privilege(&j, "update");
    assert_eq!(
        code(&as_user(
            &addr,
            "u",
            "query",
            &[("db_name", "alpha"), ("sparql", ASK)]
        )),
        0
    );
    let j = root_api(
        &addr,
        "userprivilegemanage",
        &[("type", "3"), ("op_username", "u")],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "Clear the all privileges for the user successfully!")
    );
    let j = as_user(
        &addr,
        "u",
        "query",
        &[("db_name", "alpha"), ("sparql", ASK)],
    );
    no_privilege(&j, "query");
}

/// The REST endpoints decide with the same per-database rules, for every
/// privilege class (query, update, load, unload, backup, restore, export)
/// and the root-only `build` / `drop`.
#[test]
fn rest_endpoints_check_every_privilege_class_per_database() {
    let (_srv, addr, _root) = start("rest");
    build(&addr, "alpha");
    build(&addr, "beta");
    add_user(&addr, "u");
    let forbidden = |method: &str, target: &str, body: &str| {
        let r = rest(&addr, "u", method, target, body);
        assert_eq!(r.status, 403, "{method} {target}: {}", r.body);
    };
    let ok = |method: &str, target: &str, body: &str| {
        let r = rest(&addr, "u", method, target, body);
        assert_eq!(r.status, 200, "{method} {target}: {}", r.body);
        r
    };
    // query (1)
    forbidden("POST", "/sparql?db_name=alpha", ASK);
    grant(&addr, "u", "alpha", "1");
    assert!(ok("POST", "/sparql?db_name=alpha", ASK)
        .body
        .contains("true"));
    forbidden("POST", "/sparql?db_name=beta", ASK);
    // update (4): /update, /sparql updates, batch, transactions
    forbidden("POST", "/update?db_name=alpha", INSERT);
    forbidden("POST", "/sparql?db_name=alpha", INSERT);
    forbidden("POST", "/batchInsert?db_name=alpha", DATA);
    forbidden("POST", "/begin?db_name=alpha", "");
    grant(&addr, "u", "alpha", "4");
    ok("POST", "/update?db_name=alpha", INSERT);
    ok("POST", "/batchInsert?db_name=alpha", DATA);
    let begin = ok("POST", "/begin?db_name=alpha", "").json();
    let tid = s(&begin, "TID").to_string();
    ok("POST", &format!("/rollback?tid={tid}"), "");
    forbidden("POST", "/update?db_name=beta", INSERT);
    forbidden("POST", "/checkpoint?db_name=beta", "");
    // export (7)
    forbidden("GET", "/export?db_name=alpha", "");
    grant(&addr, "u", "alpha", "7");
    assert!(ok("GET", "/export?db_name=alpha", "")
        .body
        .contains("<http://ex/a>"));
    // backup (5), restore (6)
    forbidden("POST", "/backup?db_name=alpha", "");
    grant(&addr, "u", "alpha", "5");
    ok("POST", "/backup?db_name=alpha", "");
    forbidden("POST", "/restore?db_name=alpha", "");
    grant(&addr, "u", "alpha", "6");
    let r = rest(&addr, "u", "POST", "/restore?db_name=alpha", "");
    assert_ne!(r.status, 403, "{}", r.body);
    // unload (3), load (2)
    forbidden("POST", "/unload?db_name=alpha", "");
    grant(&addr, "u", "alpha", "3");
    let r = rest(&addr, "u", "POST", "/unload?db_name=alpha", "");
    assert_ne!(r.status, 403, "{}", r.body);
    forbidden("POST", "/load?db_name=alpha", "");
    grant(&addr, "u", "alpha", "2");
    ok("POST", "/load?db_name=alpha", "");
    forbidden("POST", "/load?db_name=beta", "");
    // build and drop: root only, even with every privilege on the database.
    forbidden("POST", "/build?db_name=gamma", DATA);
    forbidden("POST", "/drop?db_name=alpha", "");
    // Creating a database by uploading into it is a build; uploading into
    // one the user may update is not.
    forbidden("POST", "/upload?db_name=gamma&format=ntriples", DATA);
    ok("POST", "/upload?db_name=alpha&format=ntriples", DATA);
    // Root-only REST endpoints.
    for target in [
        "/user/show",
        "/querylog",
        "/accesslog",
        "/txnlog",
        "/tasklist",
    ] {
        forbidden("GET", target, "");
    }
    // A legacy admin user is a superuser: it may drop.
    let r = rest(
        &addr,
        "root",
        "POST",
        "/user/create?op_username=ada&op_password=pw&privileges=admin",
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let r = rest(&addr, "ada", "POST", "/drop?db_name=beta", "");
    assert_eq!(r.status, 200, "{}", r.body);
}

#[test]
fn rest_user_endpoints_grant_per_database_or_everywhere() {
    let (_srv, addr, _root) = start("restgrant");
    build(&addr, "alpha");
    build(&addr, "beta");
    // Without db_name: every database (the earlier global model).
    let r = rest(
        &addr,
        "root",
        "POST",
        "/user/create?op_username=g&op_password=pw&privileges=query",
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    for db in ["alpha", "beta"] {
        let r = rest(&addr, "g", "POST", &format!("/sparql?db_name={db}"), ASK);
        assert_eq!(r.status, 200, "{db}: {}", r.body);
    }
    // With db_name: that database only.
    let r = rest(
        &addr,
        "root",
        "POST",
        "/user/create?op_username=d&op_password=pw&privileges=query&db_name=alpha",
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        rest(&addr, "d", "POST", "/sparql?db_name=alpha", ASK).status,
        200
    );
    assert_eq!(
        rest(&addr, "d", "POST", "/sparql?db_name=beta", ASK).status,
        403
    );
    let r = rest(
        &addr,
        "root",
        "POST",
        "/user/grant?op_username=d&privileges=1&db_name=beta",
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        rest(&addr, "d", "POST", "/sparql?db_name=beta", ASK).status,
        200
    );
    let r = rest(
        &addr,
        "root",
        "POST",
        "/user/revoke?op_username=d&privileges=query&db_name=alpha",
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        rest(&addr, "d", "POST", "/sparql?db_name=alpha", ASK).status,
        403
    );
    let show = rest(&addr, "root", "GET", "/user/show", "").body;
    assert!(
        show.contains(r#""db_privileges":{"beta":"query"}"#),
        "{show}"
    );
    let r = rest(
        &addr,
        "root",
        "POST",
        "/user/grant?op_username=d&privileges=1&db_name=system",
        "",
    );
    assert_eq!(r.status, 400, "{}", r.body);
}

// ---- root-only rules and the user operations -----------------------------------

#[test]
fn user_operations_follow_upstream() {
    let (_srv, addr, _root) = start("userops");
    build(&addr, "alpha");
    add_user(&addr, "u");
    add_user(&addr, "v");
    // Root-only operations.
    for op in [
        "showuser",
        "usermanage",
        "userprivilegemanage",
        "ipmanage",
        "querylog",
        "querylogdate",
        "accesslog",
        "accesslogdate",
        "txnlog",
        "stat",
    ] {
        no_privilege(&as_user(&addr, "u", op, &[("type", "1")]), op);
    }

    // usermanage: duplicates, unknown users, root, bad input.
    let j = root_api(
        &addr,
        "usermanage",
        &[("type", "1"), ("op_username", "u"), ("op_password", "x")],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (1005, "Username already existed, add user failed.")
    );
    let j = root_api(
        &addr,
        "usermanage",
        &[("type", "2"), ("op_username", "root")],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (1005, "You cannot delete root, delete user failed.")
    );
    let j = root_api(
        &addr,
        "usermanage",
        &[("type", "2"), ("op_username", "ghost")],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (1005, "Username not exist, delete user failed.")
    );
    let j = root_api(&addr, "usermanage", &[("type", "1"), ("op_username", "w")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1003, "the value of op_password can not be empty!")
    );
    let j = root_api(
        &addr,
        "usermanage",
        &[("type", "9"), ("op_username", "w"), ("op_password", "x")],
    );
    assert_eq!((code(&j), msg(&j)), (1003, "The operation is not support."));
    let j = root_api(&addr, "usermanage", &[("type", "1")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1003, "the value of op_username can not be empty!")
    );
    // type 3: root changes v's password.
    let j = root_api(
        &addr,
        "usermanage",
        &[("type", "3"), ("op_username", "v"), ("op_password", "new")],
    );
    assert_eq!((code(&j), msg(&j)), (0, "Change password done."));
    assert_eq!(code(&as_user(&addr, "v", "login", &[])), 1001);
    let j = api(
        &addr,
        &[
            ("username", "v"),
            ("password", "new"),
            ("operation", "login"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    // type 2 deletes.
    let j = root_api(&addr, "usermanage", &[("type", "2"), ("op_username", "v")]);
    assert_eq!((code(&j), msg(&j)), (0, "Delete user done."));
    let j = api(
        &addr,
        &[
            ("username", "v"),
            ("password", "new"),
            ("operation", "login"),
        ],
    );
    assert_eq!(code(&j), 1001);

    // userprivilegemanage: input checks.
    let upm = |extra: &[(&str, &str)]| root_api(&addr, "userprivilegemanage", extra);
    let j = upm(&[("type", "7"), ("op_username", "u")]);
    assert_eq!((code(&j), msg(&j)), (1003, "The type 7 is not support."));
    let j = upm(&[("type", "1"), ("op_username", "ghost")]);
    assert_eq!((code(&j), msg(&j)), (1004, "The username is not exists."));
    let j = upm(&[
        ("type", "1"),
        ("op_username", "root"),
        ("db_name", "alpha"),
        ("privileges", "1"),
    ]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1004, "You can't change privileges for root user.")
    );
    let j = upm(&[
        ("type", "1"),
        ("op_username", "u"),
        ("db_name", "nosuch"),
        ("privileges", "1"),
    ]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1004, "Database[nosuch] does not exist.")
    );
    let j = upm(&[("type", "1"), ("op_username", "u"), ("db_name", "alpha")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1003, "the value of privileges can not be empty!")
    );
    let j = upm(&[
        ("type", "1"),
        ("op_username", "u"),
        ("db_name", "alpha"),
        ("privileges", "0,9"),
    ]);
    assert_eq!(
        (code(&j), msg(&j)),
        (
            1003,
            "not match any valid privilege, valid values between 1 and 7."
        )
    );
    let j = upm(&[
        ("type", "1"),
        ("op_username", "u"),
        ("db_name", "alpha"),
        ("privileges", "1,2,9"),
    ]);
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "add privilege query,load successfully.")
    );

    // showuser: upstream's fields, the password masked, lists per privilege.
    let j = root_api(&addr, "showuser", &[]);
    assert_eq!((code(&j), msg(&j)), (0, "success"));
    let users = j.get("ResponseBody").and_then(Json::as_array).unwrap();
    let find = |name: &str| {
        users
            .iter()
            .find(|u| u.get("username").and_then(Json::as_str) == Some(name))
            .unwrap_or_else(|| panic!("{name} in {j}"))
    };
    let u = find("u");
    assert_eq!(s(u, "query_privilege"), "alpha,");
    assert_eq!(s(u, "load_privilege"), "alpha,");
    assert_eq!(s(u, "update_privilege"), "");
    assert_eq!(s(u, "password"), "******");
    let root = find("root");
    for field in [
        "query_privilege",
        "update_privilege",
        "load_privilege",
        "unload_privilege",
        "backup_privilege",
        "restore_privilege",
        "export_privilege",
    ] {
        assert_eq!(s(root, field), "all", "{field}");
    }
    assert!(!j.to_string().contains("pbkdf2"), "no hashes leak: {j}");

    // userpassword: a user changes their own password (only).
    let j = as_user(
        &addr,
        "u",
        "userpassword",
        &[("op_username", "root"), ("op_password", "pw2")],
    );
    assert_eq!((code(&j), msg(&j)), (0, "Change password done."));
    assert_eq!(code(&root_api(&addr, "login", &[])), 0, "root unaffected");
    let j = api(
        &addr,
        &[
            ("username", "u"),
            ("password", "pw2"),
            ("operation", "login"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    let j = api(
        &addr,
        &[
            ("username", "u"),
            ("password", "pw2"),
            ("operation", "userpassword"),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (1003, "the value of op_password can not be empty!")
    );
}

#[test]
fn max_user_num_caps_usermanage() {
    let root = tmp_root("maxusers");
    let mut cfg = GStoreConfig::default();
    cfg.max_user_num = 2;
    cfg.ip_deny_path = None;
    let (_srv, addr) = start_with(&root, cfg);
    add_user(&addr, "one");
    let j = root_api(
        &addr,
        "usermanage",
        &[("type", "1"), ("op_username", "two"), ("op_password", "pw")],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (1004, "The total number of users more than max_user_num.")
    );
}

// ---- migration ----------------------------------------------------------------------

/// A `users.db` written by an earlier version (global privileges, the raw
/// PBKDF2 format) plus a database directory, and no `system.json`: the first
/// start keeps a copy of the old user table, seeds the registry from disk,
/// and the old global privileges keep applying to every database.
#[test]
fn a_users_db_fixture_is_migrated_on_first_start() {
    let root = tmp_root("migrate");
    {
        let srv = Arc::new(ApiServer::bind(&root, "root", "127.0.0.1:0").unwrap());
        let addr = srv.local_addr().unwrap();
        let s2 = Arc::clone(&srv);
        let serving = thread::spawn(move || s2.serve_forever());
        build(&addr, "legacy");
        srv.shutdown();
        serving.join().unwrap();
    }
    let raw_entry = |name: &str, pw: &str, privs: &str| {
        let salt = b"0123456789abcdef";
        let mut hash = [0u8; 32];
        pbkdf2_hmac_sha256(pw.as_bytes(), salt, 100_000, &mut hash);
        format!(
            "{name}\tpbkdf2-sha256$100000${}${}\t{privs}\n",
            hex_encode(salt),
            hex_encode(&hash)
        )
    };
    let fixture = format!(
        "{}{}{}",
        raw_entry("alice", "alicepw", "query,update"),
        raw_entry("ada", "adapw", "admin"),
        raw_entry("root", "oldroot", "admin"),
    );
    std::fs::write(root.join("users.db"), &fixture).unwrap();
    std::fs::remove_file(root.join(sysdb::SYSTEM_FILE)).unwrap();

    let (srv, addr) = serve(ApiServer::bind(&root, "ignored", "127.0.0.1:0").unwrap());
    assert_eq!(
        std::fs::read_to_string(root.join(sysdb::USERS_BACKUP_FILE)).unwrap(),
        fixture,
        "the pre-migration user table is kept"
    );
    let r = send(
        &addr,
        "POST",
        "/load?db_name=legacy",
        &[("Authorization", &basic("root", "oldroot"))],
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let registry = std::fs::read_to_string(root.join(sysdb::SYSTEM_FILE)).unwrap();
    assert!(
        registry.contains(r#""creator":"root","database":"legacy""#)
            || registry.contains(r#""database":"legacy","creator":"root""#),
        "{registry}"
    );
    // The old passwords work, root's included; global privileges apply to
    // the database (and to every later one).
    let alice = |op: &str, extra: &[(&str, &str)]| {
        let mut pairs = vec![
            ("username", "alice"),
            ("password", "alicepw"),
            ("operation", op),
        ];
        pairs.extend_from_slice(extra);
        api(&addr, &pairs)
    };
    assert_eq!(code(&alice("login", &[])), 0);
    assert_eq!(
        code(&alice("query", &[("db_name", "legacy"), ("sparql", ASK)])),
        0
    );
    assert_eq!(
        code(&alice(
            "query",
            &[("db_name", "legacy"), ("sparql", INSERT)]
        )),
        0
    );
    no_privilege(&alice("showuser", &[]), "showuser");
    let j = api(
        &addr,
        &[
            ("username", "root"),
            ("password", "oldroot"),
            ("operation", "showuser"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    let body = j.to_string();
    assert!(body.contains(r#""query_privilege":"*,""#), "{body}");
    // The legacy admin is a superuser.
    let j = api(
        &addr,
        &[
            ("username", "ada"),
            ("password", "adapw"),
            ("operation", "showuser"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    // The plaintext login above migrated alice's hash to the current format.
    let text = std::fs::read_to_string(root.join("users.db")).unwrap();
    assert!(text.contains("alice\tpbkdf2-sha256-md5$"), "{text}");
    // A per-database grant survives a restart.
    let j = api(
        &addr,
        &[
            ("username", "root"),
            ("password", "oldroot"),
            ("operation", "usermanage"),
            ("type", "1"),
            ("op_username", "bob"),
            ("op_password", "pw"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    let j = api(
        &addr,
        &[
            ("username", "root"),
            ("password", "oldroot"),
            ("operation", "userprivilegemanage"),
            ("type", "1"),
            ("op_username", "bob"),
            ("db_name", "legacy"),
            ("privileges", "1"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    srv.shutdown();
    let users = UserStore::open(root.join("users.db"), "x").unwrap();
    assert!(users.has_priv_on("bob", Privilege::Query, "legacy"));
    assert!(!users.has_priv_on("bob", Privilege::Query, "other"));
    assert!(sysdb::allowed(&users, "bob", "query", Some("legacy")));
    assert!(!sysdb::allowed(&users, "bob", "update", Some("legacy")));
    // The second start is not a first start: the backup is not rewritten.
    std::fs::write(root.join(sysdb::USERS_BACKUP_FILE), "marker").unwrap();
    let (_users, sys) = sysdb::open_system(&root, "x").unwrap();
    assert!(!sys.first_start());
    assert_eq!(sys.names(), vec!["legacy".to_string()]);
    assert_eq!(
        std::fs::read_to_string(root.join(sysdb::USERS_BACKUP_FILE)).unwrap(),
        "marker"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn dropping_a_database_drops_its_grants_and_registration() {
    let (_srv, addr, root) = start("dropgrants");
    build(&addr, "alpha");
    add_user(&addr, "u");
    grant(&addr, "u", "alpha", "1,4");
    let registry = std::fs::read_to_string(root.join(sysdb::SYSTEM_FILE)).unwrap();
    assert!(registry.contains("\"alpha\""), "{registry}");
    let r = rest(&addr, "root", "POST", "/drop?db_name=alpha", "");
    assert_eq!(r.status, 200, "{}", r.body);
    let registry = std::fs::read_to_string(root.join(sysdb::SYSTEM_FILE)).unwrap();
    assert!(!registry.contains("\"alpha\""), "{registry}");
    // A new database of the same name does not inherit the old grants.
    build(&addr, "alpha");
    let j = as_user(
        &addr,
        "u",
        "query",
        &[("db_name", "alpha"), ("sparql", ASK)],
    );
    no_privilege(&j, "query");
}

// ---- IP lists, lockout, ipmanage --------------------------------------------------

#[test]
fn deny_and_allow_lists_block_with_1101() {
    let root = tmp_root("iplists");
    std::fs::create_dir_all(&root).unwrap();
    let deny = root.join("ipDeny.config");
    std::fs::write(&deny, "#comment\n127.0.0.1\n").unwrap();
    let mut cfg = GStoreConfig::default();
    cfg.ip_deny_path = Some(deny);
    let (_srv, addr) = start_with(&root, cfg);
    for op in ["check", "login", "testConnect", "query"] {
        let j = root_api(&addr, op, &[]);
        assert_eq!((code(&j), msg(&j)), (1101, "IP Blocked!"), "{op}: {j}");
    }

    // An allow list wins over the deny list; only listed addresses get in.
    let root2 = tmp_root("iplists_allow");
    std::fs::create_dir_all(&root2).unwrap();
    let allow = root2.join("ipAllow.config");
    std::fs::write(&allow, "127.0.0.2\n").unwrap();
    let mut cfg = GStoreConfig::default();
    cfg.ip_allow_path = Some(allow.clone());
    cfg.ip_deny_path = None;
    let (_srv2, addr2) = start_with(&root2, cfg.clone());
    assert_eq!(code(&root_api(&addr2, "check", &[])), 1101);
    std::fs::write(&allow, "127.0.0.0-127.0.0.9\n").unwrap();
    let (_srv3, addr3) = start_with(&root2, cfg);
    assert_eq!(code(&root_api(&addr3, "login", &[])), 0);
    // The lists hold on the REST endpoints of the same listener too.
    let r = rest(&addr, "root", "GET", "/show", "");
    assert_eq!(r.status, 403, "{}", r.body);
}

#[test]
fn five_failed_logins_lock_the_address_until_a_successful_login() {
    let (_srv, addr, _root) = start("lockout");
    let bad = || {
        api(
            &addr,
            &[
                ("username", "root"),
                ("password", "wrong"),
                ("operation", "login"),
            ],
        )
    };
    for _ in 0..4 {
        assert_eq!(code(&bad()), 1001);
    }
    assert_eq!(
        code(&root_api(&addr, "getCoreVersion", &[])),
        0,
        "4 failures: not yet"
    );
    assert_eq!(code(&bad()), 1001);
    // More than four failures: every operation but login / testConnect is
    // refused, even with the right credentials.
    let j = root_api(&addr, "getCoreVersion", &[]);
    assert_eq!(code(&j), 1101, "{j}");
    assert!(msg(&j).contains("too many error"), "{j}");
    assert_eq!(code(&root_api(&addr, "showuser", &[])), 1101);
    assert_eq!(code(&root_api(&addr, "testConnect", &[])), 0, "exempt");
    // A wrong password still fails (and still counts).
    assert_eq!(code(&bad()), 1001);
    // A successful login resets the count.
    assert_eq!(code(&root_api(&addr, "login", &[])), 0);
    assert_eq!(code(&root_api(&addr, "getCoreVersion", &[])), 0);
}

#[test]
fn ipmanage_round_trip() {
    let root = tmp_root("ipmanage");
    std::fs::create_dir_all(&root).unwrap();
    let deny = root.join("conf").join("ipDeny.config");
    let mut cfg = GStoreConfig::default();
    cfg.ip_deny_path = Some(deny.clone());
    let (_srv, addr) = start_with(&root, cfg);
    // Query: the deny list (a missing file is an empty list).
    let j = root_api(&addr, "ipmanage", &[("type", "1")]);
    assert_eq!(code(&j), 0, "{j}");
    let body = j.get("ResponseBody").unwrap();
    assert_eq!(s(body, "ip_type"), "1");
    assert_eq!(body.get("ips").and_then(Json::as_array).unwrap().len(), 0);
    // Save, then query again.
    let j = root_api(
        &addr,
        "ipmanage",
        &[
            ("type", "2"),
            ("ip_type", "1"),
            ("ips", "192.168.1.111,192.168.1.120-192.168.1.129"),
        ],
    );
    assert_eq!((code(&j), msg(&j)), (0, "success"), "{j}");
    let j = root_api(&addr, "ipmanage", &[("type", "1")]);
    let ips: Vec<&str> = j
        .get("ResponseBody")
        .and_then(|b| b.get("ips"))
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .filter_map(Json::as_str)
        .collect();
    assert_eq!(ips, vec!["192.168.1.111", "192.168.1.120-192.168.1.129"]);
    let file = std::fs::read_to_string(&deny).unwrap();
    assert!(file.contains("\n192.168.1.111\n"), "{file}");
    // Errors.
    let j = root_api(
        &addr,
        "ipmanage",
        &[("type", "2"), ("ip_type", "2"), ("ips", "1.2.3.4")],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (
            1005,
            "ip_allow_path is not configured, please configure it in the conf.ini file first."
        )
    );
    let j = root_api(
        &addr,
        "ipmanage",
        &[("type", "2"), ("ip_type", "1"), ("ips", "")],
    );
    assert_eq!((code(&j), msg(&j)), (1003, "the ips can't be empty"));
    let j = root_api(
        &addr,
        "ipmanage",
        &[("type", "2"), ("ip_type", "3"), ("ips", "1.2.3.4")],
    );
    assert_eq!(code(&j), 1003);
    let j = root_api(
        &addr,
        "ipmanage",
        &[("type", "2"), ("ip_type", "1"), ("ips", "not-an-ip")],
    );
    assert_eq!(code(&j), 1003, "{j}");
    let j = root_api(&addr, "ipmanage", &[("type", "5")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1003, "type is invalid, please look up the api document.")
    );
    // Denying our own address takes effect at once.
    let j = root_api(
        &addr,
        "ipmanage",
        &[("type", "2"), ("ip_type", "1"), ("ips", "127.0.0.1")],
    );
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!(code(&root_api(&addr, "check", &[])), 1101);

    // Neither list configured.
    let root2 = tmp_root("ipmanage_none");
    let mut cfg = GStoreConfig::default();
    cfg.ip_deny_path = None;
    let (_srv2, addr2) = start_with(&root2, cfg);
    let j = root_api(&addr2, "ipmanage", &[("type", "1")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (
            1005,
            "please configure ip_deny_path or ip_allow_path in the conf.ini file first."
        )
    );
}

// ---- logs ---------------------------------------------------------------------------

#[test]
fn query_access_and_transaction_logs_are_written_and_paged() {
    let (_srv, addr, root) = start("logs");
    build(&addr, "alpha");
    for i in 0..3 {
        let q = format!(
            "SELECT ?o WHERE {{ <http://ex/a> <http://ex/p> ?o }} LIMIT {}",
            i + 1
        );
        let j = root_api(&addr, "query", &[("db_name", "alpha"), ("sparql", &q)]);
        assert_eq!(code(&j), 0, "{j}");
    }
    let j = root_api(&addr, "query", &[("db_name", "alpha"), ("sparql", INSERT)]);
    assert_eq!(code(&j), 0, "{j}");

    // querylogdate / querylog.
    let j = root_api(&addr, "querylogdate", &[]);
    assert_eq!((code(&j), msg(&j)), (0, "Get query log date success"));
    let dates: Vec<&str> = list(&j).iter().filter_map(Json::as_str).collect();
    assert_eq!(dates.len(), 1, "{j}");
    let today = dates[0].to_string();
    assert_eq!(today.len(), 8);
    let file = root
        .join("logs")
        .join("endpoint")
        .join(format!("{today}.log"));
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.lines().count(), 4, "{text}");
    let j = root_api(
        &addr,
        "querylog",
        &[("date", &today), ("pageNo", "1"), ("pageSize", "3")],
    );
    assert_eq!((code(&j), msg(&j)), (0, "Get query log success."), "{j}");
    assert_eq!((n(&j, "totalSize"), n(&j, "totalPage")), (4, 2));
    assert_eq!((n(&j, "pageNo"), n(&j, "pageSize")), (1, 3));
    let first = &list(&j)[0];
    assert_eq!(s(first, "sparql"), INSERT, "newest first: {j}");
    assert_eq!(s(first, "Sparql"), INSERT, "the docs' name too");
    assert_eq!(s(first, "statusMsg"), "update query returns true.");
    assert_eq!(s(first, "dbname"), "alpha");
    assert_eq!(s(first, "remoteIP"), "127.0.0.1");
    assert_eq!(s(first, "format"), "json");
    assert_eq!(n(first, "ansNum"), 1);
    assert_eq!(
        s(first, "queryDateTime").len(),
        23,
        "YYYY-MM-DD HH:MM:SS.mmm"
    );
    let read = &list(&j)[1];
    assert_eq!(n(read, "AnsNum"), 2, "LIMIT 3 over 2 triples: {read}");
    let j = root_api(
        &addr,
        "querylog",
        &[("date", &today), ("pageNo", "2"), ("pageSize", "3")],
    );
    assert_eq!(list(&j).len(), 1);
    let j = root_api(
        &addr,
        "querylog",
        &[("date", &today), ("pageNo", "3"), ("pageSize", "3")],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (1005, "Get query log fail: more then max page number 2")
    );
    let j = root_api(&addr, "querylog", &[("date", "../../etc")]);
    assert_eq!(code(&j), 1003, "{j}");
    let j = root_api(&addr, "querylog", &[]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1003, "the value of date can not be empty!")
    );
    let j = root_api(&addr, "querylog", &[("date", "19990101")]);
    assert_eq!((code(&j), n(&j, "totalSize"), list(&j).len()), (0, 0, 0));
    // The REST query log still records /api queries.
    let r = rest(&addr, "root", "GET", "/querylog", "");
    assert!(r.body.contains("<http://ex/x>"), "{}", r.body);

    // Transactions.
    for commit in [true, false] {
        let j = root_api(&addr, "begin", &[("db_name", "alpha")]);
        let tid = s(&j, "TID").to_string();
        let end = if commit { "commit" } else { "rollback" };
        let j = root_api(&addr, end, &[("db_name", "alpha"), ("tid", &tid)]);
        assert_eq!(code(&j), 0, "{j}");
    }
    let j = root_api(&addr, "begin", &[("db_name", "alpha")]);
    let open_tid = s(&j, "TID").to_string();
    let j = root_api(&addr, "txnlog", &[("pageNo", "1"), ("pageSize", "10")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "Get transaction log success."),
        "{j}"
    );
    assert_eq!(n(&j, "totalSize"), 3, "{j}");
    let states: Vec<&str> = list(&j).iter().map(|r| s(r, "state")).collect();
    assert_eq!(states, vec!["RUNNING", "ROLLBACK", "COMMITED"], "{j}");
    let newest = &list(&j)[0];
    assert!(
        s(newest, "TID").ends_with(&format!("_{open_tid}")),
        "{newest}"
    );
    assert_eq!(s(newest, "end_time"), "INF");
    assert_eq!((s(newest, "user"), s(newest, "db_name")), ("root", "alpha"));
    let done = &list(&j)[2];
    assert!(
        s(done, "end_time").parse::<u128>().unwrap()
            >= s(done, "begin_time").parse::<u128>().unwrap()
    );
    assert!(root.join("logs").join("transaction.json").is_file());
    let j = root_api(
        &addr,
        "rollback",
        &[("db_name", "alpha"), ("tid", &open_tid)],
    );
    assert_eq!(code(&j), 0, "{j}");

    // accesslogdate / accesslog.
    let j = root_api(&addr, "accesslogdate", &[]);
    assert_eq!((code(&j), msg(&j)), (0, "Get access log date success"));
    assert_eq!(list(&j).len(), 1, "{j}");
    let j = root_api(
        &addr,
        "accesslog",
        &[
            ("date", &today),
            ("pageSize", "100"),
            ("db_operation", "query"),
        ],
    );
    assert_eq!((code(&j), msg(&j)), (0, "Get access log success."), "{j}");
    assert_eq!(n(&j, "totalSize"), 4, "{j}");
    let rec = &list(&j)[0];
    assert_eq!(s(rec, "ip"), "127.0.0.1");
    assert_eq!(s(rec, "operation"), "query");
    assert_eq!(n(rec, "code"), 0);
    assert_eq!(s(rec, "dbname"), "alpha");
    assert_eq!(n(rec, "state"), 1);
    assert_eq!(s(rec, "createtime").len(), 19, "YYYY-MM-DD HH:MM:SS");
    // Failures are logged too (a wrong password), and unknown operations not.
    let j = api(
        &addr,
        &[
            ("username", "root"),
            ("password", "bad"),
            ("operation", "showuser"),
        ],
    );
    assert_eq!(code(&j), 1001);
    assert_eq!(code(&root_api(&addr, "nosuchop", &[])), 1100);
    let j = root_api(
        &addr,
        "accesslog",
        &[("date", &today), ("db_operation", "showuser")],
    );
    let rec = &list(&j)[0];
    assert_eq!(n(rec, "code"), 1001, "{j}");
    let j = root_api(
        &addr,
        "accesslog",
        &[("date", &today), ("db_operation", "nosuchop")],
    );
    assert_eq!(n(&j, "totalSize"), 0, "{j}");
    let j = root_api(
        &addr,
        "accesslog",
        &[("date", &today), ("db_name", "alpha"), ("pageSize", "1000")],
    );
    assert!(list(&j).iter().all(|r| s(r, "dbname") == "alpha"), "{j}");
    let j = root_api(&addr, "accesslog", &[("date", &today), ("pageNo", "x")]);
    assert_eq!(code(&j), 1003, "{j}");
    let j = root_api(&addr, "accesslog", &[("date", &today), ("pageNo", "999")]);
    assert_eq!(code(&j), 1005, "{j}");
    assert!(
        msg(&j).starts_with("Get access log fail: more then max page number"),
        "{j}"
    );
}

#[test]
fn log_switches_and_paths_come_from_the_configuration() {
    let root = tmp_root("logcfg");
    let mut cfg = GStoreConfig::default();
    cfg.querylog_mode = false;
    cfg.accesslog_path = PathBuf::from("custom/access");
    cfg.ip_deny_path = None;
    let (_srv, addr) = start_with(&root, cfg);
    build(&addr, "alpha");
    let j = root_api(&addr, "query", &[("db_name", "alpha"), ("sparql", ASK)]);
    assert_eq!(code(&j), 0, "{j}");
    assert!(
        !root.join("logs").join("endpoint").exists(),
        "query log off"
    );
    let j = root_api(&addr, "querylogdate", &[]);
    assert_eq!(list(&j).len(), 0, "{j}");
    let dir = root.join("custom").join("access");
    let dates = gstore::gapi::apilog::log_dates(&dir);
    assert_eq!(dates.len(), 1, "access log under the configured path");
    let j = root_api(&addr, "accesslog", &[("date", &dates[0])]);
    assert!(n(&j, "totalSize") >= 1, "{j}");
}

// ---- the privilege model through the library API ----------------------------------

#[test]
fn every_privilege_class_maps_to_its_operations() {
    let root = tmp_root("model");
    let (mut users, _sys) = sysdb::open_system(&root, "pw").unwrap();
    users.add("u", "pw", BTreeSet::new()).unwrap();
    let classes: [(&str, &[&str]); 7] = [
        ("1", &["query", "monitor"]),
        ("2", &["load"]),
        ("3", &["unload"]),
        (
            "4",
            &[
                "update",
                "batchInsert",
                "batchRemove",
                "begin",
                "tquery",
                "commit",
                "rollback",
                "checkpoint",
            ],
        ),
        ("5", &["backup"]),
        ("6", &["restore"]),
        ("7", &["export"]),
    ];
    for (code, ops) in classes {
        let p = Privilege::parse(code).unwrap();
        for op in ops {
            assert!(!sysdb::allowed(&users, "u", op, Some("alpha")), "{op}");
        }
        users.grant_on("u", "alpha", &BTreeSet::from([p])).unwrap();
        for op in ops {
            assert!(sysdb::allowed(&users, "u", op, Some("alpha")), "{op}");
            assert!(
                !sysdb::allowed(&users, "u", op, Some("beta")),
                "{op} on beta"
            );
            assert!(!sysdb::allowed(&users, "u", op, None), "{op} without a db");
        }
        users.clear_privileges("u").unwrap();
    }
    for op in ["login", "show", "userpassword", "check", "funquery"] {
        assert!(sysdb::allowed(&users, "u", op, None), "{op}");
    }
    users.grant_all_on("u", "alpha").unwrap();
    for op in ["build", "drop", "stat", "rename", "showuser", "txnlog"] {
        assert!(!sysdb::allowed(&users, "u", op, Some("alpha")), "{op}");
        assert!(sysdb::allowed(&users, "root", op, Some("alpha")), "{op}");
    }
    assert!(users
        .get("u")
        .unwrap()
        .privilege_str()
        .contains("query@alpha"));
    let _ = std::fs::remove_dir_all(&root);
}

/// The user-management requests of upstream's `scripts/test/api_test.sh`,
/// byte for byte (root password `123456`), in its order.
#[test]
fn upstream_api_test_user_sequence() {
    let root = tmp_root("api_test_sh");
    let (_srv, addr) = serve(ApiServer::bind(&root, "123456", "127.0.0.1:0").unwrap());
    let r = send(
        &addr,
        "POST",
        "/build?db_name=lubm",
        &[("Authorization", &basic("root", "123456"))],
        DATA,
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let post = |body: &str| {
        let r = send(
            &addr,
            "POST",
            "/api",
            &[("Content-Type", "application/json")],
            body,
        );
        assert_eq!(r.status, 200, "{}", r.body);
        r.json()
    };
    let steps = [
        r#"{"operation":"usermanage","username":"root","password":"123456","type":"1","op_username":"test","op_password":"123456"}"#,
        r#"{"operation":"showuser","username":"root","password":"123456"}"#,
        r#"{"operation":"userprivilegemanage","username":"root","password":"123456","type":"1","op_username":"test","privileges":"1,2,3","db_name":"lubm"}"#,
        r#"{"operation":"userprivilegemanage","username":"root","password":"123456","type":"3","op_username":"test"}"#,
        r#"{"operation":"userpassword","username":"test","password":"123456","op_password":"111111"}"#,
        r#"{"operation":"usermanage","username":"root","password":"123456","type":"2","op_username":"test"}"#,
    ];
    let expected = [
        "Add user done.",
        "success",
        "add privilege query,load,unload successfully.",
        "Clear the all privileges for the user successfully!",
        "Change password done.",
        "Delete user done.",
    ];
    for (body, want) in steps.iter().zip(expected) {
        let j = post(body);
        assert_eq!((code(&j), msg(&j)), (0, want), "{body}: {j}");
    }
}
