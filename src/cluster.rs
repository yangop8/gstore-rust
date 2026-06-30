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
//! ## Scope (still deferred)
//!
//! What remains out of scope is **elastic membership / rebalancing of the shard
//! map**: dynamic node join/leave with partition reassignment, and running the
//! Raft replication group *per shard* of a [`NetworkShardedStore`]. Remote reads
//! are *best-effort* — a failed shard RPC is logged and treated as empty rather
//! than aborting the whole query (the trait methods cannot return errors); routed
//! [`NetworkShardedStore::insert`] does surface I/O errors to the caller.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
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
        // Raft / cluster-control RPCs are not part of the plain shard surface; a
        // [`ShardNode`] is a single-replica shard. They are served by
        // [`ClusterNode`] instead (see below).
        Request::RequestVote { .. }
        | Request::AppendEntries { .. }
        | Request::InstallSnapshot { .. }
        | Request::ClientWrite { .. }
        | Request::ClusterStatus => {
            Response::Error("cluster-control RPC sent to a non-replicated shard node".to_string())
        }
    }
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

    /// Reset the election timer (used by the transport when a node is revived,
    /// so it doesn't immediately campaign with a stale elapsed counter).
    fn note_revived(&mut self) {
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
        let mut store = self.store.lock().unwrap();
        for a in actions {
            match a {
                StoreAction::Apply(LogOp::Insert(t)) => {
                    store.insert(t);
                }
                StoreAction::Apply(LogOp::Delete(t)) => {
                    store.remove(t);
                }
                StoreAction::Reset(triples) => {
                    *store = TripleStore::new();
                    store.bulk_load(triples);
                }
            }
        }
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
        down: Vec<bool>,
    }

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
            Sim {
                nodes,
                stores,
                down: vec![false; n],
            }
        }

        fn node(&self, id: NodeId) -> &RaftState {
            &self.nodes[id as usize]
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
}
