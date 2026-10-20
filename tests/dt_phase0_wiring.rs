//! Data test: the cross-cutting Phase 0 wiring between the persistence layer,
//! the HTTP servers and the `LOAD` policy.
//!
//! - Both servers open databases with the auto-detecting [`Database::open`], so
//!   an on-disk (`gbuild --disk`) database is served *as* an on-disk database:
//!   its updates are written through to `kvstore.kv` (never a shadow
//!   `store.bin`) and survive a restart.
//! - `/restore` copies a backup into the database's own directory, so later
//!   writes never land in the backup.
//! - Every served database gets [`LoadPolicy::restricted`]: a client's SPARQL
//!   `LOAD` cannot make the server read local files unless a directory is
//!   explicitly allowed.
//! - `gserver` refuses to serve a directory whose database it cannot open
//!   (instead of serving an empty stand-in that could be saved over it).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gstore::http_api::ApiServer;
use gstore::server::{Server, ShutdownReport};
use gstore::{Database, LoadPolicy, QueryResult, StorageKind};

const BASE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";
const NEW_TRIPLE: &str = "INSERT DATA { <http://ex/c> <http://ex/p> <http://ex/d> }";

fn tmp_dir(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("gstore_wiring_{tag}_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Build an on-disk (`kvstore.kv`) database from `BASE` at `dir`.
fn build_disk(dir: &Path) {
    let src = dir.with_extension("src.nt");
    std::fs::write(&src, BASE).unwrap();
    Database::build_disk(dir, &[&src]).unwrap();
    assert_eq!(Database::storage_kind(dir), Some(StorageKind::Disk));
}

fn request(addr: &SocketAddr, method: &str, target: &str, auth: bool, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: x\r\n");
    if auth {
        // root:root
        raw.push_str("Authorization: Basic cm9vdDpyb290\r\n");
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {resp:?}"));
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

fn ask(db: &mut Database, s: &str, p: &str, o: &str) -> bool {
    match db.query(&format!("ASK {{ <{s}> <{p}> <{o}> }}")).unwrap() {
        QueryResult::Ask(b) => b,
        other => panic!("unexpected {other:?}"),
    }
}

struct Api {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    serving: thread::JoinHandle<ShutdownReport>,
}

impl Api {
    fn start(root: &Path, check_point: bool, policy: Option<LoadPolicy>) -> Api {
        let mut server = ApiServer::bind(root, "root", "127.0.0.1:0")
            .unwrap()
            .with_check_point(check_point);
        if let Some(p) = policy {
            server = server.with_load_policy(p);
        }
        let server = Arc::new(server);
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = thread::spawn(move || s2.serve_forever());
        Api {
            server,
            addr,
            serving,
        }
    }

    fn root(&self, method: &str, target: &str, body: &str) -> (u16, String) {
        request(&self.addr, method, target, true, body)
    }

    fn stop(self) -> ShutdownReport {
        self.server.shutdown();
        let report = self.serving.join().unwrap();
        assert!(report.failed.is_empty(), "{report:?}");
        report
    }
}

#[test]
fn api_serves_an_on_disk_database_and_writes_it_through() {
    let root = tmp_dir("api_disk");
    let dir = root.join("d.db");
    build_disk(&dir);

    // check_point off: the write-through must still be committed per write.
    let api = Api::start(&root, false, None);
    let (s, body) = api.root("POST", "/load?db_name=d", "");
    assert_eq!(s, 200, "{body}");
    let (s, body) = api.root("POST", "/update?db_name=d", NEW_TRIPLE);
    assert_eq!(s, 200, "{body}");
    let (s, body) = api.root(
        "POST",
        "/batchInsert?db_name=d",
        "<http://ex/b1> <http://ex/p> <http://ex/o1> .\n",
    );
    assert_eq!(s, 200, "{body}");
    assert!(!body.contains("warning"), "{body}");
    let (s, body) = api.root(
        "POST",
        "/upload?db_name=d&format=ntriples",
        "<http://ex/u1> <http://ex/p> <http://ex/o2> .\n",
    );
    assert_eq!(s, 200, "{body}");
    let (s, body) = api.root(
        "POST",
        "/batchRemove?db_name=d",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    );
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("\"applied\":1"), "{body}");
    api.stop();

    // Still an on-disk database, never shadowed by a snapshot.
    assert_eq!(Database::storage_kind(&dir), Some(StorageKind::Disk));
    assert!(!dir.join("store.bin").exists(), "no shadow snapshot is written");
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.storage(), StorageKind::Disk);
    assert!(ask(&mut db, "http://ex/c", "http://ex/p", "http://ex/d"));
    assert!(ask(&mut db, "http://ex/b1", "http://ex/p", "http://ex/o1"));
    assert!(ask(&mut db, "http://ex/u1", "http://ex/p", "http://ex/o2"));
    assert!(!ask(&mut db, "http://ex/a", "http://ex/p", "http://ex/b"));
    assert_eq!(db.triple_num(), 3);
}

#[test]
fn api_unload_commits_and_reload_sees_the_writes() {
    let root = tmp_dir("api_unload_disk");
    let dir = root.join("d.db");
    build_disk(&dir);
    let api = Api::start(&root, true, None);
    assert_eq!(api.root("POST", "/load?db_name=d", "").0, 200);
    assert_eq!(api.root("POST", "/update?db_name=d", NEW_TRIPLE).0, 200);
    let (s, body) = api.root("POST", "/unload?db_name=d", "");
    assert_eq!(s, 200, "{body}");
    // Loading again opens the store files afresh.
    assert_eq!(api.root("POST", "/load?db_name=d", "").0, 200);
    let (s, body) = api.root(
        "POST",
        "/sparql?db_name=d",
        "ASK { <http://ex/c> <http://ex/p> <http://ex/d> }",
    );
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("true"), "{body}");
    api.stop();
}

#[test]
fn api_restore_of_a_disk_backup_never_writes_into_the_backup() {
    let root = tmp_dir("api_restore_disk");
    let backup = tmp_dir("api_restore_disk_backup").join("b.db");
    build_disk(&backup);

    let api = Api::start(&root, true, None);
    let (s, body) = api.root(
        "POST",
        &format!("/restore?db_name=r&path={}", backup.display()),
        "",
    );
    assert_eq!(s, 200, "{body}");
    assert_eq!(api.root("POST", "/update?db_name=r", NEW_TRIPLE).0, 200);
    // A second restore replaces the loaded copy (its files are closed first).
    let (s, body) = api.root(
        "POST",
        &format!("/restore?db_name=r&path={}", backup.display()),
        "",
    );
    assert_eq!(s, 200, "{body}");
    let (s, body) = api.root(
        "POST",
        "/sparql?db_name=r",
        "ASK { <http://ex/c> <http://ex/p> <http://ex/d> }",
    );
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("false"), "restored again from the backup: {body}");
    assert_eq!(api.root("POST", "/update?db_name=r", NEW_TRIPLE).0, 200);
    // A path that holds no backup is refused before anything is replaced.
    let (s, _) = api.root("POST", "/restore?db_name=r&path=/nonexistent/backup", "");
    assert_eq!(s, 500);
    api.stop();

    let mut b = Database::open(&backup).unwrap();
    assert_eq!(b.triple_num(), 1, "the backup is untouched");
    assert!(!ask(&mut b, "http://ex/c", "http://ex/p", "http://ex/d"));
    drop(b);
    let restored = root.join("r.db");
    assert_eq!(Database::storage_kind(&restored), Some(StorageKind::Disk));
    let mut r = Database::open(&restored).unwrap();
    assert!(ask(&mut r, "http://ex/c", "http://ex/p", "http://ex/d"));
    assert_eq!(r.triple_num(), 2);
}

#[test]
fn api_load_is_restricted_unless_a_directory_is_allowed() {
    let data_dir = tmp_dir("api_load_data");
    let file = data_dir.join("extra.nt");
    std::fs::write(&file, "<http://ex/l> <http://ex/p> <http://ex/m> .\n").unwrap();
    let load = format!("LOAD <file://{}>", file.display());

    // Default: LOAD reads nothing.
    let root = tmp_dir("api_load_default");
    let api = Api::start(&root, true, None);
    assert_eq!(api.root("POST", "/build?db_name=w", BASE).0, 200);
    let (s, body) = api.root("POST", "/update?db_name=w", &load);
    assert_ne!(s, 200, "{body}");
    assert!(body.contains("load policy"), "{body}");
    let (_, body) = api.root(
        "POST",
        "/sparql?db_name=w",
        "ASK { <http://ex/l> <http://ex/p> <http://ex/m> }",
    );
    assert!(body.contains("false"), "{body}");
    api.stop();

    // With the directory allowed, the same LOAD works.
    let root = tmp_dir("api_load_allowed");
    let api = Api::start(
        &root,
        true,
        Some(LoadPolicy::restricted().with_allowed_dir(&data_dir)),
    );
    assert_eq!(api.root("POST", "/build?db_name=w", BASE).0, 200);
    let (s, body) = api.root("POST", "/update?db_name=w", &load);
    assert_eq!(s, 200, "{body}");
    let (_, body) = api.root(
        "POST",
        "/sparql?db_name=w",
        "ASK { <http://ex/l> <http://ex/p> <http://ex/m> }",
    );
    assert!(body.contains("true"), "{body}");
    api.stop();
}

/// Serve `db` with the single-database server.
fn start_single(
    db: Database,
    dir: Option<&Path>,
    policy: Option<LoadPolicy>,
) -> (Arc<Server>, SocketAddr, thread::JoinHandle<ShutdownReport>) {
    let mut server = Server::bind(db, "127.0.0.1:0").unwrap();
    if let Some(d) = dir {
        server = server.with_db_dir(d);
    }
    if let Some(p) = policy {
        server = server.with_load_policy(p);
    }
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    (server, addr, serving)
}

#[test]
fn single_server_serves_an_on_disk_database() {
    let dir = tmp_dir("single_disk").join("s.db");
    build_disk(&dir);
    let (server, addr, serving) = start_single(Database::open(&dir).unwrap(), Some(&dir), None);
    let (s, body) = request(&addr, "POST", "/update", false, NEW_TRIPLE);
    assert_eq!(s, 200, "{body}");
    assert!(!body.contains("warning"), "{body}");
    server.shutdown();
    let report = serving.join().unwrap();
    assert!(report.failed.is_empty(), "{report:?}");

    assert!(!dir.join("store.bin").exists(), "no shadow snapshot is written");
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.storage(), StorageKind::Disk);
    assert!(ask(&mut db, "http://ex/c", "http://ex/p", "http://ex/d"));
    assert!(ask(&mut db, "http://ex/a", "http://ex/p", "http://ex/b"));
}

#[test]
fn single_server_load_is_restricted_unless_a_directory_is_allowed() {
    let data_dir = tmp_dir("single_load_data");
    let file = data_dir.join("extra.nt");
    std::fs::write(&file, "<http://ex/l> <http://ex/p> <http://ex/m> .\n").unwrap();
    let load = format!("LOAD <file://{}>", file.display());
    let has_loaded = "ASK { <http://ex/l> <http://ex/p> <http://ex/m> }";

    let (server, addr, serving) =
        start_single(Database::build_from_str("s", BASE).unwrap(), None, None);
    let (s, body) = request(&addr, "POST", "/update", false, &load);
    assert_ne!(s, 200, "{body}");
    assert!(body.contains("load policy"), "{body}");
    let (_, body) = request(&addr, "POST", "/sparql", false, has_loaded);
    assert!(body.contains("false"), "{body}");
    server.shutdown();
    serving.join().unwrap();

    let (server, addr, serving) = start_single(
        Database::build_from_str("s", BASE).unwrap(),
        None,
        Some(LoadPolicy::restricted().with_allowed_dir(&data_dir)),
    );
    let (s, body) = request(&addr, "POST", "/update", false, &load);
    assert_eq!(s, 200, "{body}");
    let (_, body) = request(&addr, "POST", "/sparql", false, has_loaded);
    assert!(body.contains("true"), "{body}");
    server.shutdown();
    serving.join().unwrap();
}

#[test]
fn gserver_refuses_a_directory_it_cannot_open() {
    let dir = tmp_dir("gserver_corrupt").join("bad.db");
    std::fs::create_dir_all(&dir).unwrap();
    // A snapshot whose metadata is garbage.
    std::fs::write(dir.join("meta.bin"), b"definitely not bincode").unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_gserver"))
        .arg(&dir)
        .args(["--port", "0"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("gserver kept serving a database it could not open");
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success());
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(err.contains("could not open"), "{err}");
    // Nothing was written over it.
    assert_eq!(
        std::fs::read(dir.join("meta.bin")).unwrap(),
        b"definitely not bincode"
    );
}
