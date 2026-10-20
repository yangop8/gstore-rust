//! `gserver` — serve a gStore database over HTTP.
//!
//! Usage: `gserver [db_dir] [--api] [--rocksdb] [--no-checkpoint] [--load-dir DIR]… [--password PW] [--port N] [--addr HOST]`
//!
//! Two modes:
//!
//! - default (single database): serve one database over the SPARQL-protocol
//!   subset — `GET/POST /sparql`, `POST /update`, `GET /status`, and a web
//!   console at `GET /`. `db_dir` is opened whatever its layout (bincode
//!   snapshot, on-disk `kvstore.kv` from `gbuild --disk`, or RocksDB — the last
//!   needs `--features rocksdb`) and updates are persisted back to it: an
//!   on-disk / RocksDB database writes each update through, a snapshot is
//!   re-saved atomically. `--rocksdb` is accepted for compatibility and insists
//!   the directory holds a RocksDB database. A `db_dir` that does not exist yet
//!   (or is an empty directory) starts empty and is created on the first save;
//!   one that holds a database that cannot be opened is an error — gserver
//!   exits rather than serve (and later save) an empty database in its place.
//! - `--api` (full multi-database API): serve [`gstore::http_api::ApiServer`]
//!   rooted at `db_dir` (default `.`): users/RBAC (default user `root`, password
//!   from `--password`, default `123456`, mirroring gStore), `/login`, `/build`,
//!   `/load`, `/upload`, `/export`, `/backup`, `/restore`, transactions,
//!   `/tasklist`, `/cancel`, and the web console at `/`. Databases live under the
//!   root directory and are created/loaded at runtime via the API.
//!
//! - `--port N`: TCP port (default 7000)
//! - `--addr H`: bind host (default 127.0.0.1)
//! - `--no-checkpoint`: turn off gStore's `check_point` policy. By default every
//!   write that changes data is saved to disk before it is acknowledged; with
//!   this flag changes stay in memory and are saved on graceful shutdown (and,
//!   in `--api` mode, on `/checkpoint` and `/unload`). On-disk / RocksDB
//!   databases still commit each write they apply.
//! - `--load-dir DIR` (repeatable): let SPARQL `LOAD` read files inside `DIR`.
//!   By default a client's `LOAD` cannot make the server read any file.
//!
//! `SIGINT` (Ctrl-C) and `SIGTERM` trigger a graceful shutdown: stop accepting,
//! let in-flight requests finish, save unsaved changes, exit. A second Ctrl-C
//! terminates immediately.

use std::path::Path;

use gstore::http_api::ApiServer;
use gstore::server::{shutdown_on_signals, Server, ShutdownReport};
use gstore::{Database, LoadPolicy, StorageKind};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut db_dir: Option<String> = None;
    let mut port: u16 = 7000;
    let mut host = String::from("127.0.0.1");
    let mut rocksdb = false;
    let mut api = false;
    let mut check_point = true;
    let mut load_dirs: Vec<String> = Vec::new();
    let mut password = String::from("123456"); // gStore's default root password
    // TLS cert/key paths (feature `tls`). Declared unconditionally so both code
    // paths type-check; only ever set by the cfg-gated `--tls-*` arms below.
    #[allow(unused_mut)]
    let mut tls_cert: Option<String> = None;
    #[allow(unused_mut)]
    let mut tls_key: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                port = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(port);
            }
            "--addr" => {
                i += 1;
                if let Some(h) = args.get(i) {
                    host = h.clone();
                }
            }
            "--password" => {
                i += 1;
                if let Some(p) = args.get(i) {
                    password = p.clone();
                }
            }
            "--rocksdb" => rocksdb = true,
            "--api" => api = true,
            "--no-checkpoint" => check_point = false,
            "--load-dir" => {
                i += 1;
                match args.get(i) {
                    Some(d) => load_dirs.push(d.clone()),
                    None => {
                        eprintln!("error: --load-dir needs a directory");
                        std::process::exit(2);
                    }
                }
            }
            #[cfg(feature = "tls")]
            "--tls-cert" => {
                i += 1;
                tls_cert = args.get(i).cloned();
            }
            #[cfg(feature = "tls")]
            "--tls-key" => {
                i += 1;
                tls_key = args.get(i).cloned();
            }
            // Without the `tls` feature these flags are unknown; consume the
            // flag AND its value with a clear message, so the value is not
            // silently mistaken for the positional db_dir.
            #[cfg(not(feature = "tls"))]
            "--tls-cert" | "--tls-key" => {
                eprintln!(
                    "warning: {} ignored — gserver was built without the `tls` feature \
                     (rebuild with `--features tls` to serve HTTPS)",
                    args[i]
                );
                i += 1; // skip the flag's value
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: gserver [db_dir] [--api] [--rocksdb] [--no-checkpoint] [--load-dir DIR]... [--password PW] [--port N] [--addr HOST]"
                );
                #[cfg(feature = "tls")]
                eprintln!("       [--tls-cert FILE --tls-key FILE]  (serve HTTPS)");
                return;
            }
            other => db_dir = Some(other.to_string()),
        }
        i += 1;
    }

    // HTTPS needs BOTH a cert and a key; one without the other would otherwise
    // silently fall back to plaintext HTTP.
    #[cfg(feature = "tls")]
    if tls_cert.is_some() != tls_key.is_some() {
        eprintln!("error: --tls-cert and --tls-key must be supplied together to serve HTTPS");
        std::process::exit(2);
    }

    let load_policy = load_policy_for(&load_dirs);

    if api {
        if rocksdb {
            eprintln!("gserver: --rocksdb is ignored in --api mode (the API manages its own databases)");
        }
        run_api(
            db_dir,
            &password,
            &host,
            port,
            check_point,
            load_policy,
            tls_cert.as_deref(),
            tls_key.as_deref(),
        );
        return;
    }

    // The database, and the directory its updates are persisted to (`None` ⇒
    // in-memory only).
    let (db, persist_dir) = match &db_dir {
        Some(dir) => (open_or_exit(dir, rocksdb), Some(dir.clone())),
        None => (Database::new("gserver"), None),
    };

    let mut server = match Server::bind(db, (host.as_str(), port)) {
        Ok(s) => s.with_check_point(check_point).with_load_policy(load_policy),
        Err(e) => {
            eprintln!("failed to bind {host}:{port}: {e}");
            std::process::exit(1);
        }
    };
    match &persist_dir {
        Some(dir) => {
            server = server.with_db_dir(dir);
            if check_point {
                println!("gserver: database directory '{dir}' (check_point on: every update is saved there)");
            } else {
                println!(
                    "gserver: database directory '{dir}' (check_point off: changes are saved there on shutdown)"
                );
            }
        }
        None => println!("gserver: no database directory; changes are kept in memory only"),
    }
    if shutdown_on_signals(server.shutdown_handle()) {
        println!("gserver: Ctrl-C / SIGTERM shuts down gracefully (saving unsaved changes)");
    }
    // Serve HTTPS when a cert+key pair was supplied (feature `tls`).
    #[cfg(feature = "tls")]
    if let (Some(cert), Some(key)) = (tls_cert.as_deref(), tls_key.as_deref()) {
        match gstore::server::load_tls_config(cert, key) {
            Ok(cfg) => {
                match server.local_addr() {
                    Ok(a) => println!("gserver listening on https://{a}  (TLS; web console at /, POST /sparql, /update; GET /status)"),
                    Err(_) => println!("gserver (TLS) listening on {host}:{port}"),
                }
                print_report(&server.serve_forever_tls(cfg));
                return;
            }
            Err(e) => {
                eprintln!("failed to load TLS cert/key: {e}");
                std::process::exit(1);
            }
        }
    }
    match server.local_addr() {
        Ok(a) => println!("gserver listening on http://{a}  (web console at /, POST /sparql, /update; GET /status)"),
        Err(_) => println!("gserver listening on {host}:{port}"),
    }
    print_report(&server.serve_forever());
}

/// Serve the full multi-database API ([`ApiServer`]) rooted at `db_dir`
/// (default the current directory), with users/RBAC and the web console.
#[allow(clippy::too_many_arguments)]
fn run_api(
    db_dir: Option<String>,
    password: &str,
    host: &str,
    port: u16,
    check_point: bool,
    load_policy: LoadPolicy,
    tls_cert: Option<&str>,
    tls_key: Option<&str>,
) {
    let root = db_dir.unwrap_or_else(|| ".".to_string());
    if password == "123456" {
        eprintln!(
            "warning: root is using the default password '123456'; \
             pass --password to set a strong one before exposing this server"
        );
    }
    let server = match ApiServer::bind(&root, password, (host, port)) {
        Ok(s) => s.with_check_point(check_point).with_load_policy(load_policy),
        Err(e) => {
            eprintln!("failed to start API server at {host}:{port}: {e}");
            std::process::exit(1);
        }
    };
    if check_point {
        println!("gserver: databases are stored under '{root}' (check_point on: every write is saved)");
    } else {
        println!(
            "gserver: databases are stored under '{root}' (check_point off: changes are saved \
             on /checkpoint, /unload and shutdown)"
        );
    }
    if shutdown_on_signals(server.shutdown_handle()) {
        println!("gserver: Ctrl-C / SIGTERM shuts down gracefully (saving unsaved changes)");
    }
    // Serve HTTPS when a cert+key pair was supplied (feature `tls`).
    #[cfg(feature = "tls")]
    if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
        match gstore::server::load_tls_config(cert, key) {
            Ok(cfg) => {
                match server.local_addr() {
                    Ok(a) => println!(
                        "gserver (API mode, TLS) on https://{a}  root={root}  user=root  \
                         (web console at /, POST /login /sparql /update /upload …; see /status)"
                    ),
                    Err(_) => println!("gserver (API mode, TLS) on {host}:{port}  root={root}"),
                }
                print_report(&server.serve_forever_tls(cfg));
                return;
            }
            Err(e) => {
                eprintln!("failed to load TLS cert/key: {e}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(feature = "tls"))]
    let _ = (tls_cert, tls_key);
    match server.local_addr() {
        Ok(a) => println!(
            "gserver (API mode) on http://{a}  root={root}  user=root  \
             (web console at /, POST /login /sparql /update /upload …; see /status)"
        ),
        Err(_) => println!("gserver (API mode) on {host}:{port}  root={root}"),
    }
    print_report(&server.serve_forever());
}

/// The name for a fresh database at `dir`: its last path component without
/// gStore's `.db` suffix.
fn db_name_for(dir: &str) -> String {
    let base = Path::new(dir)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = base.strip_suffix(gstore::DB_SUFFIX).unwrap_or(&base);
    if name.is_empty() {
        "gserver".to_string()
    } else {
        name.to_string()
    }
}

/// Print what the graceful shutdown did; exit non-zero if a save failed.
fn print_report(report: &ShutdownReport) {
    if !report.drained {
        eprintln!("gserver: some requests were still running when the shutdown grace period ended");
    }
    if report.rolled_back > 0 {
        println!(
            "gserver: rolled back {} open transaction(s)",
            report.rolled_back
        );
    }
    for dir in &report.saved {
        println!("gserver: saved database to '{}'", dir.display());
    }
    for (dir, e) in &report.failed {
        eprintln!("gserver: FAILED to save database to '{}': {e}", dir.display());
    }
    println!("gserver: shut down");
    if !report.failed.is_empty() {
        std::process::exit(1);
    }
}

/// The `LOAD` policy for the served databases: nothing may be loaded, except
/// files inside the `--load-dir` directories.
fn load_policy_for(dirs: &[String]) -> LoadPolicy {
    let mut policy = LoadPolicy::restricted();
    for d in dirs {
        if !Path::new(d).is_dir() {
            eprintln!("error: --load-dir '{d}' is not a directory");
            std::process::exit(2);
        }
        policy = policy.with_allowed_dir(d);
        println!("gserver: SPARQL LOAD may read files under '{d}'");
    }
    policy
}

/// Open the single database at `dir`, whatever its layout. A directory that does
/// not exist yet (or is empty) gives a fresh database that is saved there on the
/// first update. Anything else that cannot be opened is fatal: serving an empty
/// stand-in could later be saved over the real database.
fn open_or_exit(dir: &str, expect_rocksdb: bool) -> Database {
    let path = Path::new(dir);
    let is_empty_dir = path.is_dir()
        && std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none());
    if !path.exists() || is_empty_dir {
        if expect_rocksdb {
            eprintln!("gserver: --rocksdb given but '{dir}' holds no database");
            std::process::exit(1);
        }
        eprintln!("'{dir}' holds no database yet; serving a fresh empty database that will be saved there");
        return Database::new(db_name_for(dir));
    }
    if expect_rocksdb && Database::storage_kind(path) != Some(StorageKind::RocksDb) {
        eprintln!("gserver: --rocksdb given but '{dir}' is not a RocksDB-backed database");
        std::process::exit(1);
    }
    match Database::open(path) {
        Ok(db) => {
            println!("gserver: opened '{dir}' ({} layout)", db.storage());
            db
        }
        Err(e) => {
            eprintln!("gserver: could not open database '{dir}': {e}");
            std::process::exit(1);
        }
    }
}
