//! The failed-login lockout (review gapi#5, sec#7, http#6): failures are
//! counted per account and client address, and once an account is locked
//! for an address every path that checks a password refuses it **before the
//! password hash runs** — `/api` `login`, `testConnect` and the other
//! operations, REST `/login` and Basic credentials, the pre-body credential
//! check, and the single-database server's Basic auth. One account's success
//! does not unlock another, and `upstream_login_lockout=on` restores
//! upstream's per-address rule (and `gserver` says so at startup).
//!
//! Whether a refused attempt ran the hash is observed with the user store's
//! verification counter (`gstore::http_users::test_hooks`), keyed by a user
//! name no other test uses.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use gstore::config::GStoreConfig;
use gstore::gapi::ipfilter::{LOCKED_MSG, LOCKOUT_MSG_PREFIX, MAX_AUTH_FAILURES};
use gstore::http_api::ApiServer;
use gstore::http_users::test_hooks::count_verifications;
use gstore::json::{self, Json};
use gstore::server::Server;
use gstore::Database;

// ---- harness ------------------------------------------------------------------

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_login_lockout_{tag}_{}_{n}",
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

/// An `ApiServer` (root password `root`) with `config`, serving on a thread.
fn api_server(dir: &TmpDir, config: GStoreConfig) -> (Arc<ApiServer>, SocketAddr) {
    let server = Arc::new(
        ApiServer::bind(dir.0.join("home"), "root", "127.0.0.1:0")
            .unwrap()
            .with_config(config),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    (server, addr)
}

struct Reply {
    status: u16,
    head: String,
    body: String,
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
    read_reply(&mut s)
}

fn read_reply(s: &mut TcpStream) -> Reply {
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
        head: head.to_string(),
        body: body.to_string(),
    }
}

/// `POST /api` with a JSON body (always HTTP 200).
fn api(addr: &SocketAddr, pairs: &[(&str, &str)]) -> Json {
    let items: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}: {}", json::quote(k), json::quote(v)))
        .collect();
    let r = send(
        addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        &format!("{{{}}}", items.join(", ")),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    json::parse(&r.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", r.body))
}

fn op(addr: &SocketAddr, user: &str, password: &str, operation: &str) -> Json {
    api(
        addr,
        &[
            ("username", user),
            ("password", password),
            ("operation", operation),
        ],
    )
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

/// Create user `name` with password `pw` (as root).
fn add_user(addr: &SocketAddr, name: &str, pw: &str) {
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

fn header<'a>(r: &'a Reply, name: &str) -> Option<&'a str> {
    r.head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

// ---- the /api protocol --------------------------------------------------------

#[test]
fn api_login_and_test_connect_are_refused_for_a_locked_account_without_hashing() {
    let dir = TmpDir::new("api");
    let (_srv, addr) = api_server(&dir, GStoreConfig::default());
    add_user(&addr, "lock_api", "right");
    add_user(&addr, "lock_api_bob", "bobpw");
    let hashes = count_verifications("lock_api");
    for _ in 0..=MAX_AUTH_FAILURES {
        assert_eq!(code(&op(&addr, "lock_api", "wrong", "login")), 1001);
    }
    let counted = hashes.count();
    assert_eq!(
        counted,
        u64::from(MAX_AUTH_FAILURES) + 1,
        "each failure hashed"
    );

    // Locked: the right password is refused too — by login, testConnect and
    // any other operation — and none of them runs the hash.
    for operation in ["login", "testConnect", "getCoreVersion"] {
        let j = op(&addr, "lock_api", "right", operation);
        assert_eq!(code(&j), 1101, "{operation}: {j}");
        assert!(msg(&j).starts_with(LOCKOUT_MSG_PREFIX), "{j}");
    }
    assert_eq!(code(&op(&addr, "lock_api", "wrong", "login")), 1101);
    assert_eq!(hashes.count(), counted, "refused attempts ran no hash");

    // Another account from the same address is not affected, and its
    // success does not unlock the first one.
    assert_eq!(code(&op(&addr, "lock_api_bob", "bobpw", "login")), 0);
    assert_eq!(code(&op(&addr, "lock_api", "right", "login")), 1101);
    // Root keeps working (the address as a whole is not locked).
    assert_eq!(code(&op(&addr, "root", "root", "getCoreVersion")), 0);
}

#[test]
fn guessing_across_user_names_locks_the_address_for_password_checks() {
    let dir = TmpDir::new("spray");
    let (_srv, addr) = api_server(&dir, GStoreConfig::default());
    add_user(&addr, "spray_target", "right");
    let max = gstore::gapi::ipfilter::LockoutPolicy::default().max_address_failures;
    let mut refused = 0;
    for i in 0..=max {
        let j = op(&addr, &format!("spray{i}"), "wrong", "login");
        if code(&j) == 1101 {
            refused += 1;
        }
    }
    assert_eq!(
        refused, 0,
        "below the address threshold every guess is checked"
    );
    let hashes = count_verifications("spray_target");
    let j = op(&addr, "spray_target", "right", "login");
    assert_eq!(code(&j), 1101, "{j}");
    assert_eq!(hashes.count(), 0, "refused without hashing");
}

// ---- the REST endpoints -------------------------------------------------------

#[test]
fn rest_login_and_basic_credentials_are_refused_for_a_locked_account() {
    let dir = TmpDir::new("rest");
    let (_srv, addr) = api_server(&dir, GStoreConfig::default());
    add_user(&addr, "lock_rest", "right");
    add_user(&addr, "lock_rest_bob", "bobpw");
    let hashes = count_verifications("lock_rest");
    for i in 0..=MAX_AUTH_FAILURES {
        let r = send(
            &addr,
            "POST",
            &format!("/login?username=lock_rest&password=bad{i}"),
            &[],
            "",
        );
        assert_eq!(r.status, 401, "{}", r.body);
    }
    let counted = hashes.count();
    // `/login` with the right password, and Basic credentials: 429 with
    // Retry-After, no hash.
    let r = send(
        &addr,
        "POST",
        "/login?username=lock_rest&password=right",
        &[],
        "",
    );
    assert_eq!(r.status, 429, "{}", r.body);
    assert!(header(&r, "Retry-After").is_some(), "{}", r.head);
    assert!(!r.body.contains("session"), "{}", r.body);
    let r = send(
        &addr,
        "GET",
        "/show",
        &[("Authorization", &basic("lock_rest", "right"))],
        "",
    );
    assert_eq!(r.status, 429, "{}", r.body);
    assert!(r.body.contains(LOCKOUT_MSG_PREFIX), "{}", r.body);
    assert_eq!(hashes.count(), counted, "refused attempts ran no hash");

    // Other accounts from the address are served; their success does not
    // unlock the first account.
    let r = send(
        &addr,
        "GET",
        "/show",
        &[("Authorization", &basic("lock_rest_bob", "bobpw"))],
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let r = send(
        &addr,
        "POST",
        "/login?username=lock_rest_bob&password=bobpw",
        &[],
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let r = send(
        &addr,
        "GET",
        "/show",
        &[("Authorization", &basic("lock_rest", "right"))],
        "",
    );
    assert_eq!(r.status, 429, "{}", r.body);
}

#[test]
fn failed_basic_credentials_count_towards_the_lockout() {
    let dir = TmpDir::new("basic");
    let (_srv, addr) = api_server(&dir, GStoreConfig::default());
    add_user(&addr, "lock_basic", "right");
    for i in 0..=MAX_AUTH_FAILURES {
        let r = send(
            &addr,
            "GET",
            "/show",
            &[("Authorization", &basic("lock_basic", &format!("bad{i}")))],
            "",
        );
        assert_eq!(r.status, 401, "{}", r.body);
    }
    // The /api login of the account is refused as well.
    let j = op(&addr, "lock_basic", "right", "login");
    assert_eq!(code(&j), 1101, "{j}");
}

// ---- the pre-body credential check ------------------------------------------

#[test]
fn the_pre_body_check_refuses_a_locked_account_before_hashing() {
    let dir = TmpDir::new("preauth");
    let (_srv, addr) = api_server(&dir, GStoreConfig::default());
    add_user(&addr, "lock_pre", "right");
    for _ in 0..=MAX_AUTH_FAILURES {
        assert_eq!(code(&op(&addr, "lock_pre", "wrong", "login")), 1001);
    }
    let hashes = count_verifications("lock_pre");
    // Credentials in the URL query and a chunked body: the pre-body hook
    // checks them before reading the body.
    let mut s = TcpStream::connect(addr).unwrap();
    let body = r#"{"operation":"testConnect"}"#;
    let raw = format!(
        "POST /api?username=lock_pre&password=right HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\
         Connection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let r = read_reply(&mut s);
    let j = json::parse(&r.body).unwrap_or_else(|e| panic!("{e}: {}", r.body));
    assert_eq!(code(&j), 1101, "{j}");
    assert_eq!(
        hashes.count(),
        0,
        "neither the pre-body check nor the handler hashed"
    );
}

// ---- the single-database server ---------------------------------------------

#[test]
fn the_single_database_server_throttles_basic_auth() {
    let db = Database::build_from_str("lockout", "<http://a> <http://b> <http://c> .\n").unwrap();
    let server = Arc::new(
        Server::bind(db, "127.0.0.1:0")
            .unwrap()
            .with_basic_auth("admin", "s3cret"),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    let get = |user: &str, pass: &str| {
        send(
            &addr,
            "GET",
            "/sparql?query=ASK%7B%3Fs%20%3Fp%20%3Fo%7D",
            &[("Authorization", &basic(user, pass))],
            "",
        )
    };
    assert_eq!(get("admin", "s3cret").status, 200);
    for i in 0..=MAX_AUTH_FAILURES {
        assert_eq!(get("admin", &format!("bad{i}")).status, 401);
    }
    let r = get("admin", "s3cret");
    assert_eq!(r.status, 429, "locked: {}", r.body);
    assert!(header(&r, "Retry-After").is_some(), "{}", r.head);
    // Counted per user name: another name is still answered 401, not 429.
    assert_eq!(get("other", "x").status, 401);
    // A request without credentials is not a failed login.
    let r = send(&addr, "GET", "/status", &[], "");
    assert_eq!(r.status, 401, "{}", r.body);
    server.shutdown();
}

// ---- the upstream-compat switch -----------------------------------------------

#[test]
fn upstream_login_lockout_restores_the_per_address_rule() {
    let dir = TmpDir::new("upstream");
    let mut config = GStoreConfig::default();
    config.upstream_login_lockout = true;
    let (_srv, addr) = api_server(&dir, config);
    add_user(&addr, "lock_up", "right");
    for _ in 0..=MAX_AUTH_FAILURES {
        assert_eq!(code(&op(&addr, "lock_up", "wrong", "login")), 1001);
    }
    // Upstream: the address is locked out of every operation but login /
    // testConnect, whoever asks.
    let j = op(&addr, "root", "root", "getCoreVersion");
    assert_eq!(code(&j), 1101, "{j}");
    assert_eq!(msg(&j), LOCKED_MSG);
    assert_eq!(code(&op(&addr, "root", "root", "testConnect")), 0, "exempt");
    // Guesses are still evaluated through login, and any successful login
    // resets the address.
    assert_eq!(code(&op(&addr, "lock_up", "wrong", "login")), 1001);
    assert_eq!(code(&op(&addr, "root", "root", "login")), 0);
    assert_eq!(code(&op(&addr, "root", "root", "getCoreVersion")), 0);
}

#[test]
fn the_upstream_login_lockout_switch_is_reported_as_a_compat_warning() {
    let mut config = GStoreConfig::default();
    assert!(!config
        .upstream_compat_warnings()
        .iter()
        .any(|w| w.contains("upstream_login_lockout")));
    config.set("upstream_login_lockout", "on").unwrap();
    assert!(
        config
            .upstream_compat_warnings()
            .iter()
            .any(|w| w.contains("upstream-compat: upstream_login_lockout=on")),
        "{:?}",
        config.upstream_compat_warnings()
    );
}

/// Kills a background `gserver` (and its supervisor) by its pid file when
/// dropped — a test that fails before `gserver -t` leaves nothing running.
#[cfg(unix)]
struct KillOnDrop(PathBuf);

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        use std::process::{Command, Stdio};
        if let Ok(text) = std::fs::read_to_string(&self.0) {
            let lines: Vec<&str> = text.lines().collect();
            for pid in [lines.get(4), lines.first()].into_iter().flatten() {
                if !pid.is_empty() && *pid != "0" {
                    let _ = Command::new("kill")
                        .args(["-9", pid])
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
    }
}

#[cfg(unix)]
fn gserver(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    use std::process::{Command, Stdio};
    let out = Command::new(env!("CARGO_BIN_EXE_gserver"))
        .args(args)
        .current_dir(cwd)
        .env_remove("GSTORE_GSERVER_BACKGROUND")
        .env_remove("GSTORE_GSERVER_SUPERVISOR")
        .env_remove("GSTORE_ROOT_PASSWORD_FILE")
        .stdin(Stdio::null())
        .output()
        .expect("run gserver");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[cfg(unix)]
#[test]
fn gserver_prints_the_upstream_login_lockout_warning_at_startup() {
    let dir = TmpDir::new("gserver");
    let home = dir.0.join("home");
    let home_s = home.to_string_lossy().into_owned();
    // A store whose root has a non-default password.
    drop(ApiServer::bind(&home, "not-the-default", "127.0.0.1:0").unwrap());
    let _kill = KillOnDrop(home.join("gserver.pid"));
    let conf = dir.0.join("compat.ini");
    std::fs::write(&conf, "[server]\nupstream_login_lockout=on\n").unwrap();
    let conf_s = conf.to_string_lossy().into_owned();
    let (code, out, err) = gserver(&dir.0, &["-b", "--api", &home_s, "-p", "0", "-c", &conf_s]);
    assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
    let log = std::fs::read_to_string(home.join("gserver.log")).unwrap_or_default();
    assert!(
        log.contains("upstream-compat: upstream_login_lockout=on")
            || err.contains("upstream-compat: upstream_login_lockout=on"),
        "log:{log}\nstderr:{err}"
    );
    let (code, _, err) = gserver(&dir.0, &["-t", "--api", &home_s]);
    assert_eq!(code, 0, "{err}");
}
