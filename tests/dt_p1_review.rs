//! Regression tests for the Phase 1 review findings on the upstream `/api`
//! layer and the REST endpoints sharing its listener:
//!
//! * wire compatibility — the encoded `?operation=…` of the Java / Node.js /
//!   C++ / PHP SDKs, trailing slashes, `init` / `refreshconf`, unknown query
//!   formats, database names with dots or in other scripts;
//! * access control — asynchronous results kept out of the public export
//!   directory, restores confined to a database's own backups, backup names
//!   that impersonate another database's, the IP lists and the lockout on
//!   every endpoint, host-local callbacks, stale grants of a database dropped
//!   outside the server, the REST listings, the reserved system user;
//! * transactions — per-database ids addressed without a database, `HEAD`
//!   on a kept-alive connection.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};

const DATA: &str = "<http://ex/a> <http://ex/p> \"TOPSECRET\" .\n";
const MINE: &str = "<http://ex/m> <http://ex/p> \"harmless\" .\n";

fn tmp_root(tag: &str) -> PathBuf {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("gstore_p1rev_{tag}_{}_{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Srv {
    _server: Arc<ApiServer>,
    addr: SocketAddr,
    root: PathBuf,
}

fn start_cfg(tag: &str, cfg: impl FnOnce(GStoreConfig) -> GStoreConfig) -> Srv {
    let root = tmp_root(tag);
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    let config = cfg(config);
    let server = ApiServer::bind(&root, "root", "127.0.0.1:0")
        .unwrap()
        .with_config(config);
    serve(server, root)
}

fn serve(server: ApiServer, root: PathBuf) -> Srv {
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    Srv {
        _server: server,
        addr,
        root,
    }
}

fn start(tag: &str) -> Srv {
    start_cfg(tag, |c| c)
}

/// One request (`Connection: close`); returns `(status, head, body)`.
fn send(
    addr: &SocketAddr,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut raw = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((&resp, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

fn parse(body: &str) -> Json {
    json::parse(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

/// `POST /api` with string parameters as a JSON object.
fn api(addr: &SocketAddr, pairs: &[(&str, &str)]) -> Json {
    let items: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}:{}", json::quote(k), json::quote(v)))
        .collect();
    let body = format!("{{{}}}", items.join(","));
    let (status, _, body) = send(
        addr,
        "POST",
        "/api",
        &[("Content-Type", "application/json")],
        &body,
    );
    assert_eq!(status, 200, "{body}");
    parse(&body)
}

/// [`api`] as `user` (password `pw`, `root` for root).
fn as_user(addr: &SocketAddr, user: &str, pairs: &[(&str, &str)]) -> Json {
    let pw = if user == "root" { "root" } else { "pw" };
    let mut all = vec![("username", user), ("password", pw)];
    all.extend_from_slice(pairs);
    api(addr, &all)
}

fn root(addr: &SocketAddr, pairs: &[(&str, &str)]) -> Json {
    as_user(addr, "root", pairs)
}

fn code(j: &Json) -> i64 {
    j.get("StatusCode")
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no StatusCode: {j}"))
}

fn msg(j: &Json) -> String {
    j.get("StatusMsg")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string()
}

fn ok(j: &Json) {
    assert_eq!(code(j), 0, "{j}");
}

fn data_file(srv: &Srv, name: &str, text: &str) -> String {
    let dir = srv.root.join("upload");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p.to_string_lossy().into_owned()
}

fn build(srv: &Srv, db: &str, text: &str) {
    let file = data_file(srv, &format!("{db}.nt"), text);
    ok(&root(
        &srv.addr,
        &[("operation", "build"), ("db_name", db), ("db_path", &file)],
    ));
}

fn add_user(addr: &SocketAddr, name: &str) {
    ok(&root(
        addr,
        &[
            ("operation", "usermanage"),
            ("type", "1"),
            ("op_username", name),
            ("op_password", "pw"),
        ],
    ));
}

/// Grant `privilege` (upstream codes: 1 query, 2 load, 3 unload, 4 update,
/// 5 backup, 6 restore, 7 export) on `db`.
fn grant(addr: &SocketAddr, user: &str, db: &str, privilege: &str) {
    ok(&root(
        addr,
        &[
            ("operation", "userprivilegemanage"),
            ("type", "1"),
            ("op_username", user),
            ("privileges", privilege),
            ("db_name", db),
        ],
    ));
}

fn select_values(j: &Json) -> Vec<String> {
    j.get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(Json::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.as_object())
                .flat_map(|r| {
                    r.iter()
                        .filter_map(|(_, v)| v.get("value").and_then(Json::as_str))
                })
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn basic(user: &str, pw: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{user}:{pw}");
    let b = raw.as_bytes();
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    format!("Basic {out}")
}

fn rest(
    addr: &SocketAddr,
    user: &str,
    pw: &str,
    method: &str,
    target: &str,
    body: &str,
) -> (u16, String) {
    let auth = basic(user, pw);
    let (s, _, b) = send(addr, method, target, &[("Authorization", &auth)], body);
    (s, b)
}

// ---------------------------------------------------------------------------
// wire compatibility
// ---------------------------------------------------------------------------

/// The Java / Node.js / C++ / PHP SDKs URL-encode the whole `?operation=…`
/// string of their default GET requests.
#[test]
fn an_encoded_query_string_reaches_the_api() {
    let srv = start("encoded");
    let (s, _, body) = send(
        &srv.addr,
        "GET",
        "/api%3Foperation%3Dlogin%26username%3Droot%26password%3Droot",
        &[],
        "",
    );
    let j = parse(&body);
    assert_eq!(
        (s, code(&j), msg(&j).as_str()),
        (200, 0, "login successfully"),
        "{body}"
    );
    let (_, _, body) = send(&srv.addr, "GET", "/api%3Foperation%3Dcheck", &[], "");
    assert_eq!(code(&parse(&body)), 0, "{body}");
    // Java's URLEncoder: spaces as `+`, `?` / `{` / `}` encoded.
    build(&srv, "enc", DATA);
    ok(&root(
        &srv.addr,
        &[("operation", "load"), ("db_name", "enc")],
    ));
    let (_, _, body) = send(
        &srv.addr,
        "GET",
        "/api%3Foperation%3Dquery%26username%3Droot%26password%3Droot%26db_name%3Denc\
         %26format%3Djson%26sparql%3Dselect+%3Fo+where+%7B+%3Fs+%3Fp+%3Fo+%7D",
        &[],
        "",
    );
    let j = parse(&body);
    assert_eq!(select_values(&j), ["TOPSECRET"], "{j}");
}

#[test]
fn trailing_slashes_are_ignored_like_upstream() {
    let srv = start("slash");
    let (s, _, body) = send(&srv.addr, "GET", "/api/?operation=check", &[], "");
    assert_eq!((s, code(&parse(&body))), (200, 0), "{body}");
    let (s, _, body) = send(
        &srv.addr,
        "POST",
        "/grpc/api/",
        &[("Content-Type", "application/json")],
        r#"{"operation":"check"}"#,
    );
    assert_eq!((s, code(&parse(&body))), (200, 0), "{body}");
    // /file/upload/ is the upload route (credentials missing → its 1003).
    let (s, _, body) = send(&srv.addr, "POST", "/file/upload/", &[], "");
    assert_eq!((s, code(&parse(&body))), (200, 1003), "{body}");
}

#[test]
fn init_registers_existing_directories_and_refreshconf_rereads_the_file() {
    let dir = tmp_root("refreshconf");
    let conf = dir.join("conf.ini");
    let home = dir.join("home");
    let write_conf = |max_dbs: usize| {
        std::fs::write(
            &conf,
            format!(
                "[server]\nport=0\nmax_database_num={max_dbs}\nip_deny_path=off\n\
                 [system]\ndb_home={}\n",
                home.display()
            ),
        )
        .unwrap();
    };
    write_conf(100);
    let mut cfg = GStoreConfig::load(&conf).unwrap();
    cfg.listen_addr = "127.0.0.1".to_string();
    cfg.root_password = "root".to_string();
    let server = ApiServer::bind_with_config(cfg).unwrap();
    let srv = serve(server, home.clone());
    build(&srv, "one", DATA);

    // refreshconf: a lower max_database_num takes effect without a restart.
    write_conf(1);
    let j = root(&srv.addr, &[("operation", "refreshconf")]);
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (0, "refreshing configuration success"),
        "{j}"
    );
    let file = data_file(&srv, "two.nt", DATA);
    let j = root(
        &srv.addr,
        &[
            ("operation", "build"),
            ("db_name", "two"),
            ("db_path", &file),
        ],
    );
    assert_eq!(code(&j), 1004, "{j}");
    assert!(msg(&j).contains("max_databse_num"), "{j}");
    // Root only.
    add_user(&srv.addr, "alice");
    assert_eq!(
        code(&as_user(
            &srv.addr,
            "alice",
            &[("operation", "refreshconf")]
        )),
        1004
    );

    // init: an existing directory (copied in outside the server) registers.
    let copy = home.join("copied.db");
    std::fs::create_dir_all(&copy).unwrap();
    for e in std::fs::read_dir(home.join("one.db")).unwrap().flatten() {
        std::fs::copy(e.path(), copy.join(e.file_name())).unwrap();
    }
    std::fs::create_dir_all(home.join("empty.db")).unwrap();
    let j = root(
        &srv.addr,
        &[("operation", "init"), ("db_names", "copied,one,nope,empty")],
    );
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (0, "init database successfully!"),
        "{j}"
    );
    let data = j.get("data").and_then(Json::as_array).unwrap();
    let got: Vec<(String, String, String)> = data
        .iter()
        .map(|d| {
            let f = |k: &str| d.get(k).and_then(Json::as_str).unwrap().to_string();
            (f("db_name"), f("status"), f("msg"))
        })
        .collect();
    assert_eq!(
        got,
        [
            ("copied".into(), "0".into(), "success".into()),
            ("one".into(), "1".into(), "exist".into()),
            ("nope".into(), "1".into(), "nope.db is not exist.".into()),
            ("empty".into(), "1".into(), "empty.db is not exist.".into()),
        ]
    );
    let j = root(&srv.addr, &[("operation", "init")]);
    assert_eq!(code(&j), 1003, "{j}");
}

/// Upstream checks `format` only when it answers a read, after running it
/// (`1005`), and ignores it for an update.
#[test]
fn an_unknown_format_is_ignored_by_an_update_and_fails_a_read_with_1005() {
    let srv = start("format");
    build(&srv, "fmt", DATA);
    ok(&root(
        &srv.addr,
        &[("operation", "load"), ("db_name", "fmt")],
    ));
    let q = |format: &str, sparql: &str| {
        root(
            &srv.addr,
            &[
                ("operation", "query"),
                ("db_name", "fmt"),
                ("format", format),
                ("sparql", sparql),
            ],
        )
    };
    let j = q(
        "html",
        "INSERT DATA { <http://ex/n> <http://ex/p> <http://ex/o> . }",
    );
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (0, "update query returns true."),
        "{j}"
    );
    let j = q("json", "ASK { <http://ex/n> <http://ex/p> <http://ex/o> }");
    assert!(j.to_string().contains("true"), "the insert applied: {j}");
    let j = q("html", "SELECT ?s WHERE { ?s ?p ?o }");
    assert_eq!(
        (code(&j), msg(&j).as_str()),
        (1005, "Unknown result format."),
        "{j}"
    );
}

#[test]
fn database_names_may_hold_dots_and_other_scripts() {
    let srv = start("names");
    for db in ["lubm.v2", "测试库"] {
        build(&srv, db, DATA);
        ok(&root(&srv.addr, &[("operation", "load"), ("db_name", db)]));
        let j = root(
            &srv.addr,
            &[
                ("operation", "query"),
                ("db_name", db),
                ("sparql", "SELECT ?o WHERE { ?s ?p ?o }"),
            ],
        );
        assert_eq!(select_values(&j), ["TOPSECRET"], "{db}: {j}");
    }
    let j = root(&srv.addr, &[("operation", "show")]);
    let text = j.to_string();
    assert!(
        text.contains("lubm.v2") && text.contains("测试库"),
        "{text}"
    );
    for bad in ["../x", ".hidden", "a..b", "a b"] {
        let j = root(&srv.addr, &[("operation", "load"), ("db_name", bad)]);
        assert_eq!(code(&j), 1003, "{bad}: {j}");
    }
}

// ---------------------------------------------------------------------------
// access control
// ---------------------------------------------------------------------------

#[test]
fn asynchronous_query_results_are_not_publicly_downloadable() {
    let srv = start("asyncdl");
    build(&srv, "secret", DATA);
    ok(&root(
        &srv.addr,
        &[("operation", "load"), ("db_name", "secret")],
    ));
    add_user(&srv.addr, "alice");
    let j = root(
        &srv.addr,
        &[
            ("operation", "query"),
            ("db_name", "secret"),
            ("sparql", "SELECT ?o WHERE { ?s ?p ?o }"),
            ("async", "true"),
        ],
    );
    let opt_id = j.get("opt_id").and_then(Json::as_str).unwrap().to_string();
    let t0 = Instant::now();
    loop {
        let j = root(
            &srv.addr,
            &[("operation", "checkOperationState"), ("opt_id", &opt_id)],
        );
        if j.get("state").and_then(Json::as_i64) == Some(1) {
            assert_eq!(select_values(&j), ["TOPSECRET"], "the owner reads it: {j}");
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "{j}");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(srv
        .root
        .join("tasks")
        .join(format!("{opt_id}.json"))
        .is_file());
    // Nobody downloads it: neither anonymously nor as another user.
    for target in [
        format!("/file/download/{opt_id}.json"),
        format!("/file/download/export/{opt_id}.json"),
    ] {
        let (s, _, body) = send(&srv.addr, "GET", &target, &[], "");
        assert!(!body.contains("TOPSECRET"), "{target}: {body}");
        assert_eq!((s, code(&parse(&body))), (200, 11), "{target}: {body}");
    }
    let j = as_user(
        &srv.addr,
        "alice",
        &[("operation", "checkOperationState"), ("opt_id", &opt_id)],
    );
    assert!(!j.to_string().contains("TOPSECRET"), "{j}");
}

/// The restore privilege on one database restores that database's own
/// backups only.
#[test]
fn restore_is_confined_to_the_databases_own_backups() {
    let srv = start("restore");
    let a = &srv.addr;
    build(&srv, "secret", DATA);
    build(&srv, "mine", MINE);
    add_user(a, "alice");
    grant(a, "alice", "mine", "1");
    grant(a, "alice", "mine", "5");
    grant(a, "alice", "mine", "6");
    let j = root(a, &[("operation", "backup"), ("db_name", "secret")]);
    ok(&j);
    let secret_backup = j
        .get("backupfilepath")
        .and_then(Json::as_str)
        .unwrap()
        .to_string();
    let name = Path::new(&secret_backup)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    // By name, by path, and over REST: refused.
    for path in [name.as_str(), secret_backup.as_str()] {
        let j = as_user(
            a,
            "alice",
            &[
                ("operation", "restore"),
                ("db_name", "mine"),
                ("backup_path", path),
            ],
        );
        assert_eq!(code(&j), 1004, "{path}: {j}");
    }
    let (s, body) = rest(
        a,
        "alice",
        "pw",
        "POST",
        &format!("/restore?db_name=mine&path={name}"),
        "",
    );
    assert_eq!(s, 403, "{body}");
    // Her own database's backup restores.
    let j = as_user(a, "alice", &[("operation", "backup"), ("db_name", "mine")]);
    ok(&j);
    let mine_backup = j
        .get("backupfilepath")
        .and_then(Json::as_str)
        .unwrap()
        .to_string();
    let j = as_user(
        a,
        "alice",
        &[
            ("operation", "restore"),
            ("db_name", "mine"),
            ("backup_path", &mine_backup),
        ],
    );
    ok(&j);
    ok(&root(a, &[("operation", "load"), ("db_name", "mine")]));
    let j = as_user(
        a,
        "alice",
        &[
            ("operation", "query"),
            ("db_name", "mine"),
            ("sparql", "SELECT ?o WHERE { ?s ?p ?o }"),
        ],
    );
    assert_eq!(select_values(&j), ["harmless"], "{j}");

    // A global restore grant cannot clone another database either.
    let (s, body) = rest(
        a,
        "root",
        "root",
        "POST",
        "/user/create?op_username=bob&op_password=pw&privileges=restore",
        "",
    );
    assert_eq!(s, 200, "{body}");
    let j = as_user(
        a,
        "bob",
        &[
            ("operation", "restore"),
            ("db_name", "clone"),
            ("backup_path", &name),
        ],
    );
    assert_eq!(code(&j), 1004, "{j}");
    // Root may (the restore creates the database, without granting it).
    ok(&root(
        a,
        &[
            ("operation", "restore"),
            ("db_name", "clone"),
            ("backup_path", &name),
        ],
    ));
    assert_eq!(
        code(&as_user(
            a,
            "bob",
            &[("operation", "load"), ("db_name", "clone")]
        )),
        1004
    );
}

/// A backup named after another database (`<db>.db_<timestamp>`) is neither
/// written nor taken as that database's newest backup.
#[test]
fn backups_cannot_impersonate_another_databases_backups() {
    let srv = start("fakebackup");
    let a = &srv.addr;
    build(&srv, "secret", DATA);
    build(&srv, "mine", MINE);
    ok(&root(a, &[("operation", "load"), ("db_name", "mine")]));
    add_user(a, "alice");
    grant(a, "alice", "mine", "5");
    ok(&root(a, &[("operation", "backup"), ("db_name", "secret")]));
    // REST: refused.
    let (s, body) = rest(
        a,
        "alice",
        "pw",
        "POST",
        "/backup?db_name=mine&path=secret.db_20991231235959",
        "",
    );
    assert_eq!(s, 409, "{body}");
    // /api: refused.
    let j = as_user(
        a,
        "alice",
        &[
            ("operation", "backup"),
            ("db_name", "mine"),
            ("backup_path", "secret.db_20991231235959"),
        ],
    );
    assert_eq!(code(&j), 1003, "{j}");
    // Planted by hand: a copy of `mine`'s backup renamed. It still records
    // `mine` and is no backup of `secret`.
    let j = root(a, &[("operation", "backup"), ("db_name", "mine")]);
    let mine_backup = PathBuf::from(j.get("backupfilepath").and_then(Json::as_str).unwrap());
    let fake = srv.root.join("backups").join("secret.db_20991231235959");
    std::fs::create_dir_all(&fake).unwrap();
    for e in std::fs::read_dir(&mine_backup).unwrap().flatten() {
        std::fs::copy(e.path(), fake.join(e.file_name())).unwrap();
    }
    let j = root(a, &[("operation", "backuppath"), ("db_name", "secret")]);
    assert!(!j.to_string().contains("2099"), "{j}");
    ok(&root(a, &[("operation", "restore"), ("db_name", "secret")]));
    ok(&root(a, &[("operation", "load"), ("db_name", "secret")]));
    let j = root(
        a,
        &[
            ("operation", "query"),
            ("db_name", "secret"),
            ("sparql", "SELECT ?o WHERE { ?s ?p ?o }"),
        ],
    );
    assert_eq!(select_values(&j), ["TOPSECRET"], "{j}");
}

#[test]
fn the_ip_lists_and_the_lockout_hold_on_the_rest_endpoints() {
    let dir = tmp_root("ipdeny");
    let deny = dir.join("deny.config");
    std::fs::write(&deny, "127.0.0.1\n").unwrap();
    let srv = start_cfg("ipdeny_srv", |mut c| {
        c.ip_deny_path = Some(deny.clone());
        c
    });
    let (s, _, body) = send(&srv.addr, "GET", "/api?operation=check", &[], "");
    assert_eq!((s, code(&parse(&body))), (200, 1101), "{body}");
    let (s, body) = rest(&srv.addr, "root", "root", "GET", "/show", "");
    assert_eq!(s, 403, "{body}");
    let (s, _, body) = send(
        &srv.addr,
        "POST",
        "/login?username=root&password=root",
        &[],
        "",
    );
    assert_eq!(s, 403, "{body}");
    assert!(!body.contains("session"), "{body}");

    // The lockout: failed Basic logins count, and lock REST out too.
    let srv = start("lockout");
    for i in 0..5 {
        let (s, _) = rest(&srv.addr, "root", &format!("bad{i}"), "GET", "/show", "");
        assert_eq!(s, 401);
    }
    let (s, body) = rest(&srv.addr, "root", "root", "GET", "/show", "");
    assert_eq!(s, 403, "locked out: {body}");
    let j = root(&srv.addr, &[("operation", "show")]);
    assert_eq!(code(&j), 1101, "{j}");
    // /shutdown honours it as well.
    let (_, _, body) = send(
        &srv.addr,
        "GET",
        "/shutdown?username=root&password=bad",
        &[],
        "",
    );
    assert_eq!(code(&parse(&body)), 1101, "{body}");
    // A successful login unlocks the address (upstream).
    let (s, _, body) = send(
        &srv.addr,
        "POST",
        "/login?username=root&password=root",
        &[],
        "",
    );
    assert_eq!(s, 200, "{body}");
    let (s, _) = rest(&srv.addr, "root", "root", "GET", "/show", "");
    assert_eq!(s, 200);
}

#[test]
fn callbacks_to_host_local_addresses_are_refused() {
    let srv = start("callback");
    build(&srv, "cbdb", DATA);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    for url in [
        format!("http://127.0.0.1:{port}/api?operation=usermanage&inner=true"),
        format!("http://localhost:{port}/x"),
        "http://169.254.169.254/latest/meta-data".to_string(),
        format!("http://[::1]:{port}/x"),
    ] {
        let j = root(
            &srv.addr,
            &[
                ("operation", "load"),
                ("db_name", "cbdb"),
                ("async", "true"),
                ("callback", &url),
            ],
        );
        assert_eq!(code(&j), 1003, "{url}: {j}");
    }
    thread::sleep(Duration::from_millis(200));
    assert!(listener.accept().is_err(), "nothing was delivered");
}

/// Grants on a database dropped outside the server (`gdrop`) do not carry
/// over to a new database of the same name.
#[test]
fn grants_do_not_outlive_a_database_dropped_outside_the_server() {
    // Dropped while the server runs.
    let srv = start("stalegrant");
    let a = &srv.addr;
    build(&srv, "secret", DATA);
    add_user(a, "alice");
    grant(a, "alice", "secret", "1");
    std::fs::remove_dir_all(srv.root.join("secret.db")).unwrap();
    build(&srv, "secret", MINE);
    ok(&root(a, &[("operation", "load"), ("db_name", "secret")]));
    let j = as_user(
        a,
        "alice",
        &[
            ("operation", "query"),
            ("db_name", "secret"),
            ("sparql", "SELECT ?o WHERE { ?s ?p ?o }"),
        ],
    );
    assert_eq!(code(&j), 1004, "{j}");
    // Over REST.
    grant(a, "alice", "secret", "1");
    ok(&root(a, &[("operation", "unload"), ("db_name", "secret")]));
    std::fs::remove_dir_all(srv.root.join("secret.db")).unwrap();
    let (s, body) = rest(a, "root", "root", "POST", "/build?db_name=secret", MINE);
    assert_eq!(s, 200, "{body}");
    let (s, body) = rest(
        a,
        "alice",
        "pw",
        "POST",
        "/sparql?db_name=secret",
        "SELECT ?o WHERE { ?s ?p ?o }",
    );
    assert_eq!(s, 403, "{body}");

    // Dropped while the server is down.
    let home = tmp_root("stalegrant_down");
    {
        let srv = serve(
            ApiServer::bind(&home, "root", "127.0.0.1:0").unwrap(),
            home.clone(),
        );
        build(&srv, "gone", DATA);
        add_user(&srv.addr, "alice");
        grant(&srv.addr, "alice", "gone", "1");
    }
    std::fs::remove_dir_all(home.join("gone.db")).unwrap();
    let srv = serve(
        ApiServer::bind(&home, "root", "127.0.0.1:0").unwrap(),
        home.clone(),
    );
    let j = root(&srv.addr, &[("operation", "showuser")]);
    assert!(!j.to_string().contains("gone"), "{j}");
}

#[test]
fn the_rest_listings_follow_the_privileges() {
    let srv = start("restlist");
    let a = &srv.addr;
    build(&srv, "secret", DATA);
    build(&srv, "mine", MINE);
    for db in ["secret", "mine"] {
        ok(&root(a, &[("operation", "load"), ("db_name", db)]));
    }
    add_user(a, "bob");
    grant(a, "bob", "mine", "1");
    let (s, body) = rest(a, "bob", "pw", "GET", "/show", "");
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("mine") && !body.contains("secret"), "{body}");
    let (s, body) = rest(a, "bob", "pw", "GET", "/monitor", "");
    assert_eq!(s, 200, "{body}");
    assert!(body.contains("mine") && !body.contains("secret"), "{body}");
    let (s, body) = rest(a, "bob", "pw", "GET", "/status", "");
    assert_eq!(s, 403, "{body}");
    let (s, body) = rest(a, "root", "root", "GET", "/show", "");
    assert!(s == 200 && body.contains("secret"), "{body}");
    let (s, _) = rest(a, "root", "root", "GET", "/status", "");
    assert_eq!(s, 200);
}

#[test]
fn the_system_users_name_is_reserved() {
    let srv = start("sysuser");
    let j = root(
        &srv.addr,
        &[
            ("operation", "usermanage"),
            ("type", "1"),
            ("op_username", "system"),
            ("op_password", "pw"),
        ],
    );
    assert_eq!(code(&j), 1003, "{j}");
    let (s, body) = rest(
        &srv.addr,
        "root",
        "root",
        "POST",
        "/user/create?op_username=system&op_password=pw",
        "",
    );
    assert_eq!(s, 400, "{body}");
    let (_, _, body) = send(
        &srv.addr,
        "GET",
        "/shutdown?username=system&password=pw",
        &[],
        "",
    );
    assert_eq!(code(&parse(&body)), 1001, "{body}");
}

// ---------------------------------------------------------------------------
// transactions, HEAD
// ---------------------------------------------------------------------------

/// Ids are numbered per database: a tid-only REST request is refused when
/// the id also names a transaction that just ended on another database (a
/// retried `/commit`), instead of ending that other transaction.
#[test]
fn a_retried_tid_only_commit_does_not_end_another_databases_transaction() {
    let srv = start("tidretry");
    let a = &srv.addr;
    for db in ["xx", "yy"] {
        let (s, body) = rest(
            a,
            "root",
            "root",
            "POST",
            &format!("/build?db_name={db}"),
            DATA,
        );
        assert_eq!(s, 200, "{body}");
    }
    let (s, body) = rest(a, "root", "root", "POST", "/begin?db_name=xx", "");
    assert!(
        s == 200 && body.contains(r#""TID":"1""#) && body.contains(r#""db_name":"xx""#),
        "{body}"
    );
    let (s, body) = rest(a, "root", "root", "POST", "/commit?tid=1", "");
    assert_eq!(s, 200, "{body}");
    let (s, body) = rest(a, "root", "root", "POST", "/begin?db_name=yy", "");
    assert!(s == 200 && body.contains(r#""TID":"1""#), "{body}");
    let (s, body) = rest(
        a,
        "root",
        "root",
        "POST",
        "/tquery?tid=1&db_name=yy",
        "INSERT DATA { <http://ex/y> <http://ex/p> <http://ex/o> . }",
    );
    assert_eq!(s, 200, "{body}");
    // The retry of xx's commit.
    let (s, body) = rest(a, "root", "root", "POST", "/commit?tid=1", "");
    assert_eq!(s, 409, "{body}");
    let (s, body) = rest(
        a,
        "root",
        "root",
        "POST",
        "/sparql?db_name=yy",
        "ASK { <http://ex/y> <http://ex/p> <http://ex/o> }",
    );
    assert!(
        s == 200 && body.contains("false"),
        "yy's transaction is still open: {body}"
    );
    let (s, body) = rest(a, "root", "root", "POST", "/commit?tid=1&db_name=yy", "");
    assert_eq!(s, 200, "{body}");
}

#[test]
fn head_responses_of_the_api_server_carry_no_body() {
    let srv = start("head");
    let mut s = TcpStream::connect(srv.addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(
        b"HEAD /status HTTP/1.1\r\nHost: x\r\n\r\n\
          GET /api?operation=check HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let (first, rest) = resp.split_once("\r\n\r\n").unwrap();
    assert!(first.contains("Content-Length:"), "{first}");
    assert!(
        rest.starts_with("HTTP/1.1 200"),
        "the HEAD body leaked: {resp}"
    );
}
