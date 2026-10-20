//! Hardening regressions: query-path panics, dictionary id-space bounds,
//! password hashing, and the SPARQL `LOAD` file-access policy.

use std::panic::{catch_unwind, AssertUnwindSafe};

use gstore::dict::Dictionary;
use gstore::parser::sparql::ast::{
    Expr, GraphPattern, PatternTerm, Projection, Query, SelectItem, SelectQuery, TriplePattern,
};
use gstore::parser::sparql::builtins::BUILTINS;
use gstore::query::Evaluator;
use gstore::store::TripleStore;
use gstore::{Database, GStoreError, IdTriple, QueryResult, Term};

const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/b> <http://ex/p> <http://ex/c> .\n\
                    <http://ex/a> <http://ex/name> \"Alice\" .\n\
                    <http://ex/a> <http://ex/age> \"42\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n";

// ---- 1. built-in arity / shape -------------------------------------------

/// The exact crash from the gap report: `FILTER(STR())` panicked the query
/// thread (index out of bounds). It is now a parse error naming the function.
#[test]
fn str_with_no_arguments_is_a_clean_parse_error() {
    let mut db = Database::build_from_str("h", DATA).unwrap();
    let err = db
        .query("SELECT ?s WHERE { ?s <http://ex/p> ?o FILTER(STR()) }")
        .unwrap_err();
    match err {
        GStoreError::SparqlParse(msg) => {
            assert_eq!(msg, "STR expects exactly 1 argument, got 0");
        }
        other => panic!("expected a SPARQL parse error, got {other:?}"),
    }
}

/// Argument expressions cycled to build calls of any arity: a variable bound
/// to an IRI, a constant IRI, an integer, and a string.
const ARG_POOL: [&str; 4] = ["?o", "<http://ex/a>", "2", "\"x\""];

fn call(name: &str, n: usize) -> String {
    let args: Vec<&str> = ARG_POOL.iter().cycle().take(n).copied().collect();
    format!("{name}({})", args.join(", "))
}

/// Table-driven: every built-in the engine recognizes, called with 0–4
/// arguments in FILTER, projection, BIND and ORDER BY position, through the
/// public `Database::query` API. No call may panic; a call with an arity the
/// built-in rejects must be a parse error naming the function.
#[test]
fn every_builtin_with_zero_to_four_args_never_panics() {
    let mut db = Database::build_from_str("arity", DATA).unwrap();
    let mut panics = Vec::new();
    let mut checked = 0;
    for spec in BUILTINS {
        for n in 0..=4 {
            let c = call(spec.name, n);
            let queries = [
                format!("SELECT ?s WHERE {{ ?s <http://ex/p> ?o FILTER({c}) }}"),
                format!("SELECT ?s ({c} AS ?r) WHERE {{ ?s <http://ex/p> ?o }}"),
                format!("SELECT ?s ?r WHERE {{ ?s <http://ex/p> ?o BIND({c} AS ?r) }}"),
                format!("SELECT ?s WHERE {{ ?s <http://ex/p> ?o }} ORDER BY ({c})"),
                format!("ASK {{ ?s <http://ex/p> ?o FILTER(!{c} || {c}) }}"),
            ];
            for q in &queries {
                checked += 1;
                let outcome = catch_unwind(AssertUnwindSafe(|| db.query(q)));
                match outcome {
                    Err(_) => panics.push(q.clone()),
                    Ok(res) => {
                        if !spec.arity.accepts(n) {
                            match res {
                                Err(GStoreError::SparqlParse(msg)) => assert!(
                                    msg.contains(spec.name) && msg.contains("expects"),
                                    "{q}: unhelpful error {msg:?}"
                                ),
                                other => panic!("{q}: wrong arity must not parse, got {other:?}"),
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(panics.is_empty(), "queries that panicked: {panics:#?}");
    assert!(checked >= BUILTINS.len() * 5 * 5);
}

/// BOUND only takes a variable (SPARQL grammar `BOUND '(' Var ')'`).
#[test]
fn bound_requires_a_variable() {
    let mut db = Database::build_from_str("b", DATA).unwrap();
    let err = db
        .query("SELECT ?s WHERE { ?s <http://ex/p> ?o FILTER(BOUND(<http://ex/a>)) }")
        .unwrap_err();
    assert!(
        err.to_string().contains("BOUND expects a variable"),
        "{err}"
    );
    let ok = db
        .select("SELECT ?s WHERE { ?s <http://ex/p> ?o FILTER(BOUND(?o)) }")
        .unwrap();
    assert_eq!(ok.row_count(), 2);
}

/// Valid calls keep working (spot check across the table's shapes).
#[test]
fn well_formed_builtin_calls_still_evaluate() {
    let mut db = Database::build_from_str("ok", DATA).unwrap();
    let rs = db
        .select(
            "SELECT (STR(?o) AS ?a) (CONCAT() AS ?b) (COALESCE(?nope, 1) AS ?c) \
                    (REGEX(\"abc\", \"B\", \"i\") AS ?d) (SUBSTR(\"abc\", 2) AS ?e) \
                    (IF(true, 1, 2) AS ?f) (BNODE() AS ?g) \
             WHERE { <http://ex/a> <http://ex/p> ?o }",
        )
        .unwrap();
    assert_eq!(rs.row_count(), 1);
    let row = &rs.rows[0];
    assert_eq!(row[0].as_deref(), Some("\"http://ex/b\""));
    assert_eq!(row[1].as_deref(), Some("\"\""));
    assert!(row[2].as_deref().unwrap().starts_with("\"1\""));
    assert!(row[3].as_deref().unwrap().contains("true"));
    assert_eq!(row[4].as_deref(), Some("\"bc\""));
    assert!(row[5].as_deref().unwrap().starts_with("\"1\""));
    assert!(row[6].as_deref().unwrap().starts_with("_:"));
}

/// An AST built without the parser (so arity was never validated) must still
/// not panic the evaluator: a wrong-arity built-in is an evaluation error.
#[test]
fn hand_built_wrong_arity_ast_does_not_panic_the_evaluator() {
    let mut d = Dictionary::new();
    let a = d.intern_term(&Term::iri("http://ex/a"));
    let b = d.intern_term(&Term::iri("http://ex/b"));
    let p = d.intern_predicate(&Term::iri("http://ex/p").dict_key());
    let mut store = TripleStore::new();
    store.bulk_load(vec![IdTriple::new(a, p, b)]);

    let bgp = GraphPattern::Bgp(vec![TriplePattern {
        subject: PatternTerm::Var("s".into()),
        predicate: PatternTerm::Term(Term::iri("http://ex/p")),
        object: PatternTerm::Var("o".into()),
    }]);
    let mut panics = Vec::new();
    for spec in BUILTINS {
        for n in 0..=4 {
            let args: Vec<Expr> = (0..n)
                .map(|i| match i % 3 {
                    0 => Expr::Var("o".into()),
                    1 => Expr::Const(Term::iri("http://ex/a")),
                    _ => Expr::Var("unbound".into()),
                })
                .collect();
            let call = Expr::Builtin(spec.name.to_string(), args);
            let q = Query::Select(SelectQuery {
                distinct: false,
                projection: Projection::Items(vec![
                    SelectItem::Var("s".into()),
                    SelectItem::Expr(call.clone(), "r".into()),
                ]),
                pattern: GraphPattern::Filter(vec![call], Box::new(bgp.clone())),
                group_by: vec![],
                having: vec![],
                order_by: vec![],
                limit: None,
                offset: None,
            });
            let r = catch_unwind(AssertUnwindSafe(|| Evaluator::new(&d, &store).evaluate(&q)));
            match r {
                Err(_) => panics.push(format!("{}/{n}", spec.name)),
                Ok(Ok(QueryResult::Select(rs))) => {
                    if !spec.arity.accepts(n) {
                        assert!(
                            rs.is_empty(),
                            "{}/{n}: an invalid call filters out every row",
                            spec.name
                        );
                    }
                }
                Ok(other) => panic!("{}/{n}: unexpected {other:?}", spec.name),
            }
        }
    }
    assert!(panics.is_empty(), "built-ins that panicked: {panics:?}");
}

/// Other crafted-query panics found in the audit.
#[test]
fn crafted_queries_found_by_the_audit_do_not_panic() {
    let mut db = Database::build_from_str("audit", DATA).unwrap();
    let queries = [
        // Non-deterministic ORDER BY key: inconsistent comparator ⇒ sort panic.
        "SELECT ?s WHERE { ?s ?p ?o } ORDER BY (RAND())",
        "SELECT ?s (STRUUID() AS ?u) WHERE { ?s ?p ?o } ORDER BY DESC(?u)",
        // i64 overflow in SUBSTR's start + length.
        "SELECT (SUBSTR(\"abc\", 9e18, 9e18) AS ?x) WHERE { ?s ?p ?o }",
        // i64 overflow converting a huge-year dateTime to an instant.
        "SELECT ?s WHERE { ?s ?p ?o FILTER(\"999999999999999999-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime> > \"2000-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>) }",
        "SELECT ?s WHERE { ?s ?p ?o } ORDER BY (\"-9223372036854775807-01-01\"^^<http://www.w3.org/2001/XMLSchema#date>)",
        // Zero-arg substring-filter shapes reach the StringIndex pre-pass.
        "SELECT ?s WHERE { ?s ?p ?o FILTER(CONTAINS(STR(), \"abc\")) }",
    ];
    for q in queries {
        let r = catch_unwind(AssertUnwindSafe(|| db.query(q)));
        assert!(r.is_ok(), "panicked: {q}");
    }
}
