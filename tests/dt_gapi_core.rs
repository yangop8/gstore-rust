//! Data test: the upstream-compatible `/api` protocol ([`gstore::gapi`]).
//!
//! Each test boots an `ApiServer` (root password `root`) on `127.0.0.1:0` and
//! talks to it the way upstream gStore clients do: JSON bodies exactly as
//! `GstoreConnector.py` sends them (`requests.post(json=…)`), `GET` with URL
//! parameters (`requests.get(params=urlencode(…, quote_via=quote))`), form and
//! multipart bodies, `curl -d '{…}'`. Every outcome must be HTTP 200 with the
//! upstream `StatusCode` / `StatusMsg` envelope and upstream field names.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::query::hash::{hex_encode, md5_hex, pbkdf2_hmac_sha256};

/// Turtle with an IRI, a language-tagged literal, an integer (typed) literal,
/// a plain literal and a blank node.
const DATA: &str = "@prefix : <http://ex/> .\n\
:a :p :b .\n\
:a :name \"Alice\"@en .\n\
:a :age 30 .\n\
:a :nick \"ally\" .\n\
:a :knows _:x .\n\
_:x :name \"Bob\" .\n";

fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("gstore_gapi_{tag}_{}_{n}", std::process::id()))
}

fn serve(server: ApiServer) -> (Arc<ApiServer>, SocketAddr) {
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    (server, addr)
}

fn start(tag: &str) -> (Arc<ApiServer>, SocketAddr, PathBuf) {
    let root = tmp_root(tag);
    let (srv, addr) = serve(ApiServer::bind(&root, "root", "127.0.0.1:0").unwrap());
    (srv, addr, root)
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Json {
        json::parse(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }
}

fn send(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let mut bytes = raw.into_bytes();
    bytes.extend_from_slice(body);
    s.write_all(&bytes).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let split = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header end");
    let head = String::from_utf8_lossy(&resp[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status");
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Reply {
        status,
        headers,
        body: String::from_utf8_lossy(&resp[split + 4..]).into_owned(),
    }
}

/// `POST /api` with a JSON body, like `GstoreConnector.post` (Python's
/// `json.dumps` puts a space after `,` and `:`).
fn post_json(addr: &SocketAddr, body: &str) -> Reply {
    send(
        addr,
        "POST",
        "/api",
        &[
            ("Content-Type", "application/json"),
            ("Accept-Encoding", "identity"),
        ],
        body.as_bytes(),
    )
}

/// Build a Python-`json.dumps`-style object from string pairs.
fn py_json(pairs: &[(&str, &str)]) -> String {
    let items: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}: {}", json::quote(k), json::quote(v)))
        .collect();
    format!("{{{}}}", items.join(", "))
}

/// `POST /api` with string parameters; the response must be HTTP 200 JSON.
fn api(addr: &SocketAddr, pairs: &[(&str, &str)]) -> Json {
    let r = post_json(addr, &py_json(pairs));
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.header("Content-Type"), Some("application/json"));
    r.json()
}

/// `api` as root.
fn root_api(addr: &SocketAddr, op: &str, extra: &[(&str, &str)]) -> Json {
    let mut pairs = vec![
        ("username", "root"),
        ("password", "root"),
        ("operation", op),
    ];
    pairs.extend_from_slice(extra);
    api(addr, &pairs)
}

/// Percent-encode like Python's `urllib.parse.quote` (with `safe=''`).
fn quote(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn get_api(addr: &SocketAddr, pairs: &[(&str, &str)]) -> Json {
    let q: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", quote(k), quote(v)))
        .collect();
    let r = send(addr, "GET", &format!("/api?{}", q.join("&")), &[], b"");
    assert_eq!(r.status, 200, "{}", r.body);
    r.json()
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .expect("StatusCode")
}

fn msg(j: &Json) -> &str {
    j.get("StatusMsg")
        .and_then(Json::as_str)
        .expect("StatusMsg")
}

fn s<'a>(j: &'a Json, key: &str) -> &'a str {
    j.get(key)
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no string {key} in {j}"))
}

fn n(j: &Json, key: &str) -> i64 {
    j.get(key)
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no number {key} in {j}"))
}

fn bindings(j: &Json) -> &[Json] {
    j.get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("no bindings in {j}"))
}

/// Basic-auth header value for the REST endpoints.
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

/// A REST request as root (Basic auth).
fn rest(addr: &SocketAddr, method: &str, target: &str, body: &str) -> Reply {
    send(
        addr,
        method,
        target,
        &[("Authorization", &basic("root", "root"))],
        body.as_bytes(),
    )
}

fn build(addr: &SocketAddr, db: &str, data: &str) {
    let r = rest(addr, "POST", &format!("/build?db_name={db}"), data);
    assert_eq!(r.status, 200, "build: {}", r.body);
}

fn add_user(addr: &SocketAddr, name: &str, privileges: &str) {
    let r = rest(
        addr,
        "POST",
        &format!("/user/create?op_username={name}&op_password=pw&privileges={privileges}"),
        "",
    );
    assert_eq!(r.status, 200, "{}", r.body);
}

// ---- protocol / envelope / auth ---------------------------------------------

#[test]
fn login_like_the_python_sdk_and_version_ops() {
    let (_srv, addr, _root) = start("login");
    // GstoreConnector.login: POST json={username, password, operation}.
    let j = api(
        &addr,
        &[
            ("username", "root"),
            ("password", "root"),
            ("operation", "login"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!(msg(&j), "login successfully");
    assert_eq!(s(&j, "CoreVersion"), "1.4");
    assert_eq!(s(&j, "licensetype"), "opensource");
    assert_eq!(s(&j, "type"), "http");
    // testConnect / getCoreVersion: the SDK's default is GET with URL params.
    for op in ["testConnect", "getCoreVersion"] {
        let j = get_api(
            &addr,
            &[
                ("username", "root"),
                ("password", "root"),
                ("operation", op),
            ],
        );
        assert_eq!((code(&j), msg(&j)), (0, "success"), "{op}: {j}");
        assert_eq!(s(&j, "CoreVersion"), "1.4");
        assert_eq!(s(&j, "type"), "http");
    }
}

#[test]
fn check_needs_no_credentials_and_get_slash_routes_by_operation() {
    let (_srv, addr, _root) = start("check");
    let j = api(&addr, &[("operation", "check")]);
    assert_eq!(code(&j), 0);
    assert_eq!(
        msg(&j),
        std::process::id().to_string(),
        "upstream answers its pid"
    );
    // GET /?operation=… is the API; plain GET / is still the web console.
    let r = send(&addr, "GET", "/?operation=check", &[], b"");
    assert_eq!(code(&r.json()), 0);
    let r = send(&addr, "GET", "/", &[], b"");
    assert!(r.body.contains("gStore web console"), "{}", r.body);
    // POST / and the legacy /grpc/api path are the API too.
    for path in ["/", "/grpc/api", "/ghttp/api"] {
        let r = send(
            &addr,
            "POST",
            path,
            &[("Content-Type", "application/json")],
            br#"{"operation":"check"}"#,
        );
        assert_eq!((r.status, code(&r.json())), (200, 0), "{path}: {}", r.body);
    }
}

#[test]
fn bodies_in_every_upstream_shape() {
    let (_srv, addr, _root) = start("shapes");
    // x-www-form-urlencoded.
    let r = send(
        &addr,
        "POST",
        "/api",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        b"operation=testConnect&username=root&password=root",
    );
    assert_eq!(code(&r.json()), 0, "{}", r.body);
    // `curl -d '{…}'` sends JSON labelled as a form; it is still JSON.
    let r = send(
        &addr,
        "POST",
        "/api",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        br#"{"operation":"login","username":"root","password":"root"}"#,
    );
    assert_eq!(msg(&r.json()), "login successfully", "{}", r.body);
    // multipart/form-data.
    let body =
        b"--b0undary\r\nContent-Disposition: form-data; name=\"operation\"\r\n\r\ntestConnect\r\n\
--b0undary\r\nContent-Disposition: form-data; name=\"username\"\r\n\r\nroot\r\n\
--b0undary\r\nContent-Disposition: form-data; name=\"password\"\r\n\r\nroot\r\n--b0undary--\r\n";
    let r = send(
        &addr,
        "POST",
        "/api",
        &[("Content-Type", "multipart/form-data; boundary=b0undary")],
        body,
    );
    assert_eq!(code(&r.json()), 0, "{}", r.body);
    // URL parameters merged with a JSON body (the body wins).
    let r = send(
        &addr,
        "POST",
        "/api?operation=login&password=wrong",
        &[("Content-Type", "application/json")],
        br#"{"username":"root","password":"root"}"#,
    );
    assert_eq!(msg(&r.json()), "login successfully", "{}", r.body);
    // A malformed JSON body: 1003, still HTTP 200.
    let r = post_json(&addr, r#"{"operation":"login","username":"root""#);
    assert_eq!((r.status, code(&r.json())), (200, 1003), "{}", r.body);
}

#[test]
fn auth_failures_and_unknown_operations_are_http_200_envelopes() {
    let (_srv, addr, _root) = start("authfail");
    let j = api(&addr, &[("operation", "login"), ("username", "root")]);
    assert_eq!((code(&j), msg(&j)), (1003, "username or password is empty"));
    let j = api(&addr, &[("operation", "query"), ("password", "root")]);
    assert_eq!((code(&j), msg(&j)), (1003, "username or password is empty"));
    let j = api(
        &addr,
        &[
            ("operation", "login"),
            ("username", "root"),
            ("password", "nope"),
        ],
    );
    assert_eq!(code(&j), 1001, "{j}");
    assert!(
        !j.to_string().contains("nope"),
        "the password is not echoed: {j}"
    );
    let j = api(
        &addr,
        &[
            ("operation", "login"),
            ("username", "ghost"),
            ("password", "x"),
        ],
    );
    assert_eq!(code(&j), 1001);
    // Unknown and missing operations.
    let j = root_api(&addr, "fquery", &[]);
    assert_eq!((code(&j), msg(&j)), (1100, "The operation is not defined"));
    let j = api(&addr, &[("username", "root"), ("password", "root")]);
    assert_eq!(code(&j), 1100);
    let r = send(&addr, "GET", "/api", &[], b"");
    assert_eq!((r.status, code(&r.json())), (200, 1100));
    // An invalid db_name.
    for bad in ["ab", "system", "x.db", "../etc"] {
        let j = root_api(&addr, "query", &[("db_name", bad), ("sparql", "ASK {}")]);
        assert_eq!(code(&j), 1003, "{bad}: {j}");
    }
    let j = root_api(&addr, "query", &[("sparql", "ASK {}")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1003, "the value of db_name can not be empty!")
    );
}

#[test]
fn encryption_1_sends_the_md5_of_the_password() {
    let (_srv, addr, _root) = start("md5");
    let md5 = md5_hex("root");
    let j = api(
        &addr,
        &[
            ("operation", "login"),
            ("username", "root"),
            ("password", &md5),
            ("encryption", "1"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    // Upper-case hex is the same digest.
    let upper = md5.to_ascii_uppercase();
    let j = api(
        &addr,
        &[
            ("operation", "login"),
            ("username", "root"),
            ("password", &upper),
            ("encryption", "1"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    // The plaintext is not the digest, and a wrong digest fails.
    let j = api(
        &addr,
        &[
            ("operation", "login"),
            ("username", "root"),
            ("password", "root"),
            ("encryption", "1"),
        ],
    );
    assert_eq!(code(&j), 1001);
    let j = api(
        &addr,
        &[
            ("operation", "login"),
            ("username", "root"),
            ("password", &md5_hex("x")),
            ("encryption", "1"),
        ],
    );
    assert_eq!(code(&j), 1001);
    // `encryption` given as a JSON number works too.
    let r = post_json(
        &addr,
        &format!(
            r#"{{"operation": "testConnect", "username": "root", "password": "{md5}", "encryption": 1}}"#
        ),
    );
    assert_eq!(code(&r.json()), 0, "{}", r.body);
    // The digest is not accepted as a plaintext password.
    let j = api(
        &addr,
        &[
            ("operation", "login"),
            ("username", "root"),
            ("password", &md5),
        ],
    );
    assert_eq!(code(&j), 1001);
}

/// A `users.db` written before the MD5-compatible format (PBKDF2 of the raw
/// password): `encryption=1` is refused with a helpful 1001 until one
/// plaintext login migrated the entry.
#[test]
fn an_older_password_entry_needs_one_plaintext_login_for_encryption_1() {
    let root_dir = tmp_root("md5_migrate");
    std::fs::create_dir_all(&root_dir).unwrap();
    let salt = b"0123456789abcdef";
    let mut hash = [0u8; 32];
    pbkdf2_hmac_sha256(b"oldpw", salt, 100_000, &mut hash);
    let line = format!(
        "root\tpbkdf2-sha256$100000${}${}\tadmin\n",
        hex_encode(salt),
        hex_encode(&hash)
    );
    std::fs::write(root_dir.join("users.db"), line).unwrap();
    let (_srv, addr) = serve(ApiServer::bind(&root_dir, "ignored", "127.0.0.1:0").unwrap());
    let digest = md5_hex("oldpw");
    let enc = [
        ("operation", "login"),
        ("username", "root"),
        ("password", digest.as_str()),
        ("encryption", "1"),
    ];
    let j = api(&addr, &enc);
    assert_eq!(code(&j), 1001, "{j}");
    assert!(msg(&j).contains("plaintext"), "{j}");
    let j = api(
        &addr,
        &[
            ("operation", "login"),
            ("username", "root"),
            ("password", "oldpw"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    let j = api(&addr, &enc);
    assert_eq!(code(&j), 0, "migrated: {j}");
    let text = std::fs::read_to_string(root_dir.join("users.db")).unwrap();
    assert!(text.starts_with("root\tpbkdf2-sha256-md5$"), "{text}");
}

#[test]
fn inner_requests_are_ignored_unless_trusted() {
    let (srv, addr, _root) = start("inner");
    let inner = [
        ("operation", "testConnect"),
        ("username", "root"),
        ("password", ""),
        ("inner", "true"),
    ];
    assert_eq!(code(&api(&addr, &inner)), 1001, "off by default");
    let mut cfg = srv.gapi_config();
    cfg.trust_inner_localhost = true;
    srv.set_gapi_config(cfg);
    assert_eq!(code(&api(&addr, &inner)), 0, "trusted from 127.0.0.1");
    let ghost = [
        ("operation", "testConnect"),
        ("username", "ghost"),
        ("password", ""),
        ("inner", "true"),
    ];
    assert_eq!(code(&api(&addr, &ghost)), 1001, "the user must exist");
}

#[test]
fn privileges_map_operations_like_upstream() {
    let (_srv, addr, _root) = start("priv");
    build(&addr, "pdb", DATA);
    add_user(&addr, "reader", "query");
    add_user(&addr, "writer", "query,update");
    let as_user = |user: &str, op: &str, extra: &[(&str, &str)]| {
        let mut pairs = vec![("username", user), ("password", "pw"), ("operation", op)];
        pairs.extend_from_slice(extra);
        api(&addr, &pairs)
    };
    // Anyone may log in; stat is root-only (upstream grants unlisted
    // operations to root alone).
    assert_eq!(code(&as_user("reader", "login", &[])), 0);
    let j = as_user("reader", "stat", &[]);
    assert_eq!((code(&j), msg(&j)), (1004, "You have no stat privilege"));
    // Query privilege: reads yes, updates no.
    let sel = [
        ("db_name", "pdb"),
        (
            "sparql",
            "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }",
        ),
    ];
    assert_eq!(code(&as_user("reader", "query", &sel)), 0);
    let ins = [
        ("db_name", "pdb"),
        (
            "sparql",
            "INSERT DATA { <http://ex/r> <http://ex/p> <http://ex/s> }",
        ),
    ];
    let j = as_user("reader", "query", &ins);
    assert_eq!((code(&j), msg(&j)), (1004, "You have no update privilege"));
    let j = as_user("reader", "begin", &[("db_name", "pdb")]);
    assert_eq!((code(&j), msg(&j)), (1004, "You have no begin privilege"));
    // Update privilege: updates and transactions.
    assert_eq!(code(&as_user("writer", "query", &ins)), 0);
    let j = as_user("writer", "begin", &[("db_name", "pdb")]);
    assert_eq!(code(&j), 0, "{j}");
    let tid = s(&j, "TID").to_string();
    // Root may end anyone's transaction (Phase 0 rule).
    let j = root_api(&addr, "rollback", &[("db_name", "pdb"), ("tid", &tid)]);
    assert_eq!(code(&j), 0, "{j}");
    // Nobody may target the system database, not even root.
    let j = root_api(
        &addr,
        "query",
        &[("db_name", "system"), ("sparql", "ASK {}")],
    );
    assert_eq!(code(&j), 1003);
}

#[test]
fn stat_reports_resources() {
    let (_srv, addr, _root) = start("stat");
    let j = root_api(&addr, "stat", &[]);
    assert_eq!((code(&j), msg(&j)), (0, "success"), "{j}");
    let cpu = s(&j, "cup_usage");
    assert_eq!(cpu, s(&j, "cpu_usage"));
    assert!(cpu.parse::<f64>().is_ok() && cpu.contains('.'), "{cpu}");
    assert!(s(&j, "mem_usage").parse::<f64>().is_ok());
    let disk: u64 = s(&j, "disk_available").parse().unwrap();
    if cfg!(all(target_os = "linux", target_pointer_width = "64")) {
        assert!(disk > 0, "{j}");
    }
}

// ---- query ------------------------------------------------------------------

#[test]
fn query_json_matches_upstream_fields_and_term_encodings() {
    let (_srv, addr, _root) = start("query");
    build(&addr, "qdb", DATA);
    let sparql = "SELECT ?p ?o WHERE { <http://ex/a> ?p ?o }";
    let r = post_json(
        &addr,
        &py_json(&[
            ("username", "root"),
            ("password", "root"),
            ("operation", "query"),
            ("db_name", "qdb"),
            ("format", "json"),
            ("sparql", sparql),
        ]),
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.header("Cache-Control"), Some("no-cache"));
    // Upstream's key order (nlohmann::json objects are std::maps).
    assert!(
        r.body
            .starts_with(r#"{"AnsNum":5,"OutputLimit":-1,"QueryTime":""#),
        "{}",
        r.body
    );
    let j = r.json();
    assert_eq!((code(&j), msg(&j)), (0, "success"));
    assert_eq!(n(&j, "AnsNum"), 5);
    assert_eq!(n(&j, "OutputLimit"), -1);
    assert!(s(&j, "QueryTime").parse::<u64>().is_ok());
    assert!(s(&j, "ThreadId").parse::<u64>().is_ok());
    let head = j.get("head").unwrap();
    assert_eq!(head.get("link"), Some(&Json::arr()));
    assert_eq!(head.get("vars").unwrap().to_string(), r#"["p","o"]"#);
    let by_pred = |p: &str| -> Json {
        bindings(&j)
            .iter()
            .find(|b| {
                b.get("p")
                    .and_then(|v| v.get("value"))
                    .and_then(Json::as_str)
                    == Some(p)
            })
            .and_then(|b| b.get("o"))
            .cloned()
            .unwrap_or_else(|| panic!("no row for {p}: {j}"))
    };
    let iri = by_pred("http://ex/p");
    assert_eq!(
        iri.sorted().to_string(),
        r#"{"type":"uri","value":"http://ex/b"}"#
    );
    let lang = by_pred("http://ex/name");
    assert_eq!(
        lang.sorted().to_string(),
        r#"{"lang":"en","type":"literal","value":"Alice"}"#
    );
    let typed = by_pred("http://ex/age");
    assert_eq!(
        typed.sorted().to_string(),
        r#"{"datatype":"http://www.w3.org/2001/XMLSchema#integer","type":"typed-literal","value":"30"}"#
    );
    let plain = by_pred("http://ex/nick");
    assert_eq!(
        plain.sorted().to_string(),
        r#"{"type":"literal","value":"ally"}"#
    );
    let bnode = by_pred("http://ex/knows");
    assert_eq!(s(&bnode, "type"), "bnode");
    assert!(s(&bnode, "value").starts_with("_:"), "{bnode}");
    // The SDK's GET form of the same query.
    let j = get_api(
        &addr,
        &[
            ("username", "root"),
            ("password", "root"),
            ("operation", "query"),
            ("db_name", "qdb"),
            ("format", "json"),
            ("sparql", sparql),
        ],
    );
    assert_eq!(n(&j, "AnsNum"), 5);
    // A LIMIT is reported as OutputLimit.
    let j = root_api(
        &addr,
        "query",
        &[
            ("db_name", "qdb"),
            ("sparql", "SELECT ?s WHERE { ?s ?p ?o } LIMIT 2"),
        ],
    );
    assert_eq!((n(&j, "AnsNum"), n(&j, "OutputLimit")), (2, 2));
    // Unbound variables are omitted from their row.
    let j = root_api(
        &addr,
        "query",
        &[
            ("db_name", "qdb"),
            (
                "sparql",
                "SELECT ?x ?y WHERE { ?x <http://ex/nick> ?n OPTIONAL { ?x <http://ex/zzz> ?y } }",
            ),
        ],
    );
    assert_eq!(bindings(&j).len(), 1);
    assert!(bindings(&j)[0].get("y").is_none(), "{j}");
}

#[test]
fn ask_and_update_through_query() {
    let (_srv, addr, _root) = start("ask");
    build(&addr, "adb", DATA);
    let ask = |q: &str| root_api(&addr, "query", &[("db_name", "adb"), ("sparql", q)]);
    let j = ask("ASK { <http://ex/a> <http://ex/p> <http://ex/b> }");
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!(
        j.get("head").unwrap().get("vars").unwrap().to_string(),
        r#"["_askResult"]"#
    );
    assert_eq!(n(&j, "AnsNum"), 1);
    assert_eq!(
        bindings(&j)[0].sorted().to_string(),
        r#"{"_askResult":{"datatype":"http://www.w3.org/2001/XMLSchema#boolean","type":"typed-literal","value":"true"}}"#
    );
    let j = ask("ASK { <http://ex/a> <http://ex/p> <http://ex/zzz> }");
    assert_eq!(
        s(bindings(&j)[0].get("_askResult").unwrap(), "value"),
        "false"
    );
    // An update through `query`.
    let j = ask("INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> . <http://ex/c> <http://ex/p> <http://ex/e> }");
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "update query returns true."),
        "{j}"
    );
    assert_eq!(n(&j, "AnsNum"), 2);
    assert!(
        j.get("OutputLimit").is_none() && j.get("head").is_none(),
        "{j}"
    );
    assert!(j.get("ThreadId").is_some() && j.get("QueryTime").is_some());
    let j = ask("ASK { <http://ex/c> <http://ex/p> <http://ex/e> }");
    assert_eq!(
        s(bindings(&j)[0].get("_askResult").unwrap(), "value"),
        "true"
    );
    // A syntax error is a 1005 envelope.
    let j = ask("SELEKT nonsense");
    assert_eq!(code(&j), 1005, "{j}");
    // Missing sparql.
    let j = root_api(&addr, "query", &[("db_name", "adb")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1003, "the value of sparql can not be empty!")
    );
}

#[test]
fn query_output_formats() {
    let (_srv, addr, root) = start("formats");
    build(&addr, "fdb", DATA);
    let q = |fmt: &str, sparql: &str| {
        root_api(
            &addr,
            "query",
            &[("db_name", "fdb"), ("format", fmt), ("sparql", sparql)],
        )
    };
    let sel = "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }";
    // n-triple: head = ?-prefixed vars, results = rows of N-Triples terms.
    let j = q("n-triple", sel);
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!(j.get("head").unwrap().to_string(), r#"["?o"]"#);
    assert_eq!(
        j.get("results").unwrap().to_string(),
        r#"[["<http://ex/b>"]]"#
    );
    assert_eq!(n(&j, "AnsNum"), 1);
    // A literal is printed as upstream stores it, its lexical form between
    // quotes unescaped: stripping the quotes off a graph function's value
    // gives its JSON.
    let j = q(
        "n-triple",
        "SELECT (shortestPathLen(<http://ex/a>, <http://ex/b>, true, {}) AS ?y) WHERE {}",
    );
    assert_eq!(code(&j), 0, "{j}");
    let cell = j.get("results").unwrap().as_array().unwrap()[0]
        .as_array()
        .unwrap()[0]
        .as_str()
        .unwrap()
        .to_string();
    let value = json::parse(&cell[1..cell.len() - 1])
        .unwrap_or_else(|e| panic!("{cell} is not a quoted JSON value: {e:?}"));
    assert!(value.get("paths").is_some(), "{cell}");
    // file: the JSON result goes to <root>/export/<FileName>.
    let j = q("file", sel);
    assert_eq!(code(&j), 0, "{j}");
    let name = s(&j, "FileName");
    assert!(
        name.starts_with("fdb_") && name.ends_with(".json"),
        "{name}"
    );
    assert!(j.get("results").is_none(), "{j}");
    let written =
        json::parse(&std::fs::read_to_string(root.join("export").join(name)).unwrap()).unwrap();
    assert_eq!(bindings(&written).len(), 1);
    assert_eq!(
        s(bindings(&written)[0].get("o").unwrap(), "value"),
        "http://ex/b"
    );
    // n-triple-file: tab-separated terms in a .txt file.
    let j = q("n-triple-file", sel);
    let name = s(&j, "FileName");
    assert!(name.ends_with(".txt"), "{name}");
    let text = std::fs::read_to_string(root.join("export").join(name)).unwrap();
    assert_eq!(text, "?o\n<http://ex/b>\n");
    let j = q(
        "n-triple-file",
        "SELECT ?o WHERE { <http://ex/zzz> <http://ex/p> ?o }",
    );
    let text = std::fs::read_to_string(root.join("export").join(s(&j, "FileName"))).unwrap();
    assert_eq!(text, "[empty result]\n");
    // An unknown format, as upstream: a read runs and then answers 1005
    // "Unknown result format."; an update ignores it and is applied.
    let j = q("xml", sel);
    assert_eq!(
        (code(&j), j.get("StatusMsg").and_then(Json::as_str)),
        (1005, Some("Unknown result format.")),
        "{j}"
    );
    let j = q(
        "csv",
        "INSERT DATA { <http://ex/new> <http://ex/p> <http://ex/x> }",
    );
    assert_eq!(code(&j), 0, "{j}");
    let j = q(
        "json",
        "ASK { <http://ex/new> <http://ex/p> <http://ex/x> }",
    );
    assert_eq!(
        s(bindings(&j)[0].get("_askResult").unwrap(), "value"),
        "true"
    );
}

#[test]
fn output_is_capped_at_max_output_size() {
    let root = tmp_root("cap");
    let (_srv, addr) = serve(
        ApiServer::bind(&root, "root", "127.0.0.1:0")
            .unwrap()
            .with_max_output_size(3),
    );
    let mut ttl = String::from("@prefix : <http://ex/> .\n");
    for i in 0..10 {
        ttl.push_str(&format!(":s{i} :p :o{i} .\n"));
    }
    build(&addr, "cdb", &ttl);
    let q = |fmt: &str, sparql: &str| {
        root_api(
            &addr,
            "query",
            &[("db_name", "cdb"), ("format", fmt), ("sparql", sparql)],
        )
    };
    let all = "SELECT ?s WHERE { ?s <http://ex/p> ?o }";
    let j = q("json", all);
    assert_eq!((n(&j, "AnsNum"), n(&j, "OutputLimit")), (10, 3), "{j}");
    assert_eq!(bindings(&j).len(), 3);
    let j = q("n-triple", all);
    assert_eq!(j.get("results").and_then(Json::as_array).unwrap().len(), 3);
    // A LIMIT above the cap is capped; below it, it stands.
    let j = q("json", "SELECT ?s WHERE { ?s <http://ex/p> ?o } LIMIT 8");
    assert_eq!(
        (n(&j, "AnsNum"), n(&j, "OutputLimit"), bindings(&j).len()),
        (8, 3, 3)
    );
    let j = q("json", "SELECT ?s WHERE { ?s <http://ex/p> ?o } LIMIT 2");
    assert_eq!(
        (n(&j, "AnsNum"), n(&j, "OutputLimit"), bindings(&j).len()),
        (2, 2, 2)
    );
    // File outputs are not capped.
    let j = q("file", all);
    let written =
        json::parse(&std::fs::read_to_string(root.join("export").join(s(&j, "FileName"))).unwrap())
            .unwrap();
    assert_eq!(bindings(&written).len(), 10);
}

#[test]
fn query_on_missing_or_unloaded_databases() {
    let (_srv, addr, _root) = start("missing");
    let j = root_api(
        &addr,
        "query",
        &[("db_name", "nosuch"), ("sparql", "ASK {}")],
    );
    assert_eq!((code(&j), msg(&j)), (1004, "Database does not exist."));
    build(&addr, "udb", DATA);
    assert_eq!(rest(&addr, "POST", "/unload?db_name=udb", "").status, 200);
    let j = root_api(&addr, "query", &[("db_name", "udb"), ("sparql", "ASK {}")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1005, "database[udb] is not loaded yet")
    );
    let j = root_api(&addr, "begin", &[("db_name", "udb")]);
    assert_eq!((code(&j), msg(&j)), (1004, "Database is not loaded yet."));
    let j = root_api(&addr, "checkpoint", &[("db_name", "udb")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1005, "database[udb] is not loaded yet")
    );
    let j = root_api(&addr, "checkpoint", &[("db_name", "nosuch")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1005, "database[nosuch] does not exist")
    );
    // Upstream checks that the database is loaded before it looks at the tid.
    for op in ["tquery", "commit", "rollback"] {
        for db in ["udb", "nosuch"] {
            let j = root_api(
                &addr,
                op,
                &[("db_name", db), ("tid", "1"), ("sparql", "ASK {}")],
            );
            assert_eq!(
                (code(&j), msg(&j)),
                (1004, "Database txn is not loaded yet."),
                "{op} on {db}"
            );
        }
    }
}

// ---- transactions -----------------------------------------------------------

#[test]
fn transactions_through_api_with_snapshot_isolation_and_owners() {
    let (_srv, addr, root) = start("txn");
    build(&addr, "tdb", DATA);
    add_user(&addr, "alice", "query,update");
    add_user(&addr, "bob", "query,update");
    let as_user = |user: &str, op: &str, extra: &[(&str, &str)]| {
        let mut pairs = vec![
            ("username", user),
            ("password", "pw"),
            ("operation", op),
            ("db_name", "tdb"),
        ];
        pairs.extend_from_slice(extra);
        api(&addr, &pairs)
    };
    let ask_new = |user: &str| {
        let j = as_user(
            user,
            "query",
            &[(
                "sparql",
                "ASK { <http://ex/n> <http://ex/p> <http://ex/m> }",
            )],
        );
        s(bindings(&j)[0].get("_askResult").unwrap(), "value").to_string()
    };
    // Upstream's ids are small and sequential: the first transaction is 1.
    let j = as_user("alice", "begin", &[("isolevel", "1")]);
    assert_eq!((code(&j), msg(&j)), (0, "Transaction begin success"), "{j}");
    assert_eq!(s(&j, "TID"), "1");
    // tquery: an update, then a read inside the transaction.
    let j = as_user(
        "alice",
        "tquery",
        &[
            ("tid", "1"),
            (
                "sparql",
                "INSERT DATA { <http://ex/n> <http://ex/p> <http://ex/m> }",
            ),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j), n(&j, "AnsNum")),
        (0, "Transaction query success", 1),
        "{j}"
    );
    let j = as_user(
        "alice",
        "tquery",
        &[
            ("tid", "1"),
            (
                "sparql",
                "SELECT ?o WHERE { <http://ex/n> <http://ex/p> ?o }",
            ),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j), n(&j, "AnsNum")),
        (0, "success", 1),
        "{j}"
    );
    assert_eq!(s(bindings(&j)[0].get("o").unwrap(), "value"), "http://ex/m");
    // Others read the begin-time snapshot, and cannot write meanwhile.
    assert_eq!(ask_new("bob"), "false");
    let j = as_user(
        "bob",
        "query",
        &[(
            "sparql",
            "INSERT DATA { <http://ex/q> <http://ex/p> <http://ex/r> }",
        )],
    );
    assert_eq!(code(&j), 1004, "{j}");
    // Owner enforcement: bob may not touch alice's transaction.
    for op in ["tquery", "commit", "rollback"] {
        let j = as_user("bob", op, &[("tid", "1"), ("sparql", "ASK {}")]);
        assert_eq!(code(&j), 1004, "{op}: {j}");
        assert!(msg(&j).contains("belongs to another user"), "{j}");
    }
    // A second begin on the same database is refused (one writer).
    let j = as_user("bob", "begin", &[]);
    assert_eq!(code(&j), 1008, "{j}");
    // Checkpoint is refused while the transaction is open.
    let j = as_user("alice", "checkpoint", &[]);
    assert_eq!(code(&j), 1004, "{j}");
    // Commit; everyone sees it, and it was saved (check_point).
    let j = as_user("alice", "commit", &[("tid", "1")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "Transaction commit success."),
        "{j}"
    );
    assert_eq!(ask_new("bob"), "true");
    let mut db = gstore::Database::open(root.join("tdb.db")).unwrap();
    assert_eq!(
        db.query("ASK { <http://ex/n> <http://ex/p> <http://ex/m> }")
            .unwrap(),
        gstore::QueryResult::Ask(true)
    );
    // Committing again: not running.
    let j = as_user("alice", "commit", &[("tid", "1")]);
    assert_eq!(code(&j), 1005);
    assert!(msg(&j).contains("not in running state"), "{j}");
    // Rollback discards.
    let j = as_user("alice", "begin", &[]);
    let tid = s(&j, "TID").to_string();
    assert_eq!(tid, "2");
    let j = as_user(
        "alice",
        "tquery",
        &[
            ("tid", &tid),
            (
                "sparql",
                "DELETE DATA { <http://ex/n> <http://ex/p> <http://ex/m> }",
            ),
        ],
    );
    assert_eq!(n(&j, "AnsNum"), 1);
    // The Workbench's "beginTime_tid" form addresses the same transaction.
    let j = as_user(
        "alice",
        "rollback",
        &[("tid", &format!("1700000000_{tid}"))],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "Transaction rollback success."),
        "{j}"
    );
    assert_eq!(ask_new("alice"), "true");
    // Malformed and unknown ids, and an id of another database.
    let j = as_user("alice", "commit", &[("tid", "abc")]);
    assert_eq!((code(&j), msg(&j)), (1003, "TID abc is not a pure number."));
    let j = as_user("alice", "commit", &[]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1003, "the value of tid can not be empty!")
    );
    let j = as_user("alice", "commit", &[("tid", "999")]);
    assert_eq!(code(&j), 1005);
    assert!(msg(&j).contains("Wrong transaction id"), "{j}");
    build(&addr, "other", DATA);
    let j = as_user("alice", "begin", &[]);
    let tid = s(&j, "TID").to_string();
    let j = api(
        &addr,
        &[
            ("username", "alice"),
            ("password", "pw"),
            ("operation", "commit"),
            ("db_name", "other"),
            ("tid", &tid),
        ],
    );
    assert_eq!(code(&j), 1005, "{j}");
    let j = as_user("alice", "rollback", &[("tid", &tid)]);
    assert_eq!(code(&j), 0);
    // isolevel is validated.
    let j = as_user("alice", "begin", &[("isolevel", "9")]);
    assert_eq!(
        (code(&j), msg(&j)),
        (1003, "The isolation level's value only can been 1/2/3")
    );
    // Checkpoint without an open transaction.
    let j = as_user("alice", "checkpoint", &[]);
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "Database saved successfully."),
        "{j}"
    );
}

#[test]
fn tids_given_as_json_numbers_work() {
    let (_srv, addr, _root) = start("tidnum");
    build(&addr, "ndb", DATA);
    let j = root_api(&addr, "begin", &[("db_name", "ndb")]);
    let tid = s(&j, "TID").to_string();
    let r = post_json(
        &addr,
        &format!(
            r#"{{"username": "root", "password": "root", "operation": "commit", "db_name": "ndb", "tid": {tid}}}"#
        ),
    );
    assert_eq!(code(&r.json()), 0, "{}", r.body);
}

// ---- coexistence with the REST endpoints -------------------------------------

#[test]
fn rest_endpoints_are_unaffected() {
    let (_srv, addr, _root) = start("rest");
    build(&addr, "rdb", DATA);
    // /sparql still answers SPARQL 1.1 Results JSON, with HTTP statuses.
    let r = rest(
        &addr,
        "POST",
        "/sparql?db_name=rdb",
        "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }",
    );
    assert_eq!(r.status, 200);
    assert!(
        r.body.starts_with(r#"{"head":{"vars":["o"]}"#),
        "{}",
        r.body
    );
    let r = send(&addr, "POST", "/sparql?db_name=rdb", &[], b"ASK {}");
    assert_eq!(r.status, 401, "REST auth unchanged");
    // A REST transaction is visible to /api and the other way round.
    let r = rest(&addr, "POST", "/begin?db_name=rdb", "");
    assert_eq!(r.status, 200, "{}", r.body);
    let tid = s(&r.json(), "TID").to_string();
    let j = root_api(
        &addr,
        "tquery",
        &[
            ("db_name", "rdb"),
            ("tid", &tid),
            (
                "sparql",
                "INSERT DATA { <http://ex/z> <http://ex/p> <http://ex/y> }",
            ),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    let r = rest(&addr, "POST", &format!("/commit?tid={tid}"), "");
    assert_eq!(r.status, 200, "{}", r.body);
    let r = rest(
        &addr,
        "POST",
        "/sparql?db_name=rdb",
        "ASK { <http://ex/z> <http://ex/p> <http://ex/y> }",
    );
    assert!(r.body.contains("true"), "{}", r.body);
    // /login sessions and /show keep working.
    let r = send(
        &addr,
        "POST",
        "/login?username=root&password=root",
        &[],
        b"",
    );
    assert_eq!(r.status, 200);
    assert!(r.body.contains("\"session\""), "{}", r.body);
    let r = rest(&addr, "GET", "/show", "");
    assert!(r.body.contains("\"rdb\""), "{}", r.body);
    // An /api query shows up in the REST query log.
    let log = rest(&addr, "GET", "/querylog", "").body;
    assert!(log.contains("<http://ex/z>"), "{log}");
}

#[cfg(feature = "gzip")]
#[test]
fn api_responses_are_gzipped_when_accepted() {
    let (_srv, addr, _root) = start("gzip");
    let r = send(
        &addr,
        "POST",
        "/api",
        &[
            ("Content-Type", "application/json"),
            ("Accept-Encoding", "gzip, deflate"),
        ],
        br#"{"operation":"login","username":"root","password":"root"}"#,
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.header("Content-Encoding"), Some("gzip"));
}
