//! Data test: the RDF import pipeline (`gstore::parser::import`, GAP_PLAN
//! §4.2) — streaming with bounded memory, tolerant import with upstream's
//! `success_num` / `failed_num` and `parse_error.log`, blank-node isolation
//! across files and batches, directory and URL sources (and the load policy),
//! named graphs kept by every backend, RDF/XML entities and collections, and
//! the CLI tools' sources and `--strict`.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::json::{self, Json};
use gstore::model::Term;
use gstore::parser::import::{self, BlankNodes, ImportOptions, ParseErrorLog, RdfFormat, Source};
use gstore::parser::nquads::Quad;
use gstore::parser::stream::{BlankLabeler, StatementSink, SyntaxError};
use gstore::parser::{nquads, ntriples, trig, turtle};
use gstore::{Database, LoadPolicy, Result};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tmp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let d = std::env::temp_dir().join(format!(
        "gstore_rdfimp_{tag}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    fs::canonicalize(&d).unwrap()
}

fn write(path: &Path, content: &str) -> PathBuf {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, content).unwrap();
    path.to_path_buf()
}

fn col0(db: &mut Database, q: &str) -> Vec<String> {
    let mut v: Vec<String> = db
        .select(q)
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r[0].clone().unwrap_or_default())
        .collect();
    v.sort();
    v
}

fn named_quads(db: &Database) -> u64 {
    db.named_graphs().values().map(|g| g.triple_count()).sum()
}

// ---------------------------------------------------------------------------
// Streaming: the reader never holds more than its window
// ---------------------------------------------------------------------------

/// A lazily generated document: a preamble, `n` statements made by `stmt`
/// (each ending exactly where the next begins), and a postamble. It records
/// how many bytes the parser has pulled and where each statement ends.
struct Gen {
    pre: Vec<u8>,
    post: Vec<u8>,
    stmt: fn(u64) -> String,
    n: u64,
    next: u64,
    buf: Vec<u8>,
    pos: usize,
    produced: u64,
    pulled: Rc<Cell<u64>>,
    ends: Rc<RefCell<Vec<u64>>>,
    post_done: bool,
}

/// Shared with the sink: bytes pulled so far, and where each statement ends.
type Probes = (Rc<Cell<u64>>, Rc<RefCell<Vec<u64>>>);

impl Gen {
    fn new(pre: &str, post: &str, n: u64, stmt: fn(u64) -> String) -> (Gen, Probes) {
        let pulled = Rc::new(Cell::new(0));
        let ends = Rc::new(RefCell::new(Vec::new()));
        let g = Gen {
            pre: pre.as_bytes().to_vec(),
            post: post.as_bytes().to_vec(),
            stmt,
            n,
            next: 0,
            buf: Vec::new(),
            pos: 0,
            produced: 0,
            pulled: Rc::clone(&pulled),
            ends: Rc::clone(&ends),
            post_done: false,
        };
        (g, (pulled, ends))
    }
}

impl Read for Gen {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos == self.buf.len() {
            self.pos = 0;
            if !self.pre.is_empty() {
                self.buf = std::mem::take(&mut self.pre);
            } else if self.next < self.n {
                self.buf = (self.stmt)(self.next).into_bytes();
                self.next += 1;
                self.ends
                    .borrow_mut()
                    .push(self.produced + self.buf.len() as u64);
            } else if !self.post_done {
                self.post_done = true;
                self.buf = std::mem::take(&mut self.post);
            } else {
                self.buf.clear();
            }
            self.produced += self.buf.len() as u64;
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        self.pulled.set(self.pulled.get() + n as u64);
        Ok(n)
    }
}

/// At every statement: the bytes pulled from the source never exceed the
/// end of that statement plus the read window (plus the reader's few
/// characters of lookahead).
struct BoundCheck {
    pulled: Rc<Cell<u64>>,
    ends: Rc<RefCell<Vec<u64>>>,
    window: u64,
    seen: usize,
    quads: u64,
    max_ahead: u64,
}

impl StatementSink for BoundCheck {
    fn statement(&mut self, quads: &mut Vec<Quad>) -> Result<()> {
        let end = self.ends.borrow()[self.seen];
        let pulled = self.pulled.get();
        let ahead = pulled.saturating_sub(end);
        self.max_ahead = self.max_ahead.max(ahead);
        assert!(
            ahead <= self.window + 32,
            "statement {} ends at byte {end} but {pulled} bytes were read (window {})",
            self.seen,
            self.window
        );
        self.seen += 1;
        self.quads += quads.len() as u64;
        quads.clear();
        Ok(())
    }
    fn error(&mut self, err: SyntaxError) -> Result<()> {
        panic!("unexpected error {err:?}");
    }
}

const WINDOW: usize = 8 * 1024;

fn check_bound(format: &str, pre: &str, post: &str, n: u64, stmt: fn(u64) -> String) -> (u64, u64) {
    let (g, (pulled, ends)) = Gen::new(pre, post, n, stmt);
    let reader = BufReader::with_capacity(WINDOW, g);
    let mut sink = BoundCheck {
        pulled: Rc::clone(&pulled),
        ends: Rc::clone(&ends),
        window: WINDOW as u64,
        seen: 0,
        quads: 0,
        max_ahead: 0,
    };
    let labels = BlankLabeler::keep();
    match format {
        "nt" => ntriples::parse_reader(reader, &labels, &mut sink).unwrap(),
        "nq" => nquads::parse_reader(reader, &labels, &mut sink).unwrap(),
        "ttl" => turtle::parse_reader(reader, labels, &mut sink).unwrap(),
        _ => trig::parse_reader(reader, labels, &mut sink).unwrap(),
    }
    assert_eq!(sink.seen as u64, n);
    let total = pulled.get();
    assert!(
        total > 100 * WINDOW as u64,
        "the document ({total} bytes) must dwarf the window"
    );
    (sink.quads, sink.max_ahead)
}

fn nt_stmt(i: u64) -> String {
    format!(
        "<http://ex.org/s{i}> <http://ex.org/p> \"value {i} é 漢 😀 \\\"q\\\" {}\" .\r\n",
        "x".repeat((i % 97) as usize)
    )
}

#[test]
fn ntriples_and_nquads_stream_within_the_window() {
    let (q, _) = check_bound("nt", "# header\n", "", 20_000, nt_stmt);
    assert_eq!(q, 20_000);
    let (q, _) = check_bound("nq", "", "", 20_000, |i| {
        format!(
            "<http://ex.org/s{i}> <http://ex.org/p> \"v{i}\" <http://g/{}> .\n",
            i % 3
        )
    });
    assert_eq!(q, 20_000);
}

#[test]
fn turtle_streams_within_the_window() {
    let pre =
        "@prefix ex: <http://ex.org/> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n";
    let (q, _) = check_bound("ttl", pre, "", 15_000, |i| {
        // Multi-line statements, nested lists, long literals (some bigger
        // than the window itself), comments.
        let long = if i % 500 == 0 {
            "L".repeat(3 * WINDOW)
        } else {
            String::new()
        };
        format!(
            "ex:s{i} a ex:T ;\n  ex:name \"\"\"name {i}\nline 2 {long}\"\"\"@en ,\n    'other' ;\n  ex:list ( {i} ex:a [ ex:p ex:o ] ) . # c\n"
        )
    });
    // Per statement: `a`, two names, the list link, 3 rdf:first + 3
    // rdf:rest, and the `[]`'s property.
    assert_eq!(q, 15_000 * 11);
}

#[test]
fn one_huge_trig_block_is_not_buffered() {
    let pre = "@prefix ex: <http://ex.org/> .\nex:g {\n";
    let (q, _) = check_bound("trig", pre, "}\n", 30_000, |i| {
        format!("ex:s{i} ex:p \"v {i} é\" , ex:o{i} .\n")
    });
    assert_eq!(q, 60_000);
}

// ---------------------------------------------------------------------------
// Tolerant import: counts and the parse error log
// ---------------------------------------------------------------------------

const MIXED_TTL: &str = "@prefix ex: <http://ex.org/> .\n\
ex:a ex:p ex:o1 .\n\
ex:b ex:p undefined:x .\n\
ex:c ex:p \"12\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n\
ex:d ex:p \"abc\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n\
\"lit\" ex:p ex:o2 .\n\
ex:e ex:p \"NaN\"^^<http://www.w3.org/2001/XMLSchema#double> .\n\
ex:f ex:p ex:o3 ; ex:q ex:o4 .\n";

#[test]
fn tolerant_build_counts_and_logs_like_upstream() {
    let dir = tmp("tolerant");
    let data = write(&dir.join("mixed.ttl"), MIXED_TTL);
    let log_path = dir.join("db/parse_error.log");
    let mut log = ParseErrorLog::open(&log_path).unwrap();
    let (db, report) = Database::build_from_sources(
        "t",
        &[Source::File(data.clone())],
        &ImportOptions::tolerant(),
        &mut log,
    )
    .unwrap();
    drop(log);
    // a, c, f (2) stored; b (syntax), d (integer), "lit" (subject), e (NaN)
    // skipped.
    assert_eq!((report.statements, report.failed), (4, 4));
    assert_eq!(db.triple_num(), 4);
    let text = fs::read_to_string(&log_path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 5, "{text}");
    assert!(lines[0].starts_with("Info "), "{text}");
    assert!(
        lines[0].ends_with(&format!("build parser info, file path {}", data.display())),
        "{text}"
    );
    assert!(lines[1].starts_with("Line 3 ("), "{text}");
    assert!(lines[1].contains("undefined prefix"), "{text}");
    // Failures found after parsing (upstream's literal checks, the subject
    // check) carry the line the statement was read from too.
    assert!(lines[2].starts_with("Line 5 ("), "{text}");
    assert!(lines[2].contains("Object integer value invalid"), "{text}");
    assert!(lines[3].starts_with("Line 6 ("), "{text}");
    assert!(lines[3].contains("invalid subject"), "{text}");
    assert!(lines[4].starts_with("Line 7 ("), "{text}");
    assert!(lines[4].contains("Object double value is NaN"), "{text}");
    assert_eq!(import::count_logged_failures(&log_path), report.failed);

    // Strict: the first failure is the error and nothing is built.
    let err = Database::build_from_files("s", &[&data]).unwrap_err();
    assert!(err.to_string().contains("line 3"), "{err}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_document_parsed_whole_counts_as_one_failure_and_respects_its_size_limit() {
    let dir = tmp("whole");
    let rdf = write(
        &dir.join("bad.rdf"),
        "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><unclosed></rdf:RDF>",
    );
    let good = write(
        &dir.join("good.rdf"),
        "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:ex=\"http://ex/\">\
         <rdf:Description rdf:about=\"http://ex/a\"><ex:p>v</ex:p></rdf:Description></rdf:RDF>",
    );
    let mut errors: Vec<(String, SyntaxError)> = Vec::new();
    let (_, r) = Database::build_from_sources(
        "w",
        &[Source::File(rdf), Source::File(good.clone())],
        &ImportOptions::tolerant(),
        &mut errors,
    )
    .unwrap();
    assert_eq!((r.statements, r.failed), (1, 1));
    // Over the limit: refused (tolerant: one failure; strict: an error).
    let tiny = ImportOptions {
        max_document_bytes: 64,
        ..ImportOptions::tolerant()
    };
    let (_, r) =
        Database::build_from_sources("w2", &[Source::File(good.clone())], &tiny, &mut errors)
            .unwrap();
    assert_eq!((r.statements, r.failed), (0, 1));
    assert!(errors.last().unwrap().1.message.contains("limit"));
    let strict_tiny = ImportOptions {
        max_document_bytes: 64,
        ..ImportOptions::strict()
    };
    assert!(
        Database::build_from_sources("w3", &[Source::File(good)], &strict_tiny, &mut ()).is_err()
    );
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Blank nodes
// ---------------------------------------------------------------------------

fn blank_subjects(db: &mut Database, pred: &str) -> Vec<String> {
    col0(db, &format!("SELECT ?s WHERE {{ ?s <{pred}> ?o }}"))
}

#[test]
fn blank_nodes_never_merge_across_files_or_batches() {
    let dir = tmp("bnodes");
    let a = write(
        &dir.join("a.ttl"),
        "_:b <http://ex/p> \"a\" . [] <http://ex/q> \"a\" .\n",
    );
    let b = write(
        &dir.join("b.nt"),
        "_:b <http://ex/p> \"b\" .\n_:genid-0 <http://ex/q> \"b\" .\n",
    );
    let mut db = Database::build_from_files("bn", &[&a, &b]).unwrap();
    let ps = blank_subjects(&mut db, "http://ex/p");
    assert_eq!(ps.len(), 2, "_:b of two files are two nodes: {ps:?}");
    assert_ne!(ps[0], ps[1]);
    assert_eq!(blank_subjects(&mut db, "http://ex/q").len(), 2);
    // Two inserts of the same file: two more nodes each time.
    for round in 1..=2 {
        let mut n = 0;
        db.insert_sources(
            &[Source::File(a.clone())],
            &ImportOptions::tolerant(),
            &mut (),
            &mut n,
        )
        .unwrap();
        assert_eq!(n, 2, "nothing merged with what is stored");
        assert_eq!(blank_subjects(&mut db, "http://ex/p").len(), 2 + round);
    }
    // Every stored label round-trips through parse_term.
    for s in col0(&mut db, "SELECT ?s WHERE { ?s ?p ?o }") {
        let t = ntriples::parse_term(&s).unwrap();
        assert!(matches!(t, Term::Blank(_)), "{s}");
        assert_eq!(t.to_string(), s);
    }
    // A removal names stored nodes verbatim: an export-style file removes
    // exactly those triples. (Pick a node whose object is "a": the labels'
    // random scopes leave the result order unspecified, and b.nt's node has
    // "b".)
    let stored = col0(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> \"a\" }");
    assert_eq!(stored.len(), 3, "{stored:?}");
    let victim = write(
        &dir.join("victim.nt"),
        &format!("{} <http://ex/p> \"a\" .\n", stored[0]),
    );
    let before = db.triple_num();
    let mut removed = 0;
    let r = db
        .remove_sources(
            &[Source::File(victim)],
            &ImportOptions::tolerant().for_removal(),
            &mut (),
            &mut removed,
        )
        .unwrap();
    assert_eq!((removed, r.failed), (1, 0));
    assert_eq!(db.triple_num(), before - 1);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn library_parsers_keep_labels_and_mint_round_trippable_ones() {
    for (fmt, doc) in [
        (RdfFormat::Turtle, "_:x <http://e/p> [ <http://e/q> ( 1 ) ] ."),
        (RdfFormat::TriG, "_:g { _:x <http://e/p> [] }"),
        (
            RdfFormat::RdfXml,
            "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" xmlns:e=\"http://e/\">\
             <rdf:Description rdf:nodeID=\"x\"><e:p><e:T/></e:p></rdf:Description></rdf:RDF>",
        ),
        (
            RdfFormat::JsonLd,
            "{\"@id\":\"_:x\",\"http://e/p\":{\"http://e/q\":\"v\"}}",
        ),
        (
            RdfFormat::Rdfa,
            "<div about=\"_:x\" rel=\"http://e/p\"><span typeof=\"http://e/T\"></span></div>",
        ),
    ] {
        let opts = ImportOptions {
            blank_nodes: BlankNodes::Keep,
            ..ImportOptions::strict()
        };
        let mut quads = Vec::new();
        import::import_reader(doc.as_bytes(), fmt, "doc", &opts, &mut (), &mut |b: &mut Vec<Quad>| {
            quads.append(b);
            Ok(())
        })
        .unwrap();
        assert!(!quads.is_empty(), "{fmt:?}");
        let mut saw_x = false;
        for q in &quads {
            for t in [&q.subject, &q.object] {
                if let Term::Blank(l) = t {
                    saw_x |= l == "x";
                    assert_eq!(&ntriples::parse_term(&t.to_string()).unwrap(), t, "{fmt:?}");
                }
            }
        }
        assert!(saw_x, "{fmt:?}: the explicit label is kept: {quads:?}");
    }
}

// ---------------------------------------------------------------------------
// Sources: directories, URLs, the load policy, gzip
// ---------------------------------------------------------------------------

#[test]
fn directory_sources_load_every_rdf_file_sorted_and_recursively() {
    let dir = tmp("dirsrc");
    let data = dir.join("data");
    write(
        &data.join("b/2.ttl"),
        "@prefix e: <http://e/> . e:s e:p e:two .\n",
    );
    write(
        &data.join("a.nt"),
        "<http://e/s> <http://e/p> <http://e/one> .\n",
    );
    write(
        &data.join("b/sub/3.nq"),
        "<http://e/s> <http://e/p> <http://e/three> <http://e/g> .\n",
    );
    write(&data.join("README.md"), "not data");
    write(
        &data.join(".hidden/x.nt"),
        "<http://e/s> <http://e/p> <http://e/hidden> .\n",
    );
    write(
        &data.join("c.rdf"),
        "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"/>",
    );
    let sources = import::sources_from_paths(&[&data]).unwrap();
    let names: Vec<String> = sources
        .iter()
        .map(|s| {
            s.name()
                .trim_start_matches(&data.display().to_string())
                .to_string()
        })
        .collect();
    assert_eq!(names, ["/a.nt", "/b/2.ttl", "/b/sub/3.nq", "/c.rdf"]);
    let mut db = Database::build_from_files("d", &[&data]).unwrap();
    assert_eq!(
        col0(&mut db, "SELECT ?o WHERE { ?s <http://e/p> ?o }"),
        ["<http://e/one>", "<http://e/two>"]
    );
    assert_eq!(
        col0(
            &mut db,
            "SELECT ?o WHERE { GRAPH <http://e/g> { ?s ?p ?o } }"
        ),
        ["<http://e/three>"]
    );
    // The load policy governs directories too.
    let input = [data.to_string_lossy().into_owned()];
    assert!(import::expand_sources(&input, &LoadPolicy::restricted()).is_err());
    let allowed = LoadPolicy::restricted().with_allowed_dir(&dir);
    assert_eq!(import::expand_sources(&input, &allowed).unwrap().len(), 4);
    // A symlink inside the allowed directory may not lead out of it.
    #[cfg(unix)]
    {
        let outside = tmp("dirsrc_outside");
        let secret = write(&outside.join("secret.nt"), "<http://e/x> <http://e/y> <http://e/z> .\n");
        std::os::unix::fs::symlink(&secret, data.join("link.nt")).unwrap();
        let err = import::expand_sources(&input, &allowed).unwrap_err();
        assert!(err.to_string().contains("load policy"), "{err}");
        // Trusted paths (the CLI's own arguments) follow it.
        assert_eq!(import::sources_from_paths(&[&data]).unwrap().len(), 5);
        let _ = fs::remove_dir_all(&outside);
    }
    let _ = fs::remove_dir_all(&dir);
}

/// A one-shot-per-connection HTTP server answering every `GET <path>` from
/// `files` (`chunked` paths with `Transfer-Encoding: chunked`).
fn http_server(files: Vec<(&'static str, String)>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut reader = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                    break;
                }
            }
            let target = first.split_whitespace().nth(1).unwrap_or("/");
            let path = target.split('?').next().unwrap_or("/").to_string();
            let found = files.iter().find(|(p, _)| *p == path);
            let resp = match found {
                Some((p, body)) if p.contains("chunked") => {
                    let mid = body.len() / 2;
                    format!(
                        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                        mid,
                        &body[..mid],
                        body.len() - mid,
                        &body[mid..]
                    )
                }
                Some((_, body)) => format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                ),
                None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_string(),
            };
            let _ = s.write_all(resp.as_bytes());
        }
    });
    addr
}

#[test]
fn url_sources_download_through_the_http_client() {
    let addr = http_server(vec![
        (
            "/data.ttl",
            "@prefix e: <http://e/> . e:u e:p e:o1 , e:o2 .\n".to_string(),
        ),
        (
            "/q.nq",
            "<http://e/u> <http://e/p> <http://e/o3> <http://e/g> .\n".to_string(),
        ),
        (
            "/chunked.nt",
            "<http://e/u> <http://e/p> <http://e/o4> .\n".to_string(),
        ),
    ]);
    let urls = [
        format!("http://{addr}/data.ttl"),
        format!("http://{addr}/q.nq?x=1"),
        format!("http://{addr}/chunked.nt"),
    ];
    let db = Database::build_from_files("u", &urls).unwrap();
    assert_eq!(db.triple_num(), 3);
    assert_eq!(named_quads(&db), 1);
    // A missing document is an error.
    assert!(Database::build_from_files("u2", &[format!("http://{addr}/nope.ttl")]).is_err());
    // The load policy decides whether URLs may be named at all.
    let err = import::expand_sources(&urls[..1], &LoadPolicy::restricted()).unwrap_err();
    assert!(err.to_string().contains("load policy"), "{err}");
    let ok = import::expand_sources(&urls[..1], &LoadPolicy::permissive()).unwrap();
    assert_eq!(ok, [Source::Url(urls[0].clone())]);
}

#[cfg(feature = "gzip")]
#[test]
fn gzip_files_are_decompressed_by_extension() {
    use flate2::write::GzEncoder;
    let dir = tmp("gz");
    let path = dir.join("d.nt.gz");
    let mut enc = GzEncoder::new(
        fs::File::create(&path).unwrap(),
        flate2::Compression::default(),
    );
    enc.write_all(b"<http://e/s> <http://e/p> <http://e/o> .\n")
        .unwrap();
    enc.finish().unwrap();
    let db = Database::build_from_files("gz", &[&dir]).unwrap();
    assert_eq!(db.triple_num(), 1);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(not(feature = "gzip"))]
#[test]
fn gzip_input_needs_the_feature() {
    let dir = tmp("nogz");
    let path = write(&dir.join("d.nt.gz"), "not really gzip");
    let err = Database::build_from_files("gz", &[&path]).unwrap_err();
    assert!(err.to_string().contains("gzip"), "{err}");
    // A directory walk skips .gz files without the feature.
    assert!(import::sources_from_paths(&[&dir]).unwrap().is_empty());
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Named graphs
// ---------------------------------------------------------------------------

const TRIG: &str = "@prefix e: <http://e/> .\n\
e:s e:p e:default .\n\
e:g1 { e:s e:p e:one . e:s e:p e:two }\n\
GRAPH e:g2 { e:s e:p e:three }\n\
_:bg { e:s e:p e:four }\n";

#[test]
fn named_graphs_survive_every_build_path() {
    let dir = tmp("graphs");
    let tg = write(&dir.join("d.trig"), TRIG);
    let nq = write(
        &dir.join("d.nq"),
        "<http://e/s> <http://e/p> <http://e/five> <http://e/g1> .\n<http://e/s> <http://e/p> <http://e/six> .\n",
    );
    let check = |db: &mut Database| {
        assert_eq!(
            col0(db, "SELECT ?o WHERE { <http://e/s> <http://e/p> ?o }"),
            ["<http://e/default>", "<http://e/six>"]
        );
        assert_eq!(
            col0(db, "SELECT ?o WHERE { GRAPH <http://e/g1> { ?s ?p ?o } }"),
            ["<http://e/five>", "<http://e/one>", "<http://e/two>"]
        );
        let graphs = col0(db, "SELECT DISTINCT ?g WHERE { GRAPH ?g { ?s ?p ?o } }");
        assert_eq!(graphs.len(), 3, "{graphs:?}");
        assert!(graphs
            .iter()
            .any(|g| g.starts_with(&format!("<{}", import::BNODE_GRAPH_PREFIX))));
    };
    // In memory, saved and reopened.
    let mut db = Database::build_from_files("g", &[&tg, &nq]).unwrap();
    check(&mut db);
    db.save(dir.join("mem.db")).unwrap();
    check(&mut Database::open(dir.join("mem.db")).unwrap());
    // On disk.
    Database::build_disk(dir.join("disk.db"), &[&tg, &nq]).unwrap();
    check(&mut Database::open(dir.join("disk.db")).unwrap());
    // RocksDB (the `gspo` column family), built and reopened.
    #[cfg(feature = "rocksdb")]
    {
        let rdir = dir.join("rocks.db");
        let mut built = Database::build_rocksdb_from_files("r", &rdir, &[&tg, &nq]).unwrap();
        check(&mut built);
        drop(built);
        check(&mut Database::open(&rdir).unwrap());
    }
    // gadd-style inserts into an existing database, and LOAD.
    let mut db = Database::new("i");
    let mut n = 0;
    db.insert_sources(
        &[Source::File(tg.clone()), Source::File(nq.clone())],
        &ImportOptions::strict(),
        &mut (),
        &mut n,
    )
    .unwrap();
    // TriG: 1 default + 2 + 1 + 1 named; N-Quads: 1 named + 1 default.
    assert_eq!(n, 7);
    check(&mut db);
    let mut db = Database::new("l");
    db.query(&format!("LOAD <file://{}>", tg.display()))
        .unwrap();
    db.query(&format!("LOAD <file://{}>", nq.display()))
        .unwrap();
    check(&mut db);
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// RDF/XML
// ---------------------------------------------------------------------------

#[test]
fn rdfxml_entities_and_collections_load() {
    let dir = tmp("rdfxml");
    let owl = write(
        &dir.join("o.owl"),
        r#"<?xml version="1.0"?>
<!DOCTYPE rdf:RDF [
  <!ENTITY xsd "http://www.w3.org/2001/XMLSchema#">
  <!ENTITY e "http://e/">
]>
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:e="&e;">
  <rdf:Description rdf:about="&e;x">
    <e:age rdf:datatype="&xsd;integer">3</e:age>
    <e:members rdf:parseType="Collection">
      <rdf:Description rdf:about="&e;m1"/>
      <rdf:Description rdf:about="&e;m2"/>
    </e:members>
  </rdf:Description>
</rdf:RDF>"#,
    );
    let mut db = Database::build_from_files("x", &[&owl]).unwrap();
    assert_eq!(
        col0(
            &mut db,
            "SELECT ?a WHERE { <http://e/x> <http://e/age> ?a }"
        ),
        ["\"3\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
    assert_eq!(
        col0(
            &mut db,
            "SELECT ?m WHERE { <http://e/x> <http://e/members> ?l . ?l <http://www.w3.org/1999/02/22-rdf-syntax-ns#rest>* ?c . ?c <http://www.w3.org/1999/02/22-rdf-syntax-ns#first> ?m }"
        ),
        ["<http://e/m1>", "<http://e/m2>"]
    );
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The /api operations
// ---------------------------------------------------------------------------

struct Srv {
    server: Arc<ApiServer>,
    addr: SocketAddr,
    root: PathBuf,
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.server.shutdown();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn start(tag: &str, policy: Option<LoadPolicy>) -> Srv {
    let root = tmp(tag);
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    config.upload_allow_extensions = ["nt", "ttl", "n3", "rdf", "txt", "nq", "trig"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut server = ApiServer::bind(&root, "root", "127.0.0.1:0")
        .unwrap()
        .with_config(config);
    if let Some(p) = policy {
        server = server.with_load_policy(p);
    }
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    Srv { server, addr, root }
}

fn api(addr: &SocketAddr, body: Json) -> Json {
    let body = body
        .with("username", "root")
        .with("password", "root")
        .to_string();
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let req = format!(
        "POST /api HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let text = String::from_utf8_lossy(&resp).into_owned();
    let (_, json_text) = text.split_once("\r\n\r\n").unwrap();
    json::parse(json_text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn op(name: &str, db: &str) -> Json {
    Json::obj().with("operation", name).with("db_name", db)
}

fn int(j: &Json, k: &str) -> i64 {
    j.get(k)
        .and_then(Json::as_i64)
        .unwrap_or_else(|| panic!("no {k} in {j}"))
}

fn code(j: &Json) -> i64 {
    int(j, "StatusCode")
}

#[test]
fn api_build_and_batches_report_upstream_counts_and_log() {
    let srv = start("api", None);
    let a = &srv.addr;
    let up = srv.root.join("upload");
    let mixed = write(&up.join("mixed.ttl"), MIXED_TTL);
    let j = api(
        a,
        op("build", "imp").with("db_path", mixed.to_string_lossy().as_ref()),
    );
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!((int(&j, "success_num"), int(&j, "failed_num")), (4, 4));
    let log = srv.root.join("imp.db").join("parse_error.log");
    let text = fs::read_to_string(&log).unwrap();
    assert!(
        text.lines()
            .next()
            .unwrap()
            .contains("build parser info, file path"),
        "{text}"
    );
    assert_eq!(import::count_logged_failures(&log), 4);

    assert_eq!(code(&api(a, op("load", "imp"))), 0);
    // batchInsert: a TriG file (named graph kept) with one bad statement,
    // blank nodes that must not merge with a second call.
    let more = write(
        &up.join("more.trig"),
        "@prefix e: <http://e/> .\ne:g { _:b e:p \"x\" . e:bad e:p }\n_:b e:q \"y\" .\n",
    );
    let insert = || {
        api(
            a,
            op("batchInsert", "imp").with("file", more.to_string_lossy().as_ref()),
        )
    };
    let j = insert();
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!((int(&j, "success_num"), int(&j, "failed_num")), (2, 1));
    let j = insert();
    assert_eq!(
        (int(&j, "success_num"), int(&j, "failed_num")),
        (2, 1),
        "no merge: {j}"
    );
    let text = fs::read_to_string(&log).unwrap();
    assert_eq!(
        text.lines()
            .filter(|l| l.contains("batch insert parser info"))
            .count(),
        2,
        "{text}"
    );
    assert_eq!(import::count_logged_failures(&log), 6);
    let j = api(
        a,
        op("query", "imp").with(
            "sparql",
            "SELECT ?s WHERE { GRAPH <http://e/g> { ?s <http://e/p> \"x\" } }",
        ),
    );
    assert_eq!(int(&j, "AnsNum"), 2, "{j}");
    // batchRemove: counted and logged too.
    let rm = write(
        &up.join("rm.nt"),
        "<http://ex.org/a> <http://ex.org/p> <http://ex.org/o1> .\nbroken line\n",
    );
    let j = api(
        a,
        op("batchRemove", "imp").with("file", rm.to_string_lossy().as_ref()),
    );
    assert_eq!(
        (int(&j, "success_num"), int(&j, "failed_num")),
        (1, 1),
        "{j}"
    );
    let text = fs::read_to_string(&log).unwrap();
    assert!(text.contains("batch remove parser info"), "{text}");
}

#[test]
fn api_remote_sources_follow_the_load_policy() {
    let http = http_server(vec![(
        "/remote.ttl",
        "@prefix e: <http://e/> . e:r e:p e:o .\n".to_string(),
    )]);
    let url = format!("http://{http}/remote.ttl");
    // Default server policy: no remote sources.
    let srv = start("remote_denied", None);
    let j = api(
        &srv.addr,
        op("build", "rem")
            .with("db_path", url.as_str())
            .with("remote", true),
    );
    assert_eq!(code(&j), 1003, "{j}");
    assert!(j.to_string().contains("load policy"), "{j}");
    drop(srv);
    // A policy allowing remote sources: downloaded, built, cleaned up.
    let mut policy = LoadPolicy::restricted();
    policy.allow_remote = true;
    let srv = start("remote_allowed", Some(policy));
    let j = api(
        &srv.addr,
        op("build", "rem")
            .with("db_path", url.as_str())
            .with("remote", true),
    );
    assert_eq!(code(&j), 0, "{j}");
    assert_eq!(int(&j, "success_num"), 1);
    let leftovers: Vec<_> = fs::read_dir(srv.root.join("upload"))
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "downloads are removed: {leftovers:?}");
    let j = api(
        &srv.addr,
        op("build", "rem2")
            .with("db_path", format!("http://{http}/missing.ttl").as_str())
            .with("remote", true),
    );
    assert_eq!(code(&j), 1005, "{j}");
    assert!(j.to_string().contains("Download file"), "{j}");
}

// ---------------------------------------------------------------------------
// CLI tools
// ---------------------------------------------------------------------------

fn run(bin: &str, args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

#[test]
fn cli_tools_take_directories_are_tolerant_and_have_strict() {
    let dir = tmp("cli");
    let src = dir.join("src");
    write(&src.join("a.ttl"), MIXED_TTL);
    write(
        &src.join("nested/b.nq"),
        "<http://e/s> <http://e/p> <http://e/o> <http://e/g> .\n",
    );
    let s = src.to_string_lossy().into_owned();
    let out = run(env!("CARGO_BIN_EXE_gbuild"), &["cli", &s], &dir);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("RDF parse error num 4"), "{stdout}");
    assert!(stdout.contains("named-graph quads=1"), "{stdout}");
    assert_eq!(
        import::count_logged_failures(&dir.join("cli.db/parse_error.log")),
        4
    );
    // --strict: the first bad statement fails the build.
    let out = run(
        env!("CARGO_BIN_EXE_gbuild"),
        &["--strict", "strict", &s],
        &dir,
    );
    assert!(!out.status.success(), "{out:?}");
    // gadd: a directory of files, tolerant, logged.
    let add = dir.join("add");
    write(
        &add.join("x.nt"),
        "<http://e/n> <http://e/p> \"new\" .\nnot rdf\n",
    );
    let out = run(
        env!("CARGO_BIN_EXE_gadd"),
        &["cli", &add.to_string_lossy()],
        &dir,
    );
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Added 1 new triple(s)"), "{stdout}");
    assert!(stdout.contains("Skipped 1 statement(s)"), "{stdout}");
    let out = run(
        env!("CARGO_BIN_EXE_gadd"),
        &["--strict", "cli", &add.to_string_lossy()],
        &dir,
    );
    assert!(!out.status.success(), "{out:?}");
    // gsub removes, from the default and the named graph.
    let out = run(
        env!("CARGO_BIN_EXE_gsub"),
        &[
            "cli",
            &add.join("x.nt").to_string_lossy(),
            &src.join("nested/b.nq").to_string_lossy(),
        ],
        &dir,
    );
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Removed 2 triple(s)"), "{stdout}");
    let log = fs::read_to_string(dir.join("cli.db/parse_error.log")).unwrap();
    assert!(log.contains("batch insert parser info"), "{log}");
    assert!(log.contains("batch remove parser info"), "{log}");
    let _ = fs::remove_dir_all(&dir);
}
