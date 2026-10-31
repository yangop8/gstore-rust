//! A minimal, dependency-free HTTP/1.1 client: SPARQL `SERVICE` federation —
//! the counterpart to the [`crate::server`] endpoint — and generic `POST`s
//! such as the asynchronous-operation callbacks of [`crate::gapi::tasks`].
//!
//! It does exactly enough to drive a federated query: open a TCP connection,
//! `POST` a SPARQL query to an `http://host:port/path` endpoint, read the whole
//! response, and parse the returned SPARQL 1.1 Query Results JSON into bindings.
//! [`post`] / [`post_with`] send any body (e.g. a JSON callback) the same way.
//!
//! Scope (mirroring `server.rs`): `http://` (and, for [`post_with`] with the
//! optional `tls` feature, `https://`; `SERVICE` stays `http://` only),
//! `Connection: close` one-shot requests, and a hand-rolled JSON reader
//! covering the result shape SPARQL endpoints emit. The body is read to EOF
//! after the headers, so it assumes a `Content-Length` / connection-close
//! response — `Transfer-Encoding: chunked` and `gzip` are NOT decoded (works
//! against this crate's own `server.rs`; some public endpoints chunk). Auth
//! and redirects are out of scope. Every exchange is bounded by [`Limits`].

use std::io::{self, Read, Write};
use std::net::{IpAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::model::Term;

/// A single returned solution: each bound variable mapped to its RDF term.
pub type ServiceSolution = Vec<(String, Term)>;

/// Bounds on one [`sparql_post`] exchange, so a slow, dead or hostile
/// endpoint cannot hold the query (and the database lock it runs under)
/// indefinitely, nor make it buffer unbounded data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The whole exchange — connect, send, receive — must finish within this.
    /// (A per-read timeout alone would let a peer trickling one byte at a time
    /// keep it going forever.)
    pub total: Duration,
    /// Largest response accepted, headers included.
    pub max_response: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            total: Duration::from_secs(30),
            max_response: 64 << 20,
        }
    }
}

/// POST `query` to a SPARQL endpoint at `url` and return the raw response body.
///
/// The body is sent verbatim with `Content-Type: application/sparql-query`,
/// which [`crate::server`] accepts as the query text. Errors on a non-2xx
/// status so callers can treat a remote failure like a connection failure, and
/// when the exchange exceeds the default [`Limits`].
pub fn sparql_post(url: &str, query: &str) -> io::Result<String> {
    sparql_post_with(url, query, Limits::default())
}

/// [`sparql_post`] with explicit [`Limits`].
pub fn sparql_post_with(url: &str, query: &str, limits: Limits) -> io::Result<String> {
    let (host, port, path) = parse_http_url(url)?;
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
    let target = HttpUrl {
        tls: false,
        host,
        port,
        path,
    };
    let raw = exchange(
        &target,
        request.as_bytes(),
        limits,
        None,
        false,
        "SERVICE endpoint",
    )?;
    let reply = parse_reply(&raw)?;
    if !reply.is_success() {
        return Err(io::Error::other(format!(
            "endpoint returned HTTP {}",
            reply.status
        )));
    }
    Ok(reply.body)
}

/// The response to a [`post`]: its HTTP status and body (decoded lossily as
/// UTF-8; a chunked body is returned as sent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpReply {
    pub status: u16,
    pub body: String,
}

impl HttpReply {
    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Options of [`post_with`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostOptions {
    /// Bounds on the exchange (time and response size).
    pub limits: Limits,
    /// For `https://` (feature `tls`): a PEM file of the CA certificates to
    /// trust. `None` = the system bundle (`$SSL_CERT_FILE`, else the usual
    /// Linux / BSD / macOS locations).
    pub ca_file: Option<PathBuf>,
    /// Refuse to connect to a host-local address ([`is_local_addr`]):
    /// checked on every address the host resolves to, right before
    /// connecting to it (so a DNS answer cannot slip one past a check made
    /// earlier).
    pub refuse_local: bool,
}

/// Whether `ip` reaches this host or its link rather than a network service
/// a client may name: loopback, unspecified, link-local (cloud metadata
/// endpoints live there), multicast and broadcast addresses — IPv4, IPv6,
/// and IPv4 mapped into IPv6.
pub fn is_local_addr(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_local_addr(&IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// `POST` `body` (labelled `content_type`) to `url` with the default
/// [`PostOptions`]; see [`post_with`].
pub fn post(url: &str, content_type: &str, body: &[u8]) -> io::Result<HttpReply> {
    post_with(url, content_type, body, &PostOptions::default())
}

/// `POST` `body` (labelled `content_type`) to `url` — `http://host[:port]/path`,
/// or `https://…` when the crate is built with the `tls` feature — and return
/// the response status and body, whatever the status. Errors are transport
/// failures: an unsupported or malformed URL, a refused connection, the
/// exchange exceeding [`Limits::total`], a response over
/// [`Limits::max_response`], a malformed response.
pub fn post_with(
    url: &str,
    content_type: &str,
    body: &[u8],
    opts: &PostOptions,
) -> io::Result<HttpReply> {
    let target = parse_url(url)?;
    if content_type.contains(['\r', '\n']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "malformed Content-Type",
        ));
    }
    let default_port = if target.tls { 443 } else { 80 };
    let host_header = if target.port == default_port {
        host_for_header(&target.host)
    } else {
        format!("{}:{}", host_for_header(&target.host), target.port)
    };
    let head = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         User-Agent: gstore-rust/0.1\r\n\
         Accept: */*\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\r\n",
        path = target.path,
        len = body.len()
    );
    let mut request = head.into_bytes();
    request.extend_from_slice(body);
    let raw = exchange(
        &target,
        &request,
        opts.limits,
        opts.ca_file.as_deref(),
        opts.refuse_local,
        "HTTP endpoint",
    )?;
    parse_reply(&raw)
}

/// The response to a [`get_with`]: its HTTP status and raw body bytes (a
/// `Transfer-Encoding: chunked` body is decoded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpGetReply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpGetReply {
    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// `GET` `url` (`http://`, or `https://` with the `tls` feature) asking for
/// `accept`, within `opts` (time, size, local-address policy), and return the
/// status and body, whatever the status. Used to download RDF sources
/// (`gbuild`/`gadd` URLs, `/api` `remote` files, SPARQL `LOAD`).
pub fn get_with(url: &str, accept: &str, opts: &PostOptions) -> io::Result<HttpGetReply> {
    let target = parse_url(url)?;
    if accept.contains(['\r', '\n']) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "malformed Accept"));
    }
    let default_port = if target.tls { 443 } else { 80 };
    let host_header = if target.port == default_port {
        host_for_header(&target.host)
    } else {
        format!("{}:{}", host_for_header(&target.host), target.port)
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         User-Agent: gstore-rust/0.1\r\n\
         Accept: {accept}\r\n\
         Connection: close\r\n\r\n",
        path = target.path,
    );
    let raw = exchange(
        &target,
        request.as_bytes(),
        opts.limits,
        opts.ca_file.as_deref(),
        opts.refuse_local,
        "HTTP endpoint",
    )?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed HTTP response"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing HTTP status"))?;
    let chunked = head.lines().skip(1).any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
        })
    });
    let body = &raw[split + 4..];
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_vec()
    };
    Ok(HttpGetReply { status, body })
}

/// Decode a `Transfer-Encoding: chunked` body.
fn dechunk(mut b: &[u8]) -> io::Result<Vec<u8>> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed chunked body");
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n").ok_or_else(bad)?;
        let size_line = std::str::from_utf8(&b[..eol]).map_err(|_| bad())?;
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| bad())?;
        b = &b[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size + 2 {
            return Err(bad());
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

/// A parsed `http://` / `https://` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpUrl {
    /// `https://`.
    pub tls: bool,
    /// The host name or IP address (an IPv6 literal without its brackets).
    pub host: String,
    pub port: u16,
    /// The path and query (`/` when the URL has none).
    pub path: String,
}

/// Parse an `http://host[:port][/path]` URL — or `https://…` when the crate
/// is built with the `tls` feature (otherwise an `https://` URL is an
/// `Unsupported` error). A fragment is dropped; user info is refused.
pub fn parse_url(url: &str) -> io::Result<HttpUrl> {
    let url = url.trim();
    let url = url.split_once('#').map_or(url, |(u, _)| u);
    let (tls, rest) = if let Some(rest) = strip_prefix_ci(url, "http://") {
        (false, rest)
    } else if let Some(rest) = strip_prefix_ci(url, "https://") {
        if !cfg!(feature = "tls") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "https:// needs the `tls` feature; only http:// is supported",
            ));
        }
        (true, rest)
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only http:// and https:// URLs are supported",
        ));
    };
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) if rest.as_bytes()[i] == b'?' => (&rest[..i], format!("/{}", &rest[i..])),
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    if authority.contains('@') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "URLs with user info are not supported",
        ));
    }
    let default_port = if tls { 443 } else { 80 };
    let bad_port = || io::Error::new(io::ErrorKind::InvalidInput, "bad port");
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (host, after) = v6
            .split_once(']')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad IPv6 host"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse::<u16>().map_err(|_| bad_port())?,
            None if after.is_empty() => default_port,
            None => return Err(bad_port()),
        };
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| bad_port())?),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty host"));
    }
    if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "malformed path (whitespace or control characters)",
        ));
    }
    Ok(HttpUrl {
        tls,
        host,
        port,
        path,
    })
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
}

/// `host` as written in a `Host:` header (an IPv6 literal in brackets).
fn host_for_header(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Connect to `target`, send `request` and read the raw response to EOF, all
/// within `limits`: the deadline covers connect, send and receive (a per-read
/// timeout alone would let a peer trickling one byte at a time go on
/// forever), and the response may not exceed the size cap.
fn exchange(
    target: &HttpUrl,
    request: &[u8],
    limits: Limits,
    ca_file: Option<&Path>,
    refuse_local: bool,
    what: &str,
) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + limits.total;
    let timed_out = || io::Error::new(io::ErrorKind::TimedOut, format!("{what} timed out"));
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(timed_out)
    };
    let mut stream = None;
    let mut last_err = io::Error::new(io::ErrorKind::NotFound, "endpoint host did not resolve");
    for addr in (target.host.as_str(), target.port).to_socket_addrs()? {
        if refuse_local && is_local_addr(&addr.ip()) {
            last_err = io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{what} address {} is host-local; refused", addr.ip()),
            );
            continue;
        }
        match TcpStream::connect_timeout(&addr, remaining()?) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = e,
        }
    }
    let mut stream = stream.ok_or(last_err)?;
    stream.set_write_timeout(Some(remaining()?))?;
    // A second handle on the socket re-arms the read timeout while the
    // (possibly TLS-wrapped) stream is borrowed for reading.
    let ctl = stream.try_clone()?;
    let arm = || -> io::Result<()> { ctl.set_read_timeout(Some(remaining()?)) };
    if target.tls {
        #[cfg(feature = "tls")]
        {
            let mut tls = tls_client::connect(stream, &target.host, ca_file)?;
            tls.write_all(request)?;
            tls.flush()?;
            return read_to_eof(&mut tls, &arm, limits, &timed_out, what);
        }
        #[cfg(not(feature = "tls"))]
        {
            let _ = ca_file;
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "https:// needs the `tls` feature",
            ));
        }
    }
    stream.write_all(request)?;
    stream.flush()?;
    read_to_eof(&mut stream, &arm, limits, &timed_out, what)
}

/// Read to EOF, re-arming the read timeout with what is left of the deadline
/// before every read (`arm`), and stop at the size cap.
fn read_to_eof<S: Read>(
    stream: &mut S,
    arm: &dyn Fn() -> io::Result<()>,
    limits: Limits,
    timed_out: &dyn Fn() -> io::Error,
    what: &str,
) -> io::Result<Vec<u8>> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        arm()?;
        let n = match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err(timed_out())
            }
            // A TLS peer that closes without `close_notify` after sending its
            // response (common with `Connection: close`): keep what arrived.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof && !raw.is_empty() => break,
            Err(e) => return Err(e),
        };
        if raw.len() + n > limits.max_response {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{what} response larger than {} bytes", limits.max_response),
            ));
        }
        raw.extend_from_slice(&buf[..n]);
    }
    Ok(raw)
}

/// Split a raw HTTP response into its status and body.
fn parse_reply(raw: &[u8]) -> io::Result<HttpReply> {
    let text = String::from_utf8_lossy(raw);
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
    Ok(HttpReply {
        status,
        body: body.to_string(),
    })
}

/// The `https://` client side (feature `tls`): `rustls` with the pure-`ring`
/// backend, trusting the CA certificates of a PEM bundle.
#[cfg(feature = "tls")]
mod tls_client {
    use std::io;
    use std::net::TcpStream;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    /// Where the system CA bundle usually lives.
    const SYSTEM_BUNDLES: &[&str] = &[
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/ca-bundle.pem",
        "/etc/ssl/cert.pem",
        "/usr/local/etc/openssl/cert.pem",
    ];

    fn bundle_path(ca_file: Option<&Path>) -> io::Result<PathBuf> {
        if let Some(p) = ca_file {
            return Ok(p.to_path_buf());
        }
        if let Some(p) = std::env::var_os("SSL_CERT_FILE") {
            return Ok(PathBuf::from(p));
        }
        SYSTEM_BUNDLES
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no CA certificate bundle found for https:// (set SSL_CERT_FILE)",
                )
            })
    }

    /// Start a TLS session to `host` over `sock`, verifying the server's
    /// certificate against the CA bundle.
    pub(super) fn connect(
        sock: TcpStream,
        host: &str,
        ca_file: Option<&Path>,
    ) -> io::Result<rustls::StreamOwned<rustls::ClientConnection, TcpStream>> {
        let path = bundle_path(ca_file)?;
        let pem = std::fs::read(&path)?;
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut io::BufReader::new(pem.as_slice())) {
            // A certificate the store rejects is skipped, not fatal.
            let _ = roots.add(cert?);
        }
        if roots.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("no usable CA certificate in {}", path.display()),
            ));
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host.to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let conn =
            rustls::ClientConnection::new(Arc::new(config), name).map_err(io::Error::other)?;
        Ok(rustls::StreamOwned::new(conn, sock))
    }
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
    use std::net::TcpListener;

    /// Serve one connection: read the request, then run `respond` on it.
    fn one_shot(respond: impl FnOnce(TcpStream) + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/sparql", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            respond(s);
        });
        url
    }

    /// A peer that trickles its response (each byte well within a per-read
    /// timeout) cannot stretch the exchange past the total deadline.
    #[test]
    fn a_trickling_endpoint_hits_the_total_deadline() {
        let url = one_shot(|mut s| {
            for _ in 0..100 {
                if s.write_all(b"x").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let limits = Limits {
            total: Duration::from_millis(700),
            ..Limits::default()
        };
        let started = Instant::now();
        let err = sparql_post_with(&url, "SELECT * WHERE { ?s ?p ?o }", limits).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    }

    /// A response over the size cap is refused instead of buffered whole.
    #[test]
    fn an_oversized_response_is_refused() {
        let url = one_shot(|mut s| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
            let _ = s.write_all(&vec![b' '; 300 * 1024]);
        });
        let limits = Limits {
            max_response: 64 * 1024,
            ..Limits::default()
        };
        let err = sparql_post_with(&url, "ASK {}", limits).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert!(err.to_string().contains("larger than"), "{err}");
    }

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

    #[test]
    fn parses_post_urls() {
        let u = parse_url("http://127.0.0.1:8080/cb?x=1").unwrap();
        assert_eq!(
            (u.tls, u.host.as_str(), u.port, u.path.as_str()),
            (false, "127.0.0.1", 8080, "/cb?x=1")
        );
        let u = parse_url("HTTP://example.org?q=1#frag").unwrap();
        assert_eq!((u.port, u.path.as_str()), (80, "/?q=1"));
        let u = parse_url("http://[::1]:9/x").unwrap();
        assert_eq!((u.host.as_str(), u.port), ("::1", 9));
        assert_eq!(parse_url("http://[::1]").unwrap().port, 80);
        for bad in [
            "ftp://x/y",
            "http://",
            "http://:80/x",
            "http://h:notaport/",
            "http://user:pw@h/",
            "http://h/a b",
            "http://h/a\r\nX: y",
            "/relative",
        ] {
            assert!(parse_url(bad).is_err(), "{bad:?}");
        }
        let https = parse_url("https://secure/x");
        if cfg!(feature = "tls") {
            assert_eq!(https.unwrap().port, 443);
        } else {
            assert_eq!(https.unwrap_err().kind(), io::ErrorKind::Unsupported);
        }
    }

    /// `post` sends the body with its content type and returns any status.
    #[test]
    fn post_sends_the_body_and_returns_the_status() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook?k=v", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            // Read until the whole body has arrived.
            while !String::from_utf8_lossy(&req).ends_with(r#"{"a":"bcdefgh"}"#) {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            s.write_all(b"HTTP/1.1 503 Busy\r\nContent-Length: 2\r\n\r\nno")
                .unwrap();
            String::from_utf8(req).unwrap()
        });
        let reply = post(&url, "application/json", br#"{"a":"bcdefgh"}"#).unwrap();
        assert_eq!(reply.status, 503);
        assert_eq!(reply.body, "no");
        assert!(!reply.is_success());
        let req = server.join().unwrap();
        assert!(req.starts_with("POST /hook?k=v HTTP/1.1\r\n"), "{req}");
        assert!(
            req.contains("\r\nContent-Type: application/json\r\n"),
            "{req}"
        );
        assert!(req.contains("\r\nContent-Length: 15\r\n"), "{req}");
        assert!(req.contains("\r\nHost: 127.0.0.1:"), "{req}");
    }

    /// A self-signed EC end-entity cert (CN=localhost, SAN DNS:localhost + IP
    /// 127.0.0.1) and its key — the same pair as the `server.rs` TLS tests.
    #[cfg(feature = "tls")]
    const CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBvTCCAWOgAwIBAgIUKHclqAxNOvVOQ3kWyfFT+rCJUHwwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDcwMjIwMjkyMFoXDTM2MDYyOTIw
MjkyMFowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0D
AQcDQgAEYHEfrA2tZnM3sdpdua9aH0oSv8G2EmRrc10JlY7xgVO8+6VoyIJqtvH5
AxyPvu6UfMZMa+/UFLAmk+WCYEKCi6OBkjCBjzAdBgNVHQ4EFgQUJOZ8AYKC5S5i
QJhbdtMpHgmjlaowHwYDVR0jBBgwFoAUJOZ8AYKC5S5iQJhbdtMpHgmjlaowGgYD
VR0RBBMwEYIJbG9jYWxob3N0hwR/AAABMAwGA1UdEwEB/wQCMAAwDgYDVR0PAQH/
BAQDAgeAMBMGA1UdJQQMMAoGCCsGAQUFBwMBMAoGCCqGSM49BAMCA0gAMEUCIQCo
ndoh3KosHMemvMMbjxtaNbbXmxGjfsgme3rBkLuziQIgFeUx/cU+Fpw/Dl5McuKP
cCWfronEM/ASghi4/9ldMUI=
-----END CERTIFICATE-----
";

    #[cfg(feature = "tls")]
    const KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgUBOdoa9/azwl851s
NPlNYkOXVnBaUKf3fk8J38AOaYehRANCAARgcR+sDa1mczex2l25r1ofShK/wbYS
ZGtzXQmVjvGBU7z7pWjIgmq28fkDHI++7pR8xkxr79QUsCaT5YJgQoKL
-----END PRIVATE KEY-----
";

    /// An unrelated self-signed certificate (CN=other-ca): a valid root that
    /// did not issue [`CERT_PEM`].
    #[cfg(feature = "tls")]
    const OTHER_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBeTCCAR6gAwIBAgIUfJWIBdjY/8dCLv+oYH0m7P9hIrgwCgYIKoZIzj0EAwIw
EzERMA8GA1UEAwwIb3RoZXItY2EwHhcNMjYxMDMxMTIzMzUzWhcNMzYxMDI4MTIz
MzUzWjATMREwDwYDVQQDDAhvdGhlci1jYTBZMBMGByqGSM49AgEGCCqGSM49AwEH
A0IABPen7Zo1/Ggc3JUCj+tkkD76osA2UJMESsotlGWGjvixxKzjTBDxg6CzRLUK
zYHGM1cVp9hqTQBvkpvUlkHqqPCjUDBOMB0GA1UdDgQWBBSrD+I4q0sHlyaKbseJ
McxIgQ8FfTAfBgNVHSMEGDAWgBSrD+I4q0sHlyaKbseJMcxIgQ8FfTAMBgNVHRMB
Af8EAjAAMAoGCCqGSM49BAMCA0kAMEYCIQCEK8zaCbrSKDrvkMGiurz/ncmDQOUC
mIjSMKbBFgquUgIhAPHupNklmfNjoKbt7eKBPIKOTzsJbKSpBaMVuL/X4bNv
-----END CERTIFICATE-----
";

    /// `post_with` speaks https (feature `tls`), trusting the given CA file
    /// and refusing a certificate it cannot verify.
    #[cfg(feature = "tls")]
    #[test]
    fn post_over_https_verifies_the_server() {
        let dir = std::env::temp_dir().join(format!("gstore_https_post_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&cert, CERT_PEM).unwrap();
        std::fs::write(&key, KEY_PEM).unwrap();
        let config = crate::server::load_tls_config(&cert, &key).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            // Two connections: the trusted exchange, then the refused one.
            for _ in 0..2 {
                let (sock, _) = listener.accept().unwrap();
                sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let conn = rustls::ServerConnection::new(std::sync::Arc::clone(&config)).unwrap();
                let mut tls = rustls::StreamOwned::new(conn, sock);
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                while !String::from_utf8_lossy(&req).ends_with("{\"x\":1}") {
                    match tls.read(&mut buf) {
                        Ok(n) if n > 0 => req.extend_from_slice(&buf[..n]),
                        _ => break,
                    }
                }
                if !req.is_empty() {
                    let _ = tls.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 4\r\n\r\ndone");
                    tls.conn.send_close_notify();
                    let _ = tls.flush();
                }
                seen.push(String::from_utf8_lossy(&req).into_owned());
            }
            seen
        });
        let url = format!("https://localhost:{port}/secure");
        let opts = PostOptions {
            ca_file: Some(cert.clone()),
            ..PostOptions::default()
        };
        let reply = post_with(&url, "application/json", br#"{"x":1}"#, &opts).unwrap();
        assert_eq!((reply.status, reply.body.as_str()), (201, "done"));
        // An empty CA bundle: nothing to verify against.
        let empty = dir.join("empty.pem");
        std::fs::write(&empty, "").unwrap();
        let opts = PostOptions {
            ca_file: Some(empty),
            ..PostOptions::default()
        };
        assert!(post_with(&url, "application/json", b"{}", &opts).is_err());
        // A bundle without the server's issuer: the handshake fails.
        let other = dir.join("other.pem");
        std::fs::write(&other, OTHER_CERT_PEM).unwrap();
        let opts = PostOptions {
            ca_file: Some(other),
            ..PostOptions::default()
        };
        assert!(post_with(&url, "application/json", br#"{"x":1}"#, &opts).is_err());
        let seen = server.join().unwrap();
        assert!(seen[0].starts_with("POST /secure HTTP/1.1\r\n"), "{seen:?}");
        assert!(
            seen[1].is_empty(),
            "nothing is sent to an unverified server"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_local_addresses() {
        for local in [
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "169.254.169.254",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(is_local_addr(&local.parse().unwrap()), "{local}");
        }
        for remote in ["10.0.0.1", "192.168.1.2", "8.8.8.8", "2001:db8::1", "fd00::1"] {
            assert!(!is_local_addr(&remote.parse().unwrap()), "{remote}");
        }
    }

    /// `refuse_local` refuses a loopback target without connecting to it.
    #[test]
    fn refuse_local_refuses_loopback_targets() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let opts = PostOptions {
            refuse_local: true,
            ..PostOptions::default()
        };
        for host in ["127.0.0.1", "localhost"] {
            let err = post_with(&format!("http://{host}:{port}/x"), "text/plain", b"x", &opts)
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        }
        assert!(listener.accept().is_err(), "nothing connected");
    }

    /// A refused connection is an error, not a hang.
    #[test]
    fn post_to_a_closed_port_fails_fast() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let started = Instant::now();
        let opts = PostOptions {
            limits: Limits {
                total: Duration::from_secs(2),
                ..Limits::default()
            },
            ..PostOptions::default()
        };
        let url = format!("http://127.0.0.1:{port}/");
        assert!(post_with(&url, "text/plain", b"x", &opts).is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
