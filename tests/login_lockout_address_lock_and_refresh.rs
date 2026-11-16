//! The failed-login lockout after the round-2 review (security-limits):
//!
//! * the address-wide lock (more than `max_address_failures` failures of
//!   any accounts from one address) refuses only accounts not known good at
//!   that address: an account that authenticated from it keeps working
//!   while someone guesses other names from behind the same proxy or NAT,
//!   and the refusal says "from this address", not "for this account";
//! * `refreshconf` applies the lockout keys it re-reads
//!   (`upstream_login_lockout`, `login_lockout_failures`,
//!   `login_lockout_max_seconds`) instead of only reporting them.
//!
//! The lock durations do not matter here: the address lock is checked right
//! after a long first lock is set ([`ApiServer::with_lockout_policy`]), and
//! upstream's per-address rule has no expiry.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::gapi::ipfilter::{LockoutPolicy, LOCKED_MSG, MAX_AUTH_FAILURES};
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};

const HANG_GUARD: Duration = Duration::from_secs(60);

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_lockout_review_{tag}_{}_{n}",
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

/// A serving server, shut down and joined when dropped (before its
/// directory goes).
struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    serving: Option<thread::JoinHandle<gstore::server::ShutdownReport>>,
}

impl Srv {
    fn serve(server: ApiServer) -> Srv {
        let server = Arc::new(server);
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = Some(thread::spawn(move || s2.serve_forever()));
        Srv {
            server,
            addr,
            serving,
        }
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(s) = self.serving.take() {
            let _ = s.join();
        }
    }
}

fn api(addr: SocketAddr, pairs: &[(&str, &str)]) -> Json {
    let items: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}: {}", json::quote(k), json::quote(v)))
        .collect();
    let body = format!("{{{}}}", items.join(", "));
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    s.write_all(
        format!(
            "POST /api HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let body = resp.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    json::parse(body).unwrap_or_else(|e| panic!("not JSON ({e}): {resp}"))
}

fn op(addr: SocketAddr, user: &str, password: &str, operation: &str) -> Json {
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
    j.get("StatusCode").and_then(Json::as_i64).unwrap()
}

fn msg(j: &Json) -> String {
    j.get("StatusMsg")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

#[test]
fn guessing_other_names_does_not_lock_out_an_account_known_good_at_the_address() {
    let dir = TmpDir::new("known");
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    let policy = LockoutPolicy {
        first_lock: Duration::from_secs(3600),
        ..LockoutPolicy::default()
    };
    let srv = Srv::serve(
        ApiServer::bind(dir.0.join("home"), "root", "127.0.0.1:0")
            .unwrap()
            .with_config(config)
            .with_lockout_policy(policy),
    );
    let j = api(
        srv.addr,
        &[
            ("username", "root"),
            ("password", "root"),
            ("operation", "usermanage"),
            ("type", "1"),
            ("op_username", "newcomer"),
            ("op_password", "pw"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    // Someone behind the same address guesses across many names.
    for i in 0..=policy.max_address_failures {
        let j = op(srv.addr, &format!("nobody{i}"), "wrong", "login");
        assert_eq!(code(&j), 1001, "guess {i}: {j}");
    }
    // Root, known good here, keeps working.
    let j = op(srv.addr, "root", "root", "show");
    assert_eq!(code(&j), 0, "{j}");
    // An account never authenticated from here is refused by the address
    // lock, and told so.
    let j = op(srv.addr, "newcomer", "pw", "login");
    assert_eq!(code(&j), 1101, "{j}");
    assert!(
        msg(&j).starts_with("too many failed logins from this address"),
        "{j}"
    );
}

#[test]
fn refreshconf_applies_the_lockout_keys() {
    let dir = TmpDir::new("refresh");
    let conf = dir.0.join("conf.ini");
    let home = dir.0.join("home");
    let write_conf = |extra: &str| {
        std::fs::write(
            &conf,
            format!(
                "[server]\nport=0\nip_deny_path=off\n{extra}\n[system]\ndb_home={}\n",
                home.display()
            ),
        )
        .unwrap();
    };
    write_conf("");
    let mut cfg = GStoreConfig::load(&conf).unwrap();
    cfg.listen_addr = "127.0.0.1".to_string();
    cfg.root_password = "root".to_string();
    let server = ApiServer::bind_with_config(cfg).unwrap();
    assert_eq!(
        server.ip_filter().lockout().policy().max_failures,
        MAX_AUTH_FAILURES
    );
    assert!(!server.ip_filter().upstream_lockout());
    let srv = Srv::serve(server);

    write_conf("login_lockout_failures=1\nlogin_lockout_max_seconds=60\nupstream_login_lockout=on");
    let j = op(srv.addr, "root", "root", "refreshconf");
    assert_eq!(code(&j), 0, "{j}");
    let config = srv.server.config();
    assert_eq!(config.login_lockout_failures, 1);
    assert!(config.upstream_login_lockout);
    // In force, not only read back.
    let policy = srv.server.ip_filter().lockout().policy();
    assert_eq!(policy.max_failures, 1);
    assert_eq!(policy.max_lock, Duration::from_secs(60));
    assert!(srv.server.ip_filter().upstream_lockout());
    // Upstream's per-address rule now locks the address out of every
    // operation but login / testConnect.
    for _ in 0..=MAX_AUTH_FAILURES {
        assert_eq!(code(&op(srv.addr, "root", "wrong", "login")), 1001);
    }
    let j = op(srv.addr, "root", "root", "getCoreVersion");
    assert_eq!(code(&j), 1101, "{j}");
    assert_eq!(msg(&j), LOCKED_MSG);
}
