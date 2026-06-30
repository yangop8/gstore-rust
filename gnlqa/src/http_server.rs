//! A dependency-free HTTP/1.1 front-end for gNLQA (std `TcpListener`, mirroring
//! gStore's `server.rs`). Endpoints:
//!
//! * `POST /ask`    `{ "question": "…" }` → `{ answer, sparql, values, rounds }`
//! * `POST /gSolve` `{ "question": "…" }` → gAnswer-compatible subset
//!   `{ status, question, sparql, answers }`
//! * `GET  /health` → `{ "status": "ok" }`
//!
//! The handler holds an [`Arc<SolveEngine>`] and answers each connection on its
//! own thread.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::thread;

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::solve::SolveEngine;

/// HTTP server wrapping a [`SolveEngine`].
pub struct HttpServer {
    engine: Arc<SolveEngine>,
    listener: TcpListener,
}

impl HttpServer {
    /// Bind to `addr` (e.g. `127.0.0.1:0` for an ephemeral port).
    pub fn bind(engine: Arc<SolveEngine>, addr: impl ToSocketAddrs) -> Result<HttpServer> {
        let listener = TcpListener::bind(addr).map_err(|e| Error::Http(e.to_string()))?;
        Ok(HttpServer { engine, listener })
    }

    /// The bound local address (useful after binding to port 0).
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        self.listener.local_addr().map_err(|e| Error::Http(e.to_string()))
    }

    /// Accept and serve connections forever (one thread per connection).
    pub fn serve_forever(&self) {
        for stream in self.listener.incoming() {
            let Ok(stream) = stream else { continue };
            let engine = Arc::clone(&self.engine);
            thread::spawn(move || {
                let _ = handle_connection(stream, &engine);
            });
        }
    }
}

/// Parse one request, route it, and write the response.
fn handle_connection(mut stream: TcpStream, engine: &SolveEngine) -> std::io::Result<()> {
    let Some((method, path, body)) = read_request(&mut stream)? else {
        return write_json(&mut stream, 400, &json!({"status":"error","message":"bad request"}));
    };

    let (status, payload) = route(engine, &method, &path, &body);
    write_json(&mut stream, status, &payload)
}

/// Dispatch by method+path, returning (status, json).
fn route(engine: &SolveEngine, method: &str, path: &str, body: &str) -> (u16, Value) {
    match (method, path) {
        ("GET", "/health") => (200, json!({"status":"ok"})),
        ("POST", "/ask") => match question_of(body) {
            Some(q) => match engine.ask(&q) {
                Ok(a) => (
                    200,
                    json!({"answer": a.text, "sparql": a.sparql, "values": a.values, "rounds": a.rounds}),
                ),
                Err(e) => (200, json!({"status":"error","message": e.to_string()})),
            },
            None => (400, json!({"status":"error","message":"missing 'question'"})),
        },
        ("POST", "/gSolve") => match question_of(body) {
            Some(q) => match engine.ask(&q) {
                // gAnswer-compatible subset.
                Ok(a) => (
                    200,
                    json!({"status":"ok","question": q, "sparql": a.sparql, "answers": a.values}),
                ),
                Err(e) => (200, json!({"status":"error","question": q, "message": e.to_string()})),
            },
            None => (400, json!({"status":"error","message":"missing 'question'"})),
        },
        _ => (404, json!({"status":"error","message":"not found"})),
    }
}

/// Extract `question` from a JSON body (or a raw non-JSON body as the question).
fn question_of(body: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return v["question"].as_str().map(str::to_string).filter(|s| !s.trim().is_empty());
    }
    let t = body.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Read method, path, and (Content-Length) body from an HTTP/1.1 request.
fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<(String, String, String)>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(None);
    }
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let (method, path) = (method.to_string(), target.split('?').next().unwrap_or("/").to_string());

    // Headers → find Content-Length.
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.split_once(':') {
            if v.0.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.1.trim().parse().unwrap_or(0);
            }
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }
    Ok(Some((method, path, String::from_utf8_lossy(&body).into_owned())))
}

/// Write a JSON response.
fn write_json(stream: &mut TcpStream, status: u16, payload: &Value) -> std::io::Result<()> {
    let body = payload.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::{MockKb, RdfTerm, SparqlAnswer, TermKind};
    use crate::llm::MockLlm;

    fn answer_row(v: &str) -> Vec<Option<RdfTerm>> {
        vec![Some(RdfTerm { kind: TermKind::Uri, value: v.into(), datatype: None, lang: None })]
    }

    /// Start an in-process server backed by mocks; return its address.
    fn start() -> std::net::SocketAddr {
        let llm = MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]);
        let kb = MockKb::new(vec![SparqlAnswer::Select {
            vars: vec!["x".into()],
            rows: vec![answer_row("http://ex/a")],
        }]);
        let engine = Arc::new(SolveEngine::new(Box::new(llm), Box::new(kb)));
        let server = HttpServer::bind(engine, "127.0.0.1:0").unwrap();
        let addr = server.local_addr().unwrap();
        thread::spawn(move || server.serve_forever());
        addr
    }

    /// Minimal HTTP client: send `method path` with `body`, return the response body.
    fn request(addr: std::net::SocketAddr, method: &str, path: &str, body: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
    }

    #[test]
    fn health_ask_and_gsolve() {
        let addr = start();

        let h = request(addr, "GET", "/health", "");
        assert!(h.contains("\"status\":\"ok\""));

        let a: Value = serde_json::from_str(&request(addr, "POST", "/ask", r#"{"question":"which x?"}"#)).unwrap();
        assert_eq!(a["values"][0], "http://ex/a");
        assert!(a["sparql"].as_str().unwrap().contains("<http://ex/p>"));
    }

    #[test]
    fn gsolve_compat_shape() {
        let addr = start();
        let g: Value = serde_json::from_str(&request(addr, "POST", "/gSolve", r#"{"question":"q"}"#)).unwrap();
        assert_eq!(g["status"], "ok");
        assert_eq!(g["answers"][0], "http://ex/a");
        assert_eq!(g["question"], "q");
    }

    #[test]
    fn missing_question_and_not_found() {
        let addr = start();
        let bad = request(addr, "POST", "/ask", r#"{}"#);
        assert!(bad.contains("missing 'question'"));
        let nf = request(addr, "GET", "/nope", "");
        assert!(nf.contains("not found"));
    }
}
