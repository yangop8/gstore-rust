//! Markdown reports of results, comparisons and gate outcomes.

use std::fmt::Write as _;

use crate::gates::{verdict, GateClass, GateOutcome, Verdict};
use crate::schema::{BackendRun, BenchResult};

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
        "- generator `{}`, data fnv `{}`, {} lines, {:.1} MB",
        w.version,
        w.data_fnv64,
        w.lines,
        w.bytes as f64 / 1e6
    );
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
    s.push('\n');

    // Overview per backend.
    let _ = writeln!(
        s,
        "| backend | build s | build peak MiB | dir MB | open first ms | open warm ms | RSS after open MiB | query HWM MiB | geomean ms |{}",
        if base.is_some() { " geomean vs base |" } else { "" }
    );
    let _ = writeln!(
        s,
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|{}",
        if base.is_some() { "---:|" } else { "" }
    );
    for run in &r.runs {
        let b = base.and_then(|b| b.run(&run.backend));
        let g = geomean_ms(run);
        let gb = b.map(geomean_ms);
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {}{} | {} | {} | {} | {} |{}",
            run.backend,
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
            match (g, gb.flatten()) {
                (Some(g), Some(gb)) if base.is_some() => format!(" {} |", ratio_cell(g, Some(gb))),
                _ if base.is_some() => " – |".into(),
                _ => String::new(),
            }
        );
    }
    s.push('\n');

    // Queries: one column per backend.
    if !r.runs.is_empty() {
        let names: Vec<&str> = r.workload.queries.iter().map(|q| q.name.as_str()).collect();
        let _ = write!(s, "| query | rows |");
        for run in &r.runs {
            let _ = write!(s, " {} first ms | {} median ms |", run.backend, run.backend);
            if base.is_some() {
                let _ = write!(s, " {} vs base |", run.backend);
            }
        }
        s.push('\n');
        let _ = write!(s, "|---|---:|");
        for _ in &r.runs {
            let _ = write!(s, "---:|---:|");
            if base.is_some() {
                let _ = write!(s, "---:|");
            }
        }
        s.push('\n');
        for n in names {
            let rows = r
                .runs
                .iter()
                .find_map(|x| x.query(n))
                .map(|q| q.rows.to_string());
            let _ = write!(s, "| {n} | {} |", rows.unwrap_or_else(|| "–".into()));
            for run in &r.runs {
                let q = run.query(n);
                let _ = write!(
                    s,
                    " {} | {} |",
                    opt(q.map(|q| q.first_ms), f2),
                    opt(q.map(|q| q.median_ms), f2)
                );
                if let Some(b) = base {
                    let bq = b
                        .run(&run.backend)
                        .and_then(|x| x.query(n))
                        .map(|q| q.median_ms);
                    let _ = write!(
                        s,
                        " {} |",
                        q.map(|q| ratio_cell(q.median_ms, bq))
                            .unwrap_or_else(|| "–".into())
                    );
                }
            }
            s.push('\n');
        }
        s.push('\n');
    }

    // Updates.
    if r.runs.iter().any(|x| x.update.is_some()) {
        let _ = writeln!(
            s,
            "| backend | single insert median / p95 ms | single delete median / p95 ms | chunk triples | chunk insert ms | chunk delete ms | checkpoint ms | reopen ms | entities before → after | round trip |"
        );
        let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|---:|---:|---:|---|");
        for run in &r.runs {
            if let Some(u) = &run.update {
                let _ = writeln!(
                    s,
                    "| {} | {} / {} | {} / {} | {} | {} | {} | {} | {} | {} → {} | {} |",
                    run.backend,
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
    }

    // Concurrency.
    if r.runs.iter().any(|x| x.conc.is_some()) {
        let _ = writeln!(
            s,
            "| backend | endpoint | query | clients | qps | mean ms | p95 ms | errors | bodies |{}",
            if base.is_some() { " qps vs base |" } else { "" }
        );
        let _ = writeln!(
            s,
            "|---|---|---|---:|---:|---:|---:|---:|---|{}",
            if base.is_some() { "---:|" } else { "" }
        );
        for run in &r.runs {
            if let Some(c) = &run.conc {
                for p in &c.points {
                    let bq = base
                        .and_then(|b| b.run(&run.backend))
                        .and_then(|x| x.conc.as_ref())
                        .and_then(|bc| {
                            bc.points.iter().find(|x| {
                                x.endpoint == p.endpoint
                                    && x.query == p.query
                                    && x.clients == p.clients
                            })
                        })
                        .map(|x| x.qps);
                    let _ = writeln!(
                        s,
                        "| {} | {} | {} | {} | {} | {} | {} | {} | {} |{}",
                        run.backend,
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
                        if base.is_some() {
                            format!(" {} |", ratio_cell(p.qps, bq))
                        } else {
                            String::new()
                        }
                    );
                }
            }
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
/// `failures_only`) the timing, memory and size gates that passed.
pub fn gates_markdown(outcomes: &[GateOutcome], failures_only: bool) -> String {
    let mut s = String::new();
    let v = verdict(outcomes);
    let failed = outcomes.iter().filter(|o| !o.pass).count();
    let _ = writeln!(
        s,
        "### Gates: {} ({} checked, {} failed)\n",
        match v {
            Verdict::Pass => "PASS",
            Verdict::PerfFail => "FAIL (performance)",
            Verdict::ExactFail => "FAIL (exact)",
        },
        outcomes.len(),
        failed
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
            if o.pass { "pass" } else { "**FAIL**" },
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
