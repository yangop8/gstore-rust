//! Workloads: a data generator, its queries and its update statements.
//!
//! A workload is fully described by a serializable [`WorkloadSpec`], which the
//! runner hands to every child process; [`WorkloadSpec::instantiate`] turns it
//! back into the [`Workload`]. Two are built in: [`crate::lubm::Lubm`] (the
//! LUBM-like reference workload) and [`Tiny`] (a few hundred triples, for the
//! harness's own tests).

use std::io::{self, BufWriter, Write};

use serde::{Deserialize, Serialize};

use crate::hash::{fnv64, HashingWriter};

/// What a generator wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GenStats {
    /// N-Triples lines written.
    pub lines: u64,
    /// Distinct triples among them (what a store holds after loading).
    pub unique_triples: u64,
    /// Bytes written.
    pub bytes: u64,
    /// FNV-1a 64 of the bytes written.
    pub fnv64: u64,
}

/// One query of a workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySpec {
    /// Short name (`q1` … `q14` for LUBM).
    pub name: String,
    /// The SPARQL text.
    pub text: String,
}

impl QuerySpec {
    /// FNV-1a 64 of the text.
    pub fn text_fnv64(&self) -> u64 {
        fnv64(self.text.as_bytes())
    }
}

/// A benchmark workload.
pub trait Workload: Send + Sync {
    /// The workload family (`lubm-like`, `tiny`).
    fn id(&self) -> &'static str;
    /// The generator version: part of every baseline's exact gates.
    fn version(&self) -> &'static str;
    /// A file-name stem unique to the generated data (family, version and
    /// every parameter), used for the data cache.
    fn file_stem(&self) -> String;
    /// The database name `gbuild` is given (the directory is `<name>.db`).
    fn db_name(&self) -> &'static str;
    /// Write the data set as N-Triples.
    fn generate(&self, out: &mut dyn Write) -> io::Result<GenStats>;
    /// Write the update chunk: triples disjoint from the data set (no triple
    /// of it is in the data), all distinct.
    fn generate_chunk(&self, out: &mut dyn Write) -> io::Result<GenStats>;
    /// The read queries, in reporting order.
    fn queries(&self) -> Vec<QuerySpec>;
    /// The queries the concurrency phase drives by default.
    fn conc_queries(&self) -> Vec<String>;
    /// The `i`-th single-triple `INSERT DATA` statement of the update phase
    /// (a triple that is not in the data set).
    fn single_insert(&self, i: usize) -> String;
    /// The statement that deletes [`single_insert`](Self::single_insert)'s
    /// triple again.
    fn single_delete(&self, i: usize) -> String;
}

/// A serializable description of a workload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadSpec {
    /// `lubm` or `tiny`.
    pub kind: String,
    /// LUBM: the number of universities. Tiny: the number of people / 50.
    pub scale: u32,
    /// The generator seed.
    #[serde(default)]
    pub seed: u64,
    /// LUBM only: at most this many departments per university (the `micro`
    /// profile uses 1). `None` = UBA's 15–25.
    #[serde(default)]
    pub departments: Option<u32>,
}

impl WorkloadSpec {
    pub fn lubm(scale: u32, seed: u64) -> WorkloadSpec {
        WorkloadSpec {
            kind: "lubm".into(),
            scale,
            seed,
            departments: None,
        }
    }

    pub fn tiny() -> WorkloadSpec {
        WorkloadSpec {
            kind: "tiny".into(),
            scale: 1,
            seed: 0,
            departments: None,
        }
    }

    /// The workload this spec describes.
    pub fn instantiate(&self) -> Result<Box<dyn Workload>, String> {
        match self.kind.as_str() {
            "lubm" | "lubm-like" => {
                if self.scale == 0 {
                    return Err("lubm needs --scale >= 1 (universities)".into());
                }
                if self.departments == Some(0) {
                    return Err("lubm needs --departments >= 1".into());
                }
                Ok(Box::new(crate::lubm::Lubm {
                    univ: self.scale,
                    seed: self.seed,
                    departments: self.departments,
                }))
            }
            "tiny" => Ok(Box::new(Tiny {
                scale: self.scale.max(1),
            })),
            other => Err(format!("unknown workload '{other}' (lubm, tiny)")),
        }
    }

    /// A short human label (`lubm-like u8`, `tiny`).
    pub fn label(&self) -> String {
        match self.kind.as_str() {
            "tiny" => format!("tiny x{}", self.scale),
            _ => match self.departments {
                Some(d) => format!("lubm-like univ {} (≤{d} dept)", self.scale),
                None => format!("lubm-like univ {}", self.scale),
            },
        }
    }
}

/// Generate a workload's data set (or chunk) into `out` through a 1 MiB
/// buffer, hashing what is written.
pub fn generate_to(
    w: &dyn Workload,
    out: &mut dyn Write,
    chunk: bool,
) -> io::Result<GenStats> {
    let mut hw = HashingWriter::new(out);
    let mut stats = {
        let mut buf = BufWriter::with_capacity(1 << 20, &mut hw);
        let s = if chunk {
            w.generate_chunk(&mut buf)?
        } else {
            w.generate(&mut buf)?
        };
        buf.flush()?;
        s
    };
    let (h, n) = hw.digest();
    stats.fnv64 = h;
    stats.bytes = n;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// tiny
// ---------------------------------------------------------------------------

/// A built-in toy workload: `50 × scale` people with names, ages, types,
/// `knows` edges and group memberships, plus five queries. Big enough to
/// exercise joins, UNION, OPTIONAL and FILTER on every backend, small enough
/// for debug-build tests.
pub struct Tiny {
    pub scale: u32,
}

const TINY: &str = "http://tiny.gstore-bench.org/";

impl Tiny {
    fn people(&self) -> u64 {
        50 * u64::from(self.scale)
    }
}

fn tiny_triples(n: u64, base: &str, out: &mut dyn Write) -> io::Result<u64> {
    let mut lines = 0;
    for i in 0..n {
        let s = format!("<{base}p{i}>");
        let kind = if i % 3 == 0 { "Student" } else { "Teacher" };
        writeln!(out, "{s} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{TINY}{kind}> .")?;
        writeln!(out, "{s} <{TINY}name> \"person {i}\" .")?;
        writeln!(out, "{s} <{TINY}knows> <{base}p{}> .", (i * 7 + 3) % n)?;
        writeln!(out, "{s} <{TINY}knows> <{base}p{}> .", (i + 1) % n)?;
        lines += 4;
        if i % 4 != 0 {
            // Some people have no age: OPTIONAL then leaves ?a unbound.
            writeln!(out, "{s} <{TINY}age> \"{}\"^^<http://www.w3.org/2001/XMLSchema#integer> .", 18 + i % 50)?;
            lines += 1;
        }
        writeln!(out, "{s} <{TINY}memberOf> <{TINY}group{}> .", i % 5)?;
        lines += 1;
    }
    Ok(lines)
}

impl Workload for Tiny {
    fn id(&self) -> &'static str {
        "tiny"
    }

    fn version(&self) -> &'static str {
        "tiny/1"
    }

    fn file_stem(&self) -> String {
        format!("tiny1-x{}", self.scale)
    }

    fn db_name(&self) -> &'static str {
        "tiny"
    }

    fn generate(&self, out: &mut dyn Write) -> io::Result<GenStats> {
        let n = self.people();
        // (i*7+3) % n and (i+1) % n coincide only when 6i+2 ≡ 0 (mod n); for
        // n = 50·scale that has solutions, so count the distinct lines.
        let lines = tiny_triples(n, TINY, out)?;
        let dup = (0..n).filter(|i| (i * 7 + 3) % n == (i + 1) % n).count() as u64;
        Ok(GenStats {
            lines,
            unique_triples: lines - dup,
            ..GenStats::default()
        })
    }

    fn generate_chunk(&self, out: &mut dyn Write) -> io::Result<GenStats> {
        let base = format!("{TINY}chunk/");
        let n = 10;
        let lines = tiny_triples(n, &base, out)?;
        let dup = (0..n).filter(|i| (i * 7 + 3) % n == (i + 1) % n).count() as u64;
        Ok(GenStats {
            lines,
            unique_triples: lines - dup,
            ..GenStats::default()
        })
    }

    fn queries(&self) -> Vec<QuerySpec> {
        let q = |name: &str, body: &str| QuerySpec {
            name: name.into(),
            text: format!("PREFIX t: <{TINY}>\n{body}\n"),
        };
        vec![
            q("t1", "SELECT ?x WHERE { ?x a t:Student . ?x t:memberOf t:group1 . }"),
            q("t2", "SELECT ?x ?y WHERE { ?x t:knows ?y . ?y a t:Student . ?x a t:Student . }"),
            q("t3", "SELECT ?x ?a WHERE { ?x a t:Teacher . OPTIONAL { ?x t:age ?a } }"),
            q("t4", "SELECT ?x WHERE { { ?x t:memberOf t:group2 } UNION { ?x t:memberOf t:group3 } }"),
            q("t5", "SELECT ?x ?n WHERE { ?x t:name ?n . ?x t:age ?a . FILTER(?a > 40) }"),
        ]
    }

    fn conc_queries(&self) -> Vec<String> {
        vec!["t1".into(), "t2".into()]
    }

    fn single_insert(&self, i: usize) -> String {
        format!("INSERT DATA {{ <{TINY}single/{i}> <{TINY}name> \"single {i}\" . }}")
    }

    fn single_delete(&self, i: usize) -> String {
        format!("DELETE DATA {{ <{TINY}single/{i}> <{TINY}name> \"single {i}\" . }}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_counts_distinct_lines() {
        for scale in [1, 2, 3] {
            let t = Tiny { scale };
            let mut v = Vec::new();
            let s = generate_to(&t, &mut v, false).unwrap();
            let text = String::from_utf8(v).unwrap();
            let mut lines: Vec<&str> = text.lines().collect();
            assert_eq!(lines.len() as u64, s.lines);
            lines.sort_unstable();
            lines.dedup();
            assert_eq!(lines.len() as u64, s.unique_triples);
            assert_eq!(s.bytes, text.len() as u64);
        }
    }
}
