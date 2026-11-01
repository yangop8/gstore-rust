//! The concurrency phase: read throughput of the bundled HTTP servers with
//! closed-loop keep-alive clients.
//!
//! Two servers are measured in turn, in-process, on an ephemeral port:
//! * `sparql` — the single-database [`Server`] (`GET /sparql`, TSV results);
//! * `api` — the multi-database [`ApiServer`] (`GET /api?operation=query`,
//!   JSON results, after `operation=load`).
//!
//! For each query and client count, `clients` threads send the same request
//! back to back on keep-alive connections for `secs` seconds. Every response
//! body is hashed (`/api` bodies without their `QueryTime` and `ThreadId`
//! fields, which change per request); all bodies of one endpoint and query
//! must be identical. The query cache is off on both servers.

use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gstore::http_api::ApiServer;
use gstore::server::Server;
use gstore::{Database, QuerySettings};
use serde::{Deserialize, Serialize};

use crate::hash::{fnv64, hex};
use crate::runner::phases::percentile;
use crate::schema::{ConcPoint, ConcResult};
use crate::sys;
use crate::workload::{QuerySpec, Workload};

/// What the concurrency phase drives.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConcSettings {
    /// Client counts, one measurement each.
    pub clients: Vec<usize>,
    /// Seconds per measurement.
    pub secs: f64,
    /// Query names (empty = the workload's default concurrency queries).
    #[serde(default)]
    pub queries: Vec<String>,
    /// `sparql`, `api`.
    pub endpoints: Vec<String>,
}

const API_USER: &str = "root";
const API_PASSWORD: &str = "123456";

/// Percent-encode everything but RFC 3986's unreserved characters.
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One HTTP response: (status, body, server closes the connection).
type Response = (u16, Vec<u8>, bool);

/// Read one response from a keep-alive connection; `None` if the server
/// closed the connection before answering.
pub fn read_response(r: &mut BufReader<TcpStream>) -> io::Result<Option<Response>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("bad status line {line:?}")))?;
    let mut len: Option<usize> = None;
    let mut chunked = false;
    let mut close = false;
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            match k.as_str() {
                "content-length" => len = v.parse().ok(),
                "transfer-encoding" => chunked |= v.to_ascii_lowercase().contains("chunked"),
                "connection" => close |= v.to_ascii_lowercase().contains("close"),
                _ => {}
            }
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            line.clear();
            r.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if size == 0 {
                // Trailers until the empty line.
                loop {
                    line.clear();
                    if r.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
                        break;
                    }
                }
                break;
            }
            let start = body.len();
            body.resize(start + size, 0);
            r.read_exact(&mut body[start..])?;
            line.clear();
            r.read_line(&mut line)?;
        }
    } else if let Some(n) = len {
        body.resize(n, 0);
        r.read_exact(&mut body)?;
    } else {
        r.read_to_end(&mut body)?;
        close = true;
    }
    Ok(Some((status, body, close)))
}

fn request(path: &str, accept: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\nAccept: {accept}\r\n\r\n"
    )
}

/// One request on a fresh connection.
pub fn get_once(addr: SocketAddr, path: &str, accept: &str) -> io::Result<(u16, Vec<u8>)> {
    let s = TcpStream::connect(addr)?;
    s.set_read_timeout(Some(Duration::from_secs(600)))?;
    let mut r = BufReader::new(s);
    r.get_mut().write_all(request(path, accept).as_bytes())?;
    match read_response(&mut r)? {
        Some((st, body, _)) => Ok((st, body)),
        None => Err(io::ErrorKind::UnexpectedEof.into()),
    }
}

/// The body hash an endpoint's response is compared by.
fn body_hash(endpoint: &str, body: &[u8]) -> u64 {
    if endpoint == "api" {
        if let Ok(serde_json::Value::Object(mut m)) = serde_json::from_slice::<serde_json::Value>(body) {
            m.remove("QueryTime");
            m.remove("ThreadId");
            let canon = serde_json::to_vec(&serde_json::Value::Object(m)).unwrap_or_default();
            return fnv64(&canon);
        }
    }
    fnv64(body)
}

struct Load {
    ok: u64,
    errors: u64,
    reconnects: u64,
    lat_ms: Vec<f64>,
    hashes: BTreeSet<u64>,
}

/// Drive `path` with `clients` closed-loop keep-alive clients for `secs`.
fn drive(addr: SocketAddr, endpoint: &str, path: &str, accept: &str, clients: usize, secs: f64) -> Result<(Load, f64), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let total = Arc::new(Mutex::new(Load {
        ok: 0,
        errors: 0,
        reconnects: 0,
        lat_ms: Vec::new(),
        hashes: BTreeSet::new(),
    }));
    let req = request(path, accept);
    let mut handles = Vec::new();
    for _ in 0..clients {
        let (stop, total, req, endpoint) = (stop.clone(), total.clone(), req.clone(), endpoint.to_string());
        handles.push(std::thread::spawn(move || -> Result<(), String> {
            let mut mine = Load {
                ok: 0,
                errors: 0,
                reconnects: 0,
                lat_ms: Vec::new(),
                hashes: BTreeSet::new(),
            };
            let mut conn: Option<BufReader<TcpStream>> = None;
            let mut failures = 0u32;
            while !stop.load(Ordering::Relaxed) {
                if conn.is_none() {
                    let s = TcpStream::connect(addr).map_err(|e| format!("connect: {e}"))?;
                    s.set_nodelay(true).ok();
                    s.set_read_timeout(Some(Duration::from_secs(600))).ok();
                    conn = Some(BufReader::new(s));
                }
                let c = conn.as_mut().expect("connected");
                let t = Instant::now();
                if c.get_mut().write_all(req.as_bytes()).is_err() {
                    conn = None;
                    mine.reconnects += 1;
                    continue;
                }
                match read_response(c) {
                    Ok(Some((status, body, close))) => {
                        if status == 200 {
                            mine.ok += 1;
                            mine.lat_ms.push(t.elapsed().as_secs_f64() * 1e3);
                            mine.hashes.insert(body_hash(&endpoint, &body));
                        } else {
                            mine.errors += 1;
                        }
                        if close {
                            conn = None;
                        }
                        failures = 0;
                    }
                    Ok(None) | Err(_) => {
                        // Closed by the server (keep-alive limit): retry on a
                        // fresh connection.
                        conn = None;
                        mine.reconnects += 1;
                        failures += 1;
                        if failures > 100 {
                            return Err("the server keeps closing connections".into());
                        }
                    }
                }
            }
            let mut tot = total.lock().unwrap_or_else(|p| p.into_inner());
            tot.ok += mine.ok;
            tot.errors += mine.errors;
            tot.reconnects += mine.reconnects;
            tot.lat_ms.extend(mine.lat_ms);
            tot.hashes.extend(mine.hashes);
            Ok(())
        }));
    }
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(secs));
    stop.store(true, Ordering::Relaxed);
    let mut err = None;
    for h in handles {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => err = Some(e),
            Err(_) => err = Some("a client thread panicked".into()),
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    if let Some(e) = err {
        return Err(e);
    }
    let load = Arc::try_unwrap(total)
        .map_err(|_| "client state still shared".to_string())?
        .into_inner()
        .unwrap_or_else(|p| p.into_inner());
    Ok((load, elapsed))
}

/// Measure every query and client count on one endpoint; returns whether all
/// bodies per query were identical.
fn measure(addr: SocketAddr, endpoint: &str, paths: &[(String, String)], accept: &str, s: &ConcSettings, points: &mut Vec<ConcPoint>) -> Result<bool, String> {
    let mut equal = true;
    for (name, path) in paths {
        // The reference body.
        let (st, body) = get_once(addr, path, accept).map_err(|e| format!("{endpoint} {name}: {e}"))?;
        if st != 200 {
            return Err(format!(
                "{endpoint} {name}: HTTP {st}: {}",
                String::from_utf8_lossy(&body[..body.len().min(500)])
            ));
        }
        let reference = body_hash(endpoint, &body);
        for &c in &s.clients {
            let (load, secs) = drive(addr, endpoint, path, accept, c, s.secs)?;
            let mut distinct = load.hashes.clone();
            distinct.insert(reference);
            equal &= distinct.len() == 1 && load.errors == 0;
            points.push(ConcPoint {
                endpoint: endpoint.into(),
                query: name.clone(),
                clients: c,
                ok: load.ok,
                http_errors: load.errors,
                reconnects: load.reconnects,
                secs,
                qps: load.ok as f64 / secs.max(1e-9),
                mean_ms: if load.lat_ms.is_empty() {
                    0.0
                } else {
                    load.lat_ms.iter().sum::<f64>() / load.lat_ms.len() as f64
                },
                p95_ms: percentile(&load.lat_ms, 95.0),
                distinct_bodies: distinct.len(),
                body_fnv64: hex(reference),
            });
        }
    }
    Ok(equal)
}

/// Run the concurrency phase on the database in `dir`.
pub fn conc_phase(dir: &Path, w: &dyn Workload, s: &ConcSettings) -> Result<ConcResult, String> {
    let names = if s.queries.is_empty() { w.conc_queries() } else { s.queries.clone() };
    let all = w.queries();
    let queries: Vec<QuerySpec> = names
        .iter()
        .map(|n| {
            all.iter()
                .find(|q| &q.name == n)
                .cloned()
                .ok_or_else(|| format!("unknown query '{n}'"))
        })
        .collect::<Result<_, _>>()?;
    let mut points = Vec::new();
    let mut equal = true;
    for endpoint in &s.endpoints {
        match endpoint.as_str() {
            "sparql" => {
                let mut db = Database::open(dir).map_err(|e| format!("open {}: {e}", dir.display()))?;
                db.set_query_cache_size(0);
                let server = Server::bind(db, "127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
                let addr = server.local_addr().map_err(|e| e.to_string())?;
                let handle = server.shutdown_handle();
                let t = std::thread::spawn(move || {
                    server.serve_forever();
                });
                let paths: Vec<(String, String)> = queries
                    .iter()
                    .map(|q| (q.name.clone(), format!("/sparql?query={}", urlencode(&q.text))))
                    .collect();
                let r = measure(addr, "sparql", &paths, "text/tab-separated-values", s, &mut points);
                handle.shutdown();
                let _ = t.join();
                equal &= r?;
            }
            "api" => {
                let root = dir.parent().ok_or("the database has no parent directory")?;
                let name = dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".db"))
                    .ok_or("the database directory is not <name>.db")?
                    .to_string();
                let settings = QuerySettings {
                    query_cache_bytes: 0,
                    ..QuerySettings::default()
                };
                let api = ApiServer::bind(root, API_PASSWORD, "127.0.0.1:0")
                    .map_err(|e| format!("bind: {e}"))?
                    .with_query_settings(settings);
                let addr = api.local_addr().map_err(|e| e.to_string())?;
                let handle = api.shutdown_handle();
                let t = std::thread::spawn(move || {
                    api.serve_forever();
                });
                let cred = format!("username={API_USER}&password={API_PASSWORD}&db_name={}", urlencode(&name));
                let r = (|| {
                    let (st, body) = get_once(addr, &format!("/api?operation=load&{cred}"), "application/json")
                        .map_err(|e| format!("api load: {e}"))?;
                    let text = String::from_utf8_lossy(&body);
                    if st != 200 || !text.contains("\"StatusCode\":0") {
                        return Err(format!("api load failed: HTTP {st}: {text}"));
                    }
                    let paths: Vec<(String, String)> = queries
                        .iter()
                        .map(|q| {
                            (
                                q.name.clone(),
                                format!("/api?operation=query&{cred}&format=json&sparql={}", urlencode(&q.text)),
                            )
                        })
                        .collect();
                    measure(addr, "api", &paths, "application/json", s, &mut points)
                })();
                handle.shutdown();
                let _ = t.join();
                equal &= r?;
            }
            other => return Err(format!("unknown endpoint '{other}' (sparql, api)")),
        }
    }
    Ok(ConcResult {
        points,
        bodies_equal: equal,
        hwm_mib: sys::hwm_mib(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_keeps_unreserved() {
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(urlencode("?x <y>\n"), "%3Fx%20%3Cy%3E%0A");
    }

    #[test]
    fn api_bodies_ignore_timing_fields() {
        let a = br#"{"StatusCode":0,"QueryTime":"3","ThreadId":"1","AnsNum":2}"#;
        let b = br#"{"AnsNum":2,"StatusCode":0,"QueryTime":"9","ThreadId":"7"}"#;
        assert_eq!(body_hash("api", a), body_hash("api", b));
        assert_ne!(body_hash("sparql", a), body_hash("sparql", b));
    }
}
