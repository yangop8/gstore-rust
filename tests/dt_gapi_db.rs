//! Data test: the database lifecycle operations of the upstream-compatible
//! `/api` protocol (`show`, `load`, `loadcsr`, `unload`, `monitor`, `build`,
//! `drop`, `rename`, `backup`, `backuppath`, `restore`, `export`,
//! `batchInsert`, `batchRemove`, `schema`), the file routes (`/file/upload`,
//! `/file/download[/*]`), the confinement of every server-side path, and the
//! inter-process lock on database directories.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::server::HttpOptions;
use gstore::LoadPolicy;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("gstore_gdb_{tag}_{}_{n}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    fs::canonicalize(&d).unwrap()
}

struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    root: PathBuf,
}

impl Srv {
    fn upload_dir(&self) -> PathBuf {
        self.root.join("upload")
    }
    fn backup_dir(&self) -> PathBuf {
        self.root.join("backups")
    }
    fn db_dir(&self, db: &str) -> PathBuf {
        self.root.join(format!("{db}.db"))
    }
    /// Put an RDF file in the upload directory; its absolute path.
    fn data_file(&self, name: &str, content: &str) -> String {
        let dir = self.upload_dir();
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p.to_string_lossy().into_owned()
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
    }
}

fn serve(server: ApiServer, root: PathBuf) -> Srv {
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || {
        s2.serve_forever();
    });
    Srv { server, addr, root }
}

fn start(tag: &str) -> Srv {
    start_with(tag, |c| c, |s| s)
}

/// A server with a tweaked configuration (`cfg`) and builder (`build`).
fn start_with(
    tag: &str,
    cfg: impl FnOnce(GStoreConfig) -> GStoreConfig,
    build: impl FnOnce(ApiServer) -> ApiServer,
) -> Srv {
    let root = tmp_root(tag);
    let mut config = GStoreConfig::default();
    // No IP deny list from the working directory.
    config.ip_deny_path = None;
    let config = cfg(config);
    let server = ApiServer::bind(&root, "root", "127.0.0.1:0")
        .unwrap()
        .with_config(config);
    serve(build(server), root)
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

fn send(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
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
    s.write_all(&bytes).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let split = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header end");
    let head = String::from_utf8_lossy(&resp[..split]).into_owned();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status");
    Reply {
        status,
        head,
        body: resp[split + 4..].to_vec(),
    }
}

/// `POST /api` with a JSON body; the answer must be HTTP 200 JSON.
fn api_json(addr: &SocketAddr, body: &Json) -> Json {
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

/// `POST /api` as `user` (password `pw`, `root` for root) with `pairs`.
fn api_as(addr: &SocketAddr, user: &str, pairs: &[(&str, &str)]) -> Json {
    let pass = if user == "root" { "root" } else { "pw" };
    let mut body = Json::obj().with("username", user).with("password", pass);
    for (k, v) in pairs {
        body.set(*k, *v);
    }
    api_json(addr, &body)
}

fn api(addr: &SocketAddr, pairs: &[(&str, &str)]) -> Json {
    api_as(addr, "root", pairs)
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no StatusCode in {j}"))
}

fn msg(j: &Json) -> String {
    j.get("StatusMsg")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

fn int(j: &Json, key: &str) -> i64 {
    j.get(key)
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no integer {key} in {j}"))
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

/// The number of answers of `SELECT * {?s ?p ?o}` on loaded `db`.
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
    int(&j, "AnsNum")
}

fn build(addr: &SocketAddr, db: &str, path: &str) -> Json {
    let j = api(
        addr,
        &[("operation", "build"), ("db_name", db), ("db_path", path)],
    );
    ok(&j);
    j
}

fn load(addr: &SocketAddr, db: &str) {
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

/// Poll `checkOperationState` until the operation has ended.
fn wait_done(addr: &SocketAddr, opt_id: &str) -> Json {
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let j = api(
            addr,
            &[("operation", "checkOperationState"), ("opt_id", opt_id)],
        );
        ok(&j);
        if int(&j, "state") != 0 {
            return j;
        }
        assert!(Instant::now() < until, "operation {opt_id} never finished");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Accept one HTTP request on `listener` (a task callback) and hand over
/// its body.
fn capture_one(listener: TcpListener) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let body = loop {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "callback connection closed early");
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw).into_owned();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                if body.len() >= len {
                    break body.to_string();
                }
            }
        };
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        tx.send(body).unwrap();
    });
    rx
}

/// A `multipart/form-data` body: text fields and one file part `file`.
fn multipart(fields: &[(&str, &str)], file: Option<(&str, &[u8])>) -> (String, Vec<u8>) {
    let boundary = "----gstoretestboundary7MA4YWxk";
    let mut body = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    if let Some((name, data)) = file {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
                 filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn upload(addr: &SocketAddr, user: &str, name: &str, data: &[u8]) -> Json {
    let pass = if user == "root" { "root" } else { "pw" };
    let (ctype, body) = multipart(
        &[("username", user), ("password", pass)],
        Some((name, data)),
    );
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

const LUBM: &str = r#"<http://ex/a> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/onto#Person> .
<http://ex/b> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/onto#Person> .
<http://ex/u> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/onto#Univ> .
<http://ex/a> <http://ex/onto#name> "Alice" .
<http://ex/a> <http://ex/onto#worksAt> <http://ex/u> .
"#;

const MORE: &str = r#"<http://ex/c> <http://ex/onto#name> "Carol" .
<http://ex/d> <http://ex/onto#name> "Dave" .
"#;

// ---------------------------------------------------------------------------
// build / show / load / monitor / schema / unload
// ---------------------------------------------------------------------------

#[test]
fn build_show_load_monitor_schema_unload() {
    let srv = start("lifecycle");
    let a = &srv.addr;
    let lubm = srv.data_file("lubm.nt", LUBM);
    let more = srv.data_file("more.nt", MORE);

    let j = build(a, "lubm", &lubm);
    assert_eq!(msg(&j), "Import RDF file to database done.");
    assert_eq!(int(&j, "success_num"), 5);
    assert_eq!(int(&j, "failed_num"), 0);
    assert!(srv.db_dir("lubm").is_dir());
    // Upstream: an existing database is never rebuilt.
    let j = api(
        a,
        &[
            ("operation", "build"),
            ("db_name", "lubm"),
            ("db_path", &lubm),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1004, "database already built.")
    );
    // db_path as an array (the SDKs send one), and no db_path at all.
    let j = api_json(
        a,
        &Json::obj()
            .with("operation", "build")
            .with("username", "root")
            .with("password", "root")
            .with("db_name", "two")
            .with(
                "db_path",
                Json::Arr(vec![Json::from(lubm.as_str()), Json::from(more.as_str())]),
            ),
    );
    ok(&j);
    assert_eq!(int(&j, "success_num"), 7);
    let j = api(a, &[("operation", "build"), ("db_name", "empty")]);
    ok(&j);
    assert_eq!(int(&j, "success_num"), 0);

    // show: every database, built by root, not loaded.
    let j = api(a, &[("operation", "show")]);
    ok(&j);
    assert_eq!(msg(&j), "Get the database list successfully!");
    let list = j
        .get("ResponseBody")
        .and_then(Json::as_array)
        .unwrap()
        .to_vec();
    let names: Vec<String> = list.iter().map(|d| string(d, "database")).collect();
    assert_eq!(names, ["empty", "lubm", "two"]);
    for d in &list {
        assert_eq!(string(d, "creator"), "root");
        assert_eq!(string(d, "status"), "already_built");
        assert_eq!(string(d, "built_time").len(), 19, "{d}");
        assert!(d.get("costTime").is_some(), "{d}");
    }
    let j = api(a, &[("operation", "show"), ("db_name", "two")]);
    assert_eq!(
        j.get("ResponseBody")
            .and_then(Json::as_array)
            .unwrap()
            .len(),
        1
    );

    // load (csr echoed), twice.
    let j = api(a, &[("operation", "load"), ("db_name", "lubm")]);
    ok(&j);
    assert_eq!(msg(&j), "Database loaded successfully.");
    assert_eq!(string(&j, "csr"), "0");
    let j = api(
        a,
        &[("operation", "load"), ("db_name", "lubm"), ("csr", "1")],
    );
    assert_eq!(msg(&j), "The database already load yet.");
    assert_eq!(string(&j, "csr"), "1");
    let j = api(a, &[("operation", "load"), ("db_name", "nosuch")]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1005, "database[nosuch] does not exist.")
    );
    assert_eq!(count(a, "lubm"), 5);
    let j = api(a, &[("operation", "show"), ("db_name", "lubm")]);
    let d = &j.get("ResponseBody").and_then(Json::as_array).unwrap()[0];
    assert_eq!(string(d, "status"), "loaded");

    // loadcsr: loaded → success; not loaded → 1005.
    let j = api(a, &[("operation", "loadcsr"), ("db_name", "lubm")]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (0, "CSR loaded successfully.")
    );
    let j = api(a, &[("operation", "loadcsr"), ("db_name", "two")]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1005, "database[two] is not loaded yet")
    );

    // monitor: upstream's fields, for this database only.
    let j = api(a, &[("operation", "monitor"), ("db_name", "lubm")]);
    ok(&j);
    assert_eq!(string(&j, "database"), "lubm");
    assert_eq!(string(&j, "creator"), "root");
    assert_eq!(string(&j, "tripleNum"), "5");
    assert_eq!(int(&j, "subjectNum"), 3);
    assert_eq!(int(&j, "predicateNum"), 3);
    assert_eq!(int(&j, "literalNum"), 1);
    assert!(int(&j, "entityNum") >= 5);
    assert!(int(&j, "connectionNum") > 0);
    assert_eq!(string(&j, "status"), "loaded");
    for key in ["builtTime", "diskUsed", "lockNum", "costTime"] {
        assert!(j.get(key).is_some(), "{key} in {j}");
    }
    let subjects = j.get("subjectList").and_then(Json::as_array).unwrap();
    assert_eq!(subjects.len(), 2);
    assert_eq!(string(&subjects[0], "name"), "http://ex/onto#Person");
    assert_eq!(int(&subjects[0], "value"), 2);
    assert!(j.get("schema").and_then(|s| s.get("nodes")).is_some());
    // The statistics follow writes (they are cached until the data changes).
    let j = api(
        a,
        &[
            ("operation", "query"),
            ("db_name", "lubm"),
            (
                "sparql",
                "INSERT DATA { <http://ex/z> \
                 <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/onto#Person> }",
            ),
        ],
    );
    ok(&j);
    let j = api(a, &[("operation", "monitor"), ("db_name", "lubm")]);
    let subjects = j.get("subjectList").and_then(Json::as_array).unwrap();
    assert_eq!(int(&subjects[0], "value"), 3, "{j}");
    assert_eq!(string(&j, "tripleNum"), "6");
    // Like upstream, an unloaded database is reported too (from disk).
    let j = api(a, &[("operation", "monitor"), ("db_name", "two")]);
    ok(&j);
    assert_eq!(string(&j, "tripleNum"), "7");
    assert_eq!(string(&j, "status"), "already_built");
    assert_eq!(int(&j, "subjectNum"), 5);
    let j = api(a, &[("operation", "schema"), ("db_name", "two")]);
    ok(&j);
    let j = api(a, &[("operation", "monitor"), ("db_name", "nosuch")]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1005, "database[nosuch] does not exist")
    );

    // schema: classes, literal properties, relations (upstream's shape).
    let j = api(a, &[("operation", "schema"), ("db_name", "lubm")]);
    ok(&j);
    let body = j.get("ResponseBody").unwrap();
    let nodes = body.get("nodes").and_then(Json::as_array).unwrap();
    let labels: Vec<(String, String, i64)> = nodes
        .iter()
        .map(|n| (string(n, "id"), string(n, "label"), int(n, "type")))
        .collect();
    assert_eq!(
        labels,
        [
            ("1".to_string(), "Person".to_string(), 0),
            ("3".to_string(), "name".to_string(), 1),
            ("2".to_string(), "Univ".to_string(), 0),
        ]
    );
    let edges = body.get("edges").and_then(Json::as_array).unwrap();
    assert_eq!(edges.len(), 2);
    assert_eq!(string(&edges[1], "label"), "worksAt");
    assert_eq!(
        (string(&edges[1], "source"), string(&edges[1], "target")),
        ("1".to_string(), "2".to_string())
    );

    // unload saves and stops serving.
    let j = api(a, &[("operation", "unload"), ("db_name", "lubm")]);
    assert_eq!((code(&j), msg(&j).as_str()), (0, "Database unloaded."));
    let j = api(a, &[("operation", "unload"), ("db_name", "lubm")]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1005, "database[lubm] is not loaded yet")
    );
    let j = api(a, &[("operation", "unload"), ("db_name", "nosuch")]);
    assert_eq!(code(&j), 1005);
}

#[test]
fn build_counts_parse_failures_and_refuses_bad_sources() {
    let srv = start("buildbad");
    let a = &srv.addr;
    let mixed = srv.data_file(
        "mixed.nt",
        "<http://a> <http://p> <http://b> .\nnot rdf at all\n\"lit\" <http://p> <http://b> .\n<http://a> <http://p> \"x\" .\n",
    );
    let j = build(a, "mixed", &mixed);
    assert_eq!((int(&j, "success_num"), int(&j, "failed_num")), (2, 2));

    // Missing (inside the roots), zip, a suffix outside the whitelist.
    let gone = srv.upload_dir().join("gone.nt");
    let j = api(
        a,
        &[
            ("operation", "build"),
            ("db_name", "x1x"),
            ("db_path", gone.to_str().unwrap()),
        ],
    );
    assert_eq!(code(&j), 1005);
    assert_eq!(msg(&j), format!("RDF file '{}' not exist.", gone.display()));
    let zip = srv.data_file("data.zip", "PK");
    let j = api(
        a,
        &[
            ("operation", "build"),
            ("db_name", "x1x"),
            ("db_path", &zip),
        ],
    );
    if cfg!(feature = "zip") {
        // Not a zip: upstream's `ZipCode` 1 (the package cannot be opened).
        assert_eq!(code(&j), 1, "{j}");
        assert!(msg(&j).starts_with("zip format or data is error."), "{j}");
    } else {
        assert_eq!(code(&j), 1003);
        assert!(msg(&j).contains("`zip` feature"), "{j}");
    }
    let csv = srv.data_file("data.csv", "a,b");
    let j = api(
        a,
        &[
            ("operation", "build"),
            ("db_name", "x1x"),
            ("db_path", &csv),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1003, "The file suffix 'csv' is not allowed.")
    );

    // Path traversal and paths outside the roots are refused, whether or not
    // they exist, and nothing is built.
    let outside = srv.root.join("outside.nt");
    fs::write(&outside, LUBM).unwrap();
    let up = srv.upload_dir();
    let attempts = [
        format!("{}/../outside.nt", up.display()),
        "../outside.nt".to_string(),
        outside.to_string_lossy().into_owned(),
        srv.root.join("nothing.nt").to_string_lossy().into_owned(),
        "/etc/hostname.nt".to_string(),
    ];
    for bad in &attempts {
        let j = api(
            a,
            &[("operation", "build"), ("db_name", "x1x"), ("db_path", bad)],
        );
        assert_eq!(code(&j), 1003, "{bad}: {j}");
        assert!(msg(&j).contains("refused"), "{bad}: {j}");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, up.join("link.nt")).unwrap();
        let j = api(
            a,
            &[
                ("operation", "build"),
                ("db_name", "x1x"),
                ("db_path", up.join("link.nt").to_str().unwrap()),
            ],
        );
        assert_eq!(code(&j), 1003, "a symlink out of the upload dir: {j}");
    }
    assert!(!srv.db_dir("x1x").exists());
    // Invalid names.
    let j = api(a, &[("operation", "build"), ("db_name", "system")]);
    assert_eq!(code(&j), 1003);
    let j = api(a, &[("operation", "build"), ("db_name", "../x")]);
    assert_eq!(code(&j), 1003);
}

#[test]
fn a_load_policy_directory_is_a_source_root() {
    let data = tmp_root("loaddir_data");
    fs::write(data.join("lubm.nt"), LUBM).unwrap();
    let dir = data.clone();
    let srv = start_with(
        "loaddir",
        |c| c,
        move |s| s.with_load_policy(LoadPolicy::restricted().with_allowed_dir(dir)),
    );
    let j = build(&srv.addr, "lubm", data.join("lubm.nt").to_str().unwrap());
    assert_eq!(int(&j, "success_num"), 5);
    let _ = fs::remove_dir_all(&data);
}

#[test]
fn max_database_num_is_enforced() {
    let srv = start_with(
        "maxdb",
        |mut c| {
            c.set("max_database_num", "2").unwrap();
            c
        },
        |s| s,
    );
    let a = &srv.addr;
    ok(&api(a, &[("operation", "build"), ("db_name", "one")]));
    ok(&api(a, &[("operation", "build"), ("db_name", "two")]));
    let j = api(a, &[("operation", "build"), ("db_name", "three")]);
    assert_eq!(code(&j), 1004);
    assert_eq!(
        msg(&j),
        "The total number of databases more than max_databse_num."
    );
    // The REST endpoints that create a database count the same way; an
    // existing one may still be rebuilt / uploaded into.
    let auth = [("Authorization", "Basic cm9vdDpyb290")]; // root:root
    for target in ["/build?db_name=three", "/upload?db_name=three"] {
        let r = send(a, "POST", target, &auth, LUBM.as_bytes());
        assert_eq!(r.status, 409, "{target}: {}", r.text());
        assert!(r.text().contains("max_database_num"), "{}", r.text());
        assert!(!srv.db_dir("three").exists(), "{target}");
    }
    for target in ["/build?db_name=one", "/upload?db_name=two"] {
        let r = send(a, "POST", target, &auth, LUBM.as_bytes());
        assert_eq!(r.status, 200, "{target}: {}", r.text());
    }
}

// ---------------------------------------------------------------------------
// privileges
// ---------------------------------------------------------------------------

#[test]
fn privileges_gate_every_operation() {
    let srv = start("privs");
    let a = &srv.addr;
    let lubm = srv.data_file("lubm.nt", LUBM);
    build(a, "lubm", &lubm);
    build(a, "other", &lubm);
    add_user(a, "bob");
    let denied = |j: &Json, op: &str| {
        assert_eq!(code(j), 1004, "{op}: {j}");
        assert_eq!(msg(j), format!("You have no {op} privilege"));
    };
    // Root-only operations.
    for (op, extra) in [
        ("build", vec![("db_name", "bobs")]),
        ("drop", vec![("db_name", "lubm")]),
        ("rename", vec![("db_name", "lubm"), ("new_name", "lubm2")]),
        ("backuppath", vec![("db_name", "lubm")]),
        ("schema", vec![("db_name", "lubm")]),
        ("loadcsr", vec![("db_name", "lubm")]),
    ] {
        let mut pairs = vec![("operation", op)];
        pairs.extend(extra);
        denied(&api_as(a, "bob", &pairs), op);
    }
    // Per-database privileges.
    for op in [
        "load",
        "unload",
        "monitor",
        "backup",
        "restore",
        "export",
        "batchInsert",
        "batchRemove",
    ] {
        denied(
            &api_as(a, "bob", &[("operation", op), ("db_name", "lubm")]),
            op,
        );
    }
    // show lists only what bob may query.
    let j = api_as(a, "bob", &[("operation", "show")]);
    ok(&j);
    assert!(j
        .get("ResponseBody")
        .and_then(Json::as_array)
        .unwrap()
        .is_empty());
    grant(a, "bob", "lubm", "query,load,unload");
    let j = api_as(a, "bob", &[("operation", "show")]);
    let list = j.get("ResponseBody").and_then(Json::as_array).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(string(&list[0], "database"), "lubm");
    ok(&api_as(
        a,
        "bob",
        &[("operation", "load"), ("db_name", "lubm")],
    ));
    ok(&api_as(
        a,
        "bob",
        &[("operation", "monitor"), ("db_name", "lubm")],
    ));
    denied(
        &api_as(a, "bob", &[("operation", "load"), ("db_name", "other")]),
        "load",
    );
    denied(
        &api_as(a, "bob", &[("operation", "backup"), ("db_name", "lubm")]),
        "backup",
    );
    ok(&api_as(
        a,
        "bob",
        &[("operation", "unload"), ("db_name", "lubm")],
    ));
    // Wrong credentials.
    let j = api_json(
        a,
        &Json::obj()
            .with("operation", "show")
            .with("username", "bob")
            .with("password", "nope"),
    );
    assert_eq!(code(&j), 1001);
}

// ---------------------------------------------------------------------------
// drop / rename
// ---------------------------------------------------------------------------

#[test]
fn drop_keeps_a_bak_by_default_and_deletes_with_is_backup_false() {
    let srv = start("drop");
    let a = &srv.addr;
    let lubm = srv.data_file("lubm.nt", LUBM);
    build(a, "keep", &lubm);
    build(a, "gone", &lubm);
    load(a, "keep");
    let j = api(a, &[("operation", "drop"), ("db_name", "keep")]);
    assert_eq!((code(&j), msg(&j).as_str()), (0, "Database keep dropped."));
    assert!(!srv.db_dir("keep").exists());
    assert!(srv.root.join("keep.db.bak").is_dir(), "logical deletion");
    let j = api(
        a,
        &[
            ("operation", "drop"),
            ("db_name", "gone"),
            ("is_backup", "false"),
        ],
    );
    ok(&j);
    assert!(!srv.db_dir("gone").exists());
    assert!(!srv.root.join("gone.db.bak").exists());
    let j = api(a, &[("operation", "show")]);
    assert!(
        j.get("ResponseBody")
            .and_then(Json::as_array)
            .unwrap()
            .is_empty(),
        "{j}"
    );
    let j = api(a, &[("operation", "drop"), ("db_name", "keep")]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1005, "database[keep] does not exist")
    );
    // A second logical deletion of a rebuilt name does not clobber the first.
    build(a, "keep", &lubm);
    ok(&api(a, &[("operation", "drop"), ("db_name", "keep")]));
    assert!(srv.root.join("keep.db.bak.1").is_dir());
    // A query on a dropped database.
    let j = api(a, &[("operation", "load"), ("db_name", "keep")]);
    assert_eq!(code(&j), 1005);
}

#[test]
fn rename_moves_the_database_and_its_grants() {
    let srv = start("rename");
    let a = &srv.addr;
    let lubm = srv.data_file("lubm.nt", LUBM);
    build(a, "lubm", &lubm);
    build(a, "taken", &lubm);
    add_user(a, "bob");
    grant(a, "bob", "lubm", "query,load");
    load(a, "lubm");
    let rename = |from: &str, to: &str| {
        api(
            a,
            &[("operation", "rename"), ("db_name", from), ("new_name", to)],
        )
    };
    let j = rename("lubm", "newlubm");
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1005, "Database is loaded, need unload first.")
    );
    ok(&api(a, &[("operation", "unload"), ("db_name", "lubm")]));
    let j = rename("lubm", "taken");
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1005, "Database name taken already exists.")
    );
    let j = rename("lubm", "a/b");
    assert_eq!(code(&j), 1003);
    let j = rename("nosuch", "fresh");
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1005, "Database[nosuch] does not exist")
    );
    let j = rename("lubm", "newlubm");
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (0, "Database rename successfully.")
    );
    assert!(!srv.db_dir("lubm").exists());
    assert!(srv.db_dir("newlubm").is_dir());
    // The grants followed the database.
    ok(&api_as(
        a,
        "bob",
        &[("operation", "load"), ("db_name", "newlubm")],
    ));
    assert_eq!(count(a, "newlubm"), 5);
    let j = api(a, &[("operation", "show"), ("db_name", "newlubm")]);
    assert_eq!(
        j.get("ResponseBody")
            .and_then(Json::as_array)
            .unwrap()
            .len(),
        1
    );
}

// ---------------------------------------------------------------------------
// backup / backuppath / restore
// ---------------------------------------------------------------------------

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        if e.path().is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()));
        } else {
            fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

#[test]
fn backups_rotate_list_and_restore_the_newest() {
    let srv = start_with(
        "backup",
        |mut c| {
            c.set("max_backups", "2").unwrap();
            c
        },
        |s| s,
    );
    let a = &srv.addr;
    let lubm = srv.data_file("lubm.nt", LUBM);
    let more = srv.data_file("more.nt", MORE);
    build(a, "lubm", &lubm);
    // Back up an unloaded database.
    let j = api(a, &[("operation", "backup"), ("db_name", "lubm")]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (0, "Database backup successfully.")
    );
    let first = PathBuf::from(string(&j, "backupfilepath"));
    let name = first.file_name().unwrap().to_string_lossy().into_owned();
    assert!(first.starts_with(srv.backup_dir()), "{}", first.display());
    let ts = name.strip_prefix("lubm.db_").expect("upstream name");
    assert!(
        ts.len() == 14 && ts.bytes().all(|b| b.is_ascii_digit()),
        "{name}"
    );
    // Two older backups of the same database (copies with earlier stamps).
    for old in ["lubm.db_20000101000000", "lubm.db_20000102000000"] {
        copy_dir(&first, &srv.backup_dir().join(old));
    }
    // Back up the loaded database after a change: the oldest go.
    load(a, "lubm");
    ok(&api(
        a,
        &[
            ("operation", "batchInsert"),
            ("db_name", "lubm"),
            ("file", &more),
        ],
    ));
    assert_eq!(count(a, "lubm"), 7);
    thread::sleep(Duration::from_millis(1100));
    let j = api(a, &[("operation", "backup"), ("db_name", "lubm")]);
    ok(&j);
    let newest = PathBuf::from(string(&j, "backupfilepath"));
    let j = api(a, &[("operation", "backuppath"), ("db_name", "lubm")]);
    ok(&j);
    let paths: Vec<String> = j
        .get("paths")
        .and_then(Json::as_array)
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        paths,
        [
            first.to_string_lossy().into_owned(),
            newest.to_string_lossy().into_owned()
        ],
        "max_backups=2 keeps the two newest"
    );
    assert!(!srv.backup_dir().join("lubm.db_20000101000000").exists());

    // Restore a given backup (5 triples), then the newest (7) by default.
    ok(&api(
        a,
        &[
            ("operation", "batchRemove"),
            ("db_name", "lubm"),
            ("file", &lubm),
        ],
    ));
    assert_eq!(count(a, "lubm"), 2);
    let j = api(
        a,
        &[
            ("operation", "restore"),
            ("db_name", "lubm"),
            ("backup_path", first.to_str().unwrap()),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (0, "Database restore successfully.")
    );
    assert_eq!(count(a, "lubm"), 5, "the loaded database was swapped");
    let j = api(
        a,
        &[
            ("operation", "restore"),
            ("db_name", "lubm"),
            ("backup_path", ""),
        ],
    );
    ok(&j);
    assert_eq!(count(a, "lubm"), 7, "the newest backup");
    // Restore under a new name creates (but does not load) the database.
    ok(&api(
        a,
        &[
            ("operation", "restore"),
            ("db_name", "copy"),
            ("backup_path", &name),
        ],
    ));
    load(a, "copy");
    assert_eq!(count(a, "copy"), 5);
    let j = api(a, &[("operation", "show"), ("db_name", "copy")]);
    let rec = &j.get("ResponseBody").and_then(Json::as_array).unwrap()[0];
    assert_eq!(
        string(rec, "built_time"),
        format!(
            "{}-{}-{} {}:{}:{}",
            &ts[0..4],
            &ts[4..6],
            &ts[6..8],
            &ts[8..10],
            &ts[10..12],
            &ts[12..14]
        )
    );

    // Refused sources and targets.
    let outside = srv.root.join("lubm.db");
    for (bad, want) in [
        (format!("{}/../lubm.db", srv.backup_dir().display()), 1003),
        (outside.to_string_lossy().into_owned(), 1003),
        (
            srv.backup_dir()
                .join("lubm.db_19990101000000")
                .to_string_lossy()
                .into_owned(),
            1003,
        ),
    ] {
        let j = api(
            a,
            &[
                ("operation", "restore"),
                ("db_name", "lubm"),
                ("backup_path", &bad),
            ],
        );
        assert_eq!(code(&j), want, "{bad}: {j}");
    }
    let j = api(
        a,
        &[
            ("operation", "restore"),
            ("db_name", "lubm"),
            (
                "backup_path",
                srv.backup_dir().join("nope").to_str().unwrap(),
            ),
        ],
    );
    assert_eq!(msg(&j), "Backup path not exist.");
    let j = api(a, &[("operation", "restore"), ("db_name", "nobackups")]);
    assert_eq!(code(&j), 1003);
    let j = api(
        a,
        &[
            ("operation", "backup"),
            ("db_name", "lubm"),
            ("backup_path", "/tmp"),
        ],
    );
    assert_eq!(code(&j), 1003, "{j}");
    let j = api(
        a,
        &[
            ("operation", "backup"),
            ("db_name", "lubm"),
            ("backup_path", "../x"),
        ],
    );
    assert_eq!(code(&j), 1003, "{j}");
    let j = api(
        a,
        &[
            ("operation", "backup"),
            ("db_name", "lubm"),
            ("compress", "true"),
        ],
    );
    if cfg!(feature = "zip") {
        // A zipped backup (tests/dt_zip_io.rs restores one).
        ok(&j);
        assert!(string(&j, "backupfilepath").ends_with(".zip"), "{j}");
    } else {
        assert_eq!(code(&j), 1003);
        assert!(msg(&j).contains("`zip` feature"), "{j}");
    }
    let j = api(a, &[("operation", "backup"), ("db_name", "nosuch")]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1004, "Database[nosuch] does not exist.")
    );
    // The SDKs' default backup_path (`./backups`, upstream's default
    // directory) names the backup directory itself.
    let j = api(
        a,
        &[
            ("operation", "backup"),
            ("db_name", "lubm"),
            ("backup_path", "./backups"),
        ],
    );
    ok(&j);
    let p = PathBuf::from(string(&j, "backupfilepath"));
    assert_eq!(p.parent().unwrap(), srv.backup_dir(), "{}", p.display());
    // A sub-directory of the backup directory is a valid backup_path.
    let j = api(
        a,
        &[
            ("operation", "backup"),
            ("db_name", "lubm"),
            ("backup_path", "weekly"),
        ],
    );
    ok(&j);
    assert!(srv.backup_dir().join("weekly").is_dir());
}

// ---------------------------------------------------------------------------
// export / download / upload
// ---------------------------------------------------------------------------

#[test]
fn export_writes_a_file_that_downloads_without_credentials() {
    let srv = start("export");
    let a = &srv.addr;
    let lubm = srv.data_file("lubm.nt", LUBM);
    build(a, "lubm", &lubm);
    let j = api(a, &[("operation", "export"), ("db_name", "lubm")]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1005, "database[lubm] is not loaded yet")
    );
    load(a, "lubm");
    let j = api(a, &[("operation", "export"), ("db_name", "lubm")]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (0, "Export the database successfully.")
    );
    let filepath = string(&j, "filepath");
    let file = PathBuf::from(&filepath);
    assert!(file.starts_with(srv.root.join("export")), "{filepath}");
    let fname = file.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        fname.starts_with("lubm_") && fname.ends_with(".nt"),
        "{fname}"
    );
    let content = fs::read_to_string(&file).unwrap();
    assert_eq!(content.lines().count(), 5);

    // The upstream script's download: GET /file/download/<filepath>.
    let r = send(a, "GET", &format!("/file/download/{filepath}"), &[], b"");
    assert_eq!(r.status, 200);
    assert_eq!(r.text(), content);
    assert_eq!(
        r.header("Content-Type").as_deref(),
        Some("application/octet-stream")
    );
    assert!(r.header("Content-Disposition").unwrap().contains(&fname));
    // A name relative to the export directory, by GET and by POST.
    let r = send(a, "GET", &format!("/file/download/{fname}"), &[], b"");
    assert_eq!((r.status, r.text()), (200, content.clone()));
    let r = send(
        a,
        "GET",
        &format!("/file/download/export/{fname}"),
        &[],
        b"",
    );
    assert_eq!(
        (r.status, r.text()),
        (200, content.clone()),
        "upstream's export/ prefix"
    );
    let r = send(
        a,
        "POST",
        "/file/download",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        format!("filepath={fname}").as_bytes(),
    );
    assert_eq!((r.status, r.text()), (200, content.clone()));
    // A byte range.
    let r = send(
        a,
        "GET",
        &format!("/file/download/{fname}"),
        &[("Range", "bytes=0-9")],
        b"",
    );
    assert_eq!(r.status, 206);
    assert_eq!(r.body, content.as_bytes()[..10].to_vec());
    assert_eq!(
        r.header("Content-Range"),
        Some(format!("bytes 0-9/{}", content.len()))
    );
    let r = send(
        a,
        "GET",
        &format!("/file/download/{fname}"),
        &[("Range", "bytes=99999-")],
        b"",
    );
    assert_eq!(r.status, 416);

    // Upstream's own defaults name the export directory itself.
    for same in ["./", "export", "./export/"] {
        let j = api(
            a,
            &[
                ("operation", "export"),
                ("db_name", "lubm"),
                ("db_path", same),
            ],
        );
        ok(&j);
        let p = PathBuf::from(string(&j, "filepath"));
        assert_eq!(p.parent().unwrap(), srv.root.join("export"), "{same}");
    }
    // An export into a sub-directory; refused targets and compress.
    let j = api(
        a,
        &[
            ("operation", "export"),
            ("db_name", "lubm"),
            ("db_path", "nightly"),
        ],
    );
    ok(&j);
    assert!(PathBuf::from(string(&j, "filepath")).starts_with(srv.root.join("export/nightly")));
    for bad in ["/tmp", "../", "nightly/../../x"] {
        let j = api(
            a,
            &[
                ("operation", "export"),
                ("db_name", "lubm"),
                ("db_path", bad),
            ],
        );
        assert_eq!(code(&j), 1003, "{bad}: {j}");
    }
    let j = api(
        a,
        &[
            ("operation", "export"),
            ("db_name", "lubm"),
            ("compress", "1"),
        ],
    );
    if cfg!(feature = "zip") {
        ok(&j);
        assert!(string(&j, "filepath").ends_with(".zip"), "{j}");
    } else {
        assert_eq!(code(&j), 1003);
        assert!(msg(&j).contains("`zip` feature"), "{j}");
    }
}

#[test]
fn downloads_are_confined_to_their_roots() {
    let srv = start("confine");
    let a = &srv.addr;
    let lubm = srv.data_file("lubm.nt", LUBM);
    build(a, "lubm", &lubm);
    let users_db = srv.root.join("users.db");
    assert!(users_db.is_file());
    let not_found = |r: &Reply| {
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(code(&r.json()), 11, "{}", r.text());
    };
    // Nothing outside the export directory without credentials: the user
    // table, an upload, a database file, traversal.
    let dbfile = fs::read_dir(srv.db_dir("lubm"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_file())
        .unwrap();
    for target in [
        users_db.to_string_lossy().into_owned(),
        lubm.clone(),
        dbfile.to_string_lossy().into_owned(),
        "/etc/passwd".to_string(),
        "users.db".to_string(),
    ] {
        let r = send(a, "GET", &format!("/file/download/{target}"), &[], b"");
        not_found(&r);
        let r = send(
            a,
            "POST",
            "/file/download",
            &[("Content-Type", "application/json")],
            Json::obj()
                .with("filepath", target.as_str())
                .to_string()
                .as_bytes(),
        );
        not_found(&r);
    }
    for trav in ["../users.db", "export/../../users.db", "%2e%2e/users.db"] {
        let r = send(a, "GET", &format!("/file/download/{trav}"), &[], b"");
        assert_eq!(code(&r.json()), 1003, "{trav}: {}", r.text());
    }
    // With credentials: uploads for any user; database files for root only;
    // the user table never.
    add_user(a, "bob");
    let with = |user: &str, target: &str| {
        let pass = if user == "root" { "root" } else { "pw" };
        send(
            a,
            "POST",
            "/file/download",
            &[("Content-Type", "application/json")],
            Json::obj()
                .with("username", user)
                .with("password", pass)
                .with("filepath", target)
                .to_string()
                .as_bytes(),
        )
    };
    let r = with("bob", &lubm);
    assert_eq!((r.status, r.text()), (200, LUBM.to_string()));
    let r = with("bob", "lubm.nt");
    assert_eq!((r.status, r.text()), (200, LUBM.to_string()), "by name");
    not_found(&with("bob", dbfile.to_str().unwrap()));
    let r = with("root", dbfile.to_str().unwrap());
    assert_eq!(r.status, 200);
    not_found(&with("root", users_db.to_str().unwrap()));
    let r = send(
        a,
        "POST",
        "/file/download",
        &[("Content-Type", "application/json")],
        Json::obj()
            .with("username", "bob")
            .with("password", "wrong")
            .with("filepath", lubm.as_str())
            .to_string()
            .as_bytes(),
    );
    assert_eq!(code(&r.json()), 1001);
    let r = send(a, "POST", "/file/download", &[], b"");
    assert_eq!(code(&r.json()), 1003);
}

#[test]
fn uploads_land_in_the_upload_dir_and_feed_build() {
    let srv = start_with(
        "upload",
        |mut c| {
            c.set("upload_max_body_size", "4096").unwrap();
            c
        },
        |s| {
            s.with_http_options(HttpOptions {
                max_body: 1 << 20,
                ..HttpOptions::default()
            })
        },
    );
    let a = &srv.addr;
    add_user(a, "bob");
    let j = upload(a, "bob", "../../evil name.nt", LUBM.as_bytes());
    assert_eq!((code(&j), msg(&j).as_str()), (0, "success"));
    let filepath = string(&j, "filepath");
    let stored = PathBuf::from(&filepath);
    assert_eq!(stored.parent().unwrap(), srv.upload_dir());
    let name = stored.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("evil_name_") && name.ends_with(".nt"),
        "{name}"
    );
    assert_eq!(fs::read_to_string(&stored).unwrap(), LUBM);
    // The answer's filepath is a valid db_path.
    let j = build(a, "fromup", &filepath);
    assert_eq!(int(&j, "success_num"), 5);
    // So is the bare file name (relative to the upload directory).
    let j = build(a, "byname", &name);
    assert_eq!(int(&j, "success_num"), 5);

    // Refusals: no file, a bad type, too big, bad credentials.
    let (ctype, body) = multipart(&[("username", "bob"), ("password", "pw")], None);
    let r = send(
        a,
        "POST",
        "/file/upload",
        &[("Content-Type", &ctype)],
        &body,
    );
    assert_eq!(
        (code(&r.json()), msg(&r.json()).as_str()),
        (1003, "Upload file can not be empty!")
    );
    let j = upload(a, "bob", "x.exe", b"MZ");
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1005, "The type of upload file is not supported!")
    );
    let j = upload(a, "bob", "big.nt", &vec![b'#'; 5000]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1005, "Upload file more than max_body_size!")
    );
    let (ctype, body) = multipart(
        &[("username", "bob"), ("password", "no")],
        Some(("y.nt", b"")),
    );
    let r = send(
        a,
        "POST",
        "/file/upload",
        &[("Content-Type", &ctype)],
        &body,
    );
    assert_eq!(code(&r.json()), 1001);
    // A zip may be uploaded (it is a configured package type); building from
    // it needs the `zip` feature (and a real zip: this one is not).
    let j = upload(a, "bob", "pack.zip", b"PK\x03\x04");
    ok(&j);
    let j = api(
        a,
        &[
            ("operation", "build"),
            ("db_name", "zipped"),
            ("db_path", &string(&j, "filepath")),
        ],
    );
    assert_eq!(code(&j), if cfg!(feature = "zip") { 1 } else { 1003 }, "{j}");
    // Only the uploads are in the upload directory (no unpacked leftovers).
    let names: Vec<String> = fs::read_dir(srv.upload_dir())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
}

// ---------------------------------------------------------------------------
// batchInsert / batchRemove
// ---------------------------------------------------------------------------

#[test]
fn batch_insert_and_remove_read_server_files_and_dirs() {
    let srv = start("batch");
    let a = &srv.addr;
    let lubm = srv.data_file("lubm.nt", LUBM);
    let more = srv.data_file("more.nt", MORE);
    let bad = srv.data_file("bad.nt", "<http://x> <http://p> <http://y> .\ngarbage\n");
    build(a, "lubm", &lubm);
    let j = api(
        a,
        &[
            ("operation", "batchInsert"),
            ("db_name", "lubm"),
            ("file", &more),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1004, "Database is not loaded yet.")
    );
    let j = api(
        a,
        &[
            ("operation", "batchInsert"),
            ("db_name", "nosuch"),
            ("file", &more),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1004, "Database[nosuch] does not exist.")
    );
    load(a, "lubm");
    let j = api(
        a,
        &[
            ("operation", "batchInsert"),
            ("db_name", "lubm"),
            ("file", &more),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (0, "Batch insert data successfully.")
    );
    assert_eq!((int(&j, "success_num"), int(&j, "failed_num")), (2, 0));
    // Duplicates are not counted; a bad statement is.
    let j = api_json(
        a,
        &Json::obj()
            .with("operation", "batchInsert")
            .with("username", "root")
            .with("password", "root")
            .with("db_name", "lubm")
            .with(
                "file",
                Json::Arr(vec![Json::from(more.as_str()), Json::from(bad.as_str())]),
            ),
    );
    ok(&j);
    assert_eq!((int(&j, "success_num"), int(&j, "failed_num")), (1, 1));
    assert_eq!(count(a, "lubm"), 8);
    let j = api(
        a,
        &[
            ("operation", "batchRemove"),
            ("db_name", "lubm"),
            ("file", &more),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (0, "Batch remove data successfully.")
    );
    assert_eq!(int(&j, "success_num"), 2);
    assert_eq!(count(a, "lubm"), 6);
    // dir: every RDF file in it.
    let dir = srv.upload_dir().join("set");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.nt"), MORE).unwrap();
    fs::write(dir.join("b.ttl"), "@prefix : <http://ex/> .\n:e :q :f .\n").unwrap();
    fs::write(dir.join("readme.md"), "not data").unwrap();
    let j = api(
        a,
        &[
            ("operation", "batchInsert"),
            ("db_name", "lubm"),
            ("dir", dir.to_str().unwrap()),
        ],
    );
    ok(&j);
    assert_eq!(int(&j, "success_num"), 3);
    assert_eq!(count(a, "lubm"), 9);
    // Refusals.
    let j = api(a, &[("operation", "batchInsert"), ("db_name", "lubm")]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1003, "the value of file can not be empty!")
    );
    let j = api(
        a,
        &[
            ("operation", "batchInsert"),
            ("db_name", "lubm"),
            ("file", "../users.db"),
        ],
    );
    assert_eq!(code(&j), 1003);
    let j = api(
        a,
        &[
            ("operation", "batchRemove"),
            ("db_name", "lubm"),
            ("file", "/etc/passwd"),
        ],
    );
    assert_eq!(code(&j), 1003);
    let gone = srv.upload_dir().join("gone.nt");
    let j = api(
        a,
        &[
            ("operation", "batchInsert"),
            ("db_name", "lubm"),
            ("file", gone.to_str().unwrap()),
        ],
    );
    assert_eq!(
        (code(&j), msg(&j)),
        (1005, format!("The file '{}' is not exist.", gone.display()))
    );
    // The changes are durable (check_point on): unload, load, count.
    ok(&api(a, &[("operation", "unload"), ("db_name", "lubm")]));
    load(a, "lubm");
    assert_eq!(count(a, "lubm"), 9);
}

// ---------------------------------------------------------------------------
// async + callback
// ---------------------------------------------------------------------------

#[test]
fn build_backup_and_batch_insert_run_asynchronously_with_callbacks() {
    // The callbacks go to listeners on the loopback interface.
    let srv = start_with(
        "async",
        |c| c,
        |s| {
            s.with_task_config(gstore::gapi::tasks::TaskConfig {
                callback_allow_local: true,
                ..Default::default()
            })
        },
    );
    let a = &srv.addr;
    let lubm = srv.data_file("lubm.nt", LUBM);
    let more = srv.data_file("more.nt", MORE);

    // build async + callback.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/cb", listener.local_addr().unwrap());
    let got = capture_one(listener);
    let j = api(
        a,
        &[
            ("operation", "build"),
            ("db_name", "lubm"),
            ("db_path", &lubm),
            ("async", "true"),
            ("callback", &url),
        ],
    );
    assert_eq!((code(&j), msg(&j).as_str()), (0, "Operation Success."));
    let opt_id = string(&j, "opt_id");
    let pushed = json::parse(&got.recv_timeout(Duration::from_secs(30)).unwrap()).unwrap();
    assert_eq!(code(&pushed), 0, "{pushed}");
    assert_eq!(msg(&pushed), "Import RDF file to database done.");
    assert_eq!(int(&pushed, "success_num"), 5);
    assert_eq!(string(&pushed, "opt_id"), opt_id);
    let state = wait_done(a, &opt_id);
    assert_eq!(int(&state, "state"), 1, "{state}");
    assert_eq!(int(&state, "success_num"), 5);

    // backup async: the callback carries the backup's path.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/cb", listener.local_addr().unwrap());
    let got = capture_one(listener);
    let j = api(
        a,
        &[
            ("operation", "backup"),
            ("db_name", "lubm"),
            ("async", "1"),
            ("callback", &url),
        ],
    );
    ok(&j);
    let opt_id = string(&j, "opt_id");
    let pushed = json::parse(&got.recv_timeout(Duration::from_secs(30)).unwrap()).unwrap();
    assert_eq!(code(&pushed), 0, "{pushed}");
    let path = PathBuf::from(string(&pushed, "backupfilepath"));
    assert!(path.is_dir(), "{}", path.display());
    let state = wait_done(a, &opt_id);
    assert_eq!(int(&state, "state"), 1);
    assert_eq!(string(&state, "backupfilepath"), path.to_string_lossy());

    // batchInsert async.
    load(a, "lubm");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/cb", listener.local_addr().unwrap());
    let got = capture_one(listener);
    let j = api(
        a,
        &[
            ("operation", "batchInsert"),
            ("db_name", "lubm"),
            ("file", &more),
            ("async", "true"),
            ("callback", &url),
        ],
    );
    ok(&j);
    let pushed = json::parse(&got.recv_timeout(Duration::from_secs(30)).unwrap()).unwrap();
    assert_eq!(msg(&pushed), "Batch insert data successfully.");
    assert_eq!(int(&pushed, "success_num"), 2);
    let state = wait_done(a, &string(&j, "opt_id"));
    assert_eq!((int(&state, "state"), int(&state, "success_num")), (1, 2));
    assert_eq!(count(a, "lubm"), 7);

    // A failing asynchronous build reports state -1.
    let j = api(
        a,
        &[
            ("operation", "build"),
            ("db_name", "lubm"),
            ("async", "true"),
        ],
    );
    ok(&j);
    let state = wait_done(a, &string(&j, "opt_id"));
    assert_eq!(int(&state, "state"), -1, "{state}");
}

// ---------------------------------------------------------------------------
// the inter-process lock
// ---------------------------------------------------------------------------

#[test]
fn a_loaded_database_is_locked_against_other_processes() {
    let srv = start("lock");
    let a = &srv.addr;
    let lubm = srv.data_file("lubm.nt", LUBM);
    build(a, "lubm", &lubm);
    load(a, "lubm");
    assert!(srv.db_dir("lubm").join("LOCK").is_file());
    let gquery = || {
        Command::new(env!("CARGO_BIN_EXE_gquery"))
            .current_dir(&srv.root)
            .args(["lubm", "-e", "SELECT * WHERE { ?s ?p ?o }"])
            .output()
            .unwrap()
    };
    // While this server holds it, another process cannot open it.
    let out = gquery();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{stderr}");
    assert!(stderr.contains("locked by another process"), "{stderr}");
    assert!(
        stderr.contains(&format!("pid {}", std::process::id())),
        "{stderr}"
    );
    // In this process, a second open shares the lock.
    let again = gstore::Database::open(srv.db_dir("lubm")).expect("same process");
    drop(again);
    assert_eq!(count(a, "lubm"), 5);
    // Unloaded, the other process gets in.
    ok(&api(a, &[("operation", "unload"), ("db_name", "lubm")]));
    let out = gquery();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A backup to try a restore from below.
    ok(&api(a, &[("operation", "backup"), ("db_name", "lubm")]));
    // And while another process holds it, the server can neither load nor
    // drop it: `gconsole lubm` opens it and waits on its stdin.
    let mut child = Command::new(env!("CARGO_BIN_EXE_gconsole"))
        .current_dir(&srv.root)
        .arg("lubm")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let mut first = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().unwrap()),
        &mut first,
    )
    .unwrap();
    assert!(first.starts_with("Loaded database 'lubm'"), "{first}");
    let j = api(a, &[("operation", "load"), ("db_name", "lubm")]);
    assert_eq!(code(&j), 1005, "{j}");
    assert!(msg(&j).contains("locked by another process"), "{j}");
    let j = api(
        a,
        &[
            ("operation", "drop"),
            ("db_name", "lubm"),
            ("is_backup", "false"),
        ],
    );
    assert_eq!(code(&j), 1005, "{j}");
    assert!(msg(&j).contains("locked by another process"), "{j}");
    assert!(srv.db_dir("lubm").is_dir());
    // The REST endpoints that rewrite, delete or swap the directory follow
    // the same rule: `409`, and nothing changes.
    let backup = fs::read_dir(srv.backup_dir())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("lubm.db_"))
        .expect("the backup taken above");
    let auth = [("Authorization", "Basic cm9vdDpyb290")]; // root:root
    for (target, body) in [
        ("/drop?db_name=lubm".to_string(), ""),
        ("/build?db_name=lubm".to_string(), LUBM),
        (format!("/restore?db_name=lubm&path={backup}"), ""),
    ] {
        let r = send(a, "POST", &target, &auth, body.as_bytes());
        assert_eq!(r.status, 409, "{target}: {}", r.text());
        assert!(
            r.text().contains("locked by another process"),
            "{target}: {}",
            r.text()
        );
        assert!(srv.db_dir("lubm").is_dir(), "{target}");
    }
    // This process cannot open it either, but may read a detached copy.
    let e = gstore::Database::open(srv.db_dir("lubm")).unwrap_err();
    assert!(e.to_string().contains("locked by another process"), "{e}");
    let copy = gstore::Database::read_copy(srv.db_dir("lubm")).unwrap();
    assert_eq!((copy.triple_num(), copy.home()), (5, None));
    // The child exits: the lock is gone with it.
    drop(stdin);
    let _ = child.wait();
    load(a, "lubm");
    assert_eq!(count(a, "lubm"), 5);
}

// ---------------------------------------------------------------------------
// upstream's scripts/test/api_test.sh
// ---------------------------------------------------------------------------

/// Field `n` (1-based) of `text` split on `"` — how `api_test.sh` extracts
/// `filepath` / `backupfilepath` (`awk -F '"' '{print $10}'`).
fn awk_field(text: &str, n: usize) -> String {
    text.split('"').nth(n - 1).unwrap_or_default().to_string()
}

fn raw_api(addr: &SocketAddr, body: &str) -> String {
    let r = send(
        addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    r.text()
}

#[test]
fn the_upstream_api_test_script_runs_unmodified() {
    // The script's data file, relative to the server's working directory
    // (here the crate root), readable through the load policy.
    let rdf = "testdata/lubm/lubm.nt";
    let srv = start_with(
        "apitest",
        |c| c,
        |s| s.with_load_policy(LoadPolicy::restricted().with_allowed_dir("testdata")),
    );
    let a = &srv.addr;
    let cred = r#""username":"root","password":"root""#;
    let call = |rest: &str| {
        let body = raw_api(a, &format!("{{{rest},{cred}}}"));
        (json::parse(&body).unwrap(), body)
    };
    let (j, _) = call(&format!(
        r#""operation":"build","db_name":"lubm","db_path":"{rdf}""#
    ));
    ok(&j);
    let triples = int(&j, "success_num");
    assert!(triples > 0);
    ok(&call(r#""operation":"show""#).0);
    let (j, _) = call(r#""operation":"monitor","db_name":"lubm""#);
    ok(&j);
    assert_eq!(string(&j, "tripleNum"), triples.to_string());
    ok(&call(r#""operation":"load","db_name":"lubm""#).0);
    let (j, _) = call(
        r#""operation":"query","db_name":"lubm","sparql":"select ?s ?p ?o where {?s ?p ?o.} limit 10""#,
    );
    assert_eq!(int(&j, "AnsNum"), 10);
    // export → download what `awk` extracted.
    let (j, body) = call(r#""operation":"export","db_name":"lubm""#);
    ok(&j);
    let export_path = awk_field(&body, 10);
    assert_eq!(export_path, string(&j, "filepath"));
    let r = send(a, "GET", &format!("/file/download/{export_path}"), &[], b"");
    assert_eq!(r.status, 200);
    assert_eq!(r.text().lines().count() as i64, triples);
    // backup → restore from what `awk` extracted.
    let (j, body) = call(r#""operation":"backup","db_name":"lubm""#);
    ok(&j);
    let backup_path = awk_field(&body, 10);
    assert_eq!(backup_path, string(&j, "backupfilepath"));
    let (j, _) = call(&format!(
        r#""operation":"batchRemove","db_name":"lubm","file":"{rdf}""#
    ));
    assert_eq!(int(&j, "success_num"), triples);
    let (j, _) = call(&format!(
        r#""operation":"batchInsert","db_name":"lubm","file":"{rdf}""#
    ));
    assert_eq!(int(&j, "success_num"), triples);
    ok(&call(r#""operation":"unload","db_name":"lubm""#).0);
    let (j, _) = call(r#""operation":"restore","db_name":"lubm","backup_path":"""#);
    ok(&j);
    ok(&call(&format!(
        r#""operation":"restore","db_name":"lubm","backup_path":"{backup_path}""#
    ))
    .0);
    ok(&call(r#""operation":"load","db_name":"lubm""#).0);
    // The transaction steps.
    let (j, _) = call(r#""operation":"begin","db_name":"lubm","isolevel":"1""#);
    assert_eq!(string(&j, "TID"), "1");
    ok(&call(
        r#""operation":"tquery","db_name":"lubm","tid":"1","sparql":"insert data {<s> <p> <o>.}""#,
    )
    .0);
    ok(&call(r#""operation":"commit","db_name":"lubm","tid":"1""#).0);
    ok(&call(r#""operation":"checkpoint","db_name":"lubm""#).0);
    // rename while loaded is refused (upstream too); the drops then clean up.
    let (j, _) = call(r#""operation":"rename","db_name":"lubm","new_name":"newlubm""#);
    assert_eq!(code(&j), 1005);
    let (j, _) = call(r#""operation":"drop","db_name":"newlubm","is_backup":"false""#);
    assert_eq!(code(&j), 1005);
    ok(&call(r#""operation":"drop","db_name":"lubm","is_backup":"false""#).0);
    assert!(!srv.db_dir("lubm").exists());
}
