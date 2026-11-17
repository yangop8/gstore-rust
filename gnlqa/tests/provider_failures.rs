//! review cluster-nlqa#16: LLM / KB failures are surfaced instead of being
//! swallowed — the intent call, the GraphRAG fallback and the linker build.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gnlqa::{
    Error, HashEmbedder, KbClient, Linker, LlmClient, LlmRequest, MockKb, MockLlm, RdfTerm,
    SolveEngine, SparqlAnswer, TermKind,
};

fn uri(v: &str) -> Option<RdfTerm> {
    Some(RdfTerm {
        kind: TermKind::Uri,
        value: v.into(),
        datatype: None,
        lang: None,
    })
}

/// Fails its first call with a provider error, then answers from `inner`.
struct FirstCallFails {
    calls: AtomicUsize,
    inner: MockLlm,
}
impl LlmClient for FirstCallFails {
    fn complete(&self, req: &LlmRequest) -> gnlqa::Result<String> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(Error::Http("HTTP 429: rate_limit_error".into()));
        }
        self.inner.complete(req)
    }
}

#[test]
fn a_provider_failure_on_the_intent_call_ends_the_request() {
    let llm = Arc::new(FirstCallFails {
        calls: AtomicUsize::new(0),
        inner: MockLlm::fixed(r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#),
    });
    let kb = MockKb::new(vec![SparqlAnswer::Select {
        vars: vec!["x".into()],
        rows: vec![vec![uri("http://ex/a")]],
    }]);
    let engine = SolveEngine::new(Box::new(Arc::clone(&llm)), Box::new(kb));
    let err = engine.ask("which x?").unwrap_err();
    assert!(
        matches!(err, Error::Http(ref m) if m.contains("429")),
        "{err:?}"
    );
    assert_eq!(
        llm.calls.load(Ordering::SeqCst),
        1,
        "no ungrounded generation after the provider failed"
    );
}

#[test]
fn a_malformed_intent_still_degrades() {
    // Not an outage: the request continues without an intent (and says so).
    let llm = MockLlm::new(vec![
        "this is not JSON".to_string(),
        r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
    ]);
    let kb = MockKb::new(vec![SparqlAnswer::Select {
        vars: vec!["x".into()],
        rows: vec![vec![uri("http://ex/a")]],
    }]);
    let a = SolveEngine::new(Box::new(llm), Box::new(kb))
        .with_citations(false)
        .ask("which x?")
        .unwrap();
    assert_eq!(a.values, vec!["http://ex/a"]);
}

/// Scripted LLM: replies in order; `Err` entries become provider errors.
struct ScriptLlm(std::sync::Mutex<Vec<Result<String, String>>>);
impl LlmClient for ScriptLlm {
    fn complete(&self, _req: &LlmRequest) -> gnlqa::Result<String> {
        let mut s = self.0.lock().unwrap();
        match if s.is_empty() {
            Ok(String::new())
        } else {
            s.remove(0)
        } {
            Ok(t) => Ok(t),
            Err(m) => Err(Error::Http(m)),
        }
    }
}

/// The generated query finds nothing; neighbourhood queries either answer
/// with one edge or (if `retrieval_down`) fail like a dead endpoint.
struct GraphKb {
    retrieval_down: bool,
}
impl KbClient for GraphKb {
    fn query(&self, sparql: &str) -> gnlqa::Result<SparqlAnswer> {
        if sparql.starts_with("SELECT ?p ?o") || sparql.starts_with("SELECT ?s ?p") {
            if self.retrieval_down {
                return Err(Error::Http("gStore endpoint unreachable".into()));
            }
            let (a, b) = if sparql.starts_with("SELECT ?p ?o") {
                ("p", "o")
            } else {
                ("s", "p")
            };
            return Ok(SparqlAnswer::Select {
                vars: vec![a.into(), b.into()],
                rows: vec![vec![uri("http://ex/directed"), uri("http://ex/Scott")]],
            });
        }
        Ok(SparqlAnswer::Select {
            vars: vec!["x".into()],
            rows: vec![],
        })
    }
}

fn alien_linker() -> Linker {
    Linker::from_labels(
        Box::new(HashEmbedder::new(64)),
        &[("http://ex/Alien".to_string(), "Alien".to_string())],
        &[],
        &[],
        0.0,
    )
    .unwrap()
}

const INTENT: &str = r#"{"qtype":"open","mentions":[{"text":"Alien","kind":"entity"}]}"#;
const GEN: &str = r#"["SELECT ?x WHERE { ?x <http://ex/p> <http://ex/Alien> }"]"#;

#[test]
fn a_graphrag_llm_outage_is_reported_not_masked() {
    let llm = ScriptLlm(std::sync::Mutex::new(vec![
        Ok(INTENT.into()),
        Ok(GEN.into()),
        Err("HTTP 503: provider down".into()),
    ]));
    let engine = SolveEngine::new(
        Box::new(llm),
        Box::new(GraphKb {
            retrieval_down: false,
        }),
    )
    .with_linker(alien_linker())
    .with_max_rounds(0);
    let r = engine.ask("tell me about Alien");
    assert!(
        matches!(r, Err(Error::Http(ref m)) if m.contains("503")),
        "the GraphRAG outage must surface, got {r:?}"
    );
}

#[test]
fn a_kb_outage_during_graphrag_retrieval_is_reported() {
    let llm = ScriptLlm(std::sync::Mutex::new(vec![
        Ok(INTENT.into()),
        Ok(GEN.into()),
        Ok("SHOULD NOT BE CALLED".into()),
    ]));
    let engine = SolveEngine::new(
        Box::new(llm),
        Box::new(GraphKb {
            retrieval_down: true,
        }),
    )
    .with_linker(alien_linker())
    .with_max_rounds(0);
    let r = engine.ask("tell me about Alien");
    assert!(
        matches!(r, Err(Error::Http(ref m)) if m.contains("unreachable")),
        "a dead endpoint during retrieval must surface, got {r:?}"
    );
}

/// Fails like a dead endpoint for its first `fail` queries, then serves the
/// linker's label / predicate / type queries.
struct FlakyLabelKb {
    fail: usize,
    calls: AtomicUsize,
}
impl KbClient for FlakyLabelKb {
    fn query(&self, sparql: &str) -> gnlqa::Result<SparqlAnswer> {
        if self.calls.fetch_add(1, Ordering::SeqCst) < self.fail {
            return Err(Error::Http("gStore endpoint unreachable".into()));
        }
        Ok(if sparql.contains("?l") {
            SparqlAnswer::Select {
                vars: vec!["s".into(), "l".into()],
                rows: vec![vec![
                    uri("http://ex/Alien"),
                    Some(RdfTerm {
                        kind: TermKind::Literal,
                        value: "Alien".into(),
                        datatype: None,
                        lang: None,
                    }),
                ]],
            }
        } else {
            SparqlAnswer::Select {
                vars: vec!["p".into(), "t".into()],
                rows: vec![],
            }
        })
    }
}

#[test]
fn a_failed_linker_build_is_retried_later() {
    let kb = Arc::new(FlakyLabelKb {
        fail: 1,
        calls: AtomicUsize::new(0),
    });
    let kb2 = Arc::clone(&kb);
    let engine = SolveEngine::new(Box::new(MockLlm::fixed("x")), Box::new(MockKb::new(vec![])))
        .with_linker_builder(Duration::ZERO, move || {
            Linker::build_from_kb(kb2.as_ref(), Box::new(HashEmbedder::new(64)), 0.0, None)
        });
    assert!(!engine.prepare_linker(), "the first build hits the outage");
    let linker = engine.linker().expect("the next use rebuilds it");
    assert_eq!(linker.sizes().0, 1);
    let used = kb.calls.load(Ordering::SeqCst);
    assert!(engine.linker().is_some());
    assert_eq!(
        kb.calls.load(Ordering::SeqCst),
        used,
        "built once, then kept"
    );
}

#[test]
fn a_failed_linker_build_waits_for_the_retry_interval() {
    let kb = Arc::new(FlakyLabelKb {
        fail: usize::MAX,
        calls: AtomicUsize::new(0),
    });
    let kb2 = Arc::clone(&kb);
    let engine = SolveEngine::new(Box::new(MockLlm::fixed("x")), Box::new(MockKb::new(vec![])))
        .with_linker_builder(Duration::from_secs(3600), move || {
            Linker::build_from_kb(kb2.as_ref(), Box::new(HashEmbedder::new(64)), 0.0, None)
        });
    assert!(engine.linker().is_none());
    assert!(engine.linker().is_none());
    assert_eq!(
        kb.calls.load(Ordering::SeqCst),
        1,
        "no rebuild storm: one attempt per retry interval"
    );
}
