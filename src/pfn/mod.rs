//! PFN — gStore's personalized (user-defined) graph functions.
//!
//! Upstream gStore 1.4 (`src/Pfn/`, `src/Server/ApiPFNOperation.cpp`,
//! `src/Query/PfnAnalysisQuery.cpp`) lets each user write C++ functions over
//! the graph, compiles them with `g++` into a shared library and `dlopen`s
//! it to answer `SELECT (PFN.name({…}) AS ?v) WHERE {…}`. This port keeps the
//! management API and the query semantics but runs the functions as
//! **sandboxed WebAssembly** (feature `pfn-wasm`, the `wasmi` interpreter):
//! a function's body is WebAssembly *text* (WAT), compiled by `funcudb`'s
//! build step into a validated module that can only call the gStore host
//! interface — no file system, no network, no clock — under an instruction
//! budget, a memory cap and the query's cancel flag. `docs/PFN.md` is the
//! user guide (host ABI, examples, limits).
//!
//! # Pieces
//!
//! * [`PfnRegistry`] — the per-server store of every user's functions
//!   (`<pfn_base_path>/src/<user>/data.json` + generated sources, compiled
//!   modules under `<pfn_base_path>/lib/<user>/`), with upstream's
//!   create / update / delete / compile / query / review operations and a
//!   cache of loaded modules. The `/api` operations `funquery`, `funcudb` and
//!   `funreview` (`crate::gapi`) are thin wrappers over it.
//! * [`PfnCaller`] — "user U calls functions of registry R with limits L":
//!   what a query evaluator needs to run `PFN.name(…)`. Functions belong to
//!   users and a query only reaches its *caller's* functions (upstream looks
//!   the `.so` up under the query's user name).
//! * [`GraphCtx`] ([`graph`]) — the graph a function sees: vertex / edge /
//!   predicate counts, per-predicate out / in neighbours and id ↔ string
//!   conversions, implemented over the CSR of the advanced graph functions
//!   ([`CsrGraphCtx`]). The trait is runtime-independent so another runtime
//!   (native, a script language) could back the same interface.
//! * [`abi`] — the host interface table (module `"gstore"`), the generated
//!   module prelude and the bounds-checked guest-memory helpers.
//! * [`call`] — upstream's argument handling: variable substitution over the
//!   WHERE solutions, the sorted distinct argument set, the result array.
//!
//! Without the `pfn-wasm` feature every metadata operation works (create,
//! update, delete, list, review), while compiling and calling a function fail
//! with an error naming the feature ([`FEATURE_HINT`]).

pub mod abi;
pub mod call;
pub mod graph;
mod registry;
#[cfg(feature = "pfn-wasm")]
mod wasm;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub use graph::{CsrGraphCtx, GraphCtx};
pub use registry::{PfnRegistry, ResolvedFn};

/// `funStatus` of a function created or updated but not compiled since
/// (upstream "1-待编译").
pub const STATUS_PENDING: &str = "1";
/// `funStatus` of a successfully compiled, callable function (upstream
/// "2-已编译").
pub const STATUS_COMPILED: &str = "2";
/// `funStatus` after a failed compilation (upstream "3-异常").
pub const STATUS_ERROR: &str = "3";

/// The text every error about the missing runtime carries.
pub const FEATURE_HINT: &str = "PFN functions run as sandboxed WebAssembly, which this build of \
     gstore does not include: rebuild it with the `pfn-wasm` cargo feature \
     (cargo build --features pfn-wasm)";

/// Whether this build can compile and run PFN functions.
pub const RUNTIME_AVAILABLE: bool = cfg!(feature = "pfn-wasm");

/// The prefix of a `funBody` that carries a binary WebAssembly module,
/// base64-encoded (standard or URL-safe alphabet, padding optional), instead
/// of WebAssembly text.
pub const BASE64_PREFIX: &str = "wasm-base64:";

/// One function's metadata — upstream's `PFNInfo` (`src/Pfn/PFNEntity.h`),
/// with its six fields. `funBody` / `funSubs` are kept as the client sent
/// them (upstream URL-decodes them only when it generates the source, see
/// [`generate_source`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PfnInfo {
    /// `funName` — the name `PFN.<name>(…)` calls and the module exports.
    pub name: String,
    /// `funDesc`
    pub desc: String,
    /// `funBody` — the body of the exported function (WAT instructions,
    /// optionally starting with `(local …)` declarations), or a whole
    /// `(module …)`, or [`BASE64_PREFIX`] + a binary module.
    pub body: String,
    /// `funSubs` — module fields placed before the function: helper
    /// functions, globals, data segments.
    pub subs: String,
    /// `funStatus` — [`STATUS_PENDING`], [`STATUS_COMPILED`] or
    /// [`STATUS_ERROR`].
    pub status: String,
    /// `lastTime` — `yyyy-MM-dd HH:mm:ss` (server local time) of the last
    /// create / update / compile.
    pub last_time: String,
}

impl PfnInfo {
    /// The info from a request's `funInfo`: a JSON object, or a string
    /// holding one (upstream `PFNInfo(const json&)` accepts both). Missing
    /// fields are empty; a number or boolean is taken as its text. `Err`
    /// describes a value that is neither.
    pub fn from_json(v: &crate::json::Json) -> Result<PfnInfo, String> {
        use crate::json::Json;
        let parsed;
        let obj = match v {
            Json::Null => return Ok(PfnInfo::default()),
            Json::Str(s) if s.trim().is_empty() => return Ok(PfnInfo::default()),
            Json::Str(s) => {
                parsed = crate::json::parse(s)
                    .map_err(|e| format!("funInfo is not a JSON object ({e})"))?;
                &parsed
            }
            other => other,
        };
        if obj.as_object().is_none() {
            return Err("funInfo must be a JSON object".to_string());
        }
        let field = |key: &str| -> Result<String, String> {
            match obj.get(key) {
                None | Some(Json::Null) => Ok(String::new()),
                Some(Json::Str(s)) => Ok(s.clone()),
                Some(Json::Num(n)) => Ok(n.clone()),
                Some(Json::Bool(b)) => Ok(b.to_string()),
                Some(_) => Err(format!("funInfo.{key} must be a string")),
            }
        };
        Ok(PfnInfo {
            name: field("funName")?,
            desc: field("funDesc")?,
            body: field("funBody")?,
            subs: field("funSubs")?,
            status: field("funStatus")?,
            last_time: field("lastTime")?,
        })
    }

    /// Upstream's `toJSON`: the six fields (serialized key-sorted, as
    /// `nlohmann::json` does).
    pub fn to_json(&self) -> crate::json::Json {
        crate::json::Json::obj()
            .with("funName", self.name.as_str())
            .with("funDesc", self.desc.as_str())
            .with("funBody", self.body.as_str())
            .with("funSubs", self.subs.as_str())
            .with("funStatus", self.status.as_str())
            .with("lastTime", self.last_time.as_str())
    }

    /// Upstream's `empty()`: neither a name nor a body.
    pub fn is_empty(&self) -> bool {
        self.name.is_empty() && self.body.is_empty()
    }
}

/// The sandbox limits of PFN functions — `conf.ini`'s `[pfn]` section
/// (Rust extensions; upstream runs native code without limits). Applied per
/// database with the other query settings
/// ([`QuerySettings::pfn`](crate::db::QuerySettings::pfn)) and, for the
/// compile-time ones, by the registry
/// ([`PfnRegistry::set_limits`]). The output of one call is bounded by the
/// graph functions' `gfun_max_output_bytes`.
///
/// **Fuel.** A call is charged one unit per WebAssembly instruction, one
/// per [`HOST_BYTES_PER_FUEL`] guest bytes a host function reads or writes,
/// and — before it starts — one per [`HOST_BYTES_PER_FUEL`] bytes its
/// instantiation sets up (the module's initial linear memory and its data
/// segments; review ext#1), plus one. Both budgets count that fuel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PfnLimits {
    /// `pfn_fuel_per_call`: the most fuel (WebAssembly instructions, host
    /// work and instantiation, see the type docs) one function call may
    /// use; `0` = unlimited (the call then stops only when cancelled or
    /// when its query's budget runs out).
    pub fuel_per_call: u64,
    /// `pfn_max_memory_pages`: the most 64 KiB pages a function's linear
    /// memory may grow to.
    pub max_memory_pages: u32,
    /// `pfn_max_module_bytes`: the largest module — source text and compiled
    /// binary — `funcudb` accepts.
    pub max_module_bytes: usize,
    /// `pfn_max_functions`: the most functions one user may own.
    pub max_functions: usize,
    /// `pfn_fuel_per_query`: the most fuel all the PFN calls of one query
    /// may use together (every argument combination, every `PFN.name(…)`
    /// of the query, its sub-queries and `GRAPH` blocks); `0` = unlimited.
    /// Without it a query could run [`call::MAX_COMBINATIONS`] calls of a
    /// full `fuel_per_call` each (review ext#1). The count belongs to the
    /// [`PfnCaller`] a query runs with: every query starts from zero (in an
    /// update, every `DELETE` / `INSERT … WHERE` operation).
    pub fuel_per_query: u64,
}

/// Default [`PfnLimits::fuel_per_call`]: one billion instructions.
pub const DEFAULT_FUEL_PER_CALL: u64 = 1_000_000_000;
/// Default [`PfnLimits::max_memory_pages`]: 1024 pages = 64 MiB.
pub const DEFAULT_MAX_MEMORY_PAGES: u32 = 1024;
/// Default [`PfnLimits::max_module_bytes`]: 1 MiB.
pub const DEFAULT_MAX_MODULE_BYTES: usize = 1 << 20;
/// Default [`PfnLimits::max_functions`] per user.
pub const DEFAULT_MAX_FUNCTIONS: usize = 100;
/// Default [`PfnLimits::fuel_per_query`]: ten billion, ten calls' worth of
/// the default per-call budget (a few tens of seconds of interpretation).
pub const DEFAULT_FUEL_PER_QUERY: u64 = 10 * DEFAULT_FUEL_PER_CALL;

/// Guest bytes a host function may read or write, or an instantiation may
/// set up, per unit of fuel it is charged (about the cost of an
/// instruction).
pub const HOST_BYTES_PER_FUEL: u64 = 8;

impl Default for PfnLimits {
    fn default() -> PfnLimits {
        PfnLimits {
            fuel_per_call: DEFAULT_FUEL_PER_CALL,
            max_memory_pages: DEFAULT_MAX_MEMORY_PAGES,
            max_module_bytes: DEFAULT_MAX_MODULE_BYTES,
            max_functions: DEFAULT_MAX_FUNCTIONS,
            fuel_per_query: DEFAULT_FUEL_PER_QUERY,
        }
    }
}

/// Who calls PFN functions, from which registry, under which limits — what a
/// query evaluator needs to evaluate `PFN.name({…})`
/// ([`Evaluator::with_pfn`](crate::query::engine::Evaluator::with_pfn)).
/// Cheap to clone.
///
/// A caller also counts the fuel its calls used, for
/// [`PfnLimits::fuel_per_query`]: clones share the count (the evaluators
/// of one query — sub-queries, `GRAPH` blocks — hold clones), while
/// [`new`](Self::new), [`with_limits`](Self::with_limits),
/// [`as_user`](Self::as_user) and [`for_query`](Self::for_query) start a
/// new one. Every database query path installs its caller through
/// `with_limits` (or `for_query`), so each query has its own budget.
#[derive(Debug, Clone)]
pub struct PfnCaller {
    registry: Arc<PfnRegistry>,
    user: String,
    limits: PfnLimits,
    /// The fuel the calls of this caller's query used so far.
    spent: Arc<AtomicU64>,
}

impl PfnCaller {
    /// `user` calls the functions of `registry`, with the registry's limits.
    pub fn new(registry: Arc<PfnRegistry>, user: impl Into<String>) -> PfnCaller {
        let limits = registry.limits();
        PfnCaller {
            registry,
            user: user.into(),
            limits,
            spent: Arc::default(),
        }
    }

    /// The same caller under `limits` (a database's
    /// [`QuerySettings::pfn`](crate::db::QuerySettings::pfn)), for a new
    /// query: its [per-query fuel](PfnLimits::fuel_per_query) count starts
    /// from zero.
    pub fn with_limits(self, limits: PfnLimits) -> PfnCaller {
        PfnCaller {
            limits,
            spent: Arc::default(),
            ..self
        }
    }

    /// The same caller for a new query: same registry, user and limits, the
    /// [per-query fuel](PfnLimits::fuel_per_query) count from zero.
    pub fn for_query(&self) -> PfnCaller {
        PfnCaller {
            registry: Arc::clone(&self.registry),
            user: self.user.clone(),
            limits: self.limits,
            spent: Arc::default(),
        }
    }

    /// The same registry and limits, called as `user` (a new query).
    pub fn as_user(&self, user: impl Into<String>) -> PfnCaller {
        PfnCaller {
            registry: Arc::clone(&self.registry),
            user: user.into(),
            limits: self.limits,
            spent: Arc::default(),
        }
    }

    /// The fuel the calls of this caller's query used so far (see
    /// [`PfnLimits::fuel_per_query`]).
    pub fn fuel_spent(&self) -> u64 {
        self.spent.load(Ordering::Relaxed)
    }

    /// The calling user.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// The registry the functions come from.
    pub fn registry(&self) -> &Arc<PfnRegistry> {
        &self.registry
    }

    /// The limits calls run under.
    pub fn limits(&self) -> PfnLimits {
        self.limits
    }

    /// Run the caller's function `name` once with argument JSON `params` over
    /// `graph`; returns the text the function emitted. `cancel` stops a
    /// running call (checked between fuel slices); `max_output` bounds the
    /// emitted bytes.
    pub fn call(
        &self,
        name: &str,
        params: &str,
        graph: &dyn GraphCtx,
        cancel: Option<&std::sync::atomic::AtomicBool>,
        max_output: usize,
    ) -> crate::Result<String> {
        let f = self.resolve(name)?;
        self.run(&f, params, graph, cancel, max_output)
    }

    /// Look the caller's function `name` up (it must exist and be compiled),
    /// to [`run`](Self::run) it any number of times.
    pub fn resolve(&self, name: &str) -> crate::Result<ResolvedFn> {
        self.registry.resolve(&self.user, name)
    }

    /// Run a [resolved](Self::resolve) function once (see
    /// [`call`](Self::call)).
    pub fn run(
        &self,
        f: &ResolvedFn,
        params: &str,
        graph: &dyn GraphCtx,
        cancel: Option<&std::sync::atomic::AtomicBool>,
        max_output: usize,
    ) -> crate::Result<String> {
        self.registry.run(
            f,
            params,
            graph,
            &CallOptions {
                limits: self.limits,
                cancel,
                max_output,
                spent: Some(&self.spent),
            },
        )
    }
}

/// Per-call execution options.
#[derive(Debug, Clone, Copy)]
pub struct CallOptions<'a> {
    pub limits: PfnLimits,
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    /// The most bytes the function may emit.
    pub max_output: usize,
    /// The fuel the earlier calls of the query used, which this call adds
    /// its own to; the call stops when the total passes
    /// [`PfnLimits::fuel_per_query`]. `None`: the call is a query of its
    /// own (only its own fuel counts against that budget).
    pub spent: Option<&'a AtomicU64>,
}

/// Whether `name` is a valid function name: what the SPARQL lexer reads as
/// one word after `PFN.` (a letter or `_`, then letters, digits or `_`), at
/// most 64 characters. Such a name is also a safe file name.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_alphabetic() || first == '_')
        && chars.all(|c| c.is_alphanumeric() || c == '_')
        && name.chars().count() <= 64
}

/// The rule [`valid_name`] enforces, for error messages.
pub const NAME_RULE: &str = "a function name starts with a letter or '_' and continues with \
     letters, digits or '_' (at most 64 characters)";

/// Whether `user` is a name the registry stores functions for: the user
/// store's rule (`[A-Za-z0-9_-]`, 1..64), which makes it a safe directory
/// name.
pub fn valid_user(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 64
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Upstream's `StringUtil::url_decode`: `+` is a space and `%XX` a byte.
/// Unlike upstream (which asserts), a `%` not followed by two hex digits is
/// kept as is; invalid UTF-8 after decoding is replaced (U+FFFD). The
/// crate's one decoder ([`crate::util::form::url_decode`]).
pub fn url_decode(s: &str) -> String {
    crate::util::form::url_decode(s)
}

/// Upstream's `StringUtil::url_encode`: ASCII letters, digits and `-_.~`
/// stay, a space is `+`, every other byte is `%XX` (upper-case hex).
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(HEX[usize::from(b >> 4)] as char);
                out.push(HEX[usize::from(b & 15)] as char);
            }
        }
    }
    out
}

/// What a function's source is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// `funBody` is the body of the exported function; the generated module
    /// wraps it (with `funSubs`) in the host-interface prelude.
    Body,
    /// `funBody` is a complete `(module …)`.
    Module,
    /// `funBody` is [`BASE64_PREFIX`] + a base64 binary module.
    Base64,
}

/// The kind of source `body` (already URL-decoded and trimmed) is.
pub fn source_kind(body: &str) -> SourceKind {
    if body.starts_with(BASE64_PREFIX) {
        SourceKind::Base64
    } else if body
        .strip_prefix('(')
        .map(str::trim_start)
        .is_some_and(|rest| rest.starts_with("module") && !rest[6..].starts_with(is_idchar))
    {
        SourceKind::Module
    } else {
        SourceKind::Body
    }
}

fn is_idchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-./:<=>?@\\^_`|~".contains(c)
}

/// Upstream's `fun_build_source_data`: the source a function compiles from,
/// shown by `funreview`. `funBody` and `funSubs` are URL-decoded (as upstream
/// clients send them encoded — see `docs/PFN.md`) and trimmed; a body is
/// wrapped, with the subs, into a module that imports every host function
/// ([`abi::HOST_FUNCTIONS`]), declares and exports one page of memory and
/// exports the function as `funName`:
///
/// ```text
/// (module
///   (import "gstore" "vertex_count" (func $vertex_count (result i64)))
///   …
///   (memory (export "memory") 1)
///   <funSubs>
///   (func (export "<funName>") (result i32)
///     <funBody>)
/// )
/// ```
///
/// A complete `(module …)` or a [`BASE64_PREFIX`] module is returned as sent.
pub fn generate_source(info: &PfnInfo) -> String {
    let body = url_decode(&info.body);
    let body = body.trim();
    if source_kind(body) != SourceKind::Body {
        return body.to_string();
    }
    let subs = url_decode(&info.subs);
    let mut out = String::with_capacity(body.len() + subs.len() + 4096);
    out.push_str(&format!(
        ";; gStore PFN function \"{}\" — generated by gstore-rust (see docs/PFN.md)\n",
        info.name
    ));
    out.push_str(&abi::prelude());
    let subs = subs.trim();
    if !subs.is_empty() {
        out.push_str("  ;; ---- funSubs ----\n");
        out.push_str(subs);
        out.push('\n');
    }
    out.push_str("  ;; ---- funBody ----\n");
    out.push_str(&format!(
        "  (func (export \"{}\") (result i32)\n",
        info.name
    ));
    out.push_str(body);
    out.push_str("\n  )\n)\n");
    out
}

/// Decode standard or URL-safe base64 (padding optional, ASCII whitespace
/// ignored; a space reads as `+`, which URL-decoding a `+` produces). The
/// crate's one decoder under its lenient rules
/// ([`crate::util::base64::LENIENT`]).
pub fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    crate::util::base64::decode(s, crate::util::base64::LENIENT).map_err(|e| e.to_string())
}

/// The current server-local time as upstream's `TimeUtil::now()`
/// (`yyyy-MM-dd HH:mm:ss`).
pub(crate) fn now() -> String {
    crate::sysdb::clock::now().datetime()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::Json;

    #[test]
    fn names_follow_the_sparql_word_rule() {
        for ok in ["f", "_x", "outDegree", "k_hop2", "度数"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "2x", "a-b", "a.b", "a b", "../x", &"x".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(valid_user("root") && valid_user("a-b_1"));
        assert!(!valid_user("") && !valid_user("..") && !valid_user("a/b"));
    }

    #[test]
    fn url_codec_matches_upstream() {
        assert_eq!(url_decode("a+b%2B%41%zz%"), "a b+A%zz%");
        assert_eq!(
            url_encode("(i32.const 1) ;; ok~"),
            "%28i32.const+1%29+%3B%3B+ok~"
        );
        let src = "(func $f (result i32) (i32.add (i32.const 1) (i32.const 2)))\n";
        assert_eq!(url_decode(&url_encode(src)), src);
        assert_eq!(url_decode(&url_encode("度数 100%")), "度数 100%");
    }

    #[test]
    fn info_reads_objects_and_strings() {
        let j = Json::obj()
            .with("funName", "f")
            .with("funBody", "(i32.const 1)")
            .with("funStatus", Json::Num("2".into()))
            .with("funArgs", "2");
        let info = PfnInfo::from_json(&j).unwrap();
        assert_eq!(info.name, "f");
        assert_eq!(info.status, "2");
        assert_eq!(info.desc, "");
        let text = Json::Str(j.to_string());
        assert_eq!(PfnInfo::from_json(&text).unwrap(), info);
        assert_eq!(PfnInfo::from_json(&Json::Null).unwrap(), PfnInfo::default());
        assert!(PfnInfo::from_json(&Json::Arr(vec![])).is_err());
        assert!(PfnInfo::from_json(&Json::obj().with("funName", Json::arr())).is_err());
        // Upstream's six fields, key-sorted on the wire.
        assert_eq!(
            info.to_json().into_sorted().to_string(),
            r#"{"funBody":"(i32.const 1)","funDesc":"","funName":"f","funStatus":"2","funSubs":"","lastTime":""}"#
        );
    }

    #[test]
    fn sources_wrap_bodies_and_keep_modules() {
        let info = PfnInfo {
            name: "deg".into(),
            body: "%28i32.const+1%29".into(),
            subs: "(global $g i32 (i32.const 0))".into(),
            ..PfnInfo::default()
        };
        let src = generate_source(&info);
        assert!(src.contains("(import \"gstore\" \"vertex_count\""), "{src}");
        assert!(src.contains("(memory (export \"memory\") 1)"), "{src}");
        assert!(src.contains("(global $g i32 (i32.const 0))"), "{src}");
        assert!(
            src.contains("(func (export \"deg\") (result i32)\n(i32.const 1)\n"),
            "{src}"
        );
        let module = PfnInfo {
            name: "m".into(),
            body: "  (module (func (export \"m\") (result i32) (i32.const 1)))  ".into(),
            ..PfnInfo::default()
        };
        assert_eq!(
            generate_source(&module),
            "(module (func (export \"m\") (result i32) (i32.const 1)))"
        );
        assert_eq!(source_kind("(module)"), SourceKind::Module);
        assert_eq!(source_kind("( module $m)"), SourceKind::Module);
        assert_eq!(source_kind("(modulex)"), SourceKind::Body);
        assert_eq!(source_kind("(local $i i32)"), SourceKind::Body);
        assert_eq!(source_kind("wasm-base64:AGFzbQ=="), SourceKind::Base64);
    }

    #[test]
    fn base64_decodes_both_alphabets() {
        assert_eq!(base64_decode("AGFzbQEAAAA=").unwrap(), b"\0asm\x01\0\0\0");
        assert_eq!(base64_decode("AGFzbQEAAAA").unwrap(), b"\0asm\x01\0\0\0");
        assert_eq!(base64_decode("-_8=").unwrap(), [0xfb, 0xff]);
        assert_eq!(base64_decode("+/8=").unwrap(), [0xfb, 0xff]);
        // A URL-decoded '+' is a space.
        assert_eq!(base64_decode(" /8=").unwrap(), [0xfb, 0xff]);
        assert!(base64_decode("A").is_err());
        assert!(base64_decode("AA=A").is_err());
        assert!(base64_decode("A*").is_err());
    }
}
