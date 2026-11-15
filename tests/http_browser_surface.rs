//! The browser attack surface of the HTTP endpoints, secure by default with
//! explicit `conf.ini` switches restoring upstream behaviour (review H-10:
//! http#2, design#2, sec#8, infra#3; sec#9 / gapi#22).
//!
//! * `GET /sparql` never runs a SPARQL UPDATE, and `/update` takes only an
//!   `application/sparql-update` or form body — unless
//!   `allow_get_update=on`.
//! * CORS headers go only to the origins listed in `cors_allow_origin`
//!   (empty by default; `*` restores upstream's `Access-Control-Allow-Origin:
//!   *`), and preflights follow the same list.
//! * A request that arrives over loopback must name a local host (an IP
//!   literal, `localhost`, a name without a dot, or one listed in
//!   `allowed_hosts`) — the DNS-rebinding guard; a state-changing request
//!   carrying a foreign `Origin` is refused. Clients that send no `Origin`
//!   (scripts, SDKs) are unaffected.
//! * A loopback peer that forwards `Forwarded` / `X-Forwarded-For` /
//!   `X-Real-IP` is a proxy, not a local client: it loses the loopback
//!   privileges (the unauthenticated single-database mode, root's default
//!   password, upstream's `trust_inner_localhost`) unless
//!   `trust_proxy_headers=on`.
//! * `gserver` in API mode refuses root's default password on every address
//!   unless `allow_default_root_password=on`.
//!
//! The servers are in-process on `127.0.0.1:0` (configured from `conf.ini`
//! text through `GStoreConfig`), plus one real `gserver`; every temp
//! directory is removed when the test ends, also when it fails.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::gapi::GapiConfig;
use gstore::http_api::ApiServer;
use gstore::server::Server;
use gstore::Database;

// ---- plumbing ---------------------------------------------------------------

/// A temp directory removed on drop (also while a failing test unwinds).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("http_surface_{tag}_{}_{n}", std::process::id()));
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

/// One response: status, header block (lowercased), body.
struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\n{}\n\n{}", self.status, self.head, self.body)
    }
}

impl Reply {
    fn acao(&self) -> Option<&str> {
        self.head
            .lines()
            .find_map(|l| l.strip_prefix("access-control-allow-origin:"))
            .map(str::trim)
    }

    /// The `/api` envelope's `StatusCode`.
    fn status_code(&self) -> i64 {
        let n = self
            .body
            .split("\"StatusCode\":")
            .nth(1)
            .unwrap_or_else(|| panic!("no StatusCode in {self:?}"));
        n.split(|c: char| c != '-' && !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }
}

/// Send one request and read the whole response (`Connection: close`). A
/// `("Host", …)` entry of `headers` replaces the default `Host: 127.0.0.1`;
/// `("Host", "")` sends none.
fn send(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\n");
    match headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("host")) {
        Some((_, "")) => {}
        Some((_, h)) => raw.push_str(&format!("Host: {h}\r\n")),
        None => raw.push_str("Host: 127.0.0.1\r\n"),
    }
    for (k, v) in headers
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("host"))
    {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    s.write_all(raw.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((resp.as_str(), ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {resp:?}"));
    Reply {
        status,
        head: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

const FORM: (&str, &str) = ("Content-Type", "application/x-www-form-urlencoded");
const SPARQL_UPDATE: (&str, &str) = ("Content-Type", "application/sparql-update");
const TEXT: (&str, &str) = ("Content-Type", "text/plain");
const EVIL: (&str, &str) = ("Origin", "http://evil.example");
/// Basic `root:pw`.
const ROOT: (&str, &str) = ("Authorization", "Basic cm9vdDpwdw==");

const ONE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";
const INSERT: &str = "INSERT DATA { <http://ex/new> <http://ex/p> <http://ex/b> }";

/// `conf.ini` text → the configuration the servers below run with.
fn config(ini: &str) -> GStoreConfig {
    let mut cfg = GStoreConfig::from_ini_str(ini);
    cfg.ip_deny_path = None;
    cfg.thread_num = 4;
    cfg
}

/// A single-database server (no auth, in memory) with the HTTP options of
/// `ini`, shut down on drop.
struct Single {
    server: Arc<Server>,
    addr: SocketAddr,
}

impl Single {
    fn start(ini: &str) -> Single {
        Single::start_with(ini, |s| s)
    }

    fn start_with(ini: &str, f: impl FnOnce(Server) -> Server) -> Single {
        let db = Database::build_from_str("single", ONE).unwrap();
        let server = Server::bind(db, "127.0.0.1:0")
            .unwrap()
            .with_http_options(config(ini).http_options());
        let server = Arc::new(f(server));
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        thread::spawn(move || s2.serve_forever());
        Single { server, addr }
    }

    fn triples(&self) -> u64 {
        let r = send(self.addr, "GET", "/status", &[], "");
        assert_eq!(r.status, 200, "{r:?}");
        let n = r.body.split("\"triples\":").nth(1).expect("triples");
        n.split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }
}

impl Drop for Single {
    fn drop(&mut self) {
        self.server.shutdown();
    }
}

/// A multi-database API server in a temp directory with root password
/// `password`, configured from `ini`; database `dba` holds one triple.
struct Api {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    _dir: TmpDir,
}

impl Api {
    fn start(tag: &str, password: &str, ini: &str) -> Api {
        Api::start_with(tag, password, ini, |s| s)
    }

    fn start_with(
        tag: &str,
        password: &str,
        ini: &str,
        f: impl FnOnce(ApiServer) -> ApiServer,
    ) -> Api {
        let dir = TmpDir::new(tag);
        let server = ApiServer::bind(dir.0.join("root"), password, "127.0.0.1:0")
            .unwrap()
            .with_config(config(ini));
        let server = Arc::new(f(server));
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        thread::spawn(move || s2.serve_forever());
        Api {
            server,
            addr,
            _dir: dir,
        }
    }

    /// `/api` with a JSON body.
    fn api(&self, headers: &[(&str, &str)], json: &str) -> Reply {
        let mut h = vec![("Content-Type", "application/json")];
        h.extend_from_slice(headers);
        send(self.addr, "POST", "/api", &h, json)
    }

    fn build_dba(&self, auth: (&str, &str)) {
        let r = send(self.addr, "POST", "/build?db_name=dba", &[auth], ONE);
        assert_eq!(r.status, 200, "build: {r:?}");
    }

    fn count_dba(&self, auth: (&str, &str)) -> u64 {
        let q = enc("SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }");
        let r = send(
            self.addr,
            "GET",
            &format!("/sparql?db_name=dba&query={q}&format=csv"),
            &[auth],
            "",
        );
        assert_eq!(r.status, 200, "{r:?}");
        r.body.lines().nth(1).unwrap().trim().parse().unwrap()
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.shutdown();
    }
}

// ---- GET never runs an UPDATE; /update takes typed bodies ----------------------

#[test]
fn get_never_runs_an_update_unless_the_compat_switch_is_on() {
    let srv = Single::start("");
    let r = send(
        srv.addr,
        "GET",
        &format!("/sparql?query={}", enc(INSERT)),
        &[],
        "",
    );
    assert_eq!(r.status, 400, "{r:?}");
    assert!(r.body.contains("allow_get_update"), "{r:?}");
    assert_eq!(srv.triples(), 1, "nothing was inserted");
    // A read over GET is fine, and so is the update over POST.
    let r = send(
        srv.addr,
        "GET",
        &format!("/sparql?query={}", enc("ASK {}")),
        &[],
        "",
    );
    assert_eq!(r.status, 200, "{r:?}");
    let r = send(srv.addr, "POST", "/update", &[SPARQL_UPDATE], INSERT);
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(srv.triples(), 2);

    let compat = Single::start("[server]\nallow_get_update=on\n");
    let r = send(
        compat.addr,
        "GET",
        &format!("/sparql?query={}", enc(INSERT)),
        &[],
        "",
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(compat.triples(), 2);

    // REST, authenticated: the same rule.
    let api = Api::start("get_update", "pw", "");
    api.build_dba(ROOT);
    let target = format!("/sparql?db_name=dba&query={}", enc(INSERT));
    let r = send(api.addr, "GET", &target, &[ROOT], "");
    assert_eq!(r.status, 400, "{r:?}");
    assert_eq!(api.count_dba(ROOT), 1);
    let r = send(
        api.addr,
        "POST",
        "/sparql?db_name=dba",
        &[ROOT, SPARQL_UPDATE],
        INSERT,
    );
    assert_eq!(
        r.status, 200,
        "an update POSTed to /sparql still runs: {r:?}"
    );
    assert_eq!(api.count_dba(ROOT), 2);
    let api2 = Api::start("get_update_compat", "pw", "[server]\nallow_get_update=on\n");
    api2.build_dba(ROOT);
    let r = send(api2.addr, "GET", &target, &[ROOT], "");
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(api2.count_dba(ROOT), 2);
}

#[test]
fn update_refuses_untyped_text_bodies_unless_the_compat_switch_is_on() {
    let srv = Single::start("");
    let r = send(srv.addr, "POST", "/update", &[TEXT], INSERT);
    assert_eq!(r.status, 415, "{r:?}");
    assert_eq!(srv.triples(), 1);
    let compat = Single::start("[server]\nallow_get_update=on\n");
    let r = send(compat.addr, "POST", "/update", &[TEXT], INSERT);
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(compat.triples(), 2);

    let api = Api::start("text_update", "pw", "");
    api.build_dba(ROOT);
    let r = send(
        api.addr,
        "POST",
        "/update?db_name=dba",
        &[ROOT, TEXT],
        INSERT,
    );
    assert_eq!(r.status, 415, "{r:?}");
    assert_eq!(api.count_dba(ROOT), 1);
    let api2 = Api::start(
        "text_update_compat",
        "pw",
        "[server]\nallow_get_update=on\n",
    );
    api2.build_dba(ROOT);
    let r = send(
        api2.addr,
        "POST",
        "/update?db_name=dba",
        &[ROOT, TEXT],
        INSERT,
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(api2.count_dba(ROOT), 2);
}

// ---- CORS ---------------------------------------------------------------------

#[test]
fn cors_headers_go_only_to_listed_origins() {
    // Default: no origin is listed, so no Access-Control-Allow-Origin at all.
    let srv = Single::start("");
    let r = send(srv.addr, "GET", "/status", &[EVIL], "");
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.acao(), None, "{r:?}");
    let r = send(srv.addr, "GET", "/status", &[], "");
    assert_eq!(r.acao(), None, "{r:?}");

    let listed =
        Single::start("[server]\ncors_allow_origin=https://app.example|http://tool.example:8080\n");
    let r = send(
        listed.addr,
        "GET",
        "/status",
        &[("Origin", "https://app.example")],
        "",
    );
    assert_eq!(r.acao(), Some("https://app.example"), "{r:?}");
    assert!(r.head.contains("vary: origin"), "{r:?}");
    let r = send(
        listed.addr,
        "GET",
        "/status",
        &[("Origin", "http://tool.example:8080")],
        "",
    );
    assert_eq!(r.acao(), Some("http://tool.example:8080"), "{r:?}");
    let r = send(listed.addr, "GET", "/status", &[EVIL], "");
    assert_eq!(r.acao(), None, "{r:?}");
    // A streamed SELECT carries the same header.
    let r = send(
        listed.addr,
        "GET",
        &format!(
            "/sparql?query={}&stream=true",
            enc("SELECT * WHERE { ?s ?p ?o }")
        ),
        &[("Origin", "https://app.example")],
        "",
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.acao(), Some("https://app.example"), "{r:?}");

    // `*`: upstream's header on every response.
    let star = Single::start("[server]\ncors_allow_origin=*\n");
    let r = send(star.addr, "GET", "/status", &[EVIL], "");
    assert_eq!(r.acao(), Some("*"), "{r:?}");
    let r = send(star.addr, "GET", "/status", &[], "");
    assert_eq!(r.acao(), Some("*"), "{r:?}");

    // The API server follows the same list.
    let api = Api::start("cors", "pw", "");
    let r = send(api.addr, "GET", "/show", &[ROOT, EVIL], "");
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.acao(), None, "{r:?}");
    let api2 = Api::start(
        "cors_listed",
        "pw",
        "[server]\ncors_allow_origin=https://app.example\n",
    );
    let r = send(
        api2.addr,
        "GET",
        "/show",
        &[ROOT, ("Origin", "https://app.example")],
        "",
    );
    assert_eq!(r.acao(), Some("https://app.example"), "{r:?}");
    let r = api2.api(
        &[("Origin", "https://app.example")],
        r#"{"operation":"check"}"#,
    );
    assert_eq!(r.acao(), Some("https://app.example"), "{r:?}");
}

#[test]
fn preflights_follow_the_origin_list() {
    let preflight = |addr, origin: Option<&str>| {
        let mut h = vec![("Access-Control-Request-Method", "POST")];
        if let Some(o) = origin {
            h.push(("Origin", o));
        }
        send(addr, "OPTIONS", "/sparql", &h, "")
    };
    let srv = Single::start("");
    let r = preflight(srv.addr, Some("http://evil.example"));
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(r.acao(), None, "{r:?}");
    assert!(!r.head.contains("access-control-allow-methods"), "{r:?}");
    // A preflight without Origin is no browser's: answered, without CORS.
    let r = preflight(srv.addr, None);
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.acao(), None, "{r:?}");

    let listed = Single::start("[server]\ncors_allow_origin=https://app.example\n");
    let r = preflight(listed.addr, Some("https://app.example"));
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.acao(), Some("https://app.example"), "{r:?}");
    assert!(r.head.contains("access-control-allow-methods"), "{r:?}");
    let r = preflight(listed.addr, Some("http://evil.example"));
    assert_eq!(r.status, 403, "{r:?}");

    let star = Single::start("[server]\ncors_allow_origin=*\n");
    let r = preflight(star.addr, Some("http://evil.example"));
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.acao(), Some("*"), "{r:?}");

    // CORS processing off: OPTIONS reaches the router like any method.
    let off = Single::start("[server]\ncors=off\ncors_allow_origin=*\n");
    let r = preflight(off.addr, Some("http://evil.example"));
    assert_eq!(r.status, 405, "{r:?}");
    assert_eq!(r.acao(), None, "{r:?}");
}

// ---- Host (DNS rebinding) and Origin (CSRF) -------------------------------------

#[test]
fn a_loopback_request_must_name_a_local_host() {
    let srv = Single::start("[server]\nallowed_hosts=gstore.example.org\n");
    let ask = format!("/sparql?query={}", enc("ASK {}"));
    for host in [
        "attacker.example",
        "attacker.example:9000",
        "127.0.0.1.attacker.example",
    ] {
        let r = send(srv.addr, "GET", &ask, &[("Host", host)], "");
        assert_eq!(r.status, 403, "Host {host}: {r:?}");
        assert!(r.body.contains("allowed_hosts"), "{r:?}");
    }
    let port = srv.addr.port().to_string();
    for host in [
        format!("127.0.0.1:{port}"),
        "127.0.0.1".to_string(),
        format!("localhost:{port}"),
        "LOCALHOST.".to_string(),
        "app.localhost".to_string(),
        format!("[::1]:{port}"),
        "192.168.1.20:9000".to_string(),
        "myhost".to_string(),
        "gstore.example.org:8443".to_string(),
        String::new(), // no Host header (an HTTP/1.0 client)
    ] {
        let r = send(srv.addr, "GET", &ask, &[("Host", host.as_str())], "");
        assert_eq!(r.status, 200, "Host {host:?}: {r:?}");
    }
    // The rebinding page cannot update either.
    let r = send(
        srv.addr,
        "POST",
        "/update",
        &[("Host", "attacker.example:9000"), SPARQL_UPDATE],
        INSERT,
    );
    assert_eq!(r.status, 403, "{r:?}");
    assert_eq!(srv.triples(), 1);

    let api = Api::start("host", "pw", "");
    let r = send(
        api.addr,
        "GET",
        "/show",
        &[ROOT, ("Host", "attacker.example")],
        "",
    );
    assert_eq!(r.status, 403, "{r:?}");
    // `/api` answers HTTP 200 with its envelope (1101: refused by policy).
    let r = api.api(
        &[("Host", "attacker.example")],
        r#"{"operation":"show","username":"root","password":"pw"}"#,
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.status_code(), 1101, "{r:?}");
    let r = api.api(
        &[],
        r#"{"operation":"show","username":"root","password":"pw"}"#,
    );
    assert_eq!(r.status_code(), 0, "{r:?}");
}

#[test]
fn a_foreign_origin_cannot_change_state() {
    let srv = Single::start("[server]\ncors_allow_origin=https://app.example\n");
    let form = format!("update={}", enc(INSERT));
    for origin in [
        "http://evil.example",
        "null",
        "http://127.0.0.1.evil.example",
    ] {
        let r = send(
            srv.addr,
            "POST",
            "/update",
            &[FORM, ("Origin", origin)],
            &form,
        );
        assert_eq!(r.status, 403, "Origin {origin}: {r:?}");
    }
    assert_eq!(srv.triples(), 1);
    // A read over GET from a foreign page is answered, but carries no CORS
    // header, so the page cannot read it.
    let r = send(
        srv.addr,
        "GET",
        &format!("/sparql?query={}", enc("ASK {}")),
        &[EVIL],
        "",
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.acao(), None);
    // No Origin (curl, the SDKs, the upstream scripts): unaffected.
    let r = send(srv.addr, "POST", "/update", &[FORM], &form);
    assert_eq!(r.status, 200, "{r:?}");
    // The server's own console (same origin) and a listed origin may write.
    let host = format!("127.0.0.1:{}", srv.addr.port());
    let same = format!("http://{host}");
    let r = send(
        srv.addr,
        "POST",
        "/update",
        &[("Host", host.as_str()), ("Origin", same.as_str()), FORM],
        &format!(
            "update={}",
            enc("INSERT DATA { <http://ex/s2> <http://ex/p> 2 }")
        ),
    );
    assert_eq!(r.status, 200, "{r:?}");
    let r = send(
        srv.addr,
        "POST",
        "/update",
        &[("Origin", "https://app.example"), FORM],
        &format!(
            "update={}",
            enc("INSERT DATA { <http://ex/s3> <http://ex/p> 3 }")
        ),
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.acao(), Some("https://app.example"));
    assert_eq!(srv.triples(), 4);

    // The API server: REST and `/api` alike (a text/plain JSON body is a
    // CORS "simple request": no preflight protects it).
    let api = Api::start("origin", "pw", "");
    api.build_dba(ROOT);
    let r = send(
        api.addr,
        "POST",
        "/update?db_name=dba",
        &[ROOT, SPARQL_UPDATE, EVIL],
        INSERT,
    );
    assert_eq!(r.status, 403, "{r:?}");
    let json = format!(
        r#"{{"operation":"query","db_name":"dba","username":"root","password":"pw","sparql":"{INSERT}"}}"#
    );
    let r = send(api.addr, "POST", "/api", &[TEXT, EVIL], &json);
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(r.status_code(), 1101, "{r:?}");
    assert_eq!(api.count_dba(ROOT), 1, "nothing was inserted");
    let r = send(api.addr, "POST", "/api", &[TEXT], &json);
    assert_eq!(r.status_code(), 0, "without Origin: {r:?}");
    assert_eq!(api.count_dba(ROOT), 2);
}

// ---- proxies: loopback privileges ---------------------------------------------

const PROXIED: [(&str, &str); 3] = [
    ("X-Forwarded-For", "203.0.113.9"),
    ("Forwarded", "for=203.0.113.9;proto=https"),
    ("X-Real-IP", "203.0.113.9"),
];

#[test]
fn a_proxy_does_not_get_the_unauthenticated_single_database_mode() {
    let srv = Single::start("");
    let ask = format!("/sparql?query={}", enc("ASK {}"));
    for h in PROXIED {
        let r = send(srv.addr, "GET", &ask, &[h], "");
        assert_eq!(r.status, 403, "{h:?}: {r:?}");
        assert!(r.body.contains("trust_proxy_headers"), "{r:?}");
        let r = send(srv.addr, "POST", "/update", &[h, SPARQL_UPDATE], INSERT);
        assert_eq!(r.status, 403, "{h:?}: {r:?}");
    }
    assert_eq!(srv.triples(), 1);
    let r = send(srv.addr, "GET", &ask, &[], "");
    assert_eq!(r.status, 200, "a local client: {r:?}");

    let trusting = Single::start("[server]\ntrust_proxy_headers=on\n");
    let r = send(trusting.addr, "GET", &ask, &[PROXIED[0]], "");
    assert_eq!(r.status, 200, "{r:?}");

    // A server with credentials does not rely on loopback: proxied requests
    // with good credentials are served.
    let authed = Single::start_with("", |s| s.with_basic_auth("u", "p"));
    let r = send(
        authed.addr,
        "GET",
        &ask,
        &[PROXIED[0], ("Authorization", "Basic dTpw")],
        "",
    );
    assert_eq!(r.status, 200, "{r:?}");
}

#[test]
fn a_proxy_does_not_get_the_default_root_password() {
    let api = Api::start("defpw_proxy", "123456", "");
    let basic = ("Authorization", "Basic cm9vdDoxMjM0NTY="); // root:123456
    let login = r#"{"operation":"login","username":"root","password":"123456"}"#;
    // A local client may use it (gserver itself refuses to start with it
    // unless allow_default_root_password=on; see the gserver test below).
    let r = send(api.addr, "GET", "/show", &[basic], "");
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(api.api(&[], login).status_code(), 0);
    let md5 = r#"{"operation":"login","username":"root","password":"e10adc3949ba59abbe56e057f20f883e","encryption":"1"}"#;
    assert_eq!(api.api(&[], md5).status_code(), 0);
    // A proxied one may not: not over REST Basic, REST /login or /api (in
    // either password form).
    for h in PROXIED {
        let r = send(api.addr, "GET", "/show", &[basic, h], "");
        assert_eq!(r.status, 401, "{h:?}: {r:?}");
        let r = send(
            api.addr,
            "POST",
            "/login",
            &[FORM, h],
            "username=root&password=123456",
        );
        assert_eq!(r.status, 401, "{h:?}: {r:?}");
        let r = api.api(&[h], login);
        assert_eq!(r.status_code(), 1001, "{h:?}: {r:?}");
        assert!(r.body.contains("default"), "{r:?}");
        assert_eq!(api.api(&[h], md5).status_code(), 1001, "{h:?}");
    }
    let trusting = Api::start(
        "defpw_trust",
        "123456",
        "[server]\ntrust_proxy_headers=on\n",
    );
    let r = send(trusting.addr, "GET", "/show", &[basic, PROXIED[0]], "");
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(trusting.api(&[PROXIED[0]], login).status_code(), 0);
    // A changed password works through the proxy.
    let changed = Api::start("defpw_changed", "s3cret", "");
    let r = send(
        changed.addr,
        "GET",
        "/show",
        &[("Authorization", "Basic cm9vdDpzM2NyZXQ="), PROXIED[0]],
        "",
    );
    assert_eq!(r.status, 200, "{r:?}");
}

#[test]
fn a_proxy_does_not_get_trust_inner_localhost() {
    let inner =
        r#"{"operation":"show","username":"root","password":"not-the-password","inner":"true"}"#;
    let api = Api::start_with("inner", "pw", "", |s| {
        s.with_gapi_config(GapiConfig {
            trust_inner_localhost: true,
            ..GapiConfig::default()
        })
    });
    assert_eq!(
        api.api(&[], inner).status_code(),
        0,
        "a local inner request"
    );
    for h in PROXIED {
        let r = api.api(&[h], inner);
        assert_eq!(r.status_code(), 1001, "{h:?}: {r:?}");
    }
    let trusting = Api::start_with(
        "inner_trust",
        "pw",
        "[server]\ntrust_proxy_headers=on\n",
        |s| {
            s.with_gapi_config(GapiConfig {
                trust_inner_localhost: true,
                ..GapiConfig::default()
            })
        },
    );
    assert_eq!(trusting.api(&[PROXIED[0]], inner).status_code(), 0);
}

// ---- gserver: root's default password -------------------------------------------

/// Kills the background gserver its pid file names (a failing test leaves
/// none running).
struct KillOnDrop(PathBuf);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
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

fn gserver(cwd: &Path, args: &[&str]) -> (i32, String, String) {
    // `-b` returns once the background server answers or has failed.
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
fn gserver_api_mode_refuses_the_default_root_password_on_loopback_too() {
    let dir = TmpDir::new("gserver_defpw");
    let home = dir.0.join("home");
    let home_s = home.to_string_lossy().into_owned();
    let _kill = KillOnDrop(home.join("gserver.pid"));
    // A new user store would be seeded with 123456: refused, on 127.0.0.1.
    let (code, out, err) = gserver(&dir.0, &["-b", "--api", &home_s, "-p", "0"]);
    assert_eq!(code, 1, "stdout:{out}\nstderr:{err}");
    assert!(err.contains("refusing to start"), "{err}");
    assert!(
        err.contains("--password-file"),
        "names the first remedy: {err}"
    );
    assert!(
        err.contains("allow_default_root_password=on"),
        "names the second remedy: {err}"
    );
    assert!(!home.join("users.db").exists(), "no store was left behind");

    // An existing store whose root still has it: refused too.
    drop(ApiServer::bind(&home, "123456", "127.0.0.1:0").unwrap());
    assert!(home.join("users.db").exists());
    let (code, out, err) = gserver(&dir.0, &["-b", "--api", &home_s, "-p", "0"]);
    assert_eq!(code, 1, "stdout:{out}\nstderr:{err}");
    assert!(err.contains("refusing to start"), "{err}");

    // The compat switch starts it (on loopback), and says so in the log.
    let conf = dir.0.join("compat.ini");
    std::fs::write(&conf, "[server]\nallow_default_root_password=on\n").unwrap();
    let conf_s = conf.to_string_lossy().into_owned();
    let (code, out, err) = gserver(&dir.0, &["-b", "--api", &home_s, "-p", "0", "-c", &conf_s]);
    assert_eq!(code, 0, "stdout:{out}\nstderr:{err}");
    let log = std::fs::read_to_string(home.join("gserver.log")).unwrap_or_default();
    assert!(
        log.contains("upstream-compat: allow_default_root_password=on"),
        "{log}"
    );
    let (code, _, err) = gserver(&dir.0, &["-t", "--api", &home_s]);
    assert_eq!(code, 0, "{err}");
}
