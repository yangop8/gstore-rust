//! A sharded / distributed-query core: an **in-process** scatter-gather
//! [`TripleSource`] over `N` partitioned [`TripleStore`]s.
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
//!   answers per-shard RPCs over TCP (the [`crate::bin::gnode`] binary is a thin
//!   wrapper around it; see `src/bin/gnode.rs`).
//! * [`RemoteShard`] — a TCP client to a [`ShardNode`], speaking the
//!   length-prefixed binary protocol in [`crate::rpc`].
//! * [`NetworkShardedStore`] — the distributed analogue of [`ShardedStore`]: a
//!   `Vec<`[`Shard`]`>` mixing **local** in-process shards and **remote** ones,
//!   running the *same* scatter-gather merge (so it is observationally identical
//!   to a single [`TripleStore`]) plus **routed inserts** (each triple is placed
//!   on `hash(subject) % N`, shipped over RPC for a remote shard).
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
//!   ([`LogOp::AddNode`](crate::rpc::LogOp::AddNode) /
//!   [`LogOp::RemoveNode`](crate::rpc::LogOp::RemoveNode)): committed by quorum and
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
//!
//! Remote reads are *best-effort* — a failed shard RPC is logged and treated as
//! empty rather than aborting the whole query (the trait methods cannot return
//! errors); routed [`NetworkShardedStore::insert`] / [`ShardCluster::insert`] do
//! surface I/O errors to the caller.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::dict::Dictionary;
use crate::error::Result;
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
#[derive(Debug)]
pub struct ShardNode {
    store: Arc<Mutex<TripleStore>>,
    listener: TcpListener,
}

impl ShardNode {
    /// Bind to `addr` (e.g. `"127.0.0.1:0"` for an ephemeral port), taking
    /// ownership of this node's local shard.
    pub fn bind<A: ToSocketAddrs>(store: TripleStore, addr: A) -> io::Result<ShardNode> {
        let listener = TcpListener::bind(addr)?;
        Ok(ShardNode {
            store: Arc::new(Mutex::new(store)),
            listener,
        })
    }

    /// The actual bound address (useful after binding to port 0).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept connections forever, handling each on its own thread.
    pub fn serve_forever(&self) {
        for stream in self.listener.incoming().flatten() {
            let store = Arc::clone(&self.store);
            thread::spawn(move || serve_connection(store, stream));
        }
    }
}

/// Serve one client connection: decode framed [`Request`]s and reply with framed
/// [`Response`]s until the peer closes or an I/O error occurs.
fn serve_connection(store: Arc<Mutex<TripleStore>>, mut stream: TcpStream) {
    loop {
        let bytes = match rpc::read_message(&mut stream) {
            Ok(b) => b,
            Err(_) => return, // peer closed or framing error
        };
        let resp = match Request::decode(&bytes) {
            Ok(req) => dispatch(&store, &req),
            Err(e) => Response::Error(e.to_string()),
        };
        if rpc::write_message(&mut stream, &resp.encode()).is_err() {
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
#[derive(Debug)]
pub struct RemoteShard {
    addr: SocketAddr,
    conn: Mutex<Option<TcpStream>>,
}

impl RemoteShard {
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
            conn: Mutex::new(None),
        })
    }

    /// The node address this client targets.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Round-trip one request, opening the connection if needed and retrying
    /// once on a fresh connection if the existing one is broken.
    fn call(&self, req: &Request) -> io::Result<Response> {
        let mut guard = self.conn.lock().unwrap();
        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(TcpStream::connect(self.addr)?);
            }
            let stream = guard.as_mut().unwrap();
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

    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> io::Result<bool> {
        self.call(&Request::Exists { sub, pred, obj })?.into_bool()
    }
    fn po_by_s(&self, sub: EntityLiteralId) -> io::Result<Vec<(u32, u32)>> {
        self.call(&Request::PoByS { sub })?.into_pairs()
    }
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> io::Result<Vec<u32>> {
        self.call(&Request::OBySp { sub, pred })?.into_ids()
    }
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> io::Result<Vec<u32>> {
        self.call(&Request::PBySo { sub, obj })?.into_ids()
    }
    fn ps_by_o(&self, obj: EntityLiteralId) -> io::Result<Vec<(u32, u32)>> {
        self.call(&Request::PsByO { obj })?.into_pairs()
    }
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> io::Result<Vec<u32>> {
        self.call(&Request::SByPo { pred, obj })?.into_ids()
    }
    fn so_by_p(&self, pred: PredId) -> io::Result<Vec<(u32, u32)>> {
        self.call(&Request::SoByP { pred })?.into_pairs()
    }
    fn subs_by_p(&self, pred: PredId) -> io::Result<Vec<u32>> {
        self.call(&Request::SubsByP { pred })?.into_ids()
    }
    fn objs_by_p(&self, pred: PredId) -> io::Result<Vec<u32>> {
        self.call(&Request::ObjsByP { pred })?.into_ids()
    }
    fn subject_keys(&self) -> io::Result<Vec<u32>> {
        self.call(&Request::SubjectKeys)?.into_ids()
    }
    fn object_keys(&self) -> io::Result<Vec<u32>> {
        self.call(&Request::ObjectKeys)?.into_ids()
    }
    fn predicates(&self) -> io::Result<Vec<u32>> {
        self.call(&Request::Predicates)?.into_ids()
    }
    fn triple_count(&self) -> io::Result<u64> {
        self.call(&Request::TripleCount)?.into_count()
    }
    fn pred_card(&self, pred: PredId) -> io::Result<u64> {
        self.call(&Request::PredCard { pred })?.into_count()
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

/// One shard of a [`NetworkShardedStore`]: an **in-process** [`TripleStore`] or
/// a **remote** [`RemoteShard`] reached over TCP. The read helpers return plain
/// values (collapsing a remote I/O error to an empty / zero result, logged to
/// stderr) so they slot directly into the scatter-gather merge.
#[derive(Debug)]
pub enum Shard {
    /// An in-process shard.
    Local(TripleStore),
    /// A shard served by a remote [`ShardNode`].
    Remote(RemoteShard),
}

/// Log a best-effort scatter-gather drop of a remote shard (the trait methods
/// cannot return errors, so a failed RPC degrades to an empty answer).
fn warn_remote(r: &RemoteShard, op: &str, e: &io::Error) {
    eprintln!(
        "gstore::cluster: best-effort scatter-gather dropped shard {} on {op}: {e}",
        r.addr()
    );
}

impl Shard {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        match self {
            Shard::Local(s) => s.exists(sub, pred, obj),
            Shard::Remote(r) => r.exists(sub, pred, obj).unwrap_or_else(|e| {
                warn_remote(r, "exists", &e);
                false
            }),
        }
    }
    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        match self {
            Shard::Local(s) => s.po_by_s(sub).to_vec(),
            Shard::Remote(r) => r.po_by_s(sub).unwrap_or_else(|e| {
                warn_remote(r, "po_by_s", &e);
                Vec::new()
            }),
        }
    }
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        match self {
            Shard::Local(s) => s.o_by_sp(sub, pred),
            Shard::Remote(r) => r.o_by_sp(sub, pred).unwrap_or_else(|e| {
                warn_remote(r, "o_by_sp", &e);
                Vec::new()
            }),
        }
    }
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        match self {
            Shard::Local(s) => s.p_by_so(sub, obj),
            Shard::Remote(r) => r.p_by_so(sub, obj).unwrap_or_else(|e| {
                warn_remote(r, "p_by_so", &e);
                Vec::new()
            }),
        }
    }
    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        match self {
            Shard::Local(s) => s.ps_by_o(obj).to_vec(),
            Shard::Remote(r) => r.ps_by_o(obj).unwrap_or_else(|e| {
                warn_remote(r, "ps_by_o", &e);
                Vec::new()
            }),
        }
    }
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        match self {
            Shard::Local(s) => s.s_by_po(pred, obj),
            Shard::Remote(r) => r.s_by_po(pred, obj).unwrap_or_else(|e| {
                warn_remote(r, "s_by_po", &e);
                Vec::new()
            }),
        }
    }
    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        match self {
            Shard::Local(s) => s.so_by_p(pred).to_vec(),
            Shard::Remote(r) => r.so_by_p(pred).unwrap_or_else(|e| {
                warn_remote(r, "so_by_p", &e);
                Vec::new()
            }),
        }
    }
    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        match self {
            Shard::Local(s) => s.subs_by_p(pred),
            Shard::Remote(r) => r.subs_by_p(pred).unwrap_or_else(|e| {
                warn_remote(r, "subs_by_p", &e);
                Vec::new()
            }),
        }
    }
    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        match self {
            Shard::Local(s) => s.objs_by_p(pred),
            Shard::Remote(r) => r.objs_by_p(pred).unwrap_or_else(|e| {
                warn_remote(r, "objs_by_p", &e);
                Vec::new()
            }),
        }
    }
    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        match self {
            Shard::Local(s) => s.subject_keys().collect(),
            Shard::Remote(r) => r.subject_keys().unwrap_or_else(|e| {
                warn_remote(r, "subject_keys", &e);
                Vec::new()
            }),
        }
    }
    fn object_keys(&self) -> Vec<EntityLiteralId> {
        match self {
            Shard::Local(s) => s.object_keys().collect(),
            Shard::Remote(r) => r.object_keys().unwrap_or_else(|e| {
                warn_remote(r, "object_keys", &e);
                Vec::new()
            }),
        }
    }
    fn predicates(&self) -> Vec<PredId> {
        match self {
            Shard::Local(s) => s.predicates().collect(),
            Shard::Remote(r) => r.predicates().unwrap_or_else(|e| {
                warn_remote(r, "predicates", &e);
                Vec::new()
            }),
        }
    }
    fn triple_count(&self) -> u64 {
        match self {
            Shard::Local(s) => s.triple_count(),
            Shard::Remote(r) => r.triple_count().unwrap_or_else(|e| {
                warn_remote(r, "triple_count", &e);
                0
            }),
        }
    }
    fn pred_card(&self, pred: PredId) -> usize {
        match self {
            Shard::Local(s) => s.pred_card(pred),
            Shard::Remote(r) => r.pred_card(pred).map(|n| n as usize).unwrap_or_else(|e| {
                warn_remote(r, "pred_card", &e);
                0
            }),
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

/// A triple set partitioned across `N` shards — a mix of **in-process**
/// ([`Shard::Local`]) and **remote, TCP-served** ([`Shard::Remote`]) shards —
/// queried by the same scatter-gather as [`ShardedStore`] and supporting routed
/// inserts. Implements [`TripleSource`] so the generic [`Evaluator`] runs over a
/// genuinely distributed deployment unchanged.
///
/// The partition rule is identical to [`ShardedStore`] (`hash(subject) % N`), so
/// every subject's adjacency co-locates on one shard and the per-shard counts
/// sum cleanly; the merge re-establishes the trait's sorted/de-duplicated `Vec`
/// contract so this store is observationally identical to a single
/// [`TripleStore`] holding the same triples.
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
    /// to a remote shard are surfaced to the caller (unlike best-effort reads).
    pub fn insert(&mut self, t: IdTriple) -> io::Result<bool> {
        let idx = shard_of(t.sub, self.shards.len());
        self.shards[idx].insert(t)
    }

    /// Routed bulk insert; returns the number of triples newly added.
    pub fn insert_all(
        &mut self,
        triples: impl IntoIterator<Item = IdTriple>,
    ) -> io::Result<u64> {
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
    /// engine. Convenience wrapper over `Evaluator::new(dict, self)`.
    pub fn query(&self, dict: &Dictionary, sparql_text: &str) -> Result<QueryResult> {
        let q = sparql::parse(sparql_text)?;
        Evaluator::new(dict, self).evaluate(&q)
    }
}

impl TripleSource for NetworkShardedStore {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        self.shards.iter().any(|s| s.exists(sub, pred, obj))
    }

    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.po_by_s(sub));
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
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.ps_by_o(obj));
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
        let mut out = Vec::new();
        for s in &self.shards {
            out.extend(s.so_by_p(pred));
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
        self.shards.iter().map(Shard::triple_count).sum()
    }

    fn distinct_subjects(&self) -> usize {
        self.subject_keys().len()
    }

    fn distinct_objects(&self) -> usize {
        self.object_keys().len()
    }

    fn num_predicates(&self) -> usize {
        let mut preds: HashSet<PredId> = HashSet::new();
        for s in &self.shards {
            preds.extend(s.predicates());
        }
        preds.len()
    }

    fn pred_card(&self, pred: PredId) -> usize {
        self.shards.iter().map(|s| s.pred_card(pred)).sum()
    }

    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        self.subs_by_p(pred).len()
    }

    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        self.objs_by_p(pred).len()
    }

    fn iter_all(&self) -> Vec<IdTriple> {
        // Reproduce a single TripleStore's predicate-major (pred, sub, obj)
        // order, exactly as ShardedStore does.
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
/// ([`LogOp::AddNode`](crate::rpc::LogOp::AddNode) /
/// [`LogOp::RemoveNode`](crate::rpc::LogOp::RemoveNode)); applying the committed
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
            LogOp::Insert(_) | LogOp::Delete(_) => false,
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
#[derive(Debug)]
pub struct ShardCluster {
    config: ClusterConfig,
    endpoints: HashMap<NodeId, Shard>,
}

impl ShardCluster {
    /// An empty cluster partitioned into `num_shards` buckets (no members yet).
    pub fn new(num_shards: usize) -> ShardCluster {
        ShardCluster {
            config: ClusterConfig::new(num_shards),
            endpoints: HashMap::new(),
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
    /// Borrow a member's shard endpoint (for inspection / tests).
    pub fn endpoint(&self, id: NodeId) -> Option<&Shard> {
        self.endpoints.get(&id)
    }

    /// Admit node `id` (at `addr`) with a freshly bound, empty (or pre-seeded)
    /// shard `endpoint`, then migrate the buckets that rendezvous now assigns to
    /// it from their previous owners — so the new node *receives its shards*. The
    /// first node admitted simply owns every bucket (nothing to migrate yet).
    pub fn add_node(&mut self, id: NodeId, addr: SocketAddr, endpoint: Shard) -> io::Result<()> {
        let old = self.config.clone();
        self.endpoints.insert(id, endpoint);
        if !self.config.add_node(id, addr) {
            return Ok(()); // already a member: no rebalance
        }
        self.rebalance(&old)
    }

    /// Evict node `id`: migrate every bucket it owned to that bucket's new
    /// (survivor) owner — no data loss — then drop its endpoint. A no-op if `id`
    /// is not a member.
    pub fn remove_node(&mut self, id: NodeId) -> io::Result<()> {
        let old = self.config.clone();
        if !self.config.remove_node(id) {
            return Ok(());
        }
        for shard in 0..self.config.num_shards() {
            if old.owner(shard) == Some(id) {
                if let Some(new_owner) = self.config.owner(shard) {
                    self.migrate_shard(shard, id, new_owner)?;
                }
            }
        }
        self.endpoints.remove(&id);
        Ok(())
    }

    /// Migrate every bucket whose owner changed between `old` and the current
    /// config (used after admitting a node).
    fn rebalance(&mut self, old: &ClusterConfig) -> io::Result<()> {
        for shard in 0..self.config.num_shards() {
            let new_owner = match self.config.owner(shard) {
                Some(o) => o,
                None => continue,
            };
            if let Some(old_owner) = old.owner(shard) {
                if old_owner != new_owner {
                    self.migrate_shard(shard, old_owner, new_owner)?;
                }
            }
        }
        Ok(())
    }

    /// Stream bucket `shard` from `from`'s endpoint to `to`'s, then purge it from
    /// `from` (cut-over). If the source endpoint is gone (e.g. a failed node) the
    /// migration is skipped rather than erroring.
    fn migrate_shard(&mut self, shard: usize, from: NodeId, to: NodeId) -> io::Result<()> {
        if from == to {
            return Ok(());
        }
        let n = self.config.num_shards() as u32;
        let triples = match self.endpoints.get(&from) {
            Some(ep) => ep.scan_shard(shard as u32, n)?,
            None => return Ok(()),
        };
        if let Some(dst) = self.endpoints.get_mut(&to) {
            for t in &triples {
                dst.insert(*t)?;
            }
        }
        if let Some(src) = self.endpoints.get_mut(&from) {
            src.purge_shard(shard as u32, n)?;
        }
        Ok(())
    }

    /// Route a triple to its bucket's owner and insert it there.
    pub fn insert(&mut self, t: IdTriple) -> io::Result<bool> {
        let owner = self.config.owner_of_subject(t.sub).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "cluster has no members to route to")
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
    /// scatter-gather across members is transparent to the engine.
    pub fn query(&self, dict: &Dictionary, sparql_text: &str) -> Result<QueryResult> {
        let q = sparql::parse(sparql_text)?;
        Evaluator::new(dict, self).evaluate(&q)
    }
}

impl TripleSource for ShardCluster {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        self.endpoints.values().any(|s| s.exists(sub, pred, obj))
    }
    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        let mut out = Vec::new();
        for s in self.endpoints.values() {
            out.extend(s.po_by_s(sub));
        }
        sort_dedup(out)
    }
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in self.endpoints.values() {
            out.extend(s.o_by_sp(sub, pred));
        }
        sort_dedup(out)
    }
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        let mut out = Vec::new();
        for s in self.endpoints.values() {
            out.extend(s.p_by_so(sub, obj));
        }
        sort_dedup(out)
    }
    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        let mut out = Vec::new();
        for s in self.endpoints.values() {
            out.extend(s.ps_by_o(obj));
        }
        sort_dedup(out)
    }
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in self.endpoints.values() {
            out.extend(s.s_by_po(pred, obj));
        }
        sort_dedup(out)
    }
    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        let mut out = Vec::new();
        for s in self.endpoints.values() {
            out.extend(s.so_by_p(pred));
        }
        sort_dedup(out)
    }
    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in self.endpoints.values() {
            out.extend(s.subs_by_p(pred));
        }
        sort_dedup(out)
    }
    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in self.endpoints.values() {
            out.extend(s.objs_by_p(pred));
        }
        sort_dedup(out)
    }
    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in self.endpoints.values() {
            out.extend(s.subject_keys());
        }
        sort_dedup(out)
    }
    fn object_keys(&self) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in self.endpoints.values() {
            out.extend(s.object_keys());
        }
        sort_dedup(out)
    }
    fn triple_count(&self) -> u64 {
        self.endpoints.values().map(Shard::triple_count).sum()
    }
    fn distinct_subjects(&self) -> usize {
        self.subject_keys().len()
    }
    fn distinct_objects(&self) -> usize {
        self.object_keys().len()
    }
    fn num_predicates(&self) -> usize {
        let mut preds: HashSet<PredId> = HashSet::new();
        for s in self.endpoints.values() {
            preds.extend(s.predicates());
        }
        preds.len()
    }
    fn pred_card(&self, pred: PredId) -> usize {
        self.endpoints.values().map(|s| s.pred_card(pred)).sum()
    }
    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        self.subs_by_p(pred).len()
    }
    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        self.objs_by_p(pred).len()
    }
    fn iter_all(&self) -> Vec<IdTriple> {
        let mut preds: Vec<PredId> = Vec::new();
        for s in self.endpoints.values() {
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
// ([`crate::rpc`]) used by the sharding layer, a background tick driver, and a
// kill/revive switch for fault-injection tests. This mirrors gStore's C++
// `Cluster` (ClusterManager term/quorum/election, ClusterEntityLeader heartbeat/
// append/failover, ClusterEntityFollower, ClusterLog) within this crate's
// zero-extra-dependency stance.
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
}

impl RaftState {
    /// Create a fresh node `id` whose cluster peers are `peers` (excluding self).
    pub fn new(id: NodeId, peers: Vec<NodeId>, cfg: &RaftConfig) -> RaftState {
        let mut seed = id
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ 0xD1B5_4A32_D192_ED03;
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
        };
        s.reset_election_timeout();
        s
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
        self.current_term += 1;
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
        let nli = self.last_log_index() + 1;
        self.next_index.clear();
        self.match_index.clear();
        let peers = self.peers.clone();
        for p in peers {
            self.next_index.insert(p, nli);
            self.match_index.insert(p, 0);
        }
        self.heartbeat_elapsed = 0;
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
        if term < self.current_term {
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
        if term < self.current_term {
            // Stale leader: reject and advertise our (higher) term so it steps
            // down. This is the split-brain guard.
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
                }
            } else {
                self.log.push(*entry);
            }
        }

        let new_match = prev_log_index + entries.len() as u64;
        if leader_commit > self.commit_index {
            self.commit_index = leader_commit.min(self.last_log_index());
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
        if term < self.current_term {
            return RaftReply::Snapshot {
                term: self.current_term,
            };
        }
        self.observe_term(term);
        self.role = Role::Follower;
        self.leader_id = Some(leader_id);
        self.reset_election_timeout();

        if last_included_index > self.base_index {
            self.base_index = last_included_index;
            self.base_term = last_included_term;
            self.log.clear();
            self.snapshot = triples.clone();
            self.pending_reset = Some(triples);
            if self.commit_index < last_included_index {
                self.commit_index = last_included_index;
            }
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
                    self.observe_term(term);
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
                    self.observe_term(term);
                    return Vec::new();
                }
                if self.role != Role::Leader || term < self.current_term {
                    return Vec::new();
                }
                if success {
                    self.match_index.insert(from, match_index);
                    self.next_index.insert(from, match_index + 1);
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
                    let next = (match_index + 1).min(cur.saturating_sub(1)).max(1);
                    self.next_index.insert(from, next);
                    self.replicate_to(from).into_iter().collect()
                }
            }
            RaftReply::Snapshot { term } => {
                if term > self.current_term {
                    self.observe_term(term);
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
}

impl std::fmt::Display for ProposeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProposeError::NotLeader(Some(l)) => write!(f, "not leader; try node {l}"),
            ProposeError::NotLeader(None) => write!(f, "not leader; leader unknown"),
        }
    }
}

impl std::error::Error for ProposeError {}

/// A cluster node: a [`RaftState`] plus a replicated [`TripleStore`], wrapped in
/// the same `std`-only length-prefixed RPC transport ([`crate::rpc`]) as the
/// sharding layer. It runs a TCP server (answering Raft RPCs, client writes,
/// status, and read-only shard queries against the replicated store) and a
/// background **driver** that ticks the state machine and delivers the messages
/// it emits. A kill/revive switch simulates node failure for tests.
#[derive(Debug)]
pub struct ClusterNode {
    id: NodeId,
    state: Arc<Mutex<RaftState>>,
    store: Arc<Mutex<TripleStore>>,
    /// The cluster's replicated membership/config, updated as committed
    /// config-change entries ([`LogOp::AddNode`]/[`LogOp::RemoveNode`]) apply.
    config: Arc<Mutex<ClusterConfig>>,
    /// id → address of every peer (filled in after binding, since ephemeral
    /// ports are only known post-bind).
    peer_addrs: Mutex<HashMap<NodeId, SocketAddr>>,
    /// Cached outbound connections, one per peer.
    clients: Mutex<HashMap<NodeId, TcpStream>>,
    listener: TcpListener,
    alive: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    tick_interval: Duration,
    io_timeout: Duration,
}

impl ClusterNode {
    /// Default shard-bucket count for the node's replicated [`ClusterConfig`].
    /// The membership/assignment is what is agreed through the log; the data
    /// itself is replicated whole-keyspace by this single group (per-shard groups
    /// are the documented generalization), so this value only sizes the agreed
    /// shard→owner map that a [`ShardCluster`] data plane would consume.
    pub const DEFAULT_SHARDS: usize = 16;

    /// Bind node `id` with cluster peers `peer_ids` (their addresses are set
    /// later via [`ClusterNode::set_peer_addr`]) and a starting `store`.
    pub fn bind<A: ToSocketAddrs>(
        id: NodeId,
        peer_ids: Vec<NodeId>,
        store: TripleStore,
        addr: A,
        cfg: RaftConfig,
    ) -> io::Result<Arc<ClusterNode>> {
        let listener = TcpListener::bind(addr)?;
        let tick_interval = cfg.tick_interval;
        let state = RaftState::new(id, peer_ids, &cfg);
        Ok(Arc::new(ClusterNode {
            id,
            state: Arc::new(Mutex::new(state)),
            store: Arc::new(Mutex::new(store)),
            config: Arc::new(Mutex::new(ClusterConfig::new(Self::DEFAULT_SHARDS))),
            peer_addrs: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            listener,
            alive: Arc::new(AtomicBool::new(true)),
            shutdown: Arc::new(AtomicBool::new(false)),
            tick_interval,
            io_timeout: Duration::from_millis(300),
        }))
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
    /// redirect hint. On success the entry is replicated immediately.
    pub fn propose(&self, op: LogOp) -> std::result::Result<u64, ProposeError> {
        let idx = {
            let mut st = self.state.lock().unwrap();
            match st.propose(op) {
                Some(i) => i,
                None => return Err(ProposeError::NotLeader(st.leader_id())),
            }
        };
        let outs = { self.state.lock().unwrap().replicate_now() };
        self.dispatch_outgoing(outs);
        self.apply_actions();
        Ok(idx)
    }

    /// Simulate a crash: stop ticking, refuse to serve, and drop cached
    /// connections. In-memory state is retained (a later [`ClusterNode::revive`]
    /// models a restart that reloaded persistent state).
    pub fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        self.clients.lock().unwrap().clear();
    }
    /// Bring a killed node back: resume ticking/serving and reset its election
    /// timer.
    pub fn revive(&self) {
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

    /// Spawn the server and driver threads. Returns their join handles (callers
    /// may ignore them; threads exit on [`ClusterNode::shutdown`]).
    pub fn start(self: &Arc<Self>) -> Vec<thread::JoinHandle<()>> {
        let server = Arc::clone(self);
        let h1 = thread::spawn(move || server.run_server());
        let driver = Arc::clone(self);
        let h2 = thread::spawn(move || driver.run_driver());
        vec![h1, h2]
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
        let _ = stream.set_read_timeout(Some(self.io_timeout));
        let _ = stream.set_write_timeout(Some(self.io_timeout));
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
                    let _ = rpc::write_message(&mut stream, &Response::Error(e.to_string()).encode());
                    continue;
                }
            };
            if !self.alive.load(Ordering::SeqCst) {
                return;
            }
            let resp = self.handle_request(req);
            if rpc::write_message(&mut stream, &resp.encode()).is_err() {
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
            } => {
                let reply = self.state.lock().unwrap().handle(RaftMsg::RequestVote {
                    term,
                    candidate_id,
                    last_log_index,
                    last_log_term,
                });
                self.apply_actions();
                reply_to_response(reply)
            }
            Request::AppendEntries {
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                leader_commit,
                entries,
            } => {
                let reply = self.state.lock().unwrap().handle(RaftMsg::AppendEntries {
                    term,
                    leader_id,
                    prev_log_index,
                    prev_log_term,
                    leader_commit,
                    entries,
                });
                self.apply_actions();
                reply_to_response(reply)
            }
            Request::InstallSnapshot {
                term,
                leader_id,
                last_included_index,
                last_included_term,
                triples,
            } => {
                let reply = self.state.lock().unwrap().handle(RaftMsg::InstallSnapshot {
                    term,
                    leader_id,
                    last_included_index,
                    last_included_term,
                    triples,
                });
                self.apply_actions();
                reply_to_response(reply)
            }
            Request::ClientWrite { op } => self.client_write_response(op),
            // On a cluster node, a routed insert goes through the replicated log.
            Request::Insert { sub, pred, obj } => {
                self.client_write_response(LogOp::Insert(IdTriple::new(sub, pred, obj)))
            }
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
            other => dispatch(&self.store, &other),
        }
    }

    fn client_write_response(&self, op: LogOp) -> Response {
        match self.propose(op) {
            Ok(index) => Response::WriteAck {
                ok: true,
                leader_hint: self.id as i64,
                index,
            },
            Err(ProposeError::NotLeader(hint)) => Response::WriteAck {
                ok: false,
                leader_hint: hint.map(|h| h as i64).unwrap_or(-1),
                index: 0,
            },
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
            let outs = { self.state.lock().unwrap().tick() };
            self.dispatch_outgoing(outs);
            self.apply_actions();
        }
    }

    /// Deliver outgoing messages, feeding each reply back into the state machine
    /// (which may produce further messages — catch-up, commit propagation —
    /// processed in the same drain). Iterative with a hard guard so a flapping
    /// peer can never spin forever.
    fn dispatch_outgoing(&self, initial: Vec<Outgoing>) {
        let mut queue: VecDeque<Outgoing> = initial.into_iter().collect();
        let mut guard = 0usize;
        while let Some(o) = queue.pop_front() {
            guard += 1;
            if guard > 100_000 || !self.alive.load(Ordering::SeqCst) {
                return;
            }
            match self.send_msg(o.to, &o.msg) {
                Ok(reply) => {
                    let more = self.state.lock().unwrap().handle_reply(o.to, reply);
                    self.apply_actions();
                    queue.extend(more);
                }
                Err(_) => { /* peer unreachable; best-effort, retried next tick */ }
            }
        }
    }

    fn send_msg(&self, to: NodeId, msg: &RaftMsg) -> io::Result<RaftReply> {
        let addr = {
            let addrs = self.peer_addrs.lock().unwrap();
            *addrs
                .get(&to)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown peer address"))?
        };
        let req = msg.to_request();
        let mut clients = self.clients.lock().unwrap();
        for attempt in 0..2 {
            use std::collections::hash_map::Entry;
            let stream = match clients.entry(to) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    let stream = TcpStream::connect(addr)?;
                    stream.set_read_timeout(Some(self.io_timeout))?;
                    stream.set_write_timeout(Some(self.io_timeout))?;
                    e.insert(stream)
                }
            };
            match rpc::round_trip(stream, &req) {
                Ok(resp) => return response_to_reply(resp),
                Err(e) => {
                    clients.remove(&to);
                    if attempt == 1 {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!("the loop returns on the second attempt")
    }

    fn apply_actions(&self) {
        let actions = { self.state.lock().unwrap().drain_apply() };
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

    /// Committed triples in this bucket (read from a serving replica; 0 if every
    /// replica is down).
    pub fn triple_count(&self) -> u64 {
        self.readable_store().map(|s| s.triple_count()).unwrap_or(0)
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
                    // (those belong to the cluster-control [`ClusterConfig`]).
                    StoreAction::Apply(LogOp::AddNode { .. } | LogOp::RemoveNode { .. }) => {}
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
/// It implements [`TripleSource`], so the generic [`Evaluator`] runs over a
/// sharded-and-replicated deployment unchanged. The multi-group analogue of the
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
    /// Raft log. Returns `Some(true)` if newly added, `Some(false)` if already
    /// present, or `None` if the owning group currently has no leader to accept
    /// it. The check against the leader's committed store keeps the op idempotent
    /// (a duplicate is not re-logged).
    pub fn insert(&mut self, t: IdTriple) -> Option<bool> {
        let g = shard_of(t.sub, self.num_shards);
        if let Some(store) = self.groups[g].readable_store() {
            if store.exists(t.sub, t.pred, t.obj) {
                return Some(false);
            }
        }
        self.groups[g].propose(LogOp::Insert(t)).map(|_| true)
    }

    /// Routed bulk insert; returns the number of triples newly committed.
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
    /// if the triple was present (and is now removed), `Some(false)` if it was
    /// absent, or `None` if the group has no leader.
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
        self.groups[g].propose(LogOp::Delete(t)).map(|_| true)
    }

    /// Tick every bucket's leader `rounds` times so any pending catch-up /
    /// snapshot installs to rejoined replicas flush through.
    pub fn settle(&mut self, rounds: usize) {
        for g in &mut self.groups {
            g.settle(rounds);
        }
    }

    /// The per-bucket serving replica stores, skipping any bucket with every
    /// replica down (a best-effort read, like the remote scatter-gather).
    fn readable_stores(&self) -> Vec<&TripleStore> {
        self.groups
            .iter()
            .filter_map(|g| g.readable_store())
            .collect()
    }

    /// Run a read query over the replicated, sharded cluster using the generic
    /// [`Evaluator`] — the per-bucket replication and the scatter-gather are both
    /// transparent to the engine.
    pub fn query(&self, dict: &Dictionary, sparql_text: &str) -> Result<QueryResult> {
        let q = sparql::parse(sparql_text)?;
        Evaluator::new(dict, self).evaluate(&q)
    }
}

impl TripleSource for ShardedRaftCluster {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        self.readable_stores().iter().any(|s| s.exists(sub, pred, obj))
    }
    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        let mut out = Vec::new();
        for s in self.readable_stores() {
            out.extend_from_slice(s.po_by_s(sub));
        }
        sort_dedup(out)
    }
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in self.readable_stores() {
            out.extend(s.o_by_sp(sub, pred));
        }
        sort_dedup(out)
    }
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        let mut out = Vec::new();
        for s in self.readable_stores() {
            out.extend(s.p_by_so(sub, obj));
        }
        sort_dedup(out)
    }
    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        let mut out = Vec::new();
        for s in self.readable_stores() {
            out.extend_from_slice(s.ps_by_o(obj));
        }
        sort_dedup(out)
    }
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in self.readable_stores() {
            out.extend(s.s_by_po(pred, obj));
        }
        sort_dedup(out)
    }
    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        let mut out = Vec::new();
        for s in self.readable_stores() {
            out.extend_from_slice(s.so_by_p(pred));
        }
        sort_dedup(out)
    }
    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in self.readable_stores() {
            out.extend(s.subs_by_p(pred));
        }
        sort_dedup(out)
    }
    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in self.readable_stores() {
            out.extend(s.objs_by_p(pred));
        }
        sort_dedup(out)
    }
    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in self.readable_stores() {
            out.extend(s.subject_keys());
        }
        sort_dedup(out)
    }
    fn object_keys(&self) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        for s in self.readable_stores() {
            out.extend(s.object_keys());
        }
        sort_dedup(out)
    }
    fn triple_count(&self) -> u64 {
        // Each triple lives in exactly one bucket → the per-bucket sum is global.
        self.readable_stores().iter().map(|s| s.triple_count()).sum()
    }
    fn distinct_subjects(&self) -> usize {
        self.subject_keys().len()
    }
    fn distinct_objects(&self) -> usize {
        self.object_keys().len()
    }
    fn num_predicates(&self) -> usize {
        let mut preds: HashSet<PredId> = HashSet::new();
        for s in self.readable_stores() {
            preds.extend(s.predicates());
        }
        preds.len()
    }
    fn pred_card(&self, pred: PredId) -> usize {
        self.readable_stores().iter().map(|s| s.pred_card(pred)).sum()
    }
    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        self.subs_by_p(pred).len()
    }
    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        self.objs_by_p(pred).len()
    }
    fn iter_all(&self) -> Vec<IdTriple> {
        // Reproduce a single TripleStore's predicate-major (pred, sub, obj)
        // order, exactly as ShardedStore does.
        let mut preds: Vec<PredId> = Vec::new();
        for s in self.readable_stores() {
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
        assert_eq!(sharded.iter_all(), base.iter_all().collect::<Vec<_>>(), "iter_all");

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
        let added = net.insert_all(triples.iter().copied()).expect("routed insert");
        assert_eq!(added, base.triple_count(), "all distinct triples inserted");
        // Each triple lands on exactly one node → per-node counts sum to total.
        assert_eq!(
            TripleSource::triple_count(&net),
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
        net.insert_all(triples.iter().copied()).expect("routed insert");

        assert_eq!(TripleSource::triple_count(&net), base.triple_count());
        assert_eq!(net.iter_all(), base.iter_all());

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
}

/// Tests for the **Raft-like replication layer**. The bulk are *deterministic*:
/// a [`Sim`] drives `N` in-memory [`RaftState`]s by hand — delivering every
/// message synchronously and ticking exactly the node we choose — so elections,
/// replication, failover, catch-up, snapshotting, and split-brain safety are
/// exercised with **no clocks and no threads** (zero wall-clock flakiness). Two
/// further tests run real [`ClusterNode`]s over TCP loopback (background threads
/// on `127.0.0.1:0`, like `tests/dt_service.rs`) and poll for convergence to
/// prove the transport, election, failover, and recovery work end-to-end.
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
        assert_eq!(sim.stores[2].triple_count(), 0, "offline node missed writes");

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
        sim.propose(0, LogOp::AddNode { id: 0, addr: a(6000) });
        sim.propose(0, LogOp::AddNode { id: 1, addr: a(6001) });
        sim.propose(0, LogOp::AddNode { id: 2, addr: a(6002) });
        sim.propose(0, LogOp::AddNode { id: 3, addr: a(6003) }); // join
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

    // ----- Networked end-to-end (real TCP, background threads) ---------------

    fn fast_cfg() -> RaftConfig {
        RaftConfig {
            election_timeout_min: 4,
            election_timeout_max: 9,
            heartbeat_timeout: 1,
            tick_interval: Duration::from_millis(12),
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

    /// Poll `cond` until it holds or `deadline` elapses.
    fn wait_until<F: Fn() -> bool>(deadline: Duration, cond: F) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
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
        let nodes = build_cluster(3, fast_cfg());
        let deadline = Duration::from_secs(20);

        // 1. A leader is elected over the wire.
        assert!(
            wait_until(deadline, || current_leader(&nodes).is_some()),
            "no leader was elected"
        );
        let l = current_leader(&nodes).unwrap();

        // 2. Replicate a few mutations; every replica converges.
        let triples = [t(1, 10, 100), t(2, 10, 200), t(3, 10, 300)];
        for &tr in &triples {
            nodes[l].propose(LogOp::Insert(tr)).expect("leader accepts write");
        }
        assert!(
            wait_until(deadline, || nodes
                .iter()
                .all(|n| n.store_triple_count() == triples.len() as u64)),
            "replicas did not converge on the initial writes"
        );
        for n in &nodes {
            for &tr in &triples {
                assert!(n.store_contains(tr));
            }
        }

        // 3. Kill the leader; a new leader emerges and still accepts writes.
        nodes[l].kill();
        assert!(
            wait_until(deadline, || matches!(current_leader(&nodes), Some(nl) if nl != l)),
            "no new leader after killing the old one"
        );
        let l2 = current_leader(&nodes).unwrap();
        assert_ne!(l2, l);
        let extra = t(4, 10, 400);
        nodes[l2]
            .propose(LogOp::Insert(extra))
            .expect("new leader accepts write");
        let survivors: Vec<usize> = (0..3).filter(|&i| i != l).collect();
        assert!(
            wait_until(deadline, || survivors
                .iter()
                .all(|&i| nodes[i].store_contains(extra))),
            "the post-failover write did not reach the survivors"
        );

        // 4. Restart the old leader; it catches up to all four triples and is no
        //    longer the leader.
        nodes[l].revive();
        let all: Vec<IdTriple> = triples.iter().copied().chain(std::iter::once(extra)).collect();
        assert!(
            wait_until(deadline, || nodes[l].store_triple_count() == 4
                && all.iter().all(|&tr| nodes[l].store_contains(tr))),
            "the revived old leader did not catch up"
        );
        assert!(
            !nodes[l].is_leader(),
            "the revived stale leader must step down"
        );

        shutdown_all(&nodes);
    }

    #[test]
    fn networked_client_write_rpc_commits_on_leader() {
        let nodes = build_cluster(3, fast_cfg());
        let deadline = Duration::from_secs(20);
        assert!(wait_until(deadline, || current_leader(&nodes).is_some()));
        let l = current_leader(&nodes).unwrap();

        // A ClientWrite RPC sent to the leader is appended and replicated.
        let laddr = nodes[l].local_addr().unwrap();
        let tr = t(7, 70, 700);
        let mut s = TcpStream::connect(laddr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let resp = crate::rpc::round_trip(&mut s, &Request::ClientWrite { op: LogOp::Insert(tr) })
            .expect("client write round-trip");
        match resp {
            Response::WriteAck { ok, .. } => assert!(ok, "leader should accept the write"),
            other => panic!("expected WriteAck, got {other:?}"),
        }
        assert!(
            wait_until(deadline, || nodes.iter().all(|n| n.store_contains(tr))),
            "client-written triple did not replicate"
        );

        // A ClientWrite to a follower is answered (with a redirect when it is not
        // the leader).
        let f = (0..3).find(|&i| i != l).unwrap();
        let faddr = nodes[f].local_addr().unwrap();
        let mut s2 = TcpStream::connect(faddr).unwrap();
        s2.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let resp2 = crate::rpc::round_trip(
            &mut s2,
            &Request::ClientWrite {
                op: LogOp::Insert(t(8, 80, 800)),
            },
        )
        .expect("follower client write round-trip");
        match resp2 {
            Response::WriteAck { ok, leader_hint, .. } => {
                if !ok {
                    assert!(leader_hint >= 0, "a redirect should name a leader");
                }
            }
            other => panic!("expected WriteAck, got {other:?}"),
        }

        shutdown_all(&nodes);
    }

    /// Task 3 (per-shard replication, minimal form): a shard backed by a Raft
    /// replication group keeps serving writes and retains its data when one
    /// **replica fails**. A 3-node group replicates the whole keyspace; killing a
    /// follower still leaves a 2/3 quorum, so the leader commits new writes, and
    /// the revived replica catches up. (The generalization — one such group *per
    /// shard* — is documented as remaining in the module scope notes.)
    #[test]
    fn replicated_shard_survives_a_replica_failure() {
        let nodes = build_cluster(3, fast_cfg());
        let deadline = Duration::from_secs(20);
        assert!(
            wait_until(deadline, || current_leader(&nodes).is_some()),
            "no leader elected"
        );
        let l = current_leader(&nodes).unwrap();

        // A first write replicates to all three replicas of the shard.
        let a = t(1, 2, 3);
        nodes[l].propose(LogOp::Insert(a)).expect("leader accepts write");
        assert!(
            wait_until(deadline, || nodes.iter().all(|n| n.store_contains(a))),
            "initial write did not replicate to every replica"
        );

        // Fail one replica (a follower, not the leader).
        let f = (0..3).find(|&i| i != l).unwrap();
        nodes[f].kill();
        // The leader retains leadership with the remaining 2/3 quorum.
        assert!(
            wait_until(deadline, || nodes[l].is_leader()),
            "leader lost quorum after a single replica failure"
        );

        // The shard still accepts and commits a new write on the survivors.
        let b = t(4, 5, 6);
        nodes[l].propose(LogOp::Insert(b)).expect("survivors still commit");
        let survivors: Vec<usize> = (0..3).filter(|&i| i != f).collect();
        assert!(
            wait_until(deadline, || survivors.iter().all(|&i| nodes[i].store_contains(b))),
            "post-failure write did not reach the surviving replicas"
        );

        // A surviving replica answers a read RPC over the wire (no data lost).
        let saddr = nodes[survivors[0]].local_addr().unwrap();
        let mut s = TcpStream::connect(saddr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let resp = crate::rpc::round_trip(&mut s, &Request::TripleCount).unwrap();
        assert!(matches!(resp, Response::Count(n) if n >= 2), "read served by survivor");

        // The failed replica revives and catches up to both writes.
        nodes[f].revive();
        assert!(
            wait_until(deadline, || nodes[f].store_contains(a) && nodes[f].store_contains(b)),
            "revived replica did not catch up"
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
        let nodes = build_cluster(3, fast_cfg());
        let deadline = Duration::from_secs(20);
        assert!(wait_until(deadline, || current_leader(&nodes).is_some()));
        let l = current_leader(&nodes).unwrap();

        // Submit an AddNode config-change as a wire ClientWrite to the leader.
        let new_addr: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let laddr = nodes[l].local_addr().unwrap();
        let mut s = TcpStream::connect(laddr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let resp = crate::rpc::round_trip(
            &mut s,
            &Request::ClientWrite {
                op: LogOp::AddNode { id: 42, addr: new_addr },
            },
        )
        .expect("client config-change round-trip");
        assert!(
            matches!(resp, Response::WriteAck { ok: true, .. }),
            "leader accepts the config-change"
        );

        // Every replica converges on the new membership.
        assert!(
            wait_until(deadline, || nodes.iter().all(|n| n.config().contains(42))),
            "membership did not replicate to every node"
        );

        // A follower serves the committed config over the wire.
        let f = (0..3).find(|&i| i != l).unwrap();
        let faddr = nodes[f].local_addr().unwrap();
        let mut sf = TcpStream::connect(faddr).unwrap();
        sf.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let cfg = crate::rpc::round_trip(&mut sf, &Request::GetConfig).expect("GetConfig round-trip");
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
        assert_eq!(a, b, "same member set => identical config regardless of order");
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
            LogOp::AddNode { id: 5, addr: addr(1005) },
            LogOp::AddNode { id: 1, addr: addr(1001) },
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
        assert_eq!(viaop.epoch(), 3, "only the 3 membership changes bump the epoch");
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
        (addr, Shard::Remote(RemoteShard::connect(addr).expect("connect")))
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
            TripleSource::triple_count(&cluster),
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
        assert!(endpoint_count(&cluster, 2) > 0, "the joiner received its shards");

        // No data was lost or duplicated, and the distributed query is unchanged.
        assert_eq!(TripleSource::triple_count(&cluster), base.triple_count());
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
        assert_eq!(TripleSource::triple_count(&cluster), base.triple_count());

        // Node 1 leaves; its buckets move to survivors.
        cluster.remove_node(1).unwrap();
        assert_eq!(cluster.members(), vec![0, 2], "node 1 left the membership");
        for s in 0..cluster.num_shards() {
            assert_ne!(cluster.config().owner(s), Some(1), "no bucket left on a departed node");
        }
        // No data lost and the query is still correct.
        assert_eq!(
            TripleSource::triple_count(&cluster),
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
            .filter(|&id| cluster.endpoint(id).unwrap().exists(alice, knows, zoe))
            .collect();
        assert_eq!(holders, vec![owner], "the write lands on exactly its owner");

        // And it is visible to a distributed query.
        let ask = "ASK { <http://ex/alice> <http://ex/knows> <http://ex/zoe> }";
        assert!(
            matches!(cluster.query(&d, ask).unwrap(), QueryResult::Ask(true)),
            "post-rebalance write is queryable"
        );
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
        assert_eq!(added, base.triple_count(), "all triples committed, none lost");
        // A duplicate routed insert is idempotent (already committed).
        assert_eq!(cluster.insert(triples[0]), Some(false));

        assert_eq!(
            TripleSource::triple_count(&cluster),
            base.triple_count(),
            "triple_count"
        );
        assert_eq!(
            cluster.distinct_subjects(),
            base.distinct_subjects(),
            "distinct_subjects"
        );
        assert_eq!(
            cluster.distinct_objects(),
            base.distinct_objects(),
            "distinct_objects"
        );
        assert_eq!(
            cluster.num_predicates(),
            base.num_predicates(),
            "num_predicates"
        );
        assert_eq!(
            TripleSource::iter_all(&cluster),
            base.iter_all().collect::<Vec<_>>(),
            "iter_all"
        );

        // Per-predicate and subject-/object-rooted accesses.
        let mut ids: Vec<EntityLiteralId> = base.subject_keys().collect();
        ids.extend(base.object_keys());
        ids = sort_dedup(ids);
        for &p in &base.predicates().collect::<Vec<_>>() {
            assert_eq!(TripleSource::so_by_p(&cluster, p), base.so_by_p(p).to_vec());
            assert_eq!(cluster.subs_by_p(p), base.subs_by_p(p));
            assert_eq!(cluster.objs_by_p(p), base.objs_by_p(p));
            assert_eq!(cluster.pred_card(p), base.pred_card(p));
        }
        for &s in &ids {
            assert_eq!(TripleSource::po_by_s(&cluster, s), base.po_by_s(s).to_vec());
            assert_eq!(TripleSource::ps_by_o(&cluster, s), base.ps_by_o(s).to_vec());
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
        assert_eq!(cluster.group(0).triple_count(), 2, "quorum committed the write");
        assert!(TripleSource::exists(&cluster, 4, 5, 6));
        assert_eq!(cluster.group(0).leader(), Some(leader), "leader retained");

        // A 3rd failure drops below majority: the leader appends but cannot
        // commit, so nothing new is applied.
        cluster.set_replica_down(0, followers[2], true);
        let before = cluster.group(0).state(leader).commit_index();
        assert_eq!(cluster.insert(t(7, 8, 9)), Some(true), "leader still appends");
        let after = cluster.group(0).state(leader).commit_index();
        assert_eq!(before, after, "no quorum => nothing new commits");
        assert!(
            !TripleSource::exists(&cluster, 7, 8, 9),
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
        assert!(base > 0, "leader compacted its committed log into a snapshot");
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
            assert!(cluster.group(0).store(follower).exists(tr.sub, tr.pred, tr.obj));
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
            assert!(cluster.group(0).store(follower).exists(tr.sub, tr.pred, tr.obj));
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
        assert_ne!(c.group(0).leader(), Some(old), "a revived stale leader must not lead");

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
        assert_eq!(c.remove(tr), None, "no leader ⇒ None, not a spurious 'absent'");
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
            TripleSource::triple_count(&cluster),
            base.triple_count(),
            "no data lost when a replica per bucket fails"
        );
        assert_eq!(
            TripleSource::iter_all(&cluster),
            base.iter_all().collect::<Vec<_>>(),
        );
        assert_eq!(
            select_rows(cluster.query(&d, BGP).unwrap()),
            base_select(&d, &base, BGP),
        );
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
