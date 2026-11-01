//! The child-process protocol.
//!
//! Every phase runs in a fresh `gstore-bench __child` process: the parent
//! writes one JSON [`ChildRequest`] to its standard input and reads exactly
//! one `GSTORE_BENCH_RESULT {json}` line from its standard output —
//! `{"ok":true,"data":…}` or `{"ok":false,"error":"…"}`. Anything else (no
//! line, two lines, a crash, a timeout) fails the run with exit code 2.
//!
//! A fresh process per phase keeps measurements independent: the open
//! starts with an empty heap, peak RSS (`VmHWM`) belongs to the phase alone,
//! and the build child's `getrusage(RUSAGE_CHILDREN)` sees only `gbuild`.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

use crate::runner::conc::ConcSettings;
use crate::runner::phases::Expected;
use crate::runner::{backends, conc, phases};
use crate::workload::WorkloadSpec;

/// The marker that starts the result line.
pub const RESULT_MARKER: &str = "GSTORE_BENCH_RESULT ";

/// The hidden subcommand that runs one phase.
pub const CHILD_COMMAND: &str = "__child";

/// One phase to run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildRequest {
    /// `calibrate`, `build`, `open`, `query`, `conc` or `update`.
    pub phase: String,
    #[serde(default)]
    pub backend: String,
    pub workload: WorkloadSpec,
    /// The backend's work directory (`gbuild`'s current directory).
    #[serde(default)]
    pub work_dir: PathBuf,
    #[serde(default)]
    pub data_file: PathBuf,
    #[serde(default)]
    pub chunk_file: Option<PathBuf>,
    #[serde(default)]
    pub chunk_triples: u64,
    #[serde(default)]
    pub bin_dir: PathBuf,
    /// `cli` or `lib` (see [`backends`]).
    #[serde(default)]
    pub builder: String,
    #[serde(default)]
    pub gbuild_args: Vec<String>,
    #[serde(default)]
    pub reps: usize,
    #[serde(default)]
    pub drop_caches: bool,
    #[serde(default)]
    pub singles: usize,
    #[serde(default)]
    pub conc: Option<ConcSettings>,
    /// The query phase's answers, which the update phase must reproduce.
    #[serde(default)]
    pub expected: Expected,
    /// Test hook: `crash:<phase>` aborts, `timeout:<phase>` hangs.
    #[serde(default)]
    pub inject: Option<String>,
    /// Shrink the calibration (tests).
    #[serde(default)]
    pub quick: bool,
}

#[derive(Serialize)]
struct Reply<'a> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a str>,
}

fn reply(r: &Result<serde_json::Value, String>) {
    let rep = match r {
        Ok(v) => Reply {
            ok: true,
            data: Some(v.clone()),
            error: None,
        },
        Err(e) => Reply {
            ok: false,
            data: None,
            error: Some(e),
        },
    };
    let line = serde_json::to_string(&rep).unwrap_or_else(|e| {
        format!("{{\"ok\":false,\"error\":\"cannot serialize the result: {e}\"}}")
    });
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{RESULT_MARKER}{line}");
    let _ = out.flush();
}

fn to_value<T: Serialize>(v: T) -> Result<serde_json::Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

fn run(req: &ChildRequest) -> Result<serde_json::Value, String> {
    let w = req.workload.instantiate()?;
    let db_dir = backends::db_dir(&req.work_dir, w.as_ref());
    match req.phase.as_str() {
        "calibrate" => to_value(crate::calib::calibrate(
            Some(&req.work_dir).filter(|p| !p.as_os_str().is_empty()).map(|p| p.as_path()),
            req.quick,
        )),
        "build" => to_value(backends::build(
            &req.backend,
            &req.builder,
            &req.bin_dir,
            &req.gbuild_args,
            &req.work_dir,
            &req.data_file,
            w.as_ref(),
        )?),
        "open" => to_value(phases::open_phase(&db_dir, req.drop_caches)?),
        "query" => to_value(phases::query_phase(&db_dir, w.as_ref(), req.reps)?),
        "conc" => {
            let s = req.conc.as_ref().ok_or("the conc phase needs settings")?;
            to_value(conc::conc_phase(&db_dir, w.as_ref(), s)?)
        }
        "update" => {
            let chunk = req.chunk_file.as_ref().ok_or("the update phase needs the chunk")?;
            to_value(phases::update_phase(
                &db_dir,
                w.as_ref(),
                req.singles,
                chunk,
                req.chunk_triples,
                &req.expected,
            )?)
        }
        other => Err(format!("unknown phase '{other}'")),
    }
}

/// The `__child` entry point.
pub fn child_main() -> ExitCode {
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        reply(&Err(format!("cannot read the request: {e}")));
        return ExitCode::from(2);
    }
    let req: ChildRequest = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => {
            reply(&Err(format!("invalid request: {e}")));
            return ExitCode::from(2);
        }
    };
    if let Some(inject) = &req.inject {
        if let Some((what, phase)) = inject.split_once(':') {
            if phase == req.phase {
                match what {
                    "crash" => std::process::abort(),
                    "timeout" => loop {
                        std::thread::sleep(std::time::Duration::from_secs(3600));
                    },
                    _ => {}
                }
            }
        }
    }
    let r = run(&req);
    reply(&r);
    if r.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
