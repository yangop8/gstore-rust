//! Outbound `SERVICE` safety (review package r1-crash-dos):
//!
//! - a hostile endpoint's reply cannot abort the process: the SPARQL-results
//!   JSON of a `SERVICE` reply is read by the depth-limited [`gstore::json`]
//!   parser (review H-3: sec#5, gapi#3, robust#1), which also decodes
//!   surrogate-pair escapes correctly (gapi#10, robust#8);
//! - an endpoint IRI with whitespace or control characters (`\u000D\u000A`
//!   inside an IRIREF decodes to CR LF) is refused before anything is sent,
//!   so it cannot inject request headers (M8: sec#6, robust#9);
//! - on the policy `gserver --allow-service` sets up, `SERVICE` does not
//!   reach a host-local address (loopback, link-local, …) unless the policy
//!   allows host-local hosts (`gserver --allow-local-hosts`); the library's
//!   permissive default keeps reaching them (M8, tests-b#1);
//! - [`gstore::http_client::is_local_addr`] covers the special-purpose
//!   ranges a client must not make the server contact (sec#13).

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use gstore::concurrent::ConcurrentDb;
use gstore::db::LoadPolicy;
use gstore::http_api::ApiServer;
use gstore::http_client::{is_local_addr, parse_sparql_json, sparql_post};
use gstore::model::Term;
use gstore::Database;

const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";

/// A temporary directory, removed when dropped (also when a test fails).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_svc_hardening_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Read one HTTP request (head and `Content-Length` body) from `s`.
fn read_request(s: &mut TcpStream) -> String {
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut req = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&req[..end]).to_ascii_lowercase();
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if req.len() >= end + 4 + len {
                break;
            }
        }
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => req.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&req).into_owned()
}

/// An endpoint on loopback answering each of the next `n` connections with
/// `body` as a `200` SPARQL-results reply. Every request it received is sent
/// on the returned channel.
fn endpoint(body: String, n: usize) -> (SocketAddr, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for _ in 0..n {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            let req = read_request(&mut s);
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/sparql-results+json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = s.write_all(body.as_bytes());
            let _ = tx.send(req);
        }
    });
    (addr, rx)
}

/// A listener nothing is expected to reach: [`assert_untouched`] checks that
/// no connection is waiting in its accept queue. (A client's `connect`
/// completes in the kernel before the query that made it returns, so a
/// synchronous query that had connected would have left one there.)
fn trap() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    (listener, addr)
}

fn assert_untouched(listener: &TcpListener) {
    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok((mut s, _)) => {
            s.set_nonblocking(false).unwrap();
            panic!("a connection reached the trap: {:?}", read_request(&mut s))
        }
        Err(e) => panic!("accept: {e}"),
    }
}

/// The policy `gserver --allow-service` sets up (`load_policy_for`).
fn allow_service_policy() -> LoadPolicy {
    let mut policy = LoadPolicy::restricted();
    policy.allow_remote = true;
    policy
}

/// The JSON / SPARQL escape `\u` + `hex`, built at run time.
fn uesc(hex: &str) -> String {
    format!("{}u{hex}", '\\')
}

fn one_binding(var: &str, value_json: &str) -> String {
    format!(
        r#"{{"head":{{"vars":["{var}"]}},"results":{{"bindings":[{{"{var}":{{"type":"literal","value":"{value_json}"}}}}]}}}}"#
    )
}

// ---- the reply parser (H-3, gapi#10) --------------------------------------------

/// The reply parser refuses absurd nesting with an error instead of
/// recursing until the stack overflows (before: a stack-overflow abort of
/// the whole process at about 50 KB of `[` on an 8 MiB stack).
#[test]
fn a_deeply_nested_reply_is_a_parse_error() {
    for open in ["[", "{\"a\":"] {
        let mut body = String::from(r#"{"head":{"vars":["s"]},"results":{"bindings":"#);
        body.push_str(&open.repeat(100_000));
        let err = parse_sparql_json(&body).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{err}");
        assert!(err.to_string().contains("too deep"), "{err}");
    }
}

/// A `SERVICE` endpoint answering 100 000 nested `[` cannot abort the
/// process; the query run on a thread the size of a release server worker
/// (8 MiB) fails instead (before: SIGABRT, exit 134). A non-`SILENT`
/// endpoint failure still answers "no solution" (review M14 is a later
/// package's); either way nothing from the hostile reply is bound.
#[test]
fn a_service_endpoint_replying_100k_nested_brackets_does_not_abort() {
    let mut body = String::from(r#"{"head":{"vars":["s"]},"results":{"bindings":"#);
    body.push_str(&"[".repeat(100_000));
    let (addr, requests) = endpoint(body, 1);
    let result = thread::Builder::new()
        .stack_size(8 << 20)
        .spawn(move || {
            let mut db = Database::build_from_str("svc_deep", DATA).unwrap();
            db.select(&format!(
                "SELECT * WHERE {{ SERVICE <http://{addr}/sparql> {{ ?s ?p ?o }} }}"
            ))
        })
        .unwrap()
        .join()
        .expect("the query thread must not panic");
    match result {
        Err(e) => assert!(!e.to_string().is_empty()),
        Ok(rs) => assert!(rs.rows.is_empty(), "{:?}", rs.rows),
    }
    // The endpoint was asked (the policy allows loopback in the library),
    // naming its port in `Host` (it is not the default one).
    let req = requests.recv_timeout(Duration::from_secs(30)).unwrap();
    assert!(req.starts_with("POST /sparql HTTP/1.1\r\n"), "{req}");
    assert!(
        req.contains(&format!("\r\nHost: 127.0.0.1:{}\r\n", addr.port())),
        "{req}"
    );
}

/// The JSON escape pair D83D DE00 is one non-BMP character (U+1F600), not
/// two U+FFFD; a lone surrogate is malformed.
#[test]
fn surrogate_pair_escapes_in_service_results_decode() {
    let grin = uesc("d83d") + &uesc("de00");
    let sols = parse_sparql_json(&one_binding("x", &format!("{grin} ok"))).unwrap();
    assert_eq!(sols[0][0].1, Term::plain_literal("\u{1F600} ok"));
    let grin_upper = uesc("D83D") + &uesc("DE00");
    let sols = parse_sparql_json(&one_binding("x", &grin_upper)).unwrap();
    assert_eq!(sols[0][0].1, Term::plain_literal("\u{1F600}"));
    for lone in [uesc("d83d"), uesc("de00"), uesc("d83d") + "x"] {
        assert!(
            parse_sparql_json(&one_binding("x", &lone)).is_err(),
            "{lone}"
        );
    }
}

/// End to end: the decoded character joins with local data holding it
/// (before: the SERVICE binding was U+FFFD U+FFFD and the join was empty).
#[test]
fn a_surrogate_pair_from_a_service_endpoint_joins_with_local_data() {
    let grin = uesc("d83d") + &uesc("de00");
    let (addr, _requests) = endpoint(one_binding("name", &format!("Grin {grin}")), 1);
    let mut db = Database::build_from_str(
        "svc_surrogate",
        "<http://ex/alice> <http://ex/name> \"Grin \u{1F600}\" .\n",
    )
    .unwrap();
    let rs = db
        .select(&format!(
            "SELECT ?who WHERE {{ ?who <http://ex/name> ?name . \
             SERVICE <http://{addr}/sparql> {{ ?x <http://ex/name> ?name }} }}"
        ))
        .unwrap();
    assert_eq!(
        rs.rows,
        vec![vec![Some("<http://ex/alice>".to_string())]],
        "{rs:?}"
    );
}

// ---- endpoint IRIs (M8: CRLF injection) ---------------------------------------

/// The SPARQL lexer decodes `\u000D\u000A` inside an IRIREF to CR LF; such
/// an endpoint IRI is refused before connecting, so no header can be
/// injected into the outbound request (before: the request carried
/// `X-Injected: yes` as a header of its own).
#[test]
fn crlf_in_a_service_endpoint_iri_is_refused_before_connecting() {
    let (trap, addr) = trap();
    let mut db = Database::build_from_str("svc_crlf", DATA).unwrap();
    for iri in [
        format!(
            "http://{addr}/x{}{}X-Injected:{}yes",
            uesc("000D"),
            uesc("000A"),
            uesc("0020")
        ),
        format!("http://{addr}/x{}HTTP/1.0", uesc("0020")),
        format!("http://{addr}/x{}y", uesc("0009")),
        format!("http://{addr}/x{}y", uesc("007F")),
    ] {
        for silent in ["", "SILENT "] {
            let q = format!("SELECT * WHERE {{ SERVICE {silent}<{iri}> {{ ?s ?p ?o }} }}");
            if let Ok(rs) = db.select(&q) {
                assert!(rs.rows.iter().flatten().all(Option::is_none), "{q}: {rs:?}");
            }
            assert_untouched(&trap);
        }
    }
}

/// The public client refuses whitespace and control characters anywhere in
/// a `SERVICE` URL (path or host) as malformed input, without connecting.
#[test]
fn sparql_post_refuses_whitespace_and_control_characters() {
    let (trap, addr) = trap();
    for url in [
        format!("http://{addr}/x\r\nX-Injected: yes"),
        format!("http://{addr}/a b"),
        format!("http://{addr}/a\tb"),
        format!("http://{addr}/a\u{0}b"),
        format!("http://{addr}/a\u{85}b"),
        format!("http://127.0.0.1\r\nX: y:{}/x", addr.port()),
    ] {
        let err = sparql_post(&url, "ASK {}").unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "{url:?}: {err}"
        );
        assert_untouched(&trap);
    }
}

// ---- host-local endpoints (M8, tests-b#1) --------------------------------------

/// Under `gserver --allow-service`'s policy a loopback endpoint is refused
/// on every evaluation path — `Database` reads and updates, `ConcurrentDb`
/// snapshots and version views — with an error naming the reason, `SILENT`
/// or not, and nothing connects to it. With host-local hosts allowed (or on
/// the library's permissive default) it is reached.
#[test]
fn service_refuses_host_local_endpoints_under_the_allow_service_policy() {
    let (trap, addr) = trap();
    let queries = [
        format!("SELECT ?x WHERE {{ SERVICE <http://{addr}/admin> {{ ?x ?y ?z }} }}"),
        format!("SELECT ?x WHERE {{ SERVICE SILENT <http://{addr}/admin> {{ ?x ?y ?z }} }}"),
        format!(
            "SELECT ?x WHERE {{ SERVICE <http://localhost:{}/admin> {{ ?x ?y ?z }} }}",
            addr.port()
        ),
        format!("ASK {{ ?s ?p ?o FILTER EXISTS {{ SERVICE <http://{addr}/a> {{ ?x ?y ?z }} }} }}"),
    ];
    let refused = |r: gstore::Result<gstore::query::QueryResult>, what: &str| match r {
        Err(e) => assert!(e.to_string().contains("host-local"), "{what}: {e}"),
        Ok(r) => panic!("{what}: answered {r:?}"),
    };
    let mut db = Database::build_from_str("svc_local", DATA).unwrap();
    db.set_load_policy(allow_service_policy());
    for q in &queries {
        refused(db.query(q), q);
        assert_untouched(&trap);
    }
    let update = format!(
        "INSERT {{ <http://ex/leak> <http://ex/p> ?x }} WHERE \
         {{ SERVICE <http://{addr}/admin> {{ ?x ?y ?z }} }}"
    );
    refused(db.query(&update), &update);
    assert_untouched(&trap);
    let cdb = ConcurrentDb::new(db);
    for q in &queries {
        refused(cdb.snapshot().query(q), q);
        refused(cdb.version_view().query(q), q);
        assert_untouched(&trap);
    }
    drop(trap);

    // Host-local hosts allowed (`--allow-local-hosts`): the endpoint answers.
    let mut policy = allow_service_policy();
    policy.allow_local_hosts = true;
    for p in [policy, LoadPolicy::permissive()] {
        let (addr, _requests) = endpoint(one_binding("x", "internal"), 1);
        let mut db = Database::build_from_str("svc_local_ok", DATA).unwrap();
        db.set_load_policy(p);
        let rs = db
            .select(&format!(
                "SELECT ?x WHERE {{ SERVICE <http://{addr}/admin> {{ ?x ?y ?z }} }}"
            ))
            .unwrap();
        assert_eq!(rs.rows, vec![vec![Some("\"internal\"".to_string())]]);
    }
}

/// One request to `addr` with root's Basic credentials (`root:root`).
fn request(addr: &SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Basic cm9vdDpyb290\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {resp:?}"));
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

/// The REST server on `--allow-service`'s policy: `/sparql` and a read
/// answered from an open transaction's snapshot both refuse a loopback
/// endpoint.
#[test]
fn the_rest_server_refuses_host_local_service_endpoints() {
    let (trap, addr) = trap();
    let dir = TempDir::new("rest");
    let server = Arc::new(
        ApiServer::bind(dir.0.join("root"), "root", "127.0.0.1:0")
            .unwrap()
            .with_load_policy(allow_service_policy()),
    );
    let api = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    let (s, b) = request(&api, "POST", "/build?db_name=d", DATA);
    assert_eq!(s, 200, "{b}");
    let q = format!("SELECT ?x WHERE {{ SERVICE <http://{addr}/admin> {{ ?x ?y ?z }} }}");
    let (s, b) = request(&api, "POST", "/sparql?db_name=d", &q);
    assert_eq!(s, 400, "{b}");
    assert!(b.contains("host-local"), "{b}");
    let (s, b) = request(&api, "POST", "/begin?db_name=d", "");
    assert_eq!(s, 200, "{b}");
    let tid = b
        .split("\"TID\":\"")
        .nth(1)
        .and_then(|x| x.split('"').next())
        .unwrap()
        .to_string();
    let (s, b) = request(&api, "POST", "/sparql?db_name=d", &q);
    assert_eq!(s, 400, "snapshot read: {b}");
    assert!(b.contains("host-local"), "{b}");
    let (s, b) = request(&api, "POST", &format!("/tquery?tid={tid}"), &q);
    assert_eq!(s, 400, "{b}");
    assert_eq!(
        request(&api, "POST", &format!("/rollback?tid={tid}"), "").0,
        200
    );
    assert_untouched(&trap);
    server.shutdown();
    let _ = serving.join();
}

// ---- is_local_addr (sec#13) -------------------------------------------------------

#[test]
fn is_local_addr_covers_special_purpose_ranges() {
    let ip = |s: &str| -> IpAddr { s.parse().unwrap() };
    for local in [
        // this host, its link (cloud metadata), multicast, broadcast
        "127.0.0.1",
        "0.0.0.0",
        "0.1.2.3",
        "169.254.169.254",
        "224.0.0.1",
        "255.255.255.255",
        // shared address space (RFC 6598; e.g. a cloud metadata service at
        // 100.100.100.200), benchmarking (RFC 2544), reserved 240/4
        "100.64.0.1",
        "100.100.100.200",
        "100.127.255.255",
        "198.18.0.1",
        "198.19.255.255",
        "240.0.0.1",
        // IPv6: loopback, unspecified, link-local, unique-local (an IPv6
        // cloud metadata endpoint lives at fd00:ec2::254), site-local
        "::1",
        "::",
        "fe80::1",
        "fc00::1",
        "fd00:ec2::254",
        "fec0::1",
        "ff02::1",
        // IPv4 embedded in IPv6: mapped, compatible (deprecated, never a
        // public endpoint), NAT64 and 6to4 forms of a local IPv4 address
        "::ffff:127.0.0.1",
        "::ffff:169.254.169.254",
        "::ffff:100.100.100.200",
        "::127.0.0.1",
        "::8.8.8.8",
        "64:ff9b::7f00:1",
        "64:ff9b::a9fe:a9fe",
        "2002:7f00:1::1",
        "2002:a9fe:a9fe::1",
    ] {
        assert!(is_local_addr(&ip(local)), "{local} must be host-local");
    }
    for remote in [
        // Internal hosts stay reachable by design (docs/DEPLOY.md).
        "10.0.0.1",
        "172.16.0.1",
        "192.168.1.2",
        "100.63.255.255",
        "100.128.0.0",
        "198.17.255.255",
        "198.20.0.0",
        "8.8.8.8",
        "2001:db8::1",
        "2606:4700::1111",
        "::ffff:8.8.8.8",
        "64:ff9b::808:808",
        "2002:808:808::1",
    ] {
        assert!(!is_local_addr(&ip(remote)), "{remote} is not host-local");
    }
}
