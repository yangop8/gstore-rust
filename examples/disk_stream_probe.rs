//! `disk_stream_probe` — the early Stream-mode signal for R3 (Phase 6,
//! package R2-DISKLIB): LUBM Q1–Q14 evaluated straight over the on-disk
//! indexes (`DiskTriples`) against the same queries over the materialized
//! in-memory `TripleStore`, plus the cost of reading through a transaction
//! overlay.
//!
//! ```text
//! cargo run --release --example disk_stream_probe -- <db_dir> [options]
//!
//!   <db_dir>             a database built by `gbuild --disk` (holds kvstore.kv)
//!   --mode M[,M...]      query | overlay | all                      (default all)
//!   --qreps N            warm timed runs per query and side          (default 5)
//!   --reps N             timed repetitions per lookup measurement    (default 7)
//!   --lookups N          random lookups per method and repetition    (default 20000)
//!   --changes N          triples the "other keys" overlay adds and
//!                        removes (each)                              (default 5000)
//!   --seed S             RNG seed of the samples                     (default 42)
//!   --queries DIR        directory holding lubm_q1.rq .. lubm_q14.rq
//!                        (default <crate>/testdata/lubm)
//!   --cache-pages N      page cache of the store (default: the whole
//!                        kvstore.kv, i.e. warm)
//!   --no-vstree          evaluate without vstree.kv (default: use it on
//!                        both sides when it is fresh)
//! ```
//!
//! Both sides share one in-memory dictionary (`DiskStore::dictionary()`), so
//! the store is the only difference; R2-DICT measures the out-of-core
//! dictionary. The directory is only read. Every line on stdout is
//! `key=value` (times in ms unless the key ends in `_ns`; "warm" = the median
//! of the timed runs after one untimed run).
//!
//! * `query`: per query `q{i}_mem_ms` (Evaluator over the materialized
//!   `TripleStore`), `q{i}_disk_ms` (Evaluator over `DiskTriples`),
//!   `q{i}_view_ms` (a fresh `DiskReadView::with_base` per run, Evaluator over
//!   a `DiskSource` with the view's cancel flag — the snapshot path), the
//!   ratio disk/mem, the row count and `q{i}_match` (rows **in order** equal
//!   on all three sides). Then the geometric means and
//!   `query_disk_over_mem` (acceptance: ≤ 1.5).
//! * `overlay`: hot random `po_by_s`, `o_by_sp`, `ps_by_o`, `s_by_po`,
//!   `count_po_by_s` and `exists` on keys an overlay does not touch, through
//!   (a) the store directly (`DiskSource`), (b) an empty `OverlayView`,
//!   (c) an `OverlayView` whose overlay adds and removes `--changes` triples
//!   under *other* keys, (d) `DiskTriples` without a transaction and (e)
//!   `DiskTriples` with an open transaction holding the changes of (c). The
//!   sources are timed round-robin; `overlay_*_ratio` is each against (a)
//!   (acceptance: ≤ 1.1 for (b) and (c)). The answers (sums of list lengths)
//!   must be equal on every source (`overlay_answers_match`). Finally Q1–Q14
//!   over (e), whose overlay touches the predicates the queries scan:
//!   `query_txn_geomean_ms` against `query_plain_geomean_ms`.

use std::collections::HashSet;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use gstore::kvstore::{DiskSource, DiskStore, DiskTriples};
use gstore::model::IdTriple;
use gstore::parser::sparql;
use gstore::query::{Evaluator, QueryResult};
use gstore::signature::VsTree;
use gstore::store::read_scope::ReadScope;
use gstore::store::{Overlay, OverlayView, TripleSource, TripleStore};

struct Opts {
    dir: PathBuf,
    modes: Vec<String>,
    qreps: usize,
    reps: usize,
    lookups: usize,
    changes: usize,
    seed: u64,
    queries: PathBuf,
    cache_pages: Option<usize>,
    vstree: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: disk_stream_probe <db_dir> [--mode query,overlay|all] [--qreps N] [--reps N] \
         [--lookups N] [--changes N] [--seed S] [--queries DIR] [--cache-pages N] [--no-vstree]"
    );
    std::process::exit(2)
}

fn parse_args() -> Opts {
    let mut args = std::env::args().skip(1);
    let mut o = Opts {
        dir: PathBuf::new(),
        modes: vec!["all".into()],
        qreps: 5,
        reps: 7,
        lookups: 20_000,
        changes: 5_000,
        seed: 42,
        queries: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm"),
        cache_pages: None,
        vstree: true,
    };
    let mut dir = None;
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--mode" => o.modes = val().split(',').map(str::to_string).collect(),
            "--qreps" => o.qreps = val().parse().unwrap_or_else(|_| usage()),
            "--reps" => o.reps = val().parse().unwrap_or_else(|_| usage()),
            "--lookups" => o.lookups = val().parse().unwrap_or_else(|_| usage()),
            "--changes" => o.changes = val().parse().unwrap_or_else(|_| usage()),
            "--seed" => o.seed = val().parse().unwrap_or_else(|_| usage()),
            "--queries" => o.queries = PathBuf::from(val()),
            "--cache-pages" => o.cache_pages = Some(val().parse().unwrap_or_else(|_| usage())),
            "--no-vstree" => o.vstree = false,
            "-h" | "--help" => usage(),
            _ if dir.is_none() && !a.starts_with("--") => dir = Some(PathBuf::from(a)),
            _ => usage(),
        }
    }
    o.dir = dir.unwrap_or_else(|| usage());
    o.qreps = o.qreps.max(1);
    o.reps = o.reps.max(1);
    o.lookups = o.lookups.max(1);
    o
}

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

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn geomean(v: &[f64]) -> f64 {
    (v.iter().map(|x| x.max(1e-3).ln()).sum::<f64>() / v.len() as f64).exp()
}

fn main() {
    let o = parse_args();
    let kv = o.dir.join("kvstore.kv");
    assert!(kv.is_file(), "no kvstore.kv in {}", o.dir.display());
    let all = o.modes.iter().any(|m| m == "all");
    let want = |m: &str| all || o.modes.iter().any(|x| x == m);

    let file_pages = (std::fs::metadata(&kv).unwrap().len() as usize).div_ceil(4096);
    let cache = o.cache_pages.unwrap_or(file_pages + 64);
    let t = Instant::now();
    let ds = DiskStore::open(&kv, cache).expect("open kvstore.kv");
    println!("open_ms={:.1}", ms(t));
    println!(
        "cache_pages={cache} file_pages={file_pages} triples={} entities={} literals={} \
         predicates={}",
        ds.triple_count(),
        ds.entity_num(),
        ds.literal_num(),
        ds.predicate_num()
    );
    let t = Instant::now();
    let dict = ds.dictionary().expect("dictionary");
    println!("dictionary_ms={:.1}", ms(t));
    let t = Instant::now();
    let mut mem = TripleStore::new();
    mem.bulk_load(ds.try_iter_spo().expect("SPO scan"));
    println!("materialize_ms={:.1}", ms(t));
    let vskv = o.dir.join("vstree.kv");
    let vstree = if o.vstree && vskv.is_file() && !ds.vstree_stale() {
        Some(VsTree::open_disk(&vskv).expect("open vstree.kv"))
    } else {
        None
    };
    println!("vstree={}", if vstree.is_some() { "on" } else { "off" });
    let dt = DiskTriples::new(ds).expect("DiskTriples");
    // Warm the page cache: one full scan touches every SPO leaf, and the
    // per-predicate scans below every POS leaf.
    black_box(dt.disk_store().try_iter_spo().expect("scan").len());

    let queries: Vec<(usize, String)> = (1..=14)
        .map(|i| {
            let p = o.queries.join(format!("lubm_q{i}.rq"));
            let text =
                std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
            (i, text)
        })
        .collect();
    if want("query") {
        query_mode(&o, &dict, &mem, &dt, vstree.as_ref(), &queries);
    }
    if want("overlay") {
        overlay_mode(&o, &dict, dt, vstree.as_ref(), &queries);
    }
}

/// Evaluate `q` over `store` in a read scope that must stay clean.
fn eval<S: TripleSource>(
    dict: &gstore::dict::Dictionary,
    store: &S,
    vstree: Option<&VsTree>,
    q: &gstore::parser::sparql::ast::Query,
) -> QueryResult {
    let scope = ReadScope::enter();
    let ev = match vstree {
        Some(v) => Evaluator::with_vstree(dict, store, v),
        None => Evaluator::new(dict, store),
    };
    let r = ev.evaluate(q).expect("query");
    assert_eq!(scope.take_error(), None, "a read error was noted");
    r
}

fn rows(r: &QueryResult) -> usize {
    match r {
        QueryResult::Select(rs) => rs.rows.len(),
        QueryResult::Ask(b) => usize::from(*b),
        QueryResult::Construct(ts) => ts.len(),
        other => panic!("unexpected result {other:?}"),
    }
}

fn query_mode(
    o: &Opts,
    dict: &gstore::dict::Dictionary,
    mem: &TripleStore,
    dt: &DiskTriples,
    vstree: Option<&VsTree>,
    queries: &[(usize, String)],
) {
    let view_eval = |q: &gstore::parser::sparql::ast::Query| -> QueryResult {
        let scope = ReadScope::enter();
        let r = dt
            .read_view()
            .with_base(|ds, cancel| {
                let src = DiskSource::new(ds);
                let ev = match vstree {
                    Some(v) => Evaluator::with_vstree(dict, &src, v),
                    None => Evaluator::new(dict, &src),
                };
                ev.with_cancel(cancel.clone()).evaluate(q)
            })
            .expect("view")
            .expect("query");
        assert_eq!(scope.take_error(), None, "a read error was noted");
        r
    };
    let (mut gm_mem, mut gm_disk, mut gm_view) = (Vec::new(), Vec::new(), Vec::new());
    let mut all_match = true;
    let mut worst = (0.0f64, 0usize);
    for (i, text) in queries {
        let q = sparql::parse(text).expect("parse");
        let t = Instant::now();
        let want = eval(dict, mem, vstree, &q);
        let mem_first = ms(t);
        let t = Instant::now();
        let got = eval(dict, dt, vstree, &q);
        let disk_first = ms(t);
        let viewed = view_eval(&q);
        let ok = got == want && viewed == want;
        all_match &= ok;
        let (mut tm, mut td, mut tv) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..o.qreps {
            let t = Instant::now();
            black_box(eval(dict, mem, vstree, &q));
            tm.push(ms(t));
            let t = Instant::now();
            black_box(eval(dict, dt, vstree, &q));
            td.push(ms(t));
            let t = Instant::now();
            black_box(view_eval(&q));
            tv.push(ms(t));
        }
        let (m, d, v) = (median(tm), median(td), median(tv));
        gm_mem.push(m);
        gm_disk.push(d);
        gm_view.push(v);
        let ratio = d.max(1e-3) / m.max(1e-3);
        if ratio > worst.0 {
            worst = (ratio, *i);
        }
        println!(
            "q{i}_rows={} q{i}_mem_first_ms={mem_first:.2} q{i}_disk_first_ms={disk_first:.2} \
             q{i}_mem_ms={m:.3} q{i}_disk_ms={d:.3} q{i}_view_ms={v:.3} \
             q{i}_disk_over_mem={ratio:.2} q{i}_match={ok}",
            rows(&want)
        );
    }
    let (gm, gd, gv) = (geomean(&gm_mem), geomean(&gm_disk), geomean(&gm_view));
    println!(
        "query_mem_geomean_ms={gm:.3} query_disk_geomean_ms={gd:.3} query_view_geomean_ms={gv:.3} \
         query_disk_over_mem={:.3} query_view_over_mem={:.3} query_worst=q{}:{:.2} \
         query_all_match={all_match}",
        gd / gm,
        gv / gm,
        worst.1,
        worst.0
    );
}

/// The lookup sample: keys the "other keys" overlay never touches.
struct Sample {
    subs: Vec<u32>,
    sp: Vec<(u32, u32)>,
    objs: Vec<u32>,
    po: Vec<(u32, u32)>,
    triples: Vec<IdTriple>,
}

impl Sample {
    fn new(rng: &mut Rng, all: &[IdTriple], n: usize) -> Sample {
        let mut pick = || all[rng.below(all.len())];
        let ts: Vec<IdTriple> = (0..n).map(|_| pick()).collect();
        let sp: Vec<(u32, u32)> = (0..n).map(|_| pick()).map(|t| (t.sub, t.pred)).collect();
        let po: Vec<(u32, u32)> = (0..n).map(|_| pick()).map(|t| (t.pred, t.obj)).collect();
        let objs: Vec<u32> = (0..n).map(|_| pick().obj).collect();
        Sample {
            subs: ts.iter().map(|t| t.sub).collect(),
            sp,
            objs,
            po,
            triples: ts,
        }
    }

    fn protected_subjects(&self) -> HashSet<u32> {
        self.subs
            .iter()
            .copied()
            .chain(self.sp.iter().map(|&(s, _)| s))
            .chain(self.triples.iter().map(|t| t.sub))
            .collect()
    }

    fn protected_objects(&self) -> HashSet<u32> {
        self.objs
            .iter()
            .copied()
            .chain(self.po.iter().map(|&(_, o)| o))
            .chain(self.triples.iter().map(|t| t.obj))
            .collect()
    }
}

const METHODS: [&str; 6] = [
    "po_by_s",
    "o_by_sp",
    "ps_by_o",
    "s_by_po",
    "count_po_by_s",
    "exists",
];

/// Run method `m` over the sample; a checksum of the answers.
fn run_method(src: &dyn TripleSource, smp: &Sample, m: usize) -> u64 {
    match m {
        0 => smp.subs.iter().map(|&s| src.po_by_s(s).len() as u64).sum(),
        1 => smp
            .sp
            .iter()
            .map(|&(s, p)| src.o_by_sp(s, p).len() as u64)
            .sum(),
        2 => smp.objs.iter().map(|&o| src.ps_by_o(o).len() as u64).sum(),
        3 => smp
            .po
            .iter()
            .map(|&(p, o)| src.s_by_po(p, o).len() as u64)
            .sum(),
        4 => smp.subs.iter().map(|&s| src.count_po_by_s(s)).sum(),
        _ => smp
            .triples
            .iter()
            .map(|t| u64::from(src.exists(t.sub, t.pred, t.obj)))
            .sum(),
    }
}

/// Time every method over every source, round-robin; returns
/// `[source][method] -> (median ns per lookup, checksum)`.
fn time_sources(
    reps: usize,
    smp: &Sample,
    n: usize,
    sources: &[&dyn TripleSource],
) -> Vec<Vec<(f64, u64)>> {
    let mut times = vec![vec![Vec::new(); METHODS.len()]; sources.len()];
    let mut sums = vec![vec![0u64; METHODS.len()]; sources.len()];
    for rep in 0..=reps {
        for m in 0..METHODS.len() {
            for (k, src) in sources.iter().enumerate() {
                let t = Instant::now();
                let sum = black_box(run_method(*src, smp, m));
                let ns = t.elapsed().as_secs_f64() * 1e9 / n as f64;
                sums[k][m] = sum;
                if rep > 0 {
                    times[k][m].push(ns); // rep 0 warms up
                }
            }
        }
    }
    times
        .into_iter()
        .zip(sums)
        .map(|(ts, ss)| ts.into_iter().map(median).zip(ss).collect())
        .collect()
}

fn overlay_mode(
    o: &Opts,
    dict: &gstore::dict::Dictionary,
    mut dt: DiskTriples,
    vstree: Option<&VsTree>,
    queries: &[(usize, String)],
) {
    let all = dt.disk_store().try_iter_spo().expect("scan");
    let mut rng = Rng(o.seed);
    let smp = Sample::new(&mut rng, &all, o.lookups);
    let (keep_s, keep_o) = (smp.protected_subjects(), smp.protected_objects());

    // The "other keys" overlay: adds under fresh subjects, removes triples of
    // subjects and objects the sample never reads.
    let max_sub = all.iter().map(|t| t.sub).max().unwrap_or(0);
    let mut added = Vec::with_capacity(o.changes);
    let mut removed = Vec::with_capacity(o.changes);
    let mut guard = 0usize;
    while (added.len() < o.changes || removed.len() < o.changes) && guard < 100 * o.changes {
        guard += 1;
        let t = all[rng.below(all.len())];
        if added.len() < o.changes && !keep_o.contains(&t.obj) {
            added.push(IdTriple::new(
                max_sub + 1 + added.len() as u32,
                t.pred,
                t.obj,
            ));
        }
        let r = all[rng.below(all.len())];
        if removed.len() < o.changes && !keep_s.contains(&r.sub) && !keep_o.contains(&r.obj) {
            removed.push(r);
        }
    }
    let mut other = Overlay::new();
    {
        let base = DiskSource::new(dt.disk_store());
        for &t in &added {
            other.insert(&base, t);
        }
        for &t in &removed {
            other.remove(&base, t);
        }
    }
    println!(
        "overlay_changes={} (added {}, removed {}) lookups={} reps={}",
        other.len(),
        other.added().triple_count(),
        other.removed().triple_count(),
        o.lookups,
        o.reps
    );

    // Phase 1: no transaction.
    let empty = Overlay::new();
    let phase1 = {
        let base = DiskSource::new(dt.disk_store());
        let v_empty = OverlayView::new(&base, &empty);
        let v_other = OverlayView::new(&base, &other);
        time_sources(
            o.reps,
            &smp,
            o.lookups,
            &[&base, &v_empty, &v_other, &dt as &dyn TripleSource],
        )
    };
    // Phase 2: a transaction holding the same changes.
    dt.begin().expect("begin");
    for &t in &added {
        dt.insert_triple(t).expect("insert");
    }
    for &t in &removed {
        dt.remove_triple(t).expect("remove");
    }
    let phase2 = {
        let base = DiskSource::new(dt.disk_store());
        time_sources(o.reps, &smp, o.lookups, &[&base, &dt as &dyn TripleSource])
    };

    let labels = ["base", "empty", "other", "dt_plain"];
    let mut answers_match = true;
    let mut ratio_logs: Vec<Vec<f64>> = vec![Vec::new(); 5];
    for (m, name) in METHODS.iter().enumerate() {
        let base_ns = phase1[0][m].0;
        let mut line = String::new();
        for (k, label) in labels.iter().enumerate() {
            let (ns, sum) = phase1[k][m];
            answers_match &= sum == phase1[0][m].1;
            line += &format!("overlay_{name}_{label}_ns={ns:.0} ");
            if k > 0 {
                ratio_logs[k].push((ns / base_ns).ln());
            }
        }
        let (base2, _) = phase2[0][m];
        let (txn_ns, sum) = phase2[1][m];
        answers_match &= sum == phase1[0][m].1 && phase2[0][m].1 == phase1[0][m].1;
        ratio_logs[4].push((txn_ns / base2).ln());
        line += &format!("overlay_{name}_base2_ns={base2:.0} overlay_{name}_dt_txn_ns={txn_ns:.0}");
        println!("{line}");
    }
    let g = |k: usize| (ratio_logs[k].iter().sum::<f64>() / ratio_logs[k].len() as f64).exp();
    let worst = |k: usize| ratio_logs[k].iter().map(|l| l.exp()).fold(0.0f64, f64::max);
    println!(
        "overlay_empty_ratio={:.3} overlay_other_ratio={:.3} overlay_dt_plain_ratio={:.3} \
         overlay_dt_txn_ratio={:.3} (geomeans over the methods; worst: empty {:.3}, other {:.3}, \
         dt_plain {:.3}, dt_txn {:.3}) overlay_answers_match={answers_match}",
        g(1),
        g(2),
        g(3),
        g(4),
        worst(1),
        worst(2),
        worst(3),
        worst(4)
    );

    // Q1–Q14 with the transaction open (its overlay touches the predicates
    // the queries scan) against the same store without it.
    let mut with_txn = Vec::new();
    let mut plain = Vec::new();
    let txn_overlay = dt
        .transaction()
        .expect("open transaction")
        .overlay()
        .clone();
    for (i, text) in queries {
        let q = sparql::parse(text).expect("parse");
        let base = DiskSource::new(dt.disk_store());
        let view = OverlayView::new(&base, &txn_overlay);
        black_box(eval(dict, &dt, vstree, &q));
        black_box(eval(dict, &base, vstree, &q));
        let same = eval(dict, &dt, vstree, &q) == eval(dict, &view, vstree, &q);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for _ in 0..o.qreps {
            let t = Instant::now();
            black_box(eval(dict, &dt, vstree, &q));
            a.push(ms(t));
            let t = Instant::now();
            black_box(eval(dict, &base, vstree, &q));
            b.push(ms(t));
        }
        let (a, b) = (median(a), median(b));
        with_txn.push(a);
        plain.push(b);
        println!("q{i}_txn_ms={a:.3} q{i}_plain_ms={b:.3} q{i}_txn_view_match={same}");
    }
    println!(
        "query_txn_geomean_ms={:.3} query_plain_geomean_ms={:.3} query_txn_over_plain={:.3}",
        geomean(&with_txn),
        geomean(&plain),
        geomean(&with_txn) / geomean(&plain)
    );
    dt.rollback();
}
