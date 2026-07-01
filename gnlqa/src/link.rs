//! Entity / predicate / type linking — map a natural-language mention to KG URIs
//! by semantic similarity over label vector indexes (built from gStore), the
//! modern replacement for gAnswer's Lucene + edit-distance + paraphrase
//! dictionary. Candidates are later validated against gStore's neighborhood
//! (C7) and used to ground SPARQL generation (C8).

use std::collections::HashSet;

use crate::embed::{Embedder, VectorIndex};
use crate::error::Result;
use crate::intent::{Mention, MentionKind};
use crate::kb::{sparql_escape_literal, KbClient, SparqlAnswer, TermKind};

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
    /// SPARQL surface form: `<uri>` for a URI term, an escaped quoted literal
    /// otherwise (the literal value can be user text, so it must be escaped).
    pub fn to_term(&self) -> String {
        if self.kind == LinkKind::Literal || self.uri.is_empty() {
            format!("\"{}\"", sparql_escape_literal(&self.label))
        } else {
            format!("<{}>", self.uri)
        }
    }
}

/// The local name of a URI — the part after the last `#`, `/`, or `:`, ignoring
/// a single trailing `/`. Falls back to the whole string if there's no separator.
pub fn local_name(uri: &str) -> String {
    let trimmed = uri.strip_suffix('/').unwrap_or(uri);
    let cut = trimmed.rfind(['#', '/', ':']).map(|i| i + 1).unwrap_or(0);
    let name = &trimmed[cut..];
    if name.is_empty() {
        trimmed.to_string()
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
    /// distinct predicates, and distinct types. Restricts labels to English /
    /// no-language tags. See [`build_from_kb_langs`](Self::build_from_kb_langs)
    /// for multilingual coverage.
    pub fn build_from_kb(
        kb: &dyn KbClient,
        embedder: Box<dyn Embedder>,
        min_score: f32,
        entity_limit: Option<usize>,
    ) -> Result<Linker> {
        Self::build_from_kb_langs(kb, embedder, min_score, entity_limit, &["en"])
    }

    /// Multilingual variant of [`build_from_kb`](Self::build_from_kb): index entity
    /// labels whose language tag is empty or in `languages`, so questions in those
    /// languages link against native labels rather than only English. Predicates
    /// and types are labeled by their URI local name (data rarely carries
    /// `rdfs:label` for them, and those names are language-neutral).
    pub fn build_from_kb_langs(
        kb: &dyn KbClient,
        embedder: Box<dyn Embedder>,
        min_score: f32,
        entity_limit: Option<usize>,
        languages: &[&str],
    ) -> Result<Linker> {
        // Restrict to the requested languages (+ no-language) and (optionally) cap
        // the count; a real KB can have millions of labels (full paging is a later
        // step).
        let lim = entity_limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
        let ent_q = format!(
            "SELECT DISTINCT ?s ?l WHERE {{ ?s <{RDFS_LABEL}> ?l \
             FILTER({}) }}{lim}",
            lang_filter_clause(languages)
        );
        let ent_ans = kb.query(&ent_q)?;
        warn_if_no_rows(&ent_ans, "entity label");
        let entities = pairs_with_label(&ent_ans, "s", "l");

        let pred_ans = kb.query("SELECT DISTINCT ?p WHERE { ?s ?p ?o }")?;
        let predicates = uris_by_local_name(&pred_ans, "p");

        let type_q = format!("SELECT DISTINCT ?t WHERE {{ ?s <{RDF_TYPE}> ?t }}");
        let type_ans = kb.query(&type_q)?;
        let types = uris_by_local_name(&type_ans, "t");

        Linker::from_labels(embedder, &entities, &predicates, &types, min_score)
    }

    /// Number of indexed (entities, predicates, types).
    pub fn sizes(&self) -> (usize, usize, usize) {
        (self.entities.len(), self.predicates.len(), self.types.len())
    }

    fn search(&self, index: &VectorIndex, kind: LinkKind, text: &str, k: usize) -> Result<Vec<Candidate>> {
        let hits = index.search_text(self.embedder.as_ref(), text, k)?;
        let mk = |h: crate::embed::Scored| Candidate { uri: h.id, label: h.text, kind, score: h.score };
        let mut kept: Vec<Candidate> =
            hits.iter().filter(|h| h.score >= self.min_score).cloned().map(mk).collect();
        // Relaxation (à la gAnswer): if the threshold pruned everything but there
        // was a best hit, keep it so linking never hard-fails on a real match —
        // downstream (C7) validates against the KG neighborhood.
        if kept.is_empty() {
            if let Some(best) = hits.into_iter().next() {
                kept.push(mk(best));
            }
        }
        Ok(kept)
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

/// Build a `FILTER` disjunction over language tags: always allows no-language
/// (`""`) labels, plus each requested code. Codes are sanitized to `[A-Za-z0-9-]`
/// (dropping anything else) so a resolved/config language can't inject into the
/// SPARQL FILTER; empties are skipped.
fn lang_filter_clause(languages: &[&str]) -> String {
    let mut clauses = vec!["lang(?l) = \"\"".to_string()];
    for l in languages {
        let code: String =
            l.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect::<String>().to_lowercase();
        if !code.is_empty() {
            clauses.push(format!("lang(?l) = \"{code}\""));
        }
    }
    clauses.join(" || ")
}

/// Build de-duplicated `(uri, label)` pairs from a 2-column SELECT (label = the
/// literal value). Skips blank-node subjects and keeps the first label per URI
/// (so multilingual duplicates don't inflate the index).
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
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let (Some(Some(ut)), Some(Some(lt))) = (r.get(ui), r.get(li)) else {
            continue;
        };
        if ut.kind == TermKind::Bnode {
            continue;
        }
        if seen.insert(ut.value.clone()) {
            out.push((ut.value.clone(), lt.value.clone()));
        }
    }
    out
}

/// Warn (don't fail) when a query expected to return rows returns none / a
/// non-SELECT shape — a common sign of a misconfigured endpoint or vocabulary.
fn warn_if_no_rows(ans: &SparqlAnswer, what: &str) {
    let empty = match ans {
        SparqlAnswer::Select { rows, .. } => rows.is_empty(),
        _ => true,
    };
    if empty {
        eprintln!("gnlqa: warning: {what} query returned no rows — linker will be sparse");
    }
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
    fn min_score_relaxes_to_best_when_all_pruned() {
        let l = Linker::from_labels(
            Box::new(HashEmbedder::new(128)),
            &[ent("http://ex/Berlin", "Berlin")],
            &[],
            &[],
            0.99, // very high threshold
        )
        .unwrap();
        // Everything is below threshold, but relaxation keeps the single best
        // (recall over a hard zero), and its score is genuinely below the floor.
        let e = l.link_mention(&Mention { text: "xyzzy plugh".into(), kind: MentionKind::Entity }, 5).unwrap();
        assert_eq!(e.len(), 1);
        assert!(e[0].score < 0.99);
    }

    #[test]
    fn literal_to_term_escapes_quotes() {
        let l = linker();
        let c = l
            .link_mention(&Mention { text: "say \"hi\"\nbye".into(), kind: MentionKind::Literal }, 1)
            .unwrap();
        // no raw quote/newline can break out of the SPARQL literal
        assert_eq!(c[0].to_term(), "\"say \\\"hi\\\"\\nbye\"");
    }

    #[test]
    fn local_name_handles_urn_and_trailing_slash() {
        assert_eq!(local_name("http://ex/Berlin/"), "Berlin");
        assert_eq!(local_name("urn:isbn:0451450523"), "0451450523");
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
        let l = Linker::build_from_kb(&kb, Box::new(HashEmbedder::new(128)), 0.0, None).unwrap();
        assert_eq!(l.sizes(), (1, 1, 1));
        let p = l.link_predicate("capitalOf", 1).unwrap();
        assert_eq!(p[0].uri, "http://ex/capitalOf"); // local-name label matched
    }

    #[test]
    fn lang_filter_always_includes_empty_and_sanitizes() {
        let c = lang_filter_clause(&["en", "zh"]);
        assert!(c.contains(r#"lang(?l) = """#)); // no-language always allowed
        assert!(c.contains(r#"lang(?l) = "en""#));
        assert!(c.contains(r#"lang(?l) = "zh""#));
        // injection attempt is stripped to safe chars ([A-Za-z0-9-]) and
        // case-normalized, collapsing to one harmless code — no query structure
        // (quotes/braces/UNION/spaces) survives to break out of the FILTER.
        let evil = lang_filter_clause(&[r#"en") } UNION { ?s ?p ?o . FILTER("#]);
        assert!(evil.contains(r#"lang(?l) = "enunionspofilter""#));
        assert!(!evil.contains('}') && !evil.contains("UNION") && !evil.contains(" ?"));
    }

    #[test]
    fn build_from_kb_langs_filters_by_language() {
        fn uri(v: &str) -> Option<RdfTerm> {
            Some(RdfTerm { kind: TermKind::Uri, value: v.into(), datatype: None, lang: None })
        }
        fn lit_lang(v: &str, l: &str) -> Option<RdfTerm> {
            Some(RdfTerm { kind: TermKind::Literal, value: v.into(), datatype: None, lang: Some(l.into()) })
        }
        // Entity labels in Chinese; the ?s ?l answer is what our FILTER'd query
        // would return. Ordering: entities, predicates, types.
        let entities = SparqlAnswer::Select {
            vars: vec!["s".into(), "l".into()],
            rows: vec![vec![uri("http://ex/Beijing"), lit_lang("北京", "zh")]],
        };
        let predicates = SparqlAnswer::Select { vars: vec!["p".into()], rows: vec![] };
        let types = SparqlAnswer::Select { vars: vec!["t".into()], rows: vec![] };
        let kb = MockKb::new(vec![entities, predicates, types]);
        let l = Linker::build_from_kb_langs(
            &kb,
            Box::new(HashEmbedder::new(128)),
            0.0,
            None,
            &["zh"],
        )
        .unwrap();
        assert_eq!(l.sizes().0, 1); // the Chinese-labeled entity was indexed
        let hits = l.link_mention(&Mention { text: "北京".into(), kind: MentionKind::Entity }, 1).unwrap();
        assert_eq!(hits[0].uri, "http://ex/Beijing");
    }
}
