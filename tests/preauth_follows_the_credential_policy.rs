//! The pre-body credential check (r1-ingest-caps, `gstore::preauth`) grants
//! the upload cap only to credentials the full check after the body
//! (`gapi::auth::verify`, r1-http-surface's browser and password policy)
//! would accept too. Root's default password is a loopback privilege: from
//! a client that is not local — a loopback proxy forwarding a remote one —
//! it is refused after the body, so before the body it must not lift the
//! pre-authentication cap (review C-2: sec#1, gapi#1; H-5: sec#8).
//!
//! Every refusal is answered before the client sends a body byte (`Expect:
//! 100-continue`), so nothing waits for a timeout; socket timeouts are hang
//! guards only.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::{GStoreConfig, DEFAULT_ROOT_PASSWORD};
use gstore::http_api::ApiServer;
use gstore::server::HttpOptions;

/// A hang guard for every socket operation (never an assertion).
const HANG_GUARD: Duration = Duration::from_secs(30);

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_preauth_policy_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A running multi-database server whose root still has the default
/// password; stopped (and its root removed) when dropped.
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

fn start_api(tag: &str, trust_proxy_headers: bool) -> Api {
    let root = TmpDir::new(tag);
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    let opts = HttpOptions {
        preauth_max_body: 64 << 10,
        max_body: 1 << 20,
        trust_proxy_headers,
        ..HttpOptions::default()
    };
    let server = ApiServer::bind(root.path(), DEFAULT_ROOT_PASSWORD, "127.0.0.1:0")
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

/// Send an upload head announcing 100 KiB (over the 64 KiB pre-auth cap,
/// under the 1 MiB upload cap) with `Expect: 100-continue`, and return the
/// status of the first response: `100` when the server would read the body
/// under the upload cap, `413` when it holds it to the pre-auth cap.
fn first_status(addr: SocketAddr, target: &str, extra: &[(&str, &str)]) -> u16 {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    s.set_write_timeout(Some(HANG_GUARD)).unwrap();
    let mut head = format!(
        "POST {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\
         Content-Type: multipart/form-data; boundary=XyZ\r\nExpect: 100-continue\r\n",
        100 << 10
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

fn upload_target(password: &str) -> String {
    format!("/file/upload?username=root&password={password}")
}

#[test]
fn the_default_root_password_from_a_forwarded_client_gets_no_upload_cap() {
    let api = start_api("forwarded", false);
    let target = upload_target(DEFAULT_ROOT_PASSWORD);
    // A local client may use the default password (the loopback privilege):
    // its upload is authenticated from the head.
    assert_eq!(first_status(api.addr, &target, &[]), 100);
    // The same credentials forwarded by a loopback proxy speak for a remote
    // client: the full check refuses them after the body, so the body is
    // held to the pre-authentication cap.
    for header in ["X-Forwarded-For", "Forwarded", "X-Real-IP"] {
        let value = if header == "Forwarded" {
            "for=203.0.113.9"
        } else {
            "203.0.113.9"
        };
        assert_eq!(
            first_status(api.addr, &target, &[(header, value)]),
            413,
            "{header}"
        );
    }
    // Nothing was counted towards the lockout (as in the full check): the
    // local client is still served.
    assert_eq!(first_status(api.addr, &target, &[]), 100);
}

#[test]
fn a_trusted_proxy_keeps_the_loopback_privilege() {
    // `trust_proxy_headers=on` (the upstream-compat switch): a forwarding
    // loopback peer counts as local, before and after the body alike.
    let api = start_api("trusted", true);
    let target = upload_target(DEFAULT_ROOT_PASSWORD);
    assert_eq!(
        first_status(api.addr, &target, &[("X-Forwarded-For", "203.0.113.9")]),
        100
    );
}
