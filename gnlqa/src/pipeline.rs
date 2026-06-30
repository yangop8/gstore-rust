//! The question-answering pipeline.
//!
//! C3 is the minimal **direct passthrough**: the LLM translates the question to a
//! single SPARQL query, gNLQA validates it with gStore's parser, executes it, and
//! renders an answer. Later commits add schema/entity linking, multi-candidate
//! generation, self-repair, grounding, etc.

use crate::error::Result;
use crate::kb::{self, KbClient, SparqlAnswer};
use crate::llm::{LlmClient, LlmRequest};

/// System prompt for the direct Text-to-SPARQL step.
const SYS_TEXT_TO_SPARQL: &str = "\
You translate a natural-language question into a SINGLE valid SPARQL 1.1 query \
for an RDF graph. Output ONLY the SPARQL query — no explanation, no markdown \
prose. Use SELECT for entity/list/factoid questions and ASK for yes/no \
questions.";

/// A produced answer.
#[derive(Debug, Clone)]
pub struct Answer {
    /// Human-readable answer text.
    pub text: String,
    /// The (validated) SPARQL that produced it, for transparency.
    pub sparql: Option<String>,
    /// The raw answer values (e.g. bound URIs/literals of the first column).
    pub values: Vec<String>,
}

/// The QA engine: an LLM front-end + a KB backend.
pub struct AskEngine {
    llm: Box<dyn LlmClient>,
    kb: Box<dyn KbClient>,
    model: Option<String>,
}

impl AskEngine {
    pub fn new(llm: Box<dyn LlmClient>, kb: Box<dyn KbClient>) -> AskEngine {
        AskEngine { llm, kb, model: None }
    }

    /// Pin a specific model for generation.
    pub fn with_model(mut self, model: impl Into<String>) -> AskEngine {
        self.model = Some(model.into());
        self
    }

    /// Answer a question via direct Text-to-SPARQL (C3).
    pub fn ask(&self, question: &str) -> Result<Answer> {
        let mut req = LlmRequest::prompt(question.to_string()).system(SYS_TEXT_TO_SPARQL);
        if let Some(m) = &self.model {
            req = req.model(m.clone());
        }
        let raw = self.llm.complete(&req)?;
        let sparql = extract_sparql(&raw);
        // Validate with gStore's own parser before executing.
        kb::validate_sparql(&sparql)?;
        let answer = self.kb.query(&sparql)?;
        Ok(Answer {
            text: render_answer(&answer),
            values: answer_values(&answer),
            sparql: Some(sparql),
        })
    }
}

/// Strip Markdown code fences / surrounding prose the LLM may have added,
/// leaving the bare SPARQL query.
pub fn extract_sparql(raw: &str) -> String {
    let s = raw.trim();
    // ```sparql ... ```  or  ``` ... ```
    if let Some(rest) = s.strip_prefix("```") {
        // Drop an optional language tag on the first line.
        let body = match rest.split_once('\n') {
            Some((_lang, after)) => after,
            None => rest,
        };
        let body = body.strip_suffix("```").unwrap_or(body);
        let body = body.trim_end().trim_end_matches("```");
        return body.trim().to_string();
    }
    s.to_string()
}

/// First-column answer values (URIs/literals) for a SELECT; the boolean for ASK.
fn answer_values(ans: &SparqlAnswer) -> Vec<String> {
    match ans {
        SparqlAnswer::Select { vars, rows } => {
            if vars.is_empty() {
                return Vec::new();
            }
            rows.iter()
                .filter_map(|r| r.first().and_then(|c| c.as_ref()).map(|t| t.value.clone()))
                .collect()
        }
        SparqlAnswer::Boolean(b) => vec![b.to_string()],
        SparqlAnswer::Graph(_) => Vec::new(),
    }
}

/// Render a human-readable answer string.
fn render_answer(ans: &SparqlAnswer) -> String {
    match ans {
        SparqlAnswer::Boolean(b) => if *b { "Yes" } else { "No" }.to_string(),
        SparqlAnswer::Graph(g) => g.clone(),
        SparqlAnswer::Select { .. } => {
            let vals = answer_values(ans);
            if vals.is_empty() {
                "(no results)".to_string()
            } else {
                vals.join(", ")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::{MockKb, RdfTerm, TermKind};
    use crate::llm::MockLlm;

    fn term(v: &str) -> RdfTerm {
        RdfTerm { kind: TermKind::Uri, value: v.into(), datatype: None, lang: None }
    }

    #[test]
    fn extract_sparql_strips_fences() {
        assert_eq!(extract_sparql("SELECT ?s WHERE { ?s ?p ?o }"), "SELECT ?s WHERE { ?s ?p ?o }");
        assert_eq!(
            extract_sparql("```sparql\nSELECT ?s WHERE { ?s ?p ?o }\n```"),
            "SELECT ?s WHERE { ?s ?p ?o }"
        );
        assert_eq!(extract_sparql("```\nASK { ?s ?p ?o }\n```"), "ASK { ?s ?p ?o }");
    }

    #[test]
    fn direct_passthrough_select() {
        let llm = MockLlm::fixed("```sparql\nSELECT ?x WHERE { ?x <http://ex/p> <http://ex/o> }\n```");
        let answer = SparqlAnswer::Select {
            vars: vec!["x".into()],
            rows: vec![vec![Some(term("http://ex/a"))], vec![Some(term("http://ex/b"))]],
        };
        let kb = MockKb::new(vec![answer]);
        let engine = AskEngine::new(Box::new(llm), Box::new(kb));
        let a = engine.ask("which x relate to o?").unwrap();
        assert_eq!(a.values, vec!["http://ex/a", "http://ex/b"]);
        assert_eq!(a.text, "http://ex/a, http://ex/b");
        assert!(a.sparql.unwrap().starts_with("SELECT ?x"));
    }

    #[test]
    fn direct_passthrough_ask() {
        let llm = MockLlm::fixed("ASK { <http://ex/a> <http://ex/p> <http://ex/b> }");
        let kb = MockKb::new(vec![SparqlAnswer::Boolean(true)]);
        let engine = AskEngine::new(Box::new(llm), Box::new(kb));
        let a = engine.ask("does a relate to b?").unwrap();
        assert_eq!(a.text, "Yes");
    }

    #[test]
    fn invalid_sparql_from_llm_is_rejected_before_execution() {
        let llm = MockLlm::fixed("this is not sparql");
        let kb = MockKb::new(vec![SparqlAnswer::Boolean(true)]);
        let engine = AskEngine::new(Box::new(llm), Box::new(kb));
        let err = engine.ask("q").unwrap_err();
        assert!(matches!(err, crate::error::Error::Sparql(_)));
        // KB must NOT have been queried with invalid SPARQL.
        // (can't read MockKb here since it moved; the Sparql error proves the guard)
    }
}
