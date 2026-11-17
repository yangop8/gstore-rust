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
