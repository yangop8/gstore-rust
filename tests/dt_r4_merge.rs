//! Phase 6 Round-4 integration: the wiring between R4-RECLAIM (lowest-first
//! id reuse), R4-SNAP (copy-on-write `TripleStore`, batched writes, cheap
//! snapshots) and the build layouts.
//!
//! * A memory database writes its default-graph statements in batches
//!   (`TripleStore::insert_many` / `remove_many`) while ids are reused; a
//!   Stream-mode disk database, which writes one triple at a time, is the
//!   oracle: same contents, term counts and answers after every step,
//!   including failed `LOAD`s and rolled-back transactions.
//! * `ConcurrentDb` snapshots keep resolving their ids to the terms they
//!   were taken with while the live database frees and reuses them.
//! * A disk database built through the server keeps a free-id list, and a
//!   transaction's `/begin` snapshot of it (a `DiskReadView`, an unpinned
//!   dictionary) defers none of the commit's frees.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::db::OpenOptions;
use gstore::http_api::ApiServer;
use gstore::parser::import::{ImportOptions, Source};
use gstore::{ConcurrentDb, Database, QueryResult, Term};

/// A temp directory removed on drop (also when a test panics).
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "gstore_dt_r4_merge_{tag}_{}_{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TempDir(std::fs::canonicalize(&d).unwrap())
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rows(db: &mut Database, q: &str) -> Vec<String> {
    match db.query(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        QueryResult::Select(rs) => sorted_rows(&rs),
        other => panic!("{other:?}"),
    }
}

fn sorted_rows(rs: &gstore::query::ResultSet) -> Vec<String> {
    let mut v: Vec<String> = rs
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|c| c.clone().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect();
    v.sort();
    v
}

/// Queries whose constants make the in-memory VS-tree filter candidates
/// (an unsound batch fold would lose answers), plus the whole contents.
const QUERIES: &[&str] = &[
    "SELECT ?s ?p ?o WHERE { ?s ?p ?o }",
    "SELECT ?g ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } }",
    "SELECT ?s WHERE { ?s <http://ex/type> <http://ex/C1> }",
    "SELECT ?s WHERE { ?s <http://ex/type> <http://ex/C9> }",
    "SELECT ?s ?o WHERE { ?s <http://ex/knows> ?o . ?o <http://ex/type> <http://ex/C2> }",
    "SELECT ?s WHERE { ?s <http://ex/name> ?n . ?s <http://ex/likes> <http://ex/s3> }",
    "SELECT ?s WHERE { <http://ex/s7> <http://ex/knows> ?s }",
    "SELECT ?s ?o WHERE { ?s <http://ex/new> ?o . ?o <http://ex/type> <http://ex/C4> }",
];

/// Both databases agree: contents, answers, term counts, integrity.
fn agree(mem: &mut Database, disk: &mut Database, step: &str) {
    for q in QUERIES {
        assert_eq!(rows(mem, q), rows(disk, q), "{step}: {q}");
    }
    assert_eq!(mem.triple_num(), disk.triple_num(), "{step}: triples");
    assert_eq!(mem.entity_num(), disk.entity_num(), "{step}: entities");
    assert_eq!(mem.literal_num(), disk.literal_num(), "{step}: literals");
    assert_eq!(mem.predicate_num(), disk.predicate_num(), "{step}: predicates");
    mem.verify_integrity().unwrap();
    disk.verify_integrity().unwrap();
}

fn base_data() -> String {
    let mut s = String::new();
    for i in 0..120 {
        s.push_str(&format!(
            "<http://ex/s{i}> <http://ex/type> <http://ex/C{}> .\n\
             <http://ex/s{i}> <http://ex/name> \"n{i}\" .\n\
             <http://ex/s{i}> <http://ex/knows> <http://ex/s{}> .\n",
            i % 5,
            (i + 1) % 120
        ));
    }
    s
}

/// Triples about subjects `from..to` with a fresh vocabulary (new terms,
/// which reuse freed ids), each subject also an object in the batch.
fn fresh_block(tag: &str, from: usize, to: usize) -> String {
    let mut s = String::new();
    for i in from..to {
        s.push_str(&format!(
            "<http://ex/{tag}{i}> <http://ex/type> <http://ex/C{}> .\n\
             <http://ex/{tag}{i}> <http://ex/name> \"{tag}-{i}\" .\n\
             <http://ex/{tag}{i}> <http://ex/likes> <http://ex/s3> .\n\
             <http://ex/{tag}{}> <http://ex/new> <http://ex/{tag}{i}> .\n",
            i % 7 + 2,
            i + 1
        ));
    }
    s
}

/// `DELETE DATA` of everything about subjects `s{from}..s{to}` (and the
/// `knows` edges pointing at them), leaving their terms unreferenced.
fn delete_block(from: usize, to: usize) -> String {
    let mut s = String::from("DELETE DATA {\n");
    for i in from..to {
        s.push_str(&format!(
            "<http://ex/s{i}> <http://ex/type> <http://ex/C{}> .\n\
             <http://ex/s{i}> <http://ex/name> \"n{i}\" .\n\
             <http://ex/s{i}> <http://ex/knows> <http://ex/s{}> .\n\
             <http://ex/s{}> <http://ex/knows> <http://ex/s{i}> .\n",
            i % 5,
            (i + 1) % 120,
            (i + 119) % 120
        ));
    }
    s.push('}');
    s
}

/// Apply `f` to both databases, then check that they agree.
fn both(
    mem: &mut Database,
    disk: &mut Database,
    step: &str,
    f: impl Fn(&mut Database) -> gstore::Result<u64>,
) {
    let a = f(mem);
    let b = f(disk);
    match (&a, &b) {
        (Ok(x), Ok(y)) => assert_eq!(x, y, "{step}: changed counts"),
        (Err(_), Err(_)) => {}
        _ => panic!("{step}: memory {a:?}, disk {b:?}"),
    }
    agree(mem, disk, step);
}

fn update(db: &mut Database, sparql: &str) -> gstore::Result<u64> {
    match db.query(sparql)? {
        QueryResult::Update { changed } => Ok(changed as u64),
        other => panic!("{other:?}"),
    }
}

#[test]
fn batched_memory_writes_answer_like_the_one_by_one_stream_path() {
    let tmp = TempDir::new("batched");
    let data = base_data();
    let src = tmp.path().join("base.nt");
    std::fs::write(&src, &data).unwrap();
    let mut mem = Database::build_from_str("mem", &data).unwrap();
    let disk_dir = tmp.path().join("disk");
    Database::build_disk(&disk_dir, &[&src]).unwrap();
    let mut disk = Database::open_with(&disk_dir, &OpenOptions::stream()).unwrap();
    assert!(mem.backend().as_memory().is_some());
    assert!(disk.backend().as_disk().is_some());
    assert!(mem.reclaims_ids() && disk.reclaims_ids());
    agree(&mut mem, &mut disk, "built");

    // INSERT DATA: new terms, a repeat inside the batch, a present triple,
    // named-graph quads in between.
    let mut ins = String::from("INSERT DATA {\n");
    ins.push_str(&fresh_block("a", 0, 30));
    ins.push_str("<http://ex/a0> <http://ex/type> <http://ex/C2> .\n");
    ins.push_str("<http://ex/s1> <http://ex/type> <http://ex/C1> .\n");
    ins.push_str("GRAPH <http://ex/g1> { <http://ex/a1> <http://ex/knows> <http://ex/s2> }\n");
    ins.push_str(&fresh_block("b", 0, 5));
    ins.push('}');
    both(&mut mem, &mut disk, "insert data", |db| update(db, &ins));

    // Free the terms of 40 subjects, then reuse their ids.
    let del = delete_block(0, 40);
    both(&mut mem, &mut disk, "delete data", |db| update(db, &del));
    let reuse = format!("INSERT DATA {{\n{}}}", fresh_block("c", 0, 40));
    both(&mut mem, &mut disk, "reusing insert", |db| update(db, &reuse));

    // DELETE/INSERT WHERE: deletes before inserts, both batched.
    let retype = "DELETE { ?s <http://ex/type> <http://ex/C1> } \
                  INSERT { ?s <http://ex/type> <http://ex/C9> . ?s <http://ex/new> ?s } \
                  WHERE { ?s <http://ex/type> <http://ex/C1> }";
    both(&mut mem, &mut disk, "modify", |db| update(db, retype));

    // gadd / gsub (insert_sources / remove_sources).
    let add = tmp.path().join("add.nt");
    std::fs::write(&add, fresh_block("d", 0, 25)).unwrap();
    both(&mut mem, &mut disk, "insert_sources", |db| {
        let mut applied = 0;
        db.insert_sources(
            &[Source::File(add.clone())],
            &ImportOptions::strict(),
            &mut (),
            &mut applied,
        )?;
        Ok(applied)
    });
    let sub = tmp.path().join("sub.nt");
    std::fs::write(&sub, fresh_block("d", 0, 12)).unwrap();
    both(&mut mem, &mut disk, "remove_sources", |db| {
        let mut applied = 0;
        db.remove_sources(
            &[Source::File(sub.clone())],
            &ImportOptions::strict().for_removal(),
            &mut (),
            &mut applied,
        )?;
        Ok(applied)
    });

    // LOAD, then a LOAD that fails part-way: it changes nothing (its
    // inserts are undone and the terms it minted freed).
    let load = tmp.path().join("load.nt");
    std::fs::write(&load, fresh_block("e", 0, 20)).unwrap();
    let load_q = format!("LOAD <file://{}>", load.display());
    both(&mut mem, &mut disk, "load", |db| update(db, &load_q));
    let bad = tmp.path().join("bad.nt");
    std::fs::write(
        &bad,
        format!("{}<http://ex/broken> <http://ex/p> .\n", fresh_block("f", 0, 30)),
    )
    .unwrap();
    let before = (mem.entity_num(), mem.literal_num(), mem.triple_num());
    let bad_q = format!("LOAD <file://{}>", bad.display());
    both(&mut mem, &mut disk, "failed load", |db| update(db, &bad_q));
    assert_eq!(
        (mem.entity_num(), mem.literal_num(), mem.triple_num()),
        before,
        "a failed LOAD changes nothing"
    );

    // A rolled-back transaction (inserts of new terms, deletes) is undone
    // run by run; a committed one frees at commit.
    let snapshot_before = rows(&mut mem, QUERIES[0]);
    for db in [&mut mem, &mut disk] {
        db.begin().unwrap();
        update(db, &format!("INSERT DATA {{\n{}}}", fresh_block("g", 0, 15))).unwrap();
        update(db, &delete_block(50, 70)).unwrap();
        update(db, &format!("INSERT DATA {{\n{}}}", fresh_block("h", 0, 10))).unwrap();
        db.rollback().unwrap();
    }
    agree(&mut mem, &mut disk, "rollback");
    assert_eq!(rows(&mut mem, QUERIES[0]), snapshot_before);
    for db in [&mut mem, &mut disk] {
        db.begin().unwrap();
        update(db, &delete_block(70, 90)).unwrap();
        update(db, &format!("INSERT DATA {{\n{}}}", fresh_block("i", 0, 20))).unwrap();
        db.commit().unwrap();
    }
    agree(&mut mem, &mut disk, "commit");

    // CLEAR then COPY back: the default graph's transfer inserts batch too.
    both(&mut mem, &mut disk, "copy", |db| {
        update(db, "COPY DEFAULT TO <http://ex/g2>")?;
        update(db, "CLEAR DEFAULT")?;
        update(db, "COPY <http://ex/g2> TO DEFAULT")
    });
}

#[test]
fn concurrent_snapshots_keep_their_terms_while_ids_are_reused() {
    let tmp = TempDir::new("cdb");
    let data = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                <http://ex/c> <http://ex/p> <http://ex/d> .\n";
    let src = tmp.path().join("d.nt");
    std::fs::write(&src, data).unwrap();
    let disk_dir = tmp.path().join("disk");
    Database::build_disk(&disk_dir, &[&src]).unwrap();
    let layouts = [
        ("memory", Database::build_from_str("mem", data).unwrap()),
        (
            "stream",
            Database::open_with(&disk_dir, &OpenOptions::stream()).unwrap(),
        ),
    ];
    for (layout, db) in layouts {
        let key = |iri: &str| Term::iri(iri).dict_key();
        let a_id = db.dict().entity_id(&key("http://ex/a")).unwrap();
        let cdb = ConcurrentDb::new(db);
        let snap = cdb.snapshot();
        cdb.update("DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }")
            .unwrap();
        cdb.update("INSERT DATA { <http://ex/x> <http://ex/q> <http://ex/y> }")
            .unwrap();
        let x_id = cdb.write(|db| db.dict().entity_id(&key("http://ex/x")).unwrap());
        if layout == "memory" {
            // The freed id was reused (lowest first) ...
            assert_eq!(x_id, a_id, "{layout}: the freed id is reused");
        } else {
            // ... while a snapshot sharing the on-disk dictionary pins it.
            assert_ne!(x_id, a_id, "{layout}: no reuse under a pin");
        }
        // ... and the snapshot still resolves it to the term it was taken with.
        let QueryResult::Select(rs) = snap
            .query("SELECT ?s ?o WHERE { ?s <http://ex/p> ?o }")
            .unwrap()
        else {
            panic!("select")
        };
        assert_eq!(
            sorted_rows(&rs),
            vec![
                "<http://ex/a>\t<http://ex/b>".to_string(),
                "<http://ex/c>\t<http://ex/d>".to_string()
            ],
            "{layout}: the snapshot keeps its terms"
        );
        let QueryResult::Select(rs) = snap
            .query("SELECT ?s WHERE { ?s <http://ex/q> ?o }")
            .unwrap()
        else {
            panic!("select")
        };
        assert!(rs.rows.is_empty(), "{layout}: the snapshot predates x");
        let QueryResult::Select(rs) = cdb
            .query("SELECT ?s ?o WHERE { ?s ?p ?o }")
            .unwrap()
        else {
            panic!("select")
        };
        assert_eq!(
            sorted_rows(&rs),
            vec![
                "<http://ex/c>\t<http://ex/d>".to_string(),
                "<http://ex/x>\t<http://ex/y>".to_string()
            ],
            "{layout}: the live data"
        );
        drop(snap);
        drop(cdb);
    }
}

// ---- the server ---------------------------------------------------------------

struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    root: TempDir,
    serving: Option<thread::JoinHandle<()>>,
}

impl Srv {
    fn start(tag: &str) -> Srv {
        let root = TempDir::new(tag);
        let server = Arc::new(
            ApiServer::bind(root.path(), "root", "127.0.0.1:0")
                .expect("bind")
                .with_shutdown_grace(Duration::from_secs(2))
                .with_open_options(OpenOptions::stream()),
        );
        let addr = server.local_addr().unwrap();
        let s2 = Arc::clone(&server);
        let serving = Some(thread::spawn(move || {
            s2.serve_forever();
        }));
        Srv {
            server,
            addr,
            root,
            serving,
        }
    }

    fn req(&self, method: &str, target: &str, body: &str) -> (u16, String) {
        let mut s = TcpStream::connect(self.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        let raw = format!(
            "{method} {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Basic cm9vdDpyb290\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(raw.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let status = resp
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .unwrap_or_else(|| panic!("no status in {resp:?}"));
        let body = resp.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        (status, body.to_string())
    }

    fn ok(&self, method: &str, target: &str, body: &str) -> String {
        let (s, b) = self.req(method, target, body);
        assert_eq!(s, 200, "{method} {target}: {b}");
        b
    }
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        if let Some(t) = self.serving.take() {
            let _ = t.join();
        }
    }
}

/// A JSON string field of a flat response.
fn field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    Some(&rest[..rest.find('"')?])
}

#[test]
fn a_server_built_disk_database_frees_at_commit_despite_its_begin_snapshot() {
    let srv = Srv::start("srv");
    let data = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                <http://ex/c> <http://ex/p> <http://ex/d> .\n";
    srv.ok("POST", "/build?db_name=r4&layout=disk", data);
    let dir = srv.root.path().join("r4.db");
    // A new disk database keeps a free-id list (feature FREE_IDS, V2).
    let head = std::fs::read(dir.join("kvstore.kv")).unwrap();
    assert_eq!(&head[0..8], b"GSTOREK2", "a new disk database is version 2");

    let b = srv.ok("POST", "/begin?db_name=r4", "");
    let tid = field(&b, "TID").expect("TID").to_string();
    // A non-transactional read runs on the transaction's snapshot.
    let b = srv.ok(
        "POST",
        "/sparql?db_name=r4",
        "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/b> }",
    );
    assert!(b.contains("http://ex/a"), "{b}");
    srv.ok(
        "POST",
        &format!("/tquery?db_name=r4&tid={tid}"),
        "DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/b> }",
    );
    srv.ok("POST", &format!("/commit?db_name=r4&tid={tid}"), "");
    let b = srv.ok(
        "POST",
        "/sparql?db_name=r4",
        "SELECT ?s WHERE { ?s <http://ex/p> ?o }",
    );
    assert!(!b.contains("http://ex/a") && b.contains("http://ex/c"), "{b}");
    // The commit freed <a> and <b> on disk: the snapshot it ended took no
    // pin (a deferred free would be lost with the unload).
    srv.ok("POST", "/unload?db_name=r4", "");
    let db = Database::open_with(&dir, &OpenOptions::stream()).unwrap();
    let key = |iri: &str| Term::iri(iri).dict_key();
    assert_eq!(db.dict().entity_id(&key("http://ex/a")), None);
    assert_eq!(db.dict().entity_id(&key("http://ex/b")), None);
    assert_eq!(db.entity_num(), 2, "only <c> and <d> are left");
    db.verify_integrity().unwrap();
}
