//! Shared harness of the upstream conformance tests (`tests/upstream_*.rs`).
//!
//! Every test drives an in-process [`ApiServer`] the way upstream gStore 1.4's
//! test scripts (`scripts/test/*.sh`) drive `bin/gserver`: `POST /api` with a
//! JSON body carrying `operation`, `username` / `password` (`root` /
//! `123456`, upstream's defaults) and the operation's parameters. The
//! upstream assets are the ones vendored under `testdata/upstream/` (see its
//! `README.md`); LUBM's 12 MB `lubm.nt` is the identical
//! `testdata/lubm/lubm.nt`.
//!
//! Included with `#[path = "upstream_scripts/harness.rs"] mod harness;` (Cargo
//! does not build files in a `tests/` subdirectory as test targets).

#![allow(dead_code)] // each test target uses a subset

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::LoadPolicy;

/// Upstream's root credentials (`conf.ini` `root_password`, the scripts'
/// `USERNAME` / `PASSWORD`).
pub const USERNAME: &str = "root";
pub const PASSWORD: &str = "123456";

/// The crate root.
pub fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `testdata/upstream/<rel>` (upstream's `GSTORE_HOME`-relative layout:
/// `data/…`, `scripts/test/…`).
pub fn upstream(rel: &str) -> PathBuf {
    manifest_dir().join("testdata/upstream").join(rel)
}

/// Upstream's `data/<dataset>/<file>`. `data/lubm/lubm.nt` is not vendored a
/// second time: it is byte-identical to `testdata/lubm/lubm.nt`.
pub fn data(rel: &str) -> PathBuf {
    if rel == "lubm/lubm.nt" {
        return manifest_dir().join("testdata/lubm/lubm.nt");
    }
    upstream("data").join(rel)
}

/// A path as the string a request carries.
pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Read a vendored text file.
pub fn read(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// A fresh scratch directory under Cargo's per-target temporary directory.
pub fn scratch(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("upstream_{tag}_{}_{n}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    fs::canonicalize(&d).unwrap()
}

/// A scratch directory with a fixed name (emptied first), for state shared
/// by a whole test binary (statics are never dropped, so a per-process name
/// would pile up a directory per run).
pub fn scratch_fixed(tag: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("upstream_{tag}"));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    fs::canonicalize(&d).unwrap()
}

/// A running in-process server (stopped, and its root removed, on drop).
pub struct Server {
    server: Arc<ApiServer>,
    pub addr: SocketAddr,
    pub root: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.server.shutdown();
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Server {
    /// A server rooted in a scratch directory, with upstream's root password,
    /// whose `build` / `batchInsert` / `batchRemove` may read the vendored
    /// test data (and anything else under `testdata/` or `extra_dir`).
    pub fn start(tag: &str) -> Server {
        Server::start_with_dir(tag, None)
    }

    pub fn start_with_dir(tag: &str, extra_dir: Option<&Path>) -> Server {
        Server::start_at(scratch(tag), extra_dir)
    }

    /// [`start_with_dir`](Self::start_with_dir) rooted at `root`.
    pub fn start_at(root: PathBuf, extra_dir: Option<&Path>) -> Server {
        let mut config = GStoreConfig::default();
        config.ip_deny_path = None;
        config.thread_num = 4;
        let mut policy = LoadPolicy::restricted().with_allowed_dir(manifest_dir().join("testdata"));
        if let Some(d) = extra_dir {
            policy = policy.with_allowed_dir(d);
        }
        let server = ApiServer::bind(&root, PASSWORD, "127.0.0.1:0")
            .unwrap()
            .with_config(config)
            .with_load_policy(policy);
        let server = Arc::new(server);
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        thread::spawn(move || {
            s2.serve_forever();
        });
        Server { server, addr, root }
    }

    /// `POST /api` with `body` as upstream's scripts send it (`Content-Type:
    /// application/json`); the answer must be HTTP 200 JSON.
    pub fn post_raw(&self, body: &str) -> Json {
        let (status, text) = http(&self.addr, "POST", "/api", body.as_bytes());
        assert_eq!(status, 200, "HTTP {status}: {text}");
        json::parse(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
    }

    /// `operation` with root's credentials and `params` (string values, as
    /// the scripts send them).
    pub fn op(&self, operation: &str, params: &[(&str, &str)]) -> Json {
        let mut body = Json::obj()
            .with("operation", operation)
            .with("username", USERNAME)
            .with("password", PASSWORD);
        for (k, v) in params {
            body.set(*k, *v);
        }
        self.post_raw(&body.to_string())
    }

    /// `build` `db` from `file` (upstream's `db_path`); the answer.
    pub fn build(&self, db: &str, file: &Path) -> Json {
        self.op("build", &[("db_name", db), ("db_path", &path_str(file))])
    }

    pub fn load(&self, db: &str) {
        ok(&self.op("load", &[("db_name", db)]));
    }

    pub fn unload(&self, db: &str) {
        ok(&self.op("unload", &[("db_name", db)]));
    }

    pub fn drop_db(&self, db: &str) {
        ok(&self.op("drop", &[("db_name", db), ("is_backup", "false")]));
    }

    /// `query` `sparql` on `db`.
    pub fn query(&self, db: &str, sparql: &str) -> Json {
        self.op("query", &[("db_name", db), ("sparql", sparql)])
    }
}

impl Server {
    /// A Rust-only REST endpoint (`/begin`, `/commit`, …) as root, with HTTP
    /// Basic credentials: (status, body).
    pub fn rest(&self, method: &str, target: &str, body: &str) -> (u16, String) {
        // base64("root:123456")
        http_with(
            &self.addr,
            method,
            target,
            &[("Authorization", "Basic cm9vdDoxMjM0NTY=")],
            body.as_bytes(),
        )
    }
}

/// One raw HTTP/1.1 exchange: (status, body).
pub fn http(addr: &SocketAddr, method: &str, target: &str, body: &[u8]) -> (u16, String) {
    http_with(addr, method, target, &[], body)
}

/// [`http`] with extra request headers.
pub fn http_with(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(600))).unwrap();
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n"
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let mut bytes = head.into_bytes();
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
    (
        status,
        String::from_utf8_lossy(&resp[split + 4..]).into_owned(),
    )
}

/// The answer's `StatusCode`.
pub fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no StatusCode in {j}"))
}

/// Assert `StatusCode` 0.
pub fn ok(j: &Json) {
    assert_eq!(code(j), 0, "expected success, got {j}");
}

/// An integer field (upstream sends some as strings, e.g. `tripleNum`).
pub fn int(j: &Json, key: &str) -> i64 {
    let v = j.get(key).unwrap_or_else(|| panic!("no {key} in {j}"));
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("{key} is not an integer in {j}"))
}

/// A string field.
pub fn string(j: &Json, key: &str) -> String {
    j.get(key)
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no string {key} in {j}"))
        .to_string()
}

// ---------------------------------------------------------------------------
// Result tables in upstream's text form
// ---------------------------------------------------------------------------

/// One cell: a term in gStore's console form (`<iri>`, `"lex"`,
/// `"lex"@lang`, `"lex"^^<datatype>`, `_:label`), or `None` when unbound.
pub type Cell = Option<String>;

/// A result table: variable names (without `?`) and rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub vars: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

impl Table {
    /// The rows with their columns in `vars` order (missing columns are
    /// unbound), sorted — an order-insensitive form for comparison.
    pub fn normalized(&self, vars: &[String]) -> Vec<Vec<Cell>> {
        let mut rows = self.reordered(vars);
        rows.sort();
        rows
    }

    /// The rows with their columns in `vars` order, in result order.
    pub fn reordered(&self, vars: &[String]) -> Vec<Vec<Cell>> {
        let idx: Vec<Option<usize>> = vars
            .iter()
            .map(|v| self.vars.iter().position(|w| w == v))
            .collect();
        self.rows
            .iter()
            .map(|r| idx.iter().map(|i| i.and_then(|i| r[i].clone())).collect())
            .collect()
    }
}

/// gStore's text form of one SPARQL-JSON binding (`type` `uri`, `literal`,
/// `typed-literal`, `bnode`; the language tag as `xml:lang` or `lang`).
pub fn term_text(b: &Json) -> String {
    let value = b.get("value").and_then(Json::as_str).unwrap_or_default();
    let ty = b.get("type").and_then(Json::as_str).unwrap_or_default();
    let lang = b
        .get("xml:lang")
        .or_else(|| b.get("lang"))
        .and_then(Json::as_str)
        .filter(|l| !l.is_empty());
    let datatype = b.get("datatype").and_then(Json::as_str);
    match ty {
        "uri" => format!("<{value}>"),
        "bnode" => format!("_:{value}"),
        _ => match (lang, datatype) {
            (Some(l), _) => format!("\"{value}\"@{l}"),
            (None, Some(dt)) => format!("\"{value}\"^^<{dt}>"),
            (None, None) => format!("\"{value}\""),
        },
    }
}

/// The table of a `query` answer in the JSON format (`head.vars`,
/// `results.bindings`).
pub fn answer_table(j: &Json) -> Table {
    let vars: Vec<String> = j
        .get("head")
        .and_then(|h| h.get("vars"))
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("no head.vars in {j}"))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let bindings = j
        .get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("no results.bindings in {j}"));
    let rows = bindings
        .iter()
        .map(|b| vars.iter().map(|v| b.get(v).map(term_text)).collect())
        .collect();
    Table { vars, rows }
}

/// Parse one of upstream's `parser_test/parser_r<N>.txt` files: gStore's
/// console table
///
/// ```text
/// ---------------------
/// |  ?name  |  ?mbox  |
/// ---------------------
/// | "Alice" | <m:a>   |
/// | "Bob"   |         |      <- unbound
/// ---------------------
/// ```
///
/// or, for the graph-function cases, a bare JSON document (returned as one
/// row with one cell holding the document re-serialized with sorted keys).
pub fn parse_console_table(text: &str) -> Table {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if !lines.first().is_some_and(|l| l.starts_with('-')) {
        let doc = json::parse(text).unwrap_or_else(|e| panic!("not a table nor JSON ({e})"));
        return Table {
            vars: Vec::new(),
            rows: vec![vec![Some(doc.sorted().to_string())]],
        };
    }
    let cells = |line: &str| -> Vec<String> {
        let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
        inner.split('|').map(|c| c.trim().to_string()).collect()
    };
    let vars: Vec<String> = cells(lines[1])
        .into_iter()
        .map(|v| v.trim_start_matches('?').to_string())
        .collect();
    let rows = lines[2..]
        .iter()
        .filter(|l| l.starts_with('|'))
        .map(|l| {
            cells(l)
                .into_iter()
                .map(|c| (!c.is_empty()).then_some(c))
                .collect()
        })
        .collect();
    Table { vars, rows }
}

/// Parse one of upstream's `bfs_test/<dataset>/<query>r.txt` files (written
/// by `scripts/test/PyTest/FormatHelper.WriteResultToFile`): a tab-separated
/// header of `?vars`, then one tab-separated row per answer, `-` for an
/// unbound cell. The header's column order is arbitrary (Python set order).
pub fn parse_tsv_table(text: &str) -> Table {
    let mut lines = text.lines();
    let vars: Vec<String> = lines
        .next()
        .unwrap_or_default()
        .split('\t')
        .map(|v| v.trim().trim_start_matches('?').to_string())
        .filter(|v| !v.is_empty())
        .collect();
    let rows = lines
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            l.split('\t')
                .map(|c| {
                    let c = c.trim();
                    (c != "-" && !c.is_empty()).then(|| c.to_string())
                })
                .collect()
        })
        .collect();
    Table { vars, rows }
}

/// Multiset difference report of two normalized row lists (for assertion
/// messages): rows only in `want`, rows only in `got`.
pub fn diff_rows(want: &[Vec<Cell>], got: &[Vec<Cell>]) -> String {
    let mut count: BTreeMap<&Vec<Cell>, i64> = BTreeMap::new();
    for r in want {
        *count.entry(r).or_default() += 1;
    }
    for r in got {
        *count.entry(r).or_default() -= 1;
    }
    let missing: Vec<_> = count
        .iter()
        .filter(|(_, &n)| n > 0)
        .map(|(r, n)| (r, *n))
        .collect();
    let extra: Vec<_> = count
        .iter()
        .filter(|(_, &n)| n < 0)
        .map(|(r, n)| (r, -n))
        .collect();
    let shown = |v: &[(&&Vec<Cell>, i64)]| -> String {
        v.iter()
            .take(10)
            .map(|(r, n)| format!("  {n}x {r:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "{} expected row(s) missing:\n{}\n{} unexpected row(s):\n{}",
        missing.len(),
        shown(&missing),
        extra.len(),
        shown(&extra)
    )
}
