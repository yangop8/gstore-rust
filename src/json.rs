//! A small, dependency-free JSON value with a parser and a serializer.
//!
//! The crate hand-writes its parsers (no `serde_json`), and several modules
//! need JSON: the JSON-LD reader ([`crate::parser::jsonld`]) and the
//! upstream-compatible `/api` layer ([`crate::gapi`]), whose requests and
//! responses are JSON documents. This module is the one shared implementation.
//!
//! * [`Json`] keeps **object entries in insertion (document) order** and
//!   **numbers in their lexical form**, so an integer stays an integer and a
//!   parsed document round-trips without loss. [`Json::sorted`] produces the
//!   key-sorted form that upstream gStore emits (its `nlohmann::json` objects
//!   are `std::map`s), for byte-level parity of `/api` responses.
//! * [`parse`] accepts RFC 8259 JSON (one value, surrounded by optional
//!   whitespace and an optional UTF-8 BOM). It is lenient in exactly one way:
//!   raw control characters inside strings are accepted as-is. Nesting deeper
//!   than [`MAX_DEPTH`] is refused (guards the recursive reader and the
//!   recursive drop against stack exhaustion on adversarial input).
//! * [`Json::to_string`] / [`Display`](std::fmt::Display) serialize compactly
//!   (no whitespace, like `nlohmann::json::dump()`), escaping `"`, `\` and the
//!   control characters (`\b \f \n \r \t`, others as `\u00XX`); everything else
//!   — including non-ASCII text — is written as UTF-8.

use std::fmt;

/// Maximum nesting depth (arrays/objects) accepted by [`parse`].
pub const MAX_DEPTH: usize = 256;

/// A JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// A number, kept in its (validated) lexical form.
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    /// An object. Entries keep insertion order. Objects built with
    /// [`Json::set`] have unique keys; a parsed document with a duplicated
    /// key keeps every entry, and [`Json::get`] returns the last one (the
    /// "last wins" rule most JSON libraries apply).
    Obj(Vec<(String, Json)>),
}

/// A [`parse`] failure, with the 1-based line it occurred on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid JSON at line {}: {}", self.line, self.msg)
    }
}

impl std::error::Error for JsonError {}

impl Json {
    /// An empty object.
    pub fn obj() -> Json {
        Json::Obj(Vec::new())
    }

    /// An empty array.
    pub fn arr() -> Json {
        Json::Arr(Vec::new())
    }

    /// Set `key` to `value` on an object: an existing entry is replaced in
    /// place (keeping its position), otherwise the entry is appended. A
    /// non-object value is first replaced by an empty object.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<Json>) -> &mut Json {
        if !matches!(self, Json::Obj(_)) {
            *self = Json::obj();
        }
        let key = key.into();
        let value = value.into();
        if let Json::Obj(entries) = self {
            match entries.iter_mut().rev().find(|(k, _)| *k == key) {
                Some(slot) => slot.1 = value,
                None => entries.push((key, value)),
            }
        }
        self
    }

    /// Builder form of [`set`](Self::set).
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Json>) -> Json {
        self.set(key, value);
        self
    }

    /// Append `value` to an array (a non-array value is first replaced by an
    /// empty array).
    pub fn push(&mut self, value: impl Into<Json>) {
        if !matches!(self, Json::Arr(_)) {
            *self = Json::arr();
        }
        if let Json::Arr(items) = self {
            items.push(value.into());
        }
    }

    /// The value of `key` on an object (the last entry if it is duplicated).
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(entries) => entries.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Mutable access to the value of `key` on an object.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Obj(entries) => entries
                .iter_mut()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Remove every entry named `key` from an object, returning the last one.
    pub fn remove(&mut self, key: &str) -> Option<Json> {
        match self {
            Json::Obj(entries) => {
                let mut out = None;
                entries.retain_mut(|(k, v)| {
                    if k == key {
                        out = Some(std::mem::replace(v, Json::Null));
                        false
                    } else {
                        true
                    }
                });
                out
            }
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// A number that is an exact integer in `i64` range (`3`, `-7`, `2.0`,
    /// `1e3`).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Num(n) => n.parse::<i64>().ok().or_else(|| {
                let f = n.parse::<f64>().ok()?;
                (f.fract() == 0.0 && f.abs() < 9.0e18).then_some(f as i64)
            }),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(n) => n
                .parse::<u64>()
                .ok()
                .or_else(|| u64::try_from(self.as_i64()?).ok()),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => n.parse::<f64>().ok(),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(entries) => Some(entries),
            _ => None,
        }
    }

    /// A copy with every object's keys sorted (byte-wise, recursively; a
    /// duplicated key keeps only its last value). This is the order upstream
    /// gStore's `nlohmann::json` (a `std::map`) serializes objects in.
    pub fn sorted(&self) -> Json {
        self.clone().into_sorted()
    }

    /// [`sorted`](Self::sorted), consuming the value — no deep copy, which
    /// matters for a large query result about to be serialized.
    pub fn into_sorted(self) -> Json {
        match self {
            Json::Arr(items) => Json::Arr(items.into_iter().map(Json::into_sorted).collect()),
            Json::Obj(mut entries) => {
                // A stable sort keeps equal keys in document order, so the
                // last of each run is the one that wins: O(n log n), so a
                // wide row object stays cheap.
                entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                let mut out: Vec<(String, Json)> = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    match out.last_mut() {
                        Some(last) if last.0 == k => last.1 = v,
                        _ => out.push((k, v)),
                    }
                }
                Json::Obj(out.into_iter().map(|(k, v)| (k, v.into_sorted())).collect())
            }
            other => other,
        }
    }

    /// Append the compact serialization of this value to `out`.
    pub fn write_to(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Num(n) => out.push_str(n),
            Json::Str(s) => write_str(s, out),
            Json::Arr(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_to(out);
                }
                out.push(']');
            }
            Json::Obj(entries) => {
                out.push('{');
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str(k, out);
                    out.push(':');
                    v.write_to(out);
                }
                out.push('}');
            }
        }
    }
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = String::new();
        self.write_to(&mut s);
        f.write_str(&s)
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Json {
        Json::Str(s.to_string())
    }
}

impl From<String> for Json {
    fn from(s: String) -> Json {
        Json::Str(s)
    }
}

impl From<&String> for Json {
    fn from(s: &String) -> Json {
        Json::Str(s.clone())
    }
}

impl From<bool> for Json {
    fn from(b: bool) -> Json {
        Json::Bool(b)
    }
}

macro_rules! json_from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Json {
            fn from(n: $t) -> Json {
                Json::Num(n.to_string())
            }
        }
    )*};
}
json_from_int!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

impl From<f64> for Json {
    /// A finite double in its shortest round-trip form; NaN and the
    /// infinities (which JSON cannot represent) become `null`.
    fn from(f: f64) -> Json {
        if f.is_finite() {
            Json::Num(format!("{f}"))
        } else {
            Json::Null
        }
    }
}

impl From<Vec<Json>> for Json {
    fn from(items: Vec<Json>) -> Json {
        Json::Arr(items)
    }
}

impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(v: Option<T>) -> Json {
        v.map_or(Json::Null, Into::into)
    }
}

/// `s` as a quoted, escaped JSON string literal.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    write_str(s, &mut out);
    out
}

/// Append `s` as a quoted, escaped JSON string literal to `out`.
pub fn write_str(s: &str, out: &mut String) {
    out.push('"');
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let esc: Option<&str> = match c {
            '"' => Some("\\\""),
            '\\' => Some("\\\\"),
            '\n' => Some("\\n"),
            '\r' => Some("\\r"),
            '\t' => Some("\\t"),
            '\u{08}' => Some("\\b"),
            '\u{0C}' => Some("\\f"),
            c if (c as u32) < 0x20 => None,
            _ => continue,
        };
        out.push_str(&s[start..i]);
        match esc {
            Some(e) => out.push_str(e),
            None => out.push_str(&format!("\\u{:04x}", c as u32)),
        }
        start = i + c.len_utf8();
    }
    out.push_str(&s[start..]);
    out.push('"');
}

/// Parse one JSON document.
pub fn parse(text: &str) -> Result<Json, JsonError> {
    let mut p = Parser {
        src: text,
        b: text.as_bytes(),
        pos: 0,
    };
    if text.starts_with('\u{FEFF}') {
        p.pos = '\u{FEFF}'.len_utf8();
    }
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.pos < p.b.len() {
        return Err(p.err("trailing content after JSON value"));
    }
    Ok(v)
}

struct Parser<'a> {
    src: &'a str,
    b: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn err(&self, msg: impl Into<String>) -> JsonError {
        let end = self.pos.min(self.b.len());
        let line = self.b[..end].iter().filter(|&&c| c == b'\n').count() + 1;
        JsonError {
            line,
            msg: msg.into(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, JsonError> {
        if depth > MAX_DEPTH {
            return Err(self.err("JSON nesting too deep"));
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.word("true", Json::Bool(true)),
            Some(b'f') => self.word("false", Json::Bool(false)),
            Some(b'n') => self.word("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(self.err("expected a JSON value")),
            None => Err(self.err("unexpected end of input, expected a JSON value")),
        }
    }

    fn word(&mut self, w: &str, v: Json) -> Result<Json, JsonError> {
        if self.b[self.pos..].starts_with(w.as_bytes()) {
            self.pos += w.len();
            Ok(v)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.pos += 1; // '{'
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(entries));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected a string key in object"));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(self.err("expected ':' after object key"));
            }
            self.pos += 1;
            let v = self.value(depth + 1)?;
            entries.push((key, v));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(entries));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.pos += 1; // opening '"'
        let mut out = String::new();
        let mut run = self.pos; // start of the current unescaped run
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    out.push_str(&self.src[run..self.pos]);
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    out.push_str(&self.src[run..self.pos]);
                    self.pos += 1;
                    let c = self
                        .peek()
                        .ok_or_else(|| self.err("dangling backslash in string"))?;
                    self.pos += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let cp = self.hex4()?;
                            let ch = if (0xD800..=0xDBFF).contains(&cp) {
                                if !self.b[self.pos..].starts_with(b"\\u") {
                                    return Err(self.err("unpaired high surrogate"));
                                }
                                self.pos += 2;
                                let lo = self.hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&lo) {
                                    return Err(self.err("invalid low surrogate"));
                                }
                                char::from_u32(0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00))
                            } else {
                                char::from_u32(cp)
                            };
                            out.push(ch.ok_or_else(|| self.err("invalid \\u code point"))?);
                        }
                        other => {
                            return Err(self.err(format!(
                                "invalid escape '\\{}'",
                                char::from(other).escape_default()
                            )))
                        }
                    }
                    run = self.pos;
                }
                // Multi-byte UTF-8 sequences never contain '"' or '\\' bytes,
                // so byte-wise scanning keeps `run..pos` on char boundaries.
                Some(_) => self.pos += 1,
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let digits = self
            .b
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| self.err("truncated \\u escape"))?;
        let mut code = 0u32;
        for &d in digits {
            let v = char::from(d)
                .to_digit(16)
                .ok_or_else(|| self.err("bad hex digit in \\u escape"))?;
            code = code * 16 + v;
        }
        self.pos += 4;
        Ok(code)
    }

    /// `-? (0 | [1-9][0-9]*) (. [0-9]+)? ([eE] [+-]? [0-9]+)?`
    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.pos;
        let digits = |p: &mut Self| {
            let s = p.pos;
            while matches!(p.peek(), Some(c) if c.is_ascii_digit()) {
                p.pos += 1;
            }
            p.pos - s
        };
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(c) if c.is_ascii_digit() => {
                digits(self);
            }
            _ => return Err(self.err("invalid number: expected a digit")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if digits(self) == 0 {
                return Err(self.err("invalid number: expected a digit after '.'"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if digits(self) == 0 {
                return Err(self.err("invalid number: expected an exponent digit"));
            }
        }
        // A number must be followed by a delimiter, so `1-2` or `1.2.3` is an
        // error here rather than a confusing one later.
        if matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'+'))
        {
            return Err(self.err("invalid number literal"));
        }
        Ok(Json::Num(self.src[start..self.pos].to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_value_kind() {
        let v = parse(r#" { "a": [1, -2.5e3, true, false, null], "b": {"c": "d"} } "#).unwrap();
        assert_eq!(v.get("a").unwrap().as_array().unwrap().len(), 5);
        assert_eq!(v.get("a").unwrap().as_array().unwrap()[0].as_i64(), Some(1));
        assert_eq!(
            v.get("a").unwrap().as_array().unwrap()[1].as_f64(),
            Some(-2500.0)
        );
        assert_eq!(v.get("b").unwrap().get("c").unwrap().as_str(), Some("d"));
        assert_eq!(parse("\u{FEFF}[]").unwrap(), Json::arr());
    }

    #[test]
    fn numbers_keep_their_lexical_form() {
        let v = parse("[10, 1.50, 1e2, -0]").unwrap();
        assert_eq!(v.to_string(), "[10,1.50,1e2,-0]");
        assert_eq!(v.as_array().unwrap()[2].as_i64(), Some(100));
        for bad in [
            "01", "1.", ".5", "-", "1-2", "1.2.3", "+1", "1e", "0x10", "1e+",
        ] {
            assert!(parse(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn strings_unescape_and_reescape() {
        let v = parse(r#""a\"b\\c\/d\n\té😀\u0001""#).unwrap();
        assert_eq!(v.as_str(), Some("a\"b\\c/d\n\té😀\u{1}"));
        assert_eq!(v.to_string(), "\"a\\\"b\\\\c/d\\n\\té😀\\u0001\"");
        assert!(parse(r#""\ud83d""#).is_err());
        assert!(parse(r#""\x""#).is_err());
        assert!(parse(r#""abc"#).is_err());
        // Lenient: a raw control character inside a string.
        assert_eq!(parse("\"a\tb\"").unwrap().as_str(), Some("a\tb"));
    }

    #[test]
    fn rejects_malformed_documents_with_a_line() {
        let e = parse("{\n\"a\": 1,\n}").unwrap_err();
        assert_eq!(e.line, 3, "{e}");
        assert!(parse("").is_err());
        assert!(parse("[1] 2").is_err());
        assert!(parse("{\"a\" 1}").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse(&"[".repeat(MAX_DEPTH + 2)).is_err());
        assert!(parse(&format!("{}{}", "[".repeat(100), "]".repeat(100))).is_ok());
    }

    #[test]
    fn objects_keep_insertion_order_and_last_duplicate_wins() {
        let mut o = Json::obj().with("z", 1).with("a", "x");
        o.set("z", 2);
        assert_eq!(o.to_string(), r#"{"z":2,"a":"x"}"#);
        let d = parse(r#"{"k":1,"k":2}"#).unwrap();
        assert_eq!(d.get("k").and_then(Json::as_i64), Some(2));
        assert_eq!(d.sorted().to_string(), r#"{"k":2}"#);
        let d = parse(r#"{"b":1,"a":1,"c":0,"b":2,"a":3}"#).unwrap();
        assert_eq!(d.sorted().to_string(), r#"{"a":3,"b":2,"c":0}"#);
        assert_eq!(o.remove("z"), Some(Json::from(2)));
        assert_eq!(o.to_string(), r#"{"a":"x"}"#);
    }

    #[test]
    fn sorted_orders_keys_bytewise_like_std_map() {
        let v = Json::obj()
            .with("results", Json::obj().with("b", 1).with("a", 2))
            .with("StatusMsg", "ok")
            .with("AnsNum", 3)
            .with("head", Json::from(vec![Json::from("y"), Json::from("x")]));
        assert_eq!(
            v.sorted().to_string(),
            r#"{"AnsNum":3,"StatusMsg":"ok","head":["y","x"],"results":{"a":2,"b":1}}"#
        );
        assert_eq!(v.clone().into_sorted(), v.sorted());
        let nested = parse(r#"[{"b":{"z":1,"y":2,"z":3},"a":0}]"#).unwrap();
        assert_eq!(
            nested.into_sorted().to_string(),
            r#"[{"a":0,"b":{"y":2,"z":3}}]"#
        );
    }

    #[test]
    fn conversions() {
        assert_eq!(Json::from(f64::NAN), Json::Null);
        assert_eq!(Json::from(0.5).to_string(), "0.5");
        assert_eq!(Json::from(None::<i32>), Json::Null);
        assert_eq!(Json::from(Some("s")).as_str(), Some("s"));
        assert_eq!(Json::from(u64::MAX).as_u64(), Some(u64::MAX));
        assert_eq!(Json::Num("2.0".into()).as_i64(), Some(2));
        assert_eq!(Json::Num("2.5".into()).as_i64(), None);
        assert_eq!(quote("\u{7f}é"), "\"\u{7f}é\"");
    }
}
