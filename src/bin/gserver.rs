//! `gserver` — serve a gStore database over HTTP.
//!
//! Usage: `gserver [db_dir] [--api] [--rocksdb] [--password PW] [--port N] [--addr HOST]`
//!
//! Two modes:
//!
//! - default (single database): serve one database over the SPARQL-protocol
//!   subset — `GET/POST /sparql`, `POST /update`, `GET /status`, and a web
//!   console at `GET /`. `db_dir` loads that database (`--rocksdb` opens it as a
//!   RocksDB-backed database; needs `--features rocksdb`).
//! - `--api` (full multi-database API): serve [`gstore::http_api::ApiServer`]
//!   rooted at `db_dir` (default `.`): users/RBAC (default user `root`, password
//!   from `--password`, default `123456`, mirroring gStore), `/login`, `/build`,
//!   `/load`, `/upload`, `/export`, `/backup`, `/restore`, transactions,
//!   `/tasklist`, `/cancel`, and the web console at `/`. Databases live under the
//!   root directory and are created/loaded at runtime via the API.
//!
//! - `--port N`: TCP port (default 7000)
//! - `--addr H`: bind host (default 127.0.0.1)

use gstore::http_api::ApiServer;
use gstore::server::Server;
use gstore::Database;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut db_dir: Option<String> = None;
    let mut port: u16 = 7000;
    let mut host = String::from("127.0.0.1");
    let mut rocksdb = false;
    let mut api = false;
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
            "-h" | "--help" => {
                eprintln!(
                    "usage: gserver [db_dir] [--api] [--rocksdb] [--password PW] [--port N] [--addr HOST]"
                );
                #[cfg(feature = "tls")]
                eprintln!("       [--tls-cert FILE --tls-key FILE]  (serve HTTPS)");
                return;
            }
            other => db_dir = Some(other.to_string()),
        }
        i += 1;
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
            tls_cert.as_deref(),
            tls_key.as_deref(),
        );
        return;
    }

    let db = match &db_dir {
        Some(dir) if rocksdb => open_rocksdb_or_exit(dir),
        Some(dir) => Database::load(dir).unwrap_or_else(|e| {
            eprintln!("could not load '{dir}' ({e}); serving a fresh empty database");
            Database::new("gserver")
        }),
        None => Database::new("gserver"),
    };

    let server = match Server::bind(db, (host.as_str(), port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to bind {host}:{port}: {e}");
            std::process::exit(1);
        }
    };
    // Serve HTTPS when a cert+key pair was supplied (feature `tls`).
    #[cfg(feature = "tls")]
    if let (Some(cert), Some(key)) = (tls_cert.as_deref(), tls_key.as_deref()) {
        match gstore::server::load_tls_config(cert, key) {
            Ok(cfg) => {
                match server.local_addr() {
                    Ok(a) => println!("gserver listening on https://{a}  (TLS; web console at /, POST /sparql, /update; GET /status)"),
                    Err(_) => println!("gserver (TLS) listening on {host}:{port}"),
                }
                server.serve_forever_tls(cfg);
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
    server.serve_forever();
}

/// Serve the full multi-database API ([`ApiServer`]) rooted at `db_dir`
/// (default the current directory), with users/RBAC and the web console.
fn run_api(
    db_dir: Option<String>,
    password: &str,
    host: &str,
    port: u16,
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
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to start API server at {host}:{port}: {e}");
            std::process::exit(1);
        }
    };
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
                server.serve_forever_tls(cfg);
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
    server.serve_forever();
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
