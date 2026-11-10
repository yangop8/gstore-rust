//! Upstream conformance: `scripts/test/update_test.sh` — single-triple
//! `insert data` statements sent through the `query` operation, the way the
//! script's `run_batch_insert` sends them (`<s{i%10}> <p{i%10}> <o{i}>`).
//!
//! The script prints the answers without checking them. Each statement adds
//! one new triple, so each answers `"update query returns true."` with
//! `AnsNum` 1 — except an insert of a triple already there, which changes
//! nothing (`AnsNum` 0). The inserts are saved (`check_point`), so they
//! survive an unload / load.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use std::path::Path;

use harness::*;

/// The script's `run_batch_insert DB TOTAL`: insert `total` triples; the
/// statement of index `idx` must change `changed(idx)` triples.
fn run_batch_insert(srv: &Server, db: &str, total: usize, changed: impl Fn(usize) -> i64) {
    for idx in 0..total {
        let (s, p, o) = (idx % 10, idx % 10, idx);
        let sparql = format!("insert data {{ <s{s}> <p{p}> <o{o}>. }}");
        let j = srv.query(db, &sparql);
        ok(&j);
        assert_eq!(string(&j, "StatusMsg"), "update query returns true.", "{j}");
        assert_eq!(int(&j, "AnsNum"), changed(idx), "{sparql}");
    }
}

fn triple_num(srv: &Server, db: &str) -> i64 {
    int(&srv.op("monitor", &[("db_name", db)]), "tripleNum")
}

/// Every triple `run_batch_insert` wrote (`total` of them) is there.
fn assert_inserted(srv: &Server, db: &str, total: usize) {
    let j = srv.query(
        db,
        "select ?s ?p ?o where { ?s ?p ?o . FILTER(isIRI(?o) && regex(str(?o), \"^o[0-9]+$\")) }",
    );
    ok(&j);
    let t = answer_table(&j);
    let vars: Vec<String> = ["s", "p", "o"].iter().map(|v| v.to_string()).collect();
    let mut want: Vec<Vec<Cell>> = (0..total)
        .map(|i| {
            vec![
                Some(format!("<s{}>", i % 10)),
                Some(format!("<p{}>", i % 10)),
                Some(format!("<o{i}>")),
            ]
        })
        .collect();
    want.sort();
    assert_eq!(t.normalized(&vars), want);
}

/// The script's default mode: build `./data/lubm/lubm.nt`, load, insert 10
/// triples, unload, drop.
#[test]
fn default_mode_on_lubm() {
    let srv = Server::start("update_lubm");
    let db = "update_auto";
    let j = srv.build(db, &data("lubm/lubm.nt"));
    ok(&j);
    assert_eq!(int(&j, "success_num"), 100_543);
    srv.load(db);
    run_batch_insert(&srv, db, 10, |_| 1);
    assert_eq!(triple_num(&srv, db), 100_553);
    assert_inserted(&srv, db, 10);
    srv.unload(db);
    // Saved: still there after loading again.
    srv.load(db);
    assert_eq!(triple_num(&srv, db), 100_553);
    assert_inserted(&srv, db, 10);
    srv.unload(db);
    srv.drop_db(db);
}

/// `update_test.sh -d data -n N`: build a new database from `data` (here the
/// script's own template `data/update_test.nt`, the triple `<s0> <p0> <o0>`),
/// insert N triples. The first statement re-inserts the template's triple.
#[test]
fn new_database_mode_from_the_template() {
    let srv = Server::start("update_template");
    let db = "update_build";
    let template = data("update_test.nt");
    assert_eq!(read(&template).trim(), "<s0> <p0> <o0>.");
    let j = srv.build(db, &template);
    ok(&j);
    assert_eq!(int(&j, "success_num"), 1);
    srv.load(db);
    run_batch_insert(&srv, db, 25, |idx| i64::from(idx != 0));
    assert_eq!(triple_num(&srv, db), 25);
    assert_inserted(&srv, db, 25);
    srv.unload(db);
    srv.drop_db(db);
}

/// `update_test.sh -b db -n N`: insert N triples into an existing database
/// (the one the previous mode would leave), which is not dropped.
#[test]
fn existing_database_mode() {
    let srv = Server::start("update_existing");
    let db = "update_existing";
    ok(&srv.build(db, Path::new(&data("small/small.nt"))));
    srv.load(db);
    run_batch_insert(&srv, db, 12, |_| 1);
    srv.unload(db);
    // The script's -b mode loads the existing database again and continues;
    // indexes 0..12 are already there.
    srv.load(db);
    run_batch_insert(&srv, db, 15, |idx| i64::from(idx >= 12));
    assert_eq!(triple_num(&srv, db), 25 + 15);
    assert_inserted(&srv, db, 15);
    srv.unload(db);
}
