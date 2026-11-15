//! Single-request process killers (review package r1-crash-dos):
//!
//! - `shortestPathCount` on a regular layered graph whose path counts reach
//!   a multiple of 2^32 (review H-8, analytics#1): the count that wraps to 0
//!   was also the "not reached" marker, so the frontier doubled every level
//!   until an allocation failure aborted the process;
//! - `maximumKplex` on a CSR without vertices (gap-gfun-fuzz#1) panicked;
//! - log retention on a sibling file whose name has a multi-byte character
//!   at byte 10 after the prefix (M15, bulkcli#1) panicked — `gserver`
//!   could not start;
//! - the SPARQL lexer was quadratic on long name runs with `-` / `.` and no
//!   `:` (H-14, parser#2): one request burnt minutes of CPU;
//! - a log line was written with two `write(2)` calls, so concurrent
//!   writers interleaved (conc#6).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use gstore::logging::{rolled_files, FileSinkConfig, Level, LogConfig, Logger};
use gstore::parser::ntriples::parse_term;
use gstore::{Database, Term};

/// A temporary directory, removed when dropped (also when a test fails).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "gstore_crash_regr_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The lexical form of the plain literal in the only cell of `q`'s result.
fn only_plain(db: &mut Database, q: &str) -> String {
    let rs = db.select(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    assert_eq!(rs.rows.len(), 1, "{q}: {rs:?}");
    let cell = rs.rows[0][0]
        .clone()
        .unwrap_or_else(|| panic!("{q}: unbound"));
    match parse_term(&cell).unwrap() {
        Term::Literal {
            value,
            datatype: None,
            lang: None,
        } => value,
        other => panic!("{q}: expected a plain literal, got {other:?}"),
    }
}

// ---- shortestPathCount (H-8) ---------------------------------------------------

/// A 2-wide ladder of `levels` levels over `<p>`: `s → {a1, b1}`,
/// `{ai, bi} → {ai+1, bi+1}`, `{aL, bL} → t` (4·L triples, 2^L shortest
/// paths `s → t`), plus — with `chain` — one more path of the same length
/// `s → c1 → … → cL → t`.
fn ladder(levels: usize, chain: bool) -> String {
    let mut nt = String::new();
    let mut edge = |a: &str, b: &str| {
        nt.push_str(&format!(
            "<http://ex/{a}> <http://ex/p> <http://ex/{b}> .\n"
        ));
    };
    edge("s", "a1");
    edge("s", "b1");
    for i in 1..levels {
        for x in ["a", "b"] {
            for y in ["a", "b"] {
                edge(&format!("{x}{i}"), &format!("{y}{}", i + 1));
            }
        }
    }
    edge(&format!("a{levels}"), "t");
    edge(&format!("b{levels}"), "t");
    if chain {
        edge("s", "c1");
        for i in 1..levels {
            edge(&format!("c{i}"), &format!("c{}", i + 1));
        }
        edge(&format!("c{levels}"), "t");
    }
    nt
}

fn path_count(db: &mut Database) -> String {
    only_plain(
        db,
        "SELECT (shortestPathCount(<http://ex/s>, <http://ex/t>, true, {<http://ex/p>}) AS ?c) \
         WHERE {}",
    )
}

fn count_entry(n: i64) -> String {
    format!(
        "{{\"paths\":[{{\"src\":\"<http://ex/s>\", \"dst\":\"<http://ex/t>\", \"count\":{n}}}]}}"
    )
}

/// The count is upstream's C `int` (wrapping, so 2^L + 1 prints as 1 for
/// L >= 32), and the search stays one vertex per level and side. Before the
/// fix the 400-triple ladder (L = 100) aborted the process: once a vertex's
/// count wrapped to exactly 0 it read as "not reached", was enqueued again
/// for every in-edge, and the frontier doubled each level until an
/// allocation failed.
#[test]
fn shortest_path_count_on_a_deep_ladder_completes_with_the_upstream_count() {
    let cases = [
        (20, false, 1 << 20),
        (20, true, (1 << 20) + 1),
        (31, false, i64::from(i32::MIN)), // 2^31 as a C int
        (32, false, 0),
        (70, true, 1),
        (100, false, 0),
        (100, true, 1),
    ];
    for (levels, chain, expected) in cases {
        let nt = ladder(levels, chain);
        let mut db = Database::build_from_str("crash_ladder", &nt).unwrap();
        let started = Instant::now();
        assert_eq!(
            path_count(&mut db),
            count_entry(expected),
            "ladder L={levels} chain={chain}"
        );
        // A hang guard, not the assertion: the fixed search is a few
        // hundred steps.
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "L={levels}: {:?}",
            started.elapsed()
        );
    }
}

// ---- maximumKplex on an empty CSR (gap-gfun-fuzz#1) ----------------------------

/// A database without entity vertices (empty, or literal objects only)
/// answers the empty k-plex for every k (before: `k <= 2` panicked inside
/// the k-plex worker and the query failed with "internal error"; `k = 3`
/// printed `{"kplex":[,,,]}` on the empty database).
#[test]
fn maximum_kplex_on_a_vertexless_graph_is_empty() {
    let empty = Database::new("crash_kplex_empty");
    let literal_only =
        Database::build_from_str("crash_kplex_lit", "<http://ex/d> <http://ex/q> \"lit\" .\n")
            .unwrap();
    for (name, mut db) in [("empty", empty), ("literal-only", literal_only)] {
        for call in [
            "maximumKplex({})",
            "maximumKplex({}, 1)",
            "maximumKplex({}, 2)",
            "maximumKplex({}, 3)",
            "maximumKplex({<http://ex/q>}, 2)",
        ] {
            assert_eq!(
                only_plain(&mut db, &format!("SELECT ({call} AS ?r) WHERE {{}}")),
                r#"{"paths":[{"kplex":[]}]}"#,
                "{name}: {call}"
            );
        }
    }
}

// ---- log retention (M15) ----------------------------------------------------------

/// Backdate `path`'s modification time by `days` days.
fn backdate(path: &Path, days: u64) {
    let f = fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(days * 86_400))
        .unwrap();
}

/// A sibling of the log file whose name has a multi-byte character at byte
/// 10 after `<name>.` is skipped, not a panic: the logger `gserver` opens at
/// start-up comes up (before: exit 101 at start-up), and the daily roll of
/// the access/query logs goes on (before: a panic in the first request of
/// each day). Rolled files are still recognised and pruned.
#[test]
fn log_retention_skips_non_ascii_sibling_names() {
    let dir = TempDir::new("logs");
    let log = dir.path().join("diag.log");
    fs::write(&log, "old\n").unwrap();
    let odd = [
        "diag.log.旧日志文件",
        "diag.log.2024-01-0é",
        "diag.log.2024-01-01é",
        "diag.log.é",
    ];
    for name in odd {
        fs::write(dir.path().join(name), "x").unwrap();
    }
    let rolled = dir.path().join("diag.log.2020-01-01");
    fs::write(&rolled, "rolled\n").unwrap();
    assert_eq!(
        rolled_files(&log)
            .into_iter()
            .map(|(p, d)| (p, d.iso()))
            .collect::<Vec<_>>(),
        vec![(rolled.clone(), "2020-01-01".to_string())]
    );
    let cfg = LogConfig {
        level: Some(Level::Info),
        stderr: None,
        file: Some(FileSinkConfig {
            path: log.clone(),
            days: 30,
            threshold: Level::Trace,
        }),
    };
    let logger = Logger::new(&cfg).expect("the logger opens");
    logger.log(Level::Info, "test", format_args!("started"));
    assert!(
        !rolled.exists(),
        "a rolled file older than 30 days is pruned"
    );
    for name in odd {
        assert!(dir.path().join(name).exists(), "{name} is not touched");
    }

    // The access/query logs roll on the first line of a new day.
    let access = dir.path().join("access.log");
    fs::write(&access, "yesterday\n").unwrap();
    fs::write(dir.path().join("access.log.旧日志文件"), "x").unwrap();
    backdate(&access, 1);
    gstore::logging::roll_daily(&access, 30);
    assert!(!access.exists(), "the stale log was rolled");
    assert_eq!(rolled_files(&access).len(), 1);
}

// ---- lexer complexity (H-14) ------------------------------------------------------

/// Lexing a 1 MB name run with `-` / `.` separators and no `:` is linear:
/// far below a second in a release build, and well within the generous
/// bound here in the test profile (before: about 20 minutes at the test
/// profile's opt-level — 80 KB already took ~3 s and each doubling cost 4×).
#[test]
fn lexing_a_megabyte_name_run_is_fast() {
    for sep in ["-", "."] {
        let run = format!("a{sep}").repeat(1 << 19);
        let q = format!("ASK {{ FILTER(?x = {run}a) }}");
        let started = Instant::now();
        let tokens = gstore::parser::sparql::lexer::tokenize(&q).unwrap();
        let took = started.elapsed();
        assert!(tokens.len() > 1 << 20, "{}", tokens.len());
        assert!(took < Duration::from_secs(20), "sep {sep:?}: {took:?}");
    }
    // A long run that does end in a prefix is one prefixed name.
    let q = format!("ASK {{ FILTER(?x = {}a:b) }}", "a-".repeat(1 << 18));
    let tokens = gstore::parser::sparql::lexer::tokenize(&q).unwrap();
    assert!(tokens.iter().any(
        |t| matches!(t, gstore::parser::sparql::lexer::Token::PName(p, l)
            if p.len() == (1 << 19) + 1 && l == "b")
    ));
}

// ---- log lines (conc#6) --------------------------------------------------------------

/// Concurrent writers of a plain-text log (the REST access/query/txn logs)
/// never interleave inside a line: each line is one `write(2)` (before: the
/// line and its newline were two, so a line could end up glued to another
/// with an empty line after them).
#[test]
fn concurrent_log_lines_never_interleave() {
    const THREADS: usize = 8;
    const LINES: usize = 4000;
    let dir = TempDir::new("loglines");
    let path = dir.path().join("access.log");
    let barrier = std::sync::Barrier::new(THREADS);
    thread::scope(|s| {
        for t in 0..THREADS {
            let (path, barrier) = (&path, &barrier);
            s.spawn(move || {
                barrier.wait();
                for i in 0..LINES {
                    let line = format!("{t}\t{i}\t{}", "x".repeat(40 + t));
                    gstore::logging::append_line(path, &line).unwrap();
                }
            });
        }
    });
    let text = fs::read_to_string(&path).unwrap();
    let mut seen = vec![0usize; THREADS];
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 3, "malformed line {line:?}");
        let t: usize = f[0].parse().unwrap_or_else(|_| panic!("{line:?}"));
        assert_eq!(f[2], "x".repeat(40 + t), "malformed line {line:?}");
        seen[t] += 1;
    }
    assert_eq!(seen, vec![LINES; THREADS]);
}

/// One `GET` to `addr` with root's Basic credentials (`root:root`); the
/// status code.
fn get_status(addr: &std::net::SocketAddr, target: &str) -> u16 {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    write!(
        s,
        "GET {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Basic cm9vdDpyb290\r\n\
         Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    resp.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status in {resp:?}"))
}

/// The REST server's access log under concurrent requests: one well-formed
/// line per request (a guard over the server path that writes through
/// [`gstore::logging::append_line`]; the interleaving window per request is
/// small, so the test above is the one that caught the two-write lines).
#[test]
fn concurrent_rest_requests_log_whole_lines() {
    const THREADS: usize = 8;
    const REQUESTS: usize = 40;
    let dir = TempDir::new("restlog");
    let root = dir.path().join("root");
    let server = std::sync::Arc::new(
        gstore::http_api::ApiServer::bind(&root, "root", "127.0.0.1:0").unwrap(),
    );
    let addr = server.local_addr().unwrap();
    let s2 = std::sync::Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                for _ in 0..REQUESTS {
                    assert_eq!(get_status(&addr, "/status"), 200);
                }
            });
        }
    });
    server.shutdown();
    let _ = serving.join();
    let text = fs::read_to_string(root.join("access.log")).unwrap();
    let mut status_lines = 0;
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 6, "malformed access-log line {line:?}");
        assert!(f[0].bytes().all(|b| b.is_ascii_digit()), "{line:?}");
        if f[1] == "GET" && f[2] == "/status" {
            status_lines += 1;
        }
    }
    assert_eq!(status_lines, THREADS * REQUESTS, "{text}");
}
