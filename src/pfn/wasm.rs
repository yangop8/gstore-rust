//! The sandboxed WebAssembly runtime of PFN functions (feature `pfn-wasm`):
//! the `wasm` interpreter crate `wasmi` plus the `wat` text parser.
//!
//! * **Compile** ([`Runtime::compile`]): WAT (or a base64 binary module) →
//!   module bytes, validated by wasmi, with only the `"gstore"` host
//!   functions as imports (exact signatures, [`abi::HOST_FUNCTIONS`]), an
//!   exported `memory` and the function exported under its name with type
//!   `() -> i32`. Start functions are refused. Sizes are bounded by
//!   [`PfnLimits::max_module_bytes`].
//! * **Run** ([`Runtime::run`]): a fresh store and instance per call (no
//!   state survives a call), memory capped at
//!   [`PfnLimits::max_memory_pages`] (a `memory.grow` past it returns `-1`,
//!   as WebAssembly specifies), at most one table of
//!   [`MAX_TABLE_ELEMENTS`], and wasmi *fuel* metering: the call runs in
//!   slices of [`FUEL_SLICE`] instructions; between slices the query's
//!   cancel flag is checked and the [`PfnLimits::fuel_per_call`] budget
//!   enforced. A host function is charged fuel for the guest bytes it reads
//!   or writes (one unit per [`HOST_BYTES_PER_FUEL`]) and checks the cancel
//!   flag itself, so a call into the host costs what its work costs. Traps
//!   (`unreachable`, an out-of-bounds access, a stack overflow, a host
//!   function refusing a bad pointer, an output past
//!   `gfun_max_output_bytes`) become query errors.
//! * **Engines.** wasmi keeps what a module registers in its engine until the
//!   engine itself is dropped. Compiling validates in a throw-away engine;
//!   modules are loaded into a shared engine that is replaced by a fresh one
//!   once [`ENGINE_RECYCLE_BYTES`] of modules were loaded into it (the
//!   registry then drops its cached modules, freeing the old engine when the
//!   last running call ends). wasmi's strict [`EnforcedLimits`] bound what one
//!   module may declare (10 000 functions, …).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use wasmi::{
    Caller, Config, EnforcedLimits, Engine, Error as WasmError, Extern, ExternType, Linker, Memory,
    Module, Store, StoreLimits, StoreLimitsBuilder, TypedResumableCall, ValType as WasmVal,
};

use super::abi::{self, ValType};
use super::graph::GraphCtx;
use super::{CallOptions, PfnLimits};
use crate::error::{GStoreError, Result};
use crate::json::Json;
use crate::model::id::PredId;

/// Instructions run between two cancel / budget checks.
pub const FUEL_SLICE: u64 = 1 << 20;

/// The most table elements a module may have.
pub const MAX_TABLE_ELEMENTS: usize = 10_000;

/// Guest bytes a host function may read or write per unit of fuel it is
/// charged (about the cost of an instruction).
pub const HOST_BYTES_PER_FUEL: u64 = 8;

/// The bytes of modules loaded into one engine before loads move to a fresh
/// one (see the module docs).
pub const ENGINE_RECYCLE_BYTES: usize = 16 << 20;

/// A compiled, validated module.
pub(crate) type LoadedModule = Module;

/// The PFN WebAssembly runtime (one per registry; see the module docs).
pub(crate) struct Runtime {
    config: Config,
    current: Mutex<EngineSlot>,
}

/// The engine modules are loaded into, and how many module bytes were.
struct EngineSlot {
    engine: Engine,
    loaded: usize,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Runtime(wasmi)")
    }
}

impl Runtime {
    pub(crate) fn new() -> Runtime {
        let mut config = Config::default();
        config
            .consume_fuel(true)
            .allow_start_fn(false)
            .enforced_limits(EnforcedLimits::strict());
        let engine = Engine::new(&config);
        Runtime {
            config,
            current: Mutex::new(EngineSlot { engine, loaded: 0 }),
        }
    }

    /// Compile function `name`'s generated source into module bytes, or the
    /// error text (what upstream returns from `g++`).
    pub(crate) fn compile(
        &self,
        name: &str,
        source: &str,
        limits: &PfnLimits,
    ) -> std::result::Result<Vec<u8>, String> {
        let source = source.trim();
        if source.len() > limits.max_module_bytes {
            return Err(format!(
                "the source is {} bytes, more than the {} bytes allowed (pfn_max_module_bytes)",
                source.len(),
                limits.max_module_bytes
            ));
        }
        let bytes = match source.strip_prefix(super::BASE64_PREFIX) {
            Some(b64) => {
                super::base64_decode(b64).map_err(|e| format!("bad base64 module: {e}"))?
            }
            None => wat::parse_str(source).map_err(|e| e.to_string())?,
        };
        if bytes.len() > limits.max_module_bytes {
            return Err(format!(
                "the module is {} bytes, more than the {} bytes allowed (pfn_max_module_bytes)",
                bytes.len(),
                limits.max_module_bytes
            ));
        }
        // Validated in an engine of its own, dropped (with everything the
        // module registered in it) right after.
        validate(&Engine::new(&self.config), name, &bytes)?;
        Ok(bytes)
    }

    /// Validate `bytes` and check the PFN interface; the module to run,
    /// loaded into the shared engine — a fresh one when the current one had
    /// [`ENGINE_RECYCLE_BYTES`] loaded into it (`true` then: drop the
    /// modules loaded before, so the old engine is freed).
    pub(crate) fn load(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> std::result::Result<(Module, bool), String> {
        let mut slot = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh =
            slot.loaded > 0 && slot.loaded.saturating_add(bytes.len()) > ENGINE_RECYCLE_BYTES;
        if fresh {
            *slot = EngineSlot {
                engine: Engine::new(&self.config),
                loaded: 0,
            };
        }
        // A module that fails validation registers in the engine too.
        slot.loaded = slot.loaded.saturating_add(bytes.len());
        Ok((validate(&slot.engine, name, bytes)?, fresh))
    }

    /// Call function `name` of `module` once with argument text `params`.
    pub(crate) fn run(
        &self,
        module: &Module,
        name: &str,
        params: &str,
        graph: &dyn GraphCtx,
        opts: &CallOptions<'_>,
    ) -> Result<String> {
        let fail = |m: String| GStoreError::Query(format!("run {name} fail: {m}"));
        let args = crate::json::parse(params).map_err(|e| fail(format!("bad arguments: {e}")))?;
        let max_memory =
            usize::try_from(u64::from(opts.limits.max_memory_pages) * 65_536).unwrap_or(usize::MAX);
        let host = Host {
            graph,
            args_text: params,
            args,
            out: Vec::new(),
            max_output: opts.max_output,
            cancel: opts.cancel,
            cancelled: false,
            debt: 0,
            limits: StoreLimitsBuilder::new()
                .memory_size(max_memory)
                .memories(1)
                .tables(1)
                .table_elements(MAX_TABLE_ELEMENTS)
                .instances(1)
                .build(),
        };
        // The store and linker belong to the module's engine (an older one
        // when the shared engine was replaced since the module was loaded).
        let engine = module.engine();
        let mut store = Store::new(engine, host);
        store.limiter(|h| &mut h.limits);
        let linker = linker(engine).map_err(|e| fail(e.to_string()))?;
        let budget = opts.limits.fuel_per_call;
        let remaining = |used: u64| {
            if budget == 0 {
                u64::MAX
            } else {
                budget.saturating_sub(used)
            }
        };
        let exhausted = || {
            fail(format!(
                "the instruction budget of {budget} (pfn_fuel_per_call) is exhausted"
            ))
        };
        let mut used: u64 = 0;
        let mut granted = FUEL_SLICE.min(remaining(0));
        store.set_fuel(granted).map_err(|e| fail(e.to_string()))?;
        let instance = linker
            .instantiate_and_start(&mut store, module)
            .map_err(|e| fail(e.to_string()))?;
        let func = instance
            .get_typed_func::<(), i32>(&store, name)
            .map_err(|e| fail(e.to_string()))?;
        let mut call = func
            .call_resumable(&mut store, ())
            .map_err(|e| fail(e.to_string()))?;
        loop {
            match call {
                TypedResumableCall::Finished(ret) => {
                    // Host work charged past the last slice counts too.
                    let left = store.get_fuel().unwrap_or(0);
                    let total = used
                        .saturating_add(granted.saturating_sub(left))
                        .saturating_add(store.data().debt);
                    if budget != 0 && total > budget {
                        return Err(exhausted());
                    }
                    let host = store.into_data();
                    let out = String::from_utf8(host.out)
                        .map_err(|_| fail("the output is not valid UTF-8".into()))?;
                    if ret == 0 {
                        let mut msg = "the function returned 0 (failure)".to_string();
                        if !out.is_empty() {
                            msg.push_str(": ");
                            msg.extend(out.chars().take(1000));
                        }
                        return Err(fail(msg));
                    }
                    return Ok(out);
                }
                TypedResumableCall::OutOfFuel(invocation) => {
                    let left = store.get_fuel().unwrap_or(0);
                    // What host functions were charged past the slice too.
                    let debt = std::mem::take(&mut store.data_mut().debt);
                    used = used
                        .saturating_add(granted.saturating_sub(left))
                        .saturating_add(debt);
                    if opts.cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                        return Err(GStoreError::Cancelled);
                    }
                    let room = remaining(used);
                    let need = invocation.required_fuel();
                    if room == 0 || need > room {
                        return Err(exhausted());
                    }
                    granted = FUEL_SLICE.max(need).min(room);
                    store.set_fuel(granted).map_err(|e| fail(e.to_string()))?;
                    call = invocation
                        .resume(&mut store)
                        .map_err(|e| fail(e.to_string()))?;
                }
                TypedResumableCall::HostTrap(trap) => {
                    if store.data().cancelled {
                        return Err(GStoreError::Cancelled);
                    }
                    return Err(fail(trap.host_error().to_string()));
                }
            }
        }
    }
}

/// Validate `bytes` in `engine` and check the PFN interface.
fn validate(engine: &Engine, name: &str, bytes: &[u8]) -> std::result::Result<Module, String> {
    let module = Module::new(engine, bytes).map_err(|e| format!("invalid module: {e}"))?;
    check_interface(&module, name)?;
    Ok(module)
}

/// Check what `module` imports and exports against the PFN interface.
fn check_interface(module: &Module, name: &str) -> std::result::Result<(), String> {
    for import in module.imports() {
        let (m, field) = (import.module(), import.name());
        let host = (m == "gstore").then(|| abi::host_fn(field)).flatten();
        let Some(host) = host else {
            return Err(format!(
                "import \"{m}\".\"{field}\" is not available: a PFN module may only import the \
                 \"gstore\" host functions (see docs/PFN.md)"
            ));
        };
        let ok = match import.ty() {
            ExternType::Func(ft) => {
                same_types(ft.params(), host.params) && same_types(ft.results(), host.results)
            }
            _ => false,
        };
        if !ok {
            return Err(format!(
                "import \"gstore\".\"{field}\" has the wrong type (see docs/PFN.md)"
            ));
        }
    }
    match module.get_export(name) {
        Some(ExternType::Func(ft)) if ft.params().is_empty() && ft.results() == [WasmVal::I32] => {}
        Some(_) => {
            return Err(format!(
                "the export \"{name}\" must be a function taking no parameters and returning i32"
            ))
        }
        None => return Err(format!("the module does not export a function \"{name}\"")),
    }
    match module.get_export("memory") {
        Some(ExternType::Memory(_)) => Ok(()),
        _ => Err("the module must export its memory as \"memory\"".to_string()),
    }
}

fn same_types(wasm: &[WasmVal], host: &[ValType]) -> bool {
    wasm.len() == host.len()
        && wasm.iter().zip(host).all(|(w, h)| {
            matches!(
                (w, h),
                (WasmVal::I32, ValType::I32)
                    | (WasmVal::I64, ValType::I64)
                    | (WasmVal::F64, ValType::F64)
            )
        })
}

// ---------------------------------------------------------------------------
// host functions
// ---------------------------------------------------------------------------

/// The store data of one call.
struct Host<'a> {
    graph: &'a dyn GraphCtx,
    args_text: &'a str,
    args: Json,
    out: Vec<u8>,
    max_output: usize,
    /// The query's cancel flag, checked by the host functions too.
    cancel: Option<&'a AtomicBool>,
    /// Set when a host function stopped the call because of `cancel`.
    cancelled: bool,
    /// Fuel host functions were charged beyond what the slice had left
    /// (added to the budget's count when the slice runs out).
    debt: u64,
    limits: StoreLimits,
}

type Cx<'c, 'a> = Caller<'c, Host<'a>>;

/// Charge the call for host work touching `bytes` guest bytes (see
/// [`HOST_BYTES_PER_FUEL`]), and stop it when its query was cancelled. The
/// charge empties the current slice at most; the rest is owed (`debt`), so
/// the next instruction runs out of fuel and the budget and the cancel flag
/// are checked.
fn charge(c: &mut Cx<'_, '_>, bytes: usize) -> std::result::Result<(), WasmError> {
    if c.data().cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
        c.data_mut().cancelled = true;
        return Err(err("query cancelled"));
    }
    let cost = 1 + bytes as u64 / HOST_BYTES_PER_FUEL;
    let left = c.get_fuel()?;
    if cost <= left {
        c.set_fuel(left - cost)?;
    } else {
        c.set_fuel(0)?;
        let h = c.data_mut();
        h.debt = h.debt.saturating_add(cost - left);
    }
    Ok(())
}

/// The length a guest passed (`u32`, as [`abi::guest_range`] reads it).
fn guest_len(len: i32) -> usize {
    len as u32 as usize
}

fn err(msg: impl Into<String>) -> WasmError {
    WasmError::new(msg.into())
}

fn memory(c: &Cx<'_, '_>) -> std::result::Result<Memory, WasmError> {
    match c.get_export("memory") {
        Some(Extern::Memory(m)) => Ok(m),
        _ => Err(err("the module exports no memory")),
    }
}

/// An `Option<u64>` result as an `i64` (`-1` for none).
fn id_or_neg(v: Option<u64>) -> i64 {
    v.and_then(|x| i64::try_from(x).ok()).unwrap_or(-1)
}

/// A predicate argument: `-1` = every predicate, `None` = invalid.
fn pred_arg(p: i64) -> Option<Option<PredId>> {
    if p == -1 {
        Some(None)
    } else {
        PredId::try_from(p).ok().map(Some)
    }
}

fn h_vertex_count(c: Cx<'_, '_>) -> i64 {
    id_or_neg(Some(c.data().graph.vertex_count()))
}

fn h_edge_count(c: Cx<'_, '_>) -> i64 {
    id_or_neg(Some(c.data().graph.edge_count()))
}

fn h_pred_count(c: Cx<'_, '_>) -> i64 {
    c.data().graph.predicates().len() as i64
}

fn h_pred_at(c: Cx<'_, '_>, i: i64) -> i64 {
    let preds = c.data().graph.predicates();
    usize::try_from(i)
        .ok()
        .and_then(|i| preds.get(i))
        .map_or(-1, |&p| i64::from(p))
}

fn degree(c: &Cx<'_, '_>, v: i64, p: i64, out: bool) -> i64 {
    let (Ok(v), Some(pred)) = (u64::try_from(v), pred_arg(p)) else {
        return -1;
    };
    let g = c.data().graph;
    id_or_neg(if out {
        g.out_degree(v, pred)
    } else {
        g.in_degree(v, pred)
    })
}

fn h_out_degree(c: Cx<'_, '_>, v: i64, p: i64) -> i64 {
    degree(&c, v, p, true)
}

fn h_in_degree(c: Cx<'_, '_>, v: i64, p: i64) -> i64 {
    degree(&c, v, p, false)
}

fn neighbor(c: &Cx<'_, '_>, v: i64, p: i64, i: i64, out: bool) -> i64 {
    let (Ok(v), Some(pred), Ok(i)) = (u64::try_from(v), pred_arg(p), u64::try_from(i)) else {
        return -1;
    };
    let g = c.data().graph;
    id_or_neg(if out {
        g.out_neighbor(v, pred, i)
    } else {
        g.in_neighbor(v, pred, i)
    })
}

fn h_out_neighbor(c: Cx<'_, '_>, v: i64, p: i64, i: i64) -> i64 {
    neighbor(&c, v, p, i, true)
}

fn h_in_neighbor(c: Cx<'_, '_>, v: i64, p: i64, i: i64) -> i64 {
    neighbor(&c, v, p, i, false)
}

fn edge_pred(c: &Cx<'_, '_>, v: i64, i: i64, out: bool) -> i64 {
    let (Ok(v), Ok(i)) = (u64::try_from(v), u64::try_from(i)) else {
        return -1;
    };
    let g = c.data().graph;
    let p = if out {
        g.out_edge_pred(v, i)
    } else {
        g.in_edge_pred(v, i)
    };
    p.map_or(-1, i64::from)
}

fn h_out_edge_pred(c: Cx<'_, '_>, v: i64, i: i64) -> i64 {
    edge_pred(&c, v, i, true)
}

fn h_in_edge_pred(c: Cx<'_, '_>, v: i64, i: i64) -> i64 {
    edge_pred(&c, v, i, false)
}

/// Look a guest string up with `f`.
fn lookup(
    mut c: Cx<'_, '_>,
    ptr: i32,
    len: i32,
    f: impl FnOnce(&dyn GraphCtx, &str) -> Option<u64>,
) -> std::result::Result<i64, WasmError> {
    charge(&mut c, guest_len(len))?;
    let mem = memory(&c)?;
    let (data, host) = mem.data_and_store_mut(&mut c);
    let s = abi::read_guest_str(data, ptr, len).map_err(err)?;
    Ok(id_or_neg(f(host.graph, s)))
}

fn h_vertex_id(c: Cx<'_, '_>, ptr: i32, len: i32) -> std::result::Result<i64, WasmError> {
    lookup(c, ptr, len, |g, s| {
        g.term_id(s).and_then(|t| g.term_vertex(t))
    })
}

fn h_term_id(c: Cx<'_, '_>, ptr: i32, len: i32) -> std::result::Result<i64, WasmError> {
    lookup(c, ptr, len, |g, s| g.term_id(s))
}

fn h_pred_id(c: Cx<'_, '_>, ptr: i32, len: i32) -> std::result::Result<i64, WasmError> {
    lookup(c, ptr, len, |g, s| g.pred_id(s).map(u64::from))
}

fn h_vertex_term(c: Cx<'_, '_>, v: i64) -> i64 {
    id_or_neg(
        u64::try_from(v)
            .ok()
            .and_then(|v| c.data().graph.vertex_term(v)),
    )
}

fn h_term_vertex(c: Cx<'_, '_>, t: i64) -> i64 {
    id_or_neg(
        u64::try_from(t)
            .ok()
            .and_then(|t| c.data().graph.term_vertex(t)),
    )
}

/// Write a host string into the guest's `(buf, cap)` (`-1` when `f` has
/// none; the buffer is checked either way). `key` is the guest's key string
/// `(ptr, len)` `f` is given (empty when `None`), read in place. Charged for
/// the key and the bytes written.
fn give<'h>(
    mut c: Cx<'_, 'h>,
    key: Option<(i32, i32)>,
    buf: i32,
    cap: i32,
    f: impl for<'x> FnOnce(&'x Host<'h>, &str) -> Option<std::borrow::Cow<'x, str>>,
) -> std::result::Result<i32, WasmError> {
    let klen = key.map_or(0, |(_, l)| guest_len(l));
    charge(&mut c, klen)?;
    let mem = memory(&c)?;
    let (data, host) = mem.data_and_store_mut(&mut c);
    abi::guest_range(data.len(), buf, cap).map_err(err)?;
    let value = match key {
        Some((p, l)) => f(host, abi::read_guest_str(data, p, l).map_err(err)?),
        None => f(host, ""),
    };
    let Some(s) = value else {
        return Ok(-1);
    };
    let written = s.len().min(guest_len(cap));
    let n = abi::write_guest(data, buf, cap, s.as_bytes()).map_err(err)?;
    drop(s);
    charge(&mut c, written)?;
    Ok(n)
}

fn h_term_str(c: Cx<'_, '_>, t: i64, buf: i32, cap: i32) -> std::result::Result<i32, WasmError> {
    give(c, None, buf, cap, |h, _| {
        u64::try_from(t)
            .ok()
            .and_then(|t| h.graph.term_str(t))
            .map(Into::into)
    })
}

fn h_vertex_str(c: Cx<'_, '_>, v: i64, buf: i32, cap: i32) -> std::result::Result<i32, WasmError> {
    give(c, None, buf, cap, |h, _| {
        u64::try_from(v)
            .ok()
            .and_then(|v| h.graph.vertex_term(v))
            .and_then(|t| h.graph.term_str(t))
            .map(Into::into)
    })
}

fn h_pred_str(c: Cx<'_, '_>, p: i64, buf: i32, cap: i32) -> std::result::Result<i32, WasmError> {
    give(c, None, buf, cap, |h, _| {
        PredId::try_from(p)
            .ok()
            .and_then(|p| h.graph.pred_str(p))
            .map(Into::into)
    })
}

fn h_args(c: Cx<'_, '_>, buf: i32, cap: i32) -> std::result::Result<i32, WasmError> {
    give(c, None, buf, cap, |h, _| Some(h.args_text.into()))
}

/// Run `f` with the guest's key string `(key, klen)`, read in place (no
/// copy) and charged for.
fn with_key<R>(
    c: &mut Cx<'_, '_>,
    key: i32,
    klen: i32,
    f: impl FnOnce(&Host<'_>, &str) -> R,
) -> std::result::Result<R, WasmError> {
    charge(c, guest_len(klen))?;
    let mem = memory(c)?;
    let (data, host) = mem.data_and_store_mut(&mut *c);
    let k = abi::read_guest_str(data, key, klen).map_err(err)?;
    Ok(f(host, k))
}

fn h_arg_str(
    c: Cx<'_, '_>,
    key: i32,
    klen: i32,
    buf: i32,
    cap: i32,
) -> std::result::Result<i32, WasmError> {
    give(c, Some((key, klen)), buf, cap, |h, k| {
        abi::arg_str(&h.args, k).map(Into::into)
    })
}

fn h_arg_int(
    mut c: Cx<'_, '_>,
    key: i32,
    klen: i32,
    default: i64,
) -> std::result::Result<i64, WasmError> {
    with_key(&mut c, key, klen, |h, k| {
        abi::arg_int(&h.args, k).unwrap_or(default)
    })
}

fn h_arg_bool(
    mut c: Cx<'_, '_>,
    key: i32,
    klen: i32,
    default: i32,
) -> std::result::Result<i32, WasmError> {
    with_key(&mut c, key, klen, |h, k| {
        abi::arg_bool(&h.args, k).map_or(default, i32::from)
    })
}

fn h_arg_len(mut c: Cx<'_, '_>, key: i32, klen: i32) -> std::result::Result<i32, WasmError> {
    with_key(&mut c, key, klen, |h, k| {
        abi::arg_array(&h.args, k)
            .and_then(|a| i32::try_from(a.len()).ok())
            .unwrap_or(-1)
    })
}

fn h_arg_item(
    c: Cx<'_, '_>,
    key: i32,
    klen: i32,
    i: i32,
    buf: i32,
    cap: i32,
) -> std::result::Result<i32, WasmError> {
    give(c, Some((key, klen)), buf, cap, |h, k| {
        let items = abi::arg_array(&h.args, k)?;
        usize::try_from(i)
            .ok()
            .and_then(|i| items.get(i))
            .and_then(abi::scalar_text)
            .map(Into::into)
    })
}

/// Append `bytes` to the result, within the output bound.
fn push_out(host: &mut Host<'_>, bytes: &[u8]) -> std::result::Result<(), WasmError> {
    if host.out.len().saturating_add(bytes.len()) > host.max_output {
        return Err(err(format!(
            "the output exceeds {} bytes (gfun_max_output_bytes)",
            host.max_output
        )));
    }
    host.out.extend_from_slice(bytes);
    Ok(())
}

fn h_emit(mut c: Cx<'_, '_>, ptr: i32, len: i32) -> std::result::Result<(), WasmError> {
    charge(&mut c, guest_len(len))?;
    let mem = memory(&c)?;
    let (data, host) = mem.data_and_store_mut(&mut c);
    let bytes = abi::read_guest(data, ptr, len).map_err(err)?;
    push_out(host, bytes)
}

/// `emit_json_str`: the guest's string as a quoted JSON string, escaped
/// straight into the output — the bound is checked as it grows (an escape
/// is at most 6 bytes per input byte), never on a copy built first.
fn h_emit_json_str(mut c: Cx<'_, '_>, ptr: i32, len: i32) -> std::result::Result<(), WasmError> {
    charge(&mut c, guest_len(len))?;
    let mem = memory(&c)?;
    let (data, host) = mem.data_and_store_mut(&mut c);
    let bytes = abi::read_guest(data, ptr, len).map_err(err)?;
    let s = std::str::from_utf8(bytes).map_err(|e| {
        err(format!(
            "the string at {} is not valid UTF-8 ({e})",
            ptr as u32
        ))
    })?;
    let exceeds = |host: &Host<'_>| {
        err(format!(
            "the output exceeds {} bytes (gfun_max_output_bytes)",
            host.max_output
        ))
    };
    // At least the quotes and the text itself, before anything is written.
    if host.out.len().saturating_add(s.len()).saturating_add(2) > host.max_output {
        return Err(exceeds(host));
    }
    let start = host.out.len();
    let mut chunk = String::new();
    host.out.push(b'"');
    // Escaped a piece at a time (whole characters), each checked.
    let mut from = 0;
    while from < s.len() {
        let mut to = (from + 64 * 1024).min(s.len());
        while !s.is_char_boundary(to) {
            to += 1;
        }
        chunk.clear();
        crate::json::write_str(&s[from..to], &mut chunk);
        // Without the quotes `write_str` adds around each piece.
        let inner = &chunk.as_bytes()[1..chunk.len() - 1];
        if host.out.len().saturating_add(inner.len()).saturating_add(1) > host.max_output {
            host.out.truncate(start);
            return Err(exceeds(host));
        }
        host.out.extend_from_slice(inner);
        from = to;
    }
    host.out.push(b'"');
    Ok(())
}

fn h_emit_i64(mut c: Cx<'_, '_>, x: i64) -> std::result::Result<(), WasmError> {
    push_out(c.data_mut(), x.to_string().as_bytes())
}

fn h_emit_f64(mut c: Cx<'_, '_>, x: f64) -> std::result::Result<(), WasmError> {
    push_out(c.data_mut(), abi::json_f64(x).as_bytes())
}

/// The linker with every host function of [`abi::HOST_FUNCTIONS`].
fn linker<'a>(engine: &Engine) -> std::result::Result<Linker<Host<'a>>, WasmError> {
    let mut l: Linker<Host<'a>> = Linker::new(engine);
    let g = "gstore";
    l.func_wrap(g, "vertex_count", h_vertex_count)?
        .func_wrap(g, "edge_count", h_edge_count)?
        .func_wrap(g, "pred_count", h_pred_count)?
        .func_wrap(g, "pred_at", h_pred_at)?
        .func_wrap(g, "out_degree", h_out_degree)?
        .func_wrap(g, "in_degree", h_in_degree)?
        .func_wrap(g, "out_neighbor", h_out_neighbor)?
        .func_wrap(g, "in_neighbor", h_in_neighbor)?
        .func_wrap(g, "out_edge_pred", h_out_edge_pred)?
        .func_wrap(g, "in_edge_pred", h_in_edge_pred)?
        .func_wrap(g, "vertex_id", h_vertex_id)?
        .func_wrap(g, "vertex_term", h_vertex_term)?
        .func_wrap(g, "term_vertex", h_term_vertex)?
        .func_wrap(g, "term_id", h_term_id)?
        .func_wrap(g, "term_str", h_term_str)?
        .func_wrap(g, "vertex_str", h_vertex_str)?
        .func_wrap(g, "pred_id", h_pred_id)?
        .func_wrap(g, "pred_str", h_pred_str)?
        .func_wrap(g, "args", h_args)?
        .func_wrap(g, "arg_str", h_arg_str)?
        .func_wrap(g, "arg_int", h_arg_int)?
        .func_wrap(g, "arg_bool", h_arg_bool)?
        .func_wrap(g, "arg_len", h_arg_len)?
        .func_wrap(g, "arg_item", h_arg_item)?
        .func_wrap(g, "emit", h_emit)?
        .func_wrap(g, "emit_i64", h_emit_i64)?
        .func_wrap(g, "emit_f64", h_emit_f64)?
        .func_wrap(g, "emit_json_str", h_emit_json_str)?;
    Ok(l)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pfn::{generate_source, CsrGraphCtx, PfnInfo};
    use crate::Database;

    fn module_for(rt: &Runtime, name: &str, body: &str, subs: &str) -> Module {
        let src = generate_source(&PfnInfo {
            name: name.into(),
            body: crate::pfn::url_encode(body),
            subs: crate::pfn::url_encode(subs),
            ..PfnInfo::default()
        });
        let bytes = rt.compile(name, &src, &PfnLimits::default()).unwrap();
        rt.load(name, &bytes).unwrap().0
    }

    fn opts(limits: PfnLimits) -> CallOptions<'static> {
        CallOptions {
            limits,
            cancel: None,
            max_output: 1 << 20,
        }
    }

    #[test]
    fn every_host_function_is_linked_and_the_prelude_compiles() {
        let rt = Runtime::new();
        // The prelude imports every host function: linking it must find all.
        let m = module_for(&rt, "f", "(i32.const 1)", "");
        let db =
            Database::build_from_str("pfn_wasm_t", "<http://a> <http://p> <http://b> .\n").unwrap();
        let csr = db.csr().unwrap();
        let g = CsrGraphCtx::new(&csr, db.dict());
        assert_eq!(
            rt.run(&m, "f", "{}", &g, &opts(PfnLimits::default()))
                .unwrap(),
            ""
        );
    }

    /// Loads move to a fresh engine once `ENGINE_RECYCLE_BYTES` were loaded
    /// into one (wasmi never frees what a module registered in its engine);
    /// a module of the replaced engine still runs.
    #[test]
    fn the_shared_engine_is_replaced_and_old_modules_still_run() {
        let rt = Runtime::new();
        let wat = format!(
            "(module (import \"gstore\" \"emit_i64\" (func $e (param i64))) \
             (memory (export \"memory\") 8) (data (i32.const 0) \"{}\") \
             (func (export \"f\") (result i32) (call $e (i64.const 5)) (i32.const 1)))",
            "a".repeat(300 * 1024)
        );
        let bytes = rt.compile("f", &wat, &PfnLimits::default()).unwrap();
        let (first, fresh) = rt.load("f", &bytes).unwrap();
        assert!(!fresh);
        let mut loads = 1;
        loop {
            let (_, fresh) = rt.load("f", &bytes).unwrap();
            loads += 1;
            if fresh {
                break;
            }
            assert!(
                loads * bytes.len() <= ENGINE_RECYCLE_BYTES + bytes.len(),
                "{loads}"
            );
        }
        assert!(!rt.load("f", &bytes).unwrap().1);
        let db =
            Database::build_from_str("pfn_wasm_e", "<http://a> <http://p> <http://b> .\n").unwrap();
        let csr = db.csr().unwrap();
        let g = CsrGraphCtx::new(&csr, db.dict());
        assert_eq!(
            rt.run(&first, "f", "{}", &g, &opts(PfnLimits::default()))
                .unwrap(),
            "5"
        );
        // Validation never touches the shared engine.
        let before = rt.current.lock().unwrap().loaded;
        rt.compile("f", &wat, &PfnLimits::default()).unwrap();
        assert_eq!(rt.current.lock().unwrap().loaded, before);
    }

    #[test]
    fn interface_violations_are_compile_errors() {
        let rt = Runtime::new();
        let l = PfnLimits::default();
        let wasi = r#"(module (import "wasi_snapshot_preview1" "fd_write"
            (func (param i32 i32 i32 i32) (result i32))) (memory (export "memory") 1)
            (func (export "f") (result i32) (i32.const 1)))"#;
        let e = rt.compile("f", wasi, &l).unwrap_err();
        assert!(e.contains("is not available"), "{e}");
        let wrong = r#"(module (import "gstore" "emit" (func (param i64)))
            (memory (export "memory") 1) (func (export "f") (result i32) (i32.const 1)))"#;
        assert!(rt
            .compile("f", wrong, &l)
            .unwrap_err()
            .contains("wrong type"));
        let no_mem = r#"(module (func (export "f") (result i32) (i32.const 1)))"#;
        assert!(rt.compile("f", no_mem, &l).unwrap_err().contains("memory"));
        let no_fn = r#"(module (memory (export "memory") 1))"#;
        assert!(rt
            .compile("f", no_fn, &l)
            .unwrap_err()
            .contains("does not export"));
        let bad_sig = r#"(module (memory (export "memory") 1) (func (export "f") (param i32) (result i32) (i32.const 1)))"#;
        assert!(rt
            .compile("f", bad_sig, &l)
            .unwrap_err()
            .contains("no parameters"));
        let start = r#"(module (memory (export "memory") 1) (func $s) (start $s)
            (func (export "f") (result i32) (i32.const 1)))"#;
        assert!(rt.compile("f", start, &l).is_err());
        let syntax = rt.compile("f", "(module (func", &l).unwrap_err();
        assert!(!syntax.is_empty());
        let tiny = PfnLimits {
            max_module_bytes: 10,
            ..l
        };
        assert!(rt
            .compile("f", no_fn, &tiny)
            .unwrap_err()
            .contains("pfn_max_module_bytes"));
    }
}
