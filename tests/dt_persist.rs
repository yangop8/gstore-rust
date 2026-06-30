//! Data test: persistence/ops features — backup & restore, the update log
//! (record + replay), and the bounded-memory / parallel N-Triples loaders.

use gstore::Database;
use std::fs;
use std::path::PathBuf;

const TTL: &str = "\
<http://ex/root> <http://ex/name> \"Bookug Lobert\" .
<http://ex/root> <http://ex/contain> <http://ex/node0> .
<http://ex/root> <http://ex/contain> <http://ex/node1> .
<http://ex/node1> <http://ex/own> <http://ex/point0> .
<http://ex/node1> <http://ex/own> <http://ex/point1> .
";

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_persist_{tag}"));
    let _ = fs::remove_dir_all(&d);
    d
}

#[test]
fn backup_and_restore_roundtrip() {
    let src = tmp("bk_src");
    let bak = tmp("bk_bak");
    let db = Database::build_from_str("bk", TTL).unwrap();
    db.save(&src).unwrap();

    // Snapshot backup of the live database, then restore it independently.
    let reloaded = Database::load(&src).unwrap();
    reloaded.backup(&bak).unwrap();
    let mut restored = Database::restore(&bak).unwrap();

    assert_eq!(restored.triple_num(), 5);
    let rs = restored
        .select("SELECT ?o WHERE { <http://ex/node1> <http://ex/own> ?o }")
        .unwrap();
    assert_eq!(rs.row_count(), 2);

    fs::remove_dir_all(&src).ok();
    fs::remove_dir_all(&bak).ok();
}

#[test]
fn backup_dir_copies_files() {
    let src = tmp("bd_src");
    let dst = tmp("bd_dst");
    let db = Database::build_from_str("bd", TTL).unwrap();
    db.save(&src).unwrap();

    Database::backup_dir(&src, &dst).unwrap();
    // The directory copy is loadable and identical.
    let loaded = Database::load(&dst).unwrap();
    assert_eq!(loaded.triple_num(), db.triple_num());
    assert!(dst.join("dict.bin").is_file());
    assert!(dst.join("store.bin").is_file());

    fs::remove_dir_all(&src).ok();
    fs::remove_dir_all(&dst).ok();
}

#[test]
fn update_log_records_and_replays() {
    let dir = tmp("ulog");
    fs::create_dir_all(&dir).unwrap();
    let log = dir.join("update.log");

    // Apply a sequence of updates with logging enabled.
    let mut db = Database::build_from_str("ul", TTL).unwrap();
    db.enable_update_log(&log);
    db.query("INSERT DATA { <http://ex/root> <http://ex/contain> <http://ex/nodeX> }")
        .unwrap();
    db.query("INSERT DATA { <http://ex/nodeX> <http://ex/own> <http://ex/pointZ> }")
        .unwrap();
    db.query("DELETE DATA { <http://ex/root> <http://ex/contain> <http://ex/node0> }")
        .unwrap();
    let after_live = db.triple_num();
    assert!(log.is_file(), "update log file should exist");

    // Replay the log onto a *fresh* copy of the base data — it must converge to
    // the same state as the live database.
    let mut replayed = Database::build_from_str("ul2", TTL).unwrap();
    let n = replayed.replay_update_log(&log).unwrap();
    assert_eq!(n, 3, "three update statements were logged");
    assert_eq!(replayed.triple_num(), after_live);

    // nodeX/pointZ present, node0 gone — exactly the replayed effect.
    let rs = replayed
        .select("SELECT ?o WHERE { <http://ex/nodeX> <http://ex/own> ?o }")
        .unwrap();
    assert_eq!(rs.rows[0][0], Some("<http://ex/pointZ>".into()));
    let rs2 = replayed
        .select("SELECT ?o WHERE { <http://ex/root> <http://ex/contain> ?o }")
        .unwrap();
    let mut got: Vec<String> = rs2.rows.iter().map(|r| r[0].clone().unwrap()).collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            "<http://ex/node1>".to_string(),
            "<http://ex/nodeX>".to_string()
        ]
    );

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn update_log_body_with_delimiter_text_is_safe() {
    // A statement whose literal contains the record-header text must round-trip
    // through the length-prefixed log without corrupting parsing.
    let dir = tmp("ulog_delim");
    fs::create_dir_all(&dir).unwrap();
    let log = dir.join("update.log");

    let mut db = Database::new("d");
    db.enable_update_log(&log);
    db.query("INSERT DATA { <http://ex/s> <http://ex/p> \"REC 0 0 5\\nnot a header\" }")
        .unwrap();

    let mut replayed = Database::new("d2");
    let n = replayed.replay_update_log(&log).unwrap();
    assert_eq!(n, 1);
    assert_eq!(replayed.triple_num(), 1);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn batched_load_matches_whole_load() {
    let dir = tmp("batched");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("data.nt");
    fs::write(&file, TTL).unwrap();

    let whole = Database::build_from_files("w", &[&file]).unwrap();
    // A tiny batch forces multiple flushes.
    let batched = Database::build_from_ntriples_batched("b", &file, 2).unwrap();

    assert_eq!(batched.triple_num(), whole.triple_num());
    assert_eq!(batched.entity_num(), whole.entity_num());
    assert_eq!(batched.literal_num(), whole.literal_num());
    assert_eq!(batched.predicate_num(), whole.predicate_num());

    let mut batched = batched;
    let rs = batched
        .select("SELECT ?o WHERE { <http://ex/root> <http://ex/contain> ?o }")
        .unwrap();
    assert_eq!(rs.row_count(), 2);

    fs::remove_dir_all(&dir).ok();
}

#[test]
fn parallel_load_matches_whole_load() {
    let dir = tmp("parallel");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("data.nt");
    fs::write(&file, TTL).unwrap();

    let whole = Database::build_from_files("w", &[&file]).unwrap();
    let par = Database::build_from_ntriples_parallel("p", &file, 4).unwrap();

    assert_eq!(par.triple_num(), whole.triple_num());
    assert_eq!(par.entity_num(), whole.entity_num());
    assert_eq!(par.predicate_num(), whole.predicate_num());

    let mut par = par;
    let rs = par
        .select("SELECT ?n WHERE { <http://ex/root> <http://ex/name> ?n }")
        .unwrap();
    assert_eq!(rs.rows[0][0], Some("\"Bookug Lobert\"".into()));

    fs::remove_dir_all(&dir).ok();
}
