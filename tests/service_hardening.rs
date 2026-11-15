//! Outbound `SERVICE` safety (review package r1-crash-dos):
//!
//! - a hostile endpoint's reply cannot abort the process: the SPARQL-results
//!   JSON of a `SERVICE` reply is read by the depth-limited [`gstore::json`]
//!   parser (review H-3: sec#5, gapi#3, robust#1), which also decodes
//!   surrogate-pair escapes correctly (gapi#10, robust#8).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use gstore::http_client::parse_sparql_json;
use gstore::model::Term;
use gstore::Database;

const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";

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
    // The endpoint was asked (the policy allows loopback in the library).
    let req = requests.recv_timeout(Duration::from_secs(30)).unwrap();
    assert!(req.starts_with("POST /sparql HTTP/1.1\r\n"), "{req}");
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
