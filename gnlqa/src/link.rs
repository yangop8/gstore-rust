//! Entity / predicate / type linking — map a natural-language mention to KG URIs
//! by semantic similarity over label vector indexes (built from gStore), the
//! modern replacement for gAnswer's Lucene + edit-distance + paraphrase
//! dictionary. Candidates are later validated against gStore's neighborhood
//! (C7) and used to ground SPARQL generation (C8).

use crate::embed::{Embedder, VectorIndex};
use crate::error::Result;
use crate::intent::{Mention, MentionKind};
use crate::kb::{KbClient, SparqlAnswer};

const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// What a linked candidate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Entity,
    Type,
    Predicate,
    /// A literal value carried through unchanged (not linked to a URI).
    Literal,
}

/// A candidate KG term for a mention, with a similarity score.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The KG URI (empty for a literal).
    pub uri: String,
    /// The label/local-name that matched.
    pub label: String,
    pub kind: LinkKind,
    pub score: f32,
}

impl Candidate {
    /// SPARQL surface form: `<uri>` for a URI term, a quoted literal otherwise.
    pub fn to_term(&self) -> String {
        if self.kind == LinkKind::Literal || self.uri.is_empty() {
            format!("\"{}\"", self.label)
        } else {
            format!("<{}>", self.uri)
        }
    }
}

/// The local name of a URI — the part after the last `#` or `/`.
pub fn local_name(uri: &str) -> String {
    let cut = uri.rfind(['#', '/']).map(|i| i + 1).unwrap_or(0);
    let name = &uri[cut..];
    if name.is_empty() {
        uri.to_string()
    } else {
        name.to_string()
    }
}

/// Linker holding one vector index per term kind.
pub struct Linker {
    embedder: Box<dyn Embedder>,
    entities: VectorIndex,
    predicates: VectorIndex,
    types: VectorIndex,
    /// Minimum cosine score for a candidate to be kept.
    min_score: f32,
}

impl Linker {
    /// Build from explicit `(uri, label)` lists (offline / testable).
    pub fn from_labels(
        embedder: Box<dyn Embedder>,
        entities: &[(String, String)],
        predicates: &[(String, String)],
        types: &[(String, String)],
        min_score: f32,
    ) -> Result<Linker> {
        let ent = VectorIndex::build(embedder.as_ref(), entities)?;
        let pred = VectorIndex::build(embedder.as_ref(), predicates)?;
        let typ = VectorIndex::build(embedder.as_ref(), types)?;
        Ok(Linker { embedder, entities: ent, predicates: pred, types: typ, min_score })
    }

    /// Build the indexes by querying gStore for entity labels (`rdfs:label`),
    /// distinct predicates, and distinct types. Predicates/types are labeled by
    /// their URI local name (data rarely carries `rdfs:label` for them).
    pub fn build_from_kb(
        kb: &dyn KbClient,
        embedder: Box<dyn Embedder>,
        min_score: f32,
    ) -> Result<Linker> {
        let ent_q = format!("SELECT ?s ?l WHERE {{ ?s <{RDFS_LABEL}> ?l }}");
        let entities = pairs_with_label(&kb.query(&ent_q)?, "s", "l");

        let pred_q = "SELECT DISTINCT ?p WHERE { ?s ?p ?o }".to_string();
        let predicates = uris_by_local_name(&kb.query(&pred_q)?, "p");

        let type_q = format!("SELECT DISTINCT ?t WHERE {{ ?s <{RDF_TYPE}> ?t }}");
        let types = uris_by_local_name(&kb.query(&type_q)?, "t");

        Linker::from_labels(embedder, &entities, &predicates, &types, min_score)
    }

    /// Number of indexed (entities, predicates, types).
    pub fn sizes(&self) -> (usize, usize, usize) {
        (self.entities.len(), self.predicates.len(), self.types.len())
    }

    fn search(&self, index: &VectorIndex, kind: LinkKind, text: &str, k: usize) -> Result<Vec<Candidate>> {
        let hits = index.search_text(self.embedder.as_ref(), text, k)?;
        Ok(hits
            .into_iter()
            .filter(|h| h.score >= self.min_score)
            .map(|h| Candidate { uri: h.id, label: h.text, kind, score: h.score })
            .collect())
    }

    /// Link a mention to its top-`k` candidates, routed by [`MentionKind`].
    pub fn link_mention(&self, mention: &Mention, k: usize) -> Result<Vec<Candidate>> {
        match mention.kind {
            MentionKind::Entity => self.search(&self.entities, LinkKind::Entity, &mention.text, k),
            MentionKind::Type => self.search(&self.types, LinkKind::Type, &mention.text, k),
            MentionKind::Literal => Ok(vec![Candidate {
                uri: String::new(),
                label: mention.text.clone(),
                kind: LinkKind::Literal,
                score: 1.0,
            }]),
        }
    }

    /// Link a relation phrase to its top-`k` predicate candidates.
    pub fn link_predicate(&self, phrase: &str, k: usize) -> Result<Vec<Candidate>> {
        self.search(&self.predicates, LinkKind::Predicate, phrase, k)
    }
}

/// Build `(uri, label)` pairs from a 2-column SELECT (label = the literal value).
fn pairs_with_label(ans: &SparqlAnswer, uri_var: &str, label_var: &str) -> Vec<(String, String)> {
    let SparqlAnswer::Select { vars, rows } = ans else {
        return Vec::new();
    };
    let (Some(ui), Some(li)) = (
        vars.iter().position(|v| v == uri_var),
        vars.iter().position(|v| v == label_var),
    ) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|r| {
            let uri = r.get(ui)?.as_ref()?.value.clone();
            let label = r.get(li)?.as_ref()?.value.clone();
            Some((uri, label))
        })
        .collect()
}

/// Build `(uri, local_name)` pairs from a 1-column SELECT of URIs.
fn uris_by_local_name(ans: &SparqlAnswer, var: &str) -> Vec<(String, String)> {
    ans.column_values(var)
        .into_iter()
        .map(|u| {
            let ln = local_name(&u);
            (u, ln)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use crate::kb::{MockKb, RdfTerm, TermKind};

    fn ent(uri: &str, label: &str) -> (String, String) {
        (uri.to_string(), label.to_string())
    }

    fn linker() -> Linker {
        Linker::from_labels(
            Box::new(HashEmbedder::new(128)),
            &[ent("http://ex/Berlin", "Berlin"), ent("http://ex/Paris", "Paris capital France")],
            &[ent("http://ex/capitalOf", "capital of"), ent("http://ex/population", "population")],
            &[ent("http://ex/City", "City"), ent("http://ex/Country", "Country")],
            0.0,
        )
        .unwrap()
    }

    #[test]
    fn local_name_strips_to_last_segment() {
        assert_eq!(local_name("http://ex/Berlin"), "Berlin");
        assert_eq!(local_name("http://x#name"), "name");
        assert_eq!(local_name("bare"), "bare");
    }

    #[test]
    fn links_entity_type_predicate() {
        let l = linker();
        assert_eq!(l.sizes(), (2, 2, 2));
        let e = l.link_mention(&Mention { text: "Berlin".into(), kind: MentionKind::Entity }, 1).unwrap();
        assert_eq!(e[0].uri, "http://ex/Berlin");
        assert_eq!(e[0].kind, LinkKind::Entity);

        let t = l.link_mention(&Mention { text: "City".into(), kind: MentionKind::Type }, 1).unwrap();
        assert_eq!(t[0].uri, "http://ex/City");

        let p = l.link_predicate("capital of", 1).unwrap();
        assert_eq!(p[0].uri, "http://ex/capitalOf");
        assert_eq!(p[0].to_term(), "<http://ex/capitalOf>");
    }

    #[test]
    fn literal_mention_passes_through() {
        let l = linker();
        let lit = l.link_mention(&Mention { text: "42".into(), kind: MentionKind::Literal }, 3).unwrap();
        assert_eq!(lit[0].kind, LinkKind::Literal);
        assert_eq!(lit[0].to_term(), "\"42\"");
    }

    #[test]
    fn min_score_filters() {
        let l = Linker::from_labels(
            Box::new(HashEmbedder::new(128)),
            &[ent("http://ex/Berlin", "Berlin")],
            &[],
            &[],
            0.99, // very high threshold
        )
        .unwrap();
        // unrelated query → below threshold → no candidates
        let e = l.link_mention(&Mention { text: "xyzzy plugh".into(), kind: MentionKind::Entity }, 5).unwrap();
        assert!(e.is_empty());
    }

    #[test]
    fn build_from_kb_runs_label_queries() {
        fn uri(v: &str) -> Option<RdfTerm> {
            Some(RdfTerm { kind: TermKind::Uri, value: v.into(), datatype: None, lang: None })
        }
        fn lit(v: &str) -> Option<RdfTerm> {
            Some(RdfTerm { kind: TermKind::Literal, value: v.into(), datatype: None, lang: None })
        }
        // Answers in query order: entities (?s ?l), predicates (?p), types (?t).
        let entities = SparqlAnswer::Select {
            vars: vec!["s".into(), "l".into()],
            rows: vec![vec![uri("http://ex/Berlin"), lit("Berlin")]],
        };
        let predicates = SparqlAnswer::Select {
            vars: vec!["p".into()],
            rows: vec![vec![uri("http://ex/capitalOf")]],
        };
        let types = SparqlAnswer::Select {
            vars: vec!["t".into()],
            rows: vec![vec![uri("http://ex/City")]],
        };
        let kb = MockKb::new(vec![entities, predicates, types]);
        let l = Linker::build_from_kb(&kb, Box::new(HashEmbedder::new(128)), 0.0).unwrap();
        assert_eq!(l.sizes(), (1, 1, 1));
        let p = l.link_predicate("capitalOf", 1).unwrap();
        assert_eq!(p[0].uri, "http://ex/capitalOf"); // local-name label matched
    }
}
