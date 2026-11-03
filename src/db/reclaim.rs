//! Immediate id reclamation: the policy, and the candidate set a
//! [`Database`](super::Database) frees after an applied delete.
//!
//! **When.** After every applied delete — `DELETE DATA` / `DELETE … WHERE`,
//! quad deletes, `CLEAR` / `DROP` / `MOVE` / `COPY`, `remove_sources`, the
//! undo of a failed `LOAD`, and the rollback of a transaction's inserts —
//! the subject, predicate, object (and graph IRI) of every removed triple
//! become *candidates*. At the end of the statement (auto-commit) or at
//! `commit` (a transaction defers its frees to one batch) each candidate
//! that nothing references any more is freed:
//!
//! * **Which.** A node id is referenced while it is a subject or object of
//!   the default graph (`TripleSource::has_subject` / `has_object`, which
//!   answer `true` on a read error — never free an id when in doubt) or the
//!   named graphs count it (`NamedGraphs::node_refs`, which includes graph
//!   IRIs); a predicate while `has_predicate` or `NamedGraphs::pred_refs`.
//!   Each candidate is decided on its own probes: one a read error left in
//!   doubt is neither freed nor dropped but checked again at the next
//!   statement, up to [`CHECK_ATTEMPTS`] checks (then `gcompact --reclaim`
//!   finds it), and holds back no other candidate's free.
//! * **Sequence.** The free is persisted first (Materialize: a key-level
//!   `DiskOp::Free`; Stream: `DiskTriples::free_ids` under the writer gate;
//!   RocksDB: a `dict` CF delete batch after the triple batch, or in the
//!   transaction's journal-clearing batch), then the dictionary retires the
//!   id, the database is `touched()`, and the id is released for reuse once
//!   no pin older than the retirement remains. A snapshot database persists
//!   it at its next save.
//! * **Order.** Frees always run after the statement's redo records and
//!   write-through (`record_redo`, `persist_mutation`).
//! * **Rollback.** Terms a transaction interned new (its *minted* terms) are
//!   candidates when it rolls back; its deletes are not (they are undone).
//!
//! [`IdReclaim::Off`] keeps the pre-Phase-6 behaviour: ids are only freed by
//! an explicit [`Database::reclaim_unused`](super::Database::reclaim_unused)
//! (`gcompact --reclaim`).
//!
//! The policy of a new [`Database`](super::Database) is the process default
//! ([`default_id_reclaim`]): [`IdReclaim::Immediate`] unless
//! [`set_default_id_reclaim`] installed another (servers install `conf.ini`'s
//! `[system] id_reclaim`), with the environment variable [`ENV_ID_RECLAIM`]
//! (`on` / `off`) on top.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};

use super::Database;
use crate::dict::{Space, TermId};
use crate::error::{GStoreError, Result};
use crate::model::IdTriple;
use crate::store::read_scope::ReadScope;
use crate::store::TripleSource;

/// When a deleted term's id is freed (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IdReclaim {
    /// Free unreferenced ids at the end of each deleting statement (at
    /// `commit` inside a transaction). The default.
    #[default]
    Immediate,
    /// Never free ids automatically (the pre-Phase-6 behaviour).
    Off,
}

impl IdReclaim {
    /// The `conf.ini` / environment spelling (`on` / `off`).
    pub fn as_str(self) -> &'static str {
        match self {
            IdReclaim::Immediate => "on",
            IdReclaim::Off => "off",
        }
    }
}

impl fmt::Display for IdReclaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for IdReclaim {
    type Err = String;
    /// `on` / `immediate` / `true` / `1`, or `off` / `false` / `0`
    /// (case-insensitive).
    fn from_str(s: &str) -> std::result::Result<IdReclaim, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "on" | "immediate" | "true" | "yes" | "1" => Ok(IdReclaim::Immediate),
            "off" | "false" | "no" | "0" => Ok(IdReclaim::Off),
            other => Err(format!("id_reclaim must be on or off, not {other:?}")),
        }
    }
}

/// The environment variable overriding the default policy (`on` / `off`).
pub const ENV_ID_RECLAIM: &str = "GSTORE_ID_RECLAIM";

/// The installed process default: 0 = not installed (Immediate), 1 =
/// Immediate, 2 = Off.
static DEFAULT_POLICY: AtomicU8 = AtomicU8::new(0);

/// Install the policy new databases of this process start with (a server
/// installs `conf.ini`'s `[system] id_reclaim`). The environment
/// ([`ENV_ID_RECLAIM`]) still overrides it.
pub fn set_default_id_reclaim(policy: IdReclaim) {
    let v = match policy {
        IdReclaim::Immediate => 1,
        IdReclaim::Off => 2,
    };
    DEFAULT_POLICY.store(v, Ordering::Relaxed);
}

/// The policy a new database starts with: [`ENV_ID_RECLAIM`] when it is set
/// to a valid value, else the installed default, else
/// [`IdReclaim::Immediate`]. An invalid environment value is logged once and
/// ignored.
pub fn default_id_reclaim() -> IdReclaim {
    if let Ok(v) = std::env::var(ENV_ID_RECLAIM) {
        match v.parse::<IdReclaim>() {
            Ok(p) => return p,
            Err(e) => {
                static WARNED: std::sync::Once = std::sync::Once::new();
                WARNED.call_once(|| crate::gs_warn!("{ENV_ID_RECLAIM}: {e}; ignored"));
            }
        }
    }
    match DEFAULT_POLICY.load(Ordering::Relaxed) {
        2 => IdReclaim::Off,
        _ => IdReclaim::Immediate,
    }
}

/// The candidate ids of a statement or transaction (see the module docs).
///
/// Each is kept packed into a `u64` ([`pack`]: the space's rank above the
/// id), so [`take`](Self::take) sorts plain integers in [`TermId`] order;
/// and a triple's term equal to the one the previous triple had in the same
/// position (subject, predicate, object, graph) is not pushed again — the
/// removed triples of a statement mostly come grouped by subject, so this
/// keeps most repeats out of the sort. (Sorting one `TermId` per position
/// of every removed triple was half the cost of reclamation on an
/// in-memory database: the quiet R4 measure stage's churn.)
#[derive(Debug)]
pub(crate) struct Candidates {
    keys: Vec<u64>,
    /// The key each position of [`note_triple`](Self::note_triple) pushed
    /// last ([`NO_KEY`] when none since the last `take` / `clear`).
    last: [u64; 4],
}

/// No candidate: above every [`pack`]ed key.
const NO_KEY: u64 = u64::MAX;

/// `t` as an integer that orders like `t` (space, then id).
fn pack(t: TermId) -> u64 {
    let rank: u64 = match t.space {
        Space::Entity => 0,
        Space::Literal => 1,
        Space::Predicate => 2,
    };
    rank << 32 | u64::from(t.id)
}

/// The inverse of [`pack`].
fn unpack(key: u64) -> TermId {
    let space = match key >> 32 {
        0 => Space::Entity,
        1 => Space::Literal,
        _ => Space::Predicate,
    };
    TermId {
        space,
        id: key as u32,
    }
}

impl Default for Candidates {
    fn default() -> Candidates {
        Candidates {
            keys: Vec::new(),
            last: [NO_KEY; 4],
        }
    }
}

impl Candidates {
    /// Push `t` as position `pos`'s candidate unless it was that position's
    /// last one.
    fn note_at(&mut self, pos: usize, t: TermId) {
        let key = pack(t);
        if self.last[pos] != key {
            self.last[pos] = key;
            self.keys.push(key);
        }
    }

    /// The terms of a removed triple (and its graph IRI).
    pub(crate) fn note_triple(&mut self, graph: Option<u32>, t: IdTriple) {
        self.note_at(0, TermId::node(t.sub));
        self.note_at(1, TermId::predicate(t.pred));
        self.note_at(2, TermId::node(t.obj));
        if let Some(g) = graph {
            self.note_at(3, TermId::node(g));
        }
    }

    /// One more candidate.
    pub(crate) fn note(&mut self, t: TermId) {
        self.keys.push(pack(t));
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Forget every candidate.
    pub(crate) fn clear(&mut self) {
        self.keys.clear();
        self.last = [NO_KEY; 4];
    }

    /// Take the candidates, sorted and de-duplicated.
    pub(crate) fn take(&mut self) -> Vec<TermId> {
        let mut keys = std::mem::take(&mut self.keys);
        self.last = [NO_KEY; 4];
        keys.sort_unstable();
        keys.dedup();
        keys.into_iter().map(unpack).collect()
    }
}

/// How many checks a candidate whose reference probe keeps failing gets
/// before it is given up — left allocated, for `gcompact --reclaim`: a
/// transient read error is retried, an unreadable index page does not make
/// every later statement probe it again.
pub(crate) const CHECK_ATTEMPTS: u32 = 4;

/// Candidates whose free waits (see [`Database::ready_frees`]): on a
/// Stream-mode database, for the dictionary's pins; on any layout, after a
/// read error stopped the check of whether one of them is still referenced
/// (up to [`CHECK_ATTEMPTS`] checks). Each carries the stamp of its latest
/// candidacy, from the dictionary's pin clock
/// ([`Dictionary::pin_clock`](crate::dict::Dictionary::pin_clock)): a pin
/// taken before that stamp may have read the id while a triple still used
/// it, one taken after cannot. Kept ordered by stamp, so finding the ones no
/// live pin predates reads nothing else.
#[derive(Debug, Default)]
pub(crate) struct Deferred {
    stamp_of: HashMap<TermId, u64>,
    by_stamp: BTreeSet<(u64, TermId)>,
    /// The checks in a row whose probe of the candidate failed.
    failures: HashMap<TermId, u32>,
}

impl Deferred {
    pub(crate) fn is_empty(&self) -> bool {
        self.stamp_of.is_empty()
    }

    /// How many candidates wait.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.stamp_of.len()
    }

    pub(crate) fn clear(&mut self) {
        self.stamp_of.clear();
        self.by_stamp.clear();
        self.failures.clear();
    }

    /// `t` is a candidate again as of `stamp` (a later candidacy replaces an
    /// earlier one: the id may have been used again in between).
    fn defer(&mut self, t: TermId, stamp: u64) {
        if let Some(old) = self.stamp_of.insert(t, stamp) {
            self.by_stamp.remove(&(old, t));
        }
        self.by_stamp.insert((stamp, t));
    }

    /// Record the check of `checked` (handed out by
    /// [`take_ready`](Self::take_ready)) whose probes of `failed` (sorted)
    /// could not be read: those are put back, ready (no pin taken since can
    /// predate them) unless a newer candidacy is already recorded — except
    /// the ones whose probe failed [`CHECK_ATTEMPTS`] times in a row, which
    /// are returned, given up. The others' failure counts are forgotten.
    fn record_check(&mut self, checked: &[TermId], failed: Vec<TermId>) -> Vec<TermId> {
        if !self.failures.is_empty() {
            for t in checked {
                if failed.binary_search(t).is_err() {
                    self.failures.remove(t);
                }
            }
        }
        let mut given_up = Vec::new();
        for t in failed {
            let n = self.failures.entry(t).or_insert(0);
            *n += 1;
            if *n >= CHECK_ATTEMPTS {
                self.failures.remove(&t);
                given_up.push(t);
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(e) = self.stamp_of.entry(t) {
                e.insert(0);
                self.by_stamp.insert((0, t));
            }
        }
        given_up
    }

    /// Take the candidates stamped before `oldest_pin` (every one when no
    /// pin is alive), sorted.
    fn take_ready(&mut self, oldest_pin: Option<u64>) -> Vec<TermId> {
        let mut ready = Vec::new();
        while let Some(&(stamp, t)) = self.by_stamp.first() {
            if oldest_pin.is_some_and(|p| stamp >= p) {
                break;
            }
            self.by_stamp.pop_first();
            self.stamp_of.remove(&t);
            ready.push(t);
        }
        ready.sort_unstable();
        ready
    }
}

/// The id spaces of a database (see [`Database::id_space_stats`]); each
/// array is `[entity, literal, predicate]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdSpaceStats {
    /// Live terms.
    pub live: [u64; 3],
    /// Ids free for reuse below the limits (retired ones a pin still holds
    /// included).
    pub free: [u64; 3],
    /// Each space's id limit: one past the highest id allocated (a literal
    /// limit counts from `LITERAL_FIRST_ID`).
    pub limits: [u64; 3],
    /// The reclamation policy.
    pub policy: IdReclaim,
    /// Whether the layout can free ids (`false` for a pre-Phase-6
    /// `kvstore.kv` until `gcompact --enable-reclaim`).
    pub enabled: bool,
}

/// The outcome of [`Database::dead_of`].
#[derive(Debug, Default)]
pub(super) struct RefCheck {
    /// The candidates nothing references.
    pub(super) dead: Vec<TermId>,
    /// The candidates whose probe a read error stopped (sorted).
    pub(super) failed: Vec<TermId>,
    /// The first of those read errors.
    pub(super) error: Option<String>,
}

// ---- the Database side ------------------------------------------------------

impl Database {
    /// Set when deleted terms' ids are freed (see the module docs). Switching
    /// to [`IdReclaim::Off`] drops the candidates collected so far.
    pub fn set_id_reclaim(&mut self, policy: IdReclaim) {
        self.id_reclaim = policy;
        if policy == IdReclaim::Off {
            self.reclaim.clear();
            self.reclaim_deferred.clear();
            if self.txn.is_some() {
                self.dict.track_allocations(false);
            }
        }
    }

    /// The reclamation policy of this database.
    pub fn id_reclaim(&self) -> IdReclaim {
        self.id_reclaim
    }

    /// Whether this database frees ids at all: the policy is
    /// [`IdReclaim::Immediate`] and its layout can persist a free — every
    /// layout but a `kvstore.kv` without the `FREE_IDS` feature (a file
    /// written before Phase 6 keeps reclamation off until `gcompact
    /// --enable-reclaim` upgrades it).
    pub fn reclaims_ids(&self) -> bool {
        self.id_reclaim == IdReclaim::Immediate && self.layout_frees_ids()
    }

    /// The terms of a removed triple become candidates (nothing happens
    /// under [`IdReclaim::Off`]).
    pub(super) fn note_removed(&mut self, graph: Option<u32>, id: IdTriple) {
        if self.id_reclaim == IdReclaim::Immediate {
            self.reclaim.note_triple(graph, id);
        }
    }

    /// The end of a deleting statement: outside a transaction, free the
    /// candidates nothing references any more (inside one they wait for the
    /// commit). A persistence failure is reported at the next commit point.
    pub(super) fn flush_reclaim(&mut self) {
        if self.txn.is_some() {
            return;
        }
        self.note_rocks_replayed();
        if self.reclaim.is_empty() && self.reclaim_deferred.is_empty() {
            return;
        }
        if !self.reclaims_ids() {
            self.reclaim.clear();
            self.reclaim_deferred.clear();
            return;
        }
        let cands = self.reclaim.take();
        let ready = self.ready_frees(cands);
        let dead = self.dead_or_requeue(ready);
        self.free_dead(dead, false);
    }

    /// At commit (the transaction just closed): the transaction's candidates
    /// that nothing references in the committed state.
    pub(super) fn committed_dead(&mut self) -> Vec<TermId> {
        self.note_rocks_replayed();
        if !self.reclaims_ids() {
            self.reclaim.clear();
            self.reclaim_deferred.clear();
            return Vec::new();
        }
        let cands = self.reclaim.take();
        let ready = self.ready_frees(cands);
        self.dead_or_requeue(ready)
    }

    /// Whether frees wait (see [`Deferred`]): for the dictionary's pins, or
    /// to be checked again after a read error.
    pub(crate) fn has_deferred_frees(&self) -> bool {
        !self.reclaim_deferred.is_empty()
    }

    /// Run the frees that wait and nothing holds any more (see
    /// [`Deferred`]), then a commit point — what a `ConcurrentDb` does when
    /// it is dropped, its snapshots' pins gone. Nothing inside a
    /// transaction.
    pub(crate) fn retry_deferred_frees(&mut self) -> Result<()> {
        if self.txn.is_some() || self.reclaim_deferred.is_empty() {
            return Ok(());
        }
        self.flush_reclaim();
        self.commit_point()
    }

    /// The candidates whose check and free may run now, from the
    /// statement's (or transaction's) `cands` and the deferred ones.
    ///
    /// A Stream-mode database frees an id in the on-disk dictionary every
    /// clone shares, so the free must wait for each pin taken before the
    /// id's candidacy: such a reader may have seen the id in use. `cands`
    /// join the deferred ones, stamped now, and come back those no live pin
    /// predates — found without reading the store, so a database held by a
    /// long-lived pin (a `ConcurrentDb`'s snapshots) pays per statement for
    /// its own candidates only, not for the backlog. A pin taken after a
    /// candidacy does not hold it back: that reader never saw the id used
    /// (any later use would have to be deleted again — a newer candidacy).
    ///
    /// Other layouts keep a pinned id retired in the in-memory dictionary
    /// instead (see [`free_dead`](Self::free_dead)): every candidate is
    /// ready, with any whose probe a read error stopped put back.
    fn ready_frees(&mut self, cands: Vec<TermId>) -> Vec<TermId> {
        let oldest_pin = if self.store.as_disk().is_some() {
            self.dict.oldest_pin()
        } else {
            None
        };
        if self.reclaim_deferred.is_empty() && oldest_pin.is_none() {
            return cands;
        }
        if !cands.is_empty() {
            let stamp = self.dict.pin_clock();
            for t in cands {
                self.reclaim_deferred.defer(t, stamp);
            }
        }
        self.reclaim_deferred.take_ready(oldest_pin)
    }

    /// The candidates of `ready` nothing references ([`dead_of`](Self::dead_of)).
    /// One whose probe a read error stopped waits for the next statement's
    /// check instead of being dropped — the others' are decided on their own
    /// probes, so it holds back no other free — for [`CHECK_ATTEMPTS`]
    /// checks; then it is left allocated (`gcompact --reclaim` frees it), so
    /// an unreadable page does not make every later statement probe it.
    fn dead_or_requeue(&mut self, ready: Vec<TermId>) -> Vec<TermId> {
        if ready.is_empty() {
            return ready;
        }
        let check = self.dead_of(&ready);
        let given_up = self.reclaim_deferred.record_check(&ready, check.failed);
        if !given_up.is_empty() {
            crate::gs_warn!(
                "id reclamation: the reference check of {} deleted term(s) failed {CHECK_ATTEMPTS} \
                 times ({}); their ids stay allocated until `gcompact --reclaim`",
                given_up.len(),
                check.error.as_deref().unwrap_or("a read error")
            );
        }
        check.dead
    }

    /// Whether term `t` is still used: by the default graph (the `has_*`
    /// probes answer `true` on a read error) or the named graphs.
    fn term_referenced(&self, t: TermId) -> bool {
        #[cfg(test)]
        test_hooks::probed(t);
        match t.space {
            Space::Predicate => self.named.pred_refs(t.id) > 0 || self.store.has_predicate(t.id),
            Space::Entity | Space::Literal => {
                self.named.node_refs(t.id) > 0
                    || self.store.has_subject(t.id)
                    || self.store.has_object(t.id)
            }
        }
    }

    /// Check `cands` (sorted): the ones nothing references (and, in memory,
    /// that still name a term), and the ones a read error left undecided.
    /// Each probe reads in a scope of its own, so a failed one decides
    /// nothing but its own candidate (its answer, "referenced", may be
    /// wrong: be strict and neither free it nor drop it).
    pub(super) fn dead_of(&self, cands: &[TermId]) -> RefCheck {
        let backed = self.dict.is_disk_backed();
        #[cfg(test)]
        let fail_all = test_hooks::take_fail_next();
        let mut check = RefCheck::default();
        for &t in cands {
            if !backed && self.dict.resolve(t.space, t.id).is_none() {
                continue;
            }
            let scope = ReadScope::enter();
            let referenced = self.term_referenced(t);
            #[cfg(test)]
            if fail_all {
                crate::store::read_scope::note_read_error("injected reference-probe read error");
            }
            match scope.take_error() {
                Some(e) => {
                    check.error.get_or_insert(e);
                    check.failed.push(t);
                }
                None if !referenced => check.dead.push(t),
                None => {}
            }
        }
        check
    }

    /// Free the unreferenced ids `dead` on this database's layout (see the
    /// module docs for the sequence); `rocks_persisted` when the commit's
    /// journal batch already deleted them from RocksDB. Returns how many ids
    /// were freed.
    pub(super) fn free_dead(&mut self, dead: Vec<TermId>, rocks_persisted: bool) -> usize {
        if dead.is_empty() {
            return 0;
        }
        // Stream mode: the backend frees by id; the candidates waited for
        // the pins taken before them (see `ready_frees`).
        if self.store.as_disk().is_some() {
            let n = self.stream_free(&dead);
            if n > 0 {
                self.touched();
            }
            return n;
        }
        // Persist first (a key-level free for Materialize, the `dict` CF for
        // RocksDB), then retire in the dictionary.
        let persisted = self.persist_frees(dead, rocks_persisted);
        let mut n = 0;
        for t in persisted {
            if self.dict.retire(t.space, t.id) {
                n += 1;
            }
        }
        if n > 0 {
            self.touched();
        }
        self.dict.release();
        n
    }

    /// Conservative id-reclamation pass (gStore's `freelist` rebuild; `gcompact
    /// --reclaim`): free the id of every term that no longer appears anywhere —
    /// neither in the default graph nor in any named graph, as subject, object,
    /// predicate or graph IRI — on every layout, whatever the policy. Returns
    /// the number of ids freed (it advances
    /// [`mutation_count`](Self::mutation_count) when that is not zero).
    ///
    /// Refused inside a transaction (its frees belong to its commit), and on
    /// a `kvstore.kv` without the `FREE_IDS` feature (`gcompact
    /// --enable-reclaim` upgrades it).
    pub fn reclaim_unused(&mut self) -> Result<usize> {
        if self.txn.is_some() {
            return Err(GStoreError::Database(
                "reclaim_unused: commit or roll back the open transaction first".into(),
            ));
        }
        if !self.layout_frees_ids() {
            return Err(GStoreError::Database(
                "this kvstore.kv predates id reclamation (no FREE_IDS feature); upgrade it \
                 with `gcompact --enable-reclaim` first"
                    .into(),
            ));
        }
        let cands = self.all_terms()?;
        let check = self.dead_of(&cands);
        if !check.failed.is_empty() {
            crate::gs_warn!(
                "reclaim_unused: {} term(s) not checked ({}); left allocated",
                check.failed.len(),
                check.error.as_deref().unwrap_or("a read error")
            );
        }
        let dead = check.dead;
        let n = if self.store.as_disk().is_some() && self.dict.is_pinned() {
            // A clone sharing the on-disk dictionary may still read these
            // ids (when they died is not known): they wait for every pin
            // alive now.
            let stamp = self.dict.pin_clock();
            for t in dead {
                self.reclaim_deferred.defer(t, stamp);
            }
            0
        } else {
            self.free_dead(dead, false)
        };
        self.commit_point()?;
        Ok(n)
    }

    /// Check the structures id reclamation relies on, reading everything
    /// once: every term a default-graph triple or a named-graph quad uses
    /// (graph IRIs included) resolves in the dictionary; the named graphs'
    /// reference counts equal a recount; and an on-disk store's free-id list
    /// is sound (each free id below its space's high-water mark names no
    /// term, the counts match the list, none is the highest). `Err` names the
    /// first violation. For tests and `gcompact --check`.
    pub fn verify_integrity(&self) -> Result<()> {
        let scope = ReadScope::enter();
        let missing = |what: &str, id: u32| {
            GStoreError::Database(format!("integrity: {what} id {id} names no term"))
        };
        for t in self.store.iter_all() {
            if self.dict.term(t.sub).is_none() {
                return Err(missing("subject", t.sub));
            }
            if self.dict.pred(t.pred).is_none() {
                return Err(missing("predicate", t.pred));
            }
            if self.dict.term(t.obj).is_none() {
                return Err(missing("object", t.obj));
            }
        }
        for (&g, graph) in self.named.iter() {
            if self.dict.term(g).is_none() {
                return Err(missing("graph", g));
            }
            for t in graph.iter_all() {
                if self.dict.term(t.sub).is_none() || self.dict.term(t.obj).is_none() {
                    return Err(missing("named-graph node", t.sub));
                }
                if self.dict.pred(t.pred).is_none() {
                    return Err(missing("named-graph predicate", t.pred));
                }
            }
        }
        self.named
            .verify_refs()
            .map_err(|e| GStoreError::Database(format!("integrity: {e}")))?;
        if let Some(dt) = self.store.as_disk() {
            dt.disk_store()?.verify_free_list()?;
        }
        if let Some(sink) = self.disk_guard().as_ref() {
            sink.store.verify_free_list()?;
        }
        if let Some(e) = scope.take_error() {
            return Err(GStoreError::Database(format!(
                "integrity: a read failed: {e}"
            )));
        }
        Ok(())
    }

    /// The number of term ids still allocated in each space `[entity,
    /// literal, predicate]` that name a term (live terms), the ids that are
    /// free for reuse, and each space's id limit (high-water mark) — what
    /// `gmonitor` shows.
    pub fn id_space_stats(&self) -> IdSpaceStats {
        let live = [
            self.dict.entity_num() as u64,
            self.dict.literal_num() as u64,
            self.dict.predicate_num() as u64,
        ];
        let limits = Space::ALL.map(|s| u64::from(self.dict.id_limit(s) - s.first_id()));
        let free = if let Some(dt) = self.store.as_disk() {
            dt.disk_store().map(|s| s.free_counts()).unwrap_or_default()
        } else {
            let f = self.dict.free_counts();
            let r = self.dict.retired_counts();
            [0, 1, 2].map(|i| (f[i] + r[i]) as u64)
        };
        IdSpaceStats {
            live,
            free,
            limits,
            policy: self.id_reclaim,
            enabled: self.layout_frees_ids(),
        }
    }

    /// Turn id reclamation on for a `kvstore.kv` written before Phase 6
    /// (`gcompact --enable-reclaim`): the file gets the `FREE_IDS` feature
    /// and becomes version 2 — **binaries older than Phase 6 refuse it from
    /// then on** (downgrade: `gexport` with this binary, `gbuild` with the
    /// old one). Committed durably here. `Ok(false)` when the layout frees
    /// ids already (every layout but such a file). Refused inside a
    /// transaction.
    pub fn enable_id_reclaim(&mut self) -> Result<bool> {
        if self.txn.is_some() {
            return Err(GStoreError::Database(
                "enable_id_reclaim: commit or roll back the open transaction first".into(),
            ));
        }
        if self.layout_frees_ids() {
            return Ok(false);
        }
        self.enable_layout_free_ids()?;
        self.commit_point()?;
        Ok(true)
    }

    /// Every term id of the dictionary (in memory, or read from the
    /// out-of-core backend).
    fn all_terms(&self) -> Result<Vec<TermId>> {
        if !self.dict.is_disk_backed() {
            return Ok(self.dict.unreferenced(|_| false));
        }
        let mut out = Vec::new();
        for space in Space::ALL {
            self.dict.for_each_term(space, space.first_id(), |id, _| {
                out.push(TermId { space, id });
                std::ops::ControlFlow::Continue(())
            })?;
        }
        Ok(out)
    }
}

/// Test-only instrumentation of the reclamation check: a count of the
/// reference probes (`term_referenced`) this thread ran, a one-shot injected
/// read error in every probe of the next [`Database::dead_of`], and a term
/// every probe of fails.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::Cell;

    use crate::dict::TermId;

    thread_local! {
        static PROBES: Cell<u64> = const { Cell::new(0) };
        static FAIL_NEXT: Cell<bool> = const { Cell::new(false) };
        static UNREADABLE: Cell<Option<TermId>> = const { Cell::new(None) };
    }

    /// A reference probe of `t`: counted, and failing (a read error) while
    /// `t` is [unreadable](set_unreadable).
    pub(super) fn probed(t: TermId) {
        PROBES.with(|p| p.set(p.get() + 1));
        if UNREADABLE.with(Cell::get) == Some(t) {
            crate::store::read_scope::note_read_error("injected unreadable index page");
        }
    }

    /// Make every reference probe of `t` on this thread record a read error
    /// (an unreadable index page under its key), or none with `None`.
    pub(crate) fn set_unreadable(t: Option<TermId>) {
        UNREADABLE.with(|u| u.set(t));
    }

    /// Reference probes run on this thread so far.
    pub(crate) fn probes() -> u64 {
        PROBES.with(Cell::get)
    }

    /// Make every probe of the next check of this thread record a read
    /// error.
    pub(crate) fn fail_next_check() {
        FAIL_NEXT.with(|f| f.set(true));
    }

    pub(super) fn take_fail_next() -> bool {
        FAIL_NEXT.with(|f| f.replace(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_parses_and_prints() {
        assert_eq!("on".parse::<IdReclaim>(), Ok(IdReclaim::Immediate));
        assert_eq!(" OFF ".parse::<IdReclaim>(), Ok(IdReclaim::Off));
        assert!("maybe".parse::<IdReclaim>().is_err());
        assert_eq!(IdReclaim::Off.to_string(), "off");
        assert_eq!(IdReclaim::default(), IdReclaim::Immediate);
    }

    #[test]
    fn candidates_dedupe() {
        let mut c = Candidates::default();
        c.note_triple(Some(9), IdTriple::new(1, 2, 1));
        c.note_triple(None, IdTriple::new(1, 2, 3));
        let v = c.take();
        assert_eq!(v.len(), 4, "{v:?}"); // nodes 1, 3, 9 and predicate 2
        assert!(c.is_empty());
    }

    /// The packed, repeat-skipping candidate set takes exactly what sorting
    /// and de-duplicating every noted `TermId` gives — across all three
    /// spaces, id 0, the largest ids, graphs, `note`, and notes after a
    /// `take` or a `clear` (a term noted again then is not skipped).
    #[test]
    fn candidates_take_the_sorted_set_of_every_noted_term() {
        use crate::model::LITERAL_FIRST_ID;
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |n: u32| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % u64::from(n)) as u32
        };
        let mut c = Candidates::default();
        for round in 0..20 {
            let mut want = BTreeSet::new();
            // Runs of one subject (the usual order), repeated predicates.
            let triples = 50 + next(400);
            let mut sub = next(30);
            for _ in 0..triples {
                if next(4) == 0 {
                    sub = match next(3) {
                        0 => 0,
                        1 => LITERAL_FIRST_ID - 1,
                        _ => next(30),
                    };
                }
                let pred = [0, 1, u32::MAX, next(5)][next(4) as usize];
                let obj = match next(4) {
                    0 => LITERAL_FIRST_ID + next(20),
                    1 => u32::MAX,
                    2 => 0,
                    _ => next(30),
                };
                let graph = (next(3) == 0).then(|| next(4));
                let t = IdTriple::new(sub, pred, obj);
                c.note_triple(graph, t);
                want.insert(TermId::node(sub));
                want.insert(TermId::predicate(pred));
                want.insert(TermId::node(obj));
                if let Some(g) = graph {
                    want.insert(TermId::node(g));
                }
                if next(50) == 0 {
                    let extra = TermId::node(next(40));
                    c.note(extra);
                    want.insert(extra);
                }
            }
            if round % 5 == 4 {
                c.clear();
                assert!(c.is_empty());
                continue;
            }
            let got = c.take();
            let want: Vec<TermId> = want.into_iter().collect();
            assert_eq!(got, want, "round {round}");
            assert!(c.is_empty());
        }
        // A term noted right before a `take` / `clear` is noted again after.
        let t = IdTriple::new(7, 3, LITERAL_FIRST_ID);
        c.note_triple(None, t);
        assert_eq!(c.take().len(), 3);
        c.note_triple(None, t);
        assert_eq!(c.take().len(), 3);
        c.note_triple(None, t);
        c.clear();
        c.note_triple(None, t);
        assert_eq!(c.take().len(), 3);
    }

    #[test]
    fn packed_candidates_order_like_term_ids() {
        use crate::model::LITERAL_FIRST_ID;
        let ids = [
            TermId::node(0),
            TermId::node(5),
            TermId::node(LITERAL_FIRST_ID - 1),
            TermId::node(LITERAL_FIRST_ID),
            TermId::node(u32::MAX),
            TermId::predicate(0),
            TermId::predicate(u32::MAX),
        ];
        for a in ids {
            assert_eq!(unpack(pack(a)), a);
            assert!(pack(a) < NO_KEY);
            for b in ids {
                assert_eq!(pack(a).cmp(&pack(b)), a.cmp(&b), "{a:?} {b:?}");
            }
        }
    }
}
