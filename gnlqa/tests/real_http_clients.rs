//! The real HTTP clients (gStore SPARQL endpoint, Anthropic, OpenAI-compatible,
//! embeddings) exercised over a socket against a scripted local stub
//! (review cluster-nlqa#24), plus the behaviours other review items fixed in
//! them: credentials never follow a redirect (cluster-nlqa#15).

mod common;

use common::{Scripted, Stub};
use gnlqa::{
    AnthropicClient, Embedder, Error, GStoreClient, HttpEmbedder, LlmClient, LlmRequest,
    OpenAiClient,
};

const KEY: &str = "sk-USER-REAL-SECRET";

/// A stub that redirects every request to `target` (a second stub).
fn redirector(target: &Stub, path: &str) -> Stub {
    Stub::start(vec![
        Scripted::json(302, "{}").header("Location", &target.url(path))
    ])
}

fn assert_redirect_refused<T: std::fmt::Debug>(r: gnlqa::Result<T>, collector: &Stub) {
    let err = r.expect_err("a redirecting endpoint must not yield a result");
    assert!(
        matches!(err, Error::Http(ref m) if m.contains("redirect")),
        "expected a redirect refusal, got {err:?}"
    );
    let leaked = collector.recorded();
    assert!(
        leaked.is_empty(),
        "the redirect target received {} request(s), e.g. headers {:?}",
        leaked.len(),
        leaked.first().map(|r| &r.headers)
    );
}

#[test]
fn anthropic_key_never_follows_a_redirect() {
    let collector = Stub::start(vec![Scripted::json(200, "{}")]);
    let origin = redirector(&collector, "/v1/messages");
    let client = AnthropicClient::new(KEY, origin.url(""), "m");
    assert_redirect_refused(client.complete(&LlmRequest::prompt("hi")), &collector);
    // the configured host did get the key
    assert_eq!(origin.recorded()[0].header("x-api-key"), Some(KEY));
}

#[test]
fn openai_key_never_follows_a_redirect() {
    let collector = Stub::start(vec![Scripted::json(200, "{}")]);
    let origin = redirector(&collector, "/v1/chat/completions");
    let client = OpenAiClient::new(KEY, origin.url("/v1"), "m");
    assert_redirect_refused(client.complete(&LlmRequest::prompt("hi")), &collector);
}

#[test]
fn gstore_credentials_never_follow_a_redirect() {
    let collector = Stub::start(vec![Scripted::json(200, "{}")]);
    let origin = redirector(&collector, "/sparql");
    let cfg = gnlqa::Config {
        gstore_endpoint: origin.url("/sparql"),
        gstore_user: Some("root".into()),
        gstore_password: Some(gnlqa::Secret::new("hunter2")),
        ..gnlqa::Config::default()
    };
    let kb = GStoreClient::from_config(&cfg);
    assert_redirect_refused(kb.query("SELECT ?s WHERE { ?s ?p ?o }"), &collector);
    assert!(origin.recorded()[0]
        .header("Authorization")
        .is_some_and(|a| a.starts_with("Basic ")));
}

#[test]
fn embedding_key_never_follows_a_redirect() {
    let collector = Stub::start(vec![Scripted::json(200, "{}")]);
    let origin = redirector(&collector, "/embeddings");
    let e = HttpEmbedder::new(origin.url("/embeddings"), KEY, "m", 2);
    assert_redirect_refused(e.embed(&["x".to_string()]), &collector);
}

// ---- review cluster-nlqa#16: transient provider failures are retried ----

const ANTHROPIC_OK: &str = r#"{"content":[{"type":"text","text":"ok"}]}"#;
const OPENAI_OK: &str = r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#;

#[test]
fn anthropic_retries_a_rate_limit_then_succeeds() {
    let stub = Stub::start(vec![
        Scripted::json(429, r#"{"type":"error"}"#).header("Retry-After", "0"),
        Scripted::json(529, r#"{"type":"overloaded"}"#).header("Retry-After", "0"),
        Scripted::json(200, ANTHROPIC_OK),
    ]);
    let client = AnthropicClient::new(KEY, stub.url(""), "m");
    let out = client.complete(&LlmRequest::prompt("hi")).unwrap();
    assert_eq!(out, "ok");
    assert_eq!(stub.recorded().len(), 3, "two retries, then success");
}

#[test]
fn openai_gives_up_after_a_bounded_number_of_retries() {
    let stub = Stub::start(vec![
        Scripted::json(503, r#"{"error":"down"}"#).header("Retry-After", "0")
    ]);
    let client = OpenAiClient::new(KEY, stub.url("/v1"), "m");
    let err = client.complete(&LlmRequest::prompt("hi")).unwrap_err();
    assert!(
        matches!(err, Error::Http(ref m) if m.contains("503")),
        "{err:?}"
    );
    assert_eq!(
        stub.recorded().len(),
        1 + gnlqa::RetryPolicy::default().max_retries as usize
    );
}

#[test]
fn a_client_error_status_is_not_retried() {
    let stub = Stub::start(vec![Scripted::json(400, r#"{"error":"bad request"}"#)]);
    let client = OpenAiClient::new(KEY, stub.url("/v1"), "m");
    assert!(client.complete(&LlmRequest::prompt("hi")).is_err());
    assert_eq!(stub.recorded().len(), 1);
}

#[test]
fn retry_back_off_doubles_without_retry_after() {
    let stub = Stub::start(vec![
        Scripted::json(502, "{}"),
        Scripted::json(200, ANTHROPIC_OK),
    ]);
    let client = AnthropicClient::new(KEY, stub.url(""), "m").with_retry(gnlqa::RetryPolicy {
        max_retries: 1,
        base_backoff: std::time::Duration::from_millis(1),
        max_backoff: std::time::Duration::from_millis(5),
    });
    assert_eq!(client.complete(&LlmRequest::prompt("hi")).unwrap(), "ok");
    assert_eq!(stub.recorded().len(), 2);
}

#[test]
fn a_retry_wait_never_outlasts_the_request_budget() {
    let stub = Stub::start(vec![
        Scripted::json(429, "{}").header("Retry-After", "30"),
        Scripted::json(200, ANTHROPIC_OK),
    ]);
    let client = AnthropicClient::new(KEY, stub.url(""), "m");
    let limits = gnlqa::CostLimits {
        timeout: Some(std::time::Duration::from_secs(10)),
        ..gnlqa::CostLimits::unlimited()
    };
    let r = gnlqa::budget::with_limits(limits, || client.complete(&LlmRequest::prompt("hi")));
    assert!(
        matches!(r, Err(Error::Http(ref m)) if m.contains("429")),
        "{r:?}"
    );
    assert_eq!(
        stub.recorded().len(),
        1,
        "no 30 s wait inside a 10 s budget"
    );
}

#[test]
fn gstore_retries_overload_but_not_query_failures() {
    let rows = r#"{"head":{"vars":["s"]},"results":{"bindings":[{"s":{"type":"uri","value":"http://ex/a"}}]}}"#;
    let stub = Stub::start(vec![
        Scripted::json(503, "busy").header("Retry-After", "0"),
        Scripted::new(
            200,
            "application/sparql-results+json",
            rows.as_bytes().to_vec(),
        ),
    ]);
    let kb = GStoreClient::new(stub.url("/sparql"));
    assert_eq!(
        kb.query("SELECT ?s WHERE { ?s ?p ?o }")
            .unwrap()
            .row_count(),
        1
    );
    assert_eq!(stub.recorded().len(), 2);

    // a 500 is gStore's answer to a failing query: no retry, repairable error
    let failing = Stub::start(vec![Scripted::json(500, "evaluation failed")]);
    let kb = GStoreClient::new(failing.url("/sparql"));
    let err = kb.query("SELECT ?s WHERE { ?s ?p ?o }").unwrap_err();
    assert!(matches!(err, Error::GStore(_)), "{err:?}");
    assert_eq!(failing.recorded().len(), 1);
}

#[test]
fn embeddings_retry_a_rate_limit() {
    let stub = Stub::start(vec![
        Scripted::json(429, "{}").header("Retry-After", "0"),
        Scripted::json(200, r#"{"data":[{"embedding":[1.0,0.0],"index":0}]}"#),
    ]);
    let e = HttpEmbedder::new(stub.url("/embeddings"), KEY, "m", 2);
    assert_eq!(e.embed(&["x".to_string()]).unwrap(), vec![vec![1.0, 0.0]]);
    assert_eq!(stub.recorded().len(), 2);
}

// ---- review cluster-nlqa#24: the clients' wire behaviour over a socket ----

#[test]
fn openai_client_sends_bearer_key_and_parses_the_reply() {
    let stub = Stub::start(vec![Scripted::json(200, OPENAI_OK)]);
    let client = OpenAiClient::new(KEY, stub.url("/v1"), "the-model");
    let out = client
        .complete(&LlmRequest::prompt("hi").system("be terse"))
        .unwrap();
    assert_eq!(out, "ok");
    let rec = &stub.recorded()[0];
    assert_eq!(rec.method, "POST");
    assert_eq!(rec.path, "/v1/chat/completions");
    assert_eq!(rec.header("Authorization"), Some(&*format!("Bearer {KEY}")));
    let body: serde_json::Value = serde_json::from_str(&rec.body).unwrap();
    assert_eq!(body["model"], "the-model");
    assert_eq!(body["messages"][0]["role"], "system");
}

/// A SPARQL Results JSON document with `n` single-variable URI rows.
fn select_json(n: usize) -> Vec<u8> {
    let mut s = String::from(r#"{"head":{"vars":["s","o"]},"results":{"bindings":["#);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            r#"{{"s":{{"type":"uri","value":"http://e/{i}"}},"o":{{"type":"uri","value":"http://e/{}"}}}}"#,
            i + 1
        ));
    }
    s.push_str("]}}");
    s.into_bytes()
}

fn rows_stub(n: usize) -> Stub {
    Stub::start(vec![Scripted::new(
        200,
        "application/sparql-results+json",
        select_json(n),
    )])
}

// ---- review cluster-nlqa#17: results over the row cap carry a signal ----

#[test]
fn rows_beyond_the_cap_are_never_silently_dropped() {
    let stub = rows_stub(gnlqa::MAX_RESULT_ROWS + 1);
    let kb = GStoreClient::new(stub.url("/sparql"));
    let err = kb.query("SELECT ?s ?o WHERE { ?s ?p ?o }").unwrap_err();
    assert!(
        matches!(err, Error::GStore(ref m) if m.contains("LIMIT")),
        "a truncated result must not pass as complete: {err:?}"
    );
    let (ans, truncated) = kb.query_capped("SELECT ?s ?o WHERE { ?s ?p ?o }").unwrap();
    assert!(truncated);
    assert_eq!(ans.row_count(), gnlqa::MAX_RESULT_ROWS);
}

#[test]
fn analytics_reports_a_sample_the_client_capped() {
    // max_edges above the client's row cap: the edge sample is cut by the
    // client, so the result must say it was computed over a truncated graph.
    let stub = rows_stub(gnlqa::MAX_RESULT_ROWS + 1);
    let kb = GStoreClient::new(stub.url("/sparql"));
    let cfg = gnlqa::AnalyticsCfg {
        max_edges: 150_000,
        top_k: 3,
        path_hops: 3,
    };
    let res = gnlqa::run_analytics(&kb, "rank nodes by pagerank", &[], cfg).unwrap();
    assert_eq!(res.edge_count, gnlqa::MAX_RESULT_ROWS);
    assert!(res.truncated, "analytics claimed a complete graph: {res:?}");
}

#[test]
fn mcp_run_sparql_flags_a_truncated_result() {
    let stub = rows_stub(gnlqa::MAX_RESULT_ROWS + 1);
    let engine = std::sync::Arc::new(gnlqa::SolveEngine::new(
        Box::new(gnlqa::MockLlm::fixed("x")),
        Box::new(GStoreClient::new(stub.url("/sparql"))),
    ));
    let srv = gnlqa::McpServer::new(engine);
    let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"run_sparql","arguments":{"query":"SELECT ?s ?o WHERE { ?s ?p ?o }"}}}"#;
    let v: serde_json::Value = serde_json::from_str(&srv.handle_message(req).unwrap()).unwrap();
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("[truncated]"),
        "{}",
        &text[text.len() - 200..]
    );
    assert!(
        !text.contains("rows total"),
        "a capped count is not a total"
    );
}

#[test]
fn anthropic_client_sends_its_headers_and_parses_the_reply() {
    let stub = Stub::start(vec![Scripted::json(200, ANTHROPIC_OK)]);
    let client = AnthropicClient::new(KEY, stub.url(""), "the-model");
    let out = client
        .complete(&LlmRequest::prompt("hi").system("be terse"))
        .unwrap();
    assert_eq!(out, "ok");
    let rec = &stub.recorded()[0];
    assert_eq!(rec.path, "/v1/messages");
    assert_eq!(rec.header("x-api-key"), Some(KEY));
    assert_eq!(rec.header("anthropic-version"), Some("2023-06-01"));
    let body: serde_json::Value = serde_json::from_str(&rec.body).unwrap();
    assert_eq!(body["system"], "be terse");
    assert_eq!(body["model"], "the-model");
}

#[test]
fn gstore_classifies_query_errors_and_transport_failures() {
    // An HTTP status is a query/server error: repairable, Error::GStore.
    let stub = Stub::start(vec![Scripted::new(
        400,
        "text/plain",
        b"bad query".to_vec(),
    )]);
    let err = GStoreClient::new(stub.url("/sparql"))
        .query("SELECT ?s WHERE { ?s ?p ?o }")
        .unwrap_err();
    assert!(
        matches!(err, Error::GStore(ref m) if m.contains("bad query")),
        "{err:?}"
    );
    // A refused connection is an outage: Error::Http, which ends the request.
    let dead = GStoreClient::new(format!("http://127.0.0.1:{}/sparql", common::closed_port()));
    let err = dead.query("SELECT ?s WHERE { ?s ?p ?o }").unwrap_err();
    assert!(matches!(err, Error::Http(_)), "{err:?}");
    assert!(err.is_fatal());
    // the request itself
    let rec = &stub.recorded()[0];
    assert_eq!(rec.header("Content-Type"), Some("application/sparql-query"));
    assert_eq!(rec.body, "SELECT ?s WHERE { ?s ?p ?o }");
}

#[test]
fn gstore_graph_replies_are_returned_as_graphs() {
    let nt = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";
    let stub = Stub::start(vec![Scripted::new(
        200,
        "application/n-triples",
        nt.as_bytes().to_vec(),
    )]);
    let ans = GStoreClient::new(stub.url("/sparql"))
        .query("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }")
        .unwrap();
    assert_eq!(ans, gnlqa::SparqlAnswer::Graph(nt.to_string()));
}

#[test]
fn gstore_reply_size_is_capped() {
    // 32 MiB + 1 byte: refused instead of read whole or silently cut.
    let body = vec![b' '; (32 << 20) + 1];
    let stub = Stub::start(vec![Scripted::new(
        200,
        "application/sparql-results+json",
        body,
    )]);
    let err = GStoreClient::new(stub.url("/sparql"))
        .query("SELECT ?s WHERE { ?s ?p ?o }")
        .unwrap_err();
    assert!(
        matches!(err, Error::GStore(ref m) if m.contains("limit")),
        "{err:?}"
    );
}
