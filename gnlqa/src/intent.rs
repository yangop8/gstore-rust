//! Question understanding: a structured [`QuestionIntent`] extracted from the
//! natural-language question by the LLM (one call, JSON output). This is the
//! input to schema/entity linking (C5–C7) and query generation (C8).

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::llm::{LlmClient, LlmRequest};

/// The kind of question, used to route generation (SPARQL vs analytics vs RAG).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QType {
    /// A single fact ("who directed X?").
    #[default]
    Factoid,
    /// A list of entities.
    List,
    /// A count ("how many …").
    Count,
    /// Yes/no.
    Boolean,
    /// Comparison / superlative ("the tallest …").
    Compare,
    /// Path / connection ("how is X related to Y?").
    Path,
    /// Graph-analytics ("shortest path", "most central").
    Analytics,
    /// Open / not expressible as a single SPARQL query (RAG fallback).
    /// Also the catch-all for any unrecognized type the model emits.
    #[serde(other)]
    Open,
}

/// What a mention refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MentionKind {
    /// A class/type ("cities", "people").
    Type,
    /// A literal value (number, string, date).
    Literal,
    /// A named entity (default; also the catch-all for unknown kinds — must be
    /// last for `#[serde(other)]`).
    #[default]
    #[serde(other)]
    Entity,
}

/// A surface mention to be linked to the KG.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mention {
    pub text: String,
    #[serde(default)]
    pub kind: MentionKind,
}

/// A relation phrase connecting two arguments (mention texts or `?var`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationPhrase {
    pub arg1: String,
    pub arg2: String,
    pub phrase: String,
}

/// An aggregation operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggOp {
    Count,
    Max,
    Min,
    Sum,
    Avg,
    /// No aggregation (default; catch-all — must be last for `#[serde(other)]`).
    #[default]
    #[serde(other)]
    None,
}

/// Aggregation / ordering directives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Aggregation {
    #[serde(default)]
    pub op: AggOp,
    #[serde(default)]
    pub by: Option<String>,
    #[serde(default)]
    pub order: Option<String>, // "asc" | "desc"
    #[serde(default)]
    pub limit: Option<u32>,
}

/// The structured understanding of a question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct QuestionIntent {
    #[serde(default)]
    pub lang: String,
    #[serde(default)]
    pub qtype: QType,
    #[serde(default)]
    pub mentions: Vec<Mention>,
    #[serde(default)]
    pub relations: Vec<RelationPhrase>,
    #[serde(default)]
    pub aggregation: Option<Aggregation>,
    /// The thing being asked for (a mention text or a free description).
    #[serde(default)]
    pub target: Option<String>,
}

/// System prompt instructing the model to return a strict JSON intent.
const SYS_INTENT: &str = "\
You analyze a natural-language question over a knowledge graph and output ONLY a \
single JSON object (no prose, no markdown) with this shape:\n\
{\"lang\":\"<ISO code>\", \"qtype\":\"factoid|list|count|boolean|compare|path|analytics|open\", \
\"mentions\":[{\"text\":\"...\",\"kind\":\"entity|type|literal\"}], \
\"relations\":[{\"arg1\":\"...\",\"arg2\":\"...\",\"phrase\":\"...\"}], \
\"aggregation\":{\"op\":\"none|count|max|min|sum|avg\",\"by\":null,\"order\":null,\"limit\":null}, \
\"target\":\"what is being asked for\"}\n\
Identify entity/type/literal mentions and the relation phrases between them. \
Use \"analytics\" for shortest-path/centrality/community questions, \"open\" if \
it cannot be answered by a structured graph query.";

/// Extract a [`QuestionIntent`] from a question using the LLM.
pub fn extract_intent(llm: &dyn LlmClient, question: &str) -> Result<QuestionIntent> {
    let req = LlmRequest::prompt(question.to_string()).system(SYS_INTENT);
    let raw = llm.complete(&req)?;
    parse_intent(&raw)
}

/// Parse the model's (possibly fence-wrapped) JSON into a [`QuestionIntent`].
pub fn parse_intent(raw: &str) -> Result<QuestionIntent> {
    let json = extract_json(raw);
    serde_json::from_str(&json)
        .map_err(|e| Error::Llm(format!("could not parse intent JSON: {e}; raw was: {json}")))
}

/// Pull the first JSON object out of an LLM response (handles code fences and
/// surrounding prose) by slicing from the first `{` to the matching last `}`.
pub fn extract_json(raw: &str) -> String {
    let s = raw.trim();
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b > a => s[a..=b].to_string(),
        _ => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::MockLlm;

    #[test]
    fn parse_full_intent() {
        let json = r#"{
            "lang":"en","qtype":"list",
            "mentions":[{"text":"Germany","kind":"entity"},{"text":"cities","kind":"type"}],
            "relations":[{"arg1":"cities","arg2":"Germany","phrase":"in"}],
            "aggregation":{"op":"count","by":null,"order":"desc","limit":10},
            "target":"cities"
        }"#;
        let it = parse_intent(json).unwrap();
        assert_eq!(it.qtype, QType::List);
        assert_eq!(it.mentions.len(), 2);
        assert_eq!(it.mentions[1].kind, MentionKind::Type);
        assert_eq!(it.relations[0].phrase, "in");
        assert_eq!(it.aggregation.unwrap().op, AggOp::Count);
    }

    #[test]
    fn extract_json_handles_fences_and_prose() {
        assert_eq!(extract_json("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(extract_json("Here you go: {\"a\":1} done"), "{\"a\":1}");
    }

    #[test]
    fn unknown_enum_values_fall_back() {
        let it = parse_intent(r#"{"qtype":"weirdtype","mentions":[{"text":"X","kind":"galaxy"}]}"#).unwrap();
        assert_eq!(it.qtype, QType::Open); // serde(other)
        assert_eq!(it.mentions[0].kind, MentionKind::Entity); // serde(other)
    }

    #[test]
    fn missing_fields_default() {
        let it = parse_intent(r#"{"qtype":"boolean"}"#).unwrap();
        assert_eq!(it.qtype, QType::Boolean);
        assert!(it.mentions.is_empty());
        assert!(it.aggregation.is_none());
    }

    #[test]
    fn extract_intent_via_mock_llm() {
        let llm = MockLlm::fixed(r#"{"qtype":"factoid","target":"director"}"#);
        let it = extract_intent(&llm, "who directed it?").unwrap();
        assert_eq!(it.qtype, QType::Factoid);
        assert_eq!(it.target.as_deref(), Some("director"));
    }

    #[test]
    fn garbage_intent_errors() {
        assert!(parse_intent("not json at all").is_err());
    }
}
