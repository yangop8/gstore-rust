//! A minimal HTTP SPARQL endpoint over a [`Database`] — the analogue of gStore's
//! `src/Server` / `ghttp`.
//!
//! This is a dependency-free, blocking HTTP/1.1 server (one request per
//! connection) implementing a useful subset of the SPARQL 1.1 Protocol:
//!
//! | route | does |
//! |-------|------|
//! | `GET /sparql?query=…` / `POST /sparql` | run a query (SELECT/ASK/CONSTRUCT/DESCRIBE) |
//! | `POST /update` | run a SPARQL UPDATE |
//! | `GET /status` | report [`DbStats`](crate::db::DbStats) |
//!
//! SELECT returns SPARQL 1.1 Query Results JSON, ASK returns the boolean JSON
//! form, and CONSTRUCT/DESCRIBE return N-Triples. The [`Database`] is shared
//! behind a [`Mutex`] so connections are serialized (correctness over
//! throughput).
//!
//! Deferred (see `docs/REFACTOR_BACKLOG.md` F): HTTPS/auth, content negotiation,
//! gRPC, streaming responses, and clustering/sharding.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Mutex;

use crate::db::Database;
use crate::model::Term;
use crate::parser::ntriples::parse_term;
use crate::query::QueryResult;

const SPARQL_JSON: &str = "application/sparql-results+json";

/// An HTTP SPARQL server wrapping a [`Database`].
pub struct Server {
    db: Mutex<Database>,
    listener: TcpListener,
}

impl Server {
    /// Bind to `addr` (e.g. `"127.0.0.1:7000"` or `"127.0.0.1:0"` for an
    /// ephemeral port), taking ownership of `db`.
    pub fn bind<A: ToSocketAddrs>(db: Database, addr: A) -> std::io::Result<Server> {
        let listener = TcpListener::bind(addr)?;
        Ok(Server {
            db: Mutex::new(db),
            listener,
        })
    }

    /// The actual bound address (useful after binding to port 0).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept and handle connections forever (one request each).
    pub fn serve_forever(&self) {
        for stream in self.listener.incoming().flatten() {
            let _ = self.handle(stream);
        }
    }

    fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        let req = match Request::parse(&mut stream) {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()), // connection closed with no request
            Err(_) => return write_response(&mut stream, 400, "text/plain", b"bad request"),
        };
        let (status, ctype, body) = self.route(&req);
        write_response(&mut stream, status, &ctype, &body)
    }

    fn route(&self, req: &Request) -> (u16, String, Vec<u8>) {
        match (req.method.as_str(), req.path.as_str()) {
            ("GET" | "POST", "/sparql") => match req.query_string() {
                Some(q) => self.run_query(&q),
                None => (400, SPARQL_JSON.into(), err_json("missing query")),
            },
            ("POST", "/update") => self.run_update(&req.body_string()),
            ("GET", "/status") => self.status_json(),
            ("GET" | "POST", _) => (404, "text/plain".into(), b"not found".to_vec()),
            _ => (405, "text/plain".into(), b"method not allowed".to_vec()),
        }
    }

    fn run_query(&self, q: &str) -> (u16, String, Vec<u8>) {
        let mut db = self.db.lock().unwrap();
        match db.query(q) {
            Ok(QueryResult::Select(rs)) => (200, SPARQL_JSON.into(), select_json(&rs)),
            Ok(QueryResult::Ask(b)) => (
                200,
                SPARQL_JSON.into(),
                format!("{{\"head\":{{}},\"boolean\":{b}}}").into_bytes(),
            ),
            Ok(QueryResult::Construct(ts)) => {
                (200, "application/n-triples".into(), ntriples_body(&ts))
            }
            Ok(QueryResult::Update { changed }) => (
                200,
                "application/json".into(),
                format!("{{\"changed\":{changed}}}").into_bytes(),
            ),
            Err(e) => (400, SPARQL_JSON.into(), err_json(&e.to_string())),
        }
    }

    fn run_update(&self, u: &str) -> (u16, String, Vec<u8>) {
        let mut db = self.db.lock().unwrap();
        match db.query(u) {
            Ok(QueryResult::Update { changed }) => (
                200,
                "application/json".into(),
                format!("{{\"changed\":{changed}}}").into_bytes(),
            ),
            Ok(_) => (
                400,
                "application/json".into(),
                err_json("/update expects a SPARQL UPDATE request"),
            ),
            Err(e) => (400, "application/json".into(), err_json(&e.to_string())),
        }
    }

    fn status_json(&self) -> (u16, String, Vec<u8>) {
        let db = self.db.lock().unwrap();
        let s = db.stats();
        let body = format!(
            "{{\"name\":{},\"triples\":{},\"entities\":{},\"literals\":{},\"predicates\":{},\"index_valid\":{},\"in_transaction\":{}}}",
            json_str(&s.name),
            s.triple_num,
            s.entity_num,
            s.literal_num,
            s.predicate_num,
            s.index_valid,
            s.in_transaction,
        );
        (200, "application/json".into(), body.into_bytes())
    }
}

/// A parsed HTTP request (method, path, query string, body).
struct Request {
    method: String,
    path: String,
    query: Option<String>,
    body: Vec<u8>,
}

impl Request {
    fn parse(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let target = parts.next().unwrap_or_default().to_string();
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), Some(q.to_string())),
            None => (target, None),
        };

        let mut content_length = 0usize;
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h)? == 0 {
                break;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break; // end of headers
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-length") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
        }

        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }
        Ok(Some(Request {
            method,
            path,
            query,
            body,
        }))
    }

    /// The query text: from the URL `?query=`, a form-encoded `query=` body, or
    /// (for POST with a `application/sparql-query` body) the raw body.
    fn query_string(&self) -> Option<String> {
        if let Some(q) = &self.query {
            if let Some(v) = form_get(q, "query") {
                return Some(v);
            }
        }
        let b = String::from_utf8_lossy(&self.body);
        if let Some(v) = form_get(&b, "query") {
            return Some(v);
        }
        let trimmed = b.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        None
    }

    /// The update text: a form-encoded `update=` body, else the raw body.
    fn body_string(&self) -> String {
        let b = String::from_utf8_lossy(&self.body);
        form_get(&b, "update").unwrap_or_else(|| b.into_owned())
    }
}

// --- response helpers --------------------------------------------------------

fn write_response(stream: &mut TcpStream, status: u16, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// SPARQL 1.1 Query Results JSON for a SELECT table.
fn select_json(rs: &crate::query::ResultSet) -> Vec<u8> {
    let mut s = String::from("{\"head\":{\"vars\":[");
    for (i, v) in rs.vars.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&json_str(v));
    }
    s.push_str("]},\"results\":{\"bindings\":[");
    for (ri, row) in rs.rows.iter().enumerate() {
        if ri > 0 {
            s.push(',');
        }
        s.push('{');
        let mut first = true;
        for (ci, cell) in row.iter().enumerate() {
            if let Some(val) = cell {
                if !first {
                    s.push(',');
                }
                first = false;
                s.push_str(&json_str(&rs.vars[ci]));
                s.push(':');
                s.push_str(&binding_json(val));
            }
        }
        s.push('}');
    }
    s.push_str("]}}");
    s.into_bytes()
}

/// One result cell (an N-Triples surface term) as an RDF-term JSON object.
fn binding_json(cell: &str) -> String {
    match parse_term(cell) {
        Ok(Term::Iri(s)) => format!("{{\"type\":\"uri\",\"value\":{}}}", json_str(&s)),
        Ok(Term::Blank(l)) => format!("{{\"type\":\"bnode\",\"value\":{}}}", json_str(&l)),
        Ok(Term::Literal {
            value,
            datatype,
            lang,
        }) => {
            if let Some(l) = lang {
                format!(
                    "{{\"type\":\"literal\",\"value\":{},\"xml:lang\":{}}}",
                    json_str(&value),
                    json_str(&l)
                )
            } else if let Some(dt) = datatype {
                format!(
                    "{{\"type\":\"literal\",\"value\":{},\"datatype\":{}}}",
                    json_str(&value),
                    json_str(&dt)
                )
            } else {
                format!("{{\"type\":\"literal\",\"value\":{}}}", json_str(&value))
            }
        }
        Err(_) => format!("{{\"type\":\"literal\",\"value\":{}}}", json_str(cell)),
    }
}

fn ntriples_body(triples: &[crate::model::Triple]) -> Vec<u8> {
    let mut s = String::new();
    for t in triples {
        s.push_str(&format!("{} {} {} .\n", t.subject, t.predicate, t.object));
    }
    s.into_bytes()
}

fn err_json(msg: &str) -> Vec<u8> {
    format!("{{\"error\":{}}}", json_str(msg)).into_bytes()
}

/// Quote + escape a string as a JSON string literal.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// --- URL form decoding -------------------------------------------------------

/// Look up `key` in an `application/x-www-form-urlencoded` string.
fn form_get(qs: &str, key: &str) -> Option<String> {
    for pair in qs.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if url_decode(k) == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
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

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

