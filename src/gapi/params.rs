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
//! A body labelled `application/sparql-query` or `application/sparql-update`
//! carries no parameters (the SPARQL Protocol's raw statement): only the URL
//! query string does then.
//!
//! The URL query and the body are merged; on a duplicated name the body wins
//! (the last occurrence wins in general). Values keep their JSON type
//! ([`Json`]) — `"tid": 1` and `"tid": "1"` both read as `"1"` through
//! [`Params::str`] — and form / query values are strings.
//!
//! # Bounds
//!
//! The body is parsed **before the caller is authenticated** (the
//! credentials may be in it), so what a body may make the parser allocate is
//! bounded, not just its size (review C-2: a parsed value costs many times
//! its text):
//!
//! * a JSON body holds at most [`MAX_JSON_VALUES`] values (every scalar,
//!   array and object; [`json::parse_with_limits`]) — a wider one is refused
//!   before more of its tree is built;
//! * a URL query or form body holds at most [`MAX_PARAMS`] `name=value`
//!   pairs, and a multipart body at most [`MAX_PARAMS`] parts — counted
//!   before anything is decoded;
//! * a multipart boundary must follow RFC 2046 (1 to 70 characters of its
//!   `bchars` set, not ending in a space), and the boundary search is linear
//!   in the body (review H-2).

use crate::json::{self, Json};
use crate::server::Request;

/// The most values (every scalar, array and object, at any depth) a JSON
/// request body may hold. Upstream's requests carry a few parameters, a list
/// of files at most; the bound keeps what an unauthenticated body can make
/// the parser allocate to a few MiB.
pub const MAX_JSON_VALUES: usize = 65_536;

/// The most `name=value` pairs a URL query string or a form body may hold,
/// and the most parts of a multipart body.
pub const MAX_PARAMS: usize = 65_536;

/// The bounds a JSON request body is parsed with ([`MAX_JSON_VALUES`],
/// [`json::MAX_DEPTH`]).
const BODY_JSON_LIMITS: json::Limits = json::Limits {
    max_depth: json::MAX_DEPTH,
    max_values: MAX_JSON_VALUES,
};

/// The longest multipart boundary RFC 2046 allows.
pub const MAX_BOUNDARY_LEN: usize = 70;

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
            check_form_size(q, "the URL query string")?;
            for (k, v) in parse_form(q) {
                p.set(k, v);
            }
        }
        let media = content_type
            .and_then(|c| c.split(';').next())
            .map(|m| m.trim().to_ascii_lowercase())
            .unwrap_or_default();
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
            return Ok(p);
        }
        // Parsed in place: the body (which may be large, and is read before
        // the caller is authenticated) is never copied into a decoded string.
        // JSON and form bodies are text; any other body carries no parameters.
        let text = match std::str::from_utf8(body) {
            Ok(t) => t,
            Err(_) => {
                let first = body.iter().find(|b| !b.is_ascii_whitespace());
                let textual = media == "application/json"
                    || media == "application/x-www-form-urlencoded"
                    || first == Some(&b'{')
                    || body.contains(&b'=');
                if textual {
                    return Err("the request body is not valid UTF-8".to_string());
                }
                ""
            }
        };
        let trimmed = text.trim_start_matches('\u{FEFF}').trim();
        // A body labelled as a raw SPARQL statement is never read as
        // parameters, whatever text it holds (review design#17: a `&sparql=`
        // or `&db_name=` inside it replaced the URL's parameters).
        let raw_sparql = matches!(
            media.as_str(),
            "application/sparql-query" | "application/sparql-update"
        );
        if trimmed.is_empty() || raw_sparql {
            // No body parameters.
        } else if media == "application/json" || trimmed.starts_with('{') {
            match json::parse_with_limits(trimmed, &BODY_JSON_LIMITS) {
                Ok(Json::Obj(entries)) => {
                    for (k, v) in entries {
                        p.set(k, v);
                    }
                }
                Ok(_) => return Err("the request body must be a JSON object".to_string()),
                Err(e) => return Err(format!("the request body is not valid JSON ({e})")),
            }
        } else if media == "application/x-www-form-urlencoded" || trimmed.contains('=') {
            check_form_size(trimmed, "the form body")?;
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

    /// The parameters as one JSON object (last value per name, at the
    /// position of the name's first occurrence), with any `password` /
    /// `op_password` value redacted — for logging. Linear in the number of
    /// parameters (review gapi#19: it used to search the object it was
    /// building for every parameter).
    pub fn to_redacted_json(&self) -> Json {
        let mut index: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::with_capacity(self.entries.len());
        let mut out: Vec<(String, Json)> = Vec::with_capacity(self.entries.len());
        for (k, v) in &self.entries {
            let v = if k == "password" || k == "op_password" {
                Json::from("***")
            } else {
                v.clone()
            };
            match index.get(k.as_str()) {
                Some(&i) => out[i].1 = v,
                None => {
                    index.insert(k.as_str(), out.len());
                    out.push((k.clone(), v));
                }
            }
        }
        Json::Obj(out)
    }
}

/// Refuse a URL query string / form body (`what`) of more than
/// [`MAX_PARAMS`] pairs — counted without decoding anything.
fn check_form_size(s: &str, what: &str) -> Result<(), String> {
    let pairs = s.split('&').filter(|pair| !pair.is_empty()).count();
    if pairs > MAX_PARAMS {
        return Err(format!("{what} holds more than {MAX_PARAMS} parameters"));
    }
    Ok(())
}

/// Decode an `application/x-www-form-urlencoded` string (also a URL query):
/// `&`-separated `name=value` pairs, `+` as a space, `%XX` escapes. A pair
/// without `=` is a name with an empty value; empty names are skipped. The
/// crate's one form decoder ([`crate::util::form::parse_form`]).
pub fn parse_form(s: &str) -> Vec<(String, String)> {
    crate::util::form::parse_form(s)
}

/// Percent-decode `s` (`+` decodes to a space; a malformed `%` escape is kept
/// literally). Invalid UTF-8 is replaced with U+FFFD. The crate's one
/// decoder ([`crate::util::form::url_decode`]).
pub fn url_decode(s: &str) -> String {
    crate::util::form::url_decode(s)
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
    check_boundary(&boundary)?;
    let delim = format!("--{boundary}").into_bytes();
    let finder = memchr::memmem::Finder::new(&delim);
    let find_delim = |from: usize| finder.find(&body[from..]).map(|i| i + from);
    let mut fields = Vec::new();
    let mut files = Vec::new();
    let mut pos = find_delim(0).ok_or("multipart body without its boundary")? + delim.len();
    loop {
        // After a delimiter: `--` ends the body, otherwise CRLF and a part.
        if body[pos..].starts_with(b"--") {
            break;
        }
        if fields.len() + files.len() >= MAX_PARAMS {
            return Err(format!(
                "the multipart body holds more than {MAX_PARAMS} parts"
            ));
        }
        pos = skip_line_end(body, pos);
        let next = find_delim(pos).ok_or("unterminated multipart part")?;
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

/// Check a multipart boundary against RFC 2046 §5.1.1: 1 to
/// [`MAX_BOUNDARY_LEN`] characters, each a digit, a letter, a space or one
/// of `'()+_,-./:=?`, the last not a space. (A longer boundary made the
/// boundary search — which runs before authentication — that much slower.)
fn check_boundary(boundary: &str) -> Result<(), String> {
    if boundary.len() > MAX_BOUNDARY_LEN {
        return Err(format!(
            "the multipart boundary is longer than {MAX_BOUNDARY_LEN} characters (RFC 2046)"
        ));
    }
    let bchar = |c: u8| c.is_ascii_alphanumeric() || b"'()+_,-./:=? ".contains(&c);
    if !boundary.bytes().all(bchar) || boundary.ends_with(' ') {
        return Err("the multipart boundary holds a character RFC 2046 does not allow".to_string());
    }
    Ok(())
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

/// The first index of `needle` in `hay` at or after `from`, in time linear
/// in `hay[from..]` and `needle` (`memchr`'s two-way search; review H-2: a
/// window-by-window comparison cost `hay` × `needle`).
fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    memchr::memmem::find(&hay[from..], needle).map(|i| i + from)
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
    fn bodies_that_are_not_text() {
        // A binary body of another type carries no parameters.
        let p = Params::from_parts(
            Some("operation=check"),
            Some("application/octet-stream"),
            &[0xff, 0xfe, 0x00, 0x80],
        )
        .unwrap();
        assert_eq!(p.keys().collect::<Vec<_>>(), ["operation"]);
        // A JSON or form body must be UTF-8.
        assert!(Params::from_parts(None, Some("application/json"), b"{\"a\":\"\xff\"}").is_err());
        assert!(Params::from_parts(None, None, b"a=\xff").is_err());
        // A multipart file is taken as bytes.
        let mut body =
            b"--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x.nt\"\r\n\r\n"
                .to_vec();
        body.extend_from_slice(&[0xff; 16]);
        body.extend_from_slice(b"\r\n--B--\r\n");
        let p = Params::from_parts(None, Some("multipart/form-data; boundary=B"), &body).unwrap();
        assert_eq!(p.file("file").unwrap().data, [0xff; 16]);
    }

    // ---- bounds (review C-2 / H-2) -------------------------------------

    #[test]
    fn a_wide_json_body_is_refused_before_its_tree_is_built() {
        // {"operation":"check","a":[0,0,…]}: the object, two members and the
        // array's elements.
        let body = |n: usize| {
            let mut b = String::from(r#"{"operation":"check","a":["#);
            b.push_str(&"0,".repeat(n - 1));
            b.push_str("0]}");
            b
        };
        let fits = body(MAX_JSON_VALUES - 3);
        let p = Params::from_parts(None, Some("application/json"), fits.as_bytes()).unwrap();
        assert_eq!(p.str("operation").as_deref(), Some("check"));
        let wide = body(MAX_JSON_VALUES - 2);
        let e = Params::from_parts(None, Some("application/json"), wide.as_bytes()).unwrap_err();
        assert!(e.contains("too many JSON values"), "{e}");
        // Detected without a Content-Type too.
        assert!(Params::from_parts(None, None, wide.as_bytes()).is_err());
        // Deep nesting stays refused.
        let deep = format!(r#"{{"a":{}}}"#, "[".repeat(json::MAX_DEPTH + 1));
        assert!(Params::from_parts(None, Some("application/json"), deep.as_bytes()).is_err());
    }

    #[test]
    fn form_bodies_and_query_strings_have_a_bounded_number_of_pairs() {
        let pairs = |n: usize| vec!["a=1"; n].join("&");
        let ok = pairs(MAX_PARAMS);
        let p = Params::from_parts(
            None,
            Some("application/x-www-form-urlencoded"),
            ok.as_bytes(),
        )
        .unwrap();
        assert_eq!(p.keys().count(), MAX_PARAMS);
        let many = pairs(MAX_PARAMS + 1);
        let e = Params::from_parts(
            None,
            Some("application/x-www-form-urlencoded"),
            many.as_bytes(),
        )
        .unwrap_err();
        assert!(e.contains("more than"), "{e}");
        // Empty pairs (`&&`) are not parameters and do not count.
        let sparse = format!("{}{}", "&".repeat(MAX_PARAMS * 2), ok);
        assert!(Params::from_parts(None, None, sparse.as_bytes()).is_ok());
        let e = Params::from_parts(Some(&many), None, b"").unwrap_err();
        assert!(e.contains("URL query string"), "{e}");
    }

    #[test]
    fn multipart_parts_are_bounded() {
        let part = "--B\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n";
        let body = |n: usize| format!("{}--B--\r\n", part.repeat(n));
        let ct = Some("multipart/form-data; boundary=B");
        let p = Params::from_parts(None, ct, body(MAX_PARAMS).as_bytes()).unwrap();
        assert_eq!(p.keys().count(), MAX_PARAMS);
        let e = Params::from_parts(None, ct, body(MAX_PARAMS + 1).as_bytes()).unwrap_err();
        assert!(e.contains("parts"), "{e}");
    }

    #[test]
    fn multipart_boundaries_follow_rfc_2046() {
        let body_for = |b: &str| {
            format!(
                "--{b}\r\nContent-Disposition: form-data; name=\"username\"\r\n\r\nroot\r\n--{b}--\r\n"
            )
        };
        let parse =
            |ct: String, b: &str| Params::from_parts(None, Some(&ct), body_for(b).as_bytes());
        // The longest boundary allowed, every special bchar, a quoted space.
        let longest = "x".repeat(MAX_BOUNDARY_LEN);
        let p = parse(format!("multipart/form-data; boundary={longest}"), &longest).unwrap();
        assert_eq!(p.str("username").as_deref(), Some("root"));
        let special = "a'()+_,-./:=?b";
        assert!(parse(
            format!("multipart/form-data; boundary=\"{special}\""),
            special
        )
        .is_ok());
        assert!(parse("multipart/form-data; boundary=\"a b\"".into(), "a b").is_ok());
        // Too long, a character outside bchars, a trailing space.
        let long = "x".repeat(MAX_BOUNDARY_LEN + 1);
        let e = parse(format!("multipart/form-data; boundary={long}"), &long).unwrap_err();
        assert!(e.contains("longer than 70"), "{e}");
        for bad in ["a*b", "a\\b", "a%b", "é", "a\u{1}b"] {
            let e = parse(format!("multipart/form-data; boundary=\"{bad}\""), bad).unwrap_err();
            assert!(e.contains("RFC 2046"), "{bad}: {e}");
        }
        let e = parse("multipart/form-data; boundary=\"ab \"".into(), "ab ").unwrap_err();
        assert!(e.contains("RFC 2046"), "{e}");
    }

    /// The fastest of `runs` timings of `f`, in seconds.
    fn best_of(runs: usize, mut f: impl FnMut()) -> f64 {
        (0..runs)
            .map(|_| {
                let t = std::time::Instant::now();
                f();
                t.elapsed().as_secs_f64()
            })
            .fold(f64::INFINITY, f64::min)
    }

    #[test]
    fn the_boundary_search_is_linear_in_the_boundary_and_the_body() {
        // The search's worst case for a window-by-window comparison: every
        // window of the body matches the needle up to its last byte. That
        // cost body × needle (review H-2: a 60,000-byte boundary made one
        // 8 MiB request take 11 s); a linear search costs body + needle,
        // whatever the needle's length.
        let hay = vec![b'-'; 1 << 20];
        let needle = |m: usize| {
            let mut n = vec![b'-'; m - 1];
            n.push(b'B');
            n
        };
        let (short, long) = (needle(64), needle(60_000));
        let t_short = best_of(5, || assert_eq!(find(&hay, &short, 0), None));
        let t_long = best_of(5, || assert_eq!(find(&hay, &long, 0), None));
        // A window-by-window search is hundreds of times slower with the
        // long needle (over a second here); a linear one takes a few
        // milliseconds either way. (The floor keeps a sub-millisecond short
        // search from making the bound meaninglessly tight.)
        assert!(
            t_long < 50.0 * t_short.max(5e-3),
            "a 60,000-byte needle took {t_long:.4} s, a 64-byte one {t_short:.4} s"
        );
        // Linear in the body: doubling it about doubles the time.
        let hay2 = vec![b'-'; 2 << 20];
        let t_double = best_of(5, || assert_eq!(find(&hay2, &long, 0), None));
        assert!(
            t_double < 6.0 * t_long.max(1e-2),
            "2 MiB took {t_double:.4} s, 1 MiB {t_long:.4} s"
        );
        // And it finds what it should.
        let mut hay3 = hay.clone();
        hay3.extend_from_slice(&short);
        assert_eq!(find(&hay3, &short, 0), Some(hay.len()));
        assert_eq!(find(&hay3, &short, hay.len() + 1), None);
        assert_eq!(find(b"abc", b"", 0), None);
        assert_eq!(find(b"abc", b"c", 4), None);
    }

    #[test]
    fn redacted_json_keeps_the_last_value_at_the_first_position() {
        let mut p = Params::new();
        p.set("a", 1);
        p.set("password", "secret");
        p.set("b", "x");
        p.set("a", 2);
        p.set("op_password", "s2");
        assert_eq!(
            p.to_redacted_json().to_string(),
            r#"{"a":2,"password":"***","b":"x","op_password":"***"}"#
        );
    }

    #[test]
    fn redacted_json_is_linear_in_the_number_of_parameters() {
        // review gapi#19: building the object with `Json::set` searched it
        // for every parameter (quadratic: 8 times the parameters cost 64
        // times the time).
        let params = |n: usize| {
            let mut p = Params::new();
            for i in 0..n {
                p.set(format!("k{i}"), i as u64);
            }
            p
        };
        let (small, large) = (params(4_000), params(32_000));
        let t_small = best_of(5, || {
            assert!(small.to_redacted_json().as_object().is_some())
        });
        let t_large = best_of(5, || {
            assert_eq!(
                large.to_redacted_json().as_object().map(<[_]>::len),
                Some(32_000)
            )
        });
        // Linear: tens of milliseconds for the large set (allocation and
        // cache effects make it more than 8 times the small one); quadratic:
        // seconds. (The floor keeps a fast small run from making the bound
        // meaninglessly tight.)
        assert!(
            t_large < 40.0 * t_small.max(5e-3),
            "32,000 parameters took {t_large:.4} s, 4,000 took {t_small:.4} s"
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
