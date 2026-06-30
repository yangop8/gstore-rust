//! `gnode` — a single distributed **shard node**: it owns one local
//! [`TripleStore`](gstore::store::TripleStore) and serves per-shard RPCs over
//! TCP for a [`NetworkShardedStore`](gstore::cluster::NetworkShardedStore)
//! coordinator (see [`gstore::cluster::ShardNode`] / [`gstore::rpc`]).
//!
//! Usage: `gnode [bind_addr] [data_file]`
//!
//! - `bind_addr`: `HOST:PORT` to listen on (default `127.0.0.1:7100`; use
//!   `127.0.0.1:0` for an ephemeral port printed at startup).
//! - `data_file`: optional initial shard contents as **id-triples** — one triple
//!   per line, three whitespace-separated unsigned integers `sub pred obj`;
//!   blank lines and `#` comments are ignored. The cluster operates in id-space
//!   (the string↔id [`Dictionary`](gstore::dict::Dictionary) lives in the
//!   coordinator), so a node's native input is id-triples.
//!
//! The wire protocol is the hand-rolled, `std`-only length-prefixed binary codec
//! in [`gstore::rpc`] — the zero-dependency stand-in for gRPC; swapping in gRPC
//! would be a serialization-codec swap, not an architectural change.

use std::process::ExitCode;

use gstore::cluster::ShardNode;
use gstore::model::IdTriple;
use gstore::store::TripleStore;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut positionals: Vec<&str> = Vec::new();
    for arg in &args[1..] {
        if arg == "-h" || arg == "--help" {
            eprintln!("usage: gnode [bind_addr] [data_file]");
            return ExitCode::SUCCESS;
        }
        positionals.push(arg);
    }

    let bind_addr = positionals.first().copied().unwrap_or("127.0.0.1:7100");
    let data_file = positionals.get(1).copied();

    let mut store = TripleStore::new();
    if let Some(path) = data_file {
        match load_id_triples(path) {
            Ok(triples) => {
                let n = triples.len();
                store.bulk_load(triples);
                eprintln!("gnode: loaded {n} id-triples from '{path}'");
            }
            Err(e) => {
                eprintln!("gnode: could not load '{path}': {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    let node = match ShardNode::bind(store, bind_addr) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("gnode: failed to bind '{bind_addr}': {e}");
            return ExitCode::FAILURE;
        }
    };
    match node.local_addr() {
        Ok(a) => println!("gnode shard listening on {a} (length-prefixed binary RPC)"),
        Err(_) => println!("gnode shard listening on {bind_addr}"),
    }
    node.serve_forever();
    ExitCode::SUCCESS
}

/// Parse a file of id-triples: one `sub pred obj` per line (three whitespace-
/// separated `u32`s); blank lines and `#`-comment lines are skipped.
fn load_id_triples(path: &str) -> std::io::Result<Vec<IdTriple>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let sub = next_id(&mut parts, lineno)?;
        let pred = next_id(&mut parts, lineno)?;
        let obj = next_id(&mut parts, lineno)?;
        if parts.next().is_some() {
            return Err(parse_err(lineno, "expected exactly three integers"));
        }
        out.push(IdTriple::new(sub, pred, obj));
    }
    Ok(out)
}

fn next_id(parts: &mut std::str::SplitWhitespace, lineno: usize) -> std::io::Result<u32> {
    match parts.next() {
        Some(tok) => tok
            .parse::<u32>()
            .map_err(|_| parse_err(lineno, "id is not a u32")),
        None => Err(parse_err(lineno, "expected three integers")),
    }
}

fn parse_err(lineno: usize, msg: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("line {}: {msg}", lineno + 1),
    )
}
