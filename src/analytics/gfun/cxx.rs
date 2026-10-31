//! Bit-exact emulation of the C/C++ runtime behaviour upstream gStore's graph
//! functions bake into their output (GCC / libstdc++ / glibc on Linux, the
//! platform upstream ships on):
//!
//! * number formatting — `std::ostream << double` ([`fmt_g`]),
//!   `std::to_string(double)` ([`fmt_f`]) and `nlohmann::json::dump()` of a
//!   double ([`fmt_nlohmann`]);
//! * the iteration order of `std::unordered_map<int, _>` /
//!   `std::unordered_set<int>` ([`CxxHashOrder`]) — upstream prints path nodes,
//!   `SSSP`/`SSSPLen` results and `WCC` members in that order;
//! * glibc's `srand`/`rand` ([`GlibcRand`]) — upstream's label propagation is
//!   seeded with `srand(114514)`, so reproducing `rand()` reproduces its
//!   communities exactly.
//!
//! The reference values in this module's tests were produced by compiling the
//! equivalent C++ with g++ 13 / libstdc++ / glibc and nlohmann/json 3.x (the
//! header upstream vendors in `include/nlohmann/json.hpp`).

/// `std::ostream << double` with the default flags and precision 6 — C's
/// `printf("%g")`: 6 significant digits, trailing zeros removed, scientific
/// notation (`1e+06`, `1.5e-05`; at least two exponent digits) when the
/// decimal exponent is `< -4` or `>= 6`. `nan`/`-nan`/`inf`/`-inf` as glibc
/// prints them. Also the format of `ostream << float` (the float is widened).
pub fn fmt_g(v: f64) -> String {
    fmt_g_prec(v, 6)
}

/// [`fmt_g`] with an explicit precision (`std::setprecision(p)`; `p == 0`
/// behaves as 1, like C).
pub fn fmt_g_prec(v: f64, prec: usize) -> String {
    if let Some(s) = non_finite(v, "nan", "inf") {
        return s;
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    let p = prec.max(1);
    let sci = format!("{:.*e}", p - 1, v);
    let (mant, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let x: i32 = exp.parse().unwrap_or(0);
    if x < -4 || x >= p as i32 {
        let mant = strip_fraction_zeros(mant);
        format!("{mant}e{}{:02}", if x < 0 { '-' } else { '+' }, x.unsigned_abs())
    } else {
        let decimals = (p as i32 - 1 - x).max(0) as usize;
        strip_fraction_zeros(&format!("{:.*}", decimals, v)).to_string()
    }
}

/// `std::to_string(double)` — C's `printf("%f")`: fixed notation with six
/// decimals (`0.555556`, `100000.000000`, `-0.000000`), `nan`/`-nan`/`inf`.
pub fn fmt_f(v: f64) -> String {
    if let Some(s) = non_finite(v, "nan", "inf") {
        return s;
    }
    format!("{v:.6}")
}

/// `nlohmann::json(double).dump()`: the shortest round-trip digits, printed
/// in fixed notation when the decimal point position `n` satisfies
/// `-4 < n <= 15` (always with a fractional part: `1.0`, `100000.0`), else
/// as `d.ddde±XX`; NaN and infinities serialise as `null`.
///
/// (nlohmann uses Grisu2, which in rare cases emits one digit more than the
/// shortest representation Rust produces; the two agree on every value in
/// this module's tests.)
pub fn fmt_nlohmann(v: f64) -> String {
    if !v.is_finite() {
        return "null".to_string();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    let sign = if v < 0.0 { "-" } else { "" };
    let sci = format!("{:e}", v.abs());
    let (mant, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let x: i32 = exp.parse().unwrap_or(0);
    let k = digits.len() as i32;
    let n = x + 1; // position of the decimal point relative to the digits
    const MIN_EXP: i32 = -4;
    const MAX_EXP: i32 = 15;
    let body = if k <= n && n <= MAX_EXP {
        format!("{digits}{}.0", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= MAX_EXP {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if MIN_EXP < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let m = if k == 1 {
            digits.clone()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.unsigned_abs())
    };
    format!("{sign}{body}")
}

fn non_finite(v: f64, nan: &str, inf: &str) -> Option<String> {
    if v.is_nan() {
        Some(if v.is_sign_negative() { format!("-{nan}") } else { nan.to_string() })
    } else if v.is_infinite() {
        Some(if v < 0.0 { format!("-{inf}") } else { inf.to_string() })
    } else {
        None
    }
}

fn strip_fraction_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// libstdc++ unordered container iteration order
// ---------------------------------------------------------------------------

/// Bucket counts a default-constructed libstdc++ unordered container passes
/// through as it grows one insertion at a time (`_Prime_rehash_policy`, max
/// load factor 1.0): 13 buckets for the first element, then
/// `_M_next_bkt(2 * buckets)` whenever the element count would exceed the
/// bucket count. Generated with libstdc++ (GCC 13); the policy is unchanged
/// since GCC 5.
const LIBSTDCXX_GROWTH: &[u64] = &[
    13,
    29,
    59,
    127,
    257,
    541,
    1109,
    2357,
    5087,
    10273,
    20753,
    42043,
    85229,
    172933,
    351061,
    712697,
    1447153,
    2938679,
    5967347,
    12117689,
    24607243,
    49969847,
    101473717,
    206062531,
    418451333,
    849749479,
    1725587117,
    3504151727,
    8589934583,
    25769803693,
    68719476731,
    206158430123,
    412316860387,
    1099511627689,
];

const NIL: u32 = u32::MAX;
/// The `_M_before_begin` sentinel as a bucket's "node before the first".
const BEFORE_BEGIN: u32 = u32::MAX - 1;

/// Emulates the node list of a libstdc++ `std::unordered_set<int>` /
/// `std::unordered_map<int, T>` (identity `std::hash<int>`, prime bucket
/// counts, max load factor 1) under insertions, reproducing its iteration
/// order exactly — including the reordering every rehash causes.
///
/// Only insertion is modelled (erasing is not needed by any graph function
/// output). Keys are the containers' `int` keys; pass entity ids as they are
/// (`std::hash<int>` of a non-negative int is the value itself).
///
/// ```
/// # use gstore::analytics::gfun::cxx::CxxHashOrder;
/// let mut m = CxxHashOrder::new();
/// for k in [2, 3, 1, 2] {
///     m.insert(k);
/// }
/// assert_eq!(m.iter().collect::<Vec<_>>(), vec![1, 3, 2]);
/// ```
#[derive(Debug, Clone, Default)]
pub struct CxxHashOrder {
    keys: Vec<u64>,
    next: Vec<u32>,
    head: u32,
    /// Per bucket: the node *before* its first node (`BEFORE_BEGIN` for the
    /// list head), or `NIL` when empty. Empty until the first insertion.
    buckets: Vec<u32>,
    growth_step: usize,
    index: std::collections::HashMap<u64, u32>,
}

impl CxxHashOrder {
    /// An empty container (libstdc++'s default-constructed state).
    pub fn new() -> CxxHashOrder {
        CxxHashOrder {
            head: NIL,
            ..CxxHashOrder::default()
        }
    }

    /// Number of distinct keys inserted.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether nothing was inserted.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Whether `key` is present.
    pub fn contains(&self, key: i64) -> bool {
        self.index.contains_key(&(key as u64))
    }

    /// The current bucket count (`bucket_count()`; 1 before the first insert).
    pub fn bucket_count(&self) -> u64 {
        if self.buckets.is_empty() {
            1
        } else {
            self.buckets.len() as u64
        }
    }

    /// `insert(key)` / `map[key]`: adds `key` if absent; returns whether it was
    /// newly inserted. An existing key keeps its position.
    pub fn insert(&mut self, key: i64) -> bool {
        let h = key as u64;
        if self.index.contains_key(&h) {
            return false;
        }
        let n = self.keys.len() as u64;
        if self.buckets.is_empty() {
            self.rehash(LIBSTDCXX_GROWTH[0]);
        } else if n + 1 > self.buckets.len() as u64 {
            let step = (self.growth_step + 1).min(LIBSTDCXX_GROWTH.len() - 1);
            self.growth_step = step;
            self.rehash(LIBSTDCXX_GROWTH[step]);
        }
        let node = self.keys.len() as u32;
        self.keys.push(h);
        self.next.push(NIL);
        self.index.insert(h, node);
        let bkt = self.bucket_of(h);
        let prev = self.buckets[bkt];
        if prev != NIL {
            // Front of the bucket's run: right after its "before" node.
            self.next[node as usize] = self.next_of(prev);
            self.set_next(prev, node);
        } else {
            // New bucket: the node becomes the global list head.
            self.next[node as usize] = self.head;
            self.head = node;
            let after = self.next[node as usize];
            if after != NIL {
                let b = self.bucket_of(self.keys[after as usize]);
                self.buckets[b] = node;
            }
            self.buckets[bkt] = BEFORE_BEGIN;
        }
        true
    }

    /// Keys in the container's iteration order.
    pub fn iter(&self) -> impl Iterator<Item = i64> + '_ {
        let mut cur = self.head;
        std::iter::from_fn(move || {
            if cur == NIL {
                None
            } else {
                let k = self.keys[cur as usize] as i64;
                cur = self.next[cur as usize];
                Some(k)
            }
        })
    }

    fn bucket_of(&self, h: u64) -> usize {
        (h % self.buckets.len() as u64) as usize
    }

    fn next_of(&self, node: u32) -> u32 {
        if node == BEFORE_BEGIN {
            self.head
        } else {
            self.next[node as usize]
        }
    }

    fn set_next(&mut self, node: u32, to: u32) {
        if node == BEFORE_BEGIN {
            self.head = to;
        } else {
            self.next[node as usize] = to;
        }
    }

    /// `_M_rehash_aux(n, true_type)`: relink every node into `n` buckets,
    /// walking the old list order.
    fn rehash(&mut self, n: u64) {
        let mut nb = vec![NIL; n as usize];
        let mut p = self.head;
        self.head = NIL;
        let mut bbegin_bkt = 0usize;
        while p != NIL {
            let next = self.next[p as usize];
            let bkt = (self.keys[p as usize] % n) as usize;
            if nb[bkt] == NIL {
                self.next[p as usize] = self.head;
                self.head = p;
                nb[bkt] = BEFORE_BEGIN;
                if self.next[p as usize] != NIL {
                    nb[bbegin_bkt] = p;
                }
                bbegin_bkt = bkt;
            } else {
                let before = nb[bkt];
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
            p = next;
        }
        self.buckets = nb;
    }
}

/// Iteration order of a libstdc++ `unordered_map<int, _>` / `unordered_set<int>`
/// filled by inserting `keys` in order (duplicates ignored).
pub fn unordered_order(keys: impl IntoIterator<Item = i64>) -> Vec<i64> {
    let mut m = CxxHashOrder::new();
    for k in keys {
        m.insert(k);
    }
    m.iter().collect()
}

// ---------------------------------------------------------------------------
// glibc rand()
// ---------------------------------------------------------------------------

/// glibc's `RAND_MAX`.
pub const RAND_MAX: i32 = 2_147_483_647;

/// glibc `srand(seed)` / `rand()` (the default `TYPE_3` additive feedback
/// generator, degree 31, separation 3). A deterministic, seedable stream:
/// with the seed upstream uses (e.g. `srand(114514)` in `labelProp`) it yields
/// the same numbers upstream's `rand()` calls saw.
///
/// Upstream seeds FORA/PPR random walks with `srand(time(NULL))`, i.e.
/// non-deterministically; a Rust port seeds it with a fixed documented
/// constant instead, so results are reproducible.
#[derive(Debug, Clone)]
pub struct GlibcRand {
    state: [i32; 31],
    f: usize,
    r: usize,
}

impl GlibcRand {
    /// `srand(seed)` (seed 0 behaves as 1, as in glibc).
    pub fn new(seed: u32) -> GlibcRand {
        let mut state = [0i32; 31];
        let seed = if seed == 0 { 1 } else { seed };
        state[0] = seed as i32;
        let mut word = i64::from(state[0]);
        for slot in state.iter_mut().skip(1) {
            let hi = word / 127_773;
            let lo = word % 127_773;
            word = 16_807 * lo - 2_836 * hi;
            if word < 0 {
                word += 2_147_483_647;
            }
            *slot = word as i32;
        }
        let mut g = GlibcRand { state, f: 3, r: 0 };
        for _ in 0..310 {
            g.next_raw();
        }
        g
    }

    fn next_raw(&mut self) -> i32 {
        let val = (self.state[self.f] as u32).wrapping_add(self.state[self.r] as u32);
        self.state[self.f] = val as i32;
        self.f = (self.f + 1) % 31;
        self.r = (self.r + 1) % 31;
        (val >> 1) as i32
    }

    /// `rand()`: the next value in `0..=RAND_MAX`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> i32 {
        self.next_raw()
    }

    /// `(double)rand() / (double)RAND_MAX` (upstream's Bernoulli draw).
    pub fn next_unit(&mut self) -> f64 {
        f64::from(self.next()) / f64::from(RAND_MAX)
    }

    /// `rand() % m` for `m > 0` (upstream's index draw; `m <= 0` yields 0).
    pub fn below(&mut self, m: i32) -> i32 {
        let x = self.next();
        if m <= 0 {
            0
        } else {
            x % m
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `printf("%.17g|", v)` input followed by ostream / to_string / nlohmann
    /// output, as printed by the reference C++ program.
    const FMT_GOLDEN: &[(f64, &str, &str, &str)] = &[
        (0.0, "0", "0.000000", "0.0"),
        (-0.0, "-0", "-0.000000", "-0.0"),
        (1.0, "1", "1.000000", "1.0"),
        (0.5, "0.5", "0.500000", "0.5"),
        (6.5, "6.5", "6.500000", "6.5"),
        (1.0 / 3.0, "0.333333", "0.333333", "0.3333333333333333"),
        (2.0 / 3.0, "0.666667", "0.666667", "0.6666666666666666"),
        (0.555555555, "0.555556", "0.555556", "0.555555555"),
        (1e-5, "1e-05", "0.000010", "1e-05"),
        (1.5e-5, "1.5e-05", "0.000015", "1.5e-05"),
        (0.0001, "0.0001", "0.000100", "0.0001"),
        (0.00012345678, "0.000123457", "0.000123", "0.00012345678"),
        (100000.0, "100000", "100000.000000", "100000.0"),
        (999999.4, "999999", "999999.400000", "999999.4"),
        (999999.5, "1e+06", "999999.500000", "999999.5"),
        (1000000.0, "1e+06", "1000000.000000", "1000000.0"),
        (1234567.0, "1.23457e+06", "1234567.000000", "1234567.0"),
        (123456.5, "123456", "123456.500000", "123456.5"),
        (0.1, "0.1", "0.100000", "0.1"),
        (0.15, "0.15", "0.150000", "0.15"),
        (1e300, "1e+300", "", "1e+300"),
        (-2.5, "-2.5", "-2.500000", "-2.5"),
        (1e-300, "1e-300", "0.000000", "1e-300"),
        (0.85f32 as f64, "0.85", "0.850000", "0.8500000238418579"),
        (3.0e-7, "3e-07", "0.000000", "3e-07"),
        (12.0, "12", "12.000000", "12.0"),
        (0.333333343267, "0.333333", "0.333333", "0.333333343267"),
        (4.0, "4", "4.000000", "4.0"),
        (1e15, "1e+15", "1000000000000000.000000", "1e+15"),
        (1e16, "1e+16", "10000000000000000.000000", "1e+16"),
        (1e21, "1e+21", "1000000000000000000000.000000", "1e+21"),
        (123456789012.0, "1.23457e+11", "123456789012.000000", "123456789012.0"),
        (0.000123, "0.000123", "0.000123", "0.000123"),
        (9.9999995, "10", "9.999999", "9.9999995"),
        (0.30000000000000004, "0.3", "0.300000", "0.30000000000000004"),
        (2.5e-5, "2.5e-05", "0.000025", "2.5e-05"),
        (0.000125, "0.000125", "0.000125", "0.000125"),
        (1e100, "1e+100", "", "1e+100"),
        (5e-324, "4.94066e-324", "0.000000", "5e-324"),
        (1.7976931348623157e308, "1.79769e+308", "", "1.7976931348623157e+308"),
        (0.1666666666666666, "0.166667", "0.166667", "0.1666666666666666"),
        (1234.5, "1234.5", "1234.500000", "1234.5"),
        (0.0000005, "5e-07", "0.000000", "5e-07"),
    ];

    #[test]
    fn number_formats_match_the_cxx_runtime() {
        for &(v, g, f, nl) in FMT_GOLDEN {
            assert_eq!(fmt_g(v), g, "ostream << {v:e}");
            if !f.is_empty() {
                assert_eq!(fmt_f(v), f, "to_string({v:e})");
            }
            assert_eq!(fmt_nlohmann(v), nl, "nlohmann dump {v:e}");
        }
        // 1e300 via %f prints the exact decimal expansion.
        assert!(fmt_f(1e300).starts_with("1000000000000000052504760255204420248704468581108159"));
        assert!(fmt_f(1e300).ends_with(".000000"));
        assert_eq!(fmt_g(f64::NAN), "nan");
        assert_eq!(fmt_g(-f64::NAN), "-nan");
        assert_eq!(fmt_g(f64::INFINITY), "inf");
        assert_eq!(fmt_g(f64::NEG_INFINITY), "-inf");
        assert_eq!(fmt_f(f64::NAN), "nan");
        assert_eq!(fmt_f(f64::NEG_INFINITY), "-inf");
        assert_eq!(fmt_nlohmann(f64::NAN), "null");
        // ostream << float widens the float first.
        assert_eq!(fmt_g(f64::from(0.333333f32)), "0.333333");
        assert_eq!(fmt_g(f64::from(2.0f32 / 3.0)), "0.666667");
    }

    #[test]
    fn glibc_rand_sequences() {
        let seq = |seed: u32| {
            let mut r = GlibcRand::new(seed);
            (0..12).map(|_| r.next()).collect::<Vec<_>>()
        };
        assert_eq!(
            seq(114514),
            vec![
                2087637649, 1995966849, 858559000, 724628000, 749315281, 2116046980, 893914299, 839874544,
                155527830, 787918980, 1841014402, 515412493
            ]
        );
        let one = vec![
            1804289383, 846930886, 1681692777, 1714636915, 1957747793, 424238335, 719885386, 1649760492,
            596516649, 1189641421, 1025202362, 1350490027,
        ];
        assert_eq!(seq(1), one);
        assert_eq!(seq(0), one, "srand(0) behaves as srand(1)");
        assert_eq!(
            seq(42),
            vec![
                71876166, 708592740, 1483128881, 907283241, 442951012, 537146758, 1366999021, 1854614940,
                647800535, 53523743, 783815874, 1643643143
            ]
        );
    }

    fn order(keys: impl IntoIterator<Item = i64>) -> (Vec<i64>, u64) {
        let mut m = CxxHashOrder::new();
        for k in keys {
            m.insert(k);
        }
        (m.iter().collect(), m.bucket_count())
    }

    #[test]
    fn unordered_iteration_order_small() {
        assert_eq!(order([2, 3, 1, 2]), (vec![1, 3, 2], 13));
        assert_eq!(order([0, 13, 26, 1, 14]), (vec![14, 1, 26, 13, 0], 13));
        assert_eq!(order([5, 18, 4, 31, 44, 57, 70]), (vec![4, 70, 57, 44, 31, 18, 5], 13));
        assert_eq!(order(std::iter::empty()), (vec![], 1));
    }

    #[test]
    fn unordered_iteration_order_across_rehashes() {
        let (o4, b4) = order((0..14).map(|i| i * 7 + 3));
        assert_eq!(b4, 29);
        assert_eq!(o4, vec![94, 3, 10, 17, 24, 31, 38, 45, 52, 59, 66, 73, 80, 87]);
        let (o5, b5) = order((0..30).map(|i| (i * 37) % 101));
        assert_eq!(b5, 59);
        assert_eq!(
            o5,
            vec![
                63, 40, 67, 30, 3, 90, 94, 57, 20, 84, 26, 47, 10, 97, 74, 16, 37, 0, 87, 77, 13, 50, 23, 60, 33,
                70, 6, 43, 80, 53
            ]
        );
        let (o6, b6) = order((0..70).map(|i| (i * 131 + 17) % 1000));
        assert_eq!(b6, 127);
        assert_eq!(
            o6,
            vec![
                56, 925, 794, 663, 532, 401, 270, 139, 8, 877, 746, 292, 410, 174, 161, 279, 43, 30, 148, 899, 781,
                768, 650, 637, 519, 506, 388, 375, 257, 244, 126, 113, 982, 864, 851, 733, 615, 720, 602, 484, 423,
                541, 305, 17, 589, 471, 353, 554, 672, 436, 685, 803, 567, 934, 816, 698, 65, 196, 78, 327, 209, 91,
                458, 340, 222, 947, 829, 995, 912, 960
            ]
        );
        let (o7, b7) = order((0..300).map(|i| (i * 7919 + 13) % 100003));
        assert_eq!(b7, 541);
        assert_eq!(&o7[..12], &[67725, 59806, 51887, 43968, 36049, 28130, 20211, 12292, 4373, 96457, 88538, 80619]);
        assert_eq!(&o7[o7.len() - 4..], &[3460, 11379, 19298, 27217]);
        assert_eq!(&o7[150..156], &[46409, 74936, 38490, 67017, 95544, 30571]);
        assert_eq!(o7.len(), 300);
    }

    #[test]
    fn growth_matches_observed_bucket_counts() {
        // observed: 13@1 29@14 59@30 127@60 257@128 541@258 1109@542 2357@1110 5087@2358
        let mut m = CxxHashOrder::new();
        let mut seen = Vec::new();
        let mut last = 1;
        for i in 0..2400 {
            m.insert(i);
            if m.bucket_count() != last {
                last = m.bucket_count();
                seen.push((last, m.len()));
            }
        }
        assert_eq!(
            seen,
            vec![(13, 1), (29, 14), (59, 30), (127, 60), (257, 128), (541, 258), (1109, 542), (2357, 1110), (5087, 2358)]
        );
    }
}
