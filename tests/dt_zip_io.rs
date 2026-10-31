//! Data test: zip packages and export formats (feature `zip`) — `build` /
//! `batchInsert` / `batchRemove` from a `.zip` of RDF files (also uploaded
//! through `/file/upload`), upstream's `ZipCode` answers for bad packages,
//! the zip-slip and zip-bomb guards and the load policy on the archive path,
//! `backup compress=true` → `restore` round trips, `export compress=true`
//! and N-Quads exports with named graphs, and the CLI's `gexport` / `gbackup`
//! / `grestore` zip and format flags. Without the feature: every zip path is
//! a clean `1003` naming the feature.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
#[cfg(feature = "zip")]
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
#[cfg(feature = "zip")]
use gstore::LoadPolicy;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("gstore_zipio_{tag}_{}_{n}", std::process::id()));
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
    /// Put a file in the upload directory; its absolute path.
    fn data_file(&self, name: &str, content: &[u8]) -> String {
        let dir = self.upload_dir();
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p.to_string_lossy().into_owned()
    }
    /// The names in the upload directory.
    fn uploads(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.upload_dir())
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn start_with(
    tag: &str,
    cfg: impl FnOnce(&mut GStoreConfig),
    build: impl FnOnce(ApiServer) -> ApiServer,
) -> Srv {
    let root = tmp_root(tag);
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    cfg(&mut config);
    let server = ApiServer::bind(&root, "root", "127.0.0.1:0")
        .unwrap()
        .with_config(config);
    let server = Arc::new(build(server));
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || {
        s2.serve_forever();
    });
    Srv { server, addr, root }
}

fn start(tag: &str) -> Srv {
    start_with(tag, |_| {}, |s| s)
}

/// An HTTP exchange: (status, body).
fn send(addr: &SocketAddr, method: &str, target: &str, ctype: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let mut raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: {ctype}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(body);
    s.write_all(&raw).unwrap();
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
    (status, resp[split + 4..].to_vec())
}

/// `POST /api` as `user` (password `pw`; `root` for root) with `pairs`.
fn api_as(addr: &SocketAddr, user: &str, pairs: &[(&str, &str)]) -> Json {
    let pass = if user == "root" { "root" } else { "pw" };
    let mut body = Json::obj().with("username", user).with("password", pass);
    for (k, v) in pairs {
        body.set(*k, *v);
    }
    let (status, bytes) = send(
        addr,
        "POST",
        "/api",
        "application/json",
        body.to_string().as_bytes(),
    );
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert_eq!(status, 200, "{text}");
    json::parse(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
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

fn string(j: &Json, key: &str) -> String {
    j.get(key)
        .and_then(Json::as_str)
        .unwrap_or_else(|| panic!("no string {key} in {j}"))
        .to_string()
}

fn ok(j: &Json) {
    assert_eq!(code(j), 0, "{j}");
}

const LUBM: &str = r#"<http://ex/a> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/onto#Person> .
<http://ex/b> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/onto#Person> .
<http://ex/u> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <http://ex/onto#Univ> .
<http://ex/a> <http://ex/onto#name> "Alice" .
<http://ex/a> <http://ex/onto#worksAt> <http://ex/u> .
"#;

// ===========================================================================
// Without the feature: a clean 1003 naming it
// ===========================================================================

#[cfg(not(feature = "zip"))]
#[test]
fn without_the_feature_every_zip_path_is_a_clean_1003() {
    let srv = start("nofeature");
    let a = &srv.addr;
    let lubm = srv.data_file("lubm.nt", LUBM.as_bytes());
    let names_feature = |j: &Json| {
        assert_eq!(code(j), 1003, "{j}");
        assert!(msg(j).contains("`zip` feature"), "{j}");
    };
    let zip = srv.data_file("data.zip", b"PK\x03\x04");
    names_feature(&api(
        a,
        &[("operation", "build"), ("db_name", "zipped"), ("db_path", &zip)],
    ));
    ok(&api(
        a,
        &[("operation", "build"), ("db_name", "lubm"), ("db_path", &lubm)],
    ));
    ok(&api(a, &[("operation", "load"), ("db_name", "lubm")]));
    names_feature(&api(
        a,
        &[("operation", "batchInsert"), ("db_name", "lubm"), ("file", &zip)],
    ));
    names_feature(&api(
        a,
        &[("operation", "backup"), ("db_name", "lubm"), ("compress", "true")],
    ));
    names_feature(&api(
        a,
        &[("operation", "export"), ("db_name", "lubm"), ("compress", "true")],
    ));
    fs::create_dir_all(srv.backup_dir()).unwrap();
    let zipped = srv.backup_dir().join("lubm.db_20240101000000.zip");
    fs::write(&zipped, b"PK\x03\x04").unwrap();
    names_feature(&api(
        a,
        &[
            ("operation", "restore"),
            ("db_name", "lubm"),
            ("backup_path", zipped.to_str().unwrap()),
        ],
    ));
    // Nothing was unpacked or written.
    assert_eq!(srv.uploads(), ["data.zip", "lubm.nt"]);
    // The export format works without the feature.
    let j = api(
        a,
        &[("operation", "export"), ("db_name", "lubm"), ("format", "nq")],
    );
    ok(&j);
    assert!(string(&j, "filepath").ends_with(".nq"), "{j}");

    // The CLI names the feature too.
    let dir = tmp_root("nofeature_cli");
    let out = Command::new(env!("CARGO_BIN_EXE_gexport"))
        .args(["-z", "nosuch", dir.join("x.nt").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("`zip` feature"));
    let out = Command::new(env!("CARGO_BIN_EXE_gbackup"))
        .args(["--zip", "nosuch", dir.join("b").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("`zip` feature"));
    let _ = fs::remove_dir_all(&dir);
}

// ===========================================================================
// With the feature
// ===========================================================================

#[cfg(feature = "zip")]
mod with_zip {
    use super::*;
    use zip::write::SimpleFileOptions;

    /// One entry of a test package.
    enum E<'a> {
        File(&'a str, &'a [u8]),
        Dir(&'a str),
        Link(&'a str, &'a str),
    }

    fn package(entries: &[E<'_>]) -> Vec<u8> {
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default();
        for e in entries {
            match e {
                E::File(name, data) => {
                    zw.start_file(*name, opts).unwrap();
                    zw.write_all(data).unwrap();
                }
                E::Dir(name) => zw.add_directory(*name, opts).unwrap(),
                E::Link(name, target) => zw.add_symlink(*name, *target, opts).unwrap(),
            }
        }
        zw.finish().unwrap().into_inner()
    }

    /// The names of a zip's entries.
    fn entries(zip: &Path) -> Vec<String> {
        let a = zip::ZipArchive::new(fs::File::open(zip).unwrap()).unwrap();
        let mut names: Vec<String> = a.file_names().map(|n| n.unwrap().into_owned()).collect();
        names.sort();
        names
    }

    /// A server that also accepts `.nq` sources (upstream's default
    /// `upload_allow_extensions` lists only `nt|ttl|n3|rdf|txt`).
    fn start_nq(tag: &str) -> Srv {
        start_with(
            tag,
            |c| c.set("upload_allow_extensions", "nt|ttl|n3|rdf|txt|nq").unwrap(),
            |s| s,
        )
    }

    fn int(j: &Json, key: &str) -> i64 {
        j.get(key)
            .and_then(Json::as_i64)
            .unwrap_or_else(|| panic!("no integer {key} in {j}"))
    }

    /// `AnsNum` of `sparql` on `db`.
    fn answers(a: &SocketAddr, db: &str, sparql: &str) -> i64 {
        let j = api(
            a,
            &[("operation", "query"), ("db_name", db), ("sparql", sparql)],
        );
        ok(&j);
        int(&j, "AnsNum")
    }

    fn count(a: &SocketAddr, db: &str) -> i64 {
        answers(a, db, "SELECT * WHERE { ?s ?p ?o }")
    }

    fn graph_count(a: &SocketAddr, db: &str) -> i64 {
        answers(a, db, "SELECT * WHERE { GRAPH ?g { ?s ?p ?o } }")
    }

    const MORE: &str = "<http://ex/c> <http://ex/onto#name> \"Carol\" .\n\
                        <http://ex/d> <http://ex/onto#name> \"Dave\" .\n";
    const TTL: &str = "@prefix ex: <http://ex/> .\nex:e ex:p ex:f .\nex:g ex:p \"x\" .\n";
    const QUADS: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                         <http://ex/a> <http://ex/p> <http://ex/c> <http://ex/g1> .\n\
                         <http://ex/b> <http://ex/p> \"lit\" <http://ex/g1> .\n\
                         <http://ex/c> <http://ex/p> <http://ex/d> <http://ex/g2> .\n";

    // -----------------------------------------------------------------------
    // build / batchInsert / batchRemove / upload
    // -----------------------------------------------------------------------

    #[test]
    fn build_and_batches_read_zip_packages() {
        let srv = start_nq("rdfpkg");
        let a = &srv.addr;
        let pkg = srv.data_file(
            "data.zip",
            &package(&[
                E::File("lubm.nt", LUBM.as_bytes()),
                E::Dir("more/"),
                E::File("more/extra.ttl", TTL.as_bytes()),
            ]),
        );
        let j = api(
            a,
            &[("operation", "build"), ("db_name", "lubm"), ("db_path", &pkg)],
        );
        ok(&j);
        assert_eq!((int(&j, "success_num"), int(&j, "failed_num")), (7, 0));
        // The unpacked directory is gone; the package stays.
        assert_eq!(srv.uploads(), ["data.zip"]);
        ok(&api(a, &[("operation", "load"), ("db_name", "lubm")]));
        assert_eq!(count(a, "lubm"), 7);

        // A package mixed with a plain file in one batchInsert.
        let more = srv.data_file("more.zip", &package(&[E::File("m.nt", MORE.as_bytes())]));
        let plain = srv.data_file(
            "plain.nt",
            b"<http://ex/z> <http://ex/p> <http://ex/y> .\n",
        );
        let body = Json::obj()
            .with("operation", "batchInsert")
            .with("username", "root")
            .with("password", "root")
            .with("db_name", "lubm")
            .with(
                "file",
                Json::from(vec![Json::from(more.as_str()), Json::from(plain.as_str())]),
            );
        let (_, bytes) = send(
            a,
            "POST",
            "/api",
            "application/json",
            body.to_string().as_bytes(),
        );
        let j = json::parse(&String::from_utf8_lossy(&bytes)).unwrap();
        ok(&j);
        assert_eq!((int(&j, "success_num"), int(&j, "failed_num")), (3, 0));
        assert_eq!(count(a, "lubm"), 10);
        let j = api(
            a,
            &[("operation", "batchRemove"), ("db_name", "lubm"), ("file", &more)],
        );
        ok(&j);
        assert_eq!(int(&j, "success_num"), 2);
        assert_eq!(count(a, "lubm"), 8);
        // A statement that does not parse is counted, like a plain file's.
        let bad = srv.data_file(
            "bad.zip",
            &package(&[E::File("x.nt", b"<http://ex/q> <http://ex/p> <http://ex/r> .\nnot rdf\n")]),
        );
        let j = api(
            a,
            &[("operation", "batchInsert"), ("db_name", "lubm"), ("file", &bad)],
        );
        ok(&j);
        assert_eq!((int(&j, "success_num"), int(&j, "failed_num")), (1, 1));
        assert_eq!(srv.uploads(), ["bad.zip", "data.zip", "more.zip", "plain.nt"]);

        // A package of N-Quads keeps its named graphs.
        let nq = srv.data_file("quads.zip", &package(&[E::File("q.nq", QUADS.as_bytes())]));
        let j = api(
            a,
            &[("operation", "build"), ("db_name", "quads"), ("db_path", &nq)],
        );
        ok(&j);
        assert_eq!(int(&j, "success_num"), 4);
        ok(&api(a, &[("operation", "load"), ("db_name", "quads")]));
        assert_eq!(graph_count(a, "quads"), 3);
    }

    #[test]
    fn uploaded_packages_feed_build() {
        let srv = start("upload");
        let a = &srv.addr;
        let data = package(&[E::File("lubm.nt", LUBM.as_bytes())]);
        let boundary = "----gstorezipboundary";
        let mut body = Vec::new();
        for (k, v) in [("username", "root"), ("password", "root")] {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                    .as_bytes(),
            );
        }
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
                 filename=\"lubm.zip\"\r\nContent-Type: application/zip\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let (status, bytes) = send(
            a,
            "POST",
            "/file/upload",
            &format!("multipart/form-data; boundary={boundary}"),
            &body,
        );
        assert_eq!(status, 200);
        let j = json::parse(&String::from_utf8_lossy(&bytes)).unwrap();
        ok(&j);
        let filepath = string(&j, "filepath");
        assert!(filepath.ends_with(".zip"), "{filepath}");
        let j = api(
            a,
            &[("operation", "build"), ("db_name", "upz"), ("db_path", &filepath)],
        );
        ok(&j);
        assert_eq!(int(&j, "success_num"), 5);
        ok(&api(a, &[("operation", "load"), ("db_name", "upz")]));
        assert_eq!(count(a, "upz"), 5);
        assert_eq!(srv.uploads().len(), 1, "{:?}", srv.uploads());
    }

    // -----------------------------------------------------------------------
    // Bad packages: upstream's ZipCodes, zip-slip, zip bombs, load policy
    // -----------------------------------------------------------------------

    #[test]
    fn bad_packages_are_refused_with_upstream_codes() {
        let srv = start_with(
            "badpkg",
            |c| {
                c.set("unzip_max_entries", "4").unwrap();
                c.set("unzip_max_size", "2048").unwrap();
            },
            |s| s,
        );
        let a = &srv.addr;
        let build = |name: &str, bytes: &[u8]| {
            let p = srv.data_file(name, bytes);
            api(
                a,
                &[("operation", "build"), ("db_name", "bad"), ("db_path", &p)],
            )
        };
        let refused = |j: &Json, want: i64| {
            assert_eq!(code(j), want, "{j}");
            assert!(msg(j).starts_with("zip format or data is error."), "{j}");
        };
        // 1: not a zip at all.
        refused(&build("fake.zip", b"PK\x03\x04 not really"), 1);
        // 3: an entry whose suffix is not an allowed RDF extension.
        refused(
            &build(
                "csv.zip",
                &package(&[E::File("a.nt", LUBM.as_bytes()), E::File("b.csv", b"x,y")]),
            ),
            3,
        );
        // 5: no file at all.
        refused(&build("dirs.zip", &package(&[E::Dir("only/")])), 5);
        // 3: zip-slip — a relative escape, an absolute path; nothing lands
        // outside (or inside) the upload directory.
        for (i, evil) in ["../evil.nt", "/tmp/gstore_zip_evil.nt", "a/../../evil.nt"]
            .iter()
            .enumerate()
        {
            let j = build(
                &format!("slip{i}.zip"),
                &package(&[E::File("ok.nt", LUBM.as_bytes()), E::File(evil, LUBM.as_bytes())]),
            );
            refused(&j, 3);
            assert!(msg(&j).contains("not a relative path"), "{j}");
        }
        assert!(!srv.root.join("evil.nt").exists());
        assert!(!Path::new("/tmp/gstore_zip_evil.nt").exists());
        // 3: more entries than unzip_max_entries.
        let many: Vec<(String, &[u8])> = (0..5)
            .map(|i| (format!("f{i}.nt"), &b"<http://ex/a> <http://ex/p> <http://ex/b> .\n"[..]))
            .collect();
        let many: Vec<E<'_>> = many.iter().map(|(n, d)| E::File(n, d)).collect();
        let j = build("many.zip", &package(&many));
        refused(&j, 3);
        assert!(msg(&j).contains("unzip_max_entries"), "{j}");
        // 3: a zip bomb — far more than unzip_max_size unpacked (it compresses
        // to a few hundred bytes).
        let zeros = vec![b' '; 1 << 20];
        let j = build("bomb.zip", &package(&[E::File("bomb.nt", &zeros)]));
        refused(&j, 3);
        assert!(msg(&j).contains("unzip_max_size"), "{j}");
        // Nothing was built, nothing unpacked is left behind.
        assert!(!srv.root.join("bad.db").exists());
        let leftovers: Vec<String> = srv
            .uploads()
            .into_iter()
            .filter(|n| !n.ends_with(".zip"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // A symbolic-link entry is ignored: the package's files load.
        let j = build(
            "link.zip",
            &package(&[E::File("lubm.nt", LUBM.as_bytes()), E::Link("passwd.nt", "/etc/passwd")]),
        );
        ok(&j);
        assert_eq!(int(&j, "success_num"), 5);
    }

    #[test]
    fn the_load_policy_applies_to_the_package_path() {
        let elsewhere = tmp_root("pkg_elsewhere");
        let pkg = elsewhere.join("p.zip");
        fs::write(&pkg, package(&[E::File("lubm.nt", LUBM.as_bytes())])).unwrap();
        // Outside every root: refused like any other source.
        let srv = start("pkgpolicy");
        let j = api(
            &srv.addr,
            &[("operation", "build"), ("db_name", "xxx"), ("db_path", pkg.to_str().unwrap())],
        );
        assert_eq!(code(&j), 1003, "{j}");
        assert!(msg(&j).contains("is refused"), "{j}");
        // Allowed by the load policy (gserver --load-dir): read.
        let dir = elsewhere.clone();
        let srv2 = start_with(
            "pkgpolicy2",
            |_| {},
            move |s| s.with_load_policy(LoadPolicy::restricted().with_allowed_dir(dir)),
        );
        let j = api(
            &srv2.addr,
            &[("operation", "build"), ("db_name", "xxx"), ("db_path", pkg.to_str().unwrap())],
        );
        ok(&j);
        assert_eq!(int(&j, "success_num"), 5);
        // The package was unpacked into the upload directory, never beside it.
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&elsewhere);
    }

    // -----------------------------------------------------------------------
    // backup compress=true → restore
    // -----------------------------------------------------------------------

    #[test]
    fn zipped_backups_round_trip() {
        let srv = start("bkzip");
        let a = &srv.addr;
        let lubm = srv.data_file("lubm.nt", LUBM.as_bytes());
        ok(&api(
            a,
            &[("operation", "build"), ("db_name", "lubm"), ("db_path", &lubm)],
        ));
        ok(&api(a, &[("operation", "load"), ("db_name", "lubm")]));
        let j = api(
            a,
            &[("operation", "backup"), ("db_name", "lubm"), ("compress", "true")],
        );
        ok(&j);
        assert_eq!(msg(&j), "Database backup successfully.");
        let zip = PathBuf::from(string(&j, "backupfilepath"));
        let name = zip.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("lubm.db_") && name.ends_with(".zip") && name.len() == 26,
            "{name}"
        );
        assert_eq!(zip.parent().unwrap(), srv.backup_dir());
        assert!(zip.is_file());
        // Upstream's archive: the backup's files at the root, the directory
        // itself gone.
        assert!(!srv.backup_dir().join(name.trim_end_matches(".zip")).exists());
        let names = entries(&zip);
        assert!(names.iter().any(|n| n == "meta.bin"), "{names:?}");
        assert!(names.iter().all(|n| !n.starts_with("lubm.db")), "{names:?}");
        // backuppath lists it.
        let j = api(a, &[("operation", "backuppath"), ("db_name", "lubm")]);
        ok(&j);
        assert!(j.to_string().contains(&name), "{j}");

        // Change the database, then restore the archive (loaded: swapped).
        let more = srv.data_file("more.nt", b"<http://ex/c> <http://ex/p> <http://ex/d> .\n");
        ok(&api(
            a,
            &[("operation", "batchInsert"), ("db_name", "lubm"), ("file", &more)],
        ));
        assert_eq!(count(a, "lubm"), 6);
        let j = api(
            a,
            &[
                ("operation", "restore"),
                ("db_name", "lubm"),
                ("backup_path", zip.to_str().unwrap()),
            ],
        );
        ok(&j);
        assert_eq!(msg(&j), "Database restore successfully.");
        assert_eq!(count(a, "lubm"), 5);
        assert!(!srv.root.join("lubm.db.unzip-tmp").exists());
        // Without backup_path: the newest backup — the archive.
        ok(&api(
            a,
            &[("operation", "batchInsert"), ("db_name", "lubm"), ("file", &more)],
        ));
        ok(&api(a, &[("operation", "restore"), ("db_name", "lubm")]));
        assert_eq!(count(a, "lubm"), 5);
        // Into a database that does not exist yet: created.
        ok(&api(
            a,
            &[
                ("operation", "restore"),
                ("db_name", "copy"),
                ("backup_path", zip.to_str().unwrap()),
            ],
        ));
        ok(&api(a, &[("operation", "load"), ("db_name", "copy")]));
        assert_eq!(count(a, "copy"), 5);

        // The owner record travels inside the archive: a user with the
        // restore privilege on `lubm` restores it, not into another database.
        ok(&api(
            a,
            &[
                ("operation", "usermanage"),
                ("type", "1"),
                ("op_username", "bob"),
                ("op_password", "pw"),
            ],
        ));
        for db in ["lubm", "copy"] {
            ok(&api(
                a,
                &[
                    ("operation", "userprivilegemanage"),
                    ("type", "1"),
                    ("op_username", "bob"),
                    ("db_name", db),
                    ("privileges", "6"),
                ],
            ));
        }
        let j = api_as(
            a,
            "bob",
            &[
                ("operation", "restore"),
                ("db_name", "copy"),
                ("backup_path", zip.to_str().unwrap()),
            ],
        );
        assert_eq!(code(&j), 1004, "{j}");
        ok(&api_as(
            a,
            "bob",
            &[
                ("operation", "restore"),
                ("db_name", "lubm"),
                ("backup_path", zip.to_str().unwrap()),
            ],
        ));

        // An archive that wraps the backup in a directory restores too.
        let dir_zip = srv.backup_dir().join("lubm.db_20200101000000.zip");
        let mut wrapped = Vec::new();
        {
            let src = zip::ZipArchive::new(fs::File::open(&zip).unwrap());
            let mut src = src.unwrap();
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut wrapped));
            zw.add_directory("lubm.db_20200101000000/", SimpleFileOptions::default())
                .unwrap();
            for i in 0..src.len() {
                let mut f = src.by_index(i).unwrap();
                if f.is_dir() {
                    continue;
                }
                let name = format!("lubm.db_20200101000000/{}", f.name().unwrap());
                let mut data = Vec::new();
                f.read_to_end(&mut data).unwrap();
                zw.start_file(name, SimpleFileOptions::default()).unwrap();
                zw.write_all(&data).unwrap();
            }
            zw.finish().unwrap();
        }
        fs::write(&dir_zip, &wrapped).unwrap();
        ok(&api(
            a,
            &[
                ("operation", "restore"),
                ("db_name", "lubm"),
                ("backup_path", dir_zip.to_str().unwrap()),
            ],
        ));
        assert_eq!(count(a, "lubm"), 5);

        // A bad archive changes nothing: upstream's `backup uncompress fail`.
        let evil = srv.backup_dir().join("lubm.db_20210101000000.zip");
        fs::write(
            &evil,
            package(&[E::File("meta.bin", b"x"), E::File("../escaped", b"x")]),
        )
        .unwrap();
        let j = api(
            a,
            &[
                ("operation", "restore"),
                ("db_name", "lubm"),
                ("backup_path", evil.to_str().unwrap()),
            ],
        );
        assert_eq!(code(&j), 1005, "{j}");
        assert!(msg(&j).starts_with("backup uncompress fail"), "{j}");
        assert!(!srv.backup_dir().parent().unwrap().join("escaped").exists());
        assert!(!srv.root.join("lubm.db.unzip-tmp").exists());
        // An archive holding no database.
        let empty = srv.backup_dir().join("lubm.db_20220101000000.zip");
        fs::write(&empty, package(&[E::File("readme.txt", b"hi")])).unwrap();
        let j = api(
            a,
            &[
                ("operation", "restore"),
                ("db_name", "lubm"),
                ("backup_path", empty.to_str().unwrap()),
            ],
        );
        assert_eq!(code(&j), 1005, "{j}");
        assert!(msg(&j).contains("does not contain a database backup"), "{j}");
        // Any other file is not a backup.
        let other = srv.backup_dir().join("notes.tar");
        fs::write(&other, b"x").unwrap();
        let j = api(
            a,
            &[
                ("operation", "restore"),
                ("db_name", "lubm"),
                ("backup_path", other.to_str().unwrap()),
            ],
        );
        assert_eq!((code(&j), msg(&j).as_str()), (1003, "Backup file is not zip file."));
        assert_eq!(count(a, "lubm"), 5);
    }

    #[test]
    fn zipped_backups_of_unloaded_databases_and_rotation() {
        let srv = start_with("bkrot", |c| c.max_backups = 2, |s| s);
        let a = &srv.addr;
        let lubm = srv.data_file("lubm.nt", LUBM.as_bytes());
        ok(&api(
            a,
            &[("operation", "build"), ("db_name", "lubm"), ("db_path", &lubm)],
        ));
        // Not loaded: backed up from disk, zipped.
        let mut made = Vec::new();
        for _ in 0..3 {
            let j = api(
                a,
                &[("operation", "backup"), ("db_name", "lubm"), ("compress", "1")],
            );
            ok(&j);
            made.push(string(&j, "backupfilepath"));
            thread::sleep(Duration::from_millis(1100));
        }
        let left: Vec<String> = fs::read_dir(srv.backup_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 2, "max_backups: {left:?}");
        assert!(!Path::new(&made[0]).exists());
        ok(&api(
            a,
            &[("operation", "restore"), ("db_name", "lubm2"), ("backup_path", &made[2])],
        ));
        ok(&api(a, &[("operation", "load"), ("db_name", "lubm2")]));
        assert_eq!(count(a, "lubm2"), 5);
    }

    // -----------------------------------------------------------------------
    // export: N-Quads, format, compress
    // -----------------------------------------------------------------------

    #[test]
    fn exports_write_nquads_and_zips() {
        let srv = start_nq("export");
        let a = &srv.addr;
        let nq = srv.data_file("q.nq", QUADS.as_bytes());
        ok(&api(
            a,
            &[("operation", "build"), ("db_name", "quad"), ("db_path", &nq)],
        ));
        ok(&api(a, &[("operation", "load"), ("db_name", "quad")]));
        // Named graphs: N-Quads by default, nothing lost.
        let j = api(a, &[("operation", "export"), ("db_name", "quad")]);
        ok(&j);
        let file = PathBuf::from(string(&j, "filepath"));
        assert!(file.to_string_lossy().ends_with(".nq"), "{j}");
        let text = fs::read_to_string(&file).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines.sort();
        let mut want: Vec<&str> = QUADS.lines().collect();
        want.sort();
        assert_eq!(lines, want);
        // format=nt: the default graph only (upstream's export).
        let j = api(
            a,
            &[("operation", "export"), ("db_name", "quad"), ("format", "nt")],
        );
        ok(&j);
        let nt = PathBuf::from(string(&j, "filepath"));
        assert!(nt.to_string_lossy().ends_with(".nt"));
        assert_eq!(
            fs::read_to_string(&nt).unwrap(),
            "<http://ex/a> <http://ex/p> <http://ex/b> .\n"
        );
        let j = api(
            a,
            &[("operation", "export"), ("db_name", "quad"), ("format", "turtle")],
        );
        assert_eq!(code(&j), 1003, "{j}");

        // compress=true: `<db>_<ts>.zip` holding the export by name.
        let j = api(
            a,
            &[("operation", "export"), ("db_name", "quad"), ("compress", "true")],
        );
        ok(&j);
        assert_eq!(msg(&j), "Export the database successfully.");
        let zip = PathBuf::from(string(&j, "filepath"));
        let zname = zip.file_name().unwrap().to_string_lossy().into_owned();
        assert!(zname.starts_with("quad_") && zname.ends_with(".zip"), "{zname}");
        let inner = format!("{}.nq", zname.trim_end_matches(".zip"));
        assert_eq!(entries(&zip), [inner.clone()]);
        assert!(!zip.with_file_name(&inner).exists(), "the plain file is removed");
        // It downloads, and it builds a database again (round trip).
        let (status, body) = send(
            a,
            "GET",
            &format!("/file/download/{zname}"),
            "text/plain",
            b"",
        );
        assert_eq!(status, 200);
        assert_eq!(body, fs::read(&zip).unwrap());
        let copy = srv.data_file("export_copy.zip", &body);
        let j = api(
            a,
            &[("operation", "build"), ("db_name", "quad2"), ("db_path", &copy)],
        );
        ok(&j);
        assert_eq!(int(&j, "success_num"), 4);
        ok(&api(a, &[("operation", "load"), ("db_name", "quad2")]));
        assert_eq!(count(a, "quad2"), 1);
        assert_eq!(graph_count(a, "quad2"), 3);

        // A database without named graphs: N-Triples, as upstream.
        let lubm = srv.data_file("lubm.nt", LUBM.as_bytes());
        ok(&api(
            a,
            &[("operation", "build"), ("db_name", "lubm"), ("db_path", &lubm)],
        ));
        ok(&api(a, &[("operation", "load"), ("db_name", "lubm")]));
        let j = api(a, &[("operation", "export"), ("db_name", "lubm"), ("compress", "1")]);
        ok(&j);
        let zip = PathBuf::from(string(&j, "filepath"));
        let only = entries(&zip);
        assert_eq!(only.len(), 1);
        assert!(only[0].ends_with(".nt"), "{only:?}");
    }

    // -----------------------------------------------------------------------
    // CLI: gexport / gbackup / grestore
    // -----------------------------------------------------------------------

    fn run(bin: &str, args: &[&str], cwd: &Path) -> std::process::Output {
        let out = Command::new(bin).args(args).current_dir(cwd).output().unwrap();
        assert!(
            out.status.success(),
            "{bin} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    #[test]
    fn cli_exports_backups_and_restores_zips() {
        let dir = tmp_root("cli");
        fs::write(dir.join("q.nq"), QUADS).unwrap();
        run(env!("CARGO_BIN_EXE_gbuild"), &["q", "q.nq"], &dir);

        // gexport: the format by extension or flag; zipped.
        run(env!("CARGO_BIN_EXE_gexport"), &["q", "out.nq"], &dir);
        assert_eq!(fs::read_to_string(dir.join("out.nq")).unwrap().lines().count(), 4);
        run(env!("CARGO_BIN_EXE_gexport"), &["q", "out.nt"], &dir);
        assert_eq!(fs::read_to_string(dir.join("out.nt")).unwrap().lines().count(), 1);
        run(
            env!("CARGO_BIN_EXE_gexport"),
            &["--format", "nq", "q", "plain"],
            &dir,
        );
        assert_eq!(fs::read_to_string(dir.join("plain")).unwrap().lines().count(), 4);
        run(env!("CARGO_BIN_EXE_gexport"), &["-z", "1", "q", "dump.nq"], &dir);
        assert_eq!(entries(&dir.join("dump.zip")), ["dump.nq"]);
        run(env!("CARGO_BIN_EXE_gexport"), &["q", "auto.zip"], &dir);
        assert_eq!(entries(&dir.join("auto.zip")), ["auto.nq"]);
        assert!(!dir.join("auto.zip.export-tmp").exists());
        let out = Command::new(env!("CARGO_BIN_EXE_gexport"))
            .args(["--format", "xml", "q", "x"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(!out.status.success());

        // gbackup -z → grestore of the archive.
        run(env!("CARGO_BIN_EXE_gbackup"), &["--zip", "q", "bk"], &dir);
        assert!(dir.join("bk.zip").is_file());
        assert!(!dir.join("bk").exists() && !dir.join("bk.zip.backup-tmp").exists());
        run(env!("CARGO_BIN_EXE_grestore"), &["bk.zip", "r"], &dir);
        assert!(!dir.join("r.db.unzip-tmp").exists());
        let restored = gstore::Database::open(dir.join("r.db")).unwrap();
        assert_eq!(restored.triple_num(), 1);
        assert_eq!(
            restored
                .named_graphs()
                .values()
                .map(|g| g.triple_count())
                .sum::<u64>(),
            3
        );
        // The archive is the server's form: the /api restore reads it.
        let srv = start("cli_srv");
        fs::create_dir_all(srv.backup_dir()).unwrap();
        let in_backups = srv.backup_dir().join("quad.db_20240101000000.zip");
        fs::copy(dir.join("bk.zip"), &in_backups).unwrap();
        ok(&api(
            &srv.addr,
            &[
                ("operation", "restore"),
                ("db_name", "quad"),
                ("backup_path", in_backups.to_str().unwrap()),
            ],
        ));
        ok(&api(&srv.addr, &[("operation", "load"), ("db_name", "quad")]));
        assert_eq!(graph_count(&srv.addr, "quad"), 3);
        let _ = fs::remove_dir_all(&dir);
    }
}
