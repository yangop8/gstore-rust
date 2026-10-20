//! Emulation of the C++ standard-library behaviour that leaks into gStore's
//! graph-function output.
//!
//! Upstream's results are not only a function of the graph: some orders and
//! tie-breaks come from libstdc++ / glibc implementation details. To produce
//! byte-identical output the ports need the same details:
//!
//! * [`CxxUnorderedOrder`] — the iteration order of a libstdc++
//!   `std::unordered_map<int, T>` / `std::unordered_set<int>` given the keys'
//!   insertion order (used for `pathVec2JSON`'s `nodes`, `SSSP` / `SSSPLen`
//!   result order, `WCC` member order).
//! * [`cxx_sort_by`] — libstdc++ `std::sort` (introsort; *unstable*: equal
//!   elements end up in a deterministic but implementation-defined order),
//!   used by `PR`'s top-k extraction. [`cxx_partial_sort_copy_by`] —
//!   `std::partial_sort_copy` (PPR's top-k).
//! * [`GlibcRand`] — glibc's `srand` / `rand` (TYPE_3 additive feedback
//!   generator), used by `labelProp` (seeded with the constant `114514`, so
//!   upstream *is* reproducible) and as the documented deterministic RNG for
//!   algorithms upstream seeds with `time(NULL)` (`PPR`'s random walks).
//!
//! All of these were checked against g++ 13 / glibc 2.39 (the golden values in
//! the unit tests were produced by small C++ programs); the algorithms have
//! been stable in libstdc++ since GCC 5 (2015).

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// unordered_map iteration order
// ---------------------------------------------------------------------------

const NIL: u32 = u32::MAX;
/// Stands for `_M_before_begin` in a bucket slot.
const BEFORE_BEGIN: u32 = u32::MAX - 1;

/// libstdc++ `__fast_bkt` table for bucket-count requests `< 14`.
const FAST_BKT: [u64; 14] = [2, 2, 2, 3, 5, 5, 7, 7, 11, 11, 11, 11, 13, 13];

/// libstdc++ `__prime_list` from 17 upward (what `_M_next_bkt` searches with
/// `lower_bound(__prime_list + 6, …)`), extracted from the shared library by
/// calling `_Prime_rehash_policy::_M_next_bkt`. Truncated above 2^34, far
/// beyond any id count a CSR can hold.
const PRIMES: [u64; 255] = [
    17,
    19,
    23,
    29,
    31,
    37,
    41,
    43,
    47,
    53,
    59,
    61,
    67,
    71,
    73,
    79,
    83,
    89,
    97,
    103,
    109,
    113,
    127,
    137,
    139,
    149,
    157,
    167,
    179,
    193,
    199,
    211,
    227,
    241,
    257,
    277,
    293,
    313,
    337,
    359,
    383,
    409,
    439,
    467,
    503,
    541,
    577,
    619,
    661,
    709,
    761,
    823,
    887,
    953,
    1031,
    1109,
    1193,
    1289,
    1381,
    1493,
    1613,
    1741,
    1879,
    2029,
    2179,
    2357,
    2549,
    2753,
    2971,
    3209,
    3469,
    3739,
    4027,
    4349,
    4703,
    5087,
    5503,
    5953,
    6427,
    6949,
    7517,
    8123,
    8783,
    9497,
    10273,
    11113,
    12011,
    12983,
    14033,
    15173,
    16411,
    17749,
    19183,
    20753,
    22447,
    24281,
    26267,
    28411,
    30727,
    33223,
    35933,
    38873,
    42043,
    45481,
    49201,
    53201,
    57557,
    62233,
    67307,
    72817,
    78779,
    85229,
    92203,
    99733,
    107897,
    116731,
    126271,
    136607,
    147793,
    159871,
    172933,
    187091,
    202409,
    218971,
    236897,
    256279,
    277261,
    299951,
    324503,
    351061,
    379787,
    410857,
    444487,
    480881,
    520241,
    562841,
    608903,
    658753,
    712697,
    771049,
    834181,
    902483,
    976369,
    1056323,
    1142821,
    1236397,
    1337629,
    1447153,
    1565659,
    1693859,
    1832561,
    1982627,
    2144977,
    2320627,
    2510653,
    2716249,
    2938679,
    3179303,
    3439651,
    3721303,
    4026031,
    4355707,
    4712381,
    5098259,
    5515729,
    5967347,
    6456007,
    6984629,
    7556579,
    8175383,
    8844859,
    9569143,
    10352717,
    11200489,
    12117689,
    13109983,
    14183539,
    15345007,
    16601593,
    17961079,
    19431899,
    21023161,
    22744717,
    24607243,
    26622317,
    28802401,
    31160981,
    33712729,
    36473443,
    39460231,
    42691603,
    46187573,
    49969847,
    54061849,
    58488943,
    63278561,
    68460391,
    74066549,
    80131819,
    86693767,
    93793069,
    101473717,
    109783337,
    118773397,
    128499677,
    139022417,
    150406843,
    162723577,
    176048909,
    190465427,
    206062531,
    222936881,
    241193053,
    260944219,
    282312799,
    305431229,
    330442829,
    357502601,
    386778277,
    418451333,
    452718089,
    489790921,
    529899637,
    573292817,
    620239453,
    671030513,
    725980837,
    785430967,
    849749479,
    919334987,
    994618837,
    1076067617,
    1164186217,
    1259520799,
    1362662261,
    1474249943,
    1594975441,
    1725587117,
    1866894511,
    2019773507,
    2185171673,
    2364114217,
    2557710269,
    2767159799,
    2993761039,
    3238918481,
    3504151727,
    3791104843,
    4101556399,
    4294967291,
    6442450933,
    8589934583,
    12884901857,
    17179869143,
    25769803693,
];

/// Simulates a libstdc++ `std::unordered_map<int, T>` (or
/// `unordered_set<int>`) with the default `std::hash<int>` (identity),
/// `max_load_factor() == 1` and the prime rehash policy, to reproduce its
/// **iteration order** from the sequence of inserted keys.
///
/// Only insertion is modelled (no erase): upstream's order-relevant maps are
/// insert-only. Inserting an existing key is a no-op, like `m[key]` on a
/// present key. The simulation is `O(1)` amortised per insert, like the real
/// container (it keeps the real singly-linked node list and bucket array).
///
/// Mechanics (libstdc++ `_Hashtable`): a new node is linked right after its
/// bucket's "before" node, or — if its bucket is empty — at the very front of
/// the global list; a rehash relinks nodes in list order the same way. The
/// first insert allocates 13 buckets, then the table grows to the next prime
/// `>= 2·n` whenever it would exceed one key per bucket. So keys in distinct
/// buckets come out in *reverse* insertion order between rehashes
/// (`2,3,1` → `1,3,2`; `4,3,1,0` → `0,1,3,4`), keys sharing a bucket
/// (`k mod bucket_count`) stay grouped, and every rehash reverses the
/// bucket-head order once more.
///
/// Version note: this is the policy of current libstdc++, verified against
/// GCC 13; to our knowledge it is unchanged since the "11 buckets on first
/// insert" rule was introduced (GCC 7 era), so it should also hold for
/// upstream's reference build (Ubuntu 20.04, GCC 9) — not verified against
/// that toolchain. It reproduces the `nodes` order of the `cyclePath` and
/// `shortestPath` examples in upstream's SPARQL_QUERY.md; the `kHopEnumerate`
/// example there (`3,2,0,1` for the path `0→1→2→3`) matches no libstdc++
/// policy for that insertion order and is not authoritative.
#[derive(Debug, Clone)]
pub struct CxxUnorderedOrder {
    /// Hash code (= `size_t(key)`) per node.
    codes: Vec<u64>,
    /// Next node in the singly-linked list.
    next: Vec<u32>,
    /// `_M_before_begin._M_nxt`.
    head: u32,
    /// Per bucket: the node *before* the bucket's first node, `BEFORE_BEGIN`,
    /// or `NIL` when empty.
    buckets: Vec<u32>,
    next_resize: u64,
    index: HashMap<u64, u32>,
}

impl Default for CxxUnorderedOrder {
    fn default() -> Self {
        CxxUnorderedOrder::new()
    }
}

impl CxxUnorderedOrder {
    /// A default-constructed container (one bucket, nothing allocated).
    pub fn new() -> CxxUnorderedOrder {
        CxxUnorderedOrder {
            codes: Vec::new(),
            next: Vec::new(),
            head: NIL,
            buckets: vec![NIL],
            next_resize: 0,
            index: HashMap::new(),
        }
    }

    /// `std::unordered_map<int, T> m(bucket_count_hint)`.
    pub fn with_bucket_hint(hint: usize) -> CxxUnorderedOrder {
        let mut m = CxxUnorderedOrder::new();
        let n = m.next_bkt(hint as u64);
        if n > 1 {
            m.buckets = vec![NIL; n as usize];
        }
        m
    }

    /// Number of distinct keys.
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    /// Whether no key was inserted.
    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }

    /// Current `bucket_count()`.
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Whether `key` was inserted.
    pub fn contains(&self, key: i64) -> bool {
        self.index.contains_key(&(key as u64))
    }

    /// Insert `key` (an upstream `int`, sign-extended like `size_t(int)`).
    /// Returns `true` if it was new.
    pub fn insert(&mut self, key: i64) -> bool {
        let code = key as u64;
        if self.index.contains_key(&code) {
            return false;
        }
        let bkt_count = self.buckets.len() as u64;
        if let Some(n) = self.need_rehash(bkt_count, self.codes.len() as u64, 1) {
            self.rehash(n);
        }
        let bkt = (code % self.buckets.len() as u64) as usize;
        let node = self.codes.len() as u32;
        self.codes.push(code);
        self.next.push(NIL);
        let before = self.buckets[bkt];
        if before != NIL {
            let after = self.next_of(before);
            self.next[node as usize] = after;
            self.set_next(before, node);
        } else {
            self.next[node as usize] = self.head;
            self.head = node;
            let nxt = self.next[node as usize];
            if nxt != NIL {
                let b = (self.codes[nxt as usize] % self.buckets.len() as u64) as usize;
                self.buckets[b] = node;
            }
            self.buckets[bkt] = BEFORE_BEGIN;
        }
        self.index.insert(code, node);
        true
    }

    /// The keys in container iteration order.
    pub fn keys(&self) -> Vec<i64> {
        let mut out = Vec::with_capacity(self.codes.len());
        let mut p = self.head;
        while p != NIL {
            out.push(self.codes[p as usize] as i64);
            p = self.next[p as usize];
        }
        out
    }

    #[inline]
    fn next_of(&self, n: u32) -> u32 {
        if n == BEFORE_BEGIN {
            self.head
        } else {
            self.next[n as usize]
        }
    }

    #[inline]
    fn set_next(&mut self, n: u32, to: u32) {
        if n == BEFORE_BEGIN {
            self.head = to;
        } else {
            self.next[n as usize] = to;
        }
    }

    /// `_Prime_rehash_policy::_M_next_bkt`.
    fn next_bkt(&mut self, n: u64) -> u64 {
        if n < FAST_BKT.len() as u64 {
            if n == 0 {
                return 1;
            }
            let b = FAST_BKT[n as usize];
            self.next_resize = b;
            return b;
        }
        let i = PRIMES.partition_point(|&p| p < n);
        match PRIMES.get(i) {
            Some(&p) => {
                self.next_resize = p;
                p
            }
            None => {
                self.next_resize = u64::MAX;
                *PRIMES.last().unwrap_or(&n)
            }
        }
    }

    /// `_Prime_rehash_policy::_M_need_rehash` (load factor 1, growth 2).
    fn need_rehash(&mut self, n_bkt: u64, n_elt: u64, n_ins: u64) -> Option<u64> {
        if n_elt + n_ins > self.next_resize {
            let floor_min = if self.next_resize == 0 { 11 } else { 0 };
            let min_bkts = (n_elt + n_ins).max(floor_min);
            if min_bkts >= n_bkt {
                return Some(self.next_bkt((min_bkts + 1).max(n_bkt * 2)));
            }
            self.next_resize = n_bkt;
        }
        None
    }

    /// `_Hashtable::_M_rehash_aux(n, true_type)`.
    fn rehash(&mut self, n: u64) {
        let mut buckets = vec![NIL; n as usize];
        let mut p = self.head;
        self.head = NIL;
        let mut bbegin_bkt = 0usize;
        while p != NIL {
            let nxt = self.next[p as usize];
            let bkt = (self.codes[p as usize] % n) as usize;
            if buckets[bkt] == NIL {
                self.next[p as usize] = self.head;
                self.head = p;
                buckets[bkt] = BEFORE_BEGIN;
                if self.next[p as usize] != NIL {
                    buckets[bbegin_bkt] = p;
                }
                bbegin_bkt = bkt;
            } else {
                let before = buckets[bkt];
                let after = if before == BEFORE_BEGIN {
                    self.head
                } else {
                    self.next[before as usize]
                };
                self.next[p as usize] = after;
                if before == BEFORE_BEGIN {
                    self.head = p;
                } else {
                    self.next[before as usize] = p;
                }
            }
            p = nxt;
        }
        self.buckets = buckets;
    }
}

/// The iteration order of an `unordered_map<int, T>` / `unordered_set<int>`
/// after inserting `keys` in order (repeats ignored).
pub fn unordered_iteration_order<I: IntoIterator<Item = i64>>(keys: I) -> Vec<i64> {
    let mut m = CxxUnorderedOrder::new();
    for k in keys {
        m.insert(k);
    }
    m.keys()
}

// ---------------------------------------------------------------------------
// std::sort / std::partial_sort_copy
// ---------------------------------------------------------------------------

/// libstdc++ `_S_threshold`.
const SORT_THRESHOLD: usize = 16;

/// libstdc++ `std::sort(first, last, comp)` with `less` as `comp`:
/// introsort (median-of-three pivot moved to the front, unguarded Hoare
/// partition, heap-sort fallback at depth `2·⌊log2 n⌋`) followed by a final
/// insertion sort. The resulting order of *equivalent* elements is exactly
/// libstdc++'s. `less` must be a strict weak ordering for the result to be
/// meaningful; unlike C++ an inconsistent comparator (e.g. NaN keys) cannot
/// read out of bounds here.
pub fn cxx_sort_by<T: Clone, F: FnMut(&T, &T) -> bool>(v: &mut [T], mut less: F) {
    let n = v.len();
    if n < 2 {
        return;
    }
    let depth = 2 * (usize::BITS - 1 - n.leading_zeros()) as usize;
    introsort_loop(v, 0, n, depth, &mut less);
    final_insertion_sort(v, 0, n, &mut less);
}

fn introsort_loop<T: Clone, F: FnMut(&T, &T) -> bool>(
    v: &mut [T],
    first: usize,
    mut last: usize,
    mut depth: usize,
    less: &mut F,
) {
    while last - first > SORT_THRESHOLD {
        if depth == 0 {
            // __partial_sort(first, last, last) = heap_select + sort_heap.
            heap_select(v, first, last, last, less);
            sort_heap(v, first, last, less);
            return;
        }
        depth -= 1;
        let cut = unguarded_partition_pivot(v, first, last, less);
        introsort_loop(v, cut, last, depth, less);
        last = cut;
    }
}

fn move_median_to_first<T, F: FnMut(&T, &T) -> bool>(
    v: &mut [T],
    result: usize,
    a: usize,
    b: usize,
    c: usize,
    less: &mut F,
) {
    if less(&v[a], &v[b]) {
        if less(&v[b], &v[c]) {
            v.swap(result, b);
        } else if less(&v[a], &v[c]) {
            v.swap(result, c);
        } else {
            v.swap(result, a);
        }
    } else if less(&v[a], &v[c]) {
        v.swap(result, a);
    } else if less(&v[b], &v[c]) {
        v.swap(result, c);
    } else {
        v.swap(result, b);
    }
}

fn unguarded_partition_pivot<T: Clone, F: FnMut(&T, &T) -> bool>(
    v: &mut [T],
    first: usize,
    last: usize,
    less: &mut F,
) -> usize {
    let mid = first + (last - first) / 2;
    move_median_to_first(v, first, first + 1, mid, last - 1, less);
    // __unguarded_partition(first + 1, last, first)
    let pivot = first;
    let (mut lo, mut hi) = (first + 1, last);
    loop {
        while lo < last && less(&v[lo], &v[pivot]) {
            lo += 1;
        }
        hi -= 1;
        while hi > first && less(&v[pivot], &v[hi]) {
            hi -= 1;
        }
        if lo >= hi {
            return lo;
        }
        v.swap(lo, hi);
        lo += 1;
    }
}

fn final_insertion_sort<T: Clone, F: FnMut(&T, &T) -> bool>(
    v: &mut [T],
    first: usize,
    last: usize,
    less: &mut F,
) {
    if last - first > SORT_THRESHOLD {
        insertion_sort(v, first, first + SORT_THRESHOLD, less);
        for i in first + SORT_THRESHOLD..last {
            unguarded_linear_insert(v, first, i, less);
        }
    } else {
        insertion_sort(v, first, last, less);
    }
}

fn insertion_sort<T: Clone, F: FnMut(&T, &T) -> bool>(
    v: &mut [T],
    first: usize,
    last: usize,
    less: &mut F,
) {
    if first == last {
        return;
    }
    for i in first + 1..last {
        if less(&v[i], &v[first]) {
            // move_backward(first, i, i + 1); *first = val
            v[first..=i].rotate_right(1);
        } else {
            unguarded_linear_insert(v, first, i, less);
        }
    }
}

/// `__unguarded_linear_insert(last)`; `floor` only guards against an
/// inconsistent comparator (C++ would run off the front).
fn unguarded_linear_insert<T: Clone, F: FnMut(&T, &T) -> bool>(
    v: &mut [T],
    floor: usize,
    mut last: usize,
    less: &mut F,
) {
    let val = v[last].clone();
    while last > floor && less(&val, &v[last - 1]) {
        v[last] = v[last - 1].clone();
        last -= 1;
    }
    v[last] = val;
}

fn heap_select<T: Clone, F: FnMut(&T, &T) -> bool>(
    v: &mut [T],
    first: usize,
    middle: usize,
    last: usize,
    less: &mut F,
) {
    make_heap(v, first, middle, less);
    for i in middle..last {
        if less(&v[i], &v[first]) {
            pop_heap(v, first, middle, i, less);
        }
    }
}

fn make_heap<T: Clone, F: FnMut(&T, &T) -> bool>(
    v: &mut [T],
    first: usize,
    last: usize,
    less: &mut F,
) {
    let len = last - first;
    if len < 2 {
        return;
    }
    let mut parent = (len - 2) / 2;
    loop {
        let value = v[first + parent].clone();
        adjust_heap(v, first, parent, len, value, less);
        if parent == 0 {
            return;
        }
        parent -= 1;
    }
}

fn adjust_heap<T: Clone, F: FnMut(&T, &T) -> bool>(
    v: &mut [T],
    first: usize,
    mut hole: usize,
    len: usize,
    value: T,
    less: &mut F,
) {
    let top = hole;
    let mut second = hole;
    while second < (len.saturating_sub(1)) / 2 {
        second = 2 * (second + 1);
        if less(&v[first + second], &v[first + second - 1]) {
            second -= 1;
        }
        v[first + hole] = v[first + second].clone();
        hole = second;
    }
    if len & 1 == 0 && second == (len - 2) / 2 {
        second = 2 * (second + 1);
        v[first + hole] = v[first + second - 1].clone();
        hole = second - 1;
    }
    push_heap(v, first, hole, top, value, less);
}

fn push_heap<T: Clone, F: FnMut(&T, &T) -> bool>(
    v: &mut [T],
    first: usize,
    mut hole: usize,
    top: usize,
    value: T,
    less: &mut F,
) {
    while hole > top {
        let parent = (hole - 1) / 2;
        if !less(&v[first + parent], &value) {
            break;
        }
        v[first + hole] = v[first + parent].clone();
        hole = parent;
    }
    v[first + hole] = value;
}

/// `__pop_heap(first, last, result)`: move the top to `result` and re-heap
/// `[first, last)` with `result`'s old value.
fn pop_heap<T: Clone, F: FnMut(&T, &T) -> bool>(
    v: &mut [T],
    first: usize,
    last: usize,
    result: usize,
    less: &mut F,
) {
    let value = v[result].clone();
    v[result] = v[first].clone();
    adjust_heap(v, first, 0, last - first, value, less);
}

fn sort_heap<T: Clone, F: FnMut(&T, &T) -> bool>(
    v: &mut [T],
    first: usize,
    mut last: usize,
    less: &mut F,
) {
    while last - first > 1 {
        last -= 1;
        pop_heap(v, first, last, last, less);
    }
}

/// libstdc++ `std::partial_sort_copy(src.begin(), src.end(), dst.begin(),
/// dst.begin() + k, comp)`: the first `min(k, src.len())` elements of `src`
/// in `less` order, with libstdc++'s heap-based tie order. Returns the copied
/// prefix (upstream PPR then keeps the remaining default-initialised slots;
/// callers emulate that themselves).
pub fn cxx_partial_sort_copy_by<T: Clone, F: FnMut(&T, &T) -> bool>(
    src: &[T],
    k: usize,
    mut less: F,
) -> Vec<T> {
    if k == 0 {
        return Vec::new();
    }
    let take = k.min(src.len());
    let mut out: Vec<T> = src[..take].to_vec();
    make_heap(&mut out, 0, take, &mut less);
    for x in &src[take..] {
        if less(x, &out[0]) {
            adjust_heap(&mut out, 0, 0, take, x.clone(), &mut less);
        }
    }
    sort_heap(&mut out, 0, take, &mut less);
    out
}

// ---------------------------------------------------------------------------
// glibc rand()
// ---------------------------------------------------------------------------

/// glibc's default `rand()` (the TYPE_3 additive feedback generator,
/// `r[i] = r[i-3] + r[i-31]`, output `r >> 1`), seeded like `srand(seed)`.
/// `RAND_MAX` is `2147483647`.
///
/// Reproducible use in upstream: `labelProp` calls `std::srand(114514)`
/// (labelProp.cpp:176) and then `rand() % maxNum` for tie-breaks, so
/// `GlibcRand::new(114514)` reproduces its choices exactly. Upstream seeds
/// FORA/PPR with `srand(time(NULL))` (SSPPR.cpp:100), which is not
/// reproducible; ports use a fixed documented seed instead (see `ppr`).
#[derive(Debug, Clone)]
pub struct GlibcRand {
    /// Ring buffer of the last 34 state words.
    r: [u32; 34],
    /// Index of the next word to produce (mod 34).
    i: usize,
}

/// glibc `RAND_MAX`.
pub const RAND_MAX: i32 = i32::MAX;

impl GlibcRand {
    /// `srand(seed)` (glibc maps seed 0 to 1, as does `rand()` without
    /// `srand`).
    pub fn new(seed: u32) -> GlibcRand {
        let seed = if seed == 0 { 1 } else { seed };
        let mut r = [0u32; 34];
        r[0] = seed;
        let mut word = seed as i32 as i64;
        for slot in r.iter_mut().take(31).skip(1) {
            let hi = word / 127_773;
            let lo = word % 127_773;
            word = 16_807 * lo - 2_836 * hi;
            if word < 0 {
                word += 2_147_483_647;
            }
            *slot = word as u32;
        }
        for k in 31..34 {
            r[k] = r[k - 31];
        }
        let mut g = GlibcRand { r, i: 34 };
        for _ in 34..344 {
            g.step();
        }
        g
    }

    #[inline]
    fn step(&mut self) -> u32 {
        let idx = self.i % 34;
        let v = self.r[(self.i + 34 - 31) % 34].wrapping_add(self.r[(self.i + 34 - 3) % 34]);
        self.r[idx] = v;
        self.i += 1;
        v
    }

    /// The next `rand()` value in `0..=RAND_MAX`.
    pub fn rand(&mut self) -> i32 {
        (self.step() >> 1) as i32
    }

    /// `(double)rand() / (double)RAND_MAX` — FORA's Bernoulli draw
    /// (PathQueryHandler.cpp:3126).
    pub fn rand_unit(&mut self) -> f64 {
        f64::from(self.rand()) / f64::from(RAND_MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unordered_order_matches_libstdcxx() {
        // Goldens printed by g++ 13 iterating std::unordered_map<int,int>.
        assert_eq!(unordered_iteration_order([2, 3, 1, 2]), vec![1, 3, 2]);
        assert_eq!(unordered_iteration_order([4, 3, 1, 0]), vec![0, 1, 3, 4]);
        assert_eq!(unordered_iteration_order([0, 1, 2, 3]), vec![3, 2, 1, 0]);
        let want40: Vec<i64> = (29..40).rev().chain((0..13).rev()).chain(13..29).collect();
        assert_eq!(unordered_iteration_order(0..40), want40);
        let b: Vec<i64> = (0..30).map(|i| (i * 37) % 101).collect();
        assert_eq!(
            unordered_iteration_order(b),
            vec![
                63, 40, 67, 30, 3, 90, 94, 57, 20, 84, 26, 47, 10, 97, 74, 16, 37, 0, 87, 77, 13,
                50, 23, 60, 33, 70, 6, 43, 80, 53
            ]
        );
        let c: Vec<i64> = (0..25).map(|i| i * 13).collect();
        assert_eq!(
            unordered_iteration_order(c),
            vec![
                312, 299, 286, 273, 260, 247, 234, 221, 208, 195, 182, 169, 0, 13, 26, 39, 52, 65,
                78, 91, 104, 117, 130, 143, 156
            ]
        );
        let mut m = CxxUnorderedOrder::new();
        let e: Vec<i64> = (0..300).map(|i| (i * 7919) % 5003).collect();
        for &k in &e {
            m.insert(k);
        }
        assert_eq!(m.bucket_count(), 541);
        let got = m.keys();
        assert_eq!(&got[..8], &[1362, 533, 962, 133, 1391, 562, 991, 162]);
        assert_eq!(&got[got.len() - 6..], &[278, 1049, 678, 249, 649, 220][..]);
        assert_eq!(got.len(), 300);
        // FNV-1a over the whole g++ order (three rehashes: 13 → 29 → … → 541).
        let mut h: u64 = 1_469_598_103_934_665_603;
        for &k in &got {
            h ^= k as u64;
            h = h.wrapping_mul(1_099_511_628_211);
        }
        assert_eq!(h, 0x3528_eb54_8db0_b953);
    }

    #[test]
    fn unordered_order_of_glibc_random_keys() {
        // g++: `srand(7); for 60 times m[rand() % 1000] = 1;` then iterate
        // (two keys repeat, so 58 remain). Cross-checks both emulations.
        let mut r = GlibcRand::new(7);
        let keys: Vec<i64> = (0..60).map(|_| i64::from(r.rand() % 1000)).collect();
        assert_eq!(
            unordered_iteration_order(keys),
            vec![
                695, 680, 935, 505, 437, 201, 413, 145, 300, 47, 829, 186, 953, 907, 855, 580, 934,
                498, 321, 315, 153, 508, 744, 213, 331, 600, 899, 620, 443, 325, 586, 963, 222,
                871, 299, 526, 1, 178, 961, 897, 720, 749, 806, 865, 570, 677, 736, 504, 257, 788,
                530, 884, 97, 746, 392, 466, 112, 230
            ]
        );
    }

    #[test]
    fn unordered_order_tracks_the_docs_examples() {
        // SPARQL_QUERY.md: the `nodes` arrays of the documented paths.
        assert_eq!(unordered_iteration_order([4, 3, 1, 0]), vec![0, 1, 3, 4]);
        assert_eq!(unordered_iteration_order([2, 3, 1, 2]), vec![1, 3, 2]);
        assert_eq!(unordered_iteration_order([0, 1, 2, 3]), vec![3, 2, 1, 0]);
    }

    #[test]
    fn bucket_hint_constructor() {
        let mut m = CxxUnorderedOrder::with_bucket_hint(10);
        assert_eq!(m.bucket_count(), 11);
        for i in 0..12 {
            m.insert(i * 7);
        }
        assert_eq!(m.bucket_count(), 23);
        assert_eq!(m.keys(), vec![77, 0, 7, 14, 21, 28, 35, 42, 49, 56, 63, 70]);
        assert!(m.contains(77) && !m.contains(78));
        assert!(!m.insert(77));
        assert_eq!(m.len(), 12);
    }

    /// The same deterministic inputs as the C++ golden generator: pairs
    /// `(i, value)` sorted by `value` descending (PR's comparator).
    fn sort_case(n: usize, b: u64) -> Vec<(i32, f64)> {
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15 ^ (n as u64 * 131 + b);
        let mut v = Vec::with_capacity(n);
        for i in 0..n {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            v.push((i as i32, ((s >> 33) % b) as f64));
        }
        cxx_sort_by(&mut v, |l, r| l.1 > r.1);
        v
    }

    fn fnv(v: &[(i32, f64)]) -> u64 {
        let mut h: u64 = 1_469_598_103_934_665_603;
        for p in v {
            h ^= p.0 as u32 as u64;
            h = h.wrapping_mul(1_099_511_628_211);
        }
        h
    }

    #[test]
    fn std_sort_matches_libstdcxx_tie_order() {
        // (n, buckets, fnv-1a of the index order, first indices) from g++ 13.
        let golden: &[(usize, u64, u64, &[i32])] = &[
            (5, 3, 0xbcf671d576e18685, &[0, 2, 3, 4, 1]),
            (5, 7, 0xd6f648d5859c65af, &[0, 2, 4, 3, 1]),
            (
                16,
                3,
                0xddf77c7ce21f11ef,
                &[3, 9, 11, 12, 14, 15, 1, 4, 5, 6, 7, 8],
            ),
            (
                16,
                1,
                0x088b84ce5271b343,
                &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            ),
            (
                17,
                1,
                0xa881bacc8cda3069,
                &[8, 16, 15, 14, 13, 12, 11, 10, 9, 0, 7, 6],
            ),
            (
                17,
                3,
                0x3d41a2686f93c529,
                &[0, 2, 7, 8, 16, 15, 14, 13, 12, 10, 5, 4],
            ),
            (
                17,
                7,
                0x62179d87ab00826b,
                &[10, 2, 6, 5, 9, 4, 13, 11, 12, 14, 8, 16],
            ),
            (
                33,
                1,
                0x3c426b00a698ef07,
                &[25, 17, 18, 19, 20, 21, 22, 23, 24, 16, 26, 27],
            ),
            (
                33,
                3,
                0xb4b27b6db0fe4eb5,
                &[16, 32, 31, 29, 25, 24, 19, 6, 3, 0, 4, 23],
            ),
            (
                33,
                1000000,
                0x8a23a2a75b7f3ea5,
                &[31, 25, 24, 23, 29, 19, 26, 27, 30, 16, 18, 7],
            ),
            (
                100,
                1,
                0x14bd58780968b56b,
                &[62, 74, 73, 72, 71, 70, 69, 68, 67, 66, 65, 64],
            ),
            (
                100,
                3,
                0x442366b67d271dd7,
                &[54, 20, 62, 61, 59, 27, 28, 89, 30, 31, 22, 36],
            ),
            (
                100,
                7,
                0x01d1dbaa6483eb9b,
                &[90, 74, 81, 36, 28, 42, 86, 20, 67, 12, 6, 57],
            ),
            (
                1000,
                1,
                0xa8656c3bd97424bb,
                &[672, 657, 658, 659, 660, 661, 662, 663, 664, 665, 666, 667],
            ),
            (
                1000,
                3,
                0xb8defbd9508e2977,
                &[687, 679, 680, 297, 681, 294, 293, 292, 683, 685, 686, 288],
            ),
            (
                1000,
                7,
                0x5d9eabd4ddf6abdf,
                &[116, 652, 904, 822, 368, 367, 750, 274, 898, 363, 748, 650],
            ),
            (
                1000,
                1000000,
                0xef1aaab0fdfcb91f,
                &[353, 707, 820, 505, 533, 14, 83, 504, 205, 778, 491, 771],
            ),
        ];
        for &(n, b, hash, prefix) in golden {
            let v = sort_case(n, b);
            let idx: Vec<i32> = v.iter().take(prefix.len()).map(|p| p.0).collect();
            assert_eq!(idx, prefix, "n={n} b={b}");
            assert_eq!(fnv(&v), hash, "n={n} b={b}");
            assert!(v.windows(2).all(|w| w[0].1 >= w[1].1));
        }
        assert!(sort_case(0, 3).is_empty());
        assert_eq!(sort_case(1, 3).len(), 1);
    }

    #[test]
    fn std_sort_survives_inconsistent_comparator() {
        let mut v: Vec<f64> = (0..200)
            .map(|i| if i % 3 == 0 { f64::NAN } else { i as f64 })
            .collect();
        cxx_sort_by(&mut v, |a, b| a < b);
        assert_eq!(v.len(), 200);
    }

    #[test]
    fn partial_sort_copy_matches_libstdcxx_tie_order() {
        // (n, buckets, k, copied indices) from g++ 13 `std::partial_sort_copy`.
        let golden: &[(usize, u64, usize, &[i32])] = &[
            (3, 3, 5, &[2, 0, 1]),
            (20, 1, 5, &[3, 4, 1, 0, 2]),
            (
                20,
                1,
                17,
                &[8, 10, 4, 16, 12, 3, 9, 1, 11, 5, 15, 13, 7, 0, 2, 6, 14],
            ),
            (
                20,
                1,
                200,
                &[
                    18, 10, 4, 16, 12, 8, 19, 1, 11, 5, 15, 13, 7, 17, 3, 9, 0, 2, 6, 14,
                ],
            ),
            (20, 3, 5, &[3, 1, 6, 8, 2]),
            (
                20,
                7,
                17,
                &[7, 0, 13, 18, 6, 8, 10, 15, 16, 3, 4, 9, 19, 17, 1, 5, 14],
            ),
            (100, 3, 1, &[4]),
            (
                100,
                3,
                17,
                &[
                    38, 31, 36, 7, 12, 44, 4, 46, 11, 5, 22, 27, 43, 48, 30, 28, 20,
                ],
            ),
            (
                100,
                7,
                17,
                &[
                    36, 10, 86, 57, 12, 97, 20, 98, 28, 74, 55, 42, 90, 81, 67, 6, 56,
                ],
            ),
        ];
        for &(n, b, k, want) in golden {
            let mut s: u64 = 0x9E37_79B9_7F4A_7C15 ^ (n as u64 * 131 + b);
            let src: Vec<(i32, f64)> = (0..n)
                .map(|i| {
                    s = s
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    (i as i32, ((s >> 33) % b) as f64)
                })
                .collect();
            let got: Vec<i32> = cxx_partial_sort_copy_by(&src, k, |l, r| l.1 > r.1)
                .iter()
                .map(|p| p.0)
                .collect();
            assert_eq!(got, want, "n={n} b={b} k={k}");
        }
    }

    #[test]
    fn partial_sort_copy_is_sorted_prefix() {
        let src: Vec<(i32, f64)> = (0..50).map(|i| (i, ((i * 7) % 11) as f64)).collect();
        let top = cxx_partial_sort_copy_by(&src, 5, |l, r| l.1 > r.1);
        assert_eq!(top.len(), 5);
        assert!(top.iter().all(|p| p.1 == 10.0));
        assert_eq!(
            cxx_partial_sort_copy_by(&src[..3], 10, |l, r| l.1 > r.1).len(),
            3
        );
        assert!(cxx_partial_sort_copy_by(&src, 0, |l, r| l.1 > r.1).is_empty());
    }

    #[test]
    fn glibc_rand_matches() {
        let mut r = GlibcRand::new(1);
        let got: Vec<i32> = (0..5).map(|_| r.rand()).collect();
        assert_eq!(
            got,
            vec![1804289383, 846930886, 1681692777, 1714636915, 1957747793]
        );
        let mut r = GlibcRand::new(114514);
        let got: Vec<i32> = (0..8).map(|_| r.rand()).collect();
        assert_eq!(
            got,
            vec![
                2087637649, 1995966849, 858559000, 724628000, 749315281, 2116046980, 893914299,
                839874544
            ]
        );
        let mut r0 = GlibcRand::new(0);
        assert_eq!(r0.rand(), 1804289383);
        let u = GlibcRand::new(7).rand_unit();
        assert!((0.0..=1.0).contains(&u));
    }
}
