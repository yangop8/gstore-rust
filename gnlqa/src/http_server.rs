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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::solve::SolveEngine;

/// Max request body (bytes) — bounds attacker-controlled Content-Length.
const MAX_BODY: usize = 1 << 20; // 1 MiB
/// Max bytes for a single request/header line.
const MAX_LINE: usize = 16 * 1024;
/// Per-connection socket timeout.
const IO_TIMEOUT: Duration = Duration::from_secs(15);

/// HTTP server wrapping a [`SolveEngine`].
pub struct HttpServer {
    engine: Arc<SolveEngine>,
    listener: TcpListener,
    max_conn: usize,
}

impl HttpServer {
    /// Bind to `addr` (e.g. `127.0.0.1:0` for an ephemeral port).
    pub fn bind(engine: Arc<SolveEngine>, addr: impl ToSocketAddrs) -> Result<HttpServer> {
        let listener = TcpListener::bind(addr).map_err(|e| Error::Http(e.to_string()))?;
        Ok(HttpServer { engine, listener, max_conn: 64 })
    }

    /// Cap the number of concurrently-handled connections (excess → 503).
    pub fn with_max_conn(mut self, n: usize) -> HttpServer {
        self.max_conn = n.max(1);
        self
    }

    /// The bound local address (useful after binding to port 0).
    pub fn local_addr(&self) -> Result<std::net::SocketAddr> {
        self.listener.local_addr().map_err(|e| Error::Http(e.to_string()))
    }

    /// Accept and serve connections forever. Each connection gets socket
    /// timeouts (anti-slowloris) and runs on its own thread, with a cap on the
    /// number of concurrent handlers (excess connections get a 503).
    pub fn serve_forever(&self) {
        let active = Arc::new(AtomicUsize::new(0));
        for stream in self.listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));

            if active.load(Ordering::Acquire) >= self.max_conn {
                let _ = write_json(&mut stream, 503, &json!({"status":"error","message":"server busy"}));
                continue;
            }
            active.fetch_add(1, Ordering::AcqRel);
            let engine = Arc::clone(&self.engine);
            let active2 = Arc::clone(&active);
            thread::spawn(move || {
                let _ = handle_connection(stream, &engine);
                active2.fetch_sub(1, Ordering::AcqRel);
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
                    json!({
                        "answer": a.text,
                        "explanation": a.explanation,
                        "sparql": a.sparql,
                        "values": a.values,
                        "confidence": round3(a.confidence),
                        "abstained": a.abstained,
                        "rounds": a.rounds,
                        "provenance": a.provenance.tag(),
                    }),
                ),
                // Native endpoint: a real failure is a 500 (proxies/monitoring
                // can detect it), unlike the gAnswer-compat /gSolve below.
                Err(e) => (500, json!({"status":"error","message": e.to_string()})),
            },
            None => (400, json!({"status":"error","message":"missing 'question'"})),
        },
        ("POST", "/gSolve") => match question_of(body) {
            Some(q) => match engine.ask(&q) {
                // gAnswer-compatible subset. Honor abstention: withhold answers
                // (they're already empty) and flag it so a client can't mistake a
                // suppressed guess for a confident result.
                Ok(a) => (
                    200,
                    json!({
                        "status": if a.abstained { "abstained" } else { "ok" },
                        "question": q,
                        "sparql": a.sparql,
                        "answers": a.values,
                        "confidence": round3(a.confidence),
                    }),
                ),
                Err(e) => (200, json!({"status":"error","question": q, "message": e.to_string()})),
            },
            None => (400, json!({"status":"error","message":"missing 'question'"})),
        },
        _ => (404, json!({"status":"error","message":"not found"})),
    }
}

/// Round an f32 to 3 decimals as f64, so JSON doesn't print f32 widening noise.
fn round3(x: f32) -> f64 {
    ((x * 1000.0).round() / 1000.0) as f64
}

/// Extract `question` from a JSON body (or a raw non-JSON body as the question).
fn question_of(body: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        return v["question"].as_str().map(str::to_string).filter(|s| !s.trim().is_empty());
    }
    let t = body.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Read method, path, and (Content-Length) body from an HTTP/1.1 request, with
/// bounded line/body sizes so a malicious client can't exhaust memory.
fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<(String, String, String)>> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let Some(request_line) = read_line_capped(&mut reader, MAX_LINE)? else {
        return Ok(None); // empty or over-long
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let (method, path) = (method.to_string(), target.split('?').next().unwrap_or("/").to_string());

    // Headers → find Content-Length (bounded line length).
    let mut content_length = 0usize;
    loop {
        let Some(line) = read_line_capped(&mut reader, MAX_LINE)? else {
            return Ok(None);
        };
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.split_once(':') {
            if v.0.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.1.trim().parse().unwrap_or(0);
            }
        }
    }

    if content_length > MAX_BODY {
        return Ok(None); // refuse oversized bodies (don't pre-allocate)
    }
    let mut body = Vec::new();
    (&mut reader).take(content_length as u64).read_to_end(&mut body)?;
    Ok(Some((method, path, String::from_utf8_lossy(&body).into_owned())))
}

/// Read one `\n`-terminated line (trailing `\r` stripped), capped at `max`
/// bytes. Returns `None` on EOF-with-nothing or if the line exceeds `max`.
fn read_line_capped<R: BufRead>(reader: &mut R, max: usize) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte)? == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > max {
            return Ok(None);
        }
    }
    let s = String::from_utf8_lossy(&buf).trim_end_matches('\r').to_string();
    Ok(Some(s))
}

/// Write a JSON response.
fn write_json(stream: &mut TcpStream, status: u16, payload: &Value) -> std::io::Result<()> {
    let body = payload.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
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
