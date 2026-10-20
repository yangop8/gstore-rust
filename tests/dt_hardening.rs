//! Hardening regressions: query-path panics, dictionary id-space bounds,
//! password hashing, and the SPARQL `LOAD` file-access policy.

use std::panic::{catch_unwind, AssertUnwindSafe};

use gstore::db::LoadPolicy;
use gstore::dict::Dictionary;
use gstore::parser::sparql::ast::{
    Expr, GraphPattern, PatternTerm, Projection, Query, SelectItem, SelectQuery, TriplePattern,
};
use gstore::parser::sparql::builtins::BUILTINS;
use gstore::query::Evaluator;
use gstore::store::TripleStore;
use gstore::{Database, GStoreError, IdTriple, QueryResult, Term};

const DATA: &str = "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
                    <http://ex/b> <http://ex/p> <http://ex/c> .\n\
                    <http://ex/a> <http://ex/name> \"Alice\" .\n\
                    <http://ex/a> <http://ex/age> \"42\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n";

// ---- 1. built-in arity / shape -------------------------------------------

/// The exact crash from the gap report: `FILTER(STR())` panicked the query
/// thread (index out of bounds). It is now a parse error naming the function.
#[test]
fn str_with_no_arguments_is_a_clean_parse_error() {
    let mut db = Database::build_from_str("h", DATA).unwrap();
    let err = db
        .query("SELECT ?s WHERE { ?s <http://ex/p> ?o FILTER(STR()) }")
        .unwrap_err();
    match err {
        GStoreError::SparqlParse(msg) => {
            assert_eq!(msg, "STR expects exactly 1 argument, got 0");
        }
        other => panic!("expected a SPARQL parse error, got {other:?}"),
    }
}

/// Argument expressions cycled to build calls of any arity: a variable bound
/// to an IRI, a constant IRI, an integer, and a string.
const ARG_POOL: [&str; 4] = ["?o", "<http://ex/a>", "2", "\"x\""];

fn call(name: &str, n: usize) -> String {
    let args: Vec<&str> = ARG_POOL.iter().cycle().take(n).copied().collect();
    format!("{name}({})", args.join(", "))
}

/// Table-driven: every built-in the engine recognizes, called with 0–4
/// arguments in FILTER, projection, BIND and ORDER BY position, through the
/// public `Database::query` API. No call may panic; a call with an arity the
/// built-in rejects must be a parse error naming the function.
#[test]
fn every_builtin_with_zero_to_four_args_never_panics() {
    let mut db = Database::build_from_str("arity", DATA).unwrap();
    let mut panics = Vec::new();
    let mut checked = 0;
    for spec in BUILTINS {
        for n in 0..=4 {
            let c = call(spec.name, n);
            let queries = [
                format!("SELECT ?s WHERE {{ ?s <http://ex/p> ?o FILTER({c}) }}"),
                format!("SELECT ?s ({c} AS ?r) WHERE {{ ?s <http://ex/p> ?o }}"),
                format!("SELECT ?s ?r WHERE {{ ?s <http://ex/p> ?o BIND({c} AS ?r) }}"),
                format!("SELECT ?s WHERE {{ ?s <http://ex/p> ?o }} ORDER BY ({c})"),
                format!("ASK {{ ?s <http://ex/p> ?o FILTER(!{c} || {c}) }}"),
            ];
            for q in &queries {
                checked += 1;
                let outcome = catch_unwind(AssertUnwindSafe(|| db.query(q)));
                match outcome {
                    Err(_) => panics.push(q.clone()),
                    Ok(res) => {
                        if !spec.arity.accepts(n) {
                            match res {
                                Err(GStoreError::SparqlParse(msg)) => assert!(
                                    msg.contains(spec.name) && msg.contains("expects"),
                                    "{q}: unhelpful error {msg:?}"
                                ),
                                other => panic!("{q}: wrong arity must not parse, got {other:?}"),
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(panics.is_empty(), "queries that panicked: {panics:#?}");
    assert!(checked >= BUILTINS.len() * 5 * 5);
}

/// BOUND only takes a variable (SPARQL grammar `BOUND '(' Var ')'`).
#[test]
fn bound_requires_a_variable() {
    let mut db = Database::build_from_str("b", DATA).unwrap();
    let err = db
        .query("SELECT ?s WHERE { ?s <http://ex/p> ?o FILTER(BOUND(<http://ex/a>)) }")
        .unwrap_err();
    assert!(
        err.to_string().contains("BOUND expects a variable"),
        "{err}"
    );
    let ok = db
        .select("SELECT ?s WHERE { ?s <http://ex/p> ?o FILTER(BOUND(?o)) }")
        .unwrap();
    assert_eq!(ok.row_count(), 2);
}

/// Valid calls keep working (spot check across the table's shapes).
#[test]
fn well_formed_builtin_calls_still_evaluate() {
    let mut db = Database::build_from_str("ok", DATA).unwrap();
    let rs = db
        .select(
            "SELECT (STR(?o) AS ?a) (CONCAT() AS ?b) (COALESCE(?nope, 1) AS ?c) \
                    (REGEX(\"abc\", \"B\", \"i\") AS ?d) (SUBSTR(\"abc\", 2) AS ?e) \
                    (IF(true, 1, 2) AS ?f) (BNODE() AS ?g) \
             WHERE { <http://ex/a> <http://ex/p> ?o }",
        )
        .unwrap();
    assert_eq!(rs.row_count(), 1);
    let row = &rs.rows[0];
    assert_eq!(row[0].as_deref(), Some("\"http://ex/b\""));
    assert_eq!(row[1].as_deref(), Some("\"\""));
    assert!(row[2].as_deref().unwrap().starts_with("\"1\""));
    assert!(row[3].as_deref().unwrap().contains("true"));
    assert_eq!(row[4].as_deref(), Some("\"bc\""));
    assert!(row[5].as_deref().unwrap().starts_with("\"1\""));
    assert!(row[6].as_deref().unwrap().starts_with("_:"));
}

/// An AST built without the parser (so arity was never validated) must still
/// not panic the evaluator: a wrong-arity built-in is an evaluation error.
#[test]
fn hand_built_wrong_arity_ast_does_not_panic_the_evaluator() {
    let mut d = Dictionary::new();
    let a = d.intern_term(&Term::iri("http://ex/a"));
    let b = d.intern_term(&Term::iri("http://ex/b"));
    let p = d.intern_predicate(&Term::iri("http://ex/p").dict_key());
    let mut store = TripleStore::new();
    store.bulk_load(vec![IdTriple::new(a, p, b)]);

    let bgp = GraphPattern::Bgp(vec![TriplePattern {
        subject: PatternTerm::Var("s".into()),
        predicate: PatternTerm::Term(Term::iri("http://ex/p")),
        object: PatternTerm::Var("o".into()),
    }]);
    let mut panics = Vec::new();
    for spec in BUILTINS {
        for n in 0..=4 {
            let args: Vec<Expr> = (0..n)
                .map(|i| match i % 3 {
                    0 => Expr::Var("o".into()),
                    1 => Expr::Const(Term::iri("http://ex/a")),
                    _ => Expr::Var("unbound".into()),
                })
                .collect();
            let call = Expr::Builtin(spec.name.to_string(), args);
            let q = Query::Select(SelectQuery {
                distinct: false,
                projection: Projection::Items(vec![
                    SelectItem::Var("s".into()),
                    SelectItem::Expr(call.clone(), "r".into()),
                ]),
                pattern: GraphPattern::Filter(vec![call], Box::new(bgp.clone())),
                group_by: vec![],
                having: vec![],
                order_by: vec![],
                limit: None,
                offset: None,
            });
            let r = catch_unwind(AssertUnwindSafe(|| Evaluator::new(&d, &store).evaluate(&q)));
            match r {
                Err(_) => panics.push(format!("{}/{n}", spec.name)),
                Ok(Ok(QueryResult::Select(rs))) => {
                    if !spec.arity.accepts(n) {
                        assert!(
                            rs.is_empty(),
                            "{}/{n}: an invalid call filters out every row",
                            spec.name
                        );
                    }
                }
                Ok(other) => panic!("{}/{n}: unexpected {other:?}", spec.name),
            }
        }
    }
    assert!(panics.is_empty(), "built-ins that panicked: {panics:?}");
}

/// Other crafted-query panics found in the audit.
#[test]
fn crafted_queries_found_by_the_audit_do_not_panic() {
    let mut db = Database::build_from_str("audit", DATA).unwrap();
    let queries = [
        // Non-deterministic ORDER BY key: inconsistent comparator ⇒ sort panic.
        "SELECT ?s WHERE { ?s ?p ?o } ORDER BY (RAND())",
        "SELECT ?s (STRUUID() AS ?u) WHERE { ?s ?p ?o } ORDER BY DESC(?u)",
        // i64 overflow in SUBSTR's start + length.
        "SELECT (SUBSTR(\"abc\", 9e18, 9e18) AS ?x) WHERE { ?s ?p ?o }",
        // i64 overflow converting a huge-year dateTime to an instant.
        "SELECT ?s WHERE { ?s ?p ?o FILTER(\"999999999999999999-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime> > \"2000-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>) }",
        "SELECT ?s WHERE { ?s ?p ?o } ORDER BY (\"-9223372036854775807-01-01\"^^<http://www.w3.org/2001/XMLSchema#date>)",
        // Zero-arg substring-filter shapes reach the StringIndex pre-pass.
        "SELECT ?s WHERE { ?s ?p ?o FILTER(CONTAINS(STR(), \"abc\")) }",
    ];
    for q in queries {
        let r = catch_unwind(AssertUnwindSafe(|| db.query(q)));
        assert!(r.is_ok(), "panicked: {q}");
    }
}

// ---- 2. dictionary id-space bounds ----------------------------------------

/// The 2,000,000,000th entity must be refused, not handed a literal-range id
/// (`LITERAL_FIRST_ID`), which would flip its entity/literal classification.
#[test]
fn dictionary_refuses_entities_beyond_literal_first_id() {
    use gstore::dict::ENTITY_ID_LIMIT;
    use gstore::model::id::{is_entity_id, LITERAL_FIRST_ID};
    let mut d = Dictionary::new();
    d.debug_advance_id_counters(ENTITY_ID_LIMIT - 1, 0, 0)
        .unwrap();
    let last = d.try_intern_term(&Term::iri("http://ex/last")).unwrap();
    assert_eq!(last, LITERAL_FIRST_ID - 1);
    assert!(is_entity_id(last));
    let err = d
        .try_intern_term(&Term::iri("http://ex/one-too-many"))
        .unwrap_err();
    assert!(matches!(err, GStoreError::Database(_)), "{err:?}");
    assert!(err.to_string().contains("entity id space exhausted"));
    // Literals live in their own bounded space and still intern.
    let lit = d.try_intern_term(&Term::plain_literal("ok")).unwrap();
    assert_eq!(lit, LITERAL_FIRST_ID);
}

// ---- 3. LOAD policy --------------------------------------------------------

/// A scratch directory tree, removed on drop:
/// `root/allowed/{inside.nt, sub/deep.nt}` and `root/outside/secret.nt`.
struct Sandbox {
    root: std::path::PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Sandbox {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "gstore_load_policy_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("allowed/sub")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        let write = |rel: &str, name: &str| {
            let line = format!("<http://ex/{name}> <http://ex/p> \"{name}\" .\n");
            std::fs::write(root.join(rel), line).unwrap();
        };
        write("allowed/inside.nt", "in");
        write("allowed/sub/deep.nt", "deep");
        write("outside/secret.nt", "secret");
        Sandbox { root }
    }

    fn path(&self, rel: &str) -> String {
        self.root.join(rel).display().to_string()
    }

    fn allowed(&self) -> std::path::PathBuf {
        self.root.join("allowed")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn load(db: &mut Database, source: &str) -> gstore::Result<QueryResult> {
    db.query(&format!("LOAD <{source}>"))
}

fn assert_denied(r: gstore::Result<QueryResult>, what: &str) {
    match r {
        Err(GStoreError::Query(msg)) => {
            assert!(
                msg.contains("load policy"),
                "{what}: unexpected message {msg:?}"
            )
        }
        other => panic!("{what}: expected a policy denial, got {other:?}"),
    }
}

#[test]
fn default_policy_keeps_loading_any_local_file() {
    let sb = Sandbox::new("default");
    let mut db = Database::new("d");
    assert_eq!(db.load_policy(), &LoadPolicy::permissive());
    let r = load(&mut db, &format!("file://{}", sb.path("outside/secret.nt"))).unwrap();
    assert_eq!(r, QueryResult::Update { changed: 1 });
    // A bare path works too (historical behaviour).
    let r = load(&mut db, &sb.path("allowed/inside.nt")).unwrap();
    assert_eq!(r, QueryResult::Update { changed: 1 });
    // Remote sources are still unsupported (no network), with the old message.
    let err = load(&mut db, "http://example.org/x.ttl").unwrap_err();
    assert!(
        err.to_string().contains("not supported (no network)"),
        "{err}"
    );
}

#[test]
fn restricted_policy_denies_local_and_remote_loads() {
    let sb = Sandbox::new("restricted");
    let mut db = Database::new("r");
    db.set_load_policy(LoadPolicy::restricted());
    let inside = sb.path("allowed/inside.nt");
    assert_denied(load(&mut db, &format!("file://{inside}")), "file IRI");
    assert_denied(load(&mut db, &inside), "bare path");
    assert_denied(load(&mut db, "file:///etc/passwd"), "/etc/passwd");
    assert_denied(load(&mut db, "http://example.org/x.ttl"), "remote");
    assert_eq!(db.triple_num(), 0);
    // SILENT turns the denial into a no-op, per SPARQL semantics.
    let r = db.query(&format!("LOAD SILENT <{inside}>")).unwrap();
    assert_eq!(r, QueryResult::Update { changed: 0 });
}

#[test]
fn allowed_dir_policy_loads_inside_and_refuses_escapes() {
    let sb = Sandbox::new("dirs");
    let mut db = Database::new("a");
    db.set_load_policy(LoadPolicy::restricted().with_allowed_dir(sb.allowed()));

    // Inside the allowed directory (and its subdirectories): allowed.
    let r = load(&mut db, &format!("file://{}", sb.path("allowed/inside.nt"))).unwrap();
    assert_eq!(r, QueryResult::Update { changed: 1 });
    let r = load(&mut db, &sb.path("allowed/sub/deep.nt")).unwrap();
    assert_eq!(r, QueryResult::Update { changed: 1 });
    // `..` that stays inside is fine once canonicalized.
    let r = load(&mut db, &sb.path("allowed/sub/../inside.nt")).unwrap();
    assert_eq!(r, QueryResult::Update { changed: 0 }, "already loaded");

    // Outside, `../` escapes, directories, missing files: all refused alike.
    assert_denied(load(&mut db, &sb.path("outside/secret.nt")), "outside");
    let escape = sb.path("allowed/../outside/secret.nt");
    assert_denied(load(&mut db, &escape), "dot-dot escape");
    let deep_escape = sb.path("allowed/sub/../../outside/secret.nt");
    assert_denied(
        load(&mut db, &format!("file://{deep_escape}")),
        "dot-dot via file IRI",
    );
    assert_denied(load(&mut db, &sb.path("allowed/sub")), "a directory");
    assert_denied(
        load(&mut db, &sb.path("allowed/missing.nt")),
        "missing file",
    );
    assert_denied(load(&mut db, "/etc/passwd"), "/etc/passwd");
    assert_denied(load(&mut db, "http://example.org/x.ttl"), "remote");

    // Missing-vs-outside is indistinguishable to the client (no existence oracle).
    let a = load(&mut db, &sb.path("outside/secret.nt"))
        .unwrap_err()
        .to_string();
    let b = load(&mut db, &sb.path("outside/nope.nt"))
        .unwrap_err()
        .to_string();
    assert_eq!(a.replace("secret", "X"), b.replace("nope", "X"));

    let rs = db
        .select("SELECT ?s WHERE { ?s <http://ex/p> ?o }")
        .unwrap();
    assert_eq!(rs.row_count(), 2, "only the two allowed files were loaded");
}

#[cfg(unix)]
#[test]
fn allowed_dir_policy_refuses_symlink_escapes() {
    use std::os::unix::fs::symlink;
    let sb = Sandbox::new("symlink");
    // A symlinked file and a symlinked directory inside `allowed/` pointing out.
    symlink(
        sb.root.join("outside/secret.nt"),
        sb.root.join("allowed/link.nt"),
    )
    .unwrap();
    symlink(sb.root.join("outside"), sb.root.join("allowed/outdir")).unwrap();
    // A symlink that stays inside is fine.
    symlink(
        sb.root.join("allowed/inside.nt"),
        sb.root.join("allowed/alias.nt"),
    )
    .unwrap();

    let mut db = Database::new("s");
    db.set_load_policy(LoadPolicy::restricted().with_allowed_dir(sb.allowed()));
    assert_denied(
        load(&mut db, &sb.path("allowed/link.nt")),
        "file symlink escape",
    );
    assert_denied(
        load(&mut db, &sb.path("allowed/outdir/secret.nt")),
        "dir symlink escape",
    );
    let r = load(&mut db, &sb.path("allowed/alias.nt")).unwrap();
    assert_eq!(r, QueryResult::Update { changed: 1 });

    // An allowed directory given *through* a symlink is canonicalized too.
    symlink(sb.allowed(), sb.root.join("allowed_alias")).unwrap();
    let mut db2 = Database::new("s2");
    db2.set_load_policy(LoadPolicy::restricted().with_allowed_dir(sb.root.join("allowed_alias")));
    let r = load(&mut db2, &sb.path("allowed/inside.nt")).unwrap();
    assert_eq!(r, QueryResult::Update { changed: 1 });
    assert_denied(
        load(&mut db2, &sb.path("outside/secret.nt")),
        "outside via alias policy",
    );
}

// ---- 4. stable password hashing --------------------------------------------

/// The persisted `users.db` entry is the versioned PBKDF2-HMAC-SHA256 format,
/// and it verifies again after a reload (no dependence on `DefaultHasher`).
#[test]
fn users_db_stores_versioned_pbkdf2_hashes() {
    use gstore::http_users::{
        hash_password, verify_password, UserStore, PBKDF2_ITERATIONS, ROOT_USER,
    };
    let path =
        std::env::temp_dir().join(format!("gstore_hardening_users_{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut store = UserStore::open(&path, "r00t").unwrap();
        store
            .add("dave", "s3cret", std::collections::BTreeSet::new())
            .unwrap();
    }
    let text = std::fs::read_to_string(&path).unwrap();
    for line in text.lines() {
        let cred = line.split('\t').nth(1).unwrap();
        let parts: Vec<&str> = cred.split('$').collect();
        assert_eq!(parts[0], "pbkdf2-sha256", "{line}");
        let iterations = parts[1].parse::<u32>().unwrap();
        assert_eq!(iterations, PBKDF2_ITERATIONS);
        assert!(iterations >= 10_000, "work factor floor");
        assert_eq!(parts[2].len(), 32, "16-byte salt");
        assert_eq!(parts[3].len(), 64, "32-byte digest");
        assert!(!line.contains("s3cret") && !line.contains("r00t"));
    }
    let mut store = UserStore::open(&path, "ignored").unwrap();
    assert!(store.verify(ROOT_USER, "r00t"));
    assert!(store.verify("dave", "s3cret"));
    assert!(!store.verify("dave", "S3cret"));
    assert!(store.login("dave", "s3cret").is_some());
    assert!(store.login("dave", "nope").is_none());

    // The same scheme is available standalone, and is pinned by a published
    // PBKDF2-HMAC-SHA256 vector (P="password", S="salt", c=4096).
    assert!(verify_password(&hash_password("x"), "x"));
    assert!(verify_password(
        "pbkdf2-sha256$4096$73616c74$c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a",
        "password"
    ));
    let _ = std::fs::remove_file(&path);
}
