//! Upstream's hand-rolled JSON output.
//!
//! Upstream gStore writes graph-function results by concatenating pieces into
//! a `std::stringstream` — no JSON library, **no escaping** of node or
//! predicate strings, and numbers in C++ stream formatting. [`JsonOut`]
//! reproduces that byte for byte; [`path_json`] is `pathVec2JSON` and
//! [`vert_vec_json`] is `vertVec2JSON` (GE ~3404-3454).

use super::cxx::{fmt_f, fmt_g, CxxHashOrder};
use super::{EntityLiteralId, GFunCtx, PredId};

/// A path as upstream encodes it (`shortestPath.cpp` header comment):
/// `[v0, e0, v1, e1, ..., vk]` where the even positions are entity ids and the
/// odd positions encode the edge between their neighbours: `p >= 0` is an
/// edge `v(i-1) -p-> v(i+1)`, `-p-1` is the reverse edge `v(i+1) -p-> v(i-1)`
/// (traversed backwards in an undirected search). Upstream sometimes stores a
/// backward step with the *positive* code (`SSSP`, the return half of an
/// undirected `cycle`); reproduce whatever upstream stores.
pub type PathVec = Vec<i64>;

/// An id as upstream's `int` sees it: [`INVALID_ENTITY_LITERAL_ID`]
/// (upstream's `INVALID`, i.e. `-1` once stored in an `int`) becomes `-1`,
/// every other id is itself. Use it for [`PathVec`] entries and printed ids.
///
/// [`INVALID_ENTITY_LITERAL_ID`]: super::INVALID_ENTITY_LITERAL_ID
pub fn upstream_int(id: EntityLiteralId) -> i64 {
    if id == super::INVALID_ENTITY_LITERAL_ID {
        -1
    } else {
        i64::from(id)
    }
}

/// Encode an edge for a [`PathVec`]: `pred` when traversed along its
/// direction, `-pred - 1` when traversed backwards.
pub fn encode_edge(pred: PredId, forward: bool) -> i64 {
    if forward {
        i64::from(pred)
    } else {
        -i64::from(pred) - 1
    }
}

/// An output buffer with upstream's stream semantics.
#[derive(Debug, Default, Clone)]
pub struct JsonOut {
    buf: String,
}

impl JsonOut {
    /// An empty buffer.
    pub fn new() -> JsonOut {
        JsonOut::default()
    }

    /// Append text verbatim (`ss << "..."`).
    pub fn raw(&mut self, s: &str) -> &mut JsonOut {
        self.buf.push_str(s);
        self
    }

    /// Append an integer (`ss << int` / `size_t` / `long long`).
    pub fn int(&mut self, v: i64) -> &mut JsonOut {
        self.buf.push_str(&v.to_string());
        self
    }

    /// Append a double as `ss << double` does (`%g`, see [`fmt_g`]).
    pub fn g(&mut self, v: f64) -> &mut JsonOut {
        self.buf.push_str(&fmt_g(v));
        self
    }

    /// Append `std::to_string(double)` (`%f`, see [`fmt_f`]).
    pub fn f(&mut self, v: f64) -> &mut JsonOut {
        self.buf.push_str(&fmt_f(v));
        self
    }

    /// Append `getStringByID(id)` unquoted and unescaped.
    pub fn term(&mut self, ctx: &GFunCtx<'_>, id: EntityLiteralId) -> &mut JsonOut {
        self.buf.push_str(ctx.term(id));
        self
    }

    /// Append `"` + `getStringByID(id)` + `"` (no escaping inside).
    pub fn quoted_term(&mut self, ctx: &GFunCtx<'_>, id: EntityLiteralId) -> &mut JsonOut {
        self.buf.push('"');
        self.buf.push_str(ctx.term(id));
        self.buf.push('"');
        self
    }

    /// Append `getPredicateByID(p)` unquoted and unescaped.
    pub fn pred(&mut self, ctx: &GFunCtx<'_>, p: PredId) -> &mut JsonOut {
        self.buf.push_str(ctx.pred_iri(p));
        self
    }

    /// The text so far.
    pub fn as_str(&self) -> &str {
        &self.buf
    }

    /// Whether nothing was written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Finish, returning the text.
    pub fn into_string(self) -> String {
        self.buf
    }
}

/// Upstream's "comma before every item but the first" idiom
/// (`if (notFirstOutput) ss << ","; else notFirstOutput = 1;`).
#[derive(Debug, Clone)]
pub struct Sep {
    first: bool,
    text: &'static str,
}

impl Sep {
    /// A separator writing `,`.
    pub fn comma() -> Sep {
        Sep::with(",")
    }

    /// A separator writing `text` (e.g. `", "` for `bfsCount`'s depth list).
    pub fn with(text: &'static str) -> Sep {
        Sep { first: true, text }
    }

    /// Write the separator unless this is the first item.
    pub fn next(&mut self, out: &mut JsonOut) {
        if self.first {
            self.first = false;
        } else {
            out.raw(self.text);
        }
    }

    /// Whether no item has been written yet.
    pub fn is_first(&self) -> bool {
        self.first
    }
}

/// `GeneralEvaluation::pathVec2JSON(src, dst, path, ss)` (GE ~3404-3441):
///
/// ```text
/// {"src":"<S>","dst":"<D>","edges":[{"fromNode":a,"toNode":b,"predIRI":"<p>"},...],"nodes":[{"nodeIndex":id,"nodeIRI":"<iri>"},...]}
/// ```
///
/// * `src`/`dst` print `getStringByID`; `fromNode`/`toNode`/`nodeIndex` print
///   raw entity ids; an edge code `e >= 0` prints `from = v[i-1], to = v[i+1],
///   pred = e`, a code `e < 0` prints `from = v[i+1], to = v[i-1], pred = -e-1`;
/// * edges are separated by `,` (upstream: after every edge but the one at
///   index `len - 2`);
/// * `nodes` lists every distinct node of the path in the iteration order of
///   the `unordered_map<int, string>` upstream fills in path order
///   ([`CxxHashOrder`]) — for short paths with distinct ids mod 13 that is
///   reverse path order;
/// * an empty path prints `"edges":[],"nodes":[]`; a one-node path `[u]`
///   prints no edges and one node. (Upstream's optional `weight` argument is
///   never passed.)
pub fn path_json(out: &mut JsonOut, ctx: &GFunCtx<'_>, src: EntityLiteralId, dst: EntityLiteralId, path: &[i64]) {
    out.raw("{\"src\":\"").term(ctx, src).raw("\",\"dst\":\"").term(ctx, dst).raw("\",");
    out.raw("\"edges\":[");
    let len = path.len();
    let mut nodes = CxxHashOrder::new();
    for i in 0..len {
        if i % 2 == 0 {
            nodes.insert(path[i]);
        } else {
            let (from, to, pred) = if path[i] >= 0 {
                (path[i - 1], path.get(i + 1).copied().unwrap_or(-1), path[i])
            } else {
                (path.get(i + 1).copied().unwrap_or(-1), path[i - 1], -path[i] - 1)
            };
            out.raw("{\"fromNode\":")
                .int(from)
                .raw(",\"toNode\":")
                .int(to)
                .raw(",\"predIRI\":\"")
                .pred(ctx, pred as PredId)
                .raw("\"}");
            if i + 2 != len {
                out.raw(",");
            }
        }
    }
    out.raw("],\"nodes\":[");
    let mut sep = Sep::comma();
    for id in nodes.iter() {
        sep.next(out);
        out.raw("{\"nodeIndex\":")
            .int(id)
            .raw(",\"nodeIRI\":\"")
            .term(ctx, id as EntityLiteralId)
            .raw("\"}");
    }
    out.raw("]}");
}

/// `GeneralEvaluation::vertVec2JSON(v, ss)` (GE ~3443-3454): `["<a>","<b>"]`
/// — each node's `getStringByID` in double quotes, comma separated.
pub fn vert_vec_json(out: &mut JsonOut, ctx: &GFunCtx<'_>, ids: &[EntityLiteralId]) {
    out.raw("[");
    let mut sep = Sep::comma();
    for &id in ids {
        sep.next(out);
        out.quoted_term(ctx, id);
    }
    out.raw("]");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::csr::CsrGraph;
    use crate::dict::Dictionary;

    /// The documentation's cyclePath example (`SPARQL_QUERY.md` ~1012-1060):
    /// Eve(2) -like-> Carol(3) -like-> Bob(1) -like-> Eve(2).
    #[test]
    fn path_json_matches_the_documented_cycle() {
        let mut dict = Dictionary::new();
        for name in ["Alice", "Bob", "Eve", "Carol"] {
            dict.intern_entity(&format!("<{name}>"));
        }
        let like = dict.intern_predicate("<like>");
        let csr = CsrGraph::default();
        let ctx = GFunCtx::new(&csr, &dict);
        let p = i64::from(like);
        let mut out = JsonOut::new();
        path_json(&mut out, &ctx, 2, 3, &[2, p, 3, p, 1, p, 2]);
        assert_eq!(
            out.as_str(),
            "{\"src\":\"<Eve>\",\"dst\":\"<Carol>\",\"edges\":[\
             {\"fromNode\":2,\"toNode\":3,\"predIRI\":\"<like>\"},\
             {\"fromNode\":3,\"toNode\":1,\"predIRI\":\"<like>\"},\
             {\"fromNode\":1,\"toNode\":2,\"predIRI\":\"<like>\"}],\
             \"nodes\":[{\"nodeIndex\":1,\"nodeIRI\":\"<Bob>\"},\
             {\"nodeIndex\":3,\"nodeIRI\":\"<Carol>\"},\
             {\"nodeIndex\":2,\"nodeIRI\":\"<Eve>\"}]}"
        );
        // Reverse edge code and the empty / single-node paths.
        let mut out = JsonOut::new();
        path_json(&mut out, &ctx, 1, 3, &[1, encode_edge(like, false), 3]);
        assert!(out.as_str().contains("{\"fromNode\":3,\"toNode\":1,\"predIRI\":\"<like>\"}"));
        let mut out = JsonOut::new();
        path_json(&mut out, &ctx, 1, 1, &[]);
        assert_eq!(out.as_str(), "{\"src\":\"<Bob>\",\"dst\":\"<Bob>\",\"edges\":[],\"nodes\":[]}");
        let mut out = JsonOut::new();
        path_json(&mut out, &ctx, 1, 1, &[1]);
        assert_eq!(
            out.as_str(),
            "{\"src\":\"<Bob>\",\"dst\":\"<Bob>\",\"edges\":[],\"nodes\":[{\"nodeIndex\":1,\"nodeIRI\":\"<Bob>\"}]}"
        );
        let mut out = JsonOut::new();
        vert_vec_json(&mut out, &ctx, &[0, 1]);
        assert_eq!(out.as_str(), "[\"<Alice>\",\"<Bob>\"]");
    }
}
