//! `conf.ini` across the phases: Phase 3's `[query]` section reaches every
//! database Phase 5's new paths open or create, `refreshconf` updates them,
//! and `refreshconf` re-reads Phase 5's diagnostic-log configuration as
//! upstream does (its `Util::configure` re-runs log4cplus' `doConfigure` on
//! the `log_mode` file).

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
use gstore::logging::{self, LogConfig};
use gstore::LoadPolicy;
use harness::*;

const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n";
const DIV: &str = "SELECT ((7/2) AS ?x) WHERE {}";

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

/// A server configured from `conf` (so `refreshconf` re-reads it), whose
/// sources may come from `data` and from remote URLs.
fn start(tag: &str, conf: &Path, data: &Path, log_refresh: bool) -> Srv {
    let root = scratch(tag);
    let config = GStoreConfig::load(conf).unwrap();
    let mut policy = LoadPolicy::restricted().with_allowed_dir(data);
    policy.allow_remote = true;
    let mut server = ApiServer::bind(&root, PASSWORD, "127.0.0.1:0")
        .unwrap()
        .with_config(config)
        .with_load_policy(policy);
    if log_refresh {
        server = server.with_log_refresh();
    }
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

    /// `7/2` as database `db` answers it.
    fn div(&self, db: &str) -> String {
        let j = self.op("query", &[("db_name", db), ("sparql", DIV)]);
        ok(&j);
        answer_table(&j).rows[0][0].clone().unwrap_or_default()
    }

    fn upload(&self, name: &str, data: &[u8]) -> String {
        let boundary = "----gstoresettings5";
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
        let (_, text) = resp.split_once("\r\n\r\n").unwrap();
        let j = json::parse(text).unwrap();
        ok(&j);
        string(&j, "filepath")
    }
}

fn http_server(path: &'static str, body: Vec<u8>) -> SocketAddr {
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
            let found = first.split_whitespace().nth(1) == Some(path);
            let payload: &[u8] = if found { &body } else { b"" };
            let status = if found { "200 OK" } else { "404 Not Found" };
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                )
                .as_bytes(),
            );
            let _ = s.write_all(payload);
        }
    });
    addr
}

fn write_conf(conf: &Path, division: &str) {
    fs::write(
        conf,
        format!(
            "[server]\nip_deny_path=off\nthread_num=4\n\
             [query]\ninteger_division={division}\n"
        ),
    )
    .unwrap();
}

fn truncated(cell: &str) -> bool {
    cell.starts_with("\"3\"")
}

fn decimal(cell: &str) -> bool {
    cell.starts_with("\"3.5\"")
}

#[test]
fn query_settings_reach_every_database_phase5_opens_and_refreshconf_updates_them() {
    let data = scratch("set5_data");
    let conf = data.join("conf.ini");
    write_conf(&conf, "truncate");
    let p1 = data.join("p1.nt");
    let p2 = data.join("p2.ttl");
    fs::write(&p1, DATA).unwrap();
    fs::write(&p2, "<http://ex/b> <http://ex/p> <http://ex/c> .\n").unwrap();
    let http = http_server("/r.nt", DATA.as_bytes().to_vec());
    let url = format!("http://{http}/r.nt");
    let srv = start("set5", &conf, &data, false);
    let uploaded = srv.upload("u.nt", DATA.as_bytes());

    let mut dbs: Vec<&str> = Vec::new();
    // build from several files (gconsole's create from a directory).
    ok(&srv.post(
        Json::obj()
            .with("operation", "build")
            .with("username", USERNAME)
            .with("password", PASSWORD)
            .with("db_name", "multi")
            .with(
                "db_path",
                Json::Arr(vec![
                    Json::from(path_str(&p1).as_str()),
                    Json::from(path_str(&p2).as_str()),
                ]),
            ),
    ));
    dbs.push("multi");
    ok(&srv.op("build", &[("db_name", "uploaded"), ("db_path", &uploaded)]));
    dbs.push("uploaded");
    ok(&srv.op(
        "build",
        &[("db_name", "remote"), ("db_path", &url), ("remote", "true")],
    ));
    dbs.push("remote");
    #[cfg(feature = "zip")]
    let zip_backup = {
        let pkg_dir = data.join("pkg");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(pkg_dir.join("x.nt"), DATA).unwrap();
        let pkg = data.join("pkg.zip");
        gstore::zipio::pack_dir(&pkg_dir, &pkg).unwrap();
        ok(&srv.op(
            "build",
            &[("db_name", "zipped"), ("db_path", &path_str(&pkg))],
        ));
        dbs.push("zipped");
        ok(&srv.op("load", &[("db_name", "zipped")]));
        let j = srv.op("backup", &[("db_name", "zipped"), ("compress", "true")]);
        ok(&j);
        string(&j, "backupfilepath")
    };
    for db in &dbs {
        ok(&srv.op("load", &[("db_name", db)]));
        assert!(truncated(&srv.div(db)), "{db}: {}", srv.div(db));
    }
    // restore of a zipped backup: the restored database gets the settings.
    #[cfg(feature = "zip")]
    {
        ok(&srv.op(
            "restore",
            &[("db_name", "zipped"), ("backup_path", &zip_backup)],
        ));
        assert!(truncated(&srv.div("zipped")), "{}", srv.div("zipped"));
    }

    // refreshconf: the loaded databases (whose `7/2` is cached) take the new
    // settings, and so does every database opened from now on.
    write_conf(&conf, "decimal");
    let j = srv.op("refreshconf", &[]);
    ok(&j);
    for db in &dbs {
        assert!(
            decimal(&srv.div(db)),
            "{db} after refreshconf: {}",
            srv.div(db)
        );
    }
    ok(&srv.op("build", &[("db_name", "later"), ("db_path", &uploaded)]));
    ok(&srv.op("load", &[("db_name", "later")]));
    assert!(decimal(&srv.div("later")), "{}", srv.div("later"));
    #[cfg(feature = "zip")]
    {
        ok(&srv.op(
            "restore",
            &[("db_name", "zipped"), ("backup_path", &zip_backup)],
        ));
        assert!(decimal(&srv.div("zipped")), "{}", srv.div("zipped"));
    }
    let _ = fs::remove_dir_all(&data);
}

#[test]
fn refreshconf_reconfigures_the_diagnostic_log_like_upstream() {
    let data = scratch("set5_log");
    let conf = data.join("conf.ini");
    let a = data.join("a.log");
    let b = data.join("b.log");
    let write = |file: &Path, level: &str| {
        fs::write(
            &conf,
            format!(
                "[server]\nip_deny_path=off\nthread_num=4\n\
                 [log]\nlog_mode=none\nlog_level={level}\nlog_file={}\nlog_stderr=off\n",
                file.display()
            ),
        )
        .unwrap();
    };
    write(&a, "info");
    // What gserver does at startup: install the log from conf.ini, then let
    // refreshconf re-read it.
    let (cfg, _) = LogConfig::from_gstore_config(&GStoreConfig::load(&conf).unwrap());
    logging::init(&cfg).unwrap();
    let srv = start("set5_log", &conf, &data, true);
    gstore::gs_info!("first record");
    gstore::gs_debug!("not recorded at info");

    // Unchanged configuration: the installed logger stays.
    let installed = logging::current().unwrap();
    ok(&srv.op("refreshconf", &[]));
    assert!(Arc::ptr_eq(&installed, &logging::current().unwrap()));

    // Another file and level: re-installed.
    write(&b, "debug");
    ok(&srv.op("refreshconf", &[]));
    gstore::gs_debug!("second record");
    let current = logging::current().unwrap();
    assert_eq!(current.file_path(), Some(b.as_path()));
    let a_text = fs::read_to_string(&a).unwrap();
    let b_text = fs::read_to_string(&b).unwrap();
    assert!(a_text.contains("first record"), "{a_text}");
    assert!(!a_text.contains("not recorded"), "{a_text}");
    assert!(!a_text.contains("second record"), "{a_text}");
    assert!(b_text.contains("second record"), "{b_text}");
    assert!(b_text.contains("diagnostic log reconfigured"), "{b_text}");

    // A server that does not own the log (the default) leaves it alone.
    drop(srv);
    let srv = start("set5_log2", &conf, &data, false);
    write(&a, "info");
    ok(&srv.op("refreshconf", &[]));
    assert_eq!(logging::current().unwrap().file_path(), Some(b.as_path()));
    drop(srv);
    logging::reset();
    let _ = fs::remove_dir_all(&data);
}
