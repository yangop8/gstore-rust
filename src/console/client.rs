//! The console's client of gserver's upstream `/api` protocol (upstream
//! `src/Api/APIConnector.cpp`): every operation is a `POST /api` whose JSON
//! body carries `operation`, the user's credentials and the operation's
//! parameters, answered with a JSON envelope (`StatusCode`, `StatusMsg`, …).
//!
//! Upstream's gconsole sends *inner* requests (`username` `root`, an empty
//! password and `inner=true`, which upstream's server trusts from
//! `127.0.0.1`); this client authenticates as the logged-in user instead, so
//! the server checks that user's privileges (and a server that does not
//! trust inner requests — this crate's default — works):
//!
//! * **a session** (review ext#12): the console logs in once with
//!   `POST /login` and sends the session token (`Authorization: Bearer`)
//!   with every `/api` request instead of the password, which then crosses
//!   the network once per login. A server whose session ended (it expired,
//!   or the user's password changed) answers `1001`, and the console logs in
//!   again and repeats the request (see `Console::request`).
//! * **upstream's protocol**, against a server without `POST /login` (an
//!   upstream gserver), or whose `/api` does not take its sessions (a
//!   gstore-rust gserver that predates them; see `ApiClient::log_in`):
//!   `username` / `password` in every request body.
//!
//! **HTTPS** (`--tls`, the `tls` cargo feature): the exchange runs over TLS
//! (`http_client::tls_client_stream`), the server's certificate
//! verified against `--ca-file` (else `$SSL_CERT_FILE` or the system
//! bundle).
//!
//! A call can be interrupted ([`ApiClient::call_interruptible`]): the
//! exchange runs on a helper thread while the caller watches for Ctrl-C
//! ([`super::term::interrupts`]); on one, the connection is shut down and
//! the call returns [`CallError::Interrupted`] at once.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::term;
use crate::json::{self, Json};

/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a TLS handshake may take.
#[cfg(feature = "tls")]
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the `POST /logout` at the end may take.
const LOGOUT_TIMEOUT: Duration = Duration::from_secs(5);
/// What `POST /login` answers wrong credentials with, and upstream's text
/// for them (`/api` `login`), which the console prints.
const REST_WRONG_CREDENTIALS: &str = "invalid username or password";
const UPSTREAM_WRONG_CREDENTIALS: &str = "Username or password is wrong.";

/// Why a call produced no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The server could not be reached (or its TLS certificate was refused).
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

/// What [`ApiClient::open_session`] got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Login {
    /// Logged in: the client holds a session.
    Session,
    /// The server refused the credentials, saying this.
    Refused(String),
    /// The server has no `POST /login` (an upstream gserver): log in with
    /// upstream's `/api` `login` and send the password with every request.
    Unsupported,
}

/// Why [`ApiClient::log_in`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginError {
    /// The server refused the credentials, saying this (unescaped).
    Refused(String),
    /// No answer.
    Call(CallError),
}

/// A client of one server, as one user.
#[derive(Debug, Clone)]
pub struct ApiClient {
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Kept to log in again when a session ends, and for the commands that
    /// ask the user to confirm it; sent only by
    /// [`open_session`](Self::open_session), unless the server has no
    /// sessions.
    pub password: String,
    /// HTTPS (`--tls`; needs the `tls` feature).
    pub tls: bool,
    /// The CA certificates to verify an HTTPS server with (`--ca-file`;
    /// `None`: `$SSL_CERT_FILE`, else the system bundle).
    pub ca_file: Option<PathBuf>,
    /// The session token of a `POST /login`, sent as `Authorization: Bearer`
    /// instead of the password in every body.
    pub session: Option<String>,
}

/// One HTTP request: its path, body and the body's media type.
struct Request<'a> {
    path: &'a str,
    content_type: &'a str,
    body: &'a str,
}

const JSON_TYPE: &str = "application/json";
const FORM_TYPE: &str = "application/x-www-form-urlencoded";

impl ApiClient {
    pub fn new(host: &str, port: u16) -> ApiClient {
        ApiClient {
            host: host.to_string(),
            port,
            username: String::new(),
            password: String::new(),
            tls: false,
            ca_file: None,
            session: None,
        }
    }

    /// `http://host:port/api` (upstream's `API_URL`; `https://` with TLS).
    pub fn url(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("{scheme}://[{}]:{}/api", self.host, self.port)
        } else {
            format!("{scheme}://{}:{}/api", self.host, self.port)
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
    /// credentials — the user name, and the password unless the client
    /// holds a session —, then the parameters.
    pub fn body(&self, operation: &str, params: Vec<(&str, Json)>) -> Json {
        let mut body = Json::obj()
            .with("operation", operation)
            .with("username", self.username.as_str());
        if self.session.is_none() {
            body.set("password", self.password.as_str());
        }
        for (k, v) in params {
            body.set(k, v);
        }
        body
    }

    /// Send `body`, waiting as long as the server takes (or `timeout`).
    pub fn call(&self, body: &Json, timeout: Option<Duration>) -> Result<Json, CallError> {
        let slot = Arc::new(Mutex::new(None));
        let text = body.to_string();
        let request = Request {
            path: "/api",
            content_type: JSON_TYPE,
            body: &text,
        };
        envelope(exchange(self, &request, timeout, &slot)?)
    }

    /// [`call`](Self::call), returning [`CallError::Interrupted`] as soon as
    /// a Ctrl-C arrives (the connection is then shut down; the server may
    /// still finish the operation).
    pub fn call_interruptible(&self, body: &Json) -> Result<Json, CallError> {
        envelope(self.send_interruptible("/api", JSON_TYPE, body.to_string())?)
    }

    /// Log in with `POST /login` (the password crosses the network here
    /// only): on success the client holds the session, which every later
    /// request presents instead of the password. Interruptible by Ctrl-C.
    pub fn open_session(&mut self) -> Result<Login, CallError> {
        self.session = None;
        let form = format!(
            "username={}&password={}",
            form_encode(&self.username),
            form_encode(&self.password)
        );
        let (status, body) = match self.send_interruptible("/login", FORM_TYPE, form) {
            Ok(answer) => answer,
            // No `/login` answer at all (a server that drops what it does
            // not know): upstream's protocol.
            Err(CallError::Transport(_)) => return Ok(Login::Unsupported),
            Err(e) => return Err(e),
        };
        let Ok(j @ Json::Obj(_)) = json::parse(&String::from_utf8_lossy(&body)) else {
            return Ok(Login::Unsupported);
        };
        let msg = j
            .get("StatusMsg")
            .and_then(Json::as_str)
            .unwrap_or_default();
        match status {
            // A token goes into a header: visible ASCII only.
            200 => match j.get("session").and_then(Json::as_str) {
                Some(token) if !token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic()) => {
                    self.session = Some(token.to_string());
                    Ok(Login::Session)
                }
                _ => Ok(Login::Unsupported),
            },
            401 if msg == REST_WRONG_CREDENTIALS => {
                Ok(Login::Refused(UPSTREAM_WRONG_CREDENTIALS.to_string()))
            }
            401 | 403 | 429 if !msg.is_empty() => Ok(Login::Refused(msg.to_string())),
            _ => Ok(Login::Unsupported),
        }
    }

    /// Log in as [`username`](Self::username) with
    /// [`password`](Self::password): a session
    /// ([`open_session`](Self::open_session)) when the server offers one
    /// and its `/api` takes it, else upstream's `/api` `login` (the
    /// password then goes with every request).
    ///
    /// The session is checked with `/api` `login`: a gstore-rust gserver
    /// older than the `/api` sessions has `POST /login` (its REST API) but
    /// answers a password-less `/api` request `1003`, and must still be
    /// served — with upstream's protocol.
    pub fn log_in(&mut self) -> Result<(), LoginError> {
        match self.open_session().map_err(LoginError::Call)? {
            Login::Session => {
                let body = self.body("login", vec![]);
                let j = self.call_interruptible(&body).map_err(LoginError::Call)?;
                if j.get("StatusCode").and_then(Json::as_i64) == Some(0) {
                    return Ok(());
                }
                // Not taken: end it, and log in the upstream way below.
                self.close_session();
            }
            Login::Refused(msg) => return Err(LoginError::Refused(msg)),
            Login::Unsupported => {}
        }
        let body = self.body("login", vec![]);
        let j = self.call_interruptible(&body).map_err(LoginError::Call)?;
        if j.get("StatusCode").and_then(Json::as_i64) == Some(0) {
            return Ok(());
        }
        let msg = j
            .get("StatusMsg")
            .and_then(Json::as_str)
            .unwrap_or_default();
        Err(LoginError::Refused(msg.to_string()))
    }

    /// End the session (`POST /logout`), if the client holds one; errors
    /// are ignored (the session expires anyway).
    pub fn close_session(&mut self) {
        if self.session.is_none() {
            return;
        }
        let slot = Arc::new(Mutex::new(None));
        let request = Request {
            path: "/logout",
            content_type: FORM_TYPE,
            body: "",
        };
        let _ = exchange(self, &request, Some(LOGOUT_TIMEOUT), &slot);
        self.session = None;
    }

    /// `POST` `body` to `path` on a helper thread, returning
    /// [`CallError::Interrupted`] as soon as a Ctrl-C arrives (see the
    /// module docs): the status and body of the answer.
    fn send_interruptible(
        &self,
        path: &'static str,
        content_type: &'static str,
        body: String,
    ) -> Result<(u16, Vec<u8>), CallError> {
        let mark = term::interrupts();
        let slot: Arc<Mutex<Option<TcpStream>>> = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel();
        let client = self.clone();
        let worker_slot = Arc::clone(&slot);
        thread::Builder::new()
            .name("gconsole-request".into())
            .spawn(move || {
                let request = Request {
                    path,
                    content_type,
                    body: &body,
                };
                let _ = tx.send(exchange(&client, &request, None, &worker_slot));
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

/// `text` percent-encoded for an `application/x-www-form-urlencoded` body
/// (every byte but the unreserved ones).
fn form_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// An `/api` answer: its JSON envelope.
fn envelope((status, body): (u16, Vec<u8>)) -> Result<Json, CallError> {
    let text = String::from_utf8_lossy(&body);
    match json::parse(&text) {
        Ok(j @ Json::Obj(_)) => Ok(j),
        _ => {
            // Quoted for the operator: escaped, so it cannot drive the
            // terminal (review ext#10).
            let snippet: String = text.chars().take(200).collect();
            Err(CallError::Transport(format!(
                "unexpected answer (HTTP {status}): {}",
                super::table::printable(&snippet, false)
            )))
        }
    }
}

/// One `POST` exchange with `client`'s server (over TLS with
/// [`ApiClient::tls`], the session as `Authorization: Bearer`): the status
/// and (de-chunked) body of the answer. The connection is published in
/// `slot` while it is open (so another thread can shut it down).
fn exchange(
    client: &ApiClient,
    request: &Request<'_>,
    timeout: Option<Duration>,
    slot: &Mutex<Option<TcpStream>>,
) -> Result<(u16, Vec<u8>), CallError> {
    let (host, port) = (client.host.as_str(), client.port);
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
    let Some(stream) = stream else {
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
    let auth = client
        .session
        .as_deref()
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         User-Agent: gconsole (gstore-rust)\r\n\
         Content-Type: {ctype}\r\n\
         Accept: application/json\r\n\
         {auth}\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n",
        path = request.path,
        ctype = request.content_type,
        len = request.body.len()
    );
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(request.body.as_bytes());
    let raw = if client.tls {
        tls_exchange(stream, client, &bytes, timeout)
    } else {
        plain_exchange(stream, &bytes)
    };
    slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    parse_response(&raw?).map_err(CallError::Transport)
}

fn transport(e: io::Error) -> CallError {
    CallError::Transport(format!("request failed: {e}"))
}

/// Send `request` and read the answer to the end, in the clear.
fn plain_exchange(mut stream: TcpStream, request: &[u8]) -> Result<Vec<u8>, CallError> {
    stream.write_all(request).map_err(transport)?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(transport)?;
    Ok(raw)
}

/// Send `request` and read the answer to the end over TLS: the handshake
/// first (bounded by [`HANDSHAKE_TIMEOUT`]; a certificate the CA bundle
/// does not vouch for is a [`CallError::Connect`] and nothing is sent),
/// then the exchange under the call's `timeout`.
#[cfg(feature = "tls")]
fn tls_exchange(
    stream: TcpStream,
    client: &ApiClient,
    request: &[u8],
    timeout: Option<Duration>,
) -> Result<Vec<u8>, CallError> {
    let host = client.host.trim_start_matches('[').trim_end_matches(']');
    let refused = |e: io::Error| {
        CallError::Connect(format!(
            "TLS with {}:{} failed: {e}",
            client.host, client.port
        ))
    };
    let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT));
    let mut tls = crate::http_client::tls_client_stream(stream, host, client.ca_file.as_deref())
        .map_err(refused)?;
    while tls.conn.is_handshaking() {
        tls.conn.complete_io(&mut tls.sock).map_err(refused)?;
    }
    let _ = tls.sock.set_read_timeout(timeout);
    let _ = tls.sock.set_write_timeout(timeout);
    tls.write_all(request).map_err(transport)?;
    tls.flush().map_err(transport)?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // A server that closes without `close_notify` after its answer
            // (`Connection: close`): keep what arrived.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && !raw.is_empty() => break,
            Err(e) => return Err(transport(e)),
        }
    }
    Ok(raw)
}

#[cfg(not(feature = "tls"))]
fn tls_exchange(
    _stream: TcpStream,
    _client: &ApiClient,
    _request: &[u8],
    _timeout: Option<Duration>,
) -> Result<Vec<u8>, CallError> {
    Err(CallError::Connect(
        "HTTPS needs a gconsole built with the `tls` cargo feature".to_string(),
    ))
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
    fn a_session_replaces_the_password_in_bodies() {
        let mut c = ApiClient::new("127.0.0.1", 9000);
        c.username = "root".into();
        c.password = "123456".into();
        c.session = Some("t0k".into());
        let b = c.body("load", vec![("db_name", Json::from("lubm"))]);
        assert_eq!(
            b.to_string(),
            r#"{"operation":"load","username":"root","db_name":"lubm"}"#
        );
        c.tls = true;
        assert_eq!(c.url(), "https://127.0.0.1:9000/api");
        assert_eq!(form_encode("a b&c=d+é~"), "a%20b%26c%3Dd%2B%C3%A9~");
    }

    #[test]
    fn unreachable_servers_are_connect_errors() {
        // Port 0: nothing can listen on it (review tests-a#7: a port bound
        // and freed again may be taken by another test's server meanwhile).
        let c = ApiClient::new("127.0.0.1", 0);
        let r = c.call(&c.body("login", vec![]), Some(Duration::from_secs(5)));
        assert!(matches!(r, Err(CallError::Connect(_))), "{r:?}");
    }
}
