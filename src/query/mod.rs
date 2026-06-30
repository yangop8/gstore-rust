//! The query engine: turn a parsed SPARQL query into results.
//!
//! Corresponds to gStore's `src/Query` plus the `Executor`/`Join`/`Optimizer`
//! pieces of `src/Database`. See [`engine::Evaluator`] for the evaluation
//! pipeline and `docs/DESIGN.md` §6.

pub mod candidates;
pub mod engine;
pub mod hash;
pub mod optimizer;
pub mod planner;
pub mod results;
pub mod value;

pub use engine::Evaluator;
pub use results::{QueryResult, ResultSet};
pub use value::Value;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Dictionary;
    use crate::model::{IdTriple, Term};
    use crate::parser::sparql::parse;
    use crate::store::TripleStore;

    /// A tiny FOAF-ish graph, built directly in id-space, plus its dictionary.
    ///
    /// alice knows bob; alice salary 2500; bob salary 3000; alice name "Alice".
    fn fixture() -> (Dictionary, TripleStore) {
        let mut d = Dictionary::new();
        let alice = d.intern_term(&Term::iri("http://ex/alice"));
        let bob = d.intern_term(&Term::iri("http://ex/bob"));
        let knows = d.intern_predicate(&Term::iri("http://ex/knows").dict_key());
        let salary = d.intern_predicate(&Term::iri("http://ex/salary").dict_key());
        let name = d.intern_predicate(&Term::iri("http://ex/name").dict_key());
        let xsd_int = "http://www.w3.org/2001/XMLSchema#integer";
        let s2500 = d.intern_term(&Term::typed_literal("2500", xsd_int));
        let s3000 = d.intern_term(&Term::typed_literal("3000", xsd_int));
        let alice_name = d.intern_term(&Term::plain_literal("Alice"));

        let mut store = TripleStore::new();
        store.bulk_load(vec![
            IdTriple::new(alice, knows, bob),
            IdTriple::new(alice, salary, s2500),
            IdTriple::new(bob, salary, s3000),
            IdTriple::new(alice, name, alice_name),
        ]);
        (d, store)
    }

    fn run(d: &Dictionary, s: &TripleStore, q: &str) -> ResultSet {
        let query = parse(q).unwrap();
        match Evaluator::new(d, s).evaluate(&query).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT result, got {other:?}"),
        }
    }

    #[test]
    fn single_pattern_with_const_predicate() {
        let (d, s) = fixture();
        let rs = run(
            &d,
            &s,
            "SELECT ?p WHERE { <http://ex/alice> <http://ex/knows> ?p }",
        );
        assert_eq!(rs.vars, vec!["p"]);
        assert_eq!(rs.rows, vec![vec![Some("<http://ex/bob>".into())]]);
    }

    #[test]
    fn two_pattern_join_on_shared_var() {
        let (d, s) = fixture();
        // who does alice know, and what's that person's salary?
        let rs = run(
            &d,
            &s,
            "SELECT ?f ?sal WHERE {
                <http://ex/alice> <http://ex/knows> ?f .
                ?f <http://ex/salary> ?sal .
             }",
        );
        assert_eq!(rs.row_count(), 1);
        assert_eq!(rs.rows[0][0], Some("<http://ex/bob>".into()));
        assert_eq!(
            rs.rows[0][1],
            Some("\"3000\"^^<http://www.w3.org/2001/XMLSchema#integer>".into())
        );
    }

    #[test]
    fn filter_numeric_comparison() {
        let (d, s) = fixture();
        let rs = run(
            &d,
            &s,
            "SELECT ?x WHERE { ?x <http://ex/salary> ?sal . FILTER(?sal < 2800) }",
        );
        assert_eq!(rs.row_count(), 1);
        assert_eq!(rs.rows[0][0], Some("<http://ex/alice>".into()));
    }

    #[test]
    fn filter_with_abs_and_arithmetic() {
        let (d, s) = fixture();
        // alice(2500) knows bob(3000); |2500-3000| = 500 < 1000 → keep
        let rs = run(
            &d,
            &s,
            "SELECT ?a ?b WHERE {
                ?a <http://ex/knows> ?b .
                ?a <http://ex/salary> ?sa .
                ?b <http://ex/salary> ?sb .
                FILTER(abs(?sa - ?sb) < 1000)
             }",
        );
        assert_eq!(rs.row_count(), 1);
    }

    #[test]
    fn select_star_lists_all_vars() {
        let (d, s) = fixture();
        let rs = run(&d, &s, "SELECT * WHERE { ?s <http://ex/salary> ?o }");
        assert_eq!(rs.vars, vec!["s", "o"]);
        assert_eq!(rs.row_count(), 2);
    }

    #[test]
    fn distinct_removes_duplicates() {
        let (d, s) = fixture();
        // project only the predicate-less subject via two patterns that both
        // bind ?x to alice → duplicate rows collapsed by DISTINCT.
        let rs = run(
            &d,
            &s,
            "SELECT DISTINCT ?x WHERE { ?x <http://ex/salary> ?any }",
        );
        assert_eq!(rs.row_count(), 2); // alice, bob (already distinct)
    }

    #[test]
    fn order_by_desc_and_limit() {
        let (d, s) = fixture();
        let rs = run(
            &d,
            &s,
            "SELECT ?x ?sal WHERE { ?x <http://ex/salary> ?sal } ORDER BY DESC(?sal) LIMIT 1",
        );
        assert_eq!(rs.row_count(), 1);
        // highest salary is bob's 3000
        assert_eq!(rs.rows[0][0], Some("<http://ex/bob>".into()));
    }

    #[test]
    fn order_by_asc_default() {
        let (d, s) = fixture();
        let rs = run(
            &d,
            &s,
            "SELECT ?sal WHERE { ?x <http://ex/salary> ?sal } ORDER BY ?sal",
        );
        assert_eq!(rs.row_count(), 2);
        // ascending: 2500 then 3000
        assert!(rs.rows[0][0].as_ref().unwrap().contains("2500"));
        assert!(rs.rows[1][0].as_ref().unwrap().contains("3000"));
    }

    #[test]
    fn missing_constant_yields_empty() {
        let (d, s) = fixture();
        let rs = run(
            &d,
            &s,
            "SELECT ?o WHERE { <http://ex/nobody> <http://ex/knows> ?o }",
        );
        assert!(rs.is_empty());
    }

    #[test]
    fn full_scan_pattern_all_vars() {
        let (d, s) = fixture();
        let rs = run(&d, &s, "SELECT * WHERE { ?s ?p ?o }");
        assert_eq!(rs.row_count(), 4); // all four triples
    }

    #[test]
    fn ask_returns_boolean() {
        let (d, s) = fixture();
        let q = parse("ASK { <http://ex/alice> <http://ex/knows> <http://ex/bob> }").unwrap();
        match Evaluator::new(&d, &s).evaluate(&q).unwrap() {
            QueryResult::Ask(b) => assert!(b),
            other => panic!("expected Ask, got {other:?}"),
        }
        let q2 = parse("ASK { <http://ex/bob> <http://ex/knows> <http://ex/alice> }").unwrap();
        match Evaluator::new(&d, &s).evaluate(&q2).unwrap() {
            QueryResult::Ask(b) => assert!(!b),
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn repeated_variable_in_pattern() {
        // self-loop matching: nobody knows themselves → empty.
        let (d, s) = fixture();
        let rs = run(&d, &s, "SELECT ?x WHERE { ?x <http://ex/knows> ?x }");
        assert!(rs.is_empty());
    }

    #[test]
    fn union_concatenates_branch_solutions() {
        let (d, s) = fixture();
        // who has a salary OR knows someone → alice (both), bob (salary)
        let rs = run(
            &d,
            &s,
            "SELECT DISTINCT ?x WHERE {
                { ?x <http://ex/salary> ?v } UNION { ?x <http://ex/knows> ?w }
             }",
        );
        let mut got: Vec<String> = rs.rows.iter().map(|r| r[0].clone().unwrap()).collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                "<http://ex/alice>".to_string(),
                "<http://ex/bob>".to_string()
            ]
        );
    }

    #[test]
    fn union_joined_with_trailing_pattern() {
        let (d, s) = fixture();
        // (alice|bob via salary) AND knows bob → only alice knows bob
        let rs = run(
            &d,
            &s,
            "SELECT ?x WHERE {
                { ?x <http://ex/salary> ?v } UNION { ?x <http://ex/name> ?n }
                ?x <http://ex/knows> <http://ex/bob>
             }",
        );
        let mut got: Vec<String> = rs.rows.iter().map(|r| r[0].clone().unwrap()).collect();
        got.sort();
        got.dedup();
        assert_eq!(got, vec!["<http://ex/alice>".to_string()]);
    }

    #[test]
    fn predicate_variable_binds_and_resolves() {
        let (d, s) = fixture();
        let rs = run(
            &d,
            &s,
            "SELECT ?p WHERE { <http://ex/alice> ?p <http://ex/bob> }",
        );
        assert_eq!(rs.row_count(), 1);
        assert_eq!(rs.rows[0][0], Some("<http://ex/knows>".into()));
    }
}
