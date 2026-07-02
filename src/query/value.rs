//! Runtime values for FILTER / ORDER BY evaluation.
//!
//! gStore evaluates filters with `EvalMultitypeValue`; this is the Rust analogue.
//! A [`Value`] is produced from a [`Term`] and carries enough type information to
//! implement SPARQL's numeric/string/boolean comparison and arithmetic, the
//! effective-boolean-value (EBV) rule, and a total order for `ORDER BY`.

use std::cmp::Ordering;

use crate::model::Term;
use crate::parser::sparql::ast::{xsd, ArithOp};

/// A typed value used during expression evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Iri(String),
    Blank(String),
    /// xsd:integer (and the integer family) within `i64` range.
    Int(i64),
    /// xsd:integer beyond `i64` range, held exactly as `i128`.
    BigInt(i128),
    /// xsd:decimal, held exactly as mantissa × 10⁻ˢᶜᵃˡᵉ.
    Decimal(Decimal),
    /// xsd:double / xsd:float, held as `f64`.
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
        const FLOAT_TYPES: &[&str] = &[xsd::DOUBLE, "http://www.w3.org/2001/XMLSchema#float"];
        if INT_TYPES.contains(&dt) {
            let t = value.trim();
            // Prefer i64; fall back to i128 for integers beyond it (exact big
            // integers); beyond i128 fall back to f64 (a documented bound).
            if let Ok(i) = t.parse::<i64>() {
                return Value::Int(i);
            }
            if let Ok(i) = t.parse::<i128>() {
                return Value::BigInt(i);
            }
            // An xsd:integer lexical that is not an integer ("1.5", "1e10",
            // "NaN", "inf", or one beyond i128) is ill-typed — it is NOT a
            // double. Fall through to a plain Typed literal (non-numeric).
        }
        // xsd:decimal is held exactly (mantissa + scale), not as lossy f64.
        if dt == xsd::DECIMAL {
            if let Some(d) = Decimal::parse(value.trim()) {
                return Value::Decimal(d);
            }
            if let Ok(f) = value.trim().parse::<f64>() {
                return Value::Double(f);
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

    /// The numeric value as `f64`, if this is a number. Lossy for big integers
    /// and decimals beyond `f64` precision — prefer [`as_i128`](Self::as_i128) /
    /// [`as_decimal`](Self::as_decimal) for exact comparison/arithmetic.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::BigInt(i) => Some(*i as f64),
            Value::Decimal(d) => Some(d.to_f64()),
            Value::Double(d) => Some(*d),
            _ => None,
        }
    }

    /// The exact integer value, if this is an integer (`xsd:integer` family).
    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Value::Int(i) => Some(*i as i128),
            Value::BigInt(i) => Some(*i),
            _ => None,
        }
    }

    /// The exact decimal value, if this is representable as a decimal (integers
    /// and `xsd:decimal`, but not `xsd:double`/`float`).
    pub fn as_decimal(&self) -> Option<Decimal> {
        match self {
            Value::Int(i) => Some(Decimal::from_i128(*i as i128)),
            Value::BigInt(i) => Some(Decimal::from_i128(*i)),
            Value::Decimal(d) => Some(*d),
            _ => None,
        }
    }

    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            Value::Int(_) | Value::BigInt(_) | Value::Decimal(_) | Value::Double(_)
        )
    }

    /// The lexical form used for `STR()` and output.
    pub fn lexical(&self) -> String {
        match self {
            Value::Iri(s) | Value::Blank(s) => s.clone(),
            Value::Int(i) => i.to_string(),
            Value::BigInt(i) => i.to_string(),
            Value::Decimal(d) => d.lexical(),
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
            Value::BigInt(i) => Some(*i != 0),
            Value::Decimal(d) => Some(!d.is_zero()),
            Value::Double(d) => Some(*d != 0.0 && !d.is_nan()),
            Value::Str { value, .. } => Some(!value.is_empty()),
            // Numeric strings stored as Typed are uncommon; IRIs/blank have no EBV.
            _ => None,
        }
    }

    /// SPARQL `=` semantics across value types. `None` means "incomparable"
    /// (a type error), which the caller maps to filter exclusion.
    pub fn sparql_eq(&self, other: &Value) -> Option<bool> {
        // Numeric equality, exact where possible: integers via i128, integers
        // and xsd:decimal via exact decimal, otherwise (any double) via f64.
        if self.is_numeric() && other.is_numeric() {
            if let (Some(a), Some(b)) = (self.as_i128(), other.as_i128()) {
                return Some(a == b);
            }
            if let (Some(a), Some(b)) = (self.as_decimal(), other.as_decimal()) {
                return Some(a == b);
            }
            if let (Some(a), Some(b)) = (self.as_f64(), other.as_f64()) {
                return Some(a == b);
            }
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
        // Numeric ordering, exact where possible: integers via i128, integers and
        // xsd:decimal via exact decimal, otherwise (any double) via f64.
        if self.is_numeric() && other.is_numeric() {
            if let (Some(a), Some(b)) = (self.as_i128(), other.as_i128()) {
                return Some(a.cmp(&b));
            }
            if let (Some(a), Some(b)) = (self.as_decimal(), other.as_decimal()) {
                if let Some(o) = a.cmp_exact(b) {
                    return Some(o);
                }
            }
            if let (Some(a), Some(b)) = (self.as_f64(), other.as_f64()) {
                return a.partial_cmp(&b);
            }
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

    /// Unary minus over the numeric tower; `None` for non-numbers. Integer
    /// negation that overflows `i64`/`i128` widens to the next representation.
    pub fn negate(&self) -> Option<Value> {
        match self {
            Value::Int(i) => Some(
                i.checked_neg()
                    .map(Value::Int)
                    .unwrap_or(Value::BigInt(-(*i as i128))),
            ),
            Value::BigInt(i) => Some(
                i.checked_neg()
                    .map(Value::BigInt)
                    .unwrap_or_else(|| Value::Double(-(*i as f64))),
            ),
            Value::Decimal(d) => Some(Value::Decimal(d.negated())),
            Value::Double(d) => Some(Value::Double(-d)),
            _ => None,
        }
    }

    /// SPARQL numeric arithmetic with XPath-style type promotion, `None` on a
    /// non-numeric operand or division by zero.
    ///
    /// `+`/`-`/`*` are exact when no `xsd:double`/`float` is involved: integer
    /// operands compute in `i128` (widening `i64`→`i128`, overflow → `f64`), and
    /// integer/decimal operands compute in exact decimal (overflow → `f64`).
    /// `/` is evaluated in `f64` (SPARQL permits implementation-defined decimal
    /// division precision), as is any operation touching a double.
    pub fn arith(&self, op: ArithOp, other: &Value) -> Option<Value> {
        if matches!(op, ArithOp::Div) {
            let (x, y) = (self.as_f64()?, other.as_f64()?);
            if y == 0.0 {
                return None;
            }
            return Some(Value::Double(x / y));
        }
        // Exact integer path (both integers, no double).
        if let (Some(x), Some(y)) = (self.as_i128(), other.as_i128()) {
            let checked = match op {
                ArithOp::Add => x.checked_add(y),
                ArithOp::Sub => x.checked_sub(y),
                ArithOp::Mul => x.checked_mul(y),
                ArithOp::Div => unreachable!(),
            };
            return Some(match checked {
                Some(v) => int_value(v),
                None => Value::Double(apply_f64(op, x as f64, y as f64)),
            });
        }
        // Exact decimal path (integers and/or xsd:decimal, no double).
        if let (Some(x), Some(y)) = (self.as_decimal(), other.as_decimal()) {
            let d = match op {
                ArithOp::Add => x.add(y),
                ArithOp::Sub => x.sub(y),
                ArithOp::Mul => x.mul(y),
                ArithOp::Div => unreachable!(),
            };
            if let Some(d) = d {
                return Some(Value::Decimal(d));
            }
            // Exact result overflowed i128 — fall through to the f64 path.
        }
        let (x, y) = (self.as_f64()?, other.as_f64()?);
        Some(Value::Double(apply_f64(op, x, y)))
    }
}

/// Narrow an exact integer to the smallest fitting [`Value`] representation.
fn int_value(v: i128) -> Value {
    match i64::try_from(v) {
        Ok(i) => Value::Int(i),
        Err(_) => Value::BigInt(v),
    }
}

/// `f64` arithmetic for the non-exact / overflow fallback (`Div` handled by the
/// caller before this is reached).
fn apply_f64(op: ArithOp, x: f64, y: f64) -> f64 {
    match op {
        ArithOp::Add => x + y,
        ArithOp::Sub => x - y,
        ArithOp::Mul => x * y,
        ArithOp::Div => x / y,
    }
}

/// A total ordering for `ORDER BY`, following SPARQL's ascending order:
/// (unbound <) blank nodes < IRIs < literals; numbers and strings by value.
/// Used on `Option<Value>` where `None` is "unbound".
pub fn order_key(v: &Option<Value>) -> impl Ord {
    // Returns (group, num, int, text) — compared lexicographically by derive(Ord).
    // group: 0 unbound, 1 blank, 2 iri, 3 numeric, 4 boolean, 5 string/other.
    // `int` is an exact i128 tie-breaker so integers beyond f64's 2^53 exact range
    // keep their true ordering when their f64 approximations collide; it is 0 for
    // non-integers (decimals fall back to the f64 key, adequate for ORDER BY).
    match v {
        None => (0u8, OrdF64(f64::NEG_INFINITY), 0i128, String::new()),
        Some(Value::Blank(s)) => (1, OrdF64(0.0), 0, s.clone()),
        Some(Value::Iri(s)) => (2, OrdF64(0.0), 0, s.clone()),
        Some(Value::Int(i)) => (3, OrdF64(*i as f64), *i as i128, String::new()),
        Some(Value::BigInt(i)) => (3, OrdF64(*i as f64), *i, String::new()),
        Some(Value::Decimal(d)) => (3, OrdF64(d.to_f64()), 0, String::new()),
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

/// An exact decimal number: `mant × 10⁻ˢᶜᵃˡᵉ`. Represents `xsd:decimal` values
/// precisely for magnitudes within `i128` (~38 significant digits); an arithmetic
/// result that would overflow `i128` falls back to `f64`. A deliberate,
/// documented bound — not an arbitrary-precision type — to stay dependency-free.
#[derive(Debug, Clone, Copy)]
pub struct Decimal {
    /// Signed significant digits.
    mant: i128,
    /// Number of digits to the right of the decimal point.
    scale: u32,
}

impl Decimal {
    fn from_i128(v: i128) -> Decimal {
        Decimal { mant: v, scale: 0 }
    }

    fn to_f64(self) -> f64 {
        self.mant as f64 / 10f64.powi(self.scale as i32)
    }

    fn is_zero(self) -> bool {
        self.mant == 0
    }

    fn negated(self) -> Decimal {
        Decimal {
            mant: self.mant.wrapping_neg(),
            scale: self.scale,
        }
    }

    /// Parse an `xsd:decimal` lexical (`-?(\d+(\.\d*)?|\.\d+)`); `None` for an
    /// empty, exponent-bearing (that is `xsd:double`), or otherwise malformed
    /// string.
    fn parse(s: &str) -> Option<Decimal> {
        let s = s.trim();
        let (neg, rest) = match s.as_bytes().first()? {
            b'+' => (false, &s[1..]),
            b'-' => (true, &s[1..]),
            _ => (false, s),
        };
        let (int_part, frac_part) = match rest.split_once('.') {
            Some((i, f)) => (i, f),
            None => (rest, ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }
        if !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let digits = format!("{int_part}{frac_part}");
        let mut mant: i128 = if digits.is_empty() { 0 } else { digits.parse().ok()? };
        if neg {
            mant = -mant;
        }
        Some(Decimal {
            mant,
            scale: frac_part.len() as u32,
        })
    }

    /// The lexical form (`123`, `1.50`, `-0.007`), preserving the scale.
    fn lexical(self) -> String {
        if self.scale == 0 {
            return self.mant.to_string();
        }
        let neg = self.mant < 0;
        let digits = self.mant.unsigned_abs().to_string();
        let scale = self.scale as usize;
        let body = if digits.len() > scale {
            let (int, frac) = digits.split_at(digits.len() - scale);
            format!("{int}.{frac}")
        } else {
            format!("0.{digits:0>scale$}")
        };
        if neg {
            format!("-{body}")
        } else {
            body
        }
    }

    /// The mantissa re-expressed at a larger `target` scale, or `None` on i128
    /// overflow.
    fn rescaled(self, target: u32) -> Option<i128> {
        let factor = 10i128.checked_pow(target - self.scale)?;
        self.mant.checked_mul(factor)
    }

    /// Exact three-way comparison; `None` only if aligning scales overflows i128.
    fn cmp_exact(self, other: Decimal) -> Option<Ordering> {
        let target = self.scale.max(other.scale);
        Some(self.rescaled(target)?.cmp(&other.rescaled(target)?))
    }

    fn add(self, other: Decimal) -> Option<Decimal> {
        let target = self.scale.max(other.scale);
        Some(Decimal {
            mant: self.rescaled(target)?.checked_add(other.rescaled(target)?)?,
            scale: target,
        })
    }

    fn sub(self, other: Decimal) -> Option<Decimal> {
        let target = self.scale.max(other.scale);
        Some(Decimal {
            mant: self.rescaled(target)?.checked_sub(other.rescaled(target)?)?,
            scale: target,
        })
    }

    fn mul(self, other: Decimal) -> Option<Decimal> {
        Some(Decimal {
            mant: self.mant.checked_mul(other.mant)?,
            scale: self.scale + other.scale,
        })
    }
}

/// Decimal equality is by numeric value, so `1.0 == 1.00` (and it composes into
/// `Value`'s derived `PartialEq`).
impl PartialEq for Decimal {
    fn eq(&self, other: &Self) -> bool {
        self.cmp_exact(*other) == Some(Ordering::Equal)
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

/// The decomposed fields of a parsed `xsd:dateTime` / `xsd:date`. Powers the
/// SPARQL date accessors (`YEAR`, `MONTH`, …, `TIMEZONE`, `TZ`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DateTimeParts {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    /// Fractional seconds in `[0, 1)`.
    pub frac: f64,
    /// Timezone offset east of UTC in seconds, or `None` when no zone was given.
    pub tz_offset_secs: Option<i64>,
}

/// Parse `-?YYYY-MM-DD(Thh:mm:ss(.s+)?)?(Z|±hh:mm)?` into its fields.
pub fn parse_datetime_parts(s: &str) -> Option<DateTimeParts> {
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

    let mut tz_offset_secs = None; // seconds east of UTC
    if i < b.len() {
        match b[i] {
            b'Z' => {
                i += 1;
                tz_offset_secs = Some(0);
            }
            b'+' | b'-' => {
                let sign = if b[i] == b'+' { 1 } else { -1 };
                i += 1;
                let th = take_n_digits(b, &mut i, 2)?;
                expect_byte(b, &mut i, b':')?;
                let tm = take_n_digits(b, &mut i, 2)?;
                tz_offset_secs = Some(sign * (th * 3600 + tm * 60));
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
    Some(DateTimeParts {
        year,
        month,
        day,
        hour: hh,
        minute: mm,
        second: ss,
        frac,
        tz_offset_secs,
    })
}

/// Parse `-?YYYY-MM-DD(Thh:mm:ss(.s+)?)?(Z|±hh:mm)?` to a UTC instant. A missing
/// timezone is treated as UTC.
fn parse_xsd_datetime(s: &str) -> Option<f64> {
    let p = parse_datetime_parts(s)?;
    let days = days_from_civil(p.year, p.month, p.day);
    let tz = p.tz_offset_secs.unwrap_or(0);
    let secs = days * 86_400 + p.hour * 3600 + p.minute * 60 + p.second - tz;
    Some(secs as f64 + p.frac)
}

/// Civil (year, month, day) from a count of days since 1970-01-01 (Howard
/// Hinnant's inverse of [`days_from_civil`]).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Format a UTC instant (seconds since the epoch) as an `xsd:dateTime` lexical
/// `YYYY-MM-DDThh:mm:ssZ`. Used by `NOW()`.
pub fn format_xsd_datetime_utc(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let rem = unix_secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
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
    fn invalid_integer_lexical_is_not_a_double() {
        // An ill-typed xsd:integer ("1.5", "1e10", "NaN") must not be accepted as
        // a numeric double — it stays a (non-numeric) typed literal.
        for bad in ["1.5", "1e10", "NaN", "inf", "  "] {
            let v = Value::from_term(&Term::typed_literal(bad, xsd::INTEGER));
            assert!(!v.is_numeric(), "{bad:?} must not be numeric, got {v:?}");
            assert!(matches!(v, Value::Typed { .. }), "{bad:?} → {v:?}");
        }
        // A valid one is still an integer.
        assert_eq!(Value::from_term(&Term::typed_literal("42", xsd::INTEGER)), Value::Int(42));
    }

    #[test]
    fn big_integer_beyond_i64_is_exact() {
        let big = "9223372036854775808"; // i64::MAX + 1
        let v = Value::from_term(&Term::typed_literal(big, xsd::INTEGER));
        assert_eq!(v, Value::BigInt(9_223_372_036_854_775_808i128));
        assert_eq!(v.lexical(), big);
        // Exact ordering vs i64::MAX (whose f64 approximations would collide).
        let max = Value::Int(i64::MAX);
        assert_eq!(max.sparql_cmp(&v), Some(Ordering::Less));
        assert_eq!(max.sparql_eq(&v), Some(false));
    }

    #[test]
    fn decimal_is_exact_not_f64() {
        let a = Value::from_term(&Term::typed_literal("0.1", xsd::DECIMAL));
        let b = Value::from_term(&Term::typed_literal("0.2", xsd::DECIMAL));
        let sum = a.arith(ArithOp::Add, &b).unwrap();
        // Exactly 0.3, unlike f64's 0.1 + 0.2 == 0.30000000000000004.
        assert_eq!(sum, Value::from_term(&Term::typed_literal("0.3", xsd::DECIMAL)));
        assert_eq!(sum.lexical(), "0.3");
    }

    #[test]
    fn decimal_equality_ignores_trailing_zeros() {
        let a = Value::from_term(&Term::typed_literal("1.0", xsd::DECIMAL));
        let b = Value::from_term(&Term::typed_literal("1.00", xsd::DECIMAL));
        assert_eq!(a.sparql_eq(&b), Some(true));
        assert_eq!(a.sparql_cmp(&b), Some(Ordering::Equal));
        assert_eq!(a.sparql_eq(&Value::Int(1)), Some(true)); // decimal == integer
    }

    #[test]
    fn integer_arithmetic_widens_on_overflow() {
        let sum = Value::Int(i64::MAX).arith(ArithOp::Add, &Value::Int(1)).unwrap();
        assert_eq!(sum, Value::BigInt(i64::MAX as i128 + 1));
        let prod = Value::BigInt(10_000_000_000)
            .arith(ArithOp::Mul, &Value::Int(10_000_000_000))
            .unwrap();
        assert_eq!(prod, Value::BigInt(100_000_000_000_000_000_000i128));
    }

    #[test]
    fn mixed_integer_decimal_compare_and_arith() {
        let i = Value::Int(3);
        let d = Value::from_term(&Term::typed_literal("3.5", xsd::DECIMAL));
        assert_eq!(i.sparql_cmp(&d), Some(Ordering::Less));
        // integer + decimal → exact decimal.
        assert_eq!(
            i.arith(ArithOp::Add, &d).unwrap(),
            Value::from_term(&Term::typed_literal("6.5", xsd::DECIMAL))
        );
        // any double operand promotes the result to double.
        assert!(matches!(
            i.arith(ArithOp::Add, &Value::Double(0.5)),
            Some(Value::Double(_))
        ));
    }

    #[test]
    fn decimal_negation_and_lexical() {
        let d = Value::from_term(&Term::typed_literal("-0.007", xsd::DECIMAL));
        assert_eq!(d.lexical(), "-0.007");
        assert_eq!(d.negate().unwrap().lexical(), "0.007");
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
    fn datetime_parts_extracts_fields() {
        let p = parse_datetime_parts("2011-01-10T14:45:13.815-05:00").unwrap();
        assert_eq!((p.year, p.month, p.day), (2011, 1, 10));
        assert_eq!((p.hour, p.minute, p.second), (14, 45, 13));
        assert!((p.frac - 0.815).abs() < 1e-9);
        assert_eq!(p.tz_offset_secs, Some(-5 * 3600));
    }

    #[test]
    fn datetime_parts_missing_zone_is_none() {
        let p = parse_datetime_parts("2020-03-04T05:06:07").unwrap();
        assert_eq!(p.tz_offset_secs, None);
        let z = parse_datetime_parts("2020-03-04T05:06:07Z").unwrap();
        assert_eq!(z.tz_offset_secs, Some(0));
    }

    #[test]
    fn format_then_parse_round_trips() {
        // 2021-06-30T12:34:56Z is a fixed instant; format and re-parse it.
        let secs = parse_xsd_datetime("2021-06-30T12:34:56Z").unwrap() as i64;
        let s = format_xsd_datetime_utc(secs);
        assert_eq!(s, "2021-06-30T12:34:56Z");
        assert_eq!(parse_xsd_datetime(&s), Some(secs as f64));
        assert_eq!(format_xsd_datetime_utc(0), "1970-01-01T00:00:00Z");
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
