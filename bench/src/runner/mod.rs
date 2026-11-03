//! The benchmark runner: profiles, the data cache, the per-phase child
//! processes and the targeted retries of `check`.
//!
//! A run, per workload (the profile's main one, then each of its `extra`
//! ones — the WatDiv-like workload in `smoke` and `reference`): the data is
//! generated (or taken from the cache) and, for a workload of query
//! templates, the templates are bound against it (the `instantiate` child,
//! cached next to the data). Then per backend: build (`gbuild` via
//! [`backends`]) → first open (cold when the page cache could be dropped) →
//! `open_reps` warm opens → the query phase → the concurrency phase → the
//! update phase → the mixed-workload phase (the last two write). Each step
//! is one child process ([`child`]); the database is deleted after the
//! backend unless `--keep-db`.

pub mod backends;
pub mod child;
pub mod cli;
pub mod conc;
pub mod phases;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::calib::Calibration;
use crate::gates::{Phase, RetryTarget};
use crate::hash::{file_fnv64, hex, Fnv64};
use crate::schema::{
    BackendRun, BenchResult, ChunkInfo, EnvInfo, OpenResult, QueryInfo, RunConfig, WorkloadInfo,
    WorkloadSection, SCHEMA,
};
use crate::workload::{bind, generate_to, BoundQuery, GenStats, Workload, WorkloadSpec};
use child::{ChildRequest, CHILD_COMMAND, RESULT_MARKER};
use conc::{ConcSettings, MixedSettings};
use phases::{median, OpenSample, QueryPhase};

/// A named set of run settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub spec: WorkloadSpec,
    pub backends: Vec<String>,
    /// Warm query repetitions (after the first run), per query process.
    pub reps: usize,
    /// Fresh processes the query phase runs in; per query the minimum of
    /// their warm medians is kept (see [`phases::fold_query_phases`]).
    #[serde(default = "one")]
    pub query_procs: usize,
    /// Warm opens, each in a fresh process (after the first open).
    pub open_reps: usize,
    /// Single-triple inserts (and deletes) of the update phase.
    pub singles: usize,
    pub update: bool,
    pub conc: Option<ConcSettings>,
    /// Readers plus a paced writer, after the update phase.
    #[serde(default)]
    pub mixed: Option<MixedSettings>,
    /// Further workloads, each measured on the same backends into its own
    /// section of the result (`extra_workloads`).
    #[serde(default)]
    pub extra: Vec<WorkloadSpec>,
    pub timeout_build_s: u64,
    pub timeout_s: u64,
    /// Shrink the calibration (tests).
    pub quick_calib: bool,
}

fn one() -> usize {
    1
}

/// The built-in profiles: `micro` (tests), `smoke` (CI) and `reference`
/// (Phase 6 measurements).
pub fn profile(name: &str) -> Option<Profile> {
    let both = |clients: Vec<usize>, secs: f64| ConcSettings {
        clients,
        secs,
        queries: Vec::new(),
        endpoints: vec!["sparql".into(), "api".into()],
    };
    let mixed = |readers: usize, secs: f64| MixedSettings {
        readers,
        write_ms: 100,
        secs,
        check_point: vec![false, true],
        endpoints: vec!["sparql".into(), "api".into()],
        query: None,
        begins: 20,
    };
    Some(match name {
        "micro" => Profile {
            name: name.into(),
            spec: WorkloadSpec {
                departments: Some(1),
                ..WorkloadSpec::lubm(1, 0)
            },
            backends: vec!["snapshot".into()],
            reps: 2,
            query_procs: 2,
            open_reps: 1,
            singles: 3,
            update: true,
            conc: Some(both(vec![1, 2], 0.25)),
            mixed: Some(MixedSettings {
                write_ms: 50,
                begins: 3,
                ..mixed(2, 0.25)
            }),
            extra: Vec::new(),
            timeout_build_s: 300,
            timeout_s: 300,
            quick_calib: true,
        },
        "smoke" => Profile {
            name: name.into(),
            spec: WorkloadSpec::lubm(1, 0),
            backends: vec!["snapshot".into(), "disk".into()],
            reps: 3,
            query_procs: 3,
            open_reps: 3,
            singles: 20,
            update: true,
            conc: Some(both(vec![1, 2], 1.0)),
            mixed: Some(mixed(2, 1.0)),
            extra: vec![WorkloadSpec::watdiv(1, 0)],
            timeout_build_s: 1800,
            timeout_s: 900,
            quick_calib: false,
        },
        "reference" => Profile {
            name: name.into(),
            spec: WorkloadSpec::lubm(8, 0),
            backends: backends::compiled().iter().map(|s| s.to_string()).collect(),
            reps: 5,
            query_procs: 5,
            open_reps: 3,
            singles: 100,
            update: true,
            conc: Some(both(vec![1, 8], 5.0)),
            mixed: Some(mixed(8, 5.0)),
            extra: vec![WorkloadSpec::watdiv(10, 0)],
            timeout_build_s: 4 * 3600,
            timeout_s: 2 * 3600,
            quick_calib: false,
        },
        _ => return None,
    })
}

/// Everything a run needs.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub profile: Profile,
    /// Where `gbuild` is.
    pub bin_dir: PathBuf,
    /// `cli` (default) or `lib`.
    pub builder: String,
    /// The generated-data cache.
    pub data_dir: PathBuf,
    /// Databases and logs.
    pub work_dir: PathBuf,
    pub keep_db: bool,
    /// `--env K=V` for every child.
    pub env: Vec<(String, String)>,
    pub gbuild_args: Vec<String>,
    pub drop_caches: bool,
    pub inject: Option<String>,
    pub calibrate: bool,
    pub verify_data: bool,
    pub quiet: bool,
    /// `--label`: recorded on every run, whose key becomes `backend@label`.
    pub label: Option<String>,
}

impl RunOptions {
    pub fn new(profile: Profile) -> RunOptions {
        RunOptions {
            profile,
            bin_dir: PathBuf::from("target/release"),
            builder: "cli".into(),
            data_dir: std::env::var_os("GSTORE_BENCH_CACHE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("target/gstore-bench/data")),
            work_dir: std::env::var_os("GSTORE_BENCH_WORK")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("target/gstore-bench/work")),
            keep_db: false,
            env: Vec::new(),
            gbuild_args: Vec::new(),
            drop_caches: false,
            inject: None,
            calibrate: true,
            verify_data: false,
            quiet: false,
            label: None,
        }
    }
}

/// The data-cache sidecar `<file>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Sidecar {
    schema: String,
    version: String,
    spec: WorkloadSpec,
    chunk: bool,
    stats: GenStats,
}

const SIDECAR_SCHEMA: &str = "gstore-bench-data/1";

/// A generated file and its statistics.
#[derive(Debug, Clone)]
pub struct DataFile {
    pub path: PathBuf,
    pub stats: GenStats,
    /// Generation time, when this call generated it.
    pub gen_ms: Option<f64>,
}

fn sidecar_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".json");
    PathBuf::from(s)
}

/// The cached data file for `spec` (or its update chunk), generated when the
/// file or its sidecar is missing, stale or does not match.
pub fn ensure_data(
    spec: &WorkloadSpec,
    w: &dyn Workload,
    dir: &Path,
    chunk: bool,
    verify: bool,
) -> Result<DataFile, String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let name = if chunk {
        format!("{}.chunk.nt", w.file_stem())
    } else {
        format!("{}.nt", w.file_stem())
    };
    let path = dir.join(name);
    let side = sidecar_path(&path);
    let cached = fs::read_to_string(&side)
        .ok()
        .and_then(|t| serde_json::from_str::<Sidecar>(&t).ok())
        .filter(|s| {
            s.schema == SIDECAR_SCHEMA
                && s.version == w.version()
                && s.spec == *spec
                && s.chunk == chunk
        })
        .filter(|s| {
            fs::metadata(&path)
                .map(|m| m.len() == s.stats.bytes)
                .unwrap_or(false)
        })
        .filter(|s| {
            !verify
                || file_fnv64(&path)
                    .map(|(h, _)| h == s.stats.fnv64)
                    .unwrap_or(false)
        });
    if let Some(s) = cached {
        return Ok(DataFile {
            path,
            stats: s.stats,
            gen_ms: None,
        });
    }
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id()
    ));
    let started = Instant::now();
    let stats = {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        let s = generate_to(w, &mut f, chunk)
            .map_err(|e| format!("generating {}: {e}", path.display()))?;
        f.sync_all().map_err(|e| e.to_string())?;
        s
    };
    let gen_ms = started.elapsed().as_secs_f64() * 1e3;
    fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    let sc = Sidecar {
        schema: SIDECAR_SCHEMA.into(),
        version: w.version().into(),
        spec: spec.clone(),
        chunk,
        stats,
    };
    let tmp = sidecar_path(&tmp);
    fs::write(
        &tmp,
        serde_json::to_string_pretty(&sc).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &side).map_err(|e| format!("{}: {e}", side.display()))?;
    Ok(DataFile {
        path,
        stats,
        gen_ms: Some(gen_ms),
    })
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn git(args: &[&str]) -> Option<String> {
    let o = Command::new("git")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// The current commit and whether the tree has local changes.
pub fn git_state() -> (Option<String>, Option<bool>) {
    let commit = git(&["rev-parse", "HEAD"]);
    let dirty = commit
        .as_ref()
        .and_then(|_| git(&["status", "--porcelain", "--untracked-files=no"]))
        .map(|s| !s.is_empty());
    (commit, dirty)
}

pub fn env_info() -> EnvInfo {
    let (git_commit, git_dirty) = git_state();
    EnvInfo {
        tool_version: crate::TOOL_VERSION.into(),
        git_commit,
        git_dirty,
        hostname: crate::sys::hostname(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        nproc: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        cpu_model: crate::sys::cpu_model(),
        mem_total_mib: crate::sys::mem_total_mib(),
        loadavg: crate::sys::loadavg(),
        debug_build: cfg!(debug_assertions),
    }
}

/// The gstore features this harness was compiled with.
pub fn features() -> Vec<String> {
    let mut v = Vec::new();
    if cfg!(feature = "rocksdb") {
        v.push("rocksdb".to_string());
    }
    v
}

fn tail(path: &Path, lines: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let v: Vec<&str> = text.lines().collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

/// The bound-queries cache `<stem>.queries.json` next to a data file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct QueriesSidecar {
    schema: String,
    version: String,
    spec: WorkloadSpec,
    data_fnv64: String,
    templates_fnv64: String,
    queries: Vec<BoundQuery>,
}

const QUERIES_SCHEMA: &str = "gstore-bench-queries/1";

/// FNV-1a 64 over a workload's query templates (names and texts).
fn templates_fnv64(w: &dyn Workload) -> String {
    let mut h = Fnv64::new();
    for q in w.queries() {
        h.update(q.name.as_bytes());
        h.update(&[0]);
        h.update(q.text.as_bytes());
        h.update(&[0]);
    }
    hex(h.finish())
}

/// A prepared run of one workload: data generated, queries bound, work
/// directory chosen.
pub struct Session {
    pub opts: RunOptions,
    /// The workload, with its queries bound when it has templates.
    pub workload: Box<dyn Workload>,
    pub data: DataFile,
    pub chunk: DataFile,
    /// The bound queries (empty for a workload without templates).
    pub bound: Vec<BoundQuery>,
    work_root: PathBuf,
    calib_score: Option<f64>,
    seq: Cell<u32>,
    /// Backends whose database the update or mixed phase has changed since
    /// it was built (see [`rerun`](Self::rerun)).
    updated: RefCell<BTreeSet<String>>,
}

impl Session {
    /// Generate (or reuse) the data, set up the work directory and bind the
    /// query templates.
    pub fn prepare(opts: RunOptions) -> Result<Session, String> {
        let workload = opts.profile.spec.instantiate()?;
        for b in &opts.profile.backends {
            backends::validate(b)?;
        }
        let data = ensure_data(
            &opts.profile.spec,
            workload.as_ref(),
            &opts.data_dir,
            false,
            opts.verify_data,
        )?;
        let chunk = ensure_data(
            &opts.profile.spec,
            workload.as_ref(),
            &opts.data_dir,
            true,
            opts.verify_data,
        )?;
        let work_root = opts.work_dir.join(format!(
            "{}-{}-{}",
            workload.file_stem(),
            std::process::id(),
            now_unix()
        ));
        fs::create_dir_all(work_root.join("logs"))
            .map_err(|e| format!("cannot create {}: {e}", work_root.display()))?;
        let work_root =
            fs::canonicalize(&work_root).map_err(|e| format!("{}: {e}", work_root.display()))?;
        let mut s = Session {
            opts,
            workload,
            data,
            chunk,
            bound: Vec::new(),
            work_root,
            calib_score: None,
            seq: Cell::new(0),
            updated: RefCell::new(BTreeSet::new()),
        };
        if let Some(ms) = s.data.gen_ms {
            s.say(&format!(
                "generated {} ({} lines, {} unique triples) in {:.1} s",
                s.data.path.display(),
                s.data.stats.lines,
                s.data.stats.unique_triples,
                ms / 1e3
            ));
        }
        if s.workload.needs_binding() {
            if let Err(e) = s.bind_queries() {
                s.cleanup_keep_logs();
                return Err(e);
            }
        }
        Ok(s)
    }

    /// Bind the workload's templates: from the cache next to the data file
    /// when it matches (generator version, parameters, data hash and
    /// templates), else by an `instantiate` child, whose answer is cached.
    /// With `--verify-data` a cached binding is recomputed and must match.
    fn bind_queries(&mut self) -> Result<(), String> {
        let path = self
            .opts
            .data_dir
            .join(format!("{}.queries.json", self.workload.file_stem()));
        let want = QueriesSidecar {
            schema: QUERIES_SCHEMA.into(),
            version: self.workload.version().into(),
            spec: self.opts.profile.spec.clone(),
            data_fnv64: hex(self.data.stats.fnv64),
            templates_fnv64: templates_fnv64(self.workload.as_ref()),
            queries: Vec::new(),
        };
        let cached = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<QueriesSidecar>(&t).ok())
            .filter(|c| {
                QueriesSidecar {
                    queries: Vec::new(),
                    ..c.clone()
                } == want
            });
        let bound = match cached {
            Some(c) if !self.opts.verify_data => c.queries,
            cached => {
                let req = self.request("instantiate", "", self.work_root.clone());
                let started = Instant::now();
                let fresh: Vec<BoundQuery> = self.child(&req)?;
                self.say(&format!(
                    "bound {} query templates of {} in {:.1} s ({} ASK tries)",
                    fresh.len(),
                    self.opts.profile.spec.label(),
                    started.elapsed().as_secs_f64(),
                    fresh.iter().map(|q| q.tries).sum::<u32>()
                ));
                if let Some(c) = cached {
                    if c.queries != fresh {
                        return Err(format!(
                            "the bound queries cached in {} differ from a fresh ASK walk \
                             over the same data: the engine answers an ASK differently",
                            path.display()
                        ));
                    }
                }
                let side = QueriesSidecar {
                    queries: fresh.clone(),
                    ..want
                };
                let tmp = self.opts.data_dir.join(format!(
                    ".{}.queries.tmp{}",
                    self.workload.file_stem(),
                    std::process::id()
                ));
                fs::write(
                    &tmp,
                    serde_json::to_string_pretty(&side).map_err(|e| e.to_string())?,
                )
                .map_err(|e| format!("{}: {e}", tmp.display()))?;
                fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
                fresh
            }
        };
        self.workload = bind(self.opts.profile.spec.instantiate()?, &bound);
        self.bound = bound;
        Ok(())
    }

    fn say(&self, msg: &str) {
        if !self.opts.quiet {
            eprintln!("[gstore-bench] {msg}");
        }
    }

    /// The work directory of `backend` (`gbuild`'s current directory).
    pub fn backend_dir(&self, backend: &str) -> PathBuf {
        self.work_root.join(backend)
    }

    /// The database directory of `backend`.
    pub fn db_dir(&self, backend: &str) -> PathBuf {
        backends::db_dir(&self.backend_dir(backend), self.workload.as_ref())
    }

    fn request(&self, phase: &str, backend: &str, work_dir: PathBuf) -> ChildRequest {
        ChildRequest {
            phase: phase.into(),
            backend: backend.into(),
            workload: self.opts.profile.spec.clone(),
            work_dir,
            data_file: fs::canonicalize(&self.data.path).unwrap_or_else(|_| self.data.path.clone()),
            chunk_file: Some(
                fs::canonicalize(&self.chunk.path).unwrap_or_else(|_| self.chunk.path.clone()),
            ),
            chunk_triples: self.chunk.stats.unique_triples,
            bin_dir: self.opts.bin_dir.clone(),
            builder: self.opts.builder.clone(),
            gbuild_args: self.opts.gbuild_args.clone(),
            reps: self.opts.profile.reps,
            drop_caches: false,
            singles: self.opts.profile.singles,
            conc: self.opts.profile.conc.clone(),
            expected: Vec::new(),
            inject: self.opts.inject.clone(),
            quick: self.opts.profile.quick_calib,
            queries: self.bound.clone(),
            mixed: self.opts.profile.mixed.clone(),
        }
    }

    /// Run one phase in a fresh child process.
    pub fn child<T: DeserializeOwned>(&self, req: &ChildRequest) -> Result<T, String> {
        let n = self.seq.get() + 1;
        self.seq.set(n);
        let log = self.work_root.join("logs").join(format!(
            "{n:03}-{}-{}.log",
            if req.backend.is_empty() {
                "all"
            } else {
                &req.backend
            },
            req.phase
        ));
        let timeout = Duration::from_secs(if req.phase == "build" {
            self.opts.profile.timeout_build_s
        } else {
            self.opts.profile.timeout_s
        });
        let v = run_child(req, &self.opts.env, &log, timeout)?;
        serde_json::from_value(v)
            .map_err(|e| format!("{} phase: unexpected result: {e}", req.phase))
    }

    /// Calibrate this machine (in a child).
    pub fn calibrate(&mut self) -> Result<Calibration, String> {
        let req = self.request("calibrate", "", self.work_root.clone());
        let c: Calibration = self.child(&req)?;
        self.calib_score = Some(c.score_ms);
        self.say(&format!("calibration score {:.2} ms", c.score_ms));
        Ok(c)
    }

    /// The workload part of a result.
    pub fn workload_info(&self) -> WorkloadInfo {
        let w = self.workload.as_ref();
        WorkloadInfo {
            spec: self.opts.profile.spec.clone(),
            id: w.id().into(),
            version: w.version().into(),
            lines: self.data.stats.lines,
            unique_triples: self.data.stats.unique_triples,
            bytes: self.data.stats.bytes,
            data_fnv64: hex(self.data.stats.fnv64),
            queries: w
                .queries()
                .iter()
                .map(|q| {
                    let b = self.bound.iter().find(|b| b.name == q.name);
                    QueryInfo {
                        name: q.name.clone(),
                        text_fnv64: hex(q.text_fnv64()),
                        bindings: b.map(|b| b.bindings.clone()).unwrap_or_default(),
                        tries: b.map(|b| b.tries),
                    }
                })
                .collect(),
            chunk: Some(ChunkInfo {
                unique_triples: self.chunk.stats.unique_triples,
                fnv64: hex(self.chunk.stats.fnv64),
            }),
            gen_ms: self.data.gen_ms,
        }
    }

    /// The result skeleton: environment and workload, no runs.
    pub fn result(&self, calibration: Option<Calibration>) -> BenchResult {
        BenchResult {
            schema: SCHEMA.into(),
            kind: "result".into(),
            provisional: false,
            note: None,
            profile: self.opts.profile.name.clone(),
            created_unix: now_unix(),
            env: env_info(),
            calibration,
            workload: self.workload_info(),
            runs: Vec::new(),
            extra_workloads: Vec::new(),
        }
    }

    fn run_config(&self) -> RunConfig {
        RunConfig {
            features: features(),
            bin_dir: Some(self.opts.bin_dir.display().to_string()),
            builder: self.opts.builder.clone(),
            git_commit: git_state().0,
            env: self.opts.env.iter().cloned().collect::<BTreeMap<_, _>>(),
            gbuild_args: self.opts.gbuild_args.clone(),
            reps: self.opts.profile.reps,
            query_procs: self.opts.profile.query_procs.max(1),
            drop_caches: self.opts.drop_caches,
            calib_score_ms: self.calib_score,
        }
    }

    /// The query phase in `query_procs` fresh processes, folded.
    fn queries(&self, backend: &str) -> Result<QueryPhase, String> {
        let req = self.request("query", backend, self.backend_dir(backend));
        let phases = (0..self.opts.profile.query_procs.max(1))
            .map(|_| self.child::<QueryPhase>(&req))
            .collect::<Result<Vec<_>, _>>()?;
        phases::fold_query_phases(phases)
    }

    fn opens(&self, backend: &str, n: usize) -> Result<Vec<OpenSample>, String> {
        let req = self.request("open", backend, self.backend_dir(backend));
        (0..n).map(|_| self.child::<OpenSample>(&req)).collect()
    }

    /// Measure every phase of `backend`.
    pub fn run_backend(&self, backend: &str) -> Result<BackendRun, String> {
        let p = &self.opts.profile;
        let dir = self.backend_dir(backend);
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut run = BackendRun::new(backend);
        run.label = self.opts.label.clone();
        run.config = self.run_config();

        let build =
            self.child::<crate::schema::BuildResult>(&self.request("build", backend, dir.clone()))?;
        self.say(&format!(
            "{backend}: build {:.2} s, peak RSS {}, {:.1} MB on disk",
            build.wall_ms / 1e3,
            build
                .peak_rss_mib
                .map(|m| format!("{m:.0} MiB"))
                .unwrap_or_else(|| "n/a".into()),
            build.dir_bytes as f64 / 1e6
        ));
        run.build = Some(build);

        let mut first_req = self.request("open", backend, dir.clone());
        first_req.drop_caches = self.opts.drop_caches;
        let first: OpenSample = self.child(&first_req)?;
        let warm = self.opens(backend, p.open_reps)?;
        let warm_ms: Vec<f64> = warm.iter().map(|s| s.open_ms).collect();
        run.open = Some(OpenResult {
            first_ms: first.open_ms,
            warm_ms: if warm_ms.is_empty() {
                first.open_ms
            } else {
                median(&warm_ms)
            },
            warm_samples_ms: warm_ms,
            cold: first.cold,
            rss_after_open_mib: first.rss_mib,
            hwm_after_open_mib: first.hwm_mib,
            triple_num: first.triple_num,
            entity_num: first.entity_num,
            literal_num: first.literal_num,
            predicate_num: first.predicate_num,
        });
        self.say(&format!(
            "{backend}: open first {:.1} ms{}, warm {:.1} ms, {} triples",
            first.open_ms,
            if first.cold { " (cold)" } else { "" },
            run.open.as_ref().map(|o| o.warm_ms).unwrap_or(0.0),
            first.triple_num
        ));

        let qp = self.queries(backend)?;
        run.queries = qp.queries;
        run.query_mem = Some(qp.mem);
        let meds: Vec<String> = run
            .queries
            .iter()
            .map(|q| format!("{} {:.1}", q.name, q.median_ms))
            .collect();
        self.say(&format!(
            "{backend}: query medians (ms): {}",
            meds.join(", ")
        ));

        if p.conc.is_some() {
            let c: crate::schema::ConcResult =
                self.child(&self.request("conc", backend, dir.clone()))?;
            let pts: Vec<String> = c
                .points
                .iter()
                .map(|x| format!("{}/{}/c{} {:.0} qps", x.endpoint, x.query, x.clients, x.qps))
                .collect();
            self.say(&format!("{backend}: concurrency: {}", pts.join(", ")));
            run.conc = Some(c);
        }

        if p.update {
            self.updated.borrow_mut().insert(backend.to_string());
            let mut req = self.request("update", backend, dir.clone());
            req.expected = run
                .queries
                .iter()
                .map(|q| (q.name.clone(), q.rows, q.rows_fnv64.clone()))
                .collect();
            let u: crate::schema::UpdateResult = self.child(&req)?;
            self.say(&format!(
                "{backend}: update: single insert median {:.2} ms, chunk insert {:.1} ms, round trip {}",
                u.single_insert_median_ms.unwrap_or(0.0),
                u.chunk_insert_ms,
                if u.round_trip_ok() { "ok" } else { "FAILED" }
            ));
            run.update = Some(u);
        }

        if p.mixed.is_some() {
            self.updated.borrow_mut().insert(backend.to_string());
            let m: crate::schema::MixedResult =
                self.child(&self.request("mixed", backend, dir.clone()))?;
            let pts: Vec<String> = m
                .points
                .iter()
                .map(|x| {
                    format!(
                        "{}/cp-{} {:.0} qps + {} writes",
                        x.endpoint,
                        if x.check_point { "on" } else { "off" },
                        x.qps,
                        x.writes
                    )
                })
                .collect();
            self.say(&format!("{backend}: mixed: {}", pts.join(", ")));
            run.mixed = Some(m);
        }
        Ok(run)
    }

    /// Re-run `phase` of `run`'s backend and keep the minimum of the
    /// medians (`check`'s targeted retries). Answers must not change. The
    /// update and mixed phases have already run (gates are evaluated after
    /// the whole run) and leave a database that differs from the built one
    /// (more triples, other ids, another file layout), so a retried open or
    /// query phase first rebuilds it: a retry measures the database the
    /// first attempt and the baseline measured.
    pub fn rerun(&self, run: &mut BackendRun, phase: Phase) -> Result<(), String> {
        let backend = run.backend.clone();
        let key = match phase {
            Phase::Build => "build",
            Phase::Open => "open",
            Phase::Query => "query",
        };
        if matches!(phase, Phase::Open | Phase::Query) && self.updated.borrow().contains(&backend) {
            self.say(&format!(
                "{backend}: rebuilding the database the update phase changed"
            ));
            let dir = self.backend_dir(&backend);
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            self.child::<crate::schema::BuildResult>(&self.request("build", &backend, dir))?;
            self.updated.borrow_mut().remove(&backend);
        }
        *run.attempts.entry(key.into()).or_insert(1) += 1;
        self.say(&format!("{backend}: retrying the {key} phase"));
        match phase {
            Phase::Build => {
                let dir = self.work_root.join(format!("{backend}-retry"));
                let _ = fs::remove_dir_all(&dir);
                fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                let r = self.child::<crate::schema::BuildResult>(&self.request(
                    "build",
                    &backend,
                    dir.clone(),
                ));
                let _ = fs::remove_dir_all(&dir);
                let b = r?;
                if let Some(old) = run.build.as_mut() {
                    old.wall_ms = old.wall_ms.min(b.wall_ms);
                    old.peak_rss_mib = match (old.peak_rss_mib, b.peak_rss_mib) {
                        (Some(x), Some(y)) => Some(x.min(y)),
                        (x, y) => x.or(y),
                    };
                }
            }
            Phase::Open => {
                let samples = self.opens(&backend, self.opts.profile.open_reps.max(1))?;
                let ms: Vec<f64> = samples.iter().map(|s| s.open_ms).collect();
                if let Some(o) = run.open.as_mut() {
                    let same = |s: &OpenSample| {
                        (s.triple_num, s.entity_num, s.literal_num, s.predicate_num)
                            == (o.triple_num, o.entity_num, o.literal_num, o.predicate_num)
                    };
                    if !samples.iter().all(same) {
                        return Err(format!(
                            "{backend}: the retried open phase saw a different database \
                             than the first attempt"
                        ));
                    }
                    o.warm_ms = o.warm_ms.min(median(&ms));
                    o.warm_samples_ms.extend(ms);
                    if let Some(rss) = samples.iter().filter_map(|s| s.rss_mib).reduce(f64::min) {
                        o.rss_after_open_mib =
                            Some(o.rss_after_open_mib.map_or(rss, |x| x.min(rss)));
                    }
                }
            }
            Phase::Query => {
                let qp = self.queries(&backend)?;
                for q in qp.queries {
                    if let Some(old) = run.queries.iter_mut().find(|o| o.name == q.name) {
                        old.stable &=
                            q.stable && q.rows == old.rows && q.rows_fnv64 == old.rows_fnv64;
                        old.median_ms = old.median_ms.min(q.median_ms);
                        old.first_ms = old.first_ms.min(q.first_ms);
                        old.runs_ms.extend(q.runs_ms);
                        old.proc_medians_ms.extend(q.proc_medians_ms);
                    }
                }
                if let (Some(m), Some(h)) = (run.query_mem.as_mut(), qp.mem.hwm_mib) {
                    m.hwm_mib = Some(m.hwm_mib.map_or(h, |x| x.min(h)));
                }
            }
        }
        Ok(())
    }

    /// Delete `backend`'s database and work directory.
    pub fn cleanup_backend(&self, backend: &str) {
        let _ = fs::remove_dir_all(self.backend_dir(backend));
    }

    /// Delete the whole work directory (databases and logs).
    pub fn cleanup(&self) {
        let _ = fs::remove_dir_all(&self.work_root);
    }

    /// After a failure: delete the databases but keep the phase logs, and
    /// say where they are.
    pub fn cleanup_keep_logs(&self) {
        if let Ok(rd) = fs::read_dir(&self.work_root) {
            for e in rd.flatten() {
                if e.file_name() != "logs" {
                    let _ = fs::remove_dir_all(e.path());
                }
            }
        }
        eprintln!(
            "[gstore-bench] phase logs kept in {}",
            self.work_root.join("logs").display()
        );
    }

    /// Measure every backend of the profile. Databases are deleted after
    /// each backend unless `keep` (or `--keep-db`).
    pub fn run_backends(&self, keep: bool) -> Result<Vec<BackendRun>, String> {
        let mut runs = Vec::new();
        for b in self.opts.profile.backends.clone() {
            let r = self.run_backend(&b);
            if !keep && !self.opts.keep_db {
                self.cleanup_backend(&b);
            }
            runs.push(r?);
        }
        Ok(runs)
    }

    pub fn work_root(&self) -> &Path {
        &self.work_root
    }
}

/// A run of a profile: one [`Session`] for its main workload and one for
/// each extra workload, measured in that order into one result (the extra
/// ones into `extra_workloads`).
pub struct Runner {
    /// The main workload's session first.
    pub sessions: Vec<Session>,
}

impl Runner {
    /// Prepare every workload of `opts`'s profile. The concurrency and
    /// mixed-workload query choices (`--conc-queries`, `--mixed-query`)
    /// apply to the main workload; the extra ones use their defaults.
    pub fn prepare(opts: RunOptions) -> Result<Runner, String> {
        let extras = opts.profile.extra.clone();
        let mut main = opts.clone();
        main.profile.extra.clear();
        let mut sessions = vec![Session::prepare(main)?];
        for spec in extras {
            let mut o = opts.clone();
            o.profile.spec = spec;
            o.profile.extra.clear();
            if let Some(c) = o.profile.conc.as_mut() {
                c.queries.clear();
            }
            if let Some(m) = o.profile.mixed.as_mut() {
                m.query = None;
            }
            match Session::prepare(o) {
                Ok(s) => sessions.push(s),
                Err(e) => {
                    for s in &sessions {
                        s.cleanup();
                    }
                    return Err(e);
                }
            }
        }
        Ok(Runner { sessions })
    }

    /// The main workload's session.
    pub fn main(&self) -> &Session {
        &self.sessions[0]
    }

    /// Calibrate, then measure every backend of every workload. Databases
    /// are deleted after each backend unless `keep` (or `--keep-db`).
    pub fn run_all(&mut self, keep: bool) -> Result<BenchResult, String> {
        let calib = if self.sessions[0].opts.calibrate {
            Some(self.sessions[0].calibrate()?)
        } else {
            None
        };
        let score = calib.as_ref().map(|c| c.score_ms);
        for s in &mut self.sessions[1..] {
            s.calib_score = score;
        }
        let mut result = self.sessions[0].result(calib);
        result.runs = self.sessions[0].run_backends(keep)?;
        for s in &self.sessions[1..] {
            let runs = s.run_backends(keep)?;
            result.extra_workloads.push(WorkloadSection {
                workload: s.workload_info(),
                runs,
            });
        }
        Ok(result)
    }

    /// Re-run the phase `t` names in `result` (`check`'s targeted retries).
    pub fn rerun(&self, result: &mut BenchResult, t: &RetryTarget) -> Result<(), String> {
        let session = match &t.section {
            None => &self.sessions[0],
            Some(id) => self.sessions[1..]
                .iter()
                .find(|s| s.workload.id() == id)
                .ok_or_else(|| format!("no session measured the {id} workload"))?,
        };
        let runs = result
            .runs_mut(t.section.as_deref())
            .ok_or("the result lacks the retried workload")?;
        let run = runs
            .iter_mut()
            .find(|r| r.key() == t.run)
            .ok_or_else(|| format!("the result lacks the retried run {}", t.run))?;
        session.rerun(run, t.phase)
    }

    /// Remove the work directories (keeping the phase logs when the run
    /// failed, and everything with `--keep-db`).
    pub fn finish(&self, ok: bool) {
        for s in &self.sessions {
            if s.opts.keep_db {
                continue;
            }
            if ok {
                s.cleanup();
            } else {
                s.cleanup_keep_logs();
            }
        }
    }
}

/// Spawn `gstore-bench __child`, send `req`, and return the `data` of its one
/// result line. Its standard error goes to `log`. Errors (crash, timeout,
/// missing or duplicated result line, `ok:false`) carry the log's tail.
pub fn run_child(
    req: &ChildRequest,
    env: &[(String, String)],
    log: &Path,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate gstore-bench: {e}"))?;
    if let Some(p) = log.parent() {
        let _ = fs::create_dir_all(p);
    }
    let errf = fs::File::create(log).map_err(|e| format!("{}: {e}", log.display()))?;
    let mut cmd = Command::new(exe);
    cmd.arg(CHILD_COMMAND)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(errf));
    for (k, v) in env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        // Its own group is out of reach of Ctrl-C and timeout(1): end it
        // with this process instead (it ends its gbuild the same way).
        let parent = std::process::id();
        // SAFETY: the hook only makes async-signal-safe syscalls.
        unsafe {
            cmd.pre_exec(move || crate::sys::die_with_parent(parent));
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start a child: {e}"))?;
    let pid = child.id();
    let body = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        let _ = stdin.write_all(&body);
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for l in BufReader::new(stdout).lines() {
            match l {
                Ok(l) => lines.push(l),
                Err(_) => break,
            }
        }
        lines
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => return Err(format!("waiting for the {} child: {e}", req.phase)),
        }
        if started.elapsed() > timeout {
            if !crate::sys::kill_process_group(pid) {
                let _ = child.kill();
            }
            let _ = child.wait();
            return Err(format!(
                "the {} phase{} timed out after {} s (log {}):\n{}",
                req.phase,
                if req.backend.is_empty() {
                    String::new()
                } else {
                    format!(" of {}", req.backend)
                },
                timeout.as_secs(),
                log.display(),
                tail(log, 20)
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let lines = reader.join().unwrap_or_default();
    let results: Vec<&String> = lines
        .iter()
        .filter(|l| l.starts_with(RESULT_MARKER))
        .collect();
    let what = format!(
        "the {} phase{}",
        req.phase,
        if req.backend.is_empty() {
            String::new()
        } else {
            format!(" of {}", req.backend)
        }
    );
    if results.len() != 1 {
        return Err(format!(
            "{what} crashed ({status}, {} result lines; log {}):\n{}",
            results.len(),
            log.display(),
            tail(log, 20)
        ));
    }
    let v: serde_json::Value = serde_json::from_str(&results[0][RESULT_MARKER.len()..])
        .map_err(|e| format!("{what}: malformed result line: {e}"))?;
    if v.get("ok").and_then(|x| x.as_bool()) != Some(true) {
        let err = v
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown error");
        return Err(format!("{what} failed: {err}\n{}", tail(log, 20)));
    }
    if !status.success() {
        return Err(format!(
            "{what} exited with {status} after reporting success"
        ));
    }
    Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null))
}
