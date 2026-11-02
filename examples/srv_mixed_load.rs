//! Mixed read/write load on the bundled HTTP servers (Phase 6, R3-SRV): `N`
//! closed-loop keep-alive readers run one LUBM query back to back while one
//! writer sends a single-triple `INSERT DATA` every `W` ms, against a
//! snapshot (in-memory) database. It prints the readers' throughput and
//! latency, and the writer's, for three runs per endpoint:
//!
//! * `read-only` — the readers alone (no writer);
//! * `checkpoint-off` — with the writer, `check_point` off (updates stay in
//!   memory until shutdown);
//! * `checkpoint-on` — with the writer, `check_point` on (every update saves
//!   the snapshot before it is acknowledged).
//!
//! The query cache is off, as in `gstore-bench`'s concurrency phase.
//!
//! ```text
//! cargo run --release --example srv_mixed_load -- \
//!     --data /tmp/gstore-bench-cache/lubm-like1-u8-s0.nt \
//!     [--secs 10] [--readers 8] [--write-ms 100] [--query q4] \
//!     [--endpoints sparql,api] [--modes read-only,checkpoint-off,checkpoint-on] \
//!     [--scratch DIR]
//! ```
//!
//! The database is built from `--data` into a scratch directory (removed at
//! the end). Each result is one line:
//! `RESULT endpoint=… mode=… query=… readers=… qps=… mean_ms=… p95_ms=…
//! writes=… write_mean_ms=… errors=…`.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gstore::http_api::ApiServer;
use gstore::server::{Server, ShutdownHandle};
use gstore::{Database, QuerySettings};

const Q1: &str = include_str!("../testdata/lubm/lubm_q1.rq");
const Q4: &str = include_str!("../testdata/lubm/lubm_q4.rq");
const API_PASSWORD: &str = "mixed-load";
const DB_NAME: &str = "mixed";

struct Opts {
    data: PathBuf,
    secs: f64,
    readers: usize,
    write_ms: u64,
    query: String,
    endpoints: Vec<String>,
    modes: Vec<String>,
    scratch: PathBuf,
}

fn usage() -> ! {
    eprintln!(
        "usage: srv_mixed_load --data FILE.nt [--secs S] [--readers N] [--write-ms MS] \
         [--query q1|q4] [--endpoints sparql,api] \
         [--modes read-only,checkpoint-off,checkpoint-on] [--scratch DIR]"
    );
    std::process::exit(2)
}

fn parse_opts() -> Opts {
    let mut o = Opts {
        data: PathBuf::new(),
        secs: 10.0,
        readers: 8,
        write_ms: 100,
        query: "q4".into(),
        endpoints: vec!["sparql".into(), "api".into()],
        modes: vec![
            "read-only".into(),
            "checkpoint-off".into(),
            "checkpoint-on".into(),
        ],
        scratch: std::env::temp_dir().join(format!("gstore_mixed_load_{}", std::process::id())),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let list = |s: &str| s.split(',').map(str::to_string).collect::<Vec<_>>();
    while i < args.len() {
        let value = || args.get(i + 1).cloned().unwrap_or_else(|| usage());
        match args[i].as_str() {
            "--data" => o.data = value().into(),
            "--secs" => o.secs = value().parse().unwrap_or_else(|_| usage()),
            "--readers" => o.readers = value().parse().unwrap_or_else(|_| usage()),
            "--write-ms" => o.write_ms = value().parse().unwrap_or_else(|_| usage()),
            "--query" => o.query = value(),
            "--endpoints" => o.endpoints = list(&value()),
            "--modes" => o.modes = list(&value()),
            "--scratch" => o.scratch = value().into(),
            "-h" | "--help" => usage(),
            _ => usage(),
        }
        i += 2;
    }
    if o.data.as_os_str().is_empty() || o.readers == 0 || o.secs <= 0.0 {
        usage();
    }
    o
}

/// Percent-encode everything but RFC 3986's unreserved characters.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Read one response (status, body, whether the server closes) from a
/// keep-alive connection.
fn read_response(r: &mut BufReader<TcpStream>) -> io::Result<(u16, Vec<u8>, bool)> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad status line"))?;
    let (mut len, mut close) = (None, false);
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
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse::<usize>().ok(),
                "connection" => close |= v.to_ascii_lowercase().contains("close"),
                _ => {}
            }
        }
    }
    let mut body = Vec::new();
    match len {
        Some(n) => {
            body.resize(n, 0);
            r.read_exact(&mut body)?;
        }
        None => {
            r.read_to_end(&mut body)?;
            close = true;
        }
    }
    Ok((status, body, close))
}

/// One request on a fresh connection.
fn once(addr: SocketAddr, raw: &str) -> io::Result<(u16, Vec<u8>)> {
    let s = TcpStream::connect(addr)?;
    s.set_read_timeout(Some(Duration::from_secs(600)))?;
    let mut r = BufReader::new(s);
    r.get_mut().write_all(raw.as_bytes())?;
    read_response(&mut r).map(|(s, b, _)| (s, b))
}

fn get(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\n\r\n")
}

fn post(path: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[derive(Default)]
struct Tally {
    ok: u64,
    errors: u64,
    lat_ms: Vec<f64>,
}

impl Tally {
    fn mean(&self) -> f64 {
        if self.lat_ms.is_empty() {
            0.0
        } else {
            self.lat_ms.iter().sum::<f64>() / self.lat_ms.len() as f64
        }
    }

    fn p95(&mut self) -> f64 {
        if self.lat_ms.is_empty() {
            return 0.0;
        }
        self.lat_ms.sort_by(f64::total_cmp);
        let i = ((self.lat_ms.len() - 1) as f64 * 0.95).round() as usize;
        self.lat_ms[i]
    }

    fn merge(&mut self, other: Tally) {
        self.ok += other.ok;
        self.errors += other.errors;
        self.lat_ms.extend(other.lat_ms);
    }
}

/// A closed-loop keep-alive reader sending `req` until `stop`.
fn reader(addr: SocketAddr, req: String, stop: Arc<AtomicBool>) -> Tally {
    let mut t = Tally::default();
    let mut conn: Option<BufReader<TcpStream>> = None;
    while !stop.load(Ordering::Relaxed) {
        if conn.is_none() {
            let Ok(s) = TcpStream::connect(addr) else {
                t.errors += 1;
                continue;
            };
            s.set_nodelay(true).ok();
            s.set_read_timeout(Some(Duration::from_secs(600))).ok();
            conn = Some(BufReader::new(s));
        }
        let Some(c) = conn.as_mut() else { continue };
        let started = Instant::now();
        if c.get_mut().write_all(req.as_bytes()).is_err() {
            conn = None;
            continue;
        }
        match read_response(c) {
            Ok((status, _, close)) => {
                if status == 200 {
                    t.ok += 1;
                    t.lat_ms.push(started.elapsed().as_secs_f64() * 1e3);
                } else {
                    t.errors += 1;
                }
                if close {
                    conn = None;
                }
            }
            // Closed by the server (keep-alive limit): reconnect.
            Err(_) => conn = None,
        }
    }
    t
}

/// The writer: one single-triple `INSERT DATA` every `every`, until `stop`.
fn writer(
    addr: SocketAddr,
    endpoint: &str,
    every: Duration,
    stop: Arc<AtomicBool>,
    first: u64,
) -> Tally {
    let mut t = Tally::default();
    let mut i = first;
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        next += every;
        let update = format!(
            "INSERT DATA {{ <http://www.gstore-bench.org/mixed/{i}> \
             <http://www.lehigh.edu/~zhp2/2004/0401/univ-bench.owl#name> \"mixed{i}\" . }}"
        );
        i += 1;
        let raw = match endpoint {
            "sparql" => post("/update", &update),
            _ => post(
                "/api",
                &format!(
                    "{{\"username\":\"root\",\"password\":\"{API_PASSWORD}\",\"operation\":\"query\",\
                     \"db_name\":\"{DB_NAME}\",\"sparql\":{}}}",
                    json_string(&update)
                ),
            ),
        };
        let started = Instant::now();
        match once(addr, &raw) {
            Ok((200, body)) if acknowledged_one(&body) => {
                t.ok += 1;
                t.lat_ms.push(started.elapsed().as_secs_f64() * 1e3);
            }
            _ => t.errors += 1,
        }
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }
    t
}

/// Whether an update answer says one triple changed (`/update`'s
/// `"changed":1`, the `/api` `query`'s `"AnsNum":1`).
fn acknowledged_one(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body);
    text.contains("\"changed\":1") || text.contains("\"AnsNum\":1")
}

/// `s` as a JSON string literal.
fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A running server: its address, how to stop it, and its thread.
struct Running {
    addr: SocketAddr,
    stop: ShutdownHandle,
    thread: std::thread::JoinHandle<()>,
}

impl Running {
    fn finish(self) {
        self.stop.shutdown();
        let _ = self.thread.join();
    }
}

fn start(endpoint: &str, root: &Path, check_point: bool) -> Result<Running, String> {
    let dir = root.join(format!("{DB_NAME}.db"));
    match endpoint {
        "sparql" => {
            let mut db = Database::open(&dir).map_err(|e| format!("open: {e}"))?;
            db.set_query_cache_size(0);
            let server = Server::bind(db, "127.0.0.1:0")
                .map_err(|e| format!("bind: {e}"))?
                .with_db_dir(&dir)
                .with_check_point(check_point);
            let addr = server.local_addr().map_err(|e| e.to_string())?;
            let stop = server.shutdown_handle();
            let thread = std::thread::spawn(move || {
                server.serve_forever();
            });
            Ok(Running { addr, stop, thread })
        }
        "api" => {
            let settings = QuerySettings {
                query_cache_bytes: 0,
                ..QuerySettings::default()
            };
            let server = ApiServer::bind(root, API_PASSWORD, "127.0.0.1:0")
                .map_err(|e| format!("bind: {e}"))?
                .with_check_point(check_point)
                .with_query_settings(settings);
            let addr = server.local_addr().map_err(|e| e.to_string())?;
            let stop = server.shutdown_handle();
            let thread = std::thread::spawn(move || {
                server.serve_forever();
            });
            let load = format!(
                "/api?operation=load&username=root&password={API_PASSWORD}&db_name={DB_NAME}"
            );
            let (status, body) = once(addr, &get(&load)).map_err(|e| format!("load: {e}"))?;
            let text = String::from_utf8_lossy(&body);
            if status != 200 || !text.contains("\"StatusCode\":0") {
                return Err(format!("load: HTTP {status}: {text}"));
            }
            Ok(Running { addr, stop, thread })
        }
        other => Err(format!("unknown endpoint '{other}' (sparql, api)")),
    }
}

fn run(o: &Opts, endpoint: &str, mode: &str, first_write: u64) -> Result<u64, String> {
    let (writes, check_point) = match mode {
        "read-only" => (false, false),
        "checkpoint-off" => (true, false),
        "checkpoint-on" => (true, true),
        other => return Err(format!("unknown mode '{other}'")),
    };
    let text = match o.query.as_str() {
        "q1" => Q1,
        "q4" => Q4,
        other => return Err(format!("unknown query '{other}' (q1, q4)")),
    };
    let server = start(endpoint, &o.scratch, check_point)?;
    let path = match endpoint {
        "sparql" => format!("/sparql?query={}", urlencode(text)),
        _ => format!(
            "/api?operation=query&username=root&password={API_PASSWORD}&db_name={DB_NAME}\
             &format=json&sparql={}",
            urlencode(text)
        ),
    };
    let req = get(&path);
    // One warm-up answer, which also checks the query works.
    let (status, body) = once(server.addr, &req).map_err(|e| format!("warm-up: {e}"))?;
    if status != 200 {
        return Err(format!(
            "warm-up: HTTP {status}: {}",
            String::from_utf8_lossy(&body[..body.len().min(300)])
        ));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..o.readers)
        .map(|_| {
            let (req, stop, addr) = (req.clone(), Arc::clone(&stop), server.addr);
            std::thread::spawn(move || reader(addr, req, stop))
        })
        .collect();
    let write = writes.then(|| {
        let (stop, addr, ep) = (Arc::clone(&stop), server.addr, endpoint.to_string());
        let every = Duration::from_millis(o.write_ms.max(1));
        std::thread::spawn(move || writer(addr, &ep, every, stop, first_write))
    });
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs_f64(o.secs));
    stop.store(true, Ordering::Relaxed);
    let mut t = Tally::default();
    for r in readers {
        t.merge(r.join().map_err(|_| "a reader panicked")?);
    }
    let elapsed = started.elapsed().as_secs_f64();
    let mut w = match write {
        Some(h) => h.join().map_err(|_| "the writer panicked")?,
        None => Tally::default(),
    };
    server.finish();
    let (read_p95, write_p95) = (t.p95(), w.p95());
    println!(
        "RESULT endpoint={endpoint} mode={mode} query={} readers={} secs={elapsed:.1} \
         qps={:.1} mean_ms={:.3} p95_ms={:.3} read_errors={} writes={} write_mean_ms={:.2} \
         write_p95_ms={:.2} write_errors={}",
        o.query,
        o.readers,
        t.ok as f64 / elapsed.max(1e-9),
        t.mean(),
        read_p95,
        t.errors,
        w.ok,
        w.mean(),
        write_p95,
        w.errors,
    );
    Ok(first_write + w.ok + w.errors)
}

fn main() {
    let o = parse_opts();
    let _ = std::fs::remove_dir_all(&o.scratch);
    if let Err(e) = std::fs::create_dir_all(&o.scratch) {
        eprintln!("scratch {}: {e}", o.scratch.display());
        std::process::exit(1);
    }
    let dir = o.scratch.join(format!("{DB_NAME}.db"));
    let t0 = Instant::now();
    let built = Database::build_from_files(DB_NAME, &[&o.data]).and_then(|db| db.save(&dir));
    if let Err(e) = built {
        eprintln!("build from {}: {e}", o.data.display());
        let _ = std::fs::remove_dir_all(&o.scratch);
        std::process::exit(1);
    }
    eprintln!(
        "built {} in {:.1} s",
        dir.display(),
        t0.elapsed().as_secs_f64()
    );
    let mut next_write = 0u64;
    let mut failed = false;
    for endpoint in &o.endpoints {
        for mode in &o.modes {
            match run(&o, endpoint, mode, next_write) {
                Ok(n) => next_write = n,
                Err(e) => {
                    eprintln!("{endpoint} {mode}: {e}");
                    failed = true;
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&o.scratch);
    if failed {
        std::process::exit(1);
    }
}
