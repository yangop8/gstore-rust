//! Where the HTTP endpoints take their parameters from (review H-9:
//! design#1, http#5; http#4; gap-doc-claims#2).
//!
//! * Only a form body (`application/x-www-form-urlencoded`, or a body with
//!   no `Content-Type` that is shaped like a form) supplies parameters. A
//!   `application/sparql-query` / `application/sparql-update` body — or any
//!   other raw body — is taken verbatim: an IRI or literal inside it that
//!   contains `&update=`, `&query=`, `&sparql=` or `&db_name=` never runs
//!   another statement or switches the database (H-9).
//! * A session token is read from the request head (or the URL), never by
//!   scanning the body before the caller is authenticated (http#4).
//! * The SPARQL Protocol's dataset parameters (`default-graph-uri`,
//!   `named-graph-uri`, `using-graph-uri`, `using-named-graph-uri`) are
//!   refused with `400`: the endpoints have no dataset support, and must not
//!   answer over the whole default graph instead (gap-doc-claims#2).
//!
//! Every server here is in-process on `127.0.0.1:0`; the API servers live in
//! temp directories removed when the test ends (also when it fails).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
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
            .join(format!("http_semantics_{tag}_{}_{n}", std::process::id()));
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

/// One response: status and body.
struct Reply {
    status: u16,
    body: String,
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.body)
    }
}

/// Send one request (`headers` are extra header lines) and read the whole
/// response (`Connection: close`).
fn send(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
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
        body: body.to_string(),
    }
}

/// `application/x-www-form-urlencoded` percent-encoding of a value.
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

const SPARQL_UPDATE: (&str, &str) = ("Content-Type", "application/sparql-update");
const SPARQL_QUERY: (&str, &str) = ("Content-Type", "application/sparql-query");
const FORM: (&str, &str) = ("Content-Type", "application/x-www-form-urlencoded");
/// Basic `root:pw`.
const ROOT: (&str, &str) = ("Authorization", "Basic cm9vdDpwdw==");

/// A single-database server (no auth, in memory, default options), shut
/// down on drop.
struct Single {
    server: Arc<Server>,
    addr: SocketAddr,
}

impl Single {
    fn start(data: &str) -> Single {
        let db = Database::build_from_str("single", data).unwrap();
        let server = Arc::new(Server::bind(db, "127.0.0.1:0").unwrap());
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

    fn count(&self, pattern: &str) -> u64 {
        let q = format!("SELECT (COUNT(*) AS ?n) WHERE {{ {pattern} }}");
        let r = send(
            self.addr,
            "GET",
            &format!("/sparql?query={}&format=csv", enc(&q)),
            &[],
            "",
        );
        assert_eq!(r.status, 200, "{r:?}");
        r.body.lines().nth(1).unwrap().trim().parse().unwrap()
    }
}

impl Drop for Single {
    fn drop(&mut self) {
        self.server.shutdown();
    }
}

/// A multi-database API server (root password `pw`) in a temp directory,
/// with databases `dba` (one triple) and `dbb` (two triples).
struct Api {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    _dir: TmpDir,
}

impl Api {
    fn start(tag: &str) -> Api {
        let dir = TmpDir::new(tag);
        let mut cfg = GStoreConfig::default();
        cfg.ip_deny_path = None;
        cfg.thread_num = 4;
        let server = ApiServer::bind(dir.0.join("root"), "pw", "127.0.0.1:0")
            .unwrap()
            .with_config(cfg);
        let server = Arc::new(server);
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        thread::spawn(move || s2.serve_forever());
        let api = Api {
            server,
            addr,
            _dir: dir,
        };
        api.build("dba", "<http://ex/a1> <http://ex/p> <http://ex/o> .\n");
        api.build(
            "dbb",
            "<http://ex/b1> <http://ex/p> <http://ex/o> .\n<http://ex/b2> <http://ex/p> <http://ex/o> .\n",
        );
        api
    }

    fn build(&self, name: &str, data: &str) {
        let r = send(
            self.addr,
            "POST",
            &format!("/build?db_name={name}"),
            &[ROOT],
            data,
        );
        assert_eq!(r.status, 200, "build {name}: {r:?}");
    }

    fn count(&self, db: &str) -> u64 {
        let q = "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }";
        let r = send(
            self.addr,
            "GET",
            &format!("/sparql?db_name={db}&query={}&format=csv", enc(q)),
            &[ROOT],
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

const ONE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";

// ---- H-9 / http#5: raw SPARQL bodies are taken verbatim -----------------------

/// The review's repro: an `application/sparql-update` body whose IRI holds
/// `&update=CLEAR%20DEFAULT` used to run `CLEAR DEFAULT` instead of the
/// INSERT (and answer `{"changed":1}`).
#[test]
fn a_raw_sparql_update_body_is_never_parsed_as_a_form() {
    let srv = Single::start(ONE);
    let update = "INSERT DATA { <http://ex/b> <http://ex/link> \
                  <http://ex/x?a=1&update=CLEAR%20DEFAULT&b=2> }";
    let r = send(srv.addr, "POST", "/update", &[SPARQL_UPDATE], update);
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.body.contains("\"changed\":1"), "{r:?}");
    assert_eq!(
        srv.triples(),
        2,
        "the triple is inserted and nothing is cleared"
    );
    assert_eq!(
        srv.count("<http://ex/b> <http://ex/link> <http://ex/x?a=1&update=CLEAR%20DEFAULT&b=2>"),
        1
    );
    // A literal holding `&update=` (http#5: used to fail with a parse error
    // on the fragment after `update=`).
    let update = "INSERT DATA { <http://ex/doc> <http://ex/url> \"http://x.org/s?a=1&update=2\" }";
    let r = send(srv.addr, "POST", "/update", &[SPARQL_UPDATE], update);
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(srv.triples(), 3);
}

#[test]
fn a_raw_sparql_query_body_is_never_parsed_as_a_form() {
    let srv = Single::start(ONE);
    let q = "SELECT ?s WHERE { ?s ?p ?o . FILTER(?o != <http://ex/search?lang=en&query=foo>) }";
    let r = send(
        srv.addr,
        "POST",
        "/sparql",
        &[SPARQL_QUERY, ("Accept", "text/csv")],
        q,
    );
    assert_eq!(r.status, 200, "used to run the fragment 'foo': {r:?}");
    assert!(r.body.contains("http://ex/a"), "{r:?}");
    // The same with no Content-Type at all: a body that is not shaped like a
    // form (it has spaces and braces) is the statement.
    let r = send(srv.addr, "POST", "/sparql", &[("Accept", "text/csv")], q);
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.body.contains("http://ex/a"), "{r:?}");
    // And a raw update body with no Content-Type: inserted verbatim.
    let update = "INSERT DATA { <http://ex/c> <http://ex/l> <http://ex/x?q=1&update=CLEAR%20ALL> }";
    let r = send(srv.addr, "POST", "/update", &[], update);
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(srv.triples(), 2);
}

/// Form bodies keep working: the console's `query=` form, `update=` forms,
/// and the legacy unlabelled form (`curl --data-urlencode` without a type is
/// labelled; hand-made clients sometimes send none).
#[test]
fn form_bodies_still_supply_the_statement() {
    let srv = Single::start(ONE);
    let body = format!(
        "update={}",
        enc("INSERT DATA { <http://ex/f> <http://ex/p> \"a&update=CLEAR ALL\" }")
    );
    let r = send(srv.addr, "POST", "/update", &[FORM], &body);
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(srv.triples(), 2);
    let body = format!(
        "update={}",
        enc("INSERT DATA { <http://ex/g> <http://ex/p> 1 }")
    );
    let r = send(srv.addr, "POST", "/update", &[], &body);
    assert_eq!(r.status, 200, "an unlabelled form: {r:?}");
    assert_eq!(srv.triples(), 3);
    let body = format!(
        "query={}&format=csv",
        enc("SELECT ?o WHERE { <http://ex/f> <http://ex/p> ?o }")
    );
    let r = send(srv.addr, "POST", "/sparql", &[FORM], &body);
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.body.contains("a&update=CLEAR ALL"), "{r:?}");
    // A form without the statement is a missing statement, not SPARQL text.
    let r = send(srv.addr, "POST", "/sparql", &[FORM], "format=csv");
    assert_eq!(r.status, 400, "{r:?}");
}

/// REST: a raw body naming another database (`&db_name=`) or carrying a
/// second statement (`&sparql=`) used to redirect the query to `dbb` / run
/// the DELETE on `dba`.
#[test]
fn a_raw_rest_body_never_switches_the_database_or_the_statement() {
    let api = Api::start("rest_raw");
    let q = "SELECT ?s WHERE { ?s ?p ?o . FILTER(?o != <http://ex/q?x=1&db_name=dbb&y=2>) }";
    let r = send(
        api.addr,
        "POST",
        "/sparql?db=dba",
        &[ROOT, SPARQL_QUERY, ("Accept", "text/csv")],
        q,
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.body.contains("http://ex/a1"), "dba's subject: {r:?}");
    assert!(!r.body.contains("http://ex/b1"), "never dbb's: {r:?}");

    let q = "SELECT ?s WHERE { ?s ?p <http://ex/q?x=1&sparql=DELETE%20WHERE%20%7B%3Fs%20%3Fp%20%3Fo%7D&y=2> }";
    let r = send(
        api.addr,
        "POST",
        "/sparql?db_name=dba",
        &[ROOT, SPARQL_QUERY],
        q,
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert!(!r.body.contains("changed"), "no update ran: {r:?}");
    assert_eq!(api.count("dba"), 1, "dba was not emptied");

    // /update: the raw body is the statement, the URL names the database.
    let u =
        "INSERT DATA { <http://ex/n> <http://ex/p> <http://ex/q?db_name=dbb&update=CLEAR%20ALL> }";
    let r = send(
        api.addr,
        "POST",
        "/update?db_name=dba",
        &[ROOT, SPARQL_UPDATE],
        u,
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(api.count("dba"), 2);
    assert_eq!(api.count("dbb"), 2, "dbb untouched");

    // /build: a Turtle body holding `&db_path=` builds from the body (it used
    // to look for a server-side file named by the IRI's tail).
    let data = "<http://ex/s> <http://ex/p> <http://ex/r?a=1&db_path=/nonexistent/x.nt&b=2> .\n";
    let r = send(api.addr, "POST", "/build?db_name=dbc", &[ROOT], data);
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(api.count("dbc"), 1);

    // A form body still names the statement and the database.
    let body = format!("db_name=dbb&sparql={}", enc("ASK { <http://ex/b2> ?p ?o }"));
    let r = send(api.addr, "POST", "/sparql", &[ROOT, FORM], &body);
    assert_eq!(r.status, 200, "{r:?}");
    assert!(r.body.contains("true"), "{r:?}");
}

// ---- http#4: tokens come from the request head --------------------------------

/// A session token in the body is not looked for: authentication never scans
/// (and copies) the body, which an unauthenticated client controls.
#[test]
fn a_session_token_in_the_body_is_not_honoured() {
    let api = Api::start("token_body");
    let r = send(
        api.addr,
        "POST",
        "/login",
        &[FORM],
        "username=root&password=pw",
    );
    assert_eq!(r.status, 200, "{r:?}");
    let token = r
        .body
        .split("\"session\":\"")
        .nth(1)
        .and_then(|t| t.split('"').next())
        .expect("a session token")
        .to_string();
    let ask = enc("ASK { ?s ?p ?o }");
    // In the header: accepted.
    let r = send(
        api.addr,
        "POST",
        "/sparql?db_name=dba",
        &[("X-Session-Token", &token), FORM],
        &format!("sparql={ask}"),
    );
    assert_eq!(r.status, 200, "{r:?}");
    // In the URL: accepted.
    let r = send(
        api.addr,
        "GET",
        &format!("/sparql?db_name=dba&session={token}&sparql={ask}"),
        &[],
        "",
    );
    assert_eq!(r.status, 200, "{r:?}");
    // In the body: not looked at.
    let r = send(
        api.addr,
        "POST",
        "/sparql?db_name=dba",
        &[FORM],
        &format!("session={token}&sparql={ask}"),
    );
    assert_eq!(r.status, 401, "{r:?}");
}

// ---- gap-doc-claims#2: dataset parameters ---------------------------------------

#[test]
fn dataset_protocol_parameters_are_refused() {
    let srv = Single::start(ONE);
    let q = enc("SELECT * WHERE { ?s ?p ?o }");
    for param in ["default-graph-uri", "named-graph-uri"] {
        let r = send(
            srv.addr,
            "GET",
            &format!("/sparql?query={q}&{param}={}", enc("http://ex/g")),
            &[],
            "",
        );
        assert_eq!(r.status, 400, "{param}: {r:?}");
        assert!(r.body.contains("not supported"), "{r:?}");
        let body = format!("query={q}&{param}={}", enc("http://ex/g"));
        let r = send(srv.addr, "POST", "/sparql", &[FORM], &body);
        assert_eq!(r.status, 400, "{param} in a form: {r:?}");
    }
    for param in ["using-graph-uri", "using-named-graph-uri"] {
        let r = send(
            srv.addr,
            "POST",
            &format!("/update?{param}={}", enc("http://ex/g")),
            &[SPARQL_UPDATE],
            "INSERT DATA { <http://ex/z> <http://ex/p> 1 }",
        );
        assert_eq!(r.status, 400, "{param}: {r:?}");
    }
    assert_eq!(srv.triples(), 1, "nothing ran");

    let api = Api::start("dataset");
    let r = send(
        api.addr,
        "GET",
        &format!(
            "/sparql?db_name=dba&query={q}&default-graph-uri={}",
            enc("http://ex/g")
        ),
        &[ROOT],
        "",
    );
    assert_eq!(r.status, 400, "{r:?}");
    assert!(r.body.contains("not supported"), "{r:?}");
    let r = send(
        api.addr,
        "POST",
        &format!("/update?db_name=dba&using-graph-uri={}", enc("http://ex/g")),
        &[ROOT, SPARQL_UPDATE],
        "INSERT DATA { <http://ex/z> <http://ex/p> 1 }",
    );
    assert_eq!(r.status, 400, "{r:?}");
    assert_eq!(api.count("dba"), 1);
}

/// An *empty* dataset parameter (`default-graph-uri=`, as a blank "default
/// graph" field of a query form sends it) names no graph: the statement runs
/// as without it, on both servers. Only a parameter naming a graph is
/// refused (round-1 review of gap-doc-claims#2: every value was refused,
/// an empty one included).
#[test]
fn empty_dataset_protocol_parameters_are_ignored() {
    let srv = Single::start(ONE);
    let q = enc("SELECT * WHERE { ?s ?p ?o }");
    for target in [
        format!("/sparql?query={q}&default-graph-uri="),
        format!("/sparql?default-graph-uri=&query={q}&named-graph-uri="),
        format!("/sparql?query={q}&named-graph-uri=%20"),
    ] {
        let r = send(srv.addr, "GET", &target, &[], "");
        assert_eq!(r.status, 200, "{target}: {r:?}");
    }
    let body = format!("query={q}&default-graph-uri=");
    let r = send(srv.addr, "POST", "/sparql", &[FORM], &body);
    assert_eq!(r.status, 200, "an empty parameter in a form: {r:?}");
    let r = send(
        srv.addr,
        "POST",
        "/update?using-graph-uri=&using-named-graph-uri=",
        &[SPARQL_UPDATE],
        "INSERT DATA { <http://ex/z> <http://ex/p> 1 }",
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(srv.triples(), 2, "the update ran");

    let api = Api::start("dataset_empty");
    let r = send(
        api.addr,
        "GET",
        &format!("/sparql?db_name=dba&query={q}&default-graph-uri="),
        &[ROOT],
        "",
    );
    assert_eq!(r.status, 200, "{r:?}");
    let r = send(
        api.addr,
        "POST",
        "/update?db_name=dba&using-graph-uri=",
        &[ROOT, SPARQL_UPDATE],
        "INSERT DATA { <http://ex/z> <http://ex/p> 1 }",
    );
    assert_eq!(r.status, 200, "{r:?}");
    assert_eq!(api.count("dba"), 2);
}
