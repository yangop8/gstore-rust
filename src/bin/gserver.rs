//! `gserver` — serve a gStore database over HTTP (SPARQL Protocol subset).
//!
//! Usage: `gserver [db_dir] [--port N] [--addr HOST]`
//!   - `db_dir`  : load this database directory (default: a fresh empty DB)
//!   - `--port N`: TCP port (default 7000)
//!   - `--addr H`: bind host (default 127.0.0.1)
//!
//! Endpoints: `GET/POST /sparql`, `POST /update`, `GET /status` (see
//! [`gstore::server`]).

use gstore::server::Server;
use gstore::Database;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut db_dir: Option<String> = None;
    let mut port: u16 = 7000;
    let mut host = String::from("127.0.0.1");

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
            "-h" | "--help" => {
                eprintln!("usage: gserver [db_dir] [--port N] [--addr HOST]");
                return;
            }
            other => db_dir = Some(other.to_string()),
        }
        i += 1;
    }

    let db = match &db_dir {
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
    match server.local_addr() {
        Ok(a) => println!("gserver listening on http://{a}  (POST /sparql, /update; GET /status)"),
        Err(_) => println!("gserver listening on {host}:{port}"),
    }
    server.serve_forever();
}
