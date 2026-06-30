//! Answer grounding: attach the supporting KG triples to an answer (so it can be
//! cited / audited, controlling hallucination), and optionally have the LLM
//! phrase a natural-language explanation from them.

use crate::error::Result;
use crate::kb::{KbClient, SparqlAnswer, TermKind};
use crate::llm::{LlmClient, LlmRequest};
use crate::schema::checked_iri;

/// One supporting triple (string surface forms).
#[derive(Debug, Clone, PartialEq)]
pub struct Citation {
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

/// Collect a few supporting triples for the URI entities appearing in `answer`
/// (`<uri> ?p ?o`), capped per-entity and overall, so an answer can be cited.
pub fn gather_citations(
    kb: &dyn KbClient,
    answer: &SparqlAnswer,
    per_entity: usize,
    max_total: usize,
) -> Result<Vec<Citation>> {
    let mut out = Vec::new();
    for uri in answer_uris(answer) {
        if out.len() >= max_total {
            break;
        }
        let Ok(uri) = checked_iri(&uri) else { continue };
        let q = format!("SELECT ?p ?o WHERE {{ <{uri}> ?p ?o }} LIMIT {per_entity}");
        if let SparqlAnswer::Select { vars, rows } = kb.query(&q)? {
            let (pi, oi) = (pos(&vars, "p"), pos(&vars, "o"));
            for r in rows {
                let (Some(p), Some(o)) = (cell(&r, pi), cell(&r, oi)) else { continue };
                out.push(Citation { subject: uri.to_string(), predicate: p, object: o });
                if out.len() >= max_total {
                    break;
                }
            }
        }
    }
    Ok(out)
}

/// Ask the LLM to phrase a grounded natural-language answer from the question,
/// the raw values, and the citations. Falls back to the raw text on LLM failure.
pub fn explain(
    llm: &dyn LlmClient,
    question: &str,
    raw_answer: &str,
    citations: &[Citation],
    fallback: &str,
    model: Option<&str>,
) -> String {
    let cites = citations
        .iter()
        .map(|c| format!("<{}> <{}> {}", c.subject, c.predicate, c.object))
        .collect::<Vec<_>>()
        .join("\n");
    let sys = "You write a concise, factual natural-language answer to the user's \
        question using ONLY the supporting triples. Do not add facts not present \
        in them. One or two sentences.";
    let user = format!(
        "Question: {question}\n\nRaw result: {raw_answer}\n\nSupporting triples:\n{cites}\n\nAnswer:"
    );
    let mut req = LlmRequest::prompt(user).system(sys).max_tokens(300);
    if let Some(m) = model {
        req = req.model(m);
    }
    match llm.complete(&req) {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => fallback.to_string(),
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
        let c = gather_citations(&kb, &answer, 10, 50).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].subject, "http://ex/Berlin");
        assert_eq!(c[0].predicate, "<http://ex/capitalOf>");
        assert_eq!(c[0].object, "<http://ex/Germany>");
    }

    #[test]
    fn literal_answers_have_no_citations() {
        let answer = SparqlAnswer::Select { vars: vec!["n".into()], rows: vec![vec![Some(lterm("3.4M"))]] };
        let kb = MockKb::new(vec![SparqlAnswer::Boolean(true)]);
        assert!(gather_citations(&kb, &answer, 10, 50).unwrap().is_empty());
    }

    #[test]
    fn explain_uses_llm_else_fallback() {
        let answer = SparqlAnswer::Select { vars: vec!["x".into()], rows: vec![vec![Some(uterm("http://ex/Berlin"))]] };
        let kb = MockKb::new(vec![SparqlAnswer::Select { vars: vec!["p".into(), "o".into()], rows: vec![] }]);
        let cites = gather_citations(&kb, &answer, 5, 5).unwrap();
        let good = MockLlm::fixed("Berlin is the capital of Germany.");
        assert_eq!(explain(&good, "capital?", "http://ex/Berlin", &cites, "fallback", None), "Berlin is the capital of Germany.");
        let bad = MockLlm::new(vec![]); // errors → fallback
        assert_eq!(explain(&bad, "q", "raw", &cites, "fallback", None), "fallback");
    }
}
