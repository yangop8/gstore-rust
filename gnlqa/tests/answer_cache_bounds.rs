//! review cluster-nlqa#19: the answer cache is bounded by bytes (and expires
//! entries by default in the shipped configuration), so clients cannot fill
//! it with very large answers.

use std::sync::Arc;

use gnlqa::{MockKb, MockLlm, RdfTerm, SolveEngine, SparqlAnswer, TermKind};

fn list_llm() -> Arc<MockLlm> {
    Arc::new(MockLlm::new(vec![
        r#"{"qtype":"list"}"#.to_string(),
        r#"["SELECT ?x WHERE { ?x <http://ex/type> <http://ex/GraduateStudent> }"]"#.to_string(),
    ]))
}

fn many_rows(n: usize) -> SparqlAnswer {
    SparqlAnswer::Select {
        vars: vec!["x".into()],
        rows: (0..n)
            .map(|i| {
                vec![Some(RdfTerm {
                    kind: TermKind::Uri,
                    value: format!("http://www.Department0.University0.edu/GraduateStudent{i:08}"),
                    datatype: None,
                    lang: None,
                })]
            })
            .collect(),
    }
}

#[test]
fn an_answer_too_large_for_the_cache_is_not_kept() {
    // ~15 MiB of answer (values + text) against the default 64 MiB budget.
    let llm = list_llm();
    let engine = SolveEngine::new(
        Box::new(Arc::clone(&llm)),
        Box::new(MockKb::new(vec![many_rows(100_000)])),
    )
    .with_citations(false)
    .with_cache(256);
    let a = engine.ask("list every graduate student").unwrap();
    assert_eq!(a.values.len(), 100_000);
    assert_eq!(llm.call_count(), 2);
    let _ = engine.ask("list every graduate student").unwrap();
    assert_eq!(
        llm.call_count(),
        4,
        "a multi-megabyte answer must not be pinned in the shared cache"
    );
}

#[test]
fn cached_bytes_stay_within_the_budget() {
    let llm = list_llm();
    let budget = 1 << 20; // 1 MiB
    let engine = SolveEngine::new(
        Box::new(Arc::clone(&llm)),
        Box::new(MockKb::new(vec![many_rows(500)])), // ~75 KiB per answer
    )
    .with_citations(false)
    .with_cache(256)
    .with_cache_max_bytes(budget);
    for i in 0..30 {
        engine
            .ask(&format!("list graduate students, phrasing {i}"))
            .unwrap();
        let u = engine.cache_usage().unwrap();
        assert!(u.bytes <= budget, "cache holds {} bytes", u.bytes);
    }
    let u = engine.cache_usage().unwrap();
    assert!(u.entries > 0 && u.entries < 30, "{u:?}");
    // the most recent answer is still served from the cache
    let calls = llm.call_count();
    engine.ask("list graduate students, phrasing 29").unwrap();
    assert_eq!(llm.call_count(), calls);
}
