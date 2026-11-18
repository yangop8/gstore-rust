//! `gnode` — a single distributed **shard node**: it owns one local
//! [`TripleStore`] and serves per-shard RPCs over
//! TCP for a [`NetworkShardedStore`](gstore::cluster::NetworkShardedStore)
//! coordinator (see [`gstore::cluster::ShardNode`] / [`gstore::rpc`]).
//!
//! **Experimental.** `gnode` exercises the experimental [`gstore::cluster`]
//! layer: a shard node persists nothing (a restarted node starts from its
//! optional `data_file` only), the RPC protocol is unauthenticated and may
//! change between releases, and it is not connected to `gserver` or the HTTP
//! API. It has no counterpart in upstream gStore 1.4. Do not use it for data
//! you need to keep.
//!
//! Usage:
//!
//! - `gnode [bind_addr] [data_file]` — run a single **shard** node (the default).
//! - `gnode cluster <id> <bind_addr> [peer_id@addr ...] (--state-dir DIR |
//!   --ephemeral)` — run a **replicated cluster** node (a Raft-like
//!   leader/follower replica; see [`gstore::cluster::ClusterNode`]).
//!
//! For the shard mode:
//!
//! - `bind_addr`: `HOST:PORT` to listen on (default `127.0.0.1:7100`; use
//!   `127.0.0.1:0` for an ephemeral port printed at startup).
//! - `data_file`: optional initial shard contents as **id-triples** — one triple
//!   per line, three whitespace-separated unsigned integers `sub pred obj`;
//!   blank lines and `#` comments are ignored. The cluster operates in id-space
//!   (the string↔id [`Dictionary`](gstore::dict::Dictionary) lives in the
//!   coordinator), so a node's native input is id-triples.
//!
//! For the cluster mode: `<id>` is this node's numeric id, `<bind_addr>` is its
//! listen address, and each `peer_id@addr` names another replica (e.g.
//! `1@127.0.0.1:7101`). One node wins leadership by majority vote; clients send
//! writes to the leader, which replicates them to a quorum before they apply.
//! One of two state choices is required:
//!
//! - `--state-dir DIR`: the replica's Raft term, vote and log are journaled
//!   in `DIR` (one directory per node id) and `fsync`ed before it answers, so
//!   it can be restarted — it keeps the vote it gave in its term and its log,
//!   and rebuilds its triples as the leader re-confirms the log;
//! - `--ephemeral`: everything stays in memory (the earlier behaviour). Such a
//!   replica must never be restarted under the same id: it would forget its
//!   vote and could vote a second time in one term (two leaders of one term),
//!   or help elect a leader that lacks committed writes (review
//!   cluster-nlqa#11).
//!
//! The wire protocol is the hand-rolled, `std`-only length-prefixed binary codec
//! in [`gstore::rpc`] — the zero-dependency stand-in for gRPC; swapping in gRPC
//! would be a serialization-codec swap, not an architectural change.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use gstore::cluster::{ClusterNode, RaftConfig, ShardNode};
use gstore::model::IdTriple;
use gstore::rpc::NodeId;
use gstore::store::TripleStore;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let mut positionals: Vec<&str> = Vec::new();
    for arg in &args[1..] {
        if arg == "-h" || arg == "--help" {
            eprintln!("usage (experimental):\n  gnode [bind_addr] [data_file]\n  {CLUSTER_USAGE}");
            return ExitCode::SUCCESS;
        }
        positionals.push(arg);
    }

    if positionals.first().copied() == Some("cluster") {
        return run_cluster(&positionals[1..]);
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
        Ok(a) => gstore::outln!("gnode shard listening on {a} (length-prefixed binary RPC)"),
        Err(_) => gstore::outln!("gnode shard listening on {bind_addr}"),
    }
    node.serve_forever();
    ExitCode::SUCCESS
}

/// The cluster-mode command line.
const CLUSTER_USAGE: &str =
    "gnode cluster <id> <bind_addr> [peer_id@addr ...] (--state-dir DIR | --ephemeral)";

/// Where a cluster replica keeps its Raft term, vote and log.
enum StateChoice {
    /// Journaled in this directory (durable across restarts).
    Dir(PathBuf),
    /// In memory only: the replica must never restart under the same id.
    Ephemeral,
}

/// Split the cluster-mode arguments into the positional ones and the state
/// choice (`--state-dir DIR`, `--state-dir=DIR` or `--ephemeral`, anywhere).
fn parse_state_choice<'a>(args: &[&'a str]) -> Result<(Vec<&'a str>, StateChoice), String> {
    let mut positionals = Vec::new();
    let mut dir: Option<PathBuf> = None;
    let mut ephemeral = false;
    let mut it = args.iter();
    while let Some(&arg) = it.next() {
        if arg == "--ephemeral" {
            ephemeral = true;
        } else if arg == "--state-dir" {
            match it.next() {
                Some(d) if !d.is_empty() => dir = Some(PathBuf::from(d)),
                _ => return Err("--state-dir needs a directory".to_string()),
            }
        } else if let Some(d) = arg.strip_prefix("--state-dir=") {
            if d.is_empty() {
                return Err("--state-dir needs a directory".to_string());
            }
            dir = Some(PathBuf::from(d));
        } else if arg.starts_with("--") {
            return Err(format!("unknown option '{arg}'"));
        } else {
            positionals.push(arg);
        }
    }
    match (dir, ephemeral) {
        (Some(d), false) => Ok((positionals, StateChoice::Dir(d))),
        (None, true) => Ok((positionals, StateChoice::Ephemeral)),
        (Some(_), true) => Err("--state-dir and --ephemeral exclude each other".to_string()),
        (None, false) => Err(
            "choose where the replica keeps its Raft term, vote and log: --state-dir DIR \
             (durable; required to restart it safely) or --ephemeral (in memory; it must \
             then never be restarted under the same id)"
                .to_string(),
        ),
    }
}

/// Run a replicated **cluster** node:
/// `cluster <id> <bind_addr> [peer_id@addr ...] (--state-dir DIR | --ephemeral)`.
fn run_cluster(args: &[&str]) -> ExitCode {
    let (args, state) = match parse_state_choice(args) {
        Ok(parsed) => parsed,
        Err(msg) => {
            eprintln!("gnode cluster: {msg}");
            eprintln!("usage: {CLUSTER_USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let args = &args[..];
    let id: NodeId = match args.first().and_then(|s| s.parse().ok()) {
        Some(id) => id,
        None => {
            eprintln!("gnode cluster: expected a numeric node <id>");
            eprintln!("usage: {CLUSTER_USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let bind_addr = match args.get(1).copied() {
        Some(a) => a,
        None => {
            eprintln!("gnode cluster: expected a <bind_addr>");
            return ExitCode::FAILURE;
        }
    };

    // Parse `peer_id@addr` entries into (id, SocketAddr) pairs.
    let mut peers: Vec<(NodeId, SocketAddr)> = Vec::new();
    for spec in &args[2..] {
        let (pid_str, addr_str) = match spec.split_once('@') {
            Some(parts) => parts,
            None => {
                eprintln!("gnode cluster: peer '{spec}' must be 'peer_id@addr'");
                return ExitCode::FAILURE;
            }
        };
        let pid: NodeId = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => {
                eprintln!("gnode cluster: peer id in '{spec}' is not a number");
                return ExitCode::FAILURE;
            }
        };
        let addr: SocketAddr = match addr_str.parse() {
            Ok(a) => a,
            Err(_) => {
                eprintln!("gnode cluster: peer addr in '{spec}' is not a socket address");
                return ExitCode::FAILURE;
            }
        };
        peers.push((pid, addr));
    }

    let peer_ids: Vec<NodeId> = peers.iter().map(|(p, _)| *p).collect();
    let bound = match &state {
        StateChoice::Dir(dir) => {
            ClusterNode::bind_persistent(id, peer_ids, dir, bind_addr, RaftConfig::default())
        }
        StateChoice::Ephemeral => {
            eprintln!(
                "gnode cluster: --ephemeral: node {id} keeps its Raft state in memory only; \
                 never restart it under the same id"
            );
            ClusterNode::bind(
                id,
                peer_ids,
                TripleStore::new(),
                bind_addr,
                RaftConfig::default(),
            )
        }
    };
    let node = match bound {
        Ok(n) => n,
        Err(e) => {
            match &state {
                StateChoice::Dir(dir) => eprintln!(
                    "gnode cluster: failed to start node {id} on '{bind_addr}' with state \
                     directory '{}': {e}",
                    dir.display()
                ),
                StateChoice::Ephemeral => {
                    eprintln!("gnode cluster: failed to bind '{bind_addr}': {e}")
                }
            }
            return ExitCode::FAILURE;
        }
    };
    for (pid, addr) in &peers {
        node.set_peer_addr(*pid, *addr);
    }
    match node.local_addr() {
        Ok(a) => gstore::outln!(
            "gnode cluster node {id} listening on {a} with {} peer(s) (Raft replication)",
            peers.len()
        ),
        Err(_) => gstore::outln!("gnode cluster node {id} listening on {bind_addr}"),
    }
    let handles = node.start();
    for h in handles {
        let _ = h.join();
    }
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
