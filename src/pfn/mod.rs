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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PfnLimits {
    /// `pfn_fuel_per_call`: the most WebAssembly instructions (wasmi fuel
    /// units) one function call may execute; `0` = unlimited (the call then
    /// stops only when cancelled).
    pub fuel_per_call: u64,
    /// `pfn_max_memory_pages`: the most 64 KiB pages a function's linear
    /// memory may grow to.
    pub max_memory_pages: u32,
    /// `pfn_max_module_bytes`: the largest module — source text and compiled
    /// binary — `funcudb` accepts.
    pub max_module_bytes: usize,
    /// `pfn_max_functions`: the most functions one user may own.
    pub max_functions: usize,
}

/// Default [`PfnLimits::fuel_per_call`]: one billion instructions.
pub const DEFAULT_FUEL_PER_CALL: u64 = 1_000_000_000;
/// Default [`PfnLimits::max_memory_pages`]: 1024 pages = 64 MiB.
pub const DEFAULT_MAX_MEMORY_PAGES: u32 = 1024;
/// Default [`PfnLimits::max_module_bytes`]: 1 MiB.
pub const DEFAULT_MAX_MODULE_BYTES: usize = 1 << 20;
/// Default [`PfnLimits::max_functions`] per user.
pub const DEFAULT_MAX_FUNCTIONS: usize = 100;

impl Default for PfnLimits {
    fn default() -> PfnLimits {
        PfnLimits {
            fuel_per_call: DEFAULT_FUEL_PER_CALL,
            max_memory_pages: DEFAULT_MAX_MEMORY_PAGES,
            max_module_bytes: DEFAULT_MAX_MODULE_BYTES,
            max_functions: DEFAULT_MAX_FUNCTIONS,
        }
    }
}

/// Who calls PFN functions, from which registry, under which limits — what a
/// query evaluator needs to evaluate `PFN.name({…})`
/// ([`Evaluator::with_pfn`](crate::query::engine::Evaluator::with_pfn)).
/// Cheap to clone.
#[derive(Debug, Clone)]
pub struct PfnCaller {
    registry: Arc<PfnRegistry>,
    user: String,
    limits: PfnLimits,
}

impl PfnCaller {
    /// `user` calls the functions of `registry`, with the registry's limits.
    pub fn new(registry: Arc<PfnRegistry>, user: impl Into<String>) -> PfnCaller {
        let limits = registry.limits();
        PfnCaller {
            registry,
            user: user.into(),
            limits,
        }
    }

    /// The same caller under `limits` (a database's
    /// [`QuerySettings::pfn`](crate::db::QuerySettings::pfn)).
    pub fn with_limits(mut self, limits: PfnLimits) -> PfnCaller {
        self.limits = limits;
        self
    }

    /// The same registry and limits, called as `user`.
    pub fn as_user(&self, user: impl Into<String>) -> PfnCaller {
        PfnCaller {
            registry: Arc::clone(&self.registry),
            user: user.into(),
            limits: self.limits,
        }
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
/// kept as is; invalid UTF-8 after decoding is replaced (U+FFFD).
pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => match (
                b.get(i + 1).copied().and_then(hex),
                b.get(i + 2).copied().and_then(hex),
            ) {
                (Some(high), Some(low)) => {
                    out.push((high << 4) | low);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
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
/// ignored; a space reads as `+`, which URL-decoding a `+` produces).
pub fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut padding = false;
    for (i, c) in s.bytes().enumerate() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' | b' ' => 62,
            b'/' | b'_' => 63,
            b'=' => {
                padding = true;
                continue;
            }
            b'\n' | b'\r' | b'\t' => continue,
            _ => return Err(format!("invalid base64 character at offset {i}")),
        };
        if padding {
            return Err(format!("base64 data after padding at offset {i}"));
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    if bits >= 6 {
        return Err("truncated base64 data".to_string());
    }
    Ok(out)
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
