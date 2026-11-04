//! How a query uses the VS-tree: the policy, its switches and its counters.
//!
//! The engine intersects the VS-tree's signature candidates into the exact
//! per-variable candidate sets of a BGP (`candidates::generate`), and those
//! sets feed the cost-based planner — so *which* set a variable ends up with
//! decides the plan, and the plan decides the solution order. A policy may
//! therefore only change **how** a set is computed, never the set itself.
//!
//! * [`VsPolicy::Tree`] — the tree for every subject variable, as before
//!   this policy existed: an in-memory tree probes a small candidate set
//!   entry by entry (`VsTree::probe`), anything else walks the tree.
//! * [`VsPolicy::Auto`] (the default) — the same sets, computed more cheaply
//!   where that is provably exact:
//!   - a variable whose candidate set already satisfies every bit the tree
//!     would test (all its edges with a known predicate also have a known
//!     neighbour, so the set was cut from exactly those edges) keeps its set
//!     minus non-entities, without touching the tree (*skipped*);
//!   - an out-of-core tree (`vstree.kv`, which cannot probe, and whose walk
//!     reads every page whose union holds the query) is not read at all: a
//!     candidate set to refine has the remaining predicate bits tested
//!     against the store (*store-checked*), and a variable without a set
//!     (all its edges' neighbours are variables, so its query signature is
//!     predicate bits only) gets the entities with those bits from the
//!     store's posting lists (*listed*). An entity's signature has the bit
//!     of predicate `p` in direction `d` iff the entity has an edge in `d`
//!     whose predicate encodes to that bit, which the store answers exactly
//!     (looking candidates up or merging posting lists, whichever its exact
//!     counts make cheaper);
//!   - everything else (an in-memory tree's probes and walks) as `Tree`.
//!
//!   On a tree consistent with the store the sets equal `Tree`'s, so plans,
//!   answers and their order are identical. After deletes the stored
//!   signatures are a superset (the tree is not shrunk): an in-memory tree
//!   gives the same sets as `Tree`; for an out-of-core tree the store's
//!   answer is exact against the current data and may keep fewer candidates
//!   than the stale entries would — the same answers, the order a freshly
//!   rebuilt tree would give (a reopened database drops a stale `vstree.kv`
//!   altogether).
//! * [`VsPolicy::Off`] — never consult the tree (as without one): the same
//!   answers, but plans — and so the order of solutions without `ORDER BY` —
//!   may differ.
//!
//! The policy comes from, in order: the evaluator
//! ([`Evaluator::with_vs_policy`](super::Evaluator::with_vs_policy)), a
//! [`scoped_vs_policy`] override of the current thread, the process default
//! ([`set_default_vs_policy`]), and the environment variable
//! `GSTORE_VS_POLICY=auto|tree|off` (read once). Every decision depends only
//! on the query, the store's exact counts and the tree's kind, never on time,
//! so it is deterministic; [`vs_stats`] counts them per thread.

use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};

/// How a query uses the VS-tree (see the [module docs](self)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VsPolicy {
    /// Exact sets, computed without the tree where that is provably exact.
    #[default]
    Auto,
    /// The tree for every subject variable (the behaviour before `Auto`).
    Tree,
    /// Never the tree.
    Off,
}

impl VsPolicy {
    /// Parse `auto`, `tree` (also `on`) or `off` (ASCII case-insensitive).
    pub fn parse(s: &str) -> Option<VsPolicy> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "policy" => Some(VsPolicy::Auto),
            "tree" | "on" | "always" => Some(VsPolicy::Tree),
            "off" | "never" | "none" => Some(VsPolicy::Off),
            _ => None,
        }
    }

    /// The name [`parse`](Self::parse) reads back.
    pub fn as_str(self) -> &'static str {
        match self {
            VsPolicy::Auto => "auto",
            VsPolicy::Tree => "tree",
            VsPolicy::Off => "off",
        }
    }

    fn code(self) -> u8 {
        match self {
            VsPolicy::Auto => 0,
            VsPolicy::Tree => 1,
            VsPolicy::Off => 2,
        }
    }

    fn from_code(c: u8) -> Option<VsPolicy> {
        match c {
            0 => Some(VsPolicy::Auto),
            1 => Some(VsPolicy::Tree),
            2 => Some(VsPolicy::Off),
            _ => None,
        }
    }
}

/// The environment variable read (once) for the process default.
pub const VS_POLICY_ENV: &str = "GSTORE_VS_POLICY";

/// Process default; `UNSET` until set or read from the environment.
static DEFAULT: AtomicU8 = AtomicU8::new(UNSET);
const UNSET: u8 = u8::MAX;

thread_local! {
    static OVERRIDE: Cell<Option<VsPolicy>> = const { Cell::new(None) };
    static STATS: Cell<VsStats> = const { Cell::new(VsStats::ZERO) };
}

/// The built-in default: [`VsPolicy::Auto`] — for the binaries, the tests
/// and every user of the library. (Tests of the tree's own walk set
/// [`VsPolicy::Tree`] with [`scoped_vs_policy`].)
const BUILT_IN_DEFAULT: VsPolicy = VsPolicy::Auto;

/// The process default: [`set_default_vs_policy`]'s value, else
/// `GSTORE_VS_POLICY` (an unknown value is ignored), else [`VsPolicy::Auto`].
pub fn default_vs_policy() -> VsPolicy {
    if let Some(p) = VsPolicy::from_code(DEFAULT.load(Ordering::Relaxed)) {
        return p;
    }
    let p = std::env::var(VS_POLICY_ENV)
        .ok()
        .and_then(|v| VsPolicy::parse(&v))
        .unwrap_or(BUILT_IN_DEFAULT);
    // A concurrent `set_default_vs_policy` wins over the environment.
    let _ = DEFAULT.compare_exchange(UNSET, p.code(), Ordering::Relaxed, Ordering::Relaxed);
    VsPolicy::from_code(DEFAULT.load(Ordering::Relaxed)).unwrap_or(p)
}

/// Set the process default (over `GSTORE_VS_POLICY`).
pub fn set_default_vs_policy(policy: VsPolicy) {
    DEFAULT.store(policy.code(), Ordering::Relaxed);
}

/// The policy queries evaluated on this thread use unless their evaluator
/// sets one: the innermost [`scoped_vs_policy`], else the process default.
pub fn current_vs_policy() -> VsPolicy {
    OVERRIDE.with(Cell::get).unwrap_or_else(default_vs_policy)
}

/// Use `policy` for the queries evaluated on this thread until the guard is
/// dropped (then the previous override, if any, applies again). The guard is
/// `!Send`: it must be dropped on the thread that made it.
pub fn scoped_vs_policy(policy: VsPolicy) -> VsPolicyGuard {
    let prev = OVERRIDE.with(|o| o.replace(Some(policy)));
    VsPolicyGuard {
        prev,
        _not_send: std::marker::PhantomData,
    }
}

/// Restores the previous thread override on drop (see [`scoped_vs_policy`]).
#[must_use = "the override ends when the guard is dropped"]
pub struct VsPolicyGuard {
    prev: Option<VsPolicy>,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl Drop for VsPolicyGuard {
    fn drop(&mut self) {
        OVERRIDE.with(|o| o.set(self.prev));
    }
}

/// Per-thread counts of the VS-tree decisions, one per subject variable with
/// a non-empty query signature (see the [module docs](self)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VsStats {
    /// Candidate set kept as it was (minus non-entities), tree not consulted.
    pub skipped: u64,
    /// Candidate set tested against the store (out-of-core tree).
    pub store_checked: u64,
    /// First set taken from the store's posting lists (out-of-core tree).
    pub listed: u64,
    /// Candidate set probed entry by entry in an in-memory tree.
    pub probed: u64,
    /// Tree walked and intersected into an existing candidate set.
    pub walked: u64,
    /// Tree walked to give a variable without candidates its first set.
    pub seeded: u64,
    /// Tree present but not consulted because of [`VsPolicy::Off`].
    pub off: u64,
}

impl VsStats {
    const ZERO: VsStats = VsStats {
        skipped: 0,
        store_checked: 0,
        listed: 0,
        probed: 0,
        walked: 0,
        seeded: 0,
        off: 0,
    };

    /// Total decisions.
    pub fn total(&self) -> u64 {
        self.skipped
            + self.store_checked
            + self.listed
            + self.probed
            + self.walked
            + self.seeded
            + self.off
    }

    /// Decisions that read the tree (probes and walks).
    pub fn tree_reads(&self) -> u64 {
        self.probed + self.walked + self.seeded
    }
}

/// This thread's decision counts since it started or the last
/// [`reset_vs_stats`].
pub fn vs_stats() -> VsStats {
    STATS.with(Cell::get)
}

/// Zero this thread's decision counts.
pub fn reset_vs_stats() {
    STATS.with(|s| s.set(VsStats::ZERO));
}

/// One decision of the kind `f` selects.
pub(crate) fn count(f: impl FnOnce(&mut VsStats)) {
    STATS.with(|s| {
        let mut v = s.get();
        f(&mut v);
        s.set(v);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips() {
        for p in [VsPolicy::Auto, VsPolicy::Tree, VsPolicy::Off] {
            assert_eq!(VsPolicy::parse(p.as_str()), Some(p));
            assert_eq!(VsPolicy::from_code(p.code()), Some(p));
        }
        assert_eq!(VsPolicy::parse(" ON "), Some(VsPolicy::Tree));
        assert_eq!(VsPolicy::parse("bogus"), None);
    }

    #[test]
    fn scoped_override_nests_and_restores() {
        let base = current_vs_policy();
        {
            let _a = scoped_vs_policy(VsPolicy::Off);
            assert_eq!(current_vs_policy(), VsPolicy::Off);
            {
                let _b = scoped_vs_policy(VsPolicy::Tree);
                assert_eq!(current_vs_policy(), VsPolicy::Tree);
            }
            assert_eq!(current_vs_policy(), VsPolicy::Off);
        }
        assert_eq!(current_vs_policy(), base);
    }

    #[test]
    fn stats_count_per_thread() {
        reset_vs_stats();
        count(|s| s.skipped += 1);
        count(|s| s.seeded += 2);
        let s = vs_stats();
        assert_eq!(
            (s.skipped, s.seeded, s.total(), s.tree_reads()),
            (1, 2, 3, 2)
        );
        std::thread::spawn(|| assert_eq!(vs_stats(), VsStats::default()))
            .join()
            .unwrap();
        reset_vs_stats();
        assert_eq!(vs_stats(), VsStats::default());
    }
}
