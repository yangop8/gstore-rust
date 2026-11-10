//! External sorting of fixed-size records under the [`Budget`].
//!
//! An [`ExtSorter`] collects records in a run buffer of its budget share.
//! When the buffer is full it is sorted, de-duplicated and spilled to a
//! scratch file as a sorted run. [`finish`](ExtSorter::finish) returns the
//! records in ascending order without duplicates: straight from the sorted
//! buffer when nothing was spilled (no file is ever created — the in-memory
//! fast path), else from a k-way merge of the runs and the last buffer, with
//! intermediate merge passes when there are more runs than the fan-in
//! (`min(64, share / 256 KiB)`, see [`Budget::fan_in`]).
//!
//! A spilled run keeps **no file descriptor**: its file is closed once
//! written and reopened by path when it is merged, so a sorter holds at most
//! fan-in + 1 descriptors (a merge's readers and its writer) however many
//! runs the input spills. The I/O buffers of writing and merging runs
//! ([`Budget::io_bytes`] each) are charged to the budget while they exist.
//!
//! A [`Spool`] is the unsorted sibling: an append-only sequence kept in
//! memory up to its share and continued in a scratch file, read back in
//! append order.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::error::Result;

use super::budget::{Budget, Charge};
use super::scratch::Scratch;

/// The largest record encoding (bytes) a sorter supports.
const MAX_RECORD_BYTES: usize = 128;

/// A fixed-size record: ordered, copyable, and encoded in [`BYTES`](Record::BYTES)
/// bytes for the scratch files.
pub(crate) trait Record: Copy + Ord + Send + 'static {
    /// Bytes of the encoding (at most 128).
    const BYTES: usize;
    /// Encode into `out[..Self::BYTES]`.
    fn put(&self, out: &mut [u8]);
    /// Decode from `b[..Self::BYTES]`.
    fn get(b: &[u8]) -> Self;
}

#[inline]
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

impl Record for [u32; 3] {
    const BYTES: usize = 12;
    #[inline]
    fn put(&self, out: &mut [u8]) {
        for (i, v) in self.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    #[inline]
    fn get(b: &[u8]) -> Self {
        [u32_at(b, 0), u32_at(b, 4), u32_at(b, 8)]
    }
}

impl Record for [u32; 4] {
    const BYTES: usize = 16;
    #[inline]
    fn put(&self, out: &mut [u8]) {
        for (i, v) in self.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
    }
    #[inline]
    fn get(b: &[u8]) -> Self {
        [u32_at(b, 0), u32_at(b, 4), u32_at(b, 8), u32_at(b, 12)]
    }
}

/// A sorted run in a scratch file. It holds no descriptor: the file is
/// closed once written and reopened by [`reader`](Run::reader).
struct Run {
    path: PathBuf,
    records: u64,
}

/// A charge of `bytes` on `budget` (an I/O buffer while it exists).
fn io_charge(budget: &Budget, bytes: usize) -> Charge<'_> {
    let mut c = Charge::new(budget);
    c.set(bytes);
    c
}

/// Write `records` (sorted, unique) through a buffer of `io` bytes to a new
/// scratch file, closed when done.
fn write_run<R: Record>(
    scratch: &Scratch,
    tag: &str,
    io: usize,
    records: impl IntoIterator<Item = Result<R>>,
) -> Result<Run> {
    let (path, file) = scratch.file(tag)?;
    let mut w = BufWriter::with_capacity(io, file);
    let mut tmp = [0u8; MAX_RECORD_BYTES];
    let mut n = 0u64;
    for r in records {
        r?.put(&mut tmp);
        w.write_all(&tmp[..R::BYTES])?;
        n += 1;
    }
    w.flush()?;
    // Closes the file (`into_inner` reports a failed final flush).
    drop(w.into_inner().map_err(|e| e.into_error())?);
    Ok(Run { path, records: n })
}

impl Run {
    fn bytes<R: Record>(&self) -> u64 {
        self.records * R::BYTES as u64
    }

    /// Reopen the run for reading through a buffer of `io` bytes.
    fn reader(self, io: usize) -> Result<RunReader> {
        let file = File::open(&self.path)?;
        Ok(RunReader {
            path: Some(self.path),
            reader: BufReader::with_capacity(io, file),
            left: self.records,
        })
    }
}

/// Reads a run back; deletes its file when dropped.
struct RunReader {
    path: Option<PathBuf>,
    reader: BufReader<File>,
    left: u64,
}

impl RunReader {
    #[inline]
    fn next<R: Record>(&mut self) -> Result<Option<R>> {
        if self.left == 0 {
            return Ok(None);
        }
        let mut tmp = [0u8; MAX_RECORD_BYTES];
        self.reader.read_exact(&mut tmp[..R::BYTES])?;
        self.left -= 1;
        Ok(Some(R::get(&tmp)))
    }
}

impl Drop for RunReader {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Sort and de-duplicate `buf` in place.
fn sort_dedup<R: Record>(buf: &mut Vec<R>) {
    buf.sort_unstable();
    buf.dedup();
}

/// An external sorter (see the module docs).
pub(crate) struct ExtSorter<'a, R: Record> {
    tag: &'static str,
    scratch: &'a Scratch,
    /// Records per run.
    cap: usize,
    fan_in: usize,
    /// Bytes of each run's read-ahead / write buffer.
    io: usize,
    buf: Vec<R>,
    charge: Charge<'a>,
    runs: Vec<Run>,
    pushed: u64,
}

impl<'a, R: Record> ExtSorter<'a, R> {
    /// A sorter whose run buffer holds `share` bytes of records.
    pub(crate) fn new(
        tag: &'static str,
        budget: &'a Budget,
        scratch: &'a Scratch,
        share: usize,
    ) -> ExtSorter<'a, R> {
        const { assert!(std::mem::size_of::<R>() > 0) };
        assert!(R::BYTES <= MAX_RECORD_BYTES);
        let cap = (share / std::mem::size_of::<R>()).max(16);
        ExtSorter {
            tag,
            scratch,
            cap,
            fan_in: Budget::fan_in(share),
            io: Budget::io_bytes(share),
            buf: Vec::new(),
            charge: Charge::new(budget),
            runs: Vec::new(),
            pushed: 0,
        }
    }

    /// Override the merge fan-in (tests: fan-in 2 forces merge passes).
    #[cfg(test)]
    pub(crate) fn with_fan_in(mut self, fan_in: usize) -> Self {
        self.fan_in = fan_in.max(2);
        self
    }

    /// Records pushed so far (duplicates included).
    #[cfg(test)]
    pub(crate) fn pushed(&self) -> u64 {
        self.pushed
    }

    /// Add a record.
    #[inline]
    pub(crate) fn push(&mut self, r: R) -> Result<()> {
        if self.buf.len() == self.buf.capacity() {
            self.make_room()?;
        }
        self.buf.push(r);
        self.pushed += 1;
        Ok(())
    }

    /// Grow the buffer up to its share, else spill it as a run.
    #[cold]
    fn make_room(&mut self) -> Result<()> {
        let have = self.buf.capacity();
        if have < self.cap {
            let want = (have * 2).max(4096).min(self.cap);
            self.buf.reserve_exact(want - self.buf.len());
            self.charge
                .set(self.buf.capacity() * std::mem::size_of::<R>());
            return Ok(());
        }
        self.spill()
    }

    fn spill(&mut self) -> Result<()> {
        sort_dedup(&mut self.buf);
        let _io = io_charge(self.charge.budget(), self.io);
        let run = write_run(
            self.scratch,
            self.tag,
            self.io,
            self.buf.iter().map(|&r| Ok(r)),
        )?;
        self.charge.budget().note_run(run.bytes::<R>());
        self.runs.push(run);
        self.buf.clear();
        Ok(())
    }

    /// Merge `runs` into one new run (their readers and the writer are
    /// charged while the pass runs).
    fn merge_pass(&self, runs: Vec<Run>) -> Result<Run> {
        let _io = io_charge(self.charge.budget(), (runs.len() + 1) * self.io);
        let readers = runs
            .into_iter()
            .map(|r| r.reader(self.io))
            .collect::<Result<Vec<_>>>()?;
        let mut merged = Merger::<R>::new(readers, Vec::new())?;
        let run = write_run(
            self.scratch,
            self.tag,
            self.io,
            std::iter::from_fn(|| merged.next().transpose()),
        )?;
        self.charge.budget().note_run(run.bytes::<R>());
        Ok(run)
    }

    /// All records in ascending order, without duplicates.
    pub(crate) fn finish(mut self) -> Result<Sorted<'a, R>> {
        sort_dedup(&mut self.buf);
        let buf = std::mem::take(&mut self.buf);
        let budget = self.charge.budget();
        let mut charge = std::mem::replace(&mut self.charge, Charge::new(budget));
        if self.runs.is_empty() {
            return Ok(Sorted {
                inner: Inner::Memory(buf.into_iter()),
                _charge: charge,
            });
        }
        // Merge passes until the runs and the buffer fit one merge.
        let mut runs = std::mem::take(&mut self.runs);
        while runs.len() + 1 > self.fan_in {
            let rest = runs.split_off(self.fan_in.min(runs.len()));
            let group = std::mem::replace(&mut runs, rest);
            let merged = self.merge_pass(group)?;
            runs.push(merged);
        }
        // The final merge's read-ahead is charged with the buffer it merges.
        charge.set(buf.capacity() * std::mem::size_of::<R>() + runs.len() * self.io);
        let readers = runs
            .into_iter()
            .map(|r| r.reader(self.io))
            .collect::<Result<Vec<_>>>()?;
        Ok(Sorted {
            inner: Inner::Merge(Box::new(Merger::new(readers, buf)?)),
            _charge: charge,
        })
    }
}

/// A k-way merge of runs (and one in-memory sorted buffer), de-duplicated.
struct Merger<R: Record> {
    readers: Vec<RunReader>,
    mem: std::vec::IntoIter<R>,
    /// `(record, source)`; source `readers.len()` is the memory buffer.
    heap: BinaryHeap<Reverse<(R, usize)>>,
    last: Option<R>,
}

impl<R: Record> Merger<R> {
    fn new(mut readers: Vec<RunReader>, mem: Vec<R>) -> Result<Merger<R>> {
        let mut heap = BinaryHeap::with_capacity(readers.len() + 1);
        for (i, r) in readers.iter_mut().enumerate() {
            if let Some(rec) = r.next::<R>()? {
                heap.push(Reverse((rec, i)));
            }
        }
        let mut mem = mem.into_iter();
        if let Some(rec) = mem.next() {
            heap.push(Reverse((rec, readers.len())));
        }
        Ok(Merger {
            readers,
            mem,
            heap,
            last: None,
        })
    }

    #[inline]
    fn next(&mut self) -> Result<Option<R>> {
        loop {
            let Some(Reverse((rec, src))) = self.heap.pop() else {
                return Ok(None);
            };
            let refill = if src == self.readers.len() {
                self.mem.next()
            } else {
                self.readers[src].next::<R>()?
            };
            if let Some(r) = refill {
                self.heap.push(Reverse((r, src)));
            }
            if self.last != Some(rec) {
                self.last = Some(rec);
                return Ok(Some(rec));
            }
        }
    }
}

enum Inner<R: Record> {
    Memory(std::vec::IntoIter<R>),
    Merge(Box<Merger<R>>),
}

/// The sorted, de-duplicated output of an [`ExtSorter`].
pub(crate) struct Sorted<'a, R: Record> {
    inner: Inner<R>,
    _charge: Charge<'a>,
}

impl<R: Record> Sorted<'_, R> {
    /// The next record in ascending order (`None` at the end).
    #[inline]
    pub(crate) fn next(&mut self) -> Result<Option<R>> {
        match &mut self.inner {
            Inner::Memory(it) => Ok(it.next()),
            Inner::Merge(m) => m.next(),
        }
    }
}

/// An append-only spool (see the module docs).
pub(crate) struct Spool<'a, R: Record> {
    tag: &'static str,
    scratch: &'a Scratch,
    cap: usize,
    /// Bytes of the file's write (then read-ahead) buffer.
    io: usize,
    buf: Vec<R>,
    charge: Charge<'a>,
    file: Option<(PathBuf, BufWriter<File>)>,
    in_file: u64,
}

impl<'a, R: Record> Spool<'a, R> {
    pub(crate) fn new(
        tag: &'static str,
        budget: &'a Budget,
        scratch: &'a Scratch,
        share: usize,
    ) -> Spool<'a, R> {
        assert!(R::BYTES <= MAX_RECORD_BYTES);
        Spool {
            tag,
            scratch,
            cap: (share / std::mem::size_of::<R>()).max(16),
            io: Budget::io_bytes(share),
            buf: Vec::new(),
            charge: Charge::new(budget),
            file: None,
            in_file: 0,
        }
    }

    /// Append a record.
    #[inline]
    pub(crate) fn push(&mut self, r: R) -> Result<()> {
        if self.buf.len() == self.buf.capacity() {
            self.make_room()?;
        }
        self.buf.push(r);
        Ok(())
    }

    #[cold]
    fn make_room(&mut self) -> Result<()> {
        let have = self.buf.capacity();
        if have < self.cap {
            let want = (have * 2).max(1024).min(self.cap);
            self.buf.reserve_exact(want - self.buf.len());
            self.charge
                .set(self.buf.capacity() * std::mem::size_of::<R>());
            return Ok(());
        }
        if self.file.is_none() {
            let (path, file) = self.scratch.file(self.tag)?;
            self.file = Some((path, BufWriter::with_capacity(self.io, file)));
            // The writer's buffer is charged with the spool's own.
            self.charge
                .set(self.buf.capacity() * std::mem::size_of::<R>() + self.io);
        }
        let (_, w) = self.file.as_mut().unwrap();
        let mut tmp = [0u8; MAX_RECORD_BYTES];
        for r in &self.buf {
            r.put(&mut tmp);
            w.write_all(&tmp[..R::BYTES])?;
        }
        let bytes = (self.buf.len() * R::BYTES) as u64;
        self.charge.budget().note_spool(bytes);
        self.in_file += self.buf.len() as u64;
        self.buf.clear();
        Ok(())
    }

    /// Read the records back in append order.
    pub(crate) fn finish(mut self) -> Result<SpoolReader<'a, R>> {
        let file = match self.file.take() {
            None => None,
            Some((path, w)) => {
                let mut file = w.into_inner().map_err(|e| e.into_error())?;
                file.seek(SeekFrom::Start(0))?;
                Some(RunReader {
                    path: Some(path),
                    reader: BufReader::with_capacity(self.io, file),
                    left: self.in_file,
                })
            }
        };
        let budget = self.charge.budget();
        let mut charge = std::mem::replace(&mut self.charge, Charge::new(budget));
        let read_ahead = if file.is_some() { self.io } else { 0 };
        charge.set(self.buf.capacity() * std::mem::size_of::<R>() + read_ahead);
        Ok(SpoolReader {
            file,
            mem: std::mem::take(&mut self.buf).into_iter(),
            _charge: charge,
        })
    }
}

/// Reads a [`Spool`] back.
pub(crate) struct SpoolReader<'a, R: Record> {
    file: Option<RunReader>,
    mem: std::vec::IntoIter<R>,
    _charge: Charge<'a>,
}

impl<R: Record> SpoolReader<'_, R> {
    #[inline]
    pub(crate) fn next(&mut self) -> Result<Option<R>> {
        if let Some(f) = &mut self.file {
            if let Some(r) = f.next::<R>()? {
                return Ok(Some(r));
            }
            self.file = None;
        }
        Ok(self.mem.next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> u32 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*seed >> 33) as u32
    }

    fn scratch(tag: &str) -> (PathBuf, Scratch) {
        let p = std::env::temp_dir().join(format!("gstore_extsort_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        let s = Scratch::new(&p);
        (p, s)
    }

    fn drain<R: Record>(mut s: Sorted<'_, R>) -> Vec<R> {
        let mut out = Vec::new();
        while let Some(r) = s.next().unwrap() {
            out.push(r);
        }
        out
    }

    /// The sorter equals `sort + dedup` across budgets (in memory, a few
    /// runs, many runs with fan-in 2 forcing merge passes).
    #[test]
    fn matches_std_sort_across_budgets_and_fan_in() {
        let (dir, sc) = scratch("budgets");
        let mut seed = 7u64;
        let input: Vec<[u32; 3]> = (0..50_000)
            .map(|_| {
                [
                    lcg(&mut seed) % 300,
                    lcg(&mut seed) % 20,
                    lcg(&mut seed) % 300,
                ]
            })
            .collect();
        let mut want = input.clone();
        want.sort_unstable();
        want.dedup();
        for (share, fan_in) in [(64 << 20, 64), (64 << 10, 64), (4 << 10, 2), (4 << 10, 3)] {
            let budget = Budget::new(1 << 30);
            let mut s = ExtSorter::<[u32; 3]>::new("t", &budget, &sc, share).with_fan_in(fan_in);
            for r in &input {
                s.push(*r).unwrap();
            }
            assert_eq!(s.pushed(), input.len() as u64);
            let spilled = budget.spilled_runs();
            let out = drain(s.finish().unwrap());
            assert_eq!(out, want, "share {share} fan-in {fan_in}");
            if share >= 64 << 20 {
                assert_eq!(spilled, 0, "the in-memory fast path writes no run");
                assert!(sc.dir().is_none(), "no scratch directory either");
            } else {
                assert!(budget.spilled_runs() > 1, "share {share} must spill");
            }
            // The run buffer, plus the charged I/O buffers of a merge (its
            // readers and its writer).
            let io = (fan_in + 1) * Budget::io_bytes(share);
            assert!(
                budget.peak() <= share + 4096 * 12 + io,
                "peak {} share {share}",
                budget.peak()
            );
        }
        // Every run file is gone once read (the directory's LOCK stays).
        if let Some(d) = sc.dir() {
            let runs = std::fs::read_dir(d)
                .unwrap()
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|x| x == "run")
                })
                .count();
            assert_eq!(runs, 0);
        }
        drop(sc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A spilled run holds no file descriptor until it is merged: however
    /// many runs a sorter spills, its open descriptors stay bounded by the
    /// merge fan-in, not by the input (an fd per run exhausted
    /// `RLIMIT_NOFILE` on a large build with a small budget).
    #[cfg(target_os = "linux")]
    #[test]
    fn spilled_runs_hold_no_descriptor() {
        let open_fds = || std::fs::read_dir("/proc/self/fd").unwrap().count();
        let (dir, sc) = scratch("fds");
        let budget = Budget::new(1 << 30);
        let mut s = ExtSorter::<[u32; 3]>::new("f", &budget, &sc, 4 << 10).with_fan_in(4);
        let before = open_fds();
        let mut seed = 3u64;
        let mut want = Vec::new();
        for _ in 0..200_000 {
            let r = [lcg(&mut seed), 1, 2];
            s.push(r).unwrap();
            want.push(r);
        }
        let runs = budget.spilled_runs();
        assert!(runs >= 300, "{runs} runs");
        let during = open_fds();
        // (Other tests of this binary open files concurrently: a margin.)
        assert!(
            during < before + 200,
            "{runs} spilled runs hold {} descriptors",
            during.saturating_sub(before)
        );
        want.sort_unstable();
        want.dedup();
        assert_eq!(drain(s.finish().unwrap()), want);
        drop(sc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The I/O buffers of spilling, merging and reading back are charged to
    /// the budget while they exist, and a merge's readers plus its writer fit
    /// one more share (so a 1 MiB budget's buffers stay near it).
    #[test]
    fn io_buffers_are_charged_while_they_exist() {
        let (dir, sc) = scratch("io");
        let budget = Budget::new(1 << 20);
        let share = budget.share(3);
        let io = Budget::io_bytes(share);
        assert!((Budget::fan_in(share) + 1) * io <= share);
        let mut s = ExtSorter::<[u32; 3]>::new("io", &budget, &sc, share);
        let mut seed = 11u64;
        for _ in 0..100_000 {
            s.push([lcg(&mut seed), 0, 0]).unwrap();
        }
        assert!(budget.spilled_runs() > 4);
        let buf_bytes = s.buf.capacity() * std::mem::size_of::<[u32; 3]>();
        assert!(
            budget.peak() >= buf_bytes + io,
            "a spill's writer is charged"
        );
        let sorted = s.finish().unwrap();
        // Fan-in 2: one run is left besides the buffer for the final merge.
        assert_eq!(budget.used(), buf_bytes + io);
        assert!(
            budget.peak() >= buf_bytes + 3 * io,
            "a merge pass's two readers and writer are charged"
        );
        assert!(budget.peak() <= 2 * share, "peak {}", budget.peak());
        drop(sorted);
        assert_eq!(budget.used(), 0);

        let mut sp = Spool::<[u32; 3]>::new("sp", &budget, &sc, share);
        for i in 0..100_000u32 {
            sp.push([i, 0, 0]).unwrap();
        }
        let spool_bytes = sp.buf.capacity() * std::mem::size_of::<[u32; 3]>();
        assert_eq!(budget.used(), spool_bytes + io, "the spool's writer");
        let mut r = sp.finish().unwrap();
        assert_eq!(budget.used(), spool_bytes + io, "the spool's read-ahead");
        let mut n = 0;
        while r.next().unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 100_000);
        drop(r);
        assert_eq!(budget.used(), 0);
        drop(sc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quads_empty_and_threads() {
        let (dir, sc) = scratch("quads");
        let budget = Budget::new(1 << 20);
        let s = ExtSorter::<[u32; 4]>::new("q", &budget, &sc, 1 << 20);
        assert!(drain(s.finish().unwrap()).is_empty());
        // Sorters on several threads share one budget and scratch directory.
        std::thread::scope(|scope| {
            for t in 0..4u32 {
                let (budget, sc) = (&budget, &sc);
                scope.spawn(move || {
                    let mut s = ExtSorter::<[u32; 4]>::new("q", budget, sc, 8 << 10);
                    for i in (0..10_000u32).rev() {
                        s.push([t, i % 977, i, 1]).unwrap();
                        s.push([t, i % 977, i, 1]).unwrap();
                    }
                    let out = drain(s.finish().unwrap());
                    let mut want: Vec<[u32; 4]> =
                        (0..10_000u32).map(|i| [t, i % 977, i, 1]).collect();
                    want.sort_unstable();
                    assert_eq!(out, want);
                });
            }
        });
        drop(sc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spool_keeps_append_order() {
        let (dir, sc) = scratch("spool");
        let budget = Budget::new(1 << 20);
        for share in [1 << 20, 2 << 10] {
            let mut sp = Spool::<[u32; 3]>::new("s", &budget, &sc, share);
            for i in 0..10_000u32 {
                sp.push([10_000 - i, i, 7]).unwrap();
            }
            let mut r = sp.finish().unwrap();
            for i in 0..10_000u32 {
                assert_eq!(r.next().unwrap(), Some([10_000 - i, i, 7]));
            }
            assert_eq!(r.next().unwrap(), None);
        }
        assert!(budget.spilled_bytes() > 0);
        drop(sc);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
