//! gStore 1.4 advanced graph functions ("gfun"): types, argument mapping,
//! dispatch, and the shared output contract.
//!
//! The functions (`shortestPath`, `kHopReachable`, `PR`, `WCC`, …) are
//! evaluated by upstream in `GeneralEvaluation::getFinalResult`
//! (`src/Query/GeneralEvaluation.cpp`, "GE" below) over the per-predicate
//! [`CsrGraph`] with the algorithms of `src/Query/Algorithm/*.cpp`. This module
//! is the engine-independent library: the query engine resolves arguments
//! into [`GArgs`], builds a [`GFunCtx`] and calls [`eval`]; every function
//! returns the **complete upstream output** as a [`GFunValue`].
//!
//! Layout: [`paths`] (path / reachability / BFS family), [`metrics`]
//! (centrality, community, similarity, …), [`fmt`] (upstream-exact number and
//! JSON formatting, `pathVec2JSON`), [`cxx`] (libstdc++/glibc behaviour that
//! leaks into output order), [`traverse`] (BFS building blocks).
//!
//! # Output contract (all functions)
//!
//! * **Envelope.** Every function except `cycleBoolean` and `confidenceDegree`
//!   yields one string `{"paths":[e1,e2,…]}` ([`GFunValue::Json`]); upstream
//!   stores it as the plain literal `"{"paths":[…]}"` (GE:1578 `ss << "\"{\"paths\":["`,
//!   GE:2019 `ss << "]}\""`), so a SPARQL JSON result shows
//!   `{"type":"literal","value":"{\"paths\":[…]}"}` (ResultSet.cpp:461-475).
//!   `cycleBoolean` yields [`GFunValue::Bool`] (`"true"^^xsd:boolean`,
//!   GE:2021-2027); `confidenceDegree` yields [`GFunValue::ResultsJson`]
//!   (the `pfn_type` marker, GE:1538-1552).
//! * **Exact text.** Field names, field order, spacing (upstream writes
//!   `", \"count\":"` with a space in some builders and not in others — copy
//!   each builder literally), number formatting ([`fmt::cxx_g`] for
//!   `ss << double`, [`fmt::cxx_to_string`] for `to_string(double)`, plain
//!   decimals for integers) and element order are part of the contract.
//! * **IRIs** print as their dictionary key, verbatim: `<http://x/a>`, blank
//!   nodes `_:b0` (upstream `kvstore->getStringByID`), inside JSON strings
//!   unless the spec says otherwise (`JaccardSimilarity`, `maximumKplex` and
//!   `coreTruss` print them **unquoted**, as upstream does). A missing
//!   (unknown) id prints as the empty string. Predicates print as their key
//!   (`<like>`). Escaping: see [`fmt::push_json_escaped`].
//! * **Vertex ids** in output (`nodeIndex`, `fromNode`, `toNode`, louvain
//!   members) are **entity ids** (not CSR node indices), printed as C `int`.
//! * **Separators — the one deliberate deviation.** Some upstream builders
//!   append consecutive entries without a `,` (e.g. the variable-argument
//!   branch of `kHopReachable`, GE:2759-2769; `betweennessCentrality` /
//!   `JaccardSimilarity` / `degreeCorrelation` / `kHopAllNeighbors` wrappers,
//!   GE:4099-4177), which only shows when more than one source binding is
//!   evaluated and yields invalid JSON. Ports emit `,` between consecutive
//!   entries there. Everything else (including the unquoted IRIs above,
//!   which the upstream docs show) is reproduced as is.
//!
//! # Argument model ([`GArgs`])
//!
//! Positional arguments follow the grammar alternative of each function
//! (`SPARQL.g4:197-229`, summarised by [`GFnKind::signature`]) and are mapped
//! to [`GArgs`] fields as upstream's `QueryParser` maps them into `PathArgs`
//! (QueryParser.cpp:470-580):
//!
//! | function (signature)                                        | srcs | dsts | directed | k | ret_num | named / nums |
//! |-------------------------------------------------------------|------|------|----------|---|---------|--------------|
//! | `simpleCyclePath/simpleCycleBoolean/cyclePath/cycleBoolean/shortestPath/shortestPathLen/shortestPathCount/shortestPaths(u, v, directed, preds)` | u | v | ✓ | – | – | – |
//! | `kHopReachable/kHopReachablePath(u, v, directed, k, preds)` | u | v | ✓ | k | – | – |
//! | `kHopEnumerate(u, v, directed, k, preds [, retNum])`        | u | v | ✓ | k | retNum (`None` → upstream `-1`) | – |
//! | `kHopCount(u, directed, k, preds)`                          | u | – | ✓ | k | – | – |
//! | `kHopNeighbor/kHopAllNeighbors(u, directed, k, preds [, retNum])` | u | – | ✓ | k | retNum (`None` → `-1`) | – |
//! | `bfsCount/SSSP/SSSPLen/closenessCentrality/betweennessCentrality(u, directed, preds)` | u | – | ✓ | – | – | – |
//! | `PPR(u, hopCnt, preds, retNum)`                             | u | – | – | hopCnt | retNum | – |
//! | `JaccardSimilarity(u, preds, k, retNum)`                    | u | – | – | k | retNum | – |
//! | `degreeCorrelation(u, k, preds)`                            | u | – | – | k | – | – |
//! | `clusterCoeff([u,] directed, preds)`                        | u or empty | – | ✓ | – | – | – |
//! | `triangleCounting(directed, preds)`                         | – | – | ✓ | – | – | – |
//! | `PR(directed, preds, retNum, alpha=a, maxIter=m, tol=t)`    | – | – | ✓ | – | retNum | `alpha`, `max_iter`, `tol` |
//! | `labelProp(directed, preds, maxIter=m)`                     | – | – | ✓ | – | – | `max_iter` |
//! | `louvain(directed, preds, maxIter=m, increase=x)`           | – | – | ✓ | – | – | `max_iter`, `increase` |
//! | `WCC(preds)`, `diameterEstimation(preds)`                   | – | – | – | – | – | – |
//! | `maximumKplex(preds [, k])`                                 | – | – | – | k (`None` → 1) | – | – |
//! | `coreTruss(preds, a, b)`                                    | – | – | – | – | – | `nums = [a, b]` |
//! | `confidenceDegree(w1, …, w6)` (no predSet: all predicates)  | – | – | – | – | – | `nums = [w1..w6]` (ignored upstream) |
//!
//! `preds` is always resolved (see [`PredFilter::resolve`]); functions
//! without a `predSet` (only `confidenceDegree`) get [`PredFilter::all`].
//! Named values are stored as written (f64); functions apply upstream's
//! single-precision `stof` rounding themselves ([`fmt::f32_round`]).
//!
//! # Node arguments and the binding cross product
//!
//! A `varOrIri` argument contributes the **distinct** bindings of the
//! variable over the WHERE solutions, in first-appearance order (or the one
//! constant). Upstream loops `for uid in srcs { for vid in dsts { … } }` and
//! skips repeated `(uid, vid)` pairs (GE:2628, GE:2666-2672); [`GArgs::pairs`] and
//! [`GArgs::distinct_srcs`] reproduce that order. Functions with only one
//! node argument loop over `srcs`; whole-graph functions (`PR`, `WCC`,
//! `triangleCounting`, …) run once regardless. An empty `srcs` (a variable
//! with no bindings) yields `{"paths":[]}` for per-source functions.
//!
//! Each node is a [`GNode`]: its entity id plus its CSR node, if any.
//!
//! * **Isolated** (`node == None`, known entity): an entity without
//!   entity→entity edges (e.g. only literal objects). Upstream still uses its
//!   raw id, so it behaves as a vertex with no edges and prints its IRI
//!   (`bfsCount` → depth 0 count 1, `SSSP` → the path to itself, …).
//! * **Missing** ([`GNode::MISSING`]): an IRI the dictionary does not know;
//!   upstream's id is `INVALID`, i.e. `-1` as `int`. Each function documents
//!   what it does with it; the general rule is: functions guarded by
//!   `if (uid < 0) return` emit nothing for it, the others treat it as an
//!   isolated vertex whose name prints as `""`. Where upstream would index an
//!   array with `-1` (undefined behaviour) the entry is skipped.
//! * A variable bound to a **literal** has no CSR node; pass its dictionary id
//!   as an isolated node (upstream prints the literal's string the same way).
//!
//! [`GArgs::where_empty`] tells whether the WHERE clause produced no
//! solutions. Upstream then evaluates through a separate code path
//! (GE:1508-2033) whose only output difference is `kHopNeighbor`'s key
//! (`"dst"` there, `"neighbor"` otherwise). In that path a *variable* argument
//! is looked up as if it were an IRI and becomes a missing node — the engine
//! should pass `srcs = [GNode::MISSING]` to reproduce it, or an empty list to
//! emit `{"paths":[]}` (the engine package decides; document the choice).
//!
//! # FILTER-position forms
//!
//! Upstream also *parses* `SIMPLECYCLEBOOLEAN`, `CYCLEBOOLEAN`,
//! `SHORTESTPATHLEN` and `KHOPREACHABLE` inside FILTER (QueryParser.cpp:866-930)
//! but never evaluates them (no branch in `TempResult::doComp`); a scalar
//! there would be a boolean ([`GFunValue::Bool`]) or an integer
//! ([`GFunValue::Int`]). `SIMILARITY(a, b)` in FILTER is the Jaro string
//! similarity, [`metrics::jaro_similarity`] ([`GFunValue::Num`]).

pub mod cxx;
pub mod fmt;
pub mod metrics;
pub mod paths;
pub mod traverse;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

pub use crate::analytics::csr::{CsrGraph, Direction, EdgeDir, Node, PredFilter};
use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::id::{EntityLiteralId, PredId, INVALID_ENTITY_LITERAL_ID};

// ---------------------------------------------------------------------------
// function kinds
// ---------------------------------------------------------------------------

/// Every advanced graph function of gStore 1.4 (upstream
/// `ProjectionVar::AggregateType`, QueryTree.h:414-423, minus the non-graph
/// aggregates; plus `confidenceDegree`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GFnKind {
    /// `simpleCyclePath(u, v, directed, preds)`
    SimpleCyclePath,
    /// `simpleCycleBoolean(u, v, directed, preds)`
    SimpleCycleBoolean,
    /// `cyclePath(u, v, directed, preds)`
    CyclePath,
    /// `cycleBoolean(u, v, directed, preds)`
    CycleBoolean,
    /// `shortestPath(u, v, directed, preds)`
    ShortestPath,
    /// `shortestPathLen(u, v, directed, preds)`
    ShortestPathLen,
    /// `kHopReachable(u, v, directed, k, preds)`
    KHopReachable,
    /// `kHopEnumerate(u, v, directed, k, preds [, retNum])`
    KHopEnumerate,
    /// `kHopReachablePath(u, v, directed, k, preds)`
    KHopReachablePath,
    /// `PPR(u, hopCnt, preds, retNum)`
    Ppr,
    /// `triangleCounting(directed, preds)`
    TriangleCounting,
    /// `closenessCentrality(u, directed, preds)`
    ClosenessCentrality,
    /// `bfsCount(u, directed, preds)`
    BfsCount,
    /// `PR(directed, preds, retNum, alpha=…, maxIter=…, tol=…)`
    Pr,
    /// `SSSP(u, directed, preds)`
    Sssp,
    /// `SSSPLen(u, directed, preds)`
    SsspLen,
    /// `labelProp(directed, preds, maxIter=…)`
    LabelProp,
    /// `WCC(preds)`
    Wcc,
    /// `clusterCoeff([u,] directed, preds)`
    ClusterCoeff,
    /// `maximumKplex(preds [, k])`
    MaximumKplex,
    /// `coreTruss(preds, alpha, beta)`
    CoreTruss,
    /// `kHopCount(u, directed, k, preds)`
    KHopCount,
    /// `kHopNeighbor(u, directed, k, preds [, retNum])`
    KHopNeighbor,
    /// `shortestPathCount(u, v, directed, preds)`
    ShortestPathCount,
    /// `louvain(directed, preds, maxIter=…, increase=…)`
    Louvain,
    /// `confidenceDegree(w1, …, w6)`
    ConfidenceDegree,
    /// `diameterEstimation(preds)`
    DiameterEstimation,
    /// `betweennessCentrality(u, directed, preds)`
    BetweennessCentrality,
    /// `JaccardSimilarity(u, preds, k, retNum)`
    JaccardSimilarity,
    /// `degreeCorrelation(u, k, preds)`
    DegreeCorrelation,
    /// `shortestPaths(u, v, directed, preds)`
    ShortestPaths,
    /// `kHopAllNeighbors(u, directed, k, preds [, retNum])`
    KHopAllNeighbors,
}

/// The kind of value a function returns in SELECT position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GFnOutput {
    /// `{"paths":[…]}` text ([`GFunValue::Json`]).
    PathsJson,
    /// `"true"/"false"^^xsd:boolean` ([`GFunValue::Bool`]).
    Boolean,
    /// Upstream `pfn_type` JSON ([`GFunValue::ResultsJson`]).
    ResultsJson,
}

/// Which library file implements a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GFnFamily {
    /// [`paths`]
    Paths,
    /// [`metrics`]
    Metrics,
}

/// The value kind of a `name=value` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamedValue {
    /// `numericLiteral` (any signed integer / decimal / double).
    Num,
    /// `num_integer` (an unsigned integer token).
    UInt,
}

/// One positional slot of a function's grammar alternative (SPARQL.g4:197-229).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArgSlot {
    /// `varOrIri` — a vertex.
    Node,
    /// `(varOrIri ',')?` — an optional leading vertex (`clusterCoeff`).
    OptNode,
    /// `booleanLiteral` — the `directed` flag (`true`/`false`).
    Bool,
    /// `integerLiteral` — a possibly signed integer.
    Int,
    /// `(',' integerLiteral)?` — an optional trailing integer.
    OptInt,
    /// `num_integer` — an unsigned integer.
    UInt,
    /// `(',' num_integer)?` — an optional trailing unsigned integer.
    OptUInt,
    /// `numericLiteral` — any number.
    Num,
    /// `predSet` — `{}` / `{<p>, !<q>, …}`.
    PredSet,
    /// `K '=' value`; the keyword (`alpha`, `maxIter`, `tol`, `increase`) is
    /// matched case-insensitively like every upstream keyword.
    Named(&'static str, NamedValue),
}

use ArgSlot as S;

const SIG_UV: &[ArgSlot] = &[S::Node, S::Node, S::Bool, S::PredSet];
const SIG_UV_K: &[ArgSlot] = &[S::Node, S::Node, S::Bool, S::Int, S::PredSet];
const SIG_UV_K_RET: &[ArgSlot] = &[S::Node, S::Node, S::Bool, S::Int, S::PredSet, S::OptInt];
const SIG_U: &[ArgSlot] = &[S::Node, S::Bool, S::PredSet];
const SIG_U_K: &[ArgSlot] = &[S::Node, S::Bool, S::Int, S::PredSet];
const SIG_U_K_RET: &[ArgSlot] = &[S::Node, S::Bool, S::Int, S::PredSet, S::OptInt];

impl GFnKind {
    /// Every function, in upstream `AggregateType` order.
    pub const ALL: [GFnKind; 32] = [
        GFnKind::SimpleCyclePath,
        GFnKind::SimpleCycleBoolean,
        GFnKind::CyclePath,
        GFnKind::CycleBoolean,
        GFnKind::ShortestPath,
        GFnKind::ShortestPathLen,
        GFnKind::KHopReachable,
        GFnKind::KHopEnumerate,
        GFnKind::KHopReachablePath,
        GFnKind::Ppr,
        GFnKind::TriangleCounting,
        GFnKind::ClosenessCentrality,
        GFnKind::BfsCount,
        GFnKind::Pr,
        GFnKind::Sssp,
        GFnKind::SsspLen,
        GFnKind::LabelProp,
        GFnKind::Wcc,
        GFnKind::ClusterCoeff,
        GFnKind::MaximumKplex,
        GFnKind::CoreTruss,
        GFnKind::KHopCount,
        GFnKind::KHopNeighbor,
        GFnKind::ShortestPathCount,
        GFnKind::Louvain,
        GFnKind::ConfidenceDegree,
        GFnKind::DiameterEstimation,
        GFnKind::BetweennessCentrality,
        GFnKind::JaccardSimilarity,
        GFnKind::DegreeCorrelation,
        GFnKind::ShortestPaths,
        GFnKind::KHopAllNeighbors,
    ];

    /// The upstream spelling (as in the docs; keywords are case-insensitive).
    pub fn name(self) -> &'static str {
        match self {
            GFnKind::SimpleCyclePath => "simpleCyclePath",
            GFnKind::SimpleCycleBoolean => "simpleCycleBoolean",
            GFnKind::CyclePath => "cyclePath",
            GFnKind::CycleBoolean => "cycleBoolean",
            GFnKind::ShortestPath => "shortestPath",
            GFnKind::ShortestPathLen => "shortestPathLen",
            GFnKind::KHopReachable => "kHopReachable",
            GFnKind::KHopEnumerate => "kHopEnumerate",
            GFnKind::KHopReachablePath => "kHopReachablePath",
            GFnKind::Ppr => "PPR",
            GFnKind::TriangleCounting => "triangleCounting",
            GFnKind::ClosenessCentrality => "closenessCentrality",
            GFnKind::BfsCount => "bfsCount",
            GFnKind::Pr => "PR",
            GFnKind::Sssp => "SSSP",
            GFnKind::SsspLen => "SSSPLen",
            GFnKind::LabelProp => "labelProp",
            GFnKind::Wcc => "WCC",
            GFnKind::ClusterCoeff => "clusterCoeff",
            GFnKind::MaximumKplex => "maximumKplex",
            GFnKind::CoreTruss => "coreTruss",
            GFnKind::KHopCount => "kHopCount",
            GFnKind::KHopNeighbor => "kHopNeighbor",
            GFnKind::ShortestPathCount => "shortestPathCount",
            GFnKind::Louvain => "louvain",
            GFnKind::ConfidenceDegree => "confidenceDegree",
            GFnKind::DiameterEstimation => "diameterEstimation",
            GFnKind::BetweennessCentrality => "betweennessCentrality",
            GFnKind::JaccardSimilarity => "JaccardSimilarity",
            GFnKind::DegreeCorrelation => "degreeCorrelation",
            GFnKind::ShortestPaths => "shortestPaths",
            GFnKind::KHopAllNeighbors => "kHopAllNeighbors",
        }
    }

    /// Look a function up by name, case-insensitively (upstream keywords are
    /// built from case-insensitive lexer fragments, SPARQL.g4 `K_*`).
    pub fn from_name(name: &str) -> Option<GFnKind> {
        GFnKind::ALL
            .into_iter()
            .find(|k| k.name().eq_ignore_ascii_case(name))
    }

    /// What the function returns in SELECT position.
    pub fn output(self) -> GFnOutput {
        match self {
            GFnKind::CycleBoolean => GFnOutput::Boolean,
            GFnKind::ConfidenceDegree => GFnOutput::ResultsJson,
            _ => GFnOutput::PathsJson,
        }
    }

    /// Which library file implements it.
    pub fn family(self) -> GFnFamily {
        match self {
            GFnKind::SimpleCyclePath
            | GFnKind::SimpleCycleBoolean
            | GFnKind::CyclePath
            | GFnKind::CycleBoolean
            | GFnKind::ShortestPath
            | GFnKind::ShortestPathLen
            | GFnKind::ShortestPathCount
            | GFnKind::ShortestPaths
            | GFnKind::KHopReachable
            | GFnKind::KHopReachablePath
            | GFnKind::KHopEnumerate
            | GFnKind::KHopCount
            | GFnKind::KHopNeighbor
            | GFnKind::KHopAllNeighbors
            | GFnKind::BfsCount
            | GFnKind::Sssp
            | GFnKind::SsspLen => GFnFamily::Paths,
            _ => GFnFamily::Metrics,
        }
    }

    /// The positional shape of the function's grammar alternative
    /// (SPARQL.g4:197-229). Optional slots are always trailing, except
    /// `clusterCoeff`'s leading [`ArgSlot::OptNode`].
    pub fn signature(self) -> &'static [ArgSlot] {
        match self {
            GFnKind::SimpleCyclePath
            | GFnKind::SimpleCycleBoolean
            | GFnKind::CyclePath
            | GFnKind::CycleBoolean
            | GFnKind::ShortestPath
            | GFnKind::ShortestPathLen
            | GFnKind::ShortestPathCount
            | GFnKind::ShortestPaths => SIG_UV,
            GFnKind::KHopReachable | GFnKind::KHopReachablePath => SIG_UV_K,
            GFnKind::KHopEnumerate => SIG_UV_K_RET,
            GFnKind::Ppr => &[S::Node, S::Int, S::PredSet, S::Int],
            GFnKind::TriangleCounting => &[S::Bool, S::PredSet],
            GFnKind::ClosenessCentrality
            | GFnKind::BfsCount
            | GFnKind::Sssp
            | GFnKind::SsspLen
            | GFnKind::BetweennessCentrality => SIG_U,
            GFnKind::Pr => &[
                S::Bool,
                S::PredSet,
                S::Int,
                S::Named("alpha", NamedValue::Num),
                S::Named("maxIter", NamedValue::UInt),
                S::Named("tol", NamedValue::Num),
            ],
            GFnKind::LabelProp => &[S::Bool, S::PredSet, S::Named("maxIter", NamedValue::UInt)],
            GFnKind::Wcc | GFnKind::DiameterEstimation => &[S::PredSet],
            GFnKind::ClusterCoeff => &[S::OptNode, S::Bool, S::PredSet],
            GFnKind::MaximumKplex => &[S::PredSet, S::OptUInt],
            GFnKind::CoreTruss => &[S::PredSet, S::UInt, S::UInt],
            GFnKind::KHopCount => SIG_U_K,
            GFnKind::KHopNeighbor | GFnKind::KHopAllNeighbors => SIG_U_K_RET,
            GFnKind::Louvain => &[
                S::Bool,
                S::PredSet,
                S::Named("maxIter", NamedValue::UInt),
                S::Named("increase", NamedValue::Num),
            ],
            GFnKind::ConfidenceDegree => &[S::Num, S::Num, S::Num, S::Num, S::Num, S::Num],
            GFnKind::JaccardSimilarity => &[S::Node, S::PredSet, S::Int, S::Int],
            GFnKind::DegreeCorrelation => &[S::Node, S::Int, S::PredSet],
        }
    }
}

impl std::fmt::Display for GFnKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------
// arguments
// ---------------------------------------------------------------------------

/// A resolved vertex argument: the entity id plus its CSR node (see the
/// module docs for isolated / missing nodes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GNode {
    /// The dictionary id ([`INVALID_ENTITY_LITERAL_ID`] for a missing node).
    pub entity: EntityLiteralId,
    /// The CSR node, when the entity has entity→entity edges.
    pub node: Option<Node>,
}

impl GNode {
    /// An IRI unknown to the dictionary (upstream id `INVALID` = `-1`).
    pub const MISSING: GNode = GNode {
        entity: INVALID_ENTITY_LITERAL_ID,
        node: None,
    };

    /// The node for a dictionary id (`None` → [`GNode::MISSING`]).
    pub fn resolve(csr: &CsrGraph, entity: Option<EntityLiteralId>) -> GNode {
        match entity {
            Some(e) if e != INVALID_ENTITY_LITERAL_ID => GNode {
                entity: e,
                node: csr.node_of(e),
            },
            _ => GNode::MISSING,
        }
    }

    /// The node of a CSR vertex.
    pub fn from_node(csr: &CsrGraph, node: Node) -> GNode {
        GNode {
            entity: csr.entity_of(node),
            node: Some(node),
        }
    }

    /// Whether this is [`GNode::MISSING`].
    pub fn is_missing(&self) -> bool {
        self.entity == INVALID_ENTITY_LITERAL_ID
    }

    /// Upstream's `int uid` for this node (`-1` when missing).
    pub fn upstream_id(&self) -> i32 {
        fmt::cxx_int_id(self.entity)
    }
}

/// The resolved arguments of one graph-function call (see the module docs
/// for the per-function mapping).
#[derive(Debug, Clone, Default)]
pub struct GArgs {
    /// Distinct bindings of the first vertex argument, first-appearance
    /// order (empty for functions without one).
    pub srcs: Vec<GNode>,
    /// Distinct bindings of the second vertex argument.
    pub dsts: Vec<GNode>,
    /// The `directed` boolean (`false` when the function has none).
    pub directed: bool,
    /// The first integer argument (`k` / `hopCnt`; `maximumKplex`'s k).
    pub k: Option<i64>,
    /// The second integer argument (`retNum`; `PR`'s third argument).
    pub ret_num: Option<i64>,
    /// The resolved `predSet`.
    pub preds: PredFilter,
    /// `alpha=` (PR).
    pub alpha: Option<f64>,
    /// `maxIter=` (PR, labelProp, louvain), as written.
    pub max_iter: Option<f64>,
    /// `tol=` (PR).
    pub tol: Option<f64>,
    /// `increase=` (louvain).
    pub increase: Option<f64>,
    /// Remaining numbers in order (`coreTruss`'s two integers,
    /// `confidenceDegree`'s six weights).
    pub nums: Vec<f64>,
    /// Whether the WHERE clause produced no solutions (see module docs).
    pub where_empty: bool,
}

impl GArgs {
    /// `srcs` without repeated entities, in order.
    pub fn distinct_srcs(&self) -> Vec<GNode> {
        let mut seen = HashSet::new();
        self.srcs
            .iter()
            .copied()
            .filter(|n| seen.insert(n.entity))
            .collect()
    }

    /// The `(src, dst)` cross product in upstream loop order (outer `srcs`,
    /// inner `dsts`), skipping repeated entity pairs (GE:2666-2672).
    pub fn pairs(&self) -> Vec<(GNode, GNode)> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for &u in &self.srcs {
            for &v in &self.dsts {
                if seen.insert((u.entity, v.entity)) {
                    out.push((u, v));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// values and context
// ---------------------------------------------------------------------------

/// A graph function's result (= upstream's output literal type).
#[derive(Debug, Clone, PartialEq)]
pub enum GFunValue {
    /// A plain literal whose lexical form is this JSON text
    /// (`{"paths":[…]}`).
    Json(String),
    /// `"true"/"false"^^<http://www.w3.org/2001/XMLSchema#boolean>`.
    Bool(bool),
    /// An integer (FILTER-position scalar forms).
    Int(i64),
    /// A number (e.g. `SIMILARITY`).
    Num(f64),
    /// Upstream's `pfn_type` result (`confidenceDegree`): `ResultSet::to_JSON`
    /// replaces the *whole* response with `{"results": <this JSON>}`
    /// (ResultSet.cpp:540-583) and the console prints it raw; an engine that
    /// cannot do that should return it as a plain literal.
    ResultsJson(String),
}

/// Everything a graph function may read: the CSR, the dictionary (id ↔
/// string) and a cooperative cancellation flag.
#[derive(Debug, Clone, Copy)]
pub struct GFunCtx<'a> {
    /// The graph.
    pub csr: &'a CsrGraph,
    /// The dictionary the CSR's ids belong to.
    pub dict: &'a Dictionary,
    cancel: Option<&'a AtomicBool>,
}

impl<'a> GFunCtx<'a> {
    /// A context without cancellation.
    pub fn new(csr: &'a CsrGraph, dict: &'a Dictionary) -> GFunCtx<'a> {
        GFunCtx {
            csr,
            dict,
            cancel: None,
        }
    }

    /// Attach a cancellation flag (the engine's `Arc<AtomicBool>`).
    pub fn with_cancel(mut self, flag: &'a AtomicBool) -> GFunCtx<'a> {
        self.cancel = Some(flag);
        self
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::Relaxed))
    }

    /// `Err(GStoreError::Cancelled)` once cancellation was requested. Long
    /// loops should call it periodically (e.g. once per source / outer
    /// iteration).
    pub fn check_cancel(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(GStoreError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// An entity's printed form (`<iri>`, `_:b`, or a literal's key); `""`
    /// for an unknown id, like upstream `getStringByID`.
    pub fn entity_str(&self, entity: EntityLiteralId) -> &'a str {
        self.dict.id_to_string(entity).unwrap_or("")
    }

    /// A CSR node's printed form.
    pub fn node_str(&self, node: Node) -> &'a str {
        self.entity_str(self.csr.entity_of(node))
    }

    /// A [`GNode`]'s printed form (`""` when missing).
    pub fn gnode_str(&self, n: &GNode) -> &'a str {
        self.entity_str(n.entity)
    }

    /// A predicate's printed form (`<p>`), `""` if unknown.
    pub fn pred_str(&self, pred: PredId) -> &'a str {
        self.dict.predicate_to_string(pred).unwrap_or("")
    }
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

/// Evaluate `kind` over `ctx` with `args`, producing the full upstream output.
pub fn eval(kind: GFnKind, ctx: &GFunCtx<'_>, args: &GArgs) -> Result<GFunValue> {
    ctx.check_cancel()?;
    match kind {
        GFnKind::SimpleCyclePath => paths::simple_cycle_path(ctx, args),
        GFnKind::SimpleCycleBoolean => paths::simple_cycle_boolean(ctx, args),
        GFnKind::CyclePath => paths::cycle_path(ctx, args),
        GFnKind::CycleBoolean => paths::cycle_boolean(ctx, args),
        GFnKind::ShortestPath => paths::shortest_path(ctx, args),
        GFnKind::ShortestPathLen => paths::shortest_path_len(ctx, args),
        GFnKind::ShortestPathCount => paths::shortest_path_count(ctx, args),
        GFnKind::ShortestPaths => paths::shortest_paths(ctx, args),
        GFnKind::KHopReachable => paths::k_hop_reachable(ctx, args),
        GFnKind::KHopReachablePath => paths::k_hop_reachable_path(ctx, args),
        GFnKind::KHopEnumerate => paths::k_hop_enumerate(ctx, args),
        GFnKind::KHopCount => paths::k_hop_count(ctx, args),
        GFnKind::KHopNeighbor => paths::k_hop_neighbor(ctx, args),
        GFnKind::KHopAllNeighbors => paths::k_hop_all_neighbors(ctx, args),
        GFnKind::BfsCount => paths::bfs_count(ctx, args),
        GFnKind::Sssp => paths::sssp(ctx, args),
        GFnKind::SsspLen => paths::sssp_len(ctx, args),
        GFnKind::Ppr => metrics::ppr(ctx, args),
        GFnKind::Pr => metrics::pr(ctx, args),
        GFnKind::TriangleCounting => metrics::triangle_counting(ctx, args),
        GFnKind::ClosenessCentrality => metrics::closeness_centrality(ctx, args),
        GFnKind::BetweennessCentrality => metrics::betweenness_centrality(ctx, args),
        GFnKind::ClusterCoeff => metrics::cluster_coeff(ctx, args),
        GFnKind::LabelProp => metrics::label_prop(ctx, args),
        GFnKind::Wcc => metrics::wcc(ctx, args),
        GFnKind::Louvain => metrics::louvain(ctx, args),
        GFnKind::JaccardSimilarity => metrics::jaccard_similarity(ctx, args),
        GFnKind::DegreeCorrelation => metrics::degree_correlation(ctx, args),
        GFnKind::DiameterEstimation => metrics::diameter_estimation(ctx, args),
        GFnKind::ConfidenceDegree => metrics::confidence_degree(ctx, args),
        GFnKind::MaximumKplex => metrics::maximum_kplex(ctx, args),
        GFnKind::CoreTruss => metrics::core_truss(ctx, args),
    }
}

/// Message prefix of [`not_implemented`] errors.
const NOT_IMPLEMENTED: &str = "graph function not implemented yet: ";

/// The error a not-yet-ported function returns.
pub fn not_implemented(kind: GFnKind) -> GStoreError {
    GStoreError::Query(format!("{NOT_IMPLEMENTED}{}", kind.name()))
}

/// Whether `e` is a [`not_implemented`] error.
pub fn is_not_implemented(e: &GStoreError) -> bool {
    matches!(e, GStoreError::Query(m) if m.starts_with(NOT_IMPLEMENTED))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_case_insensitively() {
        for k in GFnKind::ALL {
            assert_eq!(GFnKind::from_name(k.name()), Some(k));
            assert_eq!(GFnKind::from_name(&k.name().to_ascii_uppercase()), Some(k));
            assert_eq!(k.to_string(), k.name());
        }
        assert_eq!(GFnKind::from_name("ppr"), Some(GFnKind::Ppr));
        assert_eq!(GFnKind::from_name("sssplen"), Some(GFnKind::SsspLen));
        assert_eq!(GFnKind::from_name("similarity"), None);
        let distinct: HashSet<_> = GFnKind::ALL.iter().map(|k| k.name()).collect();
        assert_eq!(distinct.len(), 32);
    }

    #[test]
    fn signatures_follow_the_grammar() {
        assert_eq!(GFnKind::ShortestPath.signature().len(), 4);
        assert_eq!(
            GFnKind::KHopEnumerate.signature().last(),
            Some(&ArgSlot::OptInt)
        );
        assert_eq!(GFnKind::ClusterCoeff.signature()[0], ArgSlot::OptNode);
        assert_eq!(GFnKind::ConfidenceDegree.signature().len(), 6);
        assert!(GFnKind::Pr
            .signature()
            .contains(&ArgSlot::Named("maxIter", NamedValue::UInt)));
        for k in GFnKind::ALL {
            let sig = k.signature();
            let has_preds = sig.contains(&ArgSlot::PredSet);
            assert_eq!(has_preds, k != GFnKind::ConfidenceDegree, "{k}");
        }
        assert_eq!(GFnKind::CycleBoolean.output(), GFnOutput::Boolean);
        assert_eq!(GFnKind::ConfidenceDegree.output(), GFnOutput::ResultsJson);
        assert_eq!(GFnKind::Wcc.output(), GFnOutput::PathsJson);
        let paths = GFnKind::ALL
            .iter()
            .filter(|k| k.family() == GFnFamily::Paths)
            .count();
        assert_eq!(paths, 17);
    }

    #[test]
    fn pairs_dedup_in_loop_order() {
        let n = |e| GNode {
            entity: e,
            node: None,
        };
        let a = GArgs {
            srcs: vec![n(1), n(2), n(1)],
            dsts: vec![n(5), n(6)],
            ..GArgs::default()
        };
        let p: Vec<(u32, u32)> = a
            .pairs()
            .iter()
            .map(|(u, v)| (u.entity, v.entity))
            .collect();
        assert_eq!(p, vec![(1, 5), (1, 6), (2, 5), (2, 6)]);
        let s: Vec<u32> = a.distinct_srcs().iter().map(|u| u.entity).collect();
        assert_eq!(s, vec![1, 2]);
        assert_eq!(GNode::MISSING.upstream_id(), -1);
        assert!(GNode::MISSING.is_missing());
    }

    #[test]
    fn cancellation() {
        let g = CsrGraph::empty();
        let d = Dictionary::new();
        let flag = AtomicBool::new(false);
        let ctx = GFunCtx::new(&g, &d).with_cancel(&flag);
        assert!(ctx.check_cancel().is_ok());
        flag.store(true, Ordering::Relaxed);
        assert!(matches!(ctx.check_cancel(), Err(GStoreError::Cancelled)));
        assert!(matches!(
            eval(GFnKind::Wcc, &ctx, &GArgs::default()),
            Err(GStoreError::Cancelled)
        ));
        assert_eq!(ctx.entity_str(INVALID_ENTITY_LITERAL_ID), "");
        assert_eq!(ctx.pred_str(3), "");
    }
}
