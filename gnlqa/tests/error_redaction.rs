//! review cluster-nlqa#18: internal error detail — endpoint URLs (with any
//! userinfo), provider error bodies — stays in the server log; HTTP and MCP
//! clients get a generic message, a code and a request id.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;

use common::{closed_port, post, Scripted, Stub};
use gnlqa::{GStoreClient, HttpServer, McpServer, MockKb, MockLlm, OpenAiClient, SolveEngine};

const TOKEN: &str = "redaction-token";

fn mock_llm() -> MockLlm {
    MockLlm::new(vec![
        r#"{"qtype":"list"}"#.to_string(),
        r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
    ])
}

/// An endpoint URL carrying credentials, on a port nothing listens on.
fn dead_endpoint_with_userinfo() -> (String, u16) {
    let port = closed_port();
    (format!("http://admin:s3cret@127.0.0.1:{port}/sparql"), port)
}

fn serve(engine: SolveEngine) -> SocketAddr {
    let server = HttpServer::bind(Arc::new(engine), "127.0.0.1:0")
        .unwrap()
        .with_token(TOKEN);
    let addr = server.local_addr().unwrap();
    thread::spawn(move || server.serve_forever());
    addr
}

fn assert_clean(text: &str, port: u16) {
    // review tests-a#7: the dead port is now 0, and ":0" alone also occurs
    // in JSON (`"rounds":0`), so the address is checked with its host.
    for secret in ["s3cret", "admin:", &format!("127.0.0.1:{port}"), "/sparql"] {
        assert!(!text.contains(secret), "client saw {secret:?}: {text}");
    }
}

#[test]
fn http_errors_do_not_reveal_the_endpoint_or_its_credentials() {
    let (url, port) = dead_endpoint_with_userinfo();
    let addr = serve(SolveEngine::new(
        Box::new(mock_llm()),
        Box::new(GStoreClient::new(url)),
    ));
    let r = post(addr, "/ask", r#"{"question":"which x?"}"#, Some(TOKEN));
    assert_eq!(r.status, 500, "{r:?}");
    assert_clean(&r.body, port);
    let v = r.json();
    assert_eq!(v["code"], "upstream_unavailable");
    assert!(v["request_id"].as_str().is_some_and(|id| !id.is_empty()));

    let g = post(addr, "/gSolve", r#"{"question":"which x?"}"#, Some(TOKEN));
    assert_clean(&g.body, port);
    assert_eq!(g.json()["status"], "error");
}

#[test]
fn provider_error_bodies_are_not_echoed_to_http_clients() {
    let provider = Stub::start(vec![Scripted::json(
        401,
        r#"{"error":{"message":"Incorrect API key provided: sk-USER****CRET"}}"#,
    )]);
    let llm = OpenAiClient::new("sk-USER-REAL-SECRET", provider.url("/v1"), "m");
    let addr = serve(SolveEngine::new(
        Box::new(llm),
        Box::new(MockKb::new(vec![])),
    ));
    let r = post(addr, "/ask", r#"{"question":"which x?"}"#, Some(TOKEN));
    assert!(r.status >= 500, "{r:?}");
    assert!(!r.body.contains("Incorrect API key"), "{r:?}");
    assert!(!r.body.contains("sk-USER"), "{r:?}");
    assert!(!r.body.contains("127.0.0.1"), "{r:?}");
}

#[test]
fn library_errors_never_carry_url_userinfo() {
    let (url, _) = dead_endpoint_with_userinfo();
    let err = GStoreClient::new(url)
        .query("ASK { ?s ?p ?o }")
        .unwrap_err();
    let text = err.to_string();
    assert!(
        !text.contains("s3cret") && !text.contains("admin"),
        "{text}"
    );
}

fn mcp_call(srv: &McpServer, tool: &str, args: serde_json::Value) -> (bool, String) {
    let req = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": tool, "arguments": args }
    });
    let v: serde_json::Value =
        serde_json::from_str(&srv.handle_message(&req.to_string()).unwrap()).unwrap();
    (
        v["result"]["isError"].as_bool().unwrap(),
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string(),
    )
}

#[test]
fn mcp_errors_are_redacted_but_keep_query_feedback() {
    let (url, port) = dead_endpoint_with_userinfo();
    let srv = McpServer::new(Arc::new(SolveEngine::new(
        Box::new(mock_llm()),
        Box::new(GStoreClient::new(url)),
    )));
    let (err, text) = mcp_call(&srv, "ask_kg", serde_json::json!({"question": "which x?"}));
    assert!(err);
    assert_clean(&text, port);
    let (err, text) = mcp_call(
        &srv,
        "run_sparql",
        serde_json::json!({"query": "SELECT ?s WHERE { ?s ?p ?o }"}),
    );
    assert!(err);
    assert_clean(&text, port);

    // A query error from gStore is the feedback a client needs to fix its
    // query, so run_sparql keeps it.
    let kb = Stub::start(vec![Scripted::new(
        400,
        "text/plain",
        b"Parse error near FOO".to_vec(),
    )]);
    let srv = McpServer::new(Arc::new(SolveEngine::new(
        Box::new(mock_llm()),
        Box::new(GStoreClient::new(kb.url("/sparql"))),
    )));
    let (err, text) = mcp_call(
        &srv,
        "run_sparql",
        serde_json::json!({"query": "SELECT ?s WHERE { ?s ?p ?o }"}),
    );
    assert!(err);
    assert!(text.contains("Parse error near FOO"), "{text}");
}
