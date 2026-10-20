//! Upstream-exact text building blocks for graph-function output.
//!
//! gStore builds every graph-function result by streaming into a
//! `std::stringstream` (GeneralEvaluation.cpp, e.g. GE:1576-2033). Reproducing
//! its bytes needs C++'s number formatting and the exact JSON shapes of its
//! two shared builders (`pathVec2JSON`, `vertVec2JSON`). This module provides:
//!
//! | upstream expression                    | here                              |
//! |----------------------------------------|-----------------------------------|
//! | `ss << (double)x`                      | [`cxx_g`]                         |
//! | `ss << (float)x`                       | [`cxx_g_f32`]                     |
//! | `std::to_string((double)x)`            | [`cxx_to_string`]                 |
//! | `nlohmann::json(x).dump()` (a double)  | [`nlohmann_f64`]                  |
//! | `stof(text)` widened to `double`       | [`f32_round`]                     |
//! | `pathVec2JSON(src, dst, path, ss)`     | [`push_path_json`] with [`GPath`] |
//! | `vertVec2JSON(v, ss)`                  | [`push_vert_list_json`]           |
//! | `"{\"paths\":[" … "]}"` wrapper        | [`JsonList`] / [`paths_json`]     |
//!
//! **Strings.** Upstream splices dictionary strings (`<http://…>`, `_:b0`)
//! into the JSON *unescaped*. [`push_json_escaped`] escapes like
//! `nlohmann::json::dump` (`"` `\` and control characters only), which is a
//! no-op for every valid IRI and blank-node label, so output stays
//! byte-identical in practice while remaining valid JSON for odd inputs.
//!
//! **Integers** print as plain decimals (`ss << int`); entity / vertex ids
//! print as C `int` (an unknown id `INVALID` prints `-1`), see
//! [`cxx_int_id`].

use super::GFunCtx;
use crate::model::id::{EntityLiteralId, PredId};

// ---------------------------------------------------------------------------
// numbers
// ---------------------------------------------------------------------------

/// `std::ostream << double` with default stream state — C's `%g` with
/// precision 6: 6 significant digits, trailing zeros (and a trailing `.`)
/// removed, scientific notation (`1e+06`, `1.5e-07`, two-digit minimum
/// exponent) when the decimal exponent is `< -4` or `>= 6`.
///
/// Non-finite values print as glibc does: `inf`, `-inf`, `nan`, and `-nan`
/// for a NaN with the sign bit set. Note that on x86-64 a runtime `0.0/0.0`
/// yields a *negative* NaN, so upstream prints `-nan` for e.g. the closeness
/// centrality of an isolated vertex (`1.0*(cnt-1)/sum` with `cnt=1, sum=0`);
/// implementers reproducing such a result should pass `-f64::NAN`.
pub fn cxx_g(x: f64) -> String {
    if let Some(s) = non_finite(x) {
        return s;
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    const P: i32 = 6;
    let sci = format!("{:.*e}", (P - 1) as usize, x);
    let (mant, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if !(-4..P).contains(&exp) {
        let mant = strip_fraction_zeros(mant);
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mant}e{sign}{:02}", exp.abs())
    } else {
        let fixed = format!("{:.*}", (P - 1 - exp) as usize, x);
        strip_fraction_zeros(&fixed).to_string()
    }
}

/// `std::ostream << float` (the float is widened to double, then `%g`).
pub fn cxx_g_f32(x: f32) -> String {
    cxx_g(f64::from(x))
}

/// `std::to_string(double)` — C's `%f`: fixed, exactly 6 decimals
/// (`0.500000`, `-nan`, `inf`).
pub fn cxx_to_string(x: f64) -> String {
    if let Some(s) = non_finite(x) {
        return s;
    }
    format!("{x:.6}")
}

/// `nlohmann::json(x).dump()` for a double: the shortest round-trip digits,
/// formatted with nlohmann's rules — integral values keep a `.0`
/// (`1.0`, `100000.0`), decimal exponents in `(-4, 15]` print positionally
/// (`0.0001`, `123.456789`), others in scientific form with a signed,
/// at-least-two-digit exponent (`1e-05`, `1.5e+20`). Non-finite values dump
/// as `null`.
///
/// (nlohmann uses Grisu2, which in rare cases emits a longer-than-shortest
/// digit string; Rust's formatter is always shortest. The two agree on every
/// value the graph functions produce in practice.)
pub fn nlohmann_f64(x: f64) -> String {
    if !x.is_finite() {
        return "null".to_string();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    let neg = x < 0.0;
    let sci = format!("{:e}", x.abs()); // shortest round-trip, e.g. "1.2345e-7"
    let (mant, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp + 1; // value = 0.d1d2…dk × 10^n
    const MIN_EXP: i32 = -4;
    const MAX_EXP: i32 = 15;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if k <= n && n <= MAX_EXP {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
        out.push_str(".0");
    } else if 0 < n && n <= MAX_EXP {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if MIN_EXP < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = n - 1;
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", e.abs()));
    }
    out
}

/// Upstream parses `alpha=`, `tol=`, `increase=` and `maxIter=` with C
/// `stof` (single precision) before storing them in a `double`
/// (QueryParser.cpp:555-566). Apply this to reproduce that rounding:
/// `f32_round(0.85) == 0.8500000238418579`.
pub fn f32_round(x: f64) -> f64 {
    f64::from(x as f32)
}

/// An entity / vertex id printed as upstream's C `int` (`nodeIndex`,
/// `fromNode`, louvain member ids): ids `>= 2^31` (only the `INVALID`
/// sentinel in practice) wrap negative, so a missing node prints `-1`.
pub fn cxx_int_id(id: EntityLiteralId) -> i32 {
    id as i32
}

fn non_finite(x: f64) -> Option<String> {
    if x.is_nan() {
        Some(if x.is_sign_negative() { "-nan" } else { "nan" }.to_string())
    } else if x.is_infinite() {
        Some(if x < 0.0 { "-inf" } else { "inf" }.to_string())
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
// JSON text
// ---------------------------------------------------------------------------

/// Append `s` escaped as the *contents* of a JSON string, the way
/// `nlohmann::json::dump` escapes: `\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`,
/// other control characters as `\u00xx` (lower-case hex); everything else
/// (including non-ASCII UTF-8 and `/`) verbatim.
pub fn push_json_escaped(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

/// Append `"` + escaped `s` + `"`.
pub fn push_json_quoted(out: &mut String, s: &str) {
    out.push('"');
    push_json_escaped(out, s);
    out.push('"');
}

/// A comma-separated list of JSON fragments — upstream's `notFirstOutput` /
/// `localFirstOutput` pattern. [`item`](Self::item) inserts the separator
/// and hands out the buffer to write the next element into.
#[derive(Debug, Default, Clone)]
pub struct JsonList {
    buf: String,
    any: bool,
}

impl JsonList {
    /// An empty list.
    pub fn new() -> JsonList {
        JsonList::default()
    }

    /// Start the next element (writes `,` unless it is the first).
    pub fn item(&mut self) -> &mut String {
        if self.any {
            self.buf.push(',');
        }
        self.any = true;
        &mut self.buf
    }

    /// Whether no element was started.
    pub fn is_empty(&self) -> bool {
        !self.any
    }

    /// The joined elements (without brackets).
    pub fn as_str(&self) -> &str {
        &self.buf
    }

    /// `{"paths":[` + elements + `]}` — the envelope every graph function
    /// (except `cycleBoolean` and `confidenceDegree`) returns (GE:1578 /
    /// GE:2019; the surrounding `"` upstream adds are the literal's quotes,
    /// not part of the value).
    pub fn into_paths(self) -> String {
        paths_json(&self.buf)
    }
}

/// `{"paths":[<items>]}` around already-joined items.
pub fn paths_json(items: &str) -> String {
    let mut s = String::with_capacity(items.len() + 12);
    s.push_str("{\"paths\":[");
    s.push_str(items);
    s.push_str("]}");
    s
}

// ---------------------------------------------------------------------------
// paths
// ---------------------------------------------------------------------------

/// One edge of a [`GPath`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PathEdge {
    /// The predicate labelling the edge.
    pub pred: PredId,
    /// `false`: the edge points from the earlier path node to the later one
    /// (upstream encodes it as `pred >= 0`); `true`: it points backwards
    /// (upstream `-pred - 1`), i.e. it was traversed against its direction.
    pub reversed: bool,
}

/// A path as upstream's `vector<int>` `[v0, e0, v1, e1, …, vk]` (see
/// `shortestPath.cpp:12-15`), with node items as **entity ids** (so the
/// trivial path of an isolated or unknown vertex can still be printed) and
/// edge items as [`PathEdge`]s. Invariant: `edges.len() + 1 == nodes.len()`,
/// or both empty for the empty path (`uid == vid` special cases).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct GPath {
    /// The path's vertices as entity ids, in path order (may repeat in cycles).
    pub nodes: Vec<EntityLiteralId>,
    /// `edges[i]` joins `nodes[i]` and `nodes[i + 1]`.
    pub edges: Vec<PathEdge>,
}

impl GPath {
    /// The empty path (upstream `vector<int> path; // Empty path`).
    pub fn empty() -> GPath {
        GPath::default()
    }

    /// A single-vertex path `[v]` (e.g. SSSP's entry for its source).
    pub fn single(entity: EntityLiteralId) -> GPath {
        GPath {
            nodes: vec![entity],
            edges: Vec::new(),
        }
    }

    /// Decode upstream's flat encoding `[v0, e0, v1, …]`: even positions are
    /// vertex ids (entity ids), odd positions are `pred` (forward) or
    /// `-pred - 1` (reversed). A trailing edge item without a node is ignored.
    pub fn from_upstream(v: &[i64]) -> GPath {
        let mut p = GPath::default();
        let mut i = 0;
        while i < v.len() {
            p.nodes.push(v[i] as EntityLiteralId);
            if i + 2 < v.len() {
                let e = v[i + 1];
                p.edges.push(if e >= 0 {
                    PathEdge {
                        pred: e as PredId,
                        reversed: false,
                    }
                } else {
                    PathEdge {
                        pred: (-e - 1) as PredId,
                        reversed: true,
                    }
                });
            } else if i + 1 < v.len() {
                break;
            }
            i += 2;
        }
        p
    }

    /// Number of edges (upstream `(path.size() - 1) / 2`).
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    /// Whether this is the empty path.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// `GeneralEvaluation::pathVec2JSON` (GE:3403-3440):
///
/// ```text
/// {"src":"<S>","dst":"<D>","edges":[{"fromNode":a,"toNode":b,"predIRI":"<p>"},…],"nodes":[{"nodeIndex":i,"nodeIRI":"<I>"},…]}
/// ```
///
/// * `src` / `dst` are printed as given (the function's endpoints, not
///   necessarily the path's first/last node).
/// * `fromNode`/`toNode`/`nodeIndex` are **entity ids** printed as C `int`.
///   A forward edge prints `fromNode = nodes[i], toNode = nodes[i+1]`; a
///   reversed one swaps them.
/// * `nodes` lists each distinct path vertex once, in the iteration order of
///   upstream's `unordered_map<int, string> nodeIRI` filled in path order —
///   reproduced with [`CxxUnorderedOrder`](super::cxx::CxxUnorderedOrder)
///   (e.g. path `4→3→1→0` lists `0,1,3,4`; path `0→1→2→3` lists `3,2,0,1`).
/// * The unused `weight` parameter of the upstream builder is not supported
///   (no caller passes it; upstream would also forget a comma before it).
pub fn push_path_json(
    out: &mut String,
    ctx: &GFunCtx<'_>,
    src: EntityLiteralId,
    dst: EntityLiteralId,
    path: &GPath,
) {
    out.push_str("{\"src\":");
    push_json_quoted(out, ctx.entity_str(src));
    out.push_str(",\"dst\":");
    push_json_quoted(out, ctx.entity_str(dst));
    out.push_str(",\"edges\":[");
    for (i, e) in path.edges.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let (a, b) = (path.nodes[i], path.nodes.get(i + 1).copied().unwrap_or(path.nodes[i]));
        let (from, to) = if e.reversed { (b, a) } else { (a, b) };
        out.push_str(&format!(
            "{{\"fromNode\":{},\"toNode\":{},\"predIRI\":",
            cxx_int_id(from),
            cxx_int_id(to)
        ));
        push_json_quoted(out, ctx.pred_str(e.pred));
        out.push('}');
    }
    out.push_str("],\"nodes\":[");
    let mut order = super::cxx::CxxUnorderedOrder::new();
    for &n in &path.nodes {
        order.insert(i64::from(cxx_int_id(n)));
    }
    for (i, key) in order.keys().into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let entity = key as i32 as EntityLiteralId;
        out.push_str(&format!("{{\"nodeIndex\":{key},\"nodeIRI\":"));
        push_json_quoted(out, ctx.entity_str(entity));
        out.push('}');
    }
    out.push_str("]}");
}

/// `GeneralEvaluation::vertVec2JSON` (GE:3442-3454): `["<a>","<b>",…]` in the
/// given order.
pub fn push_vert_list_json(out: &mut String, ctx: &GFunCtx<'_>, entities: &[EntityLiteralId]) {
    out.push('[');
    for (i, &e) in entities.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_json_quoted(out, ctx.entity_str(e));
    }
    out.push(']');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goldens produced by g++ 13 / glibc (`ss << d`, `to_string(d)`,
    /// `nlohmann::json(d).dump()`), see the module docs.
    const DS: [f64; 28] = [
        0.0,
        -0.0,
        1.0,
        0.5,
        0.1,
        1.0 / 3.0,
        2.0 / 3.0,
        0.555555555,
        6.5,
        100000.0,
        999999.0,
        999999.5,
        1000000.0,
        1234567.0,
        0.0001,
        0.00012345678,
        0.00001,
        1.5e-7,
        123.456789,
        1e300,
        -2.5,
        0.0429273,
        1e21,
        12345.678,
        0.1 + 0.2,
        4.0 / 7.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ];

    #[test]
    fn g6_matches_glibc() {
        let want = "0|-0|1|0.5|0.1|0.333333|0.666667|0.555556|6.5|100000|999999|1e+06|1e+06|\
                    1.23457e+06|0.0001|0.000123457|1e-05|1.5e-07|123.457|1e+300|-2.5|0.0429273|\
                    1e+21|12345.7|0.3|0.571429|inf|-inf";
        let got: Vec<String> = DS.iter().map(|&d| cxx_g(d)).collect();
        assert_eq!(got.join("|"), want);
        assert_eq!(cxx_g(-f64::NAN), "-nan");
        assert_eq!(cxx_g(f64::NAN), "nan");
        assert_eq!(cxx_g(9.999995), "10");
    }

    #[test]
    fn float_g6_matches_glibc() {
        let fs = [1.0f32 / 3.0, 0.5, 2.0 / 3.0, 0.85, 0.333333, 0.1];
        let got: Vec<String> = fs.iter().map(|&f| cxx_g_f32(f)).collect();
        assert_eq!(got.join("|"), "0.333333|0.5|0.666667|0.85|0.333333|0.1");
    }

    #[test]
    fn to_string_matches_glibc() {
        let want = [
            "0.000000",
            "-0.000000",
            "1.000000",
            "0.500000",
            "0.100000",
            "0.333333",
            "0.666667",
            "0.555556",
            "6.500000",
            "100000.000000",
            "999999.000000",
            "999999.500000",
            "1000000.000000",
            "1234567.000000",
            "0.000100",
            "0.000123",
            "0.000010",
            "0.000000",
            "123.456789",
        ];
        for (d, w) in DS.iter().zip(want) {
            assert_eq!(cxx_to_string(*d), w, "{d}");
        }
        assert!(cxx_to_string(1e300).starts_with("1000000000000000052504760255204420248704468581108"));
        assert!(cxx_to_string(1e300).ends_with("540160.000000"));
        assert_eq!(cxx_to_string(0.0429273), "0.042927");
        assert_eq!(cxx_to_string(-f64::NAN), "-nan");
        assert_eq!(cxx_to_string(f64::INFINITY), "inf");
    }

    #[test]
    fn nlohmann_matches_dump() {
        let want = "0.0|-0.0|1.0|0.5|0.1|0.3333333333333333|0.6666666666666666|0.555555555|6.5|\
                    100000.0|999999.0|999999.5|1000000.0|1234567.0|0.0001|0.00012345678|1e-05|\
                    1.5e-07|123.456789|1e+300|-2.5|0.0429273|1e+21|12345.678|0.30000000000000004|\
                    0.5714285714285714";
        let got: Vec<String> = DS[..26].iter().map(|&d| nlohmann_f64(d)).collect();
        assert_eq!(got.join("|"), want);
        assert_eq!(nlohmann_f64(f64::NAN), "null");
        assert_eq!(nlohmann_f64(1.5e20), "1.5e+20");
        assert_eq!(nlohmann_f64(123456789012345.0), "123456789012345.0");
        assert_eq!(nlohmann_f64(1234567890123456.0), "1.234567890123456e+15");
    }

    #[test]
    fn stof_rounding() {
        assert_eq!(f32_round(0.85), 0.8500000238418579);
        assert_eq!(f32_round(0.0001), 9.999999747378752e-5);
        assert_eq!(f32_round(0.3), 0.30000001192092896);
    }

    #[test]
    fn escaping_and_lists() {
        let mut s = String::new();
        push_json_escaped(&mut s, "a\"b\\c\n\u{1}é/");
        assert_eq!(s, "a\\\"b\\\\c\\n\\u0001é/");
        let mut l = JsonList::new();
        assert!(l.is_empty());
        l.item().push('1');
        l.item().push('2');
        assert_eq!(l.as_str(), "1,2");
        assert_eq!(l.into_paths(), "{\"paths\":[1,2]}");
        assert_eq!(paths_json(""), "{\"paths\":[]}");
        assert_eq!(cxx_int_id(crate::model::id::INVALID_ENTITY_LITERAL_ID), -1);
    }

    #[test]
    fn gpath_from_upstream_encoding() {
        let p = GPath::from_upstream(&[4, 1, 3, -3, 1]);
        assert_eq!(p.nodes, vec![4, 3, 1]);
        assert_eq!(
            p.edges,
            vec![
                PathEdge {
                    pred: 1,
                    reversed: false
                },
                PathEdge {
                    pred: 2,
                    reversed: true
                }
            ]
        );
        assert_eq!(p.len(), 2);
        assert!(GPath::empty().is_empty());
        assert_eq!(GPath::from_upstream(&[7]), GPath::single(7));
    }
}
