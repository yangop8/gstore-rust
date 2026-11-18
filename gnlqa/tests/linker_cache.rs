//! review cluster-nlqa#22: the linker's label lists are fetched in ORDER BY
//! pages (a capped index is a deterministic prefix, truncation is reported),
//! and cached on disk per KB fingerprint so a repeated one-shot question does
//! not rescan the store.

mod common;

use std::sync::Mutex;
use std::time::Duration;

use common::TempDir;
use gnlqa::{
    HashEmbedder, KbClient, Linker, LinkerCache, LinkerLabels, RdfTerm, SparqlAnswer, TermKind,
};

/// A KB with `labels` sorted entity labels, three predicates and two types,
/// serving LIMIT/OFFSET slices and COUNT queries, recording every query.
struct PagedKb {
    labels: usize,
    queries: Mutex<Vec<String>>,
}

impl PagedKb {
    fn new(labels: usize) -> PagedKb {
        PagedKb {
            labels,
            queries: Mutex::new(Vec::new()),
        }
    }
    fn queries(&self) -> Vec<String> {
        self.queries.lock().unwrap().clone()
    }
    fn label_queries(&self) -> Vec<String> {
        self.queries()
            .into_iter()
            .filter(|q| q.starts_with("SELECT DISTINCT"))
            .collect()
    }
}

fn number_after(q: &str, kw: &str) -> Option<usize> {
    let i = q.find(kw)? + kw.len();
    q[i..].split_whitespace().next()?.parse().ok()
}

fn uri(v: String) -> Option<RdfTerm> {
    Some(RdfTerm {
        kind: TermKind::Uri,
        value: v,
        datatype: None,
        lang: None,
    })
}

fn lit(v: String) -> Option<RdfTerm> {
    Some(RdfTerm {
        kind: TermKind::Literal,
        value: v,
        datatype: None,
        lang: None,
    })
}

impl KbClient for PagedKb {
    fn query(&self, q: &str) -> gnlqa::Result<SparqlAnswer> {
        self.queries.lock().unwrap().push(q.to_string());
        if q.contains("COUNT(*)") {
            let n = if q.contains("rdf-schema#label") {
                self.labels
            } else {
                2
            };
            return Ok(SparqlAnswer::Select {
                vars: vec!["n".into()],
                rows: vec![vec![lit(n.to_string())]],
            });
        }
        let off = number_after(q, "OFFSET ").unwrap_or(0);
        let lim = number_after(q, "LIMIT ").unwrap_or(usize::MAX);
        let slice = |n: usize| off.min(n)..off.saturating_add(lim).min(n);
        if q.starts_with("SELECT DISTINCT ?s ?l") {
            Ok(SparqlAnswer::Select {
                vars: vec!["s".into(), "l".into()],
                rows: slice(self.labels)
                    .map(|i| {
                        vec![
                            uri(format!("http://ex/e{i:06}")),
                            lit(format!("Entity {i}")),
                        ]
                    })
                    .collect(),
            })
        } else if q.starts_with("SELECT DISTINCT ?p") {
            Ok(SparqlAnswer::Select {
                vars: vec!["p".into()],
                rows: slice(3)
                    .map(|i| vec![uri(format!("http://ex/p{i}"))])
                    .collect(),
            })
        } else {
            Ok(SparqlAnswer::Select {
                vars: vec!["t".into()],
                rows: slice(2)
                    .map(|i| vec![uri(format!("http://ex/T{i}"))])
                    .collect(),
            })
        }
    }
}

#[test]
fn label_queries_are_paged_in_a_stable_order() {
    let kb = PagedKb::new(25_000);
    let l = Linker::build_from_kb(&kb, Box::new(HashEmbedder::new(64)), 0.0, Some(15_000)).unwrap();
    assert_eq!(l.sizes(), (15_000, 3, 2));
    let label_qs = kb.label_queries();
    assert!(
        label_qs.iter().all(|q| q.contains("ORDER BY")),
        "an unordered LIMIT indexes an arbitrary subset: {label_qs:?}"
    );
    let entity_pages = label_qs.iter().filter(|q| q.contains("?s ?l")).count();
    assert_eq!(
        entity_pages, 2,
        "10000 + the rest (one extra row detects more)"
    );

    let labels = LinkerLabels::fetch(&kb, Some(15_000), &["en"]).unwrap();
    assert!(labels.entities_truncated);
    assert_eq!(labels.entities.first().unwrap().0, "http://ex/e000000");
    assert_eq!(labels.entities.last().unwrap().0, "http://ex/e014999");
    assert!(!labels.predicates_truncated && !labels.types_truncated);
}

#[test]
fn a_kb_under_the_limit_is_indexed_completely() {
    let kb = PagedKb::new(500);
    let labels = LinkerLabels::fetch(&kb, Some(10_000), &["en"]).unwrap();
    assert_eq!(labels.entities.len(), 500);
    assert!(!labels.entities_truncated);
    assert_eq!(kb.label_queries().len(), 3, "one page each");
}

fn build(kb: &PagedKb, cache: &LinkerCache) -> Linker {
    Linker::build_cached(
        kb,
        Box::new(HashEmbedder::new(64)),
        0.0,
        Some(10_000),
        &["en"],
        "http://127.0.0.1:9000/sparql",
        cache,
    )
    .unwrap()
}

#[test]
fn a_cached_index_skips_the_full_store_scans() {
    let dir = TempDir::new("linker-cache");
    let cache = LinkerCache::new(dir.path());
    let first = PagedKb::new(1_234);
    assert_eq!(build(&first, &cache).sizes(), (1_234, 3, 2));
    assert_eq!(first.label_queries().len(), 3);

    let again = PagedKb::new(1_234);
    let l = build(&again, &cache);
    assert_eq!(l.sizes(), (1_234, 3, 2));
    let qs = again.queries();
    assert_eq!(qs.len(), 2, "only the two fingerprint counts: {qs:?}");
    assert!(qs.iter().all(|q| q.contains("COUNT(*)")));
}

#[test]
fn a_changed_kb_or_a_stale_entry_is_rebuilt() {
    let dir = TempDir::new("linker-cache-stale");
    let cache = LinkerCache::new(dir.path());
    build(&PagedKb::new(100), &cache);

    let grown = PagedKb::new(101);
    assert_eq!(build(&grown, &cache).sizes().0, 101);
    assert_eq!(grown.label_queries().len(), 3, "new fingerprint, new scan");

    let stale = PagedKb::new(101);
    build(&stale, &cache.clone().with_max_age(Duration::ZERO));
    assert_eq!(
        stale.label_queries().len(),
        3,
        "an expired entry is not reused"
    );
}

#[test]
fn a_corrupt_cache_file_is_ignored_and_replaced() {
    let dir = TempDir::new("linker-cache-corrupt");
    let cache = LinkerCache::new(dir.path());
    build(&PagedKb::new(10), &cache);
    for e in std::fs::read_dir(dir.path()).unwrap() {
        std::fs::write(e.unwrap().path(), b"{not json").unwrap();
    }
    let kb = PagedKb::new(10);
    assert_eq!(build(&kb, &cache).sizes().0, 10);
    assert_eq!(kb.label_queries().len(), 3);
    let kb2 = PagedKb::new(10);
    build(&kb2, &cache);
    assert_eq!(
        kb2.label_queries().len(),
        0,
        "the rewritten entry is reused"
    );
}

#[cfg(unix)]
#[test]
fn cache_files_are_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("linker-cache-perm");
    let sub = dir.path().join("gnlqa");
    build(&PagedKb::new(10), &LinkerCache::new(&sub));
    let dmode = std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777;
    assert_eq!(dmode, 0o700);
    for e in std::fs::read_dir(&sub).unwrap() {
        let mode = e.unwrap().metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[cfg(unix)]
#[test]
fn an_existing_cache_directory_keeps_its_permissions() {
    // review (r5-gnlqa reviewer): the cache used to chmod its directory to
    // 0700 even when it already existed, so GNLQA_LINKER_CACHE_DIR pointing
    // at a shared directory (or one owned by a project) silently changed
    // its permissions. Only a directory the cache creates is made private.
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("linker-cache-existing");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    build(&PagedKb::new(10), &LinkerCache::new(dir.path()));
    let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o755,
        "the cache changed an existing directory's mode"
    );
    for e in std::fs::read_dir(dir.path()).unwrap() {
        let mode = e.unwrap().metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the cache file itself stays private");
    }
}

#[cfg(unix)]
#[test]
fn a_cache_file_readable_by_others_is_not_trusted() {
    // review (r5-gnlqa reviewer): the cache only ever writes 0600 files, so
    // a cache file that grants group/other access was not written by this
    // user's gnlqa (e.g. planted in a shared GNLQA_LINKER_CACHE_DIR); its
    // labels would steer entity linking, so it is rebuilt, not reused.
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("linker-cache-planted");
    let cache = LinkerCache::new(dir.path());
    build(&PagedKb::new(10), &cache);
    for e in std::fs::read_dir(dir.path()).unwrap() {
        let p = e.unwrap().path();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let kb = PagedKb::new(10);
    assert_eq!(build(&kb, &cache).sizes().0, 10);
    assert_eq!(
        kb.label_queries().len(),
        3,
        "a cache file other users can read or write was reused"
    );
}
