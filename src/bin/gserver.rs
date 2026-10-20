//! `gserver` — serve a gStore database over HTTP.
//!
//! Usage: `gserver [db_dir] [--api] [--rocksdb] [--no-checkpoint] [--password PW] [--port N] [--addr HOST]`
//!
//! Two modes:
//!
//! - default (single database): serve one database over the SPARQL-protocol
//!   subset — `GET/POST /sparql`, `POST /update`, `GET /status`, and a web
//!   console at `GET /`. `db_dir` loads that database (`--rocksdb` opens it as a
//!   RocksDB-backed database; needs `--features rocksdb`) and updates are
//!   persisted back to it. A `db_dir` that does not exist yet starts empty and
//!   is created on the first save; one that exists but cannot be loaded is
//!   served as an empty in-memory database and is never overwritten.
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
//!   in `--api` mode, on `/checkpoint` and `/unload`).
//!
//! `SIGINT` (Ctrl-C) and `SIGTERM` trigger a graceful shutdown: stop accepting,
//! let in-flight requests finish, save unsaved changes, exit. A second Ctrl-C
//! terminates immediately.

use std::path::Path;

use gstore::http_api::ApiServer;
use gstore::server::{shutdown_on_signals, Server, ShutdownReport};
use gstore::Database;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut db_dir: Option<String> = None;
    let mut port: u16 = 7000;
    let mut host = String::from("127.0.0.1");
    let mut rocksdb = false;
    let mut api = false;
    let mut check_point = true;
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
                    "usage: gserver [db_dir] [--api] [--rocksdb] [--no-checkpoint] [--password PW] [--port N] [--addr HOST]"
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
            tls_cert.as_deref(),
            tls_key.as_deref(),
        );
        return;
    }

    // The database, and the directory its updates are persisted to (`None` ⇒
    // in-memory only).
    let (db, persist_dir) = match &db_dir {
        Some(dir) if rocksdb => (open_rocksdb_or_exit(dir), Some(dir.clone())),
        Some(dir) => match Database::load(dir) {
            Ok(db) => (db, Some(dir.clone())),
            Err(_) if !Path::new(dir).exists() => {
                eprintln!(
                    "'{dir}' does not exist yet; serving a fresh empty database that will be saved there"
                );
                (Database::new(db_name_for(dir)), Some(dir.clone()))
            }
            Err(e) => {
                eprintln!(
                    "could not load '{dir}' ({e}); serving a fresh empty IN-MEMORY database \
                     (changes are not saved, so '{dir}' is never overwritten)"
                );
                (Database::new("gserver"), None)
            }
        },
        None => (Database::new("gserver"), None),
    };

    let mut server = match Server::bind(db, (host.as_str(), port)) {
        Ok(s) => s.with_check_point(check_point),
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
fn run_api(
    db_dir: Option<String>,
    password: &str,
    host: &str,
    port: u16,
    check_point: bool,
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
        Ok(s) => s.with_check_point(check_point),
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

/// Open a RocksDB-backed database (feature `rocksdb`), or exit with a message.
#[cfg(feature = "rocksdb")]
fn open_rocksdb_or_exit(dir: &str) -> Database {
    Database::open_rocksdb(dir).unwrap_or_else(|e| {
        eprintln!("could not open RocksDB database '{dir}': {e}");
        std::process::exit(1);
    })
}

#[cfg(not(feature = "rocksdb"))]
fn open_rocksdb_or_exit(_dir: &str) -> Database {
    eprintln!("gserver: --rocksdb requires building with `--features rocksdb`");
    std::process::exit(1);
}
