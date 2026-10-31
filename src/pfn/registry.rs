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
//!   file on success, `"3"` and the compiler's message on failure.
//! * **list** — the entries, filtered by exact `funName` and `funStatus`.
//!
//! `lastTime` is stamped on create, update and compile (upstream stamps it
//! on compile only and keeps whatever the client sent otherwise).
//!
//! Every mutation drops the cached module of the function, so a query never
//! runs a stale module after an update, a recompilation or a delete.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
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
    #[cfg(not(feature = "pfn-wasm"))]
    cache: Mutex<HashMap<(String, String), ()>>,
}

impl std::fmt::Debug for PfnRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PfnRegistry")
            .field("base", &self.base)
            .field("limits", &self.limits())
            .finish()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn query_err(msg: impl Into<String>) -> GStoreError {
    GStoreError::Query(msg.into())
}

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
            .filter_map(|l| crate::json::parse(l).ok())
            .filter_map(|j| PfnInfo::from_json(&j).ok())
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
        lock(&self.cache).remove(&(user.to_string(), name.to_string()));
    }

    fn check_source_size(&self, info: &PfnInfo) -> Result<String> {
        let src = generate_source(info);
        let max = self.limits().max_module_bytes;
        if src.len() > max {
            return Err(query_err(format!(
                "the function's source is {} bytes, more than the {max} bytes allowed \
                 (pfn_max_module_bytes)",
                src.len()
            )));
        }
        Ok(src)
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
        let src = self.check_source_size(&info)?;
        let _g = self
            .data_lock
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let mut all = self.read_all(user)?;
        let lower = info.name.to_lowercase();
        if self.source_file(user, &info.name).exists()
            || all.iter().any(|i| i.name.to_lowercase() == lower)
        {
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
        crate::db::fsutil::atomic_write(&self.source_file(user, &info.name), src.as_bytes())?;
        info.status = STATUS_PENDING.to_string();
        info.last_time = super::now();
        all.push(info.clone());
        self.write_all(user, &all)?;
        self.forget(user, &info.name);
        Ok(info)
    }

    /// Update function `info.name` of `user` (upstream `fun_update`).
    pub fn update(&self, user: &str, mut info: PfnInfo) -> Result<PfnInfo> {
        Self::check_user(user)?;
        Self::check_name(&info.name)?;
        let src = self.check_source_size(&info)?;
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
        crate::db::fsutil::atomic_write(&self.source_file(user, &info.name), src.as_bytes())?;
        info.status = STATUS_PENDING.to_string();
        info.last_time = super::now();
        *slot = info.clone();
        self.write_all(user, &all)?;
        for f in self.module_files(user, &info.name) {
            let _ = fs::remove_file(f);
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
        let _ = fs::remove_file(self.source_file(user, name));
        for f in self.module_files(user, name) {
            let _ = fs::remove_file(f);
        }
        self.forget(user, name);
        Ok(())
    }

    /// Compile function `name` of `user` (upstream `fun_build`). `Ok(Ok(info))`
    /// on success; `Ok(Err(message))` when the source does not compile (the
    /// function's status is then `"3"`); `Err` when the function does not
    /// exist, or this build has no runtime (status unchanged).
    pub fn compile(&self, user: &str, name: &str) -> Result<std::result::Result<PfnInfo, String>> {
        Self::check_user(user)?;
        Self::check_name(name)?;
        let _g = self
            .data_lock
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let mut all = self.read_all(user)?;
        let Some(idx) = all.iter().position(|i| i.name == name) else {
            return Err(query_err(format!("function {name} not exists")));
        };
        let source = match fs::read_to_string(self.source_file(user, name)) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(query_err("Function source file does not exist"))
            }
            Err(e) => return Err(e.into()),
        };
        let outcome = self.compile_source(&all[idx], &source);
        #[cfg(not(feature = "pfn-wasm"))]
        if let Err(GStoreError::Query(m)) = &outcome {
            if m.as_str() == super::FEATURE_HINT {
                return Err(query_err(super::FEATURE_HINT));
            }
        }
        let info = &mut all[idx];
        info.last_time = super::now();
        let result = match outcome {
            Ok(bytes) => {
                fs::create_dir_all(self.lib_dir(user))?;
                let target = self.module_file(user, name, &info.last_time);
                crate::db::fsutil::atomic_write(&target, &bytes)?;
                for f in self.module_files(user, name) {
                    if f != target {
                        let _ = fs::remove_file(f);
                    }
                }
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
        self.write_all(user, &all)?;
        self.forget(user, name);
        Ok(result)
    }

    /// Compile `source` (the source file of `info`): the module bytes, or the
    /// error (a [`GStoreError::Query`] with the compiler's message).
    fn compile_source(&self, info: &PfnInfo, source: &str) -> Result<Vec<u8>> {
        let body = url_decode(&info.body);
        if source_kind(body.trim()) != SourceKind::Body && !url_decode(&info.subs).trim().is_empty()
        {
            return Err(query_err(
                "funSubs must be empty when funBody is a complete module",
            ));
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
            let _ = (info, params, graph, opts);
            Err(query_err(super::FEATURE_HINT))
        }
        #[cfg(feature = "pfn-wasm")]
        {
            if info.status != STATUS_COMPILED {
                return Err(query_err(format!(
                    "abort function '{name}' not compile yet"
                )));
            }
            let module = self.module(user, name, &info.last_time)?;
            self.runtime.run(&module, name, params, graph, opts)
        }
    }

    /// The loaded module of `(user, name)` stamped `last_time`, from the
    /// cache or its file.
    #[cfg(feature = "pfn-wasm")]
    fn module(&self, user: &str, name: &str, last_time: &str) -> Result<super::wasm::LoadedModule> {
        let file = self.module_file(user, name, last_time);
        let key = (user.to_string(), name.to_string());
        if let Some((cached_file, m)) = lock(&self.cache).get(&key) {
            if *cached_file == file {
                return Ok(m.clone());
            }
        }
        let bytes = fs::read(&file).map_err(|e| {
            query_err(format!(
                "abort load module file error: {} ({e})",
                file.display()
            ))
        })?;
        let module = self
            .runtime
            .load(name, &bytes)
            .map_err(|e| query_err(format!("abort load module file error: {e}")))?;
        lock(&self.cache).insert(key, (file, module.clone()));
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
        // A complete module with funSubs is refused.
        let mut m = info("m", "(module)");
        m.subs = "(global i32 (i32.const 0))".into();
        r.create("u", m).unwrap();
        let msg = r.compile("u", "m").unwrap().unwrap_err();
        assert!(msg.contains("funSubs must be empty"), "{msg}");
        r.delete("u", "f").unwrap();
        assert!(r.module_files("u", "f").is_empty());
        let _ = fs::remove_dir_all(&base);
    }
}
