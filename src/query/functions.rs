//! User-defined scalar SPARQL functions — a safe, in-process replacement for
//! gStore's `pfnQuery` dlopen/.so plugin mechanism.
//!
//! gStore (`GeneralEvaluation`/`pfnQuery`) lets a query call a custom function
//! whose implementation lives in a shared object loaded at runtime with
//! `dlopen`. That is powerful but unsafe (arbitrary native code, ABI coupling,
//! crashes take down the server). This module provides the same *capability* —
//! extend the query language with application-defined scalar functions — but as
//! a **programmatic registry of Rust closures** evaluated in-process:
//!
//! * Register a function by name with [`FunctionRegistry::register`] (or the
//!   builder methods on [`Evaluator`](crate::query::Evaluator)).
//! * The expression evaluator consults the registry for any function name it
//!   does not recognise as a SPARQL built-in, *before* erroring (treating the
//!   call as unbound). Built-ins always take precedence.
//!
//! ## Naming and how it is called from SPARQL
//!
//! The SPARQL grammar parses a *bare-word* call `name(args…)` as a function
//! application and **upper-cases** the name. (A prefixed name like `ns:fn` lexes
//! as an IRI term, not a callable, so it is not reachable as a function call in
//! this grammar.) Registration therefore key-folds names to upper case, and
//! lookups are case-insensitive. Call a registered function `myDouble` from a
//! query as `myDouble(?x)` / `MYDOUBLE(?x)` — both resolve to the same closure.
//!
//! ## Signature
//!
//! A custom function receives its arguments already evaluated to [`Value`]s and
//! returns `Option<Value>`: `Some(v)` is the result, `None` signals an
//! evaluation error (the enclosing solution is dropped / the FILTER fails),
//! exactly like a built-in that returns `None`.

use std::collections::HashMap;
use std::sync::Arc;

use super::value::Value;

/// A registered scalar function: maps already-evaluated argument values to a
/// result value, or `None` on error. `Send + Sync` so a [`FunctionRegistry`]
/// (and the [`Database`](crate::Database) holding one) stays thread-safe — the
/// HTTP server and the concurrent database share it across threads.
pub type CustomFn = Arc<dyn Fn(&[Value]) -> Option<Value> + Send + Sync>;

/// A registry of user-defined scalar functions, keyed by upper-cased name.
///
/// Cloning is cheap: the closures are reference-counted, so a clone shares them.
#[derive(Clone, Default)]
pub struct FunctionRegistry {
    funcs: HashMap<String, CustomFn>,
}

impl std::fmt::Debug for FunctionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FunctionRegistry")
            .field("count", &self.funcs.len())
            .finish()
    }
}

impl FunctionRegistry {
    /// An empty registry.
    pub fn new() -> FunctionRegistry {
        FunctionRegistry::default()
    }

    /// Register (or replace) a function under `name`. The name is folded to
    /// upper case to match how the SPARQL parser normalises function calls, so
    /// lookups are case-insensitive.
    pub fn register<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&[Value]) -> Option<Value> + Send + Sync + 'static,
    {
        self.funcs.insert(name.to_ascii_uppercase(), Arc::new(f));
    }

    /// Register a function from an already-shared closure handle.
    pub fn register_fn(&mut self, name: &str, f: CustomFn) {
        self.funcs.insert(name.to_ascii_uppercase(), f);
    }

    /// Look up a function by name (case-insensitive). Used by the evaluator's
    /// expression interpreter for names that are not SPARQL built-ins.
    pub(crate) fn get(&self, name: &str) -> Option<&CustomFn> {
        self.funcs
            .get(name)
            .or_else(|| self.funcs.get(&name.to_ascii_uppercase()))
    }

    /// Whether any function is registered (the evaluator skips the lookup fast
    /// when empty).
    pub fn is_empty(&self) -> bool {
        self.funcs.is_empty()
    }

    /// Number of registered functions.
    pub fn len(&self) -> usize {
        self.funcs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup_is_case_insensitive() {
        let mut r = FunctionRegistry::new();
        r.register("myDouble", |args| {
            let x = args.first()?.as_f64()?;
            Some(Value::Double(x * 2.0))
        });
        assert_eq!(r.len(), 1);
        assert!(!r.is_empty());
        // Looked up by the upper-cased form the parser produces.
        let f = r.get("MYDOUBLE").expect("registered");
        assert_eq!(f(&[Value::Int(21)]), Some(Value::Double(42.0)));
        // And by the exact spelling.
        assert!(r.get("mydouble").is_some());
        assert!(r.get("nope").is_none());
    }

    #[test]
    fn missing_arg_yields_none() {
        let mut r = FunctionRegistry::new();
        r.register("needsnum", |args| Some(Value::Double(args.first()?.as_f64()? + 1.0)));
        let f = r.get("NEEDSNUM").unwrap();
        assert_eq!(f(&[Value::Iri("x".into())]), None);
    }
}
