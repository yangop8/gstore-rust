//! `Database::replay_update_log` replays the statements earlier gstore-rust
//! versions accepted and logged (round-4 review of sparqlconf#16 /
//! parser#13's fix).
//!
//! An `update.log` record is a request that succeeded when it was logged.
//! This round's parser and graph-management rules refuse requests earlier
//! versions accepted — `DROP GRAPH` / `CLEAR GRAPH` of a missing graph and
//! `CREATE GRAPH` of a populated one (each a successful no-op then), a
//! labelled blank node in `DELETE DATA` / `DELETE WHERE` / a DELETE
//! template (the stored node then), `PREFIX p:x <…>`, two triple statements
//! without a `.`, a repeated `LIMIT` — and a replay stopped at the first such
//! record, losing every statement after it. Replay now reads a record as
//! the version that logged it did; records this version logs replay as they
//! ran.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use gstore::{Database, QueryResult};

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gstore_update_log_compat_{}_{}_{tag}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// CRC-32 (IEEE), as the log's record checksums use.
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for part in parts {
        for &b in *part {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
    }
    !crc
}

/// How an earlier version framed its records.
#[derive(Debug, Clone, Copy)]
enum Framing {
    /// Before record checksums: `REC <millis> <changed> <len>`, each record
    /// a commit of its own.
    Plain,
    /// With checksums, one statement per commit (`e=<crc>`).
    Checksummed,
}

fn log_of(statements: &[&str], framing: Framing) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, s) in statements.iter().enumerate() {
        let head = format!("REC {} 0 {}", 1_700_000_000_000u64 + i as u64, s.len());
        match framing {
            Framing::Plain => out.extend_from_slice(format!("{head}\n").as_bytes()),
            Framing::Checksummed => {
                let head = format!("{head} e=");
                let crc = crc32(&[head.as_bytes(), b"\n", s.as_bytes()]);
                out.extend_from_slice(format!("{head}{crc:08x}\n").as_bytes());
            }
        }
        out.extend_from_slice(s.as_bytes());
        out.push(b'\n');
    }
    out
}

/// Every quad of `db` as sorted N-Quads lines.
fn dataset(db: &Database) -> Vec<String> {
    let mut out = Vec::new();
    gstore::zipio::write_export(db, gstore::zipio::ExportFormat::NQuads, &mut out).unwrap();
    let mut lines: Vec<String> = String::from_utf8(out)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

/// Statements earlier versions accepted (and so logged), each of which this
/// version's parser or graph-management rules refuse as a request.
const EARLIER: &[&str] = &[
    "INSERT DATA { <http://ex/a> <http://ex/p> \"1\" . _:x <http://ex/p> \"bx\" . \
     _:y <http://ex/p> \"by\" . _:w <http://ex/q> \"w\" . }",
    // A successful no-op then (`[updated 0]`).
    "DROP GRAPH <http://ex/missing>",
    "CLEAR GRAPH <http://ex/missing>",
    "INSERT DATA { GRAPH <http://ex/g> { <http://ex/a> <http://ex/p> \"g\" } }",
    // A no-op then too: the graph holds a triple.
    "CREATE GRAPH <http://ex/g>",
    // The stored node, as a WHERE pattern still reads a label.
    "DELETE DATA { _:x <http://ex/p> \"bx\" }",
    "DELETE WHERE { _:y <http://ex/p> ?o }",
    "DELETE { _:w <http://ex/q> ?o } WHERE { _:w <http://ex/q> ?o }",
    // Declared `ex:` (dropping the local part).
    "PREFIX ex:x <http://ex/> INSERT DATA { ex:b ex:p \"2\" }",
    // Two statements without a '.'.
    "INSERT DATA { <http://ex/c> <http://ex/p> \"3\" <http://ex/d> <http://ex/p> \"4\" }",
    // The last LIMIT won.
    "INSERT { ?s <http://ex/top> ?o } WHERE { { SELECT ?s ?o WHERE { ?s <http://ex/p> ?o } \
     ORDER BY ?o LIMIT 5 LIMIT 1 } }",
    "INSERT DATA { <http://ex/z> <http://ex/p> \"last\" }",
];

#[test]
fn a_log_an_earlier_version_wrote_replays_whole() {
    // Each of them is refused as a request now (so a replay that parses
    // records as requests stops at the second one).
    for s in &EARLIER[1..11] {
        if s.starts_with("INSERT DATA { GRAPH") {
            continue;
        }
        let mut db = Database::new("probe");
        db.query("INSERT DATA { GRAPH <http://ex/g> { <http://ex/a> <http://ex/p> \"g\" } }")
            .unwrap();
        assert!(db.query(s).is_err(), "accepted as a request: {s}");
    }

    let expected = [
        "<http://ex/a> <http://ex/p> \"1\" .",
        "<http://ex/a> <http://ex/p> \"g\" <http://ex/g> .",
        "<http://ex/a> <http://ex/top> \"1\" .",
        "<http://ex/b> <http://ex/p> \"2\" .",
        "<http://ex/c> <http://ex/p> \"3\" .",
        "<http://ex/d> <http://ex/p> \"4\" .",
        "<http://ex/z> <http://ex/p> \"last\" .",
    ];
    for framing in [Framing::Plain, Framing::Checksummed] {
        let tmp = TmpDir::new("earlier");
        let log = tmp.0.join("update.log");
        fs::write(&log, log_of(EARLIER, framing)).unwrap();
        let mut db = Database::new("replayed");
        let applied = db
            .replay_update_log(&log)
            .unwrap_or_else(|e| panic!("{framing:?}: {e}"));
        assert_eq!(applied, EARLIER.len(), "{framing:?}");
        assert_eq!(dataset(&db), expected, "{framing:?}");
    }
}

/// What this version logs replays as it ran: a graph-management record of
/// a graph that exists still does what it did, a failed request is not
/// logged, and a blank node named through `sameTerm` is the stored one.
#[test]
fn a_log_this_version_wrote_replays_as_it_ran() {
    let tmp = TmpDir::new("current");
    let log = tmp.0.join("update.log");
    let mut db = Database::new("orig");
    db.enable_update_log(&log);
    for (u, ok) in [
        (
            "INSERT DATA { GRAPH <http://ex/g> { <http://ex/s> <http://ex/p> \"1\" } \
             <http://ex/s> <http://ex/p> \"d\" . _:k <http://ex/p> \"k\" . \
             _:m <http://ex/p> \"k\" . }",
            true,
        ),
        ("DROP GRAPH <http://ex/g>", true),
        // Refused, not logged.
        ("DROP GRAPH <http://ex/g>", false),
        ("CREATE GRAPH <http://ex/new>", true),
        (
            "INSERT DATA { GRAPH <http://ex/h> { <http://ex/s> <http://ex/p> \"2\" } }",
            true,
        ),
        ("CREATE GRAPH <http://ex/h>", false),
        ("CLEAR GRAPH <http://ex/h>", true),
        (
            "INSERT DATA { GRAPH <http://ex/h> { <http://ex/s> <http://ex/p> \"3\" } }",
            true,
        ),
        (
            "DELETE { ?b <http://ex/p> \"k\" } WHERE { ?b <http://ex/p> \"k\" \
             FILTER(sameTerm(?b, _:k)) }",
            true,
        ),
    ] {
        match db.query(u) {
            Ok(QueryResult::Update { .. }) => assert!(ok, "accepted: {u}"),
            Ok(other) => panic!("{u}: {other:?}"),
            Err(e) => assert!(!ok, "{u}: {e}"),
        }
    }
    db.disable_update_log();
    let want = dataset(&db);
    assert_eq!(
        want,
        [
            "<http://ex/s> <http://ex/p> \"3\" <http://ex/h> .",
            "<http://ex/s> <http://ex/p> \"d\" .",
            "_:m <http://ex/p> \"k\" .",
        ]
    );
    let mut replayed = Database::new("replayed");
    assert_eq!(replayed.replay_update_log(&log).unwrap(), 7);
    assert_eq!(dataset(&replayed), want);
}
