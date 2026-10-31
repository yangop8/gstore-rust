//! Literal port of upstream's `MaxKPX` namespace (PathQueryHandler.cpp:298-3017):
//! the maximum k-plex search behind `maximumKplex` (`Graph::read` +
//! `Graph::search`, a kPlexS-style algorithm: max-degree heuristic,
//! degeneracy ordering, core shrinking, oriented triangle counting,
//! core-truss co-pruning and a branch-and-bound `Searcher` per ego network)
//! and the `(alpha, beta)` core-truss peeling behind `coreTruss`
//! (`Graph::peel`).
//!
//! The port keeps upstream's data layout (CSR arrays with `pstart` / `pend`,
//! an `int` per vertex, `long` edge pointers) and control flow one to one,
//! including behaviour that is clearly unintended but decides the output:
//!
//! * `search` starts from `kplex.resize(2k - 2)`: a fake solution of `2k - 2`
//!   copies of vertex 0, returned as such when nothing larger is found;
//! * in the search loop `ids` *is* `Qv` (`int *ids = Qv`), and
//!   `subgraph_prune(ids, …, Qv, …)` maps the pruned subgraph back through
//!   the array it just overwrote (`Qv[i] = ids[Qv[i]]`);
//! * `degen` on an ego network may replace `kplex` with ids local to that
//!   subgraph, which are later read as global ids;
//! * `peel` pops `n` times from a heap that holds fewer vertices once some
//!   were peeled: each failed pop reads the uninitialised `int u`, which
//!   g++ -O2 materialises as 0, so (shrunk) vertex 0 repeats at the end of
//!   the output while its degree is non-zero;
//! * a vertex can enter the peeling queue `Qv` twice, overflowing its `n`
//!   slots (heap corruption upstream, fatal or not depending on the
//!   allocator); the overflow is kept;
//! * upstream `assert`s that fail on some graphs (gStore's default build
//!   keeps them, so the server aborts) are reported as errors.
//!
//! Vertices are CSR nodes (upstream's raw ids; identical for dense entity
//! ids). Upstream `assert`s are active in its default build; where one
//! would abort, the port returns an error or the documented fallback.

// A literal port: index loops mirror the C++ (and its aliasing) on purpose.
#![allow(clippy::needless_range_loop, clippy::manual_memcpy)]

use std::collections::VecDeque;

use super::super::{CsrGraph, GFunCtx, Node, PredFilter};
use crate::error::{GStoreError, Result};

type Ept = i64;

/// Largest ego network (vertices) the `Searcher` loads: it allocates an
/// `n × n` byte matrix and an `n × n` `int` matrix (upstream has no limit
/// and is killed by the OOM killer instead).
const MAX_SEARCHER_N: i32 = 16_384;

/// Largest fake solution `2k - 2` accepted (upstream allocates and prints it).
pub(super) const MAX_FAKE_KPLEX: i64 = 1 << 20;

fn kerr(msg: impl Into<String>) -> GStoreError {
    GStoreError::Query(msg.into())
}

/// C++ `int < size_t` comparisons convert the `int` to `size_t`.
#[inline]
fn sz(x: i64) -> u64 {
    x as u64
}

// ---------------------------------------------------------------------------
// LinearHeap
// ---------------------------------------------------------------------------

struct LinearHeap {
    n: i32,
    key_cap: i32,
    min_key: i32,
    max_key: i32,
    key_s: Vec<i32>,
    head_s: Vec<i32>,
    pre_s: Vec<i32>,
    next_s: Vec<i32>,
}

impl LinearHeap {
    fn new(n: i32, key_cap: i32) -> LinearHeap {
        LinearHeap {
            n,
            key_cap,
            min_key: key_cap,
            max_key: key_cap,
            key_s: Vec::new(),
            head_s: Vec::new(),
            pre_s: Vec::new(),
            next_s: Vec::new(),
        }
    }

    fn init(&mut self, n_: i32, key_cap_: i32, id_s: &[i32], key_in: &[i32]) {
        let n = self.n.max(0) as usize;
        if self.key_s.is_empty() {
            self.key_s = vec![0; n];
            self.pre_s = vec![0; n];
            self.next_s = vec![0; n];
        }
        let heads = (self.key_cap + 1).max(key_cap_ + 1).max(0) as usize;
        if self.head_s.len() < heads {
            self.head_s.resize(heads, 0);
        }
        self.min_key = key_cap_;
        self.max_key = key_cap_;
        for i in 0..=key_cap_ {
            self.head_s[i as usize] = self.n;
        }
        for i in 0..n_ as usize {
            let id = id_s[i];
            let key = key_in[id as usize];
            let (idu, ku) = (id as usize, key as usize);
            self.key_s[idu] = key;
            self.pre_s[idu] = self.n;
            self.next_s[idu] = self.head_s[ku];
            if self.head_s[ku] != self.n {
                let h = self.head_s[ku] as usize;
                self.pre_s[h] = id;
            }
            self.head_s[ku] = id;
            if key < self.min_key {
                self.min_key = key;
            }
        }
    }

    fn pop_min(&mut self) -> Option<(i32, i32)> {
        while self.min_key <= self.max_key && self.head_s[self.min_key as usize] == self.n {
            self.min_key += 1;
        }
        if self.min_key > self.max_key {
            return None;
        }
        let id = self.head_s[self.min_key as usize];
        let key = self.min_key;
        self.key_s[id as usize] = self.key_cap + 1;
        let nx = self.next_s[id as usize];
        self.head_s[self.min_key as usize] = nx;
        if nx != self.n {
            self.pre_s[nx as usize] = self.n;
        }
        Some((id, key))
    }

    fn unlink(&mut self, id: i32) {
        let idu = id as usize;
        if self.pre_s[idu] == self.n {
            let k = self.key_s[idu] as usize;
            self.head_s[k] = self.next_s[idu];
            if self.next_s[idu] != self.n {
                let nx = self.next_s[idu] as usize;
                self.pre_s[nx] = self.n;
            }
        } else {
            let pid = self.pre_s[idu];
            self.next_s[pid as usize] = self.next_s[idu];
            if self.next_s[idu] != self.n {
                let nx = self.next_s[idu] as usize;
                self.pre_s[nx] = pid;
            }
        }
    }

    fn decrement(&mut self, id: i32, dec: i32) -> i32 {
        let idu = id as usize;
        if self.key_s[idu] > self.key_cap {
            return 0;
        }
        self.unlink(id);
        self.key_s[idu] -= dec;
        let key = self.key_s[idu];
        self.pre_s[idu] = self.n;
        self.next_s[idu] = self.head_s[key as usize];
        if self.head_s[key as usize] != self.n {
            let h = self.head_s[key as usize] as usize;
            self.pre_s[h] = id;
        }
        self.head_s[key as usize] = id;
        if key < self.min_key {
            self.min_key = key;
        }
        key
    }

    fn del(&mut self, id: i32) {
        if self.key_s[id as usize] > self.key_cap {
            return;
        }
        self.unlink(id);
    }
}

// ---------------------------------------------------------------------------
// Searcher (branch and bound on one ego network)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Searcher<'a> {
    n: i32,
    matrix: Vec<u8>,
    comm_neis: Vec<i32>,
    nei_in_g: Vec<i32>,
    nei_in_p: Vec<i32>,
    k: i32,
    best_sol: Vec<i32>,
    best_sz: i32,
    neighbors: Vec<i32>,
    nonneighbors: Vec<i32>,
    add_list: Vec<i32>,
    add_list_sz: i32,
    removed: Vec<(i32, i32)>,
    remove_sz: i32,
    pc: Vec<i32>,
    pc_rid: Vec<i32>,
    qv: VecDeque<i32>,
    qe: VecDeque<(i32, i32)>,
    lvl_id: Vec<i32>,
    p_end: i32,
    c_end: i32,
    lvl: i32,
    vp: Vec<(i32, i32)>,
    enable: bool,
    enable_input: bool,
    ctx: Option<GFunCtx<'a>>,
    steps: u64,
    cancelled: bool,
}

impl<'a> Searcher<'a> {
    fn new() -> Searcher<'a> {
        Searcher {
            enable: true,
            enable_input: true,
            ..Searcher::default()
        }
    }

    #[inline]
    fn mx(&self, a: i32, b: i32) -> bool {
        self.matrix[(a * self.n + b) as usize] != 0
    }

    #[inline]
    fn cn(&mut self, a: i32, b: i32) -> &mut i32 {
        let n = self.n;
        &mut self.comm_neis[(a * n + b) as usize]
    }

    fn push_removed(&mut self, p: (i32, i32)) {
        if self.removed.len() as i32 == self.remove_sz {
            self.removed.push(p);
        } else {
            self.removed[self.remove_sz as usize] = p;
        }
        self.remove_sz += 1;
    }

    fn load(&mut self, n: i32, vp: &[(i32, i32)]) {
        self.n = n;
        let nu = n as usize;
        self.matrix = vec![0; nu * nu];
        self.comm_neis = vec![0; nu * nu];
        self.nei_in_g = vec![0; nu];
        self.nei_in_p = vec![0; nu];
        self.best_sol = vec![0; nu];
        self.pc = vec![0; nu];
        self.pc_rid = vec![0; nu];
        self.neighbors = vec![0; nu];
        self.nonneighbors = vec![0; nu];
        self.lvl_id = vec![0; nu];
        for &(a, b) in vp {
            self.nei_in_g[a as usize] += 1;
            self.nei_in_g[b as usize] += 1;
            self.matrix[(a * n + b) as usize] = 1;
            self.matrix[(b * n + a) as usize] = 1;
        }
    }

    fn run(&mut self, k: i32, res: &mut Vec<i32>) {
        self.k = k;
        self.best_sz = res.len() as i32;
        self.p_end = 0;
        self.c_end = 0;
        self.lvl = 0;
        self.init();
        if self.n > 0
            && self.pc_rid[0] < self.c_end
            && !self.move_c2p_r(0)
            && self.c_end >= self.best_sz
        {
            self.enable = self.enable_input;
            self.lvl += 1;
            self.reduce(0, 0);
            self.lvl -= 1;
            self.enable = true;
        }
        if self.best_sz as usize > res.len() {
            res.clear();
            res.extend_from_slice(&self.best_sol[..self.best_sz as usize]);
        }
    }

    fn init(&mut self) {
        self.qv.clear();
        self.qe.clear();
        let n = self.n;
        let nu = n as usize;
        // seq = neighbors, core = nonneighbors, vis = PC
        let mut vis = vec![0i32; nu];
        let (mut max_core, mut idx) = (0, n);
        for i in 0..n {
            let (mut u, mut min_degree) = (0, n);
            for j in 0..n {
                if vis[j as usize] == 0 && self.nei_in_g[j as usize] < min_degree {
                    u = j;
                    min_degree = self.nei_in_g[j as usize];
                }
            }
            if min_degree > max_core {
                max_core = min_degree;
            }
            self.nonneighbors[u as usize] = max_core;
            self.neighbors[i as usize] = u;
            vis[u as usize] = 1;
            if idx == n && min_degree + self.k >= n - i {
                idx = i;
            }
            for j in 0..n {
                if vis[j as usize] == 0 && self.mx(u, j) {
                    self.nei_in_g[j as usize] -= 1;
                }
            }
        }
        // (PC was used as `vis`; it is fully rewritten below.)
        if n - idx > self.best_sz {
            self.best_sz = n - idx;
            for i in idx..n {
                self.best_sol[(i - idx) as usize] = self.neighbors[i as usize];
            }
        }
        self.nei_in_p.iter_mut().for_each(|x| *x = 0);
        self.c_end = 0;
        for i in 0..nu {
            self.pc_rid[i] = n;
            self.pc[i] = 1; // upstream's `vis` content (every vertex visited)
        }
        for i in 0..n {
            if self.nonneighbors[i as usize] + self.k > self.best_sz {
                self.pc[self.c_end as usize] = i;
                self.pc_rid[i as usize] = self.c_end;
                self.c_end += 1;
            }
        }
        if nu == 0 || self.pc_rid[0] == n {
            self.c_end = 0;
            return;
        }
        for i in 0..self.c_end {
            let u = self.pc[i as usize];
            let mut cnt = 0;
            for j in 0..self.c_end {
                if self.mx(u, self.pc[j as usize]) {
                    cnt += 1;
                }
            }
            self.nei_in_g[u as usize] = cnt;
        }
        for i in 0..self.c_end {
            let mut neighbors_n = 0usize;
            let pi = self.pc[i as usize];
            for j in 0..self.c_end {
                let pj = self.pc[j as usize];
                if self.mx(pi, pj) {
                    self.neighbors[neighbors_n] = pj;
                    neighbors_n += 1;
                }
            }
            for j in 0..neighbors_n {
                for kk in j + 1..neighbors_n {
                    let (a, b) = (self.neighbors[j], self.neighbors[kk]);
                    *self.cn(a, b) += 1;
                    *self.cn(b, a) += 1;
                }
            }
        }
        self.lvl_id.iter_mut().for_each(|x| *x = 0);
        for i in 0..self.c_end {
            let p = self.pc[i as usize];
            self.lvl_id[p as usize] = n;
        }
        for i in 0..self.c_end {
            for j in i + 1..self.c_end {
                let (a, b) = (self.pc[i as usize], self.pc[j as usize]);
                if self.mx(a, b) && self.upper_bound_based_prune(0, a, b) {
                    self.qe.push_back((a, b));
                }
            }
        }
        self.remove_sz = 0;
        if self.ctcp() {
            self.c_end = 0;
        }
    }

    fn check_nei(&self, u: i32) -> bool {
        let mut not_sat = 0;
        for i in self.p_end..self.c_end {
            let ele = self.pc[i as usize];
            if u != ele && !self.mx(u, ele) && self.k + self.nei_in_g[ele as usize] < self.c_end {
                if not_sat != 0 {
                    return false;
                }
                not_sat += 1;
            }
        }
        true
    }

    fn check_triangle(&self, u: i32) -> bool {
        let mut not_nei: Vec<i32> = Vec::new();
        for i in self.p_end..self.c_end {
            let ele = self.pc[i as usize];
            if u != ele && !self.mx(u, ele) {
                not_nei.push(ele);
            }
        }
        // upstream reads NotNei[0] / NotNei[1] unchecked (callers ensure 2).
        match (not_nei.first(), not_nei.get(1)) {
            (Some(&a), Some(&b)) => !self.mx(a, b),
            _ => false,
        }
    }

    fn add_list_set(&mut self, end_idx: &mut i32, v: i32) {
        if self.add_list_sz == *end_idx {
            self.add_list.push(v);
            *end_idx += 1;
            self.add_list_sz += 1;
        } else {
            self.add_list[*end_idx as usize] = v;
            *end_idx += 1;
        }
    }

    fn branch(&mut self, beg_idx: i32, end_idx: &mut i32) {
        *end_idx = beg_idx;
        let mut minnei = 0x3f3f3f3f;
        let mut pivot = -1;
        for i in self.p_end..self.c_end {
            let v = self.pc[i as usize];
            if !self.mx(0, v) && self.cond(v) {
                self.add_list_set(end_idx, v);
                return;
            }
            if self.nei_in_g[v as usize] < minnei {
                minnei = self.nei_in_g[v as usize];
                pivot = v;
            }
        }
        if pivot < 0 {
            return;
        }
        for i in self.p_end..self.c_end {
            let v = self.pc[i as usize];
            if !self.mx(pivot, v) {
                self.add_list_set(end_idx, v);
            }
        }
        let nei_in_g = &self.nei_in_g;
        super::super::cxx::cxx_sort_by(
            &mut self.add_list[beg_idx as usize..*end_idx as usize],
            |&a, &b| nei_in_g[a as usize] > nei_in_g[b as usize],
        );
    }

    fn reduce(&mut self, beg_idx: i32, mut end_idx: i32) {
        self.steps += 1;
        if self.steps & 0xffff == 0 && self.ctx.is_some_and(|c| c.is_cancelled()) {
            self.cancelled = true;
        }
        if self.cancelled {
            return;
        }
        if self.c_end <= self.best_sz {
            return;
        }
        if self.p_end == self.c_end {
            self.best_sz = self.p_end;
            for i in 0..self.best_sz as usize {
                self.best_sol[i] = self.pc[i];
            }
            return;
        }
        let mut is_kplex = true;
        for i in 0..self.c_end {
            if self.nei_in_g[self.pc[i as usize] as usize] + self.k < self.c_end {
                is_kplex = false;
                break;
            }
        }
        if is_kplex {
            self.best_sz = self.c_end;
            for i in 0..self.best_sz as usize {
                self.best_sol[i] = self.pc[i];
            }
            return;
        } else if self.c_end == self.best_sz + 1 {
            return;
        }
        let n = self.n;
        let mut pivot = n;
        for i in self.p_end..self.c_end {
            let v = self.pc[i as usize];
            let not_nei = self.c_end - self.nei_in_g[v as usize];
            if not_nei <= 2
                || (self.nei_in_p[v as usize] == self.p_end
                    && ((not_nei == 3 && self.check_triangle(v))
                        || (not_nei <= self.k + 1 && self.check_nei(v))))
            {
                pivot = v;
                break;
            }
        }
        if pivot == n {
            let cursor = self.bound();
            if cursor >= self.c_end {
                return;
            }
            if cursor + 1 == self.c_end {
                pivot = self.pc[cursor as usize];
            }
        }
        if pivot < n {
            let old_remove_sz = self.remove_sz;
            if !self.move_c2p_r(pivot) {
                self.lvl += 1;
                self.reduce(beg_idx, end_idx);
                self.lvl -= 1;
            }
            self.un_r(old_remove_sz);
            self.move_p2c();
            return;
        }
        if beg_idx >= end_idx || {
            let last = self.add_list[(end_idx - 1) as usize];
            let r = self.pc_rid[last as usize];
            r >= self.c_end || r < self.p_end
        } {
            self.branch(beg_idx, &mut end_idx);
        }
        end_idx -= 1;
        let pivot = self.add_list[end_idx as usize];
        let old_best_sz = self.best_sz;
        let old_remove_sz = self.remove_sz;
        if !self.move_c2p_r(pivot) {
            self.lvl += 1;
            self.reduce(beg_idx, end_idx);
            self.lvl -= 1;
        }
        self.un_r(old_remove_sz);
        self.qe.clear();
        self.push_removed((-1, pivot));
        let mut pruned = self.move_p2x_r();
        if !pruned && self.best_sz > old_best_sz {
            pruned = self.update_r();
        }
        if !pruned && !self.ctcp() {
            self.lvl += 1;
            self.reduce(end_idx, end_idx);
            self.lvl -= 1;
        }
        self.un_r(old_remove_sz);
    }

    fn mark(&mut self, v: i32) {
        self.lvl_id[v as usize] = self.lvl;
        self.qv.push_back(v);
    }

    fn move_c2p_r(&mut self, u: i32) -> bool {
        let r = self.pc_rid[u as usize];
        self.swap_pos(self.p_end, r);
        self.p_end += 1;
        let (mut neighbors_n, mut nonneighbors_n) = (0usize, 0usize);
        for i in 0..self.c_end {
            if i != self.p_end - 1 {
                let v = self.pc[i as usize];
                if self.mx(u, v) {
                    self.neighbors[neighbors_n] = v;
                    neighbors_n += 1;
                } else {
                    self.nonneighbors[nonneighbors_n] = v;
                    nonneighbors_n += 1;
                }
            }
        }
        for i in 0..neighbors_n {
            let v = self.neighbors[i];
            self.nei_in_p[v as usize] += 1;
        }
        let mut i = 0usize;
        while i < nonneighbors_n && self.pc_rid[self.nonneighbors[i] as usize] < self.p_end {
            i += 1;
        }
        if self.nei_in_p[u as usize] + self.k == self.p_end {
            // RR2: only neighbours of u in R can be candidates
            while i < nonneighbors_n {
                let v = self.nonneighbors[i];
                self.mark(v);
                i += 1;
            }
        } else {
            // RR1
            while i < nonneighbors_n {
                let v = self.nonneighbors[i];
                if self.p_end - self.nei_in_p[v as usize] >= self.k {
                    self.mark(v);
                }
                i += 1;
            }
        }
        // RR2
        let mut i = 0usize;
        while i < nonneighbors_n && self.pc_rid[self.nonneighbors[i] as usize] < self.p_end {
            let w = self.nonneighbors[i];
            if self.nei_in_p[w as usize] + self.k == self.p_end {
                for j in self.p_end..self.c_end {
                    let x = self.pc[j as usize];
                    if self.lvl_id[x as usize] > self.lvl && !self.mx(w, x) {
                        self.mark(x);
                    }
                }
            }
            i += 1;
        }
        if self.enable {
            // RR4
            for i in 0..nonneighbors_n {
                let v = self.nonneighbors[i];
                if self.pc_rid[v as usize] < self.p_end
                    || self.lvl_id[v as usize] == self.lvl
                    || self.mx(u, v)
                {
                    continue;
                }
                if self.upper_bound_based_prune(self.p_end, u, v) {
                    self.mark(v);
                }
            }
            // update cn(.,.)
            for i in 0..neighbors_n {
                for j in i + 1..neighbors_n {
                    let (a, b) = (self.neighbors[i], self.neighbors[j]);
                    *self.cn(a, b) -= 1;
                    *self.cn(b, a) -= 1;
                }
            }
            let mut new_n = 0usize;
            for i in 0..nonneighbors_n {
                let v = self.nonneighbors[i];
                if self.lvl_id[v as usize] > self.lvl {
                    self.nonneighbors[new_n] = v;
                    new_n += 1;
                }
            }
            let nonneighbors_n = new_n;
            for i in 1..nonneighbors_n {
                let w = self.nonneighbors[i];
                for j in 0..i {
                    let v = self.nonneighbors[j];
                    if !self.upper_bound_based_prune(self.p_end, v, w) {
                        continue;
                    }
                    if self.pc_rid[w as usize] < self.p_end {
                        return true; // UB2
                    } else if self.pc_rid[v as usize] >= self.p_end {
                        if self.mx(v, w) {
                            self.qe.push_back((v, w)); // RR5
                        }
                    } else {
                        self.mark(w); // RR4
                        break;
                    }
                }
            }
        }
        self.ctcp()
    }

    fn ctcp(&mut self) -> bool {
        if !self.enable {
            self.qe.clear();
        }
        while !self.qv.is_empty() || !self.qe.is_empty() {
            while let Some(u) = self.qv.pop_front() {
                self.c_end -= 1;
                let r = self.pc_rid[u as usize];
                self.swap_pos(r, self.c_end);
                let mut terminate = false;
                let mut neighbors_n = 0usize;
                for i in 0..self.c_end {
                    let w = self.pc[i as usize];
                    if self.mx(u, w) {
                        self.neighbors[neighbors_n] = w;
                        neighbors_n += 1;
                        self.nei_in_g[w as usize] -= 1;
                        if self.nei_in_g[w as usize] + self.k <= self.best_sz {
                            if i < self.p_end {
                                terminate = true; // UB1
                            } else if self.lvl_id[w as usize] > self.lvl {
                                self.mark(w); // RR3
                            }
                        }
                    }
                }
                if terminate {
                    for i in 0..neighbors_n {
                        let w = self.neighbors[i];
                        self.nei_in_g[w as usize] += 1;
                    }
                    self.lvl_id[u as usize] = self.n;
                    self.c_end += 1;
                    return true;
                }
                self.push_removed((-1, u));
                if self.enable {
                    for i in 1..neighbors_n {
                        let w = self.neighbors[i];
                        for j in 0..i {
                            let v = self.neighbors[j];
                            *self.cn(v, w) -= 1;
                            *self.cn(w, v) -= 1;
                            if !self.upper_bound_based_prune(self.p_end, v, w) {
                                continue;
                            }
                            if self.pc_rid[w as usize] < self.p_end {
                                terminate = true; // UB2
                            } else if self.pc_rid[v as usize] >= self.p_end {
                                if self.mx(v, w) {
                                    self.qe.push_back((v, w)); // RR5
                                }
                            } else if self.lvl_id[w as usize] > self.lvl {
                                self.mark(w); // RR4
                            }
                        }
                    }
                    if terminate {
                        return true;
                    }
                }
            }
            let Some(&(v, w)) = self.qe.front() else {
                break;
            };
            if self.enable {
                self.qe.pop_front();
                if self.lvl_id[v as usize] <= self.lvl
                    || self.lvl_id[w as usize] <= self.lvl
                    || !self.mx(v, w)
                {
                    continue;
                }
                if self.nei_in_g[v as usize] + self.k <= self.best_sz + 1 {
                    self.mark(v);
                }
                if self.nei_in_g[w as usize] + self.k <= self.best_sz + 1 {
                    self.mark(w);
                }
                if !self.qv.is_empty() {
                    continue;
                }
                let n = self.n;
                self.matrix[(v * n + w) as usize] = 0;
                self.matrix[(w * n + v) as usize] = 0;
                self.nei_in_g[v as usize] -= 1;
                self.nei_in_g[w as usize] -= 1;
                self.push_removed((v, w));
                for (a, b) in [(v, w), (w, v)] {
                    // for each neighbour x of a in C: cn(b, x)--, then prune
                    for i in 0..self.c_end {
                        let x = self.pc[i as usize];
                        if self.mx(a, x) {
                            *self.cn(b, x) -= 1;
                            *self.cn(x, b) -= 1;
                            if !self.upper_bound_based_prune(self.p_end, b, x) {
                                continue;
                            }
                            if i < self.p_end {
                                if self.lvl_id[b as usize] > self.lvl {
                                    self.mark(b);
                                }
                            } else if self.mx(b, x) {
                                self.qe.push_back((b, x));
                            }
                        }
                    }
                }
            }
        }
        false
    }

    fn un_r(&mut self, old_remove_sz: i32) {
        while let Some(u) = self.qv.pop_front() {
            self.lvl_id[u as usize] = self.n;
        }
        while self.remove_sz > old_remove_sz {
            self.remove_sz -= 1;
            let (v, w) = self.removed[self.remove_sz as usize];
            if v == -1 {
                let u = w;
                self.c_end += 1;
                self.lvl_id[u as usize] = self.n;
                let mut neighbors_n = 0usize;
                for i in 0..self.c_end {
                    let x = self.pc[i as usize];
                    if self.mx(u, x) {
                        self.neighbors[neighbors_n] = x;
                        neighbors_n += 1;
                        self.nei_in_g[x as usize] += 1;
                    }
                }
                if self.enable {
                    for i in 1..neighbors_n {
                        let w = self.neighbors[i];
                        for j in 0..i {
                            let v = self.neighbors[j];
                            *self.cn(v, w) += 1;
                            *self.cn(w, v) += 1;
                        }
                    }
                }
            } else {
                let n = self.n;
                self.matrix[(v * n + w) as usize] = 1;
                self.matrix[(w * n + v) as usize] = 1;
                self.nei_in_g[v as usize] += 1;
                self.nei_in_g[w as usize] += 1;
                for (a, b) in [(v, w), (w, v)] {
                    for i in 0..self.c_end {
                        let x = self.pc[i as usize];
                        if self.mx(a, x) {
                            *self.cn(b, x) += 1;
                            *self.cn(x, b) += 1;
                        }
                    }
                }
            }
        }
    }

    fn move_p2c(&mut self) {
        self.p_end -= 1;
        let u = self.pc[self.p_end as usize];
        let mut neighbors_n = 0usize;
        for i in 0..self.c_end {
            let x = self.pc[i as usize];
            if self.mx(u, x) {
                self.neighbors[neighbors_n] = x;
                neighbors_n += 1;
            }
        }
        for i in 0..neighbors_n {
            let x = self.neighbors[i];
            self.nei_in_p[x as usize] -= 1;
        }
        if self.enable {
            for i in 0..neighbors_n {
                let v = self.neighbors[i];
                for j in i + 1..neighbors_n {
                    let w = self.neighbors[j];
                    *self.cn(v, w) += 1;
                    *self.cn(w, v) += 1;
                }
            }
        }
    }

    fn move_p2x_r(&mut self) -> bool {
        let u = self.pc[(self.p_end - 1) as usize];
        self.p_end -= 1;
        self.c_end -= 1;
        self.swap_pos(self.p_end, self.c_end);
        self.lvl_id[u as usize] = self.lvl;
        let mut ret = false;
        let mut neighbors_n = 0usize;
        for i in 0..self.c_end {
            let x = self.pc[i as usize];
            if self.mx(u, x) {
                self.neighbors[neighbors_n] = x;
                neighbors_n += 1;
            }
        }
        for i in 0..neighbors_n {
            let x = self.neighbors[i];
            self.nei_in_p[x as usize] -= 1;
            self.nei_in_g[x as usize] -= 1;
            if self.nei_in_g[x as usize] + self.k <= self.best_sz {
                if self.pc_rid[x as usize] < self.p_end {
                    ret = true;
                } else {
                    self.mark(x);
                }
            }
        }
        if ret {
            return true;
        }
        if self.enable {
            for i in 1..neighbors_n {
                let w = self.neighbors[i];
                if self.lvl_id[w as usize] > self.lvl {
                    for j in 0..i {
                        let v = self.neighbors[j];
                        if !self.upper_bound_based_prune(self.p_end, v, w) {
                            continue;
                        }
                        if self.pc_rid[w as usize] < self.p_end {
                            return true;
                        } else if self.pc_rid[v as usize] >= self.p_end {
                            if self.mx(v, w) {
                                self.qe.push_back((v, w));
                            }
                        } else {
                            self.mark(w);
                            break;
                        }
                    }
                }
            }
        }
        false
    }

    fn update_r(&mut self) -> bool {
        for i in 0..self.p_end {
            if self.nei_in_g[self.pc[i as usize] as usize] + self.k <= self.best_sz {
                return true;
            }
        }
        if self.enable {
            for i in 0..self.p_end {
                for j in i + 1..self.p_end {
                    if self.upper_bound_based_prune(
                        self.p_end,
                        self.pc[i as usize],
                        self.pc[j as usize],
                    ) {
                        return true;
                    }
                }
            }
        }
        for i in self.p_end..self.c_end {
            let x = self.pc[i as usize];
            if self.lvl_id[x as usize] > self.lvl {
                if self.p_end - self.nei_in_p[x as usize] >= self.k
                    || self.nei_in_g[x as usize] + self.k <= self.best_sz
                {
                    self.mark(x);
                    continue;
                }
                for j in 0..self.p_end {
                    let y = self.pc[j as usize];
                    if (self.p_end - self.nei_in_p[y as usize] == self.k && !self.mx(x, y))
                        || (self.enable && self.upper_bound_based_prune(self.p_end, x, y))
                    {
                        self.mark(x);
                        break;
                    }
                }
            }
        }
        // (upstream's following `enable` block tests `lvlID[PC[i]] < lvl`
        // inside `lvlID[PC[i]] > lvl` and never does anything)
        false
    }

    fn bound(&mut self) -> i32 {
        self.vp.clear();
        for i in 0..self.p_end {
            let x = self.pc[i as usize];
            self.vp
                .push((self.k - (self.p_end - self.nei_in_p[x as usize]), x));
        }
        self.vp.sort_unstable();
        let (mut ub, mut cursor) = (self.p_end, self.p_end);
        for i in 0..self.vp.len() {
            let (slack, u) = self.vp[i];
            if slack == 0 {
                continue;
            }
            let mut count = 0;
            for j in cursor..self.c_end {
                if !self.mx(u, self.pc[j as usize]) {
                    if j != cursor + count {
                        self.swap_pos(j, cursor + count);
                    }
                    count += 1;
                }
            }
            let t_ub = count.min(slack);
            if ub + t_ub <= self.best_sz {
                ub += t_ub;
                cursor += count;
            } else {
                return cursor + (self.best_sz - ub);
            }
        }
        cursor += self.best_sz - ub;
        cursor.min(self.c_end)
    }

    fn upper_bound_based_prune(&self, p_end: i32, u: i32, v: i32) -> bool {
        let n = self.n;
        let mut ub = 2 * self.k + self.nei_in_p[u as usize] - p_end
            + self.nei_in_p[v as usize]
            + self.comm_neis[(u * n + v) as usize];
        if self.pc_rid[u as usize] >= p_end {
            ub -= 1;
            if self.mx(u, v) {
                ub += 1;
            }
        }
        if self.pc_rid[v as usize] >= p_end {
            ub -= 1;
            if self.mx(v, u) {
                ub += 1;
            }
        }
        ub <= self.best_sz
    }

    fn swap_pos(&mut self, i: i32, j: i32) {
        self.pc.swap(i as usize, j as usize);
        let (a, b) = (self.pc[i as usize], self.pc[j as usize]);
        self.pc_rid[a as usize] = i;
        self.pc_rid[b as usize] = j;
    }

    fn cond(&self, v: i32) -> bool {
        self.nei_in_g[v as usize] + self.k <= self.best_sz + 1
            || self.nei_in_p[v as usize] + self.k == self.p_end + 1
            || self.nei_in_p[0] + self.k == self.p_end + 1
    }
}

// ---------------------------------------------------------------------------
// Graph-level building blocks (free functions over explicit arrays, so the
// aliasing upstream relies on can be expressed)
// ---------------------------------------------------------------------------

/// `Graph::degen`: degeneracy ordering; may replace `kplex` with the
/// densest tail it finds; returns an upper bound of the maximum k-plex size.
#[allow(clippy::too_many_arguments)]
fn degen(
    k: i32,
    kplex: &mut Vec<i32>,
    n: i32,
    peel_sequence: &mut [i32],
    core: &mut [i32],
    pstart: &[Ept],
    edges: &[i32],
    degree: &mut [i32],
    vis: &mut [u8],
    heap: &mut LinearHeap,
) -> i32 {
    let threshold: i32 = if sz(kplex.len() as i64 + 1) > sz(i64::from(k)) {
        (kplex.len() as i64 + 1 - i64::from(k)) as i32
    } else {
        0
    };
    let nu = n as usize;
    for i in 0..nu {
        degree[i] = (pstart[i + 1] - pstart[i]) as i32;
    }
    let (mut queue_n, mut new_size) = (0usize, 0usize);
    for i in 0..nu {
        if degree[i] < threshold {
            peel_sequence[queue_n] = i as i32;
            queue_n += 1;
        }
    }
    let mut i = 0usize;
    while i < queue_n {
        let u = peel_sequence[i] as usize;
        degree[u] = 0;
        for j in pstart[u]..pstart[u + 1] {
            let w = edges[j as usize] as usize;
            if degree[w] > 0 {
                let before = degree[w];
                degree[w] -= 1;
                if before == threshold {
                    peel_sequence[queue_n] = w as i32;
                    queue_n += 1;
                }
            }
        }
        i += 1;
    }
    let mut ub = n;
    if queue_n == nu {
        ub = kplex.len() as i32;
    }
    vis[..nu].fill(0);
    for i in 0..nu {
        if degree[i] >= threshold {
            peel_sequence[queue_n + new_size] = i as i32;
            new_size += 1;
        } else {
            vis[i] = 1;
            core[i] = 0;
        }
    }
    if new_size != 0 {
        let ns = new_size as i32;
        heap.init(ns, ns - 1, &peel_sequence[queue_n..], degree);
        let mut max_core = 0;
        let mut idx = n;
        ub = 0;
        for i in 0..ns {
            let Some((u, key)) = heap.pop_min() else {
                break;
            };
            if key > max_core {
                max_core = key;
            }
            core[u as usize] = max_core;
            peel_sequence[queue_n + i as usize] = u;
            let t_ub = (core[u as usize] + k).min(ns - i);
            if t_ub > ub {
                ub = t_ub;
            }
            if idx == n && key + k >= ns - i {
                idx = i;
            }
            vis[u as usize] = 1;
            for j in pstart[u as usize]..pstart[u as usize + 1] {
                let w = edges[j as usize];
                if vis[w as usize] == 0 {
                    heap.decrement(w, 1);
                }
            }
        }
        if sz(i64::from(ns - idx)) > sz(kplex.len() as i64) {
            kplex.clear();
            for i in idx..ns {
                kplex.push(peel_sequence[queue_n + i as usize]);
            }
        }
    }
    ub
}

/// `Graph::shrink_graph`: keep the vertices with `core + K > |kplex|`,
/// renumbered; `in_place` = upstream's `in_mapping == out_mapping` call.
#[allow(clippy::too_many_arguments)]
fn shrink_graph(
    k: i32,
    kplex_len: usize,
    n: &mut i32,
    m: &mut Ept,
    peel_sequence: &mut [i32],
    core: &[i32],
    out_mapping: &mut [i32],
    in_place: bool,
    rid: &mut [i32],
    pstart: &mut [Ept],
    edges: &mut [i32],
) {
    let keep = |c: i32| sz(i64::from(c + k)) > sz(kplex_len as i64);
    let nu = *n as usize;
    let mut cnt = 0usize;
    for i in 0..nu {
        if keep(core[i]) {
            rid[i] = cnt as i32;
            out_mapping[cnt] = if in_place { out_mapping[i] } else { i as i32 };
            cnt += 1;
        }
    }
    cnt = 0;
    let mut pos: Ept = 0;
    for i in 0..nu {
        if keep(core[i]) {
            let t_start = pstart[i];
            pstart[cnt] = pos;
            for j in t_start..pstart[i + 1] {
                let w = edges[j as usize] as usize;
                if keep(core[w]) {
                    edges[pos as usize] = rid[w];
                    pos += 1;
                }
            }
            cnt += 1;
        }
    }
    pstart[cnt] = pos;
    for i in 0..cnt {
        peel_sequence[i] = rid[peel_sequence[nu - cnt + i] as usize];
    }
    *n = cnt as i32;
    *m = pos;
}

/// `Graph::oriented_triangle_counting`: orient every edge along the peel
/// order (`pend` ends the forward part) and count each edge's triangles
/// into `tri_cnt` (indexed by adjacency position).
#[allow(clippy::too_many_arguments)]
fn oriented_triangle_counting(
    n: usize,
    m: Ept,
    peel_sequence: &[i32],
    pstart: &[Ept],
    pend: &mut [Ept],
    edges: &mut [i32],
    tri_cnt: &mut [i32],
    adj: &mut [i32],
) {
    for i in 0..n {
        adj[peel_sequence[i] as usize] = i as i32;
    }
    for i in 0..n {
        let mut end = pstart[i];
        for j in pstart[i]..pstart[i + 1] {
            let w = edges[j as usize];
            if adj[w as usize] > adj[i] {
                edges[end as usize] = w;
                end += 1;
            }
        }
        pend[i] = end;
    }
    adj[..n].fill(0);
    tri_cnt[..m as usize].fill(0);
    for u in 0..n {
        for j in pstart[u]..pend[u] {
            adj[edges[j as usize] as usize] = (j + 1) as i32;
        }
        for j in pstart[u]..pend[u] {
            let v = edges[j as usize] as usize;
            for kk in pstart[v]..pend[v] {
                let a = adj[edges[kk as usize] as usize];
                if a != 0 {
                    tri_cnt[j as usize] += 1;
                    tri_cnt[kk as usize] += 1;
                    tri_cnt[(a - 1) as usize] += 1;
                }
            }
        }
        for j in pstart[u]..pend[u] {
            adj[edges[j as usize] as usize] = 0;
        }
    }
}

/// `Graph::reorganize_oriented_graph`: number the edges, record each
/// edge's triangle count and endpoints, and sort every adjacency list.
#[allow(clippy::too_many_arguments)]
fn reorganize_oriented_graph(
    n: usize,
    tri_cnt: &mut [i32],
    edge_list: &mut [i32],
    pstart: &[Ept],
    pend: &mut [Ept],
    pend2: &mut [Ept],
    edges: &mut [i32],
    edgelist_pointer: &mut [i32],
    buf: &mut [i32],
) {
    pend2[..n].copy_from_slice(&pend[..n]);
    let mut pos: Ept = 0;
    for i in 0..n {
        for j in pstart[i]..pend[i] {
            let ju = j as usize;
            tri_cnt[(pos >> 1) as usize] = edgelist_pointer[ju];
            edge_list[pos as usize] = i as i32;
            pos += 1;
            edge_list[pos as usize] = edges[ju];
            pos += 1;
            let w = edges[ju] as usize;
            let kk = pend2[w] as usize;
            let id = ((pos >> 1) - 1) as i32;
            edgelist_pointer[ju] = id;
            edgelist_pointer[kk] = id;
            edges[kk] = i as i32;
            pend2[w] += 1;
        }
    }
    for i in 0..n {
        pend2[i] = pend[i];
        pend[i] = pstart[i];
    }
    for i in 0..n {
        for j in pend2[i]..pstart[i + 1] {
            let ju = j as usize;
            let w = edges[ju] as usize;
            let kk = pend[w] as usize;
            edgelist_pointer[kk] = edgelist_pointer[ju];
            edges[kk] = i as i32;
            pend[w] += 1;
        }
    }
    // `ids = pend2` (an `ept` array) and `buf` as merge scratch
    for i in 0..n {
        if pend[i] == pstart[i] || pend[i] == pstart[i + 1] {
            continue;
        }
        let (mut j, mut k) = (pstart[i] as usize, pend[i] as usize);
        let (jend, kend) = (pend[i] as usize, pstart[i + 1] as usize);
        let mut pos = 0usize;
        while j < jend && k < kend {
            if edges[j] < edges[k] {
                pend2[pos] = Ept::from(edges[j]);
                buf[pos] = edgelist_pointer[j];
                j += 1;
            } else {
                pend2[pos] = Ept::from(edges[k]);
                buf[pos] = edgelist_pointer[k];
                k += 1;
            }
            pos += 1;
        }
        while j < jend {
            pend2[pos] = Ept::from(edges[j]);
            buf[pos] = edgelist_pointer[j];
            j += 1;
            pos += 1;
        }
        while k < kend {
            pend2[pos] = Ept::from(edges[k]);
            buf[pos] = edgelist_pointer[k];
            k += 1;
            pos += 1;
        }
        let base = pstart[i] as usize;
        for x in 0..pos {
            edges[base + x] = pend2[x] as i32;
            edgelist_pointer[base + x] = buf[x];
        }
    }
}

/// `Graph::find`: binary search `w` in `edges[*b..e]`, advancing `*b`;
/// `*idx` receives the probe positions and finally the edge id.
fn find(
    w: i32,
    b: &mut Ept,
    mut e: Ept,
    deleted: &[u8],
    idx: &mut Ept,
    edgelist_pointer: &[i32],
    edges: &[i32],
) -> bool {
    if *b >= e {
        return false;
    }
    while *b + 1 < e {
        *idx = *b + (e - *b) / 2;
        if edges[*idx as usize] > w {
            e = *idx;
        } else {
            *b = *idx;
        }
    }
    if edges[*b as usize] == w {
        *idx = Ept::from(edgelist_pointer[*b as usize]);
        if deleted[*idx as usize] == 0 {
            return true;
        }
    }
    false
}

/// `Qv[Qv_n] = v`. Upstream's `Qv` has `n` slots, but a vertex can be
/// queued twice (once by the caller, once when its degree crosses the
/// threshold during edge peeling), so when every vertex gets peeled the
/// write lands past the end of the array. Whether gStore survives that
/// depends on the allocator's slack after the array (otherwise glibc aborts
/// with `corrupted size vs. prev_size` or a later allocation fails); when it
/// survives, the values written are read back, which is what the port does
/// (the queue grows).
fn qv_put(qv: &mut Vec<i32>, i: i32, v: i32) {
    let i = i as usize;
    if i < qv.len() {
        qv[i] = v;
    } else {
        qv.resize(i, 0);
        qv.push(v);
    }
}

/// An upstream `assert` that fails (gStore's default build keeps asserts,
/// so the server aborts).
fn upstream_assert(what: &str) -> GStoreError {
    kerr(format!(
        "maximumKplex: upstream assertion `{what}` fails on this graph (gStore aborts)"
    ))
}

/// The arrays `peeling` works on.
struct PeelState<'a> {
    tri_cnt: &'a mut [i32],
    active_edgelist: &'a mut [i32],
    active_edgelist_n: &'a mut i32,
    edge_list: &'a [i32],
    edgelist_pointer: &'a mut [i32],
    deleted: &'a mut [u8],
    degree: &'a mut [i32],
    pstart: &'a [Ept],
    pend: &'a mut [Ept],
    edges: &'a mut [i32],
    exists: &'a mut [u8],
}

/// `Graph::peeling`: remove vertices of degree `< d_threshold` (queued in
/// `qv`) and edges in `< t_threshold` triangles; returns the number of
/// removed edges (0 when `critical_vertex` gets removed).
#[allow(clippy::too_many_arguments)]
fn peeling(
    critical_vertex: i32,
    mut linear_heap: Option<&mut LinearHeap>,
    qv: &mut Vec<i32>,
    qv_n: &mut i32,
    d_threshold: i32,
    qe: &mut [i32],
    initialize_qe: bool,
    t_threshold: i32,
    s: PeelState<'_>,
) -> Result<Ept> {
    let PeelState {
        tri_cnt,
        active_edgelist,
        active_edgelist_n,
        edge_list,
        edgelist_pointer: ep,
        deleted,
        degree,
        pstart,
        pend,
        edges,
        exists,
    } = s;
    let mut qe_n: usize = 0;
    if initialize_qe {
        let mut newn = 0usize;
        for j in 0..*active_edgelist_n as usize {
            let e = active_edgelist[j];
            if deleted[e as usize] == 0 {
                if tri_cnt[e as usize] < t_threshold {
                    qe[qe_n] = e;
                    qe_n += 1;
                } else {
                    active_edgelist[newn] = e;
                    newn += 1;
                }
            }
        }
        *active_edgelist_n = newn as i32;
    }
    let mut deleted_edges_n: Ept = 0;
    let mut qv_idx = 0usize;
    while qv_idx < *qv_n as usize || qe_n != 0 {
        if qe_n == 0 {
            let u = qv[qv_idx] as usize;
            qv_idx += 1;
            let mut u_n = pstart[u];
            for kk in pstart[u]..pend[u] {
                let ku = kk as usize;
                if deleted[ep[ku] as usize] == 0 {
                    edges[u_n as usize] = edges[ku];
                    ep[u_n as usize] = ep[ku];
                    u_n += 1;
                    exists[edges[ku] as usize] = 1;
                }
            }
            pend[u] = u_n;
            for kk in pstart[u]..pend[u] {
                deleted[ep[kk as usize] as usize] = 1;
            }
            deleted_edges_n += pend[u] - pstart[u];
            degree[u] = 0;
            if let Some(h) = linear_heap.as_deref_mut() {
                h.del(u as i32);
            }
            for kk in pstart[u]..pend[u] {
                let v = edges[kk as usize] as usize;
                let mut v_n = pstart[v];
                for x in pstart[v]..pend[v] {
                    let xu = x as usize;
                    if deleted[ep[xu] as usize] == 0 {
                        edges[v_n as usize] = edges[xu];
                        ep[v_n as usize] = ep[xu];
                        v_n += 1;
                        let ex = edges[xu];
                        if ex as usize > v && exists[ex as usize] != 0 {
                            let e = ep[xu] as usize;
                            let before = tri_cnt[e];
                            tri_cnt[e] -= 1;
                            if before == t_threshold {
                                qe[qe_n] = e as i32;
                                qe_n += 1;
                            }
                        }
                    }
                }
                pend[v] = v_n;
                let before = degree[v];
                degree[v] -= 1;
                if before == d_threshold {
                    qv_put(qv, *qv_n, v as i32);
                    *qv_n += 1;
                    if v as i32 == critical_vertex {
                        for kk in pstart[u]..pend[u] {
                            exists[edges[kk as usize] as usize] = 0;
                        }
                        return Ok(0);
                    }
                }
                if let Some(h) = linear_heap.as_deref_mut() {
                    h.decrement(v as i32, 1);
                }
            }
            for kk in pstart[u]..pend[u] {
                exists[edges[kk as usize] as usize] = 0;
            }
        }
        let mut j = 0usize;
        while j < qe_n {
            let mut idx = Ept::from(qe[j]);
            j += 1;
            let mut u = edge_list[(idx << 1) as usize] as usize;
            let mut v = edge_list[((idx << 1) + 1) as usize] as usize;
            let mut tri_n = tri_cnt[idx as usize];
            deleted[idx as usize] = 1;
            for x in [u, v] {
                let before = degree[x];
                degree[x] -= 1;
                if before == d_threshold {
                    qv_put(qv, *qv_n, x as i32);
                    *qv_n += 1;
                    if x as i32 == critical_vertex {
                        return Ok(0);
                    }
                }
            }
            if let Some(h) = linear_heap.as_deref_mut() {
                h.decrement(u as i32, 1);
                h.decrement(v as i32, 1);
            }
            deleted_edges_n += 1;
            if degree[u] < degree[v] {
                std::mem::swap(&mut u, &mut v);
            }
            if degree[u] > degree[v] * 2 {
                // binary search
                let mut v_n = pstart[v];
                let mut start = pstart[u];
                for kk in pstart[v]..pend[v] {
                    let ku = kk as usize;
                    if deleted[ep[ku] as usize] == 0 {
                        edges[v_n as usize] = edges[ku];
                        ep[v_n as usize] = ep[ku];
                        v_n += 1;
                        if tri_n != 0
                            && find(edges[ku], &mut start, pend[u], deleted, &mut idx, ep, edges)
                        {
                            tri_n -= 1;
                            let before = tri_cnt[idx as usize];
                            tri_cnt[idx as usize] -= 1;
                            if before == t_threshold {
                                qe[qe_n] = idx as i32;
                                qe_n += 1;
                            }
                            let e = ep[ku] as usize;
                            let before = tri_cnt[e];
                            tri_cnt[e] -= 1;
                            if before == t_threshold {
                                qe[qe_n] = e as i32;
                                qe_n += 1;
                            }
                        }
                    }
                }
                pend[v] = v_n;
                if tri_n != 0 {
                    return Err(upstream_assert("tri_n == 0"));
                }
            } else {
                // sorted merge
                let (mut ii, mut jj) = (pstart[u] as usize, pstart[v] as usize);
                let (mut u_n, mut v_n) = (pstart[u] as usize, pstart[v] as usize);
                let (pu, pv) = (pend[u] as usize, pend[v] as usize);
                loop {
                    while ii < pu && deleted[ep[ii] as usize] != 0 {
                        ii += 1;
                    }
                    while jj < pv && deleted[ep[jj] as usize] != 0 {
                        jj += 1;
                    }
                    if ii >= pu || jj >= pv {
                        break;
                    }
                    if edges[ii] == edges[jj] {
                        edges[u_n] = edges[ii];
                        ep[u_n] = ep[ii];
                        u_n += 1;
                        edges[v_n] = edges[jj];
                        ep[v_n] = ep[jj];
                        v_n += 1;
                        for e in [ep[ii] as usize, ep[jj] as usize] {
                            let before = tri_cnt[e];
                            tri_cnt[e] -= 1;
                            if before == t_threshold {
                                qe[qe_n] = e as i32;
                                qe_n += 1;
                            }
                        }
                        ii += 1;
                        jj += 1;
                    } else if edges[ii] < edges[jj] {
                        edges[u_n] = edges[ii];
                        ep[u_n] = ep[ii];
                        u_n += 1;
                        ii += 1;
                    } else {
                        edges[v_n] = edges[jj];
                        ep[v_n] = ep[jj];
                        v_n += 1;
                        jj += 1;
                    }
                }
                while ii < pu {
                    if deleted[ep[ii] as usize] == 0 {
                        edges[u_n] = edges[ii];
                        ep[u_n] = ep[ii];
                        u_n += 1;
                    }
                    ii += 1;
                }
                while jj < pv {
                    if deleted[ep[jj] as usize] == 0 {
                        edges[v_n] = edges[jj];
                        ep[v_n] = ep[jj];
                        v_n += 1;
                    }
                    jj += 1;
                }
                pend[u] = u_n as Ept;
                pend[v] = v_n as Ept;
            }
        }
        qe_n = 0;
    }
    Ok(deleted_edges_n)
}

// ---------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------

/// `Graph::load_graph_from_edgelist`.
fn load_graph_from_edgelist(
    n_in: i32,
    edge_list: &[(i32, i32)],
    n: &mut i32,
    m: &mut Ept,
    degree: &mut [i32],
    pstart: &mut [Ept],
    edges: &mut [i32],
) {
    *n = n_in;
    *m = edge_list.len() as Ept * 2;
    let nu = n_in as usize;
    degree[..nu].fill(0);
    for &(a, b) in edge_list {
        degree[a as usize] += 1;
        degree[b as usize] += 1;
    }
    pstart[0] = 0;
    for i in 0..nu {
        pstart[i + 1] = pstart[i] + Ept::from(degree[i]);
    }
    for &(a, b) in edge_list {
        edges[pstart[a as usize] as usize] = b;
        pstart[a as usize] += 1;
        edges[pstart[b as usize] as usize] = a;
        pstart[b as usize] += 1;
    }
    for i in 0..nu {
        pstart[i] -= Ept::from(degree[i]);
    }
}

/// `Graph::extract_subgraph(u, …)`: `u`, its neighbours and their
/// neighbours, compacting the adjacency lists on the way.
#[allow(clippy::too_many_arguments)]
fn extract_subgraph(
    u0: i32,
    ids: &mut [i32],
    ids_n: &mut i32,
    rid: &mut [i32],
    vp: &mut Vec<(i32, i32)>,
    exists: &mut [u8],
    pstart: &[Ept],
    pend: &mut [Ept],
    edges: &mut [i32],
    deleted: &[u8],
    ep: &mut [i32],
) {
    let mut n_ids = 0usize;
    vp.clear();
    ids[n_ids] = u0;
    n_ids += 1;
    exists[u0 as usize] = 1;
    rid[u0 as usize] = 0;
    let u = u0 as usize;
    let mut u_n = pstart[u] as usize;
    for i in pstart[u] as usize..pend[u] as usize {
        if deleted[ep[i] as usize] == 0 {
            edges[u_n] = edges[i];
            ep[u_n] = ep[i];
            u_n += 1;
            let v = edges[i];
            rid[v as usize] = n_ids as i32;
            ids[n_ids] = v;
            n_ids += 1;
            exists[v as usize] = 1;
        }
    }
    pend[u] = u_n as Ept;
    let old_size = n_ids;
    for i in 1..old_size {
        let u = ids[i] as usize;
        let mut u_n = pstart[u] as usize;
        for j in pstart[u] as usize..pend[u] as usize {
            if deleted[ep[j] as usize] == 0 {
                edges[u_n] = edges[j];
                ep[u_n] = ep[j];
                u_n += 1;
                let v = edges[j];
                if exists[v as usize] != 0 {
                    continue;
                }
                rid[v as usize] = n_ids as i32;
                ids[n_ids] = v;
                n_ids += 1;
                exists[v as usize] = 1;
            }
        }
        pend[u] = u_n as Ept;
    }
    for i in 0..old_size {
        let u = ids[i];
        for j in pstart[u as usize]..pend[u as usize] {
            let w = edges[j as usize];
            if w > u {
                vp.push((rid[u as usize], rid[w as usize]));
            }
        }
    }
    for i in old_size..n_ids {
        let u = ids[i];
        let uu = u as usize;
        let mut u_n = pstart[uu] as usize;
        for j in pstart[uu] as usize..pend[uu] as usize {
            if deleted[ep[j] as usize] == 0 {
                edges[u_n] = edges[j];
                ep[u_n] = ep[j];
                u_n += 1;
                let w = edges[j];
                if w > u && exists[w as usize] != 0 {
                    vp.push((rid[uu], rid[w as usize]));
                }
            }
        }
        pend[uu] = u_n as Ept;
    }
    for &v in &ids[..n_ids] {
        exists[v as usize] = 0;
    }
    *ids_n = n_ids as i32;
}

struct Graph<'a> {
    ctx: GFunCtx<'a>,
    k: i32,
    n: i32,
    m: Ept,
    pstart: Vec<Ept>,
    pend: Vec<Ept>,
    pend_buf: Vec<Ept>,
    edges: Vec<i32>,
    kplex: Vec<i32>,
    s_degree: Vec<i32>,
    s_pstart: Vec<Ept>,
    s_pend: Vec<Ept>,
    s_edges: Vec<i32>,
    s_peel_sequence: Vec<i32>,
    s_core: Vec<i32>,
    s_vis: Vec<u8>,
    s_heap: LinearHeap,
    s_edgelist_pointer: Vec<i32>,
    s_tri_cnt: Vec<i32>,
    s_edge_list: Vec<i32>,
    s_active_edgelist: Vec<i32>,
    s_deleted: Vec<u8>,
}

impl<'a> Graph<'a> {
    fn new(ctx: GFunCtx<'a>, k: i32) -> Graph<'a> {
        Graph {
            ctx,
            k,
            n: 0,
            m: 0,
            pstart: Vec::new(),
            pend: Vec::new(),
            pend_buf: Vec::new(),
            edges: Vec::new(),
            kplex: Vec::new(),
            s_degree: Vec::new(),
            s_pstart: Vec::new(),
            s_pend: Vec::new(),
            s_edges: Vec::new(),
            s_peel_sequence: Vec::new(),
            s_core: Vec::new(),
            s_vis: Vec::new(),
            s_heap: LinearHeap::new(0, -1),
            s_edgelist_pointer: Vec::new(),
            s_tri_cnt: Vec::new(),
            s_edge_list: Vec::new(),
            s_active_edgelist: Vec::new(),
            s_deleted: Vec::new(),
        }
    }

    /// `Graph::read`: the simple undirected graph of every edge in `f`
    /// (both directions, sorted, self-loops and parallel edges removed).
    fn read(&mut self, g: &CsrGraph, f: &PredFilter) {
        let n = g.num_nodes();
        self.n = n as i32;
        self.m = g.num_edges_with(f) as Ept;
        self.pstart = vec![0; n + 1];
        self.edges = vec![0; 2 * self.m as usize];
        for uid in 0..n {
            let base = self.pstart[uid] as usize;
            let mut sum = 0usize;
            for &p in f.preds() {
                for &to in g.out_nbrs(uid as Node, p) {
                    self.edges[base + sum] = to as i32;
                    sum += 1;
                }
                for &to in g.in_nbrs(uid as Node, p) {
                    self.edges[base + sum] = to as i32;
                    sum += 1;
                }
            }
            let buff = &mut self.edges[base..base + sum];
            buff.sort_unstable();
            let mut idx = 0usize;
            for j in 0..sum {
                if buff[j] == uid as i32 || (j > 0 && buff[j] == buff[j - 1]) {
                    continue;
                }
                buff[idx] = buff[j];
                idx += 1;
            }
            self.pstart[uid + 1] = self.pstart[uid] + idx as Ept;
        }
    }

    /// `Graph::heuristic_kplex_max_degree`.
    fn heuristic_kplex_max_degree(&mut self, processed_threshold: i32) {
        let n = self.n;
        let nu = n as usize;
        let k = i64::from(self.k);
        let (pstart, edges) = (&self.pstart, &self.edges);
        let mut head = vec![n; nu];
        let mut next = vec![0i32; nu];
        let mut degree = vec![0i32; nu];
        let mut vis = vec![0i32; nu];
        let mut max_degree: i32 = 0;
        for i in 0..nu {
            degree[i] = (pstart[i + 1] - pstart[i]) as i32;
            if degree[i] > max_degree {
                max_degree = degree[i];
            }
            next[i] = head[degree[i] as usize];
            head[degree[i] as usize] = i as i32;
        }
        let ge = |x: i64, len: usize| sz(x) >= len as u64;
        let mut processed = 0;
        while ge(i64::from(max_degree) + k, self.kplex.len()) && processed < processed_threshold {
            let mut u = n;
            while max_degree >= 0 && ge(i64::from(max_degree) + k, self.kplex.len()) && u == n {
                let mut v = head[max_degree as usize];
                while v != n {
                    let tmp = next[v as usize];
                    if degree[v as usize] == max_degree {
                        u = v;
                        head[max_degree as usize] = tmp;
                        break;
                    } else if ge(i64::from(degree[v as usize]) + k, self.kplex.len()) {
                        next[v as usize] = head[degree[v as usize] as usize];
                        head[degree[v as usize] as usize] = v;
                    }
                    v = tmp;
                }
                if u == n {
                    head[max_degree as usize] = n;
                    max_degree -= 1;
                }
            }
            if u == n {
                break;
            }
            vis[u as usize] = 1;
            let range = |x: i32| pstart[x as usize] as usize..pstart[x as usize + 1] as usize;
            for kk in range(u) {
                if vis[edges[kk] as usize] == 0 {
                    degree[edges[kk] as usize] -= 1;
                }
            }
            let mut vs: Vec<i32> = range(u)
                .map(|j| edges[j])
                .filter(|&w| vis[w as usize] == 0)
                .collect();
            let mut vs_deg = vec![0i32; vs.len()];
            for &w in &vs {
                vis[w as usize] = 2;
            }
            for (j, &w) in vs.iter().enumerate() {
                vs_deg[j] = range(w).filter(|&kk| vis[edges[kk] as usize] == 2).count() as i32;
            }
            for &w in &vs {
                vis[w as usize] = 0;
            }
            let mut res: Vec<i32> = vec![u];
            let mut vs_size = vs.len();
            while vs_size > 0
                && res.len() as u64 + vs_size as u64 + (k - 1) as u64 > self.kplex.len() as u64
            {
                let mut idx = 0usize;
                for j in 1..vs_size {
                    if vs_deg[j] > vs_deg[idx]
                        || (vs_deg[j] == vs_deg[idx]
                            && degree[vs[j] as usize] > degree[vs[idx] as usize])
                    {
                        idx = j;
                    }
                }
                let u = vs[idx];
                let mut new_size = 0usize;
                for j in range(u) {
                    if vis[edges[j] as usize] == 0 {
                        vis[edges[j] as usize] = 2;
                    }
                }
                for j in 0..vs_size {
                    if vis[vs[j] as usize] != 0 {
                        if j != new_size {
                            vs.swap(new_size, j);
                        }
                        vs_deg[new_size] = vs_deg[j];
                        new_size += 1;
                    }
                }
                for j in range(u) {
                    if vis[edges[j] as usize] == 2 {
                        vis[edges[j] as usize] = 0;
                    }
                }
                res.push(u);
                for (kk, &w) in vs.iter().enumerate().take(new_size) {
                    vis[w as usize] = kk as i32 + 2;
                }
                for j in new_size..vs_size {
                    let w = vs[j];
                    for kk in range(w) {
                        let x = vis[edges[kk] as usize];
                        if x >= 2 {
                            vs_deg[(x - 2) as usize] -= 1;
                        }
                    }
                }
                for &w in vs.iter().take(new_size) {
                    vis[w as usize] = 0;
                }
                vs_size = new_size;
            }
            if res.len() > self.kplex.len() {
                self.kplex = res;
            }
            processed += 1;
        }
    }

    /// `Graph::extract_subgraph_and_prune(u, ids = qv, …)` (uses the
    /// member graph `pstart` / `edges`).
    #[allow(clippy::too_many_arguments)]
    fn extract_subgraph_and_prune(
        &mut self,
        u0: i32,
        ids: &mut [i32],
        ids_n: &mut i32,
        rid: &mut [i32],
        vp: &mut Vec<(i32, i32)>,
        q: &mut [i32],
        degree: &mut [i32],
        exists: &mut [u8],
        deleted: &[u8],
        ep: &mut [i32],
    ) -> Result<()> {
        let k2 = 2 * i64::from(self.k);
        let half_m = self.m / 2;
        let ksz = self.kplex.len() as u64;
        let pstart = &self.pstart;
        let edges = &mut self.edges;
        let pend = &mut self.pend;
        vp.clear();
        let mut n_ids = 0usize;
        ids[n_ids] = u0;
        n_ids += 1;
        exists[u0 as usize] = 1;
        let u = u0 as usize;
        let mut u_n = pstart[u] as usize;
        for i in pstart[u] as usize..pend[u] as usize {
            if deleted[ep[i] as usize] == 0 {
                edges[u_n] = edges[i];
                ep[u_n] = ep[i];
                u_n += 1;
                let v = edges[i];
                ids[n_ids] = v;
                n_ids += 1;
                exists[v as usize] = 2;
            }
        }
        pend[u] = u_n as Ept;
        let mut q_n = 0usize;
        for i in 1..n_ids {
            let u = ids[i] as usize;
            let mut u_n = pstart[u] as usize;
            degree[u] = 0;
            for j in pstart[u] as usize..pend[u] as usize {
                if deleted[ep[j] as usize] == 0 {
                    edges[u_n] = edges[j];
                    ep[u_n] = ep[j];
                    u_n += 1;
                    if exists[edges[j] as usize] == 2 {
                        degree[u] += 1;
                    }
                }
            }
            pend[u] = u_n as Ept;
            if sz(i64::from(degree[u]) + k2) <= ksz {
                q[q_n] = u as i32;
                q_n += 1;
            }
        }
        let mut i = 0usize;
        while i < q_n {
            let u = q[i] as usize;
            exists[u] = 10;
            for j in pstart[u] as usize..pend[u] as usize {
                let w = edges[j] as usize;
                if exists[w] == 2 {
                    let before = degree[w];
                    degree[w] -= 1;
                    if sz(i64::from(before) + k2) == ksz + 1 {
                        if q_n as Ept >= half_m {
                            return Err(upstream_assert("Q_n < m / 2"));
                        }
                        q[q_n] = w as i32;
                        q_n += 1;
                    }
                }
            }
            i += 1;
        }
        if q_n > n_ids {
            return Err(upstream_assert("Q_n <= ids_n"));
        }
        if sz(n_ids as i64 - 1 - q_n as i64 + i64::from(self.k)) <= ksz {
            for &v in &ids[..n_ids] {
                exists[v as usize] = 0;
            }
            *ids_n = 0;
            return Ok(());
        }
        let mut nr_size = n_ids;
        for i in 1..nr_size {
            if exists[ids[i] as usize] == 2 {
                let u = ids[i] as usize;
                for j in pstart[u] as usize..pend[u] as usize {
                    let w = edges[j] as usize;
                    if exists[w] == 0 {
                        ids[n_ids] = w as i32;
                        n_ids += 1;
                        exists[w] = 3;
                        degree[w] = 1;
                    } else if exists[w] == 3 {
                        degree[w] += 1;
                    }
                }
            }
        }
        let mut new_size = 1usize;
        for i in 1..nr_size {
            if exists[ids[i] as usize] == 10 {
                exists[ids[i] as usize] = 0;
            } else {
                ids[new_size] = ids[i];
                new_size += 1;
            }
        }
        if new_size + q_n != nr_size {
            return Err(upstream_assert("new_size + Q_n == nr_size"));
        }
        let old_nr_size = nr_size;
        nr_size = new_size;
        for i in old_nr_size..n_ids {
            if sz(i64::from(degree[ids[i] as usize]) + k2) <= ksz + 2 {
                exists[ids[i] as usize] = 0;
            } else {
                ids[new_size] = ids[i];
                new_size += 1;
            }
        }
        n_ids = new_size;
        for (i, &v) in ids.iter().enumerate().take(n_ids) {
            if exists[v as usize] == 0 {
                return Err(upstream_assert("exists[ids[i]]"));
            }
            rid[v as usize] = i as i32;
        }
        for &u in ids.iter().take(nr_size) {
            for j in pstart[u as usize] as usize..pend[u as usize] as usize {
                let w = edges[j];
                if exists[w as usize] != 0 && w > u {
                    if deleted[ep[j] as usize] != 0 {
                        return Err(upstream_assert("!deleted[edgelist_pointer[j]]"));
                    }
                    vp.push((rid[u as usize], rid[w as usize]));
                }
            }
        }
        for &u in ids.iter().take(n_ids).skip(nr_size) {
            let uu = u as usize;
            let mut u_n = pstart[uu] as usize;
            for j in pstart[uu] as usize..pend[uu] as usize {
                if deleted[ep[j] as usize] == 0 {
                    edges[u_n] = edges[j];
                    ep[u_n] = ep[j];
                    u_n += 1;
                    let w = edges[j];
                    if w > u && exists[w as usize] != 0 {
                        vp.push((rid[uu], rid[w as usize]));
                    }
                }
            }
            pend[uu] = u_n as Ept;
        }
        for &v in &ids[..n_ids] {
            exists[v as usize] = 0;
        }
        *ids_n = n_ids as i32;
        Ok(())
    }

    /// `Graph::subgraph_prune(ids, _n, edge_list, rid, Qv, Qe, exists)` as
    /// called from `search`, where `ids` and `Qv` are the same array (`qv`).
    #[allow(clippy::too_many_arguments)]
    fn subgraph_prune(
        &mut self,
        qv: &mut Vec<i32>,
        n_out: &mut i32,
        edge_list: &mut Vec<(i32, i32)>,
        rid: &mut [i32],
        qe: &mut [i32],
        exists: &mut [u8],
    ) -> Result<()> {
        let k = self.k;
        let (mut s_n, mut s_m) = (0i32, 0 as Ept);
        load_graph_from_edgelist(
            *n_out,
            edge_list,
            &mut s_n,
            &mut s_m,
            &mut self.s_degree,
            &mut self.s_pstart,
            &mut self.s_edges,
        );
        degen(
            k,
            &mut self.kplex,
            s_n,
            &mut self.s_peel_sequence,
            &mut self.s_core,
            &self.s_pstart,
            &self.s_edges,
            &mut self.s_degree,
            &mut self.s_vis,
            &mut self.s_heap,
        );
        shrink_graph(
            k,
            self.kplex.len(),
            &mut s_n,
            &mut s_m,
            &mut self.s_peel_sequence,
            &self.s_core,
            qv,
            true,
            rid,
            &mut self.s_pstart,
            &mut self.s_edges,
        );
        let ksz = self.kplex.len() as i64;
        if s_n > 0 && sz(ksz + 1) > sz(2 * i64::from(k)) {
            let sn = s_n as usize;
            oriented_triangle_counting(
                sn,
                s_m,
                &self.s_peel_sequence,
                &self.s_pstart,
                &mut self.s_pend,
                &mut self.s_edges,
                &mut self.s_edgelist_pointer,
                rid,
            );
            reorganize_oriented_graph(
                sn,
                &mut self.s_tri_cnt,
                &mut self.s_edge_list,
                &self.s_pstart,
                &mut self.s_pend,
                &mut self.pend_buf,
                &mut self.s_edges,
                &mut self.s_edgelist_pointer,
                rid,
            );
            for i in 0..sn {
                self.s_pend[i] = self.s_pstart[i + 1];
                self.s_degree[i] = (self.s_pend[i] - self.s_pstart[i]) as i32;
            }
            let half = (s_m >> 1) as usize;
            for i in 0..half {
                self.s_active_edgelist[i] = i as i32;
            }
            self.s_deleted[..half].fill(0);
            let mut s_active_n = half as i32;
            let mut qv_n = 0i32;
            let removed = peeling(
                0,
                None,
                qv,
                &mut qv_n,
                (ksz + 1 - i64::from(k)) as i32,
                qe,
                true,
                (ksz + 1 - 2 * i64::from(k)) as i32,
                PeelState {
                    tri_cnt: &mut self.s_tri_cnt,
                    active_edgelist: &mut self.s_active_edgelist,
                    active_edgelist_n: &mut s_active_n,
                    edge_list: &self.s_edge_list,
                    edgelist_pointer: &mut self.s_edgelist_pointer,
                    deleted: &mut self.s_deleted,
                    degree: &mut self.s_degree,
                    pstart: &self.s_pstart,
                    pend: &mut self.s_pend,
                    edges: &mut self.s_edges,
                    exists,
                },
            )?;
            s_m -= 2 * removed;
            if qv[..qv_n as usize].contains(&0) {
                *n_out = 0;
                return Ok(());
            }
            extract_subgraph(
                0,
                qv,
                &mut s_n,
                rid,
                edge_list,
                exists,
                &self.s_pstart,
                &mut self.s_pend,
                &mut self.s_edges,
                &self.s_deleted,
                &mut self.s_edgelist_pointer,
            );
            // `Qv[i] = ids[Qv[i]]` with `ids == Qv`
            for i in 0..s_n as usize {
                let x = qv[qv[i] as usize];
                qv[i] = x;
            }
            *n_out = s_n;
            if edge_list.len() as Ept * 2 != s_m {
                return Err(upstream_assert("edge_list.size() * 2 == s_m"));
            }
        } else {
            *n_out = s_n;
            edge_list.clear();
            for i in 0..s_n as usize {
                for j in self.s_pstart[i]..self.s_pstart[i + 1] {
                    let w = self.s_edges[j as usize];
                    if w > i as i32 {
                        edge_list.push((w, i as i32));
                    }
                }
            }
            if edge_list.len() as Ept * 2 != s_m {
                return Err(upstream_assert("edge_list.size() * 2 == s_m"));
            }
        }
        Ok(())
    }

    /// `Graph::search`.
    fn search(&mut self) -> Result<()> {
        let k = self.k;
        self.kplex = vec![0; (2 * k - 2) as usize];
        self.heuristic_kplex_max_degree(10);
        let oldn = self.n;
        let n0 = self.n as usize;
        let mut seq = vec![0i32; n0];
        let mut core = vec![0i32; n0];
        let ub = {
            let mut degree = vec![0i32; n0];
            let mut vis = vec![0u8; n0];
            let mut heap = LinearHeap::new(self.n, self.n - 1);
            degen(
                k,
                &mut self.kplex,
                self.n,
                &mut seq,
                &mut core,
                &self.pstart,
                &self.edges,
                &mut degree,
                &mut vis,
                &mut heap,
            )
        };
        if sz(self.kplex.len() as i64) >= sz(i64::from(ub)) {
            return Ok(());
        }
        let old_size = self.kplex.len();
        let mut out_mapping = vec![0i32; n0];
        let mut rid = vec![0i32; n0];
        shrink_graph(
            k,
            self.kplex.len(),
            &mut self.n,
            &mut self.m,
            &mut seq,
            &core,
            &mut out_mapping,
            false,
            &mut rid,
            &mut self.pstart,
            &mut self.edges,
        );
        let n = self.n as usize;
        let m = self.m;
        let mut degree: Vec<i32> = (0..n)
            .map(|i| (self.pstart[i + 1] - self.pstart[i]) as i32)
            .collect();
        let mut linear_heap = LinearHeap::new(self.n, self.n - 1);
        linear_heap.init(self.n, self.n - 1, &seq, &degree);
        self.pend = vec![0; n];
        let mut ep = vec![0i32; m as usize];
        oriented_triangle_counting(
            n,
            m,
            &seq,
            &self.pstart,
            &mut self.pend,
            &mut self.edges,
            &mut ep,
            &mut rid,
        );
        self.pend_buf = vec![0; n];
        let mut edge_list = vec![0i32; m as usize];
        let mut tri_cnt = vec![0i32; (m / 2) as usize];
        reorganize_oriented_graph(
            n,
            &mut tri_cnt,
            &mut edge_list,
            &self.pstart,
            &mut self.pend,
            &mut self.pend_buf,
            &mut self.edges,
            &mut ep,
            &mut rid,
        );
        for i in 0..n {
            self.pend[i] = self.pstart[i + 1];
        }
        let half = (m >> 1) as usize;
        let mut active_edgelist: Vec<i32> = (0..half as i32).collect();
        let mut active_n = half as i32;
        let mut qe = vec![0i32; half];
        let mut deleted = vec![0u8; half];
        let mut exists = vec![0u8; n];
        let mut qv = vec![0i32; n];
        let mut qv_n = 0i32;
        let mut solver = Searcher::new();
        let ksz = |g: &Graph<'_>| g.kplex.len() as i64;
        macro_rules! peel {
            ($d:expr, $init:expr, $t:expr) => {{
                let removed = peeling(
                    n as i32,
                    Some(&mut linear_heap),
                    &mut qv,
                    &mut qv_n,
                    $d,
                    &mut qe,
                    $init,
                    $t,
                    PeelState {
                        tri_cnt: &mut tri_cnt,
                        active_edgelist: &mut active_edgelist,
                        active_edgelist_n: &mut active_n,
                        edge_list: &edge_list,
                        edgelist_pointer: &mut ep,
                        deleted: &mut deleted,
                        degree: &mut degree,
                        pstart: &self.pstart,
                        pend: &mut self.pend,
                        edges: &mut self.edges,
                        exists: &mut exists,
                    },
                )?;
                self.m -= 2 * removed;
            }};
        }
        let k64 = i64::from(k);
        if sz(ksz(self)) > sz(2 * k64 - 2) {
            let (d, t) = (
                (ksz(self) + 1 - k64) as i32,
                (ksz(self) + 1 - 2 * k64) as i32,
            );
            peel!(d, true, t);
        }
        let max_n = n as i32 - qv_n;
        let mu = self.m as usize;
        let mn = max_n.max(0) as usize;
        self.s_degree = vec![0; mn];
        self.s_pstart = vec![0; mn + 1];
        self.s_pend = vec![0; mn];
        self.s_edges = vec![0; mu];
        self.s_peel_sequence = vec![0; mn];
        self.s_core = vec![0; mn];
        self.s_vis = vec![0; mn];
        self.s_heap = LinearHeap::new(max_n, max_n - 1);
        self.s_edgelist_pointer = vec![0; mu];
        self.s_tri_cnt = vec![0; mu / 2];
        self.s_edge_list = vec![0; mu];
        self.s_active_edgelist = vec![0; mu / 2];
        self.s_deleted = vec![0; mu / 2];
        let mut vp: Vec<(i32, i32)> = Vec::new();
        let mut t_degree = vec![0i32; n];
        for _ in 0..n {
            if self.m == 0 || sz(ksz(self)) >= sz(i64::from(ub)) {
                break;
            }
            self.ctx.check_cancel()?;
            // A failed pop (unreachable while edges remain) leaves `u` and
            // `key` uninitialised upstream; read them as 0 like `peel`.
            let (u, key) = linear_heap.pop_min().unwrap_or((0, 0));
            if sz(i64::from(key)) < sz(ksz(self) + 1 - k64) {
                if degree[u as usize] != 0 {
                    qv[0] = u;
                    qv_n = 1;
                    let d = (ksz(self) + 1 - k64) as i32;
                    let t = if sz(ksz(self) + 1) > sz(2 * k64) {
                        (ksz(self) + 1 - 2 * k64) as i32
                    } else {
                        0
                    };
                    peel!(d, false, t);
                }
                continue;
            }
            if self.m == 0 {
                break;
            }
            if degree[u as usize] != key {
                return Err(upstream_assert("degree[u] == key"));
            }
            let mut ids_n = 0i32;
            if k < 3 || max_n == oldn {
                solver.enable_input = false;
            }
            if k >= 10 {
                solver.enable_input = true;
            }
            loop {
                let pre_size = self.kplex.len();
                self.extract_subgraph_and_prune(
                    u,
                    &mut qv,
                    &mut ids_n,
                    &mut rid,
                    &mut vp,
                    &mut qe,
                    &mut t_degree,
                    &mut exists,
                    &deleted,
                    &mut ep,
                )?;
                if sz(i64::from(ids_n)) > sz(ksz(self)) && (vp.len() as u64) * 2 < sz(self.m) {
                    self.subgraph_prune(
                        &mut qv,
                        &mut ids_n,
                        &mut vp,
                        &mut rid,
                        &mut qe,
                        &mut exists,
                    )?;
                }
                if self.kplex.len() == pre_size {
                    break;
                }
            }
            let pre_size = self.kplex.len();
            if sz(i64::from(ids_n)) > sz(ksz(self)) {
                if ids_n > MAX_SEARCHER_N {
                    return Err(kerr(format!(
                        "maximumKplex: a neighbourhood of {ids_n} vertices exceeds the search limit ({MAX_SEARCHER_N})"
                    )));
                }
                solver.ctx = Some(self.ctx);
                solver.load(ids_n, &vp);
                solver.run(k, &mut self.kplex);
                if solver.cancelled {
                    return Err(GStoreError::Cancelled);
                }
            }
            qv[0] = u;
            qv_n = 1;
            if self.kplex.len() != pre_size && sz(ksz(self)) > sz(2 * k64 - 2) {
                for j in 0..self.kplex.len() {
                    self.kplex[j] = qv[self.kplex[j] as usize];
                }
                let (d, t) = (
                    (ksz(self) + 1 - k64) as i32,
                    (ksz(self) + 1 - 2 * k64) as i32,
                );
                peel!(d, true, t);
            } else if sz(ksz(self)) > sz(2 * k64 - 2) {
                let (d, t) = (
                    (ksz(self) + 1 - k64) as i32,
                    (ksz(self) + 1 - 2 * k64) as i32,
                );
                peel!(d, false, t);
            } else {
                let d = (ksz(self) + 1 - k64) as i32;
                peel!(d, false, 0);
            }
        }
        if self.kplex.len() > old_size {
            for x in self.kplex.iter_mut() {
                *x = out_mapping[*x as usize];
            }
        }
        Ok(())
    }

    /// `Graph::peel(alpha, beta)` (see the module docs for its output
    /// quirk).
    fn peel(&mut self, alpha: i32, beta: i32) -> Result<Vec<i32>> {
        let n0 = self.n as usize;
        let alpha_len = alpha as usize;
        let mut seq = vec![0i32; n0];
        let mut core = vec![0i32; n0];
        {
            let mut degree = vec![0i32; n0];
            let mut vis = vec![0u8; n0];
            let mut heap = LinearHeap::new(self.n, self.n - 1);
            self.kplex.resize(alpha_len, 0);
            degen(
                self.k,
                &mut self.kplex,
                self.n,
                &mut seq,
                &mut core,
                &self.pstart,
                &self.edges,
                &mut degree,
                &mut vis,
                &mut heap,
            );
            self.kplex.resize(alpha_len, 0);
        }
        if !(self.n > 0 && self.m > 0) {
            // upstream: assert(n > 0 && m > 0), i.e. gStore aborts on a graph
            // without edges in the predicate set; the empty result is the
            // natural answer.
            return Ok(Vec::new());
        }
        let mut out_mapping = vec![0i32; n0];
        let mut rid = vec![0i32; n0];
        shrink_graph(
            self.k,
            self.kplex.len(),
            &mut self.n,
            &mut self.m,
            &mut seq,
            &core,
            &mut out_mapping,
            false,
            &mut rid,
            &mut self.pstart,
            &mut self.edges,
        );
        let n = self.n as usize;
        let m = self.m;
        let mut degree: Vec<i32> = (0..n)
            .map(|i| (self.pstart[i + 1] - self.pstart[i]) as i32)
            .collect();
        let mut linear_heap = LinearHeap::new(self.n, self.n - 1);
        linear_heap.init(self.n, self.n - 1, &seq, &degree);
        self.pend = vec![0; n];
        let mut ep = vec![0i32; m as usize];
        oriented_triangle_counting(
            n,
            m,
            &seq,
            &self.pstart,
            &mut self.pend,
            &mut self.edges,
            &mut ep,
            &mut rid,
        );
        self.pend_buf = vec![0; n];
        let mut edge_list = vec![0i32; m as usize];
        let mut tri_cnt = vec![0i32; (m / 2) as usize];
        reorganize_oriented_graph(
            n,
            &mut tri_cnt,
            &mut edge_list,
            &self.pstart,
            &mut self.pend,
            &mut self.pend_buf,
            &mut self.edges,
            &mut ep,
            &mut rid,
        );
        for i in 0..n {
            self.pend[i] = self.pstart[i + 1];
        }
        let half = (m >> 1) as usize;
        let mut active_edgelist: Vec<i32> = (0..half as i32).collect();
        let mut active_n = half as i32;
        let mut qe = vec![0i32; half];
        let mut deleted = vec![0u8; half];
        let mut exists = vec![0u8; n];
        let mut qv = vec![0i32; n];
        let mut qv_n = 0i32;
        let removed = peeling(
            n as i32,
            Some(&mut linear_heap),
            &mut qv,
            &mut qv_n,
            alpha,
            &mut qe,
            true,
            beta,
            PeelState {
                tri_cnt: &mut tri_cnt,
                active_edgelist: &mut active_edgelist,
                active_edgelist_n: &mut active_n,
                edge_list: &edge_list,
                edgelist_pointer: &mut ep,
                deleted: &mut deleted,
                degree: &mut degree,
                pstart: &self.pstart,
                pend: &mut self.pend,
                edges: &mut self.edges,
                exists: &mut exists,
            },
        )?;
        self.m -= 2 * removed;
        let mut res = Vec::new();
        for _ in 0..n {
            // `pop_min` fails once the peeled vertices are exhausted and
            // leaves the uninitialised `int u` untouched; g++ -O2 (13.3)
            // materialises that value as 0, so vertex 0 repeats while its
            // degree is non-zero.
            let u = linear_heap.pop_min().map_or(0, |(u, _)| u);
            if degree[u as usize] != 0 {
                res.push(out_mapping[u as usize]);
            }
        }
        Ok(res)
    }
}

/// Run `f` on a thread with a large stack: the `Searcher` recursion is as
/// deep as the largest neighbourhood it explores.
fn with_big_stack<T: Send>(f: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    std::thread::scope(|s| {
        let h = std::thread::Builder::new()
            .name("gfun-kplex".into())
            .stack_size(256 << 20)
            .spawn_scoped(s, f)
            .map_err(|e| kerr(format!("maximumKplex: cannot start worker: {e}")))?;
        h.join()
            .unwrap_or_else(|_| Err(kerr("maximumKplex: internal error")))
    })
}

/// `PathQueryHandler::maximumKplex(pred_set, k)`: the vertices (CSR nodes)
/// of the k-plex upstream reports, in its order.
pub(super) fn maximum_kplex(ctx: &GFunCtx<'_>, f: &PredFilter, k: i32) -> Result<Vec<i32>> {
    if k < 1 {
        return Err(kerr(format!(
            "maximumKplex: k must be at least 1 (got {k})"
        )));
    }
    if 2 * i64::from(k) - 2 > MAX_FAKE_KPLEX {
        return Err(kerr(format!("maximumKplex: k too large ({k})")));
    }
    let ctx = *ctx;
    with_big_stack(move || {
        let mut g = Graph::new(ctx, k);
        g.read(ctx.csr, f);
        g.search()?;
        Ok(g.kplex)
    })
}

/// `PathQueryHandler::coreTruss(pred_set, alpha, beta)`: the remaining
/// vertices (CSR nodes) in upstream's output order.
pub(super) fn core_truss(
    ctx: &GFunCtx<'_>,
    f: &PredFilter,
    alpha: i32,
    beta: i32,
) -> Result<Vec<i32>> {
    if alpha < 0 || beta < 0 {
        return Err(kerr(format!(
            "coreTruss: alpha and beta must not be negative (got {alpha}, {beta})"
        )));
    }
    ctx.check_cancel()?;
    let mut g = Graph::new(*ctx, 1);
    g.read(ctx.csr, f);
    if alpha as usize > g.n as usize + 1 {
        // Every vertex has a core number below alpha: nothing survives
        // (upstream first allocates `kplex.resize(alpha)`).
        return Ok(Vec::new());
    }
    g.peel(alpha, beta)
}
