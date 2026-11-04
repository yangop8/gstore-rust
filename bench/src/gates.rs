//! The gates `gstore-bench-gates/1` (`bench/thresholds.json`) and their
//! evaluation.
//!
//! * **Exact gates** (never retried): the generator version and data hash, the
//!   query text hashes, `triple_num == unique_triples`, every query's rows and
//!   row hash equal to the baseline and across backends, stable across runs and
//!   non-empty, the update round trip, identical concurrent response bodies
//!   (and, in the mixed workload, identical reader bodies and acknowledged
//!   writes).
//! * **Timing gates** (one-sided, calibration-normalized): per query
//!   `(m+floor)/(b+floor) > 3.0` fails, the per-backend geomean of the raw
//!   medians `> 1.5` fails (over the queries whose baseline median is at
//!   least `min_base_ms`), build `> 2.0` (`> 2.5` for disk), warm open `> 2.5`.
//! * **Memory**: `(m+32 MiB)/(b+32 MiB) > 1.5` fails (build peak, after open,
//!   query high-water mark). **Size**: directory bytes `> 1.10` fails (sizes
//!   are deterministic: identical data and code give identical bytes).
//! * **Report-only** (`info.*`, never fail): concurrency and mixed-workload
//!   throughput, and what the result has that the baseline lacks — a whole
//!   extra workload (the WatDiv-like section before a baseline has one), a
//!   run, or a query (`info.new`).
//!
//! Every workload of a result (the main one and each of `extra_workloads`)
//! is gated on its own against the baseline's workload of the same id; the
//! subjects of an extra workload's gates start with its id
//! (`watdiv-like:snapshot/L1`). Runs are matched by key
//! ([`BackendRun::key`]); an unlabelled baseline run is compared with every
//! labelled run of its backend (`disk` with `disk@stream` and
//! `disk@materialize`).
//!
//! [`self_check`] runs the exact gates that need no baseline; `run` applies it
//! to every result.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::hash::hex;
use crate::schema::{
    matching_runs, BackendRun, BenchResult, Section, WorkloadInfo, READABLE_SCHEMAS,
};

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
    /// Only queries whose baseline median is at least this long enter the
    /// geomean (all of them when fewer than three qualify): the relative
    /// jitter of sub-millisecond queries would dominate it, and the
    /// per-query gate already covers them.
    #[serde(default)]
    pub min_base_ms: f64,
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

/// What a retry re-runs: a phase of one run of one workload.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RetryTarget {
    /// `None` for the main workload, else the extra workload's id.
    pub section: Option<String>,
    /// The run's key ([`BackendRun::key`]).
    pub run: String,
    pub phase: Phase,
}

impl RetryTarget {
    /// A phase of a run of the main workload.
    pub fn main(run: &str, phase: Phase) -> RetryTarget {
        RetryTarget {
            section: None,
            run: run.to_string(),
            phase,
        }
    }
}

/// One evaluated gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateOutcome {
    /// What was checked (`exact.rows`, `time.query`, `rss.open`, …).
    pub gate: String,
    /// Where (`snapshot/q4`, `disk`, `workload`; an extra workload's
    /// subjects start with its id, `watdiv-like:snapshot/L1`).
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
    /// What a retry re-runs (timing and memory gates).
    #[serde(default)]
    pub retry: Option<RetryTarget>,
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

    fn info(gate: &str, subject: impl Into<String>, detail: impl Into<String>) -> Self {
        GateOutcome {
            class: GateClass::Info,
            ..GateOutcome::exact(gate, subject, true, detail)
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

/// The phases failing timing or memory gates name, each once.
pub fn retry_targets(outcomes: &[GateOutcome]) -> Vec<RetryTarget> {
    let mut v: Vec<RetryTarget> = outcomes
        .iter()
        .filter(|o| !o.pass)
        .filter_map(|o| o.retry.clone())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// The exact gates of one result that need no baseline, for every workload
/// of it.
pub fn self_check(r: &BenchResult) -> Vec<GateOutcome> {
    let mut out = vec![GateOutcome::exact(
        "exact.schema",
        "result",
        READABLE_SCHEMAS.contains(&r.schema.as_str()),
        format!("schema {}", r.schema),
    )];
    let mut seen = Vec::new();
    for s in r.sections() {
        if let Some(id) = s.id {
            out.push(GateOutcome::exact(
                "exact.section",
                format!("{id}:workload"),
                id != r.workload.id && !seen.contains(&id),
                "one section per workload, other than the main one",
            ));
            seen.push(id);
        }
        self_check_section(&s, &mut out);
    }
    out
}

fn self_check_section(s: &Section<'_>, out: &mut Vec<GateOutcome>) {
    let pre = s.prefix();
    // The tool's pinned query texts (LUBM).
    if s.workload.id == "lubm-like" {
        for q in &s.workload.queries {
            let pinned = crate::lubm::QUERY_TEXT_FNV64
                .iter()
                .find(|(n, _)| *n == q.name)
                .map(|(_, h)| hex(*h));
            out.push(GateOutcome::exact(
                "exact.query_text",
                format!("{pre}workload/{}", q.name),
                pinned.as_deref() == Some(q.text_fnv64.as_str()),
                format!(
                    "text fnv {} (pinned {})",
                    q.text_fnv64,
                    pinned.unwrap_or_default()
                ),
            ));
        }
    }
    let mut keys: Vec<String> = Vec::new();
    let mut by_query: BTreeMap<&str, Vec<(String, u64, &str)>> = BTreeMap::new();
    for run in s.runs {
        let b = run.key();
        if keys.contains(&b) {
            out.push(GateOutcome::exact(
                "exact.run_key",
                format!("{pre}{b}"),
                false,
                "a run key appears twice in one workload",
            ));
        }
        keys.push(b.clone());
        if let Some(open) = &run.open {
            out.push(GateOutcome::exact(
                "exact.triple_num",
                format!("{pre}{b}"),
                open.triple_num == s.workload.unique_triples,
                format!(
                    "triple_num {} vs generated unique triples {}",
                    open.triple_num, s.workload.unique_triples
                ),
            ));
        }
        for q in &run.queries {
            out.push(GateOutcome::exact(
                "exact.stable",
                format!("{pre}{b}/{}", q.name),
                q.stable,
                "same rows in every run",
            ));
            out.push(GateOutcome::exact(
                "exact.nonempty",
                format!("{pre}{b}/{}", q.name),
                q.rows > 0,
                format!("{} rows", q.rows),
            ));
            by_query.entry(q.name.as_str()).or_default().push((
                b.clone(),
                q.rows,
                q.rows_fnv64.as_str(),
            ));
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
                format!("{pre}{b}"),
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
                format!("{pre}{b}"),
                c.bodies_equal,
                "identical response bodies per endpoint and query",
            ));
        }
        if let Some(m) = &run.mixed {
            out.push(GateOutcome::exact(
                "exact.mixed",
                format!("{pre}{b}"),
                m.bodies_equal && m.writes_ok,
                format!(
                    "reader bodies identical: {}, every write acknowledged: {}",
                    m.bodies_equal, m.writes_ok
                ),
            ));
        }
    }
    for (name, seen) in by_query {
        if seen.len() < 2 {
            continue;
        }
        let (b0, rows0, h0) = &seen[0];
        for (b, rows, h) in &seen[1..] {
            out.push(GateOutcome::exact(
                "exact.cross_backend",
                format!("{pre}{b}/{name}"),
                rows == rows0 && h == h0,
                format!("{rows} rows {h} vs {b0}: {rows0} rows {h0}"),
            ));
        }
    }
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
    retry: Option<RetryTarget>,
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
    let ctx = Ctx {
        t,
        slowdown,
        base,
        m,
    };
    let bs = base.sections();
    let ms = m.sections();
    // The main workloads are always compared (a different workload fails
    // the identity gates).
    ctx.section(&bs[0], &ms[0], &mut out);
    for sm in &ms[1..] {
        match bs[1..].iter().find(|sb| sb.id == sm.id) {
            Some(sb) => ctx.section(sb, sm, &mut out),
            None => new_section(sm, &mut out),
        }
    }
    for sb in &bs[1..] {
        if !ms[1..].iter().any(|sm| sm.id == sb.id) {
            out.push(GateOutcome::exact(
                "exact.section",
                format!("{}workload", sb.prefix()),
                false,
                "the baseline has this workload; the result does not",
            ));
        }
    }
    out
}

/// A workload the baseline lacks: reported, not gated.
fn new_section(sm: &Section<'_>, out: &mut Vec<GateOutcome>) {
    let pre = sm.prefix();
    out.push(GateOutcome::info(
        "info.new",
        format!("{pre}workload"),
        format!(
            "{} ({} queries) is new: the baseline has no {} workload",
            sm.workload.spec.label(),
            sm.workload.queries.len(),
            sm.id.unwrap_or_default()
        ),
    ));
    for r in sm.runs {
        out.push(GateOutcome {
            measured: crate::report::geomean_ms(r),
            ..GateOutcome::info(
                "info.new",
                format!("{pre}{}", r.key()),
                format!(
                    "new run of {} queries (measured: their geomean ms); not gated until \
                     the baseline has it",
                    r.queries.len()
                ),
            )
        });
    }
}

struct Ctx<'a> {
    t: &'a Thresholds,
    slowdown: f64,
    base: &'a BenchResult,
    m: &'a BenchResult,
}

impl Ctx<'_> {
    fn identity(
        &self,
        wb: &WorkloadInfo,
        wm: &WorkloadInfo,
        pre: &str,
        out: &mut Vec<GateOutcome>,
    ) {
        out.push(GateOutcome::exact(
            "exact.workload",
            format!("{pre}workload"),
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
            format!("{pre}workload"),
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
                format!("{pre}baseline/{}", qb.name),
                qm.is_some_and(|q| q.text_fnv64 == qb.text_fnv64),
                format!(
                    "text fnv {} vs baseline {}",
                    qm.map(|q| q.text_fnv64.as_str()).unwrap_or("missing"),
                    qb.text_fnv64
                ),
            ));
        }
        for qm in &wm.queries {
            if !wb.queries.iter().any(|q| q.name == qm.name) {
                out.push(GateOutcome::info(
                    "info.new",
                    format!("{pre}workload/{}", qm.name),
                    "new query: not in the baseline",
                ));
            }
        }
    }

    fn section(&self, sb: &Section<'_>, sm: &Section<'_>, out: &mut Vec<GateOutcome>) {
        let pre = sm.prefix();
        self.identity(sb.workload, sm.workload, &pre, out);
        let section = sm.id.map(str::to_string);
        let mut compared: Vec<String> = Vec::new();
        for b in sb.runs {
            let matched = matching_runs(sm.runs, b);
            if matched.is_empty() {
                out.push(GateOutcome::exact(
                    "exact.backend",
                    format!("{pre}{}", b.key()),
                    false,
                    "the baseline has this run; the result does not",
                ));
            }
            for r in matched {
                compared.push(r.key());
                self.run(b, r, &pre, &section, out);
            }
        }
        for r in sm.runs {
            if !compared.contains(&r.key()) {
                out.push(GateOutcome::info(
                    "info.new",
                    format!("{pre}{}", r.key()),
                    "new run: the baseline has no run of this key or backend",
                ));
            }
        }
    }

    fn run(
        &self,
        b: &BackendRun,
        r: &BackendRun,
        pre: &str,
        section: &Option<String>,
        out: &mut Vec<GateOutcome>,
    ) {
        let t = self.t;
        let key = r.key();
        let subj = format!("{pre}{key}");
        let retry = |phase| {
            Some(RetryTarget {
                section: section.clone(),
                run: key.clone(),
                phase,
            })
        };
        let c = calib_factor(t, self.base, b, self.m, r);
        let norm = |x: f64| x * self.slowdown / c;

        // Queries: exact answers, then timing.
        let mut logs = Vec::new();
        for qb in &b.queries {
            let Some(qm) = r.query(&qb.name) else {
                out.push(GateOutcome::exact(
                    "exact.query",
                    format!("{subj}/{}", qb.name),
                    false,
                    "query missing from the result",
                ));
                continue;
            };
            out.push(GateOutcome::exact(
                "exact.rows",
                format!("{subj}/{}", qb.name),
                qm.rows == qb.rows && qm.rows_fnv64 == qb.rows_fnv64,
                format!(
                    "{} rows {} vs baseline {} rows {}",
                    qm.rows, qm.rows_fnv64, qb.rows, qb.rows_fnv64
                ),
            ));
            let mm = norm(qm.median_ms);
            out.push(ratio_gate(
                "time.query",
                format!("{subj}/{}", qb.name),
                GateClass::Timing,
                qb.median_ms,
                mm,
                t.query.floor_ms,
                t.query.max_ratio,
                retry(Phase::Query),
            ));
            let eps = t.geomean.epsilon_ms;
            logs.push((qb.median_ms, (mm.max(eps) / qb.median_ms.max(eps)).ln()));
        }
        for qm in &r.queries {
            if b.query(&qm.name).is_none() {
                out.push(GateOutcome {
                    measured: Some(qm.median_ms),
                    ..GateOutcome::info(
                        "info.new",
                        format!("{subj}/{}", qm.name),
                        "new query: not in the baseline run",
                    )
                });
            }
        }
        let long: Vec<f64> = logs
            .iter()
            .filter(|(b, _)| *b >= t.geomean.min_base_ms)
            .map(|(_, l)| *l)
            .collect();
        let logs: Vec<f64> = if long.len() >= 3 {
            long
        } else {
            logs.iter().map(|(_, l)| *l).collect()
        };
        if !logs.is_empty() {
            let g = (logs.iter().sum::<f64>() / logs.len() as f64).exp();
            out.push(GateOutcome {
                gate: "time.geomean".into(),
                subject: subj.clone(),
                class: GateClass::Timing,
                base: None,
                measured: None,
                ratio: Some(g),
                limit: Some(t.geomean.max_ratio),
                pass: g <= t.geomean.max_ratio,
                detail: format!("geomean of {} warm-median ratios", logs.len()),
                retry: retry(Phase::Query),
            });
        }
        if let (Some(mb), Some(mm)) = (&b.query_mem, &r.query_mem) {
            if let (Some(x), Some(y)) = (mb.hwm_mib, mm.hwm_mib) {
                out.push(ratio_gate(
                    "rss.query_hwm",
                    subj.clone(),
                    GateClass::Memory,
                    x,
                    y,
                    t.rss.floor_mib,
                    t.rss.max_ratio,
                    retry(Phase::Query),
                ));
            }
        }

        // Build.
        match (&b.build, &r.build) {
            (Some(bb), Some(mb)) => {
                let limit = if r.backend == "disk" {
                    t.build.max_ratio_disk.unwrap_or(t.build.max_ratio)
                } else {
                    t.build.max_ratio
                };
                out.push(ratio_gate(
                    "time.build",
                    subj.clone(),
                    GateClass::Timing,
                    bb.wall_ms,
                    norm(mb.wall_ms),
                    t.build.floor_ms,
                    limit,
                    retry(Phase::Build),
                ));
                if let (Some(x), Some(y)) = (bb.peak_rss_mib, mb.peak_rss_mib) {
                    out.push(ratio_gate(
                        "rss.build_peak",
                        subj.clone(),
                        GateClass::Memory,
                        x,
                        y,
                        t.rss.floor_mib,
                        t.rss.max_ratio,
                        retry(Phase::Build),
                    ));
                }
                let mut g = ratio_gate(
                    "bytes.dir",
                    subj.clone(),
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
                format!("{subj}/build"),
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
                    subj.clone(),
                    GateClass::Timing,
                    ob.warm_ms,
                    norm(om.warm_ms),
                    t.open.floor_ms,
                    t.open.max_ratio,
                    retry(Phase::Open),
                ));
                if let (Some(x), Some(y)) = (ob.rss_after_open_mib, om.rss_after_open_mib) {
                    out.push(ratio_gate(
                        "rss.after_open",
                        subj.clone(),
                        GateClass::Memory,
                        x,
                        y,
                        t.rss.floor_mib,
                        t.rss.max_ratio,
                        retry(Phase::Open),
                    ));
                }
            }
            (Some(_), None) => out.push(GateOutcome::exact(
                "exact.phase",
                format!("{subj}/open"),
                false,
                "the baseline measured the open; the result did not",
            )),
            _ => {}
        }
        for (what, had, has) in [
            ("update", b.update.is_some(), r.update.is_some()),
            ("conc", b.conc.is_some(), r.conc.is_some()),
            ("mixed", b.mixed.is_some(), r.mixed.is_some()),
        ] {
            if had && !has {
                out.push(GateOutcome::exact(
                    "exact.phase",
                    format!("{subj}/{what}"),
                    false,
                    format!("the baseline ran the {what} phase; the result did not"),
                ));
            }
        }
        // Report-only: concurrency and mixed-workload throughput.
        if let (Some(cb), Some(cm)) = (&b.conc, &r.conc) {
            for pb in &cb.points {
                if let Some(pm) = cm.points.iter().find(|p| {
                    p.endpoint == pb.endpoint && p.query == pb.query && p.clients == pb.clients
                }) {
                    out.push(GateOutcome {
                        base: Some(pb.qps),
                        measured: Some(pm.qps),
                        ratio: (pb.qps > 0.0).then(|| pm.qps / pb.qps),
                        ..GateOutcome::info(
                            "info.conc_qps",
                            format!("{subj}/{}/{}/c{}", pb.endpoint, pb.query, pb.clients),
                            "report-only",
                        )
                    });
                }
            }
        }
        if let (Some(xb), Some(xm)) = (&b.mixed, &r.mixed) {
            if let (Some(bb), Some(bm)) = (&xb.begin, &xm.begin) {
                out.push(GateOutcome {
                    base: Some(bb.median_ms),
                    measured: Some(bm.median_ms),
                    ratio: (bb.median_ms > 0.0).then(|| bm.median_ms / bb.median_ms),
                    ..GateOutcome::info("info.begin_ms", format!("{subj}/api/begin"), "report-only")
                });
            }
            for pb in &xb.points {
                if let Some(pm) = xm.points.iter().find(|p| {
                    p.endpoint == pb.endpoint
                        && p.query == pb.query
                        && p.readers == pb.readers
                        && p.check_point == pb.check_point
                }) {
                    out.push(GateOutcome {
                        base: Some(pb.qps),
                        measured: Some(pm.qps),
                        ratio: (pb.qps > 0.0).then(|| pm.qps / pb.qps),
                        ..GateOutcome::info(
                            "info.mixed_qps",
                            format!(
                                "{subj}/{}/{}/r{}/cp-{}",
                                pb.endpoint,
                                pb.query,
                                pb.readers,
                                if pb.check_point { "on" } else { "off" }
                            ),
                            "report-only",
                        )
                    });
                }
            }
        }
    }
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
