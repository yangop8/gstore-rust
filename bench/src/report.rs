//! Markdown reports of results, comparisons and gate outcomes.
//!
//! Everything here is a pure function of the result files: the same JSON
//! gives the same bytes, so documentation tables can be regenerated from
//! committed results and checked in CI.
//!
//! * [`markdown`] — the full report of one result (every workload, every
//!   phase), with ratios against a baseline;
//! * [`table`] — one compact table over one or more results (`gstore-bench
//!   report --table overview|queries|build|update|conc|mixed|gates|full`);
//! * [`splice`] — regenerate every table between `<!-- gstore-bench report
//!   ARGS -->` and `<!-- /gstore-bench report -->` markers of a document
//!   (`report --update FILE` / `report --check FILE`).

use std::fmt::Write as _;
use std::path::Path;

use crate::gates::{evaluate, verdict, GateClass, GateOutcome, Thresholds, Verdict};
use crate::schema::{matching_run, BackendRun, BenchResult, Section};

fn f1(x: f64) -> String {
    format!("{x:.1}")
}

fn f2(x: f64) -> String {
    format!("{x:.2}")
}

fn opt(x: Option<f64>, f: fn(f64) -> String) -> String {
    x.map(f).unwrap_or_else(|| "–".into())
}

fn ratio_cell(m: f64, b: Option<f64>) -> String {
    match b {
        Some(b) if b > 0.0 => format!("{:.2}×", m / b),
        _ => "–".into(),
    }
}

fn ratio_opt(m: Option<f64>, b: Option<f64>) -> String {
    match m {
        Some(m) => ratio_cell(m, b),
        None => "–".into(),
    }
}

/// The section of `base` that `s` is compared with (same id).
fn base_section<'a>(base: Option<&'a BenchResult>, s: &Section<'_>) -> Option<Section<'a>> {
    base.and_then(|b| b.sections().into_iter().find(|x| x.id == s.id))
}

/// A Markdown report of `r`, with ratios against `base` when given.
pub fn markdown(r: &BenchResult, base: Option<&BenchResult>) -> String {
    let mut s = String::new();
    let w = &r.workload;
    let _ = writeln!(
        s,
        "## gstore-bench: {} profile, {} ({} unique triples)\n",
        r.profile,
        w.spec.label(),
        w.unique_triples
    );
    if r.provisional {
        let _ = writeln!(
            s,
            "> **Provisional** baseline: {}\n",
            r.note.clone().unwrap_or_default()
        );
    }
    let _ = writeln!(
        s,
        "- commit `{}`{}, {} {} × {} CPUs{}, {}",
        r.env.git_commit.clone().unwrap_or_else(|| "?".into()),
        if r.env.git_dirty == Some(true) {
            " (dirty)"
        } else {
            ""
        },
        r.env.os,
        r.env.arch,
        r.env.nproc,
        r.env
            .mem_total_mib
            .map(|m| format!(", {m} MiB"))
            .unwrap_or_default(),
        r.env
            .loadavg
            .map(|l| format!("load {:.2} {:.2} {:.2}", l[0], l[1], l[2]))
            .unwrap_or_default()
    );
    if let Some(c) = &r.calibration {
        let _ = writeln!(
            s,
            "- calibration score {:.2} ms (sort {:.2}, hashmap {:.2}, format {:.2}, fsync {})",
            c.score_ms,
            c.sort_ms,
            c.hashmap_ms,
            c.format_ms,
            opt(c.fsync_ms, f2)
        );
    }
    if let Some(b) = base {
        let _ = writeln!(
            s,
            "- compared with: {} profile, commit `{}`{}",
            b.profile,
            b.env.git_commit.clone().unwrap_or_else(|| "?".into()),
            if b.provisional { " (provisional)" } else { "" }
        );
    }
    for sec in r.sections() {
        let bs = base_section(base, &sec);
        let cmp = base.is_some();
        if sec.id.is_some() {
            let _ = writeln!(
                s,
                "\n### {} ({} unique triples)",
                sec.workload.spec.label(),
                sec.workload.unique_triples
            );
            if cmp && bs.is_none() {
                let _ = writeln!(s, "\n_Not in the baseline: reported, not gated._");
            }
        }
        let w = sec.workload;
        let _ = writeln!(
            s,
            "\n- generator `{}`, data fnv `{}`, {} lines, {:.1} MB\n",
            w.version,
            w.data_fnv64,
            w.lines,
            w.bytes as f64 / 1e6
        );
        let rows: Vec<Row<'_>> = sec
            .runs
            .iter()
            .map(|run| Row {
                workload: None,
                run,
                base: bs.and_then(|b| matching_run(b.runs, run)),
            })
            .collect();
        s.push_str(&overview(&rows, cmp));
        s.push_str(&build_table(&rows));
        s.push_str(&queries(&sec, &rows, cmp));
        s.push_str(&update_table(&rows));
        s.push_str(&conc_table(&rows, cmp));
        s.push_str(&mixed_table(&rows, cmp));
    }
    s
}

/// One run in a table: its workload's label (multi-file tables), the run
/// and the baseline run it is compared with.
struct Row<'a> {
    workload: Option<String>,
    run: &'a BackendRun,
    base: Option<&'a BackendRun>,
}

impl Row<'_> {
    /// The leading cells: `| workload | run |` or `| run |`.
    fn lead(&self) -> String {
        match &self.workload {
            Some(w) => format!("| {w} | {} |", self.run.key()),
            None => format!("| {} |", self.run.key()),
        }
    }
}

fn lead_header(rows: &[Row<'_>], what: &str) -> (String, String) {
    if rows.first().is_some_and(|r| r.workload.is_some()) {
        (format!("| workload | {what} |"), "|---|---|".into())
    } else {
        (format!("| {what} |"), "|---|".into())
    }
}

fn overview(rows: &[Row<'_>], cmp: bool) -> String {
    let mut s = String::new();
    if rows.is_empty() {
        return s;
    }
    let (h, d) = lead_header(rows, "backend");
    let _ = writeln!(
        s,
        "{h} build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms |{}",
        if cmp {
            " build vs base | open vs base | geomean vs base |"
        } else {
            ""
        }
    );
    let _ = writeln!(
        s,
        "{d}---:|---:|---:|---:|---:|---:|---:|---:|{}",
        if cmp { "---:|---:|---:|" } else { "" }
    );
    for row in rows {
        let run = row.run;
        let b = row.base;
        let g = geomean_ms(run);
        let _ = writeln!(
            s,
            "{} {} | {} | {} | {}{} | {} | {} | {} | {} |{}",
            row.lead(),
            opt(run.build.as_ref().map(|x| x.wall_ms / 1e3), f2),
            opt(run.build.as_ref().and_then(|x| x.peak_rss_mib), f1),
            opt(run.build.as_ref().map(|x| x.dir_bytes as f64 / 1e6), f1),
            opt(run.open.as_ref().map(|x| x.first_ms), f1),
            if run.open.as_ref().is_some_and(|o| o.cold) {
                " (cold)"
            } else {
                ""
            },
            opt(run.open.as_ref().map(|x| x.warm_ms), f1),
            opt(run.open.as_ref().and_then(|x| x.rss_after_open_mib), f1),
            opt(run.query_mem.as_ref().and_then(|x| x.hwm_mib), f1),
            opt(g, f2),
            if cmp {
                format!(
                    " {} | {} | {} |",
                    ratio_opt(
                        run.build.as_ref().map(|x| x.wall_ms),
                        b.and_then(|b| b.build.as_ref().map(|x| x.wall_ms))
                    ),
                    ratio_opt(
                        run.open.as_ref().map(|x| x.warm_ms),
                        b.and_then(|b| b.open.as_ref().map(|x| x.warm_ms))
                    ),
                    ratio_opt(g, b.and_then(geomean_ms))
                )
            } else {
                String::new()
            }
        );
    }
    s.push('\n');
    s
}

fn build_table(rows: &[Row<'_>]) -> String {
    let mut s = String::new();
    if !rows
        .iter()
        .any(|r| r.run.build.as_ref().is_some_and(|b| b.summary.is_some()))
    {
        return s;
    }
    let (h, d) = lead_header(rows, "backend");
    let _ = writeln!(
        s,
        "{h} gbuild phases ms | budget MiB | budget peak MiB | term map MiB | threads | spilled runs | spilled MiB |"
    );
    let _ = writeln!(s, "{d}---|---:|---:|---:|---:|---:|---:|");
    for row in rows {
        let Some(x) = row.run.build.as_ref().and_then(|b| b.summary.as_ref()) else {
            continue;
        };
        let phases: Vec<String> = x
            .phases_ms
            .iter()
            .map(|(n, ms)| format!("{n} {ms:.0}"))
            .collect();
        let _ = writeln!(
            s,
            "{} {} | {} | {} | {} | {} | {} | {} |",
            row.lead(),
            if phases.is_empty() {
                "–".to_string()
            } else {
                phases.join(", ")
            },
            opt(x.budget_mib, f1),
            opt(x.budget_peak_mib, f1),
            opt(x.term_map_mib, f1),
            x.threads
                .map(|t| t.to_string())
                .unwrap_or_else(|| "–".into()),
            x.spilled_runs
                .map(|t| t.to_string())
                .unwrap_or_else(|| "–".into()),
            opt(x.spilled_mib, f1),
        );
    }
    s.push('\n');
    s
}

/// Queries: one row per query, one column (plus a ratio) per run.
fn queries(sec: &Section<'_>, rows: &[Row<'_>], cmp: bool) -> String {
    let mut s = String::new();
    if rows.is_empty() {
        return s;
    }
    let _ = write!(s, "| query | rows |");
    for row in rows {
        let k = row.run.key();
        let _ = write!(s, " {k} first ms | {k} median ms |");
        if cmp {
            let _ = write!(s, " {k} vs base |");
        }
    }
    s.push('\n');
    let _ = write!(s, "|---|---:|");
    for _ in rows {
        let _ = write!(s, "---:|---:|");
        if cmp {
            let _ = write!(s, "---:|");
        }
    }
    s.push('\n');
    for n in sec.workload.queries.iter().map(|q| q.name.as_str()) {
        let rcount = rows
            .iter()
            .find_map(|x| x.run.query(n))
            .map(|q| q.rows.to_string());
        let _ = write!(s, "| {n} | {} |", rcount.unwrap_or_else(|| "–".into()));
        for row in rows {
            let q = row.run.query(n);
            let _ = write!(
                s,
                " {} | {} |",
                opt(q.map(|q| q.first_ms), f2),
                opt(q.map(|q| q.median_ms), f2)
            );
            if cmp {
                let bq = row.base.and_then(|x| x.query(n)).map(|q| q.median_ms);
                let _ = write!(s, " {} |", ratio_opt(q.map(|q| q.median_ms), bq));
            }
        }
        s.push('\n');
    }
    s.push('\n');
    s
}

fn update_table(rows: &[Row<'_>]) -> String {
    let mut s = String::new();
    if !rows.iter().any(|x| x.run.update.is_some()) {
        return s;
    }
    let (h, d) = lead_header(rows, "backend");
    let _ = writeln!(
        s,
        "{h} single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |"
    );
    let _ = writeln!(s, "{d}---:|---:|---:|---:|---:|---:|---:|---:|---|");
    for row in rows {
        if let Some(u) = &row.run.update {
            let _ = writeln!(
                s,
                "{} {} / {} | {} / {} | {} | {} | {} | {} | {} | {} → {} | {} |",
                row.lead(),
                opt(u.single_insert_median_ms, f2),
                opt(u.single_insert_p95_ms, f2),
                opt(u.single_delete_median_ms, f2),
                opt(u.single_delete_p95_ms, f2),
                u.chunk_triples,
                f1(u.chunk_insert_ms),
                f1(u.chunk_delete_ms),
                f1(u.checkpoint_ms),
                f1(u.reopen_ms),
                u.entities_before,
                u.entities_after,
                if u.round_trip_ok() {
                    "ok"
                } else {
                    "**FAILED**"
                }
            );
        }
    }
    s.push('\n');
    s
}

fn conc_table(rows: &[Row<'_>], cmp: bool) -> String {
    let mut s = String::new();
    if !rows.iter().any(|x| x.run.conc.is_some()) {
        return s;
    }
    let (h, d) = lead_header(rows, "backend");
    let _ = writeln!(
        s,
        "{h} endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies |{}",
        if cmp { " qps vs base |" } else { "" }
    );
    let _ = writeln!(
        s,
        "{d}---|---|---:|---:|---:|---:|---:|---|{}",
        if cmp { "---:|" } else { "" }
    );
    for row in rows {
        let Some(c) = &row.run.conc else { continue };
        for p in &c.points {
            let bq = row
                .base
                .and_then(|x| x.conc.as_ref())
                .and_then(|bc| {
                    bc.points.iter().find(|x| {
                        x.endpoint == p.endpoint && x.query == p.query && x.clients == p.clients
                    })
                })
                .map(|x| x.qps);
            let _ = writeln!(
                s,
                "{} {} | {} | {} | {} | {} | {} | {} | {} |{}",
                row.lead(),
                p.endpoint,
                p.query,
                p.clients,
                f1(p.qps),
                f2(p.mean_ms),
                f2(p.p95_ms),
                p.http_errors,
                if p.distinct_bodies == 1 {
                    "identical"
                } else {
                    "**differ**"
                },
                if cmp {
                    format!(" {} |", ratio_cell(p.qps, bq))
                } else {
                    String::new()
                }
            );
        }
    }
    s.push('\n');
    s
}

fn mixed_table(rows: &[Row<'_>], cmp: bool) -> String {
    let mut s = String::new();
    if !rows.iter().any(|x| x.run.mixed.is_some()) {
        return s;
    }
    let (h, d) = lead_header(rows, "backend");
    let _ = writeln!(
        s,
        "{h} endpoint | check_point | query | readers | reader qps | mean ms | p95 ms | writes / errors | write mean / p95 ms | bodies |{}",
        if cmp { " qps vs base |" } else { "" }
    );
    let _ = writeln!(
        s,
        "{d}---|---|---|---:|---:|---:|---:|---:|---:|---|{}",
        if cmp { "---:|" } else { "" }
    );
    for row in rows {
        let Some(m) = &row.run.mixed else { continue };
        for p in &m.points {
            let bq = row
                .base
                .and_then(|x| x.mixed.as_ref())
                .and_then(|bm| {
                    bm.points.iter().find(|x| {
                        x.endpoint == p.endpoint
                            && x.query == p.query
                            && x.readers == p.readers
                            && x.check_point == p.check_point
                    })
                })
                .map(|x| x.qps);
            let _ = writeln!(
                s,
                "{} {} | {} | {} | {} | {} | {} | {} | {} / {} | {} / {} | {} |{}",
                row.lead(),
                p.endpoint,
                if p.check_point { "on" } else { "off" },
                p.query,
                p.readers,
                f1(p.qps),
                f2(p.mean_ms),
                f2(p.p95_ms),
                p.writes,
                p.write_errors,
                f2(p.write_mean_ms),
                f2(p.write_p95_ms),
                if p.distinct_bodies == 1 && p.read_errors == 0 {
                    "identical"
                } else {
                    "**differ**"
                },
                if cmp {
                    format!(" {} |", ratio_cell(p.qps, bq))
                } else {
                    String::new()
                }
            );
        }
    }
    s.push('\n');
    let begins: Vec<(&Row<'_>, &crate::schema::BeginPoint)> = rows
        .iter()
        .filter_map(|r| {
            r.run
                .mixed
                .as_ref()
                .and_then(|m| m.begin.as_ref())
                .map(|b| (r, b))
        })
        .collect();
    if !begins.is_empty() {
        let _ = writeln!(
            s,
            "{h} /api begin samples | median ms | p95 ms | max ms | RSS growth first / max MiB |{}",
            if cmp { " median vs base |" } else { "" }
        );
        let _ = writeln!(
            s,
            "{d}---:|---:|---:|---:|---:|{}",
            if cmp { "---:|" } else { "" }
        );
        for (row, b) in begins {
            let base = row
                .base
                .and_then(|x| x.mixed.as_ref())
                .and_then(|m| m.begin.as_ref())
                .map(|x| x.median_ms);
            let _ = writeln!(
                s,
                "{} {} | {} | {} | {} | {} / {} |{}",
                row.lead(),
                b.samples,
                f2(b.median_ms),
                f2(b.p95_ms),
                f2(b.max_ms),
                opt(b.rss_growth_first_mib, f1),
                opt(b.rss_growth_max_mib, f1),
                if cmp {
                    format!(" {} |", ratio_cell(b.median_ms, base))
                } else {
                    String::new()
                }
            );
        }
        s.push('\n');
    }
    s
}

/// Geometric mean of a run's warm query medians.
pub fn geomean_ms(run: &BackendRun) -> Option<f64> {
    if run.queries.is_empty() {
        return None;
    }
    let l: f64 = run.queries.iter().map(|q| q.median_ms.max(1e-3).ln()).sum();
    Some((l / run.queries.len() as f64).exp())
}

/// A Markdown table of gate outcomes: failures first, then (unless
/// `failures_only`) the timing, memory, size and report-only gates that
/// passed.
pub fn gates_markdown(outcomes: &[GateOutcome], failures_only: bool) -> String {
    let mut s = String::new();
    let v = verdict(outcomes);
    let failed = outcomes.iter().filter(|o| !o.pass).count();
    let new = outcomes
        .iter()
        .filter(|o| o.gate == "info.new" && o.subject.ends_with("workload"))
        .count();
    let _ = writeln!(
        s,
        "### Gates: {} ({} checked, {} failed{})\n",
        match v {
            Verdict::Pass => "PASS",
            Verdict::PerfFail => "FAIL (performance)",
            Verdict::ExactFail => "FAIL (exact)",
        },
        outcomes.len(),
        failed,
        if new > 0 {
            format!(", {new} new workload(s) not in the baseline")
        } else {
            String::new()
        }
    );
    let mut rows: Vec<&GateOutcome> = outcomes
        .iter()
        .filter(|o| !o.pass || (!failures_only && o.class != GateClass::Exact))
        .collect();
    rows.sort_by(|a, b| a.pass.cmp(&b.pass).then(a.class.cmp(&b.class)));
    if rows.is_empty() {
        return s;
    }
    let _ = writeln!(
        s,
        "| result | gate | subject | base | measured | ratio | limit | detail |"
    );
    let _ = writeln!(s, "|---|---|---|---:|---:|---:|---:|---|");
    for o in rows {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            if !o.pass {
                "**FAIL**"
            } else if o.gate == "info.new" {
                "new"
            } else {
                "pass"
            },
            o.gate,
            o.subject,
            opt(o.base, f2),
            opt(o.measured, f2),
            opt(o.ratio, |x| format!("{x:.2}")),
            opt(o.limit, |x| format!("{x:.2}")),
            o.detail.replace('|', "\\|").replace('\n', " ")
        );
    }
    s.push('\n');
    s
}

// ---------------------------------------------------------------------------
// Tables over several results, and document splicing
// ---------------------------------------------------------------------------

/// The tables `report --table` knows.
pub const TABLES: [&str; 8] = [
    "overview", "queries", "build", "update", "conc", "mixed", "gates", "full",
];

/// Render `kind` over `results` (each with its baseline, if any). Every
/// workload of every result contributes its runs; multi-workload tables
/// lead with a `workload` column (the workload's label).
pub fn table(
    kind: &str,
    results: &[(BenchResult, Option<BenchResult>)],
    t: &Thresholds,
) -> Result<String, String> {
    let cmp = results.iter().any(|(_, b)| b.is_some());
    let mut rows: Vec<Row<'_>> = Vec::new();
    let mut secs: Vec<(Section<'_>, usize, usize)> = Vec::new();
    for (r, b) in results {
        for sec in r.sections() {
            let bs = base_section(b.as_ref(), &sec);
            let start = rows.len();
            for run in sec.runs {
                rows.push(Row {
                    workload: Some(sec.workload.spec.label()),
                    run,
                    base: bs.and_then(|x| matching_run(x.runs, run)),
                });
            }
            secs.push((sec, start, rows.len()));
        }
    }
    Ok(match kind {
        "overview" => overview(&rows, cmp),
        "build" => build_table(&rows),
        "update" => update_table(&rows),
        "conc" => conc_table(&rows, cmp),
        "mixed" => mixed_table(&rows, cmp),
        "queries" => {
            let mut s = String::new();
            let many = secs.len() > 1;
            for (sec, a, z) in &secs {
                let sub: Vec<Row<'_>> = rows[*a..*z]
                    .iter()
                    .map(|x| Row {
                        workload: None,
                        run: x.run,
                        base: x.base,
                    })
                    .collect();
                if many {
                    let _ = writeln!(s, "**{}**\n", sec.workload.spec.label());
                }
                s.push_str(&queries(sec, &sub, cmp));
            }
            s
        }
        "gates" => {
            let mut s = String::new();
            for (r, b) in results {
                let b = b
                    .as_ref()
                    .ok_or("--table gates needs a --compare baseline per result")?;
                s.push_str(&gates_markdown(&evaluate(b, r, t, 1.0), false));
            }
            s
        }
        "full" => {
            let mut s = String::new();
            for (r, b) in results {
                s.push_str(&markdown(r, b.as_ref()));
                if let Some(b) = b {
                    s.push_str(&gates_markdown(&evaluate(b, r, t, 1.0), false));
                }
            }
            s
        }
        other => return Err(format!("unknown table '{other}' ({})", TABLES.join(", "))),
    })
}

/// The output of `gstore-bench report ARGS` (files read relative to the
/// current directory): `[--table KIND] RESULT... [--compare BASELINE]...`
/// — no `--compare`, one for every result, or one per result in order.
pub fn render_args(args: &[String], t: &Thresholds) -> Result<String, String> {
    let mut kind = "full".to_string();
    let mut files = Vec::new();
    let mut bases = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut value = |name: &str| -> Result<String, String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a {
            "--table" => kind = value("--table")?,
            "--compare" => bases.push(value("--compare")?),
            "--markdown" => {}
            s if s.starts_with("--table=") => kind = s["--table=".len()..].to_string(),
            s if s.starts_with("--compare=") => bases.push(s["--compare=".len()..].to_string()),
            s if s.starts_with("--") => return Err(format!("report: unknown option '{s}'")),
            s => files.push(s.to_string()),
        }
        i += 1;
    }
    if files.is_empty() {
        return Err("report needs at least one RESULT".into());
    }
    if !(bases.is_empty() || bases.len() == 1 || bases.len() == files.len()) {
        return Err(format!(
            "report: {} --compare for {} results (give none, one, or one per result)",
            bases.len(),
            files.len()
        ));
    }
    let mut results = Vec::new();
    for (k, f) in files.iter().enumerate() {
        let r = BenchResult::load(Path::new(f))?;
        let b = match bases.len() {
            0 => None,
            1 => Some(BenchResult::load(Path::new(&bases[0]))?),
            _ => Some(BenchResult::load(Path::new(&bases[k]))?),
        };
        results.push((r, b));
    }
    if kind == "full" && results.len() == 1 && results[0].1.is_none() {
        return Ok(markdown(&results[0].0, None));
    }
    table(&kind, &results, t)
}

/// The marker that opens a generated region: `<!-- gstore-bench report
/// ARGS -->`, where ARGS are `report`'s arguments.
pub const BEGIN: &str = "<!-- gstore-bench report ";
/// The marker that closes it.
pub const END: &str = "<!-- /gstore-bench report -->";

/// Regenerate every marked region of `doc` with `render` (which receives the
/// region's arguments). Returns the new document and the arguments of the
/// regions whose content changed.
///
/// A marker counts only where it starts a line (after optional spaces) and
/// outside a fenced code block (```` ``` ```` or `~~~`): text that shows
/// the marker syntax — in a code span, a table cell, a fenced example — is
/// left alone. The closing marker is the first one that starts a line
/// after the opening one (or follows it on the same line).
pub fn splice(
    doc: &str,
    render: &mut dyn FnMut(&[String]) -> Result<String, String>,
) -> Result<(String, Vec<String>), String> {
    let mut out = String::with_capacity(doc.len());
    let mut changed = Vec::new();
    // `doc[..copied]` is in `out` already.
    let mut copied = 0;
    let mut fence: Option<&str> = None;
    let mut lines = line_offsets(doc);
    while let Some((at, line)) = lines.next() {
        let trimmed = line.trim_start();
        if let Some(f) = fence {
            if trimmed.starts_with(f) {
                fence = None;
            }
            continue;
        }
        if let Some(f) = ["```", "~~~"].into_iter().find(|f| trimmed.starts_with(f)) {
            fence = Some(f);
            continue;
        }
        if !trimmed.starts_with(BEGIN) {
            continue;
        }
        let start = at + (line.len() - trimmed.len());
        let open_end = trimmed
            .find("-->")
            .map(|e| start + e + 3)
            .ok_or("an unterminated `<!-- gstore-bench report` marker")?;
        let args_text = doc[start + BEGIN.len()..open_end - 3].trim().to_string();
        let line_end = at + line.len();
        let close = match doc[open_end..line_end].find(END) {
            Some(e) => Some(open_end + e),
            None => loop {
                let Some((at2, line2)) = lines.next() else {
                    break None;
                };
                let t2 = line2.trim_start();
                if t2.starts_with(END) {
                    break Some(at2 + (line2.len() - t2.len()));
                }
                if t2.starts_with(BEGIN) {
                    return Err(format!("nested markers after `{BEGIN}{args_text} -->`"));
                }
            },
        };
        let close = close.ok_or_else(|| format!("no `{END}` after `{BEGIN}{args_text} -->`"))?;
        let args: Vec<String> = args_text.split_whitespace().map(str::to_string).collect();
        let mut body = render(&args)?;
        while body.ends_with("\n\n") {
            body.pop();
        }
        if !body.ends_with('\n') {
            body.push('\n');
        }
        let fresh = format!("\n{body}");
        if doc[open_end..close] != fresh {
            changed.push(args_text.clone());
        }
        out.push_str(&doc[copied..open_end]);
        out.push_str(&fresh);
        out.push_str(END);
        copied = close + END.len();
    }
    out.push_str(&doc[copied..]);
    Ok((out, changed))
}

/// The lines of `doc` (each with its line break) and their byte offsets.
fn line_offsets(doc: &str) -> impl Iterator<Item = (usize, &str)> {
    doc.split_inclusive('\n').scan(0, |at, line| {
        let here = *at;
        *at += line.len();
        Some((here, line))
    })
}
