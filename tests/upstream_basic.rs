//! Upstream conformance: `scripts/test/basic_test.sh` — build, load, batch
//! remove, batch insert, query and unload the `bbug`, `lubm`, `num` and
//! `small` datasets over `/api`, with the script's expected counts ("最新预期值
//! （全绿版）"): the triples built (`triple_num`), the `success_num` of
//! `batchRemove` / `batchInsert` of the same file (equal to the build's), each
//! query's `AnsNum` (`*_ans`, the queries run in the script's order — some are
//! updates, so later answers depend on earlier ones) and the 6 triples of
//! `small_add.nt`.
//!
//! **`num`.** The script records `num_ans=(0 0 0 1)`, but its
//! `extract_json_num` turns a missing `AnsNum` into 0 and never fails a query
//! step (every query line prints ✅): 0 is what upstream's `/api` answered,
//! not the queries' answer. Over `num.nt` (four people who know each other)
//! `num0` (who-knows-whom with names and salaries) has 8 answers, `num1`
//! (`?sx < ?sy && abs(?sx - ?sy) < 3000`) 1 (Alice → Bob: 2500 < 5000) and
//! `num2` (`?sx > ?sy && (?hx > ?hy || ?hx >= 170.0)`) 4 (Bob → Alice,
//! Bob → Carol, David → Alice, David → Carol); this port asserts those.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use harness::*;

struct Dataset {
    name: &'static str,
    /// `triple_num` of the script.
    triples: i64,
    /// The query files, in the script's order, with their expected `AnsNum`.
    queries: &'static [(&'static str, i64)],
}

fn run(ds: &Dataset) {
    let srv = Server::start(&format!("basic_{}", ds.name));
    let db = format!("{}_basic", ds.name);
    let nt = data(&format!("{0}/{0}.nt", ds.name));
    let nt_str = path_str(&nt);

    // [1/7] build
    let j = srv.build(&db, &nt);
    ok(&j);
    assert_eq!(int(&j, "success_num"), ds.triples, "{}: build", ds.name);
    // [2/7] load
    srv.load(&db);
    // [3/7] batchRemove of the whole file removes every triple...
    let j = srv.op("batchRemove", &[("db_name", &db), ("file", &nt_str)]);
    ok(&j);
    assert_eq!(int(&j, "success_num"), ds.triples, "{}: batchRemove", ds.name);
    let j = srv.op("monitor", &[("db_name", &db)]);
    assert_eq!(int(&j, "tripleNum"), 0, "{}: empty after batchRemove", ds.name);
    // [4/7] ...and batchInsert puts them back.
    let j = srv.op("batchInsert", &[("db_name", &db), ("file", &nt_str)]);
    ok(&j);
    assert_eq!(int(&j, "success_num"), ds.triples, "{}: batchInsert", ds.name);
    // [5/7] the queries, in order
    for (file, want) in ds.queries {
        let sparql = read(&data(&format!("{}/{file}", ds.name)));
        let j = srv.query(&db, &sparql);
        assert_eq!(code(&j), 0, "{}/{file}: {j}", ds.name);
        assert_eq!(int(&j, "AnsNum"), *want, "{}/{file}: AnsNum", ds.name);
    }
    // [6/7] small: insert small_add.nt
    if ds.name == "small" {
        let add = path_str(&data("small/small_add.nt"));
        let j = srv.op("batchInsert", &[("db_name", &db), ("file", &add)]);
        ok(&j);
        assert_eq!(int(&j, "success_num"), 6, "small_add.nt");
    }
    // [7/7] unload, then drop (`is_backup` false)
    srv.unload(&db);
    srv.drop_db(&db);
}

#[test]
fn bbug() {
    run(&Dataset {
        name: "bbug",
        triples: 1988,
        queries: &[
            ("bbug0d.sql", 0), // DELETE DATA of a triple not (yet) there
            ("bbug0.sql", 2),  // INSERT DATA of 2 triples
            ("bbug1.sql", 298),
            ("bbug2.sql", 10), // DELETE WHERE
            ("bbug3.sql", 2),
            ("bbug4.sql", 30),
            ("bbug5.sql", 12),
            ("bbug6.sql", 294), // DELETE WHERE
        ],
    });
}

#[test]
fn lubm() {
    run(&Dataset {
        name: "lubm",
        triples: 100_543,
        queries: &[
            ("lubm_q1.rq", 4),
            ("lubm_q2.rq", 0),
            ("lubm_q3.rq", 6),
            ("lubm_q4.rq", 10),
            ("lubm_q5.rq", 678),
            ("lubm_q6.rq", 7790),
            ("lubm_q7.rq", 67),
            ("lubm_q8.rq", 7790),
            ("lubm_q9.rq", 102),
            ("lubm_q10.rq", 4),
            ("lubm_q11.rq", 224),
            ("lubm_q12.rq", 15),
            ("lubm_q13.rq", 8330),
            ("lubm_q14.rq", 5916),
        ],
    });
}

#[test]
fn num() {
    run(&Dataset {
        name: "num",
        triples: 29,
        // basic_test.sh: num_ans=(0 0 0 1) — see the module docs.
        queries: &[
            ("num0.sql", 8),
            ("num1.sql", 1),
            ("num2.sql", 4),
            ("num3.sql", 1),
        ],
    });
}

#[test]
fn small() {
    run(&Dataset {
        name: "small",
        triples: 25,
        queries: &[
            ("small_dist.sql", 2),
            ("small_p0.sql", 2),
            ("small_p1.sql", 1),
            ("small_p2.sql", 27),
            ("small_p3.sql", 1),
            ("small_q0.sql", 1),
            ("small_q1.sql", 1),
            ("small_q2.sql", 4),
            ("small_q3.sql", 1),
            ("small_s0.sql", 5),
            ("small_s1.sql", 5),
        ],
    });
}

/// The LUBM update queries upstream ships next to the benchmark
/// (`data/lubm/lubm_insert.rq`, `lubm_delete.rq`): the insert adds
/// AssociateProfessor100's 12 triples, the delete removes them again.
#[test]
fn lubm_insert_and_delete_queries() {
    let srv = Server::start("basic_lubm_update");
    let db = "lubm_update";
    ok(&srv.build(db, &data("lubm/lubm.nt")));
    srv.load(db);
    let who = "select ?p ?o where { <http://www.Department0.University0.edu/AssociateProfessor100> ?p ?o }";
    assert_eq!(int(&srv.query(db, who), "AnsNum"), 0);
    let j = srv.query(db, &read(&data("lubm/lubm_insert.rq")));
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 12, "lubm_insert.rq: {j}");
    assert_eq!(int(&srv.query(db, who), "AnsNum"), 12);
    let j = srv.op("monitor", &[("db_name", db)]);
    assert_eq!(int(&j, "tripleNum"), 100_543 + 12);
    let j = srv.query(db, &read(&data("lubm/lubm_delete.rq")));
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 12, "lubm_delete.rq: {j}");
    assert_eq!(int(&srv.query(db, who), "AnsNum"), 0);
    srv.unload(db);
    srv.drop_db(db);
}
