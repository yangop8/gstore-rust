//! The question-answering pipeline.
//!
//! C3 is the minimal **direct passthrough**: the LLM translates the question to a
//! single SPARQL query, gNLQA validates it with gStore's parser, executes it, and
//! renders an answer. Later commits add schema/entity linking, multi-candidate
//! generation, self-repair, grounding, etc.

use std::collections::HashSet;

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
    /// How many self-repair rounds were spent (0 for the direct path).
    pub rounds: usize,
    /// Supporting triples grounding the answer (empty if grounding is off).
    pub citations: Vec<crate::ground::Citation>,
    /// Optional LLM-phrased natural-language explanation.
    pub explanation: Option<String>,
    /// Confidence in [0,1] (non-empty result, fewer repairs, grounded → higher).
    pub confidence: f32,
    /// Whether the system abstained (confidence below the configured threshold).
    pub abstained: bool,
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
        let values = answer_values(&answer);
        let confidence = if crate::repair::is_empty_answer(&answer) { 0.2 } else { 1.0 };
        Ok(Answer {
            text: render_answer(&answer, &values),
            values,
            sparql: Some(sparql),
            rounds: 0,
            citations: Vec::new(),
            explanation: None,
            confidence,
            abstained: false,
        })
    }
}

/// Strip Markdown code fences / surrounding prose the LLM may have added,
/// leaving the bare SPARQL query.
pub fn extract_sparql(raw: &str) -> String {
    let s = raw.trim();
    // Take whatever is between the first ``` and the next ``` — robust to leading
    // prose ("Here is the query:"), trailing prose, and multiple blocks (first
    // wins). "```" is ASCII, so `+ 3` byte indexing is safe.
    if let Some(open) = s.find("```") {
        let after_open = &s[open + 3..];
        // Drop an optional language tag on the fence's first line.
        let after_tag = match after_open.split_once('\n') {
            Some((_lang, rest)) => rest,
            None => after_open,
        };
        let body = match after_tag.find("```") {
            Some(close) => &after_tag[..close],
            None => after_tag,
        };
        return body.trim().to_string();
    }
    s.to_string()
}

/// Answer values for a SELECT — the first *bound* cell of each row (so a row
/// whose leading variable is unbound still contributes), de-duplicated in order;
/// the boolean string for ASK. (Multi-variable rendering is a later phase.)
pub(crate) fn answer_values(ans: &SparqlAnswer) -> Vec<String> {
    match ans {
        SparqlAnswer::Select { rows, .. } => {
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for r in rows {
                if let Some(t) = r.iter().flatten().next() {
                    if seen.insert(t.value.clone()) {
                        out.push(t.value.clone());
                    }
                }
            }
            out
        }
        SparqlAnswer::Boolean(b) => vec![b.to_string()],
        SparqlAnswer::Graph(_) => Vec::new(),
    }
}

/// Render a human-readable answer string from the answer and its precomputed
/// values (avoids recomputing them).
pub(crate) fn render_answer(ans: &SparqlAnswer, values: &[String]) -> String {
    match ans {
        SparqlAnswer::Boolean(b) => if *b { "Yes" } else { "No" }.to_string(),
        SparqlAnswer::Graph(g) => g.clone(),
        SparqlAnswer::Select { .. } => {
            if values.is_empty() {
                "(no results)".to_string()
            } else {
                values.join(", ")
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
    fn extract_sparql_survives_prose_and_multiblock() {
        // leading prose before the fence
        assert_eq!(
            extract_sparql("Here is the query:\n```sparql\nSELECT ?s WHERE { ?s ?p ?o }\n```"),
            "SELECT ?s WHERE { ?s ?p ?o }"
        );
        // trailing prose after the closing fence
        assert_eq!(
            extract_sparql("```sparql\nSELECT ?s WHERE { ?s ?p ?o }\n```\nThis lists subjects."),
            "SELECT ?s WHERE { ?s ?p ?o }"
        );
        // multiple blocks — first wins
        assert_eq!(extract_sparql("```\nASK { ?s ?p ?o }\n```\n```\nSELECT ?x {}\n```"), "ASK { ?s ?p ?o }");
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
        use std::sync::Arc;
        let llm = MockLlm::fixed("this is not sparql");
        let kb = Arc::new(MockKb::new(vec![SparqlAnswer::Boolean(true)]));
        let engine = AskEngine::new(Box::new(llm), Box::new(Arc::clone(&kb)));
        let err = engine.ask("q").unwrap_err();
        assert!(matches!(err, crate::error::Error::Sparql(_)));
        // Directly verify the invalid query never reached the KB.
        assert_eq!(kb.query_count(), 0, "KB must not be queried with invalid SPARQL");
    }
}
