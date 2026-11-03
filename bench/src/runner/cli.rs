//! The `gstore-bench` command line. Exit codes: 0 pass, 1 a gate failed,
//! 2 usage error, crash or timeout (never a pass).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use crate::gates::{evaluate, retry_targets, self_check, verdict, Thresholds, Verdict};
use crate::hash::hex;
use crate::report;
use crate::runner::conc::{ConcSettings, MixedSettings};
use crate::runner::{profile, run_child, ChildRequest, RunOptions, Runner};
use crate::schema::BenchResult;
use crate::workload::{generate_to, WorkloadSpec};

const USAGE: &str = "\
gstore-bench — the gstore-rust benchmark harness (see bench/README.md)

usage:
  gstore-bench gen [--workload lubm|watdiv|tiny] [--scale N] [--seed S] [--departments D] [--shrink K]
                   [--chunk] [--out FILE|-]
  gstore-bench queries [--workload watdiv] [--scale N] [--seed S] [--shrink K] [--data-dir DIR]
  gstore-bench run [--profile micro|smoke|reference] [RUN OPTIONS] [--out FILE]
  gstore-bench check [--baseline FILE] [--from RESULT] [--thresholds FILE] [--self-test-slowdown X]
                     [--summary FILE] [RUN OPTIONS] [--out FILE]
  gstore-bench ci [--baseline bench/baselines/smoke.json] [--summary FILE] [RUN OPTIONS] [--out FILE]
  gstore-bench compare BASELINE RESULT [--thresholds FILE] [--self-test-slowdown X] [--backends a,b]
                       [--markdown] [--summary FILE]
  gstore-bench report RESULT... [--markdown] [--table KIND] [--compare BASELINE]...
  gstore-bench report --update DOC... | --check DOC...

  check / compare: --backends also limits which of the baseline's backends are gated.
  report --table: overview, queries, build, update, conc, mixed, gates, full (default); one
  --compare for all results or one per result. --update / --check regenerate / verify the
  tables between `<!-- gstore-bench report ARGS -->` and `<!-- /gstore-bench report -->`.
  gstore-bench bless --to FILE [--from RESULT] [--provisional] [--note TEXT] [RUN OPTIONS]
  gstore-bench merge --out FILE RESULT RESULT...
  gstore-bench calibrate [--quick]

run options:
  --profile P            micro (tests), smoke (CI: univ 1 + WatDiv-like 1, snapshot+disk),
                         reference (univ 8/38 + WatDiv-like 10)
  --workload W --scale N --seed S --departments D --shrink K
  --extra none|watdiv:10[,…]  the profile's further workloads (an explicit --workload drops them)
  --label L              key every run as backend@L (e.g. one backend in two modes, then merge)
  --backends a,b         snapshot, disk, rocksdb (rocksdb needs --features rocksdb)
  --reps N --query-procs N --open-reps N --singles N --no-update
  --no-conc --conc-clients 1,8 --conc-secs S --conc-queries q1,q4 --conc-endpoints sparql,api
  --no-mixed --mixed-readers N --mixed-write-ms MS --mixed-secs S --mixed-checkpoint off,on
  --mixed-endpoints sparql,api --mixed-query Q   (--no-conc drops the mixed phase too)
  --timeout-s N          per-phase timeout (default from the profile)
  --bin-dir DIR          where gbuild is (default target/release)
  --data-dir DIR         generated-data cache (default $GSTORE_BENCH_CACHE or target/gstore-bench/data)
  --work-dir DIR         databases and logs (default $GSTORE_BENCH_WORK or target/gstore-bench/work)
  --keep-db              keep the databases
  --env K=V              environment for every phase (repeatable)
  --gbuild-arg X         extra gbuild argument (repeatable)
  --drop-caches          drop the page cache before the first open (root on Linux)
  --no-calibrate --verify-data --quiet

exit codes: 0 pass, 1 a gate failed, 2 usage error / crash / timeout.
";

/// Simple argument scanner.
struct Args {
    items: Vec<String>,
}

impl Args {
    fn flag(&mut self, name: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|a| a != name);
        before != self.items.len()
    }

    fn values(&mut self, name: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        let mut i = 0;
        let prefix = format!("{name}=");
        while i < self.items.len() {
            if self.items[i] == name {
                if i + 1 >= self.items.len() {
                    return Err(format!("{name} needs a value"));
                }
                out.push(self.items.remove(i + 1));
                self.items.remove(i);
            } else if let Some(v) = self.items[i].strip_prefix(&prefix) {
                out.push(v.to_string());
                self.items.remove(i);
            } else {
                i += 1;
            }
        }
        Ok(out)
    }

    fn value(&mut self, name: &str) -> Result<Option<String>, String> {
        Ok(self.values(name)?.pop())
    }

    fn parsed<T: std::str::FromStr>(&mut self, name: &str) -> Result<Option<T>, String> {
        match self.value(name)? {
            Some(v) => v
                .parse()
                .map(Some)
                .map_err(|_| format!("{name}: invalid value '{v}'")),
            None => Ok(None),
        }
    }

    fn list(&mut self, name: &str) -> Result<Option<Vec<String>>, String> {
        Ok(self.value(name)?.map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        }))
    }

    fn finish(self) -> Result<Vec<String>, String> {
        if let Some(o) = self.items.iter().find(|a| a.starts_with("--")) {
            return Err(format!("unknown option '{o}'"));
        }
        Ok(self.items)
    }
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("gstore-bench: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("gstore-bench: error: {msg}");
    ExitCode::from(2)
}

/// Apply the workload options to `spec`. A `--workload` other than `spec`'s
/// family starts from `default_of(family)` (a profile's own spec of that
/// family, e.g. the reference profile's WatDiv scale 10).
fn workload_options(
    a: &mut Args,
    spec: &mut WorkloadSpec,
    default_of: &dyn Fn(&str) -> Option<WorkloadSpec>,
) -> Result<(), String> {
    if let Some(w) = a.value("--workload")? {
        let w = match w.as_str() {
            "lubm-like" => "lubm".to_string(),
            "watdiv-like" => "watdiv".to_string(),
            _ => w,
        };
        if w != spec.family() {
            *spec = match default_of(&w) {
                Some(s) => s,
                None => match w.as_str() {
                    "tiny" => WorkloadSpec::tiny(),
                    "lubm" => WorkloadSpec::lubm(1, 0),
                    "watdiv" => WorkloadSpec::watdiv(1, 0),
                    other => {
                        return Err(format!("unknown workload '{other}' (lubm, watdiv, tiny)"))
                    }
                },
            };
        }
    }
    if let Some(n) = a.parsed::<u32>("--scale")? {
        spec.scale = n;
    }
    if let Some(s) = a.parsed::<u64>("--seed")? {
        spec.seed = s;
    }
    if let Some(d) = a.parsed::<u32>("--departments")? {
        spec.departments = Some(d);
    }
    if let Some(k) = a.parsed::<u32>("--shrink")? {
        spec.shrink = Some(k);
    }
    Ok(())
}

/// Parse the run options on top of `base` (a built-in profile).
fn run_options(a: &mut Args, default_profile: &str) -> Result<RunOptions, String> {
    let pname = a
        .value("--profile")?
        .unwrap_or_else(|| default_profile.to_string());
    let mut p = profile(&pname)
        .ok_or_else(|| format!("unknown profile '{pname}' (micro, smoke, reference)"))?;
    let explicit_workload = a
        .items
        .iter()
        .any(|x| x == "--workload" || x.starts_with("--workload="));
    let extras = p.extra.clone();
    workload_options(a, &mut p.spec, &|fam| {
        extras.iter().find(|s| s.family() == fam).cloned()
    })?;
    if explicit_workload {
        // `--workload X` measures X alone (unless --extra says otherwise).
        p.extra.clear();
    }
    if let Some(list) = a.list("--extra")? {
        p.extra = parse_extra(&list)?;
    }
    if let Some(b) = a.list("--backends")? {
        p.backends = b;
    }
    if let Some(n) = a.parsed("--reps")? {
        p.reps = n;
    }
    if let Some(n) = a.parsed::<usize>("--query-procs")? {
        if n == 0 {
            return Err("--query-procs must be at least 1".into());
        }
        p.query_procs = n;
    }
    if let Some(n) = a.parsed("--open-reps")? {
        p.open_reps = n;
    }
    if let Some(n) = a.parsed("--singles")? {
        p.singles = n;
    }
    if a.flag("--no-update") {
        p.update = false;
    }
    let no_conc = a.flag("--no-conc");
    let clients = a.list("--conc-clients")?;
    let secs: Option<f64> = a.parsed("--conc-secs")?;
    let queries = a.list("--conc-queries")?;
    let endpoints = a.list("--conc-endpoints")?;
    if no_conc {
        p.conc = None;
    } else if clients.is_some() || secs.is_some() || queries.is_some() || endpoints.is_some() {
        let mut c = p.conc.clone().unwrap_or(ConcSettings {
            clients: vec![1, 8],
            secs: 5.0,
            queries: Vec::new(),
            endpoints: vec!["sparql".into(), "api".into()],
        });
        if let Some(cl) = clients {
            c.clients = cl
                .iter()
                .map(|x| {
                    x.parse()
                        .map_err(|_| format!("--conc-clients: invalid '{x}'"))
                })
                .collect::<Result<_, _>>()?;
        }
        if let Some(s) = secs {
            c.secs = s;
        }
        if let Some(q) = queries {
            c.queries = q;
        }
        if let Some(e) = endpoints {
            c.endpoints = e;
        }
        p.conc = Some(c);
    }
    let no_mixed = a.flag("--no-mixed");
    let readers: Option<usize> = a.parsed("--mixed-readers")?;
    let write_ms: Option<u64> = a.parsed("--mixed-write-ms")?;
    let msecs: Option<f64> = a.parsed("--mixed-secs")?;
    let cps = a.list("--mixed-checkpoint")?;
    let mends = a.list("--mixed-endpoints")?;
    let mquery = a.value("--mixed-query")?;
    if no_conc || no_mixed {
        p.mixed = None;
    } else if readers.is_some()
        || write_ms.is_some()
        || msecs.is_some()
        || cps.is_some()
        || mends.is_some()
        || mquery.is_some()
    {
        let mut m = p.mixed.clone().unwrap_or(MixedSettings {
            readers: 8,
            write_ms: 100,
            secs: 5.0,
            check_point: vec![false, true],
            endpoints: vec!["sparql".into(), "api".into()],
            query: None,
        });
        if let Some(r) = readers {
            m.readers = r.max(1);
        }
        if let Some(w) = write_ms {
            m.write_ms = w.max(1);
        }
        if let Some(s) = msecs {
            m.secs = s;
        }
        if let Some(c) = cps {
            m.check_point = c
                .iter()
                .map(|x| match x.as_str() {
                    "off" => Ok(false),
                    "on" => Ok(true),
                    other => Err(format!("--mixed-checkpoint: '{other}' (off, on)")),
                })
                .collect::<Result<_, _>>()?;
        }
        if let Some(e) = mends {
            m.endpoints = e;
        }
        if mquery.is_some() {
            m.query = mquery;
        }
        p.mixed = Some(m);
    }
    if let Some(t) = a.parsed::<u64>("--timeout-s")? {
        p.timeout_s = t;
        p.timeout_build_s = t;
    }
    if pname != "micro" && pname != "smoke" && pname != "reference" {
        p.name = "custom".into();
    }
    let mut o = RunOptions::new(p);
    if let Some(d) = a.value("--bin-dir")? {
        o.bin_dir = PathBuf::from(d);
    }
    if let Some(b) = a.value("--builder")? {
        o.builder = b;
    }
    if let Some(d) = a.value("--data-dir")? {
        o.data_dir = PathBuf::from(d);
    }
    if let Some(d) = a.value("--work-dir")? {
        o.work_dir = PathBuf::from(d);
    }
    o.keep_db = a.flag("--keep-db");
    for kv in a.values("--env")? {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("--env expects K=V, got '{kv}'"))?;
        o.env.push((k.to_string(), v.to_string()));
    }
    o.gbuild_args = a.values("--gbuild-arg")?;
    o.drop_caches = a.flag("--drop-caches");
    o.calibrate = !a.flag("--no-calibrate");
    o.verify_data = a.flag("--verify-data");
    o.quiet = a.flag("--quiet");
    o.inject = a.value("--inject")?;
    o.label = a.value("--label")?;
    if let Some(l) = &o.label {
        if l.is_empty() || l.contains(['@', '/', '|', ' ', ':']) {
            return Err(format!(
                "--label '{l}': use a short word (no '@', '/', '|', ':' or spaces)"
            ));
        }
    }
    Ok(o)
}

/// `--extra none` or `family:scale` items (`watdiv:10`; `watdiv:1/10` also
/// shrinks the WatDiv-like workload by 10, for tests).
fn parse_extra(list: &[String]) -> Result<Vec<WorkloadSpec>, String> {
    if list.len() == 1 && list[0] == "none" {
        return Ok(Vec::new());
    }
    list.iter()
        .map(|item| {
            let (fam, scale) = item.split_once(':').unwrap_or((item.as_str(), "1"));
            let (scale, shrink) = match scale.split_once('/') {
                Some((s, k)) => (
                    s,
                    Some(
                        k.parse::<u32>()
                            .map_err(|_| format!("--extra: invalid shrink in '{item}'"))?,
                    ),
                ),
                None => (scale, None),
            };
            let scale: u32 = scale
                .parse()
                .map_err(|_| format!("--extra: invalid scale in '{item}'"))?;
            let spec = match fam {
                "watdiv" | "watdiv-like" => WorkloadSpec {
                    shrink,
                    ..WorkloadSpec::watdiv(scale, 0)
                },
                "lubm" | "lubm-like" => WorkloadSpec::lubm(scale, 0),
                "tiny" => WorkloadSpec {
                    scale,
                    ..WorkloadSpec::tiny()
                },
                other => return Err(format!("--extra: unknown workload '{other}'")),
            };
            spec.instantiate()?;
            Ok(spec)
        })
        .collect()
}

fn default_out(o: &RunOptions) -> PathBuf {
    let stem = o
        .profile
        .spec
        .instantiate()
        .map(|w| w.file_stem())
        .unwrap_or_else(|_| "result".into());
    PathBuf::from(format!(
        "target/gstore-bench/{}-{stem}.json",
        o.profile.name
    ))
}

fn append_summary(path: Option<&Path>, text: &str) {
    if let Some(p) = path {
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
        {
            Ok(mut f) => {
                let _ = f.write_all(text.as_bytes());
            }
            Err(e) => eprintln!("gstore-bench: cannot write {}: {e}", p.display()),
        }
    }
}

fn exit_for(v: Verdict) -> ExitCode {
    match v {
        Verdict::Pass => ExitCode::SUCCESS,
        _ => ExitCode::from(1),
    }
}

/// `main` of the `gstore-bench` binary.
pub fn main() -> ExitCode {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    }
    let cmd = argv.remove(0);
    let mut a = Args { items: argv };
    let r = match cmd.as_str() {
        super::child::CHILD_COMMAND => return super::child::child_main(),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        "gen" => cmd_gen(&mut a),
        "queries" => cmd_queries(&mut a),
        "run" => cmd_run(&mut a),
        "check" => cmd_check(&mut a, false),
        "ci" => cmd_check(&mut a, true),
        "compare" => cmd_compare(&mut a),
        "report" => cmd_report(&mut a),
        "bless" => cmd_bless(&mut a),
        "merge" => cmd_merge(&mut a),
        "calibrate" => cmd_calibrate(&mut a),
        other => return usage_error(&format!("unknown command '{other}'")),
    };
    match r {
        Ok(code) => code,
        Err(Error::Usage(m)) => usage_error(&m),
        Err(Error::Failed(m)) => fail(&m),
    }
}

enum Error {
    Usage(String),
    Failed(String),
}

type CmdResult = Result<ExitCode, Error>;

fn usage<T>(r: Result<T, String>) -> Result<T, Error> {
    r.map_err(Error::Usage)
}

fn failed<T>(r: Result<T, String>) -> Result<T, Error> {
    r.map_err(Error::Failed)
}

fn no_default(_: &str) -> Option<WorkloadSpec> {
    None
}

fn cmd_gen(a: &mut Args) -> CmdResult {
    let mut spec = WorkloadSpec::lubm(1, 0);
    usage(workload_options(a, &mut spec, &no_default))?;
    let chunk = a.flag("--chunk");
    let out = usage(a.value("--out"))?.unwrap_or_else(|| "-".into());
    usage(std::mem::replace(a, Args { items: Vec::new() }).finish())?;
    let w = usage(spec.instantiate())?;
    let t = Instant::now();
    let stats = if out == "-" {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        failed(generate_to(w.as_ref(), &mut lock, chunk).map_err(|e| e.to_string()))?
    } else {
        let path = PathBuf::from(&out);
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            failed(std::fs::create_dir_all(dir).map_err(|e| e.to_string()))?;
        }
        let mut f = failed(std::fs::File::create(&path).map_err(|e| format!("{out}: {e}")))?;
        failed(generate_to(w.as_ref(), &mut f, chunk).map_err(|e| e.to_string()))?
    };
    let secs = t.elapsed().as_secs_f64();
    eprintln!(
        "{} {} {}lines={} unique_triples={} bytes={} fnv64={} secs={:.3} triples_per_s={:.0}",
        w.version(),
        spec.label(),
        if chunk { "chunk " } else { "" },
        stats.lines,
        stats.unique_triples,
        stats.bytes,
        hex(stats.fnv64),
        secs,
        stats.lines as f64 / secs.max(1e-9)
    );
    Ok(ExitCode::SUCCESS)
}

/// `queries`: the workload's queries as the runs use them (WatDiv templates
/// bound against the cached data, generating and binding when needed).
fn cmd_queries(a: &mut Args) -> CmdResult {
    let mut spec = WorkloadSpec::watdiv(1, 0);
    usage(workload_options(a, &mut spec, &no_default))?;
    let mut o = RunOptions::new(usage(
        profile("micro").ok_or_else(|| "no micro profile".to_string()),
    )?);
    o.profile.spec = spec;
    o.profile.backends = Vec::new();
    o.calibrate = false;
    if let Some(d) = usage(a.value("--data-dir"))? {
        o.data_dir = PathBuf::from(d);
    }
    if let Some(d) = usage(a.value("--work-dir"))? {
        o.work_dir = PathBuf::from(d);
    }
    o.verify_data = a.flag("--verify-data");
    o.quiet = a.flag("--quiet");
    usage(std::mem::replace(a, Args { items: Vec::new() }).finish())?;
    let runner = failed(Runner::prepare(o))?;
    let s = runner.main();
    let mut out = String::new();
    for q in s.workload.queries() {
        let b = s.bound.iter().find(|b| b.name == q.name);
        out.push_str(&format!("# {}", q.name));
        if let Some(b) = b {
            for (var, value) in &b.bindings {
                out.push_str(&format!(" {var}={value}"));
            }
            out.push_str(&format!(" (ASK tries: {})", b.tries));
        }
        out.push('\n');
        out.push_str(&q.text);
        if !q.text.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    runner.finish(true);
    print!("{out}");
    Ok(ExitCode::SUCCESS)
}

fn print_self_check(result: &BenchResult) -> Verdict {
    let outcomes = self_check(result);
    let v = verdict(&outcomes);
    if v != Verdict::Pass {
        eprint!("{}", report::gates_markdown(&outcomes, true));
    }
    v
}

fn cmd_run(a: &mut Args) -> CmdResult {
    let o = usage(run_options(a, "smoke"))?;
    let out = usage(a.value("--out"))?
        .map(PathBuf::from)
        .unwrap_or_else(|| default_out(&o));
    let note = usage(a.value("--note"))?;
    usage(std::mem::replace(a, Args { items: Vec::new() }).finish())?;
    let mut runner = failed(Runner::prepare(o))?;
    let r = runner.run_all(false);
    runner.finish(r.is_ok());
    let mut result = failed(r)?;
    result.note = note;
    failed(result.save(&out))?;
    if !runner.main().opts.quiet {
        print!("{}", report::markdown(&result, None));
    }
    eprintln!("[gstore-bench] wrote {}", out.display());
    Ok(exit_for(print_self_check(&result)))
}

fn thresholds(a: &mut Args) -> Result<Thresholds, Error> {
    match usage(a.value("--thresholds"))? {
        Some(p) => failed(Thresholds::load(Path::new(&p))),
        None => {
            let p = Path::new("bench/thresholds.json");
            if p.is_file() {
                failed(Thresholds::load(p))
            } else {
                Ok(Thresholds::default())
            }
        }
    }
}

/// The distinct backends of `runs`, in order.
fn backends_of(runs: &[crate::schema::BackendRun]) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for r in runs {
        if !v.contains(&r.backend) {
            v.push(r.backend.clone());
        }
    }
    v
}

fn cmd_check(a: &mut Args, ci: bool) -> CmdResult {
    let baseline_path =
        usage(a.value("--baseline"))?.unwrap_or_else(|| "bench/baselines/smoke.json".into());
    let mut baseline = failed(BenchResult::load(Path::new(&baseline_path)))?;
    let t = thresholds(a)?;
    let slowdown: f64 = usage(a.parsed("--self-test-slowdown"))?.unwrap_or(1.0);
    let from = usage(a.value("--from"))?;
    // `--backends` also limits which of the baseline's backends are gated.
    let backends_arg: Option<Vec<String>> = a
        .items
        .iter()
        .position(|x| x == "--backends")
        .and_then(|i| a.items.get(i + 1))
        .or_else(|| a.items.iter().find(|x| x.starts_with("--backends=")))
        .map(|v| {
            v.trim_start_matches("--backends=")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        });
    if let Some(keep) = &backends_arg {
        baseline.retain_runs(keep);
    }
    let summary = usage(a.value("--summary"))?
        .or_else(|| {
            if ci {
                std::env::var("GITHUB_STEP_SUMMARY").ok()
            } else {
                None
            }
        })
        .map(PathBuf::from);
    let out_arg = usage(a.value("--out"))?;

    let (result, outcomes) = if let Some(f) = from {
        usage(a.value("--backends"))?;
        usage(std::mem::replace(a, Args { items: Vec::new() }).finish())?;
        let r = failed(BenchResult::load(Path::new(&f)))?;
        let o = evaluate(&baseline, &r, &t, slowdown);
        (r, o)
    } else {
        // Measure what the baseline measured, unless options say otherwise.
        // The profile's extra workloads the baseline lacks are measured too
        // and reported as new (not gated).
        let given = |keys: &[&str]| {
            a.items.iter().any(|x| {
                keys.iter()
                    .any(|k| x == k || x.starts_with(&format!("{k}=")))
            })
        };
        let has_profile = given(&["--profile"]);
        let explicit_spec = given(&[
            "--workload",
            "--scale",
            "--seed",
            "--departments",
            "--shrink",
        ]);
        let explicit_extra = given(&["--extra"]);
        let explicit_backends = given(&["--backends"]);
        let default_profile = if profile(&baseline.profile).is_some() {
            baseline.profile.clone()
        } else {
            "smoke".into()
        };
        let mut o = usage(run_options(a, &default_profile))?;
        if !has_profile && !explicit_spec {
            o.profile.spec = baseline.workload.spec.clone();
        }
        if !has_profile && !explicit_spec && !explicit_extra {
            for x in &baseline.extra_workloads {
                let spec = &x.workload.spec;
                match o
                    .profile
                    .extra
                    .iter_mut()
                    .find(|s| s.family() == spec.family())
                {
                    Some(s) => *s = spec.clone(),
                    None => o.profile.extra.push(spec.clone()),
                }
            }
        }
        if !has_profile && !explicit_backends {
            o.profile.backends = backends_of(&baseline.runs);
        }
        usage(std::mem::replace(a, Args { items: Vec::new() }).finish())?;
        let mut runner = failed(Runner::prepare(o))?;
        let mut result = match runner.run_all(true) {
            Ok(r) => r,
            Err(e) => {
                runner.finish(false);
                return Err(Error::Failed(e));
            }
        };
        let mut outcomes = evaluate(&baseline, &result, &t, slowdown);
        let mut attempt = 0;
        while verdict(&outcomes) == Verdict::PerfFail && attempt < t.retries {
            attempt += 1;
            for target in retry_targets(&outcomes) {
                if let Err(e) = runner.rerun(&mut result, &target) {
                    runner.finish(false);
                    return Err(Error::Failed(e));
                }
            }
            outcomes = evaluate(&baseline, &result, &t, slowdown);
        }
        runner.finish(true);
        let out = out_arg
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| default_out(&runner.main().opts));
        failed(result.save(&out))?;
        eprintln!("[gstore-bench] wrote {}", out.display());
        (result, outcomes)
    };
    let v = verdict(&outcomes);
    let mut text = report::markdown(&result, Some(&baseline));
    text.push_str(&report::gates_markdown(&outcomes, false));
    if slowdown != 1.0 {
        text.push_str(&format!(
            "_Self-test: every measured time multiplied by {slowdown}._\n\n"
        ));
    }
    print!("{text}");
    append_summary(summary.as_deref(), &text);
    eprintln!(
        "[gstore-bench] {}",
        match v {
            Verdict::Pass => "PASS",
            Verdict::PerfFail => "FAIL: a performance gate failed",
            Verdict::ExactFail => "FAIL: an exact gate failed",
        }
    );
    Ok(exit_for(v))
}

fn cmd_compare(a: &mut Args) -> CmdResult {
    let t = thresholds(a)?;
    let slowdown: f64 = usage(a.parsed("--self-test-slowdown"))?.unwrap_or(1.0);
    let _ = a.flag("--markdown");
    let summary = usage(a.value("--summary"))?.map(PathBuf::from);
    let only = usage(a.list("--backends"))?;
    let pos = usage(std::mem::replace(a, Args { items: Vec::new() }).finish())?;
    let [base, res] = pos.as_slice() else {
        return Err(Error::Usage("compare needs BASELINE and RESULT".into()));
    };
    let mut base = failed(BenchResult::load(Path::new(base)))?;
    let mut res = failed(BenchResult::load(Path::new(res)))?;
    if let Some(keep) = &only {
        base.retain_runs(keep);
        res.retain_runs(keep);
    }
    let outcomes = evaluate(&base, &res, &t, slowdown);
    let mut text = report::markdown(&res, Some(&base));
    text.push_str(&report::gates_markdown(&outcomes, false));
    print!("{text}");
    append_summary(summary.as_deref(), &text);
    Ok(exit_for(verdict(&outcomes)))
}

/// `report`: Markdown of results (`--table KIND` for one compact table over
/// several results), or the regeneration (`--update`) / verification
/// (`--check`) of the marked tables of documents.
fn cmd_report(a: &mut Args) -> CmdResult {
    let t = thresholds(a)?;
    let update = usage(a.values("--update"))?;
    let check = usage(a.values("--check"))?;
    if !update.is_empty() || !check.is_empty() {
        usage(std::mem::replace(a, Args { items: Vec::new() }).finish())?;
        let mut stale = Vec::new();
        for (doc, write) in update
            .iter()
            .map(|d| (d, true))
            .chain(check.iter().map(|d| (d, false)))
        {
            let text = failed(std::fs::read_to_string(doc).map_err(|e| format!("{doc}: {e}")))?;
            let (fresh, changed) = failed(report::splice(&text, &mut |args: &[String]| {
                report::render_args(args, &t)
            }))
            .map_err(|e| match e {
                Error::Failed(m) => Error::Failed(format!("{doc}: {m}")),
                other => other,
            })?;
            if write {
                if fresh != text {
                    failed(std::fs::write(doc, &fresh).map_err(|e| format!("{doc}: {e}")))?;
                }
                eprintln!(
                    "[gstore-bench] {doc}: {} table(s) regenerated",
                    changed.len()
                );
            } else if !changed.is_empty() {
                for c in &changed {
                    eprintln!("[gstore-bench] {doc}: stale table `{c}`");
                }
                stale.push(doc.clone());
            }
        }
        return Ok(if stale.is_empty() {
            ExitCode::SUCCESS
        } else {
            eprintln!(
                "[gstore-bench] regenerate with: gstore-bench report --update {}",
                stale.join(" ")
            );
            ExitCode::from(1)
        });
    }
    // Everything else is `report`'s own argument list (also what a marker
    // holds), handed over in order.
    let args = std::mem::take(&mut a.items);
    let text = usage_or_failed(report::render_args(&args, &t))?;
    print!("{text}");
    Ok(ExitCode::SUCCESS)
}

/// A usage error for a malformed argument list, a failure otherwise.
fn usage_or_failed<T>(r: Result<T, String>) -> Result<T, Error> {
    r.map_err(|m| {
        if m.starts_with("report") || m.contains("needs a value") {
            Error::Usage(m)
        } else {
            Error::Failed(m)
        }
    })
}

fn cmd_bless(a: &mut Args) -> CmdResult {
    let to = usage(a.value("--to"))?.ok_or_else(|| Error::Usage("bless needs --to FILE".into()))?;
    let from = usage(a.value("--from"))?;
    let provisional = a.flag("--provisional");
    let note = usage(a.value("--note"))?;
    let mut result = if let Some(f) = from {
        usage(std::mem::replace(a, Args { items: Vec::new() }).finish())?;
        failed(BenchResult::load(Path::new(&f)))?
    } else {
        let o = usage(run_options(a, "smoke"))?;
        usage(std::mem::replace(a, Args { items: Vec::new() }).finish())?;
        let mut runner = failed(Runner::prepare(o))?;
        let r = runner.run_all(false);
        runner.finish(r.is_ok());
        failed(r)?
    };
    let outcomes = self_check(&result);
    if verdict(&outcomes) != Verdict::Pass {
        eprint!("{}", report::gates_markdown(&outcomes, true));
        eprintln!("gstore-bench: refusing to bless a result that fails its own exact checks");
        return Ok(ExitCode::from(1));
    }
    result.kind = "baseline".into();
    result.provisional = provisional;
    if note.is_some() {
        result.note = note;
    }
    failed(result.save(Path::new(&to)))?;
    eprintln!(
        "[gstore-bench] blessed {to}{}",
        if provisional { " (provisional)" } else { "" }
    );
    Ok(ExitCode::SUCCESS)
}

/// Sort runs by backend (reporting order), then label.
fn sort_runs(runs: &mut [crate::schema::BackendRun]) {
    runs.sort_by_key(|r| {
        (
            crate::runner::backends::ALL
                .iter()
                .position(|b| *b == r.backend)
                .unwrap_or(usize::MAX),
            r.label.clone(),
        )
    });
}

/// Append `runs` to `into`, refusing a key it already has.
fn merge_runs(
    into: &mut Vec<crate::schema::BackendRun>,
    runs: Vec<crate::schema::BackendRun>,
    what: &str,
) -> Result<(), Error> {
    for run in runs {
        if into.iter().any(|r| r.key() == run.key()) {
            return Err(Error::Failed(format!(
                "{what}: run {} appears twice (give one of them a --label)",
                run.key()
            )));
        }
        into.push(run);
    }
    Ok(())
}

fn cmd_merge(a: &mut Args) -> CmdResult {
    let out =
        usage(a.value("--out"))?.ok_or_else(|| Error::Usage("merge needs --out FILE".into()))?;
    let pos = usage(std::mem::replace(a, Args { items: Vec::new() }).finish())?;
    if pos.len() < 2 {
        return Err(Error::Usage("merge needs at least two results".into()));
    }
    let same = |wa: &crate::schema::WorkloadInfo, wb: &crate::schema::WorkloadInfo| {
        wa.spec == wb.spec && wa.version == wb.version && wa.data_fnv64 == wb.data_fnv64
    };
    let mut merged: Option<BenchResult> = None;
    for p in &pos {
        let mut r = failed(BenchResult::load(Path::new(p)))?;
        // Keep each run's own calibration for normalization.
        let score = r.calibration.as_ref().map(|c| c.score_ms);
        for run in r
            .runs
            .iter_mut()
            .chain(r.extra_workloads.iter_mut().flat_map(|x| x.runs.iter_mut()))
        {
            if run.config.calib_score_ms.is_none() {
                run.config.calib_score_ms = score;
            }
        }
        match merged.as_mut() {
            None => merged = Some(r),
            Some(m) => {
                if !same(&m.workload, &r.workload) {
                    return Err(Error::Failed(format!(
                        "{p}: a different workload than {}",
                        pos[0]
                    )));
                }
                merge_runs(&mut m.runs, r.runs, p)?;
                for x in r.extra_workloads {
                    match m
                        .extra_workloads
                        .iter_mut()
                        .find(|y| y.workload.id == x.workload.id)
                    {
                        Some(y) => {
                            if !same(&y.workload, &x.workload) {
                                return Err(Error::Failed(format!(
                                    "{p}: its {} workload differs from {}'s",
                                    x.workload.id, pos[0]
                                )));
                            }
                            merge_runs(&mut y.runs, x.runs, p)?;
                        }
                        None => m.extra_workloads.push(x),
                    }
                }
            }
        }
    }
    let mut m = merged.expect("two inputs");
    m.schema = crate::schema::SCHEMA.into();
    sort_runs(&mut m.runs);
    for x in &mut m.extra_workloads {
        sort_runs(&mut x.runs);
    }
    failed(m.save(Path::new(&out)))?;
    eprintln!(
        "[gstore-bench] wrote {out} ({} runs{})",
        m.runs.len(),
        if m.extra_workloads.is_empty() {
            String::new()
        } else {
            format!(", {} extra workload(s)", m.extra_workloads.len())
        }
    );
    Ok(exit_for(print_self_check(&m)))
}

fn cmd_calibrate(a: &mut Args) -> CmdResult {
    let quick = a.flag("--quick");
    usage(std::mem::replace(a, Args { items: Vec::new() }).finish())?;
    let dir = std::env::temp_dir();
    let req = ChildRequest {
        phase: "calibrate".into(),
        backend: String::new(),
        workload: WorkloadSpec::tiny(),
        work_dir: dir.clone(),
        data_file: PathBuf::new(),
        chunk_file: None,
        chunk_triples: 0,
        bin_dir: PathBuf::new(),
        builder: String::new(),
        gbuild_args: Vec::new(),
        reps: 0,
        drop_caches: false,
        singles: 0,
        conc: None,
        expected: Vec::new(),
        inject: None,
        quick,
        queries: Vec::new(),
        mixed: None,
    };
    let log = dir.join(format!("gstore-bench-calibrate-{}.log", std::process::id()));
    let v = failed(run_child(
        &req,
        &[],
        &log,
        std::time::Duration::from_secs(600),
    ));
    let _ = std::fs::remove_file(&log);
    let v = v?;
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_scanner() {
        let mut a = Args {
            items: ["--x", "1", "--y=2", "pos", "--f", "--x", "3"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };
        assert!(a.flag("--f"));
        assert_eq!(a.values("--x").unwrap(), vec!["1", "3"]);
        assert_eq!(a.value("--y").unwrap().as_deref(), Some("2"));
        assert_eq!(a.finish().unwrap(), vec!["pos"]);
        let a = Args {
            items: vec!["--bogus".into()],
        };
        assert!(a.finish().is_err());
    }

    #[test]
    fn run_options_apply_overrides() {
        let mut a = Args {
            items:
                "--profile reference --scale 38 --backends snapshot --reps 2 --query-procs 3 --no-conc --env A=b"
                    .split(' ')
                    .map(String::from)
                    .collect(),
        };
        let o = run_options(&mut a, "smoke").unwrap();
        assert_eq!(o.profile.name, "reference");
        assert_eq!(o.profile.spec.scale, 38);
        assert_eq!(o.profile.backends, vec!["snapshot"]);
        assert_eq!(o.profile.reps, 2);
        assert_eq!(o.profile.query_procs, 3);
        assert!(o.profile.conc.is_none());
        assert!(o.profile.mixed.is_none(), "--no-conc drops the mixed phase");
        assert_eq!(o.env, vec![("A".to_string(), "b".to_string())]);
        // --scale changes the main workload; the WatDiv-like one stays.
        assert_eq!(o.profile.extra, vec![WorkloadSpec::watdiv(10, 0)]);
        assert!(a.finish().unwrap().is_empty());
    }

    fn parse(line: &str) -> RunOptions {
        let mut a = Args {
            items: line.split(' ').map(String::from).collect(),
        };
        let o = run_options(&mut a, "smoke").unwrap();
        assert!(a.finish().unwrap().is_empty());
        o
    }

    #[test]
    fn workloads_labels_and_mixed_options() {
        // The profiles' extra workloads.
        let smoke = parse("--profile smoke");
        assert_eq!(smoke.profile.extra, vec![WorkloadSpec::watdiv(1, 0)]);
        assert!(smoke.profile.mixed.is_some());
        // An explicit --workload measures that workload alone, at the
        // profile's scale for it.
        let w = parse("--profile reference --workload watdiv");
        assert_eq!(w.profile.spec, WorkloadSpec::watdiv(10, 0));
        assert!(w.profile.extra.is_empty());
        let w = parse("--profile smoke --workload watdiv --scale 3 --shrink 2");
        assert_eq!((w.profile.spec.scale, w.profile.spec.shrink), (3, Some(2)));
        // --extra replaces the list.
        assert!(parse("--profile reference --extra none")
            .profile
            .extra
            .is_empty());
        let x = parse("--profile micro --extra watdiv:2,watdiv:1/10");
        assert_eq!(x.profile.extra[0], WorkloadSpec::watdiv(2, 0));
        assert_eq!(x.profile.extra[1].shrink, Some(10));
        // Labels.
        assert_eq!(parse("--label stream").label.as_deref(), Some("stream"));
        let mut bad = Args {
            items: vec!["--label".into(), "a@b".into()],
        };
        assert!(run_options(&mut bad, "smoke").is_err());
        // Mixed-workload settings.
        let m = parse(
            "--mixed-readers 4 --mixed-write-ms 50 --mixed-secs 2 --mixed-checkpoint on \
             --mixed-endpoints api --mixed-query q1",
        )
        .profile
        .mixed
        .unwrap();
        assert_eq!(
            (m.readers, m.write_ms, m.secs, m.check_point, m.endpoints),
            (4, 50, 2.0, vec![true], vec!["api".to_string()])
        );
        assert_eq!(m.query.as_deref(), Some("q1"));
        assert!(parse("--no-mixed").profile.mixed.is_none());
        let mut bad = Args {
            items: vec!["--mixed-checkpoint".into(), "maybe".into()],
        };
        assert!(run_options(&mut bad, "smoke").is_err());
    }
}
