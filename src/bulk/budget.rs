//! The memory budget of a bulk build: per-phase shares and peak accounting.
//!
//! The budget covers the buffers whose size the build chooses — external-sort
//! run buffers, the signature spool, merge read-ahead. It does **not** cover
//! the term map (the dictionary under construction, about `L + 26` bytes per
//! term, reported separately in the [`BulkReport`](super::BulkReport)) nor
//! the parse pipeline (a few bounded batches per parser thread).
//!
//! A buffer charges its capacity when it grows and releases it when it is
//! dropped or spilled; the peak of the charged bytes is reported.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// The smallest share any buffer gets, whatever the budget (keeps a tiny
/// budget usable: every sorter can hold at least this many bytes).
pub(crate) const MIN_SHARE: usize = 64 << 10;

/// Bytes of read-ahead per run of a k-way merge.
pub(crate) const MERGE_READ_BYTES: usize = 256 << 10;

/// The memory budget (see the module docs).
#[derive(Debug)]
pub(crate) struct Budget {
    total: usize,
    used: AtomicUsize,
    peak: AtomicUsize,
    /// Sorted runs written to scratch files.
    spilled_runs: AtomicU64,
    /// Bytes written to scratch files (runs and spools).
    spilled_bytes: AtomicU64,
}

impl Budget {
    pub(crate) fn new(total: usize) -> Budget {
        Budget {
            total: total.max(MIN_SHARE),
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            spilled_runs: AtomicU64::new(0),
            spilled_bytes: AtomicU64::new(0),
        }
    }

    /// The whole budget in bytes.
    pub(crate) fn total(&self) -> usize {
        self.total
    }

    /// A buffer's share when `live` budgeted buffers are in use at once in a
    /// phase: a quarter of the budget split between them (the rest covers
    /// merge read-ahead, the parse pipeline and allocator slack), at least
    /// [`MIN_SHARE`].
    pub(crate) fn share(&self, live: usize) -> usize {
        (self.total / (4 * live.max(1))).max(MIN_SHARE)
    }

    /// The k-way merge fan-in for a buffer of `share` bytes:
    /// `min(64, share / 256 KiB)`, at least 2.
    pub(crate) fn fan_in(share: usize) -> usize {
        (share / MERGE_READ_BYTES).clamp(2, 64)
    }

    /// Account `bytes` more in use.
    pub(crate) fn charge(&self, bytes: usize) {
        let now = self.used.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.peak.fetch_max(now, Ordering::Relaxed);
    }

    /// Account `bytes` fewer in use.
    pub(crate) fn release(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// Bytes in use now.
    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    /// The most bytes ever in use at once.
    pub(crate) fn peak(&self) -> usize {
        self.peak.load(Ordering::Relaxed)
    }

    /// Record one spilled run of `bytes`.
    pub(crate) fn note_run(&self, bytes: u64) {
        self.spilled_runs.fetch_add(1, Ordering::Relaxed);
        self.spilled_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record `bytes` written to a spool file (not a sorted run).
    pub(crate) fn note_spool(&self, bytes: u64) {
        self.spilled_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Sorted runs spilled so far.
    pub(crate) fn spilled_runs(&self) -> u64 {
        self.spilled_runs.load(Ordering::Relaxed)
    }

    /// Bytes written to scratch files so far.
    pub(crate) fn spilled_bytes(&self) -> u64 {
        self.spilled_bytes.load(Ordering::Relaxed)
    }
}

/// A charge on a [`Budget`] that follows a buffer's capacity and is released
/// when dropped.
#[derive(Debug)]
pub(crate) struct Charge<'b> {
    budget: &'b Budget,
    bytes: usize,
}

impl<'b> Charge<'b> {
    pub(crate) fn new(budget: &'b Budget) -> Charge<'b> {
        Charge { budget, bytes: 0 }
    }

    /// Set the charged amount to `bytes`.
    pub(crate) fn set(&mut self, bytes: usize) {
        if bytes > self.bytes {
            self.budget.charge(bytes - self.bytes);
        } else {
            self.budget.release(self.bytes - bytes);
        }
        self.bytes = bytes;
    }

    /// The budget charged.
    pub(crate) fn budget(&self) -> &'b Budget {
        self.budget
    }
}

impl Drop for Charge<'_> {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_peaks_and_fan_in() {
        let b = Budget::new(128 << 20);
        assert_eq!(b.share(1), 32 << 20);
        assert_eq!(b.share(4), 8 << 20);
        assert_eq!(Budget::new(1).share(8), MIN_SHARE);
        assert_eq!(Budget::fan_in(1 << 20), 4);
        assert_eq!(Budget::fan_in(1 << 40), 64);
        assert_eq!(Budget::fan_in(0), 2);
        {
            let mut c = Charge::new(&b);
            c.set(100);
            c.set(300);
            let mut d = Charge::new(&b);
            d.set(50);
            assert_eq!(b.used(), 350);
            c.set(10);
            assert_eq!(b.used(), 60);
        }
        assert_eq!(b.used(), 0);
        assert_eq!(b.peak(), 350);
    }
}
