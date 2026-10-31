//! Upstream conformance: `scripts/test/api_test.sh` — its 28 operations in
//! the script's order, with the script's exact request bodies (the
//! `API_DATA` array, sent as `curl -d` sends them), on the `small` dataset
//! (`api_test.sh small`; the default `lubm` run is
//! `dt_gapi_db::the_upstream_api_test_script_runs_unmodified` and
//! `scripts/conformance.sh`).
//!
//! Every answer is `StatusCode` 0 except the opening `drop` of a database
//! that does not exist yet. The script addresses its transactions as
//! `"tid":"1"` and `"tid":"2"`: upstream numbers transactions per loaded
//! database, so that holds after other databases had transactions — this
//! test runs one first.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use gstore::json::{self, Json};
use harness::*;

/// Field `n` (1-based) of `text` split on `"` — how the script extracts
/// `filepath` / `backupfilepath` (`awk -F '"' '{print $10}'`).
fn awk_field(text: &str, n: usize) -> String {
    text.split('"').nth(n - 1).unwrap_or_default().to_string()
}

#[test]
fn api_test_script_on_small() {
    let srv = Server::start("api_small");
    // Another database's transactions first: the script's tid 1 and 2 are
    // still right for its own database.
    ok(&srv.op("build", &[("db_name", "other"), ("db_path", "")]));
    srv.load("other");
    for _ in 0..3 {
        let j = srv.op("begin", &[("db_name", "other"), ("isolevel", "1")]);
        ok(&j);
        let tid = string(&j, "TID");
        ok(&srv.op("commit", &[("db_name", "other"), ("tid", &tid)]));
    }

    let db = "small";
    let rdf = path_str(&data("small/small.nt"));
    let sparql1 = "select ?s ?p ?o where {?s ?p ?o.} limit 10";
    let sparql2 = "insert data {<s> <p> <o>.}";
    let sparql3 = "delete data {<s> <p> <o>.}";
    let cred = r#""username":"root","password":"123456""#;
    // One request of the script: (raw answer, parsed answer).
    let call = |body: String| -> (String, Json) {
        let (status, text) = http(&srv.addr, "POST", "/api", body.as_bytes());
        assert_eq!(status, 200, "{body}: HTTP {status}");
        let j = json::parse(&text).unwrap_or_else(|e| panic!("{body}: not JSON ({e}): {text}"));
        (text, j)
    };
    let op = |rest: &str| call(format!(r#"{{"operation":{rest},{cred}}}"#)).1;

    // check, login (no db yet)
    ok(&call(r#"{"operation":"check"}"#.to_string()).1);
    ok(&op(r#""login""#));
    // drop2: the database does not exist yet.
    let j = op(&format!(r#""drop","db_name":"{db}","is_backup":"false""#));
    assert_eq!(code(&j), 1005, "{j}");
    // build, show, monitor, load, query
    let j = op(&format!(r#""build","db_name":"{db}","db_path":"{rdf}""#));
    ok(&j);
    assert_eq!(int(&j, "success_num"), 25);
    ok(&op(r#""show""#));
    let j = op(&format!(r#""monitor","db_name":"{db}""#));
    ok(&j);
    assert_eq!(int(&j, "tripleNum"), 25);
    ok(&op(&format!(r#""load","db_name":"{db}""#)));
    let j = op(&format!(r#""query","db_name":"{db}","sparql":"{sparql1}""#));
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 10);
    // exportDb, then download what awk extracted.
    let (text, j) = call(format!(
        r#"{{"operation":"export",{cred},"db_name":"{db}"}}"#
    ));
    ok(&j);
    let export_path = awk_field(&text, 10);
    assert_eq!(
        export_path,
        string(&j, "filepath"),
        "awk's field 10 is filepath: {text}"
    );
    let (status, body) = http(
        &srv.addr,
        "GET",
        &format!("/file/download/{export_path}"),
        b"",
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.lines().count(), 25);
    // backup (the restore uses the path awk extracts)
    let (text, j) = call(format!(
        r#"{{"operation":"backup",{cred},"db_name":"{db}"}}"#
    ));
    ok(&j);
    let backup_path = awk_field(&text, 10);
    assert_eq!(backup_path, string(&j, "backupfilepath"), "{text}");
    // batchRemove, batchInsert, unload, restore + reload
    let j = op(&format!(r#""batchRemove","db_name":"{db}","file":"{rdf}""#));
    ok(&j);
    assert_eq!(int(&j, "success_num"), 25);
    let j = op(&format!(r#""batchInsert","db_name":"{db}","file":"{rdf}""#));
    ok(&j);
    assert_eq!(int(&j, "success_num"), 25);
    ok(&op(&format!(r#""unload","db_name":"{db}""#)));
    ok(&op(&format!(
        r#""restore","db_name":"{db}","backup_path":"{backup_path}""#
    )));
    ok(&op(&format!(r#""load","db_name":"{db}""#)));
    // begin, execute, commit: transaction 1 of the reloaded database.
    let j = op(&format!(r#""begin","db_name":"{db}","isolevel":"1""#));
    ok(&j);
    assert_eq!(string(&j, "TID"), "1");
    let j = op(&format!(
        r#""tquery","db_name":"{db}","tid":"1","sparql":"{sparql2}""#
    ));
    ok(&j);
    assert_eq!(int(&j, "AnsNum"), 1);
    ok(&op(&format!(r#""commit","db_name":"{db}","tid":"1""#)));
    // rollback: begin (transaction 2), tquery tid 2, rollback tid 2.
    let j = op(&format!(r#""begin","db_name":"{db}","isolevel":"1""#));
    ok(&j);
    assert_eq!(string(&j, "TID"), "2");
    ok(&op(&format!(
        r#""tquery","db_name":"{db}","tid":"2","sparql":"{sparql3}""#
    )));
    ok(&op(&format!(r#""rollback","db_name":"{db}","tid":"2""#)));
    let j = srv.query(db, "ask { <s> <p> <o> }");
    assert!(
        j.to_string().contains("true"),
        "committed, not rolled back: {j}"
    );
    // checkpoint
    ok(&op(&format!(r#""checkpoint","db_name":"{db}""#)));
    // users: add, privileges, show, clear, password, delete
    ok(&op(
        r#""usermanage","type":"1","op_username":"test","op_password":"123456""#,
    ));
    ok(&op(&format!(
        r#""userprivilegemanage","type":"1","op_username":"test","privileges":"1,2,3","db_name":"{db}""#
    )));
    let j = op(r#""showuser""#);
    ok(&j);
    assert!(j.to_string().contains(r#""username":"test""#), "{j}");
    ok(&op(
        r#""userprivilegemanage","type":"3","op_username":"test""#,
    ));
    let j = call(
        r#"{"operation":"userpassword","username":"test","password":"123456","op_password":"111111"}"#
            .to_string(),
    )
    .1;
    ok(&j);
    ok(&op(r#""usermanage","type":"2","op_username":"test""#));
    // rename: unload first, then rename; drop the renamed database.
    ok(&op(&format!(r#""unload","db_name":"{db}""#)));
    ok(&op(&format!(
        r#""rename","db_name":"{db}","new_name":"new{db}""#
    )));
    ok(&op(&format!(
        r#""drop","db_name":"new{db}","is_backup":"false""#
    )));
    let j = op(r#""show""#);
    assert!(!j.to_string().contains("newsmall"), "{j}");
}
