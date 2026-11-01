//! gstore-bench: the benchmark harness of gstore-rust.
//!
//! * [`rng`], [`hash`] — the deterministic generator stream and the content
//!   hashes every result is checked with;
//! * [`workload`], [`lubm`] — workloads: the LUBM-like reference data and
//!   queries, and a tiny one for tests;
//! * [`schema`] — the `gstore-bench/1` result and baseline format;
//! * [`gates`] — the `gstore-bench-gates/1` thresholds and their evaluation;
//! * [`runner`] — the phases (each in a fresh child process), the profiles
//!   and the command-line driver of the `gstore-bench` binary;
//! * [`report`] — Markdown reports;
//! * [`calib`], [`sys`] — machine calibration and process statistics.
//!
//! See `bench/README.md` for the command line.

pub mod calib;
pub mod gates;
pub mod hash;
pub mod lubm;
pub mod report;
pub mod rng;
pub mod runner;
pub mod schema;
pub mod sys;
pub mod workload;

/// The harness version recorded in every result.
pub const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");
