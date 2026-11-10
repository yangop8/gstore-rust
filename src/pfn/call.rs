//! How a projected `PFN.name({…})` turns into function calls — upstream's
//! `GeneralEvaluation::pfnQuery` / `pfnQueryByVar` / `pfnConvertVar`
//! (`src/Query/PfnAnalysisQuery.cpp`).
//!
//! * The argument is the `{…}` the parser kept ([`crate::parser::sparql::ast::Expr::Pfn`]),
//!   parsed as JSON; it must be an object. The function receives it
//!   **canonicalized** — compact, keys sorted — as upstream re-serializes it
//!   with `nlohmann::json::dump()`.
//! * **No solutions** (`WHERE {}`, or a WHERE clause without a match): the
//!   function runs once with the argument as written, and its output is the
//!   value as is ([`evaluate`] with `constant`).
//! * **Solutions**: every top-level value that is a string naming a WHERE
//!   variable (`"src":"?x"`) is replaced by each distinct term the variable
//!   is bound to (N-Triples form, `<http://ex/a>` / `"text"`); the function
//!   runs once per distinct combination (the cartesian product), in the
//!   byte order of the canonical argument texts (upstream collects them in a
//!   `std::set<std::string>`). The value is the JSON array of the outputs,
//!   each parsed as JSON when it is JSON and kept as a JSON string otherwise.
//!   A variable without any usable binding keeps its `"?x"` text, as
//!   upstream; when no variable has a binding the loop never runs and the
//!   value is `[]`.
//! * **Divergence**: an argument with no variable at all, next to
//!   solutions, runs once (a one-element array); upstream's loop never runs
//!   then either and answers `[]`, which only makes the call useless.

use std::collections::HashMap;

use crate::error::{GStoreError, Result};
use crate::json::Json;

/// The most argument combinations one projected call may expand to.
pub const MAX_COMBINATIONS: usize = 1_000_000;

/// Parse the argument of `PFN.name(…)`; it must be a JSON object.
pub fn parse_args(name: &str, json: &str) -> Result<Json> {
    match crate::json::parse(json) {
        Ok(v @ Json::Obj(_)) => Ok(v),
        Ok(_) => Err(GStoreError::Query(format!(
            "PFN.{name}: the argument must be a JSON object, e.g. PFN.{name}({{\"src\":\"<a>\"}})"
        ))),
        Err(e) => Err(GStoreError::Query(format!(
            "PFN.{name}: the argument is not valid JSON: {e}"
        ))),
    }
}

/// The text a function receives: compact, keys sorted (upstream's `dump()`).
pub fn canonical(args: &Json) -> String {
    args.sorted().to_string()
}

/// The top-level keys whose value is a string `?v` naming a variable for
/// which `is_var(v)` holds, as `(key, v)`, in argument order.
pub fn variable_keys(args: &Json, is_var: impl Fn(&str) -> bool) -> Vec<(String, String)> {
    args.as_object()
        .unwrap_or_default()
        .iter()
        .filter_map(|(k, v)| {
            let var = v.as_str()?.strip_prefix('?')?;
            is_var(var).then(|| (k.clone(), var.to_string()))
        })
        .collect()
}

/// Upstream's `pfnConvertVar`: every combination of `bindings` (key → its
/// values) substituted into `args`, as canonical texts — distinct, in byte
/// order (upstream collects them in a `std::set<std::string>`). They are
/// produced one at a time ([`Combinations`]), never all held at once, and
/// without recursion, so neither a wide argument object nor a large constant
/// in it can exhaust memory or the stack. Fails before producing any when
/// there would be more than `limit`.
pub fn combinations(
    args: &Json,
    bindings: &[(&str, &[String])],
    limit: usize,
) -> std::result::Result<Combinations, String> {
    let mut out = Combinations {
        slots: Vec::new(),
        idx: Vec::new(),
        done: bindings.is_empty(),
        len: 0,
    };
    if out.done {
        return Ok(out);
    }
    let by_key: HashMap<&str, &[String]> = bindings.iter().copied().collect();
    // The canonical text is "{" + the sorted entries joined by "," + "}".
    // Where combinations first differ, the order of the whole texts is that
    // of one entry's quoted value (everything before it is the same, and an
    // escaped JSON string never holds a raw `"`): so with each variable
    // entry's quoted values sorted, an odometer over the entries (the first
    // one most significant) yields the texts in byte order, each once.
    let sorted = args.sorted();
    let entries = sorted.as_object().unwrap_or_default();
    let mut count: usize = 1;
    for (k, _) in entries {
        if let Some(values) = by_key.get(k.as_str()) {
            count = count.saturating_mul(values.len());
        }
    }
    if count > limit {
        return Err(format!(
            "the variables expand to more than {limit} argument combinations"
        ));
    }
    let mut constant = String::new();
    for (k, v) in entries {
        if !constant.is_empty() || !out.slots.is_empty() {
            constant.push(',');
        }
        crate::json::write_str(k, &mut constant);
        constant.push(':');
        match by_key.get(k.as_str()) {
            Some(values) => {
                let mut quoted: Vec<String> =
                    values.iter().map(|v| crate::json::quote(v)).collect();
                quoted.sort_unstable();
                quoted.dedup();
                out.slots.push(Slot {
                    prefix: std::mem::take(&mut constant),
                    values: quoted,
                });
            }
            None => v.write_to(&mut constant),
        }
    }
    out.len = out.slots.iter().map(|s| s.prefix.len()).sum::<usize>() + constant.len() + 2;
    out.slots.push(Slot {
        prefix: constant,
        values: Vec::new(),
    });
    out.idx = vec![0; out.slots.len()];
    // A variable without values: no combination at all.
    out.done = count == 0;
    Ok(out)
}

/// One run of an argument text: the constant text before a variable entry's
/// value, and that entry's quoted values, sorted (the last slot holds the
/// constant tail only).
#[derive(Debug)]
struct Slot {
    prefix: String,
    values: Vec<String>,
}

/// The argument texts of [`combinations`], in byte order.
#[derive(Debug)]
pub struct Combinations {
    slots: Vec<Slot>,
    /// The odometer: each variable slot's current value.
    idx: Vec<usize>,
    done: bool,
    /// The length of the constant parts (a capacity hint).
    len: usize,
}

impl Iterator for Combinations {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        if self.done {
            return None;
        }
        let mut text = String::with_capacity(self.len + 64);
        text.push('{');
        for (slot, &i) in self.slots.iter().zip(&self.idx) {
            text.push_str(&slot.prefix);
            if let Some(v) = slot.values.get(i) {
                text.push_str(v);
            }
        }
        text.push('}');
        // Advance: the last variable slot fastest.
        let vars = self.slots.len() - 1;
        let mut j = vars;
        loop {
            if j == 0 {
                self.done = true;
                break;
            }
            j -= 1;
            self.idx[j] += 1;
            if self.idx[j] < self.slots[j].values.len() {
                break;
            }
            self.idx[j] = 0;
        }
        Some(text)
    }
}

/// [`combinations`], collected (tests).
#[cfg(test)]
fn expand(
    args: &Json,
    bindings: &[(String, Vec<String>)],
    limit: usize,
) -> std::result::Result<Vec<String>, String> {
    let b: Vec<(&str, &[String])> = bindings
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_slice()))
        .collect();
    Ok(combinations(args, &b, limit)?.collect())
}

/// One output as an array element: its JSON value when it parses, else the
/// text as a JSON string (upstream: `json::parse`, falling back to the
/// string on a parse error).
pub fn output_value(text: &str) -> Json {
    crate::json::parse(text).unwrap_or_else(|_| Json::Str(text.to_string()))
}

/// An error of the variable form, worded as upstream's `pfnQueryByVar`
/// words it (`run dynamic function fail: …`); a cancellation stays one.
pub fn by_var_error(e: GStoreError) -> GStoreError {
    match e {
        GStoreError::Query(m) => GStoreError::Query(format!("run dynamic function fail: {m}")),
        other => other,
    }
}

/// Evaluate one projected PFN call (see the module docs). `var_values(v)`
/// gives the distinct usable bindings of WHERE variable `v` across the
/// solutions (`None` when `v` is not a WHERE variable); `run(args)` calls
/// the function once with a canonical argument text. `max_output` bounds
/// the assembled array.
pub fn evaluate(
    name: &str,
    args: &Json,
    constant: bool,
    mut var_values: impl FnMut(&str) -> Option<Vec<String>>,
    mut run: impl FnMut(&str) -> Result<String>,
    max_output: usize,
) -> Result<String> {
    if constant {
        return run(&canonical(args));
    }
    let wrap = by_var_error;
    // Each variable's bindings are looked up once, however many keys name it.
    let keys = variable_keys(args, |_| true);
    let mut values: HashMap<&str, Option<Vec<String>>> = HashMap::new();
    for (_, var) in &keys {
        if !values.contains_key(var.as_str()) {
            values.insert(var, var_values(var));
        }
    }
    let var_keys: Vec<(&str, &[String])> = keys
        .iter()
        .filter_map(|(key, var)| Some((key.as_str(), values.get(var.as_str())?.as_deref()?)))
        .collect();
    let combos: Box<dyn Iterator<Item = String>> = if var_keys.is_empty() {
        Box::new(std::iter::once(canonical(args)))
    } else {
        let bound: Vec<(&str, &[String])> = var_keys
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .collect();
        Box::new(
            combinations(args, &bound, MAX_COMBINATIONS)
                .map_err(|m| wrap(GStoreError::Query(format!("PFN.{name}: {m}"))))?,
        )
    };
    let mut out = String::from("[");
    for (i, combo) in combos.enumerate() {
        let text = run(&combo).map_err(wrap)?;
        if i > 0 {
            out.push(',');
        }
        output_value(&text).write_to(&mut out);
        if out.len() > max_output {
            return Err(wrap(GStoreError::Query(format!(
                "PFN.{name}: the results exceed {max_output} bytes (gfun_max_output_bytes)"
            ))));
        }
    }
    out.push(']');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Json {
        parse_args("f", s).unwrap()
    }

    #[test]
    fn arguments_must_be_objects() {
        assert!(parse_args("f", "{}").is_ok());
        let e = parse_args("f", "[1]").unwrap_err().to_string();
        assert!(e.contains("JSON object"), "{e}");
        let e = parse_args("f", "{'a':1}").unwrap_err().to_string();
        assert!(e.contains("not valid JSON"), "{e}");
        assert_eq!(
            canonical(&args(r#"{"src":"<a>","k":"2","directed":"true"}"#)),
            r#"{"directed":"true","k":"2","src":"<a>"}"#
        );
    }

    #[test]
    fn variables_expand_to_sorted_distinct_combinations() {
        let a = args(r#"{"src":"?x","dst":"?y","k":"?nope","lit":"x"}"#);
        let keys = variable_keys(&a, |v| v == "x" || v == "y");
        assert_eq!(
            keys,
            vec![("src".into(), "x".into()), ("dst".into(), "y".into())]
        );
        let set = expand(
            &a,
            &[
                ("src".into(), vec!["<b>".into(), "<a>".into()]),
                ("dst".into(), vec!["<c>".into(), "<c>".into()]),
            ],
            10,
        )
        .unwrap();
        let v: Vec<&str> = set.iter().map(String::as_str).collect();
        assert_eq!(
            v,
            [
                r#"{"dst":"<c>","k":"?nope","lit":"x","src":"<a>"}"#,
                r#"{"dst":"<c>","k":"?nope","lit":"x","src":"<b>"}"#,
            ]
        );
        assert!(expand(&a, &[("src".into(), vec!["1".into(), "2".into()])], 1).is_err());
        assert!(expand(&a, &[], 10).unwrap().is_empty());
    }

    #[test]
    fn evaluation_follows_upstream_shapes() {
        let a = args(r#"{"src":"?x"}"#);
        let mut seen = Vec::new();
        // No solutions: one call, raw output.
        let v = evaluate("f", &a, true, |_| None, |p| Ok(format!("raw {p}")), 1000).unwrap();
        assert_eq!(v, r#"raw {"src":"?x"}"#);
        // Solutions: an array, outputs parsed when JSON.
        let v = evaluate(
            "f",
            &a,
            false,
            |var| (var == "x").then(|| vec!["<b>".to_string(), "<a>".to_string()]),
            |p| {
                seen.push(p.to_string());
                Ok(if p.contains("<a>") {
                    "{\"n\":1}".into()
                } else {
                    "plain".into()
                })
            },
            1000,
        )
        .unwrap();
        assert_eq!(v, r#"[{"n":1},"plain"]"#);
        assert_eq!(seen, [r#"{"src":"<a>"}"#, r#"{"src":"<b>"}"#]);
        // A WHERE variable without bindings: no call at all.
        let v = evaluate(
            "f",
            &a,
            false,
            |_| Some(Vec::new()),
            |_| panic!("no call"),
            1000,
        );
        assert_eq!(v.unwrap(), "[]");
        // No variable: one call (the documented divergence).
        let c = args(r#"{"src":"<a>"}"#);
        let v = evaluate("f", &c, false, |_| None, |_| Ok("7".into()), 1000).unwrap();
        assert_eq!(v, "[7]");
        // A variable the WHERE clause does not have stays literal.
        let v = evaluate("f", &a, false, |_| None, |p| Ok(p.to_string()), 1000).unwrap();
        assert_eq!(v, r#"[{"src":"?x"}]"#);
        // Errors are wrapped like upstream's; cancellation is not.
        let e = evaluate(
            "f",
            &c,
            false,
            |_| None,
            |_| Err(GStoreError::Query("boom".into())),
            9,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("run dynamic function fail: boom"), "{e}");
        let e = evaluate("f", &c, false, |_| None, |_| Err(GStoreError::Cancelled), 9).unwrap_err();
        assert!(matches!(e, GStoreError::Cancelled));
        // The assembled array is bounded.
        let e = evaluate(
            "f",
            &a,
            false,
            |_| Some(vec!["<a>".into(), "<b>".into()]),
            |_| Ok("x".repeat(8)),
            12,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("exceed 12 bytes"), "{e}");
    }
}
