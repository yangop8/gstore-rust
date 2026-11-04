//! Cross-phase integration over `/api`: the write paths Phase 5 added or
//! rewrote meet the Phase 2/3 caches.
//!
//! Every database keeps a byte-bounded query cache (Phase 3) and a CSR for
//! the advanced graph functions (Phase 2, built ahead of time by `loadcsr`,
//! reported lock-free by `show`), both keyed to its data. Each Phase 5 write
//! path — `batchInsert` / `batchRemove` from a file, a directory, a zip
//! package and a remote URL, an uploaded file, SPARQL `LOAD` (now read
//! through the import pipeline), `restore` of a zipped backup, and `build`
//! from several files, a zip package, an uploaded file and a URL after the
//! name was dropped — must leave no stale cached answer, CSR or `csr` flag
//! behind: a cached `COUNT`, a `loadcsr`'d CSR and a graph-function answer
//! computed before the write are never served after it.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::LoadPolicy;
use harness::*;

const BASE: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/b> <http://ex/p> <http://ex/c> .\n";
const EXTRA: &str = "<http://ex/c> <http://ex/p> <http://ex/d> .\n";
const COUNT: &str = "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }";
const REACH: &str =
    "SELECT (shortestPathLen(<http://ex/a>, <http://ex/d>, true, {}) AS ?l) WHERE {}";

/// A server whose sources may come from `data` and from remote URLs.
struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    root: PathBuf,
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn start(tag: &str, data: &Path) -> Srv {
    let root = scratch(tag);
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    config.thread_num = 4;
    let mut policy = LoadPolicy::restricted().with_allowed_dir(data);
    policy.allow_remote = true;
    // The test's HTTP server is on loopback.
    policy.allow_local_hosts = true;
    let server = ApiServer::bind(&root, PASSWORD, "127.0.0.1:0")
        .unwrap()
        .with_config(config)
        .with_load_policy(policy);
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    Srv { server, addr, root }
}

impl Srv {
    fn op(&self, operation: &str, params: &[(&str, &str)]) -> Json {
        let mut body = Json::obj()
            .with("operation", operation)
            .with("username", USERNAME)
            .with("password", PASSWORD);
        for (k, v) in params {
            body.set(*k, *v);
        }
        self.post(body)
    }

    fn post(&self, body: Json) -> Json {
        let (status, text) = http(&self.addr, "POST", "/api", body.to_string().as_bytes());
        assert_eq!(status, 200, "HTTP {status}: {text}");
        json::parse(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
    }

    fn query(&self, db: &str, sparql: &str) -> Json {
        self.op("query", &[("db_name", db), ("sparql", sparql)])
    }

    /// Whether `show` reports `db`'s CSR as built.
    fn csr_shown(&self, db: &str) -> bool {
        let j = self.op("show", &[]);
        ok(&j);
        let list = j.get("ResponseBody").and_then(Json::as_array).unwrap();
        let item = list
            .iter()
            .find(|i| i.get("database").and_then(Json::as_str) == Some(db))
            .unwrap_or_else(|| panic!("{db} not listed: {j}"));
        item.get("csr").and_then(|c| match c {
            Json::Bool(b) => Some(*b),
            other => other.as_str().map(|s| s == "true" || s == "1"),
        }) == Some(true)
    }

    /// `loadcsr`'s message.
    fn loadcsr(&self, db: &str) -> String {
        let j = self.op("loadcsr", &[("db_name", db)]);
        ok(&j);
        string(&j, "StatusMsg")
    }

    /// Upload `data` as `name` (multipart, `/file/upload`); the stored path.
    fn upload(&self, name: &str, data: &[u8]) -> String {
        let boundary = "----gstorecrossphase5";
        let mut body = Vec::new();
        for (k, v) in [("username", USERNAME), ("password", PASSWORD)] {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n"
                )
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
        // (The harness's `http_with` always sends `Content-Type:
        // application/json`.)
        let mut s = TcpStream::connect(self.addr).unwrap();
        let mut req = format!(
            "POST /file/upload HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Content-Type: multipart/form-data; boundary={boundary}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        req.extend_from_slice(&body);
        s.write_all(&req).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let (head, text) = resp.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 200"), "{resp}");
        let j = json::parse(text).unwrap();
        ok(&j);
        string(&j, "filepath")
    }
}

/// The single cell of a one-row answer.
fn cell(j: &Json) -> String {
    assert_eq!(code(j), 0, "{j}");
    let t = answer_table(j);
    assert_eq!(t.rows.len(), 1, "{j}");
    t.rows[0][0].clone().unwrap_or_default()
}

/// `(triple count, whether <a> reaches <d>)` — each asked twice, so the
/// second answer comes from the query cache / the cached CSR.
fn state(srv: &Srv, db: &str) -> (String, bool) {
    let mut seen = Vec::new();
    for _ in 0..2 {
        let n = cell(&srv.query(db, COUNT));
        let reach = cell(&srv.query(db, REACH));
        seen.push((n, reach.contains("\"length\":3")));
    }
    assert_eq!(seen[0], seen[1], "a repeated query answers the same");
    let (n, reach) = seen.pop().unwrap();
    let n = n
        .trim_start_matches('"')
        .split('"')
        .next()
        .unwrap()
        .to_string();
    (n, reach)
}

fn without() -> (String, bool) {
    ("2".to_string(), false)
}

fn with() -> (String, bool) {
    ("3".to_string(), true)
}

/// Warm every cache of `db` (answers cached, CSR built by a graph function
/// and confirmed by `loadcsr` and `show`), run `write`, and check that
/// nothing stale survives it: the CSR is reported unbuilt, `loadcsr`
/// rebuilds it, and the answers are `want`.
fn check(srv: &Srv, db: &str, what: &str, want: (String, bool), write: impl FnOnce()) {
    let before = state(srv, db);
    assert_ne!(before, want, "{what}: the write must change the answers");
    assert_eq!(srv.loadcsr(db), "CSR already loaded.", "{what}");
    assert!(srv.csr_shown(db), "{what}");
    write();
    assert!(!srv.csr_shown(db), "{what}: show reports a stale CSR");
    assert_eq!(srv.loadcsr(db), "CSR loaded successfully.", "{what}");
    assert!(srv.csr_shown(db), "{what}");
    assert_eq!(state(srv, db), want, "{what}: stale answer");
}

/// A one-request-per-connection HTTP server answering `GET <path>` from
/// `files`.
fn http_server(files: Vec<(&'static str, Vec<u8>)>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut reader = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                    break;
                }
            }
            let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
            match files.iter().find(|(p, _)| *p == path) {
                Some((_, body)) => {
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                    );
                    let _ = s.write_all(body);
                }
                None => {
                    let _ = s.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
        }
    });
    addr
}

fn write_file(dir: &Path, name: &str, content: &str) -> String {
    let p = dir.join(name);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, content).unwrap();
    path_str(&p)
}

#[test]
fn phase5_batch_and_load_paths_leave_no_stale_cache_or_csr() {
    let data = scratch("x5_batch_data");
    let base = write_file(&data, "base.nt", BASE);
    let extra = write_file(&data, "extra.nt", EXTRA);
    let extra_dir = data.join("extra_dir");
    write_file(&extra_dir, "extra.ttl", EXTRA);
    let http = http_server(vec![("/extra.nt", EXTRA.as_bytes().to_vec())]);
    let url = format!("http://{http}/extra.nt");
    let srv = start("x5_batch", &data);
    ok(&srv.op("build", &[("db_name", "cdb"), ("db_path", &base)]));
    ok(&srv.op("load", &[("db_name", "cdb")]));
    assert_eq!(state(&srv, "cdb"), without());

    let batch = |op: &str, params: &[(&str, &str)]| {
        let mut all = vec![("db_name", "cdb")];
        all.extend_from_slice(params);
        let j = srv.op(op, &all);
        ok(&j);
        assert_eq!(int(&j, "success_num"), 1, "{op} {params:?}: {j}");
    };

    // A file.
    check(&srv, "cdb", "batchInsert file", with(), || {
        batch("batchInsert", &[("file", &extra)])
    });
    check(&srv, "cdb", "batchRemove file", without(), || {
        batch("batchRemove", &[("file", &extra)])
    });
    // A directory (every RDF file below it).
    let dir = path_str(&extra_dir);
    check(&srv, "cdb", "batchInsert dir", with(), || {
        batch("batchInsert", &[("dir", &dir)])
    });
    check(&srv, "cdb", "batchRemove dir", without(), || {
        batch("batchRemove", &[("dir", &dir)])
    });
    // A remote URL.
    check(&srv, "cdb", "batchInsert remote", with(), || {
        batch("batchInsert", &[("file", &url), ("remote", "true")])
    });
    check(&srv, "cdb", "batchRemove remote", without(), || {
        batch("batchRemove", &[("file", &url), ("remote", "true")])
    });
    // An uploaded file.
    let uploaded = srv.upload("extra.nt", EXTRA.as_bytes());
    check(&srv, "cdb", "batchInsert uploaded", with(), || {
        batch("batchInsert", &[("file", &uploaded)])
    });
    check(&srv, "cdb", "batchRemove uploaded", without(), || {
        batch("batchRemove", &[("file", &uploaded)])
    });
    // A zip package.
    #[cfg(feature = "zip")]
    {
        let pkg = data.join("extra.zip");
        gstore::zipio::pack_dir(&extra_dir, &pkg).unwrap();
        let pkg = path_str(&pkg);
        check(&srv, "cdb", "batchInsert zip", with(), || {
            batch("batchInsert", &[("file", &pkg)])
        });
        check(&srv, "cdb", "batchRemove zip", without(), || {
            batch("batchRemove", &[("file", &pkg)])
        });
    }
    // SPARQL LOAD, read through the import pipeline (a Turtle file, by its
    // extension), and its INTO GRAPH form (the default graph is unchanged,
    // the CSR is rebuilt anyway).
    let ttl = write_file(&data, "extra_load.ttl", EXTRA);
    check(&srv, "cdb", "LOAD", with(), || {
        ok(&srv.query("cdb", &format!("LOAD <file://{ttl}>")));
    });
    check(&srv, "cdb", "DELETE DATA", without(), || {
        ok(&srv.query(
            "cdb",
            "DELETE DATA { <http://ex/c> <http://ex/p> <http://ex/d> }",
        ));
    });
    ok(&srv.query(
        "cdb",
        &format!("LOAD <file://{ttl}> INTO GRAPH <http://ex/g>"),
    ));
    assert!(
        !srv.csr_shown("cdb"),
        "a named-graph LOAD clears the CSR too"
    );
    assert_eq!(state(&srv, "cdb"), without());
    // A failed LOAD changes nothing, so it may keep the cache.
    let bad = write_file(
        &data,
        "bad.nt",
        "<http://ex/c> <http://ex/p> <http://ex/d> .\n<x> .\n",
    );
    assert_ne!(code(&srv.query("cdb", &format!("LOAD <{bad}>"))), 0);
    assert_eq!(state(&srv, "cdb"), without());
}

#[test]
fn phase5_build_and_restore_paths_leave_no_stale_cache_or_csr() {
    let data = scratch("x5_build_data");
    let base = write_file(&data, "base.nt", BASE);
    let both = write_file(&data, "both.nt", &format!("{BASE}{EXTRA}"));
    let part1 = write_file(&data, "dir/1.nt", BASE);
    let part2 = write_file(&data, "dir/2.ttl", EXTRA);
    let http = http_server(vec![("/both.nt", format!("{BASE}{EXTRA}").into_bytes())]);
    let url = format!("http://{http}/both.nt");
    let srv = start("x5_build", &data);
    ok(&srv.op("build", &[("db_name", "bdb"), ("db_path", &base)]));
    ok(&srv.op("load", &[("db_name", "bdb")]));
    let uploaded = srv.upload("both.nt", format!("{BASE}{EXTRA}").as_bytes());

    // Each source builds `b` anew after the old one (cached, CSR loaded) was
    // dropped: nothing of the old database may answer.
    let rebuild = |what: &str, build: &dyn Fn() -> Json| {
        assert_eq!(state(&srv, "bdb"), without(), "{what}: before");
        assert_eq!(srv.loadcsr("bdb"), "CSR already loaded.");
        srv_drop(&srv, "bdb");
        ok(&build());
        ok(&srv.op("load", &[("db_name", "bdb")]));
        assert!(
            !srv.csr_shown("bdb"),
            "{what}: a new database has no CSR yet"
        );
        assert_eq!(state(&srv, "bdb"), with(), "{what}: stale answer");
        // Back to the base data for the next round.
        srv_drop(&srv, "bdb");
        ok(&srv.op("build", &[("db_name", "bdb"), ("db_path", &base)]));
        ok(&srv.op("load", &[("db_name", "bdb")]));
    };
    rebuild("build from several files", &|| {
        let body = Json::obj()
            .with("operation", "build")
            .with("username", USERNAME)
            .with("password", PASSWORD)
            .with("db_name", "bdb")
            .with(
                "db_path",
                Json::Arr(vec![Json::from(part1.as_str()), Json::from(part2.as_str())]),
            );
        srv.post(body)
    });
    rebuild("build from an uploaded file", &|| {
        srv.op("build", &[("db_name", "bdb"), ("db_path", &uploaded)])
    });
    rebuild("build from a URL", &|| {
        srv.op(
            "build",
            &[("db_name", "bdb"), ("db_path", &url), ("remote", "true")],
        )
    });
    #[cfg(feature = "zip")]
    {
        let pkg = data.join("both.zip");
        gstore::zipio::pack_dir(&data.join("dir"), &pkg).unwrap();
        let pkg = path_str(&pkg);
        rebuild("build from a zip package", &|| {
            srv.op("build", &[("db_name", "bdb"), ("db_path", &pkg)])
        });

        // restore of a zipped backup swaps the loaded database.
        ok(&srv.op("batchInsert", &[("db_name", "bdb"), ("file", &both)]));
        let j = srv.op("backup", &[("db_name", "bdb"), ("compress", "true")]);
        ok(&j);
        let zip = string(&j, "backupfilepath");
        ok(&srv.query(
            "bdb",
            "DELETE DATA { <http://ex/c> <http://ex/p> <http://ex/d> }",
        ));
        check(&srv, "bdb", "restore of a zipped backup", with(), || {
            ok(&srv.op("restore", &[("db_name", "bdb"), ("backup_path", &zip)]));
        });
    }
    let _ = both;
}

fn srv_drop(srv: &Srv, db: &str) {
    ok(&srv.op("unload", &[("db_name", db)]));
    ok(&srv.op("drop", &[("db_name", db), ("is_backup", "false")]));
}
