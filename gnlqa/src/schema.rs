//! Schema-context assembly. For the entities/predicates a question links to, we
//! pull their gStore neighborhood (out/in predicates, types, predicate
//! domain/range) and render a compact text block that grounds Text-to-SPARQL
//! generation (C8) in the *actual* schema — the modern equivalent of gAnswer's
//! precomputed entity/type/relation "fragments", but queried live (gStore's
//! indexes already materialize them, so no offline fragment build is needed).

use crate::error::Result;
use crate::kb::{KbClient, SparqlAnswer};

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The neighborhood of one entity.
#[derive(Debug, Clone, PartialEq)]
pub struct EntitySchema {
    pub uri: String,
    pub types: Vec<String>,
    pub out_predicates: Vec<String>,
    pub in_predicates: Vec<String>,
}

/// The domain/range of one predicate (sampled).
#[derive(Debug, Clone, PartialEq)]
pub struct PredicateSchema {
    pub uri: String,
    pub domain_types: Vec<String>,
    pub range_types: Vec<String>,
}

/// Assembled schema context for a question.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchemaContext {
    pub entities: Vec<EntitySchema>,
    pub predicates: Vec<PredicateSchema>,
}

/// Fetch one entity's neighborhood (out/in predicates capped at `sample`).
pub fn entity_schema(kb: &dyn KbClient, uri: &str, sample: usize) -> Result<EntitySchema> {
    let out = kb
        .query(&format!("SELECT DISTINCT ?p WHERE {{ <{uri}> ?p ?o }} LIMIT {sample}"))?
        .column_values("p");
    let inn = kb
        .query(&format!("SELECT DISTINCT ?p WHERE {{ ?s ?p <{uri}> }} LIMIT {sample}"))?
        .column_values("p");
    let types = kb
        .query(&format!("SELECT DISTINCT ?t WHERE {{ <{uri}> <{RDF_TYPE}> ?t }}"))?
        .column_values("t");
    Ok(EntitySchema { uri: uri.to_string(), types, out_predicates: out, in_predicates: inn })
}

/// Fetch one predicate's sampled domain/range types.
pub fn predicate_schema(kb: &dyn KbClient, uri: &str, sample: usize) -> Result<PredicateSchema> {
    let domain = kb
        .query(&format!(
            "SELECT DISTINCT ?t WHERE {{ ?s <{uri}> ?o . ?s <{RDF_TYPE}> ?t }} LIMIT {sample}"
        ))?
        .column_values("t");
    let range = kb
        .query(&format!(
            "SELECT DISTINCT ?t WHERE {{ ?s <{uri}> ?o . ?o <{RDF_TYPE}> ?t }} LIMIT {sample}"
        ))?
        .column_values("t");
    Ok(PredicateSchema { uri: uri.to_string(), domain_types: domain, range_types: range })
}

/// Does the entity participate in this predicate (as subject or object)? This is
/// the data-driven compatibility check used to prune/disambiguate candidates.
pub fn entity_has_predicate(kb: &dyn KbClient, entity: &str, pred: &str) -> Result<bool> {
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
        let mut entities = Vec::new();
        for u in entity_uris {
            entities.push(entity_schema(kb, u, sample)?);
        }
        let mut predicates = Vec::new();
        for u in predicate_uris {
            predicates.push(predicate_schema(kb, u, sample)?);
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
                    "<{}> types:[{}] out:[{}] in:[{}]\n",
                    e.uri,
                    join_uris(&e.types),
                    join_uris(&e.out_predicates),
                    join_uris(&e.in_predicates),
                ));
            }
        }
        if !self.predicates.is_empty() {
            s.push_str("# Predicates\n");
            for p in &self.predicates {
                s.push_str(&format!(
                    "<{}> domain:[{}] range:[{}]\n",
                    p.uri,
                    join_uris(&p.domain_types),
                    join_uris(&p.range_types),
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
}
