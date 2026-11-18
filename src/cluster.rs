//! A sharded / distributed-query core: an **in-process** scatter-gather
//! [`TripleSource`] over `N` partitioned [`TripleStore`]s, plus an
//! experimental network transport, Raft-like replication and elastic
//! membership on top of it.
//!
//! # Experimental
//!
//! Everything in this module except the in-process [`ShardedStore`] is an
//! **experimental library feature**: it is not wired into `gserver` or the HTTP
//! API, its wire protocol ([`crate::rpc`]) is unauthenticated, and its APIs may
//! change without notice. Stores live in memory only; the one durable piece is
//! a [`ClusterNode`]'s Raft term, vote and log when it is bound with
//! [`ClusterNode::bind_persistent`] (its triple store is rebuilt from that log
//! after a restart). It is **not** a port of upstream gStore: upstream 1.4's
//! `src/Cluster` (leader/follower replication) is not compiled into its
//! release, and the distributed gStore edition is not open source. Use it for
//! experiments and tests, not production data.
//!
//! Corresponds, in spirit, to gStore's distributed deployment (gStore + the
//! `gStore`-on-cluster work): the triple set is **partitioned** across shards
//! and a query is answered by *scatter-gather* — fan the access out to every
//! shard, then **merge** the per-shard answers into one. Because the query
//! engine ([`crate::query::engine::Evaluator`]) is generic over
//! [`TripleSource`], the *exact same* optimizer + executor that runs over a
//! single [`TripleStore`] runs, unmodified, over a [`ShardedStore`].
//!
//! ## Partitioning
//!
//! Triples are partitioned by `hash(subject) % N` so that **a subject's whole
//! adjacency co-locates on one shard**. That makes every subject-rooted access
//! (`s??`, `sp?`, `s?o`, `spo`) a single-shard lookup, while object- and
//! predicate-rooted accesses (`??o`, `?po`, `?p?`) must touch all shards (the
//! same object/predicate can occur on many shards, reached from different
//! subjects).
//!
//! ## Merge contract (the load-bearing part)
//!
//! The engine relies on the precise return-type contract of each
//! [`TripleSource`] method (sorted, de-duplicated `Vec`s; `(pred,sub)` /
//! `(sub,obj)` orderings; global-distinct counts). After concatenating the
//! per-shard answers this module **re-establishes that contract** — sort +
//! dedup pair/key lists, dedup-then-count the distinct statistics, and rebuild
//! `iter_all` in predicate-major `(pred, sub, obj)` order — so that a
//! [`ShardedStore`] is **observationally identical** to a single
//! [`TripleStore`] holding the same triples. Get this wrong and the join
//! engine silently returns wrong answers; the unit tests below assert byte-for-
//! byte parity across every access pattern and a multi-pattern BGP.
//!
//! ## Network-distributed sharding
//!
//! [`ShardedStore`] is the in-process merge core. On top of it this module adds
//! a **real network transport** so shards can live in separate processes/hosts:
//!
//! * [`ShardNode`] — a server that owns one local [`TripleStore`] shard and
//!   answers per-shard RPCs over TCP (the `gnode` binary is a thin
//!   wrapper around it; see `src/bin/gnode.rs`).
//! * [`RemoteShard`] — a TCP client to a [`ShardNode`], speaking the
//!   length-prefixed binary protocol in [`crate::rpc`].
//! * [`NetworkShardedStore`] — the distributed analogue of [`ShardedStore`]: a
//!   `Vec<`[`Shard`]`>` mixing **local** in-process shards and **remote** ones,
//!   running the *same* scatter-gather merge (so it is observationally identical
//!   to a single [`TripleStore`]) plus **routed inserts** (each triple is placed
//!   on `hash(subject) % N`, shipped over RPC for a remote shard).
//!
//! ## Shard failures are errors
//!
//! A distributed read can fail part-way: a remote shard may be unreachable, or
//! every replica of a bucket may be down. Merging such a shard in as *empty*
//! would make a query return a partial answer indistinguishable from a complete
//! one, so the distributed stores ([`NetworkShardedStore`], [`ShardCluster`],
//! [`ShardedRaftCluster`]) do **not** implement the infallible
//! [`TripleSource`]. They implement [`TryTripleSource`], whose `try_*` reads
//! return `Err` naming the failed shard, and their `query` methods evaluate
//! through a [`Checked`] view that aborts the evaluation on the first shard
//! failure and returns it as [`GStoreError::Io`]. Routed writes
//! ([`NetworkShardedStore::insert`], [`ShardCluster::insert`]) surface I/O
//! errors as before.
//!
//! The wire codec is hand-rolled `std`-only TCP, not gRPC/protobuf — that is a
//! deliberate **zero-dependency** choice (see [`crate::rpc`]). gRPC would occupy
//! the exact same architectural slot; swapping it in is a *serialization-codec
//! swap* (replace [`crate::rpc`]'s framing + request/response (de)serialization
//! with generated protobuf stubs over HTTP/2), leaving the scatter-gather merge
//! here untouched.
//!
//! Replication & fault tolerance **are** implemented: see [`ClusterNode`] /
//! [`Role`] for Raft-like leader election, log replication, quorum commit,
//! heartbeat, failover and follower catch-up.
//!
//! ## Elastic membership & rebalancing
//!
//! Dynamic node join/leave with partition reassignment is implemented:
//!
//! * [`ClusterConfig`] — the replicated configuration object (member set + the
//!   shard→owner assignment, tagged with an `epoch`). Membership changes are
//!   replicated as Raft **config-change** log entries
//!   ([`LogOp::AddNode`] /
//!   [`LogOp::RemoveNode`]): committed by quorum and
//!   applied on every [`ClusterNode`] replica, which therefore converge on an
//!   identical config (served over [`Request::GetConfig`]).
//! * [`ShardCluster`] — the data-plane coordinator. It maps shards to owners by
//!   **rendezvous (consistent) hashing** and, on a join/leave, **migrates the
//!   reassigned buckets' triples between owners over RPC**
//!   ([`Request::ScanShard`] → insert → [`Request::PurgeShard`]) before cutting
//!   over routing — so a joining node receives its shards, a leaving node hands
//!   them to survivors with no data loss, and writes during/after a rebalance land
//!   on the correct owner.
//!
//! ## Per-shard replica groups (each shard highly available)
//!
//! Every shard bucket is its own **replicated group** of `N` replicas, so a
//! shard survives up to `floor((N-1)/2)` replica failures while a quorum keeps
//! serving:
//!
//! * [`ReplicaGroup`] — one shard bucket replicated across `N` replicas, each
//!   running the proven pure [`RaftState`] over its own [`TripleStore`] copy.
//!   Writes commit through the group's Raft log (quorum), a lagging/rejoining
//!   replica catches up by log backfill or — past the leader's compaction point
//!   — by a snapshot install, and leadership fails over within the group. The
//!   group also **auto-compacts** each replica's committed log into a snapshot
//!   once it grows past a threshold, so a long-running shard keeps a bounded
//!   in-memory log and a far-behind replica is caught up by
//!   [`Request::InstallSnapshot`] rather than unbounded replay.
//! * [`ShardedRaftCluster`] — the multi-group analogue of a single-group
//!   [`ClusterNode`]: one [`ReplicaGroup`] per bucket. Writes route by
//!   `hash(subject) % N` to the owning bucket's group and commit through *its*
//!   Raft log; reads scatter-gather across every bucket's serving replica and
//!   re-establish the [`TripleSource`] contract, so the whole cluster is both
//!   **sharded** (data partitioned across buckets) and **replicated** (each
//!   bucket fault-tolerant) while staying observationally identical to a single
//!   [`TripleStore`]. Like the Raft state machine it is driven by explicit
//!   logical ticks and synchronous message delivery, so every scenario —
//!   election, quorum commit, snapshot install, failover, rejoin — is fully
//!   deterministic with no threads or sockets. A deployment wraps each replica
//!   in a [`ClusterNode`]-style transport exactly as the single-group path does.
//!
//! ## Scope (still deferred — with reasons)
//!
//! * **A multi-group *network* transport.** [`ShardedRaftCluster`] is the
//!   in-process correctness core (per-shard Raft groups, snapshot catch-up,
//!   failover); binding each of its replicas to a live TCP [`ClusterNode`]
//!   endpoint (N groups × N replicas of listeners + per-group leader routing) is
//!   the remaining deployment glue. The single-group [`ClusterNode`] transport
//!   is fully wired (see `networked_cluster_replicates_then_fails_over_then_recovers`).
//! * **Auto-orchestration of migration on commit.** The control plane (agreed
//!   [`ClusterConfig`]) and data plane ([`ShardCluster`] migration) share the
//!   [`ClusterConfig`] type but are driven explicitly; wiring a leader to *drive*
//!   the data migration the instant a config-change commits is left to a
//!   deployment loop (the enacting step, [`ShardCluster::add_node`]/`remove_node`,
//!   is the tested unit).
//! * **Snapshot + config interaction.** Config-change entries live in the same
//!   replicated log as data writes; a leader that *compacts* past them would drop
//!   them from the triple-only snapshot. The transport never auto-compacts, so
//!   config entries reach rejoining nodes by normal log backfill; a
//!   config-carrying snapshot is the remaining fix.
//! * **Persistence and server integration.** Only a persistent
//!   [`ClusterNode`]'s Raft term, vote and log are durable (enough for a
//!   replica to restart without breaking Raft's safety); shard nodes and the
//!   coordinators keep everything in memory, and the cluster is not reachable
//!   from `gserver` / the HTTP API (see *Experimental* above).

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::id::{EntityLiteralId, PredId};
use crate::model::IdTriple;
use crate::parser::sparql;
use crate::query::{Evaluator, QueryResult};
use crate::rpc::{self, Request, Response};
use crate::store::{TripleSource, TripleStore};

/// A triple set partitioned across `N` [`TripleStore`] shards by
/// `hash(subject) % N`, queried by scatter-gather. Implements [`TripleSource`]
/// so the generic [`Evaluator`] runs over it unchanged.
///
/// Correctness rests on the by-construction invariant that every triple of a
/// subject lives on exactly one shard (`shard_of(subject)`); `triple_count` and
/// `pred_card` sum across shards on that basis. If a routed-`insert` API is ever
/// added it MUST place triples via `shard_of(sub)`, or those sums silently break.
#[derive(Debug, Clone)]
pub struct ShardedStore {
    shards: Vec<TripleStore>,
}

/// The shard index a subject hashes to. Stable within a process run (all that
/// scatter-gather needs); `DefaultHasher` keeps us on `std` with no new deps.
fn shard_of(sub: EntityLiteralId, num_shards: usize) -> usize {
    let mut h = DefaultHasher::new();
    sub.hash(&mut h);
    (h.finish() % num_shards as u64) as usize
}

/// Sort + dedup a gathered list back into the trait's `Vec` contract.
fn sort_dedup<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort_unstable();
    v.dedup();
    v
}

impl ShardedStore {
    /// Create an empty store with `num_shards` shards (`num_shards` is clamped
    /// to at least 1).
    pub fn new(num_shards: usize) -> ShardedStore {
        let n = num_shards.max(1);
        ShardedStore {
            shards: (0..n).map(|_| TripleStore::new()).collect(),
        }
    }

    /// Build from id-triples, routing each triple to `hash(subject) % N` and
    /// bulk-loading each shard once (so every shard's indexes are sorted +
    /// de-duplicated, exactly as a single [`TripleStore::bulk_load`]).
    pub fn from_triples(
        num_shards: usize,
        triples: impl IntoIterator<Item = IdTriple>,
    ) -> ShardedStore {
        let n = num_shards.max(1);
        let mut buckets: Vec<Vec<IdTriple>> = vec![Vec::new(); n];
        for t in triples {
            buckets[shard_of(t.sub, n)].push(t);
        }
        let shards = buckets
            .into_iter()
            .map(|b| {
                let mut s = TripleStore::new();
                s.bulk_load(b);
                s
            })
            .collect();
        ShardedStore { shards }
    }

    /// Build from an existing [`TripleStore`] by re-partitioning its triples.
    pub fn from_store(num_shards: usize, store: &TripleStore) -> ShardedStore {
        ShardedStore::from_triples(num_shards, store.iter_all())
    }

    /// Number of shards.
    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    /// Borrow shard `i` (for inspection / per-shard stats).
    pub fn shard(&self, i: usize) -> &TripleStore {
        &self.shards[i]
    }

    /// The shard index a subject lives on.
    pub fn shard_index(&self, sub: EntityLiteralId) -> usize {
        shard_of(sub, self.shards.len())
    }

    /// Per-shard triple counts (for diagnostics / asserting spread).
    pub fn shard_sizes(&self) -> Vec<u64> {
        self.shards.iter().map(TripleStore::triple_count).collect()
    }

    /// Run a read query (SELECT / ASK / CONSTRUCT / DESCRIBE) over this sharded
    /// store using the generic [`Evaluator`] — the scatter-gather is transparent
    /// to the engine. Convenience wrapper over `Evaluator::new(dict, self)`.
    pub fn query(&self, dict: &Dictionary, sparql_text: &str) -> Result<QueryResult> {
        let q = sparql::parse(sparql_text)?;
        Evaluator::new(dict, self).evaluate(&q)
    }
}

impl TripleSource for ShardedStore {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        // A subject co-locates on one shard, but OR-ing across all is equally
        // correct and robust to how the store was built.
        self.shards.iter().any(|s| s.exists(sub, pred, obj))
    }

    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        // Subject-rooted: lives on one shard. Still merge defensively so the
        // result is sorted-unique by (pred, obj), matching the single store.
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend_from_slice(s.po_by_s(sub));
        }
        sort_dedup(out)
    }

    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.o_by_sp(sub, pred));
        }
        sort_dedup(out)
    }

    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.p_by_so(sub, obj));
        }
        sort_dedup(out)
    }

    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        // Object-rooted: the same object is reached from subjects on different
        // shards — must gather all shards and re-sort/dedup by (pred, sub).
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend_from_slice(s.ps_by_o(obj));
        }
        sort_dedup(out)
    }

    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.s_by_po(pred, obj));
        }
        sort_dedup(out)
    }

    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        // Predicate-rooted: a predicate spans shards — gather + re-sort/dedup.
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend_from_slice(s.so_by_p(pred));
        }
        sort_dedup(out)
    }

    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.subs_by_p(pred));
        }
        sort_dedup(out)
    }

    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.objs_by_p(pred));
        }
        sort_dedup(out)
    }

    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.subject_keys());
        }
        sort_dedup(out)
    }

    fn object_keys(&self) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.object_keys());
        }
        sort_dedup(out)
    }

    fn triple_count(&self) -> u64 {
        // Each triple lives on exactly one shard → the sum is the global count.
        self.shards.iter().map(TripleStore::triple_count).sum()
    }

    fn distinct_subjects(&self) -> usize {
        // Subjects co-locate (disjoint across shards) but dedup anyway so the
        // count is global-distinct, never a naive sum.
        self.subject_keys().len()
    }

    fn distinct_objects(&self) -> usize {
        // Objects DO repeat across shards → must dedup, not sum.
        self.object_keys().len()
    }

    fn num_predicates(&self) -> usize {
        // Predicates repeat across shards → dedup.
        let mut preds: HashSet<PredId> = HashSet::new();
        for s in &self.shards {
            preds.extend(s.predicates());
        }
        preds.len()
    }

    fn pred_card(&self, pred: PredId) -> usize {
        // Triple counts partition cleanly → sum is correct.
        self.shards.iter().map(|s| s.pred_card(pred)).sum()
    }

    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        // Global-distinct subjects of the predicate (dedup across shards).
        self.subs_by_p(pred).len()
    }

    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        // Global-distinct objects of the predicate (dedup across shards).
        self.objs_by_p(pred).len()
    }

    fn iter_all(&self) -> Vec<IdTriple> {
        // A single TripleStore yields triples in predicate-major (pred, sub,
        // obj) order (its p2so index drives iteration). Reproduce that exactly:
        // for each distinct predicate ascending, emit the merged (sub, obj)
        // pairs (already sorted-unique from `so_by_p`).
        let mut preds: Vec<PredId> = Vec::new();
        for s in &self.shards {
            preds.extend(s.predicates());
        }
        let preds = sort_dedup(preds);
        let mut out = Vec::new();
        for pred in preds {
            for (sub, obj) in self.so_by_p(pred) {
                out.push(IdTriple::new(sub, pred, obj));
            }
        }
        out
    }
}

// ===========================================================================
// Network-distributed sharding: ShardNode (server) + RemoteShard (client) +
// NetworkShardedStore (the distributed scatter-gather over local/remote shards).
// ===========================================================================

/// A shard server: owns one local [`TripleStore`] and answers per-shard RPCs
/// over TCP. The [`gnode`](../bin/gnode/index.html) binary is a thin wrapper.
///
/// Like [`crate::server::Server`] it is a blocking, `std`-only `std::net` server;
/// unlike it, each accepted connection is handled on its own thread and may
/// carry **many** requests (the [`RemoteShard`] client keeps one connection per
/// shard and reuses it across a scatter-gather). The store is shared behind a
/// [`Mutex`] so reads and routed inserts are serialized per node.
///
/// Every connection's reads and writes are bounded by an I/O timeout
/// ([`ShardNode::DEFAULT_IO_TIMEOUT`], see [`ShardNode::with_io_timeout`]): a
/// client that goes idle, stalls mid-frame or stops reading its replies is
/// disconnected instead of pinning a thread forever (review cluster-nlqa#9).
/// A [`RemoteShard`] whose idle connection was closed reconnects on its next
/// call. The node is still unauthenticated and still spends one thread per
/// open connection — trusted networks only (see [`crate::rpc`]).
#[derive(Debug)]
pub struct ShardNode {
    store: Arc<Mutex<TripleStore>>,
    listener: TcpListener,
    io_timeout: Duration,
    /// Largest reply payload sent; a bigger answer is replaced by a
    /// [`Response::Error`] ([`rpc::MAX_FRAME`]; tests lower it).
    reply_limit: usize,
}

impl ShardNode {
    /// Default bound on each read from / write to a client connection — how
    /// long a connection may sit idle between requests, or stall inside one.
    pub const DEFAULT_IO_TIMEOUT: Duration = Duration::from_secs(60);

    /// Bind to `addr` (e.g. `"127.0.0.1:0"` for an ephemeral port), taking
    /// ownership of this node's local shard.
    pub fn bind<A: ToSocketAddrs>(store: TripleStore, addr: A) -> io::Result<ShardNode> {
        let listener = TcpListener::bind(addr)?;
        Ok(ShardNode {
            store: Arc::new(Mutex::new(store)),
            listener,
            io_timeout: Self::DEFAULT_IO_TIMEOUT,
            reply_limit: rpc::MAX_FRAME,
        })
    }

    /// Use `timeout` (clamped to at least 1 ms) instead of
    /// [`ShardNode::DEFAULT_IO_TIMEOUT`] for every read and write on the
    /// connections this node accepts.
    pub fn with_io_timeout(mut self, timeout: Duration) -> ShardNode {
        self.io_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// The actual bound address (useful after binding to port 0).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept connections forever, handling each on its own thread.
    pub fn serve_forever(&self) {
        for stream in self.listener.incoming().flatten() {
            // Request/response traffic: push each small reply frame out at once
            // rather than letting Nagle hold it for the client's delayed ACK;
            // and bound every read/write so an idle or stalled client cannot
            // hold its connection thread forever.
            if rpc::tune_stream(&stream, Some(self.io_timeout)).is_err() {
                continue;
            }
            let store = Arc::clone(&self.store);
            let limit = self.reply_limit;
            thread::spawn(move || serve_connection(store, stream, limit));
        }
    }
}

/// Serve one client connection: decode framed [`Request`]s and reply with framed
/// [`Response`]s until the peer closes, a read or write times out, or an I/O
/// error occurs. A reply larger than `reply_limit` is answered with a
/// [`Response::Error`] naming its size, and the connection stays usable.
fn serve_connection(store: Arc<Mutex<TripleStore>>, mut stream: TcpStream, reply_limit: usize) {
    loop {
        let bytes = match rpc::read_message(&mut stream) {
            Ok(b) => b,
            Err(_) => return, // peer closed, timed out, or framing error
        };
        let resp = match Request::decode(&bytes) {
            Ok(req) => dispatch(&store, &req),
            Err(e) => Response::Error(e.to_string()),
        };
        let reply = rpc::encode_reply_within(&resp, reply_limit);
        if rpc::write_message(&mut stream, &reply).is_err() {
            return;
        }
    }
}

/// Execute one [`Request`] against the node's local [`TripleStore`], producing
/// the matching [`Response`]. Mirrors the [`TripleSource`] access patterns plus
/// the routed [`Request::Insert`].
fn dispatch(store: &Mutex<TripleStore>, req: &Request) -> Response {
    match *req {
        Request::Exists { sub, pred, obj } => {
            Response::Bool(store.lock().unwrap().exists(sub, pred, obj))
        }
        Request::PoByS { sub } => Response::Pairs(store.lock().unwrap().po_by_s(sub).to_vec()),
        Request::OBySp { sub, pred } => Response::Ids(store.lock().unwrap().o_by_sp(sub, pred)),
        Request::PBySo { sub, obj } => Response::Ids(store.lock().unwrap().p_by_so(sub, obj)),
        Request::PsByO { obj } => Response::Pairs(store.lock().unwrap().ps_by_o(obj).to_vec()),
        Request::SByPo { pred, obj } => Response::Ids(store.lock().unwrap().s_by_po(pred, obj)),
        Request::SoByP { pred } => Response::Pairs(store.lock().unwrap().so_by_p(pred).to_vec()),
        Request::SubsByP { pred } => Response::Ids(store.lock().unwrap().subs_by_p(pred)),
        Request::ObjsByP { pred } => Response::Ids(store.lock().unwrap().objs_by_p(pred)),
        Request::SubjectKeys => Response::Ids(store.lock().unwrap().subject_keys().collect()),
        Request::ObjectKeys => Response::Ids(store.lock().unwrap().object_keys().collect()),
        Request::Predicates => Response::Ids(store.lock().unwrap().predicates().collect()),
        Request::TripleCount => Response::Count(store.lock().unwrap().triple_count()),
        Request::PredCard { pred } => Response::Count(store.lock().unwrap().pred_card(pred) as u64),
        Request::Insert { sub, pred, obj } => {
            Response::Bool(store.lock().unwrap().insert(IdTriple::new(sub, pred, obj)))
        }
        // Shard migration: export / purge one bucket of this node's data.
        Request::ScanShard { shard, num_shards } => {
            Response::Triples(scan_bucket(&store.lock().unwrap(), shard, num_shards))
        }
        Request::PurgeShard { shard, num_shards } => {
            Response::Count(purge_bucket(&mut store.lock().unwrap(), shard, num_shards))
        }
        // Raft / cluster-control RPCs are not part of the plain shard surface; a
        // [`ShardNode`] is a single-replica shard. They are served by
        // [`ClusterNode`] instead (see below). `GetConfig` likewise needs a
        // replicated config, which a plain shard node does not hold.
        Request::RequestVote { .. }
        | Request::AppendEntries { .. }
        | Request::InstallSnapshot { .. }
        | Request::ClientWrite { .. }
        | Request::ClusterStatus
        | Request::GetConfig => {
            Response::Error("cluster-control RPC sent to a non-replicated shard node".to_string())
        }
    }
}

/// Collect every triple in `store` whose subject hashes to bucket `shard` out of
/// `num_shards` — the data unit moved during a rebalance. A zero `num_shards`
/// yields nothing (a degenerate, member-less config).
fn scan_bucket(store: &TripleStore, shard: u32, num_shards: u32) -> Vec<IdTriple> {
    if num_shards == 0 {
        return Vec::new();
    }
    let (shard, n) = (shard as usize, num_shards as usize);
    store
        .iter_all()
        .filter(|t| shard_of(t.sub, n) == shard)
        .collect()
}

/// Remove every triple of bucket `shard` (of `num_shards`) from `store`, after it
/// has been streamed to the bucket's new owner. Returns the number removed.
fn purge_bucket(store: &mut TripleStore, shard: u32, num_shards: u32) -> u64 {
    let doomed = scan_bucket(store, shard, num_shards);
    let mut removed = 0;
    for t in doomed {
        if store.remove(t) {
            removed += 1;
        }
    }
    removed
}

/// A TCP client to a [`ShardNode`]. Holds a lazily-opened, reused connection
/// behind a [`Mutex`] (so the read methods can take `&self` and still be `Sync`);
/// a broken connection is dropped and re-established on the next call.
///
/// Every connect, read and write is bounded by a timeout
/// ([`RemoteShard::DEFAULT_TIMEOUT`], overridable with
/// [`RemoteShard::with_timeout`]), so a hung node surfaces as an error instead
/// of blocking the coordinator forever.
#[derive(Debug)]
pub struct RemoteShard {
    addr: SocketAddr,
    timeout: Duration,
    conn: Mutex<Option<TcpStream>>,
}

impl RemoteShard {
    /// Default bound on connecting to, writing to, or waiting for a reply from a
    /// shard node. Generous, because a migration scan of a large bucket is a
    /// single request; an unresponsive node still fails rather than hangs.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

    /// Resolve `addr` to a concrete [`SocketAddr`] now; the TCP connection is
    /// opened lazily on the first call (so a coordinator can be assembled before
    /// every node is up).
    pub fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<RemoteShard> {
        let addr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no socket address"))?;
        Ok(RemoteShard {
            addr,
            timeout: Self::DEFAULT_TIMEOUT,
            conn: Mutex::new(None),
        })
    }

    /// Use `timeout` (clamped to at least 1 ms) for connecting and for every
    /// read/write on the connection.
    pub fn with_timeout(mut self, timeout: Duration) -> RemoteShard {
        self.timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// The node address this client targets.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Round-trip one request, opening the connection if needed and retrying
    /// once on a fresh connection if the existing one is broken.
    fn call(&self, req: &Request) -> io::Result<Response> {
        let mut guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        for attempt in 0..2 {
            if guard.is_none() {
                let stream = TcpStream::connect_timeout(&self.addr, self.timeout)?;
                rpc::tune_stream(&stream, Some(self.timeout))?;
                *guard = Some(stream);
            }
            let stream = guard.as_mut().expect("connection was just established");
            match rpc::round_trip(stream, req) {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    *guard = None; // drop the broken connection
                    if attempt == 1 {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!("the loop returns on the second attempt")
    }

    /// Issue one *read* RPC and decode its answer, labelling any failure with
    /// this shard's address and the operation so the error a query surfaces
    /// says which shard could not answer.
    fn read<T>(
        &self,
        op: &'static str,
        req: Request,
        decode: impl FnOnce(Response) -> io::Result<T>,
    ) -> io::Result<T> {
        self.call(&req).and_then(decode).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("shard {} could not answer {op}: {e}", self.addr),
            )
        })
    }

    /// Total triples held by the remote shard.
    pub fn triple_count(&self) -> io::Result<u64> {
        self.read_triple_count()
    }

    /// Insert a routed triple on the remote shard; returns whether it was new.
    pub fn insert(&self, t: IdTriple) -> io::Result<bool> {
        self.call(&Request::Insert {
            sub: t.sub,
            pred: t.pred,
            obj: t.obj,
        })?
        .into_bool()
    }

    /// Export this remote node's triples for bucket `shard` of `num_shards`
    /// (migration source side).
    pub fn scan_shard(&self, shard: u32, num_shards: u32) -> io::Result<Vec<IdTriple>> {
        self.call(&Request::ScanShard { shard, num_shards })?
            .into_triples()
    }

    /// Delete bucket `shard` of `num_shards` from this remote node after its data
    /// has been migrated away; returns the number removed.
    pub fn purge_shard(&self, shard: u32, num_shards: u32) -> io::Result<u64> {
        self.call(&Request::PurgeShard { shard, num_shards })?
            .into_count()
    }
}

/// The fallible per-shard read primitives a scatter-gather fans out to — one
/// access pattern each, mirroring the [`Request`] read set. An in-process
/// [`TripleStore`] never fails; a [`RemoteShard`] fails when its RPC does, and
/// that failure must reach the caller rather than be merged in as an empty
/// answer.
trait ShardRead {
    fn read_exists(
        &self,
        sub: EntityLiteralId,
        pred: PredId,
        obj: EntityLiteralId,
    ) -> io::Result<bool>;
    fn read_po_by_s(&self, sub: EntityLiteralId) -> io::Result<Vec<(PredId, EntityLiteralId)>>;
    fn read_o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> io::Result<Vec<EntityLiteralId>>;
    fn read_p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> io::Result<Vec<PredId>>;
    fn read_ps_by_o(&self, obj: EntityLiteralId) -> io::Result<Vec<(PredId, EntityLiteralId)>>;
    fn read_s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> io::Result<Vec<EntityLiteralId>>;
    fn read_so_by_p(&self, pred: PredId) -> io::Result<Vec<(EntityLiteralId, EntityLiteralId)>>;
    fn read_subs_by_p(&self, pred: PredId) -> io::Result<Vec<EntityLiteralId>>;
    fn read_objs_by_p(&self, pred: PredId) -> io::Result<Vec<EntityLiteralId>>;
    fn read_subject_keys(&self) -> io::Result<Vec<EntityLiteralId>>;
    fn read_object_keys(&self) -> io::Result<Vec<EntityLiteralId>>;
    fn read_predicates(&self) -> io::Result<Vec<PredId>>;
    fn read_triple_count(&self) -> io::Result<u64>;
    fn read_pred_card(&self, pred: PredId) -> io::Result<u64>;
}

impl ShardRead for TripleStore {
    fn read_exists(
        &self,
        sub: EntityLiteralId,
        pred: PredId,
        obj: EntityLiteralId,
    ) -> io::Result<bool> {
        Ok(self.exists(sub, pred, obj))
    }
    fn read_po_by_s(&self, sub: EntityLiteralId) -> io::Result<Vec<(PredId, EntityLiteralId)>> {
        Ok(self.po_by_s(sub).to_vec())
    }
    fn read_o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> io::Result<Vec<EntityLiteralId>> {
        Ok(self.o_by_sp(sub, pred))
    }
    fn read_p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> io::Result<Vec<PredId>> {
        Ok(self.p_by_so(sub, obj))
    }
    fn read_ps_by_o(&self, obj: EntityLiteralId) -> io::Result<Vec<(PredId, EntityLiteralId)>> {
        Ok(self.ps_by_o(obj).to_vec())
    }
    fn read_s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> io::Result<Vec<EntityLiteralId>> {
        Ok(self.s_by_po(pred, obj))
    }
    fn read_so_by_p(&self, pred: PredId) -> io::Result<Vec<(EntityLiteralId, EntityLiteralId)>> {
        Ok(self.so_by_p(pred).to_vec())
    }
    fn read_subs_by_p(&self, pred: PredId) -> io::Result<Vec<EntityLiteralId>> {
        Ok(self.subs_by_p(pred))
    }
    fn read_objs_by_p(&self, pred: PredId) -> io::Result<Vec<EntityLiteralId>> {
        Ok(self.objs_by_p(pred))
    }
    fn read_subject_keys(&self) -> io::Result<Vec<EntityLiteralId>> {
        Ok(self.subject_keys().collect())
    }
    fn read_object_keys(&self) -> io::Result<Vec<EntityLiteralId>> {
        Ok(self.object_keys().collect())
    }
    fn read_predicates(&self) -> io::Result<Vec<PredId>> {
        Ok(self.predicates().collect())
    }
    fn read_triple_count(&self) -> io::Result<u64> {
        Ok(self.triple_count())
    }
    fn read_pred_card(&self, pred: PredId) -> io::Result<u64> {
        Ok(self.pred_card(pred) as u64)
    }
}

impl ShardRead for RemoteShard {
    fn read_exists(
        &self,
        sub: EntityLiteralId,
        pred: PredId,
        obj: EntityLiteralId,
    ) -> io::Result<bool> {
        self.read(
            "exists",
            Request::Exists { sub, pred, obj },
            Response::into_bool,
        )
    }
    fn read_po_by_s(&self, sub: EntityLiteralId) -> io::Result<Vec<(PredId, EntityLiteralId)>> {
        self.read("po_by_s", Request::PoByS { sub }, Response::into_pairs)
    }
    fn read_o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> io::Result<Vec<EntityLiteralId>> {
        self.read("o_by_sp", Request::OBySp { sub, pred }, Response::into_ids)
    }
    fn read_p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> io::Result<Vec<PredId>> {
        self.read("p_by_so", Request::PBySo { sub, obj }, Response::into_ids)
    }
    fn read_ps_by_o(&self, obj: EntityLiteralId) -> io::Result<Vec<(PredId, EntityLiteralId)>> {
        self.read("ps_by_o", Request::PsByO { obj }, Response::into_pairs)
    }
    fn read_s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> io::Result<Vec<EntityLiteralId>> {
        self.read("s_by_po", Request::SByPo { pred, obj }, Response::into_ids)
    }
    fn read_so_by_p(&self, pred: PredId) -> io::Result<Vec<(EntityLiteralId, EntityLiteralId)>> {
        self.read("so_by_p", Request::SoByP { pred }, Response::into_pairs)
    }
    fn read_subs_by_p(&self, pred: PredId) -> io::Result<Vec<EntityLiteralId>> {
        self.read("subs_by_p", Request::SubsByP { pred }, Response::into_ids)
    }
    fn read_objs_by_p(&self, pred: PredId) -> io::Result<Vec<EntityLiteralId>> {
        self.read("objs_by_p", Request::ObjsByP { pred }, Response::into_ids)
    }
    fn read_subject_keys(&self) -> io::Result<Vec<EntityLiteralId>> {
        self.read("subject_keys", Request::SubjectKeys, Response::into_ids)
    }
    fn read_object_keys(&self) -> io::Result<Vec<EntityLiteralId>> {
        self.read("object_keys", Request::ObjectKeys, Response::into_ids)
    }
    fn read_predicates(&self) -> io::Result<Vec<PredId>> {
        self.read("predicates", Request::Predicates, Response::into_ids)
    }
    fn read_triple_count(&self) -> io::Result<u64> {
        self.read("triple_count", Request::TripleCount, Response::into_count)
    }
    fn read_pred_card(&self, pred: PredId) -> io::Result<u64> {
        self.read(
            "pred_card",
            Request::PredCard { pred },
            Response::into_count,
        )
    }
}

/// One shard of a [`NetworkShardedStore`]: an **in-process** [`TripleStore`] or
/// a **remote** [`RemoteShard`] reached over TCP. Reads go through the fallible
/// scatter-gather ([`TryTripleSource`]), so a remote shard that cannot answer
/// fails the read instead of contributing an empty answer.
#[derive(Debug)]
pub enum Shard {
    /// An in-process shard.
    Local(TripleStore),
    /// A shard served by a remote [`ShardNode`].
    Remote(RemoteShard),
}

impl Shard {
    /// This shard's fallible read surface.
    fn reader(&self) -> &dyn ShardRead {
        match self {
            Shard::Local(s) => s,
            Shard::Remote(r) => r,
        }
    }

    /// Insert a triple on this shard (the caller has already routed it here).
    fn insert(&mut self, t: IdTriple) -> io::Result<bool> {
        match self {
            Shard::Local(s) => Ok(s.insert(t)),
            Shard::Remote(r) => r.insert(t),
        }
    }

    /// Export the triples of bucket `shard` (of `num_shards`) held here.
    fn scan_shard(&self, shard: u32, num_shards: u32) -> io::Result<Vec<IdTriple>> {
        match self {
            Shard::Local(s) => Ok(scan_bucket(s, shard, num_shards)),
            Shard::Remote(r) => r.scan_shard(shard, num_shards),
        }
    }

    /// Delete bucket `shard` (of `num_shards`) from this shard; returns the count.
    fn purge_shard(&mut self, shard: u32, num_shards: u32) -> io::Result<u64> {
        match self {
            Shard::Local(s) => Ok(purge_bucket(s, shard, num_shards)),
            Shard::Remote(r) => r.purge_shard(shard, num_shards),
        }
    }
}

// ===========================================================================
// Fallible scatter-gather: shard failures are errors, never empty answers.
// ===========================================================================

/// One scatter-gather fan-out: the per-shard read endpoints a single read of a
/// distributed store touches, as resolved by [`TryTripleSource::scatter`]. The
/// merge re-establishes the [`TripleSource`] contract (sorted, de-duplicated
/// `Vec`s; global-distinct counts) — and **fails** as soon as a shard the answer
/// depends on fails, rather than merging it in as empty. Opaque: only this
/// module's distributed stores construct one.
pub struct Scatter<'a> {
    shards: Vec<&'a dyn ShardRead>,
    /// Whether one triple may sit on more than one shard right now (a
    /// [`ShardCluster`] bucket whose migration is unfinished). Lists are
    /// de-duplicated anyway; counts then cannot simply be summed.
    overlapping: bool,
}

impl std::fmt::Debug for Scatter<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scatter")
            .field("shards", &self.shards.len())
            .field("overlapping", &self.overlapping)
            .finish()
    }
}

impl<'a> Scatter<'a> {
    fn new(shards: Vec<&'a dyn ShardRead>) -> Scatter<'a> {
        Scatter {
            shards,
            overlapping: false,
        }
    }

    /// Mark the shards as possibly holding copies of the same triples.
    fn overlapping(mut self, overlapping: bool) -> Scatter<'a> {
        self.overlapping = overlapping;
        self
    }

    /// The number of distinct triples with predicate `pred`: the per-shard
    /// counts summed, or — while shards may overlap — the size of the merged,
    /// de-duplicated `(sub, obj)` list.
    fn pred_card(&self, pred: PredId) -> io::Result<u64> {
        if self.overlapping {
            return Ok(self.gather(|s| s.read_so_by_p(pred))?.len() as u64);
        }
        self.sum(|s| s.read_pred_card(pred))
    }

    /// The number of distinct triples: the per-shard counts summed, or — while
    /// shards may overlap, when a sum would count a copied triple twice
    /// (review cluster-nlqa#1) — the distinct triples counted per predicate.
    fn triple_count(&self) -> io::Result<u64> {
        if !self.overlapping {
            return self.sum(|s| s.read_triple_count());
        }
        let mut total = 0;
        for pred in self.gather(|s| s.read_predicates())? {
            total += self.pred_card(pred)?;
        }
        Ok(total)
    }

    /// `true` as soon as any shard holds the triple (a found triple is found no
    /// matter what another shard says); `false` only when *every* shard
    /// answered "absent"; otherwise the first shard error.
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> io::Result<bool> {
        let mut first_err = None;
        for s in &self.shards {
            match s.read_exists(sub, pred, obj) {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(false),
        }
    }

    /// Concatenate every shard's list and restore the sorted-unique contract;
    /// the first shard error aborts the gather.
    fn gather<T: Ord>(
        &self,
        read: impl Fn(&dyn ShardRead) -> io::Result<Vec<T>>,
    ) -> io::Result<Vec<T>> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(read(*s)?);
        }
        Ok(sort_dedup(out))
    }

    /// Sum a per-shard count (counts partition cleanly by subject); the first
    /// shard error aborts the sum.
    fn sum(&self, read: impl Fn(&dyn ShardRead) -> io::Result<u64>) -> io::Result<u64> {
        let mut total = 0;
        for s in &self.shards {
            total += read(*s)?;
        }
        Ok(total)
    }
}

/// The **fallible** read surface of a distributed (scatter-gather) store: one
/// method per [`TripleSource`] method, returning `Err` when a shard the read
/// needs cannot answer — an unreachable / failed remote shard, or a bucket whose
/// every replica is down — instead of merging that shard in as empty, which
/// would make a query return a *partial* answer that looks complete.
///
/// The distributed stores ([`NetworkShardedStore`], [`ShardCluster`],
/// [`ShardedRaftCluster`]) implement this **instead of** [`TripleSource`]. To run
/// the generic [`Evaluator`] over one, use its `query` method or a [`Checked`]
/// view; both turn a shard failure into an `Err` result.
///
/// Only [`TryTripleSource::scatter`] is required (and only this module can build
/// a [`Scatter`]); every `try_*` method is derived from it.
pub trait TryTripleSource {
    /// Resolve the shard endpoints one read fans out to. `Err` if a shard cannot
    /// be served at all (e.g. every replica of a bucket is down).
    fn scatter(&self) -> io::Result<Scatter<'_>>;

    /// Fallible [`TripleSource::exists`].
    fn try_exists(
        &self,
        sub: EntityLiteralId,
        pred: PredId,
        obj: EntityLiteralId,
    ) -> io::Result<bool> {
        self.scatter()?.exists(sub, pred, obj)
    }
    /// Fallible [`TripleSource::po_by_s`].
    fn try_po_by_s(&self, sub: EntityLiteralId) -> io::Result<Vec<(PredId, EntityLiteralId)>> {
        self.scatter()?.gather(|s| s.read_po_by_s(sub))
    }
    /// Fallible [`TripleSource::o_by_sp`].
    fn try_o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> io::Result<Vec<EntityLiteralId>> {
        self.scatter()?.gather(|s| s.read_o_by_sp(sub, pred))
    }
    /// Fallible [`TripleSource::p_by_so`].
    fn try_p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> io::Result<Vec<PredId>> {
        self.scatter()?.gather(|s| s.read_p_by_so(sub, obj))
    }
    /// Fallible [`TripleSource::ps_by_o`].
    fn try_ps_by_o(&self, obj: EntityLiteralId) -> io::Result<Vec<(PredId, EntityLiteralId)>> {
        self.scatter()?.gather(|s| s.read_ps_by_o(obj))
    }
    /// Fallible [`TripleSource::s_by_po`].
    fn try_s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> io::Result<Vec<EntityLiteralId>> {
        self.scatter()?.gather(|s| s.read_s_by_po(pred, obj))
    }
    /// Fallible [`TripleSource::so_by_p`].
    fn try_so_by_p(&self, pred: PredId) -> io::Result<Vec<(EntityLiteralId, EntityLiteralId)>> {
        self.scatter()?.gather(|s| s.read_so_by_p(pred))
    }
    /// Fallible [`TripleSource::subs_by_p`].
    fn try_subs_by_p(&self, pred: PredId) -> io::Result<Vec<EntityLiteralId>> {
        self.scatter()?.gather(|s| s.read_subs_by_p(pred))
    }
    /// Fallible [`TripleSource::objs_by_p`].
    fn try_objs_by_p(&self, pred: PredId) -> io::Result<Vec<EntityLiteralId>> {
        self.scatter()?.gather(|s| s.read_objs_by_p(pred))
    }
    /// Fallible [`TripleSource::subject_keys`].
    fn try_subject_keys(&self) -> io::Result<Vec<EntityLiteralId>> {
        self.scatter()?.gather(|s| s.read_subject_keys())
    }
    /// Fallible [`TripleSource::object_keys`].
    fn try_object_keys(&self) -> io::Result<Vec<EntityLiteralId>> {
        self.scatter()?.gather(|s| s.read_object_keys())
    }
    /// Fallible [`TripleSource::triple_count`]. Each triple lives on exactly one
    /// shard, so the per-shard sum is the global count — except while a
    /// [`ShardCluster`] migration is unfinished, when the count de-duplicates.
    fn try_triple_count(&self) -> io::Result<u64> {
        self.scatter()?.triple_count()
    }
    /// Fallible [`TripleSource::distinct_subjects`] (global-distinct, not a sum).
    fn try_distinct_subjects(&self) -> io::Result<usize> {
        Ok(self.try_subject_keys()?.len())
    }
    /// Fallible [`TripleSource::distinct_objects`] (objects repeat across shards,
    /// so this de-duplicates rather than sums).
    fn try_distinct_objects(&self) -> io::Result<usize> {
        Ok(self.try_object_keys()?.len())
    }
    /// Fallible [`TripleSource::num_predicates`] (predicates repeat across shards).
    fn try_num_predicates(&self) -> io::Result<usize> {
        Ok(self.scatter()?.gather(|s| s.read_predicates())?.len())
    }
    /// Fallible [`TripleSource::pred_card`] (triple counts partition cleanly,
    /// except while a [`ShardCluster`] migration is unfinished).
    fn try_pred_card(&self, pred: PredId) -> io::Result<usize> {
        Ok(self.scatter()?.pred_card(pred)? as usize)
    }
    /// Fallible [`TripleSource::pred_distinct_subj`].
    fn try_pred_distinct_subj(&self, pred: PredId) -> io::Result<usize> {
        Ok(self.try_subs_by_p(pred)?.len())
    }
    /// Fallible [`TripleSource::pred_distinct_obj`].
    fn try_pred_distinct_obj(&self, pred: PredId) -> io::Result<usize> {
        Ok(self.try_objs_by_p(pred)?.len())
    }
    /// Fallible [`TripleSource::iter_all`], in a single [`TripleStore`]'s
    /// predicate-major `(pred, sub, obj)` order: for each distinct predicate
    /// ascending, the merged sorted-unique `(sub, obj)` pairs.
    fn try_iter_all(&self) -> io::Result<Vec<IdTriple>> {
        let scatter = self.scatter()?;
        let preds = scatter.gather(|s| s.read_predicates())?;
        let mut out = Vec::new();
        for pred in preds {
            for (sub, obj) in scatter.gather(|s| s.read_so_by_p(pred))? {
                out.push(IdTriple::new(sub, pred, obj));
            }
        }
        Ok(out)
    }
}

/// An infallible [`TripleSource`] view over a [`TryTripleSource`], for driving
/// the generic [`Evaluator`] over a distributed store **without** risking a
/// silently partial answer.
///
/// The first shard read error is captured and the view's cancellation flag is
/// raised: every later read short-circuits to an empty answer (no further RPCs)
/// and an evaluator built with [`Evaluator::with_cancel`]`(view.cancel_flag())`
/// aborts at its next pattern boundary. [`Checked::finish`] then replaces
/// whatever the evaluator produced with that error — so always pass the
/// evaluator's result through `finish` (the distributed stores' `query` methods
/// do exactly this).
pub struct Checked<'a, T: TryTripleSource + ?Sized> {
    src: &'a T,
    cancel: Arc<AtomicBool>,
    error: Mutex<Option<io::Error>>,
}

impl<T: TryTripleSource + ?Sized> std::fmt::Debug for Checked<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checked")
            .field("failed", &self.failed())
            .finish()
    }
}

impl<'a, T: TryTripleSource + ?Sized> Checked<'a, T> {
    /// Wrap `src` for one evaluation.
    pub fn new(src: &'a T) -> Checked<'a, T> {
        Checked {
            src,
            cancel: Arc::new(AtomicBool::new(false)),
            error: Mutex::new(None),
        }
    }

    /// The flag this view raises on the first shard failure; attach it with
    /// [`Evaluator::with_cancel`] so evaluation stops early.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// Whether a shard read has failed during this view's lifetime.
    pub fn failed(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Take the captured shard error, if any.
    pub fn take_error(&self) -> Option<io::Error> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Resolve an evaluation run over this view: the captured shard error (as
    /// [`GStoreError::Io`]) if any read failed, otherwise `result` unchanged.
    pub fn finish<R>(&self, result: Result<R>) -> Result<R> {
        match self.take_error() {
            Some(e) => Err(GStoreError::Io(e)),
            None => result,
        }
    }

    /// Run one fallible read; on failure record the first error, raise the
    /// cancel flag and yield an empty answer (which `finish` later overrides).
    fn guard<R: Default>(&self, read: impl FnOnce(&T) -> io::Result<R>) -> R {
        if self.failed() {
            return R::default();
        }
        match read(self.src) {
            Ok(v) => v,
            Err(e) => {
                let mut slot = self.error.lock().unwrap_or_else(|p| p.into_inner());
                slot.get_or_insert(e);
                self.cancel.store(true, Ordering::SeqCst);
                R::default()
            }
        }
    }
}

impl<T: TryTripleSource + ?Sized> TripleSource for Checked<'_, T> {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        self.guard(|s| s.try_exists(sub, pred, obj))
    }
    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        self.guard(|s| s.try_po_by_s(sub))
    }
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        self.guard(|s| s.try_o_by_sp(sub, pred))
    }
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        self.guard(|s| s.try_p_by_so(sub, obj))
    }
    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        self.guard(|s| s.try_ps_by_o(obj))
    }
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        self.guard(|s| s.try_s_by_po(pred, obj))
    }
    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        self.guard(|s| s.try_so_by_p(pred))
    }
    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        self.guard(|s| s.try_subs_by_p(pred))
    }
    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        self.guard(|s| s.try_objs_by_p(pred))
    }
    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        self.guard(|s| s.try_subject_keys())
    }
    fn object_keys(&self) -> Vec<EntityLiteralId> {
        self.guard(|s| s.try_object_keys())
    }
    fn triple_count(&self) -> u64 {
        self.guard(|s| s.try_triple_count())
    }
    fn distinct_subjects(&self) -> usize {
        self.guard(|s| s.try_distinct_subjects())
    }
    fn distinct_objects(&self) -> usize {
        self.guard(|s| s.try_distinct_objects())
    }
    fn num_predicates(&self) -> usize {
        self.guard(|s| s.try_num_predicates())
    }
    fn pred_card(&self, pred: PredId) -> usize {
        self.guard(|s| s.try_pred_card(pred))
    }
    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        self.guard(|s| s.try_pred_distinct_subj(pred))
    }
    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        self.guard(|s| s.try_pred_distinct_obj(pred))
    }
    fn iter_all(&self) -> Vec<IdTriple> {
        self.guard(|s| s.try_iter_all())
    }
}

/// Parse and evaluate a read query over a distributed store through a
/// [`Checked`] view: a shard that fails mid-query turns the whole query into an
/// `Err` (naming the shard), never a silently partial result.
fn query_checked<T: TryTripleSource + ?Sized>(
    src: &T,
    dict: &Dictionary,
    sparql_text: &str,
) -> Result<QueryResult> {
    let q = sparql::parse(sparql_text)?;
    let view = Checked::new(src);
    let result = Evaluator::new(dict, &view)
        .with_cancel(view.cancel_flag())
        .evaluate(&q);
    view.finish(result)
}

/// A triple set partitioned across `N` shards — a mix of **in-process**
/// ([`Shard::Local`]) and **remote, TCP-served** ([`Shard::Remote`]) shards —
/// queried by the same scatter-gather as [`ShardedStore`] and supporting routed
/// inserts. Reads go through [`TryTripleSource`] (a failed remote shard is an
/// error, not an empty answer); [`NetworkShardedStore::query`] runs the generic
/// [`Evaluator`] over a genuinely distributed deployment unchanged.
///
/// The partition rule is identical to [`ShardedStore`] (`hash(subject) % N`), so
/// every subject's adjacency co-locates on one shard and the per-shard counts
/// sum cleanly; the merge re-establishes the trait's sorted/de-duplicated `Vec`
/// contract so this store is observationally identical to a single
/// [`TripleStore`] holding the same triples — whenever every shard answers.
#[derive(Debug)]
pub struct NetworkShardedStore {
    shards: Vec<Shard>,
}

impl NetworkShardedStore {
    /// Build from an explicit set of shards (clamped to at least one shard — an
    /// empty list becomes a single empty in-process shard).
    pub fn new(shards: Vec<Shard>) -> NetworkShardedStore {
        let shards = if shards.is_empty() {
            vec![Shard::Local(TripleStore::new())]
        } else {
            shards
        };
        NetworkShardedStore { shards }
    }

    /// Number of shards.
    pub fn num_shards(&self) -> usize {
        self.shards.len()
    }

    /// Borrow shard `i`.
    pub fn shard(&self, i: usize) -> &Shard {
        &self.shards[i]
    }

    /// The shard index a subject is routed to.
    pub fn shard_index(&self, sub: EntityLiteralId) -> usize {
        shard_of(sub, self.shards.len())
    }

    /// Route a triple to `hash(subject) % N` and insert it on that shard (over
    /// RPC for a remote shard). Returns whether it was newly added. I/O errors
    /// to a remote shard are surfaced to the caller.
    pub fn insert(&mut self, t: IdTriple) -> io::Result<bool> {
        let idx = shard_of(t.sub, self.shards.len());
        self.shards[idx].insert(t)
    }

    /// Routed bulk insert; returns the number of triples newly added.
    pub fn insert_all(&mut self, triples: impl IntoIterator<Item = IdTriple>) -> io::Result<u64> {
        let mut added = 0;
        for t in triples {
            if self.insert(t)? {
                added += 1;
            }
        }
        Ok(added)
    }

    /// Run a read query over this distributed store using the generic
    /// [`Evaluator`] — the scatter-gather (local + remote) is transparent to the
    /// engine. If any shard cannot answer, the query fails with
    /// [`GStoreError::Io`] naming that shard rather than returning a partial
    /// result.
    pub fn query(&self, dict: &Dictionary, sparql_text: &str) -> Result<QueryResult> {
        query_checked(self, dict, sparql_text)
    }
}

impl TryTripleSource for NetworkShardedStore {
    fn scatter(&self) -> io::Result<Scatter<'_>> {
        Ok(Scatter::new(
            self.shards.iter().map(Shard::reader).collect(),
        ))
    }
}

// ===========================================================================
// Elastic cluster membership + shard rebalancing.
//
// [`ClusterConfig`] is the cluster's *configuration object*: the set of member
// nodes (id → address) and the **shard → owner** assignment. Membership changes
// are replicated as Raft **config-change** log entries
// ([`LogOp::AddNode`](crate::rpc::LogOp::AddNode) /
// [`LogOp::RemoveNode`](crate::rpc::LogOp::RemoveNode)): committed by quorum and
// applied by [`ClusterConfig::apply`] on every replica, so all nodes converge on
// the same membership — see [`ClusterNode`] below, which holds one and serves it
// over [`Request::GetConfig`].
//
// The shard→owner map uses **rendezvous (highest-random-weight) hashing**, a
// consistent-hashing scheme: adding or removing a node only re-homes the buckets
// that node owns (minimal movement), and every replica computes the identical map
// from the same member set. [`ShardCluster`] is the data-plane coordinator: it
// holds the member shard endpoints, routes each write to its bucket's owner, and
// on a membership change **migrates the affected buckets' triples over RPC**
// (stream from old owner → insert on new owner → purge old) before cutting over
// routing to the new owner — so reads/writes stay correct across a rebalance.
// ===========================================================================

/// The replicated description of a running cluster: its member nodes (id →
/// address) plus the shard→owner assignment, tagged with a monotonic `epoch`.
///
/// A change is a single config-change committed through the Raft log
/// ([`LogOp::AddNode`] /
/// [`LogOp::RemoveNode`]); applying the committed
/// op with [`ClusterConfig::apply`] is deterministic, so every replica that has
/// seen the same committed prefix holds a byte-identical config. The shard→owner
/// map is recomputed by rendezvous hashing over the (sorted) member set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClusterConfig {
    epoch: u64,
    num_shards: usize,
    members: BTreeMap<NodeId, SocketAddr>,
    /// `assignment[i]` is the owner of bucket `i`; empty when there are no
    /// members (nothing can be routed yet).
    assignment: Vec<NodeId>,
}

/// Rendezvous (HRW) owner of `shard` among `members` (must be non-empty): the
/// member maximizing `hash(shard, member)`. Deterministic and stable — removing a
/// non-owner leaves a bucket's owner unchanged, so only the departing/arriving
/// node's buckets move.
fn rendezvous_owner(shard: usize, members: &[NodeId]) -> NodeId {
    members
        .iter()
        .copied()
        .max_by_key(|&m| {
            let mut h = DefaultHasher::new();
            (shard as u64).hash(&mut h);
            m.hash(&mut h);
            h.finish()
        })
        .expect("rendezvous_owner requires a non-empty member set")
}

impl ClusterConfig {
    /// An empty configuration partitioned into `num_shards` buckets (no members
    /// yet, so nothing is routable until the first [`ClusterConfig::add_node`]).
    pub fn new(num_shards: usize) -> ClusterConfig {
        ClusterConfig {
            epoch: 0,
            num_shards,
            members: BTreeMap::new(),
            assignment: Vec::new(),
        }
    }

    /// The configuration version; bumped by every committed membership change.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    /// Number of shard buckets.
    pub fn num_shards(&self) -> usize {
        self.num_shards
    }
    /// Member node ids, ascending.
    pub fn members(&self) -> Vec<NodeId> {
        self.members.keys().copied().collect()
    }
    /// Member `(id, addr)` pairs, ascending by id.
    pub fn member_addrs(&self) -> Vec<(NodeId, SocketAddr)> {
        self.members.iter().map(|(&id, &a)| (id, a)).collect()
    }
    /// The address of member `id`, if a member.
    pub fn addr(&self, id: NodeId) -> Option<SocketAddr> {
        self.members.get(&id).copied()
    }
    /// Whether `id` is a member.
    pub fn contains(&self, id: NodeId) -> bool {
        self.members.contains_key(&id)
    }
    /// The shard→owner assignment (`assignment()[i]` owns bucket `i`).
    pub fn assignment(&self) -> &[NodeId] {
        &self.assignment
    }
    /// The owner of bucket `shard`, if assigned.
    pub fn owner(&self, shard: usize) -> Option<NodeId> {
        self.assignment.get(shard).copied()
    }
    /// The owner of the bucket a subject routes to, if any member exists.
    pub fn owner_of_subject(&self, sub: EntityLiteralId) -> Option<NodeId> {
        if self.num_shards == 0 || self.members.is_empty() {
            return None;
        }
        self.owner(shard_of(sub, self.num_shards))
    }

    /// Recompute the shard→owner map by rendezvous hashing over the current
    /// members (empty when there are none).
    fn reassign(&mut self) {
        if self.members.is_empty() {
            self.assignment = Vec::new();
            return;
        }
        let ms: Vec<NodeId> = self.members.keys().copied().collect();
        self.assignment = (0..self.num_shards)
            .map(|s| rendezvous_owner(s, &ms))
            .collect();
    }

    /// Admit node `id` (reachable at `addr`). Returns `true` if this changed the
    /// config (a new member), bumping the epoch and recomputing the assignment;
    /// `false` if `id` was already a member.
    pub fn add_node(&mut self, id: NodeId, addr: SocketAddr) -> bool {
        if self.members.contains_key(&id) {
            return false;
        }
        self.members.insert(id, addr);
        self.epoch += 1;
        self.reassign();
        true
    }

    /// Evict node `id`. Returns `true` if this changed the config, bumping the
    /// epoch and recomputing the assignment; `false` if `id` was not a member.
    pub fn remove_node(&mut self, id: NodeId) -> bool {
        if self.members.remove(&id).is_none() {
            return false;
        }
        self.epoch += 1;
        self.reassign();
        true
    }

    /// Apply a committed config-change log entry. Data-plane ops are ignored
    /// (they belong to the triple store, not the membership config).
    pub fn apply(&mut self, op: &LogOp) -> bool {
        match *op {
            LogOp::AddNode { id, addr } => self.add_node(id, addr),
            LogOp::RemoveNode { id } => self.remove_node(id),
            LogOp::Insert(_) | LogOp::Delete(_) | LogOp::Noop => false,
        }
    }
}

/// The data-plane coordinator of an **elastic** sharded store: it owns one shard
/// endpoint ([`Shard`], local or remote) per member node and a [`ClusterConfig`]
/// describing the shard→owner map. Routing sends each write to its bucket's
/// owner; reads scatter-gather across all members (the same merge as
/// [`NetworkShardedStore`], so it is observationally identical to a single
/// [`TripleStore`]). [`ShardCluster::add_node`] / [`ShardCluster::remove_node`]
/// re-home the affected buckets by **streaming their triples between owners over
/// RPC**, then cut over routing — so a node that joins *receives its shards* and a
/// node that leaves *hands its shards to survivors with no data loss*.
///
/// This is the data plane; the control plane (agreeing on the membership itself)
/// is [`ClusterNode`]'s Raft-replicated [`ClusterConfig`]. A deployment feeds each
/// committed config-change into [`ShardCluster::add_node`]/`remove_node` to enact
/// the migration; the two share the [`ClusterConfig`] type.
///
/// ## Interrupted migrations
///
/// A migration copies a bucket triple by triple over RPC, so it can fail part
/// way (a node dies, a link resets). The routing change is recorded first and
/// every re-homed bucket stays a *pending move* until both its copy and the
/// purge of the source succeed; a failure leaves the bucket **stranded** — its
/// triples may sit on both nodes, which reads de-duplicate, counts included —
/// and returns the error. Moves are idempotent, so they simply run again
/// (review cluster-nlqa#1; before, a retry was a silent no-op and a later
/// leave dropped the stranded triples while every call returned `Ok`):
///
/// * repeating the failed [`ShardCluster::add_node`] / `remove_node`, or
///   [`ShardCluster::resume_migrations`], finishes the stranded buckets
///   ([`ShardCluster::stranded_buckets`] lists them);
/// * any *other* membership change first resumes them, and is refused —
///   changing nothing — while a bucket stays stranded, so a node is never
///   dropped while it may hold the only copy of some triples;
/// * a leaving node is drained of *every* bucket (not only those it owned, so
///   stray copies are not left behind), and its endpoint is dropped only once
///   all of them moved.
///
/// Re-adding an existing member never replaces its endpoint — that silently
/// dropped a `Local` member's data (review cluster-nlqa#10): the same address
/// is a retry, a different one is refused.
#[derive(Debug)]
pub struct ShardCluster {
    config: ClusterConfig,
    endpoints: HashMap<NodeId, Shard>,
    /// Buckets whose re-homing `config` already records but the data plane has
    /// not finished, bucket → move (at most one per bucket).
    pending: BTreeMap<usize, BucketMove>,
}

/// One unfinished bucket migration of a [`ShardCluster`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BucketMove {
    from: NodeId,
    to: NodeId,
}

impl ShardCluster {
    /// An empty cluster partitioned into `num_shards` buckets (no members yet).
    pub fn new(num_shards: usize) -> ShardCluster {
        ShardCluster {
            config: ClusterConfig::new(num_shards),
            endpoints: HashMap::new(),
            pending: BTreeMap::new(),
        }
    }

    /// The current (committed) configuration.
    pub fn config(&self) -> &ClusterConfig {
        &self.config
    }
    /// Number of shard buckets.
    pub fn num_shards(&self) -> usize {
        self.config.num_shards()
    }
    /// Member node ids, ascending.
    pub fn members(&self) -> Vec<NodeId> {
        self.config.members()
    }
    /// The node a subject's writes route to under the current assignment.
    pub fn owner_of_subject(&self, sub: EntityLiteralId) -> Option<NodeId> {
        self.config.owner_of_subject(sub)
    }
    /// Borrow a node's shard endpoint (for inspection / tests): a member's, or
    /// a departed node's that is still being drained.
    pub fn endpoint(&self, id: NodeId) -> Option<&Shard> {
        self.endpoints.get(&id)
    }

    /// The buckets whose migration is unfinished, as `(bucket, from, to)`,
    /// ascending by bucket — empty unless a membership change failed part way.
    /// While any remain their triples may sit on two nodes (reads stay complete
    /// and exact), and other membership changes are refused until
    /// [`ShardCluster::resume_migrations`] (or a retry of the failed change)
    /// finishes them.
    pub fn stranded_buckets(&self) -> Vec<(usize, NodeId, NodeId)> {
        self.pending
            .iter()
            .map(|(&bucket, m)| (bucket, m.from, m.to))
            .collect()
    }

    /// Finish every stranded bucket migration (see
    /// [`ShardCluster::stranded_buckets`]), then drop the endpoints of departed
    /// nodes that hold nothing more. Stops at the first failure and returns it;
    /// the buckets not yet moved stay stranded for a later call. `Ok` at once
    /// when nothing is stranded.
    pub fn resume_migrations(&mut self) -> io::Result<()> {
        let moves: Vec<(usize, BucketMove)> = self.pending.iter().map(|(&b, &m)| (b, m)).collect();
        for (bucket, mv) in moves {
            if let Err(e) = self.migrate_shard(bucket, mv.from, mv.to) {
                return Err(io::Error::new(
                    e.kind(),
                    format!(
                        "migrating bucket {bucket} from node {} to node {} failed; {} bucket(s) \
                         stay stranded until the membership change is retried or \
                         resume_migrations succeeds: {e}",
                        mv.from,
                        mv.to,
                        self.pending.len()
                    ),
                ));
            }
            self.pending.remove(&bucket);
        }
        self.retire_departed();
        Ok(())
    }

    /// Drop the endpoint of every node that is no longer a member and that no
    /// pending move still involves.
    fn retire_departed(&mut self) {
        let (config, pending) = (&self.config, &self.pending);
        self.endpoints.retain(|&id, _| {
            config.contains(id) || pending.values().any(|m| m.from == id || m.to == id)
        });
    }

    /// Admit node `id` (at `addr`) with a freshly bound, empty (or pre-seeded)
    /// shard `endpoint`, then migrate the buckets that rendezvous now assigns to
    /// it from their previous owners — so the new node *receives its shards*. The
    /// first node admitted simply owns every bucket (nothing to migrate yet).
    ///
    /// If a migration fails the error is returned and the unfinished buckets
    /// stay stranded (see the type docs); calling `add_node` again with the same
    /// `id` and `addr` resumes them. For an existing member that retry is all
    /// it does: the registered endpoint is kept and the `endpoint` passed is
    /// dropped (refused if it is a `Local` shard holding triples, which would
    /// be lost); an existing member at a different `addr` is refused with
    /// [`io::ErrorKind::InvalidInput`]. A new member is refused while buckets
    /// of an earlier change stay stranded.
    pub fn add_node(&mut self, id: NodeId, addr: SocketAddr, endpoint: Shard) -> io::Result<()> {
        if self.config.contains(id) {
            let known = self.config.addr(id);
            if known != Some(addr) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "node {id} is already a member at {}; remove it before adding it at {addr}",
                        known.map_or_else(|| "an unknown address".to_string(), |a| a.to_string())
                    ),
                ));
            }
            if let Shard::Local(store) = &endpoint {
                if store.triple_count() > 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "node {id} is already a member; re-adding it with a pre-seeded \
                             local shard would discard that shard's triples"
                        ),
                    ));
                }
            }
            drop(endpoint); // a retry: the member keeps its registered endpoint
            return self.resume_migrations();
        }
        // A different change: finish an earlier one first, or change nothing.
        self.resume_migrations()?;
        let old = self.config.clone();
        self.endpoints.insert(id, endpoint);
        self.config.add_node(id, addr);
        for bucket in 0..self.config.num_shards() {
            if let (Some(from), Some(to)) = (old.owner(bucket), self.config.owner(bucket)) {
                if from != to {
                    self.pending.insert(bucket, BucketMove { from, to });
                }
            }
        }
        self.resume_migrations()
    }

    /// Evict node `id`: migrate every bucket it may hold to that bucket's new
    /// (survivor) owner — no data loss — then drop its endpoint. A no-op if `id`
    /// is neither a member nor a departed node still being drained.
    ///
    /// Refused, changing nothing, while buckets of an earlier change stay
    /// stranded, or when `id` is the last member and still holds triples
    /// (they would have nowhere to go). If a migration fails the error is
    /// returned: `id` has left the membership (writes no longer route to it)
    /// but keeps its endpoint — reads still see its triples — until a retry
    /// of `remove_node(id)` or [`ShardCluster::resume_migrations`] has moved
    /// every bucket.
    pub fn remove_node(&mut self, id: NodeId) -> io::Result<()> {
        if self.config.contains(id) {
            self.resume_migrations()?;
            if self.config.members() == [id] {
                let held = match self.endpoints.get(&id) {
                    Some(ep) => ep.reader().read_triple_count()?,
                    None => 0,
                };
                if held > 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "node {id} is the last member and holds {held} triple(s), which \
                             would have nowhere to go; add another node first"
                        ),
                    ));
                }
            }
            self.config.remove_node(id);
            for bucket in 0..self.config.num_shards() {
                if let Some(to) = self.config.owner(bucket) {
                    self.pending.insert(bucket, BucketMove { from: id, to });
                }
            }
        } else if !self.endpoints.contains_key(&id) {
            return Ok(());
        }
        self.resume_migrations()
    }

    /// Copy bucket `shard` from `from`'s endpoint to `to`'s, then purge it from
    /// `from` (cut-over). Idempotent: run again after a failure at any step it
    /// copies what is left (a triple already on `to` is a no-op insert) and
    /// purges again — which is how a stranded bucket is finished. Nothing is
    /// purged before every scanned triple reached `to`.
    fn migrate_shard(&mut self, shard: usize, from: NodeId, to: NodeId) -> io::Result<()> {
        if from == to {
            return Ok(());
        }
        let n = self.config.num_shards() as u32;
        let triples = match self.endpoints.get(&from) {
            Some(ep) => ep.scan_shard(shard as u32, n)?,
            None => return Ok(()), // the source holds nothing any more
        };
        let dst = self.endpoints.get_mut(&to).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("migration target node {to} has no registered endpoint"),
            )
        })?;
        for t in &triples {
            dst.insert(*t)?;
        }
        if let Some(src) = self.endpoints.get_mut(&from) {
            src.purge_shard(shard as u32, n)?;
        }
        Ok(())
    }

    /// Route a triple to its bucket's owner and insert it there.
    pub fn insert(&mut self, t: IdTriple) -> io::Result<bool> {
        let owner = self.config.owner_of_subject(t.sub).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "cluster has no members to route to",
            )
        })?;
        match self.endpoints.get_mut(&owner) {
            Some(ep) => ep.insert(t),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "routed owner has no registered endpoint",
            )),
        }
    }

    /// Routed bulk insert; returns the number of triples newly added.
    pub fn insert_all(&mut self, triples: impl IntoIterator<Item = IdTriple>) -> io::Result<u64> {
        let mut added = 0;
        for t in triples {
            if self.insert(t)? {
                added += 1;
            }
        }
        Ok(added)
    }

    /// Run a read query over the cluster using the generic [`Evaluator`] — the
    /// scatter-gather across members is transparent to the engine. A member
    /// endpoint that cannot answer fails the query with [`GStoreError::Io`]
    /// rather than yielding a partial result.
    pub fn query(&self, dict: &Dictionary, sparql_text: &str) -> Result<QueryResult> {
        query_checked(self, dict, sparql_text)
    }
}

impl TryTripleSource for ShardCluster {
    /// Every registered endpoint — the members', plus a departed node's that is
    /// still being drained — marked as overlapping while a bucket is stranded
    /// (its triples may then sit on two nodes).
    fn scatter(&self) -> io::Result<Scatter<'_>> {
        Ok(
            Scatter::new(self.endpoints.values().map(Shard::reader).collect())
                .overlapping(!self.pending.is_empty()),
        )
    }
}

// ===========================================================================
// Cluster high availability: a Raft-like replication layer.
//
// A *replicated shard* (or whole DB) is a group of nodes holding identical
// copies of a [`TripleStore`]. One node is the **leader**; clients send writes
// to it, it appends them to a replicated **log** of [`LogOp`]s and ships them to
// **followers** via [`Request::AppendEntries`]. An entry is **committed** once a
// **majority** (quorum) has it, after which every node applies it to its store —
// so all replicas converge. Leadership is granted by majority **vote** in a
// monotonically increasing **term** ([`Request::RequestVote`]); a follower that
// stops hearing heartbeats starts a new election, giving automatic **failover**.
// A lagging / rejoining node catches up by log backfill, or — past the leader's
// compaction point — by [`Request::InstallSnapshot`]. Term checks on every RPC
// give split-brain safety: a stale leader is rejected and steps down.
//
// The module is split into a **pure state machine** ([`RaftState`]) — driven by
// explicit logical `tick`s and message handlers, with no clocks or sockets, so
// it is exhaustively and *deterministically* testable — and a **transport**
// ([`ClusterNode`]) that wraps it with the same `std`-only length-prefixed RPC
// ([`crate::rpc`]) used by the sharding layer, a background tick driver, one
// sender thread per peer (so a slow peer only delays itself), and a
// kill/revive switch for fault-injection tests. Its vocabulary is loosely
// modelled on upstream gStore's `src/Cluster` sources (ClusterManager
// term/quorum/election, ClusterEntityLeader heartbeat/append/failover,
// ClusterEntityFollower, ClusterLog) — which upstream 1.4 does not compile into
// its release — within this crate's zero-extra-dependency stance. Experimental:
// see the module docs.
// ===========================================================================

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::rpc::{LogEntry, LogOp, NodeId};

/// A node's role in the replication protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Passive replica: applies the leader's entries, votes, and (on heartbeat
    /// timeout) becomes a candidate.
    Follower,
    /// Soliciting votes for a new term.
    Candidate,
    /// The single writer for its term: accepts client writes and replicates.
    Leader,
}

impl Role {
    /// Wire encoding used by [`Response::Status`]: 0 follower, 1 candidate, 2
    /// leader.
    pub fn as_u8(self) -> u8 {
        match self {
            Role::Follower => 0,
            Role::Candidate => 1,
            Role::Leader => 2,
        }
    }
}

/// An outgoing Raft message together with the peer it is addressed to. The state
/// machine *describes* the messages to send; the caller (transport or test
/// harness) delivers them.
#[derive(Debug, Clone)]
pub struct Outgoing {
    /// Destination node id.
    pub to: NodeId,
    /// The message to send.
    pub msg: RaftMsg,
}

/// A Raft request message (the request half of the protocol), independent of the
/// wire encoding in [`crate::rpc`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RaftMsg {
    /// A candidate solicits a vote.
    RequestVote {
        term: u64,
        candidate_id: NodeId,
        last_log_index: u64,
        last_log_term: u64,
    },
    /// A leader replicates entries (empty = heartbeat).
    AppendEntries {
        term: u64,
        leader_id: NodeId,
        prev_log_index: u64,
        prev_log_term: u64,
        leader_commit: u64,
        entries: Vec<LogEntry>,
    },
    /// A leader ships a snapshot to a far-behind follower.
    InstallSnapshot {
        term: u64,
        leader_id: NodeId,
        last_included_index: u64,
        last_included_term: u64,
        triples: Vec<IdTriple>,
    },
}

impl RaftMsg {
    /// Build the [`crate::rpc::Request`] that carries this message on the wire.
    fn to_request(&self) -> Request {
        match self {
            RaftMsg::RequestVote {
                term,
                candidate_id,
                last_log_index,
                last_log_term,
            } => Request::RequestVote {
                term: *term,
                candidate_id: *candidate_id,
                last_log_index: *last_log_index,
                last_log_term: *last_log_term,
            },
            RaftMsg::AppendEntries {
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                leader_commit,
                entries,
            } => Request::AppendEntries {
                term: *term,
                leader_id: *leader_id,
                prev_log_index: *prev_log_index,
                prev_log_term: *prev_log_term,
                leader_commit: *leader_commit,
                entries: entries.clone(),
            },
            RaftMsg::InstallSnapshot {
                term,
                leader_id,
                last_included_index,
                last_included_term,
                triples,
            } => Request::InstallSnapshot {
                term: *term,
                leader_id: *leader_id,
                last_included_index: *last_included_index,
                last_included_term: *last_included_term,
                triples: triples.clone(),
            },
        }
    }
}

/// A Raft reply message (the response half).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftReply {
    /// Reply to a vote request.
    Vote { term: u64, granted: bool },
    /// Reply to append-entries: `match_index` is the highest index now agreed
    /// with the leader on success, or a backtrack hint on failure.
    Append {
        term: u64,
        success: bool,
        match_index: u64,
    },
    /// Reply to install-snapshot.
    Snapshot { term: u64 },
}

/// Map a Raft reply onto the wire [`Response`].
fn reply_to_response(r: RaftReply) -> Response {
    match r {
        RaftReply::Vote { term, granted } => Response::Vote { term, granted },
        RaftReply::Append {
            term,
            success,
            match_index,
        } => Response::AppendAck {
            term,
            success,
            match_index,
        },
        RaftReply::Snapshot { term } => Response::SnapshotAck { term },
    }
}

/// Recover a Raft reply from the wire [`Response`].
fn response_to_reply(resp: Response) -> io::Result<RaftReply> {
    match resp {
        Response::Vote { term, granted } => Ok(RaftReply::Vote { term, granted }),
        Response::AppendAck {
            term,
            success,
            match_index,
        } => Ok(RaftReply::Append {
            term,
            success,
            match_index,
        }),
        Response::SnapshotAck { term } => Ok(RaftReply::Snapshot { term }),
        Response::Error(e) => Err(io::Error::other(e)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected response shape for a raft reply",
        )),
    }
}

/// A store mutation produced by applying committed log entries — returned by
/// [`RaftState::drain_apply`] so the owner of the state machine (transport or
/// test) applies it to its [`TripleStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreAction {
    /// Apply a single committed mutation.
    Apply(LogOp),
    /// Replace the entire store contents (snapshot install).
    Reset(Vec<IdTriple>),
}

/// Tunables for the replication protocol. Timeouts are in **logical ticks** so
/// the pure state machine is clock-free; `tick_interval` is the only wall-clock
/// value and is used solely by the [`ClusterNode`] transport's driver thread.
#[derive(Debug, Clone)]
pub struct RaftConfig {
    /// Minimum election timeout in ticks.
    pub election_timeout_min: u64,
    /// Maximum election timeout in ticks (randomized in `[min, max]` to avoid
    /// split votes).
    pub election_timeout_max: u64,
    /// Leader heartbeat period in ticks (must be `<` the election minimum).
    pub heartbeat_timeout: u64,
    /// Wall-clock duration of one driver tick (transport only).
    pub tick_interval: Duration,
}

impl Default for RaftConfig {
    fn default() -> RaftConfig {
        RaftConfig {
            election_timeout_min: 5,
            election_timeout_max: 10,
            heartbeat_timeout: 1,
            tick_interval: Duration::from_millis(15),
        }
    }
}

/// The furthest one message may move a node's term forward. Raft terms grow
/// by one per election round, so a real peer is never billions of terms
/// ahead; a larger jump can only be forged (the protocol is unauthenticated),
/// and adopting it let one message push every node to the last representable
/// term, after which the next election overflowed — a panic that poisoned the
/// node's state in debug builds, a wrap to 0 in release, and in either case a
/// cluster that never elected a leader again (review cluster-nlqa#2). With
/// this bound reaching `u64::MAX` would take billions of forged messages.
const MAX_TERM_STEP: u64 = 1 << 32;

/// The largest log index a peer may claim (snapshot base, match index): far
/// beyond any real log, and far enough from `u64::MAX` that index arithmetic
/// (`index + 1`, `base + len`) cannot overflow (review cluster-nlqa#2).
const MAX_LOG_INDEX: u64 = 1 << 62;

/// The pure Raft state machine for one node. It owns the replicated log and all
/// election/replication bookkeeping but **not** the [`TripleStore`]: committed
/// mutations are handed out via [`RaftState::drain_apply`]. Every input is an
/// explicit method call (`tick`, `handle`, `handle_reply`, `propose`), so the
/// protocol can be driven deterministically with no threads or clocks.
#[derive(Debug)]
pub struct RaftState {
    id: NodeId,
    peers: Vec<NodeId>,

    role: Role,
    current_term: u64,
    voted_for: Option<NodeId>,
    leader_id: Option<NodeId>,

    /// Log entries with indices `base_index+1 ..= base_index+log.len()`.
    log: Vec<LogEntry>,
    /// Highest index folded into the snapshot (0 = none).
    base_index: u64,
    /// Term of the entry at `base_index`.
    base_term: u64,
    /// Materialized snapshot triples (everything up to `base_index`).
    snapshot: Vec<IdTriple>,

    commit_index: u64,
    last_applied: u64,

    // Candidate bookkeeping.
    votes_granted: HashSet<NodeId>,
    // Leader bookkeeping.
    next_index: HashMap<NodeId, u64>,
    match_index: HashMap<NodeId, u64>,

    // Timing (logical ticks).
    election_elapsed: u64,
    heartbeat_elapsed: u64,
    election_timeout: u64,
    cfg_min: u64,
    cfg_max: u64,
    cfg_heartbeat: u64,
    rng: u64,

    /// Pending store-reset from a freshly installed snapshot.
    pending_reset: Option<Vec<IdTriple>>,

    // Durability bookkeeping for a persistent transport (review
    // cluster-nlqa#11): what changed since the last `take_durable_delta`.
    /// `(current_term, voted_for)` as last handed out for persisting.
    durable_hard: (u64, Option<NodeId>),
    /// Lowest log index appended or overwritten since then.
    log_dirty_from: Option<u64>,
    /// The snapshot base moved (install or compaction) since then.
    snapshot_dirty: bool,
}

/// What a durable transport must write before anything a [`RaftState`] step
/// produced — a reply, an outgoing message, a commit — may be seen: the hard
/// state if it changed, and the log suffix that changed (after the snapshot
/// when a snapshot replaced the log prefix). See
/// [`ClusterNode::bind_persistent`].
#[derive(Debug, Default, PartialEq, Eq)]
struct DurableDelta {
    /// New `(current_term, voted_for)`.
    hard: Option<(u64, Option<NodeId>)>,
    /// New snapshot `(base_index, base_term, triples)`; the log restarts after
    /// `base_index`.
    snapshot: Option<(u64, u64, Vec<IdTriple>)>,
    /// `(from, entries)`: the log from index `from` on is now exactly
    /// `entries` (everything at or after `from` before is replaced).
    entries: Option<(u64, Vec<LogEntry>)>,
}

impl DurableDelta {
    fn is_empty(&self) -> bool {
        self.hard.is_none() && self.snapshot.is_none() && self.entries.is_none()
    }
}

impl RaftState {
    /// Create a fresh node `id` whose cluster peers are `peers` (excluding self).
    pub fn new(id: NodeId, peers: Vec<NodeId>, cfg: &RaftConfig) -> RaftState {
        let mut seed = id.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        if seed == 0 {
            seed = 0x1234_5678_9ABC_DEF1;
        }
        let mut s = RaftState {
            id,
            peers,
            role: Role::Follower,
            current_term: 0,
            voted_for: None,
            leader_id: None,
            log: Vec::new(),
            base_index: 0,
            base_term: 0,
            snapshot: Vec::new(),
            commit_index: 0,
            last_applied: 0,
            votes_granted: HashSet::new(),
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            election_elapsed: 0,
            heartbeat_elapsed: 0,
            election_timeout: cfg.election_timeout_min,
            cfg_min: cfg.election_timeout_min,
            cfg_max: cfg.election_timeout_max.max(cfg.election_timeout_min),
            cfg_heartbeat: cfg.heartbeat_timeout.max(1),
            rng: seed,
            pending_reset: None,
            durable_hard: (0, None),
            log_dirty_from: None,
            snapshot_dirty: false,
        };
        s.reset_election_timeout();
        s
    }

    /// Reload the durable state a persistent transport recovered (its term,
    /// vote, snapshot and log). The commit index is volatile in Raft, so it
    /// restarts at the snapshot base; the entries after it are applied again
    /// once a leader re-confirms them.
    fn restore(&mut self, rec: RecoveredState) {
        self.current_term = rec.term;
        self.voted_for = rec.voted_for;
        self.durable_hard = (rec.term, rec.voted_for);
        self.base_index = rec.base_index;
        self.base_term = rec.base_term;
        self.log = rec.log;
        if rec.base_index > 0 {
            self.snapshot = rec.snapshot.clone();
            self.pending_reset = Some(rec.snapshot);
            self.commit_index = rec.base_index;
            self.last_applied = rec.base_index;
        }
    }

    /// Note that the log changed from `index` on.
    fn log_changed_from(&mut self, index: u64) {
        self.log_dirty_from = Some(self.log_dirty_from.map_or(index, |d| d.min(index)));
    }

    /// Hand out — and forget — what changed in the durable state since the
    /// last call (see [`DurableDelta`]).
    fn take_durable_delta(&mut self) -> DurableDelta {
        let mut d = DurableDelta::default();
        let hard = (self.current_term, self.voted_for);
        if hard != self.durable_hard {
            d.hard = Some(hard);
            self.durable_hard = hard;
        }
        if self.snapshot_dirty {
            d.snapshot = Some((self.base_index, self.base_term, self.snapshot.clone()));
            d.entries = Some((self.base_index + 1, self.log.clone()));
        } else if let Some(from) = self.log_dirty_from {
            let from = from.max(self.base_index + 1);
            let start = ((from - self.base_index - 1) as usize).min(self.log.len());
            d.entries = Some((from, self.log[start..].to_vec()));
        }
        self.snapshot_dirty = false;
        self.log_dirty_from = None;
        d
    }

    // --- read-only accessors -------------------------------------------------

    /// This node's id.
    pub fn id(&self) -> NodeId {
        self.id
    }
    /// Current role.
    pub fn role(&self) -> Role {
        self.role
    }
    /// Whether this node currently believes it is the leader.
    pub fn is_leader(&self) -> bool {
        self.role == Role::Leader
    }
    /// Current term.
    pub fn current_term(&self) -> u64 {
        self.current_term
    }
    /// Highest committed index.
    pub fn commit_index(&self) -> u64 {
        self.commit_index
    }
    /// Highest applied index.
    pub fn last_applied(&self) -> u64 {
        self.last_applied
    }
    /// Believed current leader (if any).
    pub fn leader_id(&self) -> Option<NodeId> {
        self.leader_id
    }
    /// Highest log index (snapshot base + in-memory entries).
    pub fn log_len(&self) -> u64 {
        self.last_log_index()
    }
    /// Snapshot base index.
    pub fn base_index(&self) -> u64 {
        self.base_index
    }

    // --- internal index helpers ---------------------------------------------

    fn cluster_size(&self) -> usize {
        self.peers.len() + 1
    }
    fn majority(&self) -> usize {
        self.cluster_size() / 2 + 1
    }
    fn last_log_index(&self) -> u64 {
        self.base_index + self.log.len() as u64
    }
    fn last_log_term(&self) -> u64 {
        self.log.last().map(|e| e.term).unwrap_or(self.base_term)
    }
    fn term_at(&self, index: u64) -> u64 {
        if index == 0 {
            0
        } else if index <= self.base_index {
            self.base_term
        } else {
            let pos = (index - self.base_index - 1) as usize;
            self.log.get(pos).map(|e| e.term).unwrap_or(self.base_term)
        }
    }
    fn op_at(&self, index: u64) -> Option<LogOp> {
        if index <= self.base_index {
            return None;
        }
        let pos = (index - self.base_index - 1) as usize;
        self.log.get(pos).map(|e| e.op)
    }

    // --- timers / RNG --------------------------------------------------------

    fn next_rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// Reset the election timer, choosing a fresh randomized timeout in
    /// `[min, max]` (re-randomized every reset, which is what breaks repeated
    /// split votes).
    fn reset_election_timeout(&mut self) {
        let span = self.cfg_max - self.cfg_min + 1;
        self.election_timeout = self.cfg_min + self.next_rand() % span;
        self.election_elapsed = 0;
    }

    /// On observing a higher term, revert to follower and clear the vote (Raft's
    /// universal "newer term wins" rule — the core of split-brain prevention).
    fn observe_term(&mut self, term: u64) {
        if term > self.current_term {
            self.current_term = term;
            self.voted_for = None;
            self.role = Role::Follower;
            self.leader_id = None;
            self.votes_granted.clear();
        }
    }

    /// Whether `term`, carried by a message from a peer, is one a real peer
    /// could hold: at most [`MAX_TERM_STEP`] ahead of ours. A message with a
    /// larger term is forged and is answered without being adopted.
    fn plausible_term(&self, term: u64) -> bool {
        term <= self.current_term.saturating_add(MAX_TERM_STEP)
    }

    // --- driver input: a logical clock tick ---------------------------------

    /// Advance one logical tick. A follower/candidate that exceeds its election
    /// timeout starts an election; a leader past its heartbeat period
    /// re-replicates (heartbeat). Returns the messages to send.
    pub fn tick(&mut self) -> Vec<Outgoing> {
        match self.role {
            Role::Leader => {
                self.heartbeat_elapsed += 1;
                if self.heartbeat_elapsed >= self.cfg_heartbeat {
                    return self.replicate();
                }
                Vec::new()
            }
            Role::Follower | Role::Candidate => {
                self.election_elapsed += 1;
                if self.election_elapsed >= self.election_timeout {
                    return self.start_election();
                }
                Vec::new()
            }
        }
    }

    fn start_election(&mut self) -> Vec<Outgoing> {
        let Some(next_term) = self.current_term.checked_add(1) else {
            // The term space is used up — only forged terms get here. Never
            // wrap to 0 (every peer would then refuse us forever): stay a
            // follower that can still vote and follow (review cluster-nlqa#2).
            self.role = Role::Follower;
            self.reset_election_timeout();
            return Vec::new();
        };
        self.current_term = next_term;
        self.role = Role::Candidate;
        self.voted_for = Some(self.id);
        self.votes_granted.clear();
        self.votes_granted.insert(self.id);
        self.leader_id = None;
        self.reset_election_timeout();

        if self.votes_granted.len() >= self.majority() {
            // Single-node cluster: immediately leader.
            return self.become_leader();
        }
        let term = self.current_term;
        let id = self.id;
        let last_log_index = self.last_log_index();
        let last_log_term = self.last_log_term();
        self.peers
            .iter()
            .map(|&p| Outgoing {
                to: p,
                msg: RaftMsg::RequestVote {
                    term,
                    candidate_id: id,
                    last_log_index,
                    last_log_term,
                },
            })
            .collect()
    }

    fn become_leader(&mut self) -> Vec<Outgoing> {
        self.role = Role::Leader;
        self.leader_id = Some(self.id);
        // Append a current-term no-op when the log tail is uncommitted, so those
        // earlier-term entries become committable again. `advance_commit` only
        // counts entries of the current term (Raft's leader-completeness rule),
        // so without this a value committed-and-served just before a failover
        // could stay invisible cluster-wide until the next client write. Skipped
        // when everything is already committed, which keeps a freshly-elected
        // leader's log byte-identical to its followers' in the common case.
        if self.last_log_index() > self.commit_index {
            self.log.push(LogEntry::new(self.current_term, LogOp::Noop));
            self.log_changed_from(self.last_log_index());
        }
        let nli = self.last_log_index() + 1;
        self.next_index.clear();
        self.match_index.clear();
        let peers = self.peers.clone();
        for p in peers {
            self.next_index.insert(p, nli);
            self.match_index.insert(p, 0);
        }
        self.heartbeat_elapsed = 0;
        if self.peers.is_empty() {
            // A single node is its own quorum: the no-op above commits at
            // once, and with it the log a restarted durable node recovered
            // (review cluster-nlqa#11) — not only at the next client write.
            self.advance_commit();
        }
        self.replicate()
    }

    // --- server side: handle an incoming request -----------------------------

    /// Handle an inbound Raft request and produce the reply. The sender id is
    /// carried inside the message (`candidate_id` / `leader_id`).
    pub fn handle(&mut self, msg: RaftMsg) -> RaftReply {
        match msg {
            RaftMsg::RequestVote {
                term,
                candidate_id,
                last_log_index,
                last_log_term,
            } => self.handle_request_vote(term, candidate_id, last_log_index, last_log_term),
            RaftMsg::AppendEntries {
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                leader_commit,
                entries,
            } => self.handle_append_entries(
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                leader_commit,
                entries,
            ),
            RaftMsg::InstallSnapshot {
                term,
                leader_id,
                last_included_index,
                last_included_term,
                triples,
            } => self.handle_install_snapshot(
                term,
                leader_id,
                last_included_index,
                last_included_term,
                triples,
            ),
        }
    }

    fn handle_request_vote(
        &mut self,
        term: u64,
        candidate_id: NodeId,
        last_log_index: u64,
        last_log_term: u64,
    ) -> RaftReply {
        if term < self.current_term || !self.plausible_term(term) {
            return RaftReply::Vote {
                term: self.current_term,
                granted: false,
            };
        }
        self.observe_term(term);
        // Candidate's log must be at least as up-to-date as ours.
        let our_lt = self.last_log_term();
        let our_li = self.last_log_index();
        let log_ok =
            last_log_term > our_lt || (last_log_term == our_lt && last_log_index >= our_li);
        let can_vote = self.voted_for.is_none() || self.voted_for == Some(candidate_id);
        let granted = can_vote && log_ok;
        if granted {
            self.voted_for = Some(candidate_id);
            self.role = Role::Follower;
            self.reset_election_timeout();
        }
        RaftReply::Vote {
            term: self.current_term,
            granted,
        }
    }

    fn handle_append_entries(
        &mut self,
        term: u64,
        leader_id: NodeId,
        prev_log_index: u64,
        prev_log_term: u64,
        leader_commit: u64,
        entries: Vec<LogEntry>,
    ) -> RaftReply {
        if term < self.current_term || !self.plausible_term(term) {
            // Stale leader: reject and advertise our (higher) term so it steps
            // down. This is the split-brain guard. A forged far-future term is
            // refused the same way, without adopting it.
            return RaftReply::Append {
                term: self.current_term,
                success: false,
                match_index: 0,
            };
        }
        self.observe_term(term);
        // A valid current-term leader exists: (re)become a follower of it.
        self.role = Role::Follower;
        self.leader_id = Some(leader_id);
        self.reset_election_timeout();

        // Log-consistency check at the anchor.
        if prev_log_index > self.last_log_index() {
            return RaftReply::Append {
                term: self.current_term,
                success: false,
                match_index: self.last_log_index(),
            };
        }
        if prev_log_index > self.base_index && self.term_at(prev_log_index) != prev_log_term {
            return RaftReply::Append {
                term: self.current_term,
                success: false,
                match_index: prev_log_index.saturating_sub(1),
            };
        }

        // Append, overwriting any conflicting suffix.
        for (i, entry) in entries.iter().enumerate() {
            let index = prev_log_index + 1 + i as u64;
            if index <= self.base_index {
                continue; // already covered by snapshot
            }
            if index <= self.last_log_index() {
                if self.term_at(index) != entry.term {
                    let pos = (index - self.base_index - 1) as usize;
                    self.log.truncate(pos);
                    self.log.push(*entry);
                    self.log_changed_from(index);
                }
            } else {
                self.log.push(*entry);
                self.log_changed_from(index);
            }
        }

        let new_match = prev_log_index + entries.len() as u64;
        // Raft's receiver rule: commit up to min(leaderCommit, index of the last
        // entry *this message* verified). Clamping to our own last index instead
        // would also commit any unverified suffix we still hold beyond the
        // message (e.g. stale entries from a deposed leader).
        let verified_commit = leader_commit.min(new_match);
        if verified_commit > self.commit_index {
            self.commit_index = verified_commit;
        }
        RaftReply::Append {
            term: self.current_term,
            success: true,
            match_index: new_match,
        }
    }

    fn handle_install_snapshot(
        &mut self,
        term: u64,
        leader_id: NodeId,
        last_included_index: u64,
        last_included_term: u64,
        triples: Vec<IdTriple>,
    ) -> RaftReply {
        if term < self.current_term
            || !self.plausible_term(term)
            || last_included_index > MAX_LOG_INDEX
        {
            // Stale, or forged (a far-future term, or an index no real log
            // reaches — installing it overflowed the node's next election).
            return RaftReply::Snapshot {
                term: self.current_term,
            };
        }
        self.observe_term(term);
        self.role = Role::Follower;
        self.leader_id = Some(leader_id);
        self.reset_election_timeout();

        // Only install a snapshot that carries the log *past* what we have
        // already committed. A snapshot with `last_included_index <=
        // commit_index` would clear the log and reset `last_applied` backwards to
        // an older materialized state, silently dropping committed entries that
        // `drain_apply` can then no longer replay (the log is gone). Guarding on
        // `commit_index` (>= `base_index`) keeps already-committed data.
        if last_included_index > self.commit_index {
            self.base_index = last_included_index;
            self.base_term = last_included_term;
            self.log.clear();
            self.snapshot_dirty = true;
            self.snapshot = triples.clone();
            self.pending_reset = Some(triples);
            self.commit_index = last_included_index;
            self.last_applied = last_included_index;
        }
        RaftReply::Snapshot {
            term: self.current_term,
        }
    }

    // --- leader side: handle a reply ----------------------------------------

    /// Handle a reply from `from`. May change role (won election / saw a higher
    /// term) and returns any follow-up messages (catch-up, commit propagation).
    pub fn handle_reply(&mut self, from: NodeId, reply: RaftReply) -> Vec<Outgoing> {
        match reply {
            RaftReply::Vote { term, granted } => {
                if term > self.current_term {
                    if self.plausible_term(term) {
                        self.observe_term(term);
                    }
                    return Vec::new();
                }
                if self.role != Role::Candidate || term < self.current_term {
                    return Vec::new();
                }
                if granted {
                    self.votes_granted.insert(from);
                    if self.votes_granted.len() >= self.majority() {
                        return self.become_leader();
                    }
                }
                Vec::new()
            }
            RaftReply::Append {
                term,
                success,
                match_index,
            } => {
                if term > self.current_term {
                    if self.plausible_term(term) {
                        self.observe_term(term);
                    }
                    return Vec::new();
                }
                if self.role != Role::Leader || term < self.current_term {
                    return Vec::new();
                }
                // A follower cannot hold more of our log than we do: a larger
                // claim is a lie that would wrap `next_index` (review
                // cluster-nlqa#2).
                let match_index = match_index.min(self.last_log_index());
                if success {
                    // Within one term a follower's agreed prefix only grows, so
                    // a late reply to an older AppendEntries must not move
                    // `match_index` (and with it the commit count) backwards.
                    let matched = self
                        .match_index
                        .get(&from)
                        .copied()
                        .unwrap_or(0)
                        .max(match_index);
                    self.match_index.insert(from, matched);
                    self.next_index.insert(from, matched + 1);
                    let advanced = self.advance_commit();
                    let next = *self.next_index.get(&from).unwrap_or(&0);
                    if next <= self.last_log_index() {
                        // Peer still behind: push the rest now.
                        self.replicate_to(from).into_iter().collect()
                    } else if advanced {
                        // New commit point: tell everyone (they then apply).
                        self.replicate()
                    } else {
                        Vec::new()
                    }
                } else {
                    // Log mismatch: back up next_index and retry.
                    let cur = *self.next_index.get(&from).unwrap_or(&1);
                    let next = (match_index.saturating_add(1))
                        .min(cur.saturating_sub(1))
                        .max(1);
                    self.next_index.insert(from, next);
                    self.replicate_to(from).into_iter().collect()
                }
            }
            RaftReply::Snapshot { term } => {
                if term > self.current_term {
                    if self.plausible_term(term) {
                        self.observe_term(term);
                    }
                    return Vec::new();
                }
                if self.role != Role::Leader {
                    return Vec::new();
                }
                // Follower is now caught up to our snapshot base; continue with
                // any entries after it.
                self.match_index.insert(from, self.base_index);
                self.next_index.insert(from, self.base_index + 1);
                self.replicate_to(from).into_iter().collect()
            }
        }
    }

    /// Advance `commit_index` to the highest index replicated on a majority,
    /// **restricted to entries from the current term** (Raft's safety rule that
    /// prevents committing — and thus a later split-brain overwriting —
    /// stale-term entries). Returns whether it advanced.
    fn advance_commit(&mut self) -> bool {
        let old = self.commit_index;
        let mut n = self.last_log_index();
        while n > self.commit_index {
            if self.term_at(n) == self.current_term {
                let mut count = 1; // self
                for &p in &self.peers {
                    if *self.match_index.get(&p).unwrap_or(&0) >= n {
                        count += 1;
                    }
                }
                if count >= self.majority() {
                    self.commit_index = n;
                    break;
                }
            }
            n -= 1;
        }
        self.commit_index > old
    }

    /// Append a client mutation to the leader's log; returns its index, or
    /// `None` if this node is not the leader. Single-node clusters commit at
    /// once; otherwise replication happens via [`RaftState::replicate_now`] /
    /// ticks.
    pub fn propose(&mut self, op: LogOp) -> Option<u64> {
        if self.role != Role::Leader {
            return None;
        }
        self.log.push(LogEntry::new(self.current_term, op));
        let idx = self.last_log_index();
        self.log_changed_from(idx);
        if self.peers.is_empty() {
            self.advance_commit();
        }
        Some(idx)
    }

    /// Leader-only: emit AppendEntries to every peer right now (used to push a
    /// freshly proposed entry without waiting for the next heartbeat tick).
    pub fn replicate_now(&mut self) -> Vec<Outgoing> {
        if self.role == Role::Leader {
            self.replicate()
        } else {
            Vec::new()
        }
    }

    fn replicate(&mut self) -> Vec<Outgoing> {
        let peers = self.peers.clone();
        let mut out = Vec::new();
        for p in peers {
            if let Some(o) = self.replicate_to(p) {
                out.push(o);
            }
        }
        self.heartbeat_elapsed = 0;
        out
    }

    fn replicate_to(&self, peer: NodeId) -> Option<Outgoing> {
        if self.role != Role::Leader {
            return None;
        }
        let next = *self
            .next_index
            .get(&peer)
            .unwrap_or(&(self.last_log_index() + 1));
        if next <= self.base_index {
            // Peer is behind our compaction point: send a snapshot instead.
            return Some(Outgoing {
                to: peer,
                msg: RaftMsg::InstallSnapshot {
                    term: self.current_term,
                    leader_id: self.id,
                    last_included_index: self.base_index,
                    last_included_term: self.base_term,
                    triples: self.snapshot.clone(),
                },
            });
        }
        let prev_log_index = next - 1;
        let prev_log_term = self.term_at(prev_log_index);
        let start = (next - self.base_index - 1) as usize;
        let entries = self.log[start.min(self.log.len())..].to_vec();
        Some(Outgoing {
            to: peer,
            msg: RaftMsg::AppendEntries {
                term: self.current_term,
                leader_id: self.id,
                prev_log_index,
                prev_log_term,
                leader_commit: self.commit_index,
                entries,
            },
        })
    }

    /// Drain newly committed-but-unapplied mutations (and any pending snapshot
    /// reset) so the caller can apply them to its [`TripleStore`]. Advances
    /// `last_applied`.
    pub fn drain_apply(&mut self) -> Vec<StoreAction> {
        let mut actions = Vec::new();
        if let Some(triples) = self.pending_reset.take() {
            actions.push(StoreAction::Reset(triples));
        }
        while self.last_applied < self.commit_index {
            self.last_applied += 1;
            if let Some(op) = self.op_at(self.last_applied) {
                actions.push(StoreAction::Apply(op));
            }
        }
        actions
    }

    /// Leader-only: fold committed entries up to `up_to_index` into a snapshot
    /// (`triples` = the leader's full materialized store at that point),
    /// compacting the in-memory log. No-op unless `base_index < up_to_index <=
    /// commit_index`.
    pub fn compact(&mut self, up_to_index: u64, triples: Vec<IdTriple>) {
        if up_to_index <= self.base_index || up_to_index > self.commit_index {
            return;
        }
        let new_base_term = self.term_at(up_to_index);
        let drop = ((up_to_index - self.base_index) as usize).min(self.log.len());
        self.log.drain(0..drop);
        self.base_index = up_to_index;
        self.base_term = new_base_term;
        self.snapshot = triples;
        self.snapshot_dirty = true;
    }

    /// The fate of the entry this node appended at `index` during `term`:
    /// `Some(true)` once it is committed, `Some(false)` once a *different* entry
    /// is committed at that index (it was overwritten after a leadership
    /// change), `None` while undecided. An `(index, term)` pair names a unique
    /// entry (log matching), so comparing terms is exact; an index already folded
    /// into a snapshot (other than the base itself) is reported undecided.
    fn entry_outcome(&self, index: u64, term: u64) -> Option<bool> {
        if index == 0 || self.commit_index < index || index < self.base_index {
            return None;
        }
        Some(self.term_at(index) == term)
    }

    /// Handle revival after a crash: step down from any (now stale) leadership
    /// and reset the election timer.
    ///
    /// A node that was down cannot still legitimately lead — a newer leader may
    /// have been elected in a higher term while it was gone. Keeping `Role::Leader`
    /// would let a revived old leader be picked by [`ReplicaGroup::leader`] once the
    /// real leader is down, serving stale reads and accepting writes that can never
    /// commit (a lost write). It rejoins as a follower and re-learns the current
    /// term/leader from the next `AppendEntries`.
    fn note_revived(&mut self) {
        if self.role == Role::Leader {
            self.role = Role::Follower;
        }
        self.reset_election_timeout();
    }
}

/// An error from submitting a write to a node that is not the leader; carries the
/// believed leader id for a redirect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposeError {
    /// This node is not the leader; the believed leader (if known) is enclosed.
    NotLeader(Option<NodeId>),
    /// The leader appended the write but could not confirm it **committed** in
    /// time (no quorum reachable, or leadership changed underneath it). The
    /// outcome is unknown — it may still commit under a later leader — so the
    /// client should retry (every [`LogOp`] is idempotent when re-applied). The
    /// currently believed leader (if known) is enclosed.
    NotCommitted(Option<NodeId>),
}

impl std::fmt::Display for ProposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposeError::NotLeader(Some(l)) => write!(f, "not leader; try node {l}"),
            ProposeError::NotLeader(None) => write!(f, "not leader; leader unknown"),
            ProposeError::NotCommitted(Some(l)) => write!(
                f,
                "write not confirmed committed (outcome unknown, retry); leader is node {l}"
            ),
            ProposeError::NotCommitted(None) => write!(
                f,
                "write not confirmed committed (outcome unknown, retry); leader unknown"
            ),
        }
    }
}

impl std::error::Error for ProposeError {}

/// The durable Raft state a [`RaftJournal`] recovered: what
/// [`RaftState::restore`] reloads.
#[derive(Debug, Default)]
struct RecoveredState {
    term: u64,
    voted_for: Option<NodeId>,
    base_index: u64,
    base_term: u64,
    snapshot: Vec<IdTriple>,
    log: Vec<LogEntry>,
}

/// The durable Raft state of one persistent [`ClusterNode`] (review
/// cluster-nlqa#11): an append-only journal, `raft.journal` in the node's state
/// directory, of the changes [`RaftState::take_durable_delta`] hands out —
/// term and vote, log suffixes, snapshots. Every batch is written and
/// `fsync`ed before the node answers or sends anything that depends on it, so
/// a restarted replica remembers the vote it gave in its term and the entries
/// it acknowledged; without that it could vote a second time in one term (two
/// leaders of one term) or help elect a leader that lacks committed entries.
///
/// Layout: an 8-byte magic and the owning node id, then records of `u32` BE
/// payload length, `u32` BE CRC-32 of the payload, payload. Payload tag 1 is
/// the hard state (`u64` term, `u8` has-vote, `u64` vote), 2 a snapshot
/// (`u64` base index, `u64` base term, triples), 3 a log suffix (`u64` first
/// index, entries; it replaces everything from that index on). Entries and
/// triples use the [`crate::rpc`] encodings. A torn record at the tail (a
/// crash mid-append, never acknowledged) is cut off on open; an intact record
/// that does not decode (a newer format, a damaged file) makes the open fail
/// instead, and so does a damaged record that a crash cannot have left
/// (see [`RaftJournal::check_torn_tail`]: its bytes are all there, or
/// well-formed records of a later batch follow it), so acknowledged state is
/// never dropped silently.
#[derive(Debug)]
struct RaftJournal {
    file: std::fs::File,
}

impl RaftJournal {
    /// The journal's file name inside the state directory.
    const FILE: &'static str = "raft.journal";
    const MAGIC: &'static [u8; 8] = b"GSRAFTJ1";
    const HEADER_LEN: usize = 16;

    /// Open (creating it if needed) the journal of node `id` in `dir` and
    /// replay it. Refuses a journal written by another node id, and one that
    /// another open journal (another process, or another node in this one)
    /// holds: two copies of one replica writing one journal could again vote
    /// twice in a term. The exclusive advisory lock lasts as long as the
    /// journal is open; a file system without advisory locks opens unlocked.
    fn open(dir: &std::path::Path, id: NodeId) -> io::Result<(RaftJournal, RecoveredState)> {
        use std::io::{Read as _, Write as _};
        // A new state directory's own entry (and any new ancestor's) is
        // synced too: a journal that vanished with its directory after a power
        // loss would restart the node at term 0 with no vote.
        crate::db::fsutil::create_dir_durable(dir).map_err(|e| match e {
            GStoreError::Io(e) => e,
            other => io::Error::other(other.to_string()),
        })?;
        let path = dir.join(Self::FILE);
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!(
                        "{} is in use: another running node holds this state directory",
                        path.display()
                    ),
                ))
            }
            Err(std::fs::TryLockError::Error(e)) if e.kind() == io::ErrorKind::Unsupported => {}
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if bytes.len() < Self::HEADER_LEN {
            // New (or created but never completed): start a fresh journal.
            file.set_len(0)?;
            let mut header = Self::MAGIC.to_vec();
            rpc::put_u64(&mut header, id);
            file.write_all(&header)?;
            file.sync_all()?;
            sync_parent_dir(dir)?;
            return Ok((RaftJournal { file }, RecoveredState::default()));
        }
        let mut r = rpc::Reader::new(&bytes[8..Self::HEADER_LEN]);
        if &bytes[..8] != Self::MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a raft journal", path.display()),
            ));
        }
        let owner = r.u64()?;
        if owner != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} belongs to node {owner}, not node {id}: a state directory serves one node",
                    path.display()
                ),
            ));
        }
        let mut state = RecoveredState::default();
        let mut at = Self::HEADER_LEN;
        while let Some((payload, next)) = Self::record_at(&bytes, at) {
            if let Err(e) = Self::replay(&mut state, payload) {
                // A whole record (its CRC matches) that does not decode was
                // not torn by a crash: it comes from a newer journal format or
                // a damaged file. Cutting it off — with every record after it —
                // would silently forget acknowledged state such as a vote, the
                // very thing the journal exists to keep, so refuse to start.
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{}: the record at byte {at} is intact but cannot be read ({e}); \
                         refusing to start from a raft journal this build does not understand",
                        path.display()
                    ),
                ));
            }
            at = next;
        }
        if at < bytes.len() {
            // Replay stopped at bytes that are not a well-formed record. Only
            // the tail a crash leaves in the last, unacknowledged batch may be
            // cut; damage anywhere else (review cluster-nlqa#11 follow-up: a
            // flipped bit in an older vote record used to cut every later,
            // acknowledged record with it) refuses the start and leaves the
            // file untouched.
            if let Err(why) = Self::check_torn_tail(&bytes, at) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{}: the record at byte {at} is damaged and {why}; refusing to start \
                         rather than drop acknowledged raft state (restore the state directory \
                         from a copy, or replace this replica)",
                        path.display()
                    ),
                ));
            }
            // A torn tail: it was never acknowledged — every batch is synced
            // before the node acts on it — so drop it.
            file.set_len(at as u64)?;
            file.sync_all()?;
        }
        Ok((RaftJournal { file }, state))
    }

    /// The unit a disk writes atomically, at least: a crash leaves each
    /// 512-byte block of the file either written or as it was (zeros, for
    /// bytes an append never got onto the disk).
    const SECTOR: usize = 512;

    /// Check that the bytes from `at` on — where replay stopped at something
    /// that is not a well-formed record — are what a crash during the last
    /// [`append`](Self::append) can leave, and so were never acknowledged.
    /// `Err` says why not.
    ///
    /// `append` writes one batch (at most three records, tags 1, 2, 3 in that
    /// order) with one `write` and one `sync_data`, and the node acts on a
    /// batch only after that sync, so only the last batch can be torn. A
    /// crash tears it by leaving sectors unwritten: the file ends inside the
    /// record, or a sector of it reads back as zeros (see
    /// [`looks_torn`](Self::looks_torn)); sectors written out of order may
    /// leave later records of the same batch intact. Anything else is damage
    /// to acknowledged state: a record whose bytes are all present but whose
    /// checksum does not match (a flipped bit), or well-formed records after
    /// the damage that cannot belong to its batch.
    fn check_torn_tail(bytes: &[u8], at: usize) -> std::result::Result<(), String> {
        if !Self::looks_torn(bytes, at) {
            return Err("no part of it is missing or zero-filled, as a crash while \
                 writing would leave it (a damaged file, not a torn write)"
                .to_string());
        }
        // Everything after it must fit in its batch: walk the well-formed
        // records that follow and the damaged stretches between them, giving
        // each damaged stretch the smallest tag it could have; the tags must
        // rise and stay within 1..=3.
        let mut last_tag = 1u8; // the damaged record itself
        let mut fits = |tag: Option<u8>| {
            let tag = match tag {
                None => last_tag + 1,
                Some(t) if t > last_tag => t,
                Some(_) => return false,
            };
            last_tag = tag;
            tag <= 3
        };
        let mut from = at + 1;
        while let Some(found) = (from..bytes.len()).find(|&p| Self::plausible_record_at(bytes, p)) {
            let later = || {
                format!(
                    "well-formed records follow it (from byte {found} on) that a later batch \
                     wrote, after this one had been synced and acted on"
                )
            };
            let mut next = found;
            while let Some((payload, end)) = Self::record_at(bytes, next) {
                if !fits(Some(payload[0])) {
                    return Err(later());
                }
                next = end;
            }
            if next == bytes.len() {
                break;
            }
            // Damaged bytes again: at least one more record of the batch.
            if !fits(None) {
                return Err(later());
            }
            from = next + 1;
        }
        Ok(())
    }

    /// Whether the record at `at` looks torn by a crash: the file ends inside
    /// its header or its declared payload, or one of its sector-aligned
    /// stretches is all zeros (a zero-length header counts as the header
    /// alone).
    fn looks_torn(bytes: &[u8], at: usize) -> bool {
        let Some(head) = bytes.get(at..at.saturating_add(8)) else {
            return true;
        };
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let end = (at + 8).saturating_add(len);
        if end > bytes.len() {
            return true;
        }
        let mut start = at;
        while start < end {
            let stop = ((start / Self::SECTOR + 1) * Self::SECTOR).min(end);
            if bytes[start..stop].iter().all(|&b| b == 0) {
                return true;
            }
            start = stop;
        }
        false
    }

    /// Whether a well-formed record of a known kind starts at `at`, with a
    /// payload length its kind can have — checked before the checksum, so
    /// scanning a damaged journal for later records stays cheap.
    fn plausible_record_at(bytes: &[u8], at: usize) -> bool {
        let Some(head) = bytes.get(at..at.saturating_add(9)) else {
            return false;
        };
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if (at + 8).saturating_add(len) > bytes.len() {
            return false;
        }
        let count = |off: usize| {
            bytes
                .get(at + 8 + off..at + 12 + off)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
        };
        let plausible = match head[8] {
            // term, has-vote, vote
            1 => len == 18,
            // base index, base term, then a count of 12-byte triples
            2 => len >= 21 && count(17).is_some_and(|n| n.checked_mul(12) == Some(len - 21)),
            // first index, then a count of entries of at least 9 bytes each
            3 => len >= 13 && count(9).is_some_and(|n| n <= (len - 13) / 9),
            _ => false,
        };
        plausible && Self::record_at(bytes, at).is_some()
    }

    /// The well-formed record starting at `at`: its payload and the offset of
    /// the next record. `None` at the end, or at a torn or corrupt record —
    /// including a zero-length one, which [`RaftJournal::append`] never
    /// writes but a crash can leave behind (a file grown, its new bytes still
    /// zero), and whose CRC would otherwise match.
    fn record_at(bytes: &[u8], at: usize) -> Option<(&[u8], usize)> {
        let head = bytes.get(at..at.checked_add(8)?)?;
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if len == 0 {
            return None;
        }
        let crc = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
        let end = (at + 8).checked_add(len)?;
        let payload = bytes.get(at + 8..end)?;
        (crc32(payload) == crc).then_some((payload, end))
    }

    /// Apply one record to the recovered state.
    fn replay(state: &mut RecoveredState, payload: &[u8]) -> io::Result<()> {
        let mut r = rpc::Reader::new(payload);
        match r.u8()? {
            1 => {
                state.term = r.u64()?;
                let has_vote = r.u8()? != 0;
                let vote = r.u64()?;
                state.voted_for = has_vote.then_some(vote);
            }
            2 => {
                state.base_index = r.u64()?;
                state.base_term = r.u64()?;
                state.snapshot = r.triples()?;
                state.log.clear();
            }
            3 => {
                let from = r.u64()?;
                let entries = r.entries()?;
                let keep = from.saturating_sub(state.base_index + 1);
                if keep > state.log.len() as u64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "raft journal: a log suffix starts past the end of the log",
                    ));
                }
                state.log.truncate(keep as usize);
                for (i, e) in entries.into_iter().enumerate() {
                    if from + i as u64 > state.base_index {
                        state.log.push(e);
                    }
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "raft journal: unknown record",
                ))
            }
        }
        if r.remaining() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "raft journal: trailing bytes in a record",
            ));
        }
        Ok(())
    }

    /// Append `delta` as one batch of records and `fsync` it.
    fn append(&mut self, delta: &DurableDelta) -> io::Result<()> {
        use std::io::Write as _;
        let batch = Self::batch(delta);
        if batch.is_empty() {
            return Ok(());
        }
        self.file.write_all(&batch)?;
        self.file.sync_data()
    }

    /// The records of one batch: `delta`'s hard state, snapshot and log
    /// suffix, in that order (tags 1, 2, 3), each at most once.
    fn batch(delta: &DurableDelta) -> Vec<u8> {
        let mut batch = Vec::new();
        let mut record = |payload: Vec<u8>| {
            batch.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            batch.extend_from_slice(&crc32(&payload).to_be_bytes());
            batch.extend_from_slice(&payload);
        };
        if let Some((term, vote)) = delta.hard {
            let mut p = vec![1u8];
            rpc::put_u64(&mut p, term);
            p.push(u8::from(vote.is_some()));
            rpc::put_u64(&mut p, vote.unwrap_or(0));
            record(p);
        }
        if let Some((base_index, base_term, triples)) = &delta.snapshot {
            let mut p = vec![2u8];
            rpc::put_u64(&mut p, *base_index);
            rpc::put_u64(&mut p, *base_term);
            rpc::put_triples(&mut p, triples);
            record(p);
        }
        if let Some((from, entries)) = &delta.entries {
            let mut p = vec![3u8];
            rpc::put_u64(&mut p, *from);
            rpc::put_entries(&mut p, entries);
            record(p);
        }
        batch
    }
}

/// `fsync` directory `dir`, so a file just created in it survives a crash. A
/// no-op off unix, where a directory cannot be opened for syncing.
fn sync_parent_dir(dir: &std::path::Path) -> io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// The CRC-32 (IEEE, reflected) of `data` — the raft journal's record check.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// The outbound replication channel to one peer: a single-slot mailbox holding
/// the **newest** Raft message for that peer, drained by that peer's dedicated
/// sender thread.
///
/// One slot (overwrite, not a queue) is enough because every message the state
/// machine emits for a peer is built from its *current* view of that peer
/// (`next_index`, commit index, term), so a newer message supersedes an older,
/// unsent one; and it keeps a slow peer from accumulating an unbounded backlog.
#[derive(Debug, Default)]
struct PeerLink {
    pending: Mutex<Option<RaftMsg>>,
    ready: std::sync::Condvar,
}

/// A cluster node: a [`RaftState`] plus a replicated [`TripleStore`], wrapped in
/// the same `std`-only length-prefixed RPC transport ([`crate::rpc`]) as the
/// sharding layer. It runs a TCP server (answering Raft RPCs, client writes,
/// status, and read-only shard queries against the replicated store), a
/// background **driver** that ticks the state machine, and one **sender**
/// thread per peer that delivers the messages addressed to it. A kill/revive
/// switch simulates node failure for tests.
///
/// Delivery is per peer and asynchronous: a slow, hung or unreachable peer only
/// delays its own sender, never heartbeats or commits to the rest of the
/// cluster — so a quorum keeps a stable leader while a minority is unresponsive.
#[derive(Debug)]
pub struct ClusterNode {
    id: NodeId,
    state: Arc<Mutex<RaftState>>,
    store: Arc<Mutex<TripleStore>>,
    /// Serializes the whole drain-and-apply critical section. `apply_actions`
    /// drains committed entries under the state lock, releases it, then writes
    /// them to the store — two threads (e.g. a connection handler and the tick
    /// driver) could otherwise interleave and apply out of commit order.
    apply_lock: Mutex<()>,
    /// The cluster's replicated membership/config, updated as committed
    /// config-change entries ([`LogOp::AddNode`]/[`LogOp::RemoveNode`]) apply.
    config: Arc<Mutex<ClusterConfig>>,
    /// id → address of every peer (filled in after binding, since ephemeral
    /// ports are only known post-bind).
    peer_addrs: Mutex<HashMap<NodeId, SocketAddr>>,
    /// One outbound mailbox per peer (fixed at bind time), each drained by its
    /// own sender thread that also owns the connection to that peer.
    links: HashMap<NodeId, Arc<PeerLink>>,
    listener: TcpListener,
    alive: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    tick_interval: Duration,
    io_timeout: Duration,
    /// How long a [`Request::ClientWrite`] waits for its entry to commit before
    /// answering "not confirmed".
    commit_timeout: Mutex<Duration>,
    /// The durable Raft state of a node bound with
    /// [`ClusterNode::bind_persistent`]; `None` keeps it in memory only.
    journal: Option<Mutex<RaftJournal>>,
    /// Set once the journal could not be written: the node has stopped for
    /// good (fail-stop) and [`ClusterNode::revive`] does not restart it.
    durable_failed: AtomicBool,
}

impl ClusterNode {
    /// Default shard-bucket count for the node's replicated [`ClusterConfig`].
    /// The membership/assignment is what is agreed through the log; the data
    /// itself is replicated whole-keyspace by this single group (per-shard groups
    /// are the documented generalization), so this value only sizes the agreed
    /// shard→owner map that a [`ShardCluster`] data plane would consume.
    pub const DEFAULT_SHARDS: usize = 16;

    /// Default bound on how long a [`Request::ClientWrite`] waits for its entry
    /// to commit (see [`ClusterNode::set_commit_timeout`]). Clients should use a
    /// longer read timeout than this.
    pub const DEFAULT_COMMIT_TIMEOUT: Duration = Duration::from_secs(2);

    /// Bind node `id` with cluster peers `peer_ids` (their addresses are set
    /// later via [`ClusterNode::set_peer_addr`]) and a starting `store`.
    ///
    /// The node keeps its Raft term, vote and log **in memory only**: it must
    /// never be restarted under the same id, or it could vote a second time in
    /// a term it already voted in (two leaders of one term) and help elect a
    /// leader that lacks committed entries. Use
    /// [`ClusterNode::bind_persistent`] for a replica that may restart.
    pub fn bind<A: ToSocketAddrs>(
        id: NodeId,
        peer_ids: Vec<NodeId>,
        store: TripleStore,
        addr: A,
        cfg: RaftConfig,
    ) -> io::Result<Arc<ClusterNode>> {
        let state = RaftState::new(id, peer_ids.clone(), &cfg);
        Self::bind_with(id, &peer_ids, state, store, addr, cfg, None)
    }

    /// Bind a node whose Raft term, vote and log are **durable** in
    /// `state_dir` (created if missing; one directory per node id): every
    /// change is appended to the directory's journal and `fsync`ed before the
    /// node answers a vote or append, sends a message, or counts its own entry
    /// toward a commit (review cluster-nlqa#11). On a restart from the same
    /// directory the node resumes with the term and vote it had — so it cannot
    /// vote twice in one term — and with its log; the triple store starts empty
    /// and is rebuilt as a leader re-confirms the log's entries.
    ///
    /// A state directory written by a different node id is refused. If the
    /// journal cannot be written the node stops for good (fail-stop): it never
    /// answers from state that a restart would forget.
    pub fn bind_persistent<A: ToSocketAddrs>(
        id: NodeId,
        peer_ids: Vec<NodeId>,
        state_dir: &std::path::Path,
        addr: A,
        cfg: RaftConfig,
    ) -> io::Result<Arc<ClusterNode>> {
        let (journal, recovered) = RaftJournal::open(state_dir, id)?;
        let mut state = RaftState::new(id, peer_ids.clone(), &cfg);
        state.restore(recovered);
        Self::bind_with(
            id,
            &peer_ids,
            state,
            TripleStore::new(),
            addr,
            cfg,
            Some(journal),
        )
    }

    fn bind_with<A: ToSocketAddrs>(
        id: NodeId,
        peer_ids: &[NodeId],
        state: RaftState,
        store: TripleStore,
        addr: A,
        cfg: RaftConfig,
        journal: Option<RaftJournal>,
    ) -> io::Result<Arc<ClusterNode>> {
        let listener = TcpListener::bind(addr)?;
        let tick_interval = cfg.tick_interval;
        let links = peer_ids
            .iter()
            .filter(|&&p| p != id)
            .map(|&p| (p, Arc::new(PeerLink::default())))
            .collect();
        Ok(Arc::new(ClusterNode {
            id,
            state: Arc::new(Mutex::new(state)),
            store: Arc::new(Mutex::new(store)),
            apply_lock: Mutex::new(()),
            config: Arc::new(Mutex::new(ClusterConfig::new(Self::DEFAULT_SHARDS))),
            peer_addrs: Mutex::new(HashMap::new()),
            links,
            listener,
            alive: Arc::new(AtomicBool::new(true)),
            shutdown: Arc::new(AtomicBool::new(false)),
            tick_interval,
            io_timeout: Duration::from_millis(300),
            commit_timeout: Mutex::new(Self::DEFAULT_COMMIT_TIMEOUT),
            journal: journal.map(Mutex::new),
            durable_failed: AtomicBool::new(false),
        }))
    }

    /// Make the Raft state machine's latest step durable before anything it
    /// produced — a reply, an outgoing message, a commit counting this node —
    /// can be seen; the caller holds the state lock, so journal order is state
    /// order. A no-op for an in-memory node. If the journal cannot be written
    /// the node stops for good and the error is returned — then, and on every
    /// later call: the failed batch is gone from `st`'s durability bookkeeping
    /// (its delta was handed out), so a later step that changes nothing new
    /// would otherwise look durable and could, say, grant again a vote that
    /// never reached the disk.
    fn persist(&self, st: &mut RaftState) -> io::Result<()> {
        let Some(journal) = &self.journal else {
            return Ok(());
        };
        if self.durable_failed.load(Ordering::SeqCst) {
            return Err(io::Error::other(
                "the raft journal could not be written earlier; this node has stopped",
            ));
        }
        let delta = st.take_durable_delta();
        if delta.is_empty() {
            return Ok(());
        }
        let written = journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append(&delta);
        if let Err(e) = &written {
            self.durable_failed.store(true, Ordering::SeqCst);
            self.alive.store(false, Ordering::SeqCst);
            crate::gs_error!(
                "cluster node {}: the raft journal could not be written ({e}); the node stops",
                self.id
            );
        }
        written
    }

    /// Feed an inbound Raft RPC to the state machine and answer it — only
    /// once the state it answers from is durable.
    fn handle_raft(&self, msg: RaftMsg) -> Response {
        let reply = {
            let mut st = self.state.lock().unwrap();
            let reply = st.handle(msg);
            self.persist(&mut st).map(|()| reply)
        };
        match reply {
            Ok(reply) => {
                self.apply_actions();
                reply_to_response(reply)
            }
            Err(e) => Response::Error(format!("raft state could not be made durable: {e}")),
        }
    }

    /// Bound how long a [`Request::ClientWrite`] served by this node waits for
    /// its entry to commit before replying `ok: false` (outcome unknown).
    pub fn set_commit_timeout(&self, timeout: Duration) {
        *self
            .commit_timeout
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = timeout;
    }

    /// The current [`Request::ClientWrite`] commit-wait bound.
    pub fn commit_timeout(&self) -> Duration {
        *self
            .commit_timeout
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Register a peer's resolved address (call once per peer before
    /// [`ClusterNode::start`]).
    pub fn set_peer_addr(&self, id: NodeId, addr: SocketAddr) {
        self.peer_addrs.lock().unwrap().insert(id, addr);
    }

    /// This node's id.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// The bound listen address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Whether this node currently believes it is the leader.
    pub fn is_leader(&self) -> bool {
        self.state.lock().unwrap().is_leader()
    }
    /// Current role.
    pub fn role(&self) -> Role {
        self.state.lock().unwrap().role()
    }
    /// Current term.
    pub fn current_term(&self) -> u64 {
        self.state.lock().unwrap().current_term()
    }
    /// Highest committed index.
    pub fn commit_index(&self) -> u64 {
        self.state.lock().unwrap().commit_index()
    }
    /// Believed current leader.
    pub fn leader_id(&self) -> Option<NodeId> {
        self.state.lock().unwrap().leader_id()
    }
    /// Number of triples in this replica's store.
    pub fn store_triple_count(&self) -> u64 {
        self.store.lock().unwrap().triple_count()
    }
    /// Whether this replica's store contains `t`.
    pub fn store_contains(&self, t: IdTriple) -> bool {
        self.store.lock().unwrap().exists(t.sub, t.pred, t.obj)
    }
    /// A snapshot of this node's committed cluster configuration (membership +
    /// shard→owner assignment). Equal across nodes that have applied the same
    /// committed config-change prefix.
    pub fn config(&self) -> ClusterConfig {
        self.config.lock().unwrap().clone()
    }

    /// Submit a mutation to this node. Succeeds (with the assigned log index)
    /// only on the leader; otherwise returns [`ProposeError::NotLeader`] with a
    /// redirect hint. On success the entry is handed to every peer's sender at
    /// once (replication proceeds asynchronously; this call never blocks on the
    /// network).
    ///
    /// `Ok` means **appended to the leader's log**, not committed: if the leader
    /// loses its quorum or its leadership before a majority stores the entry,
    /// a later leader may discard it. Use [`ClusterNode::propose_and_wait`] when
    /// the caller needs to know the write is durable cluster-wide.
    pub fn propose(&self, op: LogOp) -> std::result::Result<u64, ProposeError> {
        self.submit(op).map(|(idx, _term)| idx)
    }

    /// Submit a mutation and wait up to `timeout` until it is **committed** by a
    /// quorum and applied to this node's store; returns its log index.
    ///
    /// Fails with [`ProposeError::NotLeader`] if this node is not the leader, or
    /// [`ProposeError::NotCommitted`] if commitment is not confirmed in time — the
    /// leader could not reach a quorum, or a different entry was committed at
    /// that index after a leadership change. Retrying is safe: every [`LogOp`]
    /// is idempotent when re-applied (set-semantics triples, membership changes).
    pub fn propose_and_wait(
        &self,
        op: LogOp,
        timeout: Duration,
    ) -> std::result::Result<u64, ProposeError> {
        let (idx, term) = self.submit(op)?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            {
                // Under the apply lock, so "applied" really means "in the store":
                // a concurrent drain that has already advanced `last_applied`
                // finishes writing the store before we can get the lock.
                let _apply = self.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
                self.apply_actions_locked();
                let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                match st.entry_outcome(idx, term) {
                    Some(true) if st.last_applied() >= idx => return Ok(idx),
                    Some(false) => return Err(ProposeError::NotCommitted(st.leader_id())),
                    _ => {}
                }
                if std::time::Instant::now() >= deadline {
                    return Err(ProposeError::NotCommitted(st.leader_id()));
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Append `op` to the leader's log and push it to the followers at once.
    /// Returns the entry's `(index, term)`, which identify it uniquely (Raft's
    /// log-matching property).
    fn submit(&self, op: LogOp) -> std::result::Result<(u64, u64), ProposeError> {
        let (idx, term) = {
            let mut st = self.state.lock().unwrap();
            let Some(i) = st.propose(op) else {
                return Err(ProposeError::NotLeader(st.leader_id()));
            };
            // Durable before it is replicated or counted toward a commit.
            if self.persist(&mut st).is_err() {
                return Err(ProposeError::NotCommitted(None));
            }
            (i, st.current_term())
        };
        let outs = { self.state.lock().unwrap().replicate_now() };
        self.route(outs);
        self.apply_actions();
        Ok((idx, term))
    }

    /// Simulate a crash: stop ticking, sending and serving; each peer sender
    /// drops its connection and any unsent message. In-memory state is retained
    /// (a later [`ClusterNode::revive`] models a restart that reloaded
    /// persistent state).
    pub fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        for link in self.links.values() {
            link.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            link.ready.notify_one();
        }
    }
    /// Bring a killed node back: resume ticking/serving and reset its election
    /// timer.
    pub fn revive(&self) {
        if self.durable_failed.load(Ordering::SeqCst) {
            return; // stopped because its journal failed: stays down
        }
        self.state.lock().unwrap().note_revived();
        self.alive.store(true, Ordering::SeqCst);
    }
    /// Whether the node is currently alive (not killed).
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    /// Permanently stop the node's threads.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Spawn the server, driver and per-peer sender threads. Returns their join
    /// handles (callers may ignore them; threads exit on
    /// [`ClusterNode::shutdown`]).
    pub fn start(self: &Arc<Self>) -> Vec<thread::JoinHandle<()>> {
        let server = Arc::clone(self);
        let mut handles = vec![thread::spawn(move || server.run_server())];
        let driver = Arc::clone(self);
        handles.push(thread::spawn(move || driver.run_driver()));
        for (&peer, link) in &self.links {
            let me = Arc::clone(self);
            let link = Arc::clone(link);
            handles.push(thread::spawn(move || me.run_peer_sender(peer, &link)));
        }
        handles
    }

    fn run_server(self: Arc<Self>) {
        // Non-blocking accept so we can observe shutdown between connections.
        let _ = self.listener.set_nonblocking(true);
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if !self.alive.load(Ordering::SeqCst) {
                        drop(stream); // a dead node refuses service
                        continue;
                    }
                    let me = Arc::clone(&self);
                    thread::spawn(move || me.handle_conn(stream));
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => thread::sleep(Duration::from_millis(2)),
            }
        }
    }

    fn handle_conn(self: Arc<Self>, mut stream: TcpStream) {
        let _ = rpc::tune_stream(&stream, Some(self.io_timeout));
        loop {
            if self.shutdown.load(Ordering::SeqCst) || !self.alive.load(Ordering::SeqCst) {
                return; // killed/stopped: drop the connection (peer sees EOF)
            }
            let bytes = match rpc::read_message(&mut stream) {
                Ok(b) => b,
                Err(_) => return,
            };
            let req = match Request::decode(&bytes) {
                Ok(r) => r,
                Err(e) => {
                    let _ =
                        rpc::write_message(&mut stream, &Response::Error(e.to_string()).encode());
                    continue;
                }
            };
            if !self.alive.load(Ordering::SeqCst) {
                return;
            }
            let resp = self.handle_request(req);
            // An answer too large for one frame is reported, not dropped with
            // the connection (review cluster-nlqa#9).
            if rpc::write_message(&mut stream, &rpc::encode_reply(&resp)).is_err() {
                return;
            }
        }
    }

    fn handle_request(&self, req: Request) -> Response {
        match req {
            Request::RequestVote {
                term,
                candidate_id,
                last_log_index,
                last_log_term,
            } => self.handle_raft(RaftMsg::RequestVote {
                term,
                candidate_id,
                last_log_index,
                last_log_term,
            }),
            Request::AppendEntries {
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                leader_commit,
                entries,
            } => self.handle_raft(RaftMsg::AppendEntries {
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                leader_commit,
                entries,
            }),
            Request::InstallSnapshot {
                term,
                leader_id,
                last_included_index,
                last_included_term,
                triples,
            } => self.handle_raft(RaftMsg::InstallSnapshot {
                term,
                leader_id,
                last_included_index,
                last_included_term,
                triples,
            }),
            Request::ClientWrite { op } => self.client_write_response(op),
            // On a cluster node, a routed insert goes through the replicated
            // log, and answers as [`RemoteShard::insert`] expects: `Bool` (was
            // it new?) once committed, `Error` otherwise. It used to answer a
            // `WriteAck`, so a [`RemoteShard`] reported an error for a write
            // that had committed (review cluster-nlqa#3).
            Request::Insert { sub, pred, obj } => self.routed_insert(IdTriple::new(sub, pred, obj)),
            // Deleting a bucket on this replica alone, outside the log, made
            // the replicas diverge for good (review cluster-nlqa#3). Every
            // mutation of a replicated store goes through `ClientWrite`.
            Request::PurgeShard { .. } => Response::Error(
                "PurgeShard is not served by a replicated cluster node: it would change \
                 this replica outside the Raft log; send ClientWrite Delete operations"
                    .to_string(),
            ),
            Request::ClusterStatus => {
                let st = self.state.lock().unwrap();
                Response::Status {
                    term: st.current_term(),
                    role: st.role().as_u8(),
                    leader_hint: st.leader_id().map(|l| l as i64).unwrap_or(-1),
                    commit_index: st.commit_index(),
                    last_log_index: st.log_len(),
                }
            }
            // The committed cluster configuration (membership + assignment).
            Request::GetConfig => {
                let cfg = self.config.lock().unwrap();
                Response::Config {
                    epoch: cfg.epoch(),
                    num_shards: cfg.num_shards() as u32,
                    members: cfg.member_addrs(),
                    assignment: cfg.assignment().to_vec(),
                }
            }
            // Read-only shard queries are served from the replicated store.
            // (Listed one by one, so a new request kind has to be classified
            // as read-only or log-routed before it compiles.)
            read @ (Request::Exists { .. }
            | Request::PoByS { .. }
            | Request::OBySp { .. }
            | Request::PBySo { .. }
            | Request::PsByO { .. }
            | Request::SByPo { .. }
            | Request::SoByP { .. }
            | Request::SubsByP { .. }
            | Request::ObjsByP { .. }
            | Request::SubjectKeys
            | Request::ObjectKeys
            | Request::Predicates
            | Request::TripleCount
            | Request::PredCard { .. }
            | Request::ScanShard { .. }) => dispatch(&self.store, &read),
        }
    }

    /// Serve a routed [`Request::Insert`] (the [`RemoteShard`] surface) through
    /// the replicated log: `Bool(true)` once a new triple is committed and
    /// applied here, `Bool(false)` if it was already present, and an `Error`
    /// (naming the leader, when known) when this node is not the leader or the
    /// write is not confirmed committed within the commit timeout.
    fn routed_insert(&self, t: IdTriple) -> Response {
        let present = self.store_contains(t);
        match self.propose_and_wait(LogOp::Insert(t), self.commit_timeout()) {
            Ok(_) => Response::Bool(!present),
            Err(e) => Response::Error(e.to_string()),
        }
    }

    /// Serve a client write: acknowledge `ok: true` only once the entry is
    /// **committed** and applied here. (Acknowledging on append alone would tell
    /// the client a write succeeded that a leadership change could still
    /// discard.) A non-leader, or a leader that cannot confirm the commit within
    /// the commit timeout, answers `ok: false` with its best leader hint.
    fn client_write_response(&self, op: LogOp) -> Response {
        match self.propose_and_wait(op, self.commit_timeout()) {
            Ok(index) => Response::WriteAck {
                ok: true,
                leader_hint: self.id as i64,
                index,
            },
            Err(ProposeError::NotLeader(hint) | ProposeError::NotCommitted(hint)) => {
                Response::WriteAck {
                    ok: false,
                    leader_hint: hint.map(|h| h as i64).unwrap_or(-1),
                    index: 0,
                }
            }
        }
    }

    fn run_driver(self: Arc<Self>) {
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(self.tick_interval);
            if !self.alive.load(Ordering::SeqCst) {
                continue;
            }
            let outs = {
                let mut st = self.state.lock().unwrap();
                let outs = st.tick();
                // A new term and our own vote are durable before we campaign.
                match self.persist(&mut st) {
                    Ok(()) => outs,
                    Err(_) => continue,
                }
            };
            self.route(outs);
            self.apply_actions();
        }
    }

    /// Hand each outgoing message to its peer's mailbox (replacing any older,
    /// still-unsent message for that peer) and wake the peer's sender. Never
    /// blocks on the network, so the tick driver and client writes are never held
    /// up by a slow peer.
    fn route(&self, outs: Vec<Outgoing>) {
        for o in outs {
            if let Some(link) = self.links.get(&o.to) {
                *link.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(o.msg);
                link.ready.notify_one();
            }
        }
    }

    /// The sender thread for one peer: deliver the newest message in its mailbox
    /// over this thread's own connection, feed the reply back into the state
    /// machine, and route whatever that produces (catch-up for this peer, a
    /// commit broadcast, a fresh leader's heartbeats). Blocking I/O here only
    /// ever delays traffic to *this* peer.
    fn run_peer_sender(self: Arc<Self>, peer: NodeId, link: &PeerLink) {
        let mut conn: Option<TcpStream> = None;
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let msg = {
                let mut slot = link.pending.lock().unwrap_or_else(|e| e.into_inner());
                if slot.is_none() {
                    slot = link
                        .ready
                        .wait_timeout(slot, Duration::from_millis(50))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                slot.take()
            };
            if !self.alive.load(Ordering::SeqCst) {
                conn = None; // a crashed node holds no connections and sends nothing
                continue;
            }
            let Some(msg) = msg else { continue };
            let Ok(reply) = self.send_to(&mut conn, peer, &msg) else {
                continue; // unreachable: dropped; the next heartbeat retries
            };
            if !self.alive.load(Ordering::SeqCst) {
                continue; // crashed while waiting for the reply
            }
            let more = {
                let mut st = self.state.lock().unwrap();
                let more = st.handle_reply(peer, reply);
                match self.persist(&mut st) {
                    Ok(()) => more,
                    Err(_) => continue,
                }
            };
            self.apply_actions();
            self.route(more);
        }
    }

    /// One request/reply exchange with `peer` over the sender's connection
    /// `conn`, (re)connecting as needed and retrying once on a fresh connection
    /// if the cached one turns out to be broken (e.g. closed by the peer while
    /// idle).
    fn send_to(
        &self,
        conn: &mut Option<TcpStream>,
        peer: NodeId,
        msg: &RaftMsg,
    ) -> io::Result<RaftReply> {
        let addr = {
            let addrs = self.peer_addrs.lock().unwrap();
            *addrs
                .get(&peer)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown peer address"))?
        };
        let req = msg.to_request();
        for attempt in 0..2 {
            if conn.is_none() {
                let stream = TcpStream::connect_timeout(&addr, self.io_timeout)?;
                rpc::tune_stream(&stream, Some(self.io_timeout))?;
                *conn = Some(stream);
            }
            let stream = conn.as_mut().expect("connection was just established");
            match rpc::round_trip(stream, &req) {
                Ok(resp) => return response_to_reply(resp),
                Err(e) => {
                    *conn = None;
                    if attempt == 1 {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!("the loop returns on the second attempt")
    }

    fn apply_actions(&self) {
        // Hold `apply_lock` across the entire drain+apply so a concurrent caller
        // blocks here rather than draining a later prefix and racing us to the
        // store — which would apply committed entries out of order.
        let _apply = self.apply_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.apply_actions_locked();
    }

    /// Drain and apply committed entries; the caller must hold `apply_lock`.
    /// Nothing once the node's journal has failed: a single-node leader counts
    /// an entry committed as it appends it, before the journal write that then
    /// failed, and such an entry must never reach the store. (Checked under
    /// the state lock, which the failing write holds while it sets the flag.)
    fn apply_actions_locked(&self) {
        let actions = {
            let mut st = self.state.lock().unwrap();
            if self.durable_failed.load(Ordering::SeqCst) {
                return;
            }
            st.drain_apply()
        };
        if actions.is_empty() {
            return;
        }
        for a in actions {
            match a {
                StoreAction::Apply(LogOp::Insert(t)) => {
                    self.store.lock().unwrap().insert(t);
                }
                StoreAction::Apply(LogOp::Delete(t)) => {
                    self.store.lock().unwrap().remove(t);
                }
                // A committed no-op term-marker carries no state change.
                StoreAction::Apply(LogOp::Noop) => {}
                // A committed membership config-change updates the replicated
                // [`ClusterConfig`] (not the triple store) — so every node that
                // applied this committed entry agrees on the membership.
                StoreAction::Apply(op @ (LogOp::AddNode { .. } | LogOp::RemoveNode { .. })) => {
                    self.config.lock().unwrap().apply(&op);
                }
                StoreAction::Reset(triples) => {
                    let mut store = self.store.lock().unwrap();
                    *store = TripleStore::new();
                    store.bulk_load(triples);
                }
            }
        }
    }
}

// ===========================================================================
// Per-shard replica groups: one Raft group *per shard bucket* (the documented
// generalization of the single whole-keyspace [`ClusterNode`] group).
//
// [`ReplicaGroup`] replicates ONE shard bucket across `n` replicas, each running
// the proven pure [`RaftState`] over its own [`TripleStore`] copy; a bucket then
// tolerates up to floor((n-1)/2) replica failures while a quorum still serves.
// [`ShardedRaftCluster`] stacks one such group per bucket and routes writes by
// hash(subject) to the owning group, so the whole store is both *sharded* (data
// partitioned across buckets) and *replicated* (each bucket highly available).
//
// Like the deterministic Raft test harness, the group driver delivers messages
// synchronously and only advances the logical clock of the replica(s) it is
// asked to `tick`, so every scenario — election, replication, quorum commit,
// snapshot install, failover and rejoin — is fully reproducible with no threads
// or sockets. This is the in-process correctness core; a deployment would wrap
// each replica in a [`ClusterNode`]-style transport exactly as the single-group
// path already does.
// ===========================================================================

/// One shard bucket replicated across `n` replicas: a self-contained Raft group
/// of `n` [`RaftState`] machines, each over its own [`TripleStore`] copy. The
/// group tolerates up to `floor((n-1)/2)` replica failures while a majority
/// remains — the per-shard analogue of a single-group cluster.
///
/// The driver is **deterministic**: messages are delivered synchronously and
/// only the replica(s) you `tick` advance their logical clock, so elections,
/// quorum commit, snapshot install, failover and rejoin are all reproducible
/// with no threads or clocks (the same discipline as the Raft simulation tests).
///
/// Once a replica has applied more than `compact_threshold` entries past its
/// snapshot base it folds them into a fresh snapshot (log compaction); a replica
/// that then falls behind the leader's compaction point is caught up by an
/// [`Request::InstallSnapshot`] rather than an unbounded log replay, and resumes
/// incremental replication from the snapshot base.
#[derive(Debug)]
pub struct ReplicaGroup {
    /// The shard bucket this group owns.
    shard: usize,
    /// The replica ids, in a stable order (parallel to `states`/`stores`/`down`).
    replicas: Vec<NodeId>,
    /// Per-replica Raft state machines.
    states: Vec<RaftState>,
    /// Per-replica materialized triple stores.
    stores: Vec<TripleStore>,
    /// Per-replica failed flag (a killed replica neither serves nor advances).
    down: Vec<bool>,
    /// Applied-entries-past-snapshot-base a replica keeps before it compacts
    /// (0 disables auto-compaction).
    compact_threshold: u64,
}

impl ReplicaGroup {
    /// Build a group replicating bucket `shard` across the replicas named by
    /// `replicas` (each a distinct [`NodeId`]; at least one). `compact_threshold`
    /// bounds each replica's in-memory log — see the type docs.
    pub fn new(
        shard: usize,
        replicas: Vec<NodeId>,
        cfg: &RaftConfig,
        compact_threshold: u64,
    ) -> ReplicaGroup {
        assert!(
            !replicas.is_empty(),
            "a replica group needs at least one replica"
        );
        let states = replicas
            .iter()
            .map(|&id| {
                let peers: Vec<NodeId> = replicas.iter().copied().filter(|&p| p != id).collect();
                RaftState::new(id, peers, cfg)
            })
            .collect();
        let stores = replicas.iter().map(|_| TripleStore::new()).collect();
        let down = vec![false; replicas.len()];
        ReplicaGroup {
            shard,
            replicas,
            states,
            stores,
            down,
            compact_threshold,
        }
    }

    /// The shard bucket this group replicates.
    pub fn shard(&self) -> usize {
        self.shard
    }
    /// The replica ids in this group.
    pub fn replicas(&self) -> &[NodeId] {
        &self.replicas
    }
    /// The replication factor (number of replicas).
    pub fn replication_factor(&self) -> usize {
        self.replicas.len()
    }

    /// The internal index of replica `id` (panics if `id` is not in the group).
    fn pos(&self, id: NodeId) -> usize {
        self.replicas
            .iter()
            .position(|&r| r == id)
            .expect("replica id is not a member of this group")
    }

    /// Whether replica `id` is currently failed (killed).
    pub fn is_down(&self, id: NodeId) -> bool {
        self.down[self.pos(id)]
    }

    /// Fail (`down = true`, a simulated crash) or restore (`down = false`, a
    /// restart) replica `id`. A restored replica resets its election timer so it
    /// does not immediately campaign with a stale elapsed counter — mirroring
    /// [`ClusterNode::revive`].
    pub fn set_down(&mut self, id: NodeId, down: bool) {
        let i = self.pos(id);
        if self.down[i] && !down {
            self.states[i].note_revived();
        }
        self.down[i] = down;
    }

    /// Borrow replica `id`'s Raft state machine (inspection / tests).
    pub fn state(&self, id: NodeId) -> &RaftState {
        &self.states[self.pos(id)]
    }
    /// Borrow replica `id`'s materialized store (inspection / tests).
    pub fn store(&self, id: NodeId) -> &TripleStore {
        &self.stores[self.pos(id)]
    }

    /// The current leader replica — an up replica claiming leadership in the
    /// highest term — if the group has one. (After a failover the killed old
    /// leader may still *believe* it leads until it hears the new leader; it is
    /// excluded here because it is down, and steps down on revival.)
    pub fn leader(&self) -> Option<NodeId> {
        (0..self.replicas.len())
            .filter(|&i| !self.down[i] && self.states[i].is_leader())
            .max_by_key(|&i| self.states[i].current_term())
            .map(|i| self.replicas[i])
    }

    /// A replica whose store may serve reads: the leader if one is up, else any
    /// up replica (a committed-prefix follower read), else `None` (all down).
    pub fn readable_store(&self) -> Option<&TripleStore> {
        let idx = match self.leader() {
            Some(l) => self.pos(l),
            None => (0..self.replicas.len()).find(|&i| !self.down[i])?,
        };
        Some(&self.stores[idx])
    }

    /// Committed triples in this bucket, read from a serving replica. An error
    /// (not a misleading `0`) when every replica is down.
    pub fn triple_count(&self) -> io::Result<u64> {
        self.readable_store()
            .map(|s| s.triple_count())
            .ok_or_else(|| self.unavailable())
    }

    /// The error a read of this bucket fails with while every replica is down.
    fn unavailable(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::NotConnected,
            format!(
                "shard bucket {} is unavailable: all {} replicas are down",
                self.shard,
                self.replicas.len()
            ),
        )
    }

    /// Apply each replica's freshly committed mutations to its store, then run
    /// log compaction on any replica whose in-memory log has grown past the
    /// threshold.
    fn sync_apply(&mut self) {
        for i in 0..self.states.len() {
            let actions = self.states[i].drain_apply();
            for a in actions {
                match a {
                    StoreAction::Apply(LogOp::Insert(tr)) => {
                        self.stores[i].insert(tr);
                    }
                    StoreAction::Apply(LogOp::Delete(tr)) => {
                        self.stores[i].remove(tr);
                    }
                    // A per-shard *data* group never replicates membership ops
                    // (those belong to the cluster-control [`ClusterConfig`]);
                    // a no-op term-marker carries no state change either.
                    StoreAction::Apply(
                        LogOp::AddNode { .. } | LogOp::RemoveNode { .. } | LogOp::Noop,
                    ) => {}
                    StoreAction::Reset(triples) => {
                        self.stores[i] = TripleStore::new();
                        self.stores[i].bulk_load(triples);
                    }
                }
            }
        }
        self.maybe_compact();
    }

    /// Fold each up replica's committed log into a snapshot once it exceeds the
    /// compaction threshold. The snapshot is that replica's own materialized
    /// store at `last_applied` (which equals its committed prefix), so a snapshot
    /// install reproduces the exact state — while the individual entries are
    /// discarded, bounding the in-memory log.
    fn maybe_compact(&mut self) {
        if self.compact_threshold == 0 {
            return;
        }
        for i in 0..self.states.len() {
            if self.down[i] {
                continue;
            }
            let applied = self.states[i].last_applied();
            let logged = applied.saturating_sub(self.states[i].base_index());
            if logged > self.compact_threshold {
                let triples: Vec<IdTriple> = self.stores[i].iter_all().collect();
                self.states[i].compact(applied, triples);
            }
        }
    }

    /// Synchronously deliver `outs` (emitted by `src`), feeding each reply back
    /// into the sender and processing any follow-up messages (catch-up, snapshot
    /// install, commit propagation), then apply committed entries. A down replica
    /// neither answers a request nor processes a reply, modelling a dropped link.
    fn deliver(&mut self, src: NodeId, outs: Vec<Outgoing>) {
        let mut q: VecDeque<(NodeId, Outgoing)> = outs.into_iter().map(|o| (src, o)).collect();
        let mut guard = 0usize;
        while let Some((from, o)) = q.pop_front() {
            guard += 1;
            if guard > 100_000 {
                break;
            }
            let to = o.to;
            let (fi, ti) = (self.pos(from), self.pos(to));
            if self.down[ti] || self.down[fi] {
                continue;
            }
            let reply = self.states[ti].handle(o.msg);
            let more = self.states[fi].handle_reply(to, reply);
            for m in more {
                q.push_back((from, m));
            }
        }
        self.sync_apply();
    }

    /// Advance replica `id`'s logical clock by one tick and deliver whatever it
    /// emits (a leader heartbeats / catches up followers; a follower may start an
    /// election). A down replica does not tick.
    pub fn tick(&mut self, id: NodeId) {
        let i = self.pos(id);
        if self.down[i] {
            return;
        }
        let outs = self.states[i].tick();
        self.deliver(id, outs);
    }

    /// Drive replica `id` to leadership by ticking *only* it (so no other replica
    /// times out — a deterministic election). Panics if it cannot win within a
    /// bounded number of ticks (e.g. no quorum of up replicas).
    pub fn elect(&mut self, id: NodeId) {
        for _ in 0..1000 {
            let i = self.pos(id);
            if !self.down[i] && self.states[i].is_leader() {
                return;
            }
            self.tick(id);
        }
        panic!(
            "replica {id} failed to become leader of shard {}",
            self.shard
        );
    }

    /// Ensure the group has a leader, electing one if none currently leads.
    /// Returns the leader's id.
    ///
    /// Elects the **most up-to-date** up replica (greatest `(last_log_term,
    /// last_log_index)`), not the lowest-id one: Raft grants a vote only to a
    /// candidate whose log is at least as up-to-date as the voter's, so a replica
    /// with a stale log (e.g. one that missed commits while down) can be
    /// permanently unelectable even when a quorum is up. The most up-to-date up
    /// replica dominates every other up replica's log and therefore always wins
    /// the quorum.
    pub fn ensure_leader(&mut self) -> NodeId {
        if let Some(l) = self.leader() {
            return l;
        }
        // Most up-to-date log wins; ties break to the lowest id so an all-empty
        // bootstrap still deterministically elects the lowest-id replica.
        let ci = (0..self.replicas.len())
            .filter(|&i| !self.down[i])
            .max_by_key(|&i| {
                (
                    self.states[i].last_log_term(),
                    self.states[i].last_log_index(),
                    std::cmp::Reverse(self.replicas[i]),
                )
            })
            .expect("no up replica available to elect a leader");
        let cand = self.replicas[ci];
        self.elect(cand);
        cand
    }

    /// Propose a mutation to the current leader and replicate it to the up
    /// replicas immediately. Returns `(leader, index)`, or `None` if the group
    /// has no leader right now. Commit still requires a quorum to acknowledge; a
    /// write that cannot reach quorum is appended but never applied.
    pub fn propose(&mut self, op: LogOp) -> Option<(NodeId, u64)> {
        let leader = self.leader()?;
        let li = self.pos(leader);
        let idx = self.states[li].propose(op)?;
        let outs = self.states[li].replicate_now();
        self.deliver(leader, outs);
        Some((leader, idx))
    }

    /// Propose a mutation and report whether it **committed**: `Ok(index)` once
    /// a quorum stored it (delivery is synchronous, so this is decided before
    /// returning) and the leader applied it; [`ProposeError::NotLeader`] when
    /// the group has no leader; [`ProposeError::NotCommitted`] when the leader
    /// appended it but no quorum acknowledged it. The entry then stays in the
    /// leader's log and may still commit once a quorum is back (or be dropped
    /// by a later leader), so the outcome is unknown and a retry is safe —
    /// every [`LogOp`] is idempotent (review cluster-nlqa#4; the
    /// [`ClusterNode::propose_and_wait`] contract).
    pub fn commit(&mut self, op: LogOp) -> std::result::Result<u64, ProposeError> {
        let leader = self.leader().ok_or(ProposeError::NotLeader(None))?;
        let term = self.state(leader).current_term();
        let (leader, idx) = self.propose(op).ok_or(ProposeError::NotLeader(None))?;
        let st = self.state(leader);
        match st.entry_outcome(idx, term) {
            Some(true) if st.last_applied() >= idx => Ok(idx),
            _ => Err(ProposeError::NotCommitted(self.leader())),
        }
    }

    /// Tick the current leader `rounds` times so pending work — heartbeats,
    /// backfill to a rejoined follower, an [`Request::InstallSnapshot`] to a
    /// replica behind the compaction point — flushes through. A no-op once the
    /// group has no leader.
    pub fn settle(&mut self, rounds: usize) {
        for _ in 0..rounds {
            match self.leader() {
                Some(l) => self.tick(l),
                None => break,
            }
        }
    }
}

/// A fully replicated, sharded triple store: **one Raft replica group per shard
/// bucket**. Each bucket's data lives on its own [`ReplicaGroup`] of `N`
/// replicas, so a bucket survives up to `floor((N-1)/2)` replica failures with a
/// quorum still serving. Writes route by `hash(subject) % num_shards` to the
/// owning bucket's group and commit through *that* group's Raft log; reads
/// scatter-gather across every bucket's serving replica and re-establish the
/// [`TripleSource`] contract — so the cluster is observationally identical to a
/// single [`TripleStore`] over the same triples, exactly as [`ShardedStore`] is.
///
/// Reads go through [`TryTripleSource`]: a bucket with every replica down is an
/// error, never silently missing data, and [`ShardedRaftCluster::query`] runs
/// the generic [`Evaluator`] over a sharded-and-replicated deployment
/// unchanged. The multi-group analogue of the
/// single-group [`ClusterNode`], it is built on the pure [`RaftState`] + the
/// synchronous [`ReplicaGroup`] driver, so it is fully deterministic and
/// in-process (no threads, clocks, or sockets).
#[derive(Debug)]
pub struct ShardedRaftCluster {
    num_shards: usize,
    replication_factor: usize,
    groups: Vec<ReplicaGroup>,
}

impl ShardedRaftCluster {
    /// Default applied-entries-past-base a replica keeps before it compacts its
    /// log into a snapshot. Small so a long-running cluster bounds its in-memory
    /// log; a far-behind replica then catches up by snapshot install.
    pub const DEFAULT_COMPACT_THRESHOLD: u64 = 64;

    /// Build a cluster of `num_shards` buckets, each replicated across
    /// `replication_factor` replicas (each clamped to at least one). Replica `k`
    /// of bucket `g` is given the distinct global id `g * replication_factor + k`.
    pub fn new(
        num_shards: usize,
        replication_factor: usize,
        cfg: RaftConfig,
    ) -> ShardedRaftCluster {
        Self::with_compaction(
            num_shards,
            replication_factor,
            cfg,
            Self::DEFAULT_COMPACT_THRESHOLD,
        )
    }

    /// Like [`ShardedRaftCluster::new`] but with an explicit per-replica log
    /// compaction threshold (0 disables auto-compaction — a lagging replica then
    /// only ever catches up by log backfill).
    pub fn with_compaction(
        num_shards: usize,
        replication_factor: usize,
        cfg: RaftConfig,
        compact_threshold: u64,
    ) -> ShardedRaftCluster {
        let num_shards = num_shards.max(1);
        let rf = replication_factor.max(1);
        let groups = (0..num_shards)
            .map(|g| {
                let replicas: Vec<NodeId> = (0..rf).map(|k| (g * rf + k) as NodeId).collect();
                ReplicaGroup::new(g, replicas, &cfg, compact_threshold)
            })
            .collect();
        ShardedRaftCluster {
            num_shards,
            replication_factor: rf,
            groups,
        }
    }

    /// Number of shard buckets.
    pub fn num_shards(&self) -> usize {
        self.num_shards
    }
    /// Replicas per bucket.
    pub fn replication_factor(&self) -> usize {
        self.replication_factor
    }
    /// Borrow bucket `shard`'s replica group.
    pub fn group(&self, shard: usize) -> &ReplicaGroup {
        &self.groups[shard]
    }
    /// Mutably borrow bucket `shard`'s replica group (drive its clock / inject
    /// failures directly).
    pub fn group_mut(&mut self, shard: usize) -> &mut ReplicaGroup {
        &mut self.groups[shard]
    }
    /// The shard bucket a subject routes to.
    pub fn shard_of_subject(&self, sub: EntityLiteralId) -> usize {
        shard_of(sub, self.num_shards)
    }

    /// Elect a leader in every bucket's group (deterministically, the lowest-id
    /// up replica). Call once before serving writes.
    pub fn bootstrap(&mut self) {
        for g in &mut self.groups {
            g.ensure_leader();
        }
    }

    /// Fail / restore replica `id` of bucket `shard` (a simulated crash /
    /// restart of that shard replica).
    pub fn set_replica_down(&mut self, shard: usize, id: NodeId, down: bool) {
        self.groups[shard].set_down(id, down);
    }

    /// Route a triple to its bucket's group and commit it through that group's
    /// Raft log. Returns `Some(true)` once it is **committed** (newly added),
    /// `Some(false)` if already present, or `None` if it could not be
    /// committed now: the owning group has no leader, or its leader has no
    /// quorum (the entry may stay appended and commit once a quorum returns —
    /// the outcome is unknown, retry; see [`ReplicaGroup::commit`]). Before
    /// review cluster-nlqa#4 an appended-but-uncommitted write was reported
    /// `Some(true)`. The check against the leader's committed store keeps the op
    /// idempotent (a duplicate is not re-logged).
    pub fn insert(&mut self, t: IdTriple) -> Option<bool> {
        let g = shard_of(t.sub, self.num_shards);
        if let Some(store) = self.groups[g].readable_store() {
            if store.exists(t.sub, t.pred, t.obj) {
                return Some(false);
            }
        }
        self.groups[g].commit(LogOp::Insert(t)).ok().map(|_| true)
    }

    /// Routed bulk insert; returns the number of triples newly committed (a
    /// triple that could not be committed — no leader, no quorum — is not
    /// counted; see [`ShardedRaftCluster::insert`]).
    pub fn insert_all(&mut self, triples: impl IntoIterator<Item = IdTriple>) -> u64 {
        let mut added = 0;
        for t in triples {
            if self.insert(t) == Some(true) {
                added += 1;
            }
        }
        added
    }

    /// Route a delete to its bucket's group and commit it. Returns `Some(true)`
    /// once the delete of a present triple is **committed**, `Some(false)` if it
    /// was absent, or `None` if the delete could not be committed now (no
    /// leader, or no quorum — outcome unknown, retry; review cluster-nlqa#4).
    pub fn remove(&mut self, t: IdTriple) -> Option<bool> {
        let g = shard_of(t.sub, self.num_shards);
        // Decide presence only against the leader. With no leader, a probe would
        // read a possibly-stale follower and could report a committed triple as
        // "absent" (Some(false)) — a silently-lost delete the caller never
        // retries. Report "no leader" (None) instead, matching insert().
        self.groups[g].leader()?;
        let present = self.groups[g]
            .readable_store()
            .map(|s| s.exists(t.sub, t.pred, t.obj))
            .unwrap_or(false);
        if !present {
            return Some(false);
        }
        self.groups[g].commit(LogOp::Delete(t)).ok().map(|_| true)
    }

    /// Tick every bucket's leader `rounds` times so any pending catch-up /
    /// snapshot installs to rejoined replicas flush through.
    pub fn settle(&mut self, rounds: usize) {
        for g in &mut self.groups {
            g.settle(rounds);
        }
    }

    /// Run a read query over the replicated, sharded cluster using the generic
    /// [`Evaluator`] — the per-bucket replication and the scatter-gather are both
    /// transparent to the engine. If some bucket has every replica down the
    /// query fails with [`GStoreError::Io`] rather than silently omitting that
    /// bucket's triples.
    pub fn query(&self, dict: &Dictionary, sparql_text: &str) -> Result<QueryResult> {
        query_checked(self, dict, sparql_text)
    }
}

impl TryTripleSource for ShardedRaftCluster {
    /// One serving replica per bucket (its leader, else any up replica); a
    /// bucket with every replica down makes the whole read fail.
    fn scatter(&self) -> io::Result<Scatter<'_>> {
        let mut shards: Vec<&dyn ShardRead> = Vec::with_capacity(self.groups.len());
        for g in &self.groups {
            shards.push(g.readable_store().ok_or_else(|| g.unavailable())?);
        }
        Ok(Scatter::new(shards))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Dictionary;
    use crate::model::Term;
    use crate::query::{QueryResult, ResultSet};

    /// Build a connected graph with enough distinct subjects to spread across
    /// several shards, plus shared objects (a common `Person` class and a hub
    /// node) so object-/predicate-rooted accesses genuinely span shards.
    /// Returns the dictionary, the id-triples, and a few resolved ids.
    fn fixture() -> (Dictionary, Vec<IdTriple>) {
        let mut d = Dictionary::new();
        let knows = d.intern_predicate(&Term::iri("http://ex/knows").dict_key());
        let typ = d.intern_predicate(&Term::iri("http://ex/type").dict_key());
        let name = d.intern_predicate(&Term::iri("http://ex/name").dict_key());

        let person = d.intern_term(&Term::iri("http://ex/Person"));
        let hub = d.intern_term(&Term::iri("http://ex/hub"));

        let names = [
            "alice", "bob", "carol", "dave", "eve", "frank", "grace", "heidi", "ivan", "judy",
        ];
        let people: Vec<EntityLiteralId> = names
            .iter()
            .map(|n| d.intern_term(&Term::iri(format!("http://ex/{n}"))))
            .collect();

        let mut triples = Vec::new();
        for (i, &p) in people.iter().enumerate() {
            // every person is a Person (shared object across many shards)
            triples.push(IdTriple::new(p, typ, person));
            // every person knows the hub (shared object across many shards)
            triples.push(IdTriple::new(p, knows, hub));
            // ring of acquaintances (knows-edges spread over subjects)
            let next = people[(i + 1) % people.len()];
            triples.push(IdTriple::new(p, knows, next));
            // a literal name
            let lit = d.intern_term(&Term::plain_literal(*names.get(i).unwrap()));
            triples.push(IdTriple::new(p, name, lit));
        }
        // give the hub some outgoing edges too, so it is both subject and object
        triples.push(IdTriple::new(hub, typ, person));
        triples.push(IdTriple::new(hub, knows, people[0]));

        (d, triples)
    }

    fn single(triples: &[IdTriple]) -> TripleStore {
        let mut s = TripleStore::new();
        s.bulk_load(triples.iter().copied());
        s
    }

    fn rows_sorted(rs: &ResultSet) -> Vec<Vec<Option<String>>> {
        let mut r = rs.rows.clone();
        r.sort();
        r
    }

    #[test]
    fn shards_spread_triples_across_more_than_one_shard() {
        let (_d, triples) = fixture();
        let sharded = ShardedStore::from_triples(4, triples.iter().copied());
        let nonempty = sharded.shard_sizes().iter().filter(|&&c| c > 0).count();
        assert!(
            nonempty > 1,
            "expected triples spread across >1 shard, got sizes {:?}",
            sharded.shard_sizes()
        );
        // total triple count is conserved across the partition
        assert_eq!(sharded.triple_count(), single(&triples).triple_count());
    }

    #[test]
    fn from_store_repartitions_identically() {
        let (_d, triples) = fixture();
        let base = single(&triples);
        let a = ShardedStore::from_triples(4, triples.iter().copied());
        let b = ShardedStore::from_store(4, &base);
        assert_eq!(a.iter_all(), b.iter_all());
        assert_eq!(a.shard_sizes(), b.shard_sizes());
    }

    /// The heart of the task: every access-pattern method must return results
    /// byte-for-byte identical to a single TripleStore over the same triples,
    /// including ordering and de-duplication.
    #[test]
    fn every_access_pattern_matches_single_store() {
        let (_d, triples) = fixture();
        let base = single(&triples);
        let sharded = ShardedStore::from_triples(4, triples.iter().copied());

        // Global statistics / scans.
        assert_eq!(
            TripleSource::triple_count(&sharded),
            base.triple_count(),
            "triple_count"
        );
        assert_eq!(
            sharded.distinct_subjects(),
            base.distinct_subjects(),
            "distinct_subjects"
        );
        assert_eq!(
            sharded.distinct_objects(),
            base.distinct_objects(),
            "distinct_objects"
        );
        assert_eq!(
            sharded.num_predicates(),
            base.num_predicates(),
            "num_predicates"
        );
        assert_eq!(
            sharded.subject_keys(),
            base.subject_keys().collect::<Vec<_>>(),
            "subject_keys"
        );
        assert_eq!(
            sharded.object_keys(),
            base.object_keys().collect::<Vec<_>>(),
            "object_keys"
        );
        assert_eq!(
            sharded.iter_all(),
            base.iter_all().collect::<Vec<_>>(),
            "iter_all"
        );

        // Subject-rooted accesses over every id that appears anywhere.
        let mut ids: Vec<EntityLiteralId> = base.subject_keys().collect();
        ids.extend(base.object_keys());
        ids = sort_dedup(ids);
        for &s in &ids {
            assert_eq!(
                TripleSource::po_by_s(&sharded, s),
                base.po_by_s(s).to_vec(),
                "po_by_s({s})"
            );
            assert_eq!(
                TripleSource::ps_by_o(&sharded, s),
                base.ps_by_o(s).to_vec(),
                "ps_by_o({s})"
            );
        }

        // Predicate-rooted accesses + per-predicate statistics.
        let preds: Vec<PredId> = base.predicates().collect();
        for &p in &preds {
            assert_eq!(
                TripleSource::so_by_p(&sharded, p),
                base.so_by_p(p).to_vec(),
                "so_by_p({p})"
            );
            assert_eq!(sharded.subs_by_p(p), base.subs_by_p(p), "subs_by_p({p})");
            assert_eq!(sharded.objs_by_p(p), base.objs_by_p(p), "objs_by_p({p})");
            assert_eq!(sharded.pred_card(p), base.pred_card(p), "pred_card({p})");
            assert_eq!(
                sharded.pred_distinct_subj(p),
                base.pred_distinct_subj(p),
                "pred_distinct_subj({p})"
            );
            assert_eq!(
                sharded.pred_distinct_obj(p),
                base.pred_distinct_obj(p),
                "pred_distinct_obj({p})"
            );
            // Two-constant accesses across the full id × id space touched here.
            for &s in &ids {
                assert_eq!(
                    TripleSource::o_by_sp(&sharded, s, p),
                    base.o_by_sp(s, p),
                    "o_by_sp({s},{p})"
                );
                assert_eq!(
                    TripleSource::s_by_po(&sharded, p, s),
                    base.s_by_po(p, s),
                    "s_by_po({p},{s})"
                );
            }
        }

        // s?o and exact existence over a representative id cross-product.
        for &s in &ids {
            for &o in &ids {
                assert_eq!(
                    TripleSource::p_by_so(&sharded, s, o),
                    base.p_by_so(s, o),
                    "p_by_so({s},{o})"
                );
                for &p in &preds {
                    assert_eq!(
                        TripleSource::exists(&sharded, s, p, o),
                        base.exists(s, p, o),
                        "exists({s},{p},{o})"
                    );
                }
            }
        }
    }

    /// A multi-pattern BGP join, evaluated by the *same* generic Evaluator over
    /// the single store and the sharded store, must yield identical rows.
    #[test]
    fn multi_pattern_bgp_select_matches_single_store() {
        let (d, triples) = fixture();
        let base = single(&triples);
        let sharded = ShardedStore::from_triples(4, triples.iter().copied());

        let q = "SELECT ?x ?y WHERE { \
                 ?x <http://ex/knows> ?y . \
                 ?y <http://ex/type> <http://ex/Person> . \
                 ?x <http://ex/name> ?n }";

        let parsed = sparql::parse(q).unwrap();
        let base_rs = match Evaluator::new(&d, &base).evaluate(&parsed).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        let shard_rs = match sharded.query(&d, q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };

        assert_eq!(shard_rs.vars, base_rs.vars);
        assert!(!base_rs.rows.is_empty(), "fixture should produce some rows");
        assert_eq!(rows_sorted(&shard_rs), rows_sorted(&base_rs));
    }

    /// ASK over the sharded store via the generic evaluator.
    #[test]
    fn ask_query_matches_single_store() {
        let (d, triples) = fixture();
        let base = single(&triples);
        let sharded = ShardedStore::from_triples(4, triples.iter().copied());

        for q in [
            "ASK { ?p <http://ex/type> <http://ex/Person> }",
            "ASK { <http://ex/alice> <http://ex/knows> <http://ex/missing> }",
        ] {
            let parsed = sparql::parse(q).unwrap();
            let base_ans = match Evaluator::new(&d, &base).evaluate(&parsed).unwrap() {
                QueryResult::Ask(a) => a,
                other => panic!("expected ASK, got {other:?}"),
            };
            let shard_ans = match sharded.query(&d, q).unwrap() {
                QueryResult::Ask(a) => a,
                other => panic!("expected ASK, got {other:?}"),
            };
            assert_eq!(shard_ans, base_ans, "ASK mismatch for {q}");
        }
    }

    #[test]
    fn empty_and_single_shard_are_well_formed() {
        let empty = ShardedStore::new(0); // clamped to 1
        assert_eq!(empty.num_shards(), 1);
        assert_eq!(TripleSource::triple_count(&empty), 0);
        assert!(empty.iter_all().is_empty());

        let (_d, triples) = fixture();
        let one = ShardedStore::from_triples(1, triples.iter().copied());
        let base = single(&triples);
        assert_eq!(one.iter_all(), base.iter_all().collect::<Vec<_>>());
    }
}

/// Fault-injection fixtures shared by the networked test modules below.
#[cfg(test)]
mod test_support {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A TCP front for a shard node that can be **cut** and **restored** —
    /// modelling a shard host that goes down and comes back while coordinators
    /// keep its address. While cut, new connections are accepted and dropped at
    /// once and established relays are severed at their next frame: exactly what
    /// a client sees when the node's process dies. The front holds its port for
    /// the whole test, so the "down" address can never be reused by another
    /// listener in the meantime.
    pub(super) struct Link {
        addr: SocketAddr,
        up: Arc<AtomicBool>,
        /// Requests the link still forwards before it cuts itself
        /// ([`Link::fail_after`]); `usize::MAX` means unlimited.
        budget: Arc<AtomicUsize>,
    }

    impl Link {
        /// Front the shard node listening at `backend` on a fresh ephemeral port.
        pub(super) fn to(backend: SocketAddr) -> Link {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind link");
            let addr = listener.local_addr().expect("link addr");
            let up = Arc::new(AtomicBool::new(true));
            let budget = Arc::new(AtomicUsize::new(usize::MAX));
            let flag = Arc::clone(&up);
            let left = Arc::clone(&budget);
            thread::spawn(move || {
                for client in listener.incoming().flatten() {
                    if !flag.load(Ordering::SeqCst) {
                        drop(client); // the host is down: refuse service
                        continue;
                    }
                    let flag = Arc::clone(&flag);
                    let left = Arc::clone(&left);
                    thread::spawn(move || relay(client, backend, &flag, &left));
                }
            });
            Link { addr, up, budget }
        }

        /// Forward `n` more requests, then cut the link at the next one (which
        /// is not forwarded) — a deterministic mid-operation failure: the host
        /// dies after exactly `n` RPCs, whatever the timing.
        pub(super) fn fail_after(&self, n: usize) {
            self.budget.store(n, Ordering::SeqCst);
        }

        /// The address coordinators should use for this shard.
        pub(super) fn addr(&self) -> SocketAddr {
            self.addr
        }

        /// Take the shard down.
        pub(super) fn cut(&self) {
            self.up.store(false, Ordering::SeqCst);
        }

        /// Bring the shard back (and lift any [`Link::fail_after`] budget).
        pub(super) fn restore(&self) {
            self.budget.store(usize::MAX, Ordering::SeqCst);
            self.up.store(true, Ordering::SeqCst);
        }
    }

    /// Spend one request of a [`Link::fail_after`] budget; `false` once it is
    /// exhausted (`usize::MAX` is unlimited).
    fn take_one(budget: &AtomicUsize) -> bool {
        let mut cur = budget.load(Ordering::SeqCst);
        loop {
            let next = match cur {
                usize::MAX => return true,
                0 => return false,
                n => n - 1,
            };
            match budget.compare_exchange(cur, next, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return true,
                Err(seen) => cur = seen,
            }
        }
    }

    /// Relay framed request/response pairs between `client` and the node at
    /// `backend` until either side closes or the link is cut.
    fn relay(mut client: TcpStream, backend: SocketAddr, up: &AtomicBool, budget: &AtomicUsize) {
        let Ok(mut server) = TcpStream::connect(backend) else {
            return;
        };
        let _ = client.set_nodelay(true);
        let _ = server.set_nodelay(true);
        while let Ok(req) = rpc::read_message(&mut client) {
            if !up.load(Ordering::SeqCst) {
                return; // cut mid-session: sever both sockets
            }
            if !take_one(budget) {
                up.store(false, Ordering::SeqCst); // budget exhausted: the host dies now
                return;
            }
            if rpc::write_message(&mut server, &req).is_err() {
                return;
            }
            let Ok(resp) = rpc::read_message(&mut server) else {
                return;
            };
            if !up.load(Ordering::SeqCst) || rpc::write_message(&mut client, &resp).is_err() {
                return;
            }
        }
    }
}

/// Integration tests for the **network-distributed** layer: real
/// [`ShardNode`] servers on ephemeral `127.0.0.1:0` ports in background
/// threads, driven over genuine TCP loopback by [`RemoteShard`] clients — the
/// same in-process-server pattern as `tests/dt_service.rs`, kept inside the
/// crate so it stays within the files this module owns.
#[cfg(test)]
mod net_tests {
    use super::*;
    use crate::dict::Dictionary;
    use crate::model::Term;
    use crate::query::{QueryResult, ResultSet};

    /// The same connected fixture graph as the in-process tests above, rebuilt
    /// here (the other module's `fixture` is private to it).
    fn fixture() -> (Dictionary, Vec<IdTriple>) {
        let mut d = Dictionary::new();
        let knows = d.intern_predicate(&Term::iri("http://ex/knows").dict_key());
        let typ = d.intern_predicate(&Term::iri("http://ex/type").dict_key());
        let name = d.intern_predicate(&Term::iri("http://ex/name").dict_key());

        let person = d.intern_term(&Term::iri("http://ex/Person"));
        let hub = d.intern_term(&Term::iri("http://ex/hub"));

        let names = [
            "alice", "bob", "carol", "dave", "eve", "frank", "grace", "heidi", "ivan", "judy",
        ];
        let people: Vec<EntityLiteralId> = names
            .iter()
            .map(|n| d.intern_term(&Term::iri(format!("http://ex/{n}"))))
            .collect();

        let mut triples = Vec::new();
        for (i, &p) in people.iter().enumerate() {
            triples.push(IdTriple::new(p, typ, person));
            triples.push(IdTriple::new(p, knows, hub));
            let next = people[(i + 1) % people.len()];
            triples.push(IdTriple::new(p, knows, next));
            let lit = d.intern_term(&Term::plain_literal(*names.get(i).unwrap()));
            triples.push(IdTriple::new(p, name, lit));
        }
        triples.push(IdTriple::new(hub, typ, person));
        triples.push(IdTriple::new(hub, knows, people[0]));

        (d, triples)
    }

    fn rows_sorted(rs: &ResultSet) -> Vec<Vec<Option<String>>> {
        let mut r = rs.rows.clone();
        r.sort();
        r
    }

    /// Start an in-process [`ShardNode`] on an ephemeral port in a background
    /// thread; returns its bound address.
    fn start_node(initial: Vec<IdTriple>) -> SocketAddr {
        let mut store = TripleStore::new();
        store.bulk_load(initial);
        let node = Arc::new(ShardNode::bind(store, "127.0.0.1:0").expect("bind node"));
        let addr = node.local_addr().expect("addr");
        thread::spawn(move || node.serve_forever());
        addr
    }

    /// End-to-end: spin up three remote shard nodes, route-insert the whole
    /// fixture across them over RPC, then run a distributed BGP + ASK and a
    /// fresh routed insert, all matching a single in-process store.
    #[test]
    fn distributed_query_and_routed_insert_across_remote_nodes() {
        let (mut d, triples) = fixture();
        // Baseline: a single store over the same triples (ShardedStore(1)).
        let base = ShardedStore::from_triples(1, triples.iter().copied());

        // Three empty gnode servers on ephemeral ports.
        let addrs: Vec<SocketAddr> = (0..3).map(|_| start_node(Vec::new())).collect();
        let shards: Vec<Shard> = addrs
            .iter()
            .map(|a| Shard::Remote(RemoteShard::connect(a).expect("connect")))
            .collect();
        let mut net = NetworkShardedStore::new(shards);

        // Routed insert of every triple over the wire.
        let added = net
            .insert_all(triples.iter().copied())
            .expect("routed insert");
        assert_eq!(added, base.triple_count(), "all distinct triples inserted");
        // Each triple lands on exactly one node → per-node counts sum to total.
        assert_eq!(
            net.try_triple_count().unwrap(),
            base.triple_count(),
            "distributed triple_count"
        );
        // Triples really did spread across more than one node.
        let nonempty = (0..net.num_shards())
            .filter(|&i| match net.shard(i) {
                Shard::Remote(r) => r.triple_count().unwrap() > 0,
                Shard::Local(s) => s.triple_count() > 0,
            })
            .count();
        assert!(nonempty > 1, "fixture should spread across >1 node");

        // A multi-pattern BGP, evaluated by the same generic Evaluator over the
        // distributed store and the single store, must yield identical rows.
        let q = "SELECT ?x ?y WHERE { \
                 ?x <http://ex/knows> ?y . \
                 ?y <http://ex/type> <http://ex/Person> . \
                 ?x <http://ex/name> ?n }";
        let net_rs = match net.query(&d, q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        let base_rs = match base.query(&d, q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        assert_eq!(net_rs.vars, base_rs.vars);
        assert!(!base_rs.rows.is_empty(), "fixture should produce some rows");
        assert_eq!(rows_sorted(&net_rs), rows_sorted(&base_rs));

        // A fresh routed insert is visible in a subsequent distributed query.
        let zoe = d.intern_term(&Term::iri("http://ex/zoe"));
        let knows = d
            .predicate_id(&Term::iri("http://ex/knows").dict_key())
            .unwrap();
        let alice = d.term_id(&Term::iri("http://ex/alice")).unwrap();
        assert!(
            net.insert(IdTriple::new(alice, knows, zoe)).unwrap(),
            "new edge inserted"
        );
        assert!(
            !net.insert(IdTriple::new(alice, knows, zoe)).unwrap(),
            "duplicate edge rejected"
        );
        let ask = "ASK { <http://ex/alice> <http://ex/knows> <http://ex/zoe> }";
        let yes = match net.query(&d, ask).unwrap() {
            QueryResult::Ask(b) => b,
            other => panic!("expected ASK, got {other:?}"),
        };
        assert!(yes, "routed insert visible to distributed ASK");
    }

    /// A [`NetworkShardedStore`] mixing one in-process [`Shard::Local`] with two
    /// remote nodes answers a distributed query identically to a single store.
    #[test]
    fn mixed_local_and_remote_shards_match_single_store() {
        let (d, triples) = fixture();
        let base = ShardedStore::from_triples(1, triples.iter().copied());

        let addr0 = start_node(Vec::new());
        let addr1 = start_node(Vec::new());
        let shards = vec![
            Shard::Local(TripleStore::new()),
            Shard::Remote(RemoteShard::connect(addr0).expect("connect 0")),
            Shard::Remote(RemoteShard::connect(addr1).expect("connect 1")),
        ];
        let mut net = NetworkShardedStore::new(shards);
        net.insert_all(triples.iter().copied())
            .expect("routed insert");

        assert_eq!(net.try_triple_count().unwrap(), base.triple_count());
        assert_eq!(net.try_iter_all().unwrap(), base.iter_all());

        let q = "SELECT ?x ?y WHERE { \
                 ?x <http://ex/knows> ?y . \
                 ?y <http://ex/type> <http://ex/Person> . \
                 ?x <http://ex/name> ?n }";
        let net_rs = match net.query(&d, q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        let base_rs = match base.query(&d, q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        assert_eq!(rows_sorted(&net_rs), rows_sorted(&base_rs));
    }

    /// Every fallible scatter-gather read of a healthy distributed store is
    /// byte-for-byte identical to the single-store answer (ordering, dedup and
    /// global-distinct statistics included).
    #[test]
    fn fallible_reads_match_single_store_when_every_shard_answers() {
        let (_d, triples) = fixture();
        let base = ShardedStore::from_triples(1, triples.iter().copied());
        let shards = vec![
            Shard::Remote(RemoteShard::connect(start_node(Vec::new())).unwrap()),
            Shard::Local(TripleStore::new()),
            Shard::Remote(RemoteShard::connect(start_node(Vec::new())).unwrap()),
        ];
        let mut net = NetworkShardedStore::new(shards);
        net.insert_all(triples.iter().copied()).unwrap();

        assert_eq!(net.try_triple_count().unwrap(), base.triple_count());
        assert_eq!(
            net.try_distinct_subjects().unwrap(),
            base.distinct_subjects()
        );
        assert_eq!(net.try_distinct_objects().unwrap(), base.distinct_objects());
        assert_eq!(net.try_num_predicates().unwrap(), base.num_predicates());
        assert_eq!(net.try_subject_keys().unwrap(), base.subject_keys());
        assert_eq!(net.try_object_keys().unwrap(), base.object_keys());
        assert_eq!(net.try_iter_all().unwrap(), base.iter_all());

        let mut ids = base.subject_keys();
        ids.extend(base.object_keys());
        let ids = sort_dedup(ids);
        let preds = sort_dedup(base.iter_all().iter().map(|t| t.pred).collect());
        for &p in &preds {
            assert_eq!(net.try_so_by_p(p).unwrap(), base.so_by_p(p), "so_by_p({p})");
            assert_eq!(net.try_subs_by_p(p).unwrap(), base.subs_by_p(p));
            assert_eq!(net.try_objs_by_p(p).unwrap(), base.objs_by_p(p));
            assert_eq!(net.try_pred_card(p).unwrap(), base.pred_card(p));
            assert_eq!(
                net.try_pred_distinct_subj(p).unwrap(),
                base.pred_distinct_subj(p)
            );
            assert_eq!(
                net.try_pred_distinct_obj(p).unwrap(),
                base.pred_distinct_obj(p)
            );
        }
        for &s in &ids {
            assert_eq!(net.try_po_by_s(s).unwrap(), base.po_by_s(s), "po_by_s({s})");
            assert_eq!(net.try_ps_by_o(s).unwrap(), base.ps_by_o(s), "ps_by_o({s})");
            for &p in &preds {
                assert_eq!(net.try_o_by_sp(s, p).unwrap(), base.o_by_sp(s, p));
                assert_eq!(net.try_s_by_po(p, s).unwrap(), base.s_by_po(p, s));
            }
            for &o in &ids {
                assert_eq!(net.try_p_by_so(s, o).unwrap(), base.p_by_so(s, o));
                for &p in &preds {
                    assert_eq!(net.try_exists(s, p, o).unwrap(), base.exists(s, p, o));
                }
            }
        }
    }

    /// Regression: a remote shard that is down used to be merged in as an
    /// *empty* answer, so a query silently returned a partial result that looked
    /// complete. It must now fail — naming the shard — and recover once the shard
    /// is back.
    #[test]
    fn query_errors_instead_of_returning_partial_results_when_a_shard_is_down() {
        use super::test_support::Link;
        let (d, triples) = fixture();
        let base = ShardedStore::from_triples(1, triples.iter().copied());
        let q = "SELECT ?x ?y WHERE { \
                 ?x <http://ex/knows> ?y . \
                 ?y <http://ex/type> <http://ex/Person> . \
                 ?x <http://ex/name> ?n }";
        let rows = |r: QueryResult| match r {
            QueryResult::Select(rs) => rows_sorted(&rs),
            other => panic!("expected SELECT, got {other:?}"),
        };
        let expected = rows(base.query(&d, q).unwrap());

        let links: Vec<Link> = (0..3).map(|_| Link::to(start_node(Vec::new()))).collect();
        let shards = links
            .iter()
            .map(|l| Shard::Remote(RemoteShard::connect(l.addr()).unwrap()))
            .collect();
        let mut net = NetworkShardedStore::new(shards);
        net.insert_all(triples.iter().copied()).unwrap();
        assert_eq!(rows(net.query(&d, q).unwrap()), expected, "healthy cluster");

        // Take down a shard that holds part of the answer. A best-effort merge
        // would have answered from the survivors alone — a *different*, wrong
        // result that this test would previously have received.
        let victim = (0..net.num_shards())
            .find(|&i| triples.iter().any(|t| net.shard_index(t.sub) == i))
            .unwrap();
        let surviving = ShardedStore::from_triples(
            1,
            triples
                .iter()
                .copied()
                .filter(|t| net.shard_index(t.sub) != victim),
        );
        assert_ne!(
            rows(surviving.query(&d, q).unwrap()),
            expected,
            "the down shard must matter to the answer for this test to bite"
        );
        links[victim].cut();

        let err = net
            .query(&d, q)
            .expect_err("a down shard must fail the query");
        assert!(matches!(err, GStoreError::Io(_)), "got {err:?}");
        let victim_addr = links[victim].addr().to_string();
        assert!(
            err.to_string().contains(&victim_addr),
            "the error should name the down shard {victim_addr}: {err}"
        );
        assert!(net.try_triple_count().is_err());
        assert!(net.try_iter_all().is_err());
        assert!(net.try_subject_keys().is_err());
        assert!(net.try_po_by_s(0).is_err());

        // The failure is not sticky: once the shard is reachable again the
        // full, correct answer comes back.
        links[victim].restore();
        assert_eq!(rows(net.query(&d, q).unwrap()), expected, "after recovery");
        assert_eq!(net.try_triple_count().unwrap(), base.triple_count());
    }
}

/// Tests for the **Raft-like replication layer**. The bulk are *deterministic*:
/// a [`Sim`] drives `N` in-memory [`RaftState`]s by hand — delivering every
/// message synchronously and ticking exactly the node we choose — so elections,
/// replication, failover, catch-up, snapshotting, and split-brain safety are
/// exercised with **no clocks and no threads** (zero wall-clock flakiness). The
/// `networked_*` tests (and the quorum / hung-peer regressions next to them) run
/// real [`ClusterNode`]s over TCP loopback (background threads on
/// `127.0.0.1:0`, like `tests/dt_service.rs`) to prove the transport,
/// election, failover, and recovery work end-to-end; they tolerate leadership
/// changes and slow schedulers by design (see the section comment there).
#[cfg(test)]
mod raft_tests {
    use super::*;
    use std::net::{SocketAddr, TcpStream};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    fn t(s: u32, p: u32, o: u32) -> IdTriple {
        IdTriple::new(s, p, o)
    }

    // ----- Deterministic in-memory simulation -------------------------------

    /// An in-memory cluster of `RaftState` machines plus their replicated
    /// stores. Messages are routed synchronously; only the node(s) we `tick`
    /// advance their clocks, so every scenario is fully reproducible.
    struct Sim {
        nodes: Vec<RaftState>,
        stores: Vec<TripleStore>,
        /// Per-node replicated cluster config, updated as committed
        /// config-change entries apply (mirrors [`ClusterNode`]'s config).
        configs: Vec<ClusterConfig>,
        down: Vec<bool>,
    }

    /// Shard-bucket count used by the deterministic membership simulation.
    const SIM_SHARDS: usize = 8;

    impl Sim {
        fn new(n: usize) -> Sim {
            let cfg = RaftConfig::default();
            let ids: Vec<NodeId> = (0..n as u64).collect();
            let nodes = ids
                .iter()
                .map(|&id| {
                    let peers: Vec<NodeId> = ids.iter().copied().filter(|&p| p != id).collect();
                    RaftState::new(id, peers, &cfg)
                })
                .collect();
            let stores = (0..n).map(|_| TripleStore::new()).collect();
            let configs = (0..n).map(|_| ClusterConfig::new(SIM_SHARDS)).collect();
            Sim {
                nodes,
                stores,
                configs,
                down: vec![false; n],
            }
        }

        fn node(&self, id: NodeId) -> &RaftState {
            &self.nodes[id as usize]
        }

        fn config(&self, id: NodeId) -> &ClusterConfig {
            &self.configs[id as usize]
        }

        fn set_down(&mut self, id: NodeId, down: bool) {
            self.down[id as usize] = down;
        }

        /// Apply each node's freshly committed mutations to its store.
        fn sync_apply(&mut self) {
            for i in 0..self.nodes.len() {
                let actions = self.nodes[i].drain_apply();
                for a in actions {
                    match a {
                        StoreAction::Apply(LogOp::Insert(tr)) => {
                            self.stores[i].insert(tr);
                        }
                        StoreAction::Apply(LogOp::Delete(tr)) => {
                            self.stores[i].remove(tr);
                        }
                        StoreAction::Apply(
                            op @ (LogOp::AddNode { .. } | LogOp::RemoveNode { .. }),
                        ) => {
                            self.configs[i].apply(&op);
                        }
                        // A no-op term-marker carries no state change.
                        StoreAction::Apply(LogOp::Noop) => {}
                        StoreAction::Reset(triples) => {
                            self.stores[i] = TripleStore::new();
                            self.stores[i].bulk_load(triples);
                        }
                    }
                }
            }
        }

        /// Synchronously deliver `outs` (from `src`), feeding replies back and
        /// processing any follow-up messages, then apply committed entries.
        fn deliver(&mut self, src: NodeId, outs: Vec<Outgoing>) {
            let mut q: VecDeque<(NodeId, Outgoing)> = outs.into_iter().map(|o| (src, o)).collect();
            let mut guard = 0usize;
            while let Some((from, o)) = q.pop_front() {
                guard += 1;
                if guard > 100_000 {
                    break;
                }
                let to = o.to;
                if self.down[to as usize] || self.down[from as usize] {
                    continue; // a down node neither answers nor processes replies
                }
                let reply = self.nodes[to as usize].handle(o.msg);
                let more = self.nodes[from as usize].handle_reply(to, reply);
                for m in more {
                    q.push_back((from, m));
                }
            }
            self.sync_apply();
        }

        /// Advance one node's logical clock by a tick and deliver what it emits.
        fn tick(&mut self, id: NodeId) {
            if self.down[id as usize] {
                return;
            }
            let outs = self.nodes[id as usize].tick();
            self.deliver(id, outs);
        }

        /// Drive only `id`'s clock until it wins leadership (deterministic: no
        /// other node times out because we never tick it).
        fn elect(&mut self, id: NodeId) {
            for _ in 0..500 {
                if self.nodes[id as usize].is_leader() {
                    return;
                }
                self.tick(id);
            }
            panic!("node {id} failed to become leader");
        }

        /// Propose a mutation on `id` and replicate it immediately.
        fn propose(&mut self, id: NodeId, op: LogOp) -> Option<u64> {
            let idx = self.nodes[id as usize].propose(op)?;
            let outs = self.nodes[id as usize].replicate_now();
            self.deliver(id, outs);
            Some(idx)
        }
    }

    #[test]
    fn election_produces_a_single_leader() {
        let mut sim = Sim::new(3);
        sim.elect(0);
        assert!(sim.node(0).is_leader());
        assert_eq!(sim.node(1).role(), Role::Follower);
        assert_eq!(sim.node(2).role(), Role::Follower);
        assert_eq!(sim.node(0).current_term(), 1);
        // Followers learn the leader from its heartbeat.
        assert_eq!(sim.node(1).leader_id(), Some(0));
        assert_eq!(sim.node(2).leader_id(), Some(0));
    }

    #[test]
    fn log_replication_converges_on_every_node() {
        let mut sim = Sim::new(3);
        sim.elect(0);
        let triples = [t(1, 2, 3), t(4, 5, 6), t(7, 8, 9)];
        for &tr in &triples {
            assert!(sim.propose(0, LogOp::Insert(tr)).is_some());
        }
        for id in 0..3u64 {
            assert_eq!(
                sim.stores[id as usize].triple_count(),
                3,
                "node {id} triple count"
            );
            for &tr in &triples {
                assert!(
                    sim.stores[id as usize].exists(tr.sub, tr.pred, tr.obj),
                    "node {id} missing {tr:?}"
                );
            }
            assert_eq!(sim.node(id).commit_index(), 3, "node {id} commit index");
        }
    }

    #[test]
    fn delete_ops_replicate_and_apply() {
        let mut sim = Sim::new(3);
        sim.elect(0);
        let a = t(1, 2, 3);
        sim.propose(0, LogOp::Insert(a));
        assert!(sim.stores[2].exists(1, 2, 3));
        sim.propose(0, LogOp::Delete(a));
        for id in 0..3u64 {
            assert!(
                !sim.stores[id as usize].exists(1, 2, 3),
                "node {id} should have applied the delete"
            );
        }
    }

    #[test]
    fn followers_do_not_campaign_while_heartbeats_arrive() {
        let mut sim = Sim::new(3);
        sim.elect(0);
        // The leader keeps heart-beating; followers never reach their election
        // timeout, so the term stays put and node 0 stays leader.
        for _ in 0..50 {
            sim.tick(0);
        }
        assert!(sim.node(0).is_leader());
        assert_eq!(sim.node(0).current_term(), 1);
        assert_eq!(sim.node(1).role(), Role::Follower);
        assert_eq!(sim.node(2).role(), Role::Follower);
    }

    #[test]
    fn committed_entry_stays_visible_after_failover_without_a_new_write() {
        // node0 commits an entry, then dies before telling the followers it is
        // committed. The new leader must make it visible cluster-wide *without* a
        // new client write — which requires committing a no-op of its own term.
        let mut sim = Sim::new(3);
        sim.elect(0);
        let tri = t(7, 8, 9);
        let idx = sim.nodes[0]
            .propose(LogOp::Insert(tri))
            .expect("leader accepts write");

        // Replicate the entry to node1 (node0+node1 = a majority) and let node0
        // learn it committed — but deliver nothing to node2 and drop node0's
        // commit broadcast, modelling a leader that dies right after committing.
        let appends: Vec<Outgoing> = sim.nodes[0]
            .tick()
            .into_iter()
            .filter(|o| o.to == 1)
            .collect();
        assert!(
            !appends.is_empty(),
            "leader emits an AppendEntries to node1"
        );
        for o in appends {
            let reply = sim.nodes[1].handle(o.msg);
            let _dropped = sim.nodes[0].handle_reply(1, reply);
        }
        assert_eq!(
            sim.nodes[0].commit_index(),
            idx,
            "leader committed the entry"
        );
        assert_eq!(
            sim.nodes[1].commit_index(),
            0,
            "follower not told it committed"
        );

        // node0 dies; node1 (whose log holds the entry) wins the next election and
        // is driven until its own commit covers the entry — with no new write.
        sim.set_down(0, true);
        sim.elect(1);
        for _ in 0..500 {
            if sim.nodes[1].commit_index() >= idx {
                break;
            }
            sim.tick(1);
        }
        assert!(
            sim.nodes[1].commit_index() >= idx,
            "new leader must commit the pre-failover entry via a current-term no-op"
        );
        assert!(
            sim.stores[1].exists(tri.sub, tri.pred, tri.obj),
            "the committed triple is visible after failover with no new client write"
        );
    }

    #[test]
    fn a_snapshot_at_or_below_commit_is_ignored() {
        // Two entries are committed and applied everywhere. A stale InstallSnapshot
        // whose last-included index is at/below the follower's commit index must be
        // ignored: installing it would clear the log and reset the store to an
        // older state, dropping the already-committed second triple.
        let mut sim = Sim::new(3);
        sim.elect(0);
        sim.propose(0, LogOp::Insert(t(1, 1, 1)));
        sim.propose(0, LogOp::Insert(t(2, 2, 2)));
        let commit_before = sim.nodes[1].commit_index();
        assert!(commit_before >= 2, "both entries committed on the follower");
        assert!(sim.stores[1].exists(1, 1, 1) && sim.stores[1].exists(2, 2, 2));

        let stale = RaftMsg::InstallSnapshot {
            term: sim.nodes[1].current_term(),
            leader_id: 0,
            last_included_index: 1, // <= the follower's commit index
            last_included_term: 1,
            triples: vec![t(1, 1, 1)], // store *as of* index 1 — missing t(2,2,2)
        };
        let _ = sim.nodes[1].handle(stale);
        sim.sync_apply();

        assert_eq!(
            sim.nodes[1].commit_index(),
            commit_before,
            "a stale snapshot must not move commit backwards"
        );
        assert!(
            sim.stores[1].exists(2, 2, 2),
            "a committed triple must survive a stale snapshot install"
        );
    }

    #[test]
    fn failover_elects_new_leader_and_keeps_accepting_writes() {
        let mut sim = Sim::new(3);
        sim.elect(0);
        let a = t(1, 5, 1);
        sim.propose(0, LogOp::Insert(a));
        for id in 0..3u64 {
            assert_eq!(sim.stores[id as usize].triple_count(), 1);
        }
        let term0 = sim.node(0).current_term();

        // Kill the leader; a survivor takes over in a higher term.
        sim.set_down(0, true);
        sim.elect(1);
        assert!(sim.node(1).is_leader());
        assert!(
            sim.node(1).current_term() > term0,
            "a failover bumps the term"
        );

        // The cluster still accepts writes.
        let b = t(2, 5, 2);
        assert!(sim.propose(1, LogOp::Insert(b)).is_some());
        assert!(sim.stores[1].exists(b.sub, b.pred, b.obj));
        assert!(sim.stores[2].exists(b.sub, b.pred, b.obj));

        // The old leader restarts and catches up, stepping down to follower.
        sim.set_down(0, false);
        for _ in 0..6 {
            sim.tick(1);
        }
        assert!(sim.stores[0].exists(a.sub, a.pred, a.obj));
        assert!(sim.stores[0].exists(b.sub, b.pred, b.obj));
        assert_eq!(sim.stores[0].triple_count(), 2);
        assert_eq!(
            sim.node(0).role(),
            Role::Follower,
            "the stale leader steps down"
        );
        assert_eq!(sim.node(0).current_term(), sim.node(1).current_term());
    }

    #[test]
    fn lagging_follower_catches_up_by_log_backfill() {
        let mut sim = Sim::new(3);
        sim.elect(0);
        // Node 2 is offline while several writes commit on {0,1}.
        sim.set_down(2, true);
        let triples = [t(1, 1, 1), t(2, 2, 2), t(3, 3, 3)];
        for &tr in &triples {
            sim.propose(0, LogOp::Insert(tr));
        }
        assert_eq!(
            sim.stores[2].triple_count(),
            0,
            "offline node missed writes"
        );

        // Node 2 rejoins; leader heartbeats backfill the missing entries.
        sim.set_down(2, false);
        for _ in 0..10 {
            sim.tick(0);
        }
        assert_eq!(sim.stores[2].triple_count(), 3, "rejoined node caught up");
        for &tr in &triples {
            assert!(sim.stores[2].exists(tr.sub, tr.pred, tr.obj));
        }
    }

    #[test]
    fn snapshot_recovers_a_follower_past_the_compaction_point() {
        let mut sim = Sim::new(3);
        sim.elect(0);
        sim.set_down(2, true); // node 2 misses everything
        let triples = [t(1, 9, 1), t(2, 9, 2), t(3, 9, 3), t(5, 9, 5)];
        for &tr in &triples {
            sim.propose(0, LogOp::Insert(tr));
        }
        assert_eq!(sim.stores[0].triple_count(), 4);
        assert_eq!(sim.stores[1].triple_count(), 4);

        // The leader compacts its committed log into a snapshot of its store,
        // discarding the individual entries.
        let ci = sim.node(0).commit_index();
        let snap: Vec<IdTriple> = sim.stores[0].iter_all().collect();
        sim.nodes[0].compact(ci, snap);
        assert_eq!(sim.node(0).base_index(), ci, "log was compacted");

        // Node 2 rejoins; the only way to catch it up is an InstallSnapshot.
        sim.set_down(2, false);
        for _ in 0..10 {
            sim.tick(0);
        }
        assert_eq!(
            sim.stores[2].triple_count(),
            4,
            "follower recovered via snapshot"
        );
        for &tr in &triples {
            assert!(sim.stores[2].exists(tr.sub, tr.pred, tr.obj));
        }
        assert_eq!(sim.node(2).base_index(), ci, "snapshot base adopted");
    }

    #[test]
    fn stale_leader_steps_down_and_uncommitted_writes_are_discarded() {
        let mut sim = Sim::new(3);
        sim.elect(0);

        // Isolate the leader from the majority; its write cannot reach quorum.
        sim.set_down(1, true);
        sim.set_down(2, true);
        let x = t(9, 9, 9);
        sim.propose(0, LogOp::Insert(x));
        assert_eq!(
            sim.node(0).commit_index(),
            0,
            "no quorum, so nothing commits"
        );
        assert!(
            !sim.stores[0].exists(9, 9, 9),
            "an uncommitted write is never applied"
        );

        // The majority heals and elects a new leader while the old one is away.
        sim.set_down(1, false);
        sim.set_down(2, false);
        sim.set_down(0, true);
        sim.elect(1);
        assert!(sim.node(1).current_term() > sim.node(0).current_term());

        // The old leader rejoins; the new leader's write at the same index has a
        // higher term, so the stale entry is overwritten (split-brain safety).
        sim.set_down(0, false);
        let y = t(7, 7, 7);
        sim.propose(1, LogOp::Insert(y));
        for _ in 0..6 {
            sim.tick(1);
        }
        assert_eq!(
            sim.node(0).role(),
            Role::Follower,
            "the stale leader steps down"
        );
        assert!(
            sim.stores[0].exists(7, 7, 7),
            "old leader converges to the new leader's data"
        );
        assert!(
            !sim.stores[0].exists(9, 9, 9),
            "the stale uncommitted entry is discarded"
        );
    }

    #[test]
    fn single_node_cluster_self_elects_and_commits() {
        let mut sim = Sim::new(1);
        sim.elect(0);
        assert!(sim.node(0).is_leader());
        assert!(sim.propose(0, LogOp::Insert(t(1, 1, 1))).is_some());
        assert!(sim.stores[0].exists(1, 1, 1));
        assert_eq!(sim.node(0).commit_index(), 1);
    }

    /// Task 1: cluster membership is a config object replicated *through the Raft
    /// log*. A leader proposes `AddNode`/`RemoveNode` config-change entries; they
    /// are committed by quorum and applied on every replica, so all nodes converge
    /// on a byte-identical [`ClusterConfig`]. Fully deterministic (no clocks).
    #[test]
    fn membership_config_changes_replicate_through_the_raft_log() {
        use std::net::SocketAddr;
        let a = |p: u16| -> SocketAddr { format!("127.0.0.1:{p}").parse().unwrap() };

        let mut sim = Sim::new(3);
        sim.elect(0);
        // Found the cluster (admit the three running nodes), then a fourth joins,
        // then one leaves — every step a committed config-change entry.
        sim.propose(
            0,
            LogOp::AddNode {
                id: 0,
                addr: a(6000),
            },
        );
        sim.propose(
            0,
            LogOp::AddNode {
                id: 1,
                addr: a(6001),
            },
        );
        sim.propose(
            0,
            LogOp::AddNode {
                id: 2,
                addr: a(6002),
            },
        );
        sim.propose(
            0,
            LogOp::AddNode {
                id: 3,
                addr: a(6003),
            },
        ); // join
        sim.propose(0, LogOp::RemoveNode { id: 1 }); // leave

        let c0 = sim.config(0).clone();
        assert_eq!(c0.members(), vec![0, 2, 3], "final membership");
        assert_eq!(c0.epoch(), 5, "five committed config-changes");
        // Every replica that applied the same committed prefix agrees exactly.
        for id in 0..3u64 {
            assert_eq!(sim.config(id), &c0, "node {id} config diverged");
        }
        // The shard→owner map is total and only references current members.
        assert_eq!(c0.assignment().len(), c0.num_shards());
        for s in 0..c0.num_shards() {
            assert!(c0.members().contains(&c0.owner(s).unwrap()));
        }
        // The membership changes were committed (by quorum), not merely logged.
        assert_eq!(sim.node(0).commit_index(), 5);
    }

    /// Regression (Raft receiver rule): a follower may only commit entries the
    /// AppendEntries it is processing has *verified* against the leader. Clamping
    /// `leaderCommit` to the follower's own last index instead would commit a
    /// stale, never-verified suffix left behind by a deposed leader.
    #[test]
    fn follower_commits_only_entries_verified_by_the_append() {
        let mut sim = Sim::new(3);
        sim.elect(0);
        // Leader 0 (term 1) appends two entries that reach node 1 but never
        // commit (node 2 and the commit broadcast are dropped).
        sim.nodes[0].propose(LogOp::Insert(t(1, 1, 1))).unwrap();
        sim.nodes[0].propose(LogOp::Insert(t(2, 2, 2))).unwrap();
        for o in sim.nodes[0]
            .replicate_now()
            .into_iter()
            .filter(|o| o.to == 1)
        {
            let _ = sim.nodes[1].handle(o.msg);
        }
        assert_eq!(
            sim.nodes[1].log_len(),
            2,
            "node 1 holds the unverified suffix"
        );
        assert_eq!(sim.nodes[1].commit_index(), 0);

        // A newer leader's AppendEntries that only vouches for the log up to
        // index 0 must not let node 1 commit its unverified entries 1-2, however
        // far `leader_commit` reaches.
        let reply = sim.nodes[1].handle(RaftMsg::AppendEntries {
            term: 2,
            leader_id: 2,
            prev_log_index: 0,
            prev_log_term: 0,
            leader_commit: 2,
            entries: vec![],
        });
        assert!(matches!(reply, RaftReply::Append { success: true, .. }));
        assert_eq!(
            sim.nodes[1].commit_index(),
            0,
            "commit must be capped by the entries this append verified"
        );
    }

    // ----- Networked end-to-end (real TCP, background threads) ---------------
    //
    // These run real nodes on wall-clock timers, so they are written to be
    // robust on a loaded CI machine rather than to assume a quiet one:
    //
    // * timing is generous (`test_cfg`): heartbeats every 20 ms against a
    //   250-500 ms election timeout, so a leader is not deposed just because a
    //   thread was descheduled for a few tens of milliseconds;
    // * nothing assumes leadership is permanent: `stable_leader` waits until the
    //   same (leader, term) is observed twice, and every write goes through
    //   `write_committed` / `client_write_committed`, which retry across leader
    //   changes until the write is confirmed *committed* (writes are idempotent);
    // * every failure message carries a per-node status dump.

    /// Wall-clock deadline for any single convergence step.
    const DEADLINE: Duration = Duration::from_secs(30);

    /// Timing for the networked tests (see the section comment above).
    fn test_cfg() -> RaftConfig {
        RaftConfig {
            election_timeout_min: 25,
            election_timeout_max: 50,
            heartbeat_timeout: 2,
            tick_interval: Duration::from_millis(10),
        }
    }

    /// Build, wire up, and start an `n`-node TCP cluster on ephemeral ports.
    fn build_cluster(n: usize, cfg: RaftConfig) -> Vec<Arc<ClusterNode>> {
        let ids: Vec<NodeId> = (0..n as u64).collect();
        let nodes: Vec<Arc<ClusterNode>> = ids
            .iter()
            .map(|&id| {
                let peers: Vec<NodeId> = ids.iter().copied().filter(|&p| p != id).collect();
                ClusterNode::bind(id, peers, TripleStore::new(), "127.0.0.1:0", cfg.clone())
                    .expect("bind cluster node")
            })
            .collect();
        let addrs: Vec<SocketAddr> = nodes.iter().map(|nd| nd.local_addr().unwrap()).collect();
        for nd in &nodes {
            for (i, a) in addrs.iter().enumerate() {
                let pid = i as u64;
                if pid != nd.id() {
                    nd.set_peer_addr(pid, *a);
                }
            }
        }
        for nd in &nodes {
            let _ = nd.start();
        }
        nodes
    }

    /// One line per node — liveness, role, term, believed leader, commit index,
    /// store size — appended to every networked assertion so a CI failure is
    /// diagnosable from the log alone.
    fn describe(nodes: &[Arc<ClusterNode>]) -> String {
        nodes
            .iter()
            .map(|n| {
                format!(
                    "  node {}: alive={} role={:?} term={} leader={:?} commit={} triples={}",
                    n.id(),
                    n.is_alive(),
                    n.role(),
                    n.current_term(),
                    n.leader_id(),
                    n.commit_index(),
                    n.store_triple_count()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The single alive leader, if exactly one currently claims leadership.
    fn current_leader(nodes: &[Arc<ClusterNode>]) -> Option<usize> {
        let leaders: Vec<usize> = nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.is_alive() && n.is_leader())
            .map(|(i, _)| i)
            .collect();
        if leaders.len() == 1 {
            Some(leaders[0])
        } else {
            None
        }
    }

    /// Wait for a *stable* leader: the same alive leader in the same term on two
    /// observations a couple of heartbeats apart. Panics with a status dump if
    /// none emerges by [`DEADLINE`].
    fn stable_leader(nodes: &[Arc<ClusterNode>]) -> usize {
        let start = Instant::now();
        let mut last: Option<(usize, u64)> = None;
        while start.elapsed() < DEADLINE {
            let now = current_leader(nodes).map(|l| (l, nodes[l].current_term()));
            if let Some((l, _)) = now {
                if now == last {
                    return l;
                }
            }
            last = now;
            thread::sleep(Duration::from_millis(50));
        }
        panic!("no stable leader within {DEADLINE:?}\n{}", describe(nodes));
    }

    /// Commit `op` through whichever node currently leads, retrying across
    /// leadership changes until a leader confirms it committed (and applied it).
    /// Returns the index of the node that confirmed it.
    fn write_committed(nodes: &[Arc<ClusterNode>], op: LogOp) -> usize {
        let start = Instant::now();
        let mut last_err = None;
        while start.elapsed() < DEADLINE {
            let l = stable_leader(nodes);
            match nodes[l].propose_and_wait(op, Duration::from_secs(2)) {
                Ok(_) => return l,
                Err(e) => last_err = Some(e),
            }
        }
        panic!(
            "write {op:?} not committed within {DEADLINE:?} (last error: {last_err:?})\n{}",
            describe(nodes)
        );
    }

    /// Open a client connection to a node, with a read timeout comfortably
    /// above the server's commit-wait bound.
    fn client(addr: SocketAddr) -> TcpStream {
        let s = TcpStream::connect(addr).expect("connect to cluster node");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s
    }

    /// Submit `op` as a wire `ClientWrite` to the current leader, retrying on a
    /// redirect / unconfirmed ack until some node acknowledges it. Returns the
    /// acknowledging node and the committed index.
    fn client_write_committed(nodes: &[Arc<ClusterNode>], op: LogOp) -> (usize, u64) {
        let start = Instant::now();
        let mut last = None;
        while start.elapsed() < DEADLINE {
            let l = stable_leader(nodes);
            let mut s = client(nodes[l].local_addr().unwrap());
            match crate::rpc::round_trip(&mut s, &Request::ClientWrite { op }) {
                Ok(Response::WriteAck {
                    ok: true, index, ..
                }) => return (l, index),
                Ok(resp @ Response::WriteAck { ok: false, .. }) => last = Some(format!("{resp:?}")),
                Ok(other) => panic!("expected WriteAck, got {other:?}"),
                Err(e) => last = Some(e.to_string()),
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "ClientWrite {op:?} never acknowledged within {DEADLINE:?} (last: {last:?})\n{}",
            describe(nodes)
        );
    }

    /// Poll `cond` until it holds or [`DEADLINE`] elapses.
    fn wait_until<F: Fn() -> bool>(cond: F) -> bool {
        let start = Instant::now();
        while start.elapsed() < DEADLINE {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    fn shutdown_all(nodes: &[Arc<ClusterNode>]) {
        for n in nodes {
            n.shutdown();
        }
    }

    #[test]
    fn networked_cluster_replicates_then_fails_over_then_recovers() {
        let nodes = build_cluster(3, test_cfg());

        // 1-2. A leader is elected over the wire; committed writes converge on
        //      every replica.
        let triples = [t(1, 10, 100), t(2, 10, 200), t(3, 10, 300)];
        for &tr in &triples {
            write_committed(&nodes, LogOp::Insert(tr));
        }
        assert!(
            wait_until(|| nodes
                .iter()
                .all(|n| n.store_triple_count() == triples.len() as u64
                    && triples.iter().all(|&tr| n.store_contains(tr)))),
            "replicas did not converge on the initial writes\n{}",
            describe(&nodes)
        );

        // 3. Kill the leader; a new leader emerges and still commits writes.
        let l = stable_leader(&nodes);
        let old_term = nodes[l].current_term();
        nodes[l].kill();
        // (Only alive nodes count. The term bump of a failover is asserted
        // deterministically by `failover_elects_new_leader_and_keeps_accepting_writes`;
        // here leadership may already have moved just before the kill.)
        let l2 = stable_leader(&nodes);
        assert_ne!(l2, l, "a dead node cannot lead\n{}", describe(&nodes));
        let extra = t(4, 10, 400);
        write_committed(&nodes, LogOp::Insert(extra));
        let survivors: Vec<usize> = (0..3).filter(|&i| i != l).collect();
        assert!(
            wait_until(|| survivors.iter().all(|&i| nodes[i].store_contains(extra))),
            "the post-failover write did not reach the survivors\n{}",
            describe(&nodes)
        );

        // 4. Restart the old leader: it never resumes its stale leadership, and
        //    it catches up to all four triples.
        nodes[l].revive();
        assert!(
            !nodes[l].is_leader(),
            "a revived node must not resume its stale leadership"
        );
        let all: Vec<IdTriple> = triples
            .iter()
            .copied()
            .chain(std::iter::once(extra))
            .collect();
        assert!(
            wait_until(|| nodes[l].store_triple_count() == 4
                && all.iter().all(|&tr| nodes[l].store_contains(tr))),
            "the revived old leader did not catch up\n{}",
            describe(&nodes)
        );
        // It may legitimately win a *later* election, but never lead in the
        // term it led before the crash.
        assert!(
            !nodes[l].is_leader() || nodes[l].current_term() > old_term,
            "the revived stale leader must step down\n{}",
            describe(&nodes)
        );

        shutdown_all(&nodes);
    }

    #[test]
    fn networked_client_write_rpc_commits_on_leader() {
        let nodes = build_cluster(3, test_cfg());

        // A ClientWrite RPC to the leader is acknowledged only once committed:
        // the acknowledging leader has already applied it.
        let tr = t(7, 70, 700);
        let (l, index) = client_write_committed(&nodes, LogOp::Insert(tr));
        assert!(index > 0);
        assert!(
            nodes[l].store_contains(tr),
            "an acknowledged write is already applied on the leader"
        );
        assert!(
            wait_until(|| nodes.iter().all(|n| n.store_contains(tr))),
            "client-written triple did not replicate\n{}",
            describe(&nodes)
        );

        // A ClientWrite to a follower that knows the leader is answered with a
        // redirect naming a leader. (Leadership can move between choosing the
        // follower and its answer, so retry until a clean redirect is seen.)
        let start = Instant::now();
        loop {
            assert!(
                start.elapsed() < DEADLINE,
                "never saw a follower redirect\n{}",
                describe(&nodes)
            );
            let l = stable_leader(&nodes);
            let Some(f) = (0..3).find(|&i| i != l && nodes[i].leader_id() == Some(l as u64)) else {
                thread::sleep(Duration::from_millis(20));
                continue;
            };
            let mut s = client(nodes[f].local_addr().unwrap());
            let resp = crate::rpc::round_trip(
                &mut s,
                &Request::ClientWrite {
                    op: LogOp::Insert(t(8, 80, 800)),
                },
            )
            .expect("follower client write round-trip");
            match resp {
                Response::WriteAck {
                    ok: false,
                    leader_hint,
                    ..
                } if leader_hint >= 0 => {
                    assert!(
                        (0..3).contains(&leader_hint) && leader_hint != f as i64,
                        "a redirect must name another node, got {leader_hint}"
                    );
                    break;
                }
                // The follower became leader in the meantime (and committed), or
                // the leader was momentarily unknown: try again.
                Response::WriteAck { .. } => thread::sleep(Duration::from_millis(20)),
                other => panic!("expected WriteAck, got {other:?}"),
            }
        }

        shutdown_all(&nodes);
    }

    /// Regression: a `ClientWrite` used to be acknowledged `ok` as soon as the
    /// leader *appended* it, so a leader cut off from its quorum told clients
    /// writes had succeeded that could never commit (and that a later leader
    /// would discard). An acknowledgement must now mean "committed".
    #[test]
    fn client_write_is_not_acknowledged_without_a_quorum() {
        let nodes = build_cluster(3, test_cfg());
        let l = stable_leader(&nodes);
        // Isolate the leader: with both followers down no quorum exists.
        for i in (0..3).filter(|&i| i != l) {
            nodes[i].kill();
        }
        nodes[l].set_commit_timeout(Duration::from_millis(300));

        let tr = t(9, 90, 900);
        let mut s = client(nodes[l].local_addr().unwrap());
        let resp = crate::rpc::round_trip(
            &mut s,
            &Request::ClientWrite {
                op: LogOp::Insert(tr),
            },
        )
        .expect("client write round-trip");
        assert!(
            matches!(resp, Response::WriteAck { ok: false, .. }),
            "a write without a quorum must not be acknowledged, got {resp:?}\n{}",
            describe(&nodes)
        );
        assert!(
            !nodes[l].store_contains(tr),
            "an uncommitted write is never applied"
        );
        assert!(
            matches!(
                nodes[l].propose_and_wait(LogOp::Insert(t(9, 91, 901)), Duration::from_millis(200)),
                Err(ProposeError::NotCommitted(_) | ProposeError::NotLeader(_))
            ),
            "propose_and_wait must not report an unconfirmed write as committed"
        );

        // Once the quorum is back, writes commit again.
        for i in (0..3).filter(|&i| i != l) {
            nodes[i].revive();
        }
        write_committed(&nodes, LogOp::Insert(t(10, 100, 1000)));

        shutdown_all(&nodes);
    }

    /// Regression (liveness): one *hung* peer — a host that accepts TCP
    /// connections but never answers, e.g. a stopped process or a partition
    /// that drops packets — must not stall replication to the healthy majority.
    /// The transport used to deliver every message sequentially from one
    /// thread, so each heartbeat round waited out the hung peer's I/O timeouts
    /// (2 x 300 ms) before the healthy follower heard from the leader again; the
    /// follower's election timer expired every round and the cluster churned
    /// through elections while two of three nodes were perfectly healthy.
    #[test]
    fn a_hung_peer_does_not_stall_the_healthy_majority() {
        // Node 2 is a black hole: it accepts connections and never replies.
        let hole = TcpListener::bind("127.0.0.1:0").unwrap();
        let hole_addr = hole.local_addr().unwrap();
        thread::spawn(move || {
            let mut held = Vec::new();
            for s in hole.incoming().flatten() {
                held.push(s); // keep the socket open, never read or answer it
            }
        });

        let ids = [0u64, 1, 2];
        let nodes: Vec<Arc<ClusterNode>> = [0u64, 1]
            .iter()
            .map(|&id| {
                let peers: Vec<NodeId> = ids.iter().copied().filter(|&p| p != id).collect();
                ClusterNode::bind(id, peers, TripleStore::new(), "127.0.0.1:0", test_cfg()).unwrap()
            })
            .collect();
        let addr_of = |id: u64| match id {
            2 => hole_addr,
            i => nodes[i as usize].local_addr().unwrap(),
        };
        for n in &nodes {
            for &p in ids.iter().filter(|&&p| p != n.id()) {
                n.set_peer_addr(p, addr_of(p));
            }
        }
        for n in &nodes {
            let _ = n.start();
        }

        // The healthy majority elects a leader and keeps it: the term must not
        // move while heartbeats flow, however long the hung peer blocks.
        let l = stable_leader(&nodes);
        let term = nodes[l].current_term();
        let watch = Instant::now();
        while watch.elapsed() < Duration::from_millis(1500) {
            assert_eq!(
                (current_leader(&nodes), nodes[l].current_term()),
                (Some(l), term),
                "leadership churned while only one peer was hung\n{}",
                describe(&nodes)
            );
            thread::sleep(Duration::from_millis(20));
        }

        // And a write commits on the majority without waiting for the hung
        // peer's timeouts.
        let started = Instant::now();
        nodes[l]
            .propose_and_wait(LogOp::Insert(t(5, 50, 500)), Duration::from_secs(5))
            .expect("the healthy majority commits");
        let took = started.elapsed();
        assert!(
            took < Duration::from_millis(250),
            "commit waited on the hung peer ({took:?})\n{}",
            describe(&nodes)
        );

        shutdown_all(&nodes);
    }

    /// Task 3 (per-shard replication, minimal form): a shard backed by a Raft
    /// replication group keeps serving writes and retains its data when one
    /// **replica fails**. A 3-node group replicates the whole keyspace; killing a
    /// follower still leaves a 2/3 quorum, so the group keeps a leader and
    /// commits new writes, and the revived replica catches up. (The
    /// generalization — one such group *per shard* — is [`ShardedRaftCluster`].)
    #[test]
    fn replicated_shard_survives_a_replica_failure() {
        let nodes = build_cluster(3, test_cfg());

        // A first write replicates to all three replicas of the shard.
        let a = t(1, 2, 3);
        write_committed(&nodes, LogOp::Insert(a));
        assert!(
            wait_until(|| nodes.iter().all(|n| n.store_contains(a))),
            "initial write did not replicate to every replica\n{}",
            describe(&nodes)
        );

        // Fail one replica (a follower, not the leader).
        let l = stable_leader(&nodes);
        let f = (0..3).find(|&i| i != l).unwrap();
        nodes[f].kill();

        // The remaining 2/3 quorum keeps the shard available: it holds a leader
        // and commits a new write.
        let survivors: Vec<usize> = (0..3).filter(|&i| i != f).collect();
        let l2 = stable_leader(&nodes);
        assert!(survivors.contains(&l2));
        let b = t(4, 5, 6);
        write_committed(&nodes, LogOp::Insert(b));
        assert!(
            wait_until(|| survivors.iter().all(|&i| nodes[i].store_contains(b))),
            "post-failure write did not reach the surviving replicas\n{}",
            describe(&nodes)
        );

        // A surviving replica answers a read RPC over the wire (no data lost).
        let mut s = client(nodes[survivors[0]].local_addr().unwrap());
        let resp = crate::rpc::round_trip(&mut s, &Request::TripleCount).unwrap();
        assert!(
            matches!(resp, Response::Count(n) if n >= 2),
            "read served by survivor"
        );

        // The failed replica revives and catches up to both writes.
        nodes[f].revive();
        assert!(
            wait_until(|| nodes[f].store_contains(a) && nodes[f].store_contains(b)),
            "revived replica did not catch up\n{}",
            describe(&nodes)
        );

        shutdown_all(&nodes);
    }

    /// Task 1, end-to-end over TCP: a membership config-change submitted as a
    /// `ClientWrite` to the leader is committed through the Raft log and converges
    /// on every replica's [`ClusterConfig`]; a follower then serves the new
    /// membership over a `GetConfig` RPC.
    #[test]
    fn networked_membership_change_via_client_write_replicates() {
        use std::net::SocketAddr;
        let nodes = build_cluster(3, test_cfg());

        // Submit an AddNode config-change as a wire ClientWrite; the ack means
        // it is committed and applied on the acknowledging leader.
        let new_addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let (l, _) = client_write_committed(
            &nodes,
            LogOp::AddNode {
                id: 42,
                addr: new_addr,
            },
        );
        assert!(
            nodes[l].config().contains(42),
            "acknowledged ⇒ applied on the leader"
        );

        // Every replica converges on the new membership.
        assert!(
            wait_until(|| nodes.iter().all(|n| n.config().contains(42))),
            "membership did not replicate to every node\n{}",
            describe(&nodes)
        );

        // A follower serves the committed config over the wire.
        let f = (0..3).find(|&i| i != l).unwrap();
        let mut sf = client(nodes[f].local_addr().unwrap());
        let cfg =
            crate::rpc::round_trip(&mut sf, &Request::GetConfig).expect("GetConfig round-trip");
        match cfg {
            Response::Config {
                epoch,
                num_shards,
                members,
                assignment,
            } => {
                assert!(epoch >= 1, "a committed config-change bumps the epoch");
                assert!(
                    members.iter().any(|(id, a)| *id == 42 && *a == new_addr),
                    "the new member is present with its address"
                );
                assert_eq!(assignment.len(), num_shards as usize);
                // The lone member owns every bucket.
                assert!(assignment.iter().all(|&o| o == 42));
            }
            other => panic!("expected Config, got {other:?}"),
        }

        shutdown_all(&nodes);
    }
}

/// Pure, deterministic unit tests for the membership/assignment logic in
/// [`ClusterConfig`] (no clocks, sockets, or threads): rendezvous determinism,
/// minimal-movement rebalancing, and config-change application parity.
#[cfg(test)]
mod membership_tests {
    use super::*;
    use std::net::SocketAddr;

    fn addr(p: u16) -> SocketAddr {
        format!("127.0.0.1:{p}").parse().unwrap()
    }

    #[test]
    fn assignment_is_deterministic_total_and_member_owned() {
        let mut a = ClusterConfig::new(32);
        let mut b = ClusterConfig::new(32);
        // Insert in different orders: the assignment must still be identical.
        for id in [0u64, 1, 2, 3] {
            a.add_node(id, addr(7000 + id as u16));
        }
        for id in [3u64, 1, 0, 2] {
            b.add_node(id, addr(7000 + id as u16));
        }
        assert_eq!(
            a, b,
            "same member set => identical config regardless of order"
        );
        assert_eq!(a.assignment().len(), 32);
        for s in 0..32 {
            assert!(
                a.members().contains(&a.owner(s).unwrap()),
                "every bucket owned by a member"
            );
        }
        for id in 0..4u64 {
            assert!(
                a.assignment().contains(&id),
                "node {id} should own at least one of 32 buckets"
            );
        }
    }

    #[test]
    fn adding_a_node_moves_only_buckets_that_become_its_own() {
        let mut c = ClusterConfig::new(64);
        for id in 0..3u64 {
            c.add_node(id, addr(8000 + id as u16));
        }
        let before = c.assignment().to_vec();
        assert!(c.add_node(3, addr(8003)));
        let after = c.assignment().to_vec();
        assert_ne!(before, after, "the newcomer should take over some buckets");
        for s in 0..64 {
            if before[s] != after[s] {
                assert_eq!(after[s], 3, "a moved bucket must go to the new node");
            }
        }
        assert!(after.contains(&3));
    }

    #[test]
    fn removing_a_node_rehomes_only_its_buckets_with_no_orphans() {
        let mut c = ClusterConfig::new(64);
        for id in 0..4u64 {
            c.add_node(id, addr(9000 + id as u16));
        }
        let before = c.assignment().to_vec();
        assert!(c.remove_node(2));
        let after = c.assignment().to_vec();
        for s in 0..64 {
            if before[s] == 2 {
                assert_ne!(after[s], 2, "the removed node's bucket is re-homed");
            } else {
                assert_eq!(before[s], after[s], "an untouched bucket keeps its owner");
            }
        }
        assert!(
            !after.contains(&2),
            "no bucket may be owned by a removed node"
        );
        assert!(!c.contains(2));
    }

    #[test]
    fn apply_log_op_matches_direct_mutation_and_ignores_data_ops() {
        let mut viaop = ClusterConfig::new(16);
        let mut direct = ClusterConfig::new(16);
        let ops = [
            LogOp::AddNode {
                id: 5,
                addr: addr(1005),
            },
            LogOp::AddNode {
                id: 1,
                addr: addr(1001),
            },
            LogOp::RemoveNode { id: 5 },
            LogOp::Insert(IdTriple::new(1, 2, 3)), // data op: not a config change
            LogOp::Delete(IdTriple::new(1, 2, 3)),
        ];
        for op in ops {
            viaop.apply(&op);
        }
        direct.add_node(5, addr(1005));
        direct.add_node(1, addr(1001));
        direct.remove_node(5);
        assert_eq!(viaop, direct);
        assert_eq!(
            viaop.epoch(),
            3,
            "only the 3 membership changes bump the epoch"
        );
        assert_eq!(viaop.members(), vec![1]);
    }

    #[test]
    fn empty_config_routes_nothing() {
        let c = ClusterConfig::new(8);
        assert!(c.owner_of_subject(123).is_none());
        assert!(c.assignment().is_empty());
        assert_eq!(c.epoch(), 0);
    }
}

/// Networked, end-to-end tests for **elastic rebalancing**: real
/// [`ShardNode`] servers on ephemeral `127.0.0.1:0` ports in background threads,
/// with [`ShardCluster`] streaming shard buckets between owners over genuine TCP
/// when a node joins or leaves. These are deterministic despite using sockets —
/// every cluster operation is a synchronous request/response, so there is no
/// wall-clock timing to flake on.
#[cfg(test)]
mod shard_cluster_tests {
    use super::*;
    use crate::dict::Dictionary;
    use crate::model::Term;
    use crate::query::{QueryResult, ResultSet};

    fn fixture() -> (Dictionary, Vec<IdTriple>) {
        let mut d = Dictionary::new();
        let knows = d.intern_predicate(&Term::iri("http://ex/knows").dict_key());
        let typ = d.intern_predicate(&Term::iri("http://ex/type").dict_key());
        let name = d.intern_predicate(&Term::iri("http://ex/name").dict_key());
        let person = d.intern_term(&Term::iri("http://ex/Person"));
        let hub = d.intern_term(&Term::iri("http://ex/hub"));
        let names = [
            "alice", "bob", "carol", "dave", "eve", "frank", "grace", "heidi", "ivan", "judy",
        ];
        let people: Vec<EntityLiteralId> = names
            .iter()
            .map(|n| d.intern_term(&Term::iri(format!("http://ex/{n}"))))
            .collect();
        let mut triples = Vec::new();
        for (i, &p) in people.iter().enumerate() {
            triples.push(IdTriple::new(p, typ, person));
            triples.push(IdTriple::new(p, knows, hub));
            let next = people[(i + 1) % people.len()];
            triples.push(IdTriple::new(p, knows, next));
            let lit = d.intern_term(&Term::plain_literal(*names.get(i).unwrap()));
            triples.push(IdTriple::new(p, name, lit));
        }
        triples.push(IdTriple::new(hub, typ, person));
        triples.push(IdTriple::new(hub, knows, people[0]));
        (d, triples)
    }

    fn rows_sorted(rs: &ResultSet) -> Vec<Vec<Option<String>>> {
        let mut r = rs.rows.clone();
        r.sort();
        r
    }

    /// Start an empty in-process [`ShardNode`] on an ephemeral port; return its
    /// address and a [`Shard::Remote`] client to it.
    fn start_remote() -> (SocketAddr, Shard) {
        let node = Arc::new(ShardNode::bind(TripleStore::new(), "127.0.0.1:0").expect("bind"));
        let addr = node.local_addr().expect("addr");
        thread::spawn(move || node.serve_forever());
        (
            addr,
            Shard::Remote(RemoteShard::connect(addr).expect("connect")),
        )
    }

    fn endpoint_count(cluster: &ShardCluster, id: NodeId) -> u64 {
        match cluster.endpoint(id).expect("endpoint registered") {
            Shard::Remote(r) => r.triple_count().unwrap(),
            Shard::Local(s) => s.triple_count(),
        }
    }

    const BGP: &str = "SELECT ?x ?y WHERE { \
         ?x <http://ex/knows> ?y . \
         ?y <http://ex/type> <http://ex/Person> . \
         ?x <http://ex/name> ?n }";

    fn select_rows(rs: QueryResult) -> Vec<Vec<Option<String>>> {
        match rs {
            QueryResult::Select(r) => rows_sorted(&r),
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    /// Task 1+2: a node joins a running cluster and *receives its shards* over RPC.
    #[test]
    fn joining_node_receives_its_shards_over_rpc() {
        let (d, triples) = fixture();
        let base = ShardedStore::from_triples(1, triples.iter().copied());

        let mut cluster = ShardCluster::new(8);
        // Two founding members; the whole fixture routed across them.
        let (a0, e0) = start_remote();
        cluster.add_node(0, a0, e0).unwrap();
        let (a1, e1) = start_remote();
        cluster.add_node(1, a1, e1).unwrap();
        cluster.insert_all(triples.iter().copied()).unwrap();
        assert_eq!(
            cluster.try_triple_count().unwrap(),
            base.triple_count(),
            "all triples routed, none lost"
        );

        // A third node joins the running cluster.
        let (a2, e2) = start_remote();
        cluster.add_node(2, a2, e2).unwrap();

        // It now owns some buckets and has received their data over the wire.
        let owns: usize = (0..cluster.num_shards())
            .filter(|&s| cluster.config().owner(s) == Some(2))
            .count();
        assert!(owns > 0, "the joiner should own some buckets");
        assert!(
            endpoint_count(&cluster, 2) > 0,
            "the joiner received its shards"
        );

        // No data was lost or duplicated, and the distributed query is unchanged.
        assert_eq!(cluster.try_triple_count().unwrap(), base.triple_count());
        assert_eq!(
            select_rows(cluster.query(&d, BGP).unwrap()),
            select_rows(base.query(&d, BGP).unwrap()),
        );
    }

    /// Task 2: a node leaves and its shards migrate to survivors with no data loss.
    #[test]
    fn leaving_node_migrates_its_shards_with_no_data_loss() {
        let (d, triples) = fixture();
        let base = ShardedStore::from_triples(1, triples.iter().copied());

        let mut cluster = ShardCluster::new(8);
        for id in 0..3u64 {
            let (a, e) = start_remote();
            cluster.add_node(id, a, e).unwrap();
        }
        cluster.insert_all(triples.iter().copied()).unwrap();
        assert_eq!(cluster.try_triple_count().unwrap(), base.triple_count());

        // Node 1 leaves; its buckets move to survivors.
        cluster.remove_node(1).unwrap();
        assert_eq!(cluster.members(), vec![0, 2], "node 1 left the membership");
        for s in 0..cluster.num_shards() {
            assert_ne!(
                cluster.config().owner(s),
                Some(1),
                "no bucket left on a departed node"
            );
        }
        // No data lost and the query is still correct.
        assert_eq!(
            cluster.try_triple_count().unwrap(),
            base.triple_count(),
            "no data lost when a node leaves"
        );
        assert_eq!(
            select_rows(cluster.query(&d, BGP).unwrap()),
            select_rows(base.query(&d, BGP).unwrap()),
        );
    }

    /// Task 3: writes issued *after* a rebalance land on the new owner per the
    /// current assignment, and are queryable.
    #[test]
    fn writes_after_rebalance_land_on_the_new_owner_and_are_queryable() {
        let (mut d, triples) = fixture();

        let mut cluster = ShardCluster::new(8);
        for id in 0..2u64 {
            let (a, e) = start_remote();
            cluster.add_node(id, a, e).unwrap();
        }
        cluster.insert_all(triples.iter().copied()).unwrap();

        // Reshape the cluster: add a node, drop another — buckets reshuffle.
        let (a2, e2) = start_remote();
        cluster.add_node(2, a2, e2).unwrap();
        cluster.remove_node(0).unwrap();
        assert_eq!(cluster.members(), vec![1, 2]);

        // A fresh write routes to the bucket's *current* owner.
        let zoe = d.intern_term(&Term::iri("http://ex/zoe"));
        let knows = d
            .predicate_id(&Term::iri("http://ex/knows").dict_key())
            .unwrap();
        let alice = d.term_id(&Term::iri("http://ex/alice")).unwrap();
        let nt = IdTriple::new(alice, knows, zoe);
        let owner = cluster.owner_of_subject(alice).expect("a routable owner");
        assert!(cluster.insert(nt).unwrap(), "new edge inserted");

        // It landed on exactly the routed owner, and nowhere else.
        let holders: Vec<NodeId> = cluster
            .members()
            .into_iter()
            .filter(|&id| {
                cluster
                    .endpoint(id)
                    .unwrap()
                    .reader()
                    .read_exists(alice, knows, zoe)
                    .unwrap()
            })
            .collect();
        assert_eq!(holders, vec![owner], "the write lands on exactly its owner");

        // And it is visible to a distributed query.
        let ask = "ASK { <http://ex/alice> <http://ex/knows> <http://ex/zoe> }";
        assert!(
            matches!(cluster.query(&d, ask).unwrap(), QueryResult::Ask(true)),
            "post-rebalance write is queryable"
        );
    }

    /// Regression: a member whose endpoint is down must fail a cluster query
    /// (with an error naming it) instead of contributing an empty answer.
    #[test]
    fn cluster_query_errors_when_a_member_endpoint_is_down() {
        use super::test_support::Link;
        let (d, triples) = fixture();
        let base = ShardedStore::from_triples(1, triples.iter().copied());
        let expected = select_rows(base.query(&d, BGP).unwrap());

        let mut cluster = ShardCluster::new(8);
        let mut links = Vec::new();
        for id in 0..3u64 {
            let node = Arc::new(ShardNode::bind(TripleStore::new(), "127.0.0.1:0").unwrap());
            let backend = node.local_addr().unwrap();
            thread::spawn(move || node.serve_forever());
            let link = Link::to(backend);
            let ep = Shard::Remote(RemoteShard::connect(link.addr()).unwrap());
            cluster.add_node(id, link.addr(), ep).unwrap();
            links.push(link);
        }
        cluster.insert_all(triples.iter().copied()).unwrap();
        assert_eq!(select_rows(cluster.query(&d, BGP).unwrap()), expected);

        // Down a member that actually holds data.
        let victim = (0..3u64)
            .find(|&id| endpoint_count(&cluster, id) > 0)
            .unwrap();
        links[victim as usize].cut();
        let err = cluster
            .query(&d, BGP)
            .expect_err("a down member must fail the query");
        assert!(
            err.to_string()
                .contains(&links[victim as usize].addr().to_string()),
            "error should name the down member: {err}"
        );
        assert!(cluster.try_triple_count().is_err());

        links[victim as usize].restore();
        assert_eq!(select_rows(cluster.query(&d, BGP).unwrap()), expected);
        assert_eq!(cluster.try_triple_count().unwrap(), base.triple_count());
    }
}

/// Deterministic, in-process tests for the **per-shard replica groups** layer
/// ([`ReplicaGroup`] / [`ShardedRaftCluster`]): one Raft group per shard bucket,
/// each replicated `N`-ways. Every scenario drives the pure state machines by
/// hand (no threads, clocks, or sockets), so it is fully reproducible. Covers
/// (a) a shard surviving `floor((N-1)/2)` replica failures with a quorum still
/// serving, (b) a far-behind replica caught up by a snapshot install then
/// resuming incremental replication, (c) a rejoined replica converging by log
/// backfill, plus intra-group leader failover and scatter-gather read parity.
#[cfg(test)]
mod replicated_shard_tests {
    use super::*;
    use crate::dict::Dictionary;
    use crate::model::Term;
    use crate::query::{QueryResult, ResultSet};

    fn t(s: u32, p: u32, o: u32) -> IdTriple {
        IdTriple::new(s, p, o)
    }

    /// The same connected fixture graph as the other cluster tests, rebuilt here
    /// (each test module's `fixture` is private to it).
    fn fixture() -> (Dictionary, Vec<IdTriple>) {
        let mut d = Dictionary::new();
        let knows = d.intern_predicate(&Term::iri("http://ex/knows").dict_key());
        let typ = d.intern_predicate(&Term::iri("http://ex/type").dict_key());
        let name = d.intern_predicate(&Term::iri("http://ex/name").dict_key());
        let person = d.intern_term(&Term::iri("http://ex/Person"));
        let hub = d.intern_term(&Term::iri("http://ex/hub"));
        let names = [
            "alice", "bob", "carol", "dave", "eve", "frank", "grace", "heidi", "ivan", "judy",
        ];
        let people: Vec<EntityLiteralId> = names
            .iter()
            .map(|n| d.intern_term(&Term::iri(format!("http://ex/{n}"))))
            .collect();
        let mut triples = Vec::new();
        for (i, &p) in people.iter().enumerate() {
            triples.push(IdTriple::new(p, typ, person));
            triples.push(IdTriple::new(p, knows, hub));
            let next = people[(i + 1) % people.len()];
            triples.push(IdTriple::new(p, knows, next));
            let lit = d.intern_term(&Term::plain_literal(*names.get(i).unwrap()));
            triples.push(IdTriple::new(p, name, lit));
        }
        triples.push(IdTriple::new(hub, typ, person));
        triples.push(IdTriple::new(hub, knows, people[0]));
        (d, triples)
    }

    fn single(triples: &[IdTriple]) -> TripleStore {
        let mut s = TripleStore::new();
        s.bulk_load(triples.iter().copied());
        s
    }

    fn rows_sorted(rs: &ResultSet) -> Vec<Vec<Option<String>>> {
        let mut r = rs.rows.clone();
        r.sort();
        r
    }

    const BGP: &str = "SELECT ?x ?y WHERE { \
         ?x <http://ex/knows> ?y . \
         ?y <http://ex/type> <http://ex/Person> . \
         ?x <http://ex/name> ?n }";

    fn select_rows(rs: QueryResult) -> Vec<Vec<Option<String>>> {
        match rs {
            QueryResult::Select(r) => rows_sorted(&r),
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    /// Evaluate a SELECT against a plain [`TripleStore`] baseline (which has no
    /// `query` convenience) via the generic engine, returning its sorted rows.
    fn base_select(d: &Dictionary, store: &TripleStore, q: &str) -> Vec<Vec<Option<String>>> {
        let parsed = sparql::parse(q).unwrap();
        select_rows(Evaluator::new(d, store).evaluate(&parsed).unwrap())
    }

    /// A follower replica of `shard` other than its current leader.
    fn a_follower(cluster: &ShardedRaftCluster, shard: usize) -> NodeId {
        let leader = cluster.group(shard).leader().expect("a leader");
        cluster
            .group(shard)
            .replicas()
            .iter()
            .copied()
            .find(|&r| r != leader)
            .expect("a follower")
    }

    /// Every access pattern of a sharded-and-replicated cluster (4 buckets × 3
    /// replicas) matches a single [`TripleStore`] over the same triples — the
    /// replication and the scatter-gather are both transparent.
    #[test]
    fn scatter_gather_over_replica_groups_matches_single_store() {
        let (d, triples) = fixture();
        let base = single(&triples);

        let mut cluster = ShardedRaftCluster::new(4, 3, RaftConfig::default());
        cluster.bootstrap();
        let added = cluster.insert_all(triples.iter().copied());
        assert_eq!(
            added,
            base.triple_count(),
            "all triples committed, none lost"
        );
        // A duplicate routed insert is idempotent (already committed).
        assert_eq!(cluster.insert(triples[0]), Some(false));

        assert_eq!(
            cluster.try_triple_count().unwrap(),
            base.triple_count(),
            "triple_count"
        );
        assert_eq!(
            cluster.try_distinct_subjects().unwrap(),
            base.distinct_subjects(),
            "distinct_subjects"
        );
        assert_eq!(
            cluster.try_distinct_objects().unwrap(),
            base.distinct_objects(),
            "distinct_objects"
        );
        assert_eq!(
            cluster.try_num_predicates().unwrap(),
            base.num_predicates(),
            "num_predicates"
        );
        assert_eq!(
            cluster.try_iter_all().unwrap(),
            base.iter_all().collect::<Vec<_>>(),
            "iter_all"
        );

        // Per-predicate and subject-/object-rooted accesses.
        let mut ids: Vec<EntityLiteralId> = base.subject_keys().collect();
        ids.extend(base.object_keys());
        ids = sort_dedup(ids);
        for &p in &base.predicates().collect::<Vec<_>>() {
            assert_eq!(cluster.try_so_by_p(p).unwrap(), base.so_by_p(p).to_vec());
            assert_eq!(cluster.try_subs_by_p(p).unwrap(), base.subs_by_p(p));
            assert_eq!(cluster.try_objs_by_p(p).unwrap(), base.objs_by_p(p));
            assert_eq!(cluster.try_pred_card(p).unwrap(), base.pred_card(p));
        }
        for &s in &ids {
            assert_eq!(cluster.try_po_by_s(s).unwrap(), base.po_by_s(s).to_vec());
            assert_eq!(cluster.try_ps_by_o(s).unwrap(), base.ps_by_o(s).to_vec());
        }

        // A multi-pattern BGP evaluated by the same engine yields identical rows.
        assert_eq!(
            select_rows(cluster.query(&d, BGP).unwrap()),
            base_select(&d, &base, BGP),
        );
    }

    /// (a) A shard tolerates `floor((N-1)/2)` replica failures: with N=5 the
    /// group keeps a 3/5 quorum after 2 failures and still commits + serves;
    /// a 3rd failure loses quorum, so nothing new commits.
    #[test]
    fn shard_survives_n_over_2_replica_failures_with_quorum() {
        let mut cluster = ShardedRaftCluster::with_compaction(1, 5, RaftConfig::default(), 0);
        cluster.bootstrap();
        let leader = cluster.group(0).leader().unwrap();
        assert_eq!(cluster.insert(t(1, 2, 3)), Some(true));

        // Fail floor((5-1)/2) = 2 followers; a 3/5 quorum remains.
        let followers: Vec<NodeId> = cluster
            .group(0)
            .replicas()
            .iter()
            .copied()
            .filter(|&r| r != leader)
            .collect();
        cluster.set_replica_down(0, followers[0], true);
        cluster.set_replica_down(0, followers[1], true);

        // The shard still commits a new write on the survivors, and serves reads.
        assert_eq!(cluster.insert(t(4, 5, 6)), Some(true));
        assert_eq!(
            cluster.group(0).triple_count().unwrap(),
            2,
            "quorum committed the write"
        );
        assert!(cluster.try_exists(4, 5, 6).unwrap());
        assert_eq!(cluster.group(0).leader(), Some(leader), "leader retained");

        // A 3rd failure drops below majority: the leader appends but cannot
        // commit, so nothing new is applied.
        cluster.set_replica_down(0, followers[2], true);
        let before = cluster.group(0).state(leader).commit_index();
        let appended = cluster.group(0).state(leader).log_len();
        // review cluster-nlqa#12: this asserted `Some(true)` — "committed",
        // per insert's docs — for exactly this uncommitted write (the bug of
        // cluster-nlqa#4). An uncommitted write reports `None` (outcome
        // unknown); that the leader still appends it is checked directly.
        assert_eq!(
            cluster.insert(t(7, 8, 9)),
            None,
            "an uncommitted write is not reported committed"
        );
        assert_eq!(
            cluster.group(0).state(leader).log_len(),
            appended + 1,
            "leader still appends"
        );
        let after = cluster.group(0).state(leader).commit_index();
        assert_eq!(before, after, "no quorum => nothing new commits");
        assert!(
            !cluster.try_exists(7, 8, 9).unwrap(),
            "an uncommitted write is never applied"
        );
    }

    /// (b) A replica that falls behind the leader's compaction point is caught up
    /// by a snapshot install, then resumes *incremental* log replication for the
    /// tail — not an unbounded replay.
    #[test]
    fn lagging_replica_catches_up_via_snapshot_then_incremental() {
        // A small compaction threshold forces the leader to snapshot while the
        // follower is offline.
        let mut cluster = ShardedRaftCluster::with_compaction(1, 3, RaftConfig::default(), 2);
        cluster.bootstrap();
        let leader = cluster.group(0).leader().unwrap();
        let follower = a_follower(&cluster, 0);

        // Take a follower down, then commit several writes on the 2/3 survivors.
        cluster.set_replica_down(0, follower, true);
        let triples = [t(1, 9, 1), t(2, 9, 2), t(3, 9, 3), t(5, 9, 5), t(6, 9, 6)];
        for &tr in &triples {
            assert_eq!(cluster.insert(tr), Some(true));
        }
        // The leader compacted past index 0 while the follower was away.
        let base = cluster.group(0).state(leader).base_index();
        assert!(
            base > 0,
            "leader compacted its committed log into a snapshot"
        );
        assert_eq!(
            cluster.group(0).store(follower).triple_count(),
            0,
            "the offline replica missed every write"
        );

        // Revive the follower and let the leader drive catch-up. The only way
        // past the compaction point is an InstallSnapshot, then the tail entries.
        cluster.set_replica_down(0, follower, false);
        cluster.group_mut(0).settle(5);
        assert_eq!(
            cluster.group(0).store(follower).triple_count(),
            triples.len() as u64,
            "follower recovered its full state via snapshot + tail"
        );
        assert_eq!(
            cluster.group(0).state(follower).base_index(),
            base,
            "follower adopted the leader's snapshot base"
        );
        for &tr in &triples {
            assert!(cluster
                .group(0)
                .store(follower)
                .exists(tr.sub, tr.pred, tr.obj));
        }

        // A subsequent write reaches the recovered follower by ordinary
        // incremental replication (no further snapshot needed).
        assert_eq!(cluster.insert(t(7, 9, 7)), Some(true));
        cluster.group_mut(0).settle(2);
        assert!(
            cluster.group(0).store(follower).exists(7, 9, 7),
            "incremental replication resumes after the snapshot install"
        );
    }

    /// (c) A rejoined replica converges by log backfill when the leader has NOT
    /// compacted past its next index (auto-compaction disabled).
    #[test]
    fn rejoined_replica_converges_by_log_backfill() {
        let mut cluster = ShardedRaftCluster::with_compaction(1, 3, RaftConfig::default(), 0);
        cluster.bootstrap();
        let follower = a_follower(&cluster, 0);

        cluster.set_replica_down(0, follower, true);
        let triples = [t(1, 1, 1), t(2, 2, 2), t(3, 3, 3)];
        for &tr in &triples {
            assert_eq!(cluster.insert(tr), Some(true));
        }
        assert_eq!(cluster.group(0).store(follower).triple_count(), 0);

        cluster.set_replica_down(0, follower, false);
        cluster.group_mut(0).settle(5);
        assert_eq!(
            cluster.group(0).store(follower).triple_count(),
            3,
            "rejoined replica caught up"
        );
        assert_eq!(
            cluster.group(0).state(follower).base_index(),
            0,
            "caught up by backfill, not a snapshot"
        );
        for &tr in &triples {
            assert!(cluster
                .group(0)
                .store(follower)
                .exists(tr.sub, tr.pred, tr.obj));
        }
    }

    /// Leader failover *within a shard group*: the leader dies, a survivor is
    /// elected in a higher term, keeps serving writes, and the old leader —
    /// once revived — steps down and converges on the new leader's data.
    #[test]
    fn leader_failover_within_a_shard_group_keeps_data_consistent() {
        let mut cluster = ShardedRaftCluster::with_compaction(1, 3, RaftConfig::default(), 0);
        cluster.bootstrap();
        let old = cluster.group(0).leader().unwrap();
        assert_eq!(cluster.insert(t(1, 2, 3)), Some(true));
        let old_term = cluster.group(0).state(old).current_term();

        // Kill the leader; elect a survivor.
        cluster.set_replica_down(0, old, true);
        let survivor = cluster
            .group(0)
            .replicas()
            .iter()
            .copied()
            .find(|&r| r != old)
            .unwrap();
        cluster.group_mut(0).elect(survivor);
        assert_eq!(cluster.group(0).leader(), Some(survivor));
        assert!(
            cluster.group(0).state(survivor).current_term() > old_term,
            "a failover bumps the term"
        );

        // The shard still accepts writes on the new leader.
        assert_eq!(cluster.insert(t(4, 5, 6)), Some(true));

        // The old leader restarts, steps down, and catches up to both writes.
        cluster.set_replica_down(0, old, false);
        cluster.group_mut(0).settle(8);
        assert_eq!(
            cluster.group(0).state(old).role(),
            Role::Follower,
            "the stale leader steps down"
        );
        assert!(cluster.group(0).store(old).exists(1, 2, 3));
        assert!(cluster.group(0).store(old).exists(4, 5, 6));
        assert_eq!(cluster.group(0).store(old).triple_count(), 2);
    }

    /// A killed leader that REVIVES while the newer leader is down must not be
    /// picked as leader (it never heard an AppendEntries to step down through the
    /// normal path). Otherwise it would shadow a committed write and acknowledge
    /// a write that can never commit. (Regression: note_revived stepping down.)
    #[test]
    fn revived_stale_leader_does_not_shadow_a_committed_write() {
        let mut c = ShardedRaftCluster::with_compaction(1, 3, RaftConfig::default(), 0);
        c.bootstrap();
        let old = c.group(0).leader().unwrap();
        c.set_replica_down(0, old, true);
        let survivor = c
            .group(0)
            .replicas()
            .iter()
            .copied()
            .find(|&r| r != old)
            .unwrap();
        c.group_mut(0).elect(survivor);
        assert_eq!(c.insert(t(4, 5, 6)), Some(true)); // committed in the higher term

        // Old leader revives (steps down) while the new leader dies: no
        // AppendEntries will reach it, so only note_revived can demote it.
        c.set_replica_down(0, old, false);
        c.set_replica_down(0, survivor, true);
        assert_ne!(
            c.group(0).leader(),
            Some(old),
            "a revived stale leader must not lead"
        );

        // Failover to the most up-to-date survivor still sees the committed write.
        let leader = c.group_mut(0).ensure_leader();
        assert!(
            c.group(0).store(leader).exists(4, 5, 6),
            "committed write must survive the failover"
        );
    }

    /// `ensure_leader` must elect an *electable* (up-to-date) up replica, not the
    /// lowest-id one — a stale rejoined replica can never win a vote, so electing
    /// it would spin and panic even though a quorum is up. (Regression #13.)
    #[test]
    fn ensure_leader_elects_an_up_to_date_replica_not_a_stale_one() {
        let mut c = ShardedRaftCluster::with_compaction(1, 3, RaftConfig::default(), 0);
        c.bootstrap();
        assert_eq!(c.group(0).leader(), Some(0));
        c.set_replica_down(0, 1, true); // node 1 misses the commit
        assert_eq!(c.insert(t(7, 8, 9)), Some(true)); // commits on {0, 2}
        c.set_replica_down(0, 0, true); // kill leader
        c.set_replica_down(0, 1, false); // revive the stale node 1
                                         // up = {1 (stale), 2 (has the entry)} — node 2 must be elected.
        let leader = c.group_mut(0).ensure_leader();
        assert_eq!(leader, 2, "the up-to-date replica is elected, no panic");
        assert!(c.group(0).store(2).exists(7, 8, 9));
    }

    /// `remove` on a leaderless group must report `None` ("no leader"), not
    /// `Some(false)` ("absent"), for a committed triple — the latter is a
    /// silently-lost delete the caller never retries. (Regression #14.)
    #[test]
    fn remove_reports_no_leader_not_false_for_a_committed_triple() {
        let mut c = ShardedRaftCluster::with_compaction(1, 3, RaftConfig::default(), 0);
        c.bootstrap();
        c.set_replica_down(0, 1, true);
        let tr = t(7, 8, 9);
        assert_eq!(c.insert(tr), Some(true)); // committed on {0, 2}
        c.set_replica_down(0, 0, true); // kill leader
        c.set_replica_down(0, 1, false); // revive stale node 1 (empty store)
        assert_eq!(c.group(0).leader(), None, "group is leaderless");
        assert_eq!(
            c.remove(tr),
            None,
            "no leader ⇒ None, not a spurious 'absent'"
        );
    }

    /// Reads stay correct after one replica per bucket fails: each bucket keeps a
    /// quorum, so the scatter-gather is still served and matches a single store.
    #[test]
    fn reads_served_after_one_replica_per_bucket_fails() {
        let (d, triples) = fixture();
        let base = single(&triples);

        let mut cluster = ShardedRaftCluster::new(4, 3, RaftConfig::default());
        cluster.bootstrap();
        cluster.insert_all(triples.iter().copied());

        for g in 0..cluster.num_shards() {
            let f = a_follower(&cluster, g);
            cluster.set_replica_down(g, f, true);
        }

        assert_eq!(
            cluster.try_triple_count().unwrap(),
            base.triple_count(),
            "no data lost when a replica per bucket fails"
        );
        assert_eq!(
            cluster.try_iter_all().unwrap(),
            base.iter_all().collect::<Vec<_>>(),
        );
        assert_eq!(
            select_rows(cluster.query(&d, BGP).unwrap()),
            base_select(&d, &base, BGP),
        );
    }

    /// Regression: a bucket with *every* replica down used to be skipped by the
    /// scatter-gather, so its triples silently vanished from query answers. The
    /// read must fail instead, and recover as soon as one replica is back.
    #[test]
    fn query_errors_when_every_replica_of_a_bucket_is_down() {
        let (d, triples) = fixture();
        let base = single(&triples);
        let expected = base_select(&d, &base, BGP);

        let mut cluster = ShardedRaftCluster::new(4, 3, RaftConfig::default());
        cluster.bootstrap();
        cluster.insert_all(triples.iter().copied());
        assert_eq!(select_rows(cluster.query(&d, BGP).unwrap()), expected);

        // Pick a bucket that holds data, then fail all of its replicas.
        let g = (0..cluster.num_shards())
            .find(|&g| cluster.group(g).triple_count().unwrap() > 0)
            .unwrap();
        let replicas = cluster.group(g).replicas().to_vec();
        for &r in &replicas {
            cluster.set_replica_down(g, r, true);
        }

        let err = cluster
            .query(&d, BGP)
            .expect_err("an unavailable bucket must fail the query");
        assert!(
            err.to_string().contains(&format!("shard bucket {g}")),
            "error should name the unavailable bucket: {err}"
        );
        assert!(cluster.try_triple_count().is_err());
        assert!(cluster.try_iter_all().is_err());
        assert!(
            cluster.group(g).triple_count().is_err(),
            "not a misleading 0"
        );

        // One replica back (a committed-prefix follower read) restores the
        // complete answer.
        cluster.set_replica_down(g, replicas[0], false);
        assert_eq!(select_rows(cluster.query(&d, BGP).unwrap()), expected);
        assert_eq!(cluster.try_triple_count().unwrap(), base.triple_count());
    }

    /// A committed delete replicates across a bucket's replica group.
    #[test]
    fn delete_replicates_across_the_group() {
        let mut cluster = ShardedRaftCluster::with_compaction(1, 3, RaftConfig::default(), 0);
        cluster.bootstrap();
        let a = t(1, 2, 3);
        assert_eq!(cluster.insert(a), Some(true));
        for &r in cluster.group(0).replicas() {
            assert!(cluster.group(0).store(r).exists(1, 2, 3));
        }
        assert_eq!(cluster.remove(a), Some(true));
        for &r in cluster.group(0).replicas() {
            assert!(
                !cluster.group(0).store(r).exists(1, 2, 3),
                "replica {r} should have applied the delete"
            );
        }
        assert_eq!(cluster.remove(a), Some(false), "removing an absent triple");
    }
}

/// Regression tests for the review findings fixed in this module (review
/// cluster-nlqa#1-#4, #9-#12): each one reproduces the reported failure
/// through the public API and checks the fixed behaviour. Failures are
/// injected deterministically — a [`test_support::Link`] that dies after an
/// exact number of RPCs, a replica marked down, a forged message — and wall
/// clocks only bound the waits (hang guards), never decide an assertion.
#[cfg(test)]
mod review_fix_tests {
    use super::test_support::Link;
    use super::*;
    use std::time::Instant;

    /// Upper bound on any wait in this module: a hang guard, not an assertion.
    const HANG_GUARD: Duration = Duration::from_secs(30);

    fn t(s: u32, p: u32, o: u32) -> IdTriple {
        IdTriple::new(s, p, o)
    }

    /// A placeholder member address for an in-process (`Shard::Local`)
    /// endpoint: the config needs one, nothing ever connects to it.
    fn local_addr(host: u8) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, host], 0))
    }

    fn local_shard(triples: &[IdTriple]) -> Shard {
        let mut s = TripleStore::new();
        s.bulk_load(triples.iter().copied());
        Shard::Local(s)
    }

    /// A [`ShardNode`] holding `initial` on an ephemeral port, served from a
    /// background thread; returns its address.
    fn start_shard(initial: &[IdTriple]) -> SocketAddr {
        let mut store = TripleStore::new();
        store.bulk_load(initial.iter().copied());
        let node = ShardNode::bind(store, "127.0.0.1:0").expect("bind shard node");
        let addr = node.local_addr().expect("shard addr");
        thread::spawn(move || node.serve_forever());
        addr
    }

    fn remote(addr: SocketAddr) -> Shard {
        Shard::Remote(RemoteShard::connect(addr).expect("resolve shard addr"))
    }

    /// Whether member `id`'s own endpoint holds `tr`.
    fn holds(cluster: &ShardCluster, id: NodeId, tr: IdTriple) -> bool {
        cluster
            .endpoint(id)
            .expect("member endpoint")
            .reader()
            .read_exists(tr.sub, tr.pred, tr.obj)
            .expect("endpoint answers")
    }

    // ----- review cluster-nlqa#1: retry-safe migrations -----------------------

    /// The reviewer's experiment: node 0 holds 400 triples, node 1 joins over
    /// a link that dies after 30 RPCs. The retry of the join must finish the
    /// stranded buckets, and a later leave of node 0 must lose nothing (before
    /// the fix the retry was a silent no-op, the count read 430 and the leave
    /// dropped ~65 triples, every call returning `Ok`).
    #[test]
    fn a_failed_join_is_resumed_by_its_retry_and_a_later_leave_loses_nothing() {
        let triples: Vec<IdTriple> = (1..=400).map(|s| t(s, 7, s + 1000)).collect();
        let mut cluster = ShardCluster::new(16);
        cluster
            .add_node(0, local_addr(1), local_shard(&triples))
            .unwrap();

        let link = Link::to(start_shard(&[]));
        link.fail_after(30);
        let joined = cluster.add_node(1, link.addr(), remote(link.addr()));
        assert!(joined.is_err(), "the link dies mid-migration: {joined:?}");

        link.restore();
        cluster
            .add_node(1, link.addr(), remote(link.addr()))
            .expect("the retry resumes the stranded buckets");
        assert_eq!(
            cluster.try_triple_count().unwrap(),
            400,
            "no triple lost or counted twice after the retried join"
        );
        for &tr in &triples {
            let owner = cluster.owner_of_subject(tr.sub).unwrap();
            for id in cluster.members() {
                assert_eq!(
                    holds(&cluster, id, tr),
                    id == owner,
                    "{tr:?} must live on its owner {owner} only (checked node {id})"
                );
            }
        }

        cluster.remove_node(0).expect("node 0 leaves");
        assert_eq!(cluster.members(), vec![1]);
        assert_eq!(cluster.try_triple_count().unwrap(), 400);
        for &tr in &triples {
            assert!(
                cluster.try_exists(tr.sub, tr.pred, tr.obj).unwrap(),
                "{tr:?} lost by the leave"
            );
        }
    }

    /// While a failed join leaves buckets stranded, reads stay exact (the
    /// copied triples sit on both nodes but are counted once), and every other
    /// membership change is refused without changing anything — before the
    /// fix `remove_node(0)` went ahead and dropped the stranded triples.
    #[test]
    fn stranded_buckets_block_other_membership_changes_until_resumed() {
        let triples: Vec<IdTriple> = (1..=400).map(|s| t(s, 7, s + 1000)).collect();
        let mut cluster = ShardCluster::new(16);
        cluster
            .add_node(0, local_addr(1), local_shard(&triples))
            .unwrap();
        let link = Link::to(start_shard(&[]));
        link.fail_after(30);
        assert!(cluster
            .add_node(1, link.addr(), remote(link.addr()))
            .is_err());
        let stranded = cluster.stranded_buckets();
        assert!(!stranded.is_empty());
        assert!(stranded.iter().all(|&(_, from, to)| (from, to) == (0, 1)));

        // The link is still down: nothing else may change.
        assert!(
            cluster.remove_node(0).is_err(),
            "node 0 may hold the only copy"
        );
        assert!(cluster
            .add_node(2, local_addr(2), Shard::Local(TripleStore::new()))
            .is_err());
        assert_eq!(cluster.members(), vec![0, 1]);
        assert_eq!(cluster.stranded_buckets(), stranded);

        link.restore();
        assert_eq!(
            cluster.try_triple_count().unwrap(),
            400,
            "copies counted once"
        );
        assert_eq!(cluster.try_pred_card(7).unwrap(), 400);
        assert_eq!(cluster.try_iter_all().unwrap().len(), 400);

        cluster.resume_migrations().expect("the link is back");
        assert!(cluster.stranded_buckets().is_empty());
        assert_eq!(cluster.try_triple_count().unwrap(), 400);
        cluster.remove_node(0).expect("nothing stranded any more");
        assert_eq!(cluster.try_triple_count().unwrap(), 400);
    }

    /// A leave interrupted mid-migration keeps the leaving node's endpoint —
    /// reads still see everything — until a retry has drained it.
    #[test]
    fn an_interrupted_leave_keeps_the_leaving_endpoint_until_it_is_drained() {
        let triples: Vec<IdTriple> = (1..=200).map(|s| t(s, 3, s)).collect();
        let link = Link::to(start_shard(&[]));
        let mut cluster = ShardCluster::new(8);
        cluster
            .add_node(0, link.addr(), remote(link.addr()))
            .unwrap();
        cluster
            .add_node(1, local_addr(1), Shard::Local(TripleStore::new()))
            .unwrap();
        cluster.insert_all(triples.iter().copied()).unwrap();
        let held_by_0 = triples.iter().filter(|tr| holds(&cluster, 0, **tr)).count();
        assert!(
            held_by_0 > 10,
            "node 0 owns some buckets ({held_by_0} triples)"
        );

        link.fail_after(5);
        assert!(cluster.remove_node(0).is_err());
        assert_eq!(
            cluster.members(),
            vec![1],
            "writes no longer route to node 0"
        );
        assert!(cluster.endpoint(0).is_some(), "but its endpoint is kept");
        assert!(!cluster.stranded_buckets().is_empty());

        link.restore();
        assert_eq!(cluster.try_triple_count().unwrap(), 200);
        cluster.remove_node(0).expect("the retry drains node 0");
        assert!(cluster.endpoint(0).is_none(), "drained, then dropped");
        assert!(cluster.stranded_buckets().is_empty());
        assert_eq!(cluster.try_triple_count().unwrap(), 200);
        for &tr in &triples {
            assert!(holds(&cluster, 1, tr), "{tr:?} reached the survivor");
        }
    }

    /// The last member cannot leave while it holds triples: they would have
    /// nowhere to go (before, its endpoint was dropped with them).
    #[test]
    fn the_last_member_holding_data_cannot_leave() {
        let mut cluster = ShardCluster::new(4);
        cluster
            .add_node(0, local_addr(1), local_shard(&[t(1, 2, 3)]))
            .unwrap();
        let err = cluster.remove_node(0).expect_err("it holds a triple");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert_eq!(cluster.members(), vec![0]);
        assert_eq!(cluster.try_triple_count().unwrap(), 1);

        let mut empty = ShardCluster::new(4);
        empty
            .add_node(0, local_addr(1), Shard::Local(TripleStore::new()))
            .unwrap();
        empty
            .remove_node(0)
            .expect("an empty last member may leave");
        assert!(empty.members().is_empty());
    }

    // ----- review cluster-nlqa#10: re-adding a member -------------------------

    /// Re-adding an existing member must not replace (and drop) its endpoint:
    /// before the fix a second `add_node(1, .., Local(empty))` silently dropped
    /// the member's 10 triples. A re-add naming a different address is not a
    /// retry and is refused instead of leaving config and endpoint disagreeing.
    #[test]
    fn re_adding_a_member_keeps_its_endpoint_and_its_data() {
        let mut cluster = ShardCluster::new(4);
        cluster
            .add_node(1, local_addr(1), Shard::Local(TripleStore::new()))
            .unwrap();
        for i in 0..10 {
            assert!(cluster.insert(t(i, 100, i + 1)).unwrap());
        }
        cluster
            .add_node(1, local_addr(1), Shard::Local(TripleStore::new()))
            .expect("re-adding with the same address is an idempotent retry");
        assert_eq!(cluster.try_triple_count().unwrap(), 10, "member data kept");

        let err = cluster
            .add_node(1, local_addr(2), Shard::Local(TripleStore::new()))
            .expect_err("a different address is not a retry");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert_eq!(cluster.config().addr(1), Some(local_addr(1)));
        assert_eq!(cluster.try_triple_count().unwrap(), 10);
    }

    /// A re-add whose `Local` endpoint holds triples is refused rather than
    /// silently discarding them.
    #[test]
    fn re_adding_a_member_with_a_seeded_local_shard_is_refused() {
        let mut cluster = ShardCluster::new(4);
        cluster
            .add_node(1, local_addr(1), Shard::Local(TripleStore::new()))
            .unwrap();
        let err = cluster
            .add_node(1, local_addr(1), local_shard(&[t(5, 6, 7)]))
            .expect_err("the seeded triples would be discarded");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert_eq!(cluster.try_triple_count().unwrap(), 0);
    }

    // ----- review cluster-nlqa#9 (+ sec#19, conc#11, robust#15) ---------------

    /// A [`ShardNode`] disconnects a client that stays idle, or stalls inside
    /// a frame, past its I/O timeout; before, such a connection held its
    /// thread forever.
    #[test]
    fn a_shard_node_disconnects_idle_and_stalled_clients() {
        use std::io::{Read, Write};
        let node = ShardNode::bind(TripleStore::new(), "127.0.0.1:0")
            .unwrap()
            .with_io_timeout(Duration::from_millis(100));
        let addr = node.local_addr().unwrap();
        thread::spawn(move || node.serve_forever());

        let mut idle = TcpStream::connect(addr).unwrap();
        idle.set_read_timeout(Some(HANG_GUARD)).unwrap();
        assert_eq!(
            idle.read(&mut [0u8; 1])
                .expect("closed by the node, not timed out here"),
            0
        );

        // Half a frame: the length prefix announces 10 bytes, one arrives.
        let mut stalled = TcpStream::connect(addr).unwrap();
        stalled.set_read_timeout(Some(HANG_GUARD)).unwrap();
        stalled.write_all(&[0, 0, 0, 10, 1]).unwrap();
        assert_eq!(stalled.read(&mut [0u8; 1]).expect("closed by the node"), 0);

        // An active client is served as before.
        let rs = RemoteShard::connect(addr).unwrap();
        assert_eq!(rs.triple_count().unwrap(), 0);
    }

    /// A reply larger than the frame limit is answered with an error that
    /// says so, and the connection stays usable; before, the node built the
    /// reply, failed to send it and dropped the connection, so the client saw
    /// a reset, retried, and reported a generic I/O error.
    #[test]
    fn an_oversized_shard_reply_is_reported_not_dropped() {
        let triples: Vec<IdTriple> = (1..=100).map(|s| t(s, 1, s)).collect();
        let mut store = TripleStore::new();
        store.bulk_load(triples.iter().copied());
        let mut node = ShardNode::bind(store, "127.0.0.1:0").unwrap();
        node.reply_limit = 64; // 100 subject ids need 405 bytes
        let addr = node.local_addr().unwrap();
        thread::spawn(move || node.serve_forever());

        let rs = RemoteShard::connect(addr).unwrap();
        let err = rs.read_subject_keys().expect_err("the reply is too large");
        assert!(err.to_string().contains("exceeds"), "{err}");
        assert_eq!(
            rs.triple_count().unwrap(),
            100,
            "the connection still serves"
        );
    }

    // ----- review cluster-nlqa#2: forged and overflowing terms ----------------

    /// A forged `RequestVote` at `u64::MAX` must not be adopted: before the fix
    /// the node granted it, jumped to the last representable term, and the next
    /// election overflowed (`attempt to add with overflow` in debug builds, a
    /// wrap to 0 in release).
    #[test]
    fn a_forged_max_term_vote_is_refused_and_elections_continue() {
        let cfg = RaftConfig::default();
        let mut a = RaftState::new(0, vec![1, 2], &cfg);
        let reply = a.handle(RaftMsg::RequestVote {
            term: u64::MAX,
            candidate_id: 99,
            last_log_index: u64::MAX,
            last_log_term: u64::MAX,
        });
        assert_eq!(
            reply,
            RaftReply::Vote {
                term: 0,
                granted: false
            }
        );
        assert_eq!(a.current_term(), 0, "the forged term is not adopted");

        let mut outs = Vec::new();
        for _ in 0..1000 {
            outs = a.tick();
            if !outs.is_empty() {
                break;
            }
        }
        assert_eq!(a.current_term(), 1, "the node still campaigns normally");
        assert!(matches!(outs[0].msg, RaftMsg::RequestVote { term: 1, .. }));

        // Forged AppendEntries / InstallSnapshot at the maximum are refused too.
        let reply = a.handle(RaftMsg::AppendEntries {
            term: u64::MAX,
            leader_id: 99,
            prev_log_index: 0,
            prev_log_term: 0,
            leader_commit: 0,
            entries: vec![],
        });
        assert!(
            matches!(
                reply,
                RaftReply::Append {
                    term: 1,
                    success: false,
                    ..
                }
            ),
            "{reply:?}"
        );
        let reply = a.handle(RaftMsg::InstallSnapshot {
            term: u64::MAX,
            leader_id: 99,
            last_included_index: 5,
            last_included_term: u64::MAX,
            triples: vec![t(1, 2, 3)],
        });
        assert_eq!(reply, RaftReply::Snapshot { term: 1 });
        assert_eq!(a.current_term(), 1);
        assert_eq!(a.base_index(), 0, "the forged snapshot was not installed");
    }

    /// Term arithmetic is checked: a node already at the largest term stops
    /// campaigning instead of panicking (debug) or wrapping to 0 (release).
    #[test]
    fn a_node_at_the_largest_term_does_not_overflow() {
        let cfg = RaftConfig::default();
        let mut a = RaftState::new(0, vec![1, 2], &cfg);
        a.current_term = u64::MAX;
        for _ in 0..1000 {
            assert!(a.tick().is_empty(), "no election past the last term");
        }
        assert_eq!(a.current_term(), u64::MAX, "never wraps");
        assert_ne!(a.role(), Role::Leader);
    }

    /// A lying `AppendAck` (match index `u64::MAX`) must not wrap the leader's
    /// `next_index` or commit beyond its own log.
    #[test]
    fn a_lying_append_ack_cannot_wrap_next_index() {
        let cfg = RaftConfig::default();
        let mut a = RaftState::new(0, vec![1, 2], &cfg);
        while a.role() != Role::Candidate {
            a.tick();
        }
        let term = a.current_term();
        let _ = a.handle_reply(
            1,
            RaftReply::Vote {
                term,
                granted: true,
            },
        );
        assert!(a.is_leader());
        a.propose(LogOp::Insert(t(1, 2, 3))).unwrap();
        for success in [true, false] {
            let _ = a.handle_reply(
                1,
                RaftReply::Append {
                    term,
                    success,
                    match_index: u64::MAX,
                },
            );
            assert!(a.commit_index() <= a.log_len(), "commit past the log");
            assert!(a.next_index.values().all(|&n| n <= a.log_len() + 1));
        }
    }

    /// A forged snapshot claiming the last representable index is refused;
    /// before the fix it was installed and the node's next election overflowed
    /// computing `last_log_index + 1`.
    #[test]
    fn a_forged_snapshot_index_is_refused() {
        let cfg = RaftConfig::default();
        let mut a = RaftState::new(0, vec![], &cfg);
        let _ = a.handle(RaftMsg::InstallSnapshot {
            term: 1,
            leader_id: 9,
            last_included_index: u64::MAX,
            last_included_term: 1,
            triples: vec![t(1, 2, 3)],
        });
        assert_eq!(a.base_index(), 0, "the forged snapshot was not installed");
        for _ in 0..1000 {
            a.tick();
            if a.is_leader() {
                break;
            }
        }
        assert!(a.is_leader(), "the single node still elects itself");
        assert!(a.propose(LogOp::Insert(t(4, 5, 6))).is_some());
    }

    /// A single-node [`ClusterNode`] on an ephemeral port with fast ticks,
    /// shut down when dropped (also when the test fails).
    struct LiveNode(Arc<ClusterNode>);

    impl LiveNode {
        fn start(store: TripleStore) -> LiveNode {
            let cfg = RaftConfig {
                election_timeout_min: 3,
                election_timeout_max: 6,
                heartbeat_timeout: 1,
                tick_interval: Duration::from_millis(5),
            };
            let node = ClusterNode::bind(0, vec![], store, "127.0.0.1:0", cfg).expect("bind");
            let _ = node.start();
            LiveNode(node)
        }

        fn addr(&self) -> SocketAddr {
            self.0.local_addr().unwrap()
        }

        /// Wait (bounded by the hang guard) until the node leads.
        fn wait_leader(&self) {
            let start = Instant::now();
            while !self.0.is_leader() {
                assert!(start.elapsed() < HANG_GUARD, "no leader");
                thread::sleep(Duration::from_millis(2));
            }
        }
    }

    impl Drop for LiveNode {
        fn drop(&mut self) {
            self.0.shutdown();
        }
    }

    /// The reviewer's wire experiment on a live node: one forged max-term
    /// `RequestVote` used to panic the tick driver and poison the state mutex
    /// (every later call panicked) — the node must keep serving writes.
    #[test]
    fn a_forged_max_term_vote_does_not_wedge_a_live_cluster_node() {
        let node = LiveNode::start(TripleStore::new());
        node.wait_leader();
        let mut s = TcpStream::connect(node.addr()).unwrap();
        s.set_read_timeout(Some(HANG_GUARD)).unwrap();
        let reply = rpc::round_trip(
            &mut s,
            &Request::RequestVote {
                term: u64::MAX,
                candidate_id: 99,
                last_log_index: 0,
                last_log_term: 0,
            },
        )
        .unwrap();
        assert!(
            matches!(reply, Response::Vote { granted: false, term } if term < u64::MAX),
            "{reply:?}"
        );
        let start = Instant::now();
        loop {
            match node
                .0
                .propose_and_wait(LogOp::Insert(t(1, 2, 3)), Duration::from_secs(2))
            {
                Ok(_) => break,
                Err(e) => assert!(start.elapsed() < HANG_GUARD, "never committed: {e}"),
            }
        }
        assert!(node.0.current_term() < u64::MAX);
        assert!(node.0.store_contains(t(1, 2, 3)));
    }

    // ----- review cluster-nlqa#3: shard RPCs sent to a ClusterNode ------------

    /// A [`RemoteShard`] pointed at a [`ClusterNode`]: its routed insert goes
    /// through the Raft log and answers `Bool` as the [`RemoteShard`] contract
    /// says (before the fix: `Err("expected a Bool response")` for a committed
    /// write), and a `PurgeShard` — which would delete data on this replica
    /// only, outside the log — is refused.
    #[test]
    fn a_cluster_node_honours_the_shard_insert_contract_and_refuses_purges() {
        let node = LiveNode::start(TripleStore::new());
        node.wait_leader();
        let rs = RemoteShard::connect(node.addr()).unwrap();
        let tr = t(11, 22, 33);
        assert!(rs.insert(tr).unwrap(), "a new triple is reported new");
        assert!(
            !rs.insert(tr).unwrap(),
            "a present triple is reported present"
        );
        assert!(node.0.store_contains(tr));

        let commit = node.0.commit_index();
        for s in 0..4 {
            assert!(
                rs.purge_shard(s, 4).is_err(),
                "PurgeShard bypasses the replicated log and must be refused"
            );
        }
        assert!(node.0.store_contains(tr), "the committed triple survives");
        assert_eq!(node.0.commit_index(), commit);
        assert!(
            rs.read_exists(11, 22, 33).unwrap(),
            "reads are still served"
        );
    }

    /// Every request kind sent to a [`ClusterNode`] is either served from the
    /// replicated store (reads), routed through the Raft log (writes), or
    /// refused — none changes the store outside the log.
    #[test]
    fn every_request_sent_to_a_cluster_node_reads_goes_through_the_log_or_is_refused() {
        let mut seed = TripleStore::new();
        seed.bulk_load([t(1, 2, 3), t(4, 2, 6)]);
        let node = LiveNode::start(seed);
        node.wait_leader();
        let mut s = TcpStream::connect(node.addr()).unwrap();
        s.set_read_timeout(Some(HANG_GUARD)).unwrap();
        let mut ask = |req: Request| rpc::round_trip(&mut s, &req).expect("round trip");

        // Reads.
        assert_eq!(
            ask(Request::Exists {
                sub: 1,
                pred: 2,
                obj: 3
            }),
            Response::Bool(true)
        );
        assert_eq!(
            ask(Request::PoByS { sub: 1 }),
            Response::Pairs(vec![(2, 3)])
        );
        assert_eq!(
            ask(Request::OBySp { sub: 1, pred: 2 }),
            Response::Ids(vec![3])
        );
        assert_eq!(
            ask(Request::PBySo { sub: 1, obj: 3 }),
            Response::Ids(vec![2])
        );
        assert_eq!(
            ask(Request::PsByO { obj: 3 }),
            Response::Pairs(vec![(2, 1)])
        );
        assert_eq!(
            ask(Request::SByPo { pred: 2, obj: 3 }),
            Response::Ids(vec![1])
        );
        assert_eq!(
            ask(Request::SoByP { pred: 2 }),
            Response::Pairs(vec![(1, 3), (4, 6)])
        );
        assert_eq!(ask(Request::SubsByP { pred: 2 }), Response::Ids(vec![1, 4]));
        assert_eq!(ask(Request::ObjsByP { pred: 2 }), Response::Ids(vec![3, 6]));
        assert_eq!(ask(Request::SubjectKeys), Response::Ids(vec![1, 4]));
        assert_eq!(ask(Request::ObjectKeys), Response::Ids(vec![3, 6]));
        assert_eq!(ask(Request::Predicates), Response::Ids(vec![2]));
        assert_eq!(ask(Request::TripleCount), Response::Count(2));
        assert_eq!(ask(Request::PredCard { pred: 2 }), Response::Count(2));
        assert!(matches!(
            ask(Request::ScanShard { shard: 0, num_shards: 1 }),
            Response::Triples(v) if v.len() == 2
        ));
        assert!(matches!(
            ask(Request::ClusterStatus),
            Response::Status { role: 2, .. }
        ));
        assert!(matches!(ask(Request::GetConfig), Response::Config { .. }));

        // Stale Raft traffic is answered without effect.
        assert!(matches!(
            ask(Request::RequestVote {
                term: 0,
                candidate_id: 9,
                last_log_index: 0,
                last_log_term: 0
            }),
            Response::Vote { granted: false, .. }
        ));
        assert!(matches!(
            ask(Request::AppendEntries {
                term: 0,
                leader_id: 9,
                prev_log_index: 0,
                prev_log_term: 0,
                leader_commit: 0,
                entries: vec![LogEntry::new(0, LogOp::Delete(t(1, 2, 3)))],
            }),
            Response::AppendAck { success: false, .. }
        ));
        assert!(matches!(
            ask(Request::InstallSnapshot {
                term: 0,
                leader_id: 9,
                last_included_index: 99,
                last_included_term: 0,
                triples: vec![],
            }),
            Response::SnapshotAck { .. }
        ));
        assert!(
            node.0.is_leader(),
            "stale traffic does not depose the leader"
        );

        // Writes go through the log.
        let commit = node.0.commit_index();
        assert_eq!(
            ask(Request::Insert {
                sub: 7,
                pred: 2,
                obj: 8
            }),
            Response::Bool(true)
        );
        assert!(matches!(
            ask(Request::ClientWrite {
                op: LogOp::Delete(t(4, 2, 6))
            }),
            Response::WriteAck { ok: true, .. }
        ));
        assert_eq!(node.0.commit_index(), commit + 2, "both writes were logged");

        // Bucket purges are refused.
        assert!(matches!(
            ask(Request::PurgeShard {
                shard: 0,
                num_shards: 1
            }),
            Response::Error(_)
        ));
        assert_eq!(node.0.commit_index(), commit + 2);
        assert!(node.0.store_contains(t(1, 2, 3)));
        assert!(node.0.store_contains(t(7, 2, 8)));
        assert!(!node.0.store_contains(t(4, 2, 6)));
        assert_eq!(node.0.store_triple_count(), 2);
    }

    // ----- review cluster-nlqa#4: commit reporting ----------------------------

    /// With both followers down the leader can append but never commit:
    /// `insert_all` used to report 5 triples "newly committed" while the
    /// cluster held none. Writes that cannot commit are reported as such
    /// (`None`: outcome unknown, retry), and commit once a quorum is back.
    #[test]
    fn writes_without_a_quorum_are_not_reported_committed() {
        let mut c = ShardedRaftCluster::with_compaction(1, 3, RaftConfig::default(), 0);
        c.bootstrap();
        let leader = c.group(0).leader().unwrap();
        assert_eq!(
            c.insert(t(100, 1, 1)),
            Some(true),
            "committed with a quorum"
        );
        let followers: Vec<NodeId> = c
            .group(0)
            .replicas()
            .iter()
            .copied()
            .filter(|&r| r != leader)
            .collect();
        for &f in &followers {
            c.set_replica_down(0, f, true);
        }

        let n = c.insert_all((1..=5).map(|s| t(s, 2, 3)));
        assert_eq!(n, 0, "nothing was committed");
        assert_eq!(c.try_triple_count().unwrap(), 1);
        assert_eq!(c.insert(t(9, 9, 9)), None, "an uncommitted insert");
        assert_eq!(
            c.remove(t(100, 1, 1)),
            None,
            "a delete that cannot commit is not reported done"
        );
        assert!(c.try_exists(100, 1, 1).unwrap());

        // A quorum returns: the appended entries commit (which is why the
        // outcome was "unknown", not "failed").
        for &f in &followers {
            c.set_replica_down(0, f, false);
        }
        c.settle(5);
        assert_eq!(c.try_triple_count().unwrap(), 6);
        assert!(!c.try_exists(100, 1, 1).unwrap());
    }

    // ----- review cluster-nlqa#11: durable term, vote and log -----------------

    /// A unique scratch directory, removed when dropped (also on failure).
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            use std::sync::atomic::AtomicUsize;
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "gstore_cluster_review_{}_{}_{tag}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn vote(node: &ClusterNode, term: u64, candidate: NodeId) -> Response {
        node.handle_request(Request::RequestVote {
            term,
            candidate_id: candidate,
            last_log_index: 0,
            last_log_term: 0,
        })
    }

    /// The double vote the reviewer reproduced: a replica that voted in term
    /// 5 and restarts must not vote for another candidate in term 5. With
    /// [`ClusterNode::bind_persistent`] the vote survives the restart; the
    /// in-memory [`ClusterNode::bind`] shows the hazard it closes.
    #[test]
    fn a_persistent_replica_keeps_its_vote_across_a_restart() {
        let dir = TempDir::new("vote");
        let cfg = RaftConfig::default();
        let node = ClusterNode::bind_persistent(2, vec![0, 1], &dir.0, "127.0.0.1:0", cfg.clone())
            .unwrap();
        assert_eq!(
            vote(&node, 5, 0),
            Response::Vote {
                term: 5,
                granted: true
            }
        );
        drop(node); // never started: no threads hold it

        let node = ClusterNode::bind_persistent(2, vec![0, 1], &dir.0, "127.0.0.1:0", cfg.clone())
            .unwrap();
        assert_eq!(node.current_term(), 5, "the term survived");
        assert_eq!(
            vote(&node, 5, 1),
            Response::Vote {
                term: 5,
                granted: false
            },
            "no second vote in term 5"
        );
        assert_eq!(
            vote(&node, 5, 0),
            Response::Vote {
                term: 5,
                granted: true
            }
        );

        // An in-memory replica restarted the same way votes twice.
        let fresh =
            ClusterNode::bind(2, vec![0, 1], TripleStore::new(), "127.0.0.1:0", cfg).unwrap();
        assert_eq!(
            vote(&fresh, 5, 1),
            Response::Vote {
                term: 5,
                granted: true
            }
        );
    }

    /// The log is durable too: entries a replica acknowledged — including a
    /// conflicting suffix it later replaced — come back exactly after a
    /// restart, and a restarted single-node cluster re-commits them and
    /// rebuilds its store.
    #[test]
    fn a_persistent_replica_recovers_its_log_and_rebuilds_its_store() {
        let dir = TempDir::new("log");
        let cfg = RaftConfig::default();
        let follower =
            ClusterNode::bind_persistent(1, vec![0], &dir.0, "127.0.0.1:0", cfg.clone()).unwrap();
        let append = |term, prev_log_index, prev_log_term, entries| {
            follower.handle_request(Request::AppendEntries {
                term,
                leader_id: 0,
                prev_log_index,
                prev_log_term,
                leader_commit: 0,
                entries,
            })
        };
        let e = |term, s| LogEntry::new(term, LogOp::Insert(t(s, 1, 1)));
        assert!(matches!(
            append(1, 0, 0, vec![e(1, 1), e(1, 2), e(1, 3)]),
            Response::AppendAck { success: true, .. }
        ));
        // A new leader of term 2 overwrites index 3 onwards.
        assert!(matches!(
            append(2, 2, 1, vec![e(2, 30), e(2, 40)]),
            Response::AppendAck { success: true, .. }
        ));
        let expected = follower.state.lock().unwrap().log.clone();
        assert_eq!(expected, vec![e(1, 1), e(1, 2), e(2, 30), e(2, 40)]);
        drop(follower);

        // Restarted alone (as a single-node cluster) it elects itself,
        // re-commits the recovered log and rebuilds the store from it.
        let node = ClusterNode::bind_persistent(1, vec![], &dir.0, "127.0.0.1:0", cfg).unwrap();
        assert_eq!(node.state.lock().unwrap().log, expected, "the log survived");
        assert_eq!(node.current_term(), 2);
        let live = LiveNode(node);
        let _ = live.0.start();
        live.wait_leader();
        let start = Instant::now();
        while live.0.store_triple_count() < 4 {
            assert!(start.elapsed() < HANG_GUARD, "store not rebuilt");
            thread::sleep(Duration::from_millis(2));
        }
        for s in [1, 2, 30, 40] {
            assert!(live.0.store_contains(t(s, 1, 1)));
        }
        assert!(
            !live.0.store_contains(t(3, 1, 1)),
            "the replaced entry is gone"
        );
    }

    /// A torn record at the journal's tail (a crash mid-append) is cut off,
    /// a state directory of another node id is refused, and so is a second
    /// copy of the same node while the first holds the journal.
    #[test]
    fn the_raft_journal_drops_a_torn_tail_and_refuses_a_foreign_or_second_node() {
        use std::io::Write;
        let dir = TempDir::new("journal");
        let cfg = RaftConfig::default();
        let node =
            ClusterNode::bind_persistent(3, vec![4], &dir.0, "127.0.0.1:0", cfg.clone()).unwrap();
        assert!(matches!(
            vote(&node, 7, 4),
            Response::Vote { granted: true, .. }
        ));
        drop(node);
        let path = dir.0.join(RaftJournal::FILE);
        let intact = std::fs::metadata(&path).unwrap().len();
        // Half of a record: a length prefix announcing 50 bytes, 3 present.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(&[0, 0, 0, 50, 1, 2, 3, 4, 9, 9, 9]).unwrap();
        drop(f);

        let node =
            ClusterNode::bind_persistent(3, vec![4], &dir.0, "127.0.0.1:0", cfg.clone()).unwrap();
        assert_eq!(node.current_term(), 7);
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            intact,
            "torn tail cut"
        );
        assert!(matches!(
            vote(&node, 7, 5),
            Response::Vote { granted: false, .. }
        ));
        drop(node);

        let err = ClusterNode::bind_persistent(9, vec![4], &dir.0, "127.0.0.1:0", cfg.clone())
            .expect_err("node 3's state directory");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");

        // One replica, one journal: a second copy of node 3 on the same
        // directory is refused while the first one runs.
        let first =
            ClusterNode::bind_persistent(3, vec![4], &dir.0, "127.0.0.1:0", cfg.clone()).unwrap();
        let err = ClusterNode::bind_persistent(3, vec![4], &dir.0, "127.0.0.1:0", cfg.clone())
            .expect_err("the journal is held");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock, "{err}");
        drop(first);
        ClusterNode::bind_persistent(3, vec![4], &dir.0, "127.0.0.1:0", cfg)
            .expect("released when the first node is gone");
    }

    /// Replace node `node`'s journal file handle: a read-only handle makes
    /// every journal write fail (an injected disk failure), an append handle
    /// makes them work again (the disk recovered).
    fn swap_journal_file(node: &ClusterNode, file: std::fs::File) {
        node.journal
            .as_ref()
            .expect("a persistent node")
            .lock()
            .unwrap()
            .file = file;
    }

    /// review cluster-nlqa#11 (reviewer follow-up): once a journal write has
    /// failed the node answers nothing more, even after the disk recovers.
    /// The failed batch was already handed out by `take_durable_delta`, so
    /// before this a retried vote request (same candidate, same term) found
    /// nothing new to write and was *granted* — from a vote that never
    /// reached the disk, which a restart forgets: a second vote in that term.
    #[test]
    fn a_vote_that_never_reached_the_journal_is_never_granted() {
        let dir = TempDir::new("failstop_vote");
        let cfg = RaftConfig::default();
        let node = ClusterNode::bind_persistent(2, vec![0, 1], &dir.0, "127.0.0.1:0", cfg.clone())
            .unwrap();
        let path = dir.0.join(RaftJournal::FILE);
        assert_eq!(
            vote(&node, 5, 0),
            Response::Vote {
                term: 5,
                granted: true
            }
        );

        // The disk fails while the vote for candidate 1 in term 6 is written.
        swap_journal_file(&node, std::fs::File::open(&path).unwrap());
        assert!(
            matches!(vote(&node, 6, 1), Response::Error(_)),
            "a vote that could not be made durable is not granted"
        );
        assert!(!node.is_alive(), "the node stopped");
        node.revive();
        assert!(!node.is_alive(), "and stays stopped");

        // The disk recovers; the candidate retries.
        swap_journal_file(
            &node,
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
        );
        assert!(
            matches!(vote(&node, 6, 1), Response::Error(_)),
            "the retried vote must not be granted from state that is not on disk"
        );
        drop(node);

        // What a restart sees is the last durable state, and the journal
        // still opens: nothing was written after the failure.
        let node = ClusterNode::bind_persistent(2, vec![0, 1], &dir.0, "127.0.0.1:0", cfg).unwrap();
        assert_eq!(node.current_term(), 5);
    }

    /// A single-node leader counts an entry committed as it appends it, before
    /// the journal write. When that write fails, the entry must never reach the
    /// store — not through a concurrent waiter draining the log, and not
    /// through a later proposal once the disk works again (which, before, was
    /// accepted, applied the lost entry along with its own, and left a journal
    /// with a hole in its log).
    #[test]
    fn a_failed_journal_write_stops_a_single_node_leader_before_anything_is_applied() {
        let dir = TempDir::new("failstop_leader");
        let cfg = RaftConfig::default();
        let node =
            ClusterNode::bind_persistent(1, vec![], &dir.0, "127.0.0.1:0", cfg.clone()).unwrap();
        let path = dir.0.join(RaftJournal::FILE);
        {
            // Elect it by hand (no driver thread), journaling the new term.
            let mut st = node.state.lock().unwrap();
            while !st.is_leader() {
                st.tick();
            }
            node.persist(&mut st).unwrap();
        }
        let (a, b, c) = (t(1, 1, 1), t(2, 2, 2), t(3, 3, 3));
        node.propose_and_wait(LogOp::Insert(a), HANG_GUARD).unwrap();
        assert!(node.store_contains(a));

        swap_journal_file(&node, std::fs::File::open(&path).unwrap());
        assert!(node.propose(LogOp::Insert(b)).is_err(), "b is not durable");
        node.apply_actions(); // what a concurrent waiter's drain does
        assert!(!node.store_contains(b), "a lost write reached the store");

        swap_journal_file(
            &node,
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
        );
        assert!(
            node.propose(LogOp::Insert(c)).is_err(),
            "a stopped node accepts no more writes"
        );
        assert!(!node.store_contains(b));
        assert!(!node.store_contains(c));
        drop(node);

        let node = ClusterNode::bind_persistent(1, vec![], &dir.0, "127.0.0.1:0", cfg).unwrap();
        let log: Vec<LogOp> = node
            .state
            .lock()
            .unwrap()
            .log
            .iter()
            .map(|e| e.op)
            .collect();
        assert_eq!(
            log,
            vec![LogOp::Insert(a)],
            "the journal holds exactly what was durable"
        );
    }

    /// An intact journal record that this build cannot decode — a newer
    /// format, a damaged file — is refused, not cut off with everything after
    /// it (which silently forgot acknowledged votes); a zero-filled tail left
    /// by a crash is still cut as torn.
    #[test]
    fn an_intact_journal_record_that_does_not_decode_is_refused_not_cut() {
        use std::io::Write;
        let dir = TempDir::new("undecodable");
        let cfg = RaftConfig::default();
        let node =
            ClusterNode::bind_persistent(3, vec![4], &dir.0, "127.0.0.1:0", cfg.clone()).unwrap();
        assert!(matches!(
            vote(&node, 7, 4),
            Response::Vote { granted: true, .. }
        ));
        drop(node);
        let path = dir.0.join(RaftJournal::FILE);
        let intact = std::fs::metadata(&path).unwrap().len();
        let append = |bytes: &[u8]| {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(bytes).unwrap();
        };

        // A whole record (its CRC matches) with a tag this build does not know.
        let payload = [9u8, 1, 2, 3];
        let mut record = (payload.len() as u32).to_be_bytes().to_vec();
        record.extend_from_slice(&crc32(&payload).to_be_bytes());
        record.extend_from_slice(&payload);
        append(&record);
        let grown = std::fs::metadata(&path).unwrap().len();
        let err = ClusterNode::bind_persistent(3, vec![4], &dir.0, "127.0.0.1:0", cfg.clone())
            .expect_err("an intact record that does not decode is refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            grown,
            "nothing was cut"
        );

        // Without it, a zero-filled tail (the file grew, its bytes never
        // landed) is torn: cut, and the node starts with its vote.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(intact)
            .unwrap();
        append(&[0u8; 24]);
        let node = ClusterNode::bind_persistent(3, vec![4], &dir.0, "127.0.0.1:0", cfg).unwrap();
        assert_eq!(node.current_term(), 7);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), intact);
        assert!(matches!(
            vote(&node, 7, 5),
            Response::Vote { granted: false, .. }
        ));
    }

    /// review cluster-nlqa#11 (round-5 review follow-up): damage in the middle
    /// of the journal is not a torn tail. Before, replay stopped at the first
    /// record whose checksum failed and cut the file there, with every later,
    /// acknowledged record: after one flipped bit in node 3's vote of term 7
    /// it restarted at term 0 and granted a second vote in term 8. Now the
    /// start is refused and the file left as it is — for a flipped bit (its
    /// bytes all there: no crash leaves that, not even in the last record)
    /// and for a zero-filled record that a later batch's record follows. What
    /// a crash in the last batch leaves (a record zero-filled, a later record
    /// of the same batch intact) is still cut.
    #[test]
    fn a_damaged_journal_record_before_acknowledged_ones_refuses_the_start() {
        let dir = TempDir::new("damaged_middle");
        let cfg = RaftConfig::default();
        let bind =
            || ClusterNode::bind_persistent(3, vec![4, 5], &dir.0, "127.0.0.1:0", cfg.clone());
        let node = bind().unwrap();
        for (term, candidate) in [(7, 4), (8, 5)] {
            assert_eq!(
                vote(&node, term, candidate),
                Response::Vote {
                    term,
                    granted: true
                }
            );
        }
        drop(node);
        let path = dir.0.join(RaftJournal::FILE);
        let intact = std::fs::read(&path).unwrap();
        // The header, then two 26-byte hard-state records.
        let (first, second) = (RaftJournal::HEADER_LEN, RaftJournal::HEADER_LEN + 26);
        assert_eq!(intact.len(), second + 26);

        let refused = |damaged: &[u8], at: usize, what: &str| {
            std::fs::write(&path, damaged).unwrap();
            let err = bind().expect_err(what);
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{what}: {err}");
            assert!(
                err.to_string()
                    .contains(&format!("the record at byte {at} is damaged")),
                "{what}: {err}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                damaged,
                "{what}: the journal is left untouched"
            );
        };
        // One flipped bit in the first record's term.
        let mut flipped = intact.clone();
        flipped[first + 8 + 4] ^= 0x10;
        refused(&flipped, first, "a flipped bit before an acknowledged vote");
        // The first record zero-filled (a lost block), the term-8 vote intact:
        // a hard-state record starts a batch, so it was written later.
        let mut zeroed = intact.clone();
        zeroed[first..second].fill(0);
        refused(
            &zeroed,
            first,
            "a zero-filled record before an acknowledged vote",
        );
        // A flipped bit in the last record: its bytes are all there.
        let mut last = intact.clone();
        last[second + 8 + 4] ^= 0x10;
        refused(&last, second, "a flipped bit in the last record");

        // The intact journal still opens with both votes.
        std::fs::write(&path, &intact).unwrap();
        let node = bind().unwrap();
        assert_eq!(node.current_term(), 8);
        assert!(matches!(
            vote(&node, 8, 4),
            Response::Vote { granted: false, .. }
        ));
        drop(node);

        // A crash in a last batch of [hard state, log suffix]: the hard-state
        // record's sector never landed (zeros), the log suffix's did. Nothing
        // of that batch was acknowledged, so the whole batch is cut.
        let mut batch = RaftJournal::batch(&DurableDelta {
            hard: Some((9, Some(4))),
            snapshot: None,
            entries: Some((1, vec![LogEntry::new(9, LogOp::Insert(t(1, 2, 3)))])),
        });
        batch[..26].fill(0);
        let mut torn = intact.clone();
        torn.extend_from_slice(&batch);
        std::fs::write(&path, &torn).unwrap();
        let node = bind().expect("a torn last batch is cut");
        assert_eq!(node.current_term(), 8);
        assert!(node.state.lock().unwrap().log.is_empty());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            intact,
            "the torn batch is cut"
        );
    }

    /// A new state directory is created durably: the parent of every
    /// directory `bind_persistent` creates is synced, so a power loss cannot
    /// take the directory — and with it the journal and its vote — away.
    #[test]
    fn a_new_state_directory_is_created_durably() {
        use crate::db::fsutil::DURABLE_DIR_PARENT_SYNCS;
        let root = TempDir::new("mkdir");
        let dir = root.0.join("a").join("b");
        DURABLE_DIR_PARENT_SYNCS.with(|s| s.borrow_mut().clear());
        let node =
            ClusterNode::bind_persistent(1, vec![], &dir, "127.0.0.1:0", RaftConfig::default())
                .unwrap();
        drop(node);
        let synced = DURABLE_DIR_PARENT_SYNCS.with(|s| std::mem::take(&mut *s.borrow_mut()));
        assert_eq!(
            synced,
            vec![root.0.join("a"), root.0.clone(), std::env::temp_dir()],
            "the parent of each created directory, deepest first"
        );
    }
}
