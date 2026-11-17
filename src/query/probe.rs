//! The hash index both executors probe for OPTIONAL, MINUS and the joins
//! whose shared variables some solutions leave unbound (review
//! sparqlconf#2 / query#10).
//!
//! SPARQL's compatible-merge treats an unbound variable as a wildcard: a
//! solution that leaves a shared variable unbound is compatible with every
//! value of it. A plain hash table keyed on the shared variables cannot
//! answer that, which is why OPTIONAL and MINUS used to compare every left
//! solution with every right one, and why a join fell back to the full
//! cross product as soon as one row left a shared variable unbound —
//! quadratic work on results the streaming executor produced in linear time.
//!
//! [`RowIndex`] splits the right operand by *which* shared variables each
//! row binds (in practice one or two groups: an OPTIONAL's matched and
//! unmatched rows) and hashes every group on exactly those. A left solution
//! then looks each group up on the shared variables both sides bind — the
//! group's whole key when it binds all of them, a lazily built index on the
//! part it binds otherwise — and a group it shares no bound variable with
//! matches it whole. The candidates are exactly the rows that agree with the
//! left solution on every shared variable both bind, in the right operand's
//! order, so the operators keep their output order (left-major,
//! right-minor) and the work is the size of the output, not the product.
//!
//! The partial indexes are bounded (round-3 review): together they hold at
//! most [`PARTIAL_INDEX_FACTOR`] times the operand's rows. Left solutions
//! binding many different parts of a group's key (each a VALUES row
//! binding another subset, say) used to build and keep one full index of
//! the group per part — memory growing as |right| × parts, 1.6 GB for
//! 1,000 parts over 10,000 rows; once the bound is reached, a part without
//! an index is answered by a scan of its group, as the nested loop did.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::ControlFlow;

use super::{Binding, FxBuildHasher};

/// The partial indexes of a [`RowIndex`] hold at most this many times its
/// rows together; a part without an index past that is scanned.
pub(super) const PARTIAL_INDEX_FACTOR: usize = 4;

/// Row indices of the right operand, ascending.
type Rows = Vec<usize>;
/// Key values (one per key variable, in key order) → the rows holding them.
type KeyIndex = HashMap<Vec<u32>, Rows, FxBuildHasher>;

/// The rows of a solution set that bind the same shared variables.
struct Group {
    /// The shared variables every row of the group binds (and no other
    /// shared variable), in the order the shared variables were given.
    key: Vec<usize>,
    /// The group's rows, ascending.
    rows: Rows,
    /// The rows by their values of `key` (empty when `key` is).
    full: KeyIndex,
    /// Indexes on a strict, non-empty part of `key`, built the first time a
    /// left solution binds just that part. Keyed by the part.
    partial: RefCell<HashMap<Vec<usize>, KeyIndex>>,
}

impl Group {
    /// The group's rows that hold `values` in the variables `part`, which
    /// is a strict, non-empty part of the key (a copy: the index may be
    /// built here, and the caller's sink may probe again). An index on
    /// `part` is built only while `held` (the entries every partial index
    /// of the [`RowIndex`] holds) stays within `cap`; past it the group is
    /// scanned.
    fn partial_lookup(
        &self,
        all: &[Binding],
        part: &[usize],
        values: &[u32],
        held: &Cell<usize>,
        cap: usize,
    ) -> Rows {
        if let Some(found) = self.partial.borrow().get(part) {
            return found.get(values).cloned().unwrap_or_default();
        }
        let entries = held.get().saturating_add(self.rows.len());
        if entries > cap {
            return self
                .rows
                .iter()
                .copied()
                .filter(|&ri| {
                    part.iter()
                        .zip(values)
                        .all(|(&v, &x)| all[ri][v].unwrap_or(u32::MAX) == x)
                })
                .collect();
        }
        held.set(entries);
        let mut index = KeyIndex::default();
        for &ri in &self.rows {
            let k = part
                .iter()
                .map(|&v| all[ri][v].unwrap_or(u32::MAX))
                .collect();
            index.entry(k).or_default().push(ri);
        }
        let found = index.get(values).cloned().unwrap_or_default();
        self.partial.borrow_mut().insert(part.to_vec(), index);
        found
    }
}

/// A solution set (the right operand of a join, OPTIONAL or MINUS) indexed
/// on the shared variables, for finding the rows that agree with a left
/// solution on every shared variable both bind. Built over the rows it is
/// given; every lookup takes the same slice again.
pub(super) struct RowIndex {
    /// The rows grouped by the shared variables they bind, in the order of
    /// each group's first row.
    groups: Vec<Group>,
    /// The entries (row references) every group's partial indexes hold
    /// together, and the most they may ([`PARTIAL_INDEX_FACTOR`] × rows).
    partial_held: Cell<usize>,
    partial_cap: usize,
}

impl RowIndex {
    /// Index `rows` on `shared`, the variables the two operands share
    /// (which variables are listed only affects speed: the operators still
    /// check full compatibility on what a lookup returns).
    pub(super) fn new(rows: &[Binding], shared: &[usize]) -> RowIndex {
        let mut vars: Vec<usize> = Vec::with_capacity(shared.len());
        for &v in shared {
            if !vars.contains(&v) {
                vars.push(v);
            }
        }
        let mut groups: Vec<Group> = Vec::new();
        // The group of each binding pattern seen so far.
        let mut by_key: HashMap<Vec<usize>, usize, FxBuildHasher> = HashMap::default();
        let mut key: Vec<usize> = Vec::with_capacity(vars.len());
        for (ri, r) in rows.iter().enumerate() {
            key.clear();
            key.extend(vars.iter().copied().filter(|&v| r[v].is_some()));
            let gi = match by_key.get(key.as_slice()) {
                Some(&gi) => gi,
                None => {
                    by_key.insert(key.clone(), groups.len());
                    groups.push(Group {
                        key: key.clone(),
                        rows: Vec::new(),
                        full: KeyIndex::default(),
                        partial: RefCell::default(),
                    });
                    groups.len() - 1
                }
            };
            let g = &mut groups[gi];
            g.rows.push(ri);
            if !g.key.is_empty() {
                let k = g.key.iter().map(|&v| r[v].unwrap_or(u32::MAX)).collect();
                g.full.entry(k).or_default().push(ri);
            }
        }
        RowIndex {
            groups,
            partial_held: Cell::new(0),
            partial_cap: PARTIAL_INDEX_FACTOR.saturating_mul(rows.len()),
        }
    }

    /// The entries the partial indexes hold (a work counter for tests).
    #[cfg(test)]
    pub(super) fn partial_entries(&self) -> usize {
        self.partial_held.get()
    }

    /// Feed `f` the index of every row of `rows` (the slice the index was
    /// built over) that agrees with `l` on each shared variable both bind,
    /// ascending, until `f` breaks.
    pub(super) fn for_each_candidate<B>(
        &self,
        rows: &[Binding],
        l: &Binding,
        mut f: impl FnMut(usize) -> ControlFlow<B>,
    ) -> ControlFlow<B> {
        match self.groups.as_slice() {
            [] => ControlFlow::Continue(()),
            [g] => self.group_candidates(g, rows, l, &mut f),
            _ => {
                // Several groups: collect, then visit in the operand's order.
                let mut all: Rows = Vec::new();
                for g in &self.groups {
                    let _ = self.group_candidates(g, rows, l, &mut |ri| {
                        all.push(ri);
                        ControlFlow::<()>::Continue(())
                    });
                }
                all.sort_unstable();
                for ri in all {
                    f(ri)?;
                }
                ControlFlow::Continue(())
            }
        }
    }

    /// The candidates of one group, ascending (see [`for_each_candidate`](Self::for_each_candidate)).
    fn group_candidates<B>(
        &self,
        g: &Group,
        rows: &[Binding],
        l: &Binding,
        f: &mut impl FnMut(usize) -> ControlFlow<B>,
    ) -> ControlFlow<B> {
        let bound = g.key.iter().filter(|&&v| l[v].is_some()).count();
        if bound == 0 {
            // No shared variable bound on both sides: every row agrees.
            for &ri in &g.rows {
                f(ri)?;
            }
        } else if bound == g.key.len() {
            let k: Vec<u32> = g.key.iter().map(|&v| l[v].unwrap_or(u32::MAX)).collect();
            if let Some(found) = g.full.get(&k) {
                for &ri in found {
                    f(ri)?;
                }
            }
        } else {
            let part: Vec<usize> = g.key.iter().copied().filter(|&v| l[v].is_some()).collect();
            let values: Vec<u32> = part.iter().map(|&v| l[v].unwrap_or(u32::MAX)).collect();
            for ri in g.partial_lookup(rows, &part, &values, &self.partial_held, self.partial_cap) {
                f(ri)?;
            }
        }
        ControlFlow::Continue(())
    }

    /// MINUS: whether some row agrees with `l` on every shared variable both
    /// bind *and* both bind at least one shared variable (SPARQL 1.1 §18.5
    /// Minus: compatible, with intersecting domains). A group that binds no
    /// variable `l` binds can never remove it, so it is not even looked at.
    pub(super) fn removes(&self, rows: &[Binding], l: &Binding) -> bool {
        self.groups.iter().any(|g| {
            g.key.iter().any(|&v| l[v].is_some())
                && self
                    .group_candidates(g, rows, l, &mut |_| ControlFlow::Break(()))
                    .is_break()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SplitMix64.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Random rows over `width` variables binding (some of) `vars`.
    fn rows(rng: &mut Rng, width: usize, vars: &[usize], n: usize) -> Vec<Binding> {
        (0..n)
            .map(|_| {
                let mut b = vec![None; width];
                for &v in vars {
                    if rng.below(4) != 0 {
                        b[v] = Some(rng.below(3) as u32);
                    }
                }
                b
            })
            .collect()
    }

    /// The rows agreeing with `l` on every shared variable both bind, by a
    /// scan (the definition).
    fn agreeing(right: &[Binding], shared: &[usize], l: &Binding) -> Vec<usize> {
        (0..right.len())
            .filter(|&ri| {
                shared.iter().all(|&v| match (l[v], right[ri][v]) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                })
            })
            .collect()
    }

    #[test]
    fn candidates_are_the_agreeing_rows_in_order() {
        let mut rng = Rng(17);
        let width = 6;
        for _ in 0..3000 {
            let lvars: Vec<usize> = (0..width).filter(|_| rng.below(2) == 0).collect();
            let rvars: Vec<usize> = (0..width).filter(|_| rng.below(2) == 0).collect();
            let (nl, nr) = (rng.below(10), rng.below(12));
            let left = rows(&mut rng, width, &lvars, nl);
            let right = rows(&mut rng, width, &rvars, nr);
            let shared: Vec<usize> = lvars
                .iter()
                .copied()
                .filter(|v| rvars.contains(v))
                .collect();
            let index = RowIndex::new(&right, &shared);
            for l in &left {
                let mut got = Vec::new();
                let _ = index.for_each_candidate(&right, l, |ri| {
                    got.push(ri);
                    ControlFlow::<()>::Continue(())
                });
                assert_eq!(got, agreeing(&right, &shared, l), "{l:?} {right:?}");
                // A second probe of the same part reuses its index.
                let mut again = Vec::new();
                let _ = index.for_each_candidate(&right, l, |ri| {
                    again.push(ri);
                    ControlFlow::<()>::Continue(())
                });
                assert_eq!(again, got);
                let removes = agreeing(&right, &shared, l).into_iter().any(|ri| {
                    shared
                        .iter()
                        .any(|&v| l[v].is_some() && right[ri][v].is_some())
                });
                assert_eq!(index.removes(&right, l), removes, "{l:?} {right:?}");
            }
        }
    }

    #[test]
    fn a_break_stops_the_probe() {
        let right: Vec<Binding> = (0..10).map(|i| vec![Some(i % 2), None]).collect();
        let mut extra = right.clone();
        extra.push(vec![None, Some(1)]);
        for right in [right, extra] {
            let index = RowIndex::new(&right, &[0, 1]);
            let mut seen = 0;
            let flow = index.for_each_candidate(&right, &vec![Some(0), None], |_| {
                seen += 1;
                if seen == 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            });
            assert!(flow.is_break());
            assert_eq!(seen, 2);
        }
    }

    /// Round-3 review: left solutions binding many different parts of the
    /// key build partial indexes only up to the bound (they used to keep
    /// one index of the whole group per part: |right| × parts entries);
    /// past it the group is scanned, with the same candidates.
    #[test]
    fn partial_indexes_hold_at_most_a_few_times_the_rows() {
        let width = 12;
        let key: Vec<usize> = (0..width).collect();
        // Right rows binding every key variable.
        let right: Vec<Binding> = (0..500u32)
            .map(|i| (0..width).map(|v| Some((i + v as u32) % 7)).collect())
            .collect();
        let index = RowIndex::new(&right, &key);
        let mut rng = Rng(5);
        for _ in 0..300 {
            // A left solution binding a random strict part of the key.
            let mut l: Binding = vec![None; width];
            for v in 0..width {
                if rng.below(2) == 0 {
                    l[v] = Some(rng.below(7) as u32);
                }
            }
            l[rng.below(width)] = None;
            let mut got = Vec::new();
            let _ = index.for_each_candidate(&right, &l, |ri| {
                got.push(ri);
                ControlFlow::<()>::Continue(())
            });
            assert_eq!(got, agreeing(&right, &key, &l), "{l:?}");
        }
        assert!(
            index.partial_entries() <= PARTIAL_INDEX_FACTOR * right.len(),
            "{} entries for {} rows",
            index.partial_entries(),
            right.len()
        );
    }

    /// The work of a probe is what it returns: a left solution that leaves
    /// the shared variable unbound visits every row (all agree), one that
    /// binds it visits only its matches — never the whole right side per
    /// left row (the quadratic fallback of review query#10).
    #[test]
    fn a_probe_visits_only_its_candidates() {
        let n = 1000u32;
        // ?k bound in every row but one, ?x private to the right side.
        let mut right: Vec<Binding> = (0..n).map(|i| vec![Some(i), Some(i)]).collect();
        right.push(vec![None, Some(7)]);
        let index = RowIndex::new(&right, &[0]);
        let mut visited = 0usize;
        for i in 0..n {
            let _ = index.for_each_candidate(&right, &vec![Some(i), None], |_| {
                visited += 1;
                ControlFlow::<()>::Continue(())
            });
        }
        // Each bound left row: its own match and the row without ?k.
        assert_eq!(visited, 2 * n as usize);
        let mut all = 0usize;
        let _ = index.for_each_candidate(&right, &vec![None, None], |_| {
            all += 1;
            ControlFlow::<()>::Continue(())
        });
        assert_eq!(all, right.len());
    }
}
