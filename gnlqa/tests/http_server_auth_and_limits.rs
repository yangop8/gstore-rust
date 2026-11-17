//! review cluster-nlqa#13 / sec#18: the gnlqa HTTP server refuses
//! unauthenticated question requests by default, rate-limits per client and
//! caps the cost (LLM calls, KB rows, time) and size of every request.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use common::{get, post};
use gnlqa::{
    CostLimits, HttpLimits, HttpServer, MockKb, MockLlm, RdfTerm, SolveEngine, SparqlAnswer,
    TermKind,
};

const TOKEN: &str = "s3cret-client-token";

fn row(v: &str) -> Vec<Option<RdfTerm>> {
    vec![Some(RdfTerm {
        kind: TermKind::Uri,
        value: v.into(),
        datatype: None,
        lang: None,
    })]
}

fn one_row() -> SparqlAnswer {
    SparqlAnswer::Select {
        vars: vec!["x".into()],
        rows: vec![row("http://ex/SECRET")],
    }
}

/// An LLM that answers intent + one candidate query, and a KB with one row.
fn mocks() -> (Arc<MockLlm>, Arc<MockKb>) {
    let llm = Arc::new(MockLlm::new(vec![
        r#"{"qtype":"list"}"#.to_string(),
        r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
    ]));
    (llm, Arc::new(MockKb::new(vec![one_row()])))
}

fn engine(llm: &Arc<MockLlm>, kb: &Arc<MockKb>) -> Arc<SolveEngine> {
    Arc::new(
        SolveEngine::new(Box::new(Arc::clone(llm)), Box::new(Arc::clone(kb))).with_citations(false),
    )
}

fn serve(server: HttpServer) -> SocketAddr {
    let addr = server.local_addr().unwrap();
    thread::spawn(move || server.serve_forever());
    addr
}

const Q: &str = r#"{"question":"which x?"}"#;

#[test]
fn ask_without_credentials_is_refused_by_default() {
    let (llm, kb) = mocks();
    let addr = serve(HttpServer::bind(engine(&llm, &kb), "127.0.0.1:0").unwrap());
    for path in ["/ask", "/gSolve"] {
        let r = post(addr, path, Q, None);
        assert_eq!(r.status, 401, "{path} answered anonymously: {r:?}");
        assert!(!r.body.contains("SECRET"), "{path} leaked KB data: {r:?}");
        assert_eq!(r.header("WWW-Authenticate"), Some("Bearer"));
    }
    assert_eq!(
        llm.call_count(),
        0,
        "no LLM credit spent for anonymous clients"
    );
    assert_eq!(kb.query_count(), 0, "no KB query for anonymous clients");
    // the liveness probe stays open (it does no work)
    assert_eq!(get(addr, "/health").status, 200);
}

#[test]
fn rate_limit_applies_per_client_before_authentication() {
    // Default limits: 30 question requests per client per minute. They are
    // counted before authentication, so anonymous guessing is bounded too.
    let (llm, kb) = mocks();
    let addr = serve(HttpServer::bind(engine(&llm, &kb), "127.0.0.1:0").unwrap());
    let per_minute = HttpLimits::default().requests_per_minute;
    for i in 0..per_minute {
        let r = post(addr, "/ask", Q, None);
        assert_ne!(r.status, 429, "request {i} limited too early");
    }
    let r = post(addr, "/ask", Q, None);
    assert_eq!(r.status, 429, "request beyond the per-minute budget: {r:?}");
    let retry: u64 = r.header("Retry-After").unwrap().parse().unwrap();
    assert!(retry >= 1);
    // /health is not rate-limited
    assert_eq!(get(addr, "/health").status, 200);
}

#[test]
fn bearer_token_is_required_and_checked() {
    let (llm, kb) = mocks();
    let addr = serve(
        HttpServer::bind(engine(&llm, &kb), "127.0.0.1:0")
            .unwrap()
            .with_token(TOKEN),
    );
    assert_eq!(post(addr, "/ask", Q, None).status, 401);
    assert_eq!(post(addr, "/ask", Q, Some("wrong-token")).status, 401);
    assert_eq!(
        post(addr, "/gSolve", Q, Some("s3cret-client-toke")).status,
        401
    );
    // a Basic credential carrying the token is not a bearer token
    let basic = format!(
        "POST /ask HTTP/1.1\r\nHost: t\r\nAuthorization: Basic {TOKEN}\r\nContent-Length: {}\r\n\r\n{Q}",
        Q.len()
    );
    assert_eq!(common::send_raw(addr, basic.as_bytes()).status, 401);
    assert_eq!(llm.call_count(), 0);

    let ok = post(addr, "/ask", Q, Some(TOKEN));
    assert_eq!(ok.status, 200, "{ok:?}");
    assert_eq!(ok.json()["values"][0], "http://ex/SECRET");
}

#[test]
fn an_empty_token_does_not_open_the_server() {
    let (llm, kb) = mocks();
    let addr = serve(
        HttpServer::bind(engine(&llm, &kb), "127.0.0.1:0")
            .unwrap()
            .with_token("   "),
    );
    assert_eq!(post(addr, "/ask", Q, Some("")).status, 401);
    assert_eq!(post(addr, "/ask", Q, None).status, 401);
}

#[test]
fn loopback_opt_out_serves_local_clients_without_a_token() {
    let (llm, kb) = mocks();
    let addr = serve(
        HttpServer::bind(engine(&llm, &kb), "127.0.0.1:0")
            .unwrap()
            .allow_unauthenticated_loopback(),
    );
    let r = post(addr, "/ask", Q, None);
    assert_eq!(r.status, 200, "{r:?}");
}

#[test]
fn question_length_is_capped() {
    let (llm, kb) = mocks();
    let limits = HttpLimits {
        max_question_bytes: 64,
        ..HttpLimits::default()
    };
    let addr = serve(
        HttpServer::bind(engine(&llm, &kb), "127.0.0.1:0")
            .unwrap()
            .with_token(TOKEN)
            .with_limits(limits),
    );
    let long = serde_json::json!({ "question": "x".repeat(65) }).to_string();
    let r = post(addr, "/ask", &long, Some(TOKEN));
    assert_eq!(r.status, 413, "{r:?}");
    assert_eq!(
        llm.call_count(),
        0,
        "an oversized question must not reach the LLM"
    );
    let ok = serde_json::json!({ "question": "x".repeat(64) }).to_string();
    assert_eq!(post(addr, "/ask", &ok, Some(TOKEN)).status, 200);
}

#[test]
fn llm_calls_per_request_are_capped() {
    // Every query returns no rows and every repair proposes a new one, so
    // without a budget one question costs 1 (intent) + 1 (generation) +
    // max_rounds repairs = 7 LLM calls.
    let mut script = vec![
        r#"{"qtype":"list"}"#.to_string(),
        r#"["SELECT ?x WHERE { ?x <http://ex/p0> ?o }"]"#.to_string(),
    ];
    for i in 1..=5 {
        script.push(format!("SELECT ?x WHERE {{ ?x <http://ex/p{i}> ?o }}"));
    }
    let llm = Arc::new(MockLlm::new(script));
    let empty = SparqlAnswer::Select {
        vars: vec!["x".into()],
        rows: vec![],
    };
    let kb = Arc::new(MockKb::new(vec![empty]));
    let engine = Arc::new(
        SolveEngine::new(Box::new(Arc::clone(&llm)), Box::new(Arc::clone(&kb)))
            .with_max_rounds(5)
            .with_graphrag(false),
    );
    let limits = HttpLimits {
        cost: CostLimits {
            max_llm_calls: Some(3),
            ..HttpLimits::default().cost
        },
        ..HttpLimits::default()
    };
    let addr = serve(
        HttpServer::bind(engine, "127.0.0.1:0")
            .unwrap()
            .with_token(TOKEN)
            .with_limits(limits),
    );
    let r = post(addr, "/ask", Q, Some(TOKEN));
    assert_eq!(r.status, 503, "{r:?}");
    assert!(r.body.contains("LLM call"), "{r:?}");
    assert_eq!(
        llm.call_count(),
        3,
        "exactly the budgeted number of LLM calls"
    );
}

#[test]
fn kb_rows_per_request_are_capped() {
    let llm = Arc::new(MockLlm::new(vec![
        r#"{"qtype":"list"}"#.to_string(),
        r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
    ]));
    let big = SparqlAnswer::Select {
        vars: vec!["x".into()],
        rows: (0..50).map(|i| row(&format!("http://ex/r{i}"))).collect(),
    };
    let kb = Arc::new(MockKb::new(vec![big]));
    let limits = HttpLimits {
        cost: CostLimits {
            max_kb_rows: Some(10),
            ..HttpLimits::default().cost
        },
        ..HttpLimits::default()
    };
    let addr = serve(
        HttpServer::bind(engine(&llm, &kb), "127.0.0.1:0")
            .unwrap()
            .with_token(TOKEN)
            .with_limits(limits),
    );
    let r = post(addr, "/ask", Q, Some(TOKEN));
    assert_eq!(r.status, 503, "{r:?}");
    assert!(
        !r.body.contains("http://ex/r0"),
        "rows beyond the budget leaked: {r:?}"
    );
    let g = post(addr, "/gSolve", Q, Some(TOKEN));
    assert_eq!(g.json()["status"], "error", "{g:?}");
}

#[test]
fn request_time_budget_is_enforced() {
    let (llm, kb) = mocks();
    let limits = HttpLimits {
        cost: CostLimits {
            timeout: Some(Duration::ZERO),
            ..HttpLimits::default().cost
        },
        ..HttpLimits::default()
    };
    let addr = serve(
        HttpServer::bind(engine(&llm, &kb), "127.0.0.1:0")
            .unwrap()
            .with_token(TOKEN)
            .with_limits(limits),
    );
    let r = post(addr, "/ask", Q, Some(TOKEN));
    assert_eq!(r.status, 503, "{r:?}");
    assert!(r.body.contains("time budget"), "{r:?}");
    assert_eq!(llm.call_count(), 0, "no LLM call after the deadline");
}

/// An LLM whose call runs into the request's deadline: it returns once the
/// budget's time is up, with the transport error a real client reports when
/// its network timeout (capped by the time left) fires.
struct DeadlineLlm;
impl gnlqa::LlmClient for DeadlineLlm {
    fn complete(&self, _req: &gnlqa::LlmRequest) -> gnlqa::Result<String> {
        // waits for wall-clock time to pass, not for another thread
        while gnlqa::budget::remaining_time().is_some_and(|t| !t.is_zero()) {
            thread::sleep(Duration::from_millis(5));
        }
        Err(gnlqa::Error::Http("timed out reading response".into()))
    }
}

#[test]
fn a_call_cut_short_by_the_time_budget_reports_the_budget() {
    // review (r5-gnlqa reviewer): the real clients cap their network timeout
    // by the request's remaining time, so a request that runs out of time
    // mid-call failed with that call's transport error — 500
    // "upstream_unavailable", blaming a healthy provider. It is the time
    // budget that ran out: 503 "limit_exceeded".
    let kb = Arc::new(MockKb::new(vec![one_row()]));
    let engine = Arc::new(SolveEngine::new(
        Box::new(DeadlineLlm),
        Box::new(Arc::clone(&kb)),
    ));
    let limits = HttpLimits {
        cost: CostLimits {
            timeout: Some(Duration::from_millis(200)),
            ..HttpLimits::default().cost
        },
        ..HttpLimits::default()
    };
    let addr = serve(
        HttpServer::bind(engine, "127.0.0.1:0")
            .unwrap()
            .with_token(TOKEN)
            .with_limits(limits),
    );
    let r = post(addr, "/ask", Q, Some(TOKEN));
    assert_eq!(r.status, 503, "{r:?}");
    assert_eq!(r.json()["code"], "limit_exceeded", "{r:?}");
}
