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
