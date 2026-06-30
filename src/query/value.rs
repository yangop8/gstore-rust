//! Runtime values for FILTER / ORDER BY evaluation.
//!
//! gStore evaluates filters with `EvalMultitypeValue`; this is the Rust analogue.
//! A [`Value`] is produced from a [`Term`] and carries enough type information to
//! implement SPARQL's numeric/string/boolean comparison and arithmetic, the
//! effective-boolean-value (EBV) rule, and a total order for `ORDER BY`.

use std::cmp::Ordering;

use crate::model::Term;
use crate::parser::sparql::ast::xsd;

/// A typed value used during expression evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Iri(String),
    Blank(String),
    /// xsd:integer (and the integer family).
    Int(i64),
    /// xsd:decimal / xsd:double / xsd:float, all held as `f64`.
    Double(f64),
    Bool(bool),
    /// A plain or language-tagged string (and xsd:string).
    Str {
        value: String,
        lang: Option<String>,
    },
    /// Any other typed literal we don't special-case.
    Typed {
        value: String,
        datatype: String,
    },
}

impl Value {
    /// Build a value from an RDF term, classifying literals by datatype.
    pub fn from_term(t: &Term) -> Value {
        match t {
            Term::Iri(s) => Value::Iri(s.clone()),
            Term::Blank(s) => Value::Blank(s.clone()),
            Term::Literal {
                value,
                datatype,
                lang,
            } => {
                if lang.is_some() {
                    return Value::Str {
                        value: value.clone(),
                        lang: lang.clone(),
                    };
                }
                match datatype.as_deref() {
                    None => Value::Str {
                        value: value.clone(),
                        lang: None,
                    },
                    Some(dt) => Value::from_typed(value, dt),
                }
            }
        }
    }

    fn from_typed(value: &str, dt: &str) -> Value {
        // Integer family.
        const INT_TYPES: &[&str] = &[
            xsd::INTEGER,
            "http://www.w3.org/2001/XMLSchema#int",
            "http://www.w3.org/2001/XMLSchema#long",
            "http://www.w3.org/2001/XMLSchema#short",
            "http://www.w3.org/2001/XMLSchema#byte",
            "http://www.w3.org/2001/XMLSchema#nonNegativeInteger",
            "http://www.w3.org/2001/XMLSchema#positiveInteger",
            "http://www.w3.org/2001/XMLSchema#unsignedInt",
            "http://www.w3.org/2001/XMLSchema#unsignedLong",
        ];
        const FLOAT_TYPES: &[&str] = &[
            xsd::DECIMAL,
            xsd::DOUBLE,
            "http://www.w3.org/2001/XMLSchema#float",
        ];
        if INT_TYPES.contains(&dt) {
            if let Ok(i) = value.trim().parse::<i64>() {
                return Value::Int(i);
            }
        }
        if FLOAT_TYPES.contains(&dt) {
            if let Ok(f) = value.trim().parse::<f64>() {
                return Value::Double(f);
            }
        }
        if dt == xsd::BOOLEAN {
            match value.trim() {
                "true" | "1" => return Value::Bool(true),
                "false" | "0" => return Value::Bool(false),
                _ => {}
            }
        }
        if dt == xsd::STRING {
            return Value::Str {
                value: value.to_owned(),
                lang: None,
            };
        }
        Value::Typed {
            value: value.to_owned(),
            datatype: dt.to_owned(),
        }
    }

    /// The numeric value, if this is a number.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Double(d) => Some(*d),
            _ => None,
        }
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, Value::Int(_) | Value::Double(_))
    }

    /// The lexical form used for `STR()` and output.
    pub fn lexical(&self) -> String {
        match self {
            Value::Iri(s) | Value::Blank(s) => s.clone(),
            Value::Int(i) => i.to_string(),
            Value::Double(d) => format_double(*d),
            Value::Bool(b) => b.to_string(),
            Value::Str { value, .. } => value.clone(),
            Value::Typed { value, .. } => value.clone(),
        }
    }

    /// SPARQL effective boolean value. Returns `None` when EBV is a type error
    /// (the caller treats that as "filter excludes this solution").
    pub fn ebv(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Int(i) => Some(*i != 0),
            Value::Double(d) => Some(*d != 0.0 && !d.is_nan()),
            Value::Str { value, .. } => Some(!value.is_empty()),
            // Numeric strings stored as Typed are uncommon; IRIs/blank have no EBV.
            _ => None,
        }
    }

    /// SPARQL `=` semantics across value types. `None` means "incomparable"
    /// (a type error), which the caller maps to filter exclusion.
    pub fn sparql_eq(&self, other: &Value) -> Option<bool> {
        // Compare two integers directly to avoid f64 precision loss for
        // magnitudes >= 2^53.
        if let (Value::Int(a), Value::Int(b)) = (self, other) {
            return Some(a == b);
        }
        if let (Some(a), Some(b)) = (self.as_f64(), other.as_f64()) {
            return Some(a == b);
        }
        match (self, other) {
            (Value::Iri(a), Value::Iri(b)) => Some(a == b),
            (Value::Blank(a), Value::Blank(b)) => Some(a == b),
            (Value::Bool(a), Value::Bool(b)) => Some(a == b),
            (Value::Str { value: a, lang: la }, Value::Str { value: b, lang: lb }) => {
                Some(a == b && la == lb)
            }
            (
                Value::Typed {
                    value: a,
                    datatype: da,
                },
                Value::Typed {
                    value: b,
                    datatype: db,
                },
            ) => Some(a == b && da == db),
            _ => None,
        }
    }

    /// SPARQL ordering relation for `<`, `>`, `<=`, `>=`. `None` = incomparable.
    pub fn sparql_cmp(&self, other: &Value) -> Option<Ordering> {
        // Compare two integers directly to avoid f64 precision loss for
        // magnitudes >= 2^53.
        if let (Value::Int(a), Value::Int(b)) = (self, other) {
            return Some(a.cmp(b));
        }
        if let (Some(a), Some(b)) = (self.as_f64(), other.as_f64()) {
            return a.partial_cmp(&b);
        }
        match (self, other) {
            (Value::Str { value: a, .. }, Value::Str { value: b, .. }) => Some(a.cmp(b)),
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            (Value::Iri(a), Value::Iri(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }
}

/// A total ordering for `ORDER BY`, following SPARQL's ascending order:
/// (unbound <) blank nodes < IRIs < literals; numbers and strings by value.
/// Used on `Option<Value>` where `None` is "unbound".
pub fn order_key(v: &Option<Value>) -> impl Ord {
    // Returns (group, num, int, text) — compared lexicographically by derive(Ord).
    // group: 0 unbound, 1 blank, 2 iri, 3 numeric, 4 boolean, 5 string/other.
    // `int` is an exact i64 tie-breaker so integers >= 2^53 keep their true
    // ordering when their f64 approximations collide; it is 0 for non-integers.
    match v {
        None => (0u8, OrdF64(f64::NEG_INFINITY), 0i64, String::new()),
        Some(Value::Blank(s)) => (1, OrdF64(0.0), 0, s.clone()),
        Some(Value::Iri(s)) => (2, OrdF64(0.0), 0, s.clone()),
        Some(Value::Int(i)) => (3, OrdF64(*i as f64), *i, String::new()),
        Some(Value::Double(d)) => (3, OrdF64(*d), 0, String::new()),
        Some(Value::Bool(b)) => (4, OrdF64(0.0), 0, b.to_string()),
        Some(Value::Str { value, .. }) => (5, OrdF64(0.0), 0, value.clone()),
        Some(Value::Typed { value, .. }) => (5, OrdF64(0.0), 0, value.clone()),
    }
}

/// A wrapper giving `f64` a total order (NaN sorts last) for `ORDER BY` keys.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrdF64(pub f64);
impl Eq for OrdF64 {}
impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.partial_cmp(&other.0).unwrap_or_else(|| {
            // NaN handling: NaN == NaN, NaN > everything.
            match (self.0.is_nan(), other.0.is_nan()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => Ordering::Equal,
            }
        })
    }
}

/// Format a double without a trailing `.0`-less integer ambiguity, matching
/// common SPARQL serializers closely enough for output.
fn format_double(d: f64) -> String {
    if d == d.trunc() && d.is_finite() && d.abs() < 1e15 {
        format!("{:.1}", d)
    } else {
        d.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_integer_becomes_int() {
        let v = Value::from_term(&Term::typed_literal("2500", xsd::INTEGER));
        assert_eq!(v, Value::Int(2500));
    }

    #[test]
    fn typed_float_becomes_double() {
        let v = Value::from_term(&Term::typed_literal("161.5", xsd::DOUBLE));
        assert_eq!(v, Value::Double(161.5));
    }

    #[test]
    fn plain_literal_is_string() {
        let v = Value::from_term(&Term::plain_literal("hi"));
        assert_eq!(
            v,
            Value::Str {
                value: "hi".into(),
                lang: None
            }
        );
    }

    #[test]
    fn numeric_comparison_crosses_int_and_double() {
        let a = Value::Int(3);
        let b = Value::Double(3.5);
        assert_eq!(a.sparql_cmp(&b), Some(Ordering::Less));
        assert_eq!(a.sparql_eq(&Value::Double(3.0)), Some(true));
    }

    #[test]
    fn string_comparison_is_lexicographic() {
        let a = Value::Str {
            value: "alice".into(),
            lang: None,
        };
        let b = Value::Str {
            value: "bob".into(),
            lang: None,
        };
        assert_eq!(a.sparql_cmp(&b), Some(Ordering::Less));
    }

    #[test]
    fn iri_and_number_are_incomparable_for_ordering() {
        let a = Value::Iri("x".into());
        let b = Value::Int(1);
        assert_eq!(a.sparql_cmp(&b), None);
        assert_eq!(a.sparql_eq(&b), None);
    }

    #[test]
    fn ebv_rules() {
        assert_eq!(Value::Bool(true).ebv(), Some(true));
        assert_eq!(Value::Int(0).ebv(), Some(false));
        assert_eq!(Value::Int(5).ebv(), Some(true));
        assert_eq!(
            Value::Str {
                value: "".into(),
                lang: None
            }
            .ebv(),
            Some(false)
        );
        assert_eq!(Value::Iri("x".into()).ebv(), None);
    }

    #[test]
    fn order_key_groups_unbound_first() {
        let unbound = order_key(&None);
        let iri = order_key(&Some(Value::Iri("a".into())));
        assert!(unbound < iri);
    }
}
