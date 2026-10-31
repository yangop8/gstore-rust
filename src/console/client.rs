//! The console's client of gserver's upstream `/api` protocol (upstream
//! `src/Api/APIConnector.cpp`): every operation is a `POST /api` whose JSON
//! body carries `operation`, the user's `username` / `password` and the
//! operation's parameters, answered with a JSON envelope (`StatusCode`,
//! `StatusMsg`, …).
//!
//! Upstream's gconsole sends *inner* requests (`username` `root`, an empty
//! password and `inner=true`, which upstream's server trusts from
//! `127.0.0.1`); this client sends the logged-in user's own credentials with
//! every request instead, so the server checks that user's privileges (and a
//! server that does not trust inner requests — this crate's default —
//! works).
//!
//! A call can be interrupted ([`ApiClient::call_interruptible`]): the
//! exchange runs on a helper thread while the caller watches for Ctrl-C
//! ([`super::term::interrupts`]); on one, the connection is shut down and
//! the call returns [`CallError::Interrupted`] at once.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::term;
use crate::json::{self, Json};

/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a call produced no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The server could not be reached.
    Connect(String),
    /// The exchange failed, or the answer is not an `/api` JSON envelope.
    Transport(String),
    /// Ctrl-C: the connection was dropped.
    Interrupted,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Connect(e) | CallError::Transport(e) => f.write_str(e),
            CallError::Interrupted => f.write_str("interrupted"),
        }
    }
}

/// A client of one server, as one user.
#[derive(Debug, Clone)]
pub struct ApiClient {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl ApiClient {
    pub fn new(host: &str, port: u16) -> ApiClient {
        ApiClient {
            host: host.to_string(),
            port,
            username: String::new(),
            password: String::new(),
        }
    }

    /// `http://host:port/api` (upstream's `API_URL`).
    pub fn url(&self) -> String {
        if self.host.contains(':') {
            format!("http://[{}]:{}/api", self.host, self.port)
        } else {
            format!("http://{}:{}/api", self.host, self.port)
        }
    }

    /// Whether the server runs on this machine (a loopback host).
    pub fn is_local(&self) -> bool {
        let h = self.host.trim_start_matches('[').trim_end_matches(']');
        h == "localhost"
            || h.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
    }

    /// The request body of `operation` with `params`: `operation`, the
    /// credentials, then the parameters.
    pub fn body(&self, operation: &str, params: Vec<(&str, Json)>) -> Json {
        let mut body = Json::obj()
            .with("operation", operation)
            .with("username", self.username.as_str())
            .with("password", self.password.as_str());
        for (k, v) in params {
            body.set(k, v);
        }
        body
    }

    /// Send `body`, waiting as long as the server takes (or `timeout`).
    pub fn call(&self, body: &Json, timeout: Option<Duration>) -> Result<Json, CallError> {
        let slot = Arc::new(Mutex::new(None));
        exchange(&self.host, self.port, &body.to_string(), timeout, &slot)
    }

    /// [`call`](Self::call), returning [`CallError::Interrupted`] as soon as
    /// a Ctrl-C arrives (the connection is then shut down; the server may
    /// still finish the operation).
    pub fn call_interruptible(&self, body: &Json) -> Result<Json, CallError> {
        let mark = term::interrupts();
        let slot: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel();
        let (host, port, text) = (self.host.clone(), self.port, body.to_string());
        let worker_slot = Arc::clone(&slot);
        thread::Builder::new()
            .name("gconsole-request".into())
            .spawn(move || {
                let _ = tx.send(exchange(&host, port, &text, None, &worker_slot));
            })
            .map_err(|e| CallError::Transport(format!("cannot start a request: {e}")))?;
        loop {
            match rx.recv_timeout(Duration::from_millis(40)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(CallError::Transport("the request was lost".to_string()))
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if term::interrupts() != mark {
                if let Some(stream) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = stream.shutdown(Shutdown::Both);
                }
                return Err(CallError::Interrupted);
            }
        }
    }
}

/// One `POST /api` exchange; the connection is published in `slot` while
/// it is open (so another thread can shut it down).
fn exchange(
    host: &str,
    port: u16,
    body: &str,
    timeout: Option<Duration>,
    slot: &Mutex<Option<TcpStream>>,
) -> Result<Json, CallError> {
    let host_name = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<_> = (host_name, port)
        .to_socket_addrs()
        .map_err(|e| CallError::Connect(format!("cannot resolve {host}: {e}")))?
        .collect();
    let mut last = None;
    let mut stream = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let Some(mut stream) = stream else {
        let why = last.map_or_else(|| "no address".to_string(), |e| e.to_string());
        return Err(CallError::Connect(format!(
            "cannot connect to {host}:{port}: {why}"
        )));
    };
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    if let Ok(clone) = stream.try_clone() {
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(clone);
    }
    let host_header = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let request = format!(
        "POST /api HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         User-Agent: gconsole (gstore-rust)\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let io_err = |e: io::Error| CallError::Transport(format!("request failed: {e}"));
    stream.write_all(request.as_bytes()).map_err(io_err)?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(io_err)?;
    slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    let (status, body) = parse_response(&raw).map_err(CallError::Transport)?;
    let text = String::from_utf8_lossy(&body);
    match json::parse(&text) {
        Ok(j @ Json::Obj(_)) => Ok(j),
        _ => {
            let snippet: String = text.chars().take(200).collect();
            Err(CallError::Transport(format!(
                "unexpected answer (HTTP {status}): {snippet}"
            )))
        }
    }
}

/// Split a raw HTTP response into its status code and (de-chunked) body.
pub fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            if raw.is_empty() {
                "the server closed the connection without answering".to_string()
            } else {
                "malformed HTTP response".to_string()
            }
        })?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| "malformed HTTP status line".to_string())?;
    let mut chunked = false;
    let mut length = None;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked") {
            chunked = true;
        } else if k.eq_ignore_ascii_case("content-length") {
            length = v.parse::<usize>().ok();
        }
    }
    let rest = &raw[split + 4..];
    let body = if chunked {
        dechunk(rest)?
    } else if let Some(n) = length {
        rest[..n.min(rest.len())].to_vec()
    } else {
        rest.to_vec()
    };
    Ok((status, body))
}

/// Decode a `Transfer-Encoding: chunked` body.
fn dechunk(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let eol = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("truncated chunked body")?;
        let size_text = String::from_utf8_lossy(&data[..eol]);
        let size_hex = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| "bad chunk size")?;
        data = &data[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size {
            return Err("truncated chunk".to_string());
        }
        out.extend_from_slice(&data[..size]);
        data = data.get(size + 2..).unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_are_parsed() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}extra";
        assert_eq!(parse_response(raw).unwrap(), (200, b"{}".to_vec()));
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n4;x=1\r\n\":1}\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), (200, b"{\"a\":1}".to_vec()));
        let raw = b"HTTP/1.0 404 Not Found\r\n\r\nnope";
        assert_eq!(parse_response(raw).unwrap(), (404, b"nope".to_vec()));
        assert!(parse_response(b"").is_err());
    }

    #[test]
    fn bodies_carry_credentials_first() {
        let mut c = ApiClient::new("127.0.0.1", 9000);
        c.username = "root".into();
        c.password = "123456".into();
        let b = c.body("load", vec![("db_name", Json::from("lubm"))]);
        assert_eq!(
            b.to_string(),
            r#"{"operation":"load","username":"root","password":"123456","db_name":"lubm"}"#
        );
        assert_eq!(c.url(), "http://127.0.0.1:9000/api");
        assert!(c.is_local());
        assert!(!ApiClient::new("db.example.org", 9000).is_local());
        assert!(ApiClient::new("::1", 9000).is_local());
        assert_eq!(ApiClient::new("::1", 1).url(), "http://[::1]:1/api");
    }

    #[test]
    fn unreachable_servers_are_connect_errors() {
        // A port nothing listens on: bind one, then free it.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let c = ApiClient::new("127.0.0.1", port);
        let r = c.call(&c.body("login", vec![]), Some(Duration::from_secs(5)));
        assert!(matches!(r, Err(CallError::Connect(_))), "{r:?}");
    }
}
