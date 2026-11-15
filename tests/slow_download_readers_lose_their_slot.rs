//! A client that drains a streamed download a few bytes at a time cannot
//! hold its download slot (`download_max_concurrent`) indefinitely (round-1
//! review of gapi#4): the response must be taken at `min_body_rate` on
//! average after a grace of `write_timeout`, or the server closes the
//! connection and frees the slot. Before, only each single write was bounded
//! (by `write_timeout`), so a reader taking a little every few seconds kept
//! the slot for as long as the file lasted, and with the default eight slots
//! eight such readers turned every other download away with `503`.
//!
//! The slow reader is simulated by reading 64 KiB at a fixed pace, about
//! 1 MiB/s: fast enough that no single write of the server waits for its
//! `write_timeout` (5 s here; the kernel wakes a blocked writer once a good
//! part of its send buffer is free), far below the rate floor (32 MiB/s
//! here), and too slow to finish the file within the hang guard. The test
//! polls for the slot to come free (a poll interval, with the hang guard as
//! the only timeout — not the assertion).

use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::Json;
use gstore::server::HttpOptions;

/// The hang guard of the whole wait (never the assertion).
const HANG_GUARD: Duration = Duration::from_secs(60);
/// Far larger than the socket buffers of both ends, and more than the slow
/// reader takes within [`HANG_GUARD`].
const BIG: usize = 160 << 20;

/// A fresh, canonical temporary directory, removed when dropped (also when
/// the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_slow_download_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TmpDir(fs::canonicalize(&d).unwrap())
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
    }
}

/// A server with one download slot, a five-second grace and a rate floor of
/// 32 MiB/s.
fn start_at(root: &Path) -> Srv {
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    config.set("download_max_concurrent", "1").unwrap();
    let opts = HttpOptions {
        write_timeout: Duration::from_secs(5),
        min_body_rate: 32 << 20,
        ..HttpOptions::default()
    };
    let server = Arc::new(
        ApiServer::bind(root, "pw", "127.0.0.1:0")
            .unwrap()
            .with_config(config)
            .with_http_options(opts),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    Srv { server, addr }
}

fn big_file(path: &Path, size: usize) {
    let mut f = BufWriter::new(File::create(path).unwrap());
    let chunk: Vec<u8> = (0..64 * 1024).map(|i| b'a' + (i % 26) as u8).collect();
    let mut left = size;
    while left > 0 {
        let n = left.min(chunk.len());
        f.write_all(&chunk[..n]).unwrap();
        left -= n;
    }
    f.flush().unwrap();
}

/// A `POST /file/download` request for `target` as root.
fn download_request(target: &str) -> Vec<u8> {
    let body = Json::obj()
        .with("username", "root")
        .with("password", "pw")
        .with("filepath", target)
        .to_string();
    format!(
        "POST /file/download HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// The status of a complete download of `target` (body drained).
fn download_status(addr: SocketAddr, target: &str) -> u16 {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    s.write_all(&download_request(target)).unwrap();
    let mut resp = Vec::new();
    let _ = s.read_to_end(&mut resp);
    String::from_utf8_lossy(&resp)
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

#[test]
fn a_slow_download_reader_loses_its_slot() {
    let root = TmpDir::new("slot");
    let export = root.0.join("export");
    fs::create_dir_all(&export).unwrap();
    big_file(&export.join("big.nt"), BIG);
    fs::write(
        export.join("small.nt"),
        "<http://a> <http://b> <http://c> .\n",
    )
    .unwrap();
    let srv = start_at(&root.0);

    // The slow reader: it takes the slot, then reads about 1 MiB/s.
    let mut slow = TcpStream::connect(srv.addr).unwrap();
    slow.set_read_timeout(Some(HANG_GUARD)).unwrap();
    slow.write_all(&download_request("big.nt")).unwrap();
    let mut head = [0u8; 12];
    slow.read_exact(&mut head).unwrap();
    assert_eq!(&head[9..12], b"200", "{}", String::from_utf8_lossy(&head));
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop);
    let reader = thread::spawn(move || {
        let mut buf = vec![0u8; 64 << 10];
        let mut total = 0usize;
        while !stop2.load(Ordering::SeqCst) {
            match slow.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => total += n,
            }
            thread::sleep(Duration::from_millis(60)); // the slow client's pace
        }
        total
    });
    // While it holds the slot, another download is turned away.
    assert_eq!(download_status(srv.addr, "small.nt"), 503);

    // Once it has fallen behind the rate floor, the server closes its
    // connection and the slot is free again.
    let until = Instant::now() + HANG_GUARD;
    let freed = loop {
        match download_status(srv.addr, "small.nt") {
            200 => break true,
            503 if Instant::now() < until => thread::sleep(Duration::from_millis(20)),
            503 => break false,
            other => panic!("unexpected status {other}"),
        }
    };
    stop.store(true, Ordering::SeqCst);
    let read = reader.join().unwrap();
    assert!(
        freed,
        "the slow reader kept the download slot (it read {read} bytes)"
    );
    assert!(read < BIG, "the slow reader was cut off");
}
