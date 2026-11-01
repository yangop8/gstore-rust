//! Data test: PFN personalized functions (`docs/PFN.md`, `src/pfn/`).
//!
//! * the `/api` management operations `funquery` / `funcudb` / `funreview`
//!   (lifecycle, paging, review, compile errors, per-user isolation,
//!   privileges) — with and without the `pfn-wasm` runtime;
//! * `PFN.name({…})` in queries (feature `pfn-wasm`): the two examples of
//!   `docs/PFN.md`, run from that file, against an independent computation;
//!   the sandbox (instruction budget, cancellation through
//!   `operationTaskCancel`, memory cap, bad guest pointers, output bound);
//!   recompilation never served from a cache; `ConcurrentDb` snapshots and
//!   HTTP transaction snapshots;
//! * without the runtime: the clean error naming the `pfn-wasm` feature.

#[cfg(feature = "pfn-wasm")]
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
#[cfg(feature = "pfn-wasm")]
use std::time::Instant;

use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
#[cfg(feature = "pfn-wasm")]
use gstore::pfn::{PfnCaller, PfnInfo, PfnLimits, PfnRegistry};
#[cfg(feature = "pfn-wasm")]
use gstore::QueryResult;
use gstore::{Database, QuerySettings};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("gstore_dtpfn_{tag}_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn start(tag: &str, settings: Option<QuerySettings>) -> (Arc<ApiServer>, SocketAddr, PathBuf) {
    let root = tmp_root(tag);
    let mut server = ApiServer::bind(&root, "root", "127.0.0.1:0").unwrap();
    if let Some(s) = settings {
        server = server.with_query_settings(s);
    }
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || {
        s2.serve_forever();
    });
    (server, addr, root)
}

struct Reply {
    status: u16,
    body: String,
}

fn send(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
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
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status");
    Reply {
        status,
        body: String::from_utf8_lossy(&resp[split + 4..]).into_owned(),
    }
}

fn password(user: &str) -> &'static str {
    if user == "root" {
        "root"
    } else {
        "pw"
    }
}

/// `POST /api` as `user` with the fields of `body` (credentials added).
fn api_as(addr: &SocketAddr, user: &str, body: Json) -> Json {
    let mut body = body;
    body.set("username", user);
    body.set("password", password(user));
    let r = send(
        addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        body.to_string().as_bytes(),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    json::parse(&r.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", r.body))
}

fn api(addr: &SocketAddr, body: Json) -> Json {
    api_as(addr, "root", body)
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

fn basic(user: &str) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let input = format!("{user}:{}", password(user));
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

fn rest(addr: &SocketAddr, method: &str, target: &str, body: &str) -> Reply {
    send(
        addr,
        method,
        target,
        &[("Authorization", &basic("root"))],
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

fn fun_info(name: &str, desc: &str, body: &str, subs: &str) -> Json {
    Json::obj()
        .with("funName", name)
        .with("funDesc", desc)
        .with("funBody", body)
        .with("funSubs", subs)
}

/// `funcudb` as `user`.
fn funcudb(addr: &SocketAddr, user: &str, kind: &str, info: Json) -> Json {
    api_as(
        addr,
        user,
        Json::obj()
            .with("operation", "funcudb")
            .with("type", kind)
            .with("funInfo", info),
    )
}

/// `funquery` as `user` with extra fields.
fn funquery(addr: &SocketAddr, user: &str, extra: &[(&str, Json)]) -> Json {
    let mut body = Json::obj().with("operation", "funquery");
    for (k, v) in extra {
        body.set(*k, v.clone());
    }
    let j = api_as(addr, user, body);
    assert_eq!(code(&j), 0, "{j}");
    j
}

fn list_of(j: &Json) -> Vec<Json> {
    j.get("list")
        .and_then(Json::as_array)
        .expect("list")
        .to_vec()
}

fn field(j: &Json, key: &str) -> String {
    j.get(key)
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no {key} in {j}"))
        .to_string()
}

/// Upstream's `url_encode` (the encoding upstream clients apply to bodies),
/// written independently of the crate's.
fn quote_plus(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unquote_plus(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 2;
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

#[cfg(feature = "pfn-wasm")]
const DOC: &str = include_str!("../docs/PFN.md");

/// The code block of `docs/PFN.md` fenced as ```` ```wat <tag> ````.
#[cfg(feature = "pfn-wasm")]
fn doc_block(tag: &str) -> String {
    let fence = format!("```wat {tag}\n");
    let start = DOC.find(&fence).unwrap_or_else(|| panic!("no {tag} block")) + fence.len();
    let rest = &DOC[start..];
    rest[..rest.find("\n```").expect("closing fence")].to_string()
}

// ---------------------------------------------------------------------------
// test graph and an independent computation
// ---------------------------------------------------------------------------

#[cfg(feature = "pfn-wasm")]
const EX: &str = "http://ex/";

/// A deterministic graph: 24 vertices, three predicates with overlapping
/// pairs (multi-edges), a self loop, type edges, literal triples.
#[cfg(feature = "pfn-wasm")]
fn graph() -> (String, BTreeSet<(String, String, String)>) {
    let mut edges = BTreeSet::new();
    let v = |i: usize| format!("<{EX}v{i}>");
    let p = |i: usize| format!("<{EX}p{i}>");
    for i in 0..24 {
        edges.insert((v(i), p(0), v((i * 7 + 3) % 24)));
        if i % 2 == 0 {
            edges.insert((v(i), p(1), v((i * 3 + 1) % 24)));
            // the same pair under two predicates
            edges.insert((v(i), p(2), v((i * 3 + 1) % 24)));
        }
        if i % 5 == 0 {
            edges.insert((v(i), p(1), v((i + 11) % 24)));
        }
        if i % 4 == 0 {
            edges.insert((v(i), format!("<{EX}type>"), format!("<{EX}T>")));
        }
    }
    edges.insert((v(4), p(0), v(4)));
    let mut nt = String::new();
    for (s, pr, o) in &edges {
        nt.push_str(&format!("{s} {pr} {o} .\n"));
    }
    for i in 0..24 {
        nt.push_str(&format!("{} <{EX}name> \"V{i}\" .\n", v(i)));
    }
    // An entity with only a literal: not a vertex.
    nt.push_str(&format!("<{EX}lonely> <{EX}name> \"L\" .\n"));
    (nt, edges)
}

/// Out-degree of `src` over the entity edges with a predicate in `preds`
/// (every predicate when `None`).
#[cfg(feature = "pfn-wasm")]
fn expected_degree(
    edges: &BTreeSet<(String, String, String)>,
    src: &str,
    pred: Option<&str>,
) -> i64 {
    edges
        .iter()
        .filter(|(s, p, _)| s == src && pred.is_none_or(|q| q == p))
        .count() as i64
}

/// Vertices other than `src` within `k` out-hops along `preds` (every
/// predicate when empty).
#[cfg(feature = "pfn-wasm")]
fn expected_khop(
    edges: &BTreeSet<(String, String, String)>,
    src: &str,
    k: i64,
    preds: &[&str],
) -> i64 {
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for (s, p, o) in edges {
        if preds.is_empty() || preds.contains(&p.as_str()) {
            adj.entry(s).or_default().push(o);
        }
    }
    let is_vertex = edges.iter().any(|(s, _, o)| s == src || o == src);
    if !is_vertex || k <= 0 {
        return 0;
    }
    let mut seen: HashSet<&str> = HashSet::from([src]);
    let mut frontier: VecDeque<&str> = VecDeque::from([src]);
    for _ in 0..k {
        let mut next = VecDeque::new();
        while let Some(u) = frontier.pop_front() {
            for &w in adj.get(u).map(Vec::as_slice).unwrap_or_default() {
                if seen.insert(w) {
                    next.push_back(w);
                }
            }
        }
        frontier = next;
    }
    seen.len() as i64 - 1
}

// ---------------------------------------------------------------------------
// management over /api (any build)
// ---------------------------------------------------------------------------

#[test]
fn lifecycle_over_the_api() {
    let (srv, addr, root) = start("life", None);
    let body = "(call $emit_i64 (i64.const 7)) (i32.const 1)";
    // Create, with the body URL-encoded as upstream clients send it.
    let j = funcudb(
        &addr,
        "root",
        "1",
        fun_info("seven", "answers 7", &quote_plus(body), ""),
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "Function create success.".to_string()),
        "{j}"
    );
    assert!(root.join("pfn/src/root/data.json").is_file());
    assert!(root.join("pfn/src/root/seven.wat").is_file());
    let j = funcudb(&addr, "root", "1", fun_info("Seven", "", body, ""));
    assert_eq!(code(&j), 1005);
    assert_eq!(
        msg(&j),
        "Function create fail: function name Seven already exists"
    );

    // List: upstream's six fields, the status "1".
    let list = list_of(&funquery(&addr, "root", &[]));
    assert_eq!(list.len(), 1);
    let item = &list[0];
    let mut keys: Vec<&str> = item
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "funBody",
            "funDesc",
            "funName",
            "funStatus",
            "funSubs",
            "lastTime"
        ]
    );
    assert_eq!(field(item, "funName"), "seven");
    assert_eq!(field(item, "funDesc"), "answers 7");
    assert_eq!(field(item, "funBody"), quote_plus(body), "stored as sent");
    assert_eq!(field(item, "funStatus"), "1");
    assert_eq!(field(item, "lastTime").len(), "yyyy-MM-dd HH:mm:ss".len());

    // Filters and paging (a Rust extension).
    for name in ["a1", "a2", "a3"] {
        assert_eq!(
            code(&funcudb(&addr, "root", "1", fun_info(name, "", body, ""))),
            0
        );
    }
    let all = funquery(&addr, "root", &[]);
    assert_eq!(list_of(&all).len(), 4);
    assert!(
        all.get("totalSize").is_none(),
        "no paging fields without paging"
    );
    let one = funquery(
        &addr,
        "root",
        &[("funInfo", Json::obj().with("funName", "a2"))],
    );
    assert_eq!(list_of(&one).len(), 1);
    let none = funquery(
        &addr,
        "root",
        &[("funInfo", Json::obj().with("funStatus", "2"))],
    );
    assert!(list_of(&none).is_empty());
    let page = funquery(
        &addr,
        "root",
        &[("pageNo", Json::from(2)), ("pageSize", Json::from(3))],
    );
    assert_eq!(page.get("totalSize").and_then(Json::as_i64), Some(4));
    assert_eq!(page.get("totalPage").and_then(Json::as_i64), Some(2));
    assert_eq!(page.get("pageNo").and_then(Json::as_i64), Some(2));
    let names: Vec<String> = list_of(&page).iter().map(|i| field(i, "funName")).collect();
    assert_eq!(names, ["a3"], "creation order");
    let past = funquery(
        &addr,
        "root",
        &[("pageNo", Json::from(9)), ("pageSize", Json::from(3))],
    );
    assert!(list_of(&past).is_empty());

    // Review: the generated module, URL-encoded.
    let j = api(
        &addr,
        Json::obj()
            .with("operation", "funreview")
            .with("funInfo", fun_info("seven", "", &quote_plus(body), "")),
    );
    assert_eq!(code(&j), 0, "{j}");
    let src = unquote_plus(&field(&j, "result"));
    assert!(src.contains("(import \"gstore\" \"out_degree\""), "{src}");
    assert!(src.contains("(memory (export \"memory\") 1)"), "{src}");
    assert!(
        src.contains("(func (export \"seven\") (result i32)"),
        "{src}"
    );
    assert!(src.contains(body), "{src}");

    // Update replaces every field and resets the status.
    let j = funcudb(&addr, "root", "2", fun_info("seven", "", body, ""));
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "Function update success.".to_string())
    );
    let one = funquery(
        &addr,
        "root",
        &[("funInfo", Json::obj().with("funName", "seven"))],
    );
    assert_eq!(field(&list_of(&one)[0], "funDesc"), "");
    let j = funcudb(&addr, "root", "2", fun_info("nope", "", body, ""));
    assert_eq!(
        msg(&j),
        "Function update fail: function name nope does not exist"
    );

    // Compile: success, or the compiler's message and status 3.
    let j = funcudb(&addr, "root", "4", Json::obj().with("funName", "seven"));
    if cfg!(feature = "pfn-wasm") {
        assert_eq!(
            (code(&j), msg(&j)),
            (0, "Function build success.".to_string()),
            "{j}"
        );
        let one = funquery(
            &addr,
            "root",
            &[("funInfo", Json::obj().with("funName", "seven"))],
        );
        assert_eq!(field(&list_of(&one)[0], "funStatus"), "2");
        assert_eq!(
            std::fs::read_dir(root.join("pfn/lib/root"))
                .unwrap()
                .count(),
            1
        );
        funcudb(&addr, "root", "2", fun_info("a1", "", "(i64.const 1)", ""));
        let j = funcudb(&addr, "root", "4", Json::obj().with("funName", "a1"));
        assert_eq!(code(&j), 1005);
        assert!(msg(&j).contains("type mismatch"), "{j}");
        let one = funquery(
            &addr,
            "root",
            &[("funInfo", Json::obj().with("funName", "a1"))],
        );
        assert_eq!(field(&list_of(&one)[0], "funStatus"), "3");
        let j = funcudb(&addr, "root", "2", fun_info("a2", "", "(i32.const 1", ""));
        assert_eq!(code(&j), 0);
        let j = funcudb(&addr, "root", "4", Json::obj().with("funName", "a2"));
        assert_eq!(code(&j), 1005);
        assert!(!msg(&j).is_empty(), "the parser's message: {j}");
    } else {
        assert_eq!(code(&j), 1005, "{j}");
        assert!(msg(&j).starts_with("Function build fail: "), "{j}");
        assert!(msg(&j).contains("pfn-wasm"), "{j}");
        let one = funquery(
            &addr,
            "root",
            &[("funInfo", Json::obj().with("funName", "seven"))],
        );
        assert_eq!(field(&list_of(&one)[0], "funStatus"), "1", "unchanged");
    }
    let j = funcudb(&addr, "root", "4", Json::obj().with("funName", "nope"));
    // Upstream looks for the source file first (its text, sic).
    assert_eq!(
        msg(&j),
        "Function build fail: Function source file dose not exist"
    );

    // Delete.
    let j = funcudb(&addr, "root", "3", Json::obj().with("funName", "seven"));
    assert_eq!(
        (code(&j), msg(&j)),
        (0, "Function delete success.".to_string())
    );
    assert!(!root.join("pfn/src/root/seven.wat").exists());
    let j = funcudb(&addr, "root", "3", Json::obj().with("funName", "seven"));
    assert_eq!(
        (code(&j), msg(&j)),
        (
            1005,
            "Function delete fail: function seven not exists".to_string()
        )
    );

    // Parameter errors, upstream's texts.
    let j = api(
        &addr,
        Json::obj()
            .with("operation", "funcudb")
            .with("funInfo", fun_info("x", "", body, "")),
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (1003, "the value of type can not be empty!".to_string())
    );
    let j = funcudb(
        &addr,
        "root",
        "1",
        Json::obj().with("funDesc", "only a description"),
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (
            1003,
            "Neither funName nor funBody can be empty!".to_string()
        )
    );
    let j = funcudb(&addr, "root", "9", fun_info("x", "", body, ""));
    assert_eq!(
        (code(&j), msg(&j)),
        (
            1003,
            "The type is invalid, please look up the api document.".to_string()
        )
    );
    let j = api(
        &addr,
        Json::obj()
            .with("operation", "funreview")
            .with("funInfo", Json::obj()),
    );
    assert_eq!(code(&j), 1003);
    let j = funcudb(&addr, "root", "1", fun_info("bad-name", "", body, ""));
    assert_eq!(code(&j), 1005);
    assert!(msg(&j).contains("invalid funName"), "{j}");
    // funInfo as a JSON string (upstream accepts both).
    let j = funcudb(
        &addr,
        "root",
        "1",
        Json::Str(fun_info("asString", "", body, "").to_string()),
    );
    assert_eq!(code(&j), 0, "{j}");
    srv.shutdown();
}

#[test]
fn functions_belong_to_their_user() {
    let (srv, addr, root) = start("users", None);
    add_user(&addr, "alice", "query");
    // A user without any privilege still manages their own functions.
    add_user(&addr, "bob", "");
    let body = "(call $emit_i64 (i64.const 1)) (i32.const 1)";
    for user in ["root", "alice", "bob"] {
        let j = funcudb(&addr, user, "1", fun_info("f", user, body, ""));
        assert_eq!(code(&j), 0, "{user}: {j}");
    }
    let j = funcudb(&addr, "alice", "1", fun_info("onlyAlice", "", body, ""));
    assert_eq!(code(&j), 0);
    let names = |user: &str| -> Vec<(String, String)> {
        list_of(&funquery(&addr, user, &[]))
            .iter()
            .map(|i| (field(i, "funName"), field(i, "funDesc")))
            .collect()
    };
    assert_eq!(names("root"), [("f".to_string(), "root".to_string())]);
    assert_eq!(names("bob"), [("f".to_string(), "bob".to_string())]);
    assert_eq!(names("alice").len(), 2);
    assert!(root.join("pfn/src/alice/onlyalice.wat").is_file());
    // Nobody reaches another user's function, not even root.
    let j = funcudb(&addr, "root", "3", Json::obj().with("funName", "onlyAlice"));
    assert_eq!(code(&j), 1005);
    let j = funcudb(&addr, "bob", "2", fun_info("onlyAlice", "", body, ""));
    assert_eq!(code(&j), 1005);
    assert_eq!(names("alice").len(), 2);
    // Credentials are still required.
    let r = send(
        &addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        br#"{"operation":"funquery","username":"alice","password":"wrong"}"#,
    );
    let j = json::parse(&r.body).unwrap();
    assert_eq!(code(&j), 1001, "{j}");
    // Deleting a user deletes their functions: an account created later
    // under the name inherits none of them (usermanage and REST alike).
    let manage = |kind: &str, name: &str| {
        api(
            &addr,
            Json::obj()
                .with("operation", "usermanage")
                .with("type", kind)
                .with("op_username", name)
                .with("op_password", "pw"),
        )
    };
    assert_eq!(code(&manage("2", "alice")), 0);
    assert!(!root.join("pfn/src/alice").exists());
    assert_eq!(code(&manage("1", "alice")), 0);
    assert!(names("alice").is_empty(), "{:?}", names("alice"));
    let r = rest(&addr, "POST", "/user/drop?op_username=bob", "");
    assert_eq!(r.status, 200, "{}", r.body);
    add_user(&addr, "bob", "");
    assert!(names("bob").is_empty());
    assert_eq!(names("root").len(), 1);
    srv.shutdown();
}

/// The default build has no runtime: a PFN call fails with an error naming
/// the feature, wherever it runs.
#[cfg(not(feature = "pfn-wasm"))]
#[test]
fn without_the_runtime_calls_name_the_feature() {
    let (srv, addr, _root) = start("nofeature", None);
    build(
        &addr,
        "pfnnf",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    );
    let j = funcudb(&addr, "root", "1", fun_info("f", "", "(i32.const 1)", ""));
    assert_eq!(code(&j), 0);
    let j = api(
        &addr,
        Json::obj()
            .with("operation", "query")
            .with("db_name", "pfnnf")
            .with("sparql", r#"SELECT (PFN.f({}) AS ?v) WHERE {}"#),
    );
    assert_eq!(code(&j), 1005, "{j}");
    assert!(msg(&j).contains("pfn-wasm"), "{j}");
    srv.shutdown();
    // An embedded database without a registry says so, and names it too.
    let mut db = Database::build_from_str(
        "nf_embedded",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    )
    .unwrap();
    let e = db
        .query(r#"SELECT (PFN.f({}) AS ?v) WHERE {}"#)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("no PFN function registry") && e.contains("pfn-wasm"),
        "{e}"
    );
}

// ---------------------------------------------------------------------------
// running functions (feature pfn-wasm)
// ---------------------------------------------------------------------------

/// An embedded database calling `root`'s functions of a fresh registry.
#[cfg(feature = "pfn-wasm")]
fn embedded(tag: &str, data: &str) -> (Database, Arc<PfnRegistry>, PathBuf) {
    let base = tmp_root(tag);
    let reg = Arc::new(PfnRegistry::new(&base));
    let mut db = Database::build_from_str(tag, data).unwrap();
    db.set_pfn_caller(Some(PfnCaller::new(Arc::clone(&reg), "root")));
    (db, reg, base)
}

#[cfg(feature = "pfn-wasm")]
fn define(reg: &PfnRegistry, user: &str, name: &str, subs: &str, body: &str) {
    let info = PfnInfo {
        name: name.into(),
        body: quote_plus(body),
        subs: quote_plus(subs),
        ..PfnInfo::default()
    };
    match reg.get(user, name).unwrap() {
        Some(_) => reg.update(user, info).unwrap(),
        None => reg.create(user, info).unwrap(),
    };
    let r = reg.compile(user, name).unwrap();
    assert!(r.is_ok(), "compile {name}: {r:?}");
}

/// The lexical value of a lone PFN projection: on the only row, or — over
/// solutions, one row each (upstream's shape) — on the last row, the others
/// holding none.
#[cfg(feature = "pfn-wasm")]
fn value_of(r: gstore::Result<QueryResult>) -> String {
    let rs = match r.unwrap() {
        QueryResult::Select(rs) => rs,
        other => panic!("not a SELECT result: {other:?}"),
    };
    let (last, others) = rs
        .rows
        .split_last()
        .unwrap_or_else(|| panic!("no row: {rs:?}"));
    assert!(
        others.iter().all(|r| r.iter().all(Option::is_none)),
        "{rs:?}"
    );
    let cell = last.iter().flatten().next().expect("a value").clone();
    gstore::sparql_results::upstream_binding(&cell)
        .get("value")
        .and_then(Json::as_str)
        .unwrap()
        .to_string()
}

#[cfg(feature = "pfn-wasm")]
fn value_json(r: gstore::Result<QueryResult>) -> Json {
    let v = value_of(r);
    json::parse(&v).unwrap_or_else(|e| panic!("not JSON ({e}): {v}"))
}

#[cfg(feature = "pfn-wasm")]
#[test]
fn doc_examples_agree_with_an_independent_computation() {
    let (nt, edges) = graph();
    let (mut db, reg, _base) = embedded("pfn_examples", &nt);
    define(
        &reg,
        "root",
        "outDegree",
        &doc_block("funSubs=outDegree"),
        &doc_block("funBody=outDegree"),
    );
    define(
        &reg,
        "root",
        "kHopCount",
        &doc_block("funSubs=kHopCount"),
        &doc_block("funBody=kHopCount"),
    );

    let mut srcs: Vec<String> = (0..24).map(|i| format!("<{EX}v{i}>")).collect();
    srcs.push(format!("<{EX}T>"));
    srcs.push(format!("<{EX}lonely>"));
    srcs.push(format!("<{EX}unknown>"));
    let preds = [
        None,
        Some("<http://ex/p0>"),
        Some("<http://ex/p2>"),
        Some("<http://ex/none>"),
    ];
    for src in &srcs {
        for pred in preds {
            let arg = match pred {
                None => format!(r#"{{"src":"{src}"}}"#),
                Some(p) => format!(r#"{{"src":"{src}","pred":"{p}"}}"#),
            };
            let got =
                value_json(db.query(&format!("SELECT (PFN.outDegree({arg}) AS ?r) WHERE {{}}")));
            let want = Json::obj()
                .with("src", src.as_str())
                .with("outDegree", expected_degree(&edges, src, pred));
            assert_eq!(got.sorted(), want.sorted(), "{arg}");
        }
        for k in 0..4i64 {
            for filter in [
                &[][..],
                &["<http://ex/p0>"][..],
                &["<http://ex/p1>", "<http://ex/p2>"][..],
                &["<http://ex/none>"][..],
            ] {
                // (upstream's jsonVars grammar has no empty array: no
                // filter = no `preds`)
                let list: Vec<String> = filter.iter().map(|p| format!("\"{p}\"")).collect();
                let arg = if filter.is_empty() {
                    format!(r#"{{"src":"{src}","k":"{k}"}}"#)
                } else {
                    format!(
                        r#"{{"src":"{src}","k":"{k}","preds":[{}]}}"#,
                        list.join(",")
                    )
                };
                let got = value_json(
                    db.query(&format!("SELECT (PFN.kHopCount({arg}) AS ?r) WHERE {{}}")),
                );
                assert_eq!(
                    got.get("count").and_then(Json::as_i64),
                    Some(expected_khop(&edges, src, k, filter)),
                    "{arg}: {got}"
                );
                assert_eq!(got.get("k").and_then(Json::as_i64), Some(k));
            }
        }
    }
    // `k` as a JSON number, `preds` absent: every predicate.
    let got = value_json(db.query(&format!(
        r#"SELECT (PFN.kHopCount({{"src":"<{EX}v0>","k":"3"}}) AS ?r) WHERE {{}}"#
    )));
    assert_eq!(
        got.get("count").and_then(Json::as_i64),
        Some(expected_khop(&edges, &format!("<{EX}v0>"), 3, &[]))
    );

    // Variables: one call per distinct binding, in the order of the
    // canonical argument texts, results as one JSON array.
    let got = value_json(db.query(&format!(
        r#"SELECT (PFN.kHopCount({{"src":"?x","k":"2"}}) AS ?r) WHERE {{ ?x <{EX}type> <{EX}T> }}"#
    )));
    let typed: BTreeSet<String> = edges
        .iter()
        .filter(|(_, p, _)| p == &format!("<{EX}type>"))
        .map(|(s, _, _)| s.clone())
        .collect();
    let mut want_order: Vec<String> = typed.iter().cloned().collect();
    want_order.sort_by_key(|s| format!(r#"{{"k":"2","src":"{s}"}}"#));
    let items = got.as_array().expect("an array").to_vec();
    assert_eq!(items.len(), typed.len(), "{got}");
    for (item, src) in items.iter().zip(&want_order) {
        assert_eq!(
            item.get("src").and_then(Json::as_str),
            Some(src.as_str()),
            "{got}"
        );
        assert_eq!(
            item.get("count").and_then(Json::as_i64),
            Some(expected_khop(&edges, src, 2, &[])),
            "{src}"
        );
    }
    // Next to another projection the value is on the last row only.
    let rs = match db
        .query(&format!(
            r#"SELECT ?x (PFN.outDegree({{"src":"?x"}}) AS ?r) WHERE {{ ?x <{EX}type> <{EX}T> }}"#
        ))
        .unwrap()
    {
        QueryResult::Select(rs) => rs,
        _ => unreachable!(),
    };
    assert_eq!(rs.rows.len(), typed.len());
    let bound: Vec<usize> = rs
        .rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r[1].is_some())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(bound, [typed.len() - 1]);
    // A WHERE variable without a binding: nothing runs.
    let got = value_json(db.query(&format!(
        r#"SELECT (PFN.outDegree({{"src":"?x"}}) AS ?r)
           WHERE {{ ?y <{EX}p0> ?z OPTIONAL {{ ?y <{EX}nothing> ?x }} }}"#
    )));
    assert_eq!(got, Json::Arr(vec![]));
    // No solution at all: one call with the argument as written (upstream's
    // `pfnQuery`), its output as is.
    let got = value_json(db.query(&format!(
        r#"SELECT (PFN.outDegree({{"src":"?x"}}) AS ?r) WHERE {{ ?x <{EX}type> <{EX}Nothing> }}"#
    )));
    assert_eq!(
        got.sorted(),
        Json::obj().with("outDegree", 0).with("src", "?x")
    );
}

#[cfg(feature = "pfn-wasm")]
#[test]
fn api_query_answers_upstreams_results_document_as_the_caller() {
    let (srv, addr, _root) = start("apiquery", None);
    let (nt, edges) = graph();
    build(&addr, "pfng", &nt);
    add_user(&addr, "alice", "query");
    let subs = quote_plus(&doc_block("funSubs=outDegree"));
    let body = quote_plus(&doc_block("funBody=outDegree"));
    for user in ["root", "alice"] {
        assert_eq!(
            code(&funcudb(
                &addr,
                user,
                "1",
                fun_info("outDegree", "", &body, &subs)
            )),
            0
        );
        assert_eq!(
            code(&funcudb(
                &addr,
                user,
                "4",
                Json::obj().with("funName", "outDegree")
            )),
            0
        );
    }
    // alice's own function with a name root does not have.
    let answer = "(call $emit (i32.const 0) (i32.const 5)) (i32.const 1)";
    let j = funcudb(
        &addr,
        "alice",
        "1",
        fun_info("hers", "", answer, r#"(data (i32.const 0) "alice")"#),
    );
    assert_eq!(code(&j), 0);
    assert_eq!(
        code(&funcudb(
            &addr,
            "alice",
            "4",
            Json::obj().with("funName", "hers")
        )),
        0
    );
    let query = |user: &str, sparql: &str| {
        api_as(
            &addr,
            user,
            Json::obj()
                .with("operation", "query")
                .with("db_name", "pfng")
                .with("sparql", sparql),
        )
    };
    // `select (pfn.` form: upstream drops AnsNum, OutputLimit, ThreadId.
    let src = format!("<{EX}v2>");
    let j = query(
        "root",
        &format!(r#"select (PFN.outDegree({{"src":"{src}"}}) AS ?r) WHERE {{}}"#),
    );
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!(
        j.get("results").map(Json::sorted),
        Some(
            Json::obj()
                .with("outDegree", expected_degree(&edges, &src, None))
                .with("src", src.as_str())
        )
    );
    assert!(
        j.get("head").is_none() && j.get("AnsNum").is_none() && j.get("ThreadId").is_none(),
        "{j}"
    );
    assert!(j.get("QueryTime").is_some());
    // Other spellings keep them; variables give an array.
    let j = query(
        "root",
        &format!(
            r#"PREFIX ex: <{EX}> SELECT (PFN outDegree({{"src":"?x"}}) AS ?r) WHERE {{ ?x ex:type ex:T }}"#
        ),
    );
    assert_eq!(code(&j), 0, "{j}");
    // One row per solution, the value on the last (upstream's shape for a
    // PFN call, unlike a lone graph function's one row).
    assert_eq!(j.get("AnsNum").and_then(Json::as_i64), Some(6));
    assert!(j.get("OutputLimit").is_some());
    let arr = j.get("results").and_then(Json::as_array).expect("array");
    assert_eq!(arr.len(), 6, "{j}");
    // Each user calls their own functions.
    let j = query("alice", r#"SELECT (PFN.hers({}) AS ?r) WHERE {}"#);
    assert_eq!(j.get("results"), Some(&Json::Str("alice".into())), "{j}");
    let j = query("root", r#"SELECT (PFN.hers({}) AS ?r) WHERE {}"#);
    assert_eq!(code(&j), 1005, "{j}");
    assert!(
        msg(&j).contains("abort function 'hers' is not exist"),
        "{j}"
    );
    // REST /sparql: a plain literal in standard SPARQL JSON.
    let r = send(
        &addr,
        "POST",
        "/sparql?db_name=pfng",
        &[
            ("Authorization", &basic("alice")),
            ("Accept", "application/sparql-results+json"),
        ],
        br#"SELECT (PFN.hers({}) AS ?r) WHERE {}"#,
    );
    assert_eq!(r.status, 200, "{}", r.body);
    assert!(r.body.contains("\"alice\""), "{}", r.body);
    srv.shutdown();
}

#[cfg(feature = "pfn-wasm")]
#[test]
fn an_endless_loop_is_stopped_by_the_instruction_budget() {
    let (mut db, reg, _base) =
        embedded("pfn_fuel", "<http://ex/a> <http://ex/p> <http://ex/b> .\n");
    define(&reg, "root", "spin", "", "(loop $l (br $l)) (i32.const 1)");
    db.set_pfn_limits(PfnLimits {
        fuel_per_call: 3_000_000,
        ..PfnLimits::default()
    });
    let started = Instant::now();
    let e = db
        .query(r#"SELECT (PFN.spin({}) AS ?v) WHERE {}"#)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("run spin fail") && e.contains("instruction budget of 3000000"),
        "{e}"
    );
    assert!(started.elapsed() < Duration::from_secs(60));
    // A trap is a clean error too, and so is a function reporting failure.
    define(&reg, "root", "trap", "", "unreachable");
    let e = db
        .query(r#"SELECT (PFN.trap({}) AS ?v) WHERE {}"#)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("run trap fail") && e.contains("unreachable"),
        "{e}"
    );
    define(
        &reg,
        "root",
        "nope",
        r#"(data (i32.const 0) "no such vertex")"#,
        "(call $emit (i32.const 0) (i32.const 14)) (i32.const 0)",
    );
    let e = db
        .query(r#"SELECT (PFN.nope({}) AS ?v) WHERE {}"#)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("returned 0") && e.contains("no such vertex"),
        "{e}"
    );
    define(
        &reg,
        "root",
        "deep",
        "(func $r (result i32) (call $r))",
        "(call $r)",
    );
    let e = db
        .query(r#"SELECT (PFN.deep({}) AS ?v) WHERE {}"#)
        .unwrap_err()
        .to_string();
    assert!(e.contains("run deep fail"), "{e}");
}

#[cfg(feature = "pfn-wasm")]
#[test]
fn an_endless_loop_is_stopped_by_operation_task_cancel() {
    let settings = QuerySettings {
        pfn: PfnLimits {
            fuel_per_call: 0,
            ..PfnLimits::default()
        },
        ..QuerySettings::default()
    };
    let (srv, addr, _root) = start("cancel", Some(settings));
    build(
        &addr,
        "pfnc",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    );
    assert_eq!(
        code(&funcudb(
            &addr,
            "root",
            "1",
            fun_info("spin", "", "(loop $l (br $l)) (i32.const 1)", "")
        )),
        0
    );
    assert_eq!(
        code(&funcudb(
            &addr,
            "root",
            "4",
            Json::obj().with("funName", "spin")
        )),
        0
    );
    let a2 = addr;
    let running = thread::spawn(move || {
        api(
            &a2,
            Json::obj()
                .with("operation", "query")
                .with("db_name", "pfnc")
                .with("sparql", r#"SELECT (PFN.spin({}) AS ?v) WHERE {}"#),
        )
    });
    // Wait until the statement shows as running, then cancel it.
    let until = Instant::now() + Duration::from_secs(60);
    let opt_id = loop {
        let j = api(&addr, Json::obj().with("operation", "operationTaskList"));
        let found = j
            .get("list")
            .and_then(Json::as_array)
            .unwrap_or_default()
            .iter()
            .find(|t| t.get("operation").and_then(Json::as_str) == Some("query"))
            .and_then(|t| t.get("opt_id").and_then(Json::as_str).map(str::to_string));
        if let Some(id) = found {
            break id;
        }
        assert!(
            Instant::now() < until,
            "the query never showed up as running"
        );
        thread::sleep(Duration::from_millis(10));
    };
    // It keeps running (no budget) until cancelled.
    thread::sleep(Duration::from_millis(200));
    assert!(!running.is_finished());
    let j = api(
        &addr,
        Json::obj()
            .with("operation", "operationTaskCancel")
            .with("opt_id", opt_id.as_str()),
    );
    assert_eq!(code(&j), 0, "{j}");
    let answer = running.join().unwrap();
    assert_ne!(code(&answer), 0, "{answer}");
    assert!(msg(&answer).to_lowercase().contains("cancel"), "{answer}");
    // The server goes on answering.
    let j = funquery(&addr, "root", &[]);
    assert_eq!(list_of(&j).len(), 1);
    srv.shutdown();
}

#[cfg(feature = "pfn-wasm")]
#[test]
fn memory_is_capped_and_guest_pointers_are_checked() {
    let (mut db, reg, _base) = embedded("pfn_mem", "<http://ex/a> <http://ex/p> <http://ex/b> .\n");
    db.set_pfn_limits(PfnLimits {
        max_memory_pages: 4,
        ..PfnLimits::default()
    });
    // memory.grow past the cap fails (-1), within it succeeds.
    define(
        &reg,
        "root",
        "grow",
        "",
        "(call $emit_i64 (i64.extend_i32_s (memory.grow (i32.const 10))))\n\
         (call $emit_i64 (i64.extend_i32_s (memory.grow (i32.const 3))))\n\
         (call $emit_i64 (i64.extend_i32_s (memory.size)))\n\
         (i32.const 1)",
    );
    assert_eq!(
        value_of(db.query(r#"SELECT (PFN.grow({}) AS ?v) WHERE {}"#)),
        "-114"
    );
    // Writing past the memory traps cleanly.
    define(
        &reg,
        "root",
        "poke",
        "",
        "(i32.store (i32.const 70000) (i32.const 1)) (i32.const 1)",
    );
    let e = db
        .query(r#"SELECT (PFN.poke({}) AS ?v) WHERE {}"#)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("run poke fail") && e.contains("out of bounds"),
        "{e}"
    );
    // A module declaring more memory than allowed does not instantiate.
    define(
        &reg,
        "root",
        "big",
        "",
        r#"(module (memory (export "memory") 10) (func (export "big") (result i32) (i32.const 1)))"#,
    );
    let e = db
        .query(r#"SELECT (PFN.big({}) AS ?v) WHERE {}"#)
        .unwrap_err()
        .to_string();
    assert!(e.contains("run big fail"), "{e}");
    // Bad pointers and lengths handed to the host are clean errors.
    for (name, body) in [
        ("oob1", "(call $emit (i32.const 65530) (i32.const 100)) (i32.const 1)"),
        ("oob2", "(call $emit (i32.const 0) (i32.const -1)) (i32.const 1)"),
        ("oob3", "(drop (call $term_str (i64.const 0) (i32.const 65535) (i32.const 10))) (i32.const 1)"),
        ("oob4", "(drop (call $vertex_id (i32.const -8) (i32.const 4))) (i32.const 1)"),
        ("oob5", "(drop (call $arg_str (i32.const 0) (i32.const 70000) (i32.const 0) (i32.const 0))) (i32.const 1)"),
    ] {
        define(&reg, "root", name, "", body);
        let e = db.query(&format!("SELECT (PFN.{name}({{}}) AS ?v) WHERE {{}}")).unwrap_err().to_string();
        assert!(e.contains(&format!("run {name} fail")) && e.contains("outside the module's memory"), "{name}: {e}");
    }
    // Invalid UTF-8 output is refused.
    define(
        &reg,
        "root",
        "bytes",
        r#"(data (i32.const 0) "\ff\fe")"#,
        "(call $emit (i32.const 0) (i32.const 2)) (i32.const 1)",
    );
    let e = db
        .query(r#"SELECT (PFN.bytes({}) AS ?v) WHERE {}"#)
        .unwrap_err()
        .to_string();
    assert!(e.contains("not valid UTF-8"), "{e}");
}

#[cfg(feature = "pfn-wasm")]
#[test]
fn output_is_bounded_by_gfun_max_output_bytes() {
    let (mut db, reg, _base) = embedded(
        "pfn_out",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n<http://ex/b> <http://ex/p> <http://ex/a> .\n",
    );
    db.set_gfun_max_output_bytes(64);
    define(
        &reg,
        "root",
        "chatty",
        r#"(data (i32.const 0) "0123456789")"#,
        "(local $i i32)\n\
         (block $done (loop $l\n\
           (br_if $done (i32.ge_u (local.get $i) (i32.const 10)))\n\
           (call $emit (i32.const 0) (i32.const 10))\n\
           (local.set $i (i32.add (local.get $i) (i32.const 1)))\n\
           (br $l)))\n\
         (i32.const 1)",
    );
    let e = db
        .query(r#"SELECT (PFN.chatty({}) AS ?v) WHERE {}"#)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("exceeds 64 bytes") && e.contains("gfun_max_output_bytes"),
        "{e}"
    );
    // Within the bound per call, but the result array of many calls is not.
    define(&reg, "root", "forty", r#"(data (i32.const 0) "0123456789")"#,
        "(call $emit (i32.const 0) (i32.const 10)) (call $emit (i32.const 0) (i32.const 10)) \
         (call $emit (i32.const 0) (i32.const 10)) (call $emit (i32.const 0) (i32.const 10)) (i32.const 1)");
    assert_eq!(
        value_of(db.query(r#"SELECT (PFN.forty({}) AS ?v) WHERE {}"#)).len(),
        40
    );
    let e = db
        .query(r#"SELECT (PFN.forty({"s":"?s"}) AS ?v) WHERE { ?s <http://ex/p> ?o }"#)
        .unwrap_err()
        .to_string();
    assert!(e.contains("exceed 64 bytes"), "{e}");
}

#[cfg(feature = "pfn-wasm")]
#[test]
fn a_recompiled_function_is_never_answered_from_a_cache() {
    let (mut db, reg, _base) =
        embedded("pfn_cache", "<http://ex/a> <http://ex/p> <http://ex/b> .\n");
    let q = r#"SELECT (PFN.ver({}) AS ?v) WHERE {}"#;
    define(
        &reg,
        "root",
        "ver",
        "",
        "(call $emit_i64 (i64.const 7)) (i32.const 1)",
    );
    assert_eq!(value_of(db.query(q)), "7");
    assert_eq!(value_of(db.query(q)), "7");
    define(
        &reg,
        "root",
        "ver",
        "",
        "(call $emit_i64 (i64.const 8)) (i32.const 1)",
    );
    assert_eq!(value_of(db.query(q)), "8", "recompiled");
    // Updated but not compiled: not callable.
    reg.update(
        "root",
        PfnInfo {
            name: "ver".into(),
            body: "(i32.const 1)".into(),
            ..PfnInfo::default()
        },
    )
    .unwrap();
    let e = db.query(q).unwrap_err().to_string();
    assert!(e.contains("not compile yet"), "{e}");
    reg.delete("root", "ver").unwrap();
    let e = db.query(q).unwrap_err().to_string();
    assert!(e.contains("is not exist"), "{e}");

    // The same through the server's query cache.
    let (srv, addr, _root) = start("cacheapi", None);
    build(
        &addr,
        "pfnk",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    );
    let set = |n: i64| {
        let body = format!("(call $emit_i64 (i64.const {n})) (i32.const 1)");
        let kind = if n == 1 { "1" } else { "2" };
        assert_eq!(
            code(&funcudb(
                &addr,
                "root",
                kind,
                fun_info("ver", "", &body, "")
            )),
            0
        );
        assert_eq!(
            code(&funcudb(
                &addr,
                "root",
                "4",
                Json::obj().with("funName", "ver")
            )),
            0
        );
    };
    let ask = || {
        let j = api(
            &addr,
            Json::obj()
                .with("operation", "query")
                .with("db_name", "pfnk")
                .with("sparql", q),
        );
        j.get("results").cloned().unwrap_or(Json::Null)
    };
    set(1);
    assert_eq!(ask(), Json::Num("1".into()));
    assert_eq!(ask(), Json::Num("1".into()));
    set(2);
    assert_eq!(ask(), Json::Num("2".into()));
    srv.shutdown();
}

#[cfg(feature = "pfn-wasm")]
#[test]
fn snapshots_carry_the_registry() {
    // ConcurrentDb: snapshots, transactions and version views.
    let (db, reg, _base) = embedded("pfn_conc", "<http://ex/a> <http://ex/p> <http://ex/b> .\n");
    define(
        &reg,
        "root",
        "edges",
        "",
        "(call $emit_i64 (call $edge_count)) (i32.const 1)",
    );
    let c = gstore::ConcurrentDb::new(db);
    let q = r#"SELECT (PFN.edges({}) AS ?v) WHERE {}"#;
    assert_eq!(value_of(c.query(q)), "1");
    assert_eq!(value_of(c.snapshot().query(q)), "1");
    assert_eq!(value_of(c.begin().query(q)), "1");
    assert_eq!(value_of(c.version_view().query(q)), "1");
    c.update("INSERT DATA { <http://ex/b> <http://ex/p> <http://ex/c> }")
        .unwrap();
    assert_eq!(value_of(c.query(q)), "2");

    // HTTP: a read during a transaction is answered from its snapshot, as
    // the requesting user; a transactional read too.
    let (srv, addr, _root) = start("txnsnap", None);
    build(
        &addr,
        "pfnt",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    );
    add_user(&addr, "alice", "query");
    for (user, n) in [("root", 10), ("alice", 20)] {
        let body =
            format!("(call $emit_i64 (i64.add (i64.const {n}) (call $edge_count))) (i32.const 1)");
        assert_eq!(
            code(&funcudb(&addr, user, "1", fun_info("count", "", &body, ""))),
            0
        );
        assert_eq!(
            code(&funcudb(
                &addr,
                user,
                "4",
                Json::obj().with("funName", "count")
            )),
            0
        );
    }
    let j = api(
        &addr,
        Json::obj()
            .with("operation", "begin")
            .with("db_name", "pfnt"),
    );
    assert_eq!(code(&j), 0, "{j}");
    let tid = field(&j, "TID");
    let j = api(
        &addr,
        Json::obj()
            .with("operation", "tquery")
            .with("db_name", "pfnt")
            .with("tid", tid.as_str())
            .with(
                "sparql",
                "INSERT DATA { <http://ex/b> <http://ex/p> <http://ex/c> }",
            ),
    );
    assert_eq!(code(&j), 0, "{j}");
    let read = |user: &str| {
        let j = api_as(
            &addr,
            user,
            Json::obj()
                .with("operation", "query")
                .with("db_name", "pfnt")
                .with("sparql", r#"SELECT (PFN.count({}) AS ?v) WHERE {}"#),
        );
        assert_eq!(code(&j), 0, "{j}");
        j.get("results").cloned().unwrap()
    };
    // The snapshot (one edge) for both users, each with their own function.
    assert_eq!(read("root"), Json::Num("11".into()));
    assert_eq!(read("alice"), Json::Num("21".into()));
    // Inside the transaction: its own write is visible.
    let j = api(
        &addr,
        Json::obj()
            .with("operation", "tquery")
            .with("db_name", "pfnt")
            .with("tid", tid.as_str())
            .with("sparql", r#"SELECT (PFN.count({}) AS ?v) WHERE {}"#),
    );
    assert_eq!(code(&j), 0, "{j}");
    assert!(j.to_string().contains("12"), "{j}");
    let j = api(
        &addr,
        Json::obj()
            .with("operation", "rollback")
            .with("db_name", "pfnt")
            .with("tid", tid.as_str()),
    );
    assert_eq!(code(&j), 0, "{j}");
    srv.shutdown();
}

#[cfg(feature = "pfn-wasm")]
#[test]
fn the_host_interface_reads_the_graph_and_arguments() {
    let (nt, _edges) = graph();
    let (mut db, reg, _base) = embedded("pfn_host", &nt);
    // Arguments: strings, integers, booleans, arrays; the canonical text.
    define(
        &reg,
        "root",
        "args",
        r#"(data (i32.const 0) "k") (data (i32.const 8) "flag") (data (i32.const 16) "list") (data (i32.const 32) "|")"#,
        "(local $n i32)\n\
         (call $emit_i64 (call $arg_int (i32.const 0) (i32.const 1) (i64.const -1)))\n\
         (call $emit (i32.const 32) (i32.const 1))\n\
         (call $emit_i64 (i64.extend_i32_s (call $arg_bool (i32.const 8) (i32.const 4) (i32.const 7))))\n\
         (call $emit (i32.const 32) (i32.const 1))\n\
         (call $emit_i64 (i64.extend_i32_s (call $arg_len (i32.const 16) (i32.const 4))))\n\
         (call $emit (i32.const 32) (i32.const 1))\n\
         (local.set $n (call $arg_item (i32.const 16) (i32.const 4) (i32.const 1) (i32.const 1024) (i32.const 100)))\n\
         (call $emit (i32.const 1024) (local.get $n))\n\
         (call $emit (i32.const 32) (i32.const 1))\n\
         (local.set $n (call $args (i32.const 1024) (i32.const 1000)))\n\
         (call $emit (i32.const 1024) (local.get $n))\n\
         (i32.const 1)",
    );
    let v =
        value_of(db.query(
            r#"SELECT (PFN.args({"list":["a","<b>"],"k":"42","flag":"true"}) AS ?v) WHERE {}"#,
        ));
    assert_eq!(
        v,
        r#"42|1|2|<b>|{"flag":"true","k":"42","list":["a","<b>"]}"#
    );
    // Vertices, terms, predicates and neighbours, round-tripped.
    define(
        &reg,
        "root",
        "walk",
        r#"(data (i32.const 0) "<http://ex/v0>") (data (i32.const 32) ",")"#,
        "(local $v i64) (local $w i64) (local $n i32)\n\
         (local.set $v (call $vertex_id (i32.const 0) (i32.const 14)))\n\
         ;; the vertex's string, via its term\n\
         (local.set $n (call $term_str (call $vertex_term (local.get $v)) (i32.const 1024) (i32.const 100)))\n\
         (call $emit (i32.const 1024) (local.get $n))\n\
         (call $emit (i32.const 32) (i32.const 1))\n\
         ;; its first out-edge: neighbour and predicate strings\n\
         (local.set $w (call $out_neighbor (local.get $v) (i64.const -1) (i64.const 0)))\n\
         (local.set $n (call $vertex_str (local.get $w) (i32.const 1024) (i32.const 100)))\n\
         (call $emit (i32.const 1024) (local.get $n))\n\
         (call $emit (i32.const 32) (i32.const 1))\n\
         (local.set $n (call $pred_str (call $out_edge_pred (local.get $v) (i64.const 0)) (i32.const 1024) (i32.const 100)))\n\
         (call $emit (i32.const 1024) (local.get $n))\n\
         (call $emit (i32.const 32) (i32.const 1))\n\
         ;; the neighbour sees v among its in-neighbours\n\
         (call $emit_i64 (call $in_degree (local.get $w) (i64.const -1)))\n\
         (call $emit (i32.const 32) (i32.const 1))\n\
         ;; out of range: -1\n\
         (call $emit_i64 (call $out_degree (i64.const 99999) (i64.const -1)))\n\
         (call $emit (i32.const 32) (i32.const 1))\n\
         (call $emit_i64 (i64.extend_i32_s (call $term_str (i64.const -5) (i32.const 1024) (i32.const 10))))\n\
         (call $emit (i32.const 32) (i32.const 1))\n\
         ;; a truncated string reports its full length\n\
         (call $emit_i64 (i64.extend_i32_s (call $vertex_str (local.get $v) (i32.const 1024) (i32.const 3))))\n\
         (call $emit (i32.const 32) (i32.const 1))\n\
         (call $emit_f64 (f64.const 0.5))\n\
         (i32.const 1)",
    );
    let v = value_of(db.query(r#"SELECT (PFN.walk({}) AS ?v) WHERE {}"#));
    let parts: Vec<&str> = v.split(',').collect();
    assert_eq!(parts[0], "<http://ex/v0>", "{v}");
    // v0's out-edges: p0 -> v3, p1 -> v1, p2 -> v1, type -> T; the first by
    // (predicate id, neighbour) is the predicate interned first.
    assert!(parts[1].starts_with("<http://ex/"), "{v}");
    assert!(parts[2].starts_with("<http://ex/"), "{v}");
    assert!(parts[3].parse::<i64>().unwrap() >= 1, "{v}");
    assert_eq!(&parts[4..], ["-1", "-1", "14", "0.5"], "{v}");
    define(
        &reg,
        "root",
        "counts",
        r#"(data (i32.const 0) ",")"#,
        "(call $emit_i64 (call $vertex_count)) (call $emit (i32.const 0) (i32.const 1))\n\
         (call $emit_i64 (call $edge_count)) (call $emit (i32.const 0) (i32.const 1))\n\
         (call $emit_i64 (call $pred_count)) (call $emit (i32.const 0) (i32.const 1))\n\
         (call $emit_i64 (call $pred_at (i64.const 99)))\n\
         (i32.const 1)",
    );
    let counts = value_of(db.query(r#"SELECT (PFN.counts({}) AS ?v) WHERE {}"#));
    let (_, edges) = graph();
    let vertices: BTreeSet<&String> = edges.iter().flat_map(|(s, _, o)| [s, o]).collect();
    let preds: BTreeSet<&String> = edges.iter().map(|(_, p, _)| p).collect();
    assert_eq!(
        counts,
        format!("{},{},{},-1", vertices.len(), edges.len(), preds.len())
    );
}

#[cfg(feature = "pfn-wasm")]
#[test]
fn modules_may_be_complete_or_binary_but_only_import_the_host() {
    let (mut db, reg, _base) =
        embedded("pfn_forms", "<http://ex/a> <http://ex/p> <http://ex/b> .\n");
    let module = r#"(module
        (import "gstore" "emit_i64" (func $out (param i64)))
        (memory (export "memory") 1)
        (func (export "whole") (result i32) (call $out (i64.const 5)) (i32.const 1)))"#;
    define(&reg, "root", "whole", "", module);
    assert_eq!(
        value_of(db.query(r#"SELECT (PFN.whole({}) AS ?v) WHERE {}"#)),
        "5"
    );
    // The same module in binary form, base64 (URL-decoding a '+' gives a
    // space, which the decoder reads as '+').
    let wasm: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic, version
        0x01, 0x09, 0x02, 0x60, 0x01, 0x7e, 0x00, 0x60, 0x00, 0x01, 0x7f, // types
        0x02, 0x13, 0x01, 0x06, b'g', b's', b't', b'o', b'r', b'e', 0x08, b'e', b'm', b'i', b't',
        b'_', b'i', b'6', b'4', 0x00, 0x00, // import gstore.emit_i64
        0x03, 0x02, 0x01, 0x01, // func 1: type 1
        0x05, 0x03, 0x01, 0x00, 0x01, // memory 1
        0x07, 0x10, 0x02, 0x06, b'm', b'e', b'm', b'o', b'r', b'y', 0x02, 0x00, 0x03, b'b', b'i',
        b'n', 0x00, 0x01, // exports
        0x0a, 0x0a, 0x01, 0x08, 0x00, 0x42, 0x06, 0x10, 0x00, 0x41, 0x01, 0x0b, // code
    ];
    let b64 = {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in wasm.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let v = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            for i in 0..4 {
                out.push(if i <= chunk.len() {
                    A[((v >> (18 - 6 * i)) & 63) as usize] as char
                } else {
                    '='
                });
            }
        }
        out
    };
    define(&reg, "root", "bin", "", &format!("wasm-base64:{b64}"));
    assert_eq!(
        value_of(db.query(r#"SELECT (PFN.bin({}) AS ?v) WHERE {}"#)),
        "6"
    );
    // WASI and friends are refused at compile time.
    reg.create(
        "root",
        PfnInfo {
            name: "wasi".into(),
            body: quote_plus(
                r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32)))
                   (memory (export "memory") 1) (func (export "wasi") (result i32) (i32.const 1)))"#,
            ),
            ..PfnInfo::default()
        },
    )
    .unwrap();
    let e = reg.compile("root", "wasi").unwrap().unwrap_err();
    assert!(e.contains("is not available"), "{e}");
}

/// `conf.ini`: upstream's `[system] pfn_base_path` places the functions,
/// and the `[pfn]` limits are what create checks.
#[test]
fn conf_ini_places_and_limits_the_functions() {
    let root = tmp_root("conf");
    let base = root.join("custom_pfn");
    let config = gstore::config::GStoreConfig::from_ini_str(&format!(
        "[system]\npfn_base_path={}\n[pfn]\npfn_max_functions=1\npfn_max_module_bytes=200000\n",
        base.display()
    ));
    assert!(config.warnings().is_empty(), "{:?}", config.warnings());
    let server = ApiServer::bind(&root, "root", "127.0.0.1:0")
        .unwrap()
        .with_config(config);
    assert_eq!(server.pfn_registry().base_dir(), base.as_path());
    assert_eq!(server.pfn_registry().limits().max_functions, 1);
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || {
        s2.serve_forever();
    });
    let body = "(i32.const 1)";
    let j = funcudb(&addr, "root", "1", fun_info("first", "", body, ""));
    assert_eq!(code(&j), 0, "{j}");
    assert!(base.join("src/root/first.wat").is_file());
    assert!(!root.join("pfn").exists(), "not under the server root");
    let j = funcudb(&addr, "root", "1", fun_info("second", "", body, ""));
    assert_eq!(code(&j), 1005);
    assert!(msg(&j).contains("pfn_max_functions = 1"), "{j}");
    server.shutdown();
}
