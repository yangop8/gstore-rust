//! Contract C2: per-query read errors.
//!
//! A backend reports a failed read with `note_read_error`; the error lands in
//! the innermost `ReadScope` of the thread that caused the read, so it fails
//! exactly that statement — never a statement running on another thread —
//! and with no scope active it is counted process-wide.

use std::sync::{Arc, Barrier};

use gstore::store::read_scope::{in_scope, note_read_error, unscoped_read_errors, ReadScope};
use gstore::{Database, GStoreError, QueryResult, Value};

const DATA: &str = "\
<http://ex/a> <http://ex/p> <http://ex/b> .
<http://ex/b> <http://ex/p> <http://ex/c> .
<http://ex/c> <http://ex/q> \"lit\" .
";

fn db() -> Database {
    let mut db = Database::build_from_str("rs", DATA).unwrap();
    // A user function standing in for a failing backend read: it runs inside
    // the query's evaluation, on the query's thread.
    db.register_function("faulty", |_| {
        note_read_error("injected read failure");
        Some(Value::Bool(true))
    });
    db
}

#[test]
fn only_the_query_with_the_injected_error_fails() {
    let db = db();
    let good = "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o } ORDER BY ?s";
    let bad = "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o FILTER(faulty(?s)) }";
    let expected = db.query_read(good, None, None).unwrap();
    let QueryResult::Select(rs) = &expected else {
        panic!("SELECT expected")
    };
    assert_eq!(rs.row_count(), 2);
    let barrier = Arc::new(Barrier::new(2));
    let db = &db;
    std::thread::scope(|s| {
        let b1 = Arc::clone(&barrier);
        let failing = s.spawn(move || {
            b1.wait();
            (0..20)
                .map(|_| db.query_read(bad, None, None))
                .collect::<Vec<_>>()
        });
        let b2 = Arc::clone(&barrier);
        let clean = s.spawn(move || {
            b2.wait();
            (0..20)
                .map(|_| db.query_read(good, None, None))
                .collect::<Vec<_>>()
        });
        for r in failing.join().unwrap() {
            let e = r.unwrap_err();
            assert!(matches!(e, GStoreError::Database(_)), "{e}");
            assert!(e.to_string().contains("injected read failure"), "{e}");
        }
        for r in clean.join().unwrap() {
            assert_eq!(r.unwrap(), expected);
        }
    });
    // The failed query left nothing behind on this thread either.
    assert!(!in_scope());
    assert_eq!(db.query_read(good, None, None).unwrap(), expected);
}

#[test]
fn the_exclusive_path_fails_the_same_way() {
    let mut db = db();
    let bad = "ASK { ?s <http://ex/p> ?o FILTER(faulty(?s)) }";
    assert!(db.query(bad).is_err());
    // An UPDATE whose WHERE clause hits a read error applies nothing.
    let before = (db.mutation_count(), db.triple_num());
    let update = "INSERT { ?s <http://ex/r> ?o } WHERE { ?s <http://ex/p> ?o FILTER(faulty(?s)) }";
    let e = db.query(update).unwrap_err();
    assert!(e.to_string().contains("injected read failure"), "{e}");
    assert_eq!((db.mutation_count(), db.triple_num()), before);
}

#[test]
fn scopes_are_per_thread_and_unscoped_errors_are_counted() {
    let barrier = Arc::new(Barrier::new(2));
    let b1 = Arc::clone(&barrier);
    let noisy = std::thread::spawn(move || {
        let scope = ReadScope::enter();
        note_read_error("thread A");
        b1.wait(); // the other thread checks its own scope now
        b1.wait();
        scope.take_error()
    });
    let b2 = Arc::clone(&barrier);
    let quiet = std::thread::spawn(move || {
        let scope = ReadScope::enter();
        b2.wait();
        let seen = scope.has_error();
        b2.wait();
        seen
    });
    assert_eq!(noisy.join().unwrap().as_deref(), Some("thread A"));
    assert!(
        !quiet.join().unwrap(),
        "an error leaked into another thread's scope"
    );

    // No scope on this thread: counted, not lost.
    let before = unscoped_read_errors();
    std::thread::spawn(|| note_read_error("nobody is listening"))
        .join()
        .unwrap();
    assert!(unscoped_read_errors() > before);
}
