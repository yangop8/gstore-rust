//! Graph-analytics routing (C17). Some questions — shortest path between two
//! entities, "most central / most important" nodes, communities, triangle counts
//! — aren't a single SPARQL BGP; they're graph computations. gStore already ships
//! a CSR analytics engine ([`gstore::analytics::GraphView`]: BFS/shortest-path,
//! PageRank, weakly-connected components, degree centrality, triangles). gnlqa
//! talks to gStore over HTTP SPARQL, so we retrieve the (capped) edge set with a
//! SPARQL query, project it into the id-space graph, and reuse that same engine —
//! the exact algorithms the C++ graph-computation layer runs.

use std::cmp::Ordering;
use std::collections::HashMap;

use gstore::analytics::GraphView;
use gstore::model::id::{EntityLiteralId, PredId};
use gstore::model::IdTriple;
use gstore::store::TripleSource;

use crate::error::{Error, Result};
use crate::kb::{KbClient, SparqlAnswer};

/// Which graph computation a question maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyticsOp {
    /// Shortest path between two linked entities.
    ShortestPath,
    /// PageRank importance ranking (top-k).
    PageRank,
    /// Degree centrality (top-k by in+out degree).
    Centrality,
    /// Weakly-connected components (count + largest).
    Components,
    /// Triangle count.
    Triangles,
}

/// Classify the analytics operation from the question text (keyword heuristic;
/// the LLM already tagged the question `analytics`, this picks *which* one).
pub fn classify_op(question: &str) -> AnalyticsOp {
    let q = question.to_lowercase();
    let has = |p: &str| q.contains(p);
    if has("shortest path") || has("path between") || has("path from") || has("connect") && has("to")
    {
        AnalyticsOp::ShortestPath
    } else if has("triangle") {
        AnalyticsOp::Triangles
    } else if has("communit") || has("cluster") || has("connected component") || has("component") {
        AnalyticsOp::Components
    } else if has("pagerank") || has("page rank") || has("influential") || has("important") {
        AnalyticsOp::PageRank
    } else if has("central") || has("most connected") || has("highest degree") || has("hub") {
        AnalyticsOp::Centrality
    } else {
        AnalyticsOp::PageRank // sensible default: importance ranking
    }
}

/// Bounds for analytics retrieval.
#[derive(Debug, Clone, Copy)]
pub struct AnalyticsCfg {
    /// Cap on edges pulled from the KB (analytics runs over this sample).
    pub max_edges: usize,
    /// How many nodes to list for ranking operations.
    pub top_k: usize,
}

impl Default for AnalyticsCfg {
    fn default() -> Self {
        AnalyticsCfg { max_edges: 50_000, top_k: 10 }
    }
}

/// The outcome of an analytics run: a rendered answer plus graph size (so the
/// caller can note the sample was capped).
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyticsResult {
    pub text: String,
    pub op: AnalyticsOp,
    pub node_count: usize,
    pub edge_count: usize,
    /// Whether the edge sample hit `max_edges` (results are over a subgraph).
    pub truncated: bool,
}

/// A minimal in-memory [`TripleSource`] over a projected edge list, so a
/// [`GraphView`] can be built from SPARQL-retrieved edges. Only `iter_all` (and
/// `triple_count`) carry data — `GraphView::from_source` uses nothing else — so
/// the remaining trait methods are inert stubs.
struct EdgeListSource {
    triples: Vec<IdTriple>,
}

#[rustfmt::skip]
impl TripleSource for EdgeListSource {
    fn iter_all(&self) -> Vec<IdTriple> { self.triples.clone() }
    fn triple_count(&self) -> u64 { self.triples.len() as u64 }
    fn exists(&self, _s: EntityLiteralId, _p: PredId, _o: EntityLiteralId) -> bool { false }
    fn po_by_s(&self, _s: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> { Vec::new() }
    fn o_by_sp(&self, _s: EntityLiteralId, _p: PredId) -> Vec<EntityLiteralId> { Vec::new() }
    fn p_by_so(&self, _s: EntityLiteralId, _o: EntityLiteralId) -> Vec<PredId> { Vec::new() }
    fn ps_by_o(&self, _o: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> { Vec::new() }
    fn s_by_po(&self, _p: PredId, _o: EntityLiteralId) -> Vec<EntityLiteralId> { Vec::new() }
    fn so_by_p(&self, _p: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> { Vec::new() }
    fn subs_by_p(&self, _p: PredId) -> Vec<EntityLiteralId> { Vec::new() }
    fn objs_by_p(&self, _p: PredId) -> Vec<EntityLiteralId> { Vec::new() }
    fn subject_keys(&self) -> Vec<EntityLiteralId> { Vec::new() }
    fn object_keys(&self) -> Vec<EntityLiteralId> { Vec::new() }
    fn distinct_subjects(&self) -> usize { 0 }
    fn distinct_objects(&self) -> usize { 0 }
    fn num_predicates(&self) -> usize { 0 }
    fn pred_card(&self, _p: PredId) -> usize { 0 }
    fn pred_distinct_subj(&self, _p: PredId) -> usize { 0 }
    fn pred_distinct_obj(&self, _p: PredId) -> usize { 0 }
}

/// A projected graph: the dense-id [`GraphView`] plus surface-form maps to
/// translate results back to `<uri>` / `"literal"` strings.
struct ProjectedGraph {
    view: GraphView,
    id_of: HashMap<String, EntityLiteralId>,
    term_of: Vec<String>,
    truncated: bool,
}

/// Intern a surface form into a dense id (stable insertion order → `term_of`).
fn intern(id_of: &mut HashMap<String, EntityLiteralId>, term_of: &mut Vec<String>, key: String) -> EntityLiteralId {
    if let Some(&id) = id_of.get(&key) {
        return id;
    }
    let id = term_of.len() as EntityLiteralId;
    term_of.push(key.clone());
    id_of.insert(key, id);
    id
}

/// SPARQL to pull a capped edge sample (every triple projected to `?s → ?o`,
/// matching the analytics engine, which ignores predicates).
fn edge_query(limit: usize) -> String {
    format!("SELECT ?s ?o WHERE {{ ?s ?p ?o }} LIMIT {limit}")
}

/// Retrieve the edge sample and build the projected graph.
fn project(kb: &dyn KbClient, cfg: AnalyticsCfg) -> Result<ProjectedGraph> {
    let limit = cfg.max_edges.max(1);
    let ans = kb.query(&edge_query(limit))?;
    let SparqlAnswer::Select { vars, rows } = &ans else {
        return Err(Error::GStore("analytics edge query did not return a SELECT".into()));
    };
    let (Some(si), Some(oi)) =
        (vars.iter().position(|v| v == "s"), vars.iter().position(|v| v == "o"))
    else {
        return Err(Error::GStore("analytics edge query must bind ?s and ?o".into()));
    };
    let truncated = rows.len() >= limit;
    let mut id_of: HashMap<String, EntityLiteralId> = HashMap::new();
    let mut term_of: Vec<String> = Vec::new();
    let mut triples: Vec<IdTriple> = Vec::with_capacity(rows.len());
    for r in rows {
        if let (Some(Some(s)), Some(Some(o))) = (r.get(si), r.get(oi)) {
            let sid = intern(&mut id_of, &mut term_of, s.to_term_string());
            let oid = intern(&mut id_of, &mut term_of, o.to_term_string());
            triples.push(IdTriple::new(sid, 0, oid));
        }
    }
    let src = EdgeListSource { triples };
    let view = GraphView::from_source(&src);
    Ok(ProjectedGraph { view, id_of, term_of, truncated })
}

/// Render a `(id, score)` ranking as a top-k list of surface forms.
fn render_ranking(mut scored: Vec<(EntityLiteralId, f64)>, term_of: &[String], label: &str, k: usize) -> String {
    // Highest score first; break ties by id for determinism.
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal).then(a.0.cmp(&b.0)));
    if scored.is_empty() {
        return format!("No nodes to rank by {label}.");
    }
    let lines: Vec<String> = scored
        .iter()
        .take(k)
        .map(|(id, s)| format!("  {} ({s:.4})", term_of[*id as usize]))
        .collect();
    format!("Top {} by {label}:\n{}", lines.len(), lines.join("\n"))
}

fn shortest_path_answer(g: &ProjectedGraph, seeds: &[String]) -> String {
    if seeds.len() < 2 {
        return format!(
            "A shortest-path question needs two entities, but I could identify {}.",
            seeds.len()
        );
    }
    let a_key = format!("<{}>", seeds[0]);
    let b_key = format!("<{}>", seeds[1]);
    let (Some(&a), Some(&b)) = (g.id_of.get(&a_key), g.id_of.get(&b_key)) else {
        return "One or both entities are not present in the retrieved graph sample.".to_string();
    };
    match g.view.shortest_path(a, b) {
        Some(path) => {
            let steps: Vec<&str> =
                path.iter().map(|id| g.term_of[*id as usize].as_str()).collect();
            format!(
                "Shortest path ({} hop(s)): {}",
                path.len().saturating_sub(1),
                steps.join(" -> ")
            )
        }
        None => format!("No path found between {a_key} and {b_key} in the retrieved graph sample."),
    }
}

fn degree_centrality_answer(g: &ProjectedGraph, k: usize) -> String {
    let mut tot: HashMap<EntityLiteralId, usize> = g.view.all_out_degrees();
    for (id, d) in g.view.all_in_degrees() {
        *tot.entry(id).or_insert(0) += d;
    }
    let scored: Vec<(EntityLiteralId, f64)> = tot.into_iter().map(|(id, d)| (id, d as f64)).collect();
    render_ranking(scored, &g.term_of, "degree centrality", k)
}

fn components_answer(g: &ProjectedGraph) -> String {
    let (comp_map, num) = g.view.weakly_connected_components();
    let mut sizes: HashMap<EntityLiteralId, usize> = HashMap::new();
    for (_node, rep) in comp_map {
        *sizes.entry(rep).or_insert(0) += 1;
    }
    let largest = sizes.values().copied().max().unwrap_or(0);
    format!(
        "The graph has {num} weakly-connected component(s) over {} node(s); the largest has {largest} node(s).",
        g.view.node_count()
    )
}

/// Run graph analytics for `question` over the KB, seeded by linked entity URIs
/// (used only for shortest-path endpoints). Best-effort: retrieval/algorithm are
/// bounded by `cfg`.
pub fn run_analytics(
    kb: &dyn KbClient,
    question: &str,
    seeds: &[String],
    cfg: AnalyticsCfg,
) -> Result<AnalyticsResult> {
    let op = classify_op(question);
    let g = project(kb, cfg)?;
    let k = cfg.top_k.max(1);
    let text = match op {
        AnalyticsOp::ShortestPath => shortest_path_answer(&g, seeds),
        AnalyticsOp::PageRank => {
            let scores: Vec<(EntityLiteralId, f64)> =
                g.view.pagerank(0.85, 100, 1e-6).into_iter().collect();
            render_ranking(scores, &g.term_of, "PageRank", k)
        }
        AnalyticsOp::Centrality => degree_centrality_answer(&g, k),
        AnalyticsOp::Components => components_answer(&g),
        AnalyticsOp::Triangles => {
            format!("The graph sample contains {} triangle(s).", g.view.triangle_count())
        }
    };
    Ok(AnalyticsResult {
        text,
        op,
        node_count: g.view.node_count(),
        edge_count: g.view.edge_count(),
        truncated: g.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::{MockKb, RdfTerm, TermKind};

    fn t_uri(v: &str) -> RdfTerm {
        RdfTerm { kind: TermKind::Uri, value: v.into(), datatype: None, lang: None }
    }

    /// A directed chain A → B → C plus A → C, as a single ?s ?o SELECT answer.
    fn chain_kb() -> MockKb {
        let rows = vec![
            vec![Some(t_uri("http://ex/A")), Some(t_uri("http://ex/B"))],
            vec![Some(t_uri("http://ex/B")), Some(t_uri("http://ex/C"))],
            vec![Some(t_uri("http://ex/A")), Some(t_uri("http://ex/C"))],
        ];
        MockKb::new(vec![SparqlAnswer::Select { vars: vec!["s".into(), "o".into()], rows }])
    }

    #[test]
    fn classify_covers_each_op() {
        assert_eq!(classify_op("shortest path from A to B"), AnalyticsOp::ShortestPath);
        assert_eq!(classify_op("how many triangles are there?"), AnalyticsOp::Triangles);
        assert_eq!(classify_op("what communities exist?"), AnalyticsOp::Components);
        assert_eq!(classify_op("who is the most influential person?"), AnalyticsOp::PageRank);
        assert_eq!(classify_op("which node is most central?"), AnalyticsOp::Centrality);
        assert_eq!(classify_op("rank the nodes"), AnalyticsOp::PageRank); // default
    }

    #[test]
    fn shortest_path_over_projected_graph() {
        let kb = chain_kb();
        let res = run_analytics(
            &kb,
            "shortest path from A to C",
            &["http://ex/A".into(), "http://ex/C".into()],
            AnalyticsCfg::default(),
        )
        .unwrap();
        assert_eq!(res.op, AnalyticsOp::ShortestPath);
        // A -> C is a direct edge → 1 hop.
        assert!(res.text.contains("1 hop"), "text: {}", res.text);
        assert!(res.text.contains("<http://ex/A>") && res.text.contains("<http://ex/C>"));
    }

    #[test]
    fn shortest_path_needs_two_entities() {
        let kb = chain_kb();
        let res =
            run_analytics(&kb, "shortest path", &["http://ex/A".into()], AnalyticsCfg::default())
                .unwrap();
        assert!(res.text.contains("needs two entities"));
    }

    #[test]
    fn pagerank_and_centrality_rank_nodes() {
        let kb = chain_kb();
        let pr = run_analytics(&kb, "most influential node", &[], AnalyticsCfg::default()).unwrap();
        assert_eq!(pr.op, AnalyticsOp::PageRank);
        assert!(pr.text.starts_with("Top"));
        // C has the highest in-degree (from A and B) → should top PageRank.
        assert!(pr.text.contains("<http://ex/C>"));

        let cen =
            run_analytics(&kb, "which node is most central?", &[], AnalyticsCfg::default()).unwrap();
        assert_eq!(cen.op, AnalyticsOp::Centrality);
        assert!(cen.text.contains("degree centrality"));
    }

    #[test]
    fn components_and_triangles() {
        let kb = chain_kb();
        let comp =
            run_analytics(&kb, "how many connected components?", &[], AnalyticsCfg::default())
                .unwrap();
        assert_eq!(comp.op, AnalyticsOp::Components);
        assert!(comp.text.contains("1 weakly-connected component"));

        let tri = run_analytics(&kb, "count the triangles", &[], AnalyticsCfg::default()).unwrap();
        // A→B, B→C, A→C forms one directed triangle.
        assert!(tri.text.contains("1 triangle"));
    }

    #[test]
    fn truncated_flag_when_sample_hits_cap() {
        let kb = chain_kb();
        // cap of 3 with 3 rows → hit the cap → truncated.
        let res = run_analytics(
            &kb,
            "rank nodes",
            &[],
            AnalyticsCfg { max_edges: 3, top_k: 10 },
        )
        .unwrap();
        assert!(res.truncated);
    }
}
