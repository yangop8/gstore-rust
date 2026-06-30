//! Answer grounding: attach the supporting KG triples to an answer (so it can be
//! cited / audited, controlling hallucination), and optionally have the LLM
//! phrase a natural-language explanation from them.

use crate::error::Result;
use crate::kb::{KbClient, SparqlAnswer, TermKind};
use crate::llm::{LlmClient, LlmRequest};
use crate::schema::checked_iri;

/// One supporting triple. All three fields are SPARQL/Turtle surface forms
/// (`<uri>` / `"lit"`), so they render uniformly.
#[derive(Debug, Clone, PartialEq)]
pub struct Citation {
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

/// Collect related facts about the URI entities in `answer` (`<uri> ?p ?o`,
/// ordered for determinism), capped per-entity, by number of entities, and
/// overall. Best-effort: a failing entity query is skipped, not fatal.
///
/// Note: these are facts *about* the answer entities, not necessarily the exact
/// matched evidence (that lives in the executed BGP); they ground the answer and
/// support a cited explanation.
pub fn gather_citations(
    kb: &dyn KbClient,
    answer: &SparqlAnswer,
    per_entity: usize,
    max_entities: usize,
    max_total: usize,
) -> Result<Vec<Citation>> {
    let mut out = Vec::new();
    for uri in answer_uris(answer).into_iter().take(max_entities) {
        if out.len() >= max_total {
            break;
        }
        let Ok(uri) = checked_iri(&uri) else { continue };
        let q = format!("SELECT ?p ?o WHERE {{ <{uri}> ?p ?o }} ORDER BY ?p ?o LIMIT {per_entity}");
        match kb.query(&q) {
            Ok(SparqlAnswer::Select { vars, rows }) => {
                let (pi, oi) = (pos(&vars, "p"), pos(&vars, "o"));
                for r in rows {
                    let (Some(p), Some(o)) = (cell(&r, pi), cell(&r, oi)) else { continue };
                    out.push(Citation { subject: format!("<{uri}>"), predicate: p, object: o });
                    if out.len() >= max_total {
                        break;
                    }
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("gnlqa: warning: citation query for <{uri}>: {e}"),
        }
    }
    Ok(out)
}

/// Ask the LLM to phrase a concise answer grounded strictly in `citations`.
/// Returns `None` if there are no citations or the LLM call fails (the caller
/// keeps the raw rendered answer rather than risk ungrounded prose).
pub fn explain(
    llm: &dyn LlmClient,
    question: &str,
    citations: &[Citation],
    model: Option<&str>,
) -> Option<String> {
    if citations.is_empty() {
        return None;
    }
    let cites = citations
        .iter()
        .map(|c| format!("{} {} {}", c.subject, c.predicate, c.object))
        .collect::<Vec<_>>()
        .join("\n");
    let sys = "You write a concise, factual natural-language answer to the \
        question using ONLY the supporting triples. Do not add facts not present \
        in them. One or two sentences.";
    let user = format!("Question: {question}\n\nSupporting triples:\n{cites}\n\nAnswer:");
    let mut req = LlmRequest::prompt(user).system(sys).max_tokens(300);
    if let Some(m) = model {
        req = req.model(m);
    }
    match llm.complete(&req) {
        Ok(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
        _ => None,
    }
}

/// Distinct URI values appearing in a SELECT answer (in order).
fn answer_uris(answer: &SparqlAnswer) -> Vec<String> {
    let SparqlAnswer::Select { rows, .. } = answer else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        for cell in r.iter().flatten() {
            if cell.kind == TermKind::Uri && seen.insert(cell.value.clone()) {
                out.push(cell.value.clone());
            }
        }
    }
    out
}

fn pos(vars: &[String], v: &str) -> Option<usize> {
    vars.iter().position(|x| x == v)
}

fn cell(row: &[Option<crate::kb::RdfTerm>], idx: Option<usize>) -> Option<String> {
    let i = idx?;
    row.get(i)?.as_ref().map(|t| t.to_term_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::{MockKb, RdfTerm};
    use crate::llm::MockLlm;

    fn uterm(v: &str) -> RdfTerm {
        RdfTerm { kind: TermKind::Uri, value: v.into(), datatype: None, lang: None }
    }
    fn lterm(v: &str) -> RdfTerm {
        RdfTerm { kind: TermKind::Literal, value: v.into(), datatype: None, lang: None }
    }

    #[test]
    fn gathers_citations_for_uri_answers() {
        let answer = SparqlAnswer::Select { vars: vec!["x".into()], rows: vec![vec![Some(uterm("http://ex/Berlin"))]] };
        let triples = SparqlAnswer::Select {
            vars: vec!["p".into(), "o".into()],
            rows: vec![vec![Some(uterm("http://ex/capitalOf")), Some(uterm("http://ex/Germany"))]],
        };
        let kb = MockKb::new(vec![triples]);
        let c = gather_citations(&kb, &answer, 10, 10, 50).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].subject, "<http://ex/Berlin>");
        assert_eq!(c[0].predicate, "<http://ex/capitalOf>");
        assert_eq!(c[0].object, "<http://ex/Germany>");
    }

    #[test]
    fn literal_answers_have_no_citations() {
        let answer = SparqlAnswer::Select { vars: vec!["n".into()], rows: vec![vec![Some(lterm("3.4M"))]] };
        let kb = MockKb::new(vec![SparqlAnswer::Boolean(true)]);
        assert!(gather_citations(&kb, &answer, 10, 10, 50).unwrap().is_empty());
    }

    #[test]
    fn explain_returns_some_on_success_none_otherwise() {
        let cites = vec![Citation {
            subject: "<http://ex/Berlin>".into(),
            predicate: "<http://ex/capitalOf>".into(),
            object: "<http://ex/Germany>".into(),
        }];
        let good = MockLlm::fixed("Berlin is the capital of Germany.");
        assert_eq!(explain(&good, "capital?", &cites, None).as_deref(), Some("Berlin is the capital of Germany."));
        let bad = MockLlm::new(vec![]); // errors → None
        assert!(explain(&bad, "q", &cites, None).is_none());
        // no citations → None (don't risk ungrounded prose)
        assert!(explain(&good, "q", &[], None).is_none());
    }
}
