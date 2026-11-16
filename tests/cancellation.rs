//! Index builds on the request path stop when the request is cancelled
//! (review conc#5, analytics#6): a CSR build (the advanced graph functions)
//! and a substring-index build (`CONTAINS`/`STRSTARTS`/… filters) watch the
//! request's cancel flag, so `/cancel` and a shutdown's cancel are observed
//! at the build's next check, nothing is cached, and readers that only ask
//! what is cached never wait for the build.
//!
//! Each build is parked deterministically: a test hook runs at every check
//! of a cancellable build ([`gstore::analytics::csr::build_hook`],
//! [`gstore::kvstore::string_index::build_hook`]); the one installed here
//! signals "parked" at the first check and holds the build there until its
//! flag is raised. The flag is then seen by that very check. Timeouts are
//! hang guards only.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use gstore::analytics::csr::build_hook;
use gstore::db::OpenOptions;
use gstore::http_api::ApiServer;
use gstore::{Database, GStoreError};

/// How long anything here may take before the test is declared hung.
const HANG_GUARD: Duration = Duration::from_secs(60);

/// The hooks are process-wide: the tests of this file run one at a time.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A temp directory removed on drop (also when a test panics).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "gstore_cancellation_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TempDir(std::fs::canonicalize(&d).unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A parking hook: the first check of a build signals `parked` and holds
/// the build until the build's own flag is raised (or the hang guard
/// passes); every later check passes through. Counts the checks. Removed
/// when dropped.
struct Park {
    parked: mpsc::Receiver<()>,
    checks: Arc<AtomicUsize>,
    builder: Arc<Mutex<Option<thread::Thread>>>,
    /// Set by [`release`](Self::release): a held build goes on.
    released: Arc<AtomicBool>,
    /// The parked build's own cancel flag was raised when it went on.
    saw_cancel: Arc<AtomicBool>,
    uninstall: fn(),
}

/// What a parked build waits for.
#[derive(Clone, Copy, PartialEq)]
enum Until {
    /// Its own cancel flag to be raised.
    Cancelled,
    /// The test to [`release`](Park::release) it (a build that ignores a
    /// cancel, as a long read that does not poll would).
    Released,
}

/// Which build a [`Park`] holds.
#[derive(Clone, Copy)]
enum Build {
    Csr,
    StringIndex,
}

impl Park {
    fn install(build: Build) -> Park {
        Park::with(build, Some(Until::Cancelled))
    }

    /// Park the first check until [`release`](Self::release), whatever the
    /// flag says.
    fn hold(build: Build) -> Park {
        Park::with(build, Some(Until::Released))
    }

    /// A hook that only counts the checks (of a build that runs).
    fn counting(build: Build) -> Park {
        Park::with(build, None)
    }

    fn with(build: Build, until: Option<Until>) -> Park {
        let (tx, parked) = mpsc::channel();
        let tx = Mutex::new(tx);
        let first = AtomicBool::new(until.is_some());
        let checks = Arc::new(AtomicUsize::new(0));
        let builder: Arc<Mutex<Option<thread::Thread>>> = Arc::new(Mutex::new(None));
        let released = Arc::new(AtomicBool::new(false));
        let saw_cancel = Arc::new(AtomicBool::new(false));
        let (c, b, r, saw) = (
            Arc::clone(&checks),
            Arc::clone(&builder),
            Arc::clone(&released),
            Arc::clone(&saw_cancel),
        );
        let hook = move |cancel: Option<&AtomicBool>| {
            c.fetch_add(1, Ordering::SeqCst);
            if !first.swap(false, Ordering::SeqCst) {
                return;
            }
            *b.lock().unwrap() = Some(thread::current());
            let _ = tx.lock().unwrap().send(());
            let deadline = Instant::now() + HANG_GUARD;
            let done = || match until {
                Some(Until::Cancelled) => cancel.is_some_and(|f| f.load(Ordering::SeqCst)),
                _ => r.load(Ordering::SeqCst),
            };
            while !done() && !r.load(Ordering::SeqCst) && Instant::now() < deadline {
                // Woken by `release` (a raised flag is also re-read now and
                // then: a server may raise it).
                thread::park_timeout(Duration::from_millis(20));
            }
            if cancel.is_some_and(|f| f.load(Ordering::SeqCst)) {
                saw.store(true, Ordering::SeqCst);
            }
        };
        let uninstall: fn() = match build {
            Build::Csr => {
                build_hook::set(Some(Arc::new(hook)));
                || build_hook::set(None)
            }
            Build::StringIndex => {
                gstore::kvstore::string_index::build_hook::set(Some(Arc::new(hook)));
                || gstore::kvstore::string_index::build_hook::set(None)
            }
        };
        Park {
            parked,
            checks,
            builder,
            released,
            saw_cancel,
            uninstall,
        }
    }

    /// Wait until a build is parked at its first check.
    fn wait_parked(&self) {
        self.parked
            .recv_timeout(HANG_GUARD)
            .expect("no build reached a cancellation check (hang guard)");
    }

    /// Wake the parked build (a held one goes on; one waiting for its flag
    /// re-reads it).
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        if let Some(t) = self.builder.lock().unwrap().as_ref() {
            t.unpark();
        }
    }

    fn checks(&self) -> usize {
        self.checks.load(Ordering::SeqCst)
    }

    /// Whether the parked build's own cancel flag was raised when it went
    /// on (not the hang guard, nor a release).
    fn saw_cancel(&self) -> bool {
        self.saw_cancel.load(Ordering::SeqCst)
    }
}

impl Drop for Park {
    fn drop(&mut self) {
        (self.uninstall)();
        self.release();
    }
}

/// A directed chain `n0 -> n1 -> … -> n{len-1}` (entity edges) and a few
/// literal triples.
fn chain(len: usize) -> String {
    let mut s = String::new();
    for i in 0..len {
        s.push_str(&format!(
            "<http://ex/n{i}> <http://ex/p> <http://ex/n{}> .\n\
             <http://ex/n{i}> <http://ex/label> \"node number {i}\" .\n",
            i + 1
        ));
    }
    s
}

/// A query whose evaluation needs the CSR.
const GRAPH_QUERY: &str =
    "SELECT (kHopReachable(<http://ex/n0>, <http://ex/n5>, true, 6, {}) AS ?r) WHERE {}";

/// An UPDATE whose WHERE clause needs the CSR.
const GRAPH_UPDATE: &str = "INSERT { <http://ex/r> <http://ex/v> ?r } WHERE { \
                            { SELECT (kHopReachable(<http://ex/n0>, <http://ex/n5>, true, 6, {}) AS ?r) \
                            WHERE {} } }";

/// A query with a mandatory substring filter (the substring index prunes it).
const SUBSTRING_QUERY: &str =
    "SELECT ?s WHERE { ?s <http://ex/label> ?l FILTER(CONTAINS(?l, \"number 42\")) }";

/// A plain read (`Database::read_as`): the parked CSR build observes the
/// raised flag at its next check, the read fails with `Cancelled`, nothing
/// is cached — and a reader asking whether a CSR is cached does not wait
/// for the build meanwhile.
#[test]
fn a_cancelled_read_stops_its_csr_build_and_caches_nothing() {
    let _serial = serial();
    let db = Database::build_from_str("read", &chain(200)).unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    let park = Park::install(Build::Csr);
    thread::scope(|s| {
        let reader = s.spawn(|| db.query_read(GRAPH_QUERY, Some(Arc::clone(&flag)), None));
        park.wait_parked();
        // The build holds no lock a cache reader needs.
        assert!(!db.csr_is_cached());
        assert!(db.cached_csr().is_none());
        flag.store(true, Ordering::SeqCst);
        park.release();
        let r = reader.join().unwrap();
        assert!(matches!(r, Err(GStoreError::Cancelled)), "{r:?}");
    });
    assert!(!db.csr_is_cached(), "a cancelled build must not be cached");
    drop(park);
    // Without a cancel the same read builds and caches it.
    let r = db.query_read(GRAPH_QUERY, None, None).unwrap();
    assert_eq!(format!("{r:?}").matches("true").count(), 1, "{r:?}");
    assert!(db.csr_is_cached());
}

/// An UPDATE's WHERE clause (`Database::eval_modify_changes`): the same.
#[test]
fn a_cancelled_update_stops_its_csr_build_and_caches_nothing() {
    let _serial = serial();
    let mut db = Database::build_from_str("update", &chain(200)).unwrap();
    let before = db.triple_num();
    let flag = Arc::new(AtomicBool::new(false));
    let park = Park::install(Build::Csr);
    thread::scope(|s| {
        let db = &mut db;
        let f = Arc::clone(&flag);
        let writer = s.spawn(move || db.query_cancellable(GRAPH_UPDATE, f));
        park.wait_parked();
        flag.store(true, Ordering::SeqCst);
        park.release();
        let r = writer.join().unwrap();
        assert!(r.is_err(), "a cancelled UPDATE must fail: {r:?}");
    });
    assert!(!db.csr_is_cached(), "a cancelled build must not be cached");
    assert_eq!(
        db.triple_num(),
        before,
        "the cancelled UPDATE changed nothing"
    );
}

// ---- through the HTTP server --------------------------------------------------

/// An `ApiServer` serving in a background thread (root password `root`);
/// stopped when dropped.
struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    serving: Option<thread::JoinHandle<gstore::server::ShutdownReport>>,
    _root: TempDir,
}

impl Srv {
    fn start(tag: &str, stream: bool) -> Srv {
        let root = TempDir::new(tag);
        // Without check_point a write stays unsaved until the shutdown.
        let mut server = ApiServer::bind(root.path(), "root", "127.0.0.1:0")
            .expect("bind")
            .with_check_point(false)
            .with_shutdown_grace(Duration::from_millis(100));
        if stream {
            server = server.with_open_options(OpenOptions::stream());
        }
        let server = Arc::new(server);
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = Some(thread::spawn(move || s2.serve_forever()));
        Srv {
            server,
            addr,
            serving,
            _root: root,
        }
    }

    fn req(&self, method: &str, target: &str, body: &str) -> (u16, String) {
        request(&self.addr, method, target, body)
    }

    fn build(&self, name: &str, layout: &str, data: &str) {
        let (s, b) = self.req(
            "POST",
            &format!("/build?db_name={name}&layout={layout}"),
            data,
        );
        assert_eq!(s, 200, "build: {b}");
    }

    /// The id of the one running task of database `db`.
    fn running_task(&self, db: &str) -> u64 {
        let (s, list) = self.req("GET", "/tasklist", "");
        assert_eq!(s, 200, "{list}");
        let entry = list
            .split("{\"id\":")
            .skip(1)
            .find(|e| e.contains(&format!("\"db\":\"{db}\"")))
            .unwrap_or_else(|| panic!("no running task of {db}: {list}"));
        entry
            .split([',', '}'])
            .next()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("{list}"))
    }

    fn cancel(&self, db: &str) {
        let id = self.running_task(db);
        let (s, b) = self.req("POST", &format!("/cancel?id={id}"), "");
        assert_eq!(s, 200, "cancel: {b}");
    }

    fn stop(&mut self) -> gstore::server::ShutdownReport {
        self.server.shutdown();
        self.serving.take().unwrap().join().unwrap()
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        if self.serving.is_some() {
            let _ = self.stop();
        }
    }
}

fn request(addr: &SocketAddr, method: &str, target: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic cm9vdDpyb290\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status in {resp:?}"));
    let body = resp.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, body.to_string())
}

fn sparql(srv: &Srv, db: &str, q: &str) -> (u16, String) {
    srv.req("POST", &format!("/sparql?db_name={db}"), q)
}

/// `/cancel` of a read whose CSR build is parked, then the same read again:
/// the first answers cancelled, the second must build again (nothing was
/// cached) and answer.
fn cancel_a_parked_csr_read(srv: &Srv, db: &str) {
    let park = Park::install(Build::Csr);
    thread::scope(|s| {
        let reader = s.spawn(|| sparql(srv, db, GRAPH_QUERY));
        park.wait_parked();
        srv.cancel(db);
        park.release();
        let (status, body) = reader.join().unwrap();
        assert!(
            body.to_ascii_lowercase().contains("cancel"),
            "the read must answer cancelled: {status} {body}"
        );
    });
    drop(park);
    let again = Park::counting(Build::Csr);
    let (status, body) = sparql(srv, db, GRAPH_QUERY);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("true"), "{body}");
    assert!(
        again.checks() > 0,
        "the cancelled build cached its graph: the next read did not build"
    );
}

/// `/cancel` during the CSR build of a read on a Stream-mode (on-disk)
/// database is observed at the build's next check.
#[test]
fn cancel_stops_the_csr_build_of_a_stream_database() {
    let _serial = serial();
    let srv = Srv::start("stream", true);
    srv.build("s", "disk", &chain(200));
    cancel_a_parked_csr_read(&srv, "s");
}

/// `/cancel` during the CSR build of a read on an open transaction's
/// in-memory snapshot (`ReadSnapshot::read_as`, `SnapStore::Mem`).
#[test]
fn cancel_stops_the_csr_build_of_a_snapshot_read() {
    let _serial = serial();
    let srv = Srv::start("snapshot", false);
    srv.build("m", "snapshot", &chain(200));
    let (s, b) = srv.req("POST", "/begin?db_name=m", "");
    assert_eq!(s, 200, "begin: {b}");
    cancel_a_parked_csr_read(&srv, "m");
}

/// A shutdown while a read's CSR build is parked: the shutdown's cancel is
/// observed at the build's next check, and every database with unsaved
/// changes is saved.
#[test]
fn shutdown_stops_a_csr_build_and_saves_every_database() {
    let _serial = serial();
    let mut srv = Srv::start("shutdown", false);
    for db in ["a", "b"] {
        srv.build(db, "snapshot", &chain(50));
        let (s, b) = sparql(
            &srv,
            db,
            &format!("INSERT DATA {{ <http://ex/{db}> <http://ex/p> <http://ex/x> }}"),
        );
        assert_eq!(s, 200, "{b}");
    }
    let park = Park::install(Build::Csr);
    let addr = srv.addr;
    let report = thread::scope(|s| {
        let reader = s.spawn(move || request(&addr, "POST", "/sparql?db_name=a", GRAPH_QUERY));
        park.wait_parked();
        let report = srv.stop();
        let _ = reader.join();
        report
    });
    // review conc#5: the shutdown's cancel reached the parked build (before,
    // the build had no flag and ran on once the hang guard let it go).
    assert!(
        park.saw_cancel(),
        "the parked CSR build did not observe the shutdown's cancel"
    );
    let saved: Vec<String> = report
        .saved
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        saved.iter().any(|d| d.starts_with("a")) && saved.iter().any(|d| d.starts_with("b")),
        "every database must be saved: {report:?}"
    );
    assert!(report.failed.is_empty(), "{report:?}");
}

/// The files under `dir` with their sizes and modification times.
fn fingerprint(dir: &Path) -> Vec<(PathBuf, u64, Option<std::time::SystemTime>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(path),
                Ok(m) => out.push((path, m.len(), m.modified().ok())),
                Err(_) => {}
            }
        }
    }
    out.sort();
    out
}

/// A shutdown while a read keeps database `a` busy (its build ignores the
/// cancel until the test lets it go): database `b` is saved meanwhile — a
/// busy database delays only its own save — and `a` once it is free.
#[test]
fn a_database_kept_busy_delays_only_its_own_save() {
    let _serial = serial();
    let mut srv = Srv::start("busy", false);
    for db in ["a", "b"] {
        srv.build(db, "snapshot", &chain(50));
        let (s, b) = sparql(
            &srv,
            db,
            &format!("INSERT DATA {{ <http://ex/{db}> <http://ex/p> <http://ex/x> }}"),
        );
        assert_eq!(s, 200, "{b}");
    }
    let b_dir = srv._root.path().join("b.db");
    let before = fingerprint(&b_dir);
    assert!(!before.is_empty(), "{}", b_dir.display());
    let hold = Park::hold(Build::Csr);
    let addr = srv.addr;
    let server = Arc::clone(&srv.server);
    let serving = srv.serving.take().unwrap();
    let report = thread::scope(|s| {
        let reader = s.spawn(move || request(&addr, "POST", "/sparql?db_name=a", GRAPH_QUERY));
        hold.wait_parked();
        server.shutdown();
        // `b` is saved while `a`'s read is still held.
        let deadline = Instant::now() + HANG_GUARD;
        while fingerprint(&b_dir) == before {
            assert!(
                Instant::now() < deadline,
                "b was not saved while a was busy (hang guard)"
            );
            thread::park_timeout(Duration::from_millis(10));
        }
        hold.release();
        let _ = reader.join();
        serving.join().unwrap()
    });
    let saved: Vec<String> = report
        .saved
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(saved.len(), 2, "{report:?}");
    assert!(
        saved[0].starts_with('b') && saved[1].starts_with('a'),
        "b first, then a: {report:?}"
    );
}

/// The rows of SELECT `q` on `db`.
fn rows_of(db: &Database, q: &str, cancel: Option<Arc<AtomicBool>>) -> Result<usize, GStoreError> {
    match db.query_read(q, cancel, None)? {
        gstore::QueryResult::Select(rs) => Ok(rs.rows.len()),
        other => panic!("{other:?}"),
    }
}

/// The substring-index build of a substring FILTER: the parked build
/// observes the raised flag at its next check, the query fails with
/// `Cancelled`, and no index is cached.
#[test]
fn a_cancelled_substring_query_stops_its_index_build_and_caches_nothing() {
    let _serial = serial();
    let mut db = Database::build_from_str("substr", &chain(200)).unwrap();
    db.set_query_cache_size(0);
    let flag = Arc::new(AtomicBool::new(false));
    let park = Park::install(Build::StringIndex);
    thread::scope(|s| {
        let reader = s.spawn(|| rows_of(&db, SUBSTRING_QUERY, Some(Arc::clone(&flag))));
        park.wait_parked();
        flag.store(true, Ordering::SeqCst);
        park.release();
        let r = reader.join().unwrap();
        assert!(matches!(r, Err(GStoreError::Cancelled)), "{r:?}");
    });
    assert!(
        db.cached_string_index().is_none(),
        "a cancelled build must not be cached"
    );
    drop(park);
    let expected = rows_of(&db, SUBSTRING_QUERY, None).unwrap();
    assert!(db.cached_string_index().is_some());
    assert_eq!(expected, 1, "n42");
}

/// A reader never waits for another request's substring-index build: while
/// one is held, the same query on another thread answers at once (without
/// the prefilter, exactly), and the held build is cached once it ends.
#[test]
fn a_reader_does_not_wait_for_another_requests_index_build() {
    let _serial = serial();
    let mut db = Database::build_from_str("nowait", &chain(200)).unwrap();
    db.set_query_cache_size(0);
    let hold = Park::hold(Build::StringIndex);
    thread::scope(|s| {
        let builder = s.spawn(|| rows_of(&db, SUBSTRING_QUERY, None));
        hold.wait_parked();
        assert_eq!(rows_of(&db, SUBSTRING_QUERY, None).unwrap(), 1);
        assert!(db.cached_string_index().is_none(), "still being built");
        hold.release();
        assert_eq!(builder.join().unwrap().unwrap(), 1);
    });
    assert!(db.cached_string_index().is_some());
}

// ---- background statistics refreshes and server threads (conc#9, conc#14) -----

/// `POST /api` as root with `pairs` (JSON body).
fn api(srv: &Srv, pairs: &[(&str, &str)]) -> String {
    let mut body = String::from("{\"username\":\"root\",\"password\":\"root\"");
    for (k, v) in pairs {
        body.push_str(&format!(",\"{k}\":\"{v}\""));
    }
    body.push('}');
    let (s, b) = srv.req("POST", "/api", &body);
    assert_eq!(s, 200, "{b}");
    assert!(b.contains("\"StatusCode\":0"), "{b}");
    b
}

/// The threads of this process whose name starts with `prefix` (Linux:
/// `/proc/self/task/*/comm`; elsewhere none are counted).
fn threads_named(prefix: &str) -> usize {
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return 0;
    };
    tasks
        .flatten()
        .filter(|t| {
            std::fs::read_to_string(t.path().join("comm"))
                .is_ok_and(|name| name.trim_end().starts_with(prefix))
        })
        .count()
}

/// A background statistics refresh (started by `/api monitor` after a
/// write) holds no guard between its passes — a writer goes on while it is
/// parked there — and a shutdown stops it at its next check, joins its
/// thread and saves the database.
#[test]
fn a_statistics_refresh_yields_between_passes_and_stops_at_shutdown() {
    let _serial = serial();
    let mut srv = Srv::start("stats", false);
    srv.build("mon", "snapshot", &chain(50));
    let insert = |srv: &Srv, n: usize| {
        let (s, b) = sparql(
            srv,
            "mon",
            &format!("INSERT DATA {{ <http://ex/x{n}> <http://ex/p> <http://ex/y{n}> }}"),
        );
        assert_eq!(s, 200, "{b}");
    };
    // Statistics computed (and cached) for the data as built ...
    api(&srv, &[("operation", "monitor"), ("db_name", "mon")]);
    // ... then stale: the next monitor refreshes them in the background.
    insert(&srv, 0);

    // Park the refresh at its second check (after its first pass).
    let (tx, parked) = mpsc::channel();
    let tx = Mutex::new(tx);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    gstore::gapi::stats_refresh_hook::set(Some(Arc::new(move |cancel: &AtomicBool| {
        if c.fetch_add(1, Ordering::SeqCst) + 1 != 2 {
            return;
        }
        let _ = tx.lock().unwrap().send(());
        let deadline = Instant::now() + HANG_GUARD;
        while !cancel.load(Ordering::SeqCst) && Instant::now() < deadline {
            thread::park_timeout(Duration::from_millis(20));
        }
    })));
    struct Unhook;
    impl Drop for Unhook {
        fn drop(&mut self) {
            gstore::gapi::stats_refresh_hook::set(None);
        }
    }
    let _unhook = Unhook;

    api(&srv, &[("operation", "monitor"), ("db_name", "mon")]);
    parked
        .recv_timeout(HANG_GUARD)
        .expect("no background refresh reached its second check (hang guard)");
    // A writer is not blocked by the parked refresh.
    insert(&srv, 1);
    assert!(threads_named("gstore-stats-re") > 0);

    let report = srv.stop();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the refresh must stop at the check that saw the shutdown's cancel"
    );
    assert_eq!(
        threads_named("gstore-stats-re"),
        0,
        "the refresh thread was joined"
    );
    assert!(
        report.saved.iter().any(|p| p.ends_with("mon.db")),
        "{report:?}"
    );
}

/// After `serve_forever` returns, the server's connection workers (one of
/// them idling on a kept-alive connection), its statistics refreshes, the
/// cancel bridge and the idle-transaction reaper (r2-txn's, merged in
/// round 2) are gone: joined, not left running.
#[test]
fn no_server_thread_outlives_serve_forever() {
    let _serial = serial();
    let mut srv = Srv::start("threads", false);
    srv.build("t", "snapshot", &chain(20));
    let (s, b) = sparql(&srv, "t", "SELECT * WHERE { ?s ?p ?o } LIMIT 1");
    assert_eq!(s, 200, "{b}");
    // A kept-alive connection left idle.
    let mut idle = TcpStream::connect(srv.addr).unwrap();
    idle.set_read_timeout(Some(HANG_GUARD)).unwrap();
    idle.write_all(
        b"GET /status HTTP/1.1\r\nHost: localhost\r\nAuthorization: Basic cm9vdDpyb290\r\n\r\n",
    )
    .unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(idle.read(&mut byte).unwrap(), 1);
        head.push(byte[0]);
    }
    assert!(String::from_utf8_lossy(&head).contains("keep-alive"));
    assert!(threads_named("gstore-http-") > 0);
    // The reaper's name, cut to the 15 bytes `comm` keeps.
    assert!(threads_named("gstore-txn-reap") > 0);

    let report = srv.stop();
    assert!(report.drained, "{report:?}");
    for prefix in [
        "gstore-http-",
        "gstore-stats-re",
        "gstore-cancel-b",
        "gstore-txn-reap",
    ] {
        assert_eq!(
            threads_named(prefix),
            0,
            "{prefix}* threads outlived serve_forever"
        );
    }
    drop(idle);
}

/// The single-database `Server` joins its connection workers too.
#[test]
fn no_single_database_server_worker_outlives_serve_forever() {
    let _serial = serial();
    let db = Database::build_from_str("single", &chain(20)).unwrap();
    let server = Arc::new(gstore::server::Server::bind(db, "127.0.0.1:0").unwrap());
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    let (status, body) = request(&addr, "GET", "/status", "");
    assert_eq!(status, 200, "{body}");
    assert!(threads_named("gstore-http-") > 0);
    server.shutdown();
    let report = serving.join().unwrap();
    assert!(report.drained, "{report:?}");
    assert_eq!(
        threads_named("gstore-http-"),
        0,
        "workers outlived serve_forever"
    );
}
