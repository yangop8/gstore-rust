//! The in-process phases: open, query and update, each run inside a child
//! process through the public `gstore` API.
//!
//! Every query execution carries a `# gstore-bench run k` comment prefix and
//! the query cache is off (`set_query_cache_size(0)`), so each run evaluates
//! the query.

use std::path::Path;
use std::time::Instant;

use gstore::{Database, QueryResult};
use serde::{Deserialize, Serialize};

use crate::hash::{hex, rows_fnv64};
use crate::schema::{MemInfo, QueryMeasure, UpdateResult};
use crate::sys;
use crate::workload::{QuerySpec, Workload};

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// The median of `v` (the mean of the two middle values for an even count).
pub fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// The `p`-th percentile of `v` (nearest rank).
pub fn percentile(v: &[f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * s.len() as f64).ceil() as usize;
    s[rank.clamp(1, s.len()) - 1]
}

/// One fresh-process open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OpenSample {
    pub open_ms: f64,
    pub cold: bool,
    pub rss_mib: Option<f64>,
    pub hwm_mib: Option<f64>,
    pub triple_num: u64,
    pub entity_num: u64,
    pub literal_num: u64,
    pub predicate_num: u64,
}

fn open_db(dir: &Path) -> Result<(Database, f64), String> {
    let t = Instant::now();
    let db = Database::open(dir).map_err(|e| format!("open {}: {e}", dir.display()))?;
    Ok((db, ms(t)))
}

/// Open the database once (after dropping the page cache when asked).
pub fn open_phase(dir: &Path, drop_caches: bool) -> Result<OpenSample, String> {
    let cold = drop_caches && sys::drop_page_cache();
    let (db, open_ms) = open_db(dir)?;
    Ok(OpenSample {
        open_ms,
        cold,
        rss_mib: sys::rss_mib(),
        hwm_mib: sys::hwm_mib(),
        triple_num: db.triple_num(),
        entity_num: db.entity_num() as u64,
        literal_num: db.literal_num() as u64,
        predicate_num: db.predicate_num() as u64,
    })
}

/// A query result as rows of optional strings (ASK: one cell; CONSTRUCT:
/// subject, predicate, object).
pub fn result_rows(r: QueryResult) -> Result<Vec<Vec<Option<String>>>, String> {
    match r {
        QueryResult::Select(rs) => Ok(rs.rows),
        QueryResult::Ask(b) => Ok(vec![vec![Some(b.to_string())]]),
        QueryResult::Construct(ts) => Ok(ts
            .iter()
            .map(|t| {
                vec![
                    Some(t.subject.to_string()),
                    Some(t.predicate.to_string()),
                    Some(t.object.to_string()),
                ]
            })
            .collect()),
        QueryResult::Update { .. } => Err("an update where a read query was expected".into()),
    }
}

/// Run `q` once, returning (ms, rows, rows hash).
pub fn run_query(db: &mut Database, q: &QuerySpec, tag: &str) -> Result<(f64, u64, u64), String> {
    let text = format!("# gstore-bench {tag}\n{}", q.text);
    let t = Instant::now();
    let r = db.query(&text).map_err(|e| format!("{}: {e}", q.name))?;
    let el = ms(t);
    let rows = result_rows(r)?;
    Ok((el, rows.len() as u64, rows_fnv64(&rows)))
}

/// The query phase's output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct QueryPhase {
    pub open: OpenSample,
    pub queries: Vec<QueryMeasure>,
    pub mem: MemInfo,
}

/// Open, then run every query once plus `reps` warm times.
pub fn query_phase(dir: &Path, w: &dyn Workload, reps: usize) -> Result<QueryPhase, String> {
    let (mut db, open_ms) = open_db(dir)?;
    let open = OpenSample {
        open_ms,
        cold: false,
        rss_mib: sys::rss_mib(),
        hwm_mib: sys::hwm_mib(),
        triple_num: db.triple_num(),
        entity_num: db.entity_num() as u64,
        literal_num: db.literal_num() as u64,
        predicate_num: db.predicate_num() as u64,
    };
    db.set_query_cache_size(0);
    let mut queries = Vec::new();
    for q in w.queries() {
        let mut first = (0.0, 0, 0);
        let mut runs = Vec::new();
        let mut stable = true;
        for k in 0..=reps {
            let (el, rows, h) = run_query(&mut db, &q, &format!("run {k}"))?;
            if k == 0 {
                first = (el, rows, h);
            } else {
                runs.push(el);
                stable &= rows == first.1 && h == first.2;
            }
        }
        queries.push(QueryMeasure {
            name: q.name.clone(),
            rows: first.1,
            rows_fnv64: hex(first.2),
            first_ms: first.0,
            median_ms: if runs.is_empty() {
                first.0
            } else {
                median(&runs)
            },
            runs_ms: runs,
            stable,
        });
    }
    Ok(QueryPhase {
        open,
        queries,
        mem: MemInfo {
            rss_mib: sys::rss_mib(),
            hwm_mib: sys::hwm_mib(),
        },
    })
}

/// Expected answers (name, rows, rows hash) for the update phase's check.
pub type Expected = Vec<(String, u64, String)>;

fn update_ok(r: QueryResult) -> Result<usize, String> {
    match r {
        QueryResult::Update { changed } => Ok(changed),
        _ => Err("an update returned a read result".into()),
    }
}

/// The update phase: `singles` single-triple inserts and their deletes, the
/// chunk inserted and deleted, the answers re-checked, a checkpoint and a
/// reopen.
pub fn update_phase(
    dir: &Path,
    w: &dyn Workload,
    singles: usize,
    chunk: &Path,
    chunk_triples: u64,
    expected: &Expected,
) -> Result<UpdateResult, String> {
    let (mut db, _) = open_db(dir)?;
    db.set_query_cache_size(0);
    let original = db.triple_num();
    let entities_before = db.entity_num() as u64;
    let literals_before = db.literal_num() as u64;

    let mut singles_ok = true;
    let mut ins = Vec::with_capacity(singles);
    for i in 0..singles {
        let before = db.triple_num();
        let t = Instant::now();
        let r = db
            .query(&w.single_insert(i))
            .map_err(|e| format!("single insert {i}: {e}"))?;
        ins.push(ms(t));
        update_ok(r)?;
        singles_ok &= db.triple_num() == before + 1;
    }
    let mut del = Vec::with_capacity(singles);
    for i in 0..singles {
        let before = db.triple_num();
        let t = Instant::now();
        let r = db
            .query(&w.single_delete(i))
            .map_err(|e| format!("single delete {i}: {e}"))?;
        del.push(ms(t));
        update_ok(r)?;
        singles_ok &= db.triple_num() + 1 == before;
    }
    singles_ok &= db.triple_num() == original;

    let body = std::fs::read_to_string(chunk).map_err(|e| format!("{}: {e}", chunk.display()))?;
    let t = Instant::now();
    let r = db
        .query(&format!("INSERT DATA {{\n{body}}}"))
        .map_err(|e| format!("chunk insert: {e}"))?;
    let chunk_insert_ms = ms(t);
    update_ok(r)?;
    let grown = db.triple_num();
    let t = Instant::now();
    let r = db
        .query(&format!("DELETE DATA {{\n{body}}}"))
        .map_err(|e| format!("chunk delete: {e}"))?;
    let chunk_delete_ms = ms(t);
    update_ok(r)?;
    let shrunk = db.triple_num();
    let chunk_delta_ok = grown == original + chunk_triples && shrunk == original;

    let mut mismatched = Vec::new();
    for q in w.queries() {
        let (_, rows, h) = run_query(&mut db, &q, "update check")?;
        match expected.iter().find(|(n, _, _)| *n == q.name) {
            Some((_, r0, h0)) if *r0 == rows && *h0 == hex(h) => {}
            Some(_) => mismatched.push(q.name.clone()),
            None => {}
        }
    }
    let entities_after = db.entity_num() as u64;
    let literals_after = db.literal_num() as u64;
    let t = Instant::now();
    db.checkpoint().map_err(|e| format!("checkpoint: {e}"))?;
    let checkpoint_ms = ms(t);
    drop(db);
    let (db, reopen_ms) = open_db(dir)?;
    let reopen_triple_num = db.triple_num();
    Ok(UpdateResult {
        singles,
        single_insert_median_ms: (!ins.is_empty()).then(|| median(&ins)),
        single_insert_p95_ms: (!ins.is_empty()).then(|| percentile(&ins, 95.0)),
        single_delete_median_ms: (!del.is_empty()).then(|| median(&del)),
        single_delete_p95_ms: (!del.is_empty()).then(|| percentile(&del, 95.0)),
        singles_ok,
        chunk_triples,
        chunk_insert_ms,
        chunk_delete_ms,
        chunk_delta_ok,
        answers_unchanged: mismatched.is_empty(),
        mismatched,
        checkpoint_ms,
        reopen_ms,
        reopen_triple_num,
        reopen_ok: reopen_triple_num == original,
        entities_before,
        entities_after,
        literals_before,
        literals_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_percentile() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 95.0), 95.0);
        assert_eq!(percentile(&[7.0], 95.0), 7.0);
    }
}
