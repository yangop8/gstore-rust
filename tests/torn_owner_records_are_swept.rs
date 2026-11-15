//! The retention sweep of the upstream file routes (`gapi::ops_file`) does
//! not skip an owner record it cannot parse for ever (round-1 review of
//! sec#14 / gapi#13): records used to be written in place without `fsync`,
//! so a crash could leave one torn (or a crash while writing one leaves its
//! staging file), and the sweep's `continue` then kept that record and its
//! file — unreadable to its owner, outside the upload quota — for good.
//!
//! An entry of `<root>/file_owners/` that does not parse is dropped once it
//! is older than `file_retention_hours`, by its own modification time, and
//! the file it names (found by the record's name, the SHA-256 of the file's
//! path) goes with it once that file is expired too. A fresh unparsable
//! entry (a record being written) is left alone.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::query::hash::sha256_hex;

/// Basic `root:pw`.
const ROOT: (&str, &str) = ("Authorization", "Basic cm9vdDpwdw==");

/// A fresh, canonical temporary directory, removed when dropped (also when
/// the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_torn_records_{tag}_{}_{n}",
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

/// A running multi-database server at `root` keeping files for an hour;
/// shut down when dropped.
struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
    }
}

fn start_at(root: &Path) -> Srv {
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    config.set("file_retention_hours", "1").unwrap();
    let server = Arc::new(
        ApiServer::bind(root, "pw", "127.0.0.1:0")
            .unwrap()
            .with_config(config),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    Srv { server, addr }
}

/// One request (`Connection: close`): `(status, body)`.
fn send(
    addr: SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
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
        .unwrap_or_else(|| panic!("no status line: {resp:?}"));
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

fn backdate(path: &Path, by: Duration) {
    let f = File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - by).unwrap();
}

/// Where the owner record of `file` lives (`ops_file::record_path`).
fn record_of(root: &Path, file: &Path) -> PathBuf {
    let key = sha256_hex(&fs::canonicalize(file).unwrap().to_string_lossy());
    root.join("file_owners").join(format!("{key}.json"))
}

#[test]
fn a_torn_owner_record_and_its_expired_file_are_swept() {
    let root = TmpDir::new("sweep");
    let upload = root.0.join("upload");
    let export = root.0.join("export");
    let owners = root.0.join("file_owners");
    for d in [&upload, &export, &owners] {
        fs::create_dir_all(d).unwrap();
    }
    let day = Duration::from_secs(24 * 3600);
    // An old upload whose record a crash tore.
    let torn_file = upload.join("old_upload.nt");
    fs::write(&torn_file, "<http://a> <http://b> <http://c> .\n").unwrap();
    let torn = record_of(&root.0, &torn_file);
    fs::write(&torn, br#"{"path":"/some/where/old_upl"#).unwrap();
    backdate(&torn_file, day);
    backdate(&torn, day);
    // The staging file of a record a crash interrupted, for an old export.
    let staged_file = export.join("old_export.nt");
    fs::write(&staged_file, "<http://a> <http://b> <http://c> .\n").unwrap();
    let staged = owners.join(format!(
        "{}.json.tmp",
        sha256_hex(&staged_file.to_string_lossy())
    ));
    fs::write(&staged, b"").unwrap();
    backdate(&staged_file, day);
    backdate(&staged, day);
    // A torn record whose file is gone.
    let orphan = owners.join(format!("{}.json", sha256_hex("/no/such/file")));
    fs::write(&orphan, b"{").unwrap();
    backdate(&orphan, day);
    // A fresh unparsable entry (a record being written) and an old file
    // without any record (an operator's) stay.
    let fresh_file = upload.join("fresh.nt");
    fs::write(&fresh_file, "<http://a> <http://b> <http://c> .\n").unwrap();
    let fresh = record_of(&root.0, &fresh_file);
    fs::write(&fresh, b"{\"pa").unwrap();
    backdate(&fresh_file, day);
    let operator = upload.join("operator.nt");
    fs::write(&operator, "<http://a> <http://b> <http://c> .\n").unwrap();
    backdate(&operator, day);

    // The server's first file operation (an export) sweeps.
    let srv = start_at(&root.0);
    let (st, body) = send(
        srv.addr,
        "POST",
        "/build?db_name=dba",
        &[ROOT],
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    );
    assert_eq!(st, 200, "build: {body}");
    let (st, body) = send(
        srv.addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        r#"{"operation":"export","db_name":"dba","username":"root","password":"pw"}"#,
    );
    assert_eq!(st, 200, "{body}");
    let j = json::parse(&body).unwrap();
    assert_eq!(
        j.get("StatusCode").and_then(Json::as_i64),
        Some(0),
        "{body}"
    );

    assert!(!torn.exists(), "the torn record is dropped");
    assert!(!torn_file.exists(), "its expired file is removed");
    assert!(!staged.exists(), "the stale staging file is dropped");
    assert!(!staged_file.exists(), "its expired file is removed");
    assert!(!orphan.exists(), "a torn record without a file is dropped");
    assert!(fresh.exists(), "a fresh unparsable entry is left alone");
    assert!(fresh_file.exists());
    assert!(
        operator.exists(),
        "a file without a record is never removed"
    );
}
