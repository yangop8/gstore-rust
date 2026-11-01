//! The gates `gstore-bench-gates/1` (`bench/thresholds.json`) and their
//! evaluation.
//!
//! * **Exact gates** (never retried): the generator version and data hash, the
//!   query text hashes, `triple_num == unique_triples`, every query's rows and
//!   row hash equal to the baseline and across backends, stable across runs and
//!   non-empty, the update round trip, identical concurrent response bodies.
//! * **Timing gates** (one-sided, calibration-normalized): per query
//!   `(m+floor)/(b+floor) > 3.0` fails, the per-backend geomean of the raw
//!   medians `> 1.5` fails, build `> 2.0` (`> 2.5` for disk), warm open `> 2.5`.
//! * **Memory**: `(m+32 MiB)/(b+32 MiB) > 1.5` fails (build peak, after open,
//!   query high-water mark). **Size**: directory bytes `> 1.25` fails.
//!
//! [`self_check`] runs the exact gates that need no baseline; `run` applies it
//! to every result.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::hash::hex;
use crate::schema::{BackendRun, BenchResult, SCHEMA};

pub const GATES_SCHEMA: &str = "gstore-bench-gates/1";

/// The built-in thresholds: `bench/thresholds.json` as compiled in.
pub const DEFAULT_THRESHOLDS: &str = include_str!("../thresholds.json");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ratio {
    #[serde(default)]
    pub floor_ms: f64,
    pub max_ratio: f64,
    /// Build only: the disk layout's limit.
    #[serde(default)]
    pub max_ratio_disk: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeomeanGate {
    pub max_ratio: f64,
    #[serde(default = "default_eps")]
    pub epsilon_ms: f64,
}

fn default_eps() -> f64 {
    0.001
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RssGate {
    pub floor_mib: f64,
    pub max_ratio: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SizeGate {
    pub max_ratio: f64,
}

/// `bench/thresholds.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    pub schema: String,
    #[serde(default)]
    pub about: Option<String>,
    pub normalize_by_calibration: bool,
    pub calibration_clamp: [f64; 2],
    pub query: Ratio,
    pub geomean: GeomeanGate,
    pub build: Ratio,
    pub open: Ratio,
    pub rss: RssGate,
    pub disk_bytes: SizeGate,
    pub retries: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds::parse(DEFAULT_THRESHOLDS).expect("bench/thresholds.json is valid")
    }
}

impl Thresholds {
    pub fn parse(text: &str) -> Result<Thresholds, String> {
        let t: Thresholds =
            serde_json::from_str(text).map_err(|e| format!("invalid thresholds: {e}"))?;
        if t.schema != GATES_SCHEMA {
            return Err(format!(
                "thresholds schema '{}' is not {GATES_SCHEMA}",
                t.schema
            ));
        }
        Ok(t)
    }

    pub fn load(path: &std::path::Path) -> Result<Thresholds, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Thresholds::parse(&text)
    }
}

/// The class of a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum GateClass {
    Exact,
    Timing,
    Memory,
    Size,
    /// Report-only figures.
    Info,
}

/// The phase a timing or memory failure re-runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Build,
    Open,
    Query,
}

/// One evaluated gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateOutcome {
    /// What was checked (`exact.rows`, `time.query`, `rss.open`, …).
    pub gate: String,
    /// Where (`snapshot/q4`, `disk`, `workload`).
    pub subject: String,
    pub class: GateClass,
    #[serde(default)]
    pub base: Option<f64>,
    #[serde(default)]
    pub measured: Option<f64>,
    #[serde(default)]
    pub ratio: Option<f64>,
    #[serde(default)]
    pub limit: Option<f64>,
    pub pass: bool,
    #[serde(default)]
    pub detail: String,
    /// The backend and phase a retry re-runs (timing and memory gates).
    #[serde(default)]
    pub retry: Option<(String, Phase)>,
}

impl GateOutcome {
    fn exact(
        gate: &str,
        subject: impl Into<String>,
        pass: bool,
        detail: impl Into<String>,
    ) -> Self {
        GateOutcome {
            gate: gate.into(),
            subject: subject.into(),
            class: GateClass::Exact,
            base: None,
            measured: None,
            ratio: None,
            limit: None,
            pass,
            detail: detail.into(),
            retry: None,
        }
    }
}

/// The verdict over a set of outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    /// Only timing / memory / size gates failed.
    PerfFail,
    /// An exact gate failed.
    ExactFail,
}

pub fn verdict(outcomes: &[GateOutcome]) -> Verdict {
    if outcomes
        .iter()
        .any(|o| !o.pass && o.class == GateClass::Exact)
    {
        Verdict::ExactFail
    } else if outcomes.iter().any(|o| !o.pass) {
        Verdict::PerfFail
    } else {
        Verdict::Pass
    }
}

/// The (backend, phase) pairs failing timing or memory gates name.
pub fn retry_targets(outcomes: &[GateOutcome]) -> Vec<(String, Phase)> {
    let mut v: Vec<(String, Phase)> = outcomes
        .iter()
        .filter(|o| !o.pass)
        .filter_map(|o| o.retry.clone())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// The exact gates of one result that need no baseline.
pub fn self_check(r: &BenchResult) -> Vec<GateOutcome> {
    let mut out = Vec::new();
    out.push(GateOutcome::exact(
        "exact.schema",
        "result",
        r.schema == SCHEMA,
        format!("schema {}", r.schema),
    ));
    // The tool's pinned query texts (LUBM).
    if r.workload.id == "lubm-like" {
        for q in &r.workload.queries {
            let pinned = crate::lubm::QUERY_TEXT_FNV64
                .iter()
                .find(|(n, _)| *n == q.name)
                .map(|(_, h)| hex(*h));
            out.push(GateOutcome::exact(
                "exact.query_text",
                format!("workload/{}", q.name),
                pinned.as_deref() == Some(q.text_fnv64.as_str()),
                format!(
                    "text fnv {} (pinned {})",
                    q.text_fnv64,
                    pinned.unwrap_or_default()
                ),
            ));
        }
    }
    let mut by_query: BTreeMap<&str, Vec<(&str, u64, &str)>> = BTreeMap::new();
    for run in &r.runs {
        let b = run.backend.as_str();
        if let Some(open) = &run.open {
            out.push(GateOutcome::exact(
                "exact.triple_num",
                b,
                open.triple_num == r.workload.unique_triples,
                format!(
                    "triple_num {} vs generated unique triples {}",
                    open.triple_num, r.workload.unique_triples
                ),
            ));
        }
        for q in &run.queries {
            out.push(GateOutcome::exact(
                "exact.stable",
                format!("{b}/{}", q.name),
                q.stable,
                "same rows in every run",
            ));
            out.push(GateOutcome::exact(
                "exact.nonempty",
                format!("{b}/{}", q.name),
                q.rows > 0,
                format!("{} rows", q.rows),
            ));
            by_query
                .entry(q.name.as_str())
                .or_default()
                .push((b, q.rows, q.rows_fnv64.as_str()));
        }
        if let Some(u) = &run.update {
            let mut why = Vec::new();
            if !u.singles_ok {
                why.push("single inserts/deletes".to_string());
            }
            if !u.chunk_delta_ok {
                why.push("chunk triple delta".to_string());
            }
            if !u.answers_unchanged {
                why.push(format!("answers changed: {}", u.mismatched.join(",")));
            }
            if !u.reopen_ok {
                why.push(format!("reopened triple_num {}", u.reopen_triple_num));
            }
            out.push(GateOutcome::exact(
                "exact.update_round_trip",
                b,
                u.round_trip_ok(),
                if why.is_empty() {
                    "ok".into()
                } else {
                    why.join("; ")
                },
            ));
        }
        if let Some(c) = &run.conc {
            out.push(GateOutcome::exact(
                "exact.conc_bodies",
                b,
                c.bodies_equal,
                "identical response bodies per endpoint and query",
            ));
        }
    }
    for (name, seen) in by_query {
        if seen.len() < 2 {
            continue;
        }
        let (b0, rows0, h0) = seen[0];
        for &(b, rows, h) in &seen[1..] {
            out.push(GateOutcome::exact(
                "exact.cross_backend",
                format!("{b}/{name}"),
                rows == rows0 && h == h0,
                format!("{rows} rows {h} vs {b0}: {rows0} rows {h0}"),
            ));
        }
    }
    out
}

/// The calibration factor (measured machine time / baseline machine time,
/// clamped) dividing measured times.
fn calib_factor(
    t: &Thresholds,
    base: &BenchResult,
    b: &BackendRun,
    m: &BenchResult,
    r: &BackendRun,
) -> f64 {
    if !t.normalize_by_calibration {
        return 1.0;
    }
    match (base.calib_score(b), m.calib_score(r)) {
        (Some(cb), Some(cm)) => (cm / cb).clamp(t.calibration_clamp[0], t.calibration_clamp[1]),
        _ => 1.0,
    }
}

#[allow(clippy::too_many_arguments)]
fn ratio_gate(
    gate: &str,
    subject: String,
    class: GateClass,
    base: f64,
    measured: f64,
    floor: f64,
    limit: f64,
    retry: Option<(String, Phase)>,
) -> GateOutcome {
    let ratio = (measured + floor) / (base + floor).max(f64::MIN_POSITIVE);
    GateOutcome {
        gate: gate.into(),
        subject,
        class,
        base: Some(base),
        measured: Some(measured),
        ratio: Some(ratio),
        limit: Some(limit),
        pass: ratio <= limit,
        detail: String::new(),
        retry,
    }
}

/// Evaluate `m` against baseline `base`. `slowdown` multiplies every measured
/// time (the `--self-test-slowdown` canary; 1.0 normally).
pub fn evaluate(
    base: &BenchResult,
    m: &BenchResult,
    t: &Thresholds,
    slowdown: f64,
) -> Vec<GateOutcome> {
    let mut out = self_check(m);
    // Workload identity.
    let wb = &base.workload;
    let wm = &m.workload;
    out.push(GateOutcome::exact(
        "exact.workload",
        "workload",
        wb.spec == wm.spec && wb.version == wm.version,
        format!(
            "{} {} vs baseline {} {}",
            wm.version,
            wm.spec.label(),
            wb.version,
            wb.spec.label()
        ),
    ));
    out.push(GateOutcome::exact(
        "exact.data_hash",
        "workload",
        wb.data_fnv64 == wm.data_fnv64 && wb.unique_triples == wm.unique_triples,
        format!(
            "data {} ({} triples) vs baseline {} ({} triples)",
            wm.data_fnv64, wm.unique_triples, wb.data_fnv64, wb.unique_triples
        ),
    ));
    for qb in &wb.queries {
        let qm = wm.queries.iter().find(|q| q.name == qb.name);
        out.push(GateOutcome::exact(
            "exact.query_text",
            format!("baseline/{}", qb.name),
            qm.is_some_and(|q| q.text_fnv64 == qb.text_fnv64),
            format!(
                "text fnv {} vs baseline {}",
                qm.map(|q| q.text_fnv64.as_str()).unwrap_or("missing"),
                qb.text_fnv64
            ),
        ));
    }
    for b in &base.runs {
        let be = b.backend.as_str();
        let Some(r) = m.run(be) else {
            out.push(GateOutcome::exact(
                "exact.backend",
                be,
                false,
                "the baseline has this backend; the result does not",
            ));
            continue;
        };
        let c = calib_factor(t, base, b, m, r);
        let norm = |x: f64| x * slowdown / c;

        // Queries: exact answers, then timing.
        let mut logs = Vec::new();
        for qb in &b.queries {
            let Some(qm) = r.query(&qb.name) else {
                out.push(GateOutcome::exact(
                    "exact.query",
                    format!("{be}/{}", qb.name),
                    false,
                    "query missing from the result",
                ));
                continue;
            };
            out.push(GateOutcome::exact(
                "exact.rows",
                format!("{be}/{}", qb.name),
                qm.rows == qb.rows && qm.rows_fnv64 == qb.rows_fnv64,
                format!(
                    "{} rows {} vs baseline {} rows {}",
                    qm.rows, qm.rows_fnv64, qb.rows, qb.rows_fnv64
                ),
            ));
            let mm = norm(qm.median_ms);
            out.push(ratio_gate(
                "time.query",
                format!("{be}/{}", qb.name),
                GateClass::Timing,
                qb.median_ms,
                mm,
                t.query.floor_ms,
                t.query.max_ratio,
                Some((be.to_string(), Phase::Query)),
            ));
            let eps = t.geomean.epsilon_ms;
            logs.push((mm.max(eps) / qb.median_ms.max(eps)).ln());
        }
        if !logs.is_empty() {
            let g = (logs.iter().sum::<f64>() / logs.len() as f64).exp();
            out.push(GateOutcome {
                gate: "time.geomean".into(),
                subject: be.to_string(),
                class: GateClass::Timing,
                base: None,
                measured: None,
                ratio: Some(g),
                limit: Some(t.geomean.max_ratio),
                pass: g <= t.geomean.max_ratio,
                detail: format!("geomean of {} warm-median ratios", logs.len()),
                retry: Some((be.to_string(), Phase::Query)),
            });
        }
        if let (Some(mb), Some(mm)) = (&b.query_mem, &r.query_mem) {
            if let (Some(x), Some(y)) = (mb.hwm_mib, mm.hwm_mib) {
                out.push(ratio_gate(
                    "rss.query_hwm",
                    be.to_string(),
                    GateClass::Memory,
                    x,
                    y,
                    t.rss.floor_mib,
                    t.rss.max_ratio,
                    Some((be.to_string(), Phase::Query)),
                ));
            }
        }

        // Build.
        match (&b.build, &r.build) {
            (Some(bb), Some(mb)) => {
                let limit = if be == "disk" {
                    t.build.max_ratio_disk.unwrap_or(t.build.max_ratio)
                } else {
                    t.build.max_ratio
                };
                out.push(ratio_gate(
                    "time.build",
                    be.to_string(),
                    GateClass::Timing,
                    bb.wall_ms,
                    norm(mb.wall_ms),
                    t.build.floor_ms,
                    limit,
                    Some((be.to_string(), Phase::Build)),
                ));
                if let (Some(x), Some(y)) = (bb.peak_rss_mib, mb.peak_rss_mib) {
                    out.push(ratio_gate(
                        "rss.build_peak",
                        be.to_string(),
                        GateClass::Memory,
                        x,
                        y,
                        t.rss.floor_mib,
                        t.rss.max_ratio,
                        Some((be.to_string(), Phase::Build)),
                    ));
                }
                let mut g = ratio_gate(
                    "bytes.dir",
                    be.to_string(),
                    GateClass::Size,
                    bb.dir_bytes as f64,
                    mb.dir_bytes as f64,
                    0.0,
                    t.disk_bytes.max_ratio,
                    None,
                );
                if bb.dir_bytes == 0 {
                    g.ratio = Some(1.0);
                    g.pass = true;
                }
                out.push(g);
            }
            (Some(_), None) => out.push(GateOutcome::exact(
                "exact.phase",
                format!("{be}/build"),
                false,
                "the baseline measured the build; the result did not",
            )),
            _ => {}
        }

        // Open.
        match (&b.open, &r.open) {
            (Some(ob), Some(om)) => {
                out.push(ratio_gate(
                    "time.open",
                    be.to_string(),
                    GateClass::Timing,
                    ob.warm_ms,
                    norm(om.warm_ms),
                    t.open.floor_ms,
                    t.open.max_ratio,
                    Some((be.to_string(), Phase::Open)),
                ));
                if let (Some(x), Some(y)) = (ob.rss_after_open_mib, om.rss_after_open_mib) {
                    out.push(ratio_gate(
                        "rss.after_open",
                        be.to_string(),
                        GateClass::Memory,
                        x,
                        y,
                        t.rss.floor_mib,
                        t.rss.max_ratio,
                        Some((be.to_string(), Phase::Open)),
                    ));
                }
            }
            (Some(_), None) => out.push(GateOutcome::exact(
                "exact.phase",
                format!("{be}/open"),
                false,
                "the baseline measured the open; the result did not",
            )),
            _ => {}
        }
        if b.update.is_some() && r.update.is_none() {
            out.push(GateOutcome::exact(
                "exact.phase",
                format!("{be}/update"),
                false,
                "the baseline ran the update phase; the result did not",
            ));
        }
        if b.conc.is_some() && r.conc.is_none() {
            out.push(GateOutcome::exact(
                "exact.phase",
                format!("{be}/conc"),
                false,
                "the baseline ran the concurrency phase; the result did not",
            ));
        }
        // Report-only: concurrency throughput.
        if let (Some(cb), Some(cm)) = (&b.conc, &r.conc) {
            for pb in &cb.points {
                if let Some(pm) = cm.points.iter().find(|p| {
                    p.endpoint == pb.endpoint && p.query == pb.query && p.clients == pb.clients
                }) {
                    out.push(GateOutcome {
                        gate: "info.conc_qps".into(),
                        subject: format!("{be}/{}/{}/c{}", pb.endpoint, pb.query, pb.clients),
                        class: GateClass::Info,
                        base: Some(pb.qps),
                        measured: Some(pm.qps),
                        ratio: (pb.qps > 0.0).then(|| pm.qps / pb.qps),
                        limit: None,
                        pass: true,
                        detail: "report-only".into(),
                        retry: None,
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_thresholds_parse() {
        let t = Thresholds::default();
        assert_eq!(t.query.max_ratio, 3.0);
        assert_eq!(t.geomean.max_ratio, 1.5);
        assert_eq!(t.build.max_ratio_disk, Some(2.5));
        assert_eq!(t.retries, 2);
    }
}
