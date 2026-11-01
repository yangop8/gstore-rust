//! `kv_bench` — micro-benchmarks for the on-disk kvstore read path (Phase 6,
//! packages R1-KV and R2-DICT).
//!
//! ```text
//! cargo run --release --example kv_bench -- <db_dir> [options]
//!
//!   <db_dir>             a database built by `gbuild --disk` (holds kvstore.kv);
//!                        for `contains`, any database (e.g. a snapshot)
//!   --mode M[,M...]      lookup | scan | open | query | dict-ooc | contains | all
//!                        (default all = every mode but contains)
//!   --reps N             timed repetitions per measurement         (default 5)
//!   --qreps N            warm repetitions per query                (default 3)
//!   --lookups N          random lookups per repetition             (default 20000)
//!   --seed S             RNG seed for the lookup sample            (default 42)
//!   --queries DIR        directory holding lubm_q1.rq .. lubm_q14.rq
//!                        (default <crate>/testdata/lubm)
//!   --cache-pages N      page cache of the hot lookup/scan store
//!                        (default: the whole kvstore.kv file; dict-ooc: 4096)
//!   --dict-cache-mb N    dict-ooc: the out-of-core dictionary's id → string
//!                        cache budget in MiB (default 64; key → id: N / 4)
//!   --contains NEEDLE    contains: the substring of the CONTAINS filter
//!                        (default "Course1")
//! ```
//!
//! The database the R1-KV numbers use is the univ 8 LUBM-like data built once
//! with the **base** binaries and read by both builds (the format is the same):
//!
//! ```text
//! gbuild --disk <dir>/lubm8 lubm-like-univ8.nt     # → <dir>/lubm8.db
//! kv_bench <dir>/lubm8.db --queries testdata/lubm
//! ```
//!
//! Every measurement prints `key=value` lines on stdout (times in ms unless the
//! key ends in `_ns`; "median" is the median of the timed repetitions after one
//! untimed warm-up run):
//!
//! * `lookup`: hot random `po_by_s` and `o_by_sp` on a store whose page cache
//!   holds the whole file — `lookup_po_by_s_ns`, `lookup_o_by_sp_ns` (per
//!   lookup) plus the summed result sizes (`*_rows`, equal across builds).
//! * `scan`: a full SPO scan materialized as `Vec<IdTriple>` (`scan_spo_ms`);
//!   on builds that have it, also the contract-ordered `(p,s,o)` `iter_all`.
//! * `open`: the Materialize open — `DiskStore::open` + `to_memory` +
//!   `named_to_memory` (`open_to_memory_ms`) — and the whole
//!   `Database::load_disk` (`open_load_disk_ms`), first run and warm median.
//! * `query`: `DiskStore::query` (streaming, out-of-core dictionary) for
//!   LUBM Q1–Q14: first run, warm median, rows and an order-independent row
//!   hash, checked against the memory answer of `Database::load_disk` (`match`).
//!   `query_geomean_warm_ms` is the geometric mean of the warm medians.
//! * `dict-ooc` (R2-DICT): Q1–Q14 through the evaluator over the on-disk
//!   triples twice — with the out-of-core dictionary (`DiskDict` with a
//!   bounded cache, as `DiskStore::query` uses) and with the dictionary
//!   loaded into memory — `qreps` runs each, the out-of-core ones three times
//!   over (`dictooc_qN_ms` / `dictooc_qN_memdict_ms`, rows compared in
//!   order); the dictionary bytes resident after Q1–Q14 × 3
//!   (`dictooc_resident_bytes`) and the process RSS.
//! * `contains` (R2-DICT): `Database::open` of the directory (query cache
//!   off), then the same `CONTAINS` query `1 + qreps` times — the first
//!   builds the dictionary substring index, later ones reuse the database's
//!   cached one (`contains_first_ms`, `contains_second_ms`, `contains_ratio`).
//!
//! The database directory is only read (the memory comparison opens it through
//! `Database::load_disk`, which writes nothing to a database whose VS-tree is
//! fresh, as `gbuild --disk` leaves it).
//!
//! The `api` module is the only build-specific part: it names the read API of
//! the build under test, so the same harness compiles against an older commit
//! with a different `api` module (like-for-like A/B runs).

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;

use gstore::kvstore::DiskStore;
use gstore::model::IdTriple;
use gstore::{Database, QueryResult};

/// The read API of this build (see the module docs).
mod api {
    use gstore::kvstore::DiskStore;
    use gstore::model::IdTriple;

    pub const NAME: &str = "branch";

    pub fn po_by_s(ds: &DiskStore, s: u32) -> Vec<(u32, u32)> {
        ds.try_po_by_s(s).expect("po_by_s")
    }
    pub fn o_by_sp(ds: &DiskStore, s: u32, p: u32) -> Vec<u32> {
        ds.try_o_by_sp(s, p).expect("o_by_sp")
    }
    /// Every triple in SPO order.
    pub fn scan_spo(ds: &DiskStore) -> Vec<IdTriple> {
        ds.try_iter_spo().expect("scan")
    }
    /// Every triple in the C1 contract order `(p, s, o)`, when the build has it.
    pub fn iter_all_contract(ds: &DiskStore) -> Option<Vec<IdTriple>> {
        Some(ds.try_iter_all().expect("iter_all"))
    }
    pub fn subjects(ds: &DiskStore) -> Vec<u32> {
        ds.try_subject_keys().expect("subject_keys")
    }
}

struct Opts {
    dir: PathBuf,
    modes: Vec<String>,
    reps: usize,
    qreps: usize,
    lookups: usize,
    seed: u64,
    queries: PathBuf,
    cache_pages: Option<usize>,
    dict_cache_mb: usize,
    needle: String,
}

fn usage() -> ! {
    eprintln!(
        "usage: kv_bench <db_dir> [--mode lookup,scan,open,query,dict-ooc,contains|all] \
         [--reps N] [--qreps N] [--lookups N] [--seed S] [--queries DIR] [--cache-pages N] \
         [--dict-cache-mb N] [--contains NEEDLE]"
    );
    std::process::exit(2)
}

fn parse_args() -> Opts {
    let mut args = std::env::args().skip(1);
    let mut o = Opts {
        dir: PathBuf::new(),
        modes: vec!["all".into()],
        reps: 5,
        qreps: 3,
        lookups: 20_000,
        seed: 42,
        queries: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm"),
        cache_pages: None,
        dict_cache_mb: 64,
        needle: "Course1".into(),
    };
    let mut dir = None;
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--mode" => o.modes = val().split(',').map(str::to_string).collect(),
            "--reps" => o.reps = val().parse().unwrap_or_else(|_| usage()),
            "--qreps" => o.qreps = val().parse().unwrap_or_else(|_| usage()),
            "--lookups" => o.lookups = val().parse().unwrap_or_else(|_| usage()),
            "--seed" => o.seed = val().parse().unwrap_or_else(|_| usage()),
            "--queries" => o.queries = PathBuf::from(val()),
            "--cache-pages" => o.cache_pages = Some(val().parse().unwrap_or_else(|_| usage())),
            "--dict-cache-mb" => o.dict_cache_mb = val().parse().unwrap_or_else(|_| usage()),
            "--contains" => o.needle = val(),
            "-h" | "--help" => usage(),
            _ if dir.is_none() && !a.starts_with("--") => dir = Some(PathBuf::from(a)),
            _ => usage(),
        }
    }
    o.dir = dir.unwrap_or_else(|| usage());
    o.reps = o.reps.max(1);
    o.qreps = o.qreps.max(1);
    o
}

/// SplitMix64: a tiny deterministic RNG for the lookup sample.
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

/// Run `f` once untimed, then `reps` timed times; the median in ms.
fn timed<R>(reps: usize, mut f: impl FnMut() -> R) -> f64 {
    black_box(f());
    median(
        (0..reps)
            .map(|_| {
                let t = Instant::now();
                black_box(f());
                ms(t)
            })
            .collect(),
    )
}

fn main() {
    let o = parse_args();
    let kv = o.dir.join("kvstore.kv");
    let all = o.modes.iter().any(|m| m == "all");
    let want = |m: &str| all || o.modes.iter().any(|x| x == m);
    println!("build={}", api::NAME);
    if o.modes.iter().any(|x| x == "contains") {
        contains(&o);
        if !all && o.modes.iter().all(|x| x == "contains") {
            return;
        }
    }
    assert!(kv.is_file(), "no kvstore.kv in {}", o.dir.display());
    if want("lookup") || want("scan") {
        lookup_and_scan(&o, &kv, want("lookup"), want("scan"));
    }
    if want("open") {
        open(&o, &kv);
    }
    if want("query") {
        query(&o, &kv);
    }
    if want("dict-ooc") {
        dict_ooc(&o, &kv);
    }
}

fn lookup_and_scan(o: &Opts, kv: &Path, lookup: bool, scan: bool) {
    let file_pages = (std::fs::metadata(kv).unwrap().len() as usize).div_ceil(4096);
    let cache = o.cache_pages.unwrap_or(file_pages + 64);
    let ds = DiskStore::open(kv, cache).expect("open");
    println!(
        "cache_pages={cache} file_pages={file_pages} triples={}",
        ds.triple_count()
    );
    // Warm the whole cache: one full scan touches every SPO leaf.
    let triples = api::scan_spo(&ds);
    assert_eq!(triples.len() as u64, ds.triple_count());
    if lookup {
        let subjects = api::subjects(&ds);
        let mut rng = Rng(o.seed);
        let subs: Vec<u32> = (0..o.lookups)
            .map(|_| subjects[rng.below(subjects.len())])
            .collect();
        let pairs: Vec<(u32, u32)> = (0..o.lookups)
            .map(|_| {
                let t: IdTriple = triples[rng.below(triples.len())];
                (t.sub, t.pred)
            })
            .collect();
        let rows: usize = subs.iter().map(|&s| api::po_by_s(&ds, s).len()).sum();
        let per = timed(o.reps, || {
            subs.iter()
                .map(|&s| api::po_by_s(&ds, s).len())
                .sum::<usize>()
        });
        println!(
            "lookup_po_by_s_ns={:.0} lookup_po_by_s_rows={rows}",
            per * 1e6 / o.lookups as f64
        );
        let rows: usize = pairs
            .iter()
            .map(|&(s, p)| api::o_by_sp(&ds, s, p).len())
            .sum();
        let per = timed(o.reps, || {
            pairs
                .iter()
                .map(|&(s, p)| api::o_by_sp(&ds, s, p).len())
                .sum::<usize>()
        });
        println!(
            "lookup_o_by_sp_ns={:.0} lookup_o_by_sp_rows={rows}",
            per * 1e6 / o.lookups as f64
        );
    }
    if scan {
        let t = timed(o.reps, || api::scan_spo(&ds).len());
        println!("scan_spo_ms={t:.1}");
        if api::iter_all_contract(&ds).is_some() {
            let t = timed(o.reps, || api::iter_all_contract(&ds).map(|v| v.len()));
            println!("scan_iter_all_pso_ms={t:.1}");
        }
    }
}

fn open(o: &Opts, kv: &Path) {
    let open_to_memory = || {
        let ds = DiskStore::open(kv, 4096).expect("open");
        let (dict, store) = ds.to_memory().expect("to_memory");
        let named = ds.named_to_memory().expect("named");
        (dict.entity_num(), store.triple_count(), named.len())
    };
    let t = Instant::now();
    let first = open_to_memory();
    println!("open_to_memory_first_ms={:.1}", ms(t));
    println!("open_to_memory_ms={:.1}", timed(o.reps, open_to_memory));
    println!("open_entities={} open_triples={}", first.0, first.1);
    let load = || Database::load_disk(&o.dir).expect("load_disk").triple_num();
    let t = Instant::now();
    load();
    println!("open_load_disk_first_ms={:.1}", ms(t));
    println!("open_load_disk_ms={:.1}", timed(o.reps, load));
}

/// FNV-1a over a sorted copy of the rows: equal for equal row multisets.
fn rows_hash(r: &QueryResult) -> (usize, u64) {
    match r {
        QueryResult::Select(rs) => {
            let mut rows = rs.rows.clone();
            rows.sort();
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            let mut eat = |b: u8| {
                h ^= b as u64;
                h = h.wrapping_mul(0x0100_0000_01b3);
            };
            for row in &rows {
                for cell in row {
                    match cell {
                        Some(s) => s.bytes().chain([0u8]).for_each(&mut eat),
                        None => eat(0xff),
                    }
                }
                eat(b'\n');
            }
            (rows.len(), h)
        }
        QueryResult::Ask(b) => (*b as usize, *b as u64),
        other => panic!("unexpected result {other:?}"),
    }
}

fn query(o: &Opts, kv: &Path) {
    let texts: Vec<(usize, String)> = (1..=14)
        .map(|i| {
            let p = o.queries.join(format!("lubm_q{i}.rq"));
            (
                i,
                std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())),
            )
        })
        .collect();
    // The memory reference: the Materialize open of the same directory.
    let mut db = Database::load_disk(&o.dir).expect("load_disk");
    db.set_query_cache_size(0);
    let reference: Vec<(usize, u64)> = texts
        .iter()
        .map(|(_, q)| rows_hash(&db.query(q).expect("memory query")))
        .collect();
    drop(db);

    let ds = DiskStore::open(kv, 4096).expect("open");
    let mut logs = Vec::new();
    let mut all_match = true;
    for ((i, q), want) in texts.iter().zip(&reference) {
        let t = Instant::now();
        let got = rows_hash(&ds.query(q).expect("disk query"));
        let first = ms(t);
        let warm = median(
            (0..o.qreps)
                .map(|_| {
                    let t = Instant::now();
                    black_box(ds.query(q).expect("disk query"));
                    ms(t)
                })
                .collect(),
        );
        let ok = got == *want;
        all_match &= ok;
        logs.push(warm.max(1e-3).ln());
        println!(
            "query_q{i}_first_ms={first:.2} query_q{i}_warm_ms={warm:.2} query_q{i}_rows={} \
             query_q{i}_mem_rows={} query_q{i}_match={ok}",
            got.0, want.0
        );
    }
    let gm = (logs.iter().sum::<f64>() / logs.len() as f64).exp();
    println!("query_geomean_warm_ms={gm:.2} query_all_match={all_match}");
}

fn read_queries(o: &Opts) -> Vec<(usize, String)> {
    (1..=14)
        .map(|i| {
            let p = o.queries.join(format!("lubm_q{i}.rq"));
            (
                i,
                std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())),
            )
        })
        .collect()
}

/// Rows of a result, in order.
fn result_rows(r: QueryResult) -> Vec<Vec<Option<String>>> {
    match r {
        QueryResult::Select(rs) => rs.rows,
        QueryResult::Ask(b) => vec![vec![Some(b.to_string())]],
        other => panic!("unexpected result {other:?}"),
    }
}

/// The process's resident set (`VmRSS`) in KiB.
fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

/// R2-DICT: Q1–Q14 over the on-disk triples with the out-of-core dictionary
/// vs the same queries with the dictionary in memory.
fn dict_ooc(o: &Opts, kv: &Path) {
    use gstore::dict::Dictionary;
    use gstore::kvstore::DiskDict;
    use gstore::parser::sparql;
    use gstore::query::Evaluator;
    use std::sync::Arc;

    let texts = read_queries(o);
    let parsed: Vec<_> = texts
        .iter()
        .map(|(_, q)| sparql::parse(q).expect("parse"))
        .collect();
    let ds = DiskStore::open(kv, o.cache_pages.unwrap_or(4096)).expect("open");
    let rss_open = rss_kib();
    let budget = o.dict_cache_mb << 20;
    let dd = Arc::new(
        DiskDict::open_shared(ds.shared_pager())
            .expect("dictionary")
            .with_cache_budgets(budget, budget / 4),
    );
    let lazy = Dictionary::from_backing(dd);
    let run = |dict: &Dictionary, q| Evaluator::new(dict, &ds).evaluate(q).expect("query");
    // Three passes of Q1–Q14 through the out-of-core dictionary first (the
    // resident-bytes figure is taken after them), then the in-memory one.
    let mut ooc: Vec<Vec<f64>> = vec![Vec::new(); texts.len()];
    let mut ooc_rows = Vec::new();
    for pass in 0..3 {
        for (k, q) in parsed.iter().enumerate() {
            let t = Instant::now();
            let r = run(&lazy, q);
            ooc[k].push(ms(t));
            if pass == 0 {
                ooc_rows.push(result_rows(r));
            }
            for _ in 1..o.qreps {
                let t = Instant::now();
                black_box(run(&lazy, q));
                ooc[k].push(ms(t));
            }
        }
    }
    let st = lazy.cache_stats();
    println!(
        "dictooc_resident_bytes={} dictooc_term_entries={} dictooc_key_entries={} \
         dictooc_budget_bytes={} dictooc_hits={} dictooc_misses={} dictooc_evictions={} \
         dictooc_rss_open_kib={rss_open} dictooc_rss_after_kib={}",
        st.resident_bytes(),
        st.term_entries,
        st.key_entries,
        st.term_budget + st.key_budget,
        st.hits,
        st.misses,
        st.evictions,
        rss_kib()
    );
    let mem = ds.dictionary().expect("dictionary");
    let mut all_match = true;
    let (mut lo, mut lm) = (Vec::new(), Vec::new());
    for (k, (i, _)) in texts.iter().enumerate() {
        let want = result_rows(run(&mem, &parsed[k]));
        let warm_mem = median(
            (0..o.qreps.max(3))
                .map(|_| {
                    let t = Instant::now();
                    black_box(run(&mem, &parsed[k]));
                    ms(t)
                })
                .collect(),
        );
        // The first out-of-core run filled the cache: warm = the later ones.
        let warm_ooc = median(ooc[k][1..].to_vec());
        let ok = want == ooc_rows[k];
        all_match &= ok;
        lo.push(warm_ooc.max(1e-3).ln());
        lm.push(warm_mem.max(1e-3).ln());
        println!(
            "dictooc_q{i}_first_ms={:.2} dictooc_q{i}_ms={warm_ooc:.2} \
             dictooc_q{i}_memdict_ms={warm_mem:.2} dictooc_q{i}_ratio={:.2} \
             dictooc_q{i}_rows={} dictooc_q{i}_match={ok}",
            ooc[k][0],
            warm_ooc / warm_mem.max(1e-6),
            want.len()
        );
    }
    let gm = |v: &[f64]| (v.iter().sum::<f64>() / v.len() as f64).exp();
    println!(
        "dictooc_geomean_ms={:.2} dictooc_memdict_geomean_ms={:.2} dictooc_all_match={all_match}",
        gm(&lo),
        gm(&lm)
    );
}

/// R2-DICT: the same CONTAINS query `1 + qreps` times on one open database
/// (query cache off): the first builds the dictionary substring index, the
/// later ones reuse the database's cached one.
fn contains(o: &Opts) {
    let q = format!(
        "PREFIX ub: <http://www.lehigh.edu/~zhp2/2004/0401/univ-bench.owl#>\n\
         SELECT ?x ?n WHERE {{ ?x ub:name ?n . FILTER(CONTAINS(?n, \"{}\")) }}",
        o.needle
    );
    let t = Instant::now();
    let mut db = Database::open(&o.dir).expect("open");
    db.set_query_cache_size(0);
    println!("contains_open_ms={:.1}", ms(t));
    let t = Instant::now();
    let first = rows_hash(&db.query(&q).expect("query"));
    let first_ms = ms(t);
    let mut later = Vec::new();
    for _ in 0..o.qreps {
        let t = Instant::now();
        let again = rows_hash(&db.query(&q).expect("query"));
        later.push(ms(t));
        assert_eq!(again, first, "same answer");
    }
    let second_ms = later[0];
    println!(
        "contains_rows={} contains_first_ms={first_ms:.1} contains_second_ms={second_ms:.1} \
         contains_warm_median_ms={:.1} contains_ratio={:.3}",
        first.0,
        median(later.clone()),
        second_ms / first_ms
    );
}
