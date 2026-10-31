//! gStore's advanced graph functions ("gfun"), evaluated over the
//! per-predicate [`CsrGraph`] with upstream gStore 1.4's exact semantics and
//! output text.
//!
//! Upstream sources (paths relative to the gStore repository):
//! * grammar — `src/Parser/SPARQL/SPARQL.g4` ~197-229 (`builtInCall`), `predSet`
//!   ~138, `negIri` ~262;
//! * argument parsing — `src/Parser/QueryParser.cpp` ~395-600;
//! * evaluation + output — `src/Query/GeneralEvaluation.cpp` ("GE"): the
//!   all-constant branch ~1538-2058 and the WHERE-backed branch ~2547-3110,
//!   the helpers ~3404-3454 (`pathVec2JSON`, `vertVec2JSON`) and ~4093-4206;
//! * algorithms — `src/Query/Algorithm/*.cpp` (`PathQueryHandler`).
//!
//! This module holds the shared vocabulary ([`GFnKind`], [`GArgs`],
//! [`GFunValue`], [`GFunCtx`]), the dispatcher [`eval`], and building blocks
//! ([`cxx`], [`json`], [`traverse`]); [`paths`] and [`metrics`] hold one
//! function per upstream graph function, each documented with its exact
//! upstream specification.
//!
//! # Conventions shared by every function
//!
//! **One call, whole result.** A function computes upstream's *entire* result
//! for one `SELECT (f(...) AS ?v)` projection: all `srcs × dsts` pairs (or the
//! whole graph), producing the exact text upstream stores in the result cell.
//!
//! **Result wrapper.** Upstream builds the cell as the string
//! `"{"paths":[` + items + `]}"` (GE ~1583/~2629 and ~2023/~3099) — a plain
//! literal whose lexical form is the JSON text. [`GFunValue::Json`] carries that
//! lexical form *without* the outer quotes (`{"paths":[...]}`); the query
//! engine wraps it as a plain literal. Exceptions: `cycleBoolean` yields
//! [`GFunValue::Bool`] (`"true"^^xsd:boolean`, GE ~2025-2028), and
//! `confidenceDegree`'s cell is `"pfn_type" + json` (GE ~1538-1552), which
//! upstream's result serializer turns into a whole-response
//! `{"results": <json>}` (`ResultSet.cpp` ~540, ~574-588) — see
//! [`GFnKind::is_pfn_result`].
//!
//! **Pairs.** `srcs`/`dsts` are the *distinct* bindings of the first/second
//! node argument in first-appearance order over the WHERE solutions (a constant
//! IRI gives one; unbound rows are skipped). Upstream iterates
//! `for u in srcs { for v in dsts { ... } }` over the raw (duplicated) binding
//! lists with an `unordered_set` of seen `(u, v)` pairs — exactly the distinct
//! cross product in first-appearance order. A function without a first node
//! argument runs its pair loop once (upstream's dummy `-1`), so it emits once.
//! An empty `srcs` (or `dsts`, for two-node functions) means no pair at all:
//! the result is `{"paths":[]}` for per-pair functions. (Upstream's all-constant
//! branch would instead pass `-1` for a *variable* argument that has no
//! bindings; that only produces garbage and is not reproduced.)
//!
//! **Two upstream branches.** When the WHERE clause has *no* solutions (e.g.
//! `WHERE {}`), upstream takes its all-constant branch (GE ~1508), otherwise
//! the WHERE-backed branch (GE ~2547). Both print the same items except:
//! `kHopNeighbor` names its array `"dst"` in the constant branch but
//! `"neighbor"` in the WHERE-backed one, and only the WHERE-backed branch puts
//! `,` between the items of different pairs for `PPR`, `closenessCentrality`,
//! `bfsCount`, `kHopCount`, `kHopNeighbor` and `shortestPathCount` (the
//! constant branch only ever has one pair). [`GArgs::empty_where`] tells which
//! branch upstream would take. Several functions never print a separator
//! between pairs in either branch (noted per function): their multi-pair output
//! is `{...}{...}` — upstream text, reproduced verbatim.
//!
//! **Node strings.** Upstream prints nodes with `kvstore->getStringByID(id)`:
//! the dictionary key form — `<http://x/a>`, `_:b0`, or `"lit"` /
//! `"lit"^^<dt>` for a literal binding — with **no JSON escaping**. Predicates
//! print as `getPredicateByID(p)` (`<http://x/p>`). Path JSON additionally
//! prints raw **entity ids** (`fromNode`, `toNode`, `nodeIndex`): those are
//! dictionary ids, so matching upstream's numbers also requires the dictionary
//! to number entities in upstream's order. Use [`GFunCtx::term`] /
//! [`GFunCtx::pred_iri`] and the [`json`] helpers.
//!
//! **Numbers.** `ss << double` → [`cxx::fmt_g`] (`%g`, 6 significant
//! digits); `to_string(double)` → [`cxx::fmt_f`] (`%f`); `nlohmann` dumps →
//! [`cxx::fmt_nlohmann`]; integers print plainly. Each spec names which.
//!
//! **Unordered containers.** Where upstream output order comes from a
//! `std::unordered_map`/`unordered_set` (path node lists, `SSSP`, `SSSPLen`,
//! `WCC`), reproduce it with [`cxx::CxxHashOrder`] fed with upstream's
//! insertion sequence. `std::map`/`std::set` order is ascending key.
//!
//! **Vertex domain.** Upstream addresses vertices by entity id and sizes
//! scratch arrays by `getVertNum()`; whole-graph loops run
//! `vid = 0 .. getVertNum()` with `vid` as an entity id. A port iterates the
//! CSR's nodes `0..num_nodes()` instead (ascending entity id) — identical
//! whenever the entity ids `0..n` are exactly the vertices, and the intended
//! meaning otherwise (upstream then reads out of bounds). Likewise upstream's
//! "skip neighbours with id >= getVertNum()" guards (e.g. `bc_dfs`) never fire
//! here. A node argument that is an existing term but not a CSR vertex (no
//! entity–entity edge, or a literal) behaves as an isolated vertex: it has no
//! neighbours but is still printed. An IRI that is not in the dictionary
//! resolves to [`INVALID_ENTITY_LITERAL_ID`] — upstream's `-1` — and each spec
//! says what upstream does with `-1` (most functions return early or print
//! nothing for it).
//!
//! **Arguments as upstream parses them.** `alpha=`, `tol=` and `increase=`
//! are parsed with `stof` (`float` precision) and stored as `double`; `maxIter=`
//! goes through `stof` too and is truncated to `int` where used. Functions
//! apply that rounding themselves (`(x as f32) as f64`), so [`GArgs`] carries
//! the literal's full-precision value. `k`/`retNum` are `stoi` (`int`).
//!
//! **Predicate sets.** [`GArgs::preds`] is upstream's resolved `pred_id_set`
//! ([`PredFilter`]); [`PredFilter::pred_ids`] materialises upstream's exact
//! vector (needed where its *length* or *emptiness* matters).
//!
//! **Cancellation.** Long loops call [`GFunCtx::check_cancel`] periodically.

pub mod cxx;
pub mod json;
pub mod metrics;
pub mod paths;
pub mod traverse;

use std::sync::atomic::{AtomicBool, Ordering};

pub use crate::analytics::csr::{CsrGraph, Dir, NodeId, PredFilter};
use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
pub use crate::model::id::{EntityLiteralId, PredId, INVALID_ENTITY_LITERAL_ID};

/// Every upstream advanced graph function (`ProjectionVar::*_type`,
/// `QueryParser.cpp` ~397-470). Names are matched case-insensitively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GFnKind {
    SimpleCyclePath,
    SimpleCycleBoolean,
    CyclePath,
    CycleBoolean,
    ShortestPath,
    ShortestPathLen,
    KHopReachable,
    KHopEnumerate,
    KHopReachablePath,
    Ppr,
    TriangleCounting,
    ClosenessCentrality,
    BfsCount,
    Pr,
    Sssp,
    SsspLen,
    LabelProp,
    Wcc,
    ClusterCoeff,
    MaximumKplex,
    CoreTruss,
    KHopCount,
    KHopNeighbor,
    ShortestPathCount,
    Louvain,
    ConfidenceDegree,
    DiameterEstimation,
    BetweennessCentrality,
    JaccardSimilarity,
    DegreeCorrelation,
    ShortestPaths,
    KHopAllNeighbors,
}

/// The upstream output literal type of a function (in a SELECT projection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GFunValueKind {
    /// A plain literal holding JSON-ish text ([`GFunValue::Json`]).
    Json,
    /// An `xsd:boolean` ([`GFunValue::Bool`]).
    Bool,
    /// An integer ([`GFunValue::Int`]).
    Int,
    /// A double ([`GFunValue::Num`]).
    Num,
}

/// One positional slot of a function's grammar alternative (`SPARQL.g4`
/// ~197-229), mapped to the [`GArgs`] field it fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GArgSlot {
    /// `varOrIri` → [`GArgs::srcs`] (and [`GArgs::src_given`] = true).
    Src,
    /// Optional leading `(varOrIri ',')?` → [`GArgs::srcs`] / [`GArgs::src_given`]
    /// (`clusterCoeff` only).
    OptSrc,
    /// Second `varOrIri` → [`GArgs::dsts`].
    Dst,
    /// `booleanLiteral` → [`GArgs::directed`].
    Directed,
    /// `integerLiteral` (signed) → [`GArgs::k`].
    K,
    /// Optional `num_integer` (unsigned) → [`GArgs::k`]; `maximumKplex`
    /// defaults it to 1 (`QueryParser.cpp` ~521-525).
    OptK,
    /// `integerLiteral` (signed) → [`GArgs::ret_num`].
    RetNum,
    /// Optional trailing `(',' integerLiteral)?` → [`GArgs::ret_num`]
    /// (absent → upstream `-1`, i.e. `None`).
    OptRetNum,
    /// `predSet` → [`GArgs::preds`].
    Preds,
    /// `alpha = numericLiteral` → [`GArgs::alpha`].
    Alpha,
    /// `maxIter = num_integer` → [`GArgs::max_iter`].
    MaxIter,
    /// `tol = numericLiteral` → [`GArgs::tol`].
    Tol,
    /// `increase = numericLiteral` → [`GArgs::increase`].
    Increase,
    /// Positional `num_integer` → appended to [`GArgs::extra`].
    ExtraInt,
    /// Positional `numericLiteral` → appended to [`GArgs::extra`].
    ExtraNum,
}

impl GFnKind {
    /// Every function, in grammar order.
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

    /// The upstream spelling (as in the user documentation).
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
    /// case-insensitive lexer fragments).
    pub fn from_name(name: &str) -> Option<GFnKind> {
        GFnKind::ALL
            .iter()
            .copied()
            .find(|k| k.name().eq_ignore_ascii_case(name))
    }

    /// The grammar alternative's positional shape (`SPARQL.g4` ~197-229).
    pub fn signature(self) -> &'static [GArgSlot] {
        use GArgSlot::*;
        match self {
            GFnKind::SimpleCyclePath
            | GFnKind::SimpleCycleBoolean
            | GFnKind::CyclePath
            | GFnKind::CycleBoolean
            | GFnKind::ShortestPath
            | GFnKind::ShortestPathLen
            | GFnKind::ShortestPathCount
            | GFnKind::ShortestPaths => &[Src, Dst, Directed, Preds],
            GFnKind::KHopReachable | GFnKind::KHopReachablePath => &[Src, Dst, Directed, K, Preds],
            GFnKind::KHopEnumerate => &[Src, Dst, Directed, K, Preds, OptRetNum],
            GFnKind::Ppr => &[Src, K, Preds, RetNum],
            GFnKind::TriangleCounting => &[Directed, Preds],
            GFnKind::ClosenessCentrality
            | GFnKind::BfsCount
            | GFnKind::Sssp
            | GFnKind::SsspLen
            | GFnKind::BetweennessCentrality => &[Src, Directed, Preds],
            GFnKind::Pr => &[Directed, Preds, RetNum, Alpha, MaxIter, Tol],
            GFnKind::LabelProp => &[Directed, Preds, MaxIter],
            GFnKind::Wcc | GFnKind::DiameterEstimation => &[Preds],
            GFnKind::ClusterCoeff => &[OptSrc, Directed, Preds],
            GFnKind::MaximumKplex => &[Preds, OptK],
            GFnKind::CoreTruss => &[Preds, ExtraInt, ExtraInt],
            GFnKind::KHopCount => &[Src, Directed, K, Preds],
            GFnKind::KHopNeighbor | GFnKind::KHopAllNeighbors => &[Src, Directed, K, Preds, OptRetNum],
            GFnKind::Louvain => &[Directed, Preds, MaxIter, Increase],
            GFnKind::ConfidenceDegree => &[ExtraNum, ExtraNum, ExtraNum, ExtraNum, ExtraNum, ExtraNum],
            GFnKind::JaccardSimilarity => &[Src, Preds, K, RetNum],
            GFnKind::DegreeCorrelation => &[Src, K, Preds],
        }
    }

    /// The literal type upstream produces for this function in a SELECT
    /// projection: [`GFunValueKind::Bool`] for `cycleBoolean`, JSON text for
    /// everything else — including `simpleCycleBoolean`, which upstream never
    /// dispatches (its cell is the bare `{"paths":[]}` wrapper).
    pub fn output_kind(self) -> GFunValueKind {
        match self {
            GFnKind::CycleBoolean => GFunValueKind::Bool,
            _ => GFunValueKind::Json,
        }
    }

    /// Whether upstream marks the cell as a `pfn_type` result, which replaces
    /// the whole JSON response by `{"results": <json>}` (only
    /// `confidenceDegree`, GE ~1538-1552 / ~2533-2546).
    pub fn is_pfn_result(self) -> bool {
        self == GFnKind::ConfidenceDegree
    }

    /// Whether the function takes a source node (`Src` or `OptSrc` slot).
    pub fn has_src(self) -> bool {
        self.signature()
            .iter()
            .any(|s| matches!(s, GArgSlot::Src | GArgSlot::OptSrc))
    }

    /// Whether the function takes a destination node.
    pub fn has_dst(self) -> bool {
        self.signature().contains(&GArgSlot::Dst)
    }
}

impl std::fmt::Display for GFnKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Resolved arguments of one graph-function call.
///
/// Field ← grammar slot mapping per function (see [`GFnKind::signature`]):
///
/// | function | `srcs` | `dsts` | `directed` | `k` | `ret_num` | `preds` | other |
/// |---|---|---|---|---|---|---|---|
/// | simpleCyclePath / simpleCycleBoolean / cyclePath / cycleBoolean / shortestPath / shortestPathLen / shortestPathCount / shortestPaths | u | v | ✓ | | | ✓ | |
/// | kHopReachable / kHopReachablePath | u | v | ✓ | k | | ✓ | |
/// | kHopEnumerate | u | v | ✓ | k | optional (absent = `None`, upstream -1) | ✓ | |
/// | PPR | u | | | hop count `k` | retNum | ✓ | |
/// | triangleCounting | | | ✓ | | | ✓ | |
/// | closenessCentrality / bfsCount / SSSP / SSSPLen / betweennessCentrality | u | | ✓ | | | ✓ | |
/// | PR | | | ✓ | | retNum | ✓ | `alpha`, `max_iter`, `tol` |
/// | labelProp | | | ✓ | | | ✓ | `max_iter` |
/// | WCC / diameterEstimation | | | | | | ✓ | |
/// | clusterCoeff | optional u (`src_given`) | | ✓ | | | ✓ | |
/// | maximumKplex | | | | k (absent → 1) | | ✓ | |
/// | coreTruss | | | | | | ✓ | `extra = [alpha, beta]` |
/// | kHopCount | u | | ✓ | k | | ✓ | |
/// | kHopNeighbor / kHopAllNeighbors | u | | ✓ | k | optional | ✓ | |
/// | louvain | | | ✓ | | | ✓ | `max_iter`, `increase` |
/// | confidenceDegree | | | | | | | `extra` = the six numbers (ignored upstream) |
/// | JaccardSimilarity | u | | | k | retNum | ✓ | |
/// | degreeCorrelation | u | | | k | | ✓ | |
#[derive(Debug, Clone, Default)]
pub struct GArgs {
    /// Distinct bindings of the first node argument, first-appearance order
    /// (a constant gives one). Entity/literal ids; an IRI unknown to the
    /// dictionary is [`INVALID_ENTITY_LITERAL_ID`] (upstream `-1`).
    pub srcs: Vec<EntityLiteralId>,
    /// Distinct bindings of the second node argument (two-node functions).
    pub dsts: Vec<EntityLiteralId>,
    /// Whether the call has a first node argument at all (always true for
    /// `Src` signatures; distinguishes `clusterCoeff(u, d, P)` from
    /// `clusterCoeff(d, P)`).
    pub src_given: bool,
    /// The `booleanLiteral`: `true` = directed, `false` = every edge is also
    /// traversable backwards. Upstream leaves it uninitialised for functions
    /// without one; they ignore it.
    pub directed: bool,
    /// The hop bound / `k` integer, as written.
    pub k: Option<i64>,
    /// The `retNum` integer, as written (`None` when an optional one is absent).
    pub ret_num: Option<i64>,
    /// The resolved predicate set.
    pub preds: PredFilter,
    /// `alpha=` (PR), full precision as written.
    pub alpha: Option<f64>,
    /// `maxIter=` (PR, labelProp, louvain).
    pub max_iter: Option<f64>,
    /// `tol=` (PR).
    pub tol: Option<f64>,
    /// `increase=` (louvain).
    pub increase: Option<f64>,
    /// Remaining positional numbers in order (coreTruss, confidenceDegree).
    pub extra: Vec<f64>,
    /// Whether the WHERE clause produced no solutions, i.e. upstream would take
    /// its all-constant branch (see the module docs).
    pub empty_where: bool,
}

/// The result of one graph-function call.
#[derive(Debug, Clone, PartialEq)]
pub enum GFunValue {
    /// JSON text (the lexical form of upstream's plain-literal cell, without the
    /// outer quotes), e.g. `{"paths":[...]}`.
    Json(String),
    /// An `xsd:boolean` (cycleBoolean).
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A double.
    Num(f64),
}

/// What a graph function may read: the CSR, the dictionary for id ↔ string,
/// and the query's cancellation flag.
#[derive(Clone, Copy)]
pub struct GFunCtx<'a> {
    /// The per-predicate graph (`Database::csr`).
    pub csr: &'a CsrGraph,
    /// The database dictionary.
    pub dict: &'a Dictionary,
    cancel: Option<&'a AtomicBool>,
}

impl std::fmt::Debug for GFunCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GFunCtx")
            .field("csr", self.csr)
            .field("cancellable", &self.cancel.is_some())
            .finish()
    }
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

    /// Attach the query's cancellation flag (set ⇒ abort with
    /// [`GStoreError::Cancelled`]).
    pub fn with_cancel(mut self, flag: &'a AtomicBool) -> GFunCtx<'a> {
        self.cancel = Some(flag);
        self
    }

    /// `Err(Cancelled)` once the query's cancellation flag is set.
    pub fn check_cancel(&self) -> Result<()> {
        match self.cancel {
            Some(flag) if flag.load(Ordering::Relaxed) => Err(GStoreError::Cancelled),
            _ => Ok(()),
        }
    }

    /// Upstream `kvstore->getStringByID(id)`: the term's dictionary key
    /// (`<iri>`, `_:b`, `"lit"`…); `""` for an unknown / invalid id.
    pub fn term(&self, id: EntityLiteralId) -> &'a str {
        self.dict.id_to_string(id).unwrap_or("")
    }

    /// Upstream `kvstore->getPredicateByID(p)`: `<iri>`; `""` if unknown.
    pub fn pred_iri(&self, p: PredId) -> &'a str {
        self.dict.predicate_to_string(p).unwrap_or("")
    }

    /// The CSR node of an entity id (`None`: not a vertex — isolated, a
    /// literal, or [`INVALID_ENTITY_LITERAL_ID`]).
    pub fn node(&self, id: EntityLiteralId) -> Option<NodeId> {
        self.csr.node_of(id)
    }

    /// The entity id of a node.
    pub fn entity(&self, node: NodeId) -> EntityLiteralId {
        self.csr.entity_of(node)
    }
}

/// Prefix of the error a not-yet-implemented graph function returns.
pub const NOT_IMPLEMENTED: &str = "graph function not implemented yet";

/// The clean error a graph function returns while it is not implemented.
pub fn not_implemented(kind: GFnKind) -> GStoreError {
    GStoreError::Query(format!("{NOT_IMPLEMENTED}: {}", kind.name()))
}

/// Whether `e` is the [`not_implemented`] error.
pub fn is_not_implemented(e: &GStoreError) -> bool {
    matches!(e, GStoreError::Query(m) if m.starts_with(NOT_IMPLEMENTED))
}

/// Evaluate one graph function over the whole argument cross product.
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

/// The distinct `(u, v)` pairs of `srcs × dsts` in upstream's iteration order
/// (outer loop over `srcs`). Callers with no destination argument use
/// [`src_iterations`] instead.
pub fn pairs(args: &GArgs) -> impl Iterator<Item = (EntityLiteralId, EntityLiteralId)> + '_ {
    args.srcs
        .iter()
        .flat_map(move |&u| args.dsts.iter().map(move |&v| (u, v)))
}

/// The source ids a single-node function loops over: `srcs` when the call
/// has a node argument, otherwise one dummy [`INVALID_ENTITY_LITERAL_ID`]
/// iteration (upstream's `uid_ls.push_back(-1)`).
pub fn src_iterations(args: &GArgs) -> Vec<EntityLiteralId> {
    if args.src_given {
        args.srcs.clone()
    } else {
        vec![INVALID_ENTITY_LITERAL_ID]
    }
}

/// Wrap the items text as upstream's cell content: `{"paths":[` items `]}`.
pub fn paths_wrapper(items: &str) -> String {
    let mut s = String::with_capacity(items.len() + 12);
    s.push_str("{\"paths\":[");
    s.push_str(items);
    s.push_str("]}");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_case_insensitively() {
        for k in GFnKind::ALL {
            assert_eq!(GFnKind::from_name(k.name()), Some(k));
            assert_eq!(GFnKind::from_name(&k.name().to_ascii_uppercase()), Some(k));
        }
        assert_eq!(GFnKind::from_name("shortestpaths"), Some(GFnKind::ShortestPaths));
        assert_eq!(GFnKind::from_name("nope"), None);
    }

    #[test]
    fn signatures_follow_the_grammar() {
        assert_eq!(GFnKind::Ppr.signature(), &[GArgSlot::Src, GArgSlot::K, GArgSlot::Preds, GArgSlot::RetNum]);
        assert!(GFnKind::ClusterCoeff.has_src());
        assert!(!GFnKind::Wcc.has_src());
        assert!(GFnKind::ShortestPath.has_dst());
        assert!(!GFnKind::Sssp.has_dst());
        assert_eq!(GFnKind::CycleBoolean.output_kind(), GFunValueKind::Bool);
        assert_eq!(GFnKind::SimpleCycleBoolean.output_kind(), GFunValueKind::Json);
        assert!(GFnKind::ConfidenceDegree.is_pfn_result());
        assert_eq!(GFnKind::ConfidenceDegree.signature().len(), 6);
    }

    #[test]
    fn pair_iteration_order() {
        let a = GArgs {
            srcs: vec![3, 1],
            dsts: vec![5, 4],
            src_given: true,
            ..GArgs::default()
        };
        assert_eq!(pairs(&a).collect::<Vec<_>>(), vec![(3, 5), (3, 4), (1, 5), (1, 4)]);
        assert_eq!(src_iterations(&a), vec![3, 1]);
        assert_eq!(src_iterations(&GArgs::default()), vec![INVALID_ENTITY_LITERAL_ID]);
        assert_eq!(paths_wrapper(""), "{\"paths\":[]}");
    }
}
