//! A small HTTP/1.1 client for the tests, with one overall deadline per
//! request (review tests-a#8, tests-b#11).
//!
//! Each request opens a fresh connection, sends `Connection: close` and reads
//! to the end of the stream. Connecting, writing and reading together must
//! finish within the request's timeout ([`TIMEOUT`] unless the caller gives
//! one): a handler that never answers (a lock-order regression, a lost
//! wake-up) makes the test panic with the request line and what had arrived,
//! instead of blocking in `read_to_end` until the CI job is killed. The
//! timeout is a hang guard only, never the assertion: it is generous enough
//! for a debug build on a loaded machine.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use gstore::json::{self, Json};

/// The default overall deadline of one request.
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// A response.
#[derive(Debug, Clone)]
pub struct Reply {
    /// The status code of the status line.
    pub status: u16,
    /// The header fields, in order, names as sent.
    pub headers: Vec<(String, String)>,
    /// The bytes after the head, as received (not de-chunked).
    pub body: Vec<u8>,
}

impl Reply {
    /// The first header field called `name` (ASCII case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as text (invalid UTF-8 replaced).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body as JSON; panics with the body when it is not.
    pub fn json(&self) -> Json {
        let text = self.text();
        json::parse(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
    }
}

/// The request line of a raw request, for messages.
fn request_line(raw: &[u8]) -> String {
    let end = raw
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or(raw.len())
        .min(200);
    String::from_utf8_lossy(&raw[..end]).into_owned()
}

/// Send the raw request `raw` to `addr` on a new connection and return every
/// byte the server sent until it closed the connection. Panics, naming the
/// request, when that takes longer than `timeout` in all (or the connection
/// fails).
pub fn exchange(addr: &SocketAddr, raw: &[u8], timeout: Duration) -> Vec<u8> {
    let what = request_line(raw);
    let deadline = Instant::now() + timeout;
    let mut s = TcpStream::connect_timeout(addr, timeout)
        .unwrap_or_else(|e| panic!("{what} (to {addr}): cannot connect: {e}"));
    s.set_write_timeout(Some(timeout)).unwrap();
    if let Err(e) = s.write_all(raw) {
        panic!(
            "{what} (to {addr}): the request could not be sent within {timeout:?} \
             (a server that stopped reading?): {e}"
        );
    }
    let mut resp = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            panic!(
                "{what} (to {addr}): no complete response within {timeout:?} (a hung \
                 server?); {} byte(s) arrived: {:?}",
                resp.len(),
                String::from_utf8_lossy(&resp[..resp.len().min(300)])
            );
        }
        s.set_read_timeout(Some(left)).unwrap();
        match s.read(&mut buf) {
            Ok(0) => return resp,
            Ok(n) => resp.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => panic!("{what} (to {addr}): reading the response: {e}"),
        }
    }
}

/// Split a raw response into status, header fields and body; panics with
/// `what` and the bytes when it has no status line or no end of head.
pub fn parse(raw: Vec<u8>, what: &str) -> Reply {
    let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        panic!(
            "{what}: no end of the response head in {:?}",
            String::from_utf8_lossy(&raw)
        );
    };
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .filter(|l| l.starts_with("HTTP/"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("{what}: no status line in {head:?}"));
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Reply {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    }
}

/// The bytes of a request: the request line, `Host: 127.0.0.1` unless
/// `headers` has a `Host`, `headers`, `Content-Length` (unless `headers` has
/// one), `Connection: close`, then `body`.
pub fn raw_request(method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let has = |name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    let mut raw = format!("{method} {target} HTTP/1.1\r\n");
    if !has("Host") {
        raw.push_str("Host: 127.0.0.1\r\n");
    }
    for (k, v) in headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    if !has("Content-Length") {
        raw.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    raw.push_str("Connection: close\r\n\r\n");
    let mut bytes = raw.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// One request with the default deadline ([`TIMEOUT`]).
pub fn request(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    request_with_timeout(addr, method, target, headers, body, TIMEOUT)
}

/// One request that must be answered within `timeout`.
pub fn request_with_timeout(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> Reply {
    let raw = raw_request(method, target, headers, body);
    let what = format!("{method} {target} (to {addr})");
    parse(exchange(addr, &raw, timeout), &what)
}
