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
#[derive(Debug, Default)]
pub(crate) struct Candidates {
    ids: Vec<TermId>,
}

impl Candidates {
    /// The terms of a removed triple (and its graph IRI).
    pub(crate) fn note_triple(&mut self, graph: Option<u32>, t: IdTriple) {
        self.ids.push(TermId::node(t.sub));
        self.ids.push(TermId::predicate(t.pred));
        self.ids.push(TermId::node(t.obj));
        if let Some(g) = graph {
            self.ids.push(TermId::node(g));
        }
    }

    /// One more candidate.
    pub(crate) fn note(&mut self, t: TermId) {
        self.ids.push(t);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Forget every candidate.
    pub(crate) fn clear(&mut self) {
        self.ids.clear();
    }

    /// Take the candidates, sorted and de-duplicated.
    pub(crate) fn take(&mut self) -> Vec<TermId> {
        let mut v = std::mem::take(&mut self.ids);
        v.sort_unstable();
        v.dedup();
        v
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
        if self.txn.is_some() || (self.reclaim.is_empty() && self.reclaim_deferred.is_empty()) {
            return;
        }
        if !self.reclaims_ids() {
            self.reclaim.clear();
            self.reclaim_deferred.clear();
            return;
        }
        let mut cands = self.reclaim.take();
        if !self.reclaim_deferred.is_empty() {
            cands.append(&mut self.reclaim_deferred);
            cands.sort_unstable();
            cands.dedup();
        }
        let dead = self.dead_of(cands);
        self.free_dead(dead, false);
    }

    /// At commit (the transaction just closed): the transaction's candidates
    /// that nothing references in the committed state.
    pub(super) fn committed_dead(&mut self) -> Vec<TermId> {
        if !self.reclaims_ids() {
            self.reclaim.clear();
            return Vec::new();
        }
        let mut cands = self.reclaim.take();
        if !self.reclaim_deferred.is_empty() {
            cands.append(&mut self.reclaim_deferred);
            cands.sort_unstable();
            cands.dedup();
        }
        self.dead_of(cands)
    }

    /// Whether term `t` is still used: by the default graph (the `has_*`
    /// probes answer `true` on a read error) or the named graphs.
    fn term_referenced(&self, t: TermId) -> bool {
        match t.space {
            Space::Predicate => self.named.pred_refs(t.id) > 0 || self.store.has_predicate(t.id),
            Space::Entity | Space::Literal => {
                self.named.node_refs(t.id) > 0
                    || self.store.has_subject(t.id)
                    || self.store.has_object(t.id)
            }
        }
    }

    /// The candidates nothing references (and, in memory, that still name a
    /// term). Read in a scope of its own: a read error keeps the id.
    pub(super) fn dead_of(&self, cands: Vec<TermId>) -> Vec<TermId> {
        let scope = ReadScope::enter();
        let backed = self.dict.is_disk_backed();
        let dead: Vec<TermId> = cands
            .into_iter()
            .filter(|&t| {
                (backed || self.dict.resolve(t.space, t.id).is_some()) && !self.term_referenced(t)
            })
            .collect();
        if scope.take_error().is_some() {
            // Some probe failed: it answered "referenced", but be strict and
            // free nothing of this batch.
            return Vec::new();
        }
        dead
    }

    /// Free the unreferenced ids `dead` on this database's layout (see the
    /// module docs for the sequence); `rocks_persisted` when the commit's
    /// journal batch already deleted them from RocksDB. Returns how many ids
    /// were freed.
    pub(super) fn free_dead(&mut self, dead: Vec<TermId>, rocks_persisted: bool) -> usize {
        if dead.is_empty() {
            return 0;
        }
        // Stream mode: the backend frees by id. A clone sharing the on-disk
        // dictionary (a snapshot) pins it: wait until it is gone, so an id it
        // read never changes meaning under it.
        if self.store.as_disk().is_some() {
            if self.dict.is_pinned() {
                self.reclaim_deferred.extend(dead);
                return 0;
            }
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
        let dead = self.dead_of(cands);
        let n = self.free_dead(dead, false);
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
}
