//! The single-database server's `/update` answers an UPDATE stopped by
//! `query_timeout` before it applied anything as `503` — as its `/sparql`
//! does (SPARQL Protocol: a refused request, not a malformed one). Before
//! the round-2 review fix it answered `400`, contrary to the r2-limits-auth
//! changelog ("REST and single-database 503").
//!
//! The timeout is tiny and the UPDATE's WHERE clause takes far longer (a
//! three-way cross product whose FILTER never holds), so the outcome does
//! not depend on timing.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::server::Server;
use gstore::Database;

const HANG_GUARD: Duration = Duration::from_secs(120);

const RUNAWAY_UPDATE: &str = "INSERT { ?a <http://ex/q> ?d } WHERE { ?a ?b ?c . ?d ?e ?f . \
     ?g ?h ?i . FILTER(STR(?c) = CONCAT(STR(?f), STR(?i), \"x\")) }";

fn data(n: usize) -> String {
    (0..n)
        .map(|i| {
            format!(
                "<http://example.org/a/rather/long/subject/{i}> <http://example.org/p> \
                 \"value number {i} of the data\" .\n"
            )
        })
        .collect()
}

fn post(addr: SocketAddr, target: &str, ctype: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let raw = format!(
        "POST {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: {ctype}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {resp:?}"));
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

#[test]
fn a_timed_out_single_database_update_answers_503_like_sparql() {
    let db = Database::build_from_str("update_timeout", &data(100)).unwrap();
    let server = Arc::new(
        Server::bind(db, "127.0.0.1:0")
            .unwrap()
            .with_query_timeout(Duration::from_millis(50)),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    let (st, body) = post(addr, "/update", "application/sparql-update", RUNAWAY_UPDATE);
    assert_eq!(st, 503, "{body}");
    assert!(body.contains("timed out"), "{body}");
    // The same statement through /sparql (an UPDATE by POST) answers alike.
    let (st, body) = post(addr, "/sparql", "application/sparql-update", RUNAWAY_UPDATE);
    assert_eq!(st, 503, "{body}");
    // A query error is still 400.
    let (st, body) = post(
        addr,
        "/update",
        "application/sparql-update",
        "INSERT DATA { broken",
    );
    assert_eq!(st, 400, "{body}");
    server.shutdown();
    let _ = serving.join();
}
