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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Mutex;

use crate::error::{GStoreError, Result};
use crate::parser::import::{self, ErrorLog, ImportOptions, ImportReport, QuadSink, Source};
use crate::parser::nquads::Quad;
use crate::parser::stream::SyntaxError;

use super::BulkInput;

/// The operation named in the error log (as the incremental build).
pub(crate) const OPERATION: &str = "build";

/// Quads per batch on the parser → encoder channel (smaller batches than a
/// sequential import uses, so the pipeline overlaps sooner and holds less).
const PIPE_BATCH: usize = 8192;
/// Bytes of RDF terms per batch on the channel.
const PIPE_BATCH_BYTES: usize = 8 << 20;
/// Messages a source's channel holds before its parser waits.
const PIPE_DEPTH: usize = 4;

/// What a parser thread produces, in import order.
enum Event {
    Failed(SyntaxError),
    Batch(Vec<Quad>),
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

/// Forwards batches to the channel.
struct FwdSink<'a> {
    tx: &'a SyncSender<Event>,
    cap: usize,
}

impl QuadSink for FwdSink<'_> {
    fn quads(&mut self, batch: &mut Vec<Quad>) -> Result<()> {
        let full = std::mem::replace(batch, Vec::with_capacity(self.cap));
        self.tx
            .send(Event::Batch(full))
            .map_err(|_| GStoreError::Database("the bulk build stopped reading the input".into()))
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
    let mut popts = opts.clone();
    popts.batch_size = popts.batch_size.clamp(1, PIPE_BATCH);
    popts.batch_bytes = popts.batch_bytes.clamp(1, PIPE_BATCH_BYTES);
    let popts = &popts;
    let n = sources.len();
    let mut rxs: Vec<Option<Receiver<Event>>> = Vec::with_capacity(n);
    let mut txs: Vec<Mutex<Option<SyncSender<Event>>>> = Vec::with_capacity(n);
    for _ in 0..n {
        let (tx, rx) = sync_channel(PIPE_DEPTH);
        txs.push(Mutex::new(Some(tx)));
        rxs.push(Some(rx));
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
                let Some(tx) = txs[i].lock().unwrap().take() else {
                    continue;
                };
                let mut flog = FwdLog { tx: &tx };
                let mut fsink = FwdSink {
                    tx: &tx,
                    cap: popts.batch_size,
                };
                let r = import::import_source(&sources[i], OPERATION, popts, &mut flog, &mut fsink);
                let _ = tx.send(Event::Done(r));
            });
        }
        let result = replay(sources, &mut rxs, log, sink);
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
    log: &mut dyn ErrorLog,
    sink: &mut dyn QuadSink,
) -> Result<ImportReport> {
    let mut total = ImportReport::default();
    for (i, source) in sources.iter().enumerate() {
        let name = source.name();
        log.begin(OPERATION, &name);
        let rx = rxs[i].take().expect("each source is replayed once");
        loop {
            match rx.recv() {
                Ok(Event::Failed(e)) => log.failed(&name, &e),
                Ok(Event::Batch(mut b)) => sink.quads(&mut b)?,
                Ok(Event::Done(r)) => {
                    total.absorb(&r?);
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
