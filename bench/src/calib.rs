//! Machine calibration `calib/1`: three CPU/memory micro-benchmarks whose
//! geometric mean ([`Calibration::score_ms`]) normalizes timing gates between
//! machines, plus an fsync probe (report-only).
//!
//! * `sort_ms` — sort 2 Mi pseudo-random `u64`s;
//! * `hashmap_ms` — insert 1 Mi `u64` keys into a `HashMap` and look each up;
//! * `format_ms` — format 500 Ki IRI-like strings into one `String`;
//! * `fsync_ms` — write and fsync a 4 KiB file (median of 5).
//!
//! Each CPU figure is the minimum of five runs.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::hint::black_box;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::rng::SplitMix64;

pub const SCHEMA: &str = "calib/1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Calibration {
    pub schema: String,
    pub sort_ms: f64,
    pub hashmap_ms: f64,
    pub format_ms: f64,
    #[serde(default)]
    pub fsync_ms: Option<f64>,
    /// Geometric mean of the three CPU figures.
    pub score_ms: f64,
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn min_of<F: FnMut() -> f64>(n: usize, mut f: F) -> f64 {
    (0..n).map(|_| f()).fold(f64::INFINITY, f64::min)
}

fn sort_once(scale: usize) -> f64 {
    let mut r = SplitMix64::new(42);
    let mut v: Vec<u64> = (0..scale).map(|_| r.next_u64()).collect();
    let t = Instant::now();
    v.sort_unstable();
    black_box(&v);
    ms(t)
}

fn hashmap_once(scale: usize) -> f64 {
    let mut r = SplitMix64::new(43);
    let keys: Vec<u64> = (0..scale).map(|_| r.next_u64()).collect();
    let t = Instant::now();
    let mut m: HashMap<u64, u64> = HashMap::with_capacity(keys.len());
    for (i, k) in keys.iter().enumerate() {
        m.insert(*k, i as u64);
    }
    let mut sum = 0u64;
    for k in &keys {
        sum = sum.wrapping_add(m[k]);
    }
    black_box(sum);
    ms(t)
}

fn format_once(scale: usize) -> f64 {
    let t = Instant::now();
    let mut s = String::with_capacity(scale * 64);
    for i in 0..scale {
        let _ = writeln!(
            s,
            "<http://www.Department{}.University{}.edu/GraduateStudent{i}>",
            i % 25,
            i % 1000
        );
    }
    black_box(&s);
    ms(t)
}

/// Write and fsync a 4 KiB file in `dir` five times; the median in ms.
pub fn fsync_probe(dir: &Path) -> Option<f64> {
    let p = dir.join(format!(".gstore-bench-fsync-{}", std::process::id()));
    let mut samples = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let mut f = std::fs::File::create(&p).ok()?;
        f.write_all(&[0x5a; 4096]).ok()?;
        f.sync_all().ok()?;
        samples.push(ms(t));
    }
    let _ = std::fs::remove_file(&p);
    samples.sort_by(f64::total_cmp);
    Some(samples[samples.len() / 2])
}

/// Run the calibration. `quick` shrinks the workloads 16× (tests).
pub fn calibrate(fsync_dir: Option<&Path>, quick: bool) -> Calibration {
    let div = if quick { 16 } else { 1 };
    let sort_ms = min_of(5, || sort_once((2 << 20) / div));
    let hashmap_ms = min_of(5, || hashmap_once((1 << 20) / div));
    let format_ms = min_of(5, || format_once((500 << 10) / div));
    let score_ms = (sort_ms.max(1e-6) * hashmap_ms.max(1e-6) * format_ms.max(1e-6)).cbrt();
    Calibration {
        schema: SCHEMA.into(),
        sort_ms,
        hashmap_ms,
        format_ms,
        fsync_ms: fsync_dir.and_then(fsync_probe),
        score_ms,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn quick_calibration_is_positive() {
        let c = super::calibrate(Some(&std::env::temp_dir()), true);
        assert!(c.score_ms > 0.0 && c.sort_ms > 0.0);
        assert_eq!(c.schema, super::SCHEMA);
        assert!(c.fsync_ms.is_some());
    }
}
