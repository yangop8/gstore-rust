//! The read-only gate every generated, repaired or client-supplied query
//! passes before it reaches the store: no SPARQL UPDATE (guarded since
//! before the review; here also through the repair loop, review
//! cluster-nlqa#24) and no SERVICE federation anywhere (review
//! cluster-nlqa#23).

use std::sync::Arc;

use gnlqa::{
    solve_with_repair, validate_readonly_sparql, McpServer, MockKb, MockLlm, SolveEngine,
    SparqlAnswer,
};

const SSRF: &str = "<http://169.254.169.254/latest/>";

#[test]
fn service_is_refused_wherever_it_appears() {
    let cases = [
        format!("SELECT ?s WHERE {{ SERVICE {SSRF} {{ ?s ?p ?o }} }}"),
        format!("SELECT ?s WHERE {{ SERVICE SILENT {SSRF} {{ ?s ?p ?o }} }}"),
        format!("SELECT ?s WHERE {{ ?s <http://ex/p> ?o . {{ SERVICE {SSRF} {{ ?s ?p ?x }} }} }}"),
        format!("SELECT ?s WHERE {{ ?s <http://ex/p> ?o OPTIONAL {{ SERVICE {SSRF} {{ ?s ?q ?z }} }} }}"),
        format!("SELECT ?s WHERE {{ {{ ?s <http://ex/p> ?o }} UNION {{ SERVICE {SSRF} {{ ?s ?p ?o }} }} }}"),
        format!("SELECT ?s WHERE {{ ?s <http://ex/p> ?o FILTER EXISTS {{ SERVICE {SSRF} {{ ?s ?p ?o }} }} }}"),
        format!("SELECT ?s WHERE {{ ?s <http://ex/p> ?o FILTER NOT EXISTS {{ SERVICE {SSRF} {{ ?s ?p ?o }} }} }}"),
        format!("SELECT ?s WHERE {{ {{ SELECT ?s WHERE {{ SERVICE {SSRF} {{ ?s ?p ?o }} }} }} }}"),
        format!("SELECT ?s ?b WHERE {{ ?s <http://ex/p> ?o BIND(EXISTS {{ SERVICE {SSRF} {{ ?s ?p ?o }} }} AS ?b) }}"),
        format!("SELECT ?s (EXISTS {{ SERVICE {SSRF} {{ ?s ?p ?o }} }} AS ?b) WHERE {{ ?s <http://ex/p> ?o }}"),
        format!("SELECT ?s WHERE {{ GRAPH <http://ex/g> {{ SERVICE {SSRF} {{ ?s ?p ?o }} }} }}"),
        format!("SELECT ?s WHERE {{ ?s <http://ex/p> ?o MINUS {{ SERVICE {SSRF} {{ ?s ?p ?o }} }} }}"),
        format!("ASK {{ SERVICE {SSRF} {{ ?s ?p ?o }} }}"),
    ];
    for q in &cases {
        gstore::parser::sparql::parse(q).unwrap_or_else(|e| panic!("case must parse: {q}: {e}"));
        let err = validate_readonly_sparql(q).expect_err(q);
        assert!(err.to_string().contains("SERVICE"), "{q}: {err}");
    }
}

#[test]
fn ordinary_read_queries_still_pass() {
    for q in [
        "SELECT ?s WHERE { ?s <http://ex/p> ?o }",
        "SELECT ?s WHERE { ?s <http://ex/p> ?o FILTER EXISTS { ?s <http://ex/q> ?z } }",
        "SELECT (COUNT(?s) AS ?n) WHERE { ?s <http://ex/p> ?o } GROUP BY ?o HAVING (COUNT(?s) > 1)",
        "ASK { <http://ex/a> <http://ex/p> ?o }",
        // the word in a literal or IRI is not a SERVICE clause
        "SELECT ?s WHERE { ?s <http://ex/SERVICE> \"SERVICE <http://x/>\" }",
    ] {
        validate_readonly_sparql(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
}

#[test]
fn mcp_run_sparql_refuses_service() {
    let kb = Arc::new(MockKb::new(vec![]));
    let srv = McpServer::new(Arc::new(SolveEngine::new(
        Box::new(MockLlm::fixed("x")),
        Box::new(Arc::clone(&kb)),
    )));
    let q = format!("SELECT ?s WHERE {{ SERVICE {SSRF} {{ ?s ?p ?o }} }}");
    let req = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "run_sparql", "arguments": { "query": q } }
    });
    let v: serde_json::Value =
        serde_json::from_str(&srv.handle_message(&req.to_string()).unwrap()).unwrap();
    assert_eq!(v["result"]["isError"], true);
    assert!(v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("SERVICE"));
    assert_eq!(kb.query_count(), 0, "the store never saw the query");
}

#[test]
fn generated_service_candidates_never_run() {
    let llm = MockLlm::new(vec![
        r#"{"qtype":"list"}"#.to_string(),
        format!(r#"["SELECT ?s WHERE {{ SERVICE {SSRF} {{ ?s <http://ex/p> ?o }} }}"]"#),
    ]);
    let kb = Arc::new(MockKb::new(vec![]));
    let engine = SolveEngine::new(Box::new(llm), Box::new(Arc::clone(&kb))).with_graphrag(false);
    assert!(engine.ask("which s?").is_err());
    assert_eq!(kb.query_count(), 0);
}

#[test]
fn a_repair_that_proposes_an_update_or_service_is_never_executed() {
    // review cluster-nlqa#24: the repair loop shares the gate with generation.
    let empty = SparqlAnswer::Select {
        vars: vec!["x".into()],
        rows: vec![],
    };
    let kb = MockKb::new(vec![empty]);
    let llm = MockLlm::new(vec![
        "DELETE WHERE { ?s ?p ?o }".to_string(),
        format!("SELECT ?x WHERE {{ SERVICE {SSRF} {{ ?x ?p ?o }} }}"),
        "INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> }".to_string(),
    ]);
    let out = solve_with_repair(
        &llm,
        &kb,
        "q",
        "",
        "",
        "SELECT ?x WHERE { ?x <http://ex/p> ?o }",
        3,
        None,
    )
    .unwrap();
    assert_eq!(
        kb.query_count(),
        1,
        "only the initial read query reached the store"
    );
    assert_eq!(out.sparql, "SELECT ?x WHERE { ?x <http://ex/p> ?o }");
}
