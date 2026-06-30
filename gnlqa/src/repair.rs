//! Self-repair loop. A generated SPARQL query may be invalid, error on
//! execution, or return no rows. Instead of giving up, we feed the problem (the
//! gStore parser error / execution error / "no results") plus the schema back to
//! the LLM and ask for a correction, up to `max_rounds` times. This is the
//! data-driven disambiguation of gAnswer, closed by an LLM loop.

use crate::error::{Error, Result};
use crate::generate::parse_candidates;
use crate::kb::{self, KbClient, SparqlAnswer};
use crate::llm::{LlmClient, LlmRequest};

const SYS_REPAIR: &str = "\
You fix a SPARQL 1.1 query that failed. Given the question, the schema, the \
previous query, and the problem, output ONLY the corrected query (one SPARQL \
statement, no prose, no markdown). Use ONLY URIs that appear in the schema; do \
not invent IRIs or prefixes.";

/// The result of a (possibly repaired) solve.
#[derive(Debug, Clone)]
pub struct RepairOutcome {
    pub sparql: String,
    pub answer: SparqlAnswer,
    /// How many repair rounds were used (0 = the initial query worked).
    pub rounds: usize,
}

/// Whether an answer is "empty" (worth a repair attempt). A `Boolean` is never
/// empty (false is a real answer); an empty SELECT or empty graph is.
pub fn is_empty_answer(ans: &SparqlAnswer) -> bool {
    match ans {
        SparqlAnswer::Select { rows, .. } => rows.is_empty(),
        SparqlAnswer::Boolean(_) => false,
        SparqlAnswer::Graph(g) => g.trim().is_empty(),
    }
}

/// Validate → execute → (on invalid / error / empty) repair, looping up to
/// `max_rounds`. Returns the first non-empty result, else the last successfully
/// executed (possibly empty) result, else an error.
pub fn solve_with_repair(
    llm: &dyn LlmClient,
    kb: &dyn KbClient,
    question: &str,
    schema_block: &str,
    initial: &str,
    max_rounds: usize,
    model: Option<&str>,
) -> Result<RepairOutcome> {
    let mut current = initial.trim().to_string();
    let mut last_ok: Option<(String, SparqlAnswer)> = None;

    for round in 0..=max_rounds {
        let last_round = round == max_rounds;

        // 1) Syntactic validity (gStore's parser).
        if let Err(e) = kb::validate_sparql(&current) {
            if last_round {
                break;
            }
            current = repair(llm, question, schema_block, &current, &format!("parser error: {e}"), model)?;
            continue;
        }

        // 2) Execute.
        match kb.query(&current) {
            Ok(ans) => {
                if !is_empty_answer(&ans) {
                    return Ok(RepairOutcome { sparql: current, answer: ans, rounds: round });
                }
                last_ok = Some((current.clone(), ans));
                if last_round {
                    break;
                }
                current = repair(
                    llm,
                    question,
                    schema_block,
                    &current,
                    "the query is valid but returned no results; fix it to return an answer",
                    model,
                )?;
            }
            Err(e) => {
                if last_round {
                    break;
                }
                current = repair(llm, question, schema_block, &current, &format!("execution error: {e}"), model)?;
            }
        }
    }

    if let Some((sparql, answer)) = last_ok {
        return Ok(RepairOutcome { sparql, answer, rounds: max_rounds });
    }
    Err(Error::Sparql("no valid, executable query after repair".into()))
}

/// One repair attempt: ask the LLM to correct `bad` given `issue`, return the
/// corrected SPARQL (first candidate, else the raw reply).
pub fn repair(
    llm: &dyn LlmClient,
    question: &str,
    schema_block: &str,
    bad: &str,
    issue: &str,
    model: Option<&str>,
) -> Result<String> {
    let user = format!(
        "Question: {question}\n\nSchema:\n{}\n\nPrevious query:\n{bad}\n\nProblem: {issue}\n\n\
         Output the corrected SPARQL query.",
        if schema_block.is_empty() { "(none)" } else { schema_block }
    );
    let mut req = LlmRequest::prompt(user).system(SYS_REPAIR).max_tokens(1024);
    if let Some(m) = model {
        req = req.model(m);
    }
    let raw = llm.complete(&req)?;
    Ok(parse_candidates(&raw).into_iter().next().unwrap_or_else(|| raw.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::{MockKb, RdfTerm, TermKind};
    use crate::llm::MockLlm;

    fn row(v: &str) -> Vec<Option<RdfTerm>> {
        vec![Some(RdfTerm { kind: TermKind::Uri, value: v.into(), datatype: None, lang: None })]
    }
    fn nonempty() -> SparqlAnswer {
        SparqlAnswer::Select { vars: vec!["x".into()], rows: vec![row("http://ex/a")] }
    }
    fn empty() -> SparqlAnswer {
        SparqlAnswer::Select { vars: vec!["x".into()], rows: vec![] }
    }

    #[test]
    fn initial_query_works_no_repair() {
        let llm = MockLlm::fixed("unused");
        let kb = MockKb::new(vec![nonempty()]);
        let out = solve_with_repair(&llm, &kb, "q", "", "SELECT ?x WHERE { ?x ?p ?o }", 2, None).unwrap();
        assert_eq!(out.rounds, 0);
        assert_eq!(out.answer.row_count(), 1);
    }

    #[test]
    fn repairs_invalid_then_succeeds() {
        // initial is invalid → LLM returns a valid query → KB returns a row
        let llm = MockLlm::fixed("SELECT ?x WHERE { ?x ?p ?o }");
        let kb = MockKb::new(vec![nonempty()]);
        let out = solve_with_repair(&llm, &kb, "q", "", "not sparql", 2, None).unwrap();
        assert_eq!(out.rounds, 1);
        assert_eq!(out.sparql, "SELECT ?x WHERE { ?x ?p ?o }");
    }

    #[test]
    fn repairs_empty_then_succeeds() {
        // valid initial → empty → repair → second query returns a row
        let llm = MockLlm::fixed("SELECT ?x WHERE { ?x <http://ex/p> ?o }");
        let kb = MockKb::new(vec![empty(), nonempty()]);
        let out = solve_with_repair(&llm, &kb, "q", "", "SELECT ?x WHERE { ?x ?p ?o }", 2, None).unwrap();
        assert_eq!(out.rounds, 1);
        assert_eq!(out.answer.row_count(), 1);
    }

    #[test]
    fn exhausts_rounds_returns_last_empty() {
        // every attempt is valid but empty → after max_rounds, return last empty
        let llm = MockLlm::fixed("SELECT ?x WHERE { ?x ?p ?o }");
        let kb = MockKb::new(vec![empty()]); // cycles → always empty
        let out = solve_with_repair(&llm, &kb, "q", "", "SELECT ?x WHERE { ?x ?p ?o }", 1, None).unwrap();
        assert!(is_empty_answer(&out.answer));
    }

    #[test]
    fn boolean_is_not_empty() {
        assert!(!is_empty_answer(&SparqlAnswer::Boolean(false)));
        assert!(is_empty_answer(&SparqlAnswer::Graph(" ".into())));
    }
}
