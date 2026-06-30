//! A minimal, dependency-free HTTP/1.1 client for SPARQL `SERVICE` federation —
//! the counterpart to the [`crate::server`] endpoint.
//!
//! It does exactly enough to drive a federated query: open a TCP connection,
//! `POST` a SPARQL query to an `http://host:port/path` endpoint, read the whole
//! response, and parse the returned SPARQL 1.1 Query Results JSON into bindings.
//!
//! Scope (mirroring `server.rs`): plain `http://` only (no TLS), `Connection:
//! close` one-shot requests, and a hand-rolled JSON reader covering the result
//! shape SPARQL endpoints emit. HTTPS/auth/redirects are out of scope.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::model::Term;

/// A single returned solution: each bound variable mapped to its RDF term.
pub type ServiceSolution = Vec<(String, Term)>;

/// POST `query` to a SPARQL endpoint at `url` and return the raw response body.
///
/// The body is sent verbatim with `Content-Type: application/sparql-query`,
/// which [`crate::server`] accepts as the query text. Errors on a non-2xx
/// status so callers can treat a remote failure like a connection failure.
pub fn sparql_post(url: &str, query: &str) -> io::Result<String> {
    let (host, port, path) = parse_http_url(url)?;
    let mut stream = TcpStream::connect((host.as_str(), port))?;
    // Bound the request so a misbehaving/dead endpoint can't hang the query.
    let timeout = Some(Duration::from_secs(15));
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;

    let request = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: gstore-rust/0.1\r\n\
         Accept: application/sparql-results+json\r\n\
         Content-Type: application/sparql-query\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n{query}",
        len = query.len()
    );
    stream.write_all(request.as_bytes())?;
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw).into_owned();

    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed HTTP response"))?;

    // Status line: `HTTP/1.1 200 OK`.
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP status"))?;
    if !(200..300).contains(&status) {
        return Err(io::Error::other(format!("endpoint returned HTTP {status}")));
    }
    Ok(body.to_string())
}

/// Split an `http://host[:port]/path` URL into `(host, port, path)`.
fn parse_http_url(url: &str) -> io::Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "only http:// is supported"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            let port = p
                .parse::<u16>()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad port"))?;
            (h.to_string(), port)
        }
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty host"));
    }
    Ok((host, port, path))
}

/// Parse SPARQL 1.1 Query Results JSON into a list of solutions.
///
/// Expects the standard envelope
/// `{"head":{"vars":[…]},"results":{"bindings":[ {var: {type,value,…}}, … ]}}`
/// and decodes each binding object into an RDF [`Term`].
pub fn parse_sparql_json(body: &str) -> io::Result<Vec<ServiceSolution>> {
    let json = Json::parse(body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad JSON: {e}")))?;
    let bindings = json
        .get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Json::as_array)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no results.bindings array"))?;

    let mut out = Vec::with_capacity(bindings.len());
    for row in bindings {
        let obj = row
            .as_object()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "binding is not an object"))?;
        let mut sol = Vec::with_capacity(obj.len());
        for (var, cell) in obj {
            let term = rdf_term_from_json(cell)?;
            sol.push((var.clone(), term));
        }
        out.push(sol);
    }
    Ok(out)
}

/// Decode one `{"type":…,"value":…}` RDF-term JSON object into a [`Term`].
fn rdf_term_from_json(cell: &Json) -> io::Result<Term> {
    let ty = cell
        .get("type")
        .and_then(Json::as_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "binding cell missing type"))?;
    let value = cell
        .get("value")
        .and_then(Json::as_str)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "binding cell missing value"))?
        .to_string();
    Ok(match ty {
        "uri" => Term::Iri(value),
        "bnode" => Term::Blank(value),
        // "literal" and the legacy "typed-literal" both carry datatype/lang.
        _ => {
            let datatype = cell.get("datatype").and_then(Json::as_str).map(str::to_string);
            let lang = cell
                .get("xml:lang")
                .and_then(Json::as_str)
                .map(str::to_string);
            Term::Literal {
                value,
                datatype,
                lang,
            }
        }
    })
}

// --- a tiny JSON reader ------------------------------------------------------

/// A minimal JSON value, sufficient to navigate SPARQL results.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn parse(s: &str) -> Result<Json, String> {
        let mut p = JsonParser {
            chars: s.chars().collect(),
            pos: 0,
        };
        p.skip_ws();
        let v = p.value()?;
        p.skip_ws();
        if p.pos != p.chars.len() {
            return Err("trailing characters after JSON value".into());
        }
        Ok(v)
    }

    /// Object-field lookup (no-op for non-objects).
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(o) => Some(o),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
}

struct JsonParser {
    chars: Vec<char>,
    pos: usize,
}

impl JsonParser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        match self.peek() {
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('"') => Ok(Json::Str(self.string()?)),
            Some('t') | Some('f') => self.boolean(),
            Some('n') => self.null(),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            other => Err(format!("unexpected token {other:?}")),
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.expect('{')?;
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.bump();
            return Ok(Json::Obj(fields));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(':')?;
            let val = self.value()?;
            fields.push((key, val));
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some('}') => break,
                other => return Err(format!("expected ',' or '}}', found {other:?}")),
            }
        }
        Ok(Json::Obj(fields))
    }

    fn array(&mut self) -> Result<Json, String> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.bump();
            return Ok(Json::Arr(items));
        }
        loop {
            let val = self.value()?;
            items.push(val);
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some(']') => break,
                other => return Err(format!("expected ',' or ']', found {other:?}")),
            }
        }
        Ok(Json::Arr(items))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            match self.bump() {
                Some('"') => break,
                Some('\\') => match self.bump() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('/') => out.push('/'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('b') => out.push('\u{0008}'),
                    Some('f') => out.push('\u{000C}'),
                    Some('u') => {
                        let mut code = 0u32;
                        for _ in 0..4 {
                            let c = self
                                .bump()
                                .ok_or_else(|| "unterminated \\u escape".to_string())?;
                            let d = c
                                .to_digit(16)
                                .ok_or_else(|| format!("bad hex digit {c:?}"))?;
                            code = code * 16 + d;
                        }
                        out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                    }
                    other => return Err(format!("bad escape {other:?}")),
                },
                Some(c) => out.push(c),
                None => return Err("unterminated string".into()),
            }
        }
        Ok(out)
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == '-' || c == '+' || c == '.' || c == 'e' || c == 'E' || c.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s: String = self.chars[start..self.pos].iter().collect();
        s.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("bad number '{s}'"))
    }

    fn boolean(&mut self) -> Result<Json, String> {
        if self.consume_word("true") {
            Ok(Json::Bool(true))
        } else if self.consume_word("false") {
            Ok(Json::Bool(false))
        } else {
            Err("invalid literal".into())
        }
    }

    fn null(&mut self) -> Result<Json, String> {
        if self.consume_word("null") {
            Ok(Json::Null)
        } else {
            Err("invalid literal".into())
        }
    }

    fn consume_word(&mut self, word: &str) -> bool {
        let end = self.pos + word.len();
        if end <= self.chars.len() && self.chars[self.pos..end].iter().collect::<String>() == word {
            self.pos = end;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected {c:?}, found {:?}", self.peek()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_select_results_json() {
        let body = r#"{"head":{"vars":["o","name"]},"results":{"bindings":[
            {"o":{"type":"uri","value":"http://ex/b"},
             "name":{"type":"literal","value":"Alice"}},
            {"name":{"type":"literal","value":"Bonjour","xml:lang":"fr"}}
        ]}}"#;
        let sols = parse_sparql_json(body).unwrap();
        assert_eq!(sols.len(), 2);
        assert_eq!(
            sols[0],
            vec![
                ("o".to_string(), Term::iri("http://ex/b")),
                ("name".to_string(), Term::plain_literal("Alice")),
            ]
        );
        assert_eq!(
            sols[1][0].1,
            Term::Literal {
                value: "Bonjour".into(),
                datatype: None,
                lang: Some("fr".into()),
            }
        );
    }

    #[test]
    fn parses_typed_literal_and_empty_bindings() {
        let body = r#"{"head":{"vars":["n"]},"results":{"bindings":[
            {"n":{"type":"literal","value":"42","datatype":"http://www.w3.org/2001/XMLSchema#integer"}}
        ]}}"#;
        let sols = parse_sparql_json(body).unwrap();
        assert_eq!(
            sols[0][0].1,
            Term::typed_literal("42", "http://www.w3.org/2001/XMLSchema#integer")
        );

        let empty = r#"{"head":{"vars":[]},"results":{"bindings":[]}}"#;
        assert!(parse_sparql_json(empty).unwrap().is_empty());
    }

    #[test]
    fn rejects_non_results_json() {
        assert!(parse_sparql_json(r#"{"error":"boom"}"#).is_err());
        assert!(parse_sparql_json("not json").is_err());
    }

    #[test]
    fn splits_http_urls() {
        assert_eq!(
            parse_http_url("http://127.0.0.1:7000/sparql").unwrap(),
            ("127.0.0.1".to_string(), 7000, "/sparql".to_string())
        );
        assert_eq!(
            parse_http_url("http://example.org/q").unwrap(),
            ("example.org".to_string(), 80, "/q".to_string())
        );
        assert_eq!(
            parse_http_url("http://host:9").unwrap(),
            ("host".to_string(), 9, "/".to_string())
        );
        assert!(parse_http_url("https://secure/x").is_err());
    }
}
