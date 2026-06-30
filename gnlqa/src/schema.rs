//! Schema-context assembly. For the entities/predicates a question links to, we
//! pull their gStore neighborhood (out/in predicates, types, predicate
//! domain/range) and render a compact text block that grounds Text-to-SPARQL
//! generation (C8) in the *actual* schema — the modern equivalent of gAnswer's
//! precomputed entity/type/relation "fragments", but queried live (gStore's
//! indexes already materialize them, so no offline fragment build is needed).

use crate::error::{Error, Result};
use crate::kb::{KbClient, SparqlAnswer};

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// Reject any string that can't be a SPARQL IRIREF before interpolating it into
/// a query (matches kb.rs's literal-escaping posture). A URI containing
/// `< > " { } | ^ \` backtick, whitespace, or control chars could otherwise
/// break out of `<...>` and inject query text.
pub(crate) fn checked_iri(uri: &str) -> Result<&str> {
    let bad = uri.is_empty()
        || uri.chars().any(|c| {
            c.is_whitespace()
                || (c as u32) < 0x20
                || matches!(c, '<' | '>' | '"' | '{' | '}' | '|' | '^' | '\\' | '`')
        });
    if bad {
        return Err(Error::Sparql(format!("refusing to interpolate non-IRIREF URI: {uri:?}")));
    }
    Ok(uri)
}

/// Distinct values bound to `var` for `where_body`, plus whether the result hit
/// the `sample` cap (fetched as `sample+1`, trimmed to `sample`).
fn capped(kb: &dyn KbClient, where_body: &str, var: &str, sample: usize) -> Result<(Vec<String>, bool)> {
    let q = format!("SELECT DISTINCT ?{var} WHERE {{ {where_body} }} LIMIT {}", sample + 1);
    let mut vals = kb.query(&q)?.column_values(var);
    let truncated = vals.len() > sample;
    vals.truncate(sample);
    Ok((vals, truncated))
}

/// The neighborhood of one entity.
#[derive(Debug, Clone, PartialEq)]
pub struct EntitySchema {
    pub uri: String,
    pub types: Vec<String>,
    pub out_predicates: Vec<String>,
    pub in_predicates: Vec<String>,
    /// Whether any list was capped at the sample limit (so the LLM knows the
    /// neighborhood is partial and shouldn't infer absence).
    pub truncated: bool,
}

impl EntitySchema {
    /// True if the entity had no neighborhood at all (likely not in the KB).
    pub fn is_empty(&self) -> bool {
        self.types.is_empty() && self.out_predicates.is_empty() && self.in_predicates.is_empty()
    }
}

/// The domain/range of one predicate (sampled).
#[derive(Debug, Clone, PartialEq)]
pub struct PredicateSchema {
    pub uri: String,
    pub domain_types: Vec<String>,
    pub range_types: Vec<String>,
    pub truncated: bool,
}

/// Assembled schema context for a question.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchemaContext {
    pub entities: Vec<EntitySchema>,
    pub predicates: Vec<PredicateSchema>,
}

/// Fetch one entity's neighborhood (out/in predicates and types capped at
/// `sample`). `rdf:type` is dropped from out-predicates (it drives `types`).
pub fn entity_schema(kb: &dyn KbClient, uri: &str, sample: usize) -> Result<EntitySchema> {
    let uri = checked_iri(uri)?;
    let (mut out, t1) = capped(kb, &format!("<{uri}> ?p ?o"), "p", sample)?;
    out.retain(|p| p != RDF_TYPE);
    let (inn, t2) = capped(kb, &format!("?s ?p <{uri}>"), "p", sample)?;
    let (types, t3) = capped(kb, &format!("<{uri}> <{RDF_TYPE}> ?t"), "t", sample)?;
    Ok(EntitySchema {
        uri: uri.to_string(),
        types,
        out_predicates: out,
        in_predicates: inn,
        truncated: t1 || t2 || t3,
    })
}

/// Fetch one predicate's sampled domain/range types.
pub fn predicate_schema(kb: &dyn KbClient, uri: &str, sample: usize) -> Result<PredicateSchema> {
    let uri = checked_iri(uri)?;
    let (domain, t1) = capped(kb, &format!("?s <{uri}> ?o . ?s <{RDF_TYPE}> ?t"), "t", sample)?;
    let (range, t2) = capped(kb, &format!("?s <{uri}> ?o . ?o <{RDF_TYPE}> ?t"), "t", sample)?;
    Ok(PredicateSchema { uri: uri.to_string(), domain_types: domain, range_types: range, truncated: t1 || t2 })
}

/// Does the entity participate in this predicate (as subject or object)? This is
/// the data-driven compatibility check used to prune/disambiguate candidates.
pub fn entity_has_predicate(kb: &dyn KbClient, entity: &str, pred: &str) -> Result<bool> {
    let entity = checked_iri(entity)?;
    let pred = checked_iri(pred)?;
    let q = format!("ASK {{ {{ <{entity}> <{pred}> ?o }} UNION {{ ?s <{pred}> <{entity}> }} }}");
    match kb.query(&q)? {
        SparqlAnswer::Boolean(b) => Ok(b),
        _ => Ok(false),
    }
}

impl SchemaContext {
    /// Gather schema for the given entity + predicate URIs.
    pub fn gather(
        kb: &dyn KbClient,
        entity_uris: &[String],
        predicate_uris: &[String],
        sample: usize,
    ) -> Result<SchemaContext> {
        // Best-effort: one entity/predicate failing or being absent must not
        // kill grounding for the rest.
        let mut entities = Vec::new();
        for u in entity_uris {
            match entity_schema(kb, u, sample) {
                Ok(es) if !es.is_empty() => entities.push(es),
                Ok(_) => {} // not in the KB / no neighborhood — skip
                Err(e) => eprintln!("gnlqa: warning: schema for {u}: {e}"),
            }
        }
        let mut predicates = Vec::new();
        for u in predicate_uris {
            match predicate_schema(kb, u, sample) {
                Ok(ps) => predicates.push(ps),
                Err(e) => eprintln!("gnlqa: warning: schema for {u}: {e}"),
            }
        }
        Ok(SchemaContext { entities, predicates })
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.predicates.is_empty()
    }

    /// Render a compact, LLM-friendly schema block (exact URIs, so the model can
    /// copy them verbatim into SPARQL).
    pub fn render(&self) -> String {
        let mut s = String::new();
        if !self.entities.is_empty() {
            s.push_str("# Entities\n");
            for e in &self.entities {
                s.push_str(&format!(
                    "<{}> types:[{}] out:[{}] in:[{}]{}\n",
                    e.uri,
                    join_uris(&e.types),
                    join_uris(&e.out_predicates),
                    join_uris(&e.in_predicates),
                    if e.truncated { " (partial)" } else { "" },
                ));
            }
        }
        if !self.predicates.is_empty() {
            s.push_str("# Predicates\n");
            for p in &self.predicates {
                s.push_str(&format!(
                    "<{}> domain:[{}] range:[{}]{}\n",
                    p.uri,
                    join_uris(&p.domain_types),
                    join_uris(&p.range_types),
                    if p.truncated { " (partial)" } else { "" },
                ));
            }
        }
        s
    }
}

fn join_uris(uris: &[String]) -> String {
    uris.iter().map(|u| format!("<{u}>")).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::{MockKb, RdfTerm, SparqlAnswer, TermKind};

    fn sel(var: &str, uris: &[&str]) -> SparqlAnswer {
        SparqlAnswer::Select {
            vars: vec![var.to_string()],
            rows: uris
                .iter()
                .map(|u| vec![Some(RdfTerm { kind: TermKind::Uri, value: (*u).into(), datatype: None, lang: None })])
                .collect(),
        }
    }

    #[test]
    fn entity_schema_collects_neighborhood() {
        // order: out preds, in preds, types
        let kb = MockKb::new(vec![
            sel("p", &["http://ex/capitalOf", "http://ex/population"]),
            sel("p", &["http://ex/locatedIn"]),
            sel("t", &["http://ex/City"]),
        ]);
        let es = entity_schema(&kb, "http://ex/Berlin", 30).unwrap();
        assert_eq!(es.out_predicates.len(), 2);
        assert_eq!(es.in_predicates, vec!["http://ex/locatedIn"]);
        assert_eq!(es.types, vec!["http://ex/City"]);
    }

    #[test]
    fn entity_has_predicate_reads_ask() {
        let kb = MockKb::new(vec![SparqlAnswer::Boolean(true)]);
        assert!(entity_has_predicate(&kb, "http://ex/Berlin", "http://ex/capitalOf").unwrap());
        let kb2 = MockKb::new(vec![SparqlAnswer::Boolean(false)]);
        assert!(!entity_has_predicate(&kb2, "http://ex/Berlin", "http://ex/nope").unwrap());
    }

    #[test]
    fn gather_and_render() {
        // one entity (out,in,types) then one predicate (domain,range)
        let kb = MockKb::new(vec![
            sel("p", &["http://ex/capitalOf"]),
            sel("p", &[]),
            sel("t", &["http://ex/City"]),
            sel("t", &["http://ex/City"]),   // predicate domain
            sel("t", &["http://ex/Country"]), // predicate range
        ]);
        let ctx = SchemaContext::gather(
            &kb,
            &["http://ex/Berlin".to_string()],
            &["http://ex/capitalOf".to_string()],
            30,
        )
        .unwrap();
        let r = ctx.render();
        assert!(r.contains("<http://ex/Berlin> types:[<http://ex/City>]"));
        assert!(r.contains("<http://ex/capitalOf> domain:[<http://ex/City>] range:[<http://ex/Country>]"));
    }

    #[test]
    fn empty_context() {
        assert!(SchemaContext::default().is_empty());
        assert_eq!(SchemaContext::default().render(), "");
    }

    #[test]
    fn rejects_unsafe_uri() {
        let kb = MockKb::new(vec![SparqlAnswer::Boolean(true)]);
        assert!(entity_has_predicate(&kb, "http://ex/a> } INJECT", "http://ex/p").is_err());
        assert!(entity_schema(&kb, "has space", 5).is_err());
    }

    #[test]
    fn ask_query_has_both_directions_and_is_ask() {
        let kb = MockKb::new(vec![SparqlAnswer::Boolean(false)]);
        entity_has_predicate(&kb, "http://ex/E", "http://ex/P").unwrap();
        let q = kb.last_query().unwrap();
        assert!(q.starts_with("ASK"));
        assert!(q.contains("<http://ex/E> <http://ex/P>"));
        assert!(q.contains("<http://ex/P> <http://ex/E>"));
    }

    #[test]
    fn truncation_marker_when_capped() {
        // sample=1, but the out query (fetched as LIMIT 2) returns 2 → truncated
        let kb = MockKb::new(vec![
            sel("p", &["http://ex/a", "http://ex/b"]),
            sel("p", &[]),
            sel("t", &["http://ex/T"]),
        ]);
        let es = entity_schema(&kb, "http://ex/E", 1).unwrap();
        assert!(es.truncated);
        assert_eq!(es.out_predicates.len(), 1); // trimmed back to sample
        let ctx = SchemaContext { entities: vec![es], predicates: vec![] };
        assert!(ctx.render().contains("(partial)"));
    }
}
