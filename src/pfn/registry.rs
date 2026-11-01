//! [`PfnRegistry`]: every user's functions, on disk, as upstream's
//! `PFNUtil` (`src/Pfn/PFNUtil.cpp`) keeps them.
//!
//! # Layout (under `pfn_base_path`, default `<server root>/pfn`)
//!
//! ```text
//! src/<user>/data.json               one JSON object (PfnInfo) per line, upstream's data.json
//! src/<user>/<name lower-cased>.wat  the generated source (upstream: cpp/<user>/<name>.cpp)
//! lib/<user>/lib<name lower-cased><md5(lastTime)>.wasm
//!                                    the compiled module (upstream: lib<name><md5>.so)
//! ```
//!
//! Upstream keeps the C++ sources under `cpp/`; the WebAssembly sources live
//! under `src/` so the two never mix. Names are unique per user ignoring case
//! (the file name is lower-cased, as upstream's), and looked up exactly.
//!
//! # Operations (upstream semantics)
//!
//! * **create** — a new name (else "function name X already exists"):
//!   writes the source, status `"1"`.
//! * **update** — an existing name (else "… does not exist"): replaces every
//!   field with the request's (fields not sent become empty, as upstream),
//!   rewrites the source, status `"1"` — the function is not callable until
//!   it is compiled again.
//! * **delete** — removes the entry, the source and the module.
//! * **compile** — compiles the source file; status `"2"` and the module
//!   file on success, `"3"` and the compiler's message on failure. The
//!   compilation runs without the registry's lock (a function changed
//!   meanwhile is not stamped).
//! * **list** — the entries, filtered by exact `funName` and `funStatus`.
//! * **drop_user** — every function of a deleted user.
//!
//! Every stored field is bounded as it is stored, JSON-escaped
//! ([`PfnRegistry::check_definition`]), so a user's `data.json` is at most
//! about `pfn_max_functions` × (3 × `pfn_max_module_bytes` +
//! [`MAX_DESC_BYTES`]) — the 3 lets a definition within the limit be sent
//! URL-encoded.
//!
//! `data.json` is the record: a source or module file it does not name is
//! ignored (a write that failed half-way, or a crash, never blocks a name),
//! and the files it names are replaced only once it names their successors.
//!
//! `lastTime` is stamped on create, update and compile (upstream stamps it
//! on compile only and keeps whatever the client sent otherwise).
//!
//! Every mutation drops the cached module of the function, so a query never
//! runs a stale module after an update, a recompilation or a delete.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError, RwLock};

use super::{
    generate_source, source_kind, url_decode, valid_name, valid_user, CallOptions, GraphCtx,
    PfnInfo, PfnLimits, SourceKind, NAME_RULE, STATUS_COMPILED, STATUS_ERROR, STATUS_PENDING,
};
use crate::error::{GStoreError, Result};

/// The per-server store of every user's PFN functions (see the module docs).
pub struct PfnRegistry {
    base: PathBuf,
    /// Upstream's `pfn_data_lock`: readers list / look up, writers change a
    /// `data.json` and the files next to it.
    data_lock: RwLock<()>,
    limits: Mutex<PfnLimits>,
    #[cfg(feature = "pfn-wasm")]
    runtime: super::wasm::Runtime,
    /// Loaded modules by `(user, name)`, with the module file they came from.
    #[cfg(feature = "pfn-wasm")]
    cache: Mutex<HashMap<(String, String), (PathBuf, super::wasm::LoadedModule)>>,
    /// Bumped (under the cache's lock) whenever cached modules are dropped:
    /// a module loaded across a drop is not cached — its file may have been
    /// rewritten meanwhile under the same name (a recompilation stamped
    /// within the same second).
    generation: AtomicU64,
    #[cfg(not(feature = "pfn-wasm"))]
    cache: Mutex<HashMap<(String, String), ()>>,
    /// Tests: run once between loading a module and caching it.
    #[cfg(all(test, feature = "pfn-wasm"))]
    before_cache: Mutex<Option<BeforeCache>>,
}

/// A test hook of [`PfnRegistry`] (`before_cache`).
#[cfg(all(test, feature = "pfn-wasm"))]
type BeforeCache = Box<dyn FnOnce(&PfnRegistry) + Send>;

impl std::fmt::Debug for PfnRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PfnRegistry")
            .field("base", &self.base)
            .field("limits", &self.limits())
            .finish()
    }
}

/// A function looked up for calling ([`PfnRegistry::resolve`]): its loaded
/// module. Without the `pfn-wasm` runtime no function resolves.
#[derive(Clone)]
pub struct ResolvedFn {
    #[cfg(feature = "pfn-wasm")]
    name: String,
    #[cfg(feature = "pfn-wasm")]
    module: super::wasm::LoadedModule,
    #[cfg(not(feature = "pfn-wasm"))]
    _never: std::convert::Infallible,
}

impl std::fmt::Debug for ResolvedFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(feature = "pfn-wasm")]
        {
            f.debug_struct("ResolvedFn")
                .field("name", &self.name)
                .finish()
        }
        #[cfg(not(feature = "pfn-wasm"))]
        {
            let _ = f;
            match self._never {}
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn query_err(msg: impl Into<String>) -> GStoreError {
    GStoreError::Query(msg.into())
}

/// Remove a stale source or module file. The entry is already updated, so
/// a failure only leaves an unused file behind: logged (upstream logs the
/// files it removes, `SLOG_CORE`), not reported to the client.
fn remove_stale(f: &Path) {
    match fs::remove_file(f) {
        Ok(()) => crate::gs_debug!("PFN: removed '{}'", f.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => crate::gs_warn!("PFN: cannot remove the stale file '{}': {e}", f.display()),
    }
}

/// The longest `funDesc` stored, in bytes (as stored, JSON-escaped).
pub const MAX_DESC_BYTES: usize = 64 * 1024;

/// The bytes `s` takes in `data.json`: JSON-escaped (a control character
/// takes up to 6), without its quotes.
fn stored_len(s: &str) -> usize {
    s.bytes()
        .map(|b| match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0C => 2,
            b if b < 0x20 => 6,
            _ => 1,
        })
        .sum()
}

const SUBS_WITH_MODULE: &str = "funSubs must be empty when funBody is a complete module";

impl PfnRegistry {
    /// A registry rooted at `base` (created on first write), with the default
    /// [`PfnLimits`].
    pub fn new(base: impl Into<PathBuf>) -> PfnRegistry {
        PfnRegistry {
            base: base.into(),
            data_lock: RwLock::new(()),
            limits: Mutex::new(PfnLimits::default()),
            #[cfg(feature = "pfn-wasm")]
            runtime: super::wasm::Runtime::new(),
            cache: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            #[cfg(all(test, feature = "pfn-wasm"))]
            before_cache: Mutex::new(None),
        }
    }

    /// Builder form of [`set_limits`](Self::set_limits).
    pub fn with_limits(self, limits: PfnLimits) -> PfnRegistry {
        self.set_limits(limits);
        self
    }

    /// Install the limits create / update / compile check (module size,
    /// functions per user) and new [`PfnCaller`](super::PfnCaller)s start
    /// with.
    pub fn set_limits(&self, limits: PfnLimits) {
        *lock(&self.limits) = limits;
    }

    /// The installed limits.
    pub fn limits(&self) -> PfnLimits {
        *lock(&self.limits)
    }

    /// The base directory (`pfn_base_path`).
    pub fn base_dir(&self) -> &Path {
        &self.base
    }

    fn src_dir(&self, user: &str) -> PathBuf {
        self.base.join("src").join(user)
    }

    fn lib_dir(&self, user: &str) -> PathBuf {
        self.base.join("lib").join(user)
    }

    fn data_file(&self, user: &str) -> PathBuf {
        self.src_dir(user).join("data.json")
    }

    /// The generated source of `name` (`<name lower-cased>.wat`).
    pub fn source_file(&self, user: &str, name: &str) -> PathBuf {
        self.src_dir(user)
            .join(format!("{}.wat", name.to_lowercase()))
    }

    /// The compiled module of `name` stamped `last_time`
    /// (`lib<name lower-cased><md5(lastTime)>.wasm`).
    pub fn module_file(&self, user: &str, name: &str, last_time: &str) -> PathBuf {
        self.lib_dir(user).join(format!(
            "lib{}{}.wasm",
            name.to_lowercase(),
            crate::query::hash::md5_hex(last_time)
        ))
    }

    /// The compiled module files of `name` (any stamp).
    fn module_files(&self, user: &str, name: &str) -> Vec<PathBuf> {
        let prefix = format!("lib{}", name.to_lowercase());
        let Ok(rd) = fs::read_dir(self.lib_dir(user)) else {
            return Vec::new();
        };
        rd.filter_map(|e| {
            let e = e.ok()?;
            let file = e.file_name();
            let file = file.to_str()?;
            let stamp = file.strip_prefix(&prefix)?.strip_suffix(".wasm")?;
            (stamp.len() == 32 && stamp.bytes().all(|b| b.is_ascii_hexdigit())).then(|| e.path())
        })
        .collect()
    }

    fn check_user(user: &str) -> Result<()> {
        if valid_user(user) {
            Ok(())
        } else {
            Err(query_err(format!("invalid user name '{user}'")))
        }
    }

    fn check_name(name: &str) -> Result<()> {
        if valid_name(name) {
            Ok(())
        } else if name.is_empty() {
            Err(query_err("the value of funName can not be empty!"))
        } else {
            Err(query_err(format!("invalid funName '{name}': {NAME_RULE}")))
        }
    }

    /// `user`'s entries (no lock taken). A missing file is no entry; an
    /// unparsable line is skipped, as upstream's `addPFNInfo` does.
    fn read_all(&self, user: &str) -> Result<Vec<PfnInfo>> {
        let text = match fs::read_to_string(self.data_file(user)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        Ok(text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let info = crate::json::parse(l)
                    .ok()
                    .and_then(|j| PfnInfo::from_json(&j).ok());
                if info.is_none() {
                    // Upstream: `SLOG_ERROR("parse function info from json
                    // error: " + line)`.
                    crate::gs_error!(
                        "PFN: skipping an unreadable line of '{}': {l}",
                        self.data_file(user).display()
                    );
                }
                info
            })
            .filter(|i| !i.name.is_empty())
            .collect())
    }

    /// Replace `user`'s `data.json` (atomically).
    fn write_all(&self, user: &str, infos: &[PfnInfo]) -> Result<()> {
        let mut text = String::new();
        for i in infos {
            i.to_json().into_sorted().write_to(&mut text);
            text.push('\n');
        }
        fs::create_dir_all(self.src_dir(user))?;
        crate::db::fsutil::atomic_write(&self.data_file(user), text.as_bytes())
    }

    /// Drop the cached module of `(user, name)`.
    fn forget(&self, user: &str, name: &str) {
        let mut cache = lock(&self.cache);
        cache.remove(&(user.to_string(), name.to_string()));
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Check the definition `info` a client sends (create, update,
    /// `funreview`) and return its generated source: `funDesc` at most
    /// [`MAX_DESC_BYTES`], also as stored (JSON-escaped); `funBody` and
    /// `funSubs` together, URL-decoded and as sent (whitespace included), and
    /// the generated source each at most `pfn_max_module_bytes`, and
    /// `funBody` and `funSubs` as stored at most 3 × `pfn_max_module_bytes`;
    /// no `funSubs` next to a complete module. So every field stored in
    /// `data.json` is bounded.
    pub fn check_definition(&self, info: &PfnInfo) -> Result<String> {
        if info.desc.len() > MAX_DESC_BYTES {
            return Err(query_err(format!(
                "funDesc is {} bytes, more than the {MAX_DESC_BYTES} bytes allowed",
                info.desc.len()
            )));
        }
        let desc = stored_len(&info.desc);
        if desc > MAX_DESC_BYTES {
            return Err(query_err(format!(
                "funDesc takes {desc} bytes as stored (JSON-escaped), more than the \
                 {MAX_DESC_BYTES} bytes allowed"
            )));
        }
        let max = self.limits().max_module_bytes;
        let body = url_decode(&info.body);
        let subs = url_decode(&info.subs);
        let given = body.len().saturating_add(subs.len());
        if given > max {
            return Err(query_err(format!(
                "funBody and funSubs are {given} bytes, more than the {max} bytes allowed \
                 (pfn_max_module_bytes)"
            )));
        }
        let stored = stored_len(&info.body).saturating_add(stored_len(&info.subs));
        if stored > max.saturating_mul(3) {
            return Err(query_err(format!(
                "funBody and funSubs take {stored} bytes as stored (JSON-escaped), more than \
                 the {} bytes allowed (3 × pfn_max_module_bytes)",
                max.saturating_mul(3)
            )));
        }
        if source_kind(body.trim()) != SourceKind::Body && !subs.trim().is_empty() {
            return Err(query_err(SUBS_WITH_MODULE));
        }
        let src = generate_source(info);
        if src.len() > max {
            return Err(query_err(format!(
                "the function's source is {} bytes, more than the {max} bytes allowed \
                 (pfn_max_module_bytes)",
                src.len()
            )));
        }
        Ok(src)
    }

    /// Remove every function of `user` — entries, sources, modules and
    /// cached modules — when the user is deleted, so that an account later
    /// created under the name starts with none.
    pub fn drop_user(&self, user: &str) -> Result<()> {
        if !valid_user(user) {
            return Ok(());
        }
        let _g = self
            .data_lock
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        for dir in [self.src_dir(user), self.lib_dir(user)] {
            match fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut cache = lock(&self.cache);
        cache.retain(|(u, _), _| u != user);
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// `user`'s functions, filtered by exact `name` and `status` when given
    /// (upstream `fun_query`), in creation order.
    pub fn list(
        &self,
        user: &str,
        name: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<PfnInfo>> {
        if !valid_user(user) {
            return Ok(Vec::new());
        }
        let _g = self
            .data_lock
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        Ok(self
            .read_all(user)?
            .into_iter()
            .filter(|i| name.is_none_or(|n| i.name == n))
            .filter(|i| status.is_none_or(|s| i.status == s))
            .collect())
    }

    /// `user`'s function `name`.
    pub fn get(&self, user: &str, name: &str) -> Result<Option<PfnInfo>> {
        Ok(self.list(user, Some(name), None)?.into_iter().next())
    }

    /// Create function `info.name` for `user` (upstream `fun_create`).
    /// Returns the stored entry.
    pub fn create(&self, user: &str, mut info: PfnInfo) -> Result<PfnInfo> {
        Self::check_user(user)?;
        Self::check_name(&info.name)?;
        let src = self.check_definition(&info)?;
        let _g = self
            .data_lock
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let mut all = self.read_all(user)?;
        let lower = info.name.to_lowercase();
        // `data.json` decides (upstream also refuses a name whose source
        // file exists; here such a file, which no entry names, is replaced).
        if all.iter().any(|i| i.name.to_lowercase() == lower) {
            return Err(query_err(format!(
                "function name {} already exists",
                info.name
            )));
        }
        let max = self.limits().max_functions;
        if all.len() >= max {
            return Err(query_err(format!(
                "user {user} already has {} functions, the most allowed (pfn_max_functions = {max})",
                all.len()
            )));
        }
        fs::create_dir_all(self.src_dir(user))?;
        let source = self.source_file(user, &info.name);
        crate::db::fsutil::atomic_write(&source, src.as_bytes())?;
        info.status = STATUS_PENDING.to_string();
        info.last_time = super::now();
        all.push(info.clone());
        if let Err(e) = self.write_all(user, &all) {
            remove_stale(&source);
            return Err(e);
        }
        self.forget(user, &info.name);
        Ok(info)
    }

    /// Update function `info.name` of `user` (upstream `fun_update`).
    pub fn update(&self, user: &str, mut info: PfnInfo) -> Result<PfnInfo> {
        Self::check_user(user)?;
        Self::check_name(&info.name)?;
        let src = self.check_definition(&info)?;
        let _g = self
            .data_lock
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let mut all = self.read_all(user)?;
        let Some(slot) = all.iter_mut().find(|i| i.name == info.name) else {
            return Err(query_err(format!(
                "function name {} does not exist",
                info.name
            )));
        };
        let previous = generate_source(slot);
        let source = self.source_file(user, &info.name);
        crate::db::fsutil::atomic_write(&source, src.as_bytes())?;
        info.status = STATUS_PENDING.to_string();
        info.last_time = super::now();
        *slot = info.clone();
        if let Err(e) = self.write_all(user, &all) {
            // The entry is unchanged: so is its source (which a compilation
            // would build).
            let _ = crate::db::fsutil::atomic_write(&source, previous.as_bytes());
            return Err(e);
        }
        for f in self.module_files(user, &info.name) {
            remove_stale(&f);
        }
        self.forget(user, &info.name);
        Ok(info)
    }

    /// Delete function `name` of `user` (upstream `fun_delete`).
    pub fn delete(&self, user: &str, name: &str) -> Result<()> {
        Self::check_user(user)?;
        Self::check_name(name)?;
        let _g = self
            .data_lock
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let mut all = self.read_all(user)?;
        let before = all.len();
        all.retain(|i| i.name != name);
        if all.len() == before {
            return Err(query_err(format!("function {name} not exists")));
        }
        self.write_all(user, &all)?;
        remove_stale(&self.source_file(user, name));
        for f in self.module_files(user, name) {
            remove_stale(&f);
        }
        self.forget(user, name);
        Ok(())
    }

    /// Compile function `name` of `user` (upstream `fun_build`). `Ok(Ok(info))`
    /// on success; `Ok(Err(message))` when the source does not compile (the
    /// function's status is then `"3"`); `Err` when the function does not
    /// exist (upstream's messages: its source file is looked for first), was
    /// changed while it compiled, or this build has no runtime (status
    /// unchanged). The compilation itself runs without the registry's lock,
    /// so other users' lookups never wait for it.
    pub fn compile(&self, user: &str, name: &str) -> Result<std::result::Result<PfnInfo, String>> {
        Self::check_user(user)?;
        Self::check_name(name)?;
        let (compiled, source) = {
            let _g = self
                .data_lock
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            let source = match fs::read_to_string(self.source_file(user, name)) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Upstream's text (sic).
                    return Err(query_err("Function source file dose not exist"));
                }
                Err(e) => return Err(e.into()),
            };
            let Some(info) = self.read_all(user)?.into_iter().find(|i| i.name == name) else {
                return Err(query_err(format!("function {name} not exists")));
            };
            (info, source)
        };
        let outcome = self.compile_source(&compiled, &source);
        #[cfg(not(feature = "pfn-wasm"))]
        if let Err(GStoreError::Query(m)) = &outcome {
            if m.as_str() == super::FEATURE_HINT {
                return Err(query_err(super::FEATURE_HINT));
            }
        }
        let _g = self
            .data_lock
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let mut all = self.read_all(user)?;
        let Some(idx) = all.iter().position(|i| i.name == name) else {
            return Err(query_err(format!("function {name} not exists")));
        };
        if all[idx] != compiled {
            return Err(query_err(format!(
                "function {name} was changed while it was being compiled; compile it again"
            )));
        }
        let previous = self.module_file(user, name, &all[idx].last_time);
        let info = &mut all[idx];
        info.last_time = super::now();
        let mut written = None;
        let result = match outcome {
            Ok(bytes) => {
                fs::create_dir_all(self.lib_dir(user))?;
                let target = self.module_file(user, name, &info.last_time);
                crate::db::fsutil::atomic_write(&target, &bytes)?;
                written = Some(target);
                info.status = STATUS_COMPILED.to_string();
                Ok(info.clone())
            }
            Err(e) => {
                info.status = STATUS_ERROR.to_string();
                Err(match e {
                    GStoreError::Query(m) => m,
                    other => other.to_string(),
                })
            }
        };
        if let Err(e) = self.write_all(user, &all) {
            // `data.json` still names the previous module: it stays.
            if let Some(target) = written.filter(|t| *t != previous) {
                remove_stale(&target);
            }
            return Err(e);
        }
        // Only now that `data.json` names the new module do the others go.
        if let Some(target) = &written {
            for f in self.module_files(user, name) {
                if f != *target {
                    remove_stale(&f);
                }
            }
        }
        self.forget(user, name);
        Ok(result)
    }

    /// Compile `source` (the source file of `info`): the module bytes, or the
    /// error (a [`GStoreError::Query`] with the compiler's message).
    fn compile_source(&self, info: &PfnInfo, source: &str) -> Result<Vec<u8>> {
        let body = url_decode(&info.body);
        if source_kind(body.trim()) != SourceKind::Body && !url_decode(&info.subs).trim().is_empty()
        {
            return Err(query_err(SUBS_WITH_MODULE));
        }
        #[cfg(feature = "pfn-wasm")]
        {
            self.runtime
                .compile(&info.name, source, &self.limits())
                .map_err(query_err)
        }
        #[cfg(not(feature = "pfn-wasm"))]
        {
            let _ = source;
            Err(query_err(super::FEATURE_HINT))
        }
    }

    /// Run function `name` of `user` once (upstream `getPfnSoFile` +
    /// `excutePfnSoFile`): it must exist and be compiled.
    pub fn call(
        &self,
        user: &str,
        name: &str,
        params: &str,
        graph: &dyn GraphCtx,
        opts: &CallOptions<'_>,
    ) -> Result<String> {
        let f = self.resolve(user, name)?;
        self.run(&f, params, graph, opts)
    }

    /// Look function `name` of `user` up for calling (upstream
    /// `getPfnSoFile`): it must exist and be compiled; its module is loaded
    /// (or taken from the cache). A query resolves a function once and runs
    /// it for every argument combination ([`run`](Self::run)).
    pub fn resolve(&self, user: &str, name: &str) -> Result<ResolvedFn> {
        let missing = || query_err(format!("abort function '{name}' is not exist"));
        if !valid_user(user) || !valid_name(name) {
            return Err(missing());
        }
        let info = {
            let _g = self
                .data_lock
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            self.read_all(user)?
                .into_iter()
                .find(|i| i.name == name)
                .ok_or_else(missing)?
        };
        #[cfg(not(feature = "pfn-wasm"))]
        {
            let _ = info;
            Err(query_err(super::FEATURE_HINT))
        }
        #[cfg(feature = "pfn-wasm")]
        {
            if info.status != STATUS_COMPILED {
                return Err(query_err(format!(
                    "abort function '{name}' not compile yet"
                )));
            }
            Ok(ResolvedFn {
                name: name.to_string(),
                module: self.module(user, name, &info.last_time)?,
            })
        }
    }

    /// Run a [resolved](Self::resolve) function once with argument text
    /// `params` (upstream `excutePfnSoFile`).
    pub fn run(
        &self,
        f: &ResolvedFn,
        params: &str,
        graph: &dyn GraphCtx,
        opts: &CallOptions<'_>,
    ) -> Result<String> {
        #[cfg(not(feature = "pfn-wasm"))]
        {
            let _ = (f, params, graph, opts);
            Err(query_err(super::FEATURE_HINT))
        }
        #[cfg(feature = "pfn-wasm")]
        {
            self.runtime.run(&f.module, &f.name, params, graph, opts)
        }
    }

    /// The loaded module of `(user, name)` stamped `last_time`, from the
    /// cache or its file.
    #[cfg(feature = "pfn-wasm")]
    fn module(&self, user: &str, name: &str, last_time: &str) -> Result<super::wasm::LoadedModule> {
        let file = self.module_file(user, name, last_time);
        let key = (user.to_string(), name.to_string());
        let generation = {
            let cache = lock(&self.cache);
            if let Some((cached_file, m)) = cache.get(&key) {
                if *cached_file == file {
                    return Ok(m.clone());
                }
            }
            self.generation.load(Ordering::SeqCst)
        };
        let bytes = fs::read(&file).map_err(|e| {
            query_err(format!(
                "abort load module file error: {} ({e})",
                file.display()
            ))
        })?;
        let (module, fresh_engine) = self
            .runtime
            .load(name, &bytes)
            .map_err(|e| query_err(format!("abort load module file error: {e}")))?;
        #[cfg(test)]
        if let Some(f) = lock(&self.before_cache).take() {
            f(self);
        }
        let mut cache = lock(&self.cache);
        if fresh_engine {
            // The modules of the replaced engine: dropped so it is freed (a
            // call still running keeps its module, and so the engine, until
            // it ends); they load into the new engine when called again.
            cache.clear();
        }
        // Not when a function was changed meanwhile (this call still runs
        // what it loaded, as a call that began before the change).
        if self.generation.load(Ordering::SeqCst) == generation {
            cache.insert(key, (file, module.clone()));
        }
        Ok(module)
    }

    /// How many modules are loaded (tests).
    #[cfg(all(test, feature = "pfn-wasm"))]
    pub(crate) fn cached_modules(&self) -> usize {
        lock(&self.cache).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "gstore_pfn_reg_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn info(name: &str, body: &str) -> PfnInfo {
        PfnInfo {
            name: name.into(),
            desc: format!("{name} desc"),
            body: body.into(),
            ..PfnInfo::default()
        }
    }

    #[test]
    fn lifecycle_keeps_upstream_metadata() {
        let base = tmp("life");
        let r = PfnRegistry::new(&base);
        let a = r.create("alice", info("deg", "(i32.const 1)")).unwrap();
        assert_eq!(a.status, STATUS_PENDING);
        assert!(!a.last_time.is_empty());
        assert!(base.join("src/alice/data.json").is_file());
        assert!(base.join("src/alice/deg.wat").is_file());
        let e = r.create("alice", info("DEG", "x")).unwrap_err().to_string();
        assert!(e.contains("already exists"), "{e}");
        r.create("alice", info("other", "(i32.const 1)")).unwrap();
        r.create("bob", info("deg", "(i32.const 1)")).unwrap();
        assert_eq!(r.list("alice", None, None).unwrap().len(), 2);
        assert_eq!(r.list("alice", Some("deg"), None).unwrap().len(), 1);
        assert_eq!(r.list("alice", Some("de"), None).unwrap().len(), 0);
        assert_eq!(r.list("alice", None, Some("2")).unwrap().len(), 0);
        assert_eq!(r.list("carol", None, None).unwrap().len(), 0);
        // Update replaces every field.
        let u = r
            .update(
                "alice",
                PfnInfo {
                    name: "deg".into(),
                    body: "(i32.const 2)".into(),
                    ..PfnInfo::default()
                },
            )
            .unwrap();
        assert_eq!(u.desc, "");
        assert_eq!(
            r.get("alice", "deg").unwrap().unwrap().body,
            "(i32.const 2)"
        );
        let e = r
            .update("alice", info("nope", "x"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("does not exist"), "{e}");
        // data.json is upstream's: one sorted object per line.
        let text = fs::read_to_string(base.join("src/alice/data.json")).unwrap();
        assert!(
            text.lines().all(|l| l.starts_with("{\"funBody\":")),
            "{text}"
        );
        r.delete("alice", "deg").unwrap();
        assert!(!base.join("src/alice/deg.wat").exists());
        assert!(r.get("alice", "deg").unwrap().is_none());
        assert!(r
            .delete("alice", "deg")
            .unwrap_err()
            .to_string()
            .contains("not exists"));
        assert_eq!(r.list("bob", None, None).unwrap().len(), 1, "per-user");
        let e = r
            .create("alice", info("bad-name", "x"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("invalid funName"), "{e}");
        assert!(r.create("../x", info("f", "x")).is_err());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn limits_bound_functions_and_sources() {
        let base = tmp("limits");
        let r = PfnRegistry::new(&base).with_limits(PfnLimits {
            max_functions: 1,
            max_module_bytes: 4096,
            ..PfnLimits::default()
        });
        r.create("u", info("a", "(i32.const 1)")).unwrap();
        let e = r
            .create("u", info("b", "(i32.const 1)"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("pfn_max_functions"), "{e}");
        r.delete("u", "a").unwrap();
        let e = r
            .create("u", info("big", &"(nop)".repeat(2000)))
            .unwrap_err()
            .to_string();
        assert!(e.contains("pfn_max_module_bytes"), "{e}");
        // Every stored field is bounded, not only the generated source
        // (which leaves funDesc out, trims, and drops funSubs next to a
        // complete module).
        let mut big_desc = info("d", "(i32.const 1)");
        big_desc.desc = "x".repeat(MAX_DESC_BYTES + 1);
        let e = r.create("u", big_desc).unwrap_err().to_string();
        assert!(e.contains("funDesc"), "{e}");
        let padded = info("p", &format!("(i32.const 1){}", " ".repeat(5000)));
        let e = r.create("u", padded).unwrap_err().to_string();
        assert!(e.contains("pfn_max_module_bytes"), "{e}");
        let mut subs = info("s", "(module)");
        subs.subs = "x".repeat(5000);
        assert!(r.create("u", subs).is_err());
        r.create("u", info("ok", "(i32.const 1)")).unwrap();
        let mut grown = info("ok", "(i32.const 1)");
        grown.desc = "x".repeat(MAX_DESC_BYTES + 1);
        assert!(r.update("u", grown).is_err());
        let text = fs::read_to_string(base.join("src/u/data.json")).unwrap();
        assert!(text.len() < 1000, "{}", text.len());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn dropping_a_user_removes_their_functions() {
        let base = tmp("dropuser");
        let r = PfnRegistry::new(&base);
        r.create("alice", info("f", "(i32.const 1)")).unwrap();
        r.create("bob", info("f", "(i32.const 1)")).unwrap();
        r.drop_user("alice").unwrap();
        assert!(r.list("alice", None, None).unwrap().is_empty());
        assert!(!base.join("src/alice").exists());
        assert_eq!(r.list("bob", None, None).unwrap().len(), 1);
        // Idempotent, and never outside the base directory.
        r.drop_user("alice").unwrap();
        r.drop_user("../bob").unwrap();
        assert_eq!(r.list("bob", None, None).unwrap().len(), 1);
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(not(feature = "pfn-wasm"))]
    #[test]
    fn without_the_runtime_compile_and_call_name_the_feature() {
        let base = tmp("nofeature");
        let r = PfnRegistry::new(&base);
        r.create("u", info("f", "(i32.const 1)")).unwrap();
        let e = r.compile("u", "f").unwrap_err().to_string();
        assert!(e.contains("pfn-wasm"), "{e}");
        assert_eq!(r.get("u", "f").unwrap().unwrap().status, STATUS_PENDING);
        let db = crate::Database::build_from_str("pfn_nf", "<http://a> <http://p> <http://b> .\n")
            .unwrap();
        let csr = db.csr().unwrap();
        let g = super::super::CsrGraphCtx::new(&csr, db.dict());
        let opts = CallOptions {
            limits: PfnLimits::default(),
            cancel: None,
            max_output: 100,
        };
        let e = r.call("u", "f", "{}", &g, &opts).unwrap_err().to_string();
        assert!(e.contains("pfn-wasm"), "{e}");
        let e = r.call("u", "g", "{}", &g, &opts).unwrap_err().to_string();
        assert!(e.contains("is not exist"), "{e}");
        let _ = fs::remove_dir_all(&base);
    }

    /// Finding: the stored fields were bounded before JSON escaping (a
    /// control character takes 6 bytes in `data.json`), so `data.json`
    /// grew about 6× past its documented bound.
    #[test]
    fn stored_fields_are_bounded_as_stored() {
        let base = tmp("bound");
        let max = 10_000;
        let r = PfnRegistry::new(&base).with_limits(PfnLimits {
            max_module_bytes: max,
            ..PfnLimits::default()
        });
        // 6008 bytes decoded, about 36 KB stored.
        let body = format!("(module){}", "\u{1}".repeat(6000));
        let e = r.create("alice", info("f", &body)).unwrap_err().to_string();
        assert!(e.contains("stored"), "{e}");
        // 20 KB of funDesc, about 120 KB stored.
        let mut i = info("g", "(i32.const 1)");
        i.desc = "\u{1}".repeat(20_000);
        let e = r.create("alice", i).unwrap_err().to_string();
        assert!(e.contains("funDesc"), "{e}");
        // A URL-encoded definition (3 bytes a character) within the limit
        // decoded is still accepted.
        let encoded = format!("(i32.const 1){}", "%20".repeat(max / 2));
        r.create("alice", info("h", &encoded)).unwrap();
        // Whatever is accepted, an entry stays within its documented bound.
        let stored = fs::metadata(base.join("src/alice/data.json"))
            .unwrap()
            .len();
        assert!(
            stored as usize <= 3 * max + MAX_DESC_BYTES + 4096,
            "{stored}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// Finding: create wrote the source before `data.json` and refused a
    /// name whose source existed; a failed `data.json` write (or a source
    /// left by a delete) then blocked the name for good.
    #[test]
    fn a_failed_write_never_blocks_a_name() {
        let base = tmp("block");
        let r = PfnRegistry::new(&base);
        let blocker = base.join("src/alice/data.json.tmp");
        fs::create_dir_all(&blocker).unwrap();
        assert!(r.create("alice", info("deg", "(i32.const 1)")).is_err());
        fs::remove_dir_all(&blocker).unwrap();
        assert!(r.list("alice", None, None).unwrap().is_empty());
        r.create("alice", info("deg", "(i32.const 1)")).unwrap();
        // A source a delete left behind does not block the name either.
        r.delete("alice", "deg").unwrap();
        fs::write(r.source_file("alice", "deg"), "stray").unwrap();
        r.create("alice", info("DEG", "(i32.const 1)")).unwrap();
        assert_eq!(r.list("alice", None, None).unwrap().len(), 1);
        let _ = fs::remove_dir_all(&base);
    }

    /// Finding: compile removed the module `data.json` named before it
    /// rewrote `data.json`; when that write failed, the function (still
    /// compiled there) could not be loaded after a restart.
    #[cfg(feature = "pfn-wasm")]
    #[test]
    fn a_failed_compile_keeps_the_module_data_json_names() {
        let base = tmp("keepmod");
        let r = PfnRegistry::new(&base);
        let db = crate::Database::build_from_str("pfn_k", "<http://a> <http://p> <http://b> .\n")
            .unwrap();
        let csr = db.csr().unwrap();
        let g = super::super::CsrGraphCtx::new(&csr, db.dict());
        let opts = CallOptions {
            limits: PfnLimits::default(),
            cancel: None,
            max_output: 100,
        };
        r.create(
            "u",
            info("f", "(call $emit_i64 (i64.const 7)) (i32.const 1)"),
        )
        .unwrap();
        let first = r.compile("u", "f").unwrap().unwrap();
        // The next compilation is stamped with another second.
        while super::super::now() == first.last_time {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let blocker = base.join("src/u/data.json.tmp");
        fs::create_dir_all(&blocker).unwrap();
        assert!(r.compile("u", "f").is_err());
        fs::remove_dir_all(&blocker).unwrap();
        assert_eq!(r.module_files("u", "f").len(), 1);
        // After a restart the function still runs.
        let restarted = PfnRegistry::new(&base);
        assert_eq!(restarted.call("u", "f", "{}", &g, &opts).unwrap(), "7");
        let _ = fs::remove_dir_all(&base);
    }

    /// Finding: a module loaded while the function was updated and
    /// recompiled within the same second (so into the same file name) was
    /// cached after the recompilation dropped the cache, and served from
    /// then on.
    #[cfg(feature = "pfn-wasm")]
    #[test]
    fn a_module_loaded_across_a_recompilation_is_not_cached() {
        let base = tmp("stale");
        let r = PfnRegistry::new(&base);
        let db = crate::Database::build_from_str("pfn_s", "<http://a> <http://p> <http://b> .\n")
            .unwrap();
        let csr = db.csr().unwrap();
        let g = super::super::CsrGraphCtx::new(&csr, db.dict());
        let opts = CallOptions {
            limits: PfnLimits::default(),
            cancel: None,
            max_output: 100,
        };
        let emit = |n: i64| format!("(call $emit_i64 (i64.const {n})) (i32.const 1)");
        r.create("u", info("f", &emit(7))).unwrap();
        // The update and both compilations must share their second: retry
        // until they do.
        for attempt in 0..20 {
            r.update("u", info("f", &emit(7))).unwrap();
            let before = r.compile("u", "f").unwrap().unwrap().last_time;
            let emit8 = emit(8);
            *lock(&r.before_cache) = Some(Box::new(move |r: &PfnRegistry| {
                r.update("u", info("f", &emit8)).unwrap();
                r.compile("u", "f").unwrap().unwrap();
            }));
            // This call loaded the old module; the recompilation ran
            // before it was cached.
            assert_eq!(r.call("u", "f", "{}", &g, &opts).unwrap(), "7");
            let after = r.get("u", "f").unwrap().unwrap().last_time;
            if after != before {
                assert!(attempt < 19, "never within one second");
                continue;
            }
            assert_eq!(r.call("u", "f", "{}", &g, &opts).unwrap(), "8");
            break;
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(feature = "pfn-wasm")]
    #[test]
    fn compile_stamps_status_and_modules_and_drops_the_cache() {
        let base = tmp("compile");
        let r = PfnRegistry::new(&base);
        let db = crate::Database::build_from_str("pfn_c", "<http://a> <http://p> <http://b> .\n")
            .unwrap();
        let csr = db.csr().unwrap();
        let g = super::super::CsrGraphCtx::new(&csr, db.dict());
        let opts = CallOptions {
            limits: PfnLimits::default(),
            cancel: None,
            max_output: 100,
        };
        r.create(
            "u",
            info("f", "(call $emit_i64 (i64.const 7)) (i32.const 1)"),
        )
        .unwrap();
        let e = r.call("u", "f", "{}", &g, &opts).unwrap_err().to_string();
        assert!(e.contains("not compile yet"), "{e}");
        let ok = r.compile("u", "f").unwrap().unwrap();
        assert_eq!(ok.status, STATUS_COMPILED);
        assert_eq!(r.module_files("u", "f").len(), 1);
        assert_eq!(r.call("u", "f", "{}", &g, &opts).unwrap(), "7");
        assert_eq!(r.cached_modules(), 1);
        // A failing recompilation: status 3, the message, the cache dropped.
        r.update("u", info("f", "(i64.const 1)")).unwrap();
        assert_eq!(r.cached_modules(), 0);
        assert!(r.module_files("u", "f").is_empty());
        let msg = r.compile("u", "f").unwrap().unwrap_err();
        assert!(msg.contains("type mismatch"), "{msg}");
        assert_eq!(r.get("u", "f").unwrap().unwrap().status, STATUS_ERROR);
        assert!(r
            .call("u", "f", "{}", &g, &opts)
            .unwrap_err()
            .to_string()
            .contains("not compile yet"));
        // Fixed and recompiled: the new module runs.
        r.update(
            "u",
            info("f", "(call $emit_i64 (i64.const 8)) (i32.const 1)"),
        )
        .unwrap();
        r.compile("u", "f").unwrap().unwrap();
        assert_eq!(r.call("u", "f", "{}", &g, &opts).unwrap(), "8");
        // A complete module with funSubs is refused (when it is stored).
        let mut m = info("m", "(module)");
        m.subs = "(global i32 (i32.const 0))".into();
        let e = r.create("u", m).unwrap_err().to_string();
        assert!(e.contains("funSubs must be empty"), "{e}");
        r.delete("u", "f").unwrap();
        assert!(r.module_files("u", "f").is_empty());
        let _ = fs::remove_dir_all(&base);
    }
}
