//! Id-reclamation churn (Phase 6, R4-RECLAIM; gate G5): a LUBM-like dataset
//! is split by university; a database holding universities `0..8` then runs
//! ten rounds of "delete university `r`, insert university `8 + r`", with
//! immediate reclamation (`on`) and without it (`off`, the control), on the
//! memory, Stream-disk, Materialize-disk or RocksDB layout.
//!
//! ```text
//! cargo run --release --example reclaim_churn -- \
//!     --data /tmp/gstore-bench-cache/lubm-like1-u38-s0.nt \
//!     [--layout memory|stream|materialize|rocksdb] [--policy on|off|both] \
//!     [--base 8] [--rounds 10] [--scratch DIR]
//! ```
//!
//! `--data` is an N-Triples file of at least `base + rounds` universities
//! (`gstore-bench gen --workload lubm --scale 18`, or a larger one: the
//! universities past `base + rounds` are ignored); each triple belongs to the
//! university its subject IRI names. The split files and the databases live
//! in `--scratch` (default: a temp directory), removed at the end.
//!
//! One line per round and policy:
//! `ROUND layout=… policy=… round=… triples=… live=… referenced=…
//! limit=… free=… rss_mib=… kv_bytes=… delete_ms=… insert_ms=…`, where
//! `live` counts the dictionary's live terms, `referenced` the distinct
//! terms the data uses, `limit` the sum of the id spaces' high-water marks.
//! Then `SUMMARY` lines with the acceptance checks: live = referenced in
//! every round and within ±5% of round 0, limit ≤ 1.05 × peak live, RSS ≤
//! 1.15 × round 0 (memory), kvstore.kv ≤ 1.2 × round 0 (stream, materialize), and the
//! delete throughput ratio on / off.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use gstore::db::{IdReclaim, OpenOptions};
use gstore::parser::import::{ImportOptions, Source};
use gstore::{Database, TripleSource};

struct Args {
    data: PathBuf,
    layout: String,
    policies: Vec<IdReclaim>,
    base: usize,
    rounds: usize,
    scratch: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut a = Args {
        data: PathBuf::new(),
        layout: "memory".into(),
        policies: vec![IdReclaim::Immediate, IdReclaim::Off],
        base: 8,
        rounds: 10,
        scratch: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().unwrap_or_else(|| panic!("{k} needs a value"));
        match k.as_str() {
            "--data" => a.data = PathBuf::from(v()),
            "--layout" => a.layout = v(),
            "--policy" => {
                a.policies = match v().as_str() {
                    "on" => vec![IdReclaim::Immediate],
                    "off" => vec![IdReclaim::Off],
                    _ => vec![IdReclaim::Immediate, IdReclaim::Off],
                }
            }
            "--base" => a.base = v().parse().unwrap(),
            "--rounds" => a.rounds = v().parse().unwrap(),
            "--scratch" => a.scratch = Some(PathBuf::from(v())),
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(!a.data.as_os_str().is_empty(), "--data FILE is required");
    a
}

/// The university a triple belongs to: the number after `University` in its
/// subject IRI.
fn university(line: &str) -> Option<usize> {
    let subject = line.split_whitespace().next()?;
    let at = subject.find("University")? + "University".len();
    let digits: String = subject[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Split `data` into `dir/u{n}.nt` for the first `n` universities.
fn split(data: &Path, dir: &Path, n: usize) -> Vec<PathBuf> {
    let paths: Vec<PathBuf> = (0..n).map(|u| dir.join(format!("u{u}.nt"))).collect();
    if paths.iter().all(|p| p.is_file()) {
        return paths;
    }
    let mut outs: Vec<BufWriter<std::fs::File>> = paths
        .iter()
        .map(|p| BufWriter::new(std::fs::File::create(p).unwrap()))
        .collect();
    let input = BufReader::new(std::fs::File::open(data).unwrap());
    for line in input.lines() {
        let line = line.unwrap();
        if let Some(u) = university(&line).filter(|&u| u < n) {
            writeln!(outs[u], "{line}").unwrap();
        }
    }
    for mut o in outs {
        o.flush().unwrap();
    }
    paths
}

fn rss_mib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<f64>().ok())
        })
        .map_or(0.0, |kb| kb / 1024.0)
}

/// The distinct terms the default graph references (nodes + predicates).
fn referenced(db: &Database) -> usize {
    let b = db.backend();
    let mut nodes: HashSet<u32> = b.subject_keys().into_iter().collect();
    nodes.extend(b.object_keys());
    nodes.len() + b.predicates().len()
}

fn live(db: &Database) -> usize {
    db.entity_num() + db.literal_num() + db.predicate_num()
}

fn open(layout: &str, dir: &Path) -> Database {
    match layout {
        "stream" => Database::open_with(dir, &OpenOptions::stream()).unwrap(),
        "materialize" => Database::open_with(dir, &OpenOptions::materialize()).unwrap(),
        _ => Database::open(dir).unwrap(),
    }
}

fn build(layout: &str, dir: &Path, files: &[PathBuf]) -> Database {
    match layout {
        "memory" => Database::build_from_files("churn", files).unwrap(),
        "stream" | "materialize" => {
            Database::build_disk(dir, files).unwrap();
            open(layout, dir)
        }
        #[cfg(feature = "rocksdb")]
        "rocksdb" => {
            let sources: Vec<Source> = files.iter().cloned().map(Source::File).collect();
            gstore::bulk::build(
                dir,
                "churn",
                gstore::bulk::BulkInput::Sources(&sources),
                &ImportOptions::strict(),
                &mut (),
                gstore::bulk::BulkTarget::RocksDb,
                &gstore::bulk::BulkOptions::default(),
            )
            .unwrap();
            open(layout, dir)
        }
        other => panic!(
            "unknown layout {other} (memory, stream, materialize, rocksdb with --features rocksdb)"
        ),
    }
}

#[derive(Default)]
struct Totals {
    live0: usize,
    max_dev: f64,
    exact: bool,
    peak_live: usize,
    max_limit: usize,
    rss0: f64,
    max_rss: f64,
    kv0: u64,
    max_kv: u64,
    delete_ms: f64,
    deleted: u64,
}

fn run(args: &Args, files: &[PathBuf], scratch: &Path, policy: IdReclaim) -> Totals {
    let dir = scratch.join(format!("db-{}-{policy}", args.layout));
    let _ = std::fs::remove_dir_all(&dir);
    // A new on-disk database keeps a free-id list when the policy is on.
    std::env::set_var(gstore::db::ENV_ID_RECLAIM, policy.as_str());
    let mut db = build(&args.layout, &dir, &files[..args.base]);
    db.set_id_reclaim(policy);
    let kv = dir.join("kvstore.kv");
    let mut t = Totals {
        exact: true,
        ..Totals::default()
    };
    for round in 0..=args.rounds {
        let (mut del_ms, mut ins_ms) = (0.0, 0.0);
        if round > 0 {
            let r = round - 1;
            let mut applied = 0u64;
            let t0 = Instant::now();
            db.remove_sources(
                &[Source::File(files[r].clone())],
                &ImportOptions::strict().for_removal(),
                &mut (),
                &mut applied,
            )
            .unwrap();
            db.sync().unwrap();
            del_ms = t0.elapsed().as_secs_f64() * 1e3;
            t.delete_ms += del_ms;
            t.deleted += applied;
            let t0 = Instant::now();
            db.insert_sources(
                &[Source::File(files[args.base + r].clone())],
                &ImportOptions::strict(),
                &mut (),
                &mut applied,
            )
            .unwrap();
            db.sync().unwrap();
            ins_ms = t0.elapsed().as_secs_f64() * 1e3;
        }
        let l = live(&db);
        let refs = referenced(&db);
        let stats = db.id_space_stats();
        let limit = stats.limits.iter().sum::<u64>() as usize;
        let free = stats.free.iter().sum::<u64>();
        let rss = rss_mib();
        let kv_bytes = std::fs::metadata(&kv).map_or(0, |m| m.len());
        if round == 0 {
            t.live0 = l;
            t.rss0 = rss;
            t.kv0 = kv_bytes;
        }
        t.max_dev = t.max_dev.max((l as f64 / t.live0 as f64 - 1.0).abs());
        t.exact &= l == refs;
        t.peak_live = t.peak_live.max(l);
        t.max_limit = t.max_limit.max(limit);
        t.max_rss = t.max_rss.max(rss);
        t.max_kv = t.max_kv.max(kv_bytes);
        println!(
            "ROUND layout={} policy={policy} round={round} triples={} live={l} \
             referenced={refs} limit={limit} free={free} rss_mib={rss:.1} kv_bytes={kv_bytes} \
             delete_ms={del_ms:.1} insert_ms={ins_ms:.1}",
            args.layout,
            db.triple_num()
        );
    }
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    t
}

fn main() {
    let args = parse_args();
    let tmp;
    let scratch = match &args.scratch {
        Some(s) => s.clone(),
        None => {
            tmp = std::env::temp_dir().join(format!("reclaim_churn_{}", std::process::id()));
            tmp.clone()
        }
    };
    std::fs::create_dir_all(&scratch).unwrap();
    let files = split(&args.data, &scratch, args.base + args.rounds);
    let mut results = Vec::new();
    for &policy in &args.policies {
        results.push((policy, run(&args, &files, &scratch, policy)));
    }
    for (policy, t) in &results {
        println!(
            "SUMMARY layout={} policy={policy} live_eq_referenced={} max_live_dev={:.3} \
             limit_over_peak_live={:.3} rss_ratio={:.3} kv_ratio={:.3} delete_ms_total={:.1} \
             deleted={}",
            args.layout,
            t.exact,
            t.max_dev,
            t.max_limit as f64 / t.peak_live as f64,
            if t.rss0 > 0.0 {
                t.max_rss / t.rss0
            } else {
                0.0
            },
            if t.kv0 > 0 {
                t.max_kv as f64 / t.kv0 as f64
            } else {
                0.0
            },
            t.delete_ms,
            t.deleted
        );
    }
    if let (Some((_, on)), Some((_, off))) = (
        results.iter().find(|(p, _)| *p == IdReclaim::Immediate),
        results.iter().find(|(p, _)| *p == IdReclaim::Off),
    ) {
        println!(
            "SUMMARY layout={} delete_time_ratio_on_over_off={:.3}",
            args.layout,
            on.delete_ms / off.delete_ms
        );
    }
    if args.scratch.is_none() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
