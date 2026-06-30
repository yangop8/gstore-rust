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
        // xsd:dateTime / xsd:date compare by their UTC instant.
        if let (Some(a), Some(b)) = (datetime_instant(self), datetime_instant(other)) {
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
        // xsd:dateTime / xsd:date order chronologically by their UTC instant.
        if let (Some(a), Some(b)) = (datetime_instant(self), datetime_instant(other)) {
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
        Some(v @ Value::Typed { value, .. }) => match datetime_instant(v) {
            // dateTimes order chronologically (group 6, after strings).
            Some(inst) => (6, OrdF64(inst), 0, String::new()),
            None => (5, OrdF64(0.0), 0, value.clone()),
        },
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

const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
const XSD_DATE: &str = "http://www.w3.org/2001/XMLSchema#date";

/// The UTC instant (seconds since 1970-01-01T00:00:00Z, possibly fractional /
/// negative) of an `xsd:dateTime` / `xsd:date` value, if `v` is one and parses.
/// Lets dateTime comparisons in FILTER/ORDER BY order chronologically — even
/// across time zones. A missing timezone is treated as UTC (a pragmatic
/// simplification of SPARQL's indeterminate-comparison rule).
fn datetime_instant(v: &Value) -> Option<f64> {
    let Value::Typed { value, datatype } = v else {
        return None;
    };
    if datatype != XSD_DATETIME && datatype != XSD_DATE {
        return None;
    }
    parse_xsd_datetime(value)
}

/// Parse `-?YYYY-MM-DD(Thh:mm:ss(.s+)?)?(Z|±hh:mm)?` to a UTC instant.
fn parse_xsd_datetime(s: &str) -> Option<f64> {
    let b = s.trim().as_bytes();
    let mut i = 0usize;
    let neg_year = b.first() == Some(&b'-');
    if neg_year {
        i += 1;
    }
    // Year: 4+ digits.
    let y_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i - y_start < 4 {
        return None;
    }
    let mut year = digits_to_i64(&b[y_start..i])?;
    if neg_year {
        year = -year;
    }
    expect_byte(b, &mut i, b'-')?;
    let month = take_n_digits(b, &mut i, 2)?;
    expect_byte(b, &mut i, b'-')?;
    let day = take_n_digits(b, &mut i, 2)?;

    let (mut hh, mut mm, mut ss, mut frac) = (0i64, 0i64, 0i64, 0f64);
    if i < b.len() && b[i] == b'T' {
        i += 1;
        hh = take_n_digits(b, &mut i, 2)?;
        expect_byte(b, &mut i, b':')?;
        mm = take_n_digits(b, &mut i, 2)?;
        expect_byte(b, &mut i, b':')?;
        ss = take_n_digits(b, &mut i, 2)?;
        if i < b.len() && b[i] == b'.' {
            let f_start = i;
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            frac = std::str::from_utf8(&b[f_start..i]).ok()?.parse().ok()?;
        }
    }

    let mut tz_off = 0i64; // seconds east of UTC
    if i < b.len() {
        match b[i] {
            b'Z' => i += 1,
            b'+' | b'-' => {
                let sign = if b[i] == b'+' { 1 } else { -1 };
                i += 1;
                let th = take_n_digits(b, &mut i, 2)?;
                expect_byte(b, &mut i, b':')?;
                let tm = take_n_digits(b, &mut i, 2)?;
                tz_off = sign * (th * 3600 + tm * 60);
            }
            _ => return None,
        }
    }
    if i != b.len()
        || !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hh > 23
        || mm > 59
        || ss > 59
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss - tz_off;
    Some(secs as f64 + frac)
}

fn expect_byte(b: &[u8], i: &mut usize, c: u8) -> Option<()> {
    if *i < b.len() && b[*i] == c {
        *i += 1;
        Some(())
    } else {
        None
    }
}

fn take_n_digits(b: &[u8], i: &mut usize, n: usize) -> Option<i64> {
    if *i + n > b.len() {
        return None;
    }
    let v = digits_to_i64(&b[*i..*i + n])?;
    *i += n;
    Some(v)
}

fn digits_to_i64(b: &[u8]) -> Option<i64> {
    if b.is_empty() {
        return None;
    }
    let mut v = 0i64;
    for &c in b {
        let d = (c as char).to_digit(10)?;
        v = v.checked_mul(10)?.checked_add(d as i64)?;
    }
    Some(v)
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Number of days in month `m` of year `y` (proleptic Gregorian).
fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days from 1970-01-01 to `y-m-d` (proleptic Gregorian; Howard Hinnant's
/// algorithm). Valid for the full xsd:dateTime year range.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
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

    fn dt(s: &str) -> Value {
        Value::from_term(&Term::typed_literal(s, XSD_DATETIME))
    }

    #[test]
    fn datetime_compares_chronologically() {
        let a = dt("2020-01-01T00:00:00Z");
        let b = dt("2021-06-15T12:30:00Z");
        assert_eq!(a.sparql_cmp(&b), Some(Ordering::Less));
        assert_eq!(a.sparql_eq(&dt("2020-01-01T00:00:00Z")), Some(true));
    }

    #[test]
    fn datetime_normalizes_timezone() {
        // 2020-01-01T12:00:00+02:00 == 2020-01-01T10:00:00Z (same instant).
        let east = dt("2020-01-01T12:00:00+02:00");
        let utc = dt("2020-01-01T10:00:00Z");
        assert_eq!(east.sparql_eq(&utc), Some(true));
        assert_eq!(east.sparql_cmp(&utc), Some(Ordering::Equal));
    }

    #[test]
    fn datetime_fraction_and_date_only() {
        let a = dt("2020-01-01T00:00:00.5Z");
        let b = dt("2020-01-01T00:00:00Z");
        assert_eq!(a.sparql_cmp(&b), Some(Ordering::Greater));
        // xsd:date (no time) parses to midnight UTC.
        let d = Value::from_term(&Term::typed_literal("2020-01-02", XSD_DATE));
        assert_eq!(d.sparql_cmp(&b), Some(Ordering::Greater));
    }

    #[test]
    fn datetime_order_key_is_chronological_not_lexical() {
        // Lexically "2021…" > "2020…", but chronologically the +10:00 instant
        // (Dec 31 14:00 UTC) precedes the Z one (Dec 31 23:00 UTC).
        let chrono_earlier = order_key(&Some(dt("2021-01-01T00:00:00+10:00")));
        let chrono_later = order_key(&Some(dt("2020-12-31T23:00:00Z")));
        assert!(chrono_earlier < chrono_later);
    }

    #[test]
    fn epoch_is_zero() {
        assert_eq!(parse_xsd_datetime("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_xsd_datetime("1970-01-02T00:00:00Z"), Some(86_400.0));
    }

    #[test]
    fn malformed_datetime_is_none() {
        assert_eq!(parse_xsd_datetime("not-a-date"), None);
        assert_eq!(parse_xsd_datetime("2020-13-01T00:00:00Z"), None); // month 13
        assert_eq!(parse_xsd_datetime("2020-01-01T00:00:00XY"), None); // bad tz
    }

    #[test]
    fn day_of_month_is_validated() {
        assert_eq!(parse_xsd_datetime("2020-02-30T00:00:00Z"), None); // Feb 30
        assert_eq!(parse_xsd_datetime("2021-04-31T00:00:00Z"), None); // Apr 31
        assert_eq!(parse_xsd_datetime("2021-02-29T00:00:00Z"), None); // 2021 not leap
        assert!(parse_xsd_datetime("2020-02-29T00:00:00Z").is_some()); // 2020 leap
        assert_eq!(parse_xsd_datetime("2020-01-01T24:00:00Z"), None); // hour 24
        assert_eq!(parse_xsd_datetime("2020-01-01T00:60:00Z"), None); // minute 60
    }
}
