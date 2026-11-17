//! The statistics `/api monitor` caches for an unloaded database describe
//! the files they are cached under (round-4 review of gapi#17).
//!
//! `inspect` read the statistics (a snapshot without the directory's lock,
//! the other layouts under the lock the open took), closed the database and
//! only then fingerprinted its files (`DirPrint`) to key the cache. A writer
//! that changed the files in between — another process saving the snapshot
//! it has open, or opening the database the moment its lock was released —
//! left the old statistics cached under the new files' print, answered
//! until the files changed again. A test hook runs a writer in exactly that
//! window; the next `monitor` must answer what the files hold.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::server::ShutdownReport;
use gstore::Database;

const HANG_GUARD: Duration = Duration::from_secs(30);
const ROOT: (&str, &str) = ("root", "root");
const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/b> a <http://ex/K> .\n";
const MORE: &str = "<http://ex/c> <http://ex/p> <http://ex/d> .\n";

/// The hook is process-wide: the tests of this file take turns.
static SERIAL: Mutex<()> = Mutex::new(());

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "gstore_unloaded_stats_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
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

/// Removes the hook when the test ends, also when it fails.
struct HookGuard;

impl Drop for HookGuard {
    fn drop(&mut self) {
        gstore::gapi::set_unloaded_stats_hook(None);
    }
}

fn base64(s: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n = chunk.iter().fold(0u32, |a, &x| (a << 8) | u32::from(x)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// One request on a fresh connection: (status, body).
fn send(addr: SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let auth = base64(&format!("{}:{}", ROOT.0, ROOT.1));
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic {auth}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp)
        .unwrap_or_else(|e| panic!("{method} {target}: no answer within the hang guard: {e}"));
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((&resp, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {resp:?}"));
    (status, body.to_string())
}

struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    serving: Option<thread::JoinHandle<ShutdownReport>>,
    root: TmpDir,
}

impl Srv {
    fn start(tag: &str) -> Srv {
        let root = TmpDir::new(tag);
        let server = ApiServer::bind(&root.0, ROOT.1, "127.0.0.1:0")
            .expect("bind")
            .with_shutdown_grace(Duration::from_millis(100));
        let server = Arc::new(server);
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = Some(thread::spawn(move || s2.serve_forever()));
        Srv {
            server,
            addr,
            serving,
            root,
        }
    }

    fn ok(&self, method: &str, target: &str, body: &str) {
        let (status, body) = send(self.addr, method, target, body);
        assert_eq!(status, 200, "{method} {target}: {body}");
    }

    fn monitor(&self, db: &str) -> Json {
        let obj = Json::obj()
            .with("username", ROOT.0)
            .with("password", ROOT.1)
            .with("operation", "monitor")
            .with("db_name", db);
        let (status, body) = send(self.addr, "POST", "/api", &obj.to_string());
        assert_eq!(status, 200, "{body}");
        let j = json::parse(&body).unwrap();
        assert_eq!(j.get("StatusCode").and_then(Json::as_i64), Some(0), "{j}");
        j
    }

    fn db_dir(&self, db: &str) -> PathBuf {
        self.root.0.join(format!("{db}.db"))
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(h) = self.serving.take() {
            let _ = h.join();
        }
    }
}

fn triples(j: &Json) -> String {
    j.get("tripleNum")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Build `cold` in `layout`, unload it, and let `writer` run once in the
/// window between the first `monitor`'s read and its fingerprint. The
/// following `monitor`s must answer what the files then hold.
fn a_writer_in_the_window(layout: &str, writer: impl Fn(&Path) + Send + Sync + 'static) {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let srv = Srv::start(layout);
    srv.ok(
        "POST",
        &format!("/build?db_name=cold&layout={layout}"),
        DATA,
    );
    srv.ok("POST", "/unload?db_name=cold", "");
    let dir = srv.db_dir("cold");
    let fired = Arc::new(AtomicBool::new(false));
    let _guard = HookGuard;
    {
        let (dir, fired) = (dir.clone(), Arc::clone(&fired));
        gstore::gapi::set_unloaded_stats_hook(Some(Arc::new(move |d: &Path| {
            if d == dir && !fired.swap(true, Ordering::SeqCst) {
                writer(d);
            }
        })));
    }
    let reads = || gstore::gapi::unloaded_stats_reads(&dir);
    let start = reads();
    let first = srv.monitor("cold");
    assert!(fired.load(Ordering::SeqCst), "the hook did not run");
    assert_eq!(triples(&first), "2", "read before the writer: {first}");
    let second = srv.monitor("cold");
    let third = srv.monitor("cold");
    gstore::gapi::set_unloaded_stats_hook(None);
    // The truth, read only now: opening the database may touch its files.
    let truth = Database::open(&dir).unwrap().triple_num().to_string();
    assert_eq!(
        triples(&second),
        truth,
        "monitor answered statistics cached from before the change ({} read(s)): {second}",
        reads() - start
    );
    assert_eq!(triples(&third), truth, "{third}");
}

/// A snapshot is read without the directory's lock: a save of the files
/// in the window is possible, and the statistics read before it must not
/// be cached under the saved files' print.
#[test]
fn a_snapshot_saved_while_its_statistics_are_read_is_read_again() {
    a_writer_in_the_window("snapshot", |dir| {
        let mut db = Database::open(dir).unwrap();
        db.query(&format!("INSERT DATA {{ {MORE} }}")).unwrap();
        db.save(dir).unwrap();
    });
}

/// A disk database is read under its lock; another process that tries to
/// write it in the window (`gadd`) must find it still locked, so the
/// statistics cached describe the files.
#[cfg(unix)]
#[test]
fn another_process_cannot_write_a_disk_database_between_its_read_and_its_print() {
    let gadd: Arc<Mutex<Option<std::process::Output>>> = Arc::default();
    let seen = Arc::clone(&gadd);
    a_writer_in_the_window("disk", move |dir| {
        let root = dir.parent().unwrap();
        let more = root.join("more.nt");
        fs::write(&more, MORE).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_gadd"))
            .args(["cold", more.to_str().unwrap()])
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        *seen.lock().unwrap() = Some(out);
    });
    let out = gadd.lock().unwrap().take().expect("gadd ran");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && err.contains("locked by another process"),
        "gadd wrote the database between its read and its print: {:?}\n{err}",
        out.status
    );
}
