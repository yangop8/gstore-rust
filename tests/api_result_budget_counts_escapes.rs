//! The `/api` result byte budget (`max_result_bytes`, review sec#10) counts
//! each value as the escaped JSON string it is serialized as (round-2
//! review): a control character takes six bytes (`\u0001`), a quote two.
//! Before the fix it counted raw lengths, so an answer of escape-heavy
//! literals was built and sent at several times the budget by `/api`
//! `query` and `tquery` — while REST `/sparql`, which counts the bytes it
//! serializes, refused the same answer.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};

const HANG_GUARD: Duration = Duration::from_secs(60);
const ROOT: &str = "Basic cm9vdDpwdw==";
const BUDGET: u64 = 20_000;

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_api_budget_escapes_{tag}_{}_{n}",
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

fn send(addr: SocketAddr, target: &str, auth: Option<&str>, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let mut raw = format!("POST {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    if let Some(a) = auth {
        raw.push_str(&format!("Authorization: {a}\r\n"));
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap();
    let body = resp.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, body.to_string())
}

fn api(addr: SocketAddr, pairs: &[(&str, &str)]) -> (Json, usize) {
    let mut items = vec![
        format!("{}: {}", json::quote("username"), json::quote("root")),
        format!("{}: {}", json::quote("password"), json::quote("pw")),
    ];
    items.extend(
        pairs
            .iter()
            .map(|(k, v)| format!("{}: {}", json::quote(k), json::quote(v))),
    );
    let (st, body) = send(addr, "/api", None, &format!("{{{}}}", items.join(", ")));
    assert_eq!(st, 200, "{body}");
    let j = json::parse(&body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"));
    (j, body.len())
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode").and_then(Json::as_i64).unwrap()
}

/// Five literals of 2000 control characters each (`\u0001`): about 10 kB
/// counted raw, about 60 kB as JSON.
fn data() -> String {
    let lit = "\\u0001".repeat(2000);
    (0..5)
        .map(|i| format!("<http://ex/s{i}> <http://ex/p> \"{lit}\" .\n"))
        .collect()
}

#[test]
fn an_escape_heavy_api_answer_over_the_budget_is_refused() {
    let tmp = TmpDir::new("escapes");
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    config.max_result_bytes = BUDGET;
    let server = Arc::new(
        ApiServer::bind(tmp.0.join("root"), "pw", "127.0.0.1:0")
            .unwrap()
            .with_config(config),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    let (st, body) = send(addr, "/build?db_name=esc", Some(ROOT), &data());
    assert_eq!(st, 200, "{body}");
    let all = "SELECT ?o WHERE { ?s ?p ?o }";

    // REST counts what it serializes: refused.
    let (st, body) = send(addr, "/sparql?db_name=esc", Some(ROOT), all);
    assert_eq!(st, 503, "{}", &body[..body.len().min(300)]);

    // /api query and tquery: refused too, not sent at three times the budget.
    let (j, len) = api(
        addr,
        &[("operation", "query"), ("db_name", "esc"), ("sparql", all)],
    );
    assert_eq!(code(&j), 1005, "{len} bytes answered");
    assert!(j.to_string().contains("result too large"), "{j}");
    let (j, _) = api(addr, &[("operation", "begin"), ("db_name", "esc")]);
    assert_eq!(code(&j), 0, "{j}");
    let tid = j.get("TID").and_then(Json::as_str).unwrap().to_string();
    let (j, len) = api(
        addr,
        &[
            ("operation", "tquery"),
            ("db_name", "esc"),
            ("tid", &tid),
            ("sparql", all),
        ],
    );
    assert_eq!(code(&j), 1005, "{len} bytes answered");
    let (j, _) = api(
        addr,
        &[("operation", "rollback"), ("db_name", "esc"), ("tid", &tid)],
    );
    assert_eq!(code(&j), 0, "{j}");

    // One such literal fits.
    let (j, len) = api(
        addr,
        &[
            ("operation", "query"),
            ("db_name", "esc"),
            ("sparql", "SELECT ?o WHERE { ?s ?p ?o } LIMIT 1"),
        ],
    );
    assert_eq!(code(&j), 0, "{j}");
    assert!(len < BUDGET as usize, "{len}");

    server.shutdown();
    let _ = serving.join();
}
