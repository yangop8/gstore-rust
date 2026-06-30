//! Data test: SPARQL 1.1 federated query (`SERVICE`).
//!
//! Fully self-contained: an in-process [`Server`] (the [`gstore::server`] HTTP
//! SPARQL endpoint) holds a small "remote" database in a background thread, and
//! a *separate* local database runs a query whose `SERVICE <http://127.0.0.1:…>`
//! block pulls bindings from that endpoint and joins them with local solutions.

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread;

use gstore::server::Server;
use gstore::Database;

/// The "remote" dataset, exposed over HTTP by the in-process server.
const REMOTE: &str = r#"
@prefix : <http://ex/> .
:bob   :name "Bob" .
:carol :name "Carol" .
:dave  :name "Dave" .
"#;

/// The local dataset: alice knows bob and carol (but not dave).
const LOCAL: &str = r#"
@prefix : <http://ex/> .
:alice :knows :bob .
:alice :knows :carol .
"#;

/// Start an in-process SPARQL endpoint over `data`; returns its bound address.
fn start_endpoint(name: &str, data: &str) -> SocketAddr {
    let db = Database::build_from_str(name, data).expect("build remote db");
    let server = Arc::new(Server::bind(db, "127.0.0.1:0").expect("bind"));
    let addr = server.local_addr().expect("addr");
    thread::spawn(move || server.serve_forever());
    addr
}

/// An address that nothing is listening on (a bound-then-dropped ephemeral port),
/// so connecting to it is refused — a "dead" endpoint.
fn dead_addr() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    addr
}

#[test]
fn service_joins_remote_bindings() {
    let addr = start_endpoint("svc_remote", REMOTE);
    let mut local = Database::build_from_str("svc_local", LOCAL).expect("build local db");

    let query = format!(
        "SELECT ?name WHERE {{ \
            <http://ex/alice> <http://ex/knows> ?friend . \
            SERVICE <http://{addr}/sparql> {{ ?friend <http://ex/name> ?name }} \
         }}"
    );
    let rs = local.select(&query).expect("federated query");

    let mut names: Vec<String> = rs
        .rows
        .iter()
        .map(|r| r[0].clone().unwrap_or_default())
        .collect();
    names.sort();
    // Only alice's friends (bob, carol) are joined; dave's remote name is dropped
    // because dave is not a local friend of alice.
    assert_eq!(names, vec!["\"Bob\"".to_string(), "\"Carol\"".to_string()]);
}

#[test]
fn service_returns_both_join_columns() {
    let addr = start_endpoint("svc_remote2", REMOTE);
    let mut local = Database::build_from_str("svc_local2", LOCAL).expect("build local db");

    let query = format!(
        "SELECT ?friend ?name WHERE {{ \
            <http://ex/alice> <http://ex/knows> ?friend . \
            SERVICE <http://{addr}/sparql> {{ ?friend <http://ex/name> ?name }} \
         }}"
    );
    let rs = local.select(&query).expect("federated query");
    assert_eq!(rs.rows.len(), 2);
    // Each row pairs a local friend IRI with its remote name.
    let mut pairs: Vec<(String, String)> = rs
        .rows
        .iter()
        .map(|r| {
            (
                r[0].clone().unwrap_or_default(),
                r[1].clone().unwrap_or_default(),
            )
        })
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("<http://ex/bob>".to_string(), "\"Bob\"".to_string()),
            ("<http://ex/carol>".to_string(), "\"Carol\"".to_string()),
        ]
    );
}

#[test]
fn service_silent_dead_endpoint_yields_identity() {
    let dead = dead_addr();
    let mut local = Database::build_from_str("svc_silent", LOCAL).expect("build local db");

    // A standalone SILENT service against a dead endpoint must not fail or panic;
    // it yields the single identity solution (one row of unbound variables).
    let query = format!(
        "SELECT * WHERE {{ SERVICE SILENT <http://{dead}/sparql> {{ ?s <http://ex/name> ?o }} }}"
    );
    let rs = local.select(&query).expect("silent service returns Ok");
    assert_eq!(rs.rows.len(), 1, "identity solution is a single row");
    assert!(
        rs.rows[0].iter().all(|c| c.is_none()),
        "identity solution leaves every variable unbound"
    );
}

#[test]
fn service_silent_join_dead_endpoint_preserves_outer_rows() {
    let dead = dead_addr();
    let mut local = Database::build_from_str("svc_silent2", LOCAL).expect("build local db");

    // Per SPARQL 1.1 §18.5, a SILENT failure behaves as the join identity Z, so
    // Join(Ω, Z) = Ω — the outer solutions MUST be preserved (with the inner
    // SERVICE variables left unbound), not dropped.
    let query = format!(
        "SELECT ?friend WHERE {{ \
            <http://ex/alice> <http://ex/knows> ?friend . \
            SERVICE SILENT <http://{dead}/sparql> {{ ?friend <http://ex/name> ?o }} \
         }}"
    );
    let rs = local.select(&query).expect("SILENT join must not error");
    let mut friends: Vec<String> = rs
        .rows
        .iter()
        .map(|r| r[0].clone().unwrap_or_default())
        .collect();
    friends.sort();
    assert_eq!(
        friends,
        vec!["<http://ex/bob>".to_string(), "<http://ex/carol>".to_string()],
        "outer rows must survive a SILENT SERVICE failure"
    );
}

#[test]
fn service_non_silent_dead_endpoint_returns_no_rows() {
    let dead = dead_addr();
    let mut local = Database::build_from_str("svc_hard", LOCAL).expect("build local db");

    // Without SILENT a failed service contributes no solutions (the query
    // degrades to empty rather than panicking).
    let query = format!(
        "SELECT ?name WHERE {{ \
            <http://ex/alice> <http://ex/knows> ?friend . \
            SERVICE <http://{dead}/sparql> {{ ?friend <http://ex/name> ?name }} \
         }}"
    );
    let rs = local.select(&query).expect("query returns Ok");
    assert!(rs.rows.is_empty());
}
