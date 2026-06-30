//! A minimal, dependency-free binary RPC wire codec for the distributed shard
//! layer ([`crate::cluster`]).
//!
//! ## What this is
//!
//! A compact **length-prefixed** request/response protocol over any
//! [`Read`]/[`Write`] (in practice a [`std::net::TcpStream`]). Every message on
//! the wire is:
//!
//! ```text
//! ┌──────────────┬─────────────────────────────┐
//! │ u32 BE length │ payload (length bytes)      │
//! └──────────────┴─────────────────────────────┘
//! ```
//!
//! The payload is a hand-rolled byte encoding of a [`Request`] or a
//! [`Response`]: a one-byte tag selects the variant, followed by its fields as
//! big-endian integers (and length-prefixed vectors). No `serde`, no `bincode`,
//! no third-party framing — just `std` and byte buffers, matching this crate's
//! zero-runtime-dependency stance for its network code (see [`crate::server`],
//! the hand-rolled HTTP server).
//!
//! ## Why not gRPC / protobuf?
//!
//! A "real" cluster would normally ship sub-queries with gRPC + protobuf. That
//! pulls in `tonic`/`prost` (and `tokio`), which is **deliberately out of this
//! crate's zero-dependency scope**. This module is the *std-TCP equivalent*: it
//! occupies exactly the same slot in the architecture (the shard transport),
//! and swapping it for gRPC would be a **serialization-codec swap** — replace
//! the [`Request`]/[`Response`] (de)serialization and the [`write_message`] /
//! [`read_message`] framing with generated protobuf stubs over an HTTP/2
//! channel, leaving [`crate::cluster`]'s scatter-gather logic untouched.
//!
//! ## Operations
//!
//! The request set mirrors the per-shard primitives that
//! [`crate::store::TripleSource`] scatter-gather needs (one access-pattern
//! lookup each, plus the global scans/counts), and adds a routed [`Request::Insert`]
//! so the coordinator can place a triple on the shard owning its subject. Every
//! derived statistic ([`TripleSource::distinct_subjects`](crate::store::TripleSource::distinct_subjects),
//! `num_predicates`, `iter_all`, …) is reconstructed by the coordinator from
//! these primitives, exactly as the in-process `ShardedStore` does — so the
//! wire surface stays small.

use std::io::{self, Read, Write};

use crate::model::id::{EntityLiteralId, PredId};
use crate::model::IdTriple;

/// Upper bound on a single frame's payload, a guard against a corrupt or
/// hostile length prefix triggering a huge allocation. 256 MiB is far above any
/// legitimate shard answer in this in-memory store.
pub const MAX_FRAME: usize = 256 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Cluster / replication wire types (used by the Raft-like layer in
// [`crate::cluster`]). They live here, beside the codec, so there is a *single*
// on-wire definition that both the state machine and the transport reuse.
// ---------------------------------------------------------------------------

/// A node's stable identity within a cluster.
pub type NodeId = u64;

/// A single replicated mutation: insert or delete one triple. This is the unit
/// of the replicated log — followers apply committed [`LogOp`]s to their store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogOp {
    /// Add a triple (`store.insert`).
    Insert(IdTriple),
    /// Remove a triple (`store.remove`).
    Delete(IdTriple),
}

impl LogOp {
    /// The triple this op acts on.
    pub fn triple(&self) -> IdTriple {
        match *self {
            LogOp::Insert(t) | LogOp::Delete(t) => t,
        }
    }
}

/// One entry of the replicated log: the leader's `term` when it was created plus
/// the mutation. The `(index, term)` pair is what Raft's consistency check and
/// commit rule are built on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogEntry {
    /// Leader term in which this entry was created.
    pub term: u64,
    /// The replicated mutation.
    pub op: LogOp,
}

impl LogEntry {
    /// Construct a log entry.
    pub fn new(term: u64, op: LogOp) -> LogEntry {
        LogEntry { term, op }
    }
}

// ---------------------------------------------------------------------------
// Framing: u32 BE length + payload.
// ---------------------------------------------------------------------------

/// Write one length-prefixed message: a 4-byte big-endian length followed by
/// `payload`, then flush. Errors if `payload` exceeds [`MAX_FRAME`].
pub fn write_message<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "rpc frame exceeds MAX_FRAME",
        ));
    }
    let len = payload.len() as u32;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read one length-prefixed message: a 4-byte big-endian length, then exactly
/// that many payload bytes. Errors if the length exceeds [`MAX_FRAME`]. An EOF
/// before the length prefix surfaces as [`io::ErrorKind::UnexpectedEof`].
pub fn read_message<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rpc frame exceeds MAX_FRAME",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Primitive byte (de)serialization helpers — std only, big-endian.
// ---------------------------------------------------------------------------

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_be_bytes());
}

fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_be_bytes());
}

/// Encode a `Vec<u32>` as a `u32` count followed by the elements.
fn put_ids(buf: &mut Vec<u8>, ids: &[u32]) {
    put_u32(buf, ids.len() as u32);
    for &id in ids {
        put_u32(buf, id);
    }
}

/// Encode a `Vec<(u32, u32)>` as a `u32` count followed by the pairs.
fn put_pairs(buf: &mut Vec<u8>, pairs: &[(u32, u32)]) {
    put_u32(buf, pairs.len() as u32);
    for &(a, b) in pairs {
        put_u32(buf, a);
        put_u32(buf, b);
    }
}

/// Encode a string as a `u32` byte-length followed by its UTF-8 bytes.
fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}

/// Encode an `i64` as its two's-complement `u64` bit pattern.
fn put_i64(buf: &mut Vec<u8>, v: i64) {
    put_u64(buf, v as u64);
}

/// Encode a [`LogOp`]: a one-byte kind tag (`1` insert, `2` delete) then the
/// triple's `(sub, pred, obj)`.
fn put_op(buf: &mut Vec<u8>, op: &LogOp) {
    let (kind, t) = match *op {
        LogOp::Insert(t) => (1u8, t),
        LogOp::Delete(t) => (2u8, t),
    };
    buf.push(kind);
    put_u32(buf, t.sub);
    put_u32(buf, t.pred);
    put_u32(buf, t.obj);
}

/// Encode a slice of [`LogEntry`] as a `u32` count then each `term` + op.
fn put_entries(buf: &mut Vec<u8>, entries: &[LogEntry]) {
    put_u32(buf, entries.len() as u32);
    for e in entries {
        put_u64(buf, e.term);
        put_op(buf, &e.op);
    }
}

/// Encode a slice of [`IdTriple`] (snapshot payload) as a `u32` count then each
/// `(sub, pred, obj)`.
fn put_triples(buf: &mut Vec<u8>, triples: &[IdTriple]) {
    put_u32(buf, triples.len() as u32);
    for t in triples {
        put_u32(buf, t.sub);
        put_u32(buf, t.pred);
        put_u32(buf, t.obj);
    }
}

/// A forward cursor over a byte buffer, returning [`io::Error`] of kind
/// [`io::ErrorKind::InvalidData`] on truncation — so a malformed frame is a
/// clean error rather than a panic.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len());
        match end {
            Some(end) => {
                let slice = &self.buf[self.pos..end];
                self.pos = end;
                Ok(slice)
            }
            None => Err(bad("rpc message truncated")),
        }
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> io::Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> io::Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn ids(&mut self) -> io::Result<Vec<u32>> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            out.push(self.u32()?);
        }
        Ok(out)
    }

    fn pairs(&mut self) -> io::Result<Vec<(u32, u32)>> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            out.push((self.u32()?, self.u32()?));
        }
        Ok(out)
    }

    fn string(&mut self) -> io::Result<String> {
        let n = self.u32()? as usize;
        let bytes = self.take(n)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| bad("rpc string is not valid UTF-8"))
    }

    fn i64(&mut self) -> io::Result<i64> {
        Ok(self.u64()? as i64)
    }

    fn op(&mut self) -> io::Result<LogOp> {
        let kind = self.u8()?;
        let t = IdTriple::new(self.u32()?, self.u32()?, self.u32()?);
        match kind {
            1 => Ok(LogOp::Insert(t)),
            2 => Ok(LogOp::Delete(t)),
            _ => Err(bad("unknown log-op kind")),
        }
    }

    fn entries(&mut self) -> io::Result<Vec<LogEntry>> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            let term = self.u64()?;
            let op = self.op()?;
            out.push(LogEntry { term, op });
        }
        Ok(out)
    }

    fn triples(&mut self) -> io::Result<Vec<IdTriple>> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            out.push(IdTriple::new(self.u32()?, self.u32()?, self.u32()?));
        }
        Ok(out)
    }
}

fn bad(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

// ---------------------------------------------------------------------------
// Request: one shard operation. Tags are explicit and stable on the wire.
// ---------------------------------------------------------------------------

/// A single shard operation, addressed to one [`gnode`](crate::cluster::ShardNode).
///
/// The read variants correspond one-for-one to [`crate::store::TripleSource`]
/// access patterns; [`Request::Insert`] carries a routed triple to add to the
/// shard. Field order on the wire matches the struct field order below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `s p o` — does this exact triple exist on the shard?
    Exists {
        sub: EntityLiteralId,
        pred: PredId,
        obj: EntityLiteralId,
    },
    /// `s ? ?` — `(pred, obj)` pairs for a subject.
    PoByS { sub: EntityLiteralId },
    /// `s p ?` — objects of `(sub, pred)`.
    OBySp { sub: EntityLiteralId, pred: PredId },
    /// `s ? o` — predicates linking a subject to an object.
    PBySo {
        sub: EntityLiteralId,
        obj: EntityLiteralId,
    },
    /// `? ? o` — `(pred, sub)` pairs for an object.
    PsByO { obj: EntityLiteralId },
    /// `? p o` — subjects of `(pred, obj)`.
    SByPo { pred: PredId, obj: EntityLiteralId },
    /// `? p ?` — `(sub, obj)` pairs for a predicate.
    SoByP { pred: PredId },
    /// Distinct subjects appearing with a predicate.
    SubsByP { pred: PredId },
    /// Distinct objects appearing with a predicate.
    ObjsByP { pred: PredId },
    /// All ids that appear as a subject on the shard.
    SubjectKeys,
    /// All ids that appear as an object on the shard.
    ObjectKeys,
    /// All predicate ids present on the shard.
    Predicates,
    /// Total triple count on the shard.
    TripleCount,
    /// Number of triples with a predicate.
    PredCard { pred: PredId },
    /// Insert a routed triple; the response reports whether it was new.
    Insert {
        sub: EntityLiteralId,
        pred: PredId,
        obj: EntityLiteralId,
    },

    // --- Raft replication / cluster control (see [`crate::cluster`]). ---
    /// Raft: a candidate solicits a vote for `term`.
    RequestVote {
        term: u64,
        candidate_id: NodeId,
        last_log_index: u64,
        last_log_term: u64,
    },
    /// Raft: a leader replicates `entries` (empty = heartbeat) after the
    /// `(prev_log_index, prev_log_term)` anchor, advertising `leader_commit`.
    AppendEntries {
        term: u64,
        leader_id: NodeId,
        prev_log_index: u64,
        prev_log_term: u64,
        leader_commit: u64,
        entries: Vec<LogEntry>,
    },
    /// Raft: a leader ships a full snapshot (`triples`) to a follower whose log
    /// has fallen behind the leader's compaction point.
    InstallSnapshot {
        term: u64,
        leader_id: NodeId,
        last_included_index: u64,
        last_included_term: u64,
        triples: Vec<IdTriple>,
    },
    /// A client submits a mutation to the cluster; the leader appends it, a
    /// follower replies with a redirect hint.
    ClientWrite { op: LogOp },
    /// Introspection: ask a node for its raft role / term / commit index.
    ClusterStatus,
}

impl Request {
    /// Serialize to a self-describing byte payload (without the frame length).
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16);
        match *self {
            Request::Exists { sub, pred, obj } => {
                buf.push(1);
                put_u32(&mut buf, sub);
                put_u32(&mut buf, pred);
                put_u32(&mut buf, obj);
            }
            Request::PoByS { sub } => {
                buf.push(2);
                put_u32(&mut buf, sub);
            }
            Request::OBySp { sub, pred } => {
                buf.push(3);
                put_u32(&mut buf, sub);
                put_u32(&mut buf, pred);
            }
            Request::PBySo { sub, obj } => {
                buf.push(4);
                put_u32(&mut buf, sub);
                put_u32(&mut buf, obj);
            }
            Request::PsByO { obj } => {
                buf.push(5);
                put_u32(&mut buf, obj);
            }
            Request::SByPo { pred, obj } => {
                buf.push(6);
                put_u32(&mut buf, pred);
                put_u32(&mut buf, obj);
            }
            Request::SoByP { pred } => {
                buf.push(7);
                put_u32(&mut buf, pred);
            }
            Request::SubsByP { pred } => {
                buf.push(8);
                put_u32(&mut buf, pred);
            }
            Request::ObjsByP { pred } => {
                buf.push(9);
                put_u32(&mut buf, pred);
            }
            Request::SubjectKeys => buf.push(10),
            Request::ObjectKeys => buf.push(11),
            Request::Predicates => buf.push(12),
            Request::TripleCount => buf.push(13),
            Request::PredCard { pred } => {
                buf.push(14);
                put_u32(&mut buf, pred);
            }
            Request::Insert { sub, pred, obj } => {
                buf.push(15);
                put_u32(&mut buf, sub);
                put_u32(&mut buf, pred);
                put_u32(&mut buf, obj);
            }
            Request::RequestVote {
                term,
                candidate_id,
                last_log_index,
                last_log_term,
            } => {
                buf.push(16);
                put_u64(&mut buf, term);
                put_u64(&mut buf, candidate_id);
                put_u64(&mut buf, last_log_index);
                put_u64(&mut buf, last_log_term);
            }
            Request::AppendEntries {
                term,
                leader_id,
                prev_log_index,
                prev_log_term,
                leader_commit,
                ref entries,
            } => {
                buf.push(17);
                put_u64(&mut buf, term);
                put_u64(&mut buf, leader_id);
                put_u64(&mut buf, prev_log_index);
                put_u64(&mut buf, prev_log_term);
                put_u64(&mut buf, leader_commit);
                put_entries(&mut buf, entries);
            }
            Request::InstallSnapshot {
                term,
                leader_id,
                last_included_index,
                last_included_term,
                ref triples,
            } => {
                buf.push(18);
                put_u64(&mut buf, term);
                put_u64(&mut buf, leader_id);
                put_u64(&mut buf, last_included_index);
                put_u64(&mut buf, last_included_term);
                put_triples(&mut buf, triples);
            }
            Request::ClientWrite { ref op } => {
                buf.push(19);
                put_op(&mut buf, op);
            }
            Request::ClusterStatus => buf.push(20),
        }
        buf
    }

    /// Parse a [`Request`] from a payload produced by [`Request::encode`].
    pub fn decode(payload: &[u8]) -> io::Result<Request> {
        let mut r = Reader::new(payload);
        let tag = r.u8()?;
        let req = match tag {
            1 => Request::Exists {
                sub: r.u32()?,
                pred: r.u32()?,
                obj: r.u32()?,
            },
            2 => Request::PoByS { sub: r.u32()? },
            3 => Request::OBySp {
                sub: r.u32()?,
                pred: r.u32()?,
            },
            4 => Request::PBySo {
                sub: r.u32()?,
                obj: r.u32()?,
            },
            5 => Request::PsByO { obj: r.u32()? },
            6 => Request::SByPo {
                pred: r.u32()?,
                obj: r.u32()?,
            },
            7 => Request::SoByP { pred: r.u32()? },
            8 => Request::SubsByP { pred: r.u32()? },
            9 => Request::ObjsByP { pred: r.u32()? },
            10 => Request::SubjectKeys,
            11 => Request::ObjectKeys,
            12 => Request::Predicates,
            13 => Request::TripleCount,
            14 => Request::PredCard { pred: r.u32()? },
            15 => Request::Insert {
                sub: r.u32()?,
                pred: r.u32()?,
                obj: r.u32()?,
            },
            16 => Request::RequestVote {
                term: r.u64()?,
                candidate_id: r.u64()?,
                last_log_index: r.u64()?,
                last_log_term: r.u64()?,
            },
            17 => Request::AppendEntries {
                term: r.u64()?,
                leader_id: r.u64()?,
                prev_log_index: r.u64()?,
                prev_log_term: r.u64()?,
                leader_commit: r.u64()?,
                entries: r.entries()?,
            },
            18 => Request::InstallSnapshot {
                term: r.u64()?,
                leader_id: r.u64()?,
                last_included_index: r.u64()?,
                last_included_term: r.u64()?,
                triples: r.triples()?,
            },
            19 => Request::ClientWrite { op: r.op()? },
            20 => Request::ClusterStatus,
            _ => return Err(bad("unknown rpc request tag")),
        };
        Ok(req)
    }

    /// The routed triple, if this is an [`Request::Insert`].
    pub fn as_insert(&self) -> Option<IdTriple> {
        match *self {
            Request::Insert { sub, pred, obj } => Some(IdTriple::new(sub, pred, obj)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Response: a small set of result shapes covering every request.
// ---------------------------------------------------------------------------

/// A shard's answer to a [`Request`]. The variant is chosen by the request:
/// `Exists`/`Insert` → [`Response::Bool`]; `TripleCount`/`PredCard` →
/// [`Response::Count`]; key/predicate scans and `SubsByP`/`ObjsByP`/`SByPo`/
/// `OBySp`/`PBySo` → [`Response::Ids`]; `PoByS`/`PsByO`/`SoByP` →
/// [`Response::Pairs`]. [`Response::Error`] reports a server-side failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// A boolean result (existence / "was newly inserted").
    Bool(bool),
    /// A scalar count (`triple_count`, `pred_card`).
    Count(u64),
    /// A list of ids (`EntityLiteralId` or `PredId`; both are `u32`).
    Ids(Vec<u32>),
    /// A list of `(u32, u32)` pairs (`(pred, obj)` / `(pred, sub)` / `(sub, obj)`).
    Pairs(Vec<(u32, u32)>),
    /// A server-side error message.
    Error(String),

    // --- Raft replication / cluster control replies. ---
    /// Reply to [`Request::RequestVote`].
    Vote { term: u64, granted: bool },
    /// Reply to [`Request::AppendEntries`]: `match_index` is the highest log
    /// index the follower now agrees with the leader on (0 on rejection-hint).
    AppendAck {
        term: u64,
        success: bool,
        match_index: u64,
    },
    /// Reply to [`Request::InstallSnapshot`].
    SnapshotAck { term: u64 },
    /// Reply to [`Request::ClientWrite`]: `ok` if appended by the leader;
    /// `leader_hint` is the known leader id (`-1` if unknown) for a redirect;
    /// `index` is the assigned log index when `ok`.
    WriteAck {
        ok: bool,
        leader_hint: i64,
        index: u64,
    },
    /// Reply to [`Request::ClusterStatus`]: `role` is `0` follower, `1`
    /// candidate, `2` leader.
    Status {
        term: u64,
        role: u8,
        leader_hint: i64,
        commit_index: u64,
        last_log_index: u64,
    },
}

impl Response {
    /// Serialize to a self-describing byte payload (without the frame length).
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16);
        match self {
            Response::Bool(b) => {
                buf.push(1);
                buf.push(u8::from(*b));
            }
            Response::Count(n) => {
                buf.push(2);
                put_u64(&mut buf, *n);
            }
            Response::Ids(ids) => {
                buf.push(3);
                put_ids(&mut buf, ids);
            }
            Response::Pairs(pairs) => {
                buf.push(4);
                put_pairs(&mut buf, pairs);
            }
            Response::Error(msg) => {
                buf.push(5);
                put_str(&mut buf, msg);
            }
            Response::Vote { term, granted } => {
                buf.push(6);
                put_u64(&mut buf, *term);
                buf.push(u8::from(*granted));
            }
            Response::AppendAck {
                term,
                success,
                match_index,
            } => {
                buf.push(7);
                put_u64(&mut buf, *term);
                buf.push(u8::from(*success));
                put_u64(&mut buf, *match_index);
            }
            Response::SnapshotAck { term } => {
                buf.push(8);
                put_u64(&mut buf, *term);
            }
            Response::WriteAck {
                ok,
                leader_hint,
                index,
            } => {
                buf.push(9);
                buf.push(u8::from(*ok));
                put_i64(&mut buf, *leader_hint);
                put_u64(&mut buf, *index);
            }
            Response::Status {
                term,
                role,
                leader_hint,
                commit_index,
                last_log_index,
            } => {
                buf.push(10);
                put_u64(&mut buf, *term);
                buf.push(*role);
                put_i64(&mut buf, *leader_hint);
                put_u64(&mut buf, *commit_index);
                put_u64(&mut buf, *last_log_index);
            }
        }
        buf
    }

    /// Parse a [`Response`] from a payload produced by [`Response::encode`].
    pub fn decode(payload: &[u8]) -> io::Result<Response> {
        let mut r = Reader::new(payload);
        let tag = r.u8()?;
        let resp = match tag {
            1 => Response::Bool(r.u8()? != 0),
            2 => Response::Count(r.u64()?),
            3 => Response::Ids(r.ids()?),
            4 => Response::Pairs(r.pairs()?),
            5 => Response::Error(r.string()?),
            6 => Response::Vote {
                term: r.u64()?,
                granted: r.u8()? != 0,
            },
            7 => Response::AppendAck {
                term: r.u64()?,
                success: r.u8()? != 0,
                match_index: r.u64()?,
            },
            8 => Response::SnapshotAck { term: r.u64()? },
            9 => Response::WriteAck {
                ok: r.u8()? != 0,
                leader_hint: r.i64()?,
                index: r.u64()?,
            },
            10 => Response::Status {
                term: r.u64()?,
                role: r.u8()?,
                leader_hint: r.i64()?,
                commit_index: r.u64()?,
                last_log_index: r.u64()?,
            },
            _ => return Err(bad("unknown rpc response tag")),
        };
        Ok(resp)
    }

    /// Coerce to a boolean, mapping [`Response::Error`] (and any unexpected
    /// shape) to an [`io::Error`].
    pub fn into_bool(self) -> io::Result<bool> {
        match self {
            Response::Bool(b) => Ok(b),
            Response::Error(e) => Err(remote_err(e)),
            _ => Err(bad("expected a Bool response")),
        }
    }

    /// Coerce to a count.
    pub fn into_count(self) -> io::Result<u64> {
        match self {
            Response::Count(n) => Ok(n),
            Response::Error(e) => Err(remote_err(e)),
            _ => Err(bad("expected a Count response")),
        }
    }

    /// Coerce to a list of ids.
    pub fn into_ids(self) -> io::Result<Vec<u32>> {
        match self {
            Response::Ids(v) => Ok(v),
            Response::Error(e) => Err(remote_err(e)),
            _ => Err(bad("expected an Ids response")),
        }
    }

    /// Coerce to a list of pairs.
    pub fn into_pairs(self) -> io::Result<Vec<(u32, u32)>> {
        match self {
            Response::Pairs(v) => Ok(v),
            Response::Error(e) => Err(remote_err(e)),
            _ => Err(bad("expected a Pairs response")),
        }
    }
}

fn remote_err(msg: String) -> io::Error {
    io::Error::other(format!("remote shard error: {msg}"))
}

// ---------------------------------------------------------------------------
// Client round-trip.
// ---------------------------------------------------------------------------

/// Send one [`Request`] and read back one [`Response`] over a duplex stream
/// (a [`TcpStream`](std::net::TcpStream) in practice): frame-encode the request,
/// then frame-decode the reply.
pub fn round_trip<S: Read + Write>(stream: &mut S, req: &Request) -> io::Result<Response> {
    write_message(stream, &req.encode())?;
    let bytes = read_message(stream)?;
    Response::decode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    /// The framing layer round-trips arbitrary payloads, in order, over a
    /// purely in-memory buffer (no sockets).
    #[test]
    fn framing_round_trips_in_memory() {
        let mut buf: Vec<u8> = Vec::new();
        let payloads: [&[u8]; 4] = [b"", b"\x00\x01\x02", b"hello rpc", &[0xFFu8; 1000]];
        for p in payloads {
            write_message(&mut buf, p).unwrap();
        }
        let mut cur = Cursor::new(buf);
        for p in payloads {
            assert_eq!(read_message(&mut cur).unwrap(), p);
        }
        // Nothing left to read → clean EOF.
        assert_eq!(
            read_message(&mut cur).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn oversize_frame_is_rejected() {
        // A length prefix beyond MAX_FRAME must error rather than allocate.
        let mut bytes = ((MAX_FRAME as u32) + 1).to_be_bytes().to_vec();
        bytes.push(0);
        let mut cur = Cursor::new(bytes);
        assert_eq!(
            read_message(&mut cur).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    /// Every request variant survives an encode → decode round-trip unchanged.
    #[test]
    fn request_codec_round_trips_all_variants() {
        let reqs = [
            Request::Exists {
                sub: 1,
                pred: 2,
                obj: 3,
            },
            Request::PoByS { sub: 7 },
            Request::OBySp { sub: 7, pred: 8 },
            Request::PBySo { sub: 7, obj: 9 },
            Request::PsByO { obj: 9 },
            Request::SByPo { pred: 2, obj: 9 },
            Request::SoByP { pred: 2 },
            Request::SubsByP { pred: 2 },
            Request::ObjsByP { pred: 2 },
            Request::SubjectKeys,
            Request::ObjectKeys,
            Request::Predicates,
            Request::TripleCount,
            Request::PredCard { pred: 2 },
            Request::Insert {
                sub: 1,
                pred: 2,
                obj: 2_000_000_000,
            },
            Request::RequestVote {
                term: 9,
                candidate_id: 2,
                last_log_index: 7,
                last_log_term: 8,
            },
            Request::AppendEntries {
                term: 9,
                leader_id: 1,
                prev_log_index: 3,
                prev_log_term: 8,
                leader_commit: 2,
                entries: vec![
                    LogEntry::new(8, LogOp::Insert(IdTriple::new(1, 2, 3))),
                    LogEntry::new(9, LogOp::Delete(IdTriple::new(4, 5, 6))),
                ],
            },
            Request::AppendEntries {
                term: 9,
                leader_id: 1,
                prev_log_index: 0,
                prev_log_term: 0,
                leader_commit: 0,
                entries: vec![],
            },
            Request::InstallSnapshot {
                term: 9,
                leader_id: 1,
                last_included_index: 5,
                last_included_term: 8,
                triples: vec![IdTriple::new(1, 2, 3), IdTriple::new(4, 5, 6)],
            },
            Request::ClientWrite {
                op: LogOp::Insert(IdTriple::new(7, 8, 9)),
            },
            Request::ClusterStatus,
        ];
        for req in reqs {
            let decoded = Request::decode(&req.encode()).unwrap();
            assert_eq!(decoded, req);
        }
    }

    /// Every response variant survives an encode → decode round-trip unchanged.
    #[test]
    fn response_codec_round_trips_all_variants() {
        let resps = [
            Response::Bool(true),
            Response::Bool(false),
            Response::Count(0),
            Response::Count(u64::MAX),
            Response::Ids(vec![]),
            Response::Ids(vec![1, 2, u32::MAX]),
            Response::Pairs(vec![]),
            Response::Pairs(vec![(1, 2), (3, 4)]),
            Response::Error("boom".to_string()),
            Response::Vote {
                term: 4,
                granted: true,
            },
            Response::Vote {
                term: 4,
                granted: false,
            },
            Response::AppendAck {
                term: 4,
                success: true,
                match_index: 12,
            },
            Response::SnapshotAck { term: 4 },
            Response::WriteAck {
                ok: true,
                leader_hint: -1,
                index: 0,
            },
            Response::WriteAck {
                ok: false,
                leader_hint: 2,
                index: 9,
            },
            Response::Status {
                term: 4,
                role: 2,
                leader_hint: 1,
                commit_index: 8,
                last_log_index: 9,
            },
        ];
        for resp in resps {
            let decoded = Response::decode(&resp.encode()).unwrap();
            assert_eq!(decoded, resp);
        }
    }

    #[test]
    fn truncated_payload_is_an_error_not_a_panic() {
        // A PoByS needs 4 bytes of subject after the tag; give it 2.
        let bytes = vec![2u8, 0x00, 0x01];
        assert!(Request::decode(&bytes).is_err());
        // Unknown tag is rejected too.
        assert!(Request::decode(&[200u8]).is_err());
        assert!(Response::decode(&[200u8]).is_err());
    }

    /// A real client↔server loopback over TCP: a background thread echoes a
    /// canned answer per request kind; the client round-trips each request and
    /// checks the framing + codec end to end across a socket.
    #[test]
    fn loopback_client_server_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Serve requests on this one connection until the client hangs up.
            while let Ok(bytes) = read_message(&mut stream) {
                let req = Request::decode(&bytes).unwrap();
                let resp = match req {
                    Request::Exists { .. } => Response::Bool(true),
                    Request::TripleCount => Response::Count(42),
                    Request::SubjectKeys => Response::Ids(vec![10, 20, 30]),
                    Request::PoByS { sub } => Response::Pairs(vec![(sub, sub + 1)]),
                    Request::Insert { .. } => Response::Bool(true),
                    _ => Response::Error("unsupported".to_string()),
                };
                write_message(&mut stream, &resp.encode()).unwrap();
            }
        });

        let mut client = TcpStream::connect(addr).unwrap();
        assert_eq!(
            round_trip(
                &mut client,
                &Request::Exists {
                    sub: 1,
                    pred: 2,
                    obj: 3
                }
            )
            .unwrap(),
            Response::Bool(true)
        );
        assert_eq!(
            round_trip(&mut client, &Request::TripleCount).unwrap(),
            Response::Count(42)
        );
        assert_eq!(
            round_trip(&mut client, &Request::SubjectKeys)
                .unwrap()
                .into_ids()
                .unwrap(),
            vec![10, 20, 30]
        );
        assert_eq!(
            round_trip(&mut client, &Request::PoByS { sub: 5 })
                .unwrap()
                .into_pairs()
                .unwrap(),
            vec![(5, 6)]
        );
        // An unsupported op comes back as a typed remote error.
        assert!(round_trip(&mut client, &Request::Predicates)
            .unwrap()
            .into_ids()
            .is_err());

        drop(client); // let the server thread's read loop end
        server.join().unwrap();
    }
}
