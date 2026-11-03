//! Phase 6 R4-RECLAIM: id reuse never shows a reader an id that changed
//! meaning.
//!
//! * `ConcurrentDb`: a writer churns pairs `(<s_k>, :v, "v-s_k")` — every
//!   delete frees both terms and the next insert reuses their ids for new
//!   ones — while readers check the invariant `o = "v-" + local(s)` on every
//!   row they see. A reader whose snapshot resolved a reused id through the
//!   new term would see a mismatched pair.
//! * HTTP: a transaction (`begin` / `tquery` / `commit` through `/api`)
//!   deletes and inserts pairs that reuse freed ids while 8 readers query the
//!   same invariant non-transactionally.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::{ConcurrentDb, QueryResult};

const PAIRS: u64 = 40;

fn pair(k: u64) -> String {
    format!("<http://ex/s{k}> <http://ex/v> \"v-s{k}\" .")
}

/// Every row `(?s, ?o)` satisfies `o = "v-" + local(s)`; returns the rows.
fn check_rows(rows: &[(String, String)]) -> usize {
    for (s, o) in rows {
        let local = s.trim_start_matches("<http://ex/").trim_end_matches('>');
        assert_eq!(
            o,
            &format!("\"v-{local}\""),
            "a reader saw {s} with {o}: an id changed meaning under it"
        );
    }
    rows.len()
}

fn select_rows(r: QueryResult) -> Vec<(String, String)> {
    match r {
        QueryResult::Select(rs) => rs
            .rows
            .into_iter()
            .map(|r| {
                (
                    r[0].clone().unwrap_or_default(),
                    r[1].clone().unwrap_or_default(),
                )
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

const READ: &str = "SELECT ?s ?o WHERE { ?s <http://ex/v> ?o }";

#[test]
fn concurrent_readers_keep_the_pair_invariant_under_churn() {
    let data: String = (0..PAIRS).map(pair).collect::<Vec<_>>().join("\n");
    let db = Arc::new(ConcurrentDb::build_from_str("churn", &data).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let mut readers = Vec::new();
    for _ in 0..4 {
        let (db, stop, reads) = (Arc::clone(&db), Arc::clone(&stop), Arc::clone(&reads));
        readers.push(thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let rows = select_rows(db.query(READ).unwrap());
                assert_eq!(check_rows(&rows) as u64, PAIRS, "pairs are swapped whole");
                reads.fetch_add(1, Ordering::Relaxed);
                // A transaction's snapshot reads too.
                let txn = db.begin();
                let rows = select_rows(txn.query(READ).unwrap());
                check_rows(&rows);
                txn.abort();
            }
        }));
    }
    let mut next = PAIRS;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut rounds = 0u64;
    while Instant::now() < deadline || rounds < 200 {
        // Replace the oldest pair by a new one in one update: the delete
        // frees two ids (subject and literal) the insert reuses.
        let old = next - PAIRS;
        db.update(&format!(
            "DELETE DATA {{ {} }} ; INSERT DATA {{ {} }}",
            pair(old).trim_end_matches(" ."),
            pair(next).trim_end_matches(" .")
        ))
        .unwrap();
        next += 1;
        rounds += 1;
    }
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }
    assert!(reads.load(Ordering::Relaxed) > 0);
    // The id space did not grow with the churn: freed ids were reused.
    let snap = db.snapshot();
    let rows = select_rows(snap.query(READ).unwrap());
    assert_eq!(check_rows(&rows) as u64, PAIRS);
}

// ---- HTTP -----------------------------------------------------------------------

fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!(
        "gstore_reclaim_conc_{tag}_{}_{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// The server root, removed on drop (also when the test panics).
struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn send(addr: &SocketAddr, method: &str, target: &str, auth: bool, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    if auth {
        // root:root
        raw.push_str("Authorization: Basic cm9vdDpyb290\r\n");
    } else {
        raw.push_str("Content-Type: application/json\r\nAccept-Encoding: identity\r\n");
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    s.write_all(raw.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
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

fn api(addr: &SocketAddr, pairs: &[(&str, &str)]) -> Json {
    let mut all = vec![("username", "root"), ("password", "root")];
    all.extend_from_slice(pairs);
    let body = format!(
        "{{{}}}",
        all.iter()
            .map(|(k, v)| format!("{}: {}", json::quote(k), json::quote(v)))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let (status, text) = send(addr, "POST", "/api", false, &body);
    assert_eq!(status, 200, "{text}");
    json::parse(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .expect("StatusCode")
}

fn api_rows(j: &Json) -> Vec<(String, String)> {
    let bindings = j
        .get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Json::as_array)
        .unwrap_or_else(|| panic!("no bindings in {j}"));
    bindings
        .iter()
        .map(|b| {
            let s = b
                .get("s")
                .and_then(|v| v.get("value"))
                .and_then(Json::as_str)
                .unwrap_or_default();
            let o = b
                .get("o")
                .and_then(|v| v.get("value"))
                .and_then(Json::as_str)
                .unwrap_or_default();
            (format!("<{s}>"), format!("\"{o}\""))
        })
        .collect()
}

#[test]
fn http_transaction_reusing_ids_while_readers_read() {
    let root = Root(tmp_root("http"));
    let server = Arc::new(ApiServer::bind(&root.0, "root", "127.0.0.1:0").unwrap());
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    let data: String = (0..PAIRS).map(pair).collect::<Vec<_>>().join("\n");
    let (status, body) = send(&addr, "POST", "/build?db_name=rdb", true, &data);
    assert_eq!(status, 200, "build: {body}");
    let j = api(&addr, &[("operation", "load"), ("db_name", "rdb")]);
    assert_eq!(code(&j), 0, "{j}");

    let stop = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..8 {
        let stop = Arc::clone(&stop);
        readers.push(thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::Relaxed) {
                let j = api(
                    &addr,
                    &[
                        ("operation", "query"),
                        ("db_name", "rdb"),
                        ("format", "json"),
                        ("sparql", READ),
                    ],
                );
                assert_eq!(code(&j), 0, "{j}");
                let rows = api_rows(&j);
                assert_eq!(check_rows(&rows) as u64, PAIRS, "pairs are swapped whole");
                n += 1;
            }
            n
        }));
    }
    let mut next = PAIRS;
    for _round in 0..10 {
        let j = api(&addr, &[("operation", "begin"), ("db_name", "rdb")]);
        assert_eq!(code(&j), 0, "{j}");
        let tid = j.get("TID").and_then(Json::as_str).unwrap().to_string();
        for _ in 0..4 {
            let old = next - PAIRS;
            let sparql = format!(
                "DELETE DATA {{ {} }} ; INSERT DATA {{ {} }}",
                pair(old).trim_end_matches(" ."),
                pair(next).trim_end_matches(" .")
            );
            let j = api(
                &addr,
                &[
                    ("operation", "tquery"),
                    ("db_name", "rdb"),
                    ("tid", &tid),
                    ("sparql", &sparql),
                ],
            );
            assert_eq!(code(&j), 0, "{j}");
            next += 1;
        }
        let j = api(
            &addr,
            &[("operation", "commit"), ("db_name", "rdb"), ("tid", &tid)],
        );
        assert_eq!(code(&j), 0, "{j}");
    }
    stop.store(true, Ordering::Relaxed);
    let total: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();
    assert!(total > 0);
    let j = api(
        &addr,
        &[
            ("operation", "query"),
            ("db_name", "rdb"),
            ("format", "json"),
            ("sparql", READ),
        ],
    );
    let rows = api_rows(&j);
    assert_eq!(check_rows(&rows) as u64, PAIRS);
    assert!(
        rows.iter().all(|(s, _)| {
            let k: u64 = s
                .trim_start_matches("<http://ex/s")
                .trim_end_matches('>')
                .parse()
                .unwrap();
            k >= next - PAIRS
        }),
        "the committed pairs are the newest"
    );
    let j = api(&addr, &[("operation", "unload"), ("db_name", "rdb")]);
    assert_eq!(code(&j), 0, "{j}");
}
