//! The policy of the upstream `/api` layer's file routes (`/file/download`)
//! and of the files they serve (review gapi#4, robust#7, sec#11): a
//! download is streamed (bounded memory), and only so many run at once.
//!
//! Memory is measured with a counting global allocator, so the tests of this
//! file run one at a time ([`serial`]).

use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::Json;

// ---------------------------------------------------------------------------
// A counting allocator: the bytes allocated now, and the peak since a reset
// ---------------------------------------------------------------------------

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(n: usize) {
    let now = CURRENT.fetch_add(n, Ordering::Relaxed) + n;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

fn shrank(n: usize) {
    CURRENT.fetch_sub(n, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        shrank(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size > layout.size() {
                grew(new_size - layout.size());
            } else {
                shrank(layout.size() - new_size);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Run `f`, returning its value and the most heap memory it took above what
/// was allocated when it started (the whole process: the server's threads
/// included).
fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let out = f();
    (out, PEAK.load(Ordering::SeqCst).saturating_sub(base))
}

/// One test at a time (the allocator's counts are process-wide).
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A fresh, canonical temporary directory, removed on drop.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> TempRoot {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("gstore_dlpol_{tag}_{}_{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TempRoot(fs::canonicalize(&d).unwrap())
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    root: PathBuf,
    serving: Option<thread::JoinHandle<()>>,
}

impl Srv {
    fn export_dir(&self) -> PathBuf {
        self.root.join("export")
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(serving) = self.serving.take() {
            let _ = serving.join();
        }
    }
}

/// A server on `root` (root's password `root`), `cfg` applied to the
/// default configuration.
fn start_at(root: &Path, cfg: impl FnOnce(&mut GStoreConfig)) -> Srv {
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    cfg(&mut config);
    let server = Arc::new(
        ApiServer::bind(root, "root", "127.0.0.1:0")
            .unwrap()
            .with_config(config),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || {
        s2.serve_forever();
    });
    Srv {
        server,
        addr,
        root: root.to_path_buf(),
        serving: Some(serving),
    }
}

/// `key=value` set as `conf.ini` would.
fn set(c: &mut GStoreConfig, key: &str, value: &str) {
    c.set(key, value)
        .unwrap_or_else(|e| panic!("{key}={value}: {e}"));
}

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
    }
}

fn request_bytes(method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let mut bytes = raw.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn connect(addr: &SocketAddr) -> TcpStream {
    let s = TcpStream::connect(addr).unwrap();
    // A hang guard only.
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    s
}

/// Read a response head (through the empty line); returns it and the body
/// bytes read past it.
fn read_head(s: &mut TcpStream) -> (String, Vec<u8>) {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(i) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&got[..i]).into_owned();
            return (head, got[i + 4..].to_vec());
        }
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "the connection closed inside the response head");
        got.extend_from_slice(&buf[..n]);
    }
}

fn status_of(head: &str) -> u16 {
    head.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status")
}

fn send(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let mut s = connect(addr);
    s.write_all(&request_bytes(method, target, headers, body))
        .unwrap();
    let (head, mut body) = read_head(&mut s);
    s.read_to_end(&mut body).unwrap();
    Reply {
        status: status_of(&head),
        head,
        body,
    }
}

fn password(user: &str) -> &'static str {
    if user == "root" {
        "root"
    } else {
        "pw"
    }
}

/// `POST /file/download` as `user`.
fn download_as(addr: &SocketAddr, user: &str, target: &str) -> Reply {
    send(
        addr,
        "POST",
        "/file/download",
        &[("Content-Type", "application/json")],
        Json::obj()
            .with("username", user)
            .with("password", password(user))
            .with("filepath", target)
            .to_string()
            .as_bytes(),
    )
}

const DATA_B: &str = "<http://ex/x> <http://ex/p> <http://ex/y> .\n";

// ---------------------------------------------------------------------------
// Streaming, the download cap
// ---------------------------------------------------------------------------

/// Write `size` bytes (a repeating pattern) to `path` without holding them.
fn big_file(path: &Path, size: usize) {
    let mut f = io::BufWriter::new(File::create(path).unwrap());
    let chunk: Vec<u8> = (0..64 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
    let mut left = size;
    while left > 0 {
        let n = left.min(chunk.len());
        f.write_all(&chunk[..n]).unwrap();
        left -= n;
    }
    f.flush().unwrap();
}

/// Download `target` as root, draining the body into a sink: (status, the
/// response head, body bytes).
fn download_drained(addr: &SocketAddr, target: &str, extra: &[(&str, &str)]) -> (u16, String, u64) {
    let body = Json::obj()
        .with("username", "root")
        .with("password", "root")
        .with("filepath", target)
        .to_string();
    let mut headers = vec![("Content-Type", "application/json")];
    headers.extend_from_slice(extra);
    let mut s = connect(addr);
    s.write_all(&request_bytes(
        "POST",
        "/file/download",
        &headers,
        body.as_bytes(),
    ))
    .unwrap();
    let (head, early) = read_head(&mut s);
    let rest = io::copy(&mut s, &mut io::sink()).unwrap();
    (status_of(&head), head, early.len() as u64 + rest)
}

const BIG: usize = 64 << 20;
/// Far below the 2x-the-file a buffered download takes (review gapi#4).
const STREAM_BOUND: usize = 8 << 20;

/// gapi#4 / robust#7 / sec#11: a large download is streamed — its peak heap
/// is a small constant, not twice the file.
#[test]
fn a_large_download_is_streamed_in_bounded_memory() {
    let _g = serial();
    let root = TempRoot::new("stream");
    let srv = start_at(&root.0, |_| {});
    fs::create_dir_all(srv.export_dir()).unwrap();
    big_file(&srv.export_dir().join("big.nt"), BIG);
    fs::write(srv.export_dir().join("small.nt"), DATA_B).unwrap();
    // Warm up (thread-locals, logs, the user table) on a small file.
    assert_eq!(download_drained(&srv.addr, "small.nt", &[]).0, 200);
    let ((status, head, n), peak) = peak_during(|| download_drained(&srv.addr, "big.nt", &[]));
    eprintln!("download of {BIG} bytes: peak heap {peak} bytes");
    assert_eq!(status, 200, "{head}");
    assert_eq!(n, BIG as u64);
    assert!(
        peak < STREAM_BOUND,
        "a {BIG}-byte download took {peak} bytes of heap at its peak"
    );
    // A range is streamed the same way, exactly.
    let ((status, head, n), peak) =
        peak_during(|| download_drained(&srv.addr, "big.nt", &[("Range", "bytes=1000-40000999")]));
    assert_eq!((status, n), (206, 40_000_000), "{head}");
    assert!(
        head.contains("Content-Range: bytes 1000-40000999/"),
        "{head}"
    );
    assert!(peak < STREAM_BOUND, "a ranged download took {peak} bytes");
}

/// The `gzip` feature does not buffer (or compress) a streamed download.
#[cfg(feature = "gzip")]
#[test]
fn a_gzip_accepting_download_is_streamed_too() {
    let _g = serial();
    let root = TempRoot::new("gzip");
    let srv = start_at(&root.0, |_| {});
    fs::create_dir_all(srv.export_dir()).unwrap();
    big_file(&srv.export_dir().join("big.nt"), BIG);
    fs::write(srv.export_dir().join("small.nt"), DATA_B).unwrap();
    let gz = [("Accept-Encoding", "gzip, deflate")];
    assert_eq!(download_drained(&srv.addr, "small.nt", &gz).0, 200);
    let ((status, head, n), peak) = peak_during(|| download_drained(&srv.addr, "big.nt", &gz));
    assert_eq!((status, n), (200, BIG as u64), "{head}");
    assert!(
        !head.to_ascii_lowercase().contains("content-encoding"),
        "{head}"
    );
    assert!(
        peak < STREAM_BOUND,
        "a gzip-accepting download took {peak} bytes"
    );
    // Envelopes are still compressed.
    let body = Json::obj().with("operation", "check").to_string();
    let r = send(
        &srv.addr,
        "POST",
        "/api",
        &[
            ("Content-Type", "application/json"),
            ("Accept-Encoding", "gzip"),
        ],
        body.as_bytes(),
    );
    assert_eq!(
        r.header("Content-Encoding").as_deref(),
        Some("gzip"),
        "{}",
        r.head
    );
}

/// gapi#4: with `download_max_concurrent=1`, a second download while one is
/// in flight is answered 503; the slot is free again once the first is done.
#[test]
fn concurrent_downloads_are_capped() {
    let _g = serial();
    let root = TempRoot::new("cap");
    let srv = start_at(&root.0, |c| set(c, "download_max_concurrent", "1"));
    let a = &srv.addr;
    fs::create_dir_all(srv.export_dir()).unwrap();
    big_file(&srv.export_dir().join("big.nt"), BIG);
    fs::write(srv.export_dir().join("small.nt"), DATA_B).unwrap();
    // The first download: its head has arrived, so it holds the slot; it
    // cannot finish while nobody reads (the file is far larger than the
    // socket buffers).
    let body = Json::obj()
        .with("username", "root")
        .with("password", "root")
        .with("filepath", "big.nt")
        .to_string();
    let mut first = connect(a);
    first
        .write_all(&request_bytes(
            "POST",
            "/file/download",
            &[("Content-Type", "application/json")],
            body.as_bytes(),
        ))
        .unwrap();
    let (head, early) = read_head(&mut first);
    assert_eq!(status_of(&head), 200, "{head}");
    // A second one is turned away.
    let r = download_as(a, "root", "small.nt");
    assert_eq!(r.status, 503, "{}", r.text());
    assert!(r.header("Retry-After").is_some(), "{}", r.head);
    // Drain the first to the end of the connection (the server closes it
    // after the response, its slot already released).
    let rest = io::copy(&mut first, &mut io::sink()).unwrap();
    assert_eq!(early.len() as u64 + rest, BIG as u64);
    let r = download_as(a, "root", "small.nt");
    assert_eq!((r.status, r.text()), (200, DATA_B.to_string()));
}
