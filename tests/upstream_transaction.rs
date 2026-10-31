//! Upstream conformance: `scripts/test/transaction_test.sh` — six transactions
//! with 31 transactional queries (`tquery`) on an empty database, over
//! `/api` exactly as the script sends them.
//!
//! The script prints the answers without checking them; the expectations here
//! follow from the statements (and upstream's `ApiHandler::tquery`: an update
//! answers `"Transaction query success"` with `AnsNum` = the triples it
//! changed, a read its result table with `"success"`). Plain `"0"` and
//! `"0"^^xsd:integer` are different RDF terms, so the script's `delete data
//! { <V1> <R1> "0"^^xsd:integer }` deletes nothing.
//!
//! Also here: upstream numbers transaction ids per loaded database (each
//! database gets its own `Txn_manager`, whose counter starts at 1, when it is
//! loaded), which `api_test.sh` relies on (`"tid":"1"` after loading).

#[path = "upstream_scripts/harness.rs"]
mod harness;

use gstore::json::Json;
use harness::*;

const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";

struct Txn<'a> {
    srv: &'a Server,
    db: &'a str,
    tid: String,
    step: usize,
}

impl<'a> Txn<'a> {
    /// `begin` with the script's `isolevel` 1; the TID must be `want_tid`.
    fn begin(srv: &'a Server, db: &'a str, want_tid: &str) -> Txn<'a> {
        let j = srv.op("begin", &[("db_name", db), ("isolevel", "1")]);
        ok(&j);
        assert_eq!(string(&j, "StatusMsg"), "Transaction begin success");
        let tid = string(&j, "TID");
        assert_eq!(tid, want_tid, "transaction ids are sequential per database");
        Txn {
            srv,
            db,
            tid,
            step: 0,
        }
    }

    fn tquery(&mut self, sparql: &str) -> Json {
        self.step += 1;
        let j = self.srv.op(
            "tquery",
            &[("db_name", self.db), ("tid", &self.tid), ("sparql", sparql)],
        );
        assert_eq!(
            code(&j),
            0,
            "tid {} step {}: {sparql}: {j}",
            self.tid,
            self.step
        );
        j
    }

    /// An update statement that changes `changed` triples.
    fn update(&mut self, sparql: &str, changed: i64) {
        let j = self.tquery(sparql);
        assert_eq!(string(&j, "StatusMsg"), "Transaction query success", "{j}");
        assert_eq!(int(&j, "AnsNum"), changed, "{sparql}: triples changed");
    }

    /// A read whose answer rows (cells in gStore's text form, in `vars`
    /// order) must be `rows`, order-insensitively.
    fn select(&mut self, sparql: &str, vars: &[&str], rows: &[&[&str]]) {
        let j = self.tquery(sparql);
        assert_eq!(string(&j, "StatusMsg"), "success", "{j}");
        let vars: Vec<String> = vars.iter().map(|v| v.to_string()).collect();
        let got = answer_table(&j);
        assert_eq!(got.vars, vars, "{sparql}: variables");
        let mut want: Vec<Vec<Cell>> = rows
            .iter()
            .map(|r| r.iter().map(|c| Some(c.to_string())).collect())
            .collect();
        want.sort();
        assert_eq!(got.normalized(&vars), want, "{sparql}: rows");
        assert_eq!(int(&j, "AnsNum"), rows.len() as i64, "{sparql}: AnsNum");
    }

    fn commit(self) {
        let j = self
            .srv
            .op("commit", &[("db_name", self.db), ("tid", &self.tid)]);
        ok(&j);
        assert_eq!(string(&j, "StatusMsg"), "Transaction commit success.");
    }

    fn rollback(self) {
        let j = self
            .srv
            .op("rollback", &[("db_name", self.db), ("tid", &self.tid)]);
        ok(&j);
        assert_eq!(string(&j, "StatusMsg"), "Transaction rollback success.");
    }
}

fn int_lit(v: &str) -> String {
    format!("\"{v}\"^^<{XSD_INTEGER}>")
}

#[test]
fn transaction_test_script() {
    let srv = Server::start("txn");
    let db = "txn_test";
    // [1/8] build an empty database (db_path ""), [2/8] load it.
    let j = srv.op("build", &[("db_name", db), ("db_path", "")]);
    ok(&j);
    assert_eq!(int(&j, "success_num"), 0);
    srv.load(db);

    let sel_mingzhi = "select ?x where { ?x <mingzhi> ?y}";

    // Transaction #1 (commit): 7 statements.
    let mut t = Txn::begin(&srv, db, "1");
    t.update("delete where {?x <mingzhi> ?y}", 0);
    t.select(sel_mingzhi, &["x"], &[]);
    t.update("insert data { <me> <mingzhi> <zhangzhe>}", 1);
    t.update("insert data { <you> <mingzhi> <SBBBBB>}", 1);
    t.select(sel_mingzhi, &["x"], &[&["<me>"], &["<you>"]]);
    t.update("delete data { <you> <mingzhi> <SBBBBB>}", 1);
    t.select(sel_mingzhi, &["x"], &[&["<me>"]]);
    assert_eq!(t.step, 7);
    t.commit();

    // Transaction #2 (rollback): 5 statements.
    let mut t = Txn::begin(&srv, db, "2");
    t.select(sel_mingzhi, &["x"], &[&["<me>"]]);
    t.update("insert data { <me> <mingzhi> <zhangzhe>}", 0);
    t.select(sel_mingzhi, &["x"], &[&["<me>"]]);
    t.update("delete where {?x <mingzhi> ?y}", 1);
    t.select(sel_mingzhi, &["x"], &[]);
    assert_eq!(t.step, 5);
    t.rollback();

    // Transaction #3 (delete everything, commit): 1 statement. The rollback
    // left <me> <mingzhi> <zhangzhe>, the database's only triple.
    let mut t = Txn::begin(&srv, db, "3");
    t.update("delete where {?x ?y ?z}", 1);
    assert_eq!(t.step, 1);
    t.commit();

    // Transaction #4 (the gStore node): 5 statements.
    let sel_gstore = "select ?x ?y where{<gStore> ?x ?y}";
    let mut t = Txn::begin(&srv, db, "4");
    t.select(sel_gstore, &["x", "y"], &[]);
    t.update(
        "insert data {<gStore> <test> <commit>.<gStore> <test> <abort>.<gStore> <test> <begin>.<gStore> <test> <all>.}",
        4,
    );
    t.select(
        sel_gstore,
        &["x", "y"],
        &[
            &["<test>", "<commit>"],
            &["<test>", "<abort>"],
            &["<test>", "<begin>"],
            &["<test>", "<all>"],
        ],
    );
    t.update("delete where {<gStore> ?x ?y}", 4);
    t.select(sel_gstore, &["x", "y"], &[]);
    assert_eq!(t.step, 5);
    t.commit();

    // Transaction #5 (incremental inserts): 6 statements.
    let sel_r1 = "select ?x ?y where{?x <R1> ?y}";
    let mut t = Txn::begin(&srv, db, "5");
    t.update("insert data {<V1> <R1> \"0\".}", 1);
    t.select(sel_r1, &["x", "y"], &[&["<V1>", "\"0\""]]);
    t.update("insert data {<V1> <R1> \"10\".}", 1);
    t.select(
        sel_r1,
        &["x", "y"],
        &[&["<V1>", "\"0\""], &["<V1>", "\"10\""]],
    );
    t.update("insert data {<V2> <R1> \"20\".}", 1);
    t.select(
        sel_r1,
        &["x", "y"],
        &[&["<V1>", "\"0\""], &["<V1>", "\"10\""], &["<V2>", "\"20\""]],
    );
    assert_eq!(t.step, 6);
    t.commit();

    // Transaction #6 (update by delete + insert of typed values): 7 statements.
    let sel_v = "select ?v {<V1> <R1> ?v.}";
    let one = int_lit("1");
    let two = int_lit("2");
    let mut t = Txn::begin(&srv, db, "6");
    t.select(sel_v, &["v"], &[&["\"0\""], &["\"10\""]]);
    t.update(
        &format!("delete data {{ <V1> <R1> \"0\"^^<{XSD_INTEGER}> }}"),
        0,
    );
    t.update(
        &format!("insert data {{ <V1> <R1> \"1\"^^<{XSD_INTEGER}> }}"),
        1,
    );
    t.select(sel_v, &["v"], &[&["\"0\""], &["\"10\""], &[&one]]);
    t.update(
        &format!("delete data {{ <V1> <R1> \"1\"^^<{XSD_INTEGER}> }}"),
        1,
    );
    t.update(
        &format!("insert data {{ <V1> <R1> \"2\"^^<{XSD_INTEGER}> }}"),
        1,
    );
    t.select(sel_v, &["v"], &[&["\"0\""], &["\"10\""], &[&two]]);
    assert_eq!(t.step, 7);
    t.commit();

    // The committed state, outside any transaction, and after a reload (the
    // commits were saved).
    let all = "select ?s ?p ?o where { ?s ?p ?o }";
    let want: Vec<Vec<Cell>> = {
        let mut w: Vec<Vec<Cell>> = [
            ["<V1>", "<R1>", "\"0\""],
            ["<V1>", "<R1>", "\"10\""],
            ["<V2>", "<R1>", "\"20\""],
            ["<V1>", "<R1>", two.as_str()],
        ]
        .iter()
        .map(|r| r.iter().map(|c| Some(c.to_string())).collect())
        .collect();
        w.sort();
        w
    };
    let vars: Vec<String> = ["s", "p", "o"].iter().map(|v| v.to_string()).collect();
    let j = srv.query(db, all);
    ok(&j);
    assert_eq!(answer_table(&j).normalized(&vars), want);
    // [7/8] unload, then load again: the data is on disk.
    srv.unload(db);
    srv.load(db);
    let j = srv.query(db, all);
    ok(&j);
    assert_eq!(answer_table(&j).normalized(&vars), want);
    // A fresh load starts a fresh transaction manager: ids restart at 1.
    Txn::begin(&srv, db, "1").rollback();
    srv.unload(db);
    // [8/8] drop
    srv.drop_db(db);
}

#[test]
fn transaction_ids_are_numbered_per_loaded_database() {
    let srv = Server::start("txnids");
    for db in ["alpha", "beta"] {
        ok(&srv.op("build", &[("db_name", db), ("db_path", "")]));
        srv.load(db);
    }
    // Each database counts from 1, whatever the other one did.
    Txn::begin(&srv, "alpha", "1").commit();
    let mut a = Txn::begin(&srv, "alpha", "2");
    let mut b = Txn::begin(&srv, "beta", "1");
    a.update("insert data { <a> <p> <o> }", 1);
    b.update("insert data { <b> <p> <o> }", 1);
    // tid 2 is alpha's only: beta has no transaction 2.
    let j = srv.op(
        "tquery",
        &[("db_name", "beta"), ("tid", "2"), ("sparql", "ASK {}")],
    );
    assert_ne!(code(&j), 0, "{j}");
    a.commit();
    // (beta's transaction 1 stays open; it is ended over REST below.)
    // Ended ids answer "already committed" on their database only.
    let j = srv.op("commit", &[("db_name", "alpha"), ("tid", "2")]);
    assert_ne!(code(&j), 0, "{j}");
    assert!(string(&j, "StatusMsg").contains("already committed"), "{j}");

    // Unloading and loading again restarts the numbering (upstream creates a
    // new Txn_manager on load) — and forgets how the old ids ended.
    srv.unload("alpha");
    srv.load("alpha");
    let mut a = Txn::begin(&srv, "alpha", "1");
    a.update("insert data { <a2> <p> <o> }", 1);
    // One writer per database: a second begin is refused.
    let j = srv.op("begin", &[("db_name", "alpha"), ("isolevel", "1")]);
    assert_eq!(code(&j), 1008, "{j}");

    // The REST endpoints take the database to tell the two open
    // transactions 1 apart; without it the id is ambiguous.
    let (status, body) = srv.rest("POST", "/commit?tid=1", "");
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("several databases"), "{body}");
    let (status, body) = srv.rest("POST", "/commit?tid=1&db_name=alpha", "");
    assert_eq!(status, 200, "{body}");
    // Only beta's transaction 1 is open now: the bare id resolves to it.
    let (status, body) = srv.rest("POST", "/rollback?tid=1", "");
    assert_eq!(status, 200, "{body}");
    let j = srv.query("alpha", "select ?s where { ?s <p> <o> }");
    assert_eq!(int(&j, "AnsNum"), 2, "alpha's two commits: {j}");
    let j = srv.query("beta", "select ?s where { ?s <p> <o> }");
    assert_eq!(
        int(&j, "AnsNum"),
        0,
        "beta's transaction was rolled back: {j}"
    );
}
