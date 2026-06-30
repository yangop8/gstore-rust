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
//! ## Scope (still deferred)
//!
//! What remains out of scope is **membership / rebalancing**: node join/leave,
//! partition reassignment, replication, and fault tolerance. Remote reads are
//! *best-effort* — a failed shard RPC is logged and treated as empty rather than
//! aborting the whole query (the trait methods cannot return errors); routed
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
