//! The PFN host interface: what a function module may import from the host
//! (module `"gstore"`), the prelude [`generate_source`](super::generate_source)
//! wraps bodies in, and the runtime-independent helpers behind the host
//! functions (argument lookup, number formatting, bounds-checked guest
//! memory access). `docs/PFN.md` documents the same table for users.
//!
//! # Conventions
//!
//! * Vertices, terms and predicates are `i64` ids ([`super::graph`]); `-1`
//!   means "none" (unknown string, vertex out of range, no such neighbour).
//!   A predicate argument of `-1` means "every predicate".
//! * A string goes **into** the host as `(ptr: i32, len: i32)` — UTF-8 bytes
//!   in the module's exported `memory`.
//! * A string comes **out of** the host `snprintf`-style: the host writes at
//!   most `cap` bytes at `buf` and returns the string's full length (so a
//!   result `> cap` means "truncated, retry with a bigger buffer"), or `-1`
//!   when there is no such string.
//! * Every `(ptr, len)` / `(buf, cap)` range must lie inside the memory, or
//!   the call traps — a clean query error, never a host fault.
//! * The function's result is the text it [`emit`](HOST_FUNCTIONS)s; the
//!   exported function returns `1` (any non-zero value) on success, `0` on
//!   failure.

use crate::json::Json;

/// A WebAssembly value type of the host interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    I32,
    I64,
    F64,
}

impl ValType {
    fn wat(self) -> &'static str {
        match self {
            ValType::I32 => "i32",
            ValType::I64 => "i64",
            ValType::F64 => "f64",
        }
    }
}

/// One host function of module `"gstore"`.
#[derive(Debug, Clone, Copy)]
pub struct HostFn {
    pub name: &'static str,
    pub params: &'static [ValType],
    pub results: &'static [ValType],
    /// One line for the prelude / docs.
    pub doc: &'static str,
}

use ValType::{F64, I32, I64};

/// The host interface, in prelude order. The names double as the prelude's
/// `$identifiers` (`(call $out_degree …)`).
pub const HOST_FUNCTIONS: &[HostFn] = &[
    // ---- graph ----
    HostFn {
        name: "vertex_count",
        params: &[],
        results: &[I64],
        doc: "number of vertices",
    },
    HostFn {
        name: "edge_count",
        params: &[],
        results: &[I64],
        doc: "number of edges (one per predicate)",
    },
    HostFn {
        name: "pred_count",
        params: &[],
        results: &[I64],
        doc: "number of predicates labelling an edge",
    },
    HostFn {
        name: "pred_at",
        params: &[I64],
        results: &[I64],
        doc: "(i) -> the i-th such predicate id, ascending; -1 out of range",
    },
    HostFn {
        name: "out_degree",
        params: &[I64, I64],
        results: &[I64],
        doc: "(v, pred|-1) -> out-degree; -1 for a bad vertex",
    },
    HostFn {
        name: "in_degree",
        params: &[I64, I64],
        results: &[I64],
        doc: "(v, pred|-1) -> in-degree; -1 for a bad vertex",
    },
    HostFn {
        name: "out_neighbor",
        params: &[I64, I64, I64],
        results: &[I64],
        doc: "(v, pred|-1, i) -> i-th out-neighbour; -1 if none",
    },
    HostFn {
        name: "in_neighbor",
        params: &[I64, I64, I64],
        results: &[I64],
        doc: "(v, pred|-1, i) -> i-th in-neighbour; -1 if none",
    },
    HostFn {
        name: "out_edge_pred",
        params: &[I64, I64],
        results: &[I64],
        doc: "(v, i) -> predicate of the i-th out-edge (all-predicate order)",
    },
    HostFn {
        name: "in_edge_pred",
        params: &[I64, I64],
        results: &[I64],
        doc: "(v, i) -> predicate of the i-th in-edge (all-predicate order)",
    },
    // ---- dictionary ----
    HostFn {
        name: "vertex_id",
        params: &[I32, I32],
        results: &[I64],
        doc: "(ptr, len) term string -> vertex; -1 if not a vertex",
    },
    HostFn {
        name: "vertex_term",
        params: &[I64],
        results: &[I64],
        doc: "(v) -> term id; -1 for a bad vertex",
    },
    HostFn {
        name: "term_vertex",
        params: &[I64],
        results: &[I64],
        doc: "(t) -> vertex; -1 if the term is not a vertex",
    },
    HostFn {
        name: "term_id",
        params: &[I32, I32],
        results: &[I64],
        doc: "(ptr, len) term string -> term id; -1 if unknown",
    },
    HostFn {
        name: "term_str",
        params: &[I64, I32, I32],
        results: &[I32],
        doc: "(t, buf, cap) -> length of the term's string; -1 if unknown",
    },
    HostFn {
        name: "vertex_str",
        params: &[I64, I32, I32],
        results: &[I32],
        doc: "(v, buf, cap) -> length of the vertex's term string; -1 if bad",
    },
    HostFn {
        name: "pred_id",
        params: &[I32, I32],
        results: &[I64],
        doc: "(ptr, len) predicate string -> predicate id; -1 if unknown",
    },
    HostFn {
        name: "pred_str",
        params: &[I64, I32, I32],
        results: &[I32],
        doc: "(p, buf, cap) -> length of the predicate's string; -1 if unknown",
    },
    // ---- arguments ----
    HostFn {
        name: "args",
        params: &[I32, I32],
        results: &[I32],
        doc: "(buf, cap) -> length of the call's argument JSON",
    },
    HostFn {
        name: "arg_str",
        params: &[I32, I32, I32, I32],
        results: &[I32],
        doc: "(key, klen, buf, cap) -> length of a scalar argument's text; -1 if absent",
    },
    HostFn {
        name: "arg_int",
        params: &[I32, I32, I64],
        results: &[I64],
        doc: "(key, klen, default) -> integer argument (number or numeric string)",
    },
    HostFn {
        name: "arg_bool",
        params: &[I32, I32, I32],
        results: &[I32],
        doc: "(key, klen, default) -> 1/0 for true/\"true\"/\"1\" or false/\"false\"/\"0\"",
    },
    HostFn {
        name: "arg_len",
        params: &[I32, I32],
        results: &[I32],
        doc: "(key, klen) -> length of an array argument; -1 if absent / not an array",
    },
    HostFn {
        name: "arg_item",
        params: &[I32, I32, I32, I32, I32],
        results: &[I32],
        doc: "(key, klen, i, buf, cap) -> length of the i-th array item's text; -1 if none",
    },
    // ---- output ----
    HostFn {
        name: "emit",
        params: &[I32, I32],
        results: &[],
        doc: "(ptr, len) append UTF-8 text to the result",
    },
    HostFn {
        name: "emit_i64",
        params: &[I64],
        results: &[],
        doc: "(x) append x in decimal",
    },
    HostFn {
        name: "emit_f64",
        params: &[F64],
        results: &[],
        doc: "(x) append x as a JSON number (null if not finite)",
    },
    HostFn {
        name: "emit_json_str",
        params: &[I32, I32],
        results: &[],
        doc: "(ptr, len) append the UTF-8 text as a quoted JSON string",
    },
];

/// The host function named `name`.
pub fn host_fn(name: &str) -> Option<&'static HostFn> {
    HOST_FUNCTIONS.iter().find(|f| f.name == name)
}

/// The module prelude of a generated source: `(module`, one import per host
/// function, and one exported page of memory.
pub fn prelude() -> String {
    let mut out =
        String::from("(module\n  ;; ---- gStore host interface (module \"gstore\") ----\n");
    for f in HOST_FUNCTIONS {
        let mut sig = String::new();
        if !f.params.is_empty() {
            sig.push_str(" (param");
            for p in f.params {
                sig.push(' ');
                sig.push_str(p.wat());
            }
            sig.push(')');
        }
        if !f.results.is_empty() {
            sig.push_str(" (result");
            for r in f.results {
                sig.push(' ');
                sig.push_str(r.wat());
            }
            sig.push(')');
        }
        out.push_str(&format!(
            "  (import \"gstore\" \"{0}\" (func ${0}{sig})) ;; {1}\n",
            f.name, f.doc
        ));
    }
    out.push_str("  (memory (export \"memory\") 1)\n");
    out
}

// ---------------------------------------------------------------------------
// guest memory
// ---------------------------------------------------------------------------

/// The byte range `(ptr, len)` names in a memory of `mem_len` bytes (both
/// read as unsigned 32-bit), or an error describing why it is not inside.
pub fn guest_range(mem_len: usize, ptr: i32, len: i32) -> Result<std::ops::Range<usize>, String> {
    let start = ptr as u32 as usize;
    let len = len as u32 as usize;
    match start.checked_add(len) {
        Some(end) if end <= mem_len => Ok(start..end),
        _ => Err(format!(
            "the buffer at {start} of {len} bytes is outside the module's memory ({mem_len} bytes)"
        )),
    }
}

/// Read the guest's `(ptr, len)` bytes.
pub fn read_guest(mem: &[u8], ptr: i32, len: i32) -> Result<&[u8], String> {
    Ok(&mem[guest_range(mem.len(), ptr, len)?])
}

/// Read the guest's `(ptr, len)` bytes as UTF-8 text.
pub fn read_guest_str(mem: &[u8], ptr: i32, len: i32) -> Result<&str, String> {
    std::str::from_utf8(read_guest(mem, ptr, len)?)
        .map_err(|e| format!("the string at {} is not valid UTF-8 ({e})", ptr as u32))
}

/// `snprintf`-style output: write at most `cap` bytes of `bytes` to the
/// guest's `buf` and return the full length (`-1`-free; an error when the
/// buffer is outside the memory or the length does not fit an `i32`).
pub fn write_guest(mem: &mut [u8], buf: i32, cap: i32, bytes: &[u8]) -> Result<i32, String> {
    let range = guest_range(mem.len(), buf, cap)?;
    let n = bytes.len().min(range.len());
    mem[range.start..range.start + n].copy_from_slice(&bytes[..n]);
    i32::try_from(bytes.len()).map_err(|_| "the string is longer than 2 GiB".to_string())
}

// ---------------------------------------------------------------------------
// arguments
// ---------------------------------------------------------------------------

/// A scalar JSON value as text: a string as is, a number in its lexical
/// form, a boolean as `true` / `false`.
pub fn scalar_text(v: &Json) -> Option<String> {
    match v {
        Json::Str(s) => Some(s.clone()),
        Json::Num(n) => Some(n.clone()),
        Json::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Top-level argument `key` as text ([`scalar_text`]).
pub fn arg_str(args: &Json, key: &str) -> Option<String> {
    args.get(key).and_then(scalar_text)
}

/// Top-level argument `key` as an integer: a JSON integer or a string
/// holding one (upstream clients send `"k":"2"`).
pub fn arg_int(args: &Json, key: &str) -> Option<i64> {
    match args.get(key)? {
        n @ Json::Num(_) => n.as_i64(),
        Json::Str(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Top-level argument `key` as a flag: `true` / `"true"` / `"1"` / `1`, or
/// `false` / `"false"` / `"0"` / `0`.
pub fn arg_bool(args: &Json, key: &str) -> Option<bool> {
    match args.get(key)? {
        Json::Bool(b) => Some(*b),
        Json::Str(s) => match s.trim() {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        },
        n @ Json::Num(_) => match n.as_i64() {
            Some(1) => Some(true),
            Some(0) => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Top-level array argument `key`.
pub fn arg_array<'j>(args: &'j Json, key: &str) -> Option<&'j [Json]> {
    args.get(key)?.as_array()
}

/// An `f64` as a JSON number (shortest round-trip form; `null` when not
/// finite).
pub fn json_f64(x: f64) -> String {
    Json::from(x).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_table_has_unique_names_and_a_prelude_line_each() {
        let mut names: Vec<&str> = HOST_FUNCTIONS.iter().map(|f| f.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), HOST_FUNCTIONS.len());
        let p = prelude();
        assert!(p.starts_with("(module\n"));
        assert!(p.contains(
            "(import \"gstore\" \"out_neighbor\" (func $out_neighbor (param i64 i64 i64) (result i64)))"
        ));
        assert!(p.contains("(import \"gstore\" \"emit\" (func $emit (param i32 i32)))"));
        assert!(
            p.contains("(import \"gstore\" \"vertex_count\" (func $vertex_count (result i64)))")
        );
        assert_eq!(p.matches("(import ").count(), HOST_FUNCTIONS.len());
        assert!(host_fn("emit_f64").is_some() && host_fn("fd_write").is_none());
    }

    #[test]
    fn guest_ranges_are_bounds_checked() {
        assert_eq!(guest_range(10, 2, 3), Ok(2..5));
        assert_eq!(guest_range(10, 0, 10), Ok(0..10));
        assert_eq!(guest_range(10, 10, 0), Ok(10..10));
        assert!(guest_range(10, 8, 3).is_err());
        assert!(guest_range(10, 11, 0).is_err());
        // Negative i32s are huge unsigned offsets / lengths.
        assert!(guest_range(10, -1, 1).is_err());
        assert!(guest_range(10, 0, -1).is_err());
        let mut mem = vec![0u8; 8];
        assert_eq!(write_guest(&mut mem, 2, 3, b"hello"), Ok(5), "full length");
        assert_eq!(&mem, b"\0\0hel\0\0\0");
        assert_eq!(write_guest(&mut mem, 0, 0, b"x"), Ok(1));
        assert!(write_guest(&mut mem, 6, 4, b"x").is_err());
        assert_eq!(read_guest(&mem, 2, 3), Ok(&b"hel"[..]));
        assert_eq!(read_guest_str(&mem, 2, 3), Ok("hel"));
        mem[0] = 0xff;
        assert!(read_guest_str(&mem, 0, 1).is_err());
        assert!(read_guest(&mem, 7, 2).is_err());
    }

    #[test]
    fn arguments_read_like_upstream_params() {
        let args = crate::json::parse(
            r#"{"src":"<a>","k":"2","n":3,"directed":"true","d2":false,"f":1.5,
                "preds":["<p>","<q>"],"obj":{}}"#,
        )
        .unwrap();
        assert_eq!(arg_str(&args, "src").as_deref(), Some("<a>"));
        assert_eq!(arg_str(&args, "n").as_deref(), Some("3"));
        assert_eq!(arg_str(&args, "preds"), None);
        assert_eq!(arg_str(&args, "missing"), None);
        assert_eq!(arg_int(&args, "k"), Some(2));
        assert_eq!(arg_int(&args, "n"), Some(3));
        assert_eq!(arg_int(&args, "f"), None);
        assert_eq!(arg_int(&args, "src"), None);
        assert_eq!(arg_bool(&args, "directed"), Some(true));
        assert_eq!(arg_bool(&args, "d2"), Some(false));
        assert_eq!(arg_bool(&args, "src"), None);
        assert_eq!(arg_array(&args, "preds").map(<[Json]>::len), Some(2));
        assert_eq!(arg_array(&args, "src"), None);
        assert_eq!(json_f64(0.5), "0.5");
        assert_eq!(json_f64(2.0), "2");
        assert_eq!(json_f64(f64::NAN), "null");
    }
}
