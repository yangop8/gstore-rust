//! `kv_bench` — micro-benchmarks for the on-disk kvstore read path (Phase 6,
//! package R1-KV).
//!
//! ```text
//! cargo run --release --example kv_bench -- <db_dir> [options]
//!
//!   <db_dir>             a database built by `gbuild --disk` (holds kvstore.kv)
//!   --mode M[,M...]      lookup | scan | open | query | all      (default all)
//!   --reps N             timed repetitions per measurement         (default 5)
//!   --qreps N            warm repetitions per query                (default 3)
//!   --lookups N          random lookups per repetition             (default 20000)
//!   --seed S             RNG seed for the lookup sample            (default 42)
//!   --queries DIR        directory holding lubm_q1.rq .. lubm_q14.rq
//!                        (default <crate>/testdata/lubm)
//!   --cache-pages N      page cache of the hot lookup/scan store
//!                        (default: the whole kvstore.kv file)
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
}

fn usage() -> ! {
    eprintln!(
        "usage: kv_bench <db_dir> [--mode lookup,scan,open,query|all] [--reps N] [--qreps N] \
         [--lookups N] [--seed S] [--queries DIR] [--cache-pages N]"
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
    assert!(kv.is_file(), "no kvstore.kv in {}", o.dir.display());
    let all = o.modes.iter().any(|m| m == "all");
    let want = |m: &str| all || o.modes.iter().any(|x| x == m);
    println!("build={}", api::NAME);
    if want("lookup") || want("scan") {
        lookup_and_scan(&o, &kv, want("lookup"), want("scan"));
    }
    if want("open") {
        open(&o, &kv);
    }
    if want("query") {
        query(&o, &kv);
    }
}

fn lookup_and_scan(o: &Opts, kv: &Path, lookup: bool, scan: bool) {
    let file_pages = (std::fs::metadata(kv).unwrap().len() as usize).div_ceil(4096);
    let cache = o.cache_pages.unwrap_or(file_pages + 64);
    let ds = DiskStore::open(kv, cache).expect("open");
    println!("cache_pages={cache} file_pages={file_pages} triples={}", ds.triple_count());
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
        let rows: usize = pairs.iter().map(|&(s, p)| api::o_by_sp(&ds, s, p).len()).sum();
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
            (i, std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
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
