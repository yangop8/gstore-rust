//! The built-in function vocabulary: every function name the query engine
//! evaluates natively, with the number (and, where the grammar restricts it,
//! the shape) of arguments it accepts.
//!
//! This table is the single source of truth shared by both ends of the query
//! path:
//!
//! * the **parser** rejects a call to a known built-in with the wrong arity at
//!   parse time ([`check_call`]), e.g. `STR()` ⇒
//!   `SPARQL parse error: STR expects exactly 1 argument, got 0`;
//! * the **evaluator** (`query::engine`) dispatches only the names listed here
//!   as built-ins and treats a wrong-arity call that still reaches it (an AST
//!   built programmatically, bypassing the parser) as an ordinary evaluation
//!   error — the solution is dropped — never as an out-of-bounds index.
//!
//! Names that are not listed are user-defined functions
//! ([`crate::query::FunctionRegistry`], gStore's PFN); their arity is up to the
//! registered closure, so the parser accepts any argument count for them.
//!
//! Function names are matched in upper case (the parser upper-cases every
//! call name). Keep [`BUILTINS`] sorted by name: lookups binary-search it.

use super::ast::Expr;

/// The accepted argument count of a built-in: `min ..= max` (`max == None` ⇒
/// variadic, any count `>= min`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arity {
    pub min: usize,
    pub max: Option<usize>,
}

impl Arity {
    /// Exactly `n` arguments.
    pub const fn exactly(n: usize) -> Arity {
        Arity {
            min: n,
            max: Some(n),
        }
    }

    /// Between `min` and `max` arguments, inclusive.
    pub const fn range(min: usize, max: usize) -> Arity {
        Arity {
            min,
            max: Some(max),
        }
    }

    /// `min` or more arguments.
    pub const fn at_least(min: usize) -> Arity {
        Arity { min, max: None }
    }

    /// Whether a call with `n` arguments is well-formed.
    pub fn accepts(self, n: usize) -> bool {
        n >= self.min && self.max.is_none_or(|m| n <= m)
    }

    /// Human-readable form for error messages ("exactly 1 argument",
    /// "2 to 3 arguments", "any number of arguments", …).
    pub fn describe(self) -> String {
        let plural = |n: usize| if n == 1 { "argument" } else { "arguments" };
        match self.max {
            Some(0) => "no arguments".to_string(),
            Some(m) if m == self.min => format!("exactly {m} {}", plural(m)),
            Some(m) => format!("{} to {m} arguments", self.min),
            None if self.min == 0 => "any number of arguments".to_string(),
            None => format!("at least {} {}", self.min, plural(self.min)),
        }
    }
}

/// One built-in function: its (upper-case) name and accepted arity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Builtin {
    pub name: &'static str,
    pub arity: Arity,
}

const fn b(name: &'static str, arity: Arity) -> Builtin {
    Builtin { name, arity }
}

const fn n(k: usize) -> Arity {
    Arity::exactly(k)
}

/// Every built-in the engine evaluates natively, sorted by name.
///
/// Standard SPARQL 1.1 functions (§17.4) plus the gStore extensions (graph
/// analytics, `SIMILARITY`, `SIMPLECYCLEBOOLEAN`, …). Aggregates (`COUNT`,
/// `SUM`, …) are parsed separately and are not listed.
pub const BUILTINS: &[Builtin] = &[
    b("ABS", n(1)),
    b("BETWEENNESSCENTRALITY", n(1)),
    b("BFSCOUNT", Arity::range(1, 2)),
    b("BNODE", Arity::range(0, 1)),
    // BOUND additionally requires its argument to be a variable (see `check_call`).
    b("BOUND", n(1)),
    b("CEIL", n(1)),
    b("CLOSENESSCENTRALITY", n(1)),
    b("CLUSTERINGCOEFF", Arity::range(0, 1)),
    b("COALESCE", Arity::at_least(0)),
    b("CONCAT", Arity::at_least(0)),
    b("CONTAINS", n(2)),
    b("CYCLEBOOLEAN", n(0)),
    b("DATATYPE", n(1)),
    b("DAY", n(1)),
    b("DEGREECORRELATION", n(0)),
    b("DIAMETERESTIMATION", n(0)),
    b("ENCODE_FOR_URI", n(1)),
    b("FLOOR", n(1)),
    b("HOURS", n(1)),
    b("IF", n(3)),
    b("IRI", n(1)),
    b("ISBLANK", n(1)),
    b("ISIRI", n(1)),
    b("ISLITERAL", n(1)),
    b("ISNUMERIC", n(1)),
    b("ISURI", n(1)),
    b("JACCARDSIMILARITY", n(2)),
    b("KHOPALLNEIGHBORS", n(2)),
    b("KHOPCOUNT", n(2)),
    b("KHOPENUMERATEPATH", n(2)),
    b("KHOPNEIGHBOR", n(2)),
    b("KHOPREACHABLE", n(3)),
    b("LABELPROP", Arity::range(1, 2)),
    b("LANG", n(1)),
    b("LANGMATCHES", n(2)),
    b("LCASE", n(1)),
    b("LOUVAIN", Arity::range(1, 2)),
    b("MD5", n(1)),
    b("MINUTES", n(1)),
    b("MONTH", n(1)),
    b("NOW", n(0)),
    b("PPR", n(2)),
    b("RAND", n(0)),
    b("REGEX", Arity::range(2, 3)),
    b("REPLACE", Arity::range(3, 4)),
    b("ROUND", n(1)),
    b("SAMETERM", n(2)),
    b("SECONDS", n(1)),
    b("SHA1", n(1)),
    b("SHA256", n(1)),
    b("SHA384", n(1)),
    b("SHA512", n(1)),
    b("SHORTESTPATHLEN", n(2)),
    b("SHORTESTPATHS", n(2)),
    b("SIMILARITY", n(2)),
    b("SIMPLECYCLEBOOLEAN", n(0)),
    b("SSSP", n(2)),
    b("SSSPLEN", n(2)),
    b("STR", n(1)),
    b("STRAFTER", n(2)),
    b("STRBEFORE", n(2)),
    b("STRDT", n(2)),
    b("STRENDS", n(2)),
    b("STRLANG", n(2)),
    b("STRLEN", n(1)),
    b("STRSTARTS", n(2)),
    b("STRUUID", n(0)),
    b("SUBSTR", Arity::range(2, 3)),
    b("TIMEZONE", n(1)),
    b("TRIANGLECOUNTING", Arity::range(0, 1)),
    b("TZ", n(1)),
    b("UCASE", n(1)),
    b("URI", n(1)),
    b("UUID", n(0)),
    b("WCC", Arity::range(1, 2)),
    b("YEAR", n(1)),
];

/// The built-in named `name` (upper case), if it is one.
pub fn lookup(name: &str) -> Option<&'static Builtin> {
    BUILTINS
        .binary_search_by(|e| e.name.cmp(name))
        .ok()
        .map(|i| &BUILTINS[i])
}

/// Whether `name` (upper case) is a built-in rather than a user-defined function.
pub fn is_builtin(name: &str) -> bool {
    lookup(name).is_some()
}

/// Validate a call `name(args…)`: a known built-in must receive an accepted
/// number of arguments (and `BOUND` a variable). Unknown names — user-defined
/// functions — are always accepted. The error message names the function and
/// its expected arity.
pub fn check_call(name: &str, args: &[Expr]) -> Result<(), String> {
    let Some(spec) = lookup(name) else {
        return Ok(());
    };
    if !spec.arity.accepts(args.len()) {
        return Err(format!(
            "{name} expects {}, got {}",
            spec.arity.describe(),
            args.len()
        ));
    }
    // SPARQL grammar: 'BOUND' '(' Var ')'.
    if name == "BOUND" && !matches!(args, [Expr::Var(_)]) {
        return Err("BOUND expects a variable argument, e.g. BOUND(?x)".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_and_unique_so_binary_search_works() {
        for w in BUILTINS.windows(2) {
            assert!(
                w[0].name < w[1].name,
                "BUILTINS must be strictly sorted: {} !< {}",
                w[0].name,
                w[1].name
            );
        }
        for e in BUILTINS {
            assert_eq!(lookup(e.name), Some(e), "{} must be found", e.name);
            assert_eq!(e.name, e.name.to_ascii_uppercase(), "names are upper case");
        }
        assert!(lookup("NOT_A_BUILTIN").is_none());
        assert!(lookup("str").is_none(), "lookups take the upper-cased name");
    }

    #[test]
    fn arity_accepts_and_describes() {
        assert!(Arity::exactly(1).accepts(1));
        assert!(!Arity::exactly(1).accepts(0));
        assert!(!Arity::exactly(1).accepts(2));
        assert!(Arity::range(2, 3).accepts(3));
        assert!(!Arity::range(2, 3).accepts(4));
        assert!(Arity::at_least(0).accepts(0));
        assert!(Arity::at_least(0).accepts(99));
        assert_eq!(Arity::exactly(0).describe(), "no arguments");
        assert_eq!(Arity::exactly(1).describe(), "exactly 1 argument");
        assert_eq!(Arity::exactly(3).describe(), "exactly 3 arguments");
        assert_eq!(Arity::range(2, 3).describe(), "2 to 3 arguments");
        assert_eq!(Arity::at_least(0).describe(), "any number of arguments");
        assert_eq!(Arity::at_least(1).describe(), "at least 1 argument");
    }

    #[test]
    fn check_call_reports_name_and_expected_arity() {
        let e = check_call("STR", &[]).unwrap_err();
        assert_eq!(e, "STR expects exactly 1 argument, got 0");
        let v = Expr::Var("x".into());
        assert!(check_call("STR", std::slice::from_ref(&v)).is_ok());
        let e = check_call("REGEX", std::slice::from_ref(&v)).unwrap_err();
        assert!(e.contains("REGEX") && e.contains("2 to 3"), "{e}");
        // BOUND needs a variable, not an arbitrary expression.
        assert!(check_call("BOUND", std::slice::from_ref(&v)).is_ok());
        let c = Expr::Const(crate::model::Term::iri("x"));
        assert!(check_call("BOUND", &[c]).unwrap_err().contains("variable"));
        // Unknown names are user-defined functions: any arity is fine.
        assert!(check_call("MYFN", &[]).is_ok());
        assert!(check_call("MYFN", &[v.clone(), v.clone(), v]).is_ok());
    }
}
