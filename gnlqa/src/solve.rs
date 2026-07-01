//! The full question-answering pipeline (C10): understand → link → ground →
//! generate N candidates → self-repair each → rank → answer. This is the
//! multi-candidate, data-driven disambiguation gAnswer pioneered, here driven by
//! the LLM and validated against gStore.

use crate::error::{Error, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use crate::generate::generate_candidates;
use crate::ground::{explain, gather_citations};
use crate::intent::{extract_intent, QType, QuestionIntent};
use crate::kb::KbClient;
use crate::link::{LinkKind, Linker};
use crate::llm::LlmClient;
use crate::pipeline::{answer_values, render_answer, Answer};
use crate::repair::{is_empty_answer, solve_with_repair, RepairOutcome};
use crate::schema::SchemaContext;

/// Score a solved outcome for ranking: a non-empty answer dominates an empty
/// one; each repair round is a small penalty (prefer queries that worked sooner).
/// The penalty is clamped so a non-empty answer always outranks an empty one,
/// regardless of `max_rounds`. This is a *ranking* heuristic reused as a rough
/// confidence — it is NOT a calibrated correctness probability.
pub fn score_outcome(o: &RepairOutcome) -> f32 {
    let base = if is_empty_answer(&o.answer) { 0.1 } else { 1.0 };
    let penalty = (0.05 * o.rounds as f32).min(0.89);
    base - penalty
}

/// Order-preserving de-duplication of URIs.
fn dedup(uris: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    uris.into_iter().filter(|u| seen.insert(u.clone())).collect()
}

/// Solve every candidate (with repair) and return the highest-scoring outcome.
#[allow(clippy::too_many_arguments)]
pub fn best_of(
    llm: &dyn LlmClient,
    kb: &dyn KbClient,
    question: &str,
    links: &str,
    schema: &str,
    candidates: &[String],
    max_rounds: usize,
    model: Option<&str>,
) -> Option<RepairOutcome> {
    let mut best: Option<(RepairOutcome, f32)> = None;
    for c in candidates {
        if let Ok(o) = solve_with_repair(llm, kb, question, links, schema, c, max_rounds, model) {
            let s = score_outcome(&o);
            if best.as_ref().is_none_or(|(_, bs)| s > *bs) {
                best = Some((o, s));
            }
        }
    }
    best.map(|(o, _)| o)
}

/// A small bounded answer cache (FIFO eviction). gNLQA does not mutate the KB,
/// but the KB may change externally, so caching is opt-in and staleness-bounded.
struct Cache {
    cap: usize,
    map: HashMap<String, Answer>,
    order: VecDeque<String>,
}

impl Cache {
    fn new(cap: usize) -> Cache {
        Cache { cap: cap.max(1), map: HashMap::new(), order: VecDeque::new() }
    }
    fn get(&self, k: &str) -> Option<Answer> {
        self.map.get(k).cloned()
    }
    fn put(&mut self, k: String, v: Answer) {
        if self.map.insert(k.clone(), v).is_some() {
            return; // already present, order unchanged
        }
        self.order.push_back(k);
        while self.map.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

/// The full QA engine: LLM + KB + (optional) linker.
pub struct SolveEngine {
    llm: Box<dyn LlmClient>,
    kb: Box<dyn KbClient>,
    linker: Option<Linker>,
    candidates: usize,
    max_rounds: usize,
    sample: usize,
    link_k: usize,
    cite: bool,
    explain: bool,
    abstain_below: f32,
    model: Option<String>,
    fast_model: Option<String>,
    cache: Option<Mutex<Cache>>,
    graphrag: bool,
    rag: crate::graphrag::RetrievalCfg,
    analytics: bool,
    analytics_cfg: crate::analytics::AnalyticsCfg,
}

/// Heuristic confidence for a GraphRAG answer: plausible-from-context, but not
/// KB-verified the way a SELECT result is. Kept below the structured-answer band.
const RAG_CONFIDENCE: f32 = 0.5;

/// Confidence for a graph-analytics answer: a deterministic computation, but over
/// a capped edge sample — high, not certain.
const ANALYTICS_CONFIDENCE: f32 = 0.85;

impl SolveEngine {
    pub fn new(llm: Box<dyn LlmClient>, kb: Box<dyn KbClient>) -> SolveEngine {
        SolveEngine {
            llm,
            kb,
            linker: None,
            candidates: 4,
            max_rounds: 2,
            sample: 30,
            link_k: 3,
            cite: true,
            explain: false,
            abstain_below: 0.0,
            model: None,
            fast_model: None,
            cache: None,
            graphrag: true,
            rag: crate::graphrag::RetrievalCfg::default(),
            analytics: true,
            analytics_cfg: crate::analytics::AnalyticsCfg::default(),
        }
    }
    /// Enable/disable graph-analytics routing for `analytics`-typed questions
    /// (shortest path, centrality, PageRank, communities, triangles; default on).
    pub fn with_analytics(mut self, on: bool) -> SolveEngine {
        self.analytics = on;
        self
    }
    /// Cap on edges pulled from the KB for an analytics run.
    pub fn with_analytics_max_edges(mut self, max_edges: usize) -> SolveEngine {
        self.analytics_cfg.max_edges = max_edges.max(1);
        self
    }
    /// Enable/disable the GraphRAG fallback for open questions and empty
    /// structured results (default on).
    pub fn with_graphrag(mut self, on: bool) -> SolveEngine {
        self.graphrag = on;
        self
    }
    /// BFS depth for GraphRAG subgraph retrieval (≥1).
    pub fn with_rag_hops(mut self, hops: usize) -> SolveEngine {
        self.rag.hops = hops.max(1);
        self
    }
    /// Cheaper model for simple questions (factoid/list/count/boolean); complex
    /// ones (analytics/path/compare/open) still use the primary model.
    pub fn with_fast_model(mut self, model: impl Into<String>) -> SolveEngine {
        self.fast_model = Some(model.into());
        self
    }
    /// Enable a bounded answer cache of `capacity` questions (0 disables).
    pub fn with_cache(mut self, capacity: usize) -> SolveEngine {
        self.cache = (capacity > 0).then(|| Mutex::new(Cache::new(capacity)));
        self
    }

    /// Pick the model for generation/repair based on question difficulty.
    fn route_model(&self, intent: &QuestionIntent) -> Option<String> {
        let hard = matches!(intent.qtype, QType::Analytics | QType::Path | QType::Compare | QType::Open);
        if hard {
            self.model.clone()
        } else {
            self.fast_model.clone().or_else(|| self.model.clone())
        }
    }
    /// Abstain (return a low-confidence "not sure" answer) when the winning
    /// outcome's confidence is below `threshold` (default 0.0 = never abstain).
    pub fn with_abstain_below(mut self, threshold: f32) -> SolveEngine {
        self.abstain_below = threshold;
        self
    }
    /// Attach supporting triples to the answer (default on).
    pub fn with_citations(mut self, on: bool) -> SolveEngine {
        self.cite = on;
        self
    }
    /// Have the LLM phrase a grounded natural-language answer (default off).
    pub fn with_explain(mut self, on: bool) -> SolveEngine {
        self.explain = on;
        self
    }
    pub fn with_linker(mut self, linker: Linker) -> SolveEngine {
        self.linker = Some(linker);
        self
    }
    pub fn with_model(mut self, model: impl Into<String>) -> SolveEngine {
        self.model = Some(model.into());
        self
    }
    pub fn candidates(mut self, n: usize) -> SolveEngine {
        self.candidates = n.max(1);
        self
    }
    pub fn with_max_rounds(mut self, n: usize) -> SolveEngine {
        self.max_rounds = n;
        self
    }
    pub fn with_sample(mut self, n: usize) -> SolveEngine {
        self.sample = n.max(1);
        self
    }
    /// Top-k candidates to retrieve per linked mention/relation.
    pub fn with_link_k(mut self, k: usize) -> SolveEngine {
        self.link_k = k.max(1);
        self
    }

    /// The KB client, for direct SPARQL / analytics access (e.g. the MCP server).
    pub fn kb(&self) -> &dyn KbClient {
        self.kb.as_ref()
    }
    /// The configured entity linker, if any.
    pub fn linker(&self) -> Option<&Linker> {
        self.linker.as_ref()
    }
    /// The analytics retrieval configuration.
    pub fn analytics_cfg(&self) -> crate::analytics::AnalyticsCfg {
        self.analytics_cfg
    }

    /// Answer a question end-to-end (cache-aware).
    pub fn ask(&self, question: &str) -> Result<Answer> {
        let key = question.trim();
        if let Some(cache) = &self.cache {
            if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(key) {
                return Ok(hit);
            }
        }
        let answer = self.solve_inner(question)?;
        // Don't cache abstentions (a borderline "not sure" shouldn't be sticky).
        if let Some(cache) = &self.cache {
            if !answer.abstained {
                let val = answer.clone(); // clone outside the lock
                cache.lock().unwrap_or_else(|e| e.into_inner()).put(key.to_string(), val);
            }
        }
        Ok(answer)
    }

    /// Rewrite a follow-up into a standalone question using prior `(question,
    /// answer)` turns (routed to the fast model), falling back to the raw
    /// question on any error. Exposed so a [`Session`](crate::session::Session)
    /// can record the *resolved* question rather than the elliptical one.
    pub fn rewrite_followup_question(&self, history: &[(String, String)], question: &str) -> String {
        let m = self.fast_model.clone().or_else(|| self.model.clone());
        crate::session::rewrite_followup(self.llm.as_ref(), history, question, m.as_deref())
            .unwrap_or_else(|_| question.to_string())
    }

    /// Answer a follow-up given prior `(question, answer)` turns: rewrite the
    /// (possibly elliptical) question into a standalone one, then [`ask`](Self::ask).
    pub fn ask_followup(&self, history: &[(String, String)], question: &str) -> Result<Answer> {
        let standalone = self.rewrite_followup_question(history, question);
        self.ask(&standalone)
    }

    fn solve_inner(&self, question: &str) -> Result<Answer> {
        // 1) Understand. Intent is a cheap classification → run it on the fast
        // model. A parse failure shouldn't kill the query — degrade.
        let intent_model = self.fast_model.clone().or_else(|| self.model.clone());
        let intent =
            extract_intent(self.llm.as_ref(), question, intent_model.as_deref()).unwrap_or_default();
        // Resolve the answer language (LLM tag → script fallback → English) so
        // LLM-generated answers come back in the user's language.
        let lang = crate::lang::resolve_lang(&intent.lang, question);
        // Route generation/repair to the fast or primary model by difficulty.
        let model_owned = self.route_model(&intent);
        let model = model_owned.as_deref();

        // 2) Link + 3) ground (only if a linker is configured).
        let mut links = String::new();
        let mut ent_uris = Vec::new();
        let mut pred_uris = Vec::new();
        if let Some(linker) = &self.linker {
            for m in &intent.mentions {
                let cands = linker.link_mention(m, self.link_k)?;
                if !cands.is_empty() {
                    links.push_str(&format!("{:?} '{}' -> ", m.kind, m.text));
                    links.push_str(&cands.iter().map(|c| c.to_term()).collect::<Vec<_>>().join(", "));
                    links.push('\n');
                }
                for c in cands {
                    match c.kind {
                        LinkKind::Entity if !c.uri.is_empty() => ent_uris.push(c.uri),
                        LinkKind::Type if !c.uri.is_empty() => ent_uris.push(c.uri),
                        _ => {}
                    }
                }
            }
            for r in &intent.relations {
                let cands = linker.link_predicate(&r.phrase, self.link_k)?;
                if !cands.is_empty() {
                    links.push_str(&format!("relation '{}' -> ", r.phrase));
                    links.push_str(&cands.iter().map(|c| c.to_term()).collect::<Vec<_>>().join(", "));
                    links.push('\n');
                }
                for c in cands {
                    if !c.uri.is_empty() {
                        pred_uris.push(c.uri);
                    }
                }
            }
        }
        // De-duplicate before grounding so we don't run identical schema queries
        // (and bloat the prompt) for a URI linked by several mentions.
        let ent_uris = dedup(ent_uris);
        let pred_uris = dedup(pred_uris);

        // Graph-analytics routing: shortest path / centrality / PageRank /
        // communities / triangles aren't a single BGP — run gStore's analytics
        // engine over a retrieved edge sample. Gated by the abstain threshold for
        // the same reason as the GraphRAG fallback. On any error, fall through to
        // the normal Text-to-SPARQL pipeline.
        if self.analytics
            && intent.qtype == QType::Analytics
            && ANALYTICS_CONFIDENCE >= self.abstain_below
        {
            match crate::analytics::run_analytics(
                self.kb.as_ref(),
                question,
                &ent_uris,
                self.analytics_cfg,
            ) {
                Ok(res) => {
                    let text = if res.truncated {
                        format!(
                            "{}\n(computed over a capped sample of {} edges / {} nodes)",
                            res.text, res.edge_count, res.node_count
                        )
                    } else {
                        res.text
                    };
                    return Ok(Answer {
                        text,
                        values: Vec::new(),
                        sparql: None,
                        rounds: 0,
                        citations: Vec::new(),
                        explanation: None,
                        confidence: ANALYTICS_CONFIDENCE,
                        abstained: false,
                    });
                }
                // Don't mask the failure silently — surface it, then fall through
                // to the Text-to-SPARQL path (which may still answer or abstain).
                Err(e) => eprintln!("gnlqa: analytics routing failed, falling back to SPARQL: {e}"),
            }
        }

        let schema = if ent_uris.is_empty() && pred_uris.is_empty() {
            String::new()
        } else {
            SchemaContext::gather(self.kb.as_ref(), &ent_uris, &pred_uris, self.sample)?.render()
        };

        // 4) Generate N candidates, 5) repair-solve each, 6) rank. If generation
        // produced nothing parser-valid, fail honestly rather than fabricating an
        // answer from a wildcard query.
        let candidates =
            generate_candidates(self.llm.as_ref(), question, &links, &schema, self.candidates, model)?;
        let best = best_of(
            self.llm.as_ref(),
            self.kb.as_ref(),
            question,
            &links,
            &schema,
            &candidates,
            self.max_rounds,
            model,
        );

        // GraphRAG fallback: when the structured path produced no non-empty
        // answer, retrieve a subgraph around the linked entities and let the LLM
        // answer from it (open/relational questions a single BGP can't express).
        let structured_ok = best.as_ref().is_some_and(|o| !is_empty_answer(&o.answer));
        // Skip the fallback when its fixed confidence couldn't clear the caller's
        // abstain threshold — otherwise we'd present a RAG answer the structured
        // path would have withheld. Falling through lets `match best` abstain.
        if !structured_ok
            && self.graphrag
            && !ent_uris.is_empty()
            && RAG_CONFIDENCE >= self.abstain_below
        {
            if let Some(ans) = self.graphrag_answer(question, &ent_uris, model, &lang) {
                return Ok(ans);
            }
        }

        match best {
            Some(o) => {
                let values = answer_values(&o.answer);
                let confidence = score_outcome(&o).clamp(0.0, 1.0);

                // Abstain when too unsure: keep the SPARQL (transparency) but
                // don't present a possibly-wrong answer.
                if confidence < self.abstain_below {
                    return Ok(Answer {
                        text: crate::lang::abstain_message(&lang).to_string(),
                        values: Vec::new(), // suppress the withheld answer
                        sparql: Some(o.sparql),
                        rounds: o.rounds,
                        citations: Vec::new(),
                        explanation: None,
                        confidence,
                        abstained: true,
                    });
                }

                let text = render_answer(&o.answer, &values);
                let citations = if self.cite {
                    gather_citations(self.kb.as_ref(), &o.answer, 5, 10, 30).unwrap_or_default()
                } else {
                    Vec::new()
                };
                // Grounded prose is exposed via `explanation` (only when it could
                // be grounded on citations); `text` stays the rendered answer.
                let explanation = if self.explain {
                    explain(self.llm.as_ref(), question, &citations, model, &lang)
                } else {
                    None
                };
                Ok(Answer {
                    text,
                    values,
                    sparql: Some(o.sparql),
                    rounds: o.rounds,
                    citations,
                    explanation,
                    confidence,
                    abstained: false,
                })
            }
            None => Err(Error::Sparql("no candidate query produced an answer".into())),
        }
    }

    /// Retrieve a subgraph around `seeds` and let the LLM answer from it. Returns
    /// `None` when nothing was retrieved or the model declined ("I don't know"),
    /// so the caller can fall through to the honest structured result/error.
    fn graphrag_answer(
        &self,
        question: &str,
        seeds: &[String],
        model: Option<&str>,
        lang: &str,
    ) -> Option<Answer> {
        use crate::graphrag::{answer_from_subgraph, is_dont_know, render_subgraph, retrieve_subgraph};
        let triples = retrieve_subgraph(self.kb.as_ref(), seeds, self.rag);
        if triples.is_empty() {
            return None;
        }
        let subgraph = render_subgraph(&triples);
        let raw = answer_from_subgraph(self.llm.as_ref(), question, &subgraph, model, lang).ok()?;
        if is_dont_know(&raw) {
            return None;
        }
        let citations =
            if self.cite { triples.iter().take(30).map(|t| t.to_citation()).collect() } else { Vec::new() };
        Some(Answer {
            text: raw.trim().to_string(),
            values: Vec::new(),
            sparql: None, // GraphRAG has no single query to expose
            rounds: 0,
            citations,
            explanation: None,
            confidence: RAG_CONFIDENCE,
            abstained: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::{MockKb, RdfTerm, SparqlAnswer, TermKind};
    use crate::llm::MockLlm;
    use crate::repair::RepairOutcome;

    fn nonempty() -> SparqlAnswer {
        SparqlAnswer::Select {
            vars: vec!["x".into()],
            rows: vec![vec![Some(RdfTerm { kind: TermKind::Uri, value: "http://ex/a".into(), datatype: None, lang: None })]],
        }
    }
    fn empty() -> SparqlAnswer {
        SparqlAnswer::Select { vars: vec!["x".into()], rows: vec![] }
    }

    #[test]
    fn score_prefers_nonempty_and_fewer_rounds() {
        let a = RepairOutcome { sparql: "q".into(), answer: nonempty(), rounds: 0 };
        let b = RepairOutcome { sparql: "q".into(), answer: nonempty(), rounds: 2 };
        let c = RepairOutcome { sparql: "q".into(), answer: empty(), rounds: 0 };
        assert!(score_outcome(&a) > score_outcome(&b));
        assert!(score_outcome(&b) > score_outcome(&c));
    }

    #[test]
    fn best_of_picks_nonempty_candidate() {
        // candidate 1 → empty, candidate 2 → nonempty. best_of must pick #2.
        let llm = MockLlm::fixed("unused");
        let kb = MockKb::new(vec![empty(), nonempty()]);
        let cands = vec![
            "SELECT ?x WHERE { ?x <http://ex/p1> ?o }".to_string(),
            "SELECT ?x WHERE { ?x <http://ex/p2> ?o }".to_string(),
        ];
        let best = best_of(&llm, &kb, "q", "", "", &cands, 0, None).unwrap();
        assert!(!is_empty_answer(&best.answer));
        assert!(best.sparql.contains("p2"));
    }

    #[test]
    fn engine_ask_end_to_end_without_linker() {
        // intent JSON, then the generation array, then queries answered.
        let llm = MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]);
        let kb = MockKb::new(vec![nonempty()]);
        let engine = SolveEngine::new(Box::new(llm), Box::new(kb));
        let a = engine.ask("which x?").unwrap();
        assert_eq!(a.values, vec!["http://ex/a"]);
        assert!(a.sparql.unwrap().contains("<http://ex/p>"));
    }

    #[test]
    fn confidence_high_for_nonempty_and_abstains_when_unsure() {
        let llm = MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]);
        let kb = MockKb::new(vec![nonempty()]);
        let a = SolveEngine::new(Box::new(llm), Box::new(kb)).with_citations(false).ask("q").unwrap();
        assert!(a.confidence > 0.9 && !a.abstained);

        // empty result + high abstain threshold → abstain, but SPARQL stays visible
        let llm2 = MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]);
        let kb2 = MockKb::new(vec![empty()]);
        let a2 = SolveEngine::new(Box::new(llm2), Box::new(kb2))
            .with_max_rounds(0)
            .with_abstain_below(0.5)
            .ask("q")
            .unwrap();
        assert!(a2.abstained && a2.confidence < 0.5);
        assert!(a2.sparql.is_some());
    }

    #[test]
    fn cache_serves_repeat_without_recompute() {
        use std::sync::Arc;
        let llm = Arc::new(MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]));
        let kb = MockKb::new(vec![nonempty()]);
        let engine = SolveEngine::new(Box::new(Arc::clone(&llm)), Box::new(kb))
            .with_citations(false)
            .with_cache(8);
        let a1 = engine.ask("q").unwrap();
        let a2 = engine.ask("q").unwrap(); // from cache
        assert_eq!(a1.values, a2.values);
        assert_eq!(llm.call_count(), 2, "second ask must be served from cache");
    }

    #[test]
    fn model_routing_by_difficulty() {
        use crate::intent::{QType, QuestionIntent};
        let e = SolveEngine::new(Box::new(MockLlm::fixed("x")), Box::new(MockKb::new(vec![])))
            .with_model("opus-primary")
            .with_fast_model("sonnet-fast");
        let factoid = QuestionIntent { qtype: QType::Factoid, ..Default::default() };
        let analytics = QuestionIntent { qtype: QType::Analytics, ..Default::default() };
        assert_eq!(e.route_model(&factoid).as_deref(), Some("sonnet-fast"));
        assert_eq!(e.route_model(&analytics).as_deref(), Some("opus-primary"));
    }

    #[test]
    fn engine_fails_honestly_when_no_valid_candidate() {
        // generation yields only an invalid candidate, GraphRAG disabled → no
        // fabricated answer
        let llm = MockLlm::new(vec![r#"{"qtype":"factoid"}"#.to_string(), r#"["not sparql"]"#.to_string()]);
        let kb = MockKb::new(vec![nonempty()]);
        let engine = SolveEngine::new(Box::new(llm), Box::new(kb)).with_graphrag(false);
        assert!(engine.ask("q").is_err());
    }

    #[test]
    fn graphrag_fallback_when_no_structured_answer() {
        use crate::embed::HashEmbedder;
        use crate::link::Linker;
        // linker maps "Alien" → a URI so the entity becomes a GraphRAG seed
        let linker = Linker::from_labels(
            Box::new(HashEmbedder::new(64)),
            &[("http://ex/Alien".to_string(), "Alien".to_string())],
            &[],
            &[],
            0.0,
        )
        .unwrap();
        // LLM: intent (open + Alien mention), generation (no valid query), RAG answer.
        let llm = MockLlm::new(vec![
            r#"{"qtype":"open","mentions":[{"text":"Alien","kind":"entity"}]}"#.to_string(),
            r#"["not a query"]"#.to_string(),
            "Ridley Scott directed Alien.".to_string(),
        ]);
        // Every KB query returns a (p, o) row (schema gather + GraphRAG outgoing).
        let po = SparqlAnswer::Select {
            vars: vec!["p".into(), "o".into()],
            rows: vec![vec![
                Some(RdfTerm { kind: TermKind::Uri, value: "http://ex/directed".into(), datatype: None, lang: None }),
                Some(RdfTerm { kind: TermKind::Uri, value: "http://ex/RidleyScott".into(), datatype: None, lang: None }),
            ]],
        };
        let kb = MockKb::new(vec![po]);
        let engine = SolveEngine::new(Box::new(llm), Box::new(kb))
            .with_linker(linker)
            .with_model("primary");
        let a = engine.ask("tell me about Alien").unwrap();
        assert_eq!(a.text, "Ridley Scott directed Alien.");
        assert!(a.sparql.is_none()); // GraphRAG path exposes no single query
        assert!(!a.citations.is_empty()); // grounded on the retrieved triple
        assert!((a.confidence - 0.5).abs() < 1e-6);
    }

    #[test]
    fn graphrag_respects_abstain_threshold() {
        use crate::embed::HashEmbedder;
        use crate::link::Linker;
        let linker = Linker::from_labels(
            Box::new(HashEmbedder::new(64)),
            &[("http://ex/Alien".to_string(), "Alien".to_string())],
            &[],
            &[],
            0.0,
        )
        .unwrap();
        // A *valid* query that returns empty → structured Some(empty), conf ~0.1.
        // RAG_CONFIDENCE (0.5) is below abstain_below (0.7), so the fallback must
        // be skipped and the engine must abstain rather than emit a RAG answer.
        let llm = MockLlm::new(vec![
            r#"{"qtype":"open","mentions":[{"text":"Alien","kind":"entity"}]}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
            "SHOULD NOT BE CALLED — RAG must be skipped".to_string(),
        ]);
        let kb = MockKb::new(vec![empty()]);
        let a = SolveEngine::new(Box::new(llm), Box::new(kb))
            .with_linker(linker)
            .with_max_rounds(0)
            .with_abstain_below(0.7)
            .ask("tell me about Alien")
            .unwrap();
        assert!(a.abstained, "sub-threshold RAG must not override the abstain policy");
        assert!(a.confidence < 0.7);
    }
}
