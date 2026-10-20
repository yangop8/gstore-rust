//! Request parameters of the `/api` protocol: one merged view of the URL query
//! string and the request body.
//!
//! Upstream clients send parameters in any of these shapes, and an operation
//! must not care which (upstream `parseRequest` in `src/Main/grpc.cpp`):
//!
//! * a **JSON object** body — `Content-Type: application/json`, or any body
//!   whose first non-blank character is `{` (so `curl -d '{…}'`, which labels
//!   its body `x-www-form-urlencoded`, works too). This is what the SDKs send
//!   for `POST` (e.g. `GstoreConnector.py`'s `requests.post(json=…)`), and what
//!   the upstream test scripts send;
//! * an `application/x-www-form-urlencoded` body;
//! * a `multipart/form-data` body (form fields, plus uploaded files — a part
//!   named `file` also sets the `filename` parameter, like upstream);
//! * the URL query string (`GET /api?operation=…&…`).
//!
//! The URL query and the body are merged; on a duplicated name the body wins
//! (the last occurrence wins in general). Values keep their JSON type
//! ([`Json`]) — `"tid": 1` and `"tid": "1"` both read as `"1"` through
//! [`Params::str`] — and form / query values are strings.

use crate::json::{self, Json};
use crate::server::Request;

/// A file sent in a `multipart/form-data` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedFile {
    /// The form field (`name=`) it was sent under.
    pub field: String,
    /// The client-side file name (`filename=`), possibly empty.
    pub filename: String,
    /// The part's `Content-Type`, if given.
    pub content_type: Option<String>,
    pub data: Vec<u8>,
}

/// The merged request parameters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Params {
    entries: Vec<(String, Json)>,
    files: Vec<UploadedFile>,
}

impl Params {
    /// No parameters.
    pub fn new() -> Params {
        Params::default()
    }

    /// The parameters of an HTTP request (its URL query plus its body).
    pub(crate) fn from_request(req: &Request) -> Result<Params, String> {
        Params::from_parts(req.query.as_deref(), req.header("content-type"), &req.body)
    }

    /// Parameters from a raw URL query string (without the `?`), the body's
    /// `Content-Type` and the body. Errors describe a malformed body (invalid
    /// JSON, a JSON body that is not an object, broken multipart framing).
    pub fn from_parts(
        query: Option<&str>,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<Params, String> {
        let mut p = Params::new();
        if let Some(q) = query {
            for (k, v) in parse_form(q) {
                p.set(k, v);
            }
        }
        let media = content_type
            .and_then(|c| c.split(';').next())
            .map(|m| m.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let text = String::from_utf8_lossy(body);
        let trimmed = text.trim_start_matches('\u{FEFF}').trim();
        if media == "multipart/form-data" {
            let ctype = content_type.unwrap_or_default();
            let (fields, files) = parse_multipart(ctype, body)?;
            for (k, v) in fields {
                p.set(k, v);
            }
            for f in files {
                if f.field == "file" && !p.has("filename") {
                    p.set("filename", f.filename.clone());
                }
                p.files.push(f);
            }
        } else if trimmed.is_empty() {
            // No body parameters.
        } else if media == "application/json" || trimmed.starts_with('{') {
            match json::parse(trimmed) {
                Ok(Json::Obj(entries)) => {
                    for (k, v) in entries {
                        p.set(k, v);
                    }
                }
                Ok(_) => return Err("the request body must be a JSON object".to_string()),
                Err(e) => return Err(format!("the request body is not valid JSON ({e})")),
            }
        } else if media == "application/x-www-form-urlencoded" || trimmed.contains('=') {
            for (k, v) in parse_form(trimmed) {
                p.set(k, v);
            }
        }
        // Any other body (e.g. raw text) carries no parameters.
        Ok(p)
    }

    /// Parameters from a JSON object (non-objects give no parameters).
    pub fn from_json(obj: &Json) -> Params {
        let mut p = Params::new();
        if let Some(entries) = obj.as_object() {
            for (k, v) in entries {
                p.set(k.clone(), v.clone());
            }
        }
        p
    }

    /// Set (append) a parameter; a later value for the same name wins.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<Json>) {
        self.entries.push((key.into(), value.into()));
    }

    /// The raw value of `key` (the last one when repeated).
    pub fn raw(&self, key: &str) -> Option<&Json> {
        self.entries
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    /// Whether `key` was sent at all (even empty or `null`).
    pub fn has(&self, key: &str) -> bool {
        self.raw(key).is_some()
    }

    /// `key` as a string: a string as-is, a number in its lexical form, a
    /// boolean as `"true"`/`"false"`. `None` when absent, `null`, an array or
    /// an object.
    pub fn str(&self, key: &str) -> Option<String> {
        match self.raw(key)? {
            Json::Str(s) => Some(s.clone()),
            Json::Num(n) => Some(n.clone()),
            Json::Bool(b) => Some(b.to_string()),
            Json::Null | Json::Arr(_) | Json::Obj(_) => None,
        }
    }

    /// [`str`](Self::str), but `None` for an empty (or all-blank) value too.
    pub fn non_empty(&self, key: &str) -> Option<String> {
        self.str(key).filter(|s| !s.trim().is_empty())
    }

    /// [`str`](Self::str) with a default for an absent value.
    pub fn str_or(&self, key: &str, default: &str) -> String {
        self.str(key).unwrap_or_else(|| default.to_string())
    }

    /// `key` as a flag, upstream's `jsonBoolParam`: a JSON boolean; the
    /// strings `"true"` / `"1"` (anything else is `false`); the number `1`.
    /// `default` when absent.
    pub fn bool(&self, key: &str, default: bool) -> bool {
        match self.raw(key) {
            None | Some(Json::Null) => default,
            Some(Json::Bool(b)) => *b,
            Some(Json::Str(s)) => matches!(s.trim(), "true" | "1"),
            Some(n @ Json::Num(_)) => n.as_i64() == Some(1),
            Some(_) => false,
        }
    }

    /// `key` as an integer (a JSON number or a numeric string). `Ok(None)`
    /// when absent or empty; `Err` (a message naming `key`) when it is not
    /// an integer.
    pub fn int(&self, key: &str) -> Result<Option<i64>, String> {
        let bad = || format!("the value of {key} must be an integer");
        match self.raw(key) {
            None | Some(Json::Null) => Ok(None),
            Some(n @ Json::Num(_)) => n.as_i64().map(Some).ok_or_else(bad),
            Some(Json::Str(s)) if s.trim().is_empty() => Ok(None),
            Some(Json::Str(s)) => s.trim().parse::<i64>().map(Some).map_err(|_| bad()),
            Some(_) => Err(bad()),
        }
    }

    /// `key` as a list of strings: a JSON array of scalars (each converted as
    /// by [`str`](Self::str); empty items dropped), or a single non-empty
    /// scalar as a one-element list. Empty when absent.
    pub fn strings(&self, key: &str) -> Vec<String> {
        let scalar = |v: &Json| match v {
            Json::Str(s) => Some(s.clone()),
            Json::Num(n) => Some(n.clone()),
            Json::Bool(b) => Some(b.to_string()),
            _ => None,
        };
        match self.raw(key) {
            Some(Json::Arr(items)) => items
                .iter()
                .filter_map(scalar)
                .filter(|s| !s.is_empty())
                .collect(),
            Some(v) => scalar(v).filter(|s| !s.is_empty()).into_iter().collect(),
            None => Vec::new(),
        }
    }

    /// The uploaded file sent under form field `field`.
    pub fn file(&self, field: &str) -> Option<&UploadedFile> {
        self.files.iter().rev().find(|f| f.field == field)
    }

    /// Every uploaded file, in request order.
    pub fn files(&self) -> &[UploadedFile] {
        &self.files
    }

    /// The parameter names, in request order (repeated names repeat).
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }

    /// The parameters as one JSON object (last value per name), with any
    /// `password` value redacted — for logging.
    pub fn to_redacted_json(&self) -> Json {
        let mut out = Json::obj();
        for (k, v) in &self.entries {
            if k == "password" || k == "op_password" {
                out.set(k.clone(), "***");
            } else {
                out.set(k.clone(), v.clone());
            }
        }
        out
    }
}

/// Decode an `application/x-www-form-urlencoded` string (also a URL query):
/// `&`-separated `name=value` pairs, `+` as a space, `%XX` escapes. A pair
/// without `=` is a name with an empty value; empty names are skipped.
pub fn parse_form(s: &str) -> Vec<(String, String)> {
    s.split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let k = url_decode(k);
            (!k.is_empty()).then(|| (k, url_decode(v)))
        })
        .collect()
}

/// Percent-decode `s` (`+` decodes to a space; a malformed `%` escape is kept
/// literally). Invalid UTF-8 is replaced with U+FFFD.
pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => match (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push((h << 4) | l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    char::from(b).to_digit(16).map(|d| d as u8)
}

/// Parse a `multipart/form-data` body (RFC 7578): text fields and files.
/// The text fields and the files of a multipart body.
type MultipartParts = (Vec<(String, String)>, Vec<UploadedFile>);

fn parse_multipart(content_type: &str, body: &[u8]) -> Result<MultipartParts, String> {
    let boundary = content_type
        .split(';')
        .skip(1)
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("boundary"))
        .map(|(_, v)| v.trim().trim_matches('"').to_string())
        .filter(|b| !b.is_empty())
        .ok_or_else(|| "multipart/form-data without a boundary".to_string())?;
    let delim = format!("--{boundary}").into_bytes();
    let mut fields = Vec::new();
    let mut files = Vec::new();
    let mut pos = find(body, &delim, 0).ok_or("multipart body without its boundary")? + delim.len();
    loop {
        // After a delimiter: `--` ends the body, otherwise CRLF and a part.
        if body[pos..].starts_with(b"--") {
            break;
        }
        pos = skip_line_end(body, pos);
        let next = find(body, &delim, pos).ok_or("unterminated multipart part")?;
        // The part's content ends with the CRLF preceding the delimiter.
        let mut end = next;
        if end >= 2 && &body[end - 2..end] == b"\r\n" {
            end -= 2;
        } else if end >= 1 && body[end - 1] == b'\n' {
            end -= 1;
        }
        let part = &body[pos..end.max(pos)];
        let (head, data) = split_part(part)?;
        let mut name = None;
        let mut filename = None;
        let mut ctype = None;
        for line in head.lines() {
            let Some((hname, hval)) = line.split_once(':') else {
                continue;
            };
            let hname = hname.trim().to_ascii_lowercase();
            if hname == "content-disposition" {
                for attr in hval.split(';').skip(1) {
                    if let Some((k, v)) = attr.trim().split_once('=') {
                        let v = v.trim().trim_matches('"').to_string();
                        match k.trim().to_ascii_lowercase().as_str() {
                            "name" => name = Some(v),
                            "filename" => filename = Some(v),
                            _ => {}
                        }
                    }
                }
            } else if hname == "content-type" {
                ctype = Some(hval.trim().to_string());
            }
        }
        let name = name.ok_or("a multipart part without a name")?;
        match filename {
            Some(filename) => files.push(UploadedFile {
                field: name,
                filename,
                content_type: ctype,
                data: data.to_vec(),
            }),
            None => fields.push((name, String::from_utf8_lossy(data).into_owned())),
        }
        pos = next + delim.len();
        if pos >= body.len() {
            break;
        }
    }
    Ok((fields, files))
}

/// Split one multipart part into its header block (text) and its content.
fn split_part(part: &[u8]) -> Result<(String, &[u8]), String> {
    if let Some(i) = find(part, b"\r\n\r\n", 0) {
        return Ok((
            String::from_utf8_lossy(&part[..i]).into_owned(),
            &part[i + 4..],
        ));
    }
    if let Some(i) = find(part, b"\n\n", 0) {
        return Ok((
            String::from_utf8_lossy(&part[..i]).into_owned(),
            &part[i + 2..],
        ));
    }
    Err("a multipart part without a header/body separator".to_string())
}

/// Skip transport padding and one line ending after a multipart delimiter.
fn skip_line_end(body: &[u8], mut pos: usize) -> usize {
    while pos < body.len() && (body[pos] == b' ' || body[pos] == b'\t') {
        pos += 1;
    }
    if body[pos..].starts_with(b"\r\n") {
        pos + 2
    } else if body[pos..].starts_with(b"\n") {
        pos + 1
    } else {
        pos
    }
}

/// The first index of `needle` in `hay` at or after `from`.
fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_body_with_typed_values() {
        let body = br#"{"operation":"query","db_name":"lubm","tid":1,"async":true,"db_path":["a.nt","b.nt"],"pageNo":"2"}"#;
        let p = Params::from_parts(None, Some("application/json"), body).unwrap();
        assert_eq!(p.str("operation").as_deref(), Some("query"));
        assert_eq!(p.str("tid").as_deref(), Some("1"));
        assert!(p.bool("async", false));
        assert_eq!(p.strings("db_path"), vec!["a.nt", "b.nt"]);
        assert_eq!(p.int("pageNo"), Ok(Some(2)));
        assert_eq!(p.int("tid"), Ok(Some(1)));
        assert!(p.int("operation").is_err());
        assert_eq!(p.int("missing"), Ok(None));
        assert_eq!(p.str("db_path"), None);
        assert_eq!(p.strings("db_name"), vec!["lubm"]);
    }

    #[test]
    fn a_json_body_is_detected_without_its_content_type() {
        // `curl -d '{…}'` labels the body x-www-form-urlencoded.
        let p = Params::from_parts(
            None,
            Some("application/x-www-form-urlencoded"),
            br#" {"operation":"check"} "#,
        )
        .unwrap();
        assert_eq!(p.str("operation").as_deref(), Some("check"));
        let p = Params::from_parts(None, None, br#"{"operation":"login"}"#).unwrap();
        assert_eq!(p.str("operation").as_deref(), Some("login"));
        assert!(Params::from_parts(None, Some("application/json"), b"{oops").is_err());
        assert!(Params::from_parts(None, Some("application/json"), b"[1]").is_err());
        assert!(Params::from_parts(None, Some("application/json"), b"")
            .unwrap()
            .keys()
            .next()
            .is_none());
    }

    #[test]
    fn form_body_and_url_query_merge_with_body_winning() {
        let p = Params::from_parts(
            Some(
                "operation=query&db_name=url&sparql=select%20*%20where%20%7B%3Fs%20%3Fp%20%3Fo%7D",
            ),
            Some("application/x-www-form-urlencoded; charset=utf-8"),
            b"db_name=body&format=json&x+y=a+b%2Bc",
        )
        .unwrap();
        assert_eq!(p.str("operation").as_deref(), Some("query"));
        assert_eq!(p.str("db_name").as_deref(), Some("body"));
        assert_eq!(
            p.str("sparql").as_deref(),
            Some("select * where {?s ?p ?o}")
        );
        assert_eq!(p.str("x y").as_deref(), Some("a b+c"));
        assert_eq!(p.non_empty("format").as_deref(), Some("json"));
    }

    #[test]
    fn bool_follows_upstream_json_bool_param() {
        let p = Params::from_parts(
            Some("a=true&b=1&c=yes&d=0&e=false"),
            Some("application/json"),
            br#"{"f":1,"g":2,"h":false,"i":true}"#,
        )
        .unwrap();
        assert!(p.bool("a", false) && p.bool("b", false));
        assert!(!p.bool("c", true) && !p.bool("d", true) && !p.bool("e", true));
        assert!(p.bool("f", false) && !p.bool("g", true));
        assert!(!p.bool("h", true) && p.bool("i", false));
        assert!(p.bool("missing", true) && !p.bool("missing", false));
    }

    #[test]
    fn url_decoding_edge_cases() {
        assert_eq!(url_decode("a%20b+c"), "a b c");
        assert_eq!(url_decode("%41"), "A", "an escape at the very end decodes");
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode("%zz%4"), "%zz%4");
        assert_eq!(url_decode("%E4%B8%AD"), "中");
        assert_eq!(
            parse_form("a&=x&b=&c=1=2"),
            vec![
                ("a".to_string(), String::new()),
                ("b".to_string(), String::new()),
                ("c".to_string(), "1=2".to_string())
            ]
        );
    }

    #[test]
    fn multipart_fields_and_files() {
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"username\"\r\n\r\nroot\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"data.nt\"\r\nContent-Type: application/n-triples\r\n\r\n<a> <b> <c> .\r\n\r\n--XyZ--\r\n";
        let p = Params::from_parts(
            Some("operation=x"),
            Some("multipart/form-data; boundary=XyZ"),
            body,
        )
        .unwrap();
        assert_eq!(p.str("username").as_deref(), Some("root"));
        assert_eq!(p.str("operation").as_deref(), Some("x"));
        assert_eq!(p.str("filename").as_deref(), Some("data.nt"));
        let f = p.file("file").unwrap();
        assert_eq!(f.data, b"<a> <b> <c> .\r\n");
        assert_eq!(f.content_type.as_deref(), Some("application/n-triples"));
        assert!(Params::from_parts(None, Some("multipart/form-data"), body).is_err());
        assert!(
            Params::from_parts(None, Some("multipart/form-data; boundary=nope"), body).is_err()
        );
    }

    #[test]
    fn redacts_passwords_for_logging() {
        let p = Params::from_parts(Some("username=u&password=secret"), None, b"").unwrap();
        let j = p.to_redacted_json().to_string();
        assert!(
            !j.contains("secret") && j.contains("\"username\":\"u\""),
            "{j}"
        );
    }
}
