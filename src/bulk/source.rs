//! Parsing the input: per source on up to `threads` parser threads, with
//! ordered replay.
//!
//! Each source is parsed by the ordinary streaming import
//! ([`import::import_source`]) on a worker thread. Everything it would hand
//! the error log and the quad sink — failed statements and batches, in the
//! order the import produces them — travels through the source's bounded
//! channel. The calling thread replays the sources strictly in order:
//! `log.begin` for the source, then its events in order, then the next
//! source. So the encoder sees the quads in exactly the sequential order, and
//! the [`ImportReport`], the error-log lines and the first error of a strict
//! import are those of a sequential [`import::import_sources`]. With one
//! thread (or a single in-memory document) the import simply runs on the
//! calling thread. The parser of a lone source still runs beside the encoder,
//! so parsing and encoding overlap.
//!
//! The parser threads batch with the caller's `batch_size` / `batch_bytes`,
//! exactly as the sequential import, but hand each batch over in parts of at
//! most [`PIPE_BATCH`] quads / [`PIPE_BATCH_BYTES`] of terms as they are
//! parsed ([`QuadSink::handoff`]), so the channel stays small and the encoder
//! keeps up with the parser. The end of each batch travels as its own event.
//! The encoder (the sink) can refuse a quad (an over-long key, an id-space
//! bound); such an error is held until the end of its batch, where the
//! sequential import would have raised it — after the same error-log lines,
//! and behind a strict parse error that comes first.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender};
use std::sync::Mutex;

use crate::error::{GStoreError, Result};
use crate::parser::import::{self, ErrorLog, ImportOptions, ImportReport, QuadSink, Source};
use crate::parser::nquads::Quad;
use crate::parser::stream::SyntaxError;

use super::BulkInput;

/// The operation named in the error log (as the incremental build).
pub(crate) const OPERATION: &str = "build";

/// Quads per part of a batch on the parser → encoder channel (smaller than
/// a sequential import's batches, so the pipeline overlaps sooner and holds
/// less).
const PIPE_BATCH: usize = 8192;
/// Bytes of RDF terms per part on the channel.
const PIPE_BATCH_BYTES: usize = 8 << 20;
/// Messages a source's channel holds before its parser waits.
const PIPE_DEPTH: usize = 4;

/// What a parser thread produces, in import order.
enum Event {
    Failed(SyntaxError),
    /// A part of a batch.
    Batch(Vec<Quad>),
    /// The end of a (non-empty) batch.
    BatchEnd,
    Done(Result<ImportReport>),
}

/// Forwards failures to the channel (`begin` is replayed by the consumer).
struct FwdLog<'a> {
    tx: &'a SyncSender<Event>,
}

impl ErrorLog for FwdLog<'_> {
    fn failed(&mut self, _source: &str, err: &SyntaxError) {
        let _ = self.tx.send(Event::Failed(err.clone()));
    }
}

/// Forwards the parts of each batch, and its end, to the channel. The
/// consumer sends each part back once it is encoded, and the quads are
/// dropped here, on the thread that allocated them (freeing another
/// thread's allocations is markedly slower), and the part's buffer is
/// reused.
struct FwdSink<'a> {
    tx: &'a SyncSender<Event>,
    back: &'a Receiver<Vec<Quad>>,
}

impl FwdSink<'_> {
    fn send(&self, e: Event) -> Result<()> {
        self.tx
            .send(e)
            .map_err(|_| GStoreError::Database("the bulk build stopped reading the input".into()))
    }
}

impl QuadSink for FwdSink<'_> {
    fn quads(&mut self, batch: &mut Vec<Quad>) -> Result<()> {
        let mut next = self
            .back
            .try_recv()
            .unwrap_or_else(|_| Vec::with_capacity(PIPE_BATCH));
        next.clear();
        let part = std::mem::replace(batch, next);
        self.send(Event::Batch(part))
    }

    fn handoff(&self) -> Option<(usize, usize)> {
        Some((PIPE_BATCH, PIPE_BATCH_BYTES))
    }

    fn batch_end(&mut self) -> Result<()> {
        self.send(Event::BatchEnd)
    }
}

/// Parse `input` into `sink` (see the module docs).
pub(crate) fn import(
    input: &BulkInput<'_>,
    opts: &ImportOptions,
    log: &mut dyn ErrorLog,
    threads: usize,
    sink: &mut dyn QuadSink,
) -> Result<ImportReport> {
    match input {
        BulkInput::Text { text, format, name } => {
            import::import_reader(text.as_bytes(), *format, name, opts, log, sink)
        }
        BulkInput::Sources(sources) if threads <= 1 || sources.is_empty() => {
            import::import_sources(sources, OPERATION, opts, log, sink)
        }
        BulkInput::Sources(sources) => parallel(sources, opts, log, threads, sink),
    }
}

fn parallel(
    sources: &[Source],
    opts: &ImportOptions,
    log: &mut dyn ErrorLog,
    threads: usize,
    sink: &mut dyn QuadSink,
) -> Result<ImportReport> {
    // The sequential batching, unchanged (see the module docs).
    let popts = opts;
    let n = sources.len();
    let mut rxs: Vec<Option<Receiver<Event>>> = Vec::with_capacity(n);
    let mut backs: Vec<Sender<Vec<Quad>>> = Vec::with_capacity(n);
    type WorkerEnds = (SyncSender<Event>, Receiver<Vec<Quad>>);
    let mut txs: Vec<Mutex<Option<WorkerEnds>>> = Vec::with_capacity(n);
    for _ in 0..n {
        let (tx, rx) = sync_channel(PIPE_DEPTH);
        let (back_tx, back_rx) = channel();
        txs.push(Mutex::new(Some((tx, back_rx))));
        rxs.push(Some(rx));
        backs.push(back_tx);
    }
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let workers = threads.min(n).max(1);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let (txs, next, stop) = (&txs, &next, &stop);
            scope.spawn(move || loop {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    return;
                }
                let Some((tx, back)) = txs[i].lock().unwrap().take() else {
                    continue;
                };
                let mut flog = FwdLog { tx: &tx };
                let mut fsink = FwdSink {
                    tx: &tx,
                    back: &back,
                };
                let r = import::import_source(&sources[i], OPERATION, popts, &mut flog, &mut fsink);
                let _ = tx.send(Event::Done(r));
            });
        }
        let result = replay(sources, &mut rxs, &backs, log, sink);
        if result.is_err() {
            stop.store(true, Ordering::Relaxed);
        }
        // Dropping the receivers releases every parser blocked on a send.
        rxs.clear();
        result
    })
}

/// Consume the sources' events in source order.
fn replay(
    sources: &[Source],
    rxs: &mut [Option<Receiver<Event>>],
    backs: &[Sender<Vec<Quad>>],
    log: &mut dyn ErrorLog,
    sink: &mut dyn QuadSink,
) -> Result<ImportReport> {
    let mut total = ImportReport::default();
    for (i, source) in sources.iter().enumerate() {
        let name = source.name();
        log.begin(OPERATION, &name);
        let rx = rxs[i].take().expect("each source is replayed once");
        // The sink's error, held until the end of its batch (see the module
        // docs); nothing more is encoded after it.
        let mut held: Option<GStoreError> = None;
        loop {
            match rx.recv() {
                Ok(Event::Failed(e)) => log.failed(&name, &e),
                Ok(Event::Batch(mut b)) => {
                    if held.is_none() {
                        held = sink.quads(&mut b).err();
                    }
                    // Back to the parser, which drops what is left in it.
                    let _ = backs[i].send(b);
                }
                Ok(Event::BatchEnd) => {
                    if let Some(e) = held.take() {
                        return Err(e);
                    }
                }
                Ok(Event::Done(r)) => {
                    // A failed import (a strict parse error) ends before its
                    // batch does: its error comes first, as sequentially.
                    let r = r?;
                    if let Some(e) = held.take() {
                        return Err(e);
                    }
                    total.absorb(&r);
                    break;
                }
                Err(_) => {
                    return Err(GStoreError::Database(format!(
                        "the parser of '{name}' stopped without a result"
                    )))
                }
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parallel parsing replays exactly the sequential import: the same quads
    /// in the same order, the same log calls, the same report and the same
    /// first strict error.
    #[test]
    fn parallel_replay_equals_sequential_import() {
        let dir = std::env::temp_dir().join(format!("gstore_bulk_source_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut sources = Vec::new();
        for f in 0..6 {
            let mut text = String::new();
            for i in 0..3000 {
                if i % 997 == 13 {
                    text.push_str("this is not rdf\n");
                }
                text.push_str(&format!(
                    "<http://e/s{f}_{i}> <http://e/p{}> \"v{i}\" .\n",
                    i % 3
                ));
            }
            let p = dir.join(format!("f{f}.nt"));
            std::fs::write(&p, text).unwrap();
            sources.push(Source::File(p));
        }
        sources.push(Source::File(dir.join("missing.nt")));
        let run = |threads: usize, strict: bool, srcs: &[Source]| {
            let opts = if strict {
                ImportOptions::strict()
            } else {
                ImportOptions::tolerant()
            };
            let mut log: Vec<(String, SyntaxError)> = Vec::new();
            let mut quads = Vec::new();
            let r = import(
                &BulkInput::Sources(srcs),
                &opts,
                &mut log,
                threads,
                &mut |b: &mut Vec<Quad>| {
                    quads.append(b);
                    Ok(())
                },
            );
            let log: Vec<String> = log.iter().map(|(s, e)| format!("{s}: {e:?}")).collect();
            (r.map_err(|e| e.to_string()), log, quads)
        };
        for strict in [false, true] {
            for srcs in [&sources[..6], &sources[..]] {
                let want = run(1, strict, srcs);
                for threads in [2, 3, 8] {
                    let got = run(threads, strict, srcs);
                    assert_eq!(got.0, want.0, "threads {threads} strict {strict}");
                    assert_eq!(got.1, want.1);
                    if !strict && got.0.is_ok() {
                        assert_eq!(got.2, want.2);
                    }
                }
            }
        }
        // A sink error surfaces where the sequential import raises it: at the
        // sequential batch boundary, after the same error-log lines (and a
        // strict parse error before that boundary wins).
        let mut text = format!("<http://e/bad{}> <http://e/p> \"v\" .\n", "x".repeat(50));
        for i in 0..10_000 {
            text.push_str(&format!("<http://e/s{i}> <http://e/p> \"v{i}\" .\n"));
        }
        text.push_str("this is not rdf\n");
        text.push_str("<http://e/after> <http://e/p> \"v\" .\n");
        let p = dir.join("sinkerr.nt");
        std::fs::write(&p, text).unwrap();
        let one = [Source::File(p)];
        let failing = |threads: usize, strict: bool| {
            let opts = if strict {
                ImportOptions::strict()
            } else {
                ImportOptions::tolerant()
            };
            let mut log: Vec<(String, SyntaxError)> = Vec::new();
            let r = import(
                &BulkInput::Sources(&one),
                &opts,
                &mut log,
                threads,
                &mut |b: &mut Vec<Quad>| {
                    if b.iter().any(|q| q.subject.to_string().contains("/bad")) {
                        Err(GStoreError::Database("key too long".into()))
                    } else {
                        Ok(())
                    }
                },
            );
            let log: Vec<String> = log.iter().map(|(s, e)| format!("{s}: {e:?}")).collect();
            (r.map_err(|e| e.to_string()), log)
        };
        for strict in [false, true] {
            let want = failing(1, strict);
            assert!(want.0.is_err());
            if strict {
                assert!(
                    want.0.as_ref().unwrap_err().contains("line 10002"),
                    "{want:?}"
                );
            } else {
                assert!(want.0.as_ref().unwrap_err().contains("key too long"));
                assert_eq!(want.1.len(), 1, "the bad line is logged first: {want:?}");
            }
            for threads in [2, 4] {
                assert_eq!(
                    failing(threads, strict),
                    want,
                    "threads {threads} strict {strict}"
                );
            }
        }

        // A sink error stops everything and is returned as is.
        let e = import(
            &BulkInput::Sources(&sources[..6]),
            &ImportOptions::tolerant(),
            &mut (),
            4,
            &mut |_: &mut Vec<Quad>| Err(GStoreError::Database("sink full".into())),
        )
        .unwrap_err();
        assert!(e.to_string().contains("sink full"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
