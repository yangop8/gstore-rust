//! Setting a password — the deliberately slow PBKDF2 hash and the users.db
//! rewrite with its `fsync` — runs with the server's state lock released
//! (review gapi#6, http#7): while one is in progress, requests that need
//! that lock (every request counts itself under it) are served.
//!
//! The hash is parked by a test hook (`gstore::http_users::test_hooks`) and
//! a request needing the lock is sent meanwhile; it must be answered while
//! the hash is still parked. A receive timeout is only a hang guard.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use gstore::http_api::ApiServer;
use gstore::http_users::test_hooks::park_hash_of;
use gstore::http_users::UserStore;
use gstore::json::{self, Json};

const HANG_GUARD: Duration = Duration::from_secs(60);

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_pw_off_lock_{tag}_{}_{n}",
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

fn start(dir: &TmpDir) -> (Arc<ApiServer>, SocketAddr) {
    let server = Arc::new(ApiServer::bind(dir.0.join("home"), "root", "127.0.0.1:0").unwrap());
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    (server, addr)
}

fn send(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
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
    (status, body.to_string())
}

fn api(addr: SocketAddr, pairs: &[(&str, &str)]) -> Json {
    let items: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}: {}", json::quote(k), json::quote(v)))
        .collect();
    let (status, body) = send(
        addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        &format!("{{{}}}", items.join(", ")),
    );
    assert_eq!(status, 200, "{body}");
    json::parse(&body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .expect("StatusCode")
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

fn add_user(addr: SocketAddr, name: &str, pw: &str) {
    let j = api(
        addr,
        &[
            ("username", "root"),
            ("password", "root"),
            ("operation", "usermanage"),
            ("type", "1"),
            ("op_username", name),
            ("op_password", pw),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
}

/// While a hash of `password` started by `change` (on its own thread) is
/// parked, an unauthenticated `/api` `check` — which takes the state lock
/// to count the request — must be answered. Returns what `change` answered.
fn while_parked<T: Send + 'static>(
    addr: SocketAddr,
    password: &str,
    change: impl FnOnce() -> T + Send + 'static,
) -> T {
    let park = park_hash_of(password);
    let changing = thread::spawn(change);
    assert!(
        park.wait_parked(HANG_GUARD),
        "the password hash never started"
    );
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let j = api(addr, &[("operation", "check")]);
        let _ = tx.send(code(&j));
    });
    let answered = rx.recv_timeout(HANG_GUARD);
    // Let the hash go before asserting, so a failure does not leave the
    // changing thread parked.
    park.release();
    let out = changing.join().expect("the change panicked");
    assert_eq!(
        answered,
        Ok(0),
        "a request needing the state lock was not answered while a password was hashed"
    );
    out
}

#[test]
fn rest_user_password_hashes_without_the_state_lock() {
    let dir = TmpDir::new("rest_pw");
    let (_srv, addr) = start(&dir);
    add_user(addr, "pw_rest", "old");
    let (status, body) = while_parked(addr, "new-pw-rest", move || {
        send(
            addr,
            "POST",
            "/user/password?op_password=new-pw-rest",
            &[("Authorization", &basic("pw_rest", "old"))],
            "",
        )
    });
    assert_eq!(status, 200, "{body}");
    let (status, _) = send(
        addr,
        "GET",
        "/show",
        &[("Authorization", &basic("pw_rest", "new-pw-rest"))],
        "",
    );
    assert_eq!(status, 200, "the new password is in force");
    let (status, _) = send(
        addr,
        "GET",
        "/show",
        &[("Authorization", &basic("pw_rest", "old"))],
        "",
    );
    assert_eq!(status, 401, "the old one is not");
}

#[test]
fn rest_user_create_hashes_without_the_state_lock() {
    let dir = TmpDir::new("rest_create");
    let (_srv, addr) = start(&dir);
    let (status, body) = while_parked(addr, "new-pw-create", move || {
        send(
            addr,
            "POST",
            "/user/create?op_username=pw_created&op_password=new-pw-create&privileges=query",
            &[("Authorization", &basic("root", "root"))],
            "",
        )
    });
    assert_eq!(status, 200, "{body}");
    let (status, _) = send(
        addr,
        "GET",
        "/show",
        &[("Authorization", &basic("pw_created", "new-pw-create"))],
        "",
    );
    assert_eq!(status, 200);
    // Persisted: a reload of users.db knows the user and its privilege.
    let store = UserStore::open(dir.0.join("home").join("users.db"), "unused").unwrap();
    assert!(store.verify("pw_created", "new-pw-create"));
    assert!(store.has_priv("pw_created", gstore::http_users::Privilege::Query));
}

#[test]
fn api_userpassword_hashes_without_the_state_lock() {
    let dir = TmpDir::new("api_pw");
    let (_srv, addr) = start(&dir);
    add_user(addr, "pw_api", "old");
    let j = while_parked(addr, "new-pw-api", move || {
        api(
            addr,
            &[
                ("username", "pw_api"),
                ("password", "old"),
                ("operation", "userpassword"),
                ("op_password", "new-pw-api"),
            ],
        )
    });
    assert_eq!(code(&j), 0, "{j}");
    let j = api(
        addr,
        &[
            ("username", "pw_api"),
            ("password", "new-pw-api"),
            ("operation", "login"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
}

#[test]
fn api_usermanage_hashes_without_the_state_lock() {
    let dir = TmpDir::new("api_manage");
    let (_srv, addr) = start(&dir);
    let j = while_parked(addr, "new-pw-manage", move || {
        api(
            addr,
            &[
                ("username", "root"),
                ("password", "root"),
                ("operation", "usermanage"),
                ("type", "1"),
                ("op_username", "pw_managed"),
                ("op_password", "new-pw-manage"),
            ],
        )
    });
    assert_eq!(code(&j), 0, "{j}");
    let j = while_parked(addr, "newer-pw-manage", move || {
        api(
            addr,
            &[
                ("username", "root"),
                ("password", "root"),
                ("operation", "usermanage"),
                ("type", "3"),
                ("op_username", "pw_managed"),
                ("op_password", "newer-pw-manage"),
            ],
        )
    });
    assert_eq!(code(&j), 0, "{j}");
    let j = api(
        addr,
        &[
            ("username", "pw_managed"),
            ("password", "newer-pw-manage"),
            ("operation", "login"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
}

#[test]
fn a_user_dropped_while_its_password_is_hashed_is_not_recreated() {
    let dir = TmpDir::new("dropped");
    let (_srv, addr) = start(&dir);
    add_user(addr, "pw_gone", "old");
    let park = park_hash_of("new-pw-gone");
    let changing = thread::spawn(move || {
        send(
            addr,
            "POST",
            "/user/password?username=pw_gone&op_password=new-pw-gone",
            &[("Authorization", &basic("root", "root"))],
            "",
        )
    });
    assert!(
        park.wait_parked(HANG_GUARD),
        "the password hash never started"
    );
    // Dropped meanwhile (the state lock is free).
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(send(
            addr,
            "POST",
            "/user/drop?op_username=pw_gone",
            &[("Authorization", &basic("root", "root"))],
            "",
        ));
    });
    let dropped = rx.recv_timeout(HANG_GUARD);
    park.release();
    let (status, body) = dropped.expect("the drop was not served while the hash was parked");
    assert_eq!(status, 200, "{body}");
    let (status, body) = changing.join().unwrap();
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("does not exist"), "{body}");
    let store = UserStore::open(dir.0.join("home").join("users.db"), "unused").unwrap();
    assert!(
        !store.exists("pw_gone"),
        "not brought back by the password change"
    );
}
