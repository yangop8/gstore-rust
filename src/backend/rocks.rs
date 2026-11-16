//! A persistent RocksDB-backed triple store.
//!
//! Mirrors the in-memory [`TripleStore`](crate::store::TripleStore): the same
//! three redundant orderings answer every triple-pattern shape, but the data
//! lives on disk in RocksDB column families and survives process restarts.
//!
//! | column family | composite key (big-endian) | answers                          |
//! |---------------|----------------------------|----------------------------------|
//! | `spo`         | `s ‖ p ‖ o`                | `exists`, `po_by_s`, `o_by_sp`   |
//! | `pos`         | `p ‖ o ‖ s`                | `s_by_po`, `so_by_p`, `*_by_p`   |
//! | `osp`         | `o ‖ s ‖ p`                | `ps_by_o`, `p_by_so`, `object_keys` |
//! | `stats`       | tagged counter keys        | optimizer statistics             |
//! | `dict`        | `space ‖ id`               | the string dictionary (`id → key`) |
//! | `gspo`        | `g ‖ s ‖ p ‖ o`            | named-graph quads                |
//! | `meta`        | UTF-8 name                 | database name, layout markers    |
//!
//! Each id component is a 4-byte **big-endian** `u32`, so RocksDB's byte order
//! equals numeric order and a prefix range scan over an ordering returns exactly
//! the matching triples, already sorted — the disk analogue of `TripleStore`'s
//! sorted adjacency vectors. Values are empty: the key *is* the triple.
//!
//! ## Optimizer statistics: maintained counters (the `stats` CF)
//!
//! The cost-based optimizer calls `triple_count` / `distinct_subjects` /
//! `distinct_objects` / `num_predicates` / `pred_card` / `pred_distinct_subj` /
//! `pred_distinct_obj` on the hot planning path, so they must be **O(1)**, not
//! range scans. We therefore *maintain* them by reference counting on every
//! insert/remove, persisted in the `stats` CF (and cached in memory for the four
//! global ones). Each logical statistic is backed by a tagged counter key:
//!
//! | tag            | key bytes        | meaning                                   |
//! |----------------|------------------|-------------------------------------------|
//! | `T`/`P`/`S`/`O`| 1 byte           | triples / distinct preds / subjects / objs (global) |
//! | `c` + `p`      | 5 bytes          | `pred_card(p)`                            |
//! | `u` + `p`      | 5 bytes          | `pred_distinct_subj(p)`                    |
//! | `v` + `p`      | 5 bytes          | `pred_distinct_obj(p)`                     |
//! | `s` + `s`      | 5 bytes          | per-subject triple count (transition aid) |
//! | `o` + `o`      | 5 bytes          | per-object triple count (transition aid)  |
//! | `x` + `p` + `s`| 9 bytes          | per-`(pred,subj)` count (transition aid)  |
//! | `y` + `p` + `o`| 9 bytes          | per-`(pred,obj)` count (transition aid)   |
//!
//! A distinct-count is bumped only when its underlying per-key counter crosses
//! `0 ↔ 1`; counters that hit `0` are deleted, so `subject_keys`/`object_keys`/
//! `predicates` can be read by scanning the relevant `stats` prefix. This trades
//! a handful of extra `stats` writes per triple for O(1), *exact* statistics
//! (as the disk B+ tree store maintains its own exact counts).
//!
//! ## Durability of the dictionary and named graphs
//!
//! A RocksDB-backed [`Database`](crate::Database) keeps its whole persistent
//! state in this one RocksDB instance: besides the triple orderings, the `dict`
//! CF holds every dictionary term (`E`/`L`/`P` space tag + 4-byte big-endian
//! id → the term's dictionary key) and the `gspo` CF holds the named-graph
//! quads. New terms are written *before* the first triple that references them
//! (a triple whose term is missing would be damage), so the state RocksDB
//! recovers after a crash is always self-consistent and nothing depends on a
//! separately-saved `dict.bin`. While a journal records (below), the new
//! terms are journaled too, so a crash between a term and its triple does
//! not leave the term behind as an orphan.
//!
//! ## Transactions and statements: the undo journal
//!
//! A `Database` transaction writes straight through to RocksDB (queries inside
//! it read their own writes), so to keep it crash-atomic every mutation made
//! while one is open ([`begin_journal`](RocksStore::begin_journal)) carries, *in
//! the same atomic write batch*, an undo record in the `meta` CF: the inverse
//! operation, keyed by a sequence number. The `Database` journals every
//! auto-commit statement the same way — a SPARQL UPDATE request (`LOAD`,
//! `CLEAR`, `DROP`, `COPY`, `MOVE`, `ADD` included) and an `insert_sources` /
//! `remove_sources` import (`gadd`, `gsub`, `/api batchInsert` /
//! `batchRemove`) — so that a statement is crash-atomic as a whole: never a
//! prefix of it (half of a DELETE/INSERT rename), whatever its size. (The
//! disk layout's commit point writes a statement as one only while its dirty
//! pages fit the page cache; a larger one is committed in several batches
//! and a crash can leave a prefix of it, D12.) A failed statement's journal
//! is committed too, and so is a panicking one's: what it applied before
//! failing stays applied, as documented.
//!
//! Commit deletes the whole journal in one batch, together with the
//! dictionary terms the transaction or statement freed
//! ([`commit_journal_with`](RocksStore::commit_journal_with)); until then a
//! freed id is neither deleted nor reused (the `Database` defers its frees),
//! since the triples a replay restores may still name it. Rollback replays
//! it newest-first and deletes it in one batch with the terms that are
//! unreferenced afterwards ([`rollback_keeping_journal`](RocksStore::rollback_keeping_journal)).
//!
//! If the process dies with a journal present (an uncommitted transaction or
//! statement, or a rollback cut short), the next open undoes it before
//! anything else — so after a crash no uncommitted write survives — in
//! bounded chunks: the newest 65,536 records (`REPLAY_CHUNK`) at a time, read with a
//! reverse iterator, are undone, then deleted from the journal in one batch
//! that also writes their reclamation candidates (the terms of the triples
//! and quads they removed, and the terms recorded as written new) to the
//! pending range of the `meta` CF (`\0reclaim-pending\0` ‖ space tag ‖
//! be32(id), one key per term). The journal left on disk is thus always an
//! older prefix, and the open holds one chunk of records at a time however
//! long the journal is (an auto-commit `gadd` journals a record per triple
//! and per new term). Replaying is idempotent (each record sets one key
//! present or absent), so a chunk cut short is simply repeated. The
//! `Database` then checks the pending candidates in bounded batches and
//! deletes each batch from the range in one batch with the terms of it
//! nothing references ([`pending_reclaim`](RocksStore::pending_reclaim),
//! [`settle_pending`](RocksStore::settle_pending)) before it serves the
//! database. Each step of a crash recovery is therefore complete or
//! repeated: no window leaves an orphan term. (A binary that predates the
//! pending range ignores it: the candidates then stay allocated until
//! `gcompact --reclaim`.) The recovery costs about one write batch per
//! record, as the statement did.
//!
//! A record is 18 bytes: the undo op (`+` re-insert, `-` remove), a kind
//! byte, 4 bytes, then the triple's three ids. Kind `0` is a default-graph
//! triple, `1` a quad of the graph whose id the 4 bytes hold, `2` a
//! dictionary term written new (its space tag in the first of the 4 bytes,
//! its id three times). A term record replays as nothing but a reclamation
//! candidate ([`take_replayed`](RocksStore::take_replayed)); it is a `-`
//! record, so a binary that predates term records replays it as the
//! removal of the triple `(id, id, id)` — which cannot be stored at that
//! point of the history, its term being new — and frees the term the same
//! way: the journal format stays readable by every earlier binary.
//!
//! If the journal cannot be deleted or replayed (a write error), the store
//! refuses further triple and quad writes until a retry succeeds — a stale
//! journal replayed later would otherwise undo newer writes.
//!
//! ## Commit points and bounded resources
//!
//! The `Database`'s commit point fsyncs RocksDB's write-ahead log
//! ([`sync_wal`](RocksStore::sync_wal)): every write acknowledged so far
//! survives a crash or a power failure. That includes a save at the
//! database's home — the servers' `check_point` save after every write — so
//! memtables are flushed to table files only by an explicit
//! `Database::checkpoint`, a backup (a RocksDB checkpoint) and when the
//! store is closed, and RocksDB's own size limits (a full memtable, the WAL
//! total of 256 MiB). A flush per write would add no
//! durability, only a few small table files — and with them open
//! descriptors and compaction work — per write. RocksDB keeps at most
//! [`RocksTuning::max_open_files`] files open (`[storage]
//! rocksdb_max_open_files`, default [`DEFAULT_MAX_OPEN_FILES`]) and
//! four old info logs.
//!
//! ## Write errors
//!
//! [`MutableStore`] returns a plain `bool`, so a failed RocksDB write cannot be
//! returned from `insert`/`remove`. Instead the write is reported as "not
//! applied" (`false`), the in-memory counters are left untouched, and the error
//! is kept until [`RocksStore::take_error`] collects it — the `Database` facade
//! surfaces it at its next commit point. Nothing panics on an I/O failure.
//!
//! ## Reads: bounded raw-iterator scans, exact counts
//!
//! Every prefix scan runs a raw iterator whose `iterate_upper_bound` is the
//! prefix's successor, decoding each key in place (no allocation per key).
//! An iterator or point-read error is reported through
//! [`note_read_error`](crate::store::read_scope::note_read_error), so the
//! statement reading it fails instead of seeing a silently shortened list —
//! and so is a key whose length is not the one its column family's
//! decoder needs (a damaged store, or a key another tool wrote): it is
//! never sliced into a panic.
//! The contract-C1 counts come from the exact counters above
//! (`count_po_by_s` = `s|s`, `count_ps_by_o` = `o|o`, `count_o_by_sp` =
//! `x|p|s`, `count_s_by_po` = `y|p|o`, `count_so_by_p` = `c|p`) — one point
//! read, never a scan — and equal the lengths of the lists exactly. The
//! per-predicate counters `c|`/`u|`/`v|` are mirrored in memory (loaded at
//! open after the journal replay, changed only after a successful write
//! batch, reloaded after every journal replay), so `pred_card` and friends
//! never touch RocksDB. The contract order mostly comes without sorting:
//! `subs_by_p` / `objs_by_p` read the `x|p` / `y|p` keys, and `iter_all`
//! places every SPO triple straight into its predicate's slice (sized by the
//! exact `c|p`), in `(p, s, o)` order. `so_by_p` chooses by the mirrored
//! counters: a predicate whose subjects have long object runs (`c|p` ≥
//! [`SEEK_RUN`] × `u|p`) walks its `x|p` subjects with one SPO seek each;
//! any other reads its POS range and sorts it (a seek costs about as much
//! as reading tens of keys, so for short runs — LUBM's predicates average
//! 1–3 objects per subject — the scan and sort is faster; measured at univ
//! 8).
//!
//! ## Tuning (options only; the format is unchanged)
//!
//! [`RocksTuning`]: one LRU block cache shared by every column family (and
//! every store of the process with the same capacity; default
//! [`DEFAULT_BLOCK_CACHE_MB`] MiB, environment [`BLOCK_CACHE_ENV`]) and
//! whole-key bloom filters on the column families read by point lookups
//! (`spo`, `stats`, `gspo`), and the bound on open files above. None
//! changes a key, a value or the column family set: files written with or
//! without them open either way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use rocksdb::{
    checkpoint::Checkpoint, BlockBasedOptions, Cache, ColumnFamilyDescriptor, DBCompressionType,
    IngestExternalFileOptions, Options, ReadOptions, WriteBatch, DB,
};

use crate::error::{GStoreError, Result};
use crate::model::id::{is_literal_id, EntityLiteralId, PredId};
use crate::model::IdTriple;
use crate::store::read_scope::note_read_error;
use crate::store::{MutableStore, TripleSource};

pub(crate) const CF_SPO: &str = "spo";
pub(crate) const CF_POS: &str = "pos";
pub(crate) const CF_OSP: &str = "osp";
pub(crate) const CF_STATS: &str = "stats";
pub(crate) const CF_DICT: &str = "dict";
pub(crate) const CF_GSPO: &str = "gspo";
pub(crate) const CF_META: &str = "meta";
/// Every column family, in creation order.
pub(crate) const ALL_CFS: [&str; 7] = [CF_SPO, CF_POS, CF_OSP, CF_STATS, CF_DICT, CF_GSPO, CF_META];

/// `meta` CF key holding the database name (byte for byte the `Database`
/// layer's `NAME_KEY`).
pub const META_NAME_KEY: &str = "name";
/// `meta` CF key marking that the dictionary and named graphs live in their
/// column families (the `Database` layer's `LAYOUT_KEY`).
pub const META_LAYOUT_KEY: &str = "layout";
/// The value of the layout marker: RocksDB layout `"1"`.
pub const LAYOUT_VERSION: &[u8] = b"1";

/// `meta` CF key prefix of the open transaction's undo journal; each record is
/// `JOURNAL_PREFIX ‖ be64(seq)` (see the module docs). The leading NUL keeps it
/// apart from the UTF-8 names (`name`, `layout`) stored in the same CF.
const JOURNAL_PREFIX: &[u8] = b"\x00txn-undo\x00";
/// The exclusive upper bound of the journal key range.
const JOURNAL_END: &[u8] = b"\x00txn-undo\x01";
/// Journal record op: undo by re-inserting / by removing.
const UNDO_INSERT: u8 = b'+';
const UNDO_REMOVE: u8 = b'-';
/// The kind byte of a journal record: a default-graph triple, a named-graph
/// quad, or a dictionary term written new (see the module docs).
const REC_TRIPLE: u8 = 0;
const REC_QUAD: u8 = 1;
const REC_TERM: u8 = 2;
/// Up to this many records, clearing a journal deletes them one by one
/// (their keys are written again by the next journal, so the deletes
/// collapse in the memtable); a longer one is cleared with one range delete.
const POINT_CLEAR_MAX: u64 = 1024;
/// `meta` CF key prefix of the reclamation candidates a journal replay at
/// open has left (see the module docs): `PENDING_PREFIX ‖ space tag ‖
/// be32(id)`, with an empty value — one key per term, however many records
/// named it.
const PENDING_PREFIX: &[u8] = b"\x00reclaim-pending\x00";
/// The journal records a replay at open holds and undoes at once: each
/// such chunk, newest first, is deleted from the journal in one batch with
/// its reclamation candidates (see the module docs).
const REPLAY_CHUNK: usize = 65_536;

#[cfg(test)]
thread_local! {
    /// Test-only: the [`REPLAY_CHUNK`] of replays on this thread (0: the
    /// constant).
    pub(crate) static REPLAY_CHUNK_FOR_TEST: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    /// Test-only: the most journal records a replay at open on this thread
    /// held at once.
    pub(crate) static REPLAY_MAX_HELD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Test-only failpoint: a replay at open on this thread stops (as if the
    /// process died) after this many chunks.
    pub(crate) static REPLAY_STOP_AFTER_CHUNKS: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

/// The key of the pending reclamation candidate `(space, id)`.
fn pending_key(space: TermSpace, id: u32) -> Vec<u8> {
    let mut key = PENDING_PREFIX.to_vec();
    key.push(space.tag());
    key.extend_from_slice(&id.to_be_bytes());
    key
}

/// A decoded journal record: `(undo op, kind, graph or term tag, triple)`.
type JournalRecord = (u8, u8, u32, IdTriple);

/// One journal record decoded from its value (see the module docs).
fn decode_record(v: &[u8]) -> std::result::Result<JournalRecord, String> {
    let valid = v.len() == 18
        && matches!(v[0], UNDO_INSERT | UNDO_REMOVE)
        && match v[1] {
            REC_TERM => v[0] == UNDO_REMOVE && TermSpace::from_tag(v[2]).is_some(),
            _ => true,
        };
    if !valid {
        return Err("corrupt rocksdb undo journal record".to_string());
    }
    // Kind byte 1 is a quad; any other but a term record is a default-graph
    // triple (as every earlier binary read it).
    let kind = match v[1] {
        REC_QUAD | REC_TERM => v[1],
        _ => REC_TRIPLE,
    };
    let tag = match kind {
        REC_TERM => u32::from(v[2]),
        _ => de(&v[2..6]),
    };
    Ok((
        v[0],
        kind,
        tag,
        IdTriple::new(de(&v[6..10]), de(&v[10..14]), de(&v[14..18])),
    ))
}

/// The dictionary terms undoing record `(op, kind, tag, t)` may leave
/// unreferenced: those of an undone insert, and a term written new.
fn record_candidates(
    op: u8,
    kind: u8,
    tag: u32,
    t: IdTriple,
    out: &mut impl Extend<(TermSpace, u32)>,
) {
    if kind == REC_TERM {
        if let Some(space) = TermSpace::from_tag(tag as u8) {
            out.extend([(space, t.sub)]);
        }
        return;
    }
    if op == UNDO_REMOVE {
        let obj = if is_literal_id(t.obj) {
            TermSpace::Literal
        } else {
            TermSpace::Entity
        };
        out.extend([
            (TermSpace::Entity, t.sub),
            (TermSpace::Predicate, t.pred),
            (obj, t.obj),
        ]);
        if kind == REC_QUAD {
            out.extend([(TermSpace::Entity, tag)]);
        }
    }
}

/// The key of journal record `seq`.
fn journal_key(seq: u64) -> Vec<u8> {
    let mut key = JOURNAL_PREFIX.to_vec();
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

/// The length of a triple-ordering key (`spo`, `pos`, `osp`): three ids.
const KEY3_LEN: usize = 12;
/// The length of a quad key (`gspo`): four ids.
const KEY4_LEN: usize = 16;
/// The length of a single-id counter key (`c|`, `u|`, `v|`, `s|`, `o|`) and
/// of a dictionary key (`dict`): a tag and one id.
const TAG1_LEN: usize = 5;
/// The length of a pair counter key (`x|`, `y|`): a tag and two ids.
const TAG2_LEN: usize = 9;

/// A key of column family `cf` that does not have the length `len` its
/// decoder needs — a damaged store, or a key another tool wrote — as the
/// read error reporting it (never a slice-index panic).
fn check_len(cf: &str, k: &[u8], len: usize) -> std::result::Result<(), String> {
    if k.len() == len {
        return Ok(());
    }
    let shown = &k[..k.len().min(16)];
    Err(format!(
        "corrupt rocksdb key in column family '{cf}': {} byte(s) {shown:02x?}{}, expected {len}",
        k.len(),
        if k.len() > shown.len() { "…" } else { "" }
    ))
}

/// The default bound on the files RocksDB keeps open (`[storage]
/// rocksdb_max_open_files`). RocksDB's own default (unlimited) keeps a
/// descriptor per table file for as long as the store is open.
pub const DEFAULT_MAX_OPEN_FILES: i32 = 4096;
/// How many old RocksDB info logs (`LOG.old.*`) are kept: each open starts
/// a new one, and a backup copies them all.
const KEEP_LOG_FILES: usize = 4;
/// The write-ahead logs' total size at which RocksDB flushes the column
/// families that still hold the oldest log. A commit point only syncs the
/// WAL (a memtable flush adds no durability), so without a bound the logs a
/// rarely written column family keeps alive would pile up until its
/// memtable filled.
const MAX_TOTAL_WAL_BYTES: u64 = 256 << 20;

/// The environment variable setting the shared block cache, in MiB (`0`:
/// RocksDB's own default, a 32 MiB cache per column family).
pub const BLOCK_CACHE_ENV: &str = "GSTORE_ROCKS_BLOCK_CACHE_MB";
/// The default shared block cache, in MiB.
pub const DEFAULT_BLOCK_CACHE_MB: usize = 256;
/// The default bloom filter density, in bits per key.
pub const DEFAULT_BLOOM_BITS: f64 = 10.0;

/// Read-side tuning of a [`RocksStore`]: options only — keys, values and the
/// column family set are the same with any tuning (see the module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct RocksTuning {
    /// Capacity of the LRU block cache shared by every column family (and by
    /// every store of this process opened with the same capacity); `0` keeps
    /// RocksDB's default of one 32 MiB cache per column family.
    pub block_cache_bytes: usize,
    /// Bits per key of the whole-key bloom filters written for the column
    /// families read by point lookups (`spo`, `stats`, `gspo`); `0` = none.
    pub bloom_bits_per_key: f64,
    /// The most files RocksDB keeps open (table files beyond it are closed
    /// and reopened through its table cache); `-1` = unlimited, RocksDB's
    /// own default (one descriptor per table file).
    pub max_open_files: i32,
}

impl Default for RocksTuning {
    /// [`RocksTuning::from_env`].
    fn default() -> Self {
        RocksTuning::from_env()
    }
}

/// The tuning installed by [`set_default_rocks_tuning`] (`conf.ini`'s
/// `[storage]` keys in a server), if any.
static INSTALLED_TUNING: Mutex<Option<RocksTuning>> = Mutex::new(None);

/// Install the tuning stores of this process open with (a server installs
/// `conf.ini`'s `[storage] rocksdb_block_cache_mb` / `rocksdb_bloom` /
/// `rocksdb_max_open_files`); the environment ([`BLOCK_CACHE_ENV`]) still
/// overrides the block cache.
pub fn set_default_rocks_tuning(tuning: RocksTuning) {
    *INSTALLED_TUNING
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(tuning);
}

impl RocksTuning {
    /// The defaults — the installed tuning ([`set_default_rocks_tuning`]),
    /// else [`DEFAULT_BLOCK_CACHE_MB`], [`DEFAULT_BLOOM_BITS`] and
    /// [`DEFAULT_MAX_OPEN_FILES`] — with the
    /// block cache taken from [`BLOCK_CACHE_ENV`] when it is set to a number
    /// of MiB (the environment wins, as for the other storage settings). A
    /// value that is not one is logged (once per process) and ignored.
    pub fn from_env() -> RocksTuning {
        let installed = INSTALLED_TUNING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let base = installed.unwrap_or(RocksTuning {
            block_cache_bytes: DEFAULT_BLOCK_CACHE_MB << 20,
            bloom_bits_per_key: DEFAULT_BLOOM_BITS,
            max_open_files: DEFAULT_MAX_OPEN_FILES,
        });
        let env = std::env::var(BLOCK_CACHE_ENV).ok();
        let (mb, warning) = block_cache_mb(env.as_deref());
        if let Some(w) = warning {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| crate::gs_warn!("rocksdb tuning: {w}"));
        }
        // Only a valid value overrides.
        let env_set = env
            .as_deref()
            .map(str::trim)
            .is_some_and(|v| v.parse::<usize>().is_ok());
        RocksTuning {
            block_cache_bytes: if env_set {
                mb.saturating_mul(1 << 20)
            } else {
                base.block_cache_bytes
            },
            bloom_bits_per_key: base.bloom_bits_per_key,
            max_open_files: base.max_open_files,
        }
    }

    /// RocksDB's own defaults (per-column-family caches, no filters, no
    /// bound on open files): the options of stores before Phase 6.
    pub fn rocksdb_defaults() -> RocksTuning {
        RocksTuning {
            block_cache_bytes: 0,
            bloom_bits_per_key: 0.0,
            max_open_files: -1,
        }
    }
}

/// The process-wide LRU block cache of `bytes` capacity, created on first
/// use and shared by every store opened with that capacity.
fn shared_cache(bytes: usize) -> Cache {
    static CACHES: OnceLock<Mutex<Vec<(usize, Cache)>>> = OnceLock::new();
    let mut caches = CACHES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some((_, c)) = caches.iter().find(|(b, _)| *b == bytes) {
        return c.clone();
    }
    let cache = Cache::new_lru_cache(bytes);
    caches.push((bytes, cache.clone()));
    cache
}

/// The block cache size, in MiB, that [`BLOCK_CACHE_ENV`]'s value `v` asks
/// for, and a warning when it is set but is not a number of MiB (the default
/// is used then: an operator capping the cache with `64M` must learn that
/// the cap was not applied).
fn block_cache_mb(v: Option<&str>) -> (usize, Option<String>) {
    match v.map(str::trim).filter(|v| !v.is_empty()) {
        None => (DEFAULT_BLOCK_CACHE_MB, None),
        Some(t) => match t.parse::<usize>() {
            Ok(mb) => (mb, None),
            Err(_) => (
                DEFAULT_BLOCK_CACHE_MB,
                Some(format!(
                    "{BLOCK_CACHE_ENV}={t:?} is not a number of MiB; \
                     the default of {DEFAULT_BLOCK_CACHE_MB} MiB is used"
                )),
            ),
        },
    }
}

/// The options of column family `name`: LZ4 for the triple orderings, and
/// the shared block cache and bloom filters of `tuning`. The bulk build
/// writes its SST files with exactly these options.
pub(crate) fn cf_options(name: &str, tuning: &RocksTuning) -> Options {
    let mut o = Options::default();
    if matches!(name, CF_SPO | CF_POS | CF_OSP | CF_GSPO) {
        // Byte order = numeric order comes from the big-endian keys; just
        // enable a space-saving compressor.
        o.set_compression_type(DBCompressionType::Lz4);
    }
    let cache = tuning.block_cache_bytes > 0;
    let bloom = tuning.bloom_bits_per_key > 0.0 && matches!(name, CF_SPO | CF_STATS | CF_GSPO);
    if cache || bloom {
        let mut t = BlockBasedOptions::default();
        if cache {
            t.set_block_cache(&shared_cache(tuning.block_cache_bytes));
        }
        if bloom {
            // A full (not block-based) filter over whole keys.
            t.set_bloom_filter(tuning.bloom_bits_per_key, false);
        }
        o.set_block_based_table_factory(&t);
    }
    o
}

/// The state of the undo journal (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Journal {
    /// No transaction: mutations are not journaled.
    Off,
    /// A transaction is open; the next record gets this sequence number.
    Recording(u64),
    /// The journal of an ended transaction is still on disk because deleting
    /// (`replay == false`: it committed) or replaying (`replay == true`: it
    /// rolled back) it failed. Triple/quad writes are refused until a retry
    /// ([`RocksStore::heal_journal`]) succeeds.
    Stale { replay: bool, why: String },
}

/// Which id space a dictionary term belongs to (the `dict` CF key tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermSpace {
    /// IRIs and blank nodes (ids `< LITERAL_FIRST_ID`).
    Entity,
    /// Literals (public ids `>= LITERAL_FIRST_ID`).
    Literal,
    /// Predicates (their own id space).
    Predicate,
}

impl TermSpace {
    /// The `dict` CF key tag of this space.
    pub(crate) fn tag(self) -> u8 {
        match self {
            TermSpace::Entity => b'E',
            TermSpace::Literal => b'L',
            TermSpace::Predicate => b'P',
        }
    }

    fn from_tag(tag: u8) -> Option<TermSpace> {
        match tag {
            b'E' => Some(TermSpace::Entity),
            b'L' => Some(TermSpace::Literal),
            b'P' => Some(TermSpace::Predicate),
            _ => None,
        }
    }
}

// Global counter keys (single byte).
pub(crate) const K_TRIPLES: &[u8] = b"T";
pub(crate) const K_PREDS: &[u8] = b"P";
pub(crate) const K_SUBJECTS: &[u8] = b"S";
pub(crate) const K_OBJECTS: &[u8] = b"O";

// Tagged per-key counter prefixes.
pub(crate) const T_CARD: u8 = b'c'; // c|p          -> pred_card(p)
pub(crate) const T_PSUBJ: u8 = b'u'; // u|p         -> pred_distinct_subj(p)
pub(crate) const T_POBJ: u8 = b'v'; // v|p          -> pred_distinct_obj(p)
pub(crate) const T_SUB: u8 = b's'; // s|s          -> per-subject count
pub(crate) const T_OBJ: u8 = b'o'; // o|o          -> per-object count
pub(crate) const T_PS: u8 = b'x'; // x|p|s         -> per-(pred,subj) count
pub(crate) const T_PO: u8 = b'y'; // y|p|o         -> per-(pred,obj) count

/// The in-memory mirror of one predicate's `c|`/`u|`/`v|` counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PredCounts {
    /// `c|p`: triples with the predicate.
    card: u64,
    /// `u|p`: its distinct subjects.
    subjects: u64,
    /// `v|p`: its distinct objects.
    objects: u64,
}

/// A persistent, RocksDB-backed triple store. Implements the same
/// [`TripleSource`] + [`MutableStore`] seam as [`TripleStore`](crate::store::TripleStore),
/// so it is a drop-in [`StorageBackend`](crate::store::StorageBackend).
pub struct RocksStore {
    db: DB,
    // In-memory caches of the four global counters (reloaded from the `stats` CF
    // on open) so the hot read path never touches disk for them.
    triple_count: u64,
    num_predicates: u64,
    distinct_subjects: u64,
    distinct_objects: u64,
    /// The mirror of the `c|`/`u|`/`v|` counters, by predicate (see the
    /// module docs); predicates without triples are absent.
    preds: BTreeMap<PredId, PredCounts>,
    /// The first write failure not yet collected by [`take_error`](Self::take_error).
    error: Mutex<Option<String>>,
    /// The transaction undo journal (see the module docs).
    journal: Mutex<Journal>,
    /// Terms of the triples and quads a journal replay removed (see
    /// [`take_replayed`](Self::take_replayed)).
    replayed: Mutex<Vec<(TermSpace, u32)>>,
    /// Whether the `meta` CF may hold pending reclamation candidates (left
    /// by a journal replay at open; see
    /// [`pending_reclaim`](Self::pending_reclaim)).
    pending: AtomicBool,
    /// Every read fails (see [`RocksStore::debug_fail_reads`]).
    fail_reads: AtomicBool,
    /// The write batches written since a write hook was installed, and the
    /// hook (see [`RocksStore::debug_set_write_hook`]).
    writes: AtomicU64,
    write_hook: Mutex<Option<WriteHook>>,
    /// Test-only failpoints (see [`RocksStore::inject_failures`]).
    #[cfg(test)]
    failpoints: Failpoints,
}

/// A hook run after each write batch a store writes, with the number of
/// batches written since it was installed (see
/// [`RocksStore::debug_set_write_hook`]).
pub type WriteHook = Arc<dyn Fn(u64) + Send + Sync>;

/// Test-only write-failure injection, standing in for e.g. `ENOSPC`.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Failpoints {
    /// Writes to the `dict` and `gspo` column families fail.
    pub(crate) side_writes: std::sync::atomic::AtomicBool,
    /// Deleting the undo journal fails.
    pub(crate) journal_clear: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl Failpoints {
    fn hit(flag: &std::sync::atomic::AtomicBool, what: &str) -> Result<()> {
        if flag.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(GStoreError::Database(format!(
                "rocksdb write ({what}): injected failure (No space left on device)"
            )));
        }
        Ok(())
    }
}

impl std::fmt::Debug for RocksStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksStore")
            .field("path", &self.db.path())
            .field("triple_count", &self.triple_count)
            .finish()
    }
}

/// The value of a counter key (`be64`); `None` for a value of another size.
fn counter_value(v: &[u8]) -> Option<u64> {
    let b: [u8; 8] = v.try_into().ok()?;
    Some(u64::from_be_bytes(b))
}

/// The smallest key greater than every key starting with `prefix` (the
/// exclusive upper bound of a prefix scan); `None` when there is none (an
/// empty or all-`0xFF` prefix).
pub(crate) fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xFF {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

/// A scan's read error, or the error a visitor stopped it with.
enum ScanError {
    Read(String),
    Visit(String),
}

impl ScanError {
    /// As a [`GStoreError`]: a read error names what was read.
    fn into_error(self, what: &str) -> GStoreError {
        match self {
            ScanError::Read(e) => GStoreError::Database(format!("rocksdb read ({what}): {e}")),
            ScanError::Visit(e) => GStoreError::Database(e),
        }
    }
}

/// After how many keys of one predicate `distinct_p_of_s` seeks past it
/// instead of stepping on.
const SKIP_AFTER: u32 = 8;

/// `so_by_p` walks a predicate's subjects with SPO seeks when it has at
/// least this many triples per distinct subject (see the module docs).
pub(crate) const SEEK_RUN: u64 = 32;

impl RocksStore {
    /// Open (creating if absent) a RocksDB triple store at directory `path`,
    /// with the default [`RocksTuning`] (shared block cache, bloom filters).
    /// Persisted data and maintained statistics are reloaded.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<RocksStore> {
        RocksStore::open_with(path, &RocksTuning::default())
    }

    /// [`open`](Self::open) with explicit tuning.
    ///
    /// A journal left behind (by a process that died in a transaction or
    /// statement) is undone first, in bounded chunks; the dictionary terms
    /// the undo may have left unreferenced are kept in the pending range
    /// until they are checked and freed ([`pending_reclaim`](Self::pending_reclaim),
    /// [`settle_pending`](Self::settle_pending), or
    /// [`take_replayed`](Self::take_replayed)), so no crash leaves them
    /// behind as orphans (see the module docs).
    pub fn open_with<P: AsRef<Path>>(path: P, tuning: &RocksTuning) -> Result<RocksStore> {
        RocksStore::open_inner(path.as_ref(), tuning)
    }

    fn open_inner(path: &Path, tuning: &RocksTuning) -> Result<RocksStore> {
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);
        // Bounded resources (options only, the files are the same): open
        // table files, old info logs, and the WAL kept alive by column
        // families that are written rarely (see the constants).
        db_opts.set_max_open_files(tuning.max_open_files);
        db_opts.set_keep_log_file_num(KEEP_LOG_FILES);
        db_opts.set_max_total_wal_size(MAX_TOTAL_WAL_BYTES);

        let cfs = ALL_CFS
            .iter()
            .map(|name| ColumnFamilyDescriptor::new(*name, cf_options(name, tuning)))
            .collect::<Vec<_>>();
        let db = DB::open_cf_descriptors(&db_opts, path, cfs)
            .map_err(|e| GStoreError::Database(format!("rocksdb open: {e}")))?;

        let mut store = RocksStore {
            db,
            triple_count: 0,
            num_predicates: 0,
            distinct_subjects: 0,
            distinct_objects: 0,
            preds: BTreeMap::new(),
            error: Mutex::new(None),
            journal: Mutex::new(Journal::Off),
            replayed: Mutex::new(Vec::new()),
            pending: AtomicBool::new(true),
            fail_reads: AtomicBool::new(false),
            writes: AtomicU64::new(0),
            write_hook: Mutex::new(None),
            #[cfg(test)]
            failpoints: Failpoints::default(),
        };
        store.load_counters()?;
        // A journal left behind is an uncommitted transaction or statement
        // (or a rollback cut short) of a process that died: undo it before
        // anything reads, in bounded chunks whose candidates are kept in the
        // pending range (see the module docs). The replay reloads the
        // counters once it has run.
        store.recover_journal()?;
        // Whether candidates wait (also ones an earlier open left): read
        // once here, cleared once they are settled.
        store.pending_reclaim(1)?;
        store.sync_wal()?;
        Ok(store)
    }

    /// (Re)load the cached global counters and the `c|`/`u|`/`v|` mirror from
    /// the `stats` CF.
    fn load_counters(&mut self) -> Result<()> {
        let global = |key: &[u8]| -> Result<u64> {
            self.get_counter(key)
                .map_err(|e| GStoreError::Database(format!("rocksdb read (stats): {e}")))
        };
        let (t, p, s, o) = (
            global(K_TRIPLES)?,
            global(K_PREDS)?,
            global(K_SUBJECTS)?,
            global(K_OBJECTS)?,
        );
        let mut preds: BTreeMap<PredId, PredCounts> = BTreeMap::new();
        for tag in [T_CARD, T_PSUBJ, T_POBJ] {
            self.try_scan(CF_STATS, &[tag], |k, v| {
                let (true, Some(n)) = (k.len() == 5, counter_value(v)) else {
                    return Err(format!("corrupt rocksdb counter {k:?}"));
                };
                let c = preds.entry(de(&k[1..5])).or_default();
                match tag {
                    T_CARD => c.card = n,
                    T_PSUBJ => c.subjects = n,
                    _ => c.objects = n,
                }
                Ok(true)
            })
            .map_err(|e| e.into_error("stats"))?;
        }
        preds.retain(|_, c| c.card > 0);
        self.triple_count = t;
        self.num_predicates = p;
        self.distinct_subjects = s;
        self.distinct_objects = o;
        self.preds = preds;
        Ok(())
    }

    /// Flush memtables to SST so all data is durable on disk (every column
    /// family: triples, statistics, dictionary, named graphs and metadata).
    pub fn flush(&self) -> Result<()> {
        for name in ALL_CFS {
            self.db
                .flush_cf(self.cf(name))
                .map_err(|e| GStoreError::Database(format!("rocksdb flush: {e}")))?;
        }
        Ok(())
    }

    /// Fsync RocksDB's write-ahead log: every write acknowledged so far survives
    /// a power failure, not only a process crash. Cheaper than [`flush`](Self::flush)
    /// (no memtable → SST compaction); the `Database` facade calls it at each
    /// commit point.
    pub fn sync_wal(&self) -> Result<()> {
        self.db
            .flush_wal(true)
            .map_err(|e| GStoreError::Database(format!("rocksdb WAL sync: {e}")))
    }

    /// The directory this RocksDB store lives in.
    pub fn path(&self) -> PathBuf {
        self.db.path().to_path_buf()
    }

    /// Write a consistent, openable point-in-time copy of the whole store (all
    /// column families) into the **new** directory `dest` via a RocksDB
    /// checkpoint (hard-linked SST files where possible). `dest` must not exist.
    pub fn checkpoint_to<P: AsRef<Path>>(&self, dest: P) -> Result<()> {
        let cp = Checkpoint::new(&self.db)
            .map_err(|e| GStoreError::Database(format!("rocksdb checkpoint: {e}")))?;
        cp.create_checkpoint(dest.as_ref())
            .map_err(|e| GStoreError::Database(format!("rocksdb checkpoint: {e}")))
    }

    /// Ingest the finished SST files `files` (sorted keys, written with the
    /// options [`cf_options`] gives `cf`) into column family `cf`, moving
    /// them into the store. For the bulk build, on a fresh store: the cached
    /// counters are not reloaded (reopen the store).
    pub(crate) fn ingest_sst(&self, cf: &str, files: &[PathBuf]) -> Result<()> {
        if files.is_empty() {
            return Ok(());
        }
        let mut opts = IngestExternalFileOptions::default();
        opts.set_move_files(true);
        self.db
            .ingest_external_file_cf_opts(self.cf(cf), &opts, files.to_vec())
            .map_err(|e| GStoreError::Database(format!("rocksdb ingest ({cf}): {e}")))
    }

    /// Collect (and clear) the first write error recorded since the last call.
    /// `insert`/`remove` report a failed write as "not applied" and stash the
    /// error here instead of panicking.
    pub fn take_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn record_error_msg(&self, what: &str, e: &str) {
        let mut slot = self.error.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(format!("rocksdb write ({what}): {e}"));
        }
    }

    /// Test-only: the write-failure injection switches of this store.
    #[cfg(test)]
    pub(crate) fn inject_failures(&self) -> &Failpoints {
        &self.failpoints
    }

    /// A test hook: while `on`, every read of the triple orderings and the
    /// statistics fails as an I/O error would — reported through
    /// [`note_read_error`]; lists come back empty, counts zero, existence
    /// checks `true` — and writes, which read first, fail.
    #[doc(hidden)]
    pub fn debug_fail_reads(&self, on: bool) {
        self.fail_reads.store(on, Ordering::SeqCst);
    }

    /// The injected read failure, while [`debug_fail_reads`](Self::debug_fail_reads) is on.
    #[inline]
    fn injected_read_failure(&self) -> Option<String> {
        self.fail_reads
            .load(Ordering::Relaxed)
            .then(|| "injected read failure (Input/output error)".to_string())
    }

    /// A test hook for crash tests: run `hook` after every write batch this
    /// store writes from now on (each triple or quad change, dictionary
    /// batch and journal deletion is one), with the number written since
    /// the call — so a test child can stop at an exact point of a statement
    /// (and be killed there). `None` removes it. Only the batch counter is
    /// touched when no hook is installed.
    #[doc(hidden)]
    pub fn debug_set_write_hook(&self, hook: Option<WriteHook>) {
        self.writes.store(0, Ordering::SeqCst);
        *self
            .write_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = hook;
    }

    /// Write `wb` atomically (every write of this store goes through here),
    /// then run the write hook, if one is installed.
    fn write(&self, wb: WriteBatch) -> std::result::Result<(), rocksdb::Error> {
        self.db.write(wb)?;
        let n = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
        let hook = self
            .write_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(n);
        }
        Ok(())
    }

    // ---- transaction undo journal (meta CF) -----------------------------

    fn journal_state(&self) -> std::sync::MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start journaling mutations for a transaction or an auto-commit
    /// statement (see the module docs). Fails if one is already being
    /// journaled, or if the journal of an earlier one is still stale and
    /// cannot be cleaned up now.
    pub fn begin_journal(&mut self) -> Result<()> {
        self.heal_journal()?;
        let mut j = self.journal_state();
        if *j != Journal::Off {
            return Err(GStoreError::Database(
                "rocksdb: a transaction is already being journaled".into(),
            ));
        }
        *j = Journal::Recording(0);
        Ok(())
    }

    /// Whether a transaction's mutations are currently being journaled.
    pub fn journaling(&self) -> bool {
        matches!(*self.journal_state(), Journal::Recording(_))
    }

    /// Commit the journaled transaction: delete its undo journal in one atomic
    /// batch. On failure the journal stays stale (writes are refused) until a
    /// later [`heal_journal`](Self::heal_journal) deletes it.
    pub fn commit_journal(&mut self) -> Result<()> {
        self.commit_journal_with(&[])
    }

    /// [`commit_journal`](Self::commit_journal), deleting the dictionary
    /// terms `frees` (the ids the transaction freed) in the same atomic batch
    /// as the journal: either the transaction is committed with its frees or
    /// (a crash before the batch) it is undone at the next open with its
    /// terms still stored. Without a journal the terms are deleted alone.
    ///
    /// The same call finishes a journal that was replayed but kept — by
    /// [`rollback_keeping_journal`](Self::rollback_keeping_journal) —
    /// deleting it with `frees`, the terms the replay left unreferenced.
    /// (A journal recovered at open is deleted chunk by chunk by the open
    /// itself, its candidates kept in the pending range.)
    pub fn commit_journal_with(&mut self, frees: &[(TermSpace, u32)]) -> Result<()> {
        let records = match &*self.journal_state() {
            Journal::Off => None,
            Journal::Recording(n) => Some(Some(*n)),
            Journal::Stale { replay: false, .. } => Some(None),
            Journal::Stale { replay: true, why } => {
                return Err(GStoreError::Database(format!(
                    "rocksdb: a rolled-back transaction's undo journal must be replayed first \
                     ({why})"
                )))
            }
        };
        let Some(records) = records else {
            return self.delete_terms(frees);
        };
        match self.clear_journal_with(records, frees) {
            Ok(()) => {
                *self.journal_state() = Journal::Off;
                Ok(())
            }
            Err(e) => {
                // Stale until a retry deletes it (the frees are then not
                // made; the terms stay as orphans).
                let why = e.to_string();
                *self.journal_state() = Journal::Stale {
                    replay: false,
                    why: why.clone(),
                };
                Err(GStoreError::Database(format!(
                    "rocksdb: a transaction's undo journal could not be cleared: {why}"
                )))
            }
        }
    }

    /// The dictionary ids completed journal replays left possibly
    /// unreferenced — the terms of the triples and quads they removed, and
    /// the terms the journal recorded as written new — since the last call:
    /// the replay at open (an interrupted transaction or statement undone),
    /// a rollback, or the retry that finished a replay cut short. The
    /// database takes them at open, after every statement and at every
    /// commit, and frees the ones nothing references.
    ///
    /// The candidates a replay at open left in the pending range are handed
    /// over too — all of them at once, deleted from the range here: a
    /// `Database` takes them in bounded batches instead, deleted with the
    /// terms it frees ([`pending_reclaim`](Self::pending_reclaim),
    /// [`settle_pending`](Self::settle_pending)). An error reading or
    /// deleting them leaves them pending.
    pub fn take_replayed(&self) -> Vec<(TermSpace, u32)> {
        let mut out =
            std::mem::take(&mut *self.replayed.lock().unwrap_or_else(PoisonError::into_inner));
        if self.pending.load(Ordering::SeqCst) {
            if let Ok(pending) = self.pending_reclaim(usize::MAX) {
                if self.settle_pending(&pending, &[]).is_ok() {
                    self.pending.store(false, Ordering::SeqCst);
                    out.extend(pending);
                }
            }
        }
        out
    }

    /// The first `limit` reclamation candidates (sorted by space tag, then
    /// id) a journal replay at open left in the pending range: the terms of
    /// the triples and quads it removed, and the terms the journal recorded
    /// as written new. They stay pending — a crash keeps them for the next
    /// open — until [`settle_pending`](Self::settle_pending) deletes them.
    pub fn pending_reclaim(&self, limit: usize) -> Result<Vec<(TermSpace, u32)>> {
        let mut out = Vec::new();
        if limit == 0 {
            return Ok(out);
        }
        self.try_scan(CF_META, PENDING_PREFIX, |k, _| {
            let rest = &k[PENDING_PREFIX.len()..];
            let space = (rest.len() == 5)
                .then(|| TermSpace::from_tag(rest[0]))
                .flatten()
                .ok_or_else(|| "corrupt rocksdb pending reclamation key".to_string())?;
            out.push((space, de(&rest[1..5])));
            Ok(out.len() < limit)
        })
        .map_err(|e| e.into_error("pending reclamation"))?;
        if out.is_empty() {
            // Settled: later `take_replayed` calls (one per statement of a
            // `Database`) need not look again.
            self.pending.store(false, Ordering::SeqCst);
        }
        Ok(out)
    }

    /// Delete the pending reclamation candidates `taken` (from
    /// [`pending_reclaim`](Self::pending_reclaim)) and the dictionary terms
    /// `frees` (those of them nothing references) in one atomic batch: a
    /// crash before it leaves both as they were, to be checked again.
    pub fn settle_pending(
        &self,
        taken: &[(TermSpace, u32)],
        frees: &[(TermSpace, u32)],
    ) -> Result<()> {
        let mut wb = WriteBatch::default();
        let meta = self.cf(CF_META);
        for (space, id) in taken {
            wb.delete_cf(meta, pending_key(*space, *id));
        }
        let dict = self.cf(CF_DICT);
        for (space, id) in frees {
            wb.delete_cf(dict, tag1(space.tag(), *id));
        }
        if wb.is_empty() {
            return Ok(());
        }
        self.write(wb)
            .map_err(|e| GStoreError::Database(format!("rocksdb write (pending reclamation): {e}")))
    }

    /// Roll back the journaled transaction: replay its undo journal newest
    /// first (restoring every triple and quad it changed), then delete it. On
    /// failure the journal stays stale (writes are refused) until a later
    /// [`heal_journal`](Self::heal_journal) finishes the rollback; a crash
    /// finishes it on the next [`open`](Self::open). The counter mirror is
    /// reloaded from the `stats` CF either way.
    pub fn rollback_journal(&mut self) -> Result<()> {
        if !self.journaling() {
            return Ok(());
        }
        *self.journal_state() = Journal::Stale {
            replay: true,
            why: String::new(),
        };
        self.heal_journal()
    }

    /// [`rollback_journal`](Self::rollback_journal), but the replayed
    /// journal stays on disk until
    /// [`commit_journal_with`](Self::commit_journal_with) deletes it in one
    /// batch with the terms the rollback leaves unreferenced (its
    /// [`take_replayed`](Self::take_replayed) terms that nothing references
    /// any more): a crash before that batch replays the journal again at the
    /// next open — which frees those terms then — so no crash leaves them
    /// behind as orphans. Triple and quad writes are refused in between. If
    /// the replay fails, the journal stays stale as after a failed
    /// [`rollback_journal`](Self::rollback_journal).
    pub fn rollback_keeping_journal(&mut self) -> Result<()> {
        if !self.journaling() {
            return Ok(());
        }
        self.replay_kept_journal()
    }

    /// Roll back the transaction whose commit failed to delete its undo
    /// journal ([`commit_journal_with`](Self::commit_journal_with) left it
    /// stale for deletion): replay the journal instead, keeping it as
    /// [`rollback_keeping_journal`](Self::rollback_keeping_journal) does
    /// until `commit_journal_with` deletes it with the terms the replay
    /// left unreferenced. Without this a later
    /// [`heal_journal`](Self::heal_journal) would delete the journal —
    /// committing the transaction — while a reopen before it would replay
    /// it, undoing the transaction (review gap-rocksdb-exec#5). If the
    /// replay fails, the journal stays stale for replay: the next write or
    /// open finishes the rollback, so the transaction never becomes
    /// durable. A no-op unless the journal is stale for deletion.
    pub fn rollback_uncleared_journal(&mut self) -> Result<()> {
        if !matches!(*self.journal_state(), Journal::Stale { replay: false, .. }) {
            return Ok(());
        }
        self.replay_kept_journal()
    }

    /// Replay the journal on disk, keeping it (see
    /// [`rollback_keeping_journal`](Self::rollback_keeping_journal)).
    fn replay_kept_journal(&mut self) -> Result<()> {
        // No longer recording while it replays: the replay's own writes
        // must not append records to the journal they undo (a crash replays
        // that journal again from its start anyway).
        *self.journal_state() = Journal::Stale {
            replay: true,
            why: "being rolled back".into(),
        };
        let res = self.replay_records_on_disk();
        let mut j = self.journal_state();
        match res {
            Ok(_) => {
                *j = Journal::Stale {
                    replay: false,
                    why: "rolled back; not deleted yet".into(),
                };
                Ok(())
            }
            Err(e) => {
                let why = e.to_string();
                *j = Journal::Stale {
                    replay: true,
                    why: why.clone(),
                };
                Err(GStoreError::Database(format!(
                    "rocksdb: a transaction's undo journal could not be replayed: {why}"
                )))
            }
        }
    }

    /// Retry the cleanup of a stale journal — one whose deletion (after a
    /// commit) or replay (after a rollback) failed; a no-op otherwise. Errs,
    /// keeping it stale, if the retry fails too.
    pub fn heal_journal(&mut self) -> Result<()> {
        let replay = match &*self.journal_state() {
            Journal::Stale { replay, .. } => *replay,
            _ => return Ok(()),
        };
        let res = if replay {
            self.replay_journal()
        } else {
            self.clear_journal_with(None, &[])
        };
        let mut j = self.journal_state();
        match res {
            Ok(()) => {
                *j = Journal::Off;
                Ok(())
            }
            Err(e) => {
                let why = e.to_string();
                *j = Journal::Stale {
                    replay,
                    why: why.clone(),
                };
                Err(GStoreError::Database(format!(
                    "rocksdb: a transaction's undo journal could not be {}: {why}",
                    if replay { "replayed" } else { "cleared" }
                )))
            }
        }
    }

    /// The reason triple/quad writes are refused, if they are (a stale journal).
    pub fn journal_error(&self) -> Option<String> {
        match &*self.journal_state() {
            Journal::Stale { replay, why } => Some(format!(
                "a transaction's undo journal could not be {} ({why}); writes are refused \
                 until that succeeds",
                if *replay { "replayed" } else { "cleared" }
            )),
            _ => None,
        }
    }

    /// The next journal key while a journal is recording (advancing its
    /// sequence number), else `None`.
    fn next_journal_key(&self) -> Option<Vec<u8>> {
        let mut j = self.journal_state();
        let Journal::Recording(seq) = &mut *j else {
            return None;
        };
        let key = journal_key(*seq);
        *seq += 1;
        Some(key)
    }

    /// Append the undo record of a mutation to `wb` when a transaction is being
    /// journaled. `undo` is the op that reverses it.
    fn journal_into(&self, wb: &mut WriteBatch, undo: u8, graph: Option<u32>, t: IdTriple) {
        let Some(key) = self.next_journal_key() else {
            return;
        };
        let mut val = Vec::with_capacity(18);
        val.push(undo);
        match graph {
            Some(g) => {
                val.push(REC_QUAD);
                val.extend_from_slice(&be(g));
            }
            None => {
                val.push(REC_TRIPLE);
                val.extend_from_slice(&[0; 4]);
            }
        }
        val.extend_from_slice(&key3(t.sub, t.pred, t.obj));
        wb.put_cf(self.cf(CF_META), key, val);
    }

    /// Append the record of a dictionary term written new to `wb` when a
    /// journal is recording, so that a replay hands the term over as a
    /// reclamation candidate (see the module docs). The record is an
    /// `UNDO_REMOVE` of the triple `(id, id, id)` with kind [`REC_TERM`]: a
    /// binary that predates term records replays it as the removal of a
    /// triple that cannot be stored at that point of the history (its term
    /// is new) — a no-op — and frees the term the same way.
    fn journal_term_into(&self, wb: &mut WriteBatch, space: TermSpace, id: u32) {
        let Some(key) = self.next_journal_key() else {
            return;
        };
        let mut val = Vec::with_capacity(18);
        val.push(UNDO_REMOVE);
        val.push(REC_TERM);
        val.extend_from_slice(&[space.tag(), 0, 0, 0]);
        val.extend_from_slice(&key3(id, id, id));
        wb.put_cf(self.cf(CF_META), key, val);
    }

    /// Every journal record `(undo op, kind, graph or term tag, triple)`,
    /// oldest first.
    #[allow(clippy::type_complexity)]
    fn journal_records(&self) -> Result<Vec<(u8, u8, u32, IdTriple)>> {
        let mut out = Vec::new();
        self.try_scan(CF_META, JOURNAL_PREFIX, |_, v| {
            out.push(decode_record(v)?);
            Ok(true)
        })
        .map_err(|e| e.into_error("journal"))?;
        Ok(out)
    }

    /// Recover the journal a process that died left behind (at open):
    /// undo it newest first in chunks of [`REPLAY_CHUNK`] records, read
    /// with a reverse iterator below the previous chunk. Each chunk's
    /// records are deleted, once undone, in one batch with its reclamation
    /// candidates (written to the pending range, one key per term), so the
    /// journal left on disk is always an older prefix: a crash during the
    /// recovery resumes where it stopped, and the replay holds one chunk of
    /// records at a time, whatever the journal's length (a `gadd` of
    /// millions of triples journals a record per triple and per new term).
    /// The counters are reloaded afterwards, whatever it got through.
    fn recover_journal(&mut self) -> Result<()> {
        let recovered = self.recover_journal_chunks();
        let reloaded = self.load_counters();
        recovered?;
        reloaded
    }

    fn recover_journal_chunks(&mut self) -> Result<()> {
        #[cfg(test)]
        let chunk_len = match REPLAY_CHUNK_FOR_TEST.with(std::cell::Cell::get) {
            0 => REPLAY_CHUNK,
            n => n,
        };
        #[cfg(not(test))]
        let chunk_len = REPLAY_CHUNK;
        let mut upper = JOURNAL_END.to_vec();
        #[cfg(test)]
        let mut chunks = 0usize;
        loop {
            // The newest records below `upper`, newest first.
            let mut records: Vec<(Vec<u8>, JournalRecord)> = Vec::new();
            {
                let mut it = self.raw_iter(CF_META, Some(upper.clone()));
                it.seek_to_last();
                while records.len() < chunk_len {
                    let Some((k, v)) = it.item() else { break };
                    if !k.starts_with(JOURNAL_PREFIX) {
                        break;
                    }
                    let rec = decode_record(v).map_err(GStoreError::Database)?;
                    records.push((k.to_vec(), rec));
                    it.prev();
                }
                it.status().map_err(|e| {
                    GStoreError::Database(format!("rocksdb read (journal): {}", e.into_string()))
                })?;
            }
            let Some((oldest, _)) = records.last() else {
                return Ok(());
            };
            let oldest = oldest.clone();
            #[cfg(test)]
            REPLAY_MAX_HELD.with(|m| m.set(m.get().max(records.len())));
            let mut candidates = std::collections::HashSet::new();
            for (_, (op, kind, tag, t)) in records {
                record_candidates(op, kind, tag, t, &mut candidates);
                self.apply_undo(op, kind, tag, t)?;
            }
            #[cfg(test)]
            {
                chunks += 1;
                if REPLAY_STOP_AFTER_CHUNKS.with(std::cell::Cell::get) == Some(chunks - 1) {
                    return Err(GStoreError::Database(
                        "rocksdb journal recovery: injected stop".into(),
                    ));
                }
            }
            let mut wb = WriteBatch::default();
            let meta = self.cf(CF_META);
            wb.delete_range_cf(meta, &oldest, &upper);
            for (space, id) in candidates {
                wb.put_cf(meta, pending_key(space, id), b"");
            }
            self.write(wb).map_err(|e| {
                GStoreError::Database(format!("rocksdb write (journal recovery): {e}"))
            })?;
            self.pending.store(true, Ordering::SeqCst);
            upper = oldest;
        }
    }

    /// Undo one journal record `(op, kind, tag, t)` (idempotent: it sets one
    /// key present or absent).
    fn apply_undo(&mut self, op: u8, kind: u8, tag: u32, t: IdTriple) -> Result<()> {
        let insert = op == UNDO_INSERT;
        match kind {
            REC_TERM => {}
            REC_QUAD => {
                self.quad_write(tag, t, insert)?;
            }
            _ => {
                let res = if insert {
                    self.write_insert(t)
                } else {
                    self.write_remove(t)
                };
                res.map_err(|e| {
                    GStoreError::Database(format!("rocksdb write (journal replay): {e}"))
                })?;
            }
        }
        Ok(())
    }

    /// Undo every journaled mutation (newest first), then delete the journal.
    /// Idempotent: a replay cut short is completed by running it again. The
    /// cached counters and the mirror are reloaded from the `stats` CF after
    /// the replay, whatever it got through.
    fn replay_journal(&mut self) -> Result<()> {
        if self.replay_records_on_disk()? {
            self.clear_journal_with(None, &[])?;
        }
        Ok(())
    }

    /// Undo every journaled mutation (newest first), keeping the journal;
    /// `Ok(false)` when there is none. The cached counters and the mirror
    /// are reloaded from the `stats` CF after the replay, whatever it got
    /// through.
    fn replay_records_on_disk(&mut self) -> Result<bool> {
        let records = self.journal_records()?;
        if records.is_empty() {
            return Ok(false);
        }
        let replayed = self.replay_records(records);
        let reloaded = self.load_counters();
        replayed?;
        reloaded?;
        Ok(true)
    }

    fn replay_records(&mut self, records: Vec<(u8, u8, u32, IdTriple)>) -> Result<()> {
        // Undoing an insert may leave its terms unreferenced, and so may
        // a term written new. They are handed over (`take_replayed`) only
        // once the whole replay has run: a replay cut short adds nothing,
        // and the retry that completes it adds them once.
        let mut terms = Vec::new();
        for (op, kind, tag, t) in &records {
            record_candidates(*op, *kind, *tag, *t, &mut terms);
        }
        for (op, kind, tag, t) in records.into_iter().rev() {
            self.apply_undo(op, kind, tag, t)?;
        }
        self.replayed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(terms);
        Ok(())
    }

    /// Delete the journal — the `Some(n)` records a recording journal wrote,
    /// or (`None`) whatever is on disk — and the dictionary terms `frees` in
    /// one atomic batch. Nothing is written when there is nothing to delete.
    fn clear_journal_with(&self, records: Option<u64>, frees: &[(TermSpace, u32)]) -> Result<()> {
        #[cfg(test)]
        Failpoints::hit(&self.failpoints.journal_clear, "journal")?;
        let mut wb = WriteBatch::default();
        let meta = self.cf(CF_META);
        match records {
            Some(n) if n <= POINT_CLEAR_MAX => {
                for seq in 0..n {
                    wb.delete_cf(meta, journal_key(seq));
                }
            }
            _ => wb.delete_range_cf(meta, JOURNAL_PREFIX, JOURNAL_END),
        }
        let dict = self.cf(CF_DICT);
        for (space, id) in frees {
            wb.delete_cf(dict, tag1(space.tag(), *id));
        }
        if wb.is_empty() {
            return Ok(());
        }
        self.write(wb)
            .map_err(|e| GStoreError::Database(format!("rocksdb write (journal): {e}")))
    }

    /// Refuse a triple/quad write while the journal is stale (see the module
    /// docs), trying to clean it up first.
    fn writable(&mut self) -> std::result::Result<(), String> {
        if self.journal_error().is_none() {
            return Ok(());
        }
        self.heal_journal().map_err(|e| e.to_string())
    }

    // ---- dictionary (dict CF) ---------------------------------------------

    /// Persist dictionary terms `(space, id, key)` in one atomic batch
    /// (overwriting any previous term stored under the same id).
    pub fn put_terms(&self, terms: &[(TermSpace, u32, &str)]) -> Result<()> {
        self.write_terms(terms, false)
    }

    /// [`put_terms`](Self::put_terms) for terms that are **new** to the
    /// store — no stored triple or quad uses their ids — written before the
    /// first triple that uses them. While a journal is recording, each one
    /// is journaled in the same batch (see the module docs), so a crash
    /// before the triple that would use it lets the next open free it
    /// instead of leaving it behind as an orphan.
    pub fn put_new_terms(&self, terms: &[(TermSpace, u32, &str)]) -> Result<()> {
        self.write_terms(terms, true)
    }

    fn write_terms(&self, terms: &[(TermSpace, u32, &str)], new: bool) -> Result<()> {
        if terms.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        Failpoints::hit(&self.failpoints.side_writes, "dictionary")?;
        let cf = self.cf(CF_DICT);
        let mut wb = WriteBatch::default();
        for (space, id, key) in terms {
            wb.put_cf(cf, tag1(space.tag(), *id), key.as_bytes());
            if new {
                self.journal_term_into(&mut wb, *space, *id);
            }
        }
        self.write(wb)
            .map_err(|e| GStoreError::Database(format!("rocksdb write (dictionary): {e}")))
    }

    /// Delete the dictionary terms `(space, id)` in one atomic batch: ids
    /// freed by reclamation (a gap in the `dict` CF is a free id). Written
    /// after the triple batches that left them unreferenced, so a crash in
    /// between only leaves orphan terms.
    pub fn delete_terms(&self, terms: &[(TermSpace, u32)]) -> Result<()> {
        if terms.is_empty() {
            return Ok(());
        }
        #[cfg(test)]
        Failpoints::hit(&self.failpoints.side_writes, "dictionary")?;
        let cf = self.cf(CF_DICT);
        let mut wb = WriteBatch::default();
        for (space, id) in terms {
            wb.delete_cf(cf, tag1(space.tag(), *id));
        }
        self.write(wb)
            .map_err(|e| GStoreError::Database(format!("rocksdb write (dictionary): {e}")))
    }

    /// Replace the entire stored dictionary with `terms`, atomically (used after
    /// id reclamation and when migrating a legacy `dict.bin`).
    pub fn replace_dictionary<'a, I>(&self, terms: I) -> Result<()>
    where
        I: IntoIterator<Item = (TermSpace, u32, &'a str)>,
    {
        let cf = self.cf(CF_DICT);
        let mut wb = WriteBatch::default();
        // Every dict key is 5 bytes `tag ‖ be(id)`; this range covers them all.
        wb.delete_range_cf(cf, vec![0u8; 5], vec![0xFFu8; 6]);
        for (space, id, key) in terms {
            wb.put_cf(cf, tag1(space.tag(), id), key.as_bytes());
        }
        self.write(wb)
            .map_err(|e| GStoreError::Database(format!("rocksdb write (dictionary): {e}")))
    }

    /// Every stored dictionary term as `(space, id, key)`, in `(space, id)` order.
    pub fn load_terms(&self) -> Result<Vec<(TermSpace, u32, String)>> {
        let mut out = Vec::new();
        self.try_scan(CF_DICT, &[], |k, v| {
            let space = (k.len() == TAG1_LEN)
                .then(|| TermSpace::from_tag(k[0]))
                .flatten()
                .ok_or_else(|| "corrupt rocksdb dictionary key".to_string())?;
            let key = std::str::from_utf8(v)
                .map_err(|_| "non-UTF-8 rocksdb dictionary term".to_string())?;
            out.push((space, de(&k[1..5]), key.to_string()));
            Ok(true)
        })
        .map_err(|e| e.into_error("dictionary"))?;
        Ok(out)
    }

    // ---- named graphs (gspo CF) --------------------------------------------

    /// Insert a quad (graph-IRI entity id + triple). Returns `true` if new.
    /// Journaled inside a transaction; refused while the journal is stale.
    pub fn insert_quad(&self, g: u32, t: IdTriple) -> Result<bool> {
        if let Some(e) = self.journal_error() {
            return Err(GStoreError::Database(format!("rocksdb write (quad): {e}")));
        }
        self.quad_write(g, t, true)
    }

    /// Remove a quad. Returns `true` if it existed. Journaled inside a
    /// transaction; refused while the journal is stale.
    pub fn remove_quad(&self, g: u32, t: IdTriple) -> Result<bool> {
        if let Some(e) = self.journal_error() {
            return Err(GStoreError::Database(format!("rocksdb write (quad): {e}")));
        }
        self.quad_write(g, t, false)
    }

    /// Set quad `(g, t)` present (`insert`) or absent, journaling the change.
    /// Returns whether it changed.
    fn quad_write(&self, g: u32, t: IdTriple, insert: bool) -> Result<bool> {
        let cf = self.cf(CF_GSPO);
        let key = key4(g, t.sub, t.pred, t.obj);
        let present = self
            .db
            .get_pinned_cf(cf, key)
            .map_err(|e| GStoreError::Database(format!("rocksdb read (quad): {e}")))?
            .is_some();
        if present == insert {
            return Ok(false);
        }
        #[cfg(test)]
        Failpoints::hit(&self.failpoints.side_writes, "quad")?;
        let mut wb = WriteBatch::default();
        if insert {
            wb.put_cf(cf, key, b"");
            self.journal_into(&mut wb, UNDO_REMOVE, Some(g), t);
        } else {
            wb.delete_cf(cf, key);
            self.journal_into(&mut wb, UNDO_INSERT, Some(g), t);
        }
        self.write(wb)
            .map_err(|e| GStoreError::Database(format!("rocksdb write (quad): {e}")))?;
        Ok(true)
    }

    /// Every named-graph quad as `(graph_id, triple)`, sorted.
    pub fn quads(&self) -> Result<Vec<(u32, IdTriple)>> {
        let mut out = Vec::new();
        self.try_scan(CF_GSPO, &[], |k, _| {
            if k.len() != KEY4_LEN {
                return Err("corrupt rocksdb quad key".to_string());
            }
            out.push((
                de(&k[0..4]),
                IdTriple::new(de(&k[4..8]), de(&k[8..12]), de(&k[12..16])),
            ));
            Ok(true)
        })
        .map_err(|e| e.into_error("quads"))?;
        Ok(out)
    }

    // ---- metadata (meta CF) ------------------------------------------------

    /// Store a metadata value (database name, layout markers).
    pub fn put_meta(&self, key: &str, value: &[u8]) -> Result<()> {
        self.db
            .put_cf(self.cf(CF_META), key.as_bytes(), value)
            .map_err(|e| GStoreError::Database(format!("rocksdb write (meta): {e}")))
    }

    /// Read a metadata value.
    pub fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.db
            .get_cf(self.cf(CF_META), key.as_bytes())
            .map_err(|e| GStoreError::Database(format!("rocksdb read (meta): {e}")))
    }

    // ---- column-family handles -------------------------------------------

    fn cf(&self, name: &str) -> &rocksdb::ColumnFamily {
        self.db
            .cf_handle(name)
            .expect("column family was created at open")
    }

    // ---- counter helpers (stats CF) --------------------------------------

    /// The counter at `key` (`0` when absent), or the read error.
    fn get_counter(&self, key: &[u8]) -> std::result::Result<u64, String> {
        if let Some(e) = self.injected_read_failure() {
            return Err(e);
        }
        match self.db.get_pinned_cf(self.cf(CF_STATS), key) {
            Ok(None) => Ok(0),
            Ok(Some(v)) => {
                counter_value(&v).ok_or_else(|| format!("corrupt rocksdb counter {key:?}"))
            }
            Err(e) => Err(e.into_string()),
        }
    }

    /// The counter at `key` for a query: a read error is reported (see the
    /// module docs) and reads as `0`.
    fn counter(&self, key: &[u8]) -> u64 {
        self.get_counter(key).unwrap_or_else(|e| {
            note_read_error(format!("rocksdb read (stats): {e}"));
            0
        })
    }

    /// Whether the counter at `key` is present: `true` on a read error,
    /// which is reported (an existence check never answers "absent" when in
    /// doubt).
    fn counter_present(&self, key: &[u8]) -> bool {
        match self.get_counter(key) {
            Ok(n) => n > 0,
            Err(e) => {
                note_read_error(format!("rocksdb read (stats): {e}"));
                true
            }
        }
    }

    /// Apply a `+1`/`-1` delta to the counter at `key`, deleting it when it
    /// reaches zero. Returns `(old, new)` so callers can detect `0 ↔ 1`
    /// transitions that move a distinct-count. A read error fails the write.
    fn step(
        &self,
        wb: &mut WriteBatch,
        key: &[u8],
        up: bool,
    ) -> std::result::Result<(u64, u64), String> {
        let old = self.get_counter(key)?;
        let new = if up { old + 1 } else { old.saturating_sub(1) };
        let cf = self.cf(CF_STATS);
        if new == 0 {
            wb.delete_cf(cf, key);
        } else {
            wb.put_cf(cf, key, new.to_be_bytes());
        }
        Ok((old, new))
    }

    fn set_global(&self, wb: &mut WriteBatch, key: &[u8], value: u64) {
        wb.put_cf(self.cf(CF_STATS), key, value.to_be_bytes());
    }

    // ---- scans -----------------------------------------------------------

    /// A raw iterator over column family `cf`, bounded above by `upper`.
    fn raw_iter(&self, cf: &str, upper: Option<Vec<u8>>) -> rocksdb::DBRawIterator<'_> {
        let mut opts = ReadOptions::default();
        if let Some(end) = upper {
            opts.set_iterate_upper_bound(end);
        }
        self.db.raw_iterator_cf_opt(self.cf(cf), opts)
    }

    /// Visit every `(key, value)` of column family `cf` that starts with
    /// `prefix` (empty: the whole CF), ascending, until `f` returns
    /// `Ok(false)` or an error. The iterator is bounded by the prefix's
    /// successor, and nothing is copied: `f` decodes in place.
    fn try_scan(
        &self,
        cf: &str,
        prefix: &[u8],
        mut f: impl FnMut(&[u8], &[u8]) -> std::result::Result<bool, String>,
    ) -> std::result::Result<(), ScanError> {
        if matches!(cf, CF_SPO | CF_POS | CF_OSP | CF_STATS) {
            if let Some(e) = self.injected_read_failure() {
                return Err(ScanError::Read(e));
            }
        }
        let mut it = self.raw_iter(cf, prefix_end(prefix));
        if prefix.is_empty() {
            it.seek_to_first();
        } else {
            it.seek(prefix);
        }
        while let Some((k, v)) = it.item() {
            if !k.starts_with(prefix) || !f(k, v).map_err(ScanError::Visit)? {
                break;
            }
            it.next();
        }
        it.status().map_err(|e| ScanError::Read(e.into_string()))
    }

    /// Visit every key of `cf` starting with `prefix`, ascending, decoded in
    /// place; every one must be `len` bytes long (a key of another length —
    /// a damaged store — ends the scan with a read error instead of reaching
    /// the decoder). A read error is reported through [`note_read_error`]
    /// and ends the scan (the statement fails; see the module docs).
    fn scan(&self, cf: &str, prefix: &[u8], len: usize, mut f: impl FnMut(&[u8])) {
        let res = self.try_scan(cf, prefix, |k, _| {
            check_len(cf, k, len)?;
            f(k);
            Ok(true)
        });
        if let Err(ScanError::Read(e) | ScanError::Visit(e)) = res {
            note_read_error(format!("rocksdb read ({cf}): {e}"));
        }
    }

    /// The number of keys of `cf` starting with `prefix` (each `len` bytes).
    fn count_keys(&self, cf: &str, prefix: &[u8], len: usize) -> u64 {
        let mut n = 0u64;
        self.scan(cf, prefix, len, |_| n += 1);
        n
    }

    /// The ids of a tagged single-id `stats` prefix (`s`/`o`), each live
    /// counter key being `tag ‖ be(id)`: ascending.
    fn ids_under(&self, tag: u8) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        self.scan(CF_STATS, &[tag], TAG1_LEN, |k| out.push(de(&k[1..5])));
        out
    }

    /// The second ids under a tagged pair prefix (the `x|p` subjects, the
    /// `y|p` objects): ascending.
    fn ids_under_pred(&self, tag: u8, pred: PredId, hint: u64) -> Vec<EntityLiteralId> {
        let mut out = reserved(hint);
        self.scan(CF_STATS, &tag1(tag, pred), TAG2_LEN, |k| {
            out.push(de(&k[5..9]))
        });
        out
    }

    /// All predicate ids currently present (one `c`-tagged `stats` key each),
    /// ascending — from the in-memory mirror.
    pub fn predicates(&self) -> Vec<PredId> {
        self.preds.keys().copied().collect()
    }

    /// The mirrored counters of `pred` (zero when it has no triples).
    #[inline]
    fn pred_counts(&self, pred: PredId) -> PredCounts {
        self.preds.get(&pred).copied().unwrap_or_default()
    }

    // ---- mutation core ----------------------------------------------------

    /// Whether triple `t` is stored (an SPO point read).
    fn contains(&self, t: IdTriple) -> std::result::Result<bool, String> {
        if let Some(e) = self.injected_read_failure() {
            return Err(e);
        }
        self.db
            .get_pinned_cf(self.cf(CF_SPO), key3(t.sub, t.pred, t.obj))
            .map(|v| v.is_some())
            .map_err(|e| e.into_string())
    }

    fn insert_ids(&mut self, t: IdTriple) -> bool {
        if let Err(e) = self.writable() {
            self.record_error_msg("insert", &e);
            return false;
        }
        match self.write_insert(t) {
            Ok(changed) => changed,
            Err(e) => {
                self.record_error_msg("insert", &e);
                false
            }
        }
    }

    fn remove_ids(&mut self, t: IdTriple) -> bool {
        if let Err(e) = self.writable() {
            self.record_error_msg("remove", &e);
            return false;
        }
        match self.write_remove(t) {
            Ok(changed) => changed,
            Err(e) => {
                self.record_error_msg("remove", &e);
                false
            }
        }
    }

    /// Insert `t` (journaled inside a transaction). `Ok(false)` if present.
    /// A failed read or write changes nothing: the cached counters and the
    /// mirror advance only once the batch is written.
    fn write_insert(&mut self, t: IdTriple) -> std::result::Result<bool, String> {
        if self.contains(t)? {
            return Ok(false);
        }
        let IdTriple { sub, pred, obj } = t;
        let mut wb = WriteBatch::default();
        wb.put_cf(self.cf(CF_SPO), key3(sub, pred, obj), b"");
        wb.put_cf(self.cf(CF_POS), key3(pred, obj, sub), b"");
        wb.put_cf(self.cf(CF_OSP), key3(obj, sub, pred), b"");

        let triples = self.triple_count + 1;
        let mut preds = self.num_predicates;
        let mut subjects = self.distinct_subjects;
        let mut objects = self.distinct_objects;
        let mut pc = self.pred_counts(pred);
        self.set_global(&mut wb, K_TRIPLES, triples);

        // predicate cardinality + distinct-predicate transition
        let (old, new) = self.step(&mut wb, &tag1(T_CARD, pred), true)?;
        pc.card = new;
        if old == 0 {
            preds += 1;
            self.set_global(&mut wb, K_PREDS, preds);
        }
        // distinct subjects / objects
        if self.step(&mut wb, &tag1(T_SUB, sub), true)?.0 == 0 {
            subjects += 1;
            self.set_global(&mut wb, K_SUBJECTS, subjects);
        }
        if self.step(&mut wb, &tag1(T_OBJ, obj), true)?.0 == 0 {
            objects += 1;
            self.set_global(&mut wb, K_OBJECTS, objects);
        }
        // per-predicate distinct subjects / objects
        if self.step(&mut wb, &tag2(T_PS, pred, sub), true)?.0 == 0 {
            pc.subjects = self.step(&mut wb, &tag1(T_PSUBJ, pred), true)?.1;
        }
        if self.step(&mut wb, &tag2(T_PO, pred, obj), true)?.0 == 0 {
            pc.objects = self.step(&mut wb, &tag1(T_POBJ, pred), true)?.1;
        }
        self.journal_into(&mut wb, UNDO_REMOVE, None, t);

        self.write(wb).map_err(|e| e.into_string())?;
        self.triple_count = triples;
        self.num_predicates = preds;
        self.distinct_subjects = subjects;
        self.distinct_objects = objects;
        self.set_pred_counts(pred, pc);
        Ok(true)
    }

    /// Remove `t` (journaled inside a transaction). `Ok(false)` if absent.
    fn write_remove(&mut self, t: IdTriple) -> std::result::Result<bool, String> {
        if !self.contains(t)? {
            return Ok(false);
        }
        let IdTriple { sub, pred, obj } = t;
        let mut wb = WriteBatch::default();
        wb.delete_cf(self.cf(CF_SPO), key3(sub, pred, obj));
        wb.delete_cf(self.cf(CF_POS), key3(pred, obj, sub));
        wb.delete_cf(self.cf(CF_OSP), key3(obj, sub, pred));

        let triples = self.triple_count.saturating_sub(1);
        let mut preds = self.num_predicates;
        let mut subjects = self.distinct_subjects;
        let mut objects = self.distinct_objects;
        let mut pc = self.pred_counts(pred);
        self.set_global(&mut wb, K_TRIPLES, triples);

        let (_, new) = self.step(&mut wb, &tag1(T_CARD, pred), false)?;
        pc.card = new;
        if new == 0 {
            preds = preds.saturating_sub(1);
            self.set_global(&mut wb, K_PREDS, preds);
        }
        if self.step(&mut wb, &tag1(T_SUB, sub), false)?.1 == 0 {
            subjects = subjects.saturating_sub(1);
            self.set_global(&mut wb, K_SUBJECTS, subjects);
        }
        if self.step(&mut wb, &tag1(T_OBJ, obj), false)?.1 == 0 {
            objects = objects.saturating_sub(1);
            self.set_global(&mut wb, K_OBJECTS, objects);
        }
        if self.step(&mut wb, &tag2(T_PS, pred, sub), false)?.1 == 0 {
            pc.subjects = self.step(&mut wb, &tag1(T_PSUBJ, pred), false)?.1;
        }
        if self.step(&mut wb, &tag2(T_PO, pred, obj), false)?.1 == 0 {
            pc.objects = self.step(&mut wb, &tag1(T_POBJ, pred), false)?.1;
        }
        self.journal_into(&mut wb, UNDO_INSERT, None, t);

        self.write(wb).map_err(|e| e.into_string())?;
        self.triple_count = triples;
        self.num_predicates = preds;
        self.distinct_subjects = subjects;
        self.distinct_objects = objects;
        self.set_pred_counts(pred, pc);
        Ok(true)
    }

    /// Record `pred`'s counters in the mirror (after a successful write).
    fn set_pred_counts(&mut self, pred: PredId, pc: PredCounts) {
        if pc.card == 0 {
            self.preds.remove(&pred);
        } else {
            self.preds.insert(pred, pc);
        }
    }
}

// ---- TripleSource (read face) --------------------------------------------

impl TripleSource for RocksStore {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        self.contains(IdTriple::new(sub, pred, obj))
            .unwrap_or_else(|e| {
                note_read_error(format!("rocksdb read ({CF_SPO}): {e}"));
                false
            })
    }

    /// `s ? ?` → `(pred, obj)` pairs, sorted by `(pred, obj)` (SPO key order).
    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        let mut out = Vec::new();
        self.scan(CF_SPO, &be(sub), KEY3_LEN, |k| {
            out.push((de(&k[4..8]), de(&k[8..12])))
        });
        out
    }

    /// `s p ?` → objects, ascending (SPO key order).
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        self.scan(CF_SPO, &key2(sub, pred), KEY3_LEN, |k| {
            out.push(de(&k[8..12]))
        });
        out
    }

    /// `s ? o` → predicates linking subject to object, ascending. OSP key is
    /// `o ‖ s ‖ p`, so the `(o, s)` prefix yields predicates in ascending order.
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        let mut out = Vec::new();
        self.scan(CF_OSP, &key2(obj, sub), KEY3_LEN, |k| {
            out.push(de(&k[8..12]))
        });
        out
    }

    /// `? ? o` → `(pred, sub)` pairs, sorted by `(pred, sub)` to match
    /// [`TripleStore`](crate::store::TripleStore): the OSP scan yields them
    /// in `(s, p)` order, so the object's own (in-degree sized) list is
    /// sorted.
    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        let mut out = Vec::new();
        self.scan(CF_OSP, &be(obj), KEY3_LEN, |k| {
            out.push((de(&k[8..12]), de(&k[4..8])))
        });
        out.sort_unstable();
        out
    }

    /// `? p o` → subjects, ascending (POS `(p, o)` prefix).
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        let mut out = Vec::new();
        self.scan(CF_POS, &key2(pred, obj), KEY3_LEN, |k| {
            out.push(de(&k[8..12]))
        });
        out
    }

    /// `? p ?` → `(sub, obj)` pairs in `(sub, obj)` order: for long object
    /// runs the predicate's subjects (its `x|p` counter keys, ascending),
    /// each followed by its objects from an SPO seek to `(s, p)`; otherwise
    /// the POS range sorted (see the module docs). Both give the same list.
    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        let c = self.pred_counts(pred);
        if c.card >= SEEK_RUN.saturating_mul(c.subjects.max(1)) {
            return self.so_by_p_seek(pred);
        }
        let mut out = reserved(c.card);
        self.scan(CF_POS, &be(pred), KEY3_LEN, |k| {
            out.push((de(&k[8..12]), de(&k[4..8])))
        });
        out.sort_unstable();
        out
    }

    /// Distinct subjects of a predicate: its `x|p` counter keys, ascending.
    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        self.ids_under_pred(T_PS, pred, self.pred_counts(pred).subjects)
    }

    /// Distinct objects of a predicate: its `y|p` counter keys, ascending.
    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        self.ids_under_pred(T_PO, pred, self.pred_counts(pred).objects)
    }

    /// Every subject: the `s|` counter keys, ascending.
    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        self.ids_under(T_SUB)
    }

    /// Every object: the `o|` counter keys, ascending.
    fn object_keys(&self) -> Vec<EntityLiteralId> {
        self.ids_under(T_OBJ)
    }

    fn triple_count(&self) -> u64 {
        self.triple_count
    }

    fn distinct_subjects(&self) -> usize {
        self.distinct_subjects as usize
    }

    fn distinct_objects(&self) -> usize {
        self.distinct_objects as usize
    }

    fn num_predicates(&self) -> usize {
        self.num_predicates as usize
    }

    fn pred_card(&self, pred: PredId) -> usize {
        self.pred_counts(pred).card as usize
    }

    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        self.pred_counts(pred).subjects as usize
    }

    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        self.pred_counts(pred).objects as usize
    }

    /// Every triple in `(p, s, o)` order: one SPO scan, each triple placed
    /// straight into its predicate's slice of the result (sized by the exact
    /// `c|p` counters). Within a predicate the SPO order is `(s, o)`.
    fn iter_all(&self) -> Vec<IdTriple> {
        self.iter_all_by_pred()
    }

    // ---- C1 counts, from the exact counters (see the module docs) ----------

    fn count_po_by_s(&self, sub: EntityLiteralId) -> u64 {
        self.counter(&tag1(T_SUB, sub))
    }

    fn count_o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> u64 {
        self.counter(&tag2(T_PS, pred, sub))
    }

    /// No counter covers `(s, o)`: the OSP `(o, s)` keys are counted.
    fn count_p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> u64 {
        self.count_keys(CF_OSP, &key2(obj, sub), KEY3_LEN)
    }

    fn count_ps_by_o(&self, obj: EntityLiteralId) -> u64 {
        self.counter(&tag1(T_OBJ, obj))
    }

    fn count_s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> u64 {
        self.counter(&tag2(T_PO, pred, obj))
    }

    fn count_so_by_p(&self, pred: PredId) -> u64 {
        self.pred_counts(pred).card
    }

    /// The runs of predicates in the subject's SPO keys, seeking past a
    /// predicate with many objects.
    fn distinct_p_of_s(&self, sub: EntityLiteralId) -> u64 {
        self.distinct_preds_of_subject(sub)
    }

    /// The predicates of the object's OSP keys when it has few triples, else
    /// one `y|p|o` point read per predicate.
    fn distinct_p_of_o(&self, obj: EntityLiteralId) -> u64 {
        let degree = self.counter(&tag1(T_OBJ, obj));
        if degree == 0 {
            return 0;
        }
        if degree <= self.preds.len().max(16) as u64 {
            let mut ps = Vec::with_capacity(degree as usize);
            self.scan(CF_OSP, &be(obj), KEY3_LEN, |k| ps.push(de(&k[8..12])));
            ps.sort_unstable();
            ps.dedup();
            return ps.len() as u64;
        }
        self.preds
            .keys()
            .filter(|&&p| self.counter(&tag2(T_PO, p, obj)) > 0)
            .count() as u64
    }

    fn has_subject(&self, sub: EntityLiteralId) -> bool {
        self.counter_present(&tag1(T_SUB, sub))
    }

    fn has_object(&self, obj: EntityLiteralId) -> bool {
        self.counter_present(&tag1(T_OBJ, obj))
    }

    fn has_predicate(&self, pred: PredId) -> bool {
        self.preds.contains_key(&pred)
    }
}

/// An empty list with room for `hint` items when that much can be had: a
/// capacity taken from a stored counter must not abort the process (an
/// allocation failure) when the counter is damaged — the list then grows as
/// it is read instead.
fn reserved<T>(hint: u64) -> Vec<T> {
    let mut v = Vec::new();
    let _ = v.try_reserve_exact(usize::try_from(hint).unwrap_or(usize::MAX));
    v
}

impl RocksStore {
    /// [`so_by_p`](TripleSource::so_by_p) through the `x|p` subjects and SPO
    /// seeks.
    fn so_by_p_seek(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        let mut out = reserved(self.pred_counts(pred).card);
        if let Some(e) = self.injected_read_failure() {
            note_read_error(format!("rocksdb read ({CF_SPO}): {e}"));
            return out;
        }
        let mut spo = self.raw_iter(CF_SPO, None);
        let res = self.try_scan(CF_STATS, &tag1(T_PS, pred), |k, _| {
            check_len(CF_STATS, k, TAG2_LEN)?;
            let s = de(&k[5..9]);
            let at = key2(s, pred);
            spo.seek(at);
            while let Some(key) = spo.key() {
                if !key.starts_with(&at) {
                    break;
                }
                check_len(CF_SPO, key, KEY3_LEN)?;
                out.push((s, de(&key[8..12])));
                spo.next();
            }
            // Checked after every subject's run: the next seek resets the
            // iterator's status, so an error that ended this run (a bad
            // block) would be lost — and the list silently short.
            spo.status()
                .map(|()| true)
                .map_err(|e| format!("rocksdb read ({CF_SPO}): {}", e.into_string()))
        });
        match res {
            Ok(()) => {}
            Err(ScanError::Read(e)) => note_read_error(format!("rocksdb read ({CF_STATS}): {e}")),
            Err(ScanError::Visit(e)) => note_read_error(e),
        }
        out
    }

    /// [`iter_all`](TripleSource::iter_all) (see there).
    fn iter_all_by_pred(&self) -> Vec<IdTriple> {
        /// One predicate's slice of the result: `start..end`, filled up to `next`.
        struct Slot {
            pred: PredId,
            start: usize,
            next: usize,
            end: usize,
        }
        // Each predicate's slice starts after the triples of the smaller ones.
        // The counters only size the result when they add up (without
        // overflow) to the global triple counter and the space can be had:
        // a damaged `stats` CF must not abort the process on a huge
        // allocation (or wrap the sum), so it is read and sorted instead.
        let total = self
            .preds
            .values()
            .try_fold(0u64, |n, c| n.checked_add(c.card))
            .filter(|&n| n == self.triple_count)
            .and_then(|n| usize::try_from(n).ok());
        let mut out: Vec<IdTriple> = Vec::new();
        let Some(total) = total.filter(|&n| out.try_reserve_exact(n).is_ok()) else {
            let mut all: Vec<IdTriple> = reserved(self.triple_count);
            self.scan(CF_SPO, &[], KEY3_LEN, |k| {
                all.push(IdTriple::new(de(&k[0..4]), de(&k[4..8]), de(&k[8..12])));
            });
            all.sort_unstable_by_key(|t| (t.pred, t.sub, t.obj));
            return all;
        };
        let mut slots: Vec<Slot> = Vec::with_capacity(self.preds.len());
        let mut at = 0usize;
        for (&pred, c) in &self.preds {
            // In range: the cards add up to `total` without overflow.
            let end = at + c.card as usize;
            slots.push(Slot {
                pred,
                start: at,
                next: at,
                end,
            });
            at = end;
        }
        out.resize(total, IdTriple::new(0, 0, 0));
        let mut spill: Vec<IdTriple> = Vec::new();
        self.scan(CF_SPO, &[], KEY3_LEN, |k| {
            let t = IdTriple::new(de(&k[0..4]), de(&k[4..8]), de(&k[8..12]));
            match slots.binary_search_by_key(&t.pred, |s| s.pred) {
                Ok(i) if slots[i].next < slots[i].end => {
                    out[slots[i].next] = t;
                    slots[i].next += 1;
                }
                _ => spill.push(t),
            }
        });
        if spill.is_empty() && slots.iter().all(|s| s.next == s.end) {
            return out;
        }
        // The counters disagree with the SPO keys (a read error, already
        // reported, cut the scan short — or the `stats` CF is damaged): sort
        // what was read instead.
        let mut all: Vec<IdTriple> = Vec::with_capacity(out.len() + spill.len());
        for s in &slots {
            all.extend_from_slice(&out[s.start..s.next]);
        }
        all.extend(spill);
        all.sort_unstable_by_key(|t| (t.pred, t.sub, t.obj));
        all
    }

    /// [`distinct_p_of_s`](TripleSource::distinct_p_of_s) (see there).
    fn distinct_preds_of_subject(&self, sub: EntityLiteralId) -> u64 {
        if let Some(e) = self.injected_read_failure() {
            note_read_error(format!("rocksdb read ({CF_SPO}): {e}"));
            return 0;
        }
        let prefix = be(sub);
        let mut it = self.raw_iter(CF_SPO, prefix_end(&prefix));
        it.seek(prefix);
        let mut n = 0u64;
        let mut last: Option<PredId> = None;
        let mut run = 0u32;
        while let Some(k) = it.key() {
            if !k.starts_with(&prefix) {
                break;
            }
            if let Err(e) = check_len(CF_SPO, k, KEY3_LEN) {
                note_read_error(format!("rocksdb read ({CF_SPO}): {e}"));
                return n;
            }
            let p = de(&k[4..8]);
            if last != Some(p) {
                n += 1;
                last = Some(p);
                run = 0;
            }
            run += 1;
            if run < SKIP_AFTER {
                it.next();
                continue;
            }
            // A long run of one predicate: seek to the next predicate.
            match p.checked_add(1) {
                Some(next) => {
                    it.seek(key2(sub, next));
                    run = 0;
                }
                None => break,
            }
        }
        if let Err(e) = it.status() {
            note_read_error(format!("rocksdb read ({CF_SPO}): {e}"));
        }
        n
    }

    // ---- diagnostics (tests, tools) ---------------------------------------

    /// Every `(key, value)` of column family `cf` (one of `spo`, `pos`,
    /// `osp`, `stats`, `dict`, `gspo`, `meta`), in key order. A diagnostic
    /// for tests and tools: it copies the whole column family into memory.
    #[doc(hidden)]
    pub fn debug_dump_cf(&self, cf: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let Some(name) = ALL_CFS.iter().find(|n| **n == cf) else {
            return Err(GStoreError::Database(format!(
                "no rocksdb column family '{cf}'"
            )));
        };
        let mut out = Vec::new();
        self.try_scan(name, &[], |k, v| {
            out.push((k.to_vec(), v.to_vec()));
            Ok(true)
        })
        .map_err(|e| e.into_error(name))?;
        Ok(out)
    }

    /// Check that the statistics are exact: every counter of the `stats` CF
    /// equals a recount from the SPO keys (and no counter is missing or
    /// left at zero), POS and OSP hold exactly the SPO triples, and the
    /// cached global counters and the `c|`/`u|`/`v|` mirror equal the
    /// `stats` CF. A diagnostic for tests and tools: it holds every counter
    /// in memory. `Err` names the first difference.
    #[doc(hidden)]
    pub fn check_stats(&self) -> std::result::Result<(), String> {
        let read = |e: ScanError| match e {
            ScanError::Read(e) | ScanError::Visit(e) => e,
        };
        let mut want: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        let mut triples = 0u64;
        let mut spo: Vec<[u32; 3]> = Vec::new();
        self.try_scan(CF_SPO, &[], |k, _| {
            check_len(CF_SPO, k, KEY3_LEN)?;
            let (s, p, o) = (de(&k[0..4]), de(&k[4..8]), de(&k[8..12]));
            spo.push([s, p, o]);
            triples += 1;
            for key in [
                tag1(T_CARD, p).to_vec(),
                tag1(T_SUB, s).to_vec(),
                tag1(T_OBJ, o).to_vec(),
                tag2(T_PS, p, s).to_vec(),
                tag2(T_PO, p, o).to_vec(),
            ] {
                *want.entry(key).or_default() += 1;
            }
            Ok(true)
        })
        .map_err(read)?;
        let mut derived: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        let (mut subjects, mut objects, mut preds) = (0u64, 0u64, 0u64);
        for k in want.keys() {
            match k[0] {
                T_PS => {
                    *derived
                        .entry(tag1(T_PSUBJ, de(&k[1..5])).to_vec())
                        .or_default() += 1
                }
                T_PO => {
                    *derived
                        .entry(tag1(T_POBJ, de(&k[1..5])).to_vec())
                        .or_default() += 1
                }
                T_SUB => subjects += 1,
                T_OBJ => objects += 1,
                T_CARD => preds += 1,
                _ => {}
            }
        }
        want.extend(derived);
        let mut have: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
        self.try_scan(CF_STATS, &[], |k, v| {
            let n = counter_value(v)
                .ok_or_else(|| format!("corrupt counter value at {}", show_key(k)))?;
            have.insert(k.to_vec(), n);
            Ok(true)
        })
        .map_err(read)?;
        // The global counters may be stored as 0 (after deletes) or absent.
        for (key, n) in [
            (K_TRIPLES, triples),
            (K_PREDS, preds),
            (K_SUBJECTS, subjects),
            (K_OBJECTS, objects),
        ] {
            let stored = have.remove(key).unwrap_or(0);
            if stored != n {
                return Err(format!(
                    "global counter {:?}: stored {stored}, recounted {n}",
                    key[0] as char
                ));
            }
        }
        for (key, n) in &want {
            match have.get(key) {
                Some(m) if m == n => {}
                other => {
                    return Err(format!(
                        "counter {}: stored {other:?}, recounted {n}",
                        show_key(key)
                    ))
                }
            }
        }
        if let Some((key, n)) = have.iter().find(|(k, _)| !want.contains_key(*k)) {
            return Err(format!("stray counter {} = {n}", show_key(key)));
        }
        for (cf, perm) in [(CF_POS, [1usize, 2, 0]), (CF_OSP, [2, 0, 1])] {
            let mut expect: Vec<[u32; 3]> = spo
                .iter()
                .map(|t| [t[perm[0]], t[perm[1]], t[perm[2]]])
                .collect();
            expect.sort_unstable();
            let mut got = Vec::with_capacity(expect.len());
            self.try_scan(cf, &[], |k, _| {
                check_len(cf, k, KEY3_LEN)?;
                got.push([de(&k[0..4]), de(&k[4..8]), de(&k[8..12])]);
                Ok(true)
            })
            .map_err(read)?;
            if got != expect {
                return Err(format!("{cf} does not hold exactly the SPO triples"));
            }
        }
        if (
            self.triple_count,
            self.num_predicates,
            self.distinct_subjects,
            self.distinct_objects,
        ) != (triples, preds, subjects, objects)
        {
            return Err("the cached global counters differ from the stats CF".into());
        }
        let mut mirror: BTreeMap<PredId, PredCounts> = BTreeMap::new();
        for (k, n) in &want {
            let c = match k[0] {
                T_CARD | T_PSUBJ | T_POBJ => mirror.entry(de(&k[1..5])).or_default(),
                _ => continue,
            };
            match k[0] {
                T_CARD => c.card = *n,
                T_PSUBJ => c.subjects = *n,
                _ => c.objects = *n,
            }
        }
        if mirror != self.preds {
            return Err("the c|/u|/v| mirror differs from the stats CF".into());
        }
        Ok(())
    }
}

/// A stats key for a message: its tag as a character, then the id bytes.
fn show_key(k: &[u8]) -> String {
    match k.split_first() {
        Some((t, rest)) => format!("{:?}{rest:?}", *t as char),
        None => "(an empty key)".into(),
    }
}

/// A forward cursor over the stored counters of one `stats` tag, in key
/// order, for the merges of [`RocksStore::verify_stats`].
struct CounterCursor<'a> {
    it: rocksdb::DBRawIterator<'a>,
    len: usize,
}

impl<'a> CounterCursor<'a> {
    /// The counters tagged `tag`, each key `len` bytes long.
    fn new(store: &'a RocksStore, tag: u8, len: usize) -> CounterCursor<'a> {
        let mut it = store.raw_iter(CF_STATS, prefix_end(&[tag]));
        it.seek([tag]);
        CounterCursor { it, len }
    }

    /// The next stored counter `(key, key length, value)`, `None` after the
    /// last.
    fn peek(&self) -> std::result::Result<Option<([u8; TAG2_LEN], usize, u64)>, String> {
        let Some((k, v)) = self.it.item() else {
            return self
                .it
                .status()
                .map(|()| None)
                .map_err(|e| format!("rocksdb read ({CF_STATS}): {}", e.into_string()));
        };
        check_len(CF_STATS, k, self.len)?;
        let n =
            counter_value(v).ok_or_else(|| format!("corrupt counter value at {}", show_key(k)))?;
        let mut key = [0u8; TAG2_LEN];
        key[..k.len()].copy_from_slice(k);
        Ok(Some((key, k.len(), n)))
    }

    /// The counter `key` must be the next one stored, with the value `n`:
    /// a stored counter before it is a stray.
    fn expect(&mut self, key: &[u8], n: u64) -> std::result::Result<(), String> {
        match self.peek()? {
            Some((k, len, m)) if &k[..len] == key => {
                if m != n {
                    return Err(format!(
                        "counter {}: stored {m}, recounted {n}",
                        show_key(key)
                    ));
                }
                self.it.next();
                Ok(())
            }
            Some((k, len, m)) if &k[..len] < key => {
                Err(format!("stray counter {} = {m}", show_key(&k[..len])))
            }
            _ => Err(format!("counter {}: missing, recounted {n}", show_key(key))),
        }
    }

    /// The end of the merge: no stored counter may be left.
    fn finish(&self) -> std::result::Result<(), String> {
        match self.peek()? {
            Some((k, len, m)) => Err(format!("stray counter {} = {m}", show_key(&k[..len]))),
            None => Ok(()),
        }
    }
}

impl RocksStore {
    /// Check that the statistics are exact — what
    /// [`check_stats`](Self::check_stats) checks — without holding them in
    /// memory: each ordering and each counter prefix is read once in key
    /// order and merged with the counts derived from the orderings (POS and
    /// OSP keys are point-checked against SPO, each `x|p|s` counter against
    /// the length of its SPO `(s, p)` run), so memory stays constant
    /// whatever the size of the store. [`verify_integrity`] (`gcompact
    /// --check`) runs it. `Err` names the first difference, or the first
    /// malformed key or counter value.
    ///
    /// [`verify_integrity`]: crate::Database::verify_integrity
    pub fn verify_stats(&self) -> std::result::Result<(), String> {
        if let Some(e) = self.injected_read_failure() {
            return Err(e);
        }
        let read = |e: ScanError| match e {
            ScanError::Read(e) => format!("rocksdb read: {e}"),
            ScanError::Visit(e) => e,
        };
        // Every stats key has a known tag, and that tag's length.
        self.try_scan(CF_STATS, &[], |k, _| {
            let len = match k.first() {
                Some(b'T' | b'P' | b'S' | b'O') => 1,
                Some(&(T_CARD | T_PSUBJ | T_POBJ | T_SUB | T_OBJ)) => TAG1_LEN,
                Some(&(T_PS | T_PO)) => TAG2_LEN,
                _ => return Err(format!("stray stats key {}", show_key(k))),
            };
            check_len(CF_STATS, k, len).map(|()| true)
        })
        .map_err(read)?;

        // SPO: the triples; `s|s` is the length of the subject's run.
        let mut s_cur = CounterCursor::new(self, T_SUB, TAG1_LEN);
        let (mut triples, mut subjects) = (0u64, 0u64);
        let mut run: Option<(u32, u64)> = None;
        self.try_scan(CF_SPO, &[], |k, _| {
            check_len(CF_SPO, k, KEY3_LEN)?;
            let s = de(&k[0..4]);
            triples += 1;
            match &mut run {
                Some((rs, n)) if *rs == s => *n += 1,
                _ => {
                    if let Some((rs, n)) = run.replace((s, 1)) {
                        subjects += 1;
                        s_cur.expect(&tag1(T_SUB, rs), n)?;
                    }
                }
            }
            Ok(true)
        })
        .map_err(read)?;
        if let Some((rs, n)) = run {
            subjects += 1;
            s_cur.expect(&tag1(T_SUB, rs), n)?;
        }
        s_cur.finish()?;

        // POS: every key is an SPO triple; `c|p` is the length of the
        // predicate's run, `v|p` its number of distinct objects, `y|p|o`
        // the length of the `(p, o)` run.
        let close_p = |c: &mut CounterCursor, v: &mut CounterCursor, p: u32, n: u64, d: u64| {
            c.expect(&tag1(T_CARD, p), n)?;
            v.expect(&tag1(T_POBJ, p), d)
        };
        let mut c_cur = CounterCursor::new(self, T_CARD, TAG1_LEN);
        let mut v_cur = CounterCursor::new(self, T_POBJ, TAG1_LEN);
        let mut y_cur = CounterCursor::new(self, T_PO, TAG2_LEN);
        let (mut pos_triples, mut preds) = (0u64, 0u64);
        // The predicate's run: (p, triples, distinct objects); the (p, o) run.
        let mut p_run: Option<(u32, u64, u64)> = None;
        let mut po_run: Option<(u32, u32, u64)> = None;
        self.try_scan(CF_POS, &[], |k, _| {
            check_len(CF_POS, k, KEY3_LEN)?;
            let (p, o, s) = (de(&k[0..4]), de(&k[4..8]), de(&k[8..12]));
            self.spo_must_hold(CF_POS, s, p, o)?;
            pos_triples += 1;
            if p_run.is_none_or(|(rp, _, _)| rp != p) {
                if let Some((rp, n, d)) = p_run.replace((p, 0, 0)) {
                    preds += 1;
                    close_p(&mut c_cur, &mut v_cur, rp, n, d)?;
                }
            }
            if po_run.is_none_or(|(rp, ro, _)| (rp, ro) != (p, o)) {
                if let Some((rp, ro, n)) = po_run.replace((p, o, 0)) {
                    y_cur.expect(&tag2(T_PO, rp, ro), n)?;
                }
                if let Some((_, _, d)) = &mut p_run {
                    *d += 1;
                }
            }
            if let (Some((_, n, _)), Some((_, _, m))) = (&mut p_run, &mut po_run) {
                *n += 1;
                *m += 1;
            }
            Ok(true)
        })
        .map_err(read)?;
        if let Some((rp, ro, n)) = po_run {
            y_cur.expect(&tag2(T_PO, rp, ro), n)?;
        }
        if let Some((rp, n, d)) = p_run {
            preds += 1;
            close_p(&mut c_cur, &mut v_cur, rp, n, d)?;
        }
        c_cur.finish()?;
        v_cur.finish()?;
        y_cur.finish()?;

        // OSP: every key is an SPO triple; `o|o` is the length of the
        // object's run.
        let mut o_cur = CounterCursor::new(self, T_OBJ, TAG1_LEN);
        let (mut osp_triples, mut objects) = (0u64, 0u64);
        let mut run: Option<(u32, u64)> = None;
        self.try_scan(CF_OSP, &[], |k, _| {
            check_len(CF_OSP, k, KEY3_LEN)?;
            let (o, s, p) = (de(&k[0..4]), de(&k[4..8]), de(&k[8..12]));
            self.spo_must_hold(CF_OSP, s, p, o)?;
            osp_triples += 1;
            match &mut run {
                Some((ro, n)) if *ro == o => *n += 1,
                _ => {
                    if let Some((ro, n)) = run.replace((o, 1)) {
                        objects += 1;
                        o_cur.expect(&tag1(T_OBJ, ro), n)?;
                    }
                }
            }
            Ok(true)
        })
        .map_err(read)?;
        if let Some((ro, n)) = run {
            objects += 1;
            o_cur.expect(&tag1(T_OBJ, ro), n)?;
        }
        o_cur.finish()?;
        for (cf, n) in [(CF_POS, pos_triples), (CF_OSP, osp_triples)] {
            if n != triples {
                return Err(format!("{cf} holds {n} triples, spo {triples}"));
            }
        }

        // `x|p|s` is the length of the SPO `(s, p)` run; per predicate they
        // add up to `c|p` (checked against POS above), and `u|p` counts them.
        let mut u_cur = CounterCursor::new(self, T_PSUBJ, TAG1_LEN);
        let close_x = |u: &mut CounterCursor, p: u32, sum: u64, keys: u64| {
            let card = self.get_counter(&tag1(T_CARD, p))?;
            if card != sum {
                return Err(format!(
                    "the x| counters of predicate {p} add up to {sum}, its c| counter is {card}"
                ));
            }
            u.expect(&tag1(T_PSUBJ, p), keys)
        };
        let mut spo = self.raw_iter(CF_SPO, None);
        let mut x_run: Option<(u32, u64, u64)> = None;
        let mut x_preds = 0u64;
        self.try_scan(CF_STATS, &[T_PS], |k, v| {
            check_len(CF_STATS, k, TAG2_LEN)?;
            let n = counter_value(v)
                .ok_or_else(|| format!("corrupt counter value at {}", show_key(k)))?;
            let (p, s) = (de(&k[1..5]), de(&k[5..9]));
            let at = key2(s, p);
            spo.seek(at);
            let mut len = 0u64;
            while let Some(key) = spo.key() {
                if !key.starts_with(&at) {
                    break;
                }
                check_len(CF_SPO, key, KEY3_LEN)?;
                len += 1;
                spo.next();
            }
            spo.status()
                .map_err(|e| format!("rocksdb read ({CF_SPO}): {}", e.into_string()))?;
            if len != n || n == 0 {
                return Err(format!(
                    "counter {}: stored {n}, recounted {len}",
                    show_key(k)
                ));
            }
            if x_run.is_none_or(|(rp, _, _)| rp != p) {
                if let Some((rp, sum, keys)) = x_run.replace((p, 0, 0)) {
                    x_preds += 1;
                    close_x(&mut u_cur, rp, sum, keys)?;
                }
            }
            if let Some((_, sum, keys)) = &mut x_run {
                *sum += n;
                *keys += 1;
            }
            Ok(true)
        })
        .map_err(read)?;
        if let Some((rp, sum, keys)) = x_run {
            x_preds += 1;
            close_x(&mut u_cur, rp, sum, keys)?;
        }
        u_cur.finish()?;
        if x_preds != preds {
            return Err(format!(
                "{preds} predicate(s) have triples, {x_preds} have x| counters"
            ));
        }

        // The global counters (stored as 0 after deletes, or absent), the
        // cached ones, and the c|/u|/v| mirror.
        for (key, n) in [
            (K_TRIPLES, triples),
            (K_PREDS, preds),
            (K_SUBJECTS, subjects),
            (K_OBJECTS, objects),
        ] {
            let stored = self.get_counter(key)?;
            if stored != n {
                return Err(format!(
                    "global counter {}: stored {stored}, recounted {n}",
                    show_key(key)
                ));
            }
        }
        if (
            self.triple_count,
            self.num_predicates,
            self.distinct_subjects,
            self.distinct_objects,
        ) != (triples, preds, subjects, objects)
        {
            return Err("the cached global counters differ from the stats CF".into());
        }
        if self.preds.len() as u64 != preds {
            return Err("the c|/u|/v| mirror differs from the stats CF".into());
        }
        let mut mirror = self.preds.iter();
        self.try_scan(CF_STATS, &[T_CARD], |k, v| {
            let p = de(&k[1..5]);
            let stored = PredCounts {
                card: counter_value(v).unwrap_or(0),
                subjects: self.get_counter(&tag1(T_PSUBJ, p))?,
                objects: self.get_counter(&tag1(T_POBJ, p))?,
            };
            match mirror.next() {
                Some((&mp, c)) if mp == p && *c == stored => Ok(true),
                _ => Err("the c|/u|/v| mirror differs from the stats CF".into()),
            }
        })
        .map_err(read)?;
        Ok(())
    }

    /// The check of a `cf` key that names the triple `(s, p, o)`: SPO must
    /// hold it.
    fn spo_must_hold(&self, cf: &str, s: u32, p: u32, o: u32) -> std::result::Result<(), String> {
        match self.db.get_pinned_cf(self.cf(CF_SPO), key3(s, p, o)) {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(format!(
                "{cf} holds the triple ({s}, {p}, {o}), which spo lacks"
            )),
            Err(e) => Err(format!("rocksdb read ({CF_SPO}): {}", e.into_string())),
        }
    }
}

impl Drop for RocksStore {
    /// Closing flushes the memtables: a commit point only syncs the WAL
    /// (see the module docs), so without this the next open would replay
    /// the log written since the last flush. Not needed for durability; a
    /// failure is ignored (the WAL still holds every write).
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

// ---- MutableStore (write face) -------------------------------------------

impl MutableStore for RocksStore {
    fn insert(&mut self, t: IdTriple) -> bool {
        self.insert_ids(t)
    }
    fn remove(&mut self, t: IdTriple) -> bool {
        self.remove_ids(t)
    }
    fn bulk_load(&mut self, triples: Vec<IdTriple>) {
        for t in triples {
            self.insert_ids(t);
        }
    }
}

// ---- key encoding ---------------------------------------------------------

/// 4-byte big-endian id (ordering-preserving).
#[inline]
pub(crate) fn be(x: u32) -> [u8; 4] {
    x.to_be_bytes()
}

/// Decode a 4-byte big-endian id.
#[inline]
pub(crate) fn de(b: &[u8]) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[0..4]);
    u32::from_be_bytes(a)
}

/// 12-byte composite key from three ids.
#[inline]
pub(crate) fn key3(a: u32, b: u32, c: u32) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[0..4].copy_from_slice(&be(a));
    k[4..8].copy_from_slice(&be(b));
    k[8..12].copy_from_slice(&be(c));
    k
}

/// 16-byte quad key `g ‖ s ‖ p ‖ o`.
#[inline]
pub(crate) fn key4(g: u32, s: u32, p: u32, o: u32) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[0..4].copy_from_slice(&be(g));
    k[4..16].copy_from_slice(&key3(s, p, o));
    k
}

/// 8-byte two-id prefix.
#[inline]
pub(crate) fn key2(a: u32, b: u32) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[0..4].copy_from_slice(&be(a));
    k[4..8].copy_from_slice(&be(b));
    k
}

/// Tagged 5-byte counter key: `tag ‖ be(id)`.
#[inline]
pub(crate) fn tag1(tag: u8, id: u32) -> [u8; 5] {
    let mut k = [0u8; 5];
    k[0] = tag;
    k[1..5].copy_from_slice(&be(id));
    k
}

/// Tagged 9-byte counter key: `tag ‖ be(a) ‖ be(b)`.
#[inline]
pub(crate) fn tag2(tag: u8, a: u32, b: u32) -> [u8; 9] {
    let mut k = [0u8; 9];
    k[0] = tag;
    k[1..5].copy_from_slice(&be(a));
    k[5..9].copy_from_slice(&be(b));
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_end_is_the_exclusive_successor() {
        assert_eq!(prefix_end(b""), None);
        assert_eq!(prefix_end(&[0xFF, 0xFF]), None);
        assert_eq!(prefix_end(&[1, 2, 3]), Some(vec![1, 2, 4]));
        assert_eq!(prefix_end(&[1, 0xFF]), Some(vec![2]));
        assert_eq!(prefix_end(&[0, 0xFF, 0xFF]), Some(vec![1]));
    }

    #[test]
    fn counter_values_are_eight_byte_big_endian() {
        assert_eq!(counter_value(&7u64.to_be_bytes()), Some(7));
        assert_eq!(counter_value(&[0, 1]), None);
        assert_eq!(counter_value(&[0; 9]), None);
    }

    /// A fresh scratch directory for one test.
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("gstore_ut_rocks_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_block_cache_variable_warns_unless_it_is_a_number_of_mib() {
        assert_eq!(block_cache_mb(None), (DEFAULT_BLOCK_CACHE_MB, None));
        assert_eq!(block_cache_mb(Some("  ")), (DEFAULT_BLOCK_CACHE_MB, None));
        assert_eq!(block_cache_mb(Some("64")), (64, None));
        assert_eq!(block_cache_mb(Some(" 32 ")), (32, None));
        assert_eq!(block_cache_mb(Some("0")), (0, None));
        for bad in ["64M", "1g", "64 MiB", "-1"] {
            let (mb, warning) = block_cache_mb(Some(bad));
            assert_eq!(mb, DEFAULT_BLOCK_CACHE_MB, "{bad}");
            let w = warning.unwrap_or_else(|| panic!("{bad} was taken silently"));
            assert!(w.contains(BLOCK_CACHE_ENV) && w.contains(bad), "{w}");
        }
    }

    /// The store at `dir` with a statement journaled but never ended (the
    /// process died): `n` triples inserted, each with a new subject term.
    /// Returns the triples.
    fn crashed_statement(dir: &std::path::Path, n: u32) -> Vec<IdTriple> {
        let mut store = RocksStore::open_with(dir, &RocksTuning::rocksdb_defaults()).unwrap();
        assert!(store.insert(IdTriple::new(1, 1, 2)), "a committed triple");
        store.begin_journal().unwrap();
        let mut added = Vec::new();
        for i in 0..n {
            let t = IdTriple::new(100 + i, 1, 2);
            store
                .put_new_terms(&[(TermSpace::Entity, 100 + i, "<http://ex/new>")])
                .unwrap();
            assert!(store.insert(t));
            added.push(t);
        }
        assert_eq!(store.journal_records().unwrap().len(), 2 * n as usize);
        added
    }

    /// A journal a crash left is recovered at open in chunks (review of
    /// round 1: the open read the whole journal into memory, collected
    /// three or four candidates per record, and deleted the journal only
    /// after the whole replay): at most one chunk of records is held at a
    /// time, the journal is gone afterwards, and the candidates — one per
    /// term — wait in the pending range until they are settled.
    #[test]
    fn a_long_journal_is_recovered_in_bounded_chunks() {
        struct Gone(std::path::PathBuf);
        impl Drop for Gone {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = Gone(scratch("bounded_recovery"));
        let n = 1000u32;
        let added = crashed_statement(&dir.0, n);
        REPLAY_CHUNK_FOR_TEST.with(|c| c.set(64));
        REPLAY_MAX_HELD.with(|m| m.set(0));
        let store = RocksStore::open_with(&dir.0, &RocksTuning::rocksdb_defaults()).unwrap();
        REPLAY_CHUNK_FOR_TEST.with(|c| c.set(0));
        let held = REPLAY_MAX_HELD.with(std::cell::Cell::get);
        assert!(
            (1..=64).contains(&held),
            "the recovery held {held} records at once (journal: {})",
            2 * n
        );
        assert!(store.journal_records().unwrap().is_empty());
        assert!(store.journal_error().is_none());
        assert_eq!(store.triple_count, 1, "the statement is undone");
        assert!(added.iter().all(|t| store.contains(*t) == Ok(false)));
        let pending = store.pending_reclaim(usize::MAX).unwrap();
        // Each new subject once, and the predicate and object of the
        // undone inserts once each.
        assert_eq!(pending.len(), n as usize + 2, "{:?}", &pending[..4]);
        assert!(pending.contains(&(TermSpace::Entity, 100)));
        assert!(pending.contains(&(TermSpace::Predicate, 1)));
        // Settled in batches: gone from the range with the terms freed.
        let first = store.pending_reclaim(10).unwrap();
        assert_eq!(first.len(), 10);
        store.settle_pending(&first, &first).unwrap();
        assert_eq!(
            store.pending_reclaim(usize::MAX).unwrap().len(),
            n as usize - 8
        );
        let rest = store.take_replayed();
        assert_eq!(rest.len(), n as usize - 8);
        assert!(store.pending_reclaim(usize::MAX).unwrap().is_empty());
    }

    /// A recovery cut short (the process died again) resumes where it
    /// stopped: the journal left is an older prefix of the original, the
    /// pending range keeps the candidates of the chunks already undone, and
    /// the next open finishes with the same result as an uninterrupted one.
    #[test]
    fn a_recovery_cut_short_resumes_where_it_stopped() {
        struct Gone(std::path::PathBuf);
        impl Drop for Gone {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = Gone(scratch("resumed_recovery"));
        let n = 500u32;
        let added = crashed_statement(&dir.0, n);
        REPLAY_CHUNK_FOR_TEST.with(|c| c.set(100));
        REPLAY_STOP_AFTER_CHUNKS.with(|c| c.set(Some(3)));
        let failed = RocksStore::open_with(&dir.0, &RocksTuning::rocksdb_defaults());
        REPLAY_STOP_AFTER_CHUNKS.with(|c| c.set(None));
        assert!(failed.is_err(), "the injected stop");
        drop(failed);
        {
            // Three chunks were undone and deleted; the fourth was undone
            // but not deleted (it is replayed again).
            let store = RocksStore::open_with(&dir.0, &RocksTuning::rocksdb_defaults()).unwrap();
            assert!(store.journal_records().unwrap().is_empty());
            assert_eq!(store.triple_count, 1);
            assert!(added.iter().all(|t| store.contains(*t) == Ok(false)));
            let pending = store.pending_reclaim(usize::MAX).unwrap();
            assert_eq!(pending.len(), n as usize + 2);
        }
        REPLAY_CHUNK_FOR_TEST.with(|c| c.set(0));
    }

    /// A rollback that keeps its journal (for the batch that deletes it
    /// with the terms it frees) does not journal its own replay: the journal
    /// it leaves holds the transaction's records only — not one more record
    /// per undone change, which doubled the journal and the work of the
    /// next open's replay after a crash.
    #[test]
    fn a_rollback_keeping_its_journal_does_not_journal_its_replay() {
        struct Gone(std::path::PathBuf);
        impl Drop for Gone {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = Gone(scratch("rollback_keeping"));
        let mut store = RocksStore::open_with(&dir.0, &RocksTuning::rocksdb_defaults()).unwrap();
        let (kept, added, quad) = (
            IdTriple::new(1, 1, 2),
            IdTriple::new(3, 1, 4),
            IdTriple::new(5, 1, 6),
        );
        assert!(store.insert(kept));
        store.begin_journal().unwrap();
        assert!(store.insert(added));
        assert!(store.remove(kept));
        assert!(store.insert_quad(9, quad).unwrap());
        store
            .put_new_terms(&[(TermSpace::Entity, 7, "<http://ex/t>")])
            .unwrap();
        assert_eq!(store.journal_records().unwrap().len(), 4);

        store.rollback_keeping_journal().unwrap();
        assert_eq!(
            store.journal_records().unwrap().len(),
            4,
            "the rollback journaled its own replay"
        );
        assert_eq!(store.contains(kept), Ok(true));
        assert_eq!(store.contains(added), Ok(false));
        assert!(store.quads().unwrap().is_empty());
        assert!(store.journal_error().is_some(), "kept until deleted");
        let mut freed = store.take_replayed();
        freed.sort_by_key(|(space, id)| (space.tag(), *id));
        assert!(freed.contains(&(TermSpace::Entity, 7)), "{freed:?}");

        store
            .commit_journal_with(&[(TermSpace::Entity, 7)])
            .unwrap();
        assert!(store.journal_records().unwrap().is_empty());
        assert!(store.journal_error().is_none());
        assert!(store.load_terms().unwrap().is_empty());
    }

    /// A predicate on the seek path of `so_by_p` (many objects per subject)
    /// whose SPO data block in the middle is damaged on disk: the list must
    /// not come back short without an error — the seek to the next subject
    /// resets the iterator's status, so the error has to be seen before it.
    #[test]
    fn a_bad_spo_block_fails_so_by_p_instead_of_shortening_it() {
        let dir = scratch("sobyp_corrupt");
        const PRED: PredId = 3;
        let (subjects, objects) = (10u32, 1000u32);
        {
            let mut store = RocksStore::open_with(&dir, &RocksTuning::rocksdb_defaults()).unwrap();
            for s in 0..subjects {
                for o in 0..objects {
                    assert!(store.insert(IdTriple::new(100 + s, PRED, 100_000 + o)));
                }
            }
            let c = store.pred_counts(PRED);
            assert!(c.card >= SEEK_RUN * c.subjects, "not the seek path");
            store.flush().unwrap();
        }
        // The SPO table file: flip bytes a third of the way in (its data
        // blocks; the index and footer are at its end).
        let sst = {
            let db = DB::open_cf(&Options::default(), &dir, ALL_CFS).unwrap();
            let files = db.live_files().unwrap();
            let f = files
                .iter()
                .find(|f| f.column_family_name == CF_SPO)
                .expect("an spo table file");
            dir.join(f.name.trim_start_matches('/'))
        };
        let mut bytes = std::fs::read(&sst).unwrap();
        let at = bytes.len() / 3;
        for b in &mut bytes[at..at + 8] {
            *b ^= 0x5A;
        }
        std::fs::write(&sst, &bytes).unwrap();

        let store = RocksStore::open_with(&dir, &RocksTuning::rocksdb_defaults()).unwrap();
        let scope = crate::store::read_scope::ReadScope::enter();
        let got = TripleSource::so_by_p(&store, PRED);
        let err = scope.take_error();
        assert!(
            err.is_some(),
            "so_by_p answered {} of {} pairs with no read error",
            got.len(),
            subjects * objects
        );
        drop(scope);
        // The same damage seen by a full scan, for comparison.
        let scope = crate::store::read_scope::ReadScope::enter();
        let _ = TripleSource::iter_all(&store);
        assert!(scope.take_error().is_some());
        drop(scope);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Damaged `c|p` counters (well-formed values that are wrong) make
    /// `iter_all` read and sort the keys instead: never a huge allocation
    /// (an abort) or a wrapped sum (an out-of-bounds panic).
    #[test]
    fn damaged_card_counters_do_not_size_iter_all() {
        let dir = scratch("iterall_card");
        let triples: Vec<IdTriple> = (0..10u32)
            .flat_map(|i| [IdTriple::new(i, 1, 50 + i), IdTriple::new(i, 2, 70 + i)])
            .collect();
        let mut want = triples.clone();
        want.sort_unstable_by_key(|t| (t.pred, t.sub, t.obj));
        {
            let mut store = RocksStore::open_with(&dir, &RocksTuning::rocksdb_defaults()).unwrap();
            for t in &triples {
                assert!(store.insert(*t));
            }
            assert_eq!(TripleSource::iter_all(&store), want);
        }
        for bad in [1000u64, 0x0000_4000_0000_0000, u64::MAX - 5, u64::MAX] {
            {
                let store = RocksStore::open_with(&dir, &RocksTuning::rocksdb_defaults()).unwrap();
                store
                    .db
                    .put_cf(store.cf(CF_STATS), tag1(T_CARD, 1), bad.to_be_bytes())
                    .unwrap();
                store
                    .db
                    .put_cf(store.cf(CF_STATS), tag1(T_CARD, 2), bad.to_be_bytes())
                    .unwrap();
            }
            let store = RocksStore::open_with(&dir, &RocksTuning::rocksdb_defaults()).unwrap();
            assert_eq!(store.pred_counts(1).card, bad);
            let scope = crate::store::read_scope::ReadScope::enter();
            assert_eq!(TripleSource::iter_all(&store), want, "c|p = {bad:#x}");
            // The other counter-sized lists are only hints too.
            assert_eq!(TripleSource::so_by_p(&store, 1).len(), 10, "c|p = {bad:#x}");
            assert_eq!(scope.take_error(), None);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
