//! The WatDiv-like workload: determinism and the golden hash of scale 1,
//! the composition (each class's triples within ±10% of the design), the
//! 20 templates (each parses, binds by the ASK walk and answers non-empty),
//! and equal answers (row hashes) on the snapshot and the disk layouts.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use gstore::{Database, QueryResult};
use gstore_bench::hash::{fnv64, hex, rows_fnv64};
use gstore_bench::runner::phases::result_rows;
use gstore_bench::watdiv::{self, WatDiv, CLASSES, TEMPLATES, VERSION};
use gstore_bench::workload::{generate_to, BoundQuery, GenStats, Workload, WorkloadSpec};

/// A scratch directory removed on drop (also when a test panics).
struct Tmp(PathBuf);

impl Tmp {
    fn new(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("gsb-watdiv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Tmp(p)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn gen(w: &dyn Workload, chunk: bool) -> (GenStats, Vec<u8>) {
    let mut v = Vec::new();
    let s = generate_to(w, &mut v, chunk).unwrap();
    (s, v)
}

fn scale(scale: u32, shrink: Option<u32>) -> WatDiv {
    WatDiv {
        scale,
        seed: 0,
        shrink,
    }
}

#[test]
fn watdiv_scale1_seed0_is_pinned_and_deterministic() {
    let w = scale(1, None);
    assert_eq!(w.version(), VERSION);
    let (s1, data) = gen(&w, false);
    let (s2, again) = gen(&w, false);
    assert_eq!(s1, s2, "deterministic");
    assert_eq!(data, again);
    assert_eq!(hex(s1.fnv64), "70c039a88063c158");
    assert_eq!(
        (s1.lines, s1.unique_triples, s1.bytes),
        (105_620, 105_620, 14_399_328)
    );
    // Every line is a distinct triple.
    let text = std::str::from_utf8(&data).unwrap();
    let distinct: HashSet<&str> = text.lines().collect();
    assert_eq!(distinct.len() as u64, s1.unique_triples);
    // Another seed gives other data.
    let other = WatDiv {
        seed: 1,
        ..w.clone()
    };
    assert_ne!(gen(&other, false).0.fnv64, s1.fnv64);
    // The fixed constants of the templates exist.
    for c in [
        "Product0>",
        "Role2>",
        "Country1>",
        "Country5>",
        "Language0>",
        "ProductCategory2>",
    ] {
        assert!(
            text.contains(&format!("<{}{c}", watdiv::WSDBM)),
            "{c} is missing"
        );
    }

    // The chunk: new users only, disjoint from the data, no duplicates.
    let (c, chunk) = gen(&w, true);
    assert_eq!(hex(c.fnv64), "557df2a2e1aa2561");
    assert_eq!((c.lines, c.unique_triples), (7_426, 7_426));
    let chunk_text = std::str::from_utf8(&chunk).unwrap();
    let chunk_lines: HashSet<&str> = chunk_text.lines().collect();
    assert_eq!(chunk_lines.len() as u64, c.unique_triples);
    assert!(chunk_lines.is_disjoint(&distinct));
    let first_new = w.count(watdiv::USER);
    for l in &chunk_lines {
        let subject = l.split(' ').next().unwrap();
        let n: u64 = subject
            .strip_prefix(&format!("<{}User", watdiv::WSDBM))
            .and_then(|r| r.strip_suffix('>'))
            .and_then(|r| r.parse().ok())
            .unwrap_or_else(|| panic!("chunk subject {subject} is not a user"));
        assert!(n >= first_new, "{subject} is a data-set user");
    }
}

/// The class of a subject IRI (the longest class name prefix followed by
/// digits only).
fn class_of(subject: &str) -> Option<usize> {
    let local = subject
        .strip_prefix(&format!("<{}", watdiv::WSDBM))?
        .strip_suffix('>')?;
    CLASSES
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            local
                .strip_prefix(c.name)
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .max_by_key(|(_, c)| c.name.len())
        .map(|(i, _)| i)
}

#[test]
fn composition_is_within_ten_percent_of_the_design() {
    for s in [1, 2] {
        let w = scale(s, None);
        let (st, data) = gen(&w, false);
        let text = std::str::from_utf8(&data).unwrap();
        let mut triples = [0u64; CLASSES.len()];
        let mut subjects: Vec<HashSet<&str>> = vec![HashSet::new(); CLASSES.len()];
        for l in text.lines() {
            let subject = l.split(' ').next().unwrap();
            let c = class_of(subject).unwrap_or_else(|| panic!("unclassified subject {subject}"));
            triples[c] += 1;
            subjects[c].insert(subject);
        }
        let mut expected_total = 0.0;
        for (c, class) in CLASSES.iter().enumerate() {
            let want = w.expected_triples(c);
            expected_total += want;
            let got = triples[c] as f64;
            assert!(
                (got - want).abs() <= 0.10 * want,
                "scale {s}: {} has {got} triples, the design {want:.0} (±10%)",
                class.name
            );
            assert!(
                subjects[c].len() as u64 <= w.count(c),
                "scale {s}: more {} subjects than entities",
                class.name
            );
        }
        let total = st.unique_triples as f64;
        assert!(
            (total - expected_total).abs() <= 0.10 * expected_total,
            "scale {s}: {total} triples, the design {expected_total:.0}"
        );
        // About 100K triples per scale unit, 70% of them social edges.
        assert!((90_000.0 * s as f64..120_000.0 * s as f64).contains(&total));
        let social = text
            .lines()
            .filter(|l| l.contains("/wsdbm/friendOf>") || l.contains("/wsdbm/follows>"))
            .count() as f64;
        assert!((0.6..0.8).contains(&(social / total)), "{}", social / total);
        // The scalable classes grow with the scale, the others do not.
        assert_eq!(w.count(watdiv::USER), 1000 * u64::from(s));
        assert_eq!(w.count(watdiv::COUNTRY), 25);
    }
}

fn build(dir: &Path, data: &[u8]) -> (PathBuf, Database) {
    let f = dir.join("data.nt");
    std::fs::write(&f, data).unwrap();
    let mut db = Database::build_from_files("watdiv", &[&f]).unwrap();
    db.set_query_cache_size(0);
    (f, db)
}

fn answer(db: &mut Database, q: &str) -> (u64, String) {
    let rows = result_rows(db.query(q).unwrap_or_else(|e| panic!("{e}\n{q}"))).unwrap();
    (rows.len() as u64, hex(rows_fnv64(&rows)))
}

fn bind(w: &WatDiv, db: &mut Database) -> Vec<BoundQuery> {
    w.instantiate(&mut |q: &str| match db.query(q) {
        Ok(QueryResult::Ask(b)) => Ok(b),
        other => Err(format!("{other:?}")),
    })
    .unwrap()
}

#[test]
fn every_template_binds_parses_and_answers_at_scale_1() {
    assert_eq!(TEMPLATES.len(), 20);
    let t = Tmp::new("scale1");
    let w = scale(1, None);
    let (_, data) = gen(&w, false);
    let (_, mut db) = build(&t.0, &data);
    let bound = bind(&w, &mut db);
    let names: Vec<&str> = bound.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "L1", "L2", "L3", "L4", "L5", "S1", "S2", "S3", "S4", "S5", "S6", "S7", "F1", "F2",
            "F3", "F4", "F5", "C1", "C2", "C3"
        ]
    );
    let mut all = String::new();
    for b in &bound {
        assert!(b.tries >= 1 && b.tries <= watdiv::MAX_TRIES, "{b:?}");
        assert!(!b.text.contains('%'), "{}: unbound placeholder", b.name);
        assert!(
            gstore::parser::sparql::parse(&b.text).is_ok(),
            "{} does not parse:\n{}",
            b.name,
            b.text
        );
        let (rows, _) = answer(&mut db, &b.text);
        assert!(rows > 0, "{} is empty:\n{}", b.name, b.text);
        all.push_str(&b.text);
    }
    // The walk is deterministic: the bound texts are pinned.
    assert_eq!(hex(fnv64(all.as_bytes())), GOLDEN_BOUND_SCALE1, "{all}");
    // The same data and seed bind the same way again.
    assert_eq!(bind(&w, &mut db), bound);
}

const GOLDEN_BOUND_SCALE1: &str = "3c59ad915698caa6";

#[test]
fn snapshot_and_disk_answer_alike() {
    let t = Tmp::new("layouts");
    let w = scale(1, Some(10));
    let (_, data) = gen(&w, false);
    let (file, mut mem) = build(&t.0, &data);
    let bound = bind(&w, &mut mem);
    let disk_dir = t.0.join("watdiv.db");
    Database::build_disk(&disk_dir, &[&file]).unwrap();
    let mut disk = Database::open(&disk_dir).unwrap();
    disk.set_query_cache_size(0);
    assert_eq!(disk.triple_num(), mem.triple_num());
    let mut answers = BTreeMap::new();
    for b in &bound {
        let m = answer(&mut mem, &b.text);
        let d = answer(&mut disk, &b.text);
        assert!(m.0 > 0, "{} is empty", b.name);
        assert_eq!(m, d, "{}: snapshot and disk differ", b.name);
        answers.insert(b.name.clone(), m.0);
    }
    assert_eq!(answers.len(), 20);
}

#[test]
fn specs_select_the_watdiv_workload() {
    let spec = WorkloadSpec::watdiv(10, 0);
    let w = spec.instantiate().unwrap();
    assert_eq!(w.id(), "watdiv-like");
    assert_eq!(w.file_stem(), "watdiv-like1-x10-s0");
    assert!(w.needs_binding());
    assert_eq!(w.queries().len(), 20);
    assert_eq!(spec.label(), "watdiv-like scale 10");
    let shrunk = WorkloadSpec {
        shrink: Some(10),
        ..WorkloadSpec::watdiv(1, 0)
    };
    assert_eq!(
        shrunk.instantiate().unwrap().file_stem(),
        "watdiv-like1-x1-s0-k10"
    );
    // A spec without `shrink` serializes as before (no new field).
    let json = serde_json::to_string(&WorkloadSpec::lubm(8, 0)).unwrap();
    assert!(!json.contains("shrink"), "{json}");
    assert!(WorkloadSpec::watdiv(0, 0).instantiate().is_err());
    assert_eq!(
        WorkloadSpec {
            kind: "watdiv-like".into(),
            ..WorkloadSpec::watdiv(1, 0)
        }
        .family(),
        "watdiv"
    );
}
