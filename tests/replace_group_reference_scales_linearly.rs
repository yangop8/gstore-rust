//! `REPLACE`'s replacement string: a `$` followed by a long run of digits
//! is parsed in time linear in the run (round-3 review, XPath `fn:replace`
//! §5.6.3; SPARQL 1.1 §17.4.3.15).
//!
//! §5.6.3 makes a reference `$N` whose N names no group (and is above 9)
//! give its last digit back as literal text, repeatedly. The parser gave
//! the digits back one at a time with an insert at the front of the
//! literal tail, so a run of n digits cost O(n²) — per evaluated solution,
//! with no cancellation check inside: 400,000 digits took 1.7 s, 1.6 million
//! 28 s, and a replacement at the 16 MiB `sparql_max_size` would have taken
//! about 45 minutes per row. The reference is now chosen in one forward
//! pass (the prefix values only grow), and the rest of the run is appended
//! as one piece of text.

use std::time::{Duration, Instant};

use gstore::query::Evaluator;
use gstore::{Database, QueryResult};

/// The query replacing the one `a` of the data by `$1` followed by `digits`
/// more ones: the reference names group 1, the other digits are text.
fn query(digits: usize) -> String {
    format!(
        "SELECT (REPLACE(?o, \"(a)\", \"$1{}\") AS ?r) WHERE {{ ?s ?p ?o }}",
        "1".repeat(digits)
    )
}

/// The CPU time this thread has used (Linux; wall time elsewhere): what
/// an evaluation costs whatever else the machine runs — wall time also
/// counts the time the thread waits for a CPU, which on a busy machine hits
/// a long run far more than a short one.
fn thread_cpu_time() -> Duration {
    #[cfg(target_os = "linux")]
    {
        #[repr(C)]
        struct Timespec {
            tv_sec: std::ffi::c_long,
            tv_nsec: std::ffi::c_long,
        }
        extern "C" {
            fn clock_gettime(clock: i32, tp: *mut Timespec) -> i32;
        }
        const CLOCK_THREAD_CPUTIME_ID: i32 = 3;
        let mut ts = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec.
        if unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0 {
            return Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32);
        }
    }
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

/// Evaluate `q` against a fresh evaluator (no result cache), timing the
/// evaluation only (the query text is parsed outside the clock) as the CPU
/// time of this thread.
fn run(db: &Database, q: &str, digits: usize) -> Duration {
    let parsed = gstore::parser::sparql::parse(q).expect("parse");
    let t = thread_cpu_time();
    let r = Evaluator::new(db.dict(), db.store()).evaluate(&parsed);
    let elapsed = thread_cpu_time().saturating_sub(t);
    match r {
        Ok(QueryResult::Select(rs)) => {
            assert_eq!(rs.rows.len(), 1);
            let cell = rs.rows[0][0].as_deref().expect("?r is bound");
            // "a" replaced by the group's "a" and the digits as text, in
            // the literal's quoted form.
            let ones = cell.bytes().filter(|&b| b == b'1').count();
            assert_eq!(ones, digits, "the digits after the reference are text");
            assert!(cell.starts_with("\"a1"), "{}", &cell[..cell.len().min(20)]);
        }
        other => panic!("REPLACE with {digits} digits: {other:?}"),
    }
    elapsed
}

/// D9 (a doubling test, timed because the parser has no work counter):
/// four times the digits cost about four times as long. The CPU time of
/// the evaluating thread is measured, the small and the large run
/// alternate, best of three each, so a change of the machine's load hits
/// both alike, and a miss is measured again (up to three times) before it
/// fails; the bound (8×, plus 50 ms) leaves a linear parser room and still
/// fails a quadratic one by twice (200,000 digits 0.43 s against 6.5 s for
/// 800,000 before the fix).
#[test]
fn a_long_digit_run_after_a_dollar_costs_linear_time() {
    let db = Database::build_from_str("replace", "<http://ex/s> <http://ex/p> \"a\" .\n")
        .expect("build the test database");
    let (n_small, n_large) = (200_000, 800_000);
    let (q_small, q_large) = (query(n_small), query(n_large));
    let mut miss = String::new();
    for attempt in 0..3 {
        let (mut small, mut large) = (Duration::MAX, Duration::MAX);
        for _ in 0..3 {
            small = small.min(run(&db, &q_small, n_small));
            large = large.min(run(&db, &q_large, n_large));
        }
        if large <= small * 8 + Duration::from_millis(50) {
            return;
        }
        miss = format!(
            "4x the digits took {:.1}x as long ({small:?} -> {large:?} CPU time, attempt {attempt})",
            large.as_secs_f64() / small.as_secs_f64().max(1e-9)
        );
        eprintln!("{miss}");
    }
    panic!("{miss}");
}

/// The references a long run resolves to keep §5.6.3's answers: a run of
/// zeros is group 0 (the whole match), digits after an existing group are
/// text, and a reference above 9 to no group gives its digits back.
#[test]
fn long_digit_runs_keep_the_xpath_answers() {
    let mut db = Database::build_from_str("replace", "<http://ex/s> <http://ex/p> \"xay\" .\n")
        .expect("build the test database");
    let cell = |db: &mut Database, rep: &str| -> String {
        let q = format!("SELECT (REPLACE(?o, \"(a)\", \"{rep}\") AS ?r) WHERE {{ ?s ?p ?o }}");
        match db.query(&q) {
            Ok(QueryResult::Select(rs)) => rs.rows[0][0].clone().expect("?r is bound"),
            other => panic!("{rep}: {other:?}"),
        }
    };
    let zeros = "0".repeat(10_000);
    assert_eq!(cell(&mut db, &format!("[${zeros}]")), "\"x[a]y\"");
    assert_eq!(
        cell(&mut db, &format!("$1{}", "2".repeat(5))),
        "\"xa22222y\""
    );
    // $23… names no group (one exists) and is above 9: "$2" (absent, empty
    // text) followed by the digits after it.
    assert_eq!(cell(&mut db, "$2345"), "\"x345y\"");
}
