//! Literal port of upstream's single-source personalized PageRank
//! (`PathQueryHandler::SSPPR`, SSPPR.cpp:96-191, a FORA top-k variant) and
//! its helpers (`kth_ppr` / `if_stop`, SSPPR.cpp:5-94;
//! `set_ppr_bounds`, `calculate_lambda`, `random_walk`,
//! `forward_local_update_linear_topk`, `compute_ppr_with_reserve`,
//! `compute_ppr_with_fwdidx_topk_with_bound`, PathQueryHandler.cpp:3025-3311).
//!
//! Vertices are CSR nodes (upstream's raw vertex ids; identical when entity
//! ids are dense, see [`csr`](crate::analytics::csr) "Node ids"). The only
//! intended difference is the random source: upstream seeds `rand()` with
//! `srand(time(NULL))` on every call; this port reseeds a [`GlibcRand`] with
//! [`PPR_SEED`](super::PPR_SEED) on every call, so results are reproducible
//! (and equal to upstream's for a process whose `time()` returned that seed).
//!
//! Upstream behaviour that looks like a bug is kept, because it decides the
//! output (see [`ppr`](super::ppr) for the user-visible consequences):
//!
//! * `kth_ppr` reads `v2ppr[it->second]` — it indexes the map with a PPR
//!   *value* converted to `int` (0, or 1 for a value of exactly 1.0),
//!   inserting a zero entry for that vertex while iterating the map.
//! * `if_stop` evaluates `v2ppr[nodeid] <= 0` for vertices outside the
//!   current top-k, inserting zero entries.
//! * Walk estimates of earlier `delta` rounds stay in `v2ppr` (only vertices
//!   with a reserve are overwritten before new walks are added).
//!
//! `v2ppr` is a libstdc++ `unordered_map<int, double>`; its iteration order
//! (which decides ties in the final top-k) is reproduced with
//! [`CxxUnorderedOrder`] keyed by entity id.
//!
//! Out-of-bounds reads upstream performs (`kth_ppr` with `retNum` larger
//! than `v2ppr`) read `0.0` here.

use super::super::cxx::{cxx_partial_sort_copy_by, CxxUnorderedOrder, GlibcRand};
use super::super::fmt::cxx_int_id;
use super::super::{CsrGraph, GFunCtx, GNode, Node, PredFilter};
use crate::error::Result;
use crate::model::id::EntityLiteralId;

/// `nil` of upstream's `iMap`s in SSPPR.
const NIL: f64 = -9.0;

/// Upstream's `iMap<double>` / `iMap<int>`: a dense array with a sentinel
/// "absent" value and the insertion list `occur`.
struct IMap {
    data: Vec<f64>,
    occur: Vec<Node>,
}

impl IMap {
    /// `initialize(n)` with `nil = -9`.
    fn new(n: usize) -> IMap {
        IMap {
            data: vec![NIL; n],
            occur: Vec::new(),
        }
    }

    /// `init_keys(n)` followed by setting every value to `v`
    /// (`reset_one_values` / `reset_zero_values`).
    fn all_keys(n: usize, v: f64) -> IMap {
        IMap {
            data: vec![v; n],
            occur: (0..n as Node).collect(),
        }
    }

    fn exist(&self, p: Node) -> bool {
        self.data[p as usize] != NIL
    }

    fn get(&self, p: Node) -> f64 {
        self.data[p as usize]
    }

    fn set(&mut self, p: Node, v: f64) {
        self.data[p as usize] = v;
    }

    fn insert(&mut self, p: Node, v: f64) {
        if self.data[p as usize] == NIL {
            self.occur.push(p);
        }
        self.data[p as usize] = v;
    }

    fn clean(&mut self) {
        for &p in &self.occur {
            self.data[p as usize] = NIL;
        }
        self.occur.clear();
    }
}

/// libstdc++ `std::max(a, b)`: `a < b ? b : a`.
fn cxx_max(a: f64, b: f64) -> f64 {
    if a < b {
        b
    } else {
        a
    }
}

/// libstdc++ `std::min(a, b)`: `b < a ? b : a`.
fn cxx_min(a: f64, b: f64) -> f64 {
    if b < a {
        b
    } else {
        a
    }
}

/// glibc `pow`, kept out of compile-time constant folding.
fn pow(x: f64, y: f64) -> f64 {
    std::hint::black_box(x).powf(std::hint::black_box(y))
}

/// State shared by the sources of one `PPR` call: the per-node out-degree
/// over the predicate filter (upstream `getSetOutSize`), the filtered edge
/// count (`getSetEdgeNum`) and `kth_ppr`'s function-`static` buffer.
pub(super) struct Session {
    deg: Vec<u32>,
    edges: u64,
    temp_ppr: Vec<f64>,
}

impl Session {
    pub(super) fn new(g: &CsrGraph, f: &PredFilter) -> Session {
        Session {
            deg: (0..g.num_nodes() as Node)
                .map(|v| g.out_degree(v, f) as u32)
                .collect(),
            edges: g.num_edges_with(f),
            temp_ppr: Vec::new(),
        }
    }
}

/// upstream `unordered_map<int, double> v2ppr`, keyed by vertex.
struct V2Ppr<'g> {
    g: &'g CsrGraph,
    order: CxxUnorderedOrder,
    val: Vec<Option<f64>>,
}

impl<'g> V2Ppr<'g> {
    fn new(g: &'g CsrGraph) -> V2Ppr<'g> {
        V2Ppr {
            g,
            order: CxxUnorderedOrder::new(),
            val: vec![None; g.num_nodes()],
        }
    }

    fn key(&self, v: Node) -> i64 {
        i64::from(cxx_int_id(self.g.entity_of(v)))
    }

    fn node(&self, key: i64) -> Node {
        self.g
            .node_of(key as i32 as EntityLiteralId)
            .expect("v2ppr keys are CSR vertices")
    }

    fn get(&self, v: Node) -> Option<f64> {
        self.val[v as usize]
    }

    /// `v2ppr[v]` (inserting `0.0` when absent).
    fn at(&mut self, v: Node) -> f64 {
        match self.val[v as usize] {
            Some(x) => x,
            None => {
                self.order.insert(self.key(v));
                self.val[v as usize] = Some(0.0);
                0.0
            }
        }
    }

    /// `v2ppr[v] = x`.
    fn set(&mut self, v: Node, x: f64) {
        if self.val[v as usize].is_none() {
            self.order.insert(self.key(v));
        }
        self.val[v as usize] = Some(x);
    }

    /// The vertices in iteration order.
    fn nodes(&self) -> Vec<Node> {
        self.order.keys().into_iter().map(|k| self.node(k)).collect()
    }

    fn len(&self) -> usize {
        self.order.len()
    }
}

struct Ppr<'a, 'g> {
    ctx: &'a GFunCtx<'g>,
    g: &'g CsrGraph,
    f: &'a PredFilter,
    out: &'a mut Session,
    n: usize,
    rng: GlibcRand,
    walks: u64,
}

/// `PathQueryHandler::SSPPR(uid, retNum, k, pred_set, topkV2ppr)`; returns
/// the `(vertex, PPR)` list upstream prints (vertices as entity ids).
/// `ret_num` must be positive (the caller handles `retNum <= 0`).
pub(super) fn ssppr(
    ctx: &GFunCtx<'_>,
    out: &mut Session,
    src: &GNode,
    f: &PredFilter,
    ret_num: usize,
    k: i32,
) -> Result<Vec<(EntityLiteralId, f64)>> {
    let g = ctx.csr;
    let n = g.num_nodes();
    // Upstream's vertex `0` (the default top-k slot) as an entity id.
    let vertex0 = if n > 0 { g.entity_of(0) } else { 0 };
    // `while (delta >= min_delta)` with `delta = 1/4`, `min_delta = 1/n`
    // never runs on graphs with fewer than 4 vertices: `v2ppr` stays empty
    // and the result is the single default slot.
    if n < 4 {
        return Ok(vec![(vertex0, 0.0)]);
    }
    let Some(uid) = src.node else {
        // An entity without entity edges is a vertex without out-edges:
        // `fwd_idx.first.insert(uid, 1)` → `v2ppr = {uid: 1}`.
        return Ok(vec![(src.entity, 1.0)]);
    };
    let mut p = Ppr {
        ctx,
        g,
        f,
        out,
        n,
        rng: GlibcRand::new(super::PPR_SEED),
        walks: 0,
    };
    let v2ppr = p.run(uid, ret_num, k)?;
    // Extract top-k results (SSPPR.cpp:178-190).
    let items: Vec<(Node, f64)> = v2ppr
        .nodes()
        .into_iter()
        .map(|v| (v, v2ppr.get(v).unwrap_or(0.0)))
        .collect();
    let mut topk: Vec<(Option<Node>, f64)> = vec![(None, 0.0); ret_num];
    let sorted = cxx_partial_sort_copy_by(&items, ret_num, |l, r| l.1 > r.1);
    for (slot, (v, x)) in topk.iter_mut().zip(sorted) {
        *slot = (Some(v), x);
    }
    let mut i = topk.len() - 1;
    while topk[i].1 == 0.0 && i != 0 {
        i -= 1;
    }
    topk.truncate(i + 1);
    Ok(topk
        .into_iter()
        .map(|(v, x)| (v.map_or(vertex0, |v| g.entity_of(v)), x))
        .collect())
}

impl<'g> Ppr<'_, 'g> {
    fn deg(&self, v: Node) -> u32 {
        self.out.deg[v as usize]
    }

    /// `residual / getSetOutSize(v)` (a zero degree divides by `0.0`).
    fn per_out(&self, residual: f64, v: Node) -> f64 {
        residual / f64::from(self.deg(v))
    }

    fn run(&mut self, uid: Node, ret_num: usize, k: i32) -> Result<V2Ppr<'g>> {
        let n = self.n;
        let nf = n as f64;
        let mut v2ppr = V2Ppr::new(self.g);
        // fwd_idx.first = reserve, fwd_idx.second = residual
        let mut reserve = IMap::new(n);
        let mut residual = IMap::new(n);
        let mut upper = IMap::all_keys(n, 1.0);
        let mut lower = IMap::all_keys(n, 0.0);
        let mut topk_filter = IMap::new(n);

        let num_pred_edges = self.out.edges as i32;
        let ppr_decay_alpha = 0.77;
        let pfail = 1.0 / nf / nf / nf.ln();
        let mut delta = 1.0 / 4.0;
        let epsilon = 0.5;
        let rmax_scale = 1.0;
        let alpha = 0.2;
        let min_delta = 1.0 / nf;
        let threshold =
            (1.0 - ppr_decay_alpha) / pow(500.0, ppr_decay_alpha) / pow(nf, 1.0 - ppr_decay_alpha);
        let log2pfail = (2.0 / pfail).ln();
        let lowest_delta_rmax =
            epsilon * (min_delta / 3.0 / f64::from(num_pred_edges) / log2pfail).sqrt();
        let mut rsum = 1.0;

        let mut forward_from: Vec<(Node, i32)> = vec![(uid, 0)];
        residual.insert(uid, rsum);
        let mut zero_ppr_upper_bound = 1.0;

        while delta >= min_delta {
            self.ctx.check_cancel()?;
            let mut rmax = epsilon * (delta / 3.0 / f64::from(num_pred_edges) / log2pfail).sqrt();
            rmax *= rmax_scale;
            let omega = (2.0 + epsilon) * log2pfail / delta / epsilon / epsilon;
            if self.deg(uid) == 0 {
                // (upstream also sets `rsum = 0.0`, never read afterwards)
                reserve.insert(uid, 1.0);
                compute_ppr_with_reserve(&reserve, &mut v2ppr);
                break;
            }
            self.forward_push(
                uid,
                &mut rsum,
                rmax,
                lowest_delta_rmax,
                &mut forward_from,
                &mut reserve,
                &mut residual,
                alpha,
                k,
            );
            self.walks_with_bounds(
                rsum,
                &reserve,
                &residual,
                &mut v2ppr,
                delta < threshold,
                alpha,
                pfail,
                &mut zero_ppr_upper_bound,
                omega,
                &mut upper,
                &mut lower,
            )?;
            if if_stop(
                ret_num,
                delta,
                threshold,
                epsilon,
                &mut topk_filter,
                &upper,
                &lower,
                &mut v2ppr,
                &mut self.out.temp_ppr,
            ) || delta <= min_delta
            {
                break;
            }
            delta = cxx_max(min_delta, delta / 2.0);
        }
        let _ = zero_ppr_upper_bound;
        Ok(v2ppr)
    }

    /// `forward_local_update_linear_topk` (PathQueryHandler.cpp:3156-3253).
    #[allow(clippy::too_many_arguments)]
    fn forward_push(
        &self,
        s: Node,
        rsum: &mut f64,
        rmax: f64,
        lowest_rmax: f64,
        forward_from: &mut Vec<(Node, i32)>,
        reserve: &mut IMap,
        residual: &mut IMap,
        alpha: f64,
        k: i32,
    ) {
        let n = self.n;
        let myeps = rmax;
        let mut in_forward = vec![false; n];
        let mut in_next_forward = vec![false; n];
        let mut next_forward_from: Vec<(Node, i32)> = Vec::new();
        for &(v, _) in forward_from.iter() {
            in_forward[v as usize] = true;
        }
        let hop_ok = |level: i32| k == -1 || level < k;
        let mut i = 0usize;
        while i < forward_from.len() {
            let (v, level) = forward_from[i];
            i += 1;
            in_forward[v as usize] = false;
            if self.per_out(residual.get(v), v) >= myeps {
                let out_neighbor = self.deg(v);
                let v_residue = residual.get(v);
                residual.set(v, 0.0);
                if !reserve.exist(v) {
                    reserve.insert(v, v_residue * alpha);
                } else {
                    reserve.set(v, reserve.get(v) + v_residue * alpha);
                }
                *rsum -= v_residue * alpha;
                if out_neighbor == 0 {
                    residual.set(s, residual.get(s) + v_residue * (1.0 - alpha));
                    if self.deg(s) > 0
                        && !in_forward[s as usize]
                        && self.per_out(residual.get(s), s) >= myeps
                    {
                        forward_from.push((s, 0));
                        in_forward[s as usize] = true;
                    } else if !in_next_forward[s as usize]
                        && self.per_out(residual.get(s), s) >= lowest_rmax
                    {
                        next_forward_from.push((s, 0));
                        in_next_forward[s as usize] = true;
                    }
                    continue;
                }
                let avg_push_residual = ((1.0 - alpha) * v_residue) / f64::from(out_neighbor);
                let (g, f) = (self.g, self.f);
                g.for_each_out(v, f, |next, _| {
                    if !residual.exist(next) {
                        residual.insert(next, avg_push_residual);
                    } else {
                        residual.set(next, residual.get(next) + avg_push_residual);
                    }
                    let r = self.per_out(residual.get(next), next);
                    if !in_forward[next as usize] && r >= myeps && hop_ok(level) {
                        forward_from.push((next, level + 1));
                        in_forward[next as usize] = true;
                    } else if !in_next_forward[next as usize] && r >= lowest_rmax && hop_ok(level) {
                        next_forward_from.push((next, level + 1));
                        in_next_forward[next as usize] = true;
                    }
                });
            } else if !in_next_forward[v as usize]
                && self.per_out(residual.get(v), v) >= lowest_rmax
            {
                next_forward_from.push((v, level));
                in_next_forward[v as usize] = true;
            }
        }
        *forward_from = next_forward_from;
    }

    /// `random_walk(start, alpha, pred_set)` (PathQueryHandler.cpp:3118-3154).
    fn random_walk(&mut self, start: Node, alpha: f64) -> Node {
        let mut cur = start;
        if self.deg(start) == 0 {
            return start;
        }
        loop {
            if self.rng.rand_unit() <= alpha {
                return cur;
            }
            let d = self.deg(cur);
            if d != 0 {
                let mut idx = (self.rng.rand() % d as i32) as usize;
                for &p in self.f.preds() {
                    let nb = self.g.out_nbrs(cur, p);
                    if nb.len() <= idx {
                        idx -= nb.len();
                    } else {
                        cur = nb[idx];
                        break;
                    }
                }
            } else {
                cur = start;
            }
        }
    }

    /// `compute_ppr_with_fwdidx_topk_with_bound` (PathQueryHandler.cpp:3268-3311).
    #[allow(clippy::too_many_arguments)]
    fn walks_with_bounds(
        &mut self,
        check_rsum: f64,
        reserve: &IMap,
        residual: &IMap,
        v2ppr: &mut V2Ppr<'_>,
        below_threshold: bool,
        alpha: f64,
        pfail: f64,
        zero_ppr_upper_bound: &mut f64,
        omega: f64,
        upper: &mut IMap,
        lower: &mut IMap,
    ) -> Result<()> {
        compute_ppr_with_reserve(reserve, v2ppr);
        if check_rsum == 0.0 {
            return Ok(());
        }
        let num_random_walk = (omega * check_rsum) as i64;
        let mut real_num_rand_walk: i64 = 0;
        for &source in &residual.occur {
            let r = residual.get(source);
            let num_s_rw = (r / check_rsum * num_random_walk as f64).ceil() as i64;
            let a_s = r / check_rsum * num_random_walk as f64 / num_s_rw as f64;
            real_num_rand_walk += num_s_rw;
            let ppr_incre = a_s * check_rsum / num_random_walk as f64;
            for _ in 0..num_s_rw {
                self.walks += 1;
                if self.walks & 0xfff == 0 {
                    self.ctx.check_cancel()?;
                }
                let des = self.random_walk(source, alpha);
                match v2ppr.get(des) {
                    None => v2ppr.set(des, ppr_incre),
                    Some(x) => v2ppr.set(des, x + ppr_incre),
                }
            }
        }
        if below_threshold {
            self.set_ppr_bounds(
                reserve,
                check_rsum,
                real_num_rand_walk,
                v2ppr,
                pfail,
                zero_ppr_upper_bound,
                upper,
                lower,
            );
        } else {
            *zero_ppr_upper_bound =
                calculate_lambda(check_rsum, pfail, *zero_ppr_upper_bound, real_num_rand_walk);
        }
        Ok(())
    }

    /// `set_ppr_bounds` (PathQueryHandler.cpp:3025-3108).
    #[allow(clippy::too_many_arguments)]
    fn set_ppr_bounds(
        &self,
        reserve_map: &IMap,
        rsum: f64,
        total_rw_num: i64,
        v2ppr: &mut V2Ppr<'_>,
        pfail: f64,
        zero_ppr_upper_bound: &mut f64,
        upper: &mut IMap,
        lower: &mut IMap,
    ) {
        let nf = self.n as f64;
        let min_ppr = 1.0 / nf;
        let sqrt_min_ppr = (1.0 / nf).sqrt();
        let epsilon_v_div = (2.67 * rsum * (2.0 / pfail).ln() / total_rw_num as f64).sqrt();
        let default_epsilon_v = epsilon_v_div / sqrt_min_ppr;
        *zero_ppr_upper_bound = calculate_lambda(rsum, pfail, *zero_ppr_upper_bound, total_rw_num);
        for nodeid in v2ppr.nodes() {
            let p = v2ppr.at(nodeid);
            if p <= 0.0 {
                continue;
            }
            let mut reserve = 0.0;
            if reserve_map.exist(nodeid) {
                reserve = reserve_map.get(nodeid);
            }
            let epsilon_a = if upper.exist(nodeid) {
                if upper.get(nodeid) > reserve {
                    calculate_lambda(rsum, pfail, upper.get(nodeid) - reserve, total_rw_num)
                } else {
                    calculate_lambda(rsum, pfail, 1.0 - reserve, total_rw_num)
                }
            } else {
                calculate_lambda(rsum, pfail, 1.0 - reserve, total_rw_num)
            };
            let ub_eps_a = p + epsilon_a;
            let mut lb_eps_a = p - epsilon_a;
            if lb_eps_a.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
                lb_eps_a = 0.0;
            }
            let mut epsilon_v = default_epsilon_v;
            if reserve_map.exist(nodeid) && reserve_map.get(nodeid) > min_ppr {
                if lower.exist(nodeid) {
                    reserve = cxx_max(reserve, lower.get(nodeid));
                }
                epsilon_v = epsilon_v_div / reserve.sqrt();
            } else if lower.get(nodeid) > 0.0 {
                epsilon_v = epsilon_v_div / lower.get(nodeid).sqrt();
            }
            let (mut ub_eps_v, mut lb_eps_v) = (1.0, 0.0);
            if 1.0 - epsilon_v > 0.0 {
                ub_eps_v = p / (1.0 - epsilon_v);
                lb_eps_v = p / (1.0 + epsilon_v);
            }
            let up_bound = cxx_min(cxx_min(ub_eps_a, ub_eps_v), 1.0);
            let low_bound = cxx_max(cxx_max(lb_eps_a, lb_eps_v), reserve);
            if up_bound > 0.0 {
                upper.insert(nodeid, up_bound);
            }
            if low_bound >= 0.0 {
                lower.insert(nodeid, low_bound);
            }
        }
    }
}

/// `compute_ppr_with_reserve` (PathQueryHandler.cpp:3255-3266).
fn compute_ppr_with_reserve(reserve: &IMap, v2ppr: &mut V2Ppr<'_>) {
    for &v in &reserve.occur {
        let r = reserve.get(v);
        if r != 0.0 {
            v2ppr.set(v, r);
        }
    }
}

/// `calculate_lambda` (PathQueryHandler.cpp:3110-3116).
fn calculate_lambda(rsum: f64, pfail: f64, upper_bound: f64, total_rw_num: i64) -> f64 {
    let l = (2.0 / pfail).ln();
    let t = total_rw_num as f64;
    1.0 / 3.0 * l * rsum / t
        + (4.0 / 9.0 * l * l * rsum * rsum
            + (8i64.wrapping_mul(total_rw_num)) as f64 * l * rsum * upper_bound)
            .sqrt()
            / 2.0
            / t
}

/// `kth_ppr(v2ppr, retNum)` (SSPPR.cpp:5-20), including its quirks:
///
/// * every slot reads `v2ppr[(int)value]`, inserting that vertex while the
///   map is being iterated (iteration then continues from the current
///   element in the map's new order; a slot past the original size is an
///   out-of-bounds write into the buffer's spare capacity);
/// * `temp_ppr` is a function-`static` vector: `clear()` + `resize(size)`
///   zero only `[0, size)`, so with `retNum > size` the result is the stale
///   value an earlier call left at index `retNum - 1` (`0.0` beyond the
///   buffer's capacity, where upstream reads foreign heap memory). The
///   buffer lives in [`Session`], i.e. it persists across the sources of
///   one call (upstream: for the life of the process).
fn kth_ppr(v2ppr: &mut V2Ppr<'_>, ret_num: usize, temp: &mut Vec<f64>) -> f64 {
    let size = v2ppr.len();
    // temp_ppr.clear(); temp_ppr.resize(size): a reallocation (size beyond
    // the capacity) gets a fresh buffer of exactly `size` elements.
    if size > temp.len() {
        *temp = vec![0.0; size];
    } else {
        temp[..size].fill(0.0);
    }
    let cap = temp.len();
    let mut order = v2ppr.nodes();
    let mut idx = 0usize;
    let mut i = 0usize;
    while idx < order.len() {
        let cur = order[idx];
        let value = v2ppr.get(cur).unwrap_or(0.0);
        // C++ `(int)double` truncation; the values are PPR estimates in
        // [0, ~1], i.e. vertex 0 (or 1).
        let as_vertex = value as i64;
        let x = if as_vertex >= 0 && (as_vertex as usize) < v2ppr.val.len() {
            let v = as_vertex as Node;
            let before = v2ppr.len();
            let x = v2ppr.at(v);
            if v2ppr.len() != before {
                order = v2ppr.nodes();
                idx = order.iter().position(|&n| n == cur).unwrap_or(idx);
            }
            x
        } else {
            0.0
        };
        if i < cap {
            temp[i] = x;
        }
        i += 1;
        idx += 1;
    }
    if ret_num == 0 {
        return 0.0;
    }
    let nth = ret_num - 1;
    nth_element_desc(&mut temp[..size], nth);
    temp.get(nth).copied().unwrap_or(0.0)
}

/// libstdc++ `std::nth_element(first, first + nth, last, [](x, y){ return x > y; })`
/// (`__introselect`), on `v`; `nth >= v.len()` (out of range upstream) runs
/// the same partition steps without the final out-of-range accesses.
fn nth_element_desc(v: &mut [f64], nth: usize) {
    let less = |a: f64, b: f64| a > b;
    let len = v.len();
    if len == 0 || nth == len {
        return;
    }
    let (mut first, mut last) = (0usize, len);
    let mut depth = 2 * (usize::BITS - 1 - len.leading_zeros()) as usize;
    while last - first > 3 {
        if depth == 0 {
            if nth < len {
                heap_select(v, first, nth + 1, last, less);
                v.swap(first, nth);
            }
            return;
        }
        depth -= 1;
        // __unguarded_partition_pivot
        let mid = first + (last - first) / 2;
        let (a, b, c) = (first + 1, mid, last - 1);
        let pick = if less(v[a], v[b]) {
            if less(v[b], v[c]) {
                b
            } else if less(v[a], v[c]) {
                c
            } else {
                a
            }
        } else if less(v[a], v[c]) {
            a
        } else if less(v[b], v[c]) {
            c
        } else {
            b
        };
        v.swap(first, pick);
        let pivot = v[first];
        let (mut lo, mut hi) = (first + 1, last);
        let cut = loop {
            while less(v[lo], pivot) {
                lo += 1;
            }
            hi -= 1;
            while less(pivot, v[hi]) {
                hi -= 1;
            }
            if lo >= hi {
                break lo;
            }
            v.swap(lo, hi);
            lo += 1;
        };
        if cut <= nth {
            first = cut;
        } else {
            last = cut;
        }
    }
    // __insertion_sort(first, last)
    for i in first + 1..last {
        let val = v[i];
        if less(val, v[first]) {
            v.copy_within(first..i, first + 1);
            v[first] = val;
        } else {
            let mut j = i;
            while less(val, v[j - 1]) {
                v[j] = v[j - 1];
                j -= 1;
            }
            v[j] = val;
        }
    }
}

/// libstdc++ `__heap_select(first, middle, last)` with `less`.
fn heap_select(v: &mut [f64], first: usize, middle: usize, last: usize, less: impl Fn(f64, f64) -> bool + Copy) {
    let len = middle - first;
    if len > 1 {
        let mut parent = (len - 2) / 2;
        loop {
            let value = v[first + parent];
            adjust_heap(v, first, parent, len, value, less);
            if parent == 0 {
                break;
            }
            parent -= 1;
        }
    }
    for i in middle..last {
        if less(v[i], v[first]) {
            // __pop_heap(first, middle, i)
            let value = v[i];
            v[i] = v[first];
            adjust_heap(v, first, 0, len, value, less);
        }
    }
}

/// libstdc++ `__adjust_heap` + `__push_heap`.
fn adjust_heap(v: &mut [f64], first: usize, mut hole: usize, len: usize, value: f64, less: impl Fn(f64, f64) -> bool) {
    let top = hole;
    let mut child = hole;
    while len > 0 && child < (len - 1) / 2 {
        child = 2 * (child + 1);
        if less(v[first + child], v[first + child - 1]) {
            child -= 1;
        }
        v[first + hole] = v[first + child];
        hole = child;
    }
    if len & 1 == 0 && len >= 2 && child == (len - 2) / 2 {
        child = 2 * (child + 1);
        v[first + hole] = v[first + child - 1];
        hole = child - 1;
    }
    let mut parent = if hole > 0 { (hole - 1) / 2 } else { 0 };
    while hole > top && less(v[first + parent], value) {
        v[first + hole] = v[first + parent];
        hole = parent;
        parent = if hole > 0 { (hole - 1) / 2 } else { 0 };
    }
    v[first + hole] = value;
}

/// `if_stop` (SSPPR.cpp:22-94).
#[allow(clippy::too_many_arguments)]
fn if_stop(
    ret_num: usize,
    delta: f64,
    threshold: f64,
    epsilon: f64,
    topk_filter: &mut IMap,
    upper: &IMap,
    lower: &IMap,
    v2ppr: &mut V2Ppr<'_>,
    temp_ppr: &mut Vec<f64>,
) -> bool {
    if kth_ppr(v2ppr, ret_num, temp_ppr) >= 2.0 * delta {
        return true;
    }
    if delta >= threshold {
        return false;
    }
    let error = 1.0 + epsilon;
    let error_2 = 1.0 + epsilon;
    topk_filter.clean();
    let temp_bounds: Vec<(Node, f64)> = lower.occur.iter().map(|&v| (v, lower.get(v))).collect();
    let mut topk: Vec<(Node, f64)> = vec![(0, 0.0); ret_num];
    let sorted = cxx_partial_sort_copy_by(&temp_bounds, ret_num, |l, r| l.1 > r.1);
    for (slot, x) in topk.iter_mut().zip(sorted) {
        *slot = x;
    }
    for &(v, _) in &topk {
        topk_filter.insert(v, 1.0);
        let ratio = upper.get(v) / lower.get(v);
        if ratio > error_2 {
            return false;
        }
    }
    let low_bound_k = topk[ret_num - 1].1;
    if low_bound_k <= delta {
        return false;
    }
    for &v in &upper.occur {
        if topk_filter.exist(v) || v2ppr.at(v) <= 0.0 {
            continue;
        }
        let upper_temp = upper.get(v);
        let lower_temp = lower.get(v);
        #[allow(clippy::neg_cmp_op_on_partial_ord)] // literal port (NaN behaviour)
        if upper_temp > low_bound_k * error
            && !(upper_temp > (1.0 + epsilon) / (1.0 - epsilon) * lower_temp)
        {
            return false;
        }
    }
    true
}
