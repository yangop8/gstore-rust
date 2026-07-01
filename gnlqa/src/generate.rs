//! Schema-grounded Text-to-SPARQL generation. The LLM is given the question, the
//! linked term URIs, and the gStore schema context (C6/C7), and asked for N
//! candidate SPARQL queries. Each candidate is validated with gStore's parser
//! (C2); only syntactically valid, de-duplicated candidates are returned, ready
//! for multi-candidate execution + disambiguation (C10) and self-repair (C9).

use std::collections::HashSet;

use crate::error::Result;
use crate::kb;
use crate::llm::{LlmClient, LlmRequest};

const SYS_GENERATE: &str = "\
You translate a natural-language question into SPARQL 1.1 for an RDF graph. You \
are given the linked term URIs and the relevant schema. Rules:\n\
- Use ONLY the URIs provided (copy them verbatim inside <…>); do not invent IRIs \
or prefixes.\n\
- Prefer the directions/types shown in the schema.\n\
- Output ONLY a JSON array of distinct candidate query strings, most-likely \
first, e.g. [\"SELECT ?x WHERE { … }\", \"ASK { … }\"]. No prose, no markdown.";

/// Build the user prompt for generation.
pub fn build_prompt(question: &str, links_block: &str, schema_block: &str, n: usize) -> String {
    format!(
        "Question: {question}\n\n\
         Linked terms:\n{links}\n\n\
         Schema:\n{schema}\n\n\
         Produce up to {n} candidate SPARQL 1.1 queries as a JSON array of strings.",
        links = if links_block.is_empty() { "(none)" } else { links_block },
        schema = if schema_block.is_empty() { "(none)" } else { schema_block },
    )
}

/// Generate up to `n` valid, de-duplicated candidate SPARQL queries.
///
/// Returns an **empty Vec** if the model produced no parser-valid candidate
/// (truncation, malformed JSON, or all-invalid). Callers (self-repair C9 /
/// picker C10) must treat empty as "needs repair/retry", not index `[0]`.
pub fn generate_candidates(
    llm: &dyn LlmClient,
    question: &str,
    links_block: &str,
    schema_block: &str,
    n: usize,
    model: Option<&str>,
) -> Result<Vec<String>> {
    // Grow the token budget with the number of requested candidates so the JSON
    // array doesn't truncate mid-string (which would drop everything).
    let max_tokens = 512 + (n as u32) * 512;
    let mut req = LlmRequest::prompt(build_prompt(question, links_block, schema_block, n))
        .system(SYS_GENERATE)
        .max_tokens(max_tokens);
    if let Some(m) = model {
        req = req.model(m);
    }
    let raw = llm.complete(&req)?;
    Ok(valid_candidates(&parse_candidates(&raw), n))
}

/// Keep only syntactically valid (per gStore's parser), de-duplicated, non-empty
/// candidates, capped at `n`.
pub fn valid_candidates(raw_candidates: &[String], n: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for c in raw_candidates {
        let c = c.trim().to_string();
        // Read-only (reject SPARQL UPDATE), grounded (reference a concrete term,
        // not an all-variable wildcard), and de-duplicated.
        if c.is_empty()
            || kb::validate_readonly_sparql(&c).is_err()
            || !is_grounded(&c)
            || !seen.insert(c.clone())
        {
            continue;
        }
        out.push(c);
        if out.len() >= n {
            break;
        }
    }
    out
}

/// Whether a candidate references at least one concrete term — an IRI (`<...>`)
/// or a literal (`"..."`). A pure all-variable BGP like
/// `SELECT ?x WHERE { ?x ?p ?o }` is ungrounded: it returns arbitrary rows and
/// can outrank a precise (empty) answer, so it is rejected.
fn is_grounded(sparql: &str) -> bool {
    sparql.contains('<') || sparql.contains('"')
}

/// Parse candidate SPARQL strings from the model output: a JSON array of strings
/// if present, else every fenced code block, else the whole text as one.
pub fn parse_candidates(raw: &str) -> Vec<String> {
    // Try each `[`-started balanced array, returning the first that parses as a
    // list of strings (so brackets in prose, e.g. "[a,b]", don't defeat it).
    // Among all balanced `[…]` that parse as a string array, prefer the one with
    // the most elements (a real candidate list beats a small prose array).
    let mut best: Option<Vec<String>> = None;
    for (i, &b) in raw.as_bytes().iter().enumerate() {
        if b == b'[' {
            if let Some(end) = matched_bracket(raw, i) {
                if let Ok(v) = serde_json::from_str::<Vec<String>>(&raw[i..=end]) {
                    if !v.is_empty() && best.as_ref().is_none_or(|cur| v.len() > cur.len()) {
                        best = Some(v);
                    }
                }
            }
        }
    }
    if let Some(v) = best {
        return v;
    }
    let blocks = fenced_blocks(raw);
    if !blocks.is_empty() {
        return blocks;
    }
    vec![raw.trim().to_string()]
}

/// Index of the `]` matching the `[` at `start` (string literals skipped), or
/// `None` if unbalanced.
fn matched_bracket(raw: &str, start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (off, &c) in raw.as_bytes()[start..].iter().enumerate() {
        if in_str {
            match c {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_str = false,
                _ => {}
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'[' => depth += 1,
                b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(start + off);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Every ``` … ``` fenced block body (language tag on the fence line dropped).
fn fenced_blocks(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = raw;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let body_start = match after.split_once('\n') {
            Some((_lang, b)) => b,
            None => after,
        };
        match body_start.find("```") {
            Some(close) => {
                let body = body_start[..close].trim();
                if !body.is_empty() {
                    out.push(body.to_string());
                }
                rest = &body_start[close + 3..];
            }
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::MockLlm;

    #[test]
    fn parse_json_array_of_candidates() {
        let raw = r#"[ "SELECT ?x WHERE { ?x ?p ?o }", "ASK { ?s ?p ?o }" ]"#;
        let c = parse_candidates(raw);
        assert_eq!(c.len(), 2);
        assert!(c[0].starts_with("SELECT"));
    }

    #[test]
    fn parse_json_array_with_prose_and_brackets() {
        // brackets in prose before the array must not confuse it
        let raw = "Consider [a,b]. Here: [\"SELECT ?x WHERE { ?x ?p ?o }\"] done";
        let c = parse_candidates(raw);
        assert_eq!(c.len(), 1);
        assert!(c[0].starts_with("SELECT"));
    }

    #[test]
    fn prefers_largest_parseable_array() {
        // a small valid prose array before the real (larger) candidate array
        let raw = "Options: [\"a\"]. Answer: [\"SELECT ?x WHERE { ?x ?p ?o }\", \"ASK { ?s ?p ?o }\"]";
        let c = parse_candidates(raw);
        assert_eq!(c.len(), 2);
        assert!(c[0].starts_with("SELECT"));
    }

    #[test]
    fn parse_fenced_blocks_fallback() {
        let raw = "```sparql\nSELECT ?a WHERE { ?a ?p ?o }\n```\n```sparql\nASK { ?s ?p ?o }\n```";
        let c = parse_candidates(raw);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn parse_single_unfenced_fallback() {
        let c = parse_candidates("SELECT ?x WHERE { ?x ?p ?o }");
        assert_eq!(c, vec!["SELECT ?x WHERE { ?x ?p ?o }"]);
    }

    #[test]
    fn valid_candidates_filters_and_dedups() {
        let cands = vec![
            "SELECT ?x WHERE { ?x <http://ex/p> ?o }".to_string(),
            "not sparql".to_string(),                              // invalid → dropped
            "SELECT ?x WHERE { ?x <http://ex/p> ?o }".to_string(), // dup → dropped
            "ASK { ?s <http://ex/p> \"v\" }".to_string(),
        ];
        let v = valid_candidates(&cands, 10);
        assert_eq!(v.len(), 2);
        assert!(v[0].starts_with("SELECT") && v[1].starts_with("ASK"));
    }

    #[test]
    fn valid_candidates_rejects_update_and_ungrounded() {
        let cands = vec![
            // read-only enforcement: a mutating UPDATE must be rejected
            "DELETE WHERE { ?s ?p ?o }".to_string(),
            "DROP DEFAULT".to_string(),
            "INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> }".to_string(),
            // ungrounded all-variable wildcard must be rejected
            "SELECT ?x WHERE { ?x ?p ?o }".to_string(),
            // a grounded read-only query survives
            "SELECT ?x WHERE { ?x <http://ex/p> <http://ex/o> }".to_string(),
        ];
        let v = valid_candidates(&cands, 10);
        assert_eq!(v, vec!["SELECT ?x WHERE { ?x <http://ex/p> <http://ex/o> }".to_string()]);
    }

    #[test]
    fn generate_candidates_end_to_end_with_mock() {
        let llm = MockLlm::fixed(
            r#"["SELECT ?c WHERE { ?c <http://ex/capitalOf> <http://ex/Germany> }", "bad query"]"#,
        );
        let v = generate_candidates(&llm, "capital of Germany?", "<http://ex/Germany>", "", 3, None).unwrap();
        assert_eq!(v.len(), 1); // the invalid one is dropped
        assert!(v[0].contains("capitalOf"));
    }

    #[test]
    fn build_prompt_includes_parts() {
        let p = build_prompt("q?", "links", "schema", 4);
        assert!(p.contains("q?") && p.contains("links") && p.contains("schema") && p.contains("4"));
    }
}
