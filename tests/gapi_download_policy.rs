//! The policy of the upstream `/api` layer's file routes (`/file/download`,
//! `/file/upload`) and of the files they serve (review sec#4, gapi#2,
//! gapi#4, robust#7, sec#11, sec#14, gapi#13, gapi#14, gapi#11):
//!
//! * a download needs credentials by default; `anonymous_download=on`
//!   restores upstream's public export directory;
//! * a user reads an export or a `format=file` result only with the matching
//!   privilege on its database, and an upload only when it is his — as a
//!   download and as a `batchInsert` source;
//! * export and result names carry 128 random bits;
//! * a download is streamed (bounded memory), and only so many run at once;
//! * `format=file` results are written without a result tree in memory;
//! * expired server-written files and orphaned task results are removed;
//!   uploads have an optional per-user quota.
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
use std::time::{Duration, Instant, SystemTime};

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};

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
    fn upload_dir(&self) -> PathBuf {
        self.root.join("upload")
    }
    /// An RDF file put in the upload directory by the operator (no owner
    /// record); its absolute path.
    fn data_file(&self, name: &str, content: &str) -> String {
        fs::create_dir_all(self.upload_dir()).unwrap();
        let p = self.upload_dir().join(name);
        fs::write(&p, content).unwrap();
        p.to_string_lossy().into_owned()
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
    fn json(&self) -> Json {
        json::parse(&self.text()).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
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

/// `POST /api` as `user` with `pairs`; the HTTP 200 JSON envelope.
fn api_as(addr: &SocketAddr, user: &str, pairs: &[(&str, &str)]) -> Json {
    let mut body = Json::obj()
        .with("username", user)
        .with("password", password(user));
    for (k, v) in pairs {
        body.set(*k, *v);
    }
    let r = send(
        addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        body.to_string().as_bytes(),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()
}

fn api(addr: &SocketAddr, pairs: &[(&str, &str)]) -> Json {
    api_as(addr, "root", pairs)
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no StatusCode in {j}"))
}

fn string(j: &Json, key: &str) -> String {
    j.get(key)
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no string {key} in {j}"))
        .to_string()
}

fn ok(j: &Json) {
    assert_eq!(code(j), 0, "{j}");
}

/// Build and load `db` from `path` (as root).
fn build_loaded(addr: &SocketAddr, db: &str, path: &str) {
    ok(&api(
        addr,
        &[("operation", "build"), ("db_name", db), ("db_path", path)],
    ));
    ok(&api(addr, &[("operation", "load"), ("db_name", db)]));
}

fn add_user(addr: &SocketAddr, name: &str) {
    ok(&api(
        addr,
        &[
            ("operation", "usermanage"),
            ("type", "1"),
            ("op_username", name),
            ("op_password", "pw"),
        ],
    ));
}

fn grant(addr: &SocketAddr, user: &str, db: &str, privileges: &str) {
    ok(&api(
        addr,
        &[
            ("operation", "userprivilegemanage"),
            ("type", "1"),
            ("op_username", user),
            ("db_name", db),
            ("privileges", privileges),
        ],
    ));
}

/// `export` of `db` as `user`: the reported file path.
fn export_as(addr: &SocketAddr, user: &str, db: &str) -> String {
    let j = api_as(addr, user, &[("operation", "export"), ("db_name", db)]);
    ok(&j);
    string(&j, "filepath")
}

/// A `format=file` query on `db` as `user`: the reported file name.
fn query_file_as(addr: &SocketAddr, user: &str, db: &str, sparql: &str) -> String {
    let j = api_as(
        addr,
        user,
        &[
            ("operation", "query"),
            ("db_name", db),
            ("format", "file"),
            ("sparql", sparql),
        ],
    );
    ok(&j);
    string(&j, "FileName")
}

/// `GET /file/download/<target>` without credentials.
fn download_anonymous(addr: &SocketAddr, target: &str) -> Reply {
    send(addr, "GET", &format!("/file/download/{target}"), &[], b"")
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

/// The `FileNotFound` envelope (`StatusCode` 11, HTTP 200).
fn assert_not_found(r: &Reply, what: &str) {
    assert_eq!(r.status, 200, "{what}: {}", r.text());
    assert_eq!(code(&r.json()), 11, "{what}: {}", r.text());
}

/// `multipart/form-data` for `/file/upload`.
fn multipart(user: &str, name: &str, data: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----gstoredownloadpolicy0x5f";
    let mut body = Vec::new();
    for (k, v) in [("username", user), ("password", password(user))] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// `/file/upload` of `data` as `name` by `user`: the envelope.
fn upload_as(addr: &SocketAddr, user: &str, name: &str, data: &[u8]) -> Json {
    let (ctype, body) = multipart(user, name, data);
    let r = send(
        addr,
        "POST",
        "/file/upload",
        &[("Content-Type", &ctype)],
        &body,
    );
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()
}

const SECRET_A: &str = "<http://ex/alice> <http://ex/ssn> \"123-45-6789\" .\n\
<http://ex/bob> <http://ex/ssn> \"987-65-4321\" .\n";
const DATA_B: &str = "<http://ex/x> <http://ex/p> <http://ex/y> .\n";

/// Databases `dba` (secret) and `dbb`; users `alice` (query + export +
/// update on `dba`), `bob` (query + export + update on `dbb` only), `carol`
/// (query on `dba`).
fn two_databases(srv: &Srv) {
    let a = &srv.addr;
    let pa = srv.data_file("a.nt", SECRET_A);
    let pb = srv.data_file("b.nt", DATA_B);
    build_loaded(a, "dba", &pa);
    build_loaded(a, "dbb", &pb);
    for u in ["alice", "bob", "carol"] {
        add_user(a, u);
    }
    grant(a, "alice", "dba", "query,export,update");
    grant(a, "bob", "dbb", "query,export,update");
    grant(a, "carol", "dba", "query");
}

fn count(addr: &SocketAddr, db: &str) -> i64 {
    let j = api(
        addr,
        &[
            ("operation", "query"),
            ("db_name", db),
            ("sparql", "SELECT * WHERE { ?s ?p ?o }"),
        ],
    );
    ok(&j);
    j.get("AnsNum").and_then(Json::as_i64).unwrap()
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// Authentication and owner checks
// ---------------------------------------------------------------------------

/// sec#4 / gapi#2: without credentials nothing is downloaded by default — an
/// export and a `format=file` result answer 401 and leak nothing.
#[test]
fn anonymous_downloads_are_refused_by_default() {
    let _g = serial();
    let root = TempRoot::new("anon");
    let srv = start_at(&root.0, |_| {});
    let a = &srv.addr;
    two_databases(&srv);
    let export = export_as(a, "root", "dba");
    let result = query_file_as(a, "root", "dba", "SELECT ?o WHERE { ?s ?p ?o }");
    let export_name = file_name(&export);
    for target in [export.clone(), export_name.clone(), result.clone()] {
        let r = download_anonymous(a, &target);
        assert_eq!(r.status, 401, "{target}: {}", r.text());
        assert!(!r.text().contains("123-45-6789"), "{target}: {}", r.text());
        assert_eq!(code(&r.json()), 1001, "{target}");
        assert!(
            r.header("WWW-Authenticate")
                .is_some_and(|h| h.starts_with("Basic")),
            "{}",
            r.head
        );
        // POST with only the file path: the same.
        let r = send(
            a,
            "POST",
            "/file/download",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            format!("filepath={target}").as_bytes(),
        );
        assert_eq!(r.status, 401, "{target}: {}", r.text());
    }
    // A file that does not exist answers the same (nothing is revealed).
    assert_eq!(download_anonymous(a, "nothing.nt").status, 401);
    // With credentials — parameters, or HTTP Basic — root downloads both.
    let r = download_as(a, "root", &export_name);
    assert_eq!(
        (r.status, r.text()),
        (200, fs::read_to_string(&export).unwrap())
    );
    let r = download_as(a, "root", &result);
    assert_eq!(r.status, 200);
    assert!(r.text().contains("123-45-6789"));
    // "root:root" in base64.
    let r = send(
        a,
        "GET",
        &format!("/file/download/{export_name}"),
        &[("Authorization", "Basic cm9vdDpyb290")],
        b"",
    );
    assert_eq!(r.status, 200, "{}", r.text());
    // "root:nope": refused like a wrong password parameter.
    let r = send(
        a,
        "GET",
        &format!("/file/download/{export_name}"),
        &[("Authorization", "Basic cm9vdDpub3Bl")],
        b"",
    );
    assert_eq!(code(&r.json()), 1001, "{}", r.text());
}

/// The upstream-compat switch restores upstream's public export directory
/// (and only that: uploads stay private).
#[test]
fn anonymous_download_switch_restores_the_public_export_directory() {
    let _g = serial();
    let root = TempRoot::new("compat");
    let srv = start_at(&root.0, |c| set(c, "anonymous_download", "on"));
    let a = &srv.addr;
    two_databases(&srv);
    let export = export_as(a, "root", "dba");
    let result = query_file_as(a, "root", "dba", "SELECT ?o WHERE { ?s ?p ?o }");
    let r = download_anonymous(a, &export);
    assert_eq!(
        (r.status, r.text()),
        (200, fs::read_to_string(&export).unwrap())
    );
    let r = download_anonymous(a, &result);
    assert_eq!(r.status, 200);
    assert!(r.text().contains("123-45-6789"));
    // bob (no rights on `a`) reads it too: the directory is public.
    assert_eq!(download_as(a, "bob", &result).status, 200);
    // An upload is not part of it.
    let j = upload_as(a, "alice", "mine.nt", DATA_B.as_bytes());
    ok(&j);
    let up = string(&j, "filepath");
    assert_not_found(&download_anonymous(a, &up), "anonymous upload");
    assert_not_found(&download_as(a, "bob", &up), "another user's upload");
}

/// sec#4: a user reads an export only with the `export` privilege on its
/// database, a query result only with `query`; owner records and operator
/// files are root's.
#[test]
fn downloads_are_checked_against_the_files_database() {
    let _g = serial();
    let root = TempRoot::new("owner");
    let srv = start_at(&root.0, |_| {});
    let a = &srv.addr;
    two_databases(&srv);
    let export = export_as(a, "root", "dba");
    let result = query_file_as(a, "root", "dba", "SELECT ?o WHERE { ?s ?p ?o }");
    // alice (query + export on a): both.
    assert_eq!(download_as(a, "alice", &export).status, 200);
    assert_eq!(download_as(a, "alice", &result).status, 200);
    // carol (query on a): the result, not the export.
    assert_eq!(download_as(a, "carol", &result).status, 200);
    assert_not_found(&download_as(a, "carol", &export), "carol's export");
    // bob (nothing on a): neither.
    assert_not_found(&download_as(a, "bob", &export), "bob, export of a");
    assert_not_found(&download_as(a, "bob", &result), "bob, result of a");
    // bob's own export of b, which alice may not read.
    let export_b = export_as(a, "bob", "dbb");
    assert_eq!(download_as(a, "bob", &export_b).status, 200);
    assert_not_found(&download_as(a, "alice", &export_b), "alice, export of b");
    // A file the operator put in the export directory: root's alone.
    fs::write(srv.export_dir().join("manual.nt"), DATA_B).unwrap();
    assert_not_found(&download_as(a, "alice", "manual.nt"), "operator file");
    assert_eq!(download_as(a, "root", "manual.nt").status, 200);
    // Owner records (under the root, file_owners/) are never served.
    let records: Vec<PathBuf> = fs::read_dir(srv.root.join("file_owners"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert!(!records.is_empty(), "exports carry owner records");
    for rec in &records {
        let rec = rec.to_string_lossy();
        assert_not_found(&download_as(a, "root", &rec), "owner record");
        assert_not_found(&download_as(a, "alice", &rec), "owner record");
    }
}

/// sec#4: an export directory file is not a `batchInsert` source for a user
/// without access to the database it came from.
#[test]
fn batch_insert_from_another_databases_export_is_refused() {
    let _g = serial();
    let root = TempRoot::new("batch");
    let srv = start_at(&root.0, |_| {});
    let a = &srv.addr;
    two_databases(&srv);
    let export = export_as(a, "root", "dba");
    let before = count(a, "dbb");
    // bob may update b, but not read a's export.
    let j = api_as(
        a,
        "bob",
        &[
            ("operation", "batchInsert"),
            ("db_name", "dbb"),
            ("file", &export),
        ],
    );
    assert_eq!(code(&j), 1003, "{j}");
    assert!(!j.to_string().contains("123-45-6789"), "{j}");
    // Nor through `dir` (every RDF file of the export directory).
    let dir = srv.export_dir().to_string_lossy().into_owned();
    let j = api_as(
        a,
        "bob",
        &[
            ("operation", "batchInsert"),
            ("db_name", "dbb"),
            ("dir", &dir),
        ],
    );
    assert_eq!(code(&j), 1003, "{j}");
    assert_eq!(count(a, "dbb"), before, "nothing of a reached b");
    // alice holds export on a: she may use it (into a, which she updates).
    let j = api_as(
        a,
        "alice",
        &[
            ("operation", "batchInsert"),
            ("db_name", "dba"),
            ("file", &export),
        ],
    );
    ok(&j);
    // root may use anything.
    ok(&api(
        a,
        &[
            ("operation", "batchInsert"),
            ("db_name", "dbb"),
            ("file", &export),
        ],
    ));
    assert_eq!(count(a, "dbb"), before + 2);
}

/// gapi#14: an upload is its uploader's (and root's) — to download and to
/// use as a source.
#[test]
fn uploads_are_private_to_their_uploader() {
    let _g = serial();
    let root = TempRoot::new("uploads");
    let srv = start_at(&root.0, |_| {});
    let a = &srv.addr;
    two_databases(&srv);
    let private = "<http://ex/s> <http://ex/p> \"alice-private-secret\" .\n";
    let j = upload_as(a, "alice", "private.nt", private.as_bytes());
    ok(&j);
    let path = string(&j, "filepath");
    let name = file_name(&path);
    // bob: neither by path nor by name, nor as a batchInsert source.
    assert_not_found(&download_as(a, "bob", &path), "bob, by path");
    assert_not_found(&download_as(a, "bob", &name), "bob, by name");
    let before = count(a, "dbb");
    let j = api_as(
        a,
        "bob",
        &[
            ("operation", "batchInsert"),
            ("db_name", "dbb"),
            ("file", &path),
        ],
    );
    assert_eq!(code(&j), 1003, "{j}");
    assert_eq!(count(a, "dbb"), before);
    // An operator's file in the upload directory is root's alone too.
    let operator = srv.data_file("operator.nt", DATA_B);
    assert_not_found(&download_as(a, "bob", &operator), "operator upload");
    // alice and root read it; alice inserts it into a.
    let r = download_as(a, "alice", &path);
    assert_eq!((r.status, r.text()), (200, private.to_string()));
    assert_eq!(download_as(a, "root", &name).status, 200);
    ok(&api_as(
        a,
        "alice",
        &[
            ("operation", "batchInsert"),
            ("db_name", "dba"),
            ("file", &path),
        ],
    ));
}

/// sec#14 / gapi#14: with `upload_user_quota`, each user's uploads are
/// capped.
#[test]
fn uploads_are_capped_per_user() {
    let _g = serial();
    let root = TempRoot::new("quota");
    let srv = start_at(&root.0, |c| set(c, "upload_user_quota", "100"));
    let a = &srv.addr;
    for u in ["alice", "bob"] {
        add_user(a, u);
    }
    let sixty = format!("<http://ex/s> <http://ex/p> \"{}\" .\n", "x".repeat(27));
    assert_eq!(sixty.len(), 60);
    ok(&upload_as(a, "alice", "one.nt", sixty.as_bytes()));
    let j = upload_as(a, "alice", "two.nt", sixty.as_bytes());
    assert_eq!(code(&j), 1005, "{j}");
    assert!(string(&j, "StatusMsg").contains("quota"), "{j}");
    // bob's quota is his own.
    ok(&upload_as(a, "bob", "one.nt", sixty.as_bytes()));
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// `<digits>` of exactly `n` decimal digits.
fn digits(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_digit())
}

fn hex128(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// sec#4 / gapi#2: export and result names carry 128 random bits.
#[test]
fn export_and_result_names_are_unguessable() {
    let _g = serial();
    let root = TempRoot::new("names");
    let srv = start_at(&root.0, |_| {});
    let a = &srv.addr;
    two_databases(&srv);
    let mut tags = Vec::new();
    for _ in 0..2 {
        // dba_<YYYYmmddHHMMSS>_<32 hex>.nt
        let name = file_name(&export_as(a, "root", "dba"));
        let stem = name.strip_suffix(".nt").expect(&name);
        let parts: Vec<&str> = stem.split('_').collect();
        assert_eq!(parts.len(), 3, "{name}");
        assert!(
            parts[0] == "dba" && digits(parts[1], 14) && hex128(parts[2]),
            "{name}"
        );
        tags.push(parts[2].to_string());
        // dba_<thread>_<YYYYmmddHHMMSS>_<32 hex>.json
        let name = query_file_as(a, "root", "dba", "SELECT ?s WHERE { ?s ?p ?o }");
        let stem = name.strip_suffix(".json").expect(&name);
        let parts: Vec<&str> = stem.split('_').collect();
        assert_eq!(parts.len(), 4, "{name}");
        assert!(
            parts[0] == "dba"
                && parts[1].bytes().all(|b| b.is_ascii_digit())
                && digits(parts[2], 14)
                && hex128(parts[3]),
            "{name}"
        );
        tags.push(parts[3].to_string());
    }
    tags.sort();
    tags.dedup();
    assert_eq!(tags.len(), 4, "every name has its own random part");
}

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

// ---------------------------------------------------------------------------
// format=file results
// ---------------------------------------------------------------------------

/// gapi#11: a `format=file` result is written row by row — the request's
/// peak heap stays near the result set itself, far below the JSON tree plus
/// text the buffered writer built (about 8x the result set, review gapi#11).
#[test]
fn format_file_results_are_written_without_a_result_tree() {
    let _g = serial();
    let root = TempRoot::new("ffile");
    let srv = start_at(&root.0, |c| set(c, "query_cache_size", "0"));
    let a = &srv.addr;
    let rows = 20_000;
    let mut nt = String::new();
    for i in 0..rows {
        nt.push_str(&format!(
            "<http://ex/s{i}> <http://ex/p> \"{} {i}\" .\n",
            "long literal value ".repeat(4)
        ));
    }
    let path = srv.data_file("big.nt", &nt);
    drop(nt);
    build_loaded(a, "big", &path);
    let sparql = "SELECT ?s ?p ?o WHERE { ?s ?p ?o }";
    // Warm up once (the first query compiles and caches what it can).
    query_file_as(a, "root", "big", sparql);
    let (name, peak) = peak_during(|| query_file_as(a, "root", "big", sparql));
    let file = srv.export_dir().join(&name);
    let size = fs::metadata(&file).unwrap().len() as usize;
    eprintln!("format=file result of {size} bytes ({rows} rows): peak heap {peak} bytes");
    let written = json::parse(&fs::read_to_string(&file).unwrap()).unwrap();
    let bindings = written
        .get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Json::as_array)
        .unwrap();
    assert_eq!(bindings.len(), rows);
    assert!(
        peak < 3 * size,
        "a {size}-byte format=file result took {peak} bytes of heap at its peak"
    );
}

// ---------------------------------------------------------------------------
// Retention, orphaned task results
// ---------------------------------------------------------------------------

fn backdate(path: &Path, by: Duration) {
    let f = File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - by).unwrap();
}

/// The names in `dir` that contain `needle` (files and their owner records).
fn entries_with(dir: &Path, needle: &str) -> Vec<String> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.contains(needle))
                .collect()
        })
        .unwrap_or_default()
}

/// Poll `checkOperationState` until the operation has ended.
fn wait_done(addr: &SocketAddr, opt_id: &str) {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let j = api(
            addr,
            &[("operation", "checkOperationState"), ("opt_id", opt_id)],
        );
        ok(&j);
        if j.get("state").and_then(Json::as_i64) != Some(0) {
            return;
        }
        assert!(Instant::now() < until, "operation {opt_id} never finished");
        thread::yield_now();
    }
}

/// sec#14 / gapi#13: expired uploads, exports and `format=file` results are
/// removed (operator files are not), and so are the spilled task results a
/// previous run of the server left behind.
#[test]
fn expired_files_and_orphaned_task_results_are_removed() {
    let _g = serial();
    let root = TempRoot::new("retention");
    // The first run writes an export, a result, an upload and an async
    // query result, then stops.
    let (export, result, upload, opt_id) = {
        let srv = start_at(&root.0, |_| {});
        let a = &srv.addr;
        two_databases(&srv);
        let export = file_name(&export_as(a, "root", "dba"));
        let result = query_file_as(a, "root", "dba", "SELECT ?o WHERE { ?s ?p ?o }");
        let j = upload_as(a, "alice", "old.nt", DATA_B.as_bytes());
        ok(&j);
        let upload = file_name(&string(&j, "filepath"));
        let j = api(
            a,
            &[
                ("operation", "query"),
                ("db_name", "dba"),
                ("sparql", "SELECT ?o WHERE { ?s ?p ?o }"),
                ("async", "true"),
            ],
        );
        ok(&j);
        let opt_id = string(&j, "opt_id");
        wait_done(a, &opt_id);
        (export, result, upload, opt_id)
    };
    let tasks = root.0.join("tasks");
    let spilled = tasks.join(format!("{opt_id}.json"));
    assert!(spilled.is_file(), "the async result was spilled");
    let export_dir = root.0.join("export");
    let upload_dir = root.0.join("upload");
    let day = Duration::from_secs(24 * 3600);
    backdate(&export_dir.join(&export), day);
    backdate(&export_dir.join(&result), day);
    backdate(&upload_dir.join(&upload), day);
    // An old file the operator put there: never the sweep's.
    fs::write(export_dir.join("operator.nt"), DATA_B).unwrap();
    backdate(&export_dir.join("operator.nt"), day);
    backdate(&upload_dir.join("a.nt"), day);
    // The next run keeps files for an hour; its first file operation sweeps.
    let srv = start_at(&root.0, |c| set(c, "file_retention_hours", "1"));
    ok(&api(
        &srv.addr,
        &[("operation", "load"), ("db_name", "dba")],
    ));
    let fresh = file_name(&export_as(&srv.addr, "root", "dba"));
    assert!(
        entries_with(&export_dir, &export).is_empty(),
        "expired export (and record)"
    );
    assert!(
        entries_with(&export_dir, &result).is_empty(),
        "expired result (and record)"
    );
    assert!(
        entries_with(&upload_dir, &upload).is_empty(),
        "expired upload (and record)"
    );
    assert!(export_dir.join(&fresh).is_file(), "a fresh export stays");
    assert!(
        export_dir.join("operator.nt").is_file(),
        "operator files stay"
    );
    assert!(upload_dir.join("a.nt").is_file(), "operator files stay");
    assert!(
        !spilled.exists(),
        "the previous run's task result is removed"
    );
    // The records of the removed files went with them: the fresh export's
    // is the only one left.
    let recorded: Vec<String> = fs::read_dir(root.0.join("file_owners"))
        .unwrap()
        .flatten()
        .map(|e| {
            let j = json::parse(&fs::read_to_string(e.path()).unwrap()).unwrap();
            string(&j, "path")
        })
        .collect();
    let fresh_path = export_dir.join(&fresh).to_string_lossy().into_owned();
    assert_eq!(recorded, [fresh_path]);
    // This run's own async results stay.
    let j = api(
        &srv.addr,
        &[
            ("operation", "query"),
            ("db_name", "dba"),
            ("sparql", "SELECT ?o WHERE { ?s ?p ?o }"),
            ("async", "true"),
        ],
    );
    ok(&j);
    let mine = string(&j, "opt_id");
    wait_done(&srv.addr, &mine);
    assert!(tasks.join(format!("{mine}.json")).is_file());
    let j = api(
        &srv.addr,
        &[("operation", "checkOperationState"), ("opt_id", &mine)],
    );
    ok(&j);
}
