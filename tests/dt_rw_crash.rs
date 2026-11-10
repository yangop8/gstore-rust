//! Data test (Phase 6, R3-SRV): a real `gserver` killed with `SIGKILL` while
//! readers and writers hammer it — single-database and `--api` mode, on the
//! snapshot and on-disk layouts (and RocksDB with the `rocksdb` feature).
//!
//! Every write the server acknowledged (`200`, one triple changed) must be in
//! the database reopened after the kill, and nothing that was never sent
//! may appear. Each test runs `GSTORE_RW_CRASH_SEEDS` seeds (default 10);
//! the seed picks how long the load runs before the kill and how the
//! writers pace themselves.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use gstore::{Database, QueryResult};

const BASE: &str = "<http://ex/base> <http://ex/p> \"base\" .\n";
const PRED: &str = "http://ex/p";
const WRITERS: u64 = 3;
const READERS: usize = 2;
/// `Basic root:pw`.
const ROOT: &str = "Basic cm9vdDpwdw==";

fn seeds() -> u64 {
    std::env::var("GSTORE_RW_CRASH_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(10)
}

/// splitmix64: a tiny deterministic generator for the seed's choices.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A temp directory removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("gstore_rwcrash_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `gserver` child, killed when dropped (a failing test leaves none).
struct Gserver {
    child: Child,
    addr: SocketAddr,
}

impl Gserver {
    fn spawn(cwd: &Path, args: &[&str]) -> Gserver {
        let mut child = Command::new(env!("CARGO_BIN_EXE_gserver"))
            .args(args)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn gserver");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(rest) = line.split("http://").nth(1) {
                    let addr = rest.split_whitespace().next().unwrap_or("").to_string();
                    let _ = tx.send(addr);
                }
            }
        });
        let addr = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("gserver printed its address")
            .parse()
            .expect("socket address");
        Gserver { child, addr }
    }

    /// `SIGKILL`, and wait for it to be gone.
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Gserver {
    fn drop(&mut self) {
        self.kill();
    }
}

/// One HTTP/1.1 request; `None` when the connection fails (the server was
/// killed).
fn request(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    auth: Option<&str>,
    body: &str,
) -> Option<(u16, String)> {
    let mut s = TcpStream::connect(addr).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(30))).ok()?;
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: x\r\n");
    if let Some(a) = auth {
        raw.push_str(&format!("Authorization: {a}\r\n"));
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s.write_all(raw.as_bytes()).ok()?;
    let mut resp = String::new();
    s.read_to_string(&mut resp).ok()?;
    let status = resp.split_whitespace().nth(1)?.parse().ok()?;
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    Some((status, body.to_string()))
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    Snapshot,
    Disk,
    #[cfg(feature = "rocksdb")]
    Rocks,
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Single,
    Api,
}

/// Create the database at `dir` in `layout`.
fn create(dir: &Path, layout: Layout) {
    match layout {
        Layout::Snapshot => Database::build_from_str("crash", BASE)
            .unwrap()
            .save(dir)
            .unwrap(),
        Layout::Disk => {
            let rdf = dir.with_extension("nt");
            std::fs::write(&rdf, BASE).unwrap();
            Database::build_disk(dir, &[&rdf]).unwrap();
            std::fs::remove_file(&rdf).unwrap();
        }
        #[cfg(feature = "rocksdb")]
        Layout::Rocks => {
            Database::build_rocksdb_from_str("crash", dir, BASE).unwrap();
        }
    }
}

/// The subjects of `PRED` triples in the database at `dir` (reopened after
/// the kill: recovery included).
fn subjects(dir: &Path) -> BTreeSet<String> {
    let mut db = Database::open(dir).unwrap_or_else(|e| panic!("reopen {}: {e}", dir.display()));
    match db
        .query(&format!("SELECT ?s WHERE {{ ?s <{PRED}> ?o }}"))
        .unwrap()
    {
        QueryResult::Select(rs) => rs.rows.iter().filter_map(|r| r[0].clone()).collect(),
        other => panic!("expected Select, got {other:?}"),
    }
}

/// One seed: serve, load, kill, reopen, compare. Returns how many writes
/// were acknowledged.
fn run(mode: Mode, layout: Layout, seed: u64) -> usize {
    let tmp = TmpDir::new(&format!("{mode:?}_{layout:?}_{seed}").to_lowercase());
    let r = mix(seed ^ 0x5EED);
    let load_ms = 120 + r % 380;
    let pause_ms = (r >> 16) % 4;
    let (mut server, dir, update, read) = match mode {
        Mode::Single => {
            let dir = tmp.0.join("crash.db");
            create(&dir, layout);
            let dir_s = dir.to_string_lossy().into_owned();
            let s = Gserver::spawn(&tmp.0, &[&dir_s, "--port", "0"]);
            (s, dir, "/update".to_string(), "/sparql".to_string())
        }
        Mode::Api => {
            let root = tmp.0.join("root");
            std::fs::create_dir_all(&root).unwrap();
            let dir = root.join("crash.db");
            create(&dir, layout);
            let root_s = root.to_string_lossy().into_owned();
            let s = Gserver::spawn(
                &tmp.0,
                &[&root_s, "--api", "--port", "0", "--password", "pw"],
            );
            let (st, b) = request(&s.addr, "POST", "/load?db_name=crash", Some(ROOT), "")
                .expect("the server answers");
            assert_eq!(st, 200, "load: {b}");
            (
                s,
                dir,
                "/update?db_name=crash".to_string(),
                "/sparql?db_name=crash".to_string(),
            )
        }
    };
    let auth = matches!(mode, Mode::Api).then_some(ROOT);
    let addr = server.addr;
    let stop = Arc::new(AtomicBool::new(false));
    let sent = Arc::new(Mutex::new(BTreeSet::new()));
    let acked = Arc::new(Mutex::new(BTreeSet::new()));
    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let (stop, sent, acked, update) = (
                Arc::clone(&stop),
                Arc::clone(&sent),
                Arc::clone(&acked),
                update.clone(),
            );
            thread::spawn(move || {
                for i in 0.. {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let s = format!("<http://ex/w{w}/{i}>");
                    sent.lock().unwrap().insert(s.clone());
                    let u = format!("INSERT DATA {{ {s} <{PRED}> \"{i}\" }}");
                    match request(&addr, "POST", &update, auth, &u) {
                        Some((200, b)) => {
                            assert!(b.contains("\"changed\":1"), "{b}");
                            acked.lock().unwrap().insert(s);
                        }
                        Some((st, b)) => panic!("update answered {st}: {b}"),
                        None => break,
                    }
                    if pause_ms > 0 {
                        thread::sleep(Duration::from_millis(pause_ms));
                    }
                }
            })
        })
        .collect();
    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let (stop, read) = (Arc::clone(&stop), read.clone());
            thread::spawn(move || {
                let q = format!("SELECT (COUNT(*) AS ?n) WHERE {{ ?s <{PRED}> ?o }}");
                while !stop.load(Ordering::SeqCst) {
                    match request(&addr, "POST", &read, auth, &q) {
                        Some((st, b)) => assert_eq!(st, 200, "{b}"),
                        None => break,
                    }
                }
            })
        })
        .collect();
    thread::sleep(Duration::from_millis(load_ms));
    server.kill();
    stop.store(true, Ordering::SeqCst);
    for t in writers.into_iter().chain(readers) {
        t.join().unwrap();
    }
    let acked = acked.lock().unwrap().clone();
    let sent = sent.lock().unwrap().clone();
    let found = subjects(&dir);
    let lost: Vec<_> = acked.difference(&found).collect();
    assert!(
        lost.is_empty(),
        "{mode:?}/{layout:?} seed {seed}: {} acknowledged write(s) lost after SIGKILL at {load_ms} ms: \
         {lost:?}",
        lost.len()
    );
    let base = "<http://ex/base>".to_string();
    let invented: Vec<_> = found
        .iter()
        .filter(|s| **s != base && !sent.contains(*s))
        .collect();
    assert!(invented.is_empty(), "never sent: {invented:?}");
    assert!(found.contains(&base));
    acked.len()
}

fn run_seeds(mode: Mode, layout: Layout) {
    let acked: usize = (0..seeds()).map(|seed| run(mode, layout, seed)).sum();
    eprintln!("{mode:?}/{layout:?}: {acked} acknowledged writes checked");
    assert!(acked > 0, "the load acknowledged no write at all");
}

#[test]
fn single_db_snapshot_keeps_every_acknowledged_write() {
    run_seeds(Mode::Single, Layout::Snapshot);
}

#[test]
fn single_db_disk_keeps_every_acknowledged_write() {
    run_seeds(Mode::Single, Layout::Disk);
}

#[test]
fn api_snapshot_keeps_every_acknowledged_write() {
    run_seeds(Mode::Api, Layout::Snapshot);
}

#[test]
fn api_disk_keeps_every_acknowledged_write() {
    run_seeds(Mode::Api, Layout::Disk);
}

#[cfg(feature = "rocksdb")]
#[test]
fn single_db_rocksdb_keeps_every_acknowledged_write() {
    run_seeds(Mode::Single, Layout::Rocks);
}

#[cfg(feature = "rocksdb")]
#[test]
fn api_rocksdb_keeps_every_acknowledged_write() {
    run_seeds(Mode::Api, Layout::Rocks);
}
