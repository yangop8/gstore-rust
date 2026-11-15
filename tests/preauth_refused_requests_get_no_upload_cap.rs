//! A request that a server refuses before routing — after its body has been
//! read — never earns the upload cap before the body: the pre-body hook of
//! `gstore::preauth` applies the same rules as the refusals in
//! `ServerInner::handle` / `ApiInner::respond`.
//!
//! * The open single-database server (no credentials) serves local clients
//!   only: a loopback reverse proxy forwarding `Forwarded` /
//!   `X-Forwarded-For` / `X-Real-IP` is answered `403` after the body
//!   (unless `trust_proxy_headers=on`), so its body is held to the
//!   pre-authentication cap (round-1 review: the hook ignored the forwarding
//!   headers, which the API hook already honoured since the merge fix).
//! * The browser policy (`allowed_hosts`, `cors_allow_origin`) refuses a
//!   foreign `Host` on a loopback connection and a foreign `Origin` on a
//!   state-changing request in both servers; such a request is held to the
//!   pre-authentication cap too, whatever credentials its head carries.
//!
//! Every answer that matters is the first response to a head sent with
//! `Expect: 100-continue` (`100`: the upload cap; `413`: held to the
//! pre-authentication cap), so nothing waits for a timeout; socket timeouts
//! are hang guards only.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::{GStoreConfig, DEFAULT_ROOT_PASSWORD};
use gstore::http_api::ApiServer;
use gstore::server::{HttpOptions, Server};
use gstore::Database;

/// A hang guard for every socket operation (never an assertion).
const HANG_GUARD: Duration = Duration::from_secs(30);
/// The pre-authentication cap of every server here.
const PREAUTH: usize = 64 << 10;
/// The body every head announces: over [`PREAUTH`], under the upload cap.
const ANNOUNCED: usize = 100 << 10;

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_preauth_refused_{tag}_{}_{n}",
            std::process::id()
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

fn opts(trust_proxy_headers: bool, cors_allow_origin: &[&str]) -> HttpOptions {
    HttpOptions {
        preauth_max_body: PREAUTH,
        max_body: 1 << 20,
        trust_proxy_headers,
        cors_allow_origin: cors_allow_origin.iter().map(|s| s.to_string()).collect(),
        ..HttpOptions::default()
    }
}

/// A running single-database server; stopped when dropped.
struct Single {
    server: Arc<Server>,
    addr: SocketAddr,
    serving: Option<thread::JoinHandle<()>>,
}

impl Drop for Single {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(t) = self.serving.take() {
            let _ = t.join();
        }
    }
}

fn start_single(auth: Option<(&str, &str)>, opts: HttpOptions) -> Single {
    let db = Database::build_from_str("refused", "<http://a> <http://b> <http://c> .\n").unwrap();
    let mut server = Server::bind(db, "127.0.0.1:0")
        .unwrap()
        .with_http_options(opts);
    if let Some((u, p)) = auth {
        server = server.with_basic_auth(u, p);
    }
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || {
        s2.serve_forever();
    });
    Single {
        server,
        addr,
        serving: Some(serving),
    }
}

/// A running multi-database server whose root has the default password
/// (a loopback privilege); stopped (and its root removed) when dropped.
struct Api {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    serving: Option<thread::JoinHandle<()>>,
    _root: TmpDir,
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(t) = self.serving.take() {
            let _ = t.join();
        }
    }
}

fn start_api(opts: HttpOptions) -> Api {
    let root = TmpDir::new("api");
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    let server = ApiServer::bind(&root.0, DEFAULT_ROOT_PASSWORD, "127.0.0.1:0")
        .unwrap()
        .with_config(config)
        .with_http_options(opts);
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || {
        s2.serve_forever();
    });
    Api {
        server,
        addr,
        serving: Some(serving),
        _root: root,
    }
}

/// Send a `POST target` head announcing [`ANNOUNCED`] bytes with `Expect:
/// 100-continue`, `Host: host` and `extra` headers, and return the status of
/// the first response.
fn first_status(addr: SocketAddr, target: &str, host: &str, extra: &[(&str, &str)]) -> u16 {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    s.set_write_timeout(Some(HANG_GUARD)).unwrap();
    let mut head = format!(
        "POST {target} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {ANNOUNCED}\r\n\
         Content-Type: application/sparql-update\r\nExpect: 100-continue\r\n"
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        match s.read(&mut byte) {
            Ok(1) => line.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&line)
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// A complete small `POST target` (`Connection: close`): its status.
fn small_request(addr: SocketAddr, target: &str, extra: &[(&str, &str)]) -> u16 {
    let body = "INSERT DATA { <http://x> <http://y> <http://z> }";
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let mut head = format!(
        "POST {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\
         Content-Type: application/sparql-update\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut resp = String::new();
    let _ = s.read_to_string(&mut resp);
    resp.split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// `creds` followed by `extra`.
fn headers<'a>(
    creds: &[(&'a str, &'a str)],
    extra: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    creds.iter().chain(extra).copied().collect()
}

const FORWARDED: [(&str, &str); 3] = [
    ("X-Forwarded-For", "203.0.113.9"),
    ("Forwarded", "for=203.0.113.9"),
    ("X-Real-IP", "203.0.113.9"),
];

#[test]
fn the_open_single_db_server_gives_a_forwarded_client_no_upload_cap() {
    let single = start_single(None, opts(false, &[]));
    for target in ["/update", "/sparql"] {
        // A local client of the open server is served: the upload cap.
        assert_eq!(first_status(single.addr, target, "127.0.0.1", &[]), 100);
        for header in FORWARDED {
            // Refused after the body (403), so held to the pre-auth cap.
            assert_eq!(
                first_status(single.addr, target, "127.0.0.1", &[header]),
                413,
                "{target} {header:?}"
            );
        }
    }
    // The refusal after the body is what the hook anticipates.
    assert_eq!(small_request(single.addr, "/update", &[FORWARDED[0]]), 403);
    assert_eq!(small_request(single.addr, "/update", &[]), 200);
}

#[test]
fn a_trusted_proxy_keeps_the_upload_cap_of_the_open_single_db_server() {
    // `trust_proxy_headers=on` (upstream-compat): a forwarding loopback peer
    // is local, before and after the body alike.
    let single = start_single(None, opts(true, &[]));
    for header in FORWARDED {
        assert_eq!(
            first_status(single.addr, "/update", "127.0.0.1", &[header]),
            100,
            "{header:?}"
        );
    }
    assert_eq!(small_request(single.addr, "/update", &[FORWARDED[0]]), 200);
}

#[test]
fn the_single_db_server_gives_a_request_its_browser_policy_refuses_no_upload_cap() {
    let listed = "https://db.example.com";
    for auth in [None, Some(("u", "p"))] {
        let single = start_single(auth, opts(false, &[listed]));
        let basic = ("Authorization", "Basic dTpw"); // u:p
        let creds: &[(&str, &str)] = if auth.is_some() { &[basic] } else { &[] };
        let own_origin = format!("http://127.0.0.1:{}", single.addr.port());
        // Admitted: no Origin, the server's own origin, a listed origin.
        for extra in [
            headers(creds, &[]),
            headers(creds, &[("Origin", own_origin.as_str())]),
            headers(creds, &[("Origin", listed)]),
        ] {
            let host = format!("127.0.0.1:{}", single.addr.port());
            assert_eq!(
                first_status(single.addr, "/update", &host, &extra),
                100,
                "{auth:?} {extra:?}"
            );
        }
        // Refused before routing: a foreign Origin, a foreign Host.
        assert_eq!(
            first_status(
                single.addr,
                "/update",
                "127.0.0.1",
                &headers(creds, &[("Origin", "https://evil.example.com")])
            ),
            413,
            "{auth:?} foreign Origin"
        );
        assert_eq!(
            first_status(
                single.addr,
                "/sparql",
                "evil.example.com",
                &headers(creds, &[])
            ),
            413,
            "{auth:?} foreign Host"
        );
    }
}

#[test]
fn the_api_server_gives_a_request_its_browser_policy_refuses_no_upload_cap() {
    let api = start_api(opts(false, &[]));
    let target = format!("/file/upload?username=root&password={DEFAULT_ROOT_PASSWORD}");
    // Valid credentials from a local client: the upload cap.
    assert_eq!(first_status(api.addr, &target, "127.0.0.1", &[]), 100);
    // The same credentials on a request the browser policy refuses.
    assert_eq!(
        first_status(
            api.addr,
            &target,
            "127.0.0.1",
            &[("Origin", "https://evil.example.com")]
        ),
        413
    );
    assert_eq!(
        first_status(api.addr, &target, "evil.example.com", &[]),
        413
    );
}
