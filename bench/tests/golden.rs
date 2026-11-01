//! Golden values: the generator stream, the hashes, the LUBM-like data and
//! queries. A change to any of these changes every baseline, so it needs a
//! new generator version (`lubm::VERSION`) and re-blessed baselines.

use std::collections::HashSet;
use std::io::{self, Write};

use gstore::{Database, QueryResult};
use gstore_bench::hash::{fnv64, hex, rows_fnv64, Fnv64};
use gstore_bench::lubm::{Lubm, QUERIES, QUERY_TEXT_FNV64, VERSION};
use gstore_bench::rng::{stream_seed, SplitMix64};
use gstore_bench::workload::{generate_to, GenStats, Workload, WorkloadSpec};

/// A sink that only hashes.
struct Sink(Fnv64, u64);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.update(b);
        self.1 += b.len() as u64;
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn gen(w: &dyn Workload, chunk: bool) -> (GenStats, Vec<u8>) {
    let mut v = Vec::new();
    let s = generate_to(w, &mut v, chunk).unwrap();
    (s, v)
}

#[test]
fn splitmix_golden_sequences() {
    let mut r = SplitMix64::new(0);
    let v: Vec<u64> = (0..4).map(|_| r.next_u64()).collect();
    assert_eq!(
        v,
        [0xe220a8397b1dcdaf, 0x6e789e6aa1b965f4, 0x06c45d188009454f, 0xf88bb8a8724c81ec]
    );
    let mut r = SplitMix64::new(42);
    let below: Vec<u64> = (0..8).map(|_| r.below(10)).collect();
    assert_eq!(below, GOLDEN_BELOW);
    let sample = SplitMix64::new(7).sample(100, 5);
    assert_eq!(sample, GOLDEN_SAMPLE);
    let big_sample = SplitMix64::new(7).sample(6, 5);
    assert_eq!(big_sample, GOLDEN_SAMPLE_DENSE);
    let mut items: Vec<u32> = (0..8).collect();
    SplitMix64::new(9).shuffle(&mut items);
    assert_eq!(items, GOLDEN_SHUFFLE);
    assert_eq!(stream_seed(0, 1, 2, 3), GOLDEN_STREAM_SEED);
}

const GOLDEN_BELOW: [u64; 8] = [7, 1, 2, 3, 0, 8, 2, 8];
const GOLDEN_SAMPLE: [usize; 5] = [38, 1, 90, 58, 45];
const GOLDEN_SAMPLE_DENSE: [usize; 5] = [2, 1, 5, 4, 3];
const GOLDEN_SHUFFLE: [u32; 8] = [2, 4, 0, 6, 3, 1, 7, 5];
const GOLDEN_STREAM_SEED: u64 = 0x9b04_d6d1_7c2a_2f71;

#[test]
fn hash_golden_values() {
    assert_eq!(fnv64(b"gstore"), GOLDEN_FNV_GSTORE);
    let rows = vec![
        vec![Some("<http://a>".to_string()), None],
        vec![Some("\"x\"".to_string()), Some(String::new())],
    ];
    assert_eq!(hex(rows_fnv64(&rows)), GOLDEN_ROWS);
    assert_eq!(hex(rows_fnv64(&[])), GOLDEN_ROWS_EMPTY);
}

const GOLDEN_FNV_GSTORE: u64 = 0xdb99_b0ce_3e88_42d7;
const GOLDEN_ROWS: &str = "8e33c45ec67dd433";
const GOLDEN_ROWS_EMPTY: &str = "f52a15e9a9b5e89b";

#[test]
fn lubm_univ1_seed0_is_pinned_and_deterministic() {
    let w = Lubm {
        univ: 1,
        seed: 0,
        departments: None,
    };
    assert_eq!(w.version(), VERSION);
    let (s1, data) = gen(&w, false);
    let mut sink = Sink(Fnv64::new(), 0);
    let s2 = generate_to(&w, &mut sink, false).unwrap();
    assert_eq!(s1, s2, "deterministic");
    assert_eq!(sink.0.finish(), s1.fnv64);
    assert_eq!(hex(s1.fnv64), "b45d2e4d786f1a33");
    assert_eq!((s1.lines, s1.unique_triples, s1.bytes), (143_930, 139_905, 25_325_755));
    // unique_triples counts exactly the distinct lines.
    let text = std::str::from_utf8(&data).unwrap();
    let distinct: HashSet<&str> = text.lines().collect();
    assert_eq!(distinct.len() as u64, s1.unique_triples);
    // Within ±5% of the prototype's 143K triples at univ 1.
    assert!((136_000..=150_600).contains(&s1.unique_triples));
    // Another seed gives other data.
    let other = Lubm { seed: 1, ..w.clone() };
    assert_ne!(gen(&other, false).0.fnv64, s1.fnv64);
}

#[test]
fn department_counts_follow_the_prototype_scale_points() {
    let w = |univ| Lubm {
        univ,
        seed: 0,
        departments: None,
    };
    assert_eq!(w(1).departments_of(0), 21);
    assert_eq!((0..8).map(|u| w(8).departments_of(u)).sum::<u64>(), 156);
    assert_eq!((0..38).map(|u| w(38).departments_of(u)).sum::<u64>(), 779);
    let capped = Lubm {
        departments: Some(2),
        ..w(3)
    };
    assert!((0..3).all(|u| capped.departments_of(u) <= 2));
}

#[test]
fn micro_data_and_chunk_are_pinned_and_disjoint() {
    let w = Lubm {
        univ: 1,
        seed: 0,
        departments: Some(1),
    };
    let (s, data) = gen(&w, false);
    assert_eq!(hex(s.fnv64), "d5f50a8e697fc892");
    assert_eq!((s.lines, s.unique_triples), (7_521, 7_491));
    let (c, chunk) = gen(&w, true);
    assert_eq!(c.lines, c.unique_triples, "the chunk has no duplicate lines");
    let data: HashSet<&str> = std::str::from_utf8(&data).unwrap().lines().collect();
    let chunk_text = std::str::from_utf8(&chunk).unwrap();
    let chunk_lines: HashSet<&str> = chunk_text.lines().collect();
    assert_eq!(chunk_lines.len() as u64, c.unique_triples);
    assert!(chunk_lines.is_disjoint(&data), "the chunk is disjoint from the data");
    for l in &chunk_lines {
        assert!(l.starts_with("<http://www.Department0.University1.edu"), "{l}");
    }
}

#[test]
fn query_texts_are_pinned() {
    assert_eq!(QUERIES.len(), 14);
    for ((name, text), (pname, h)) in QUERIES.iter().zip(QUERY_TEXT_FNV64.iter()) {
        assert_eq!(name, pname);
        assert_eq!(fnv64(text.as_bytes()), *h, "{name}: testdata/lubm/lubm_{name}.rq changed");
    }
}

fn rows(db: &mut Database, text: &str) -> usize {
    match db.query(text).unwrap() {
        QueryResult::Select(rs) => rs.rows.len(),
        other => panic!("{other:?}"),
    }
}

fn build(w: &dyn Workload) -> Database {
    let (_, data) = gen(w, false);
    let dir = std::env::temp_dir().join(format!("gsb-golden-{}-{}", w.file_stem(), std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("data.nt");
    std::fs::write(&f, &data).unwrap();
    let db = Database::build_from_files("golden", &[&f]).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    db
}

#[test]
fn all_queries_answer_at_univ1_and_micro_counts_are_pinned() {
    let full = Lubm {
        univ: 1,
        seed: 0,
        departments: None,
    };
    let mut db = build(&full);
    assert_eq!(db.triple_num(), 139_905, "triple_num == unique_triples");
    for (name, text) in QUERIES {
        assert!(rows(&mut db, text) > 0, "{name} is empty at univ 1");
    }
    let micro = Lubm {
        departments: Some(1),
        ..full
    };
    let mut db = build(&micro);
    assert_eq!(db.triple_num(), 7_491);
    let counts: Vec<usize> = QUERIES.iter().map(|(_, t)| rows(&mut db, t)).collect();
    assert_eq!(counts, [3, 1, 10, 9, 576, 576, 23, 576, 5, 3, 14, 1, 613, 432]);
}

#[test]
fn workload_specs_round_trip() {
    let spec = WorkloadSpec::lubm(8, 0);
    let w = spec.instantiate().unwrap();
    assert_eq!(w.file_stem(), "lubm-like1-u8-s0");
    let json = serde_json::to_string(&spec).unwrap();
    assert_eq!(serde_json::from_str::<WorkloadSpec>(&json).unwrap(), spec);
    assert!(WorkloadSpec::lubm(0, 0).instantiate().is_err());
    assert!(WorkloadSpec {
        kind: "nope".into(),
        ..spec
    }
    .instantiate()
    .is_err());
    let t = WorkloadSpec::tiny().instantiate().unwrap();
    assert_eq!(t.queries().len(), 5);
}
