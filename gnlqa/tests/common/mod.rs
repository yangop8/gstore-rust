//! Shared helpers for gnlqa's integration tests: a raw HTTP/1.1 client for
//! the gnlqa server, and a scripted local HTTP stub that stands in for the
//! gStore SPARQL endpoint and the LLM / embedding providers (review
//! cluster-nlqa#24: the real clients are exercised over a socket, not mocked).
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// A parsed HTTP response.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|e| panic!("response body is not JSON ({e}): {:?}", self.body))
    }
}

/// Parse a raw HTTP response (status line, headers, body).
pub fn parse_response(raw: &str) -> Response {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Response {
        status,
        headers,
        body: body.to_string(),
    }
}

/// Send raw request bytes and read the response until the server closes.
pub fn send_raw(addr: SocketAddr, raw: &[u8]) -> Response {
    let mut s = TcpStream::connect(addr).expect("connect to the server under test");
    // hang guard only: a correct server always answers and closes
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    s.write_all(raw).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    parse_response(&String::from_utf8_lossy(&buf))
}

/// `POST path` with a body and an optional bearer token.
pub fn post(addr: SocketAddr, path: &str, body: &str, token: Option<&str>) -> Response {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: test\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    send_raw(addr, req.as_bytes())
}

/// `GET path`.
pub fn get(addr: SocketAddr, path: &str) -> Response {
    let req = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    send_raw(addr, req.as_bytes())
}

/// One request the stub received.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A scripted reply: status, extra headers, body.
#[derive(Debug, Clone)]
pub struct Scripted {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Scripted {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Scripted {
        Scripted {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body: body.into(),
        }
    }
    pub fn json(status: u16, body: &str) -> Scripted {
        Scripted::new(status, "application/json", body.as_bytes().to_vec())
    }
    pub fn header(mut self, k: &str, v: &str) -> Scripted {
        self.headers.push((k.into(), v.into()));
        self
    }
}

/// A local HTTP stub: answers each request with the next scripted reply (the
/// last one repeats once the script is exhausted) and records every request.
/// Dropping it stops the accept loop and joins its thread.
pub struct Stub {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Stub {
    pub fn start(script: Vec<Scripted>) -> Stub {
        assert!(!script.is_empty(), "a stub needs at least one reply");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (reqs, stop2) = (Arc::clone(&requests), Arc::clone(&stop));
        let handle = thread::spawn(move || {
            let mut next = 0usize;
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut conn) = conn else { continue };
                let _ = conn.set_read_timeout(Some(Duration::from_secs(30)));
                let Some(rec) = read_one(&mut conn) else {
                    continue;
                };
                reqs.lock().unwrap().push(rec);
                let reply = &script[next.min(script.len() - 1)];
                next += 1;
                let mut head = format!(
                    "HTTP/1.1 {} Stub\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    reply.body.len()
                );
                for (k, v) in &reply.headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = conn.write_all(head.as_bytes());
                let _ = conn.write_all(&reply.body);
                let _ = conn.flush();
            }
        });
        Stub {
            addr,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    /// `http://127.0.0.1:<port>` + `path`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr); // wake the accept loop
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn read_one(conn: &mut TcpStream) -> Option<Recorded> {
    let mut r = BufReader::new(conn.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    Some(Recorded {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// A port on 127.0.0.1 with nothing listening: port 0, which nothing can
/// listen on, so a connection to it is refused at once. (Round-5 review
/// follow-up of tests-a#7: a port bound and released here could be taken
/// by a server or stub that a test running in parallel binds to port 0, and
/// the "dead" endpoint then answered.)
pub fn closed_port() -> u16 {
    0
}

/// A uniquely named temporary directory, removed (recursively) on drop —
/// also when the test panics.
pub struct TempDir(pub std::path::PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        use std::sync::atomic::AtomicUsize;
        static N: AtomicUsize = AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!(
            "gnlqa-test-{tag}-{}-{}-{nanos}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
