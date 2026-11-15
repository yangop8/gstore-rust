//! What an unauthenticated client can make the HTTP servers buffer (review
//! C-2: sec#1, gapi#1, infra#1; H-2: sec#2, robust#2): the body caps by route
//! class decided from the request head before any body byte is read, the
//! body budget shared by all connections, the `/api` parameter parser's
//! bounds and the multipart boundary rules — see `gstore::preauth`.
//!
//! Every exchange is deterministic: a refused request is answered before the
//! client sends any body byte (so no test waits for a timeout, and no unread
//! data turns the server's close into a reset), and the budget test waits on
//! the server's own budget counter (`preauth::wait_body_budget`). Socket
//! timeouts are hang guards only.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gstore::config::GStoreConfig;
use gstore::gapi::Params;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::server::{HttpOptions, Server};
use gstore::Database;

/// Root's password on the test servers.
const PW: &str = "Pre-auth-test-pw1";

/// One byte over the default pre-authentication cap (64 MiB).
const OVER_DEFAULT_PREAUTH: usize = (64 << 20) + 1;

/// A hang guard for every socket operation (never an assertion).
const HANG_GUARD: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A temporary directory, removed when dropped (also when a test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d =
            std::env::temp_dir().join(format!("gstore_preauth_{tag}_{}_{n}", std::process::id()));
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

/// A running multi-database server; stopped (and its root removed) when
/// dropped.
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

fn start_api(tag: &str, opts: HttpOptions) -> Api {
    let root = TmpDir::new(tag);
    let mut config = GStoreConfig::default();
    // No IP deny list from the working directory.
    config.ip_deny_path = None;
    let server = ApiServer::bind(root.path(), PW, "127.0.0.1:0")
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
    let db = Database::build_from_str("preauth", "<http://a> <http://b> <http://c> .\n").unwrap();
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

/// Options that compile against any version: only `max_body` (the upload
/// cap) is raised, so every request may carry 128 MiB unless the server
/// holds unauthenticated bodies to the default pre-authentication cap. The
/// short read timeout makes a server that waits for a body it should have
/// refused answer (`400`) quickly instead of hanging the test.
fn big_upload_cap() -> HttpOptions {
    HttpOptions {
        max_body: 128 << 20,
        read_timeout: Duration::from_secs(3),
        ..HttpOptions::default()
    }
}

fn connect(addr: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    s.set_write_timeout(Some(HANG_GUARD)).unwrap();
    s
}

/// One response read from `r`: its head (through the blank line) and, by
/// its `Content-Length`, its body. An interim `100 Continue` has no body.
fn read_response<R: Read>(r: &mut R) -> (String, String) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match r.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if len > 0 {
        let _ = r.read_exact(&mut body);
    }
    (head, String::from_utf8_lossy(&body).into_owned())
}

fn status(head: &str) -> u16 {
    head.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Send a request head (no body byte) and read the first response: its
/// status, head and body.
fn send_head(addr: SocketAddr, head: &str) -> (u16, String, String) {
    let mut s = connect(addr);
    s.write_all(head.as_bytes()).unwrap();
    let (h, b) = read_response(&mut s);
    (status(&h), h, b)
}

/// A complete request (closing the connection after it): status and body.
fn request(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String) {
    let mut s = connect(addr);
    let mut req = format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let (h, b) = read_response(&mut s);
    (status(&h), b)
}

/// A request head announcing a body of `len` bytes.
fn head_with_body(method: &str, target: &str, extra: &[(&str, &str)], len: usize) -> String {
    let mut h =
        format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {len}\r\n");
    for (k, v) in extra {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    h.push_str("\r\n");
    h
}

fn status_code(body: &str) -> i64 {
    json::parse(body)
        .ok()
        .and_then(|j| j.get("StatusCode").and_then(Json::as_i64))
        .unwrap_or(-1)
}

fn status_msg(body: &str) -> String {
    json::parse(body)
        .ok()
        .and_then(|j| {
            j.get("StatusMsg")
                .and_then(Json::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn basic(user: &str, pass: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let input = format!("{user}:{pass}").into_bytes();
    let mut out = String::from("Basic ");
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The lowercase hex MD5 digest of `s` (upstream's `encryption=1`).
fn md5_hex(s: &str) -> String {
    gstore::query::hash::md5_hex(s)
}

// ---------------------------------------------------------------------------
// The pre-authentication cap (with the default caps)
// ---------------------------------------------------------------------------

#[test]
fn an_unauthenticated_body_over_the_preauth_cap_is_refused_before_it_is_read() {
    let api = start_api("api413", big_upload_cap());
    let json = [("Content-Type", "application/json")];
    // Only the head is sent: the answer must come without the body.
    let (st, _, body) = send_head(
        api.addr,
        &head_with_body("POST", "/api", &json, OVER_DEFAULT_PREAUTH),
    );
    assert_eq!(st, 413, "{body}");
    assert!(body.contains("unauthenticated"), "{body}");
    // `Expect: 100-continue` is answered with the refusal, never with
    // `100 Continue`.
    let (st, head, _) = send_head(
        api.addr,
        &head_with_body(
            "POST",
            "/api",
            &[json[0], ("Expect", "100-continue")],
            OVER_DEFAULT_PREAUTH,
        ),
    );
    assert_eq!(st, 413, "{head}");
    // Credentials inside the (unsent) body do not count: the multipart
    // upload of an upstream client is held to the same cap.
    let (st, head, _) = send_head(
        api.addr,
        &head_with_body(
            "POST",
            "/file/upload",
            &[("Content-Type", "multipart/form-data; boundary=XyZ")],
            OVER_DEFAULT_PREAUTH,
        ),
    );
    assert_eq!(st, 413, "{head}");
}

#[test]
fn a_chunked_unauthenticated_body_is_cut_off_at_the_preauth_cap() {
    let api = start_api("chunk413", big_upload_cap());
    // The first chunk alone would pass the cap: refused at its size line.
    let head = format!(
        "POST /api HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Transfer-Encoding: chunked\r\n\r\n{OVER_DEFAULT_PREAUTH:x}\r\n"
    );
    let (st, _, body) = send_head(api.addr, &head);
    assert_eq!(st, 413, "{body}");
}

#[test]
fn wrong_head_credentials_are_refused_before_the_body_and_count_towards_the_lockout() {
    let api = start_api("lockout", big_upload_cap());
    let mp = [("Content-Type", "multipart/form-data; boundary=XyZ")];
    let wrong = head_with_body(
        "POST",
        "/file/upload?username=root&password=wrong",
        &mp,
        OVER_DEFAULT_PREAUTH,
    );
    // Five failures lock the address out (`ipfilter::MAX_AUTH_FAILURES`).
    for i in 0..5 {
        let (st, _, body) = send_head(api.addr, &wrong);
        assert_eq!(st, 401, "attempt {i}: {body}");
    }
    // The failures are the address's, for every route: `/api` now refuses
    // even the right password, and so does the pre-body check (the body is
    // then held to the pre-authentication cap).
    let (st, body) = request(
        api.addr,
        "GET",
        &format!("/api?operation=show&username=root&password={PW}"),
        &[],
        b"",
    );
    assert_eq!((st, status_code(&body)), (200, 1101), "{body}");
    let right = head_with_body(
        "POST",
        &format!("/file/upload?username=root&password={PW}"),
        &mp,
        OVER_DEFAULT_PREAUTH,
    );
    let (st, _, body) = send_head(api.addr, &right);
    assert_eq!(st, 413, "{body}");
}

#[test]
fn rest_payload_routes_authenticate_from_the_head() {
    let api = start_api("rest", big_upload_cap());
    let expect = ("Expect", "100-continue");
    let ctype = ("Content-Type", "application/sparql-update");
    let update = |auth: Option<&str>| {
        let mut extra = vec![ctype, expect];
        if let Some(a) = auth {
            extra.push(("Authorization", a));
        }
        send_head(
            api.addr,
            &head_with_body("POST", "/update", &extra, OVER_DEFAULT_PREAUTH),
        )
    };
    let (st, head, _) = update(None);
    assert_eq!(st, 413, "{head}");
    let (st, head, _) = update(Some(&basic("root", "nope")));
    assert_eq!(st, 401, "{head}");
    // Valid Basic credentials: the upload cap applies, the body is wanted.
    let (st, head, _) = update(Some(&basic("root", PW)));
    assert_eq!(st, 100, "{head}");
    // So with a session token.
    let (st, body) = request(
        api.addr,
        "POST",
        "/login",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        format!("username=root&password={PW}").as_bytes(),
    );
    assert_eq!(st, 200, "{body}");
    let token = json::parse(&body)
        .unwrap()
        .get("session")
        .and_then(Json::as_str)
        .unwrap()
        .to_string();
    let (st, head, _) = update(Some(&format!("Bearer {token}")));
    assert_eq!(st, 100, "{head}");
    let (st, head, _) = send_head(
        api.addr,
        &head_with_body(
            "POST",
            "/upload",
            &[("X-Session-Token", &token), expect],
            OVER_DEFAULT_PREAUTH,
        ),
    );
    assert_eq!(st, 100, "{head}");
    // A route that carries no payload stays capped whoever asks.
    let (st, head, _) = send_head(
        api.addr,
        &head_with_body(
            "POST",
            "/login",
            &[("Authorization", &basic("root", PW)), expect],
            OVER_DEFAULT_PREAUTH,
        ),
    );
    assert_eq!(st, 413, "{head}");
}

#[test]
fn the_single_database_server_holds_unauthenticated_bodies_to_the_preauth_cap() {
    let expect = ("Expect", "100-continue");
    let guarded = start_single(Some(("u", "p")), big_upload_cap());
    let (st, head, _) = send_head(
        guarded.addr,
        &head_with_body("POST", "/update", &[expect], OVER_DEFAULT_PREAUTH),
    );
    assert_eq!(st, 413, "{head}");
    let (st, head, _) = send_head(
        guarded.addr,
        &head_with_body(
            "POST",
            "/update",
            &[expect, ("Authorization", &basic("u", "p"))],
            OVER_DEFAULT_PREAUTH,
        ),
    );
    assert_eq!(st, 100, "{head}");
    // A server without credentials serves everyone as before on its payload
    // routes, and holds every other route to the pre-authentication cap.
    let open = start_single(None, big_upload_cap());
    for path in ["/update", "/sparql"] {
        let (st, head, _) = send_head(
            open.addr,
            &head_with_body("POST", path, &[expect], OVER_DEFAULT_PREAUTH),
        );
        assert_eq!(st, 100, "{path}: {head}");
    }
    let (st, head, _) = send_head(
        open.addr,
        &head_with_body("POST", "/status", &[expect], OVER_DEFAULT_PREAUTH),
    );
    assert_eq!(st, 413, "{head}");
}

// ---------------------------------------------------------------------------
// The /api parameter parser's bounds and the multipart boundary
// ---------------------------------------------------------------------------

#[test]
fn a_wide_json_body_is_refused_before_its_tree_is_built() {
    let api = start_api("widejson", big_upload_cap());
    // 100,000 values in 200 KB: well inside the body cap, but each would
    // cost tens of bytes as a parsed value (review gapi#1: ~33x).
    let mut body = String::from(r#"{"operation":"check","a":["#);
    body.push_str(&"0,".repeat(99_999));
    body.push_str("0]}");
    let (st, reply) = request(
        api.addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    );
    assert_eq!((st, status_code(&reply)), (200, 1003), "{reply}");
    assert!(
        status_msg(&reply).contains("too many JSON values"),
        "{reply}"
    );
    // An ordinary request passes.
    let (st, reply) = request(
        api.addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        br#"{"operation":"check","a":[1,2,3]}"#,
    );
    assert_eq!((st, status_code(&reply)), (200, 0), "{reply}");
}

#[test]
fn a_multipart_boundary_longer_than_rfc_2046_allows_is_refused() {
    let api = start_api("longboundary", big_upload_cap());
    let boundary = format!("{}B", "-".repeat(60_000));
    let ctype = format!("multipart/form-data; boundary={boundary}");
    let body = format!("{}\r\n", "-".repeat(1 << 16));
    let t = Instant::now();
    let (st, reply) = request(
        api.addr,
        "POST",
        "/api",
        &[("Content-Type", &ctype)],
        body.as_bytes(),
    );
    assert_eq!((st, status_code(&reply)), (200, 1003), "{reply}");
    assert!(status_msg(&reply).contains("longer than 70"), "{reply}");
    // Refused at once (a hang guard, not the assertion that matters).
    assert!(t.elapsed() < HANG_GUARD);
}

#[test]
fn a_64_character_boundary_over_megabytes_parses_in_linear_time() {
    // The search's worst case: the file's bytes match the delimiter up to
    // its last byte at every position.
    let boundary = format!("{}B", "-".repeat(63));
    let ctype = format!("multipart/form-data; boundary={boundary}");
    let body = |n: usize| {
        let mut b = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x.nt\"\r\n\r\n"
        )
        .into_bytes();
        b.extend(std::iter::repeat_n(b'-', n));
        b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        b
    };
    let (small, large) = (body(4 << 20), body(8 << 20));
    let parse = |b: &[u8]| {
        let p = Params::from_parts(None, Some(&ctype), b).unwrap();
        p.file("file").unwrap().data.len()
    };
    let best = |b: &[u8], n: usize| {
        (0..3)
            .map(|_| {
                let t = Instant::now();
                assert_eq!(parse(b), n);
                t.elapsed().as_secs_f64()
            })
            .fold(f64::INFINITY, f64::min)
    };
    let (t_small, t_large) = (best(&small, 4 << 20), best(&large, 8 << 20));
    // Linear: twice the body, about twice the time (a generous bound).
    assert!(
        t_large < 4.0 * t_small.max(2e-3),
        "8 MiB took {t_large:.4} s, 4 MiB {t_small:.4} s"
    );
}

// ---------------------------------------------------------------------------
// Authenticated uploads and the body budget (configured caps)
// ---------------------------------------------------------------------------

/// Small caps: 64 KiB before authentication, 1 MiB for an authenticated
/// upload.
fn small_caps() -> HttpOptions {
    HttpOptions {
        preauth_max_body: 64 << 10,
        max_body: 1 << 20,
        ..HttpOptions::default()
    }
}

/// A multipart body with a `file` part of `size` bytes (plus `fields`).
fn multipart(fields: &[(&str, &str)], size: usize) -> (String, Vec<u8>) {
    let b = "PreauthBoundary7";
    let mut body = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"big.nt\"\r\n\
             Content-Type: application/n-triples\r\n\r\n"
        )
        .as_bytes(),
    );
    let line = b"<http://example.org/s> <http://example.org/p> \"o\" .\n";
    while body.len() < size {
        body.extend_from_slice(line);
    }
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), body)
}

#[test]
fn an_upload_authenticated_from_its_head_may_carry_the_upload_cap() {
    let api = start_api("upload", small_caps());
    let (ctype, body) = multipart(&[], 200 << 10);
    // Credentials in the URL query string: plain, and as upstream's
    // `encryption=1` MD5 digest.
    for target in [
        format!("/file/upload?username=root&password={PW}"),
        format!(
            "/file/upload?username=root&password={}&encryption=1",
            md5_hex(PW)
        ),
    ] {
        let (st, reply) = request(
            api.addr,
            "POST",
            &target,
            &[("Content-Type", &ctype)],
            &body,
        );
        assert_eq!((st, status_code(&reply)), (200, 0), "{target}: {reply}");
    }
    // The same upload with its credentials in the body only (as upstream's
    // clients send them) is held to the pre-authentication cap.
    let (ctype, body) = multipart(&[("username", "root"), ("password", PW)], 200 << 10);
    let (st, head, _) = send_head(
        api.addr,
        &head_with_body(
            "POST",
            "/file/upload",
            &[("Content-Type", &ctype), ("Expect", "100-continue")],
            body.len(),
        ),
    );
    assert_eq!(st, 413, "{head}");
    // Within that cap it works as before.
    let (ctype, body) = multipart(&[("username", "root"), ("password", PW)], 10 << 10);
    let (st, reply) = request(
        api.addr,
        "POST",
        "/file/upload",
        &[("Content-Type", &ctype)],
        &body,
    );
    assert_eq!((st, status_code(&reply)), (200, 0), "{reply}");
    // An authenticated upload past the upload cap is refused too.
    let (st, head, _) = send_head(
        api.addr,
        &head_with_body(
            "POST",
            &format!("/file/upload?username=root&password={PW}"),
            &[("Content-Type", &ctype), ("Expect", "100-continue")],
            (1 << 20) + 1,
        ),
    );
    assert_eq!(st, 413, "{head}");
    // An operation that carries no payload is not an upload route.
    let (st, head, _) = send_head(
        api.addr,
        &head_with_body(
            "POST",
            &format!("/api?operation=check&username=root&password={PW}"),
            &[
                ("Content-Type", "application/json"),
                ("Expect", "100-continue"),
            ],
            100 << 10,
        ),
    );
    assert_eq!(st, 413, "{head}");
}

#[test]
fn a_refused_body_is_never_buffered_and_a_chunked_one_never_past_the_cap() {
    let api = start_api("peak", small_caps());
    let (st, _, _) = send_head(
        api.addr,
        &head_with_body(
            "POST",
            "/api",
            &[("Content-Type", "application/json")],
            100 << 10,
        ),
    );
    assert_eq!(st, 413);
    assert_eq!(gstore::preauth::body_budget_in_use(api.addr), Some((0, 0)));
    // Four 16 KiB chunks fill the 64 KiB cap; the fifth chunk's size line
    // is refused before its data is sent.
    let mut s = connect(api.addr);
    s.write_all(
        b"POST /api HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
          Transfer-Encoding: chunked\r\n\r\n",
    )
    .unwrap();
    let chunk = vec![b' '; 16 << 10];
    for _ in 0..4 {
        s.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .unwrap();
        s.write_all(&chunk).unwrap();
        s.write_all(b"\r\n").unwrap();
    }
    s.write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
        .unwrap();
    let (head, _) = read_response(&mut s);
    assert_eq!(status(&head), 413, "{head}");
    drop(s);
    assert!(gstore::preauth::wait_body_budget(
        api.addr,
        HANG_GUARD,
        |held| held == 0
    ));
    let (_, peak) = gstore::preauth::body_budget_in_use(api.addr).unwrap();
    // The body (64 KiB) and its chunk framing, never more.
    assert!((64 << 10..=(64 << 10) + 64).contains(&peak), "{peak}");
}

#[test]
fn the_body_budget_bounds_all_bodies_in_memory() {
    let opts = HttpOptions {
        preauth_max_body: 1 << 20,
        max_body: 1 << 20,
        body_budget: 1 << 20,
        ..HttpOptions::default()
    };
    let api = start_api("budget", opts);
    // A holds 768 KiB of a 1 MiB body in the server.
    let mut a = connect(api.addr);
    a.write_all(
        head_with_body(
            "POST",
            "/api",
            &[("Content-Type", "application/json")],
            1 << 20,
        )
        .as_bytes(),
    )
    .unwrap();
    a.write_all(&vec![b' '; 768 << 10]).unwrap();
    assert!(gstore::preauth::wait_body_budget(
        api.addr,
        HANG_GUARD,
        |held| held >= 768 << 10
    ));
    // B's 512 KiB do not fit next to them: refused before its body.
    let (st, head, body) = send_head(
        api.addr,
        &head_with_body(
            "POST",
            "/api",
            &[
                ("Content-Type", "application/json"),
                ("Expect", "100-continue"),
            ],
            512 << 10,
        ),
    );
    assert_eq!(st, 503, "{head}{body}");
    assert!(body.contains("request_body_budget"), "{body}");
    // Once A gives up, the budget is free again.
    drop(a);
    assert!(gstore::preauth::wait_body_budget(
        api.addr,
        HANG_GUARD,
        |held| held == 0
    ));
    let mut body = String::from(r#"{"operation":"check","pad":""#);
    body.push_str(&"x".repeat(500 << 10));
    body.push_str(r#""}"#);
    let (st, reply) = request(
        api.addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    );
    assert_eq!((st, status_code(&reply)), (200, 0), "{reply}");
    let (_, peak) = gstore::preauth::body_budget_in_use(api.addr).unwrap();
    assert!(peak <= 1 << 20, "{peak}");
}

#[test]
fn the_configured_caps_reach_the_servers() {
    let c = GStoreConfig::from_ini_str(
        "[upload]\nupload_max_body_size=4096\npreauth_max_body_size=1024\n\
         request_body_budget=65536\n",
    );
    assert!(c.warnings().is_empty(), "{:?}", c.warnings());
    let h = c.http_options();
    assert_eq!(
        (h.max_body, h.preauth_max_body, h.body_budget),
        (4096, 1024, 65536)
    );
    // The shipped defaults: 256 MiB uploads, 64 MiB before authentication,
    // 512 MiB in all.
    let d = GStoreConfig::default().http_options();
    assert_eq!(
        (d.max_body, d.preauth_max_body, d.body_budget),
        (256 << 20, 64 << 20, 512 << 20)
    );
}

// ---------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------

#[cfg(feature = "tls")]
mod tls {
    use super::*;

    // The self-signed pair of the `server.rs` TLS tests (CN=localhost, SAN
    // DNS:localhost + IP 127.0.0.1).
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

    fn server_config(dir: &TmpDir) -> Arc<rustls::ServerConfig> {
        let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
        fs::write(&cert, CERT_PEM).unwrap();
        fs::write(&key, KEY_PEM).unwrap();
        gstore::server::load_tls_config(&cert, &key).unwrap()
    }

    fn client(addr: SocketAddr) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut CERT_PEM.as_bytes()) {
            roots.add(cert.unwrap()).unwrap();
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let conn = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
        rustls::StreamOwned::new(conn, connect(addr))
    }

    fn send_head_tls(addr: SocketAddr, head: &str) -> u16 {
        let mut s = client(addr);
        s.write_all(head.as_bytes()).unwrap();
        s.flush().unwrap();
        let (h, _) = read_response(&mut s);
        status(&h)
    }

    #[test]
    fn the_tls_listeners_decide_the_caps_before_the_body_too() {
        let certs = TmpDir::new("tls_certs");
        let tls = server_config(&certs);
        let expect = ("Expect", "100-continue");

        let root = TmpDir::new("tls_api");
        let mut config = GStoreConfig::default();
        config.ip_deny_path = None;
        let api = Arc::new(
            ApiServer::bind(root.path(), PW, "127.0.0.1:0")
                .unwrap()
                .with_config(config)
                .with_http_options(small_caps()),
        );
        let addr = api.local_addr().unwrap();
        let (a2, t2) = (Arc::clone(&api), Arc::clone(&tls));
        let serving = thread::spawn(move || {
            a2.serve_forever_tls(t2);
        });
        let unauth = head_with_body("POST", "/api", &[expect], 100 << 10);
        assert_eq!(send_head_tls(addr, &unauth), 413);
        let authed = head_with_body(
            "POST",
            &format!("/file/upload?username=root&password={PW}"),
            &[expect],
            100 << 10,
        );
        assert_eq!(send_head_tls(addr, &authed), 100);
        api.shutdown();
        serving.join().unwrap();

        let db = Database::build_from_str("tls", "").unwrap();
        let single = Arc::new(
            Server::bind(db, "127.0.0.1:0")
                .unwrap()
                .with_basic_auth("u", "p")
                .with_http_options(small_caps()),
        );
        let addr = single.local_addr().unwrap();
        let s2 = Arc::clone(&single);
        let serving = thread::spawn(move || {
            s2.serve_forever_tls(tls);
        });
        let unauth = head_with_body("POST", "/update", &[expect], 100 << 10);
        assert_eq!(send_head_tls(addr, &unauth), 413);
        let authed = head_with_body(
            "POST",
            "/update",
            &[expect, ("Authorization", &basic("u", "p"))],
            100 << 10,
        );
        assert_eq!(send_head_tls(addr, &authed), 100);
        single.shutdown();
        serving.join().unwrap();
    }
}
