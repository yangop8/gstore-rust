//! The bridge from the SPARQL AST to the gfun library.
//!
//! The parser turns `shortestPath(?x, <b>, true, {<p>})` into
//! [`Expr::GraphFn`]`{ func: `[`GFnName`]`, args: Vec<`[`GArg`]`> }`: one
//! [`GArg`] per grammar slot, in source order, with omitted optional slots
//! left out. The library evaluates a [`GFnKind`] over resolved [`GArgs`].
//! This module is the engine-independent half of that mapping:
//!
//! * `GFnKind::from(GFnName)` and `GFnName::from(GFnKind)` map the 32
//!   functions one to one (same names, same order).
//! * [`GCall::from_ast`] (or [`GCall::from_expr`]) checks `args` against
//!   [`GFnKind::signature`] and maps every argument onto the [`GArgs`] fields
//!   as the table in the [module docs](super) prescribes. Vertex arguments
//!   stay [`VarOrIri`]s and the `predSet` stays the written IRI lists, because
//!   resolving them needs the WHERE solutions and the dictionary.
//! * [`GCall::to_gargs`] takes the resolved vertex lists from the engine and
//!   produces the [`GArgs`] for [`super::eval`], resolving the `predSet` with
//!   [`PredFilter::resolve`] (or [`PredFilter::all`] for `confidenceDegree`,
//!   which has none).
//! * [`iri_node`] resolves a constant vertex IRI (the parser stores IRIs
//!   expanded and without angle brackets; dictionary keys carry them).
//!
//! What stays with the engine: collecting the distinct bindings of a
//! variable vertex argument over the WHERE solutions (first-appearance
//! order), resolving them with [`GNode::resolve`], and setting
//! [`GArgs::where_empty`] (see "Result row shape" in the module docs).

use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::id::EntityLiteralId;
use crate::parser::sparql::ast::{Expr, GArg, GFnName, GNum, VarOrIri};

use super::{ArgSlot, CsrGraph, GArgs, GFnKind, GNode, NamedValue, PredFilter};

impl From<GFnName> for GFnKind {
    fn from(f: GFnName) -> GFnKind {
        use GFnKind as K;
        use GFnName as N;
        match f {
            N::SimpleCyclePath => K::SimpleCyclePath,
            N::SimpleCycleBoolean => K::SimpleCycleBoolean,
            N::CyclePath => K::CyclePath,
            N::CycleBoolean => K::CycleBoolean,
            N::ShortestPath => K::ShortestPath,
            N::ShortestPathLen => K::ShortestPathLen,
            N::KHopReachable => K::KHopReachable,
            N::KHopEnumerate => K::KHopEnumerate,
            N::KHopReachablePath => K::KHopReachablePath,
            N::Ppr => K::Ppr,
            N::TriangleCounting => K::TriangleCounting,
            N::ClosenessCentrality => K::ClosenessCentrality,
            N::BfsCount => K::BfsCount,
            N::Pr => K::Pr,
            N::Sssp => K::Sssp,
            N::SsspLen => K::SsspLen,
            N::LabelProp => K::LabelProp,
            N::Wcc => K::Wcc,
            N::ClusterCoeff => K::ClusterCoeff,
            N::MaximumKplex => K::MaximumKplex,
            N::CoreTruss => K::CoreTruss,
            N::KHopCount => K::KHopCount,
            N::KHopNeighbor => K::KHopNeighbor,
            N::ShortestPathCount => K::ShortestPathCount,
            N::Louvain => K::Louvain,
            N::ConfidenceDegree => K::ConfidenceDegree,
            N::DiameterEstimation => K::DiameterEstimation,
            N::BetweennessCentrality => K::BetweennessCentrality,
            N::JaccardSimilarity => K::JaccardSimilarity,
            N::DegreeCorrelation => K::DegreeCorrelation,
            N::ShortestPaths => K::ShortestPaths,
            N::KHopAllNeighbors => K::KHopAllNeighbors,
        }
    }
}

impl From<GFnKind> for GFnName {
    fn from(k: GFnKind) -> GFnName {
        use GFnKind as K;
        use GFnName as N;
        match k {
            K::SimpleCyclePath => N::SimpleCyclePath,
            K::SimpleCycleBoolean => N::SimpleCycleBoolean,
            K::CyclePath => N::CyclePath,
            K::CycleBoolean => N::CycleBoolean,
            K::ShortestPath => N::ShortestPath,
            K::ShortestPathLen => N::ShortestPathLen,
            K::KHopReachable => N::KHopReachable,
            K::KHopEnumerate => N::KHopEnumerate,
            K::KHopReachablePath => N::KHopReachablePath,
            K::Ppr => N::Ppr,
            K::TriangleCounting => N::TriangleCounting,
            K::ClosenessCentrality => N::ClosenessCentrality,
            K::BfsCount => N::BfsCount,
            K::Pr => N::Pr,
            K::Sssp => N::Sssp,
            K::SsspLen => N::SsspLen,
            K::LabelProp => N::LabelProp,
            K::Wcc => N::Wcc,
            K::ClusterCoeff => N::ClusterCoeff,
            K::MaximumKplex => N::MaximumKplex,
            K::CoreTruss => N::CoreTruss,
            K::KHopCount => N::KHopCount,
            K::KHopNeighbor => N::KHopNeighbor,
            K::ShortestPathCount => N::ShortestPathCount,
            K::Louvain => N::Louvain,
            K::ConfidenceDegree => N::ConfidenceDegree,
            K::DiameterEstimation => N::DiameterEstimation,
            K::BetweennessCentrality => N::BetweennessCentrality,
            K::JaccardSimilarity => N::JaccardSimilarity,
            K::DegreeCorrelation => N::DegreeCorrelation,
            K::ShortestPaths => N::ShortestPaths,
            K::KHopAllNeighbors => N::KHopAllNeighbors,
        }
    }
}

/// A `predSet` argument as written: expanded predicate IRIs (no angle
/// brackets) in source order; `!<p>` negations go to `exclude`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PredSetArg {
    /// The listed (allowed) predicates.
    pub include: Vec<String>,
    /// The negated (excluded) predicates.
    pub exclude: Vec<String>,
}

impl PredSetArg {
    /// Resolve against the dictionary with upstream semantics
    /// ([`PredFilter::resolve`]: `{}` is every predicate, any negation means
    /// "all but these").
    pub fn resolve(&self, dict: &Dictionary) -> PredFilter {
        PredFilter::resolve(&self.include, &self.exclude, dict)
    }
}

/// One graph-function call with every argument mapped onto its [`GArgs`]
/// role, except that vertex arguments and the `predSet` are still
/// unresolved (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct GCall {
    /// The function.
    pub kind: GFnKind,
    /// The first vertex argument (`u`), feeding [`GArgs::srcs`]. `None` when
    /// the function has no vertex argument, or `clusterCoeff`'s optional
    /// leading one was omitted (whole-graph form).
    pub src: Option<VarOrIri>,
    /// The second vertex argument (`v`), feeding [`GArgs::dsts`].
    pub dst: Option<VarOrIri>,
    /// The `predSet`; `None` only for `confidenceDegree`, which has none and
    /// runs over every predicate.
    pub pred_set: Option<PredSetArg>,
    /// [`GArgs::directed`] (`false` when the function has no `directed` flag).
    pub directed: bool,
    /// [`GArgs::k`].
    pub k: Option<i64>,
    /// [`GArgs::ret_num`].
    pub ret_num: Option<i64>,
    /// [`GArgs::alpha`].
    pub alpha: Option<f64>,
    /// [`GArgs::max_iter`].
    pub max_iter: Option<f64>,
    /// [`GArgs::tol`].
    pub tol: Option<f64>,
    /// [`GArgs::increase`].
    pub increase: Option<f64>,
    /// [`GArgs::nums`].
    pub nums: Vec<f64>,
}

/// Where an integer argument goes, by its position among the function's
/// integer slots.
#[derive(Clone, Copy)]
enum IntRole {
    K,
    RetNum,
    Nums,
}

/// The [`GArgs`] field of each integer slot, in order (module-docs table):
/// `PR`'s only integer is its `retNum`, `coreTruss`'s two go to `nums`, and
/// everywhere else the first is `k` (`hopCnt` for PPR, maximumKplex's `k`)
/// and the second `retNum`.
fn int_roles(kind: GFnKind) -> &'static [IntRole] {
    match kind {
        GFnKind::Pr => &[IntRole::RetNum],
        GFnKind::CoreTruss => &[IntRole::Nums, IntRole::Nums],
        _ => &[IntRole::K, IntRole::RetNum],
    }
}

fn describe_slot(slot: ArgSlot) -> String {
    match slot {
        ArgSlot::Node | ArgSlot::OptNode => "a varOrIri".into(),
        ArgSlot::Bool => "a booleanLiteral".into(),
        ArgSlot::Int | ArgSlot::OptInt => "an integerLiteral".into(),
        ArgSlot::UInt | ArgSlot::OptUInt => "a num_integer".into(),
        ArgSlot::Num => "a numericLiteral".into(),
        ArgSlot::PredSet => "a predSet".into(),
        ArgSlot::Named(key, NamedValue::Num) => format!("{key}=numericLiteral"),
        ArgSlot::Named(key, NamedValue::UInt) => format!("{key}=num_integer"),
    }
}

fn describe_arg(arg: &GArg) -> String {
    match arg {
        GArg::Node(VarOrIri::Var(v)) => format!("?{v}"),
        GArg::Node(VarOrIri::Iri(i)) => format!("<{i}>"),
        GArg::Bool(b) => b.to_string(),
        GArg::Int(i) => i.to_string(),
        GArg::Num(x) => x.to_string(),
        GArg::PredSet { .. } => "a predSet".into(),
        GArg::Named { key, value } => match value {
            GNum::Int(i) => format!("{key}={i}"),
            GNum::Num(x) => format!("{key}={x}"),
        },
    }
}

impl GCall {
    /// Map a parsed call. Fails (with [`GStoreError::Query`]) when `args`
    /// does not fit the function's grammar alternative, which cannot happen
    /// for an AST the parser produced.
    pub fn from_ast(func: GFnName, args: &[GArg]) -> Result<GCall> {
        let kind = GFnKind::from(func);
        let err = |msg: String| GStoreError::Query(format!("{}: {msg}", kind.name()));
        let mut call = GCall {
            kind,
            src: None,
            dst: None,
            pred_set: None,
            directed: false,
            k: None,
            ret_num: None,
            alpha: None,
            max_iter: None,
            tol: None,
            increase: None,
            nums: Vec::new(),
        };
        let roles = int_roles(kind);
        let mut ints = 0usize;
        let mut nodes = 0usize;
        let mut rest = args.iter().enumerate().peekable();
        for &slot in kind.signature() {
            let optional = matches!(slot, ArgSlot::OptNode | ArgSlot::OptInt | ArgSlot::OptUInt);
            // An optional slot is present when the next argument has its type.
            let next = rest.peek().map(|&(_, a)| a);
            let present = match (slot, next) {
                (ArgSlot::OptNode, Some(GArg::Node(_))) => true,
                (ArgSlot::OptInt | ArgSlot::OptUInt, Some(GArg::Int(_))) => true,
                (ArgSlot::OptNode | ArgSlot::OptInt | ArgSlot::OptUInt, _) => false,
                _ => true,
            };
            if !present {
                continue;
            }
            let Some((i, arg)) = rest.next() else {
                return Err(err(format!(
                    "missing argument {}: expected {}",
                    args.len() + 1,
                    describe_slot(slot)
                )));
            };
            let mismatch = || {
                err(format!(
                    "argument {}: expected {}, found {}",
                    i + 1,
                    describe_slot(slot),
                    describe_arg(arg)
                ))
            };
            match (slot, arg) {
                (ArgSlot::Node | ArgSlot::OptNode, GArg::Node(n)) => {
                    match nodes {
                        0 => call.src = Some(n.clone()),
                        _ => call.dst = Some(n.clone()),
                    }
                    nodes += 1;
                }
                (ArgSlot::Bool, GArg::Bool(b)) => call.directed = *b,
                (
                    ArgSlot::Int | ArgSlot::OptInt | ArgSlot::UInt | ArgSlot::OptUInt,
                    GArg::Int(v),
                ) => {
                    let unsigned = matches!(slot, ArgSlot::UInt | ArgSlot::OptUInt);
                    if unsigned && *v < 0 {
                        return Err(mismatch());
                    }
                    match roles.get(ints) {
                        Some(IntRole::K) => call.k = Some(*v),
                        Some(IntRole::RetNum) => call.ret_num = Some(*v),
                        Some(IntRole::Nums) => call.nums.push(*v as f64),
                        // Every signature has at most `roles.len()` integers.
                        None => return Err(mismatch()),
                    }
                    ints += 1;
                }
                (ArgSlot::Num, GArg::Num(x)) => call.nums.push(*x),
                (ArgSlot::PredSet, GArg::PredSet { include, exclude }) => {
                    call.pred_set = Some(PredSetArg {
                        include: include.clone(),
                        exclude: exclude.clone(),
                    });
                }
                (ArgSlot::Named(want, kind_of), GArg::Named { key, value })
                    if key.eq_ignore_ascii_case(want) =>
                {
                    let x = match (kind_of, *value) {
                        (NamedValue::UInt, GNum::Int(v)) if v >= 0 => v as f64,
                        (NamedValue::Num, GNum::Int(v)) => v as f64,
                        (NamedValue::Num, GNum::Num(x)) => x,
                        _ => return Err(mismatch()),
                    };
                    let field = match want {
                        "alpha" => &mut call.alpha,
                        "maxIter" => &mut call.max_iter,
                        "tol" => &mut call.tol,
                        "increase" => &mut call.increase,
                        _ => return Err(mismatch()),
                    };
                    *field = Some(x);
                }
                _ => {
                    debug_assert!(!optional, "optional slots are matched by type");
                    return Err(mismatch());
                }
            }
        }
        if let Some((i, arg)) = rest.next() {
            return Err(err(format!(
                "argument {}: unexpected extra argument {}",
                i + 1,
                describe_arg(arg)
            )));
        }
        Ok(call)
    }

    /// [`GCall::from_ast`] of an [`Expr::GraphFn`]; `None` for any other
    /// expression.
    pub fn from_expr(e: &Expr) -> Option<Result<GCall>> {
        match e {
            Expr::GraphFn { func, args } => Some(GCall::from_ast(*func, args)),
            _ => None,
        }
    }

    /// The vertex arguments as written, `src` first.
    pub fn node_args(&self) -> impl Iterator<Item = &VarOrIri> {
        self.src.iter().chain(self.dst.iter())
    }

    /// The variables among the vertex arguments (names without the sigil),
    /// `src` first; a variable used twice is listed twice.
    pub fn variables(&self) -> impl Iterator<Item = &str> {
        self.node_args().filter_map(|n| match n {
            VarOrIri::Var(v) => Some(v.as_str()),
            VarOrIri::Iri(_) => None,
        })
    }

    /// The resolved predicate filter: the written `predSet` resolved against
    /// `dict`, or every predicate for `confidenceDegree`.
    pub fn preds(&self, dict: &Dictionary) -> PredFilter {
        match &self.pred_set {
            Some(p) => p.resolve(dict),
            None => PredFilter::all(dict),
        }
    }

    /// The [`GArgs`] for [`super::eval`], given the resolved vertex lists:
    /// `srcs` for [`GCall::src`] and `dsts` for [`GCall::dst`] (the distinct
    /// bindings of a variable in first-appearance order, or the one constant,
    /// see [`iri_node`]; empty when the function has no such argument).
    /// [`GArgs::where_empty`] is left `false` for the engine to set.
    pub fn to_gargs(&self, srcs: Vec<GNode>, dsts: Vec<GNode>, dict: &Dictionary) -> GArgs {
        GArgs {
            srcs,
            dsts,
            directed: self.directed,
            k: self.k,
            ret_num: self.ret_num,
            preds: self.preds(dict),
            alpha: self.alpha,
            max_iter: self.max_iter,
            tol: self.tol,
            increase: self.increase,
            nums: self.nums.clone(),
            where_empty: false,
        }
    }
}

/// The dictionary id of a vertex IRI as the parser stores it (expanded, no
/// angle brackets; an already bracketed key is used as is).
pub fn iri_entity(dict: &Dictionary, iri: &str) -> Option<EntityLiteralId> {
    if iri.starts_with('<') && iri.ends_with('>') {
        dict.entity_id(iri)
    } else {
        dict.entity_id(&format!("<{iri}>"))
    }
}

/// A constant vertex argument resolved to a [`GNode`]: [`GNode::MISSING`]
/// for an IRI the dictionary does not know (upstream id `-1`), an isolated
/// node for an entity without entity→entity edges.
pub fn iri_node(csr: &CsrGraph, dict: &Dictionary, iri: &str) -> GNode {
    GNode::resolve(csr, iri_entity(dict, iri))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::parser::sparql::ast::{GParam, GSlot, Projection, Query, SelectItem};
    use crate::parser::sparql::parse;

    /// The parser's grammar slot as the library's [`ArgSlot`].
    fn slot_of(p: &GParam) -> ArgSlot {
        match (p.slot, p.optional) {
            (GSlot::Node, false) => ArgSlot::Node,
            (GSlot::Node, true) => ArgSlot::OptNode,
            (GSlot::Bool, false) => ArgSlot::Bool,
            (GSlot::Int, false) => ArgSlot::Int,
            (GSlot::Int, true) => ArgSlot::OptInt,
            (GSlot::UInt, false) => ArgSlot::UInt,
            (GSlot::UInt, true) => ArgSlot::OptUInt,
            (GSlot::Num, false) => ArgSlot::Num,
            (GSlot::PredSet, false) => ArgSlot::PredSet,
            (GSlot::NamedNum(k), false) => ArgSlot::Named(k, NamedValue::Num),
            (GSlot::NamedUInt(k), false) => ArgSlot::Named(k, NamedValue::UInt),
            other => panic!("no ArgSlot for optional {other:?}"),
        }
    }

    #[test]
    fn names_map_one_to_one_in_the_same_order() {
        assert_eq!(GFnName::ALL.len(), GFnKind::ALL.len());
        for (n, k) in GFnName::ALL.iter().zip(GFnKind::ALL) {
            assert_eq!(GFnKind::from(*n), k);
            assert_eq!(GFnName::from(k), *n);
            assert_eq!(n.name(), k.name());
            assert_eq!(GFnKind::from_name(n.keyword()), Some(k));
        }
    }

    #[test]
    fn parser_slots_match_library_signatures() {
        for n in GFnName::ALL {
            let parser: Vec<ArgSlot> = n.params().iter().map(slot_of).collect();
            assert_eq!(parser, GFnKind::from(n).signature(), "{n}");
        }
    }

    fn first_call(q: &str) -> GCall {
        let Query::Select(s) = parse(q).unwrap() else {
            panic!("not a SELECT: {q}")
        };
        let Projection::Items(items) = &s.projection else {
            panic!("no projection items: {q}")
        };
        let e = items
            .iter()
            .find_map(|it| match it {
                SelectItem::Expr(e, _) => Some(e),
                SelectItem::Var(_) => None,
            })
            .expect("an expression projection");
        GCall::from_expr(e).expect("a graph function").unwrap()
    }

    fn sel(call: &str) -> String {
        format!("PREFIX ex: <http://ex/> SELECT ({call} AS ?r) WHERE {{ ?x ?p ?y . }}")
    }

    fn iri(s: &str) -> Option<VarOrIri> {
        Some(VarOrIri::Iri(s.into()))
    }

    fn var(s: &str) -> Option<VarOrIri> {
        Some(VarOrIri::Var(s.into()))
    }

    fn preds(include: &[&str], exclude: &[&str]) -> Option<PredSetArg> {
        Some(PredSetArg {
            include: include.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
        })
    }

    #[test]
    fn maps_path_functions() {
        let c = first_call(&sel("shortestPath(?x, ex:b, true, {ex:p, !<http://ex/q>})"));
        assert_eq!(c.kind, GFnKind::ShortestPath);
        assert_eq!(c.src, var("x"));
        assert_eq!(c.dst, iri("http://ex/b"));
        assert!(c.directed);
        assert_eq!(c.pred_set, preds(&["http://ex/p"], &["http://ex/q"]));
        assert_eq!((c.k, c.ret_num), (None, None));
        assert_eq!(c.variables().collect::<Vec<_>>(), ["x"]);

        let c = first_call(&sel("kHopEnumerate(ex:a, ?y, false, -1, {}, 5)"));
        assert_eq!((c.src, c.dst), (iri("http://ex/a"), var("y")));
        assert!(!c.directed);
        assert_eq!((c.k, c.ret_num), (Some(-1), Some(5)));
        assert_eq!(c.pred_set, preds(&[], &[]));

        let c = first_call(&sel("kHopEnumerate(ex:a, ?y, false, 3, {})"));
        assert_eq!((c.k, c.ret_num), (Some(3), None));

        let c = first_call(&sel("kHopNeighbor(?x, true, 2, {ex:p}, 10)"));
        assert_eq!((c.src, c.dst), (var("x"), None));
        assert_eq!((c.k, c.ret_num), (Some(2), Some(10)));

        let c = first_call(&sel("kHopCount(?x, true, 2, {ex:p})"));
        assert_eq!((c.k, c.ret_num), (Some(2), None));

        let c = first_call(&sel("SSSPLen(?x, false, {})"));
        assert_eq!((c.src, c.dst, c.k), (var("x"), None, None));
    }

    #[test]
    fn maps_metric_functions() {
        let c = first_call(&sel("PPR(ex:a, 3, {ex:p}, 20)"));
        assert_eq!(
            (c.src, c.k, c.ret_num),
            (iri("http://ex/a"), Some(3), Some(20))
        );
        assert!(!c.directed);

        let c = first_call(&sel("PR(true, {}, 7, alpha=0.85, maxIter=100, tol=0.001)"));
        assert_eq!((c.k, c.ret_num), (None, Some(7)));
        assert_eq!(
            (c.alpha, c.max_iter, c.tol),
            (Some(0.85), Some(100.0), Some(0.001))
        );
        assert!(c.directed);

        let c = first_call(&sel("louvain(false, {}, MAXITER=5, increase=1)"));
        assert_eq!((c.max_iter, c.increase), (Some(5.0), Some(1.0)));

        let c = first_call(&sel("labelProp(true, {ex:p}, maxIter=4)"));
        assert_eq!(c.max_iter, Some(4.0));

        let c = first_call(&sel("clusterCoeff(?x, true, {})"));
        assert_eq!(c.src, var("x"));
        let c = first_call(&sel("clusterCoeff(false, {})"));
        assert_eq!(c.src, None);
        assert!(!c.directed);

        let c = first_call(&sel("maximumKplex({ex:p}, 2)"));
        assert_eq!(c.k, Some(2));
        let c = first_call(&sel("maximumKplex({ex:p})"));
        assert_eq!(c.k, None);

        let c = first_call(&sel("coreTruss({}, 3, 4)"));
        assert_eq!((c.k, c.nums.as_slice()), (None, &[3.0, 4.0][..]));

        let c = first_call(&sel("confidenceDegree(1, 2.5, 3, 4, 5, 6)"));
        assert_eq!(c.nums, [1.0, 2.5, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(c.pred_set, None);

        let c = first_call(&sel("JaccardSimilarity(?x, {}, 2, 5)"));
        assert_eq!((c.k, c.ret_num), (Some(2), Some(5)));

        let c = first_call(&sel("degreeCorrelation(?x, 2, {})"));
        assert_eq!((c.k, c.ret_num), (Some(2), None));

        let c = first_call(&sel("WCC({})"));
        assert_eq!((c.src, c.directed), (None, false));
    }

    #[test]
    fn every_function_maps_from_a_parsed_call() {
        // One parsed call per function, built from its own grammar slots.
        for n in GFnName::ALL {
            let mut args = Vec::new();
            let mut node = 0;
            for p in n.params() {
                args.push(match p.slot {
                    GSlot::Node => {
                        node += 1;
                        format!("?v{node}")
                    }
                    GSlot::Bool => "true".into(),
                    GSlot::Int | GSlot::UInt => "2".into(),
                    GSlot::Num => "0.5".into(),
                    GSlot::PredSet => "{ex:p}".into(),
                    GSlot::NamedNum(k) => format!("{k}=0.5"),
                    GSlot::NamedUInt(k) => format!("{k}=3"),
                });
            }
            let c = first_call(&sel(&format!("{}({})", n.name(), args.join(", "))));
            assert_eq!(c.kind, GFnKind::from(n));
            assert_eq!(c.variables().count(), node, "{n}");
        }
    }

    #[test]
    fn mismatched_arguments_are_errors() {
        let e = GCall::from_ast(GFnName::Wcc, &[GArg::Bool(true)]).unwrap_err();
        assert_eq!(
            e.to_string(),
            GStoreError::Query("WCC: argument 1: expected a predSet, found true".into())
                .to_string()
        );
        let set = GArg::PredSet {
            include: vec![],
            exclude: vec![],
        };
        let e = GCall::from_ast(GFnName::Wcc, &[set.clone(), GArg::Int(1)]).unwrap_err();
        assert!(e.to_string().contains("unexpected extra argument 1"), "{e}");
        let e = GCall::from_ast(GFnName::TriangleCounting, &[GArg::Bool(true)]).unwrap_err();
        assert!(
            e.to_string()
                .contains("missing argument 2: expected a predSet"),
            "{e}"
        );
        let e = GCall::from_ast(GFnName::MaximumKplex, &[set.clone(), GArg::Int(-1)]).unwrap_err();
        assert!(
            e.to_string().contains("expected a num_integer, found -1"),
            "{e}"
        );
        let named = |key: &str, value| GArg::Named {
            key: key.into(),
            value,
        };
        let e = GCall::from_ast(
            GFnName::LabelProp,
            &[GArg::Bool(true), set.clone(), named("tol", GNum::Int(3))],
        )
        .unwrap_err();
        assert!(
            e.to_string()
                .contains("expected maxIter=num_integer, found tol=3"),
            "{e}"
        );
        let e = GCall::from_ast(
            GFnName::LabelProp,
            &[GArg::Bool(true), set, named("maxIter", GNum::Num(1.5))],
        )
        .unwrap_err();
        assert!(e.to_string().contains("found maxIter=1.5"), "{e}");
        let not_gfn = Expr::Var("x".into());
        assert!(GCall::from_expr(&not_gfn).is_none());
    }

    #[test]
    fn to_gargs_resolves_predicates_and_constant_nodes() {
        let db = Database::build_from_str(
            "from_ast",
            "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
             <http://ex/b> <http://ex/q> <http://ex/c> .\n\
             <http://ex/c> <http://ex/r> \"lit\" .\n\
             <http://ex/d> <http://ex/r> \"lit2\" .\n",
        )
        .unwrap();
        let dict = db.dict();
        let csr = db.csr().unwrap();
        let pid = |p: &str| dict.predicate_id(&format!("<http://ex/{p}>")).unwrap();

        let c = first_call(&sel("shortestPath(ex:a, ex:c, true, {ex:q, ex:p, ex:zzz})"));
        let src = iri_node(&csr, dict, "http://ex/a");
        assert_eq!(src.entity, dict.entity_id("<http://ex/a>").unwrap());
        assert!(src.node.is_some());
        let dst = iri_node(&csr, dict, "<http://ex/c>");
        let g = c.to_gargs(vec![src], vec![dst], dict);
        assert_eq!(g.preds.preds(), [pid("q"), pid("p")]);
        assert_eq!((g.srcs, g.dsts), (vec![src], vec![dst]));
        assert!(g.directed && !g.where_empty);

        // Isolated (only literal objects) and unknown vertices.
        let d = iri_node(&csr, dict, "http://ex/d");
        assert!(!d.is_missing() && d.node.is_none());
        assert!(iri_node(&csr, dict, "http://ex/nope").is_missing());

        let all = PredFilter::all(dict);
        let c = first_call(&sel("WCC({})"));
        assert_eq!(c.to_gargs(vec![], vec![], dict).preds, all);
        let c = first_call(&sel("WCC({!ex:r})"));
        assert_eq!(c.preds(dict).preds(), [pid("p"), pid("q")]);
        let c = first_call(&sel("confidenceDegree(1, 1, 1, 1, 1, 1)"));
        let g = c.to_gargs(vec![], vec![], dict);
        assert_eq!(g.preds, all);
        assert_eq!(g.nums, [1.0; 6]);

        let c = first_call(&sel("PR(false, {ex:p}, 3, alpha=0.5, maxIter=10, tol=0.1)"));
        let g = c.to_gargs(vec![], vec![], dict);
        assert_eq!(
            (g.ret_num, g.alpha, g.max_iter, g.tol),
            (Some(3), Some(0.5), Some(10.0), Some(0.1))
        );
    }
}
