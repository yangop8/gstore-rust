//! Path, reachability and BFS-family graph functions — literal ports of
//! gStore 1.4's algorithms with upstream-exact output.
//!
//! Each function's doc comment is its specification (re-checked against the
//! upstream code; where an earlier version of the spec was wrong the comment
//! says so). Conventions used in the specs (see the [module docs](super) for
//! the shared output contract):
//!
//! * "GE:n" = `src/Query/GeneralEvaluation.cpp` line n (upstream 1.4,
//!   `bebe1ea`); first the constant-argument branch (GE:1508-2033, taken when
//!   the WHERE produced no rows), then the variable branch (GE:2547-3107).
//!   Other files are relative to `src/Query/Algorithm/`.
//! * "entry" = one element of the `{"paths":[…]}` array; entries are written
//!   for the [`GArgs::pairs`] (or [`GArgs::distinct_srcs`]) in order,
//!   separated by `,`.
//! * `‹u›` = the printed form of a vertex ([`GFunCtx::gnode_str`]), e.g.
//!   `<http://ex/Alice>`; `‹p›` = a predicate's printed form.
//! * "path entry" = [`push_path_json`](super::fmt::push_path_json) of a
//!   [`GPath`](super::fmt::GPath); "empty-path entry" = the same with
//!   [`GPath::empty`](super::fmt::GPath::empty), i.e.
//!   `{"src":"‹u›","dst":"‹u›","edges":[],"nodes":[]}`.
//! * The upstream loops are ported **literally** (same predicate / direction
//!   interleaving — see [`traverse::NbrOrder`](super::traverse::NbrOrder) —,
//!   same queue discipline, same `std::set` / `std::map` (ascending) and
//!   `unordered_map` ([`CxxUnorderedOrder`](super::cxx::CxxUnorderedOrder))
//!   iteration orders): they decide which of several equal paths is printed.
//!   Predicates without an edge at the expanded vertex are skipped, which is
//!   invisible (every upstream loop over them is empty).
//! * `k' = if k < 0 { 999 } else { k }` is [`GArgs::hop_limit`]; upstream's
//!   `retNum < 0 ? default : retNum` is [`GArgs::ret_num_or`].
//! * Upstream vertex ids are entity ids; the algorithms here run on CSR
//!   [`Node`]s (same order, see [`csr`](crate::analytics::csr) "Node ids").
//!   "Negative" = upstream's `int uid < 0`: a [`GNode::MISSING`] argument (and,
//!   in principle, a literal id `>= 2^31`); `if (uid < 0)` guards are
//!   reproduced with [`GNode::upstream_id`]. "Isolated" = a non-negative id
//!   without a CSR node: it behaves as a vertex without edges. Each spec
//!   states the special cases.
//!
//! # Resource bounds and cancellation
//!
//! All loops poll [`GFunCtx::check_cancel`] (per pair / source and every few
//! thousand inner steps). The enumerating functions whose output can grow
//! exponentially and that upstream does not bound ([`k_hop_enumerate`] with
//! no or a negative `retNum`, [`shortest_paths`]) write their entries as they
//! find them and stop with a [`GStoreError::Query`] once a call produces more
//! than [`MAX_ENUMERATED_PATHS`] entries or [`MAX_ENUMERATED_BYTES`] of JSON,
//! instead of exhausting memory (the upstream server would); their search
//! time is not bounded (as upstream) beyond cancellation. Everything else is
//! polynomial: BFS-style functions are `O(V + E)` per source / pair (the
//! cycle / shortest-path primitives too), `kHopReachable` does at most a
//! constant number of vertex expansions (see its spec).
//!
//! # The shared shortest-path primitive
//!
//! `PathQueryHandler::shortestPath(uid, vid, directed, preds)`
//! (shortestPath.cpp:22-276) is used by `shortestPath`, `shortestPathLen`,
//! `kHopReachablePath` and directed `cyclePath` / `cycleBoolean`; it is
//! ported once ([`shortest_path_primitive`]):
//!
//! 1. `uid < 0 || vid < 0` (missing) → empty. (An isolated endpoint can never
//!    be met, so it yields empty as well.)
//! 2. Bidirectional BFS with distance maps `dis_u` (from `uid`) and `dis_v`
//!    (to `vid`). While no meet and either queue is non-empty: pop the
//!    **whole** current `u` frontier in FIFO order; for each vertex, for each
//!    predicate in filter order: its out-neighbours `t` — if `t ∈ dis_v` →
//!    meet at `t` (`dis_u[t] = dis_u[x] + 1`), stop everything; if `t ∈ dis_u`
//!    skip; else enqueue, `dis_u[t] = dis_u[x] + 1` —, then (undirected only)
//!    its in-neighbours the same way. Then the whole `v` frontier symmetrically
//!    with **in**-neighbours first and (undirected) out-neighbours second,
//!    meeting on `t ∈ dis_u`.
//! 3. Reconstruct from the meet node towards `vid`: repeatedly take, in
//!    predicate order, the first out-neighbour `t` with
//!    `dis_v[t] == dis_v[cur] - 1` (edge `pred`, forward), else (undirected)
//!    the first in-neighbour (edge reversed); then from the meet node back to
//!    `uid`: the first in-neighbour `t` with `dis_u[t] == dis_u[cur] - 1`
//!    (edge forward `t → cur`), else (undirected) the first out-neighbour
//!    (reversed). The result runs `uid → … → vid`.
//! 4. No meet → empty.

use std::collections::{HashMap, VecDeque};

use super::cxx::CxxUnorderedOrder;
use super::fmt::{push_json_quoted, push_path_json, GPath, JsonList, PathEdge};
use super::traverse::NodeMarks;
use super::{CsrGraph, GArgs, GFnKind, GFunCtx, GFunValue, GNode, Node, PredFilter};
use crate::error::{GStoreError, Result};
use crate::model::id::{EntityLiteralId, PredId};

/// The most path entries one call of an enumerating function
/// ([`k_hop_enumerate`], [`shortest_paths`]) may produce before it fails with
/// a [`GStoreError::Query`] (a resource guard; upstream has no bound and runs
/// out of memory instead).
pub const MAX_ENUMERATED_PATHS: usize = 1_000_000;

/// The most bytes of JSON one call of an enumerating function may produce
/// (the same kind of guard as [`MAX_ENUMERATED_PATHS`], for long paths).
pub const MAX_ENUMERATED_BYTES: usize = 256 << 20;

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// How many inner-loop steps pass between two cancellation polls.
const CANCEL_POLL_MASK: u32 = 0xFFF;

/// A cancellation poll that only looks at the flag every
/// `CANCEL_POLL_MASK + 1` calls.
struct Poll<'c, 'a> {
    ctx: &'c GFunCtx<'a>,
    n: u32,
}

impl<'c, 'a> Poll<'c, 'a> {
    fn new(ctx: &'c GFunCtx<'a>) -> Self {
        Poll { ctx, n: 0 }
    }

    #[inline]
    fn tick(&mut self) -> Result<()> {
        self.n = self.n.wrapping_add(1);
        if self.n & CANCEL_POLL_MASK == 0 {
            self.ctx.check_cancel()
        } else {
            Ok(())
        }
    }
}

/// The output guard of the enumerating functions: entries are written to the
/// result as they are found, and the call fails once it exceeds
/// [`MAX_ENUMERATED_PATHS`] entries or [`MAX_ENUMERATED_BYTES`] bytes.
struct OutputGuard {
    kind: GFnKind,
    max_paths: usize,
    max_bytes: usize,
    paths: usize,
}

impl OutputGuard {
    fn new(kind: GFnKind) -> Self {
        OutputGuard::with_limits(kind, MAX_ENUMERATED_PATHS, MAX_ENUMERATED_BYTES)
    }

    fn with_limits(kind: GFnKind, max_paths: usize, max_bytes: usize) -> Self {
        OutputGuard {
            kind,
            max_paths,
            max_bytes,
            paths: 0,
        }
    }

    /// Account for one more entry, already written to `list`.
    fn emitted(&mut self, list: &JsonList) -> Result<()> {
        self.paths += 1;
        if self.paths > self.max_paths || list.as_str().len() > self.max_bytes {
            return Err(GStoreError::Query(format!(
                "{}: more than {} paths or {} bytes of output; narrow the query{}",
                self.kind.name(),
                self.max_paths,
                self.max_bytes,
                if self.kind == GFnKind::KHopEnumerate {
                    " or pass a retNum"
                } else {
                    ""
                }
            )));
        }
        Ok(())
    }
}

/// Upstream's `if (uid < 0)` (a missing node, or an id that wraps negative
/// as `int`).
#[inline]
fn negative(n: &GNode) -> bool {
    n.upstream_id() < 0
}

/// The CSR node of a resolved argument; `None` for negative and isolated ids.
fn csr_node(ctx: &GFunCtx<'_>, n: &GNode) -> Option<Node> {
    if negative(n) {
        return None;
    }
    n.node.or_else(|| ctx.csr.node_of(n.entity))
}

/// A per-node map with `O(1)` reset (generation stamps); values of untouched
/// nodes read as absent. Integer value types get zero-initialised (lazily
/// mapped) storage, so a map over a large graph costs only the pages a search
/// touches.
#[derive(Debug, Clone)]
struct Dense<T> {
    stamp: Vec<u32>,
    val: Vec<T>,
    cur: u32,
}

impl<T: Copy + Default> Dense<T> {
    fn new(n: usize) -> Self {
        Dense {
            stamp: vec![0; n],
            val: vec![T::default(); n],
            cur: 1,
        }
    }

    fn reset(&mut self) {
        if self.cur == u32::MAX {
            self.stamp.iter_mut().for_each(|s| *s = 0);
            self.cur = 1;
        } else {
            self.cur += 1;
        }
    }

    #[inline]
    fn get(&self, x: Node) -> Option<T> {
        (self.stamp[x as usize] == self.cur).then(|| self.val[x as usize])
    }

    /// The value, or `T::default()` when absent (upstream `vector<int>(N, 0)`).
    #[inline]
    fn get_or_zero(&self, x: Node) -> T {
        self.get(x).unwrap_or_default()
    }

    #[inline]
    fn has(&self, x: Node) -> bool {
        self.stamp[x as usize] == self.cur
    }

    #[inline]
    fn set(&mut self, x: Node, v: T) {
        self.stamp[x as usize] = self.cur;
        self.val[x as usize] = v;
    }
}

/// `(parent, pred)` packed into a `u64` for [`Dense`] (zeroed storage).
#[inline]
fn pack(parent: Node, pred: PredId) -> u64 {
    (u64::from(parent) << 32) | u64::from(pred)
}

#[inline]
fn unpack(x: u64) -> (Node, PredId) {
    ((x >> 32) as Node, x as PredId)
}

/// The "no parent" marker of a search root in a packed parent map.
const ROOT: u64 = u64::MAX;

/// Per-predicate neighbour groups of one vertex: `(pred, out, in)` for each
/// predicate of the filter, **in filter order**, that labels at least one
/// requested edge of the vertex. `out` / `in` are ascending (upstream
/// `getOutVertID(v, pred, 0..getOutSize(v, pred))`); a direction that was not
/// requested is empty.
struct PredGroups<'g> {
    filter: &'g PredFilter,
    ascending: bool,
    // ascending mode: merge the vertex's (pred, nbr)-sorted adjacency runs
    op: &'g [PredId],
    on: &'g [Node],
    ip: &'g [PredId],
    inn: &'g [Node],
    i: usize,
    j: usize,
    // list mode: look each filter predicate up
    g: &'g CsrGraph,
    node: Node,
    k: usize,
    want_out: bool,
    want_in: bool,
}

fn groups<'g>(
    g: &'g CsrGraph,
    node: Node,
    filter: &'g PredFilter,
    want_out: bool,
    want_in: bool,
) -> PredGroups<'g> {
    let empty: (&[PredId], &[Node]) = (&[], &[]);
    let (op, on) = if want_out { g.out_edges(node) } else { empty };
    let (ip, inn) = if want_in { g.in_edges(node) } else { empty };
    PredGroups {
        filter,
        ascending: filter.is_ascending(),
        op,
        on,
        ip,
        inn,
        i: 0,
        j: 0,
        g,
        node,
        k: 0,
        want_out,
        want_in,
    }
}

impl<'g> Iterator for PredGroups<'g> {
    type Item = (PredId, &'g [Node], &'g [Node]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.ascending {
            loop {
                let p = match (self.op.get(self.i), self.ip.get(self.j)) {
                    (Some(&a), Some(&b)) => a.min(b),
                    (Some(&a), None) => a,
                    (None, Some(&b)) => b,
                    (None, None) => return None,
                };
                let i_end = self.i + self.op[self.i..].partition_point(|&x| x <= p);
                let j_end = self.j + self.ip[self.j..].partition_point(|&x| x <= p);
                let out = &self.on[self.i..i_end];
                let inn = &self.inn[self.j..j_end];
                self.i = i_end;
                self.j = j_end;
                if self.filter.contains(p) {
                    return Some((p, out, inn));
                }
            }
        } else {
            while let Some(&p) = self.filter.preds().get(self.k) {
                self.k += 1;
                let out = if self.want_out {
                    self.g.out_nbrs(self.node, p)
                } else {
                    &[]
                };
                let inn = if self.want_in {
                    self.g.in_nbrs(self.node, p)
                } else {
                    &[]
                };
                if !out.is_empty() || !inn.is_empty() {
                    return Some((p, out, inn));
                }
            }
            None
        }
    }
}

/// Upstream's `for (int d = directed; d <= 1; ++d) for (pred) …` neighbour
/// loops (bfsCount, kHopNeighbor, shortestPathCount): `first_in` → all
/// predicates' in-neighbours, then all predicates' out-neighbours
/// (`NbrOrder::AllInThenAllOut`); otherwise out first, then in. `directed`
/// keeps only the second pass. `f` receives every neighbour, once per edge.
fn for_each_two_pass<F: FnMut(Node)>(
    g: &CsrGraph,
    x: Node,
    preds: &PredFilter,
    directed: bool,
    second_is_out: bool,
    mut f: F,
) {
    // pass d = 0 (undirected only) uses the opposite direction of pass d = 1
    if !directed {
        for (_, out, inn) in groups(g, x, preds, !second_is_out, second_is_out) {
            for &n in if second_is_out { inn } else { out } {
                f(n);
            }
        }
    }
    for (_, out, inn) in groups(g, x, preds, second_is_out, !second_is_out) {
        for &n in if second_is_out { out } else { inn } {
            f(n);
        }
    }
}

/// Builds a [`GPath`] (entity ids) from CSR nodes and edges.
fn gpath(g: &CsrGraph, nodes: &[Node], edges: Vec<PathEdge>) -> GPath {
    GPath {
        nodes: nodes.iter().map(|&n| g.entity_of(n)).collect(),
        edges,
    }
}

#[inline]
fn fwd(pred: PredId) -> PathEdge {
    PathEdge {
        pred,
        reversed: false,
    }
}

#[inline]
fn rev(pred: PredId) -> PathEdge {
    PathEdge {
        pred,
        reversed: true,
    }
}

/// `{"src":"‹u›"` + `sep` + `"dst":"‹v›"`.
fn push_src_dst(s: &mut String, ctx: &GFunCtx<'_>, u: &GNode, sep: &str, v: EntityLiteralId) {
    s.push_str("{\"src\":");
    push_json_quoted(s, ctx.gnode_str(u));
    s.push_str(sep);
    s.push_str("\"dst\":");
    push_json_quoted(s, ctx.entity_str(v));
}

// ---------------------------------------------------------------------------
// the shared shortest-path primitive (shortestPath.cpp:22-276)
// ---------------------------------------------------------------------------

/// Scratch space of [`shortest_path_primitive`], reused across pairs.
struct SpScratch {
    du: Dense<u32>,
    dv: Dense<u32>,
}

impl SpScratch {
    fn new(g: &CsrGraph) -> Self {
        SpScratch {
            du: Dense::new(g.num_nodes()),
            dv: Dense::new(g.num_nodes()),
        }
    }
}

/// `PathQueryHandler::shortestPath(u, v, directed, preds)` on CSR nodes (see
/// the module docs). Callers map missing / isolated endpoints to `None`.
fn shortest_path_primitive(
    g: &CsrGraph,
    sc: &mut SpScratch,
    poll: &mut Poll<'_, '_>,
    u: Node,
    v: Node,
    directed: bool,
    preds: &PredFilter,
) -> Result<Option<GPath>> {
    let SpScratch { du, dv } = sc;
    du.reset();
    dv.reset();
    du.set(u, 0);
    dv.set(v, 0);
    let mut qu = vec![u];
    let mut qv = vec![v];
    let mut meet: Option<Node> = None;
    while meet.is_none() && (!qu.is_empty() || !qv.is_empty()) {
        let mut next = Vec::new();
        'u: for &x in &qu {
            poll.tick()?;
            let dx = du.get(x).unwrap_or(0);
            for (_, out, inn) in groups(g, x, preds, true, !directed) {
                for &t in out.iter().chain(inn) {
                    if dv.has(t) {
                        du.set(t, dx + 1);
                        meet = Some(t);
                        break 'u;
                    }
                    if du.has(t) {
                        continue;
                    }
                    next.push(t);
                    du.set(t, dx + 1);
                }
            }
        }
        qu = next;
        if meet.is_some() {
            break;
        }
        let mut next = Vec::new();
        'v: for &x in &qv {
            poll.tick()?;
            let dx = dv.get(x).unwrap_or(0);
            for (_, out, inn) in groups(g, x, preds, !directed, true) {
                for &t in inn.iter().chain(out) {
                    if du.has(t) {
                        dv.set(t, dx + 1);
                        meet = Some(t);
                        break 'v;
                    }
                    if dv.has(t) {
                        continue;
                    }
                    next.push(t);
                    dv.set(t, dx + 1);
                }
            }
        }
        qv = next;
    }
    let Some(meet) = meet else {
        return Ok(None);
    };

    // meet → v: first out-neighbour (forward), else in-neighbour (reversed)
    // one step closer to v, per predicate in filter order.
    let mut tail: Vec<(PathEdge, Node)> = Vec::new();
    let mut cur = meet;
    while cur != v {
        let Some(want) = dv.get(cur).and_then(|d| d.checked_sub(1)) else {
            return Ok(None); // unreachable: every dis_v vertex has a parent
        };
        let mut step = None;
        'f: for (p, out, inn) in groups(g, cur, preds, true, !directed) {
            for &t in out {
                if dv.get(t) == Some(want) {
                    step = Some((fwd(p), t));
                    break 'f;
                }
            }
            for &t in inn {
                if dv.get(t) == Some(want) {
                    step = Some((rev(p), t));
                    break 'f;
                }
            }
        }
        let Some((e, t)) = step else {
            return Ok(None);
        };
        tail.push((e, t));
        cur = t;
    }
    // meet → u: first in-neighbour (edge t → cur, forward), else out-neighbour
    // (edge cur → t, reversed).
    let mut head: Vec<(PathEdge, Node)> = Vec::new();
    cur = meet;
    while cur != u {
        let Some(want) = du.get(cur).and_then(|d| d.checked_sub(1)) else {
            return Ok(None);
        };
        let mut step = None;
        'b: for (p, out, inn) in groups(g, cur, preds, !directed, true) {
            for &t in inn {
                if du.get(t) == Some(want) {
                    step = Some((fwd(p), t));
                    break 'b;
                }
            }
            for &t in out {
                if du.get(t) == Some(want) {
                    step = Some((rev(p), t));
                    break 'b;
                }
            }
        }
        let Some((e, t)) = step else {
            return Ok(None);
        };
        head.push((e, t));
        cur = t;
    }
    let mut nodes: Vec<Node> = head.iter().rev().map(|&(_, t)| t).collect();
    nodes.push(meet);
    nodes.extend(tail.iter().map(|&(_, t)| t));
    let edges = head
        .iter()
        .rev()
        .map(|&(e, _)| e)
        .chain(tail.iter().map(|&(e, _)| e))
        .collect();
    Ok(Some(gpath(g, &nodes, edges)))
}

/// The primitive on resolved arguments (`None` for negative / isolated ends).
fn shortest_path_args(
    ctx: &GFunCtx<'_>,
    sc: &mut SpScratch,
    poll: &mut Poll<'_, '_>,
    u: &GNode,
    v: &GNode,
    directed: bool,
    preds: &PredFilter,
) -> Result<Option<GPath>> {
    match (csr_node(ctx, u), csr_node(ctx, v)) {
        (Some(un), Some(vn)) => shortest_path_primitive(ctx.csr, sc, poll, un, vn, directed, preds),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// cycles (cycle.cpp)
// ---------------------------------------------------------------------------

/// Scratch space of [`cycle`].
struct CycleScratch {
    sp: SpScratch,
    ru: Dense<u64>,
    rv: Dense<u64>,
}

impl CycleScratch {
    fn new(g: &CsrGraph) -> Self {
        CycleScratch {
            sp: SpScratch::new(g),
            ru: Dense::new(g.num_nodes()),
            rv: Dense::new(g.num_nodes()),
        }
    }
}

/// `PathQueryHandler::shortestPath0(u, v, false, preds)` (cycle.cpp:20-176):
/// bidirectional BFS keeping a route per vertex (here: parent pointers), every
/// edge recorded with its **plain** predicate id — also edges traversed
/// against their direction. Returns the vertex sequence and predicates.
fn shortest_path0(
    g: &CsrGraph,
    sc: &mut CycleScratch,
    poll: &mut Poll<'_, '_>,
    u: Node,
    v: Node,
    directed: bool,
    preds: &PredFilter,
) -> Result<Option<(Vec<Node>, Vec<PredId>)>> {
    let CycleScratch { ru, rv, .. } = sc;
    ru.reset();
    rv.reset();
    ru.set(u, ROOT);
    rv.set(v, ROOT);
    let mut qu = vec![u];
    let mut qv = vec![v];
    // (meet node, expanded vertex, pred, found from the u side)
    let mut meet: Option<(Node, Node, PredId, bool)> = None;
    while meet.is_none() && (!qu.is_empty() || !qv.is_empty()) {
        let mut next = Vec::new();
        'u: for &x in &qu {
            poll.tick()?;
            for (p, out, inn) in groups(g, x, preds, true, !directed) {
                for &t in out.iter().chain(inn) {
                    if rv.has(t) {
                        meet = Some((t, x, p, true));
                        break 'u;
                    }
                    if ru.has(t) {
                        continue;
                    }
                    next.push(t);
                    ru.set(t, pack(x, p));
                }
            }
        }
        qu = next;
        if meet.is_some() {
            break;
        }
        let mut next = Vec::new();
        'v: for &x in &qv {
            poll.tick()?;
            for (p, out, inn) in groups(g, x, preds, !directed, true) {
                for &t in inn.iter().chain(out) {
                    if ru.has(t) {
                        meet = Some((t, x, p, false));
                        break 'v;
                    }
                    if rv.has(t) {
                        continue;
                    }
                    next.push(t);
                    rv.set(t, pack(x, p));
                }
            }
        }
        qv = next;
    }
    let Some((t, x, p, from_u)) = meet else {
        return Ok(None);
    };
    // route_u[a] = [u, …, a] (parent chain reversed); route_v[b] reversed =
    // [b, …, v] (parent chain as is).
    let route_u = |a: Node| {
        let mut nodes = vec![a];
        let mut ps = Vec::new();
        let mut c = a;
        while let Some(packed) = ru.get(c).filter(|&r| r != ROOT) {
            let (par, pr) = unpack(packed);
            ps.push(pr);
            nodes.push(par);
            c = par;
        }
        nodes.reverse();
        ps.reverse();
        (nodes, ps)
    };
    let route_v_rev = |b: Node| {
        let mut nodes = vec![b];
        let mut ps = Vec::new();
        let mut c = b;
        while let Some(packed) = rv.get(c).filter(|&r| r != ROOT) {
            let (par, pr) = unpack(packed);
            ps.push(pr);
            nodes.push(par);
            c = par;
        }
        (nodes, ps)
    };
    // u side: route_u[x] + [p] + reverse(route_v[t]);
    // v side: route_u[t] + reverse(route_v[x] + [p]).
    let (head, tail) = if from_u { (x, t) } else { (t, x) };
    let (mut nodes, mut ps) = route_u(head);
    let (tn, tp) = route_v_rev(tail);
    ps.push(p);
    ps.extend(tp);
    nodes.extend(tn);
    Ok(Some((nodes, ps)))
}

/// `PathQueryHandler::cycle(u, v, directed, preds)` (cycle.cpp:192-311) on
/// resolved, distinct arguments.
fn cycle(
    ctx: &GFunCtx<'_>,
    sc: &mut CycleScratch,
    poll: &mut Poll<'_, '_>,
    u: &GNode,
    v: &GNode,
    directed: bool,
    preds: &PredFilter,
) -> Result<Option<GPath>> {
    let g = ctx.csr;
    let (Some(un), Some(vn)) = (csr_node(ctx, u), csr_node(ctx, v)) else {
        // directed: shortestPath's `uid < 0` guard; undirected: a vertex
        // without edges is never met. Either way: no cycle.
        return Ok(None);
    };
    if directed {
        let Some(p1) = shortest_path_primitive(g, &mut sc.sp, poll, un, vn, true, preds)? else {
            return Ok(None);
        };
        let Some(p2) = shortest_path_primitive(g, &mut sc.sp, poll, vn, un, true, preds)? else {
            return Ok(None);
        };
        // ans1 + ans2[1 .. len-1) + [uid] == ans1 + ans2 without its first node
        let mut c = p1;
        c.nodes.extend_from_slice(&p2.nodes[1..]);
        c.edges.extend(p2.edges);
        Ok(Some(c))
    } else {
        let Some((nodes, ps)) = shortest_path0(g, sc, poll, un, vn, false, preds)? else {
            return Ok(None);
        };
        // ans + ans[len-2], ans[len-3], …, ans[1] + [uid]: walk the path back;
        // every edge keeps its plain predicate id, so all print forward.
        let mut all = nodes.clone();
        all.extend(nodes[..nodes.len() - 1].iter().rev());
        let edges = ps.iter().chain(ps.iter().rev()).map(|&p| fwd(p)).collect();
        Ok(Some(gpath(g, &all, edges)))
    }
}

/// `simpleCyclePath(u, v, directed, preds)`.
///
/// **Upstream:** parsed (QueryParser.cpp:411-412) but never evaluated: no
/// branch of GE:1619-2016 / GE:2673-3094 handles `simpleCyclePath_type`, and
/// `PathQueryHandler::simpleCycle` is commented out
/// (PathQueryHandler.cpp:109-122).
///
/// **Output:** always `{"paths":[]}` ([`GFunValue::Json`]), whatever the
/// arguments. There is no simple-cycle search: there is no upstream output to
/// match.
pub fn simple_cycle_path(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let _ = a;
    ctx.check_cancel()?;
    Ok(GFunValue::Json(JsonList::new().into_paths()))
}

/// `simpleCycleBoolean(u, v, directed, preds)`.
///
/// **Upstream:** like [`simple_cycle_path`] — parsed, never evaluated. Only
/// `cycleBoolean_type` is turned into an `xsd:boolean` (GE:2021, GE:3099), so
/// this returns the JSON envelope, not a boolean.
///
/// **Output:** always `{"paths":[]}` ([`GFunValue::Json`]).
pub fn simple_cycle_boolean(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    simple_cycle_path(ctx, a)
}

/// `cyclePath(u, v, directed, preds)` — a cycle through `u` and `v`.
///
/// **Upstream:** GE:1619-1632 / GE:2673-2686; `PathQueryHandler::cycle`
/// (cycle.cpp:192-311).
///
/// **Per pair** `(u, v)`: `u == v` → no entry (skipped); otherwise compute
/// `cycle(u, v)` and, if non-empty, write the path entry with `src = u`,
/// `dst = v`. Pairs without a cycle produce nothing.
///
/// **cycle(u, v):**
/// * directed: `p1 = shortestPath(u, v, true)`, `p2 = shortestPath(v, u, true)`
///   (the shared primitive); either empty → none; else `p1` followed by `p2`
///   without its first and last element, then `u`: the cycle
///   `u → … → v → … → u` (all edges forward).
/// * undirected: `p = shortestPath0(u, v, false)` (cycle.cpp:20-176 — a
///   *different* bidirectional BFS that keeps whole routes per vertex and
///   records every edge with its **plain predicate id**, also for edges
///   traversed against their direction; its meet/join logic is cycle.cpp:44-175);
///   empty → none; else `p` + `p[len-2], p[len-3], …, p[1]` + `u`
///   (cycle.cpp:302-309): the same path walked back. Consequently every edge
///   prints as a forward edge (`fromNode` = earlier node), and any connected
///   pair has a "cycle" (a single edge `u – v` yields `u → v → u`) — upstream
///   quirks, reproduced.
///
/// **Missing / isolated:** no path → no entry.
///
/// **Example** (SPARQL_QUERY.md, sample social data):
/// `cyclePath(?x, <Carol>, true, {<like>})` with `?x ∈ {<Dave>, <Eve>}` →
/// `{"paths":[{"src":"<Eve>","dst":"<Carol>","edges":[{"fromNode":2,"toNode":3,"predIRI":"<like>"},{"fromNode":3,"toNode":1,"predIRI":"<like>"},{"fromNode":1,"toNode":2,"predIRI":"<like>"}],"nodes":[{"nodeIndex":1,"nodeIRI":"<Bob>"},{"nodeIndex":3,"nodeIRI":"<Carol>"},{"nodeIndex":2,"nodeIRI":"<Eve>"}]}]}`
pub fn cycle_path(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let mut sc = CycleScratch::new(ctx.csr);
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for (u, v) in a.pairs() {
        ctx.check_cancel()?;
        if u.entity == v.entity {
            continue;
        }
        if let Some(p) = cycle(ctx, &mut sc, &mut poll, &u, &v, a.directed, &a.preds)? {
            push_path_json(list.item(), ctx, u.entity, v.entity, &p);
        }
    }
    Ok(GFunValue::Json(list.into_paths()))
}

/// `cycleBoolean(u, v, directed, preds)` — is there a cycle through `u`
/// and `v`?
///
/// **Upstream:** GE:1633-1643 / GE:2687-2697, result conversion GE:2021-2027 /
/// GE:3099-3105.
///
/// Iterate the pairs; skip `u == v`; the first pair whose [`cycle_path`]
/// cycle is non-empty makes the result `true` and stops the iteration.
///
/// **Output:** [`GFunValue::Bool`] (`"true"/"false"^^xsd:boolean`); no pairs
/// → `false`. (The upstream docs print the datatype as `XMLSchema#` — a doc
/// typo; the code emits `XMLSchema#boolean`.)
pub fn cycle_boolean(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let mut sc = CycleScratch::new(ctx.csr);
    let mut poll = Poll::new(ctx);
    for (u, v) in a.pairs() {
        ctx.check_cancel()?;
        if u.entity == v.entity {
            continue;
        }
        if cycle(ctx, &mut sc, &mut poll, &u, &v, a.directed, &a.preds)?.is_some() {
            return Ok(GFunValue::Bool(true));
        }
    }
    Ok(GFunValue::Bool(false))
}

// ---------------------------------------------------------------------------
// shortest paths
// ---------------------------------------------------------------------------

/// `shortestPath(u, v, directed, preds)` — one shortest path.
///
/// **Upstream:** GE:1644-1665 / GE:2698-2719; the shared primitive (module
/// docs, shortestPath.cpp:22-276).
///
/// **Per pair:** `u == v` (same entity, missing included) → the empty-path
/// entry; otherwise the primitive's path, if non-empty, as a path entry
/// (`src = u`, `dst = v`; edges traversed against their direction print
/// reversed `fromNode`/`toNode`). Unreachable → no entry. Missing `u` or `v`
/// → no entry (primitive guard).
///
/// **Example** (SPARQL_QUERY.md): `shortestPath(<Francis>, ?x, true, {<like>, <focus>})`
/// with `?x = <Alice>` →
/// `{"paths":[{"src":"<Francis>","dst":"<Alice>","edges":[{"fromNode":4,"toNode":3,"predIRI":"<like>"},{"fromNode":3,"toNode":1,"predIRI":"<like>"},{"fromNode":1,"toNode":0,"predIRI":"<focus>"}],"nodes":[{"nodeIndex":0,"nodeIRI":"<Alice>"},{"nodeIndex":1,"nodeIRI":"<Bob>"},{"nodeIndex":3,"nodeIRI":"<Carol>"},{"nodeIndex":4,"nodeIRI":"<Francis>"}]}]}`
pub fn shortest_path(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let mut sc = SpScratch::new(ctx.csr);
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for (u, v) in a.pairs() {
        ctx.check_cancel()?;
        if u.entity == v.entity {
            push_path_json(list.item(), ctx, u.entity, v.entity, &GPath::empty());
            continue;
        }
        if let Some(p) = shortest_path_args(ctx, &mut sc, &mut poll, &u, &v, a.directed, &a.preds)?
        {
            push_path_json(list.item(), ctx, u.entity, v.entity, &p);
        }
    }
    Ok(GFunValue::Json(list.into_paths()))
}

/// `shortestPathLen(u, v, directed, preds)` — the shortest-path length.
///
/// **Upstream:** GE:1720-1745 / GE:2720-2745.
///
/// **Per pair:** `u == v` → `{"src":"‹u›","dst":"‹u›","length":0}`; otherwise
/// the shared primitive's path; if non-empty →
/// `{"src":"‹u›","dst":"‹v›","length":L}` with `L` = number of edges;
/// unreachable / missing → no entry.
///
/// **Example:** `shortestPathLen(<Francis>, ?x, true, {<like>, <focus>})` →
/// `{"paths":[{"src":"<Francis>","dst":"<Alice>","length":3}]}`.
pub fn shortest_path_len(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let mut sc = SpScratch::new(ctx.csr);
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for (u, v) in a.pairs() {
        ctx.check_cancel()?;
        let len = if u.entity == v.entity {
            0
        } else {
            match shortest_path_args(ctx, &mut sc, &mut poll, &u, &v, a.directed, &a.preds)? {
                Some(p) => p.len(),
                None => continue,
            }
        };
        let s = list.item();
        push_src_dst(s, ctx, &u, ",", v.entity);
        s.push_str(&format!(",\"length\":{len}}}"));
    }
    Ok(GFunValue::Json(list.into_paths()))
}

/// Scratch space of [`shortest_path_count_nodes`].
struct CountScratch {
    scnt: Dense<i32>,
    tcnt: Dense<i32>,
    sd: Dense<i32>,
    td: Dense<i32>,
}

/// `PathQueryHandler::shortestPathCount` (shortestPathCount.cpp:15-163) on CSR
/// nodes; C `int` arithmetic (wrapping).
fn shortest_path_count_nodes(
    g: &CsrGraph,
    sc: &mut CountScratch,
    poll: &mut Poll<'_, '_>,
    s: Node,
    t: Node,
    directed: bool,
    preds: &PredFilter,
) -> Result<i32> {
    let CountScratch { scnt, tcnt, sd, td } = sc;
    for d in [&mut *scnt, &mut *tcnt, &mut *sd, &mut *td] {
        d.reset();
    }
    scnt.set(s, 1);
    tcnt.set(t, 1);
    sd.set(s, 0);
    td.set(t, 0);
    let mut q = vec![s];
    let mut tq = vec![t];
    let mut joint: Vec<Node> = Vec::new();
    let mut meet = false;
    while !meet && !q.is_empty() && !tq.is_empty() {
        // Expand the smaller frontier; ties go to the target side.
        let source_side = q.len() < tq.len();
        let (front, cnt, dep, other) = if source_side {
            (&mut q, &mut *scnt, &mut *sd, &*tcnt)
        } else {
            (&mut tq, &mut *tcnt, &mut *td, &*scnt)
        };
        let mut next = Vec::new();
        for &v in front.iter() {
            poll.tick()?;
            let vcnt = cnt.get_or_zero(v);
            let vd = dep.get_or_zero(v);
            // source side: (in, then) out; target side: (out, then) in
            for_each_two_pass(g, v, preds, directed, source_side, |n| {
                if cnt.get_or_zero(n) == 0 {
                    if other.get_or_zero(n) != 0 {
                        meet = true;
                        joint.push(n);
                    }
                    if !meet {
                        next.push(n);
                    }
                    cnt.set(n, vcnt);
                    dep.set(n, vd.wrapping_add(1));
                } else if dep.get_or_zero(n) == vd.wrapping_add(1) {
                    cnt.set(n, cnt.get_or_zero(n).wrapping_add(vcnt));
                }
            });
        }
        *front = next;
    }
    if !meet {
        return Ok(0);
    }
    Ok(joint.iter().fold(0i32, |acc, &j| {
        acc.wrapping_add(scnt.get_or_zero(j).wrapping_mul(tcnt.get_or_zero(j)))
    }))
}

/// `shortestPathCount(u, v, directed, preds)` — the number of shortest paths.
///
/// **Upstream:** GE:1954-1960 / GE:3028-3038; shortestPathCount.cpp:15-163.
///
/// **Per pair** (every pair, also `u == v` and unreachable ones):
/// `{"src":"‹u›", "dst":"‹v›", "count":N}` — note the spaces after the
/// commas, exactly as upstream writes them.
///
/// **N:** a level-synchronous bidirectional BFS with per-vertex path counts
/// `scnt` / `tcnt` and depths `sd` / `td`. Each round expands the smaller
/// frontier (`qsz < tqsz` → source side, otherwise — ties included — the
/// target side). Expanding a vertex `x` (count `c`, depth `d`): directed →
/// source side follows out-edges, target side in-edges; undirected → all
/// predicates' in-edges first, then all predicates' out-edges
/// (`NbrOrder::AllInThenAllOut`; the target side mirrors it: out first,
/// then in). Each edge is processed separately (multi-edges of different
/// predicates — and, undirected, the two directions of one predicate — count
/// as different paths): a fresh neighbour `n` (`scnt[n] == 0`) gets
/// `scnt[n] = c`, `sd[n] = d + 1`, and if it was already reached by the other
/// side it is a *joint* (meet; the current round still finishes but nothing
/// more is enqueued); a neighbour at depth `d + 1` gets `scnt[n] += c`. Result
/// `Σ_joint scnt[j] · tcnt[j]`, computed in C `int` (wrap like `i32`), `0`
/// without a meet. For `u ≠ v` this is exactly the number of shortest
/// edge sequences.
///
/// **`u == v`:** both sides start marked at the same vertex, so it can never
/// be a joint; the result is `0` unless the search meets elsewhere — e.g. when
/// the target side's frontier outgrows the source side's and the source side
/// then reaches a vertex on a cycle through `u` (`u → a → u` plus a second
/// in-neighbour of `u` → 1). (An earlier version of this spec claimed "always
/// 0"; the code is ported literally instead.)
///
/// **Missing `u` / `v`:** upstream indexes its arrays with `-1` (undefined
/// behaviour) → no entry. **Isolated** → `count` 0.
pub fn shortest_path_count(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let n = ctx.csr.num_nodes();
    let mut sc = CountScratch {
        scnt: Dense::new(n),
        tcnt: Dense::new(n),
        sd: Dense::new(n),
        td: Dense::new(n),
    };
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for (u, v) in a.pairs() {
        ctx.check_cancel()?;
        if negative(&u) || negative(&v) {
            continue;
        }
        let count = match (csr_node(ctx, &u), csr_node(ctx, &v)) {
            (Some(s), Some(t)) => {
                shortest_path_count_nodes(ctx.csr, &mut sc, &mut poll, s, t, a.directed, &a.preds)?
            }
            // a vertex without edges: one side's frontier empties at once
            _ => 0,
        };
        let s = list.item();
        push_src_dst(s, ctx, &u, ", ", v.entity);
        s.push_str(&format!(", \"count\":{count}}}"));
    }
    Ok(GFunValue::Json(list.into_paths()))
}

/// Scratch space of [`all_shortest_paths`], reused across pairs.
struct AspScratch {
    dis: Dense<u32>,
    visited: NodeMarks,
    pre: HashMap<Node, Vec<Node>>,
    nl: Vec<Node>,
}

/// `PathQueryHandler::kHopShortestPaths(u, v, directed, preds)` +
/// `djstl_dfs` (kHopShortestPaths.cpp:5-194) for `u != v` on CSR nodes,
/// writing one path entry per path (`src = ue`, `dst = ve`) as it is found.
#[allow(clippy::too_many_arguments)]
fn all_shortest_paths(
    ctx: &GFunCtx<'_>,
    sc: &mut AspScratch,
    poll: &mut Poll<'_, '_>,
    guard: &mut OutputGuard,
    list: &mut JsonList,
    (u, ue): (Node, EntityLiteralId),
    (v, ve): (Node, EntityLiteralId),
    directed: bool,
    preds: &PredFilter,
) -> Result<()> {
    let g = ctx.csr;
    let AspScratch {
        dis,
        visited,
        pre,
        nl,
    } = sc;
    dis.reset();
    visited.clear();
    pre.clear();
    // BFS from u over everything reachable (:36-109): `dis_pre[t]` collects
    // the distinct predecessors at shortest distance in discovery order; the
    // neighbours of an expanded vertex form a `std::set` whose unvisited
    // members are enqueued ascending.
    let mut q = VecDeque::from([u]);
    dis.set(u, 0);
    visited.mark(u);
    while let Some(x) = q.pop_front() {
        poll.tick()?;
        let distance = dis.get_or_zero(x) + 1;
        nl.clear();
        for (_, outs, ins) in groups(g, x, preds, true, !directed) {
            for &t in outs.iter().chain(ins) {
                nl.push(t);
                match dis.get(t) {
                    None => {
                        dis.set(t, distance);
                        pre.entry(t).or_default().push(x);
                    }
                    // (`dis_u[t] > distance` cannot happen in a FIFO BFS.)
                    Some(d) if d == distance => {
                        // `x` can only be the latest entry: entries are
                        // appended while their vertex is being expanded.
                        let l = pre.entry(t).or_default();
                        if l.last() != Some(&x) {
                            l.push(x);
                        }
                    }
                    Some(_) => {}
                }
            }
        }
        nl.sort_unstable();
        nl.dedup();
        for &m in nl.iter() {
            if visited.mark(m) {
                q.push_back(m);
            }
        }
    }

    // djstl_dfs: depth-first from v back to u over the predecessor lists, in
    // list order; every complete vertex sequence is fanned out right away.
    let mut stack: Vec<(Node, usize)> = vec![(v, 0)];
    let mut seq: Vec<Node> = Vec::new();
    while let Some(&mut (x, ref mut i)) = stack.last_mut() {
        poll.tick()?;
        if x == u {
            seq.clear();
            seq.extend(stack.iter().rev().map(|&(n, _)| n));
            fan_out(ctx, poll, guard, list, &seq, ue, ve, directed, preds)?;
            stack.pop();
            continue;
        }
        match pre.get(&x).and_then(|l| l.get(*i)) {
            Some(&p) => {
                *i += 1;
                stack.push((p, 0));
            }
            None => {
                stack.pop();
            }
        }
    }
    Ok(())
}

/// The predicate fan-out of one shortest vertex sequence `u … v`
/// (kHopShortestPaths.cpp:115-178): the candidates of a hop `x → y` are the
/// predicates `p` with `x -p-> y` or (undirected) `y -p-> x`, ascending
/// (`std::set`), recorded **forward** (plain id) either way. Upstream extends
/// its path list in place with the first candidate and appends copies of the
/// pre-hop list for each further one, so the first hop's choice varies
/// fastest; the combinations are generated lazily in that order.
#[allow(clippy::too_many_arguments)]
fn fan_out(
    ctx: &GFunCtx<'_>,
    poll: &mut Poll<'_, '_>,
    guard: &mut OutputGuard,
    list: &mut JsonList,
    seq: &[Node],
    ue: EntityLiteralId,
    ve: EntityLiteralId,
    directed: bool,
    preds: &PredFilter,
) -> Result<()> {
    let g = ctx.csr;
    let mut cands: Vec<Vec<PredId>> = Vec::with_capacity(seq.len().saturating_sub(1));
    for w in seq.windows(2) {
        let (x, y) = (w[0], w[1]);
        let mut c: Vec<PredId> = groups(g, x, preds, true, !directed)
            .filter(|(_, outs, ins)| {
                outs.binary_search(&y).is_ok() || ins.binary_search(&y).is_ok()
            })
            .map(|(p, _, _)| p)
            .collect();
        c.sort_unstable();
        // `y` was discovered from `x` over an allowed edge, so `c` is never
        // empty.
        cands.push(c);
    }
    let mut path = GPath {
        nodes: seq.iter().map(|&n| g.entity_of(n)).collect(),
        edges: cands.iter().map(|c| fwd(c[0])).collect(),
    };
    let mut idx = vec![0usize; cands.len()];
    loop {
        poll.tick()?;
        push_path_json(list.item(), ctx, ue, ve, &path);
        guard.emitted(list)?;
        // odometer step, hop 0 fastest
        let mut h = 0;
        loop {
            let Some(i) = idx.get_mut(h) else {
                return Ok(());
            };
            *i += 1;
            if *i < cands[h].len() {
                path.edges[h] = fwd(cands[h][*i]);
                break;
            }
            *i = 0;
            path.edges[h] = fwd(cands[h][0]);
            h += 1;
        }
    }
}

/// `shortestPaths(u, v, directed, preds)` — all shortest paths.
///
/// **Upstream:** GE:2008-2011 / GE:3086-3089 → wrapper
/// `GeneralEvaluation::kHopShortestPaths` (GE:4134-4159);
/// `PathQueryHandler::kHopShortestPaths` + `djstl_dfs`
/// (kHopShortestPaths.cpp:5-194).
///
/// **Per pair:** negative `u` or `v` → nothing (the wrapper's guard comes
/// first, so two missing vertices give nothing either); `u == v` → the
/// empty-path entry; otherwise one path entry per path, in this order:
///
/// 1. BFS from `u` over everything reachable (out-edges, plus in-edges when
///    undirected, per predicate in filter order), recording for every vertex
///    the list `dis_pre[t]` of *distinct* predecessors at shortest distance,
///    in the order they were found. All neighbours of an expanded vertex are
///    collected in a `std::set`; its not yet visited members are enqueued
///    **ascending**.
/// 2. Depth-first from `v` back to `u` over `dis_pre` (in list order) yields
///    vertex sequences `u … v` (`v` unreachable / isolated → none).
/// 3. Each vertex sequence fans out into one path per combination of
///    predicates: for every hop `x → y` the candidate predicates are those
///    with `x -p-> y` or (undirected) `y -p-> x`, ascending (`std::set`). The
///    first candidate extends the current path list in place; each further
///    candidate appends copies of the pre-hop list extended with it
///    (kHopShortestPaths.cpp:141-171), so the first hop's predicate varies
///    fastest. All edges are recorded **forward** (plain predicate id) even
///    when they were in-edges — reproduced. Unlike `shortestPathCount`, the
///    two directions of one predicate between the same vertices count once.
///
/// The number of paths can be exponential and upstream does not bound it:
/// more than [`MAX_ENUMERATED_PATHS`] entries (or [`MAX_ENUMERATED_BYTES`]
/// of output) in one call → error.
pub fn shortest_paths(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    shortest_paths_guarded(ctx, a, &mut OutputGuard::new(GFnKind::ShortestPaths))
}

fn shortest_paths_guarded(
    ctx: &GFunCtx<'_>,
    a: &GArgs,
    guard: &mut OutputGuard,
) -> Result<GFunValue> {
    let g = ctx.csr;
    let mut sc = AspScratch {
        dis: Dense::new(g.num_nodes()),
        visited: NodeMarks::new(g.num_nodes()),
        pre: HashMap::new(),
        nl: Vec::new(),
    };
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for (u, v) in a.pairs() {
        ctx.check_cancel()?;
        if negative(&u) || negative(&v) {
            continue;
        }
        if u.entity == v.entity {
            push_path_json(list.item(), ctx, u.entity, v.entity, &GPath::empty());
            guard.emitted(&list)?;
            continue;
        }
        let (Some(un), Some(vn)) = (csr_node(ctx, &u), csr_node(ctx, &v)) else {
            continue;
        };
        all_shortest_paths(
            ctx,
            &mut sc,
            &mut poll,
            guard,
            &mut list,
            (un, u.entity),
            (vn, v.entity),
            a.directed,
            &a.preds,
        )?;
    }
    Ok(GFunValue::Json(list.into_paths()))
}

// ---------------------------------------------------------------------------
// k-hop reachability (kHopReachable.cpp)
// ---------------------------------------------------------------------------

/// A vertex inside `kHopReachable`: a CSR node, or [`SENTINEL`].
type Vid = i64;

/// `getOutVertID(v, pred, i)` for `i` past the out-degree (and every lookup on
/// the sentinel itself): upstream's `-1`, a pseudo-vertex without edges.
const SENTINEL: Vid = -1;

/// C `int / int` with a defined result for a zero divisor (upstream raises
/// `SIGFPE` there and the server dies; only reachable in the push phase).
#[inline]
fn idiv(a: i64, b: i64) -> i64 {
    if b == 0 {
        0
    } else {
        a / b
    }
}

/// The state of one `PathQueryHandler::kHopReachable` call (literal port, see
/// [`k_hop_reachable`]).
struct Khr<'g> {
    g: &'g CsrGraph,
    preds: &'g PredFilter,
    directed: bool,
    k: i64,
    l1: HashMap<Vid, i64>,
    l2: HashMap<Vid, i64>,
    f1: HashMap<Vid, i64>,
    f2: HashMap<Vid, i64>,
    residue1: HashMap<Vid, i64>,
    residue2: HashMap<Vid, i64>,
}

const KHR_ALPHA: f64 = 0.2;
const KHR_BATCH: usize = 10;

impl<'g> Khr<'g> {
    fn groups(&self, x: Vid, want_out: bool, want_in: bool) -> PredGroups<'g> {
        if x < 0 {
            // a pseudo-vertex: no edges (ascending mode over empty runs)
            let mut gr = groups(self.g, 0, self.preds, false, false);
            gr.ascending = true;
            return gr;
        }
        groups(self.g, x as Node, self.preds, want_out, want_in)
    }

    fn set_out(&self, x: Vid) -> i64 {
        if x < 0 {
            0
        } else {
            self.g.out_degree(x as Node, self.preds) as i64
        }
    }

    fn set_in(&self, x: Vid) -> i64 {
        if x < 0 {
            0
        } else {
            self.g.in_degree(x as Node, self.preds) as i64
        }
    }

    fn res1(&self, x: Vid) -> i64 {
        self.residue1.get(&x).copied().unwrap_or(0)
    }

    fn res2(&self, x: Vid) -> i64 {
        self.residue2.get(&x).copied().unwrap_or(0)
    }

    /// The push denominators: `!directed` → out-degree (forward) / in-degree
    /// (backward) only, `directed` → out + in (the branches are swapped
    /// upstream).
    fn fwd_denom(&self, x: Vid) -> i64 {
        if !self.directed {
            self.set_out(x)
        } else {
            self.set_out(x) + self.set_in(x)
        }
    }

    fn bwd_denom(&self, x: Vid) -> i64 {
        if !self.directed {
            self.set_in(x)
        } else {
            self.set_in(x) + self.set_out(x)
        }
    }

    /// One `adj` of the forward push (kHopReachable.cpp:95-107); `true` = meet.
    #[allow(clippy::too_many_arguments)]
    fn push_fwd_visit(
        &mut self,
        adj: Vid,
        level: i64,
        r: i64,
        denom: i64,
        rmax_fwd: f64,
        cand: &mut VecDeque<(Vid, i64)>,
        cand_next: &mut VecDeque<(Vid, i64)>,
    ) -> bool {
        if self.l1.contains_key(&adj) {
            return false;
        }
        let la = level + 1;
        self.l1.insert(adj, la);
        if self.l2.get(&adj).is_some_and(|&x| la + x <= self.k) {
            return true;
        }
        if self.f2.get(&adj).is_some_and(|&x| la + x <= self.k) {
            return true;
        }
        let ra = ((1.0 - KHR_ALPHA) * r as f64 / denom as f64) as i64;
        self.residue1.insert(adj, ra);
        if idiv(ra, self.fwd_denom(adj)) as f64 >= rmax_fwd {
            cand.push_back((adj, la));
        } else {
            cand_next.push_back((adj, la));
        }
        false
    }

    /// One `adj` of the backward push (kHopReachable.cpp:248-260); note the
    /// upstream slips kept here: the new residue derives from `residue1` and
    /// is compared with `rmax_fwd`.
    #[allow(clippy::too_many_arguments)]
    fn push_bwd_visit(
        &mut self,
        adj: Vid,
        level: i64,
        r1: i64,
        denom: i64,
        rmax_fwd: f64,
        cand: &mut VecDeque<(Vid, i64)>,
        cand_next: &mut VecDeque<(Vid, i64)>,
    ) -> bool {
        if self.l2.contains_key(&adj) {
            return false;
        }
        let la = level + 1;
        self.l2.insert(adj, la);
        if self.l1.get(&adj).is_some_and(|&x| x + la <= self.k) {
            return true;
        }
        if self.f1.get(&adj).is_some_and(|&x| x + la <= self.k) {
            return true;
        }
        let ra = ((1.0 - KHR_ALPHA) * r1 as f64 / denom as f64) as i64;
        self.residue2.insert(adj, ra);
        if idiv(ra, self.bwd_denom(adj)) as f64 >= rmax_fwd {
            cand.push_back((adj, la));
        } else {
            cand_next.push_back((adj, la));
        }
        false
    }

    /// A forward-BFS discovery (kHopReachable.cpp:189-197); `true` = meet.
    fn bfs_fwd_visit(&mut self, adj: Vid, level: i64, next: &mut VecDeque<Vid>) -> bool {
        if self.f1.contains_key(&adj) {
            return false;
        }
        self.f1.insert(adj, level);
        if self.l2.get(&adj).is_some_and(|&x| level + x <= self.k) {
            return true;
        }
        if self.f2.get(&adj).is_some_and(|&x| level + x <= self.k) {
            return true;
        }
        next.push_back(adj);
        false
    }

    /// A backward-BFS discovery (kHopReachable.cpp:341-349); pushes into the
    /// **forward** next-queue, as upstream does.
    fn bfs_bwd_visit(&mut self, adj: Vid, level: i64, fwd_next: &mut VecDeque<Vid>) -> bool {
        if self.f2.contains_key(&adj) {
            return false;
        }
        self.f2.insert(adj, level);
        if self.l1.get(&adj).is_some_and(|&x| x + level <= self.k) {
            return true;
        }
        if self.f1.get(&adj).is_some_and(|&x| x + level <= self.k) {
            return true;
        }
        fwd_next.push_back(adj);
        false
    }

    /// kHopReachable.cpp:34-373 after the degree check.
    fn run(&mut self, u: Node, v: Node, init_rmax: f64) -> bool {
        let k = self.k;
        let (u, v) = (Vid::from(u), Vid::from(v));
        let mut rmax_fwd = 0.001f64;
        let mut rmax_bwd = 0.001f64;
        let mut cand1: VecDeque<(Vid, i64)> = VecDeque::from([(u, 0)]);
        let mut cand1_next: VecDeque<(Vid, i64)> = VecDeque::new();
        let mut cand2: VecDeque<(Vid, i64)> = VecDeque::from([(v, 0)]);
        let mut cand2_next: VecDeque<(Vid, i64)> = VecDeque::new();
        self.residue1.insert(u, 1);
        self.residue2.insert(v, 1);
        self.l1.insert(u, 0);
        self.l2.insert(v, 0);
        let mut fwd_q: VecDeque<Vid> = VecDeque::from([u]);
        let mut fwd_next: VecDeque<Vid> = VecDeque::new();
        let mut bwd_q: VecDeque<Vid> = VecDeque::from([v]);
        let mut bwd_next: VecDeque<Vid> = VecDeque::new();
        self.f1.insert(u, 0);
        self.f2.insert(v, 0);
        let (push_bs, bfs_bs) = (KHR_BATCH, KHR_BATCH);
        let mut full1 = 1i64;
        let mut full2 = 1i64;
        while full1 <= k
            && full2 <= k
            && (!fwd_q.is_empty() || !fwd_next.is_empty())
            && (!bwd_q.is_empty() || !bwd_next.is_empty())
        {
            // forward partial push (only on graphs with > ~10^8 vertices)
            let mut op = 0;
            while (!cand1.is_empty() || !cand1_next.is_empty())
                && op < push_bs
                && rmax_fwd > init_rmax
            {
                let Some((cur, level)) = cand1.pop_front() else {
                    std::mem::swap(&mut cand1, &mut cand1_next);
                    rmax_fwd /= 2.0;
                    continue;
                };
                op += 1;
                if level > k {
                    continue;
                }
                let denom = self.fwd_denom(cur);
                let r = self.res1(cur);
                if idiv(r, denom) as f64 >= rmax_fwd {
                    for (_, outs, ins) in self.groups(cur, true, self.directed) {
                        for &adj in outs.iter().chain(ins) {
                            if self.push_fwd_visit(
                                Vid::from(adj),
                                level,
                                r,
                                denom,
                                rmax_fwd,
                                &mut cand1,
                                &mut cand1_next,
                            ) {
                                return true;
                            }
                        }
                    }
                    self.residue1.insert(cur, 0);
                } else {
                    cand1_next.push_back((cur, level));
                }
            }

            // forward BFS: at most `bfs_bs` pops
            let mut op = 0;
            while (!fwd_q.is_empty() || !fwd_next.is_empty()) && op < bfs_bs && full1 <= k {
                let Some(cur) = fwd_q.pop_front() else {
                    std::mem::swap(&mut fwd_q, &mut fwd_next);
                    full1 += 1;
                    continue;
                };
                op += 1;
                for (_, outs, ins) in self.groups(cur, true, !self.directed) {
                    for &adj in outs {
                        if self.bfs_fwd_visit(Vid::from(adj), full1, &mut fwd_next) {
                            return true;
                        }
                    }
                    // undirected: in-neighbour *positions* read through
                    // getOutVertID (kHopReachable.cpp:206)
                    for i in 0..ins.len() {
                        let adj = outs.get(i).map_or(SENTINEL, |&n| Vid::from(n));
                        if self.bfs_fwd_visit(adj, full1, &mut fwd_next) {
                            return true;
                        }
                    }
                }
            }

            // backward partial push
            let mut op = 0;
            while (!cand2.is_empty() || !cand2_next.is_empty())
                && op < push_bs
                && rmax_bwd > init_rmax
            {
                let Some((cur, level)) = cand2.pop_front() else {
                    std::mem::swap(&mut cand2, &mut cand2_next);
                    rmax_bwd /= 2.0;
                    continue;
                };
                op += 1;
                if level > k {
                    continue;
                }
                let denom = self.bwd_denom(cur);
                if idiv(self.res2(cur), denom) as f64 >= rmax_bwd {
                    let r1 = self.res1(cur);
                    for (_, outs, ins) in self.groups(cur, self.directed, true) {
                        for &adj in ins.iter().chain(outs) {
                            if self.push_bwd_visit(
                                Vid::from(adj),
                                level,
                                r1,
                                denom,
                                rmax_fwd,
                                &mut cand2,
                                &mut cand2_next,
                            ) {
                                return true;
                            }
                        }
                    }
                    self.residue2.insert(cur, 0);
                } else {
                    cand2_next.push_back((cur, level));
                }
            }

            // backward BFS: at most `bfs_bs` pops
            let mut op = 0;
            while (!bwd_q.is_empty() || !bwd_next.is_empty()) && op < bfs_bs && full2 <= k {
                let Some(cur) = bwd_q.pop_front() else {
                    std::mem::swap(&mut bwd_q, &mut bwd_next);
                    full2 += 1;
                    continue;
                };
                op += 1;
                for (_, outs, ins) in self.groups(cur, true, true) {
                    // in-neighbour positions read through getOutVertID (:340)
                    for i in 0..ins.len() {
                        let adj = outs.get(i).map_or(SENTINEL, |&n| Vid::from(n));
                        if self.bfs_bwd_visit(adj, full2, &mut fwd_next) {
                            return true;
                        }
                    }
                    if !self.directed {
                        for &adj in outs {
                            if self.bfs_bwd_visit(Vid::from(adj), full2, &mut fwd_next) {
                                return true;
                            }
                        }
                    }
                }
            }
        }
        false
    }
}

/// `PathQueryHandler::kHopReachable(u, v, directed, k, preds)` for `u != v`
/// on CSR nodes. `init_rmax` overrides the push threshold (tests only).
fn k_hop_reachable_nodes(
    g: &CsrGraph,
    u: Node,
    v: Node,
    directed: bool,
    k: i64,
    preds: &PredFilter,
    init_rmax: Option<f64>,
) -> bool {
    let mut u_total = g.out_degree(u, preds);
    let mut v_total = g.in_degree(v, preds);
    if !directed {
        u_total += g.in_degree(u, preds);
        v_total += g.out_degree(v, preds);
    }
    if u_total == 0 || v_total == 0 {
        return false;
    }
    let init_rmax =
        init_rmax.unwrap_or_else(|| 100.0 / (g.num_nodes() as f64).powf(3.0 / 4.0) / 0.1);
    let mut st = Khr {
        g,
        preds,
        directed,
        k,
        l1: HashMap::new(),
        l2: HashMap::new(),
        f1: HashMap::new(),
        f2: HashMap::new(),
        residue1: HashMap::new(),
        residue2: HashMap::new(),
    };
    st.run(u, v, init_rmax)
}

/// `kHopReachable(u, v, directed, k, preds)` — is `v` reachable from `u`
/// within `k` hops (`k < 0`: unbounded)?
///
/// **Upstream:** GE:1746-1769 / GE:2746-2770; kHopReachable.cpp:16-373.
///
/// **Per pair** (every pair yields an entry): `u == v` →
/// `{"src":"‹u›","dst":"‹u›","value":"true"}`; otherwise
/// `k' = if k < 0 { 999 } else { k }` and
/// `{"src":"‹u›","dst":"‹v›","value":"true"}` or `…"value":"false"}` — the
/// value is a quoted string. Missing `u` prints `"src":""` with `"false"`.
/// (In the variable branch upstream forgets the `,` between these entries;
/// we write it — see the separator rule.)
///
/// **Boolean (ported literally — the upstream code is *not* plain k-hop
/// reachability):**
/// * `false` immediately if `u` has no out-edges or `v` no in-edges over
///   `preds` (undirected: no incident edges at all) (:20-32), i.e. also for
///   missing / isolated endpoints; `k' == 0` → `false`.
/// * A "partial push" phase runs only while `rmax > 1000 / n^0.75` with
///   `rmax` starting at 0.001, i.e. only for graphs with more than ~10^8
///   vertices; its `directed` / `!directed` branches are swapped, its
///   residues are C `int`s (so only `u` / `v` with exactly one incident edge
///   in the respective sense ever push) and the backward push reads
///   `residue1` and `rmax_fwd` (:85-166, :224-320). Where it would divide by
///   a zero degree (upstream: `SIGFPE`) the quotient is taken as 0.
/// * The BFS phase alternates batches of 10 pops between a forward BFS
///   (levels `f1`) and a backward BFS (levels `f2`), answering `true` as soon
///   as a newly discovered vertex `x` has `f1[x] + f2[x] <= k'` (or meets the
///   push levels `l1` / `l2`). Upstream slips, kept: the undirected forward
///   step reads in-neighbour *positions* with `getOutVertID` (:206), the
///   backward step does the same for every in-neighbour (:340) and pushes its
///   discoveries into the **forward** next-queue (:348, :366).
///   `getOutVertID(v, p, i)` with `i` beyond the out-degree returns `-1`,
///   modelled as a sentinel pseudo-vertex without edges; it "meets" when both
///   sides recorded it (undirected: `u` and `v` each have a predicate with
///   more in- than out-edges → `true` for `k' >= 2`, connected or not).
/// * Because the backward queue never refills, the whole search is **one
///   round**: at most 10 forward pops (BFS order from `u`), then one backward
///   expansion of `v`. So `true` means: `v` was discovered within 10 forward
///   pops at depth `<= k'`; or one of `v`'s "backward" neighbours (its
///   out-neighbours at the in-neighbour positions, `-1` beyond — for
///   undirected also its real out-neighbours) is `u` (with `k' >= 1`) or a
///   forward-discovered vertex with `f1 + 1 <= k'`. Distant pairs come out
///   `false` even when reachable; a `v -> u` edge can make an unreachable pair
///   `true`.
///
/// The docs' examples (`kHopReachable(<Francis>, ?x, true, 2, {<like>,<focus>})`
/// → `"false"`, with `k = -1` → `"true"`) come out unchanged.
pub fn k_hop_reachable(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let k = a.hop_limit();
    let mut list = JsonList::new();
    for (u, v) in a.pairs() {
        ctx.check_cancel()?;
        let value = if u.entity == v.entity {
            true
        } else {
            match (csr_node(ctx, &u), csr_node(ctx, &v)) {
                (Some(un), Some(vn)) => {
                    k_hop_reachable_nodes(ctx.csr, un, vn, a.directed, k, &a.preds, None)
                }
                _ => false,
            }
        };
        let s = list.item();
        push_src_dst(s, ctx, &u, ",", v.entity);
        s.push_str(if value {
            ",\"value\":\"true\"}"
        } else {
            ",\"value\":\"false\"}"
        });
    }
    Ok(GFunValue::Json(list.into_paths()))
}

/// `kHopReachablePath(u, v, directed, k, preds)` — a path of at most `k` hops.
///
/// **Upstream:** GE:1666-1690 / GE:2771-2795;
/// `PathQueryHandler::kHopReachablePath` (PathQueryHandler.cpp:236-243).
///
/// **Per pair:** `u == v` → the empty-path entry; otherwise
/// `k' = if k < 0 { 999 } else { k }`, take the shared primitive's shortest
/// path and drop it if it has more than `k'` edges; a remaining path → path
/// entry; else nothing. (So the path is always a *shortest* one; the docs'
/// remark that it "could also be a non-shortest path with a ring" does not
/// apply to the code.)
pub fn k_hop_reachable_path(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let k = a.hop_limit();
    let mut sc = SpScratch::new(ctx.csr);
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for (u, v) in a.pairs() {
        ctx.check_cancel()?;
        if u.entity == v.entity {
            push_path_json(list.item(), ctx, u.entity, v.entity, &GPath::empty());
            continue;
        }
        if let Some(p) = shortest_path_args(ctx, &mut sc, &mut poll, &u, &v, a.directed, &a.preds)?
        {
            if p.len() as i64 <= k {
                push_path_json(list.item(), ctx, u.entity, v.entity, &p);
            }
        }
    }
    Ok(GFunValue::Json(list.into_paths()))
}

// ---------------------------------------------------------------------------
// k-hop enumeration (kHopEnumeratePath.cpp)
// ---------------------------------------------------------------------------

/// A DFS frame's neighbour cursor: for each predicate, out-neighbours, then
/// (undirected) in-neighbours. `groups == None` is a frame that is not
/// expanded (its hop budget is used up).
struct NbrCursor<'g> {
    groups: Option<PredGroups<'g>>,
    cur: Option<(PredId, &'g [Node], &'g [Node])>,
    pos: usize,
    in_phase: bool,
}

impl<'g> NbrCursor<'g> {
    fn new(groups: Option<PredGroups<'g>>) -> Self {
        NbrCursor {
            groups,
            cur: None,
            pos: 0,
            in_phase: false,
        }
    }

    fn next(&mut self) -> Option<(Node, PathEdge)> {
        loop {
            if let Some((p, out, inn)) = self.cur {
                let list = if self.in_phase { inn } else { out };
                if let Some(&n) = list.get(self.pos) {
                    self.pos += 1;
                    let e = if self.in_phase { rev(p) } else { fwd(p) };
                    return Some((n, e));
                }
                if !self.in_phase {
                    self.in_phase = true;
                    self.pos = 0;
                    continue;
                }
            }
            self.cur = Some(self.groups.as_mut()?.next()?);
            self.pos = 0;
            self.in_phase = false;
        }
    }
}

/// Scratch space of [`k_hop_enumerate_pair`], reused across pairs.
struct EnumScratch {
    bar: Dense<u32>,
    /// All clear between pairs: the DFS unmarks what it marks.
    on_stack: Vec<bool>,
    nodes: Vec<Node>,
}

/// `PathQueryHandler::kHopEnumeratePath(u, v, retNum, directed, k, preds)`
/// (kHopEnumeratePath.cpp:118-174, `bc_dfs` :5-104) for `u != v` on CSR nodes,
/// writing one path entry per path (`src = ue`, `dst = ve`) as it is found.
#[allow(clippy::too_many_arguments)]
fn k_hop_enumerate_pair(
    ctx: &GFunCtx<'_>,
    sc: &mut EnumScratch,
    poll: &mut Poll<'_, '_>,
    guard: &mut OutputGuard,
    list: &mut JsonList,
    (u, ue): (Node, EntityLiteralId),
    (v, ve): (Node, EntityLiteralId),
    ret_num: i64,
    directed: bool,
    k: i64,
    preds: &PredFilter,
) -> Result<()> {
    let g = ctx.csr;
    let EnumScratch {
        bar,
        on_stack,
        nodes,
    } = sc;
    // Reverse k-BFS from v (:128-168): bar[x] = hop distance x → v, 0 when
    // unreached (`bar[to] == 0` doubles as "unvisited"; v itself stays 0).
    bar.reset();
    let bar_of = |bar: &Dense<u32>, x: Node| i64::from(bar.get_or_zero(x));
    let mut q = VecDeque::from([v]);
    while let Some(cur) = q.pop_front() {
        poll.tick()?;
        let bc = bar_of(bar, cur);
        if bc >= k {
            continue;
        }
        for (_, outs, ins) in groups(g, cur, preds, !directed, true) {
            for &to in ins.iter().chain(outs) {
                if bar.get_or_zero(to) == 0 && to != v {
                    bar.set(to, (bc + 1) as u32);
                    q.push_back(to);
                }
            }
        }
    }

    // bc_dfs with an explicit stack: (vertex, edge into it, neighbour cursor).
    // `s.size() - 1 < k` (kHopEnumeratePath.cpp:31) decides whether a frame
    // is expanded at all; `on_stack` replaces the linear `stacked` scan.
    let mut budget = ret_num;
    let mut stack: Vec<(Node, Option<PathEdge>, NbrCursor<'_>)> = Vec::new();
    let expand = |depth_edges: i64, x: Node| {
        NbrCursor::new((depth_edges < k).then(|| groups(g, x, preds, true, !directed)))
    };
    on_stack[u as usize] = true;
    stack.push((u, None, expand(0, u)));
    // Once the budget is spent no further path can be recorded (upstream
    // keeps looping without recursing), so stop right away.
    while budget != 0 {
        let Some(top) = stack.last_mut() else {
            break;
        };
        poll.tick()?;
        let Some((to, e)) = top.2.next() else {
            let (x, _, _) = stack.pop().expect("non-empty stack");
            on_stack[x as usize] = false;
            continue;
        };
        if on_stack[to as usize] || stack.len() as i64 + bar_of(bar, to) > k {
            continue;
        }
        if to == v {
            // record the stack + v
            nodes.clear();
            nodes.extend(stack.iter().map(|f| f.0));
            nodes.push(v);
            let edges = stack
                .iter()
                .filter_map(|f| f.1)
                .chain(std::iter::once(e))
                .collect();
            push_path_json(list.item(), ctx, ue, ve, &gpath(g, nodes, edges));
            if budget > 0 {
                budget -= 1;
            }
            // (an error ends the whole call, scratch space included)
            guard.emitted(list)?;
            continue;
        }
        let depth_edges = stack.len() as i64; // edges once `to` is pushed
        on_stack[to as usize] = true;
        stack.push((to, Some(e), expand(depth_edges, to)));
    }
    for (x, _, _) in stack.drain(..) {
        on_stack[x as usize] = false;
    }
    Ok(())
}

/// `kHopEnumerate(u, v, directed, k, preds [, retNum])` — all simple paths
/// of at most `k` hops.
///
/// **Upstream:** GE:1691-1719 / GE:2796-2823;
/// `kHopEnumeratePath` + `bc_dfs` (kHopEnumeratePath.cpp:5-174).
///
/// **Per pair:** `u == v` → the empty-path entry (checked first, so two
/// missing vertices give it too); negative `u`/`v` → nothing; otherwise
/// `k' = if k < 0 { 999 } else { k }`, `retNum = a.ret_num` (`None` → `-1`
/// = unlimited) and one path entry per enumerated path in discovery order:
///
/// 1. `bar[x]` = hop distance from `x` to `v` by a reverse BFS from `v`
///    (in-edges, plus out-edges when undirected, per predicate) that stops
///    expanding at distance `k'`. Unreached vertices keep `bar = 0` (so they
///    are not pruned — kept; the pruning never removes an output path).
/// 2. DFS from `u` with a stack of `(vertex, edge)` (`u` with edge `-1`):
///    on reaching `v` record the path and pop; otherwise, while the stack
///    holds fewer than `k' + 1` vertices, for each predicate in filter order:
///    out-neighbours `t` (edge `pred`), then (undirected) in-neighbours
///    (edge `-pred-1`, i.e. reversed); skip `t` already on the stack (simple
///    paths) and `t` with `stack_len + bar[t] > k'`; recurse only while the
///    remaining budget is non-zero. So the result is every simple path
///    `u → v` of at most `k'` edges that meets `v` only at its end, each
///    predicate (and, undirected, each direction) a separate path.
/// 3. Budget: starts at `retNum`, decremented per recorded path while
///    positive; a negative budget never reaches 0 (unlimited), `0` enumerates
///    nothing. Note that the budget is per pair (upstream calls
///    `kHopEnumeratePath` once per pair).
///
/// Unbounded enumeration can be exponential: more than
/// [`MAX_ENUMERATED_PATHS`] entries (or [`MAX_ENUMERATED_BYTES`] of output)
/// in one call → error. The search itself is not bounded (neither is
/// upstream's); it polls the cancellation flag.
///
/// **Example** (SPARQL_QUERY.md): `kHopEnumerate(<Alice>, ?x, true, 3, {<like>, <focus>})`
/// with `?x = <Carol>` → two paths `Alice -like-> Bob -like-> Eve -like-> Carol`
/// then `Alice -focus-> Bob -like-> Eve -like-> Carol` (predicates in the
/// written order). The docs list each path's `nodes` as `3,2,0,1`; the
/// `pathVec2JSON` map order for the insertion sequence `0,1,2,3` is
/// `3,2,1,0` under libstdc++ (see
/// [`CxxUnorderedOrder`](super::cxx::CxxUnorderedOrder)), which also matches
/// the docs' other examples — the docs' order here is stale.
pub fn k_hop_enumerate(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    k_hop_enumerate_guarded(ctx, a, &mut OutputGuard::new(GFnKind::KHopEnumerate))
}

fn k_hop_enumerate_guarded(
    ctx: &GFunCtx<'_>,
    a: &GArgs,
    guard: &mut OutputGuard,
) -> Result<GFunValue> {
    let g = ctx.csr;
    let k = a.hop_limit();
    let ret_num = a.ret_num.unwrap_or(-1);
    let mut sc = EnumScratch {
        bar: Dense::new(g.num_nodes()),
        on_stack: vec![false; g.num_nodes()],
        nodes: Vec::new(),
    };
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for (u, v) in a.pairs() {
        ctx.check_cancel()?;
        if u.entity == v.entity {
            push_path_json(list.item(), ctx, u.entity, v.entity, &GPath::empty());
            guard.emitted(&list)?;
            continue;
        }
        if negative(&u) || negative(&v) {
            continue;
        }
        // A vertex without edges is never reached / never leaves.
        let (Some(un), Some(vn)) = (csr_node(ctx, &u), csr_node(ctx, &v)) else {
            continue;
        };
        k_hop_enumerate_pair(
            ctx,
            &mut sc,
            &mut poll,
            guard,
            &mut list,
            (un, u.entity),
            (vn, v.entity),
            ret_num,
            a.directed,
            k,
            &a.preds,
        )?;
    }
    Ok(GFunValue::Json(list.into_paths()))
}

// ---------------------------------------------------------------------------
// k-hop neighbourhoods
// ---------------------------------------------------------------------------

/// `PathQueryHandler::kHopNeighbor(u, directed, k, preds, retNum)`
/// (kHopNeighbor.cpp:14-76) from a CSR node.
#[allow(clippy::too_many_arguments)]
fn k_hop_neighbor_nodes(
    g: &CsrGraph,
    visited: &mut NodeMarks,
    poll: &mut Poll<'_, '_>,
    s: Node,
    directed: bool,
    k: i64,
    preds: &PredFilter,
    ret_num: i64,
) -> Result<Vec<Node>> {
    visited.clear();
    visited.mark(s);
    let mut cnt: Vec<Node> = Vec::new();
    let mut q = vec![s];
    let mut k = k;
    while k != 0 && !q.is_empty() {
        let mut next = Vec::new();
        for &v in &q {
            poll.tick()?;
            let mut done = false;
            for_each_two_pass(g, v, preds, directed, true, |n| {
                if done || !visited.mark(n) {
                    return;
                }
                if k == 1 {
                    cnt.push(n);
                    if ret_num >= 0 && cnt.len() as i64 >= ret_num {
                        done = true;
                    }
                } else {
                    next.push(n);
                }
            });
            if done {
                return Ok(cnt);
            }
        }
        k -= 1;
        q = next;
    }
    Ok(cnt)
}

/// The `kHopNeighbor` list of a resolved source (`[]` for negative and
/// isolated sources).
fn k_hop_neighbor_list(
    ctx: &GFunCtx<'_>,
    visited: &mut NodeMarks,
    poll: &mut Poll<'_, '_>,
    u: &GNode,
    a: &GArgs,
    k: i64,
    ret_num: i64,
) -> Result<Vec<Node>> {
    match csr_node(ctx, u) {
        Some(s) => {
            k_hop_neighbor_nodes(ctx.csr, visited, poll, s, a.directed, k, &a.preds, ret_num)
        }
        None => Ok(Vec::new()),
    }
}

/// `kHopCount(u, directed, k, preds)` — how many vertices lie exactly `k`
/// hops away.
///
/// **Upstream:** GE:1924-1931 / GE:2990-3001; `kHopCount` =
/// `kHopNeighbor(uid, directed, k, preds, -1).size()`
/// (PathQueryHandler.cpp:3435-3438).
///
/// **Per source:** `{"src":"‹u›","depth":K, "count":N}` (no space before
/// `"depth"`, one before `"count"`), `K` = `k` as written (no `999`
/// substitution), `N` = size of [`k_hop_neighbor`]'s list without a cap. The
/// docs say "within k layers" but the code counts vertices at distance
/// *exactly* `k` (follow the code). `k <= 0` → 0. Missing `u` → `"src":""`,
/// count 0. (`k` is mandatory in the grammar; an absent one counts as `-1`.)
///
/// **Example** (SPARQL_QUERY.md): `{"paths":[{"src":"<Alice>","depth":3, "count":1}]}`.
pub fn k_hop_count(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let k = a.k.unwrap_or(-1);
    let mut visited = NodeMarks::new(ctx.csr.num_nodes());
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for u in a.distinct_srcs() {
        ctx.check_cancel()?;
        let n = k_hop_neighbor_list(ctx, &mut visited, &mut poll, &u, a, k, -1)?.len();
        let s = list.item();
        s.push_str("{\"src\":");
        push_json_quoted(s, ctx.gnode_str(&u));
        s.push_str(&format!(",\"depth\":{k}, \"count\":{n}}}"));
    }
    Ok(GFunValue::Json(list.into_paths()))
}

/// `kHopNeighbor(u, directed, k, preds [, retNum])` — the vertices exactly
/// `k` hops away.
///
/// **Upstream:** GE:1932-1953 / GE:3002-3027; kHopNeighbor.cpp:14-76.
///
/// **Per source:** `{"src":"‹u›","depth":K, "dst":["‹a›","‹b›",…]}` when
/// [`GArgs::where_empty`] (constant branch, GE:1942), otherwise the key is
/// `"neighbor"` (GE:3016): `{"src":"‹u›","depth":K, "neighbor":[…]}`.
/// `K` = `k` as written; `retNum` = `a.ret_num`, `None` or negative → 100.
///
/// **List:** level-synchronous BFS from `u` marking vertices visited when
/// discovered; directed → out-edges; undirected → for each expanded vertex
/// all predicates' in-neighbours, then all predicates' out-neighbours
/// (`NbrOrder::AllInThenAllOut`). Vertices first discovered on level `k`
/// are collected in discovery order (levels `< k` are only expanded);
/// `k <= 0` → empty (a negative `k` exhausts the BFS without collecting).
/// After each collected vertex, stop if `len >= retNum` — so `retNum = 0`
/// still returns one vertex (quirk). Missing `u` → `[]`; isolated → `[]`.
pub fn k_hop_neighbor(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let k = a.k.unwrap_or(-1);
    let ret_num = a.ret_num_or(100);
    let key = if a.where_empty { "dst" } else { "neighbor" };
    let mut visited = NodeMarks::new(ctx.csr.num_nodes());
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for u in a.distinct_srcs() {
        ctx.check_cancel()?;
        let nbrs = k_hop_neighbor_list(ctx, &mut visited, &mut poll, &u, a, k, ret_num)?;
        let s = list.item();
        s.push_str("{\"src\":");
        push_json_quoted(s, ctx.gnode_str(&u));
        s.push_str(&format!(",\"depth\":{k}, \"{key}\":["));
        for (i, &n) in nbrs.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            push_json_quoted(s, ctx.node_str(n));
        }
        s.push_str("]}");
    }
    Ok(GFunValue::Json(list.into_paths()))
}

/// `PathQueryHandler::kHopAllNeighbors` (kHopAllNeighbors.cpp:5-86) from a CSR
/// node, with the wrapper's `k'` / `retNum`. Returns the result set ascending.
#[allow(clippy::too_many_arguments)]
fn k_hop_all_neighbors_nodes(
    g: &CsrGraph,
    dis: &mut Dense<u32>,
    visited: &mut NodeMarks,
    poll: &mut Poll<'_, '_>,
    s: Node,
    directed: bool,
    k: i64,
    preds: &PredFilter,
    ret_num: i64,
) -> Result<Vec<Node>> {
    dis.reset();
    visited.clear();
    let mut q = VecDeque::from([s]);
    dis.set(s, 0);
    visited.mark(s);
    let mut result: Vec<Node> = Vec::new();
    let mut nl: Vec<Node> = Vec::new();
    while let Some(x) = q.pop_front() {
        poll.tick()?;
        let dx = dis.get_or_zero(x);
        let distance = dx + 1;
        nl.clear();
        for (_, outs, ins) in groups(g, x, preds, true, !directed) {
            for &t in outs.iter().chain(ins) {
                nl.push(t);
                if dis.get(t).is_none_or(|d| d > distance) {
                    dis.set(t, distance);
                }
            }
        }
        if result.len() as i64 >= ret_num {
            break;
        }
        if i64::from(dx) <= k {
            result.push(x);
            nl.sort_unstable();
            nl.dedup();
            for &m in &nl {
                if !visited.is_marked(m) && i64::from(dis.get_or_zero(m)) <= k {
                    visited.mark(m);
                    q.push_back(m);
                }
            }
        }
    }
    result.sort_unstable();
    Ok(result)
}

/// `kHopAllNeighbors(u, directed, k, preds [, retNum])` — the vertices within
/// `k` hops (the source included).
///
/// **Upstream:** GE:2012-2015 / GE:3090-3093 → wrapper GE:4161-4177;
/// kHopAllNeighbors.cpp:5-86.
///
/// **Per source:** negative `u` → nothing; otherwise
/// `k' = if k < 0 { 999 } else { k }`, `retNum` `None`/negative → 100, and
/// `{"results":["‹a›","‹b›",…]}`. (Upstream writes consecutive sources'
/// objects without a `,`; we separate them.)
///
/// **List:** BFS from `u` with a distance map (out-edges, plus in-edges when
/// undirected, per predicate in filter order); for each dequeued vertex `x`:
/// collect its neighbours into a `std::set` `nl` and lower their recorded
/// distances; **then** stop if `|result| >= retNum`; else, if
/// `dist(x) <= k'`, add `x` to the result and enqueue the unvisited members of
/// `nl` (ascending) whose distance is `<= k'`. The result is a `std::set`, so
/// it prints **ascending by entity id** (which vertices make it in is decided
/// by the BFS order: the first `retNum` dequeued). `retNum = 0` → `[]`; an
/// isolated `u` → `["‹u›"]`.
pub fn k_hop_all_neighbors(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let g = ctx.csr;
    let k = a.hop_limit();
    let ret_num = a.ret_num_or(100);
    let mut dis = Dense::new(g.num_nodes());
    let mut visited = NodeMarks::new(g.num_nodes());
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for u in a.distinct_srcs() {
        ctx.check_cancel()?;
        if negative(&u) {
            continue;
        }
        let members: Vec<EntityLiteralId> = match csr_node(ctx, &u) {
            Some(s) => k_hop_all_neighbors_nodes(
                g,
                &mut dis,
                &mut visited,
                &mut poll,
                s,
                a.directed,
                k,
                &a.preds,
                ret_num,
            )?
            .into_iter()
            .map(|n| g.entity_of(n))
            .collect(),
            // no edges: the source alone (unless retNum == 0)
            None if ret_num > 0 => vec![u.entity],
            None => Vec::new(),
        };
        let s = list.item();
        s.push_str("{\"results\":[");
        for (i, &e) in members.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            push_json_quoted(s, ctx.entity_str(e));
        }
        s.push_str("]}");
    }
    Ok(GFunValue::Json(list.into_paths()))
}

// ---------------------------------------------------------------------------
// BFS levels and single-source shortest paths
// ---------------------------------------------------------------------------

/// `PathQueryHandler::bfsCount` (bfsCount.cpp:15-85) from a CSR node.
fn bfs_count_nodes(
    g: &CsrGraph,
    visited: &mut NodeMarks,
    poll: &mut Poll<'_, '_>,
    s: Node,
    directed: bool,
    preds: &PredFilter,
) -> Result<Vec<u64>> {
    visited.clear();
    visited.mark(s);
    let mut q = VecDeque::from([s]);
    let mut current: u64 = 1;
    let mut next: u64 = 0;
    let mut ans = vec![1u64];
    while let Some(now) = q.pop_front() {
        poll.tick()?;
        if current == 0 {
            current = next;
            next = 0;
            ans.push(current);
        }
        current -= 1;
        for_each_two_pass(g, now, preds, directed, true, |n| {
            if visited.mark(n) {
                q.push_back(n);
                next += 1;
            }
        });
    }
    Ok(ans)
}

/// `bfsCount(u, directed, preds)` — vertices per BFS level.
///
/// **Upstream:** GE:1799-1811 / GE:2861-2877; bfsCount.cpp:15-85.
///
/// **Per source:**
/// `{"src":"‹u›","results":[{"depth":0, "count":1}, {"depth":1, "count":2}]}`
/// — inside an element `", \"count\":"` has a space, and elements are
/// separated by `", "` (comma **and** space).
///
/// **Counts:** BFS from `u` (directed: out-edges per predicate; undirected:
/// all predicates' in-edges, then all predicates' out-edges); `count[d]` =
/// vertices first discovered at depth `d`; depth 0 is `u` itself (1); no
/// trailing zero level. Missing / isolated `u` → `[{"depth":0, "count":1}]`.
///
/// **Example** (SPARQL_QUERY.md): `bfsCount(<Alice>, true, {<like>,<focus>,<dislike>})` →
/// `{"paths":[{"src":"<Alice>","results":[{"depth":0, "count":1}, {"depth":1, "count":2}, {"depth":2, "count":1}, {"depth":3, "count":1}, {"depth":4, "count":1}]}]}`.
pub fn bfs_count(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let mut visited = NodeMarks::new(ctx.csr.num_nodes());
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for u in a.distinct_srcs() {
        ctx.check_cancel()?;
        let counts = match csr_node(ctx, &u) {
            Some(s) => bfs_count_nodes(ctx.csr, &mut visited, &mut poll, s, a.directed, &a.preds)?,
            None => vec![1],
        };
        let s = list.item();
        s.push_str("{\"src\":");
        push_json_quoted(s, ctx.gnode_str(&u));
        s.push_str(",\"results\":[");
        for (d, c) in counts.iter().enumerate() {
            if d > 0 {
                s.push_str(", ");
            }
            s.push_str(&format!("{{\"depth\":{d}, \"count\":{c}}}"));
        }
        s.push_str("]}");
    }
    Ok(GFunValue::Json(list.into_paths()))
}

/// The BFS of `PathQueryHandler::SSSP` / `SSSPLen` (SSSP.cpp:14-58,
/// SSSPLen.cpp:14-54): discovery order (source first) with each vertex's BFS
/// parent and predicate.
fn sssp_bfs(
    g: &CsrGraph,
    parent: &mut Dense<u64>,
    poll: &mut Poll<'_, '_>,
    s: Node,
    directed: bool,
    preds: &PredFilter,
) -> Result<Vec<Node>> {
    parent.reset();
    parent.set(s, ROOT);
    let mut order = vec![s];
    let mut head = 0;
    while let Some(&x) = order.get(head) {
        head += 1;
        poll.tick()?;
        for (p, outs, ins) in groups(g, x, preds, true, !directed) {
            for &t in outs.iter().chain(ins) {
                if !parent.has(t) {
                    parent.set(t, pack(x, p));
                    order.push(t);
                }
            }
        }
    }
    Ok(order)
}

/// Discovery order → `unordered_map<int, …>` iteration order (indices into
/// `keys`).
fn unordered_order(keys: &[i64]) -> Vec<usize> {
    let mut m = CxxUnorderedOrder::new();
    let mut index: HashMap<i64, usize> = HashMap::with_capacity(keys.len());
    for (i, &key) in keys.iter().enumerate() {
        m.insert(key);
        index.insert(key, i);
    }
    m.keys().into_iter().map(|key| index[&key]).collect()
}

/// `SSSP(u, directed, preds)` — a shortest path from `u` to every reachable
/// vertex.
///
/// **Upstream:** GE:1832-1851 / GE:2898-2917; SSSP.cpp:14-58.
///
/// **Per source** (sources separated by `,`): one path entry per reachable
/// vertex `t` (`u` itself included, as the single-node path), comma-separated,
/// with `src = u`, `dst = t`.
///
/// * Paths: plain BFS from `u` (FIFO, visited-on-discovery; per predicate in
///   filter order: out-neighbours, then in-neighbours when undirected —
///   `NbrOrder::PerPredOutThenIn`); `path[t] = path[parent] + [pred, t]`.
///   **Every edge is recorded with its plain predicate id**, also in-edges,
///   so it prints forward (`fromNode` = earlier node) — reproduced.
/// * Order: the result is an `unordered_map<int, vector<int>>` filled in
///   discovery order (`u` first); iterate it in
///   [`CxxUnorderedOrder`](super::cxx::CxxUnorderedOrder) order of the
///   discovered entity ids.
/// * Missing / isolated `u`: only its own single-node path (a missing one
///   prints `"src":"","dst":""` and `{"nodeIndex":-1,"nodeIRI":""}`).
pub fn sssp(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let g = ctx.csr;
    let mut parent = Dense::new(g.num_nodes());
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for u in a.distinct_srcs() {
        ctx.check_cancel()?;
        let Some(s) = csr_node(ctx, &u) else {
            push_path_json(
                list.item(),
                ctx,
                u.entity,
                u.entity,
                &GPath::single(u.entity),
            );
            continue;
        };
        let order = sssp_bfs(g, &mut parent, &mut poll, s, a.directed, &a.preds)?;
        let keys: Vec<i64> = order
            .iter()
            .map(|&n| i64::from(g.entity_of(n) as i32))
            .collect();
        for i in unordered_order(&keys) {
            poll.tick()?;
            let t = order[i];
            let mut nodes = vec![t];
            let mut edges = Vec::new();
            let mut c = t;
            while let Some(packed) = parent.get(c).filter(|&r| r != ROOT) {
                let (par, p) = unpack(packed);
                edges.push(fwd(p));
                nodes.push(par);
                c = par;
            }
            nodes.reverse();
            edges.reverse();
            let path = gpath(g, &nodes, edges);
            push_path_json(list.item(), ctx, u.entity, g.entity_of(t), &path);
        }
    }
    Ok(GFunValue::Json(list.into_paths()))
}

/// `SSSPLen(u, directed, preds)` — shortest-path lengths from `u`.
///
/// **Upstream:** GE:1852-1871 / GE:2918-2937; SSSPLen.cpp:14-54.
///
/// **Per source** (sources separated by `,`): one
/// `{"src":"‹u›","dst":"‹t›","length":L}` per reachable vertex `t` (`u`
/// itself with length 0), comma-separated, in the
/// [`CxxUnorderedOrder`](super::cxx::CxxUnorderedOrder) order of the
/// entity ids in BFS discovery order (same BFS as [`sssp`]). Missing /
/// isolated `u` → only its own `length 0` entry.
pub fn sssp_len(ctx: &GFunCtx<'_>, a: &GArgs) -> Result<GFunValue> {
    let g = ctx.csr;
    let mut parent = Dense::new(g.num_nodes());
    let mut dist = Dense::<u32>::new(g.num_nodes());
    let mut poll = Poll::new(ctx);
    let mut list = JsonList::new();
    for u in a.distinct_srcs() {
        ctx.check_cancel()?;
        let Some(s) = csr_node(ctx, &u) else {
            let item = list.item();
            push_src_dst(item, ctx, &u, ",", u.entity);
            item.push_str(",\"length\":0}");
            continue;
        };
        let order = sssp_bfs(g, &mut parent, &mut poll, s, a.directed, &a.preds)?;
        dist.reset();
        for &t in &order {
            let d = match parent.get(t).filter(|&r| r != ROOT) {
                Some(packed) => dist.get_or_zero(unpack(packed).0) + 1,
                None => 0,
            };
            dist.set(t, d);
        }
        let keys: Vec<i64> = order
            .iter()
            .map(|&n| i64::from(g.entity_of(n) as i32))
            .collect();
        for i in unordered_order(&keys) {
            let t = order[i];
            let item = list.item();
            push_src_dst(item, ctx, &u, ",", g.entity_of(t));
            item.push_str(&format!(",\"length\":{}}}", dist.get_or_zero(t)));
        }
    }
    Ok(GFunValue::Json(list.into_paths()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IdTriple;

    fn graph(edges: &[(u32, u32, u32)]) -> CsrGraph {
        CsrGraph::from_id_triples(
            edges
                .iter()
                .map(|&(s, p, o)| IdTriple::new(s, p, o))
                .collect(),
        )
        .unwrap()
    }

    /// The resource guard of the enumerating functions, with small limits:
    /// the diamond a -p-> b -p-> d, a -p-> c -p-> d, a -q-> b has three
    /// shortest paths a → d and three simple paths of at most 2 hops.
    #[test]
    fn enumeration_output_guard() {
        let g = graph(&[(0, 0, 1), (0, 0, 2), (1, 0, 3), (2, 0, 3), (0, 1, 1)]);
        let d = crate::dict::Dictionary::new();
        let ctx = GFunCtx::new(&g, &d);
        let args = GArgs {
            srcs: vec![GNode::from_node(&g, 0)],
            dsts: vec![GNode::from_node(&g, 3)],
            directed: true,
            k: Some(2),
            preds: PredFilter::from_ids([0, 1]),
            ..GArgs::default()
        };
        let run = |kind, paths, bytes| {
            let mut guard = OutputGuard::with_limits(kind, paths, bytes);
            if kind == GFnKind::ShortestPaths {
                shortest_paths_guarded(&ctx, &args, &mut guard)
            } else {
                k_hop_enumerate_guarded(&ctx, &args, &mut guard)
            }
        };
        for kind in [GFnKind::ShortestPaths, GFnKind::KHopEnumerate] {
            let full = run(kind, 3, usize::MAX).unwrap();
            let GFunValue::Json(s) = &full else {
                panic!("{full:?}")
            };
            assert_eq!(s.matches("{\"src\"").count(), 3, "{s}");
            let err = run(kind, 2, usize::MAX).unwrap_err();
            assert!(
                matches!(&err, GStoreError::Query(m) if m.contains(kind.name())),
                "{err}"
            );
            // The byte cap applies to the entries written so far (the
            // envelope `{"paths":[…]}` adds 12 bytes).
            assert_eq!(run(kind, 3, s.len() - 12).unwrap(), full);
            assert!(run(kind, 3, s.len() - 13).is_err());
        }
        let guard = OutputGuard::new(GFnKind::ShortestPaths);
        assert_eq!(
            (guard.max_paths, guard.max_bytes),
            (MAX_ENUMERATED_PATHS, MAX_ENUMERATED_BYTES)
        );
    }

    #[test]
    fn pred_groups_follow_filter_order_and_skip_empty_predicates() {
        // 0 -p0-> 1, 0 -p2-> 2, 3 -p1-> 0, 0 -p2-> 3
        let g = graph(&[(0, 0, 1), (0, 2, 2), (3, 1, 0), (0, 2, 3)]);
        let asc = PredFilter::from_ids([0, 1, 2]);
        let got: Vec<_> = groups(&g, 0, &asc, true, true).collect();
        assert_eq!(
            got,
            vec![
                (0, &[1][..], &[][..]),
                (1, &[][..], &[3][..]),
                (2, &[2, 3][..], &[][..])
            ]
        );
        let list = PredFilter::from_ids([2, 7, 0]);
        let got: Vec<_> = groups(&g, 0, &list, true, false).collect();
        assert_eq!(got, vec![(2, &[2, 3][..], &[][..]), (0, &[1][..], &[][..])]);
        // excluded predicate 1 is skipped in ascending mode
        let ex = PredFilter::from_ids([0, 2]);
        let got: Vec<_> = groups(&g, 0, &ex, false, true).collect();
        assert!(got.is_empty());
    }

    #[test]
    fn two_pass_orders() {
        // node 0: out p0 → 1, out p1 → 2, in p0 ← 3, in p1 ← 4
        let g = graph(&[(0, 0, 1), (0, 1, 2), (3, 0, 0), (4, 1, 0)]);
        let f = PredFilter::from_ids([1, 0]);
        let mut v = Vec::new();
        for_each_two_pass(&g, 0, &f, false, true, |n| v.push(n));
        assert_eq!(v, vec![4, 3, 2, 1], "all in (filter order), then all out");
        v.clear();
        for_each_two_pass(&g, 0, &f, false, false, |n| v.push(n));
        assert_eq!(v, vec![2, 1, 4, 3], "all out, then all in");
        v.clear();
        for_each_two_pass(&g, 0, &f, true, true, |n| v.push(n));
        assert_eq!(v, vec![2, 1]);
        v.clear();
        for_each_two_pass(&g, 0, &f, true, false, |n| v.push(n));
        assert_eq!(v, vec![4, 3]);
    }

    #[test]
    fn dense_map_resets() {
        let mut d = Dense::<u32>::new(4);
        assert_eq!(d.get(1), None);
        d.set(1, 7);
        assert_eq!((d.get(1), d.get_or_zero(2)), (Some(7), 0));
        d.reset();
        assert!(!d.has(1));
        d.cur = u32::MAX;
        d.reset();
        assert_eq!((d.cur, d.get(1)), (1, None));
        assert_eq!(unpack(pack(5, 9)), (5, 9));
    }

    /// The partial-push phase only runs on graphs with > ~10^8 vertices; force
    /// it with a tiny threshold and trace kHopReachable.cpp by hand.
    ///
    /// Graph: 0 -p0-> 1 -p0-> 2, 3 -p0-> 2 (all p0). u = 0, v = 2, directed,
    /// k = 1. Upstream's directed push uses the *undirected* branch
    /// (out + in degrees).
    /// * degree check: out(0) = 1, in(2) = 2 → go.
    /// * fwd push: cand1 = [(0,0)], residue1[0] = 1, denom = out+in(0) = 1,
    ///   1/1 = 1 >= 0.001 → push 0: adj 1 (out) → l1[1] = 1; l2 = {2:0},
    ///   f2 = {2:0} → no meet; residue1[1] = int(0.8*1/1) = 0 → cand1_next.
    ///   Then cand1 empty → swap, rmax 0.0005; pop (1,1): 1 <= k, residue 0 →
    ///   0 >= 0.0005 false → cand1_next … halving until rmax <= init_rmax.
    /// * fwd BFS (k = 1): pop 0 → f1[1] = 1 (l2/f2 lack 1); fwd empty → swap,
    ///   fullL1 = 2 > k → stop.
    /// * bwd push: cand2 = [(2,0)], residue2[2] = 1, denom = in+out(2) = 2,
    ///   1/2 = 0 → cand2_next; swaps until rmax_bwd <= init_rmax.
    /// * bwd BFS: pop 2: p0 in-positions 0,1 → getOutVertID(2, p0, i) = -1
    ///   (2 has no out-edges) → f2[-1] = 1, no meet (l1 = {0,1}, f1 = {0,1}).
    /// * Loop ends (bwd queues empty) → false. With k = 2, the fwd push's
    ///   l1[1] = 1 meets nothing either, but the BFS phase then reaches 2 at
    ///   f1 = 2 with f2[2] = 0 → true.
    #[test]
    fn k_hop_reachable_partial_push_phase() {
        let g = graph(&[(0, 0, 1), (1, 0, 2), (3, 0, 2)]);
        let f = PredFilter::from_ids([0]);
        assert!(!k_hop_reachable_nodes(&g, 0, 2, true, 1, &f, Some(1e-4)));
        assert!(k_hop_reachable_nodes(&g, 0, 2, true, 2, &f, Some(1e-4)));
        // The push levels take part in the meet test: 0 -p0-> 1 with v = 1 is
        // met by the push itself (l1[1] = 1, l2[1] = 0).
        assert!(k_hop_reachable_nodes(&g, 0, 1, true, 1, &f, Some(1e-4)));
        // Undirected push uses out-degree only: u = 3 has out-degree 1 →
        // pushes to 2 (l1[2] = 1); v = 2: l2[2] = 0 → 1 + 0 <= 1 → true.
        assert!(k_hop_reachable_nodes(&g, 3, 2, false, 1, &f, Some(1e-4)));
        // A zero divisor (upstream: SIGFPE) reads as 0 and the search goes
        // on. u = 1, v = 3, undirected, k = 1: the forward push from 1
        // (out-degree 1) sets l1[2] = 1, residue1[2] = int(0.8) = 0 and then
        // divides by out-degree(2) = 0; the backward push divides by
        // in-degree(3) = 0. BFS: f1[2] = 1; backward from 3 over its real
        // out-edge: f2[2] = 1, and l1[2] + 1 = f1[2] + 1 = 2 > 1 → false.
        assert!(!k_hop_reachable_nodes(&g, 1, 3, false, 1, &f, Some(1e-4)));
        // u = 1, v = 0: same zero divisors, but the backward BFS from 0 finds
        // its out-neighbour 1 = u with l1[1] = 0 → 0 + 1 <= 1 → true.
        assert!(k_hop_reachable_nodes(&g, 1, 0, false, 1, &f, Some(1e-4)));
    }
}
