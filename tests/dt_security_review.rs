//! Security-review regressions, exercised over HTTP against real servers:
//!
//! - a SPARQL request or RDF/XML document nested absurdly deep is a `400`, not
//!   a stack overflow that aborts the whole server process;
//! - `/build?db_path=` reads only what the server's LOAD policy allows;
//! - `/backup` writes only inside the server's backup directory and never
//!   clobbers another database or its backups; `/restore` reads only from
//!   there (or from directories the LOAD policy allows);
//! - a federated `SERVICE` cannot make a server on the restricted policy send
//!   requests to other hosts (SSRF).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::db::LoadPolicy;
use gstore::http_api::ApiServer;
use gstore::server::Server;
use gstore::Database;

const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";

fn tmp_dir(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let p = std::env::temp_dir().join(format!("gstore_secrev_{tag}_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// One request; `auth` adds root's Basic credentials (`root:root`).
fn request(addr: &SocketAddr, method: &str, target: &str, auth: bool, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let auth = if auth { "Authorization: Basic cm9vdDpyb290\r\n" } else { "" };
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {resp:?} (did the server die?)"));
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

struct Api {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    root: PathBuf,
}

impl Api {
    fn start(tag: &str, policy: Option<LoadPolicy>) -> Api {
        let root = tmp_dir(tag).join("root");
        let mut server = ApiServer::bind(&root, "root", "127.0.0.1:0").unwrap();
        if let Some(p) = policy {
            server = server.with_load_policy(p);
        }
        let server = Arc::new(server);
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        thread::spawn(move || s2.serve_forever());
        Api { server, addr, root }
    }

    fn root(&self, method: &str, target: &str, body: &str) -> (u16, String) {
        request(&self.addr, method, target, true, body)
    }

    fn alive(&self) {
        let (s, b) = self.root("GET", "/status", "");
        assert_eq!(s, 200, "the server must keep serving: {b}");
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.shutdown();
    }
}

fn deep_filter(n: usize) -> String {
    format!("SELECT * WHERE {{ ?s ?p ?o FILTER({}1{}) }}", "(".repeat(n), ")".repeat(n))
}

fn deep_groups(n: usize) -> String {
    format!("SELECT * WHERE {}?s ?p ?o{}", "{ ".repeat(n), " }".repeat(n))
}

fn deep_rdfxml(n: usize) -> String {
    format!(
        "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">{}{}</rdf:RDF>",
        "<a>".repeat(n),
        "</a>".repeat(n)
    )
}

// ---- nesting depth -----------------------------------------------------------

#[test]
fn deeply_nested_sparql_is_refused_by_the_api_server() {
    let api = Api::start("deep_api", None);
    assert_eq!(api.root("POST", "/build?db_name=d", DATA).0, 200);
    for q in [deep_filter(100_000), deep_groups(100_000), deep_filter(1000)] {
        for target in ["/sparql?db_name=d", "/update?db_name=d"] {
            let (s, b) = api.root("POST", target, &q);
            assert_eq!(s, 400, "{target}: {b}");
            assert!(b.contains("nested too deeply"), "{b}");
        }
        api.alive();
    }
    // Inside a transaction too.
    let (s, b) = api.root("POST", "/begin?db_name=d", "");
    assert_eq!(s, 200, "{b}");
    let tid = b.split("\"TID\":\"").nth(1).and_then(|x| x.split('"').next()).unwrap().to_string();
    let (s, b) = api.root("POST", &format!("/tquery?tid={tid}"), &deep_groups(100_000));
    assert_eq!(s, 400, "{b}");
    api.alive();
    // Reasonable nesting still works.
    let (s, b) = api.root("POST", "/sparql?db_name=d", &deep_filter(50));
    assert_eq!(s, 200, "{b}");
}

#[test]
fn deeply_nested_sparql_is_refused_by_the_single_database_server() {
    let db = Database::build_from_str("s", DATA).unwrap();
    let server = Arc::new(Server::bind(db, "127.0.0.1:0").unwrap());
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    for q in [deep_filter(100_000), deep_groups(100_000)] {
        let (s, b) = request(&addr, "POST", "/sparql", false, &q);
        assert_eq!(s, 400, "{b}");
    }
    let (s, b) = request(&addr, "POST", "/sparql", false, "ASK { ?s ?p ?o }");
    assert_eq!(s, 200, "still serving: {b}");
    server.shutdown();
}

#[test]
fn deeply_nested_rdfxml_is_refused_by_upload_and_batch_insert() {
    let api = Api::start("deep_rdfxml", None);
    assert_eq!(api.root("POST", "/build?db_name=x", DATA).0, 200);
    let doc = deep_rdfxml(50_000);
    for target in ["/upload?db_name=x&format=rdfxml", "/batchInsert?db_name=x&format=rdfxml"] {
        let (s, b) = api.root("POST", target, &doc);
        assert_eq!(s, 400, "{target}: {b}");
        assert!(b.contains("nested"), "{b}");
        api.alive();
    }
}

// ---- /build?db_path= -----------------------------------------------------------

#[test]
fn build_from_a_server_path_follows_the_load_policy() {
    let outside = tmp_dir("build_outside");
    let secret = outside.join("secret.nt");
    std::fs::write(&secret, "<http://ex/s> <http://ex/p> \"TOP-SECRET\" .\n").unwrap();

    // The default (restricted) policy: no server-local file at all, the same
    // as SPARQL LOAD.
    let api = Api::start("build_restricted", None);
    for path in [secret.display().to_string(), "/etc/passwd".into(), "/dev/zero".into()] {
        let (s, b) = api.root("POST", &format!("/build?db_name=leak&db_path={path}"), "");
        assert_eq!(s, 403, "{path}: {b}");
        assert!(!b.contains("TOP-SECRET") && !b.contains("root:"), "{b}");
    }
    let (s, _) = api.root("POST", "/sparql?db_name=leak", "SELECT * WHERE { ?s ?p ?o }");
    assert_ne!(s, 200, "nothing was built");
    // Refusals do not reveal whether a path exists.
    let (_, missing) = api.root("POST", "/build?db_name=leak&db_path=/nonexistent/x.nt", "");
    let (_, present) = api.root("POST", &format!("/build?db_name=leak&db_path={}", secret.display()), "");
    assert_eq!(
        missing.replace("/nonexistent/x.nt", "P"),
        present.replace(&secret.display().to_string(), "P")
    );

    // An allowed directory: files inside are built, files outside (or
    // escaping it) are not, and non-regular files never.
    let allowed = tmp_dir("build_allowed");
    std::fs::write(allowed.join("ok.nt"), DATA).unwrap();
    let api = Api::start("build_allowed_srv", Some(LoadPolicy::restricted().with_allowed_dir(&allowed)));
    let ok = allowed.join("ok.nt").display().to_string();
    let (s, b) = api.root("POST", &format!("/build?db_name=ok&db_path={ok}"), "");
    assert_eq!(s, 200, "{b}");
    let escape = format!("{}/../{}", allowed.display(), secret.strip_prefix(secret.parent().unwrap().parent().unwrap()).unwrap().display());
    for path in [secret.display().to_string(), escape, allowed.display().to_string()] {
        let (s, b) = api.root("POST", &format!("/build?db_name=no&db_path={path}"), "");
        assert_eq!(s, 403, "{path}: {b}");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/dev/zero", allowed.join("zero.nt")).unwrap();
        let (s, b) = api.root(
            "POST",
            &format!("/build?db_name=no&db_path={}", allowed.join("zero.nt").display()),
            "",
        );
        assert_eq!(s, 403, "a device is never read: {b}");
    }
}

// ---- /backup and /restore --------------------------------------------------

fn snapshot_files(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn backup_writes_only_inside_the_backup_directory() {
    let api = Api::start("backup_confine", None);
    assert_eq!(api.root("POST", "/build?db_name=d1", DATA).0, 200);
    assert_eq!(api.root("POST", "/build?db_name=victim", DATA).0, 200);
    let backups = api.root.join("backups");

    // Outside the backup directory: refused, and nothing there is touched —
    // in particular no other layout's store files are deleted.
    let outside = tmp_dir("backup_victim");
    std::fs::create_dir_all(outside.join("rocksdb")).unwrap();
    std::fs::write(outside.join("rocksdb/CURRENT"), b"x").unwrap();
    std::fs::write(outside.join("kvstore.kv"), b"x").unwrap();
    let victim_db = api.root.join("victim.db");
    let before = snapshot_files(&victim_db);
    for path in [
        outside.display().to_string(),
        victim_db.display().to_string(),
        api.root.display().to_string(),
        "../victim.db".to_string(),
        "x/../../victim.db".to_string(),
        backups.display().to_string(),
    ] {
        let (s, b) = api.root("POST", &format!("/backup?db_name=d1&path={path}"), "");
        assert_eq!(s, 403, "{path}: {b}");
    }
    assert!(outside.join("rocksdb/CURRENT").exists() && outside.join("kvstore.kv").exists());
    assert_eq!(snapshot_files(&victim_db), before, "another database is untouched");
    #[cfg(unix)]
    {
        // A symlink inside the backup directory pointing out of it.
        std::fs::create_dir_all(&backups).unwrap();
        std::os::unix::fs::symlink(&outside, backups.join("link")).unwrap();
        let (s, b) = api.root("POST", "/backup?db_name=d1&path=link", "");
        assert_eq!(s, 403, "{b}");
        assert!(outside.join("kvstore.kv").exists());
    }

    // Inside it: the default target and a relative path; repeatable.
    for _ in 0..2 {
        let (s, b) = api.root("POST", "/backup?db_name=d1", "");
        assert_eq!(s, 200, "{b}");
        let (s, b) = api.root("POST", "/backup?db_name=d1&path=nightly/d1", "");
        assert_eq!(s, 200, "{b}");
    }
    assert!(Database::storage_kind(backups.join("d1.backup")).is_some());
    assert!(Database::storage_kind(backups.join("nightly/d1")).is_some());

    // Another database's backup, a directory holding other data, or a path
    // nested in a backup are never overwritten.
    let (s, b) = api.root("POST", "/backup?db_name=victim", "");
    assert_eq!(s, 200, "{b}");
    let victim_backup = snapshot_files(&backups.join("victim.backup"));
    std::fs::create_dir_all(backups.join("other")).unwrap();
    std::fs::write(backups.join("other/kvstore.kv"), b"precious").unwrap();
    for (path, status) in [("victim.backup", 409), ("other", 409), ("victim.backup/sub", 409)] {
        let (s, b) = api.root("POST", &format!("/backup?db_name=d1&path={path}"), "");
        assert_eq!(s, status, "{path}: {b}");
    }
    assert_eq!(snapshot_files(&backups.join("victim.backup")), victim_backup);
    assert_eq!(std::fs::read(backups.join("other/kvstore.kv")).unwrap(), b"precious");
}

#[test]
fn restore_reads_only_from_the_backup_directory_or_allowed_dirs() {
    let elsewhere = tmp_dir("restore_elsewhere");
    let foreign = elsewhere.join("foreign.db");
    Database::build_from_str("foreign", "<http://ex/f> <http://ex/p> \"FOREIGN\" .\n")
        .unwrap()
        .save(&foreign)
        .unwrap();

    let api = Api::start("restore_confine", None);
    assert_eq!(api.root("POST", "/build?db_name=r", DATA).0, 200);
    let (s, b) = api.root("POST", &format!("/restore?db_name=r&path={}", foreign.display()), "");
    assert_eq!(s, 403, "{b}");
    // Another server database's own directory is not a backup source either.
    assert_eq!(api.root("POST", "/build?db_name=other", DATA).0, 200);
    let (s, b) = api.root(
        "POST",
        &format!("/restore?db_name=r&path={}", api.root.join("other.db").display()),
        "",
    );
    assert_eq!(s, 403, "{b}");
    // A backup made by this server restores (relative or absolute path).
    assert_eq!(api.root("POST", "/backup?db_name=r&path=snap", "").0, 200);
    let (s, b) = api.root("POST", "/restore?db_name=r&path=snap", "");
    assert_eq!(s, 200, "{b}");
    let abs = api.root.join("backups/snap").display().to_string();
    let (s, b) = api.root("POST", &format!("/restore?db_name=r&path={abs}"), "");
    assert_eq!(s, 200, "{b}");

    // A directory the load policy allows is a valid source.
    let api = Api::start("restore_allowed", Some(LoadPolicy::restricted().with_allowed_dir(&elsewhere)));
    let (s, b) = api.root("POST", &format!("/restore?db_name=r&path={}", foreign.display()), "");
    assert_eq!(s, 200, "{b}");
    let (_, b) = api.root("POST", "/sparql?db_name=r", "SELECT ?o WHERE { ?s ?p ?o }");
    assert!(b.contains("FOREIGN"), "{b}");
}

// ---- SERVICE (SSRF) -------------------------------------------------------------

/// A listener standing in for an internal service; records whether anything
/// connected to it and answers like a SPARQL endpoint.
fn internal_service() -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/admin/secret", listener.local_addr().unwrap());
    let hit = Arc::new(AtomicBool::new(false));
    let h = Arc::clone(&hit);
    thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            h.store(true, Ordering::SeqCst);
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = r#"{"head":{"vars":["x"]},"results":{"bindings":[{"x":{"type":"literal","value":"internal-secret"}}]}}"#;
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/sparql-results+json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    (url, hit)
}

#[test]
fn service_cannot_reach_other_hosts_from_a_restricted_server() {
    let (url, hit) = internal_service();
    let queries = [
        format!("SELECT ?x WHERE {{ SERVICE <{url}> {{ ?x ?y ?z }} }}"),
        format!("SELECT ?x WHERE {{ SERVICE SILENT <{url}> {{ ?x ?y ?z }} }}"),
        format!("ASK {{ ?s ?p ?o FILTER EXISTS {{ SERVICE <{url}> {{ ?x ?y ?z }} }} }}"),
    ];

    // The multi-database API server (restricted by default).
    let api = Api::start("ssrf_api", None);
    assert_eq!(api.root("POST", "/build?db_name=d", DATA).0, 200);
    for q in &queries {
        let (s, b) = api.root("POST", "/sparql?db_name=d", q);
        assert_eq!(s, 400, "{q}: {b}");
        assert!(b.contains("SERVICE is disabled") && !b.contains("internal-secret"), "{b}");
    }
    let update = format!(
        "INSERT {{ <http://ex/leak> <http://ex/p> ?x }} WHERE {{ SERVICE <{url}> {{ ?x ?y ?z }} }}"
    );
    let (s, b) = api.root("POST", "/update?db_name=d", &update);
    assert_eq!(s, 400, "{b}");

    // The single-database server (unauthenticated, restricted by default).
    let db = Database::build_from_str("s", DATA).unwrap();
    let server = Arc::new(Server::bind(db, "127.0.0.1:0").unwrap());
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    for q in &queries {
        let (s, b) = request(&addr, "POST", "/sparql", false, q);
        assert_eq!(s, 400, "{q}: {b}");
        assert!(!b.contains("internal-secret"), "{b}");
    }
    server.shutdown();
    thread::sleep(Duration::from_millis(100));
    assert!(!hit.load(Ordering::SeqCst), "no connection may reach the internal host");

    // An operator who opts in (`gserver --allow-service`) gets federation.
    let mut policy = LoadPolicy::restricted();
    policy.allow_remote = true;
    let api = Api::start("ssrf_opt_in", Some(policy));
    assert_eq!(api.root("POST", "/build?db_name=d", DATA).0, 200);
    let (s, b) = api.root("POST", "/sparql?db_name=d", &queries[0]);
    assert_eq!(s, 200, "{b}");
    assert!(b.contains("internal-secret"), "{b}");
    assert!(hit.load(Ordering::SeqCst));
}
