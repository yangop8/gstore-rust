//! Data test: the gStore-parity HTTP API ([`gstore::http_api::ApiServer`]).
//!
//! Each test boots an `ApiServer` on `127.0.0.1:0` in a background thread (like
//! `tests/dt_service.rs`) and drives it with a hand-rolled `std::net::TcpStream`
//! HTTP/1.1 client — covering login + privilege enforcement (401/403), an HTTP
//! transaction (commit visible, rollback not), a backup→restore round-trip,
//! batch insert reflected in a query, and the `/show` + `/drop` lifecycle.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use gstore::http_api::ApiServer;

const DATA: &str = "@prefix : <http://ex/> .\n:a :p :b .\n:a :name \"Alice\" .\n";

/// A unique temp directory for one server's databases + logs + user store.
fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let mut p = std::env::temp_dir();
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    p.push(format!("gstore_api_{tag}_{pid}_{n}"));
    p
}

/// Start an `ApiServer` (root password "root") in a background thread.
fn start(tag: &str) -> (Arc<ApiServer>, SocketAddr, PathBuf) {
    let root = tmp_root(tag);
    let server = Arc::new(ApiServer::bind(&root, "root", "127.0.0.1:0").expect("bind"));
    let addr = server.local_addr().expect("addr");
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    (server, addr, root)
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
    let status = resp
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .expect("status code");
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

/// `Authorization: Basic …` value for `user:pass`.
fn basic(user: &str, pass: &str) -> String {
    format!("Basic {}", base64(format!("{user}:{pass}").as_bytes()))
}

/// Extract a JSON string field's value (naive — fine for our flat responses).
fn json_field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

// -----------------------------------------------------------------------------

#[test]
fn login_and_privilege_enforcement() {
    let (_srv, addr, _root) = start("auth");
    let root = basic("root", "root");

    // No credentials at all -> 401.
    let (s, _) = request(&addr, "POST", "/update?db_name=g", None, "INSERT DATA {}");
    assert_eq!(s, 401, "missing auth must be 401");

    // Wrong password -> 401 on login.
    let (s, _) = request(
        &addr,
        "POST",
        "/login?username=root&password=wrong",
        None,
        "",
    );
    assert_eq!(s, 401, "bad login must be 401");

    // root can log in and receives a session token.
    let (s, body) = request(&addr, "POST", "/login?username=root&password=root", None, "");
    assert_eq!(s, 200);
    let token = json_field(&body, "session").expect("session token").to_string();
    assert!(!token.is_empty());

    // root builds a database (RDF in the body).
    let (s, _) = request(&addr, "POST", "/build?db_name=g", Some(&root), DATA);
    assert_eq!(s, 200);

    // root creates a query-only user "reader".
    let (s, body) = request(
        &addr,
        "POST",
        "/user/create?op_username=reader&op_password=pw&privileges=query",
        Some(&format!("Bearer {token}")),
        "",
    );
    assert_eq!(s, 200, "user/create via bearer token: {body}");

    let reader = basic("reader", "pw");

    // reader CAN run a read query (Query privilege).
    let (s, body) = request(
        &addr,
        "POST",
        "/sparql?db_name=g",
        Some(&reader),
        "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }",
    );
    assert_eq!(s, 200, "reader query should succeed: {body}");
    assert!(body.contains("http://ex/b"));

    // reader CANNOT update (no Update privilege) -> 403.
    let (s, _) = request(
        &addr,
        "POST",
        "/update?db_name=g",
        Some(&reader),
        "INSERT DATA { <http://ex/x> <http://ex/p> <http://ex/y> }",
    );
    assert_eq!(s, 403, "reader update must be forbidden");

    // reader CANNOT manage users (no Admin) -> 403.
    let (s, _) = request(
        &addr,
        "POST",
        "/user/create?op_username=evil&op_password=pw",
        Some(&reader),
        "",
    );
    assert_eq!(s, 403, "reader user-create must be forbidden");
}

#[test]
fn http_transaction_commit_and_rollback() {
    let (_srv, addr, _root) = start("txn");
    let root = basic("root", "root");

    // Empty database to transact over.
    let (s, _) = request(&addr, "POST", "/build?db_name=tx", Some(&root), "");
    assert_eq!(s, 200);

    // --- commit path: begin -> insert -> commit -> visible ---
    let (s, body) = request(&addr, "POST", "/begin?db_name=tx", Some(&root), "");
    assert_eq!(s, 200);
    let tid = json_field(&body, "TID").expect("TID").to_string();

    let (s, _) = request(
        &addr,
        "POST",
        &format!("/tquery?txn={tid}"),
        Some(&root),
        "INSERT DATA { <http://ex/s1> <http://ex/p> <http://ex/o1> }",
    );
    assert_eq!(s, 200);
    let (s, _) = request(&addr, "POST", &format!("/commit?txn={tid}"), Some(&root), "");
    assert_eq!(s, 200);

    let (s, body) = request(
        &addr,
        "POST",
        "/sparql?db_name=tx",
        Some(&root),
        "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/o1> }",
    );
    assert_eq!(s, 200);
    assert!(
        body.contains("http://ex/s1"),
        "committed insert must be visible: {body}"
    );

    // --- rollback path: begin -> insert -> rollback -> NOT visible ---
    let (s, body) = request(&addr, "POST", "/begin?db_name=tx", Some(&root), "");
    assert_eq!(s, 200);
    let tid = json_field(&body, "TID").expect("TID").to_string();

    let (s, _) = request(
        &addr,
        "POST",
        &format!("/tquery?txn={tid}"),
        Some(&root),
        "INSERT DATA { <http://ex/s2> <http://ex/p> <http://ex/o2> }",
    );
    assert_eq!(s, 200);
    let (s, _) = request(
        &addr,
        "POST",
        &format!("/rollback?txn={tid}"),
        Some(&root),
        "",
    );
    assert_eq!(s, 200);

    let (s, body) = request(
        &addr,
        "POST",
        "/sparql?db_name=tx",
        Some(&root),
        "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/o2> }",
    );
    assert_eq!(s, 200);
    assert!(
        !body.contains("http://ex/s2"),
        "rolled-back insert must NOT be visible: {body}"
    );
}

#[test]
fn backup_and_restore_roundtrip() {
    let (_srv, addr, root_dir) = start("bk");
    let root = basic("root", "root");
    let backup_path = root_dir.join("bk_snapshot");

    // Build with the original triple :a :p :b .
    let (s, _) = request(&addr, "POST", "/build?db_name=bk", Some(&root), DATA);
    assert_eq!(s, 200);

    // Backup the original state.
    let target = format!("/backup?db_name=bk&path={}", backup_path.display());
    let (s, body) = request(&addr, "POST", &target, Some(&root), "");
    assert_eq!(s, 200, "backup: {body}");

    // Mutate after the backup: add :c :p :d .
    let (s, _) = request(
        &addr,
        "POST",
        "/update?db_name=bk",
        Some(&root),
        "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }",
    );
    assert_eq!(s, 200);
    let (_s, body) = request(
        &addr,
        "POST",
        "/sparql?db_name=bk",
        Some(&root),
        "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/d> }",
    );
    assert!(body.contains("http://ex/c"), "mutation visible pre-restore");

    // Restore from the backup: the post-backup mutation must vanish.
    let target = format!("/restore?db_name=bk&path={}", backup_path.display());
    let (s, body) = request(&addr, "POST", &target, Some(&root), "");
    assert_eq!(s, 200, "restore: {body}");

    let (_s, body) = request(
        &addr,
        "POST",
        "/sparql?db_name=bk",
        Some(&root),
        "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/d> }",
    );
    assert!(
        !body.contains("http://ex/c"),
        "post-backup mutation must be gone after restore: {body}"
    );

    // The original triple is still present.
    let (_s, body) = request(
        &addr,
        "POST",
        "/sparql?db_name=bk",
        Some(&root),
        "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }",
    );
    assert!(
        body.contains("http://ex/b"),
        "original data must survive restore: {body}"
    );
}

#[test]
fn batch_insert_reflected_in_query() {
    let (_srv, addr, _root) = start("batch");
    let root = basic("root", "root");

    let (s, _) = request(&addr, "POST", "/build?db_name=batch", Some(&root), "");
    assert_eq!(s, 200);

    // Many triples in one request (full-IRI N-Triples in the body).
    let triples = "<http://ex/p1> <http://ex/k> <http://ex/v1> . \
                   <http://ex/p2> <http://ex/k> <http://ex/v2> . \
                   <http://ex/p3> <http://ex/k> <http://ex/v3> .";
    let (s, body) = request(&addr, "POST", "/batchInsert?db_name=batch", Some(&root), triples);
    assert_eq!(s, 200, "batchInsert: {body}");
    assert!(body.contains("\"changed\":3"), "three triples added: {body}");

    let (s, body) = request(
        &addr,
        "POST",
        "/sparql?db_name=batch",
        Some(&root),
        "SELECT ?s WHERE { ?s <http://ex/k> ?o }",
    );
    assert_eq!(s, 200);
    for s in ["http://ex/p1", "http://ex/p2", "http://ex/p3"] {
        assert!(body.contains(s), "batch triple {s} must be queryable: {body}");
    }

    // batchRemove takes some away again.
    let (s, body) = request(
        &addr,
        "POST",
        "/batchRemove?db_name=batch",
        Some(&root),
        "<http://ex/p2> <http://ex/k> <http://ex/v2> .",
    );
    assert_eq!(s, 200, "batchRemove: {body}");
    assert!(body.contains("\"changed\":1"));
}

#[test]
fn show_and_drop_lifecycle() {
    let (_srv, addr, _root) = start("life");
    let root = basic("root", "root");

    let (s, _) = request(&addr, "POST", "/build?db_name=alpha", Some(&root), DATA);
    assert_eq!(s, 200);
    let (s, _) = request(&addr, "POST", "/build?db_name=beta", Some(&root), DATA);
    assert_eq!(s, 200);

    // /show lists both.
    let (s, body) = request(&addr, "GET", "/show", Some(&root), "");
    assert_eq!(s, 200);
    assert!(body.contains("\"alpha\""), "show lists alpha: {body}");
    assert!(body.contains("\"beta\""), "show lists beta: {body}");

    // Drop alpha.
    let (s, body) = request(&addr, "POST", "/drop?db_name=alpha", Some(&root), "");
    assert_eq!(s, 200, "drop alpha: {body}");

    // /show no longer lists alpha; beta remains.
    let (s, body) = request(&addr, "GET", "/show", Some(&root), "");
    assert_eq!(s, 200);
    assert!(!body.contains("\"alpha\""), "alpha gone from show: {body}");
    assert!(body.contains("\"beta\""), "beta remains: {body}");

    // Dropping a non-existent database is 404.
    let (s, _) = request(&addr, "POST", "/drop?db_name=ghost", Some(&root), "");
    assert_eq!(s, 404);
}

#[test]
fn export_returns_ntriples() {
    let (_srv, addr, _root) = start("export");
    let root = basic("root", "root");
    let (s, _) = request(&addr, "POST", "/build?db_name=ex", Some(&root), DATA);
    assert_eq!(s, 200);

    let (s, body) = request(&addr, "GET", "/export?db_name=ex", Some(&root), "");
    assert_eq!(s, 200);
    assert!(body.contains("<http://ex/a>"), "export contains subjects: {body}");
    assert!(body.contains("<http://ex/p>"), "export contains predicates: {body}");
    assert!(body.trim_end().ends_with('.'), "N-Triples lines end with a dot");
}

#[test]
fn monitor_and_logs_are_admin_gated() {
    let (_srv, addr, _root) = start("mon");
    let root = basic("root", "root");
    let (s, _) = request(&addr, "POST", "/build?db_name=m", Some(&root), DATA);
    assert_eq!(s, 200);

    // /monitor reflects the loaded database for any authenticated user.
    let (s, body) = request(&addr, "GET", "/monitor", Some(&root), "");
    assert_eq!(s, 200);
    assert!(body.contains("\"databases\""), "monitor json: {body}");
    assert!(body.contains("\"m\""), "monitor lists db m: {body}");

    // The access log (admin-only) records the requests we just made.
    let (s, body) = request(&addr, "GET", "/accesslog", Some(&root), "");
    assert_eq!(s, 200);
    assert!(body.contains("/monitor"), "access log captured /monitor: {body}");

    // A non-admin user cannot read logs.
    let (s, _) = request(
        &addr,
        "POST",
        "/user/create?op_username=joe&op_password=pw&privileges=query",
        Some(&root),
        "",
    );
    assert_eq!(s, 200);
    let joe = basic("joe", "pw");
    let (s, _) = request(&addr, "GET", "/accesslog", Some(&joe), "");
    assert_eq!(s, 403, "non-admin cannot read logs");
}
