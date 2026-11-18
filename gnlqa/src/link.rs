//! Entity / predicate / type linking — map a natural-language mention to KG URIs
//! by semantic similarity over label vector indexes (built from gStore), the
//! modern replacement for gAnswer's Lucene + edit-distance + paraphrase
//! dictionary. Candidates are later validated against gStore's neighborhood
//! (C7) and used to ground SPARQL generation (C8).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::embed::{Embedder, VectorIndex};
use crate::error::{Error, Result};
use crate::intent::{Mention, MentionKind};
use crate::kb::{sparql_escape_literal, KbClient, SparqlAnswer, TermKind};

const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The smallest similarity that counts as evidence of a match. A hit at or below
/// this (no shared token under [`HashEmbedder`], or an anti-correlated embedding)
/// is treated as "no candidate" rather than linked to an arbitrary entity.
const MIN_EVIDENCE: f32 = 1e-6;

/// What a linked candidate is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Entity,
    Type,
    Predicate,
    /// A literal value carried through unchanged (not linked to a URI).
    Literal,
}

/// A candidate KG term for a mention, with a similarity score.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The KG URI (empty for a literal).
    pub uri: String,
    /// The label/local-name that matched.
    pub label: String,
    pub kind: LinkKind,
    pub score: f32,
}

impl Candidate {
    /// SPARQL surface form: `<uri>` for a URI term, an escaped quoted literal
    /// otherwise (the literal value can be user text, so it must be escaped).
    pub fn to_term(&self) -> String {
        if self.kind == LinkKind::Literal || self.uri.is_empty() {
            format!("\"{}\"", sparql_escape_literal(&self.label))
        } else {
            format!("<{}>", self.uri)
        }
    }
}

/// The local name of a URI — the part after the last `#`, `/`, or `:`, ignoring
/// a single trailing `/`. Falls back to the whole string if there's no separator.
pub fn local_name(uri: &str) -> String {
    let trimmed = uri.strip_suffix('/').unwrap_or(uri);
    let cut = trimmed.rfind(['#', '/', ':']).map(|i| i + 1).unwrap_or(0);
    let name = &trimmed[cut..];
    if name.is_empty() {
        trimmed.to_string()
    } else {
        name.to_string()
    }
}

/// Linker holding one vector index per term kind.
pub struct Linker {
    embedder: Box<dyn Embedder>,
    entities: VectorIndex,
    predicates: VectorIndex,
    types: VectorIndex,
    /// Minimum cosine score for a candidate to be kept.
    min_score: f32,
}

impl Linker {
    /// Build from explicit `(uri, label)` lists (offline / testable).
    pub fn from_labels(
        embedder: Box<dyn Embedder>,
        entities: &[(String, String)],
        predicates: &[(String, String)],
        types: &[(String, String)],
        min_score: f32,
    ) -> Result<Linker> {
        let ent = VectorIndex::build(embedder.as_ref(), entities)?;
        let pred = VectorIndex::build(embedder.as_ref(), predicates)?;
        let typ = VectorIndex::build(embedder.as_ref(), types)?;
        Ok(Linker {
            embedder,
            entities: ent,
            predicates: pred,
            types: typ,
            min_score,
        })
    }

    /// Build the indexes by querying gStore for entity labels (`rdfs:label`),
    /// distinct predicates, and distinct types. Restricts labels to English /
    /// no-language tags. See [`build_from_kb_langs`](Self::build_from_kb_langs)
    /// for multilingual coverage.
    pub fn build_from_kb(
        kb: &dyn KbClient,
        embedder: Box<dyn Embedder>,
        min_score: f32,
        entity_limit: Option<usize>,
    ) -> Result<Linker> {
        Self::build_from_kb_langs(kb, embedder, min_score, entity_limit, &["en"])
    }

    /// Multilingual variant of [`build_from_kb`](Self::build_from_kb): index entity
    /// labels whose language tag is empty or in `languages`, so questions in those
    /// languages link against native labels rather than only English. Predicates
    /// and types are labeled by their URI local name (data rarely carries
    /// `rdfs:label` for them, and those names are language-neutral).
    ///
    /// The label, predicate and type lists are fetched in `ORDER BY` pages
    /// (see [`LinkerLabels::fetch`]), so a capped list is a deterministic
    /// prefix rather than an arbitrary subset, and the sizes and any
    /// truncation are logged (review cluster-nlqa#22).
    pub fn build_from_kb_langs(
        kb: &dyn KbClient,
        embedder: Box<dyn Embedder>,
        min_score: f32,
        entity_limit: Option<usize>,
        languages: &[&str],
    ) -> Result<Linker> {
        let labels = LinkerLabels::fetch(kb, entity_limit, languages)?;
        labels.log_summary(entity_limit);
        Linker::from_label_set(embedder, &labels, min_score)
    }

    /// Like [`build_from_kb_langs`](Self::build_from_kb_langs), but reuses the
    /// label lists cached on disk for the same KB fingerprint (see
    /// [`kb_fingerprint`] and [`LinkerCache`]): a repeated one-shot CLI
    /// question then costs two predicate-bound `COUNT` queries instead of
    /// full-store `DISTINCT` scans (review cluster-nlqa#22). A cache that
    /// cannot be written only costs the next build; it is logged, not fatal.
    pub fn build_cached(
        kb: &dyn KbClient,
        embedder: Box<dyn Embedder>,
        min_score: f32,
        entity_limit: Option<usize>,
        languages: &[&str],
        endpoint: &str,
        cache: &LinkerCache,
    ) -> Result<Linker> {
        let fp = kb_fingerprint(kb, endpoint, entity_limit, languages)?;
        if let Some(labels) = cache.load(&fp) {
            eprintln!(
                "gnlqa: linker: reusing the cached label index ({} entities, {} predicates, \
                 {} types)",
                labels.entities.len(),
                labels.predicates.len(),
                labels.types.len()
            );
            return Linker::from_label_set(embedder, &labels, min_score);
        }
        let labels = LinkerLabels::fetch(kb, entity_limit, languages)?;
        labels.log_summary(entity_limit);
        if let Err(e) = cache.store(&fp, &labels) {
            eprintln!("gnlqa: warning: cannot cache the linker index: {e}");
        }
        Linker::from_label_set(embedder, &labels, min_score)
    }

    /// Build from a fetched (or cached) [`LinkerLabels`].
    pub fn from_label_set(
        embedder: Box<dyn Embedder>,
        labels: &LinkerLabels,
        min_score: f32,
    ) -> Result<Linker> {
        Linker::from_labels(
            embedder,
            &labels.entities,
            &labels.predicates,
            &labels.types,
            min_score,
        )
    }

    /// Number of indexed (entities, predicates, types).
    pub fn sizes(&self) -> (usize, usize, usize) {
        (self.entities.len(), self.predicates.len(), self.types.len())
    }

    fn search(
        &self,
        index: &VectorIndex,
        kind: LinkKind,
        text: &str,
        k: usize,
    ) -> Result<Vec<Candidate>> {
        let hits = index.search_text(self.embedder.as_ref(), text, k)?;
        let mk = |h: crate::embed::Scored| Candidate {
            uri: h.id,
            label: h.text,
            kind,
            score: h.score,
        };
        // A zero (or negative) score means no lexical/semantic overlap at all —
        // there is no evidence, so it must never link (otherwise a mention with no
        // matching label deterministically links to the lexicographically-first
        // entity, feeding a bogus seed to GraphRAG / analytics). Keep the primary
        // threshold, but never below this evidence floor.
        let floor = self.min_score.max(MIN_EVIDENCE);
        let mut kept: Vec<Candidate> = hits
            .iter()
            .filter(|h| h.score >= floor)
            .cloned()
            .map(mk)
            .collect();
        // Relaxation (à la gAnswer): if the threshold pruned everything but there
        // was a best hit *with real evidence*, keep it so linking never hard-fails
        // on a genuine but low-scoring match — downstream (C7) validates against
        // the KG neighborhood. A zero-evidence best hit is still dropped.
        if kept.is_empty() {
            if let Some(best) = hits.into_iter().next() {
                if best.score >= MIN_EVIDENCE {
                    kept.push(mk(best));
                }
            }
        }
        Ok(kept)
    }

    /// Link a mention to its top-`k` candidates, routed by [`MentionKind`].
    pub fn link_mention(&self, mention: &Mention, k: usize) -> Result<Vec<Candidate>> {
        match mention.kind {
            MentionKind::Entity => self.search(&self.entities, LinkKind::Entity, &mention.text, k),
            MentionKind::Type => self.search(&self.types, LinkKind::Type, &mention.text, k),
            MentionKind::Literal => Ok(vec![Candidate {
                uri: String::new(),
                label: mention.text.clone(),
                kind: LinkKind::Literal,
                score: 1.0,
            }]),
        }
    }

    /// Link a relation phrase to its top-`k` predicate candidates.
    pub fn link_predicate(&self, phrase: &str, k: usize) -> Result<Vec<Candidate>> {
        self.search(&self.predicates, LinkKind::Predicate, phrase, k)
    }
}

/// Rows per page of the paged linker queries (well under the client's
/// [`MAX_RESULT_ROWS`](crate::kb::MAX_RESULT_ROWS)).
pub const LABEL_PAGE: usize = 10_000;

/// Cap on the indexed predicates and on the indexed types (each).
pub const MAX_SCHEMA_TERMS: usize = 100_000;

/// The `(uri, label)` lists a [`Linker`] indexes, as fetched from the KB —
/// also what [`LinkerCache`] stores.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkerLabels {
    pub entities: Vec<(String, String)>,
    pub predicates: Vec<(String, String)>,
    pub types: Vec<(String, String)>,
    /// Whether the entity labels were cut at the requested limit.
    pub entities_truncated: bool,
    /// Whether the predicates / types were cut at [`MAX_SCHEMA_TERMS`].
    pub predicates_truncated: bool,
    pub types_truncated: bool,
}

impl LinkerLabels {
    /// Fetch the entity labels (language-filtered, at most `entity_limit`),
    /// distinct predicates and distinct types, each in `ORDER BY` pages of
    /// [`LABEL_PAGE`] rows: a capped list is the same prefix on every build,
    /// and no single reply can hit the client's row cap.
    pub fn fetch(
        kb: &dyn KbClient,
        entity_limit: Option<usize>,
        languages: &[&str],
    ) -> Result<LinkerLabels> {
        let ent_base = format!(
            "SELECT DISTINCT ?s ?l WHERE {{ ?s <{RDFS_LABEL}> ?l FILTER({}) }}",
            lang_filter_clause(languages)
        );
        let mut ent_rows = Vec::new();
        let entities_truncated = fetch_paged(
            kb,
            &ent_base,
            "?s ?l",
            entity_limit.unwrap_or(usize::MAX),
            |ans| ent_rows.extend(pairs_with_label(ans, "s", "l")),
        )?;
        if ent_rows.is_empty() {
            eprintln!(
                "gnlqa: warning: entity label query returned no rows — linker will be sparse"
            );
        }
        ent_rows.truncate(entity_limit.unwrap_or(usize::MAX));

        let mut predicates = Vec::new();
        let predicates_truncated = fetch_paged(
            kb,
            "SELECT DISTINCT ?p WHERE { ?s ?p ?o }",
            "?p",
            MAX_SCHEMA_TERMS,
            |ans| predicates.extend(uris_by_local_name(ans, "p")),
        )?;
        predicates.truncate(MAX_SCHEMA_TERMS);

        let mut types = Vec::new();
        let types_truncated = fetch_paged(
            kb,
            &format!("SELECT DISTINCT ?t WHERE {{ ?s <{RDF_TYPE}> ?t }}"),
            "?t",
            MAX_SCHEMA_TERMS,
            |ans| types.extend(uris_by_local_name(ans, "t")),
        )?;
        types.truncate(MAX_SCHEMA_TERMS);

        Ok(LinkerLabels {
            entities: ent_rows,
            predicates,
            types,
            entities_truncated,
            predicates_truncated,
            types_truncated,
        })
    }

    /// Log the index sizes, and loudly any truncation (entities beyond the
    /// limit can never be linked).
    pub fn log_summary(&self, entity_limit: Option<usize>) {
        eprintln!(
            "gnlqa: linker: indexed {} entity labels, {} predicates, {} types",
            self.entities.len(),
            self.predicates.len(),
            self.types.len()
        );
        if self.entities_truncated {
            eprintln!(
                "gnlqa: warning: the KB has more than {} entity labels; only the first {} \
                 (in entity-IRI order) are indexed and the rest cannot be linked — raise \
                 GNLQA_LINKER_MAX_LABELS",
                entity_limit.unwrap_or(0),
                self.entities.len()
            );
        }
        if self.predicates_truncated || self.types_truncated {
            eprintln!(
                "gnlqa: warning: more than {MAX_SCHEMA_TERMS} distinct predicates or types; \
                 the linker indexes the first {MAX_SCHEMA_TERMS} of each"
            );
        }
    }
}

/// Run `base ORDER BY order LIMIT … OFFSET …` page by page, handing each page
/// to `collect`, until a short page or more than `limit` rows; returns
/// whether rows beyond `limit` exist (the caller trims to `limit`).
fn fetch_paged(
    kb: &dyn KbClient,
    base: &str,
    order: &str,
    limit: usize,
    mut collect: impl FnMut(&SparqlAnswer),
) -> Result<bool> {
    let wanted_total = limit.saturating_add(1); // one extra row detects truncation
    let mut got = 0usize;
    loop {
        let want = LABEL_PAGE.min(wanted_total - got);
        let q = format!("{base} ORDER BY {order} LIMIT {want} OFFSET {got}");
        let ans = kb.query(&q)?;
        let n = ans.row_count();
        collect(&ans);
        got += n;
        if got > limit {
            return Ok(true);
        }
        if n < want {
            return Ok(false);
        }
    }
}

/// A cheap fingerprint of what the linker index depends on: the endpoint,
/// the label languages and limit, and the number of `rdfs:label` and
/// `rdf:type` triples (two predicate-bound `COUNT`s — no full-store scan).
/// A [`LinkerCache`] entry is reused only for the same fingerprint (and
/// within its maximum age, which also bounds staleness from changes the
/// counts cannot see, such as a new predicate without labels or types).
pub fn kb_fingerprint(
    kb: &dyn KbClient,
    endpoint: &str,
    entity_limit: Option<usize>,
    languages: &[&str],
) -> Result<String> {
    let count = |q: String| -> Result<String> {
        let ans = kb.query(&q)?;
        Ok(ans
            .column_values("n")
            .into_iter()
            .next()
            .unwrap_or_default())
    };
    let labels = count(format!(
        "SELECT (COUNT(*) AS ?n) WHERE {{ ?s <{RDFS_LABEL}> ?l }}"
    ))?;
    let types = count(format!(
        "SELECT (COUNT(*) AS ?n) WHERE {{ ?s <{RDF_TYPE}> ?t }}"
    ))?;
    let key = format!(
        "linker-v1|{}|{}|{:?}|labels={labels}|types={types}",
        crate::error::redact_userinfo(endpoint),
        languages.join(","),
        entity_limit
    );
    // FNV-1a, 64 bit: stable across builds and platforms (unlike DefaultHasher).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!("{h:016x}"))
}

/// The on-disk cache of linker label lists, one JSON file per KB
/// fingerprint. Files are private to the user (mode 0600 on Unix, in a
/// 0700 directory when the cache creates it): the labels are KB data.
#[derive(Debug, Clone)]
pub struct LinkerCache {
    dir: PathBuf,
    max_age: Duration,
}

#[derive(Serialize, Deserialize)]
struct CachedLabels {
    format: u32,
    fingerprint: String,
    labels: LinkerLabels,
}

impl LinkerCache {
    /// Default maximum age of a cache entry (a day).
    pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(24 * 3600);

    pub fn new(dir: impl Into<PathBuf>) -> LinkerCache {
        LinkerCache {
            dir: dir.into(),
            max_age: Self::DEFAULT_MAX_AGE,
        }
    }

    /// Rebuild entries older than `max_age` even if the fingerprint matches.
    pub fn with_max_age(mut self, max_age: Duration) -> LinkerCache {
        self.max_age = max_age;
        self
    }

    /// The per-user default: `$XDG_CACHE_HOME/gnlqa`, else `$HOME/.cache/gnlqa`.
    pub fn default_dir(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
        let non_empty = |k: &str| var(k).filter(|v| !v.trim().is_empty());
        non_empty("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| non_empty("HOME").map(|h| Path::new(&h).join(".cache")))
            .map(|p| p.join("gnlqa"))
    }

    fn path(&self, fingerprint: &str) -> PathBuf {
        self.dir.join(format!("linker-{fingerprint}.json"))
    }

    /// The cached labels for `fingerprint`, if present, readable, current
    /// (younger than the maximum age), private and well-formed.
    ///
    /// On Unix only a file this user's gnlqa can have written is trusted:
    /// [`store`](Self::store) writes regular `0600` files owned by the
    /// process's effective user. A symbolic link, a file owned by another
    /// user (even with mode `0600`: a process running as root can read it)
    /// or one that grants group or other users any access was not written
    /// by it — for example, it was planted in a shared cache directory — and
    /// its labels would steer entity linking. The checks are made on the
    /// opened file, so a file swapped in between is refused too.
    pub fn load(&self, fingerprint: &str) -> Option<LinkerLabels> {
        use std::io::Read as _;
        let path = self.path(fingerprint);
        // The directory entry itself (not followed): a regular file, so
        // opening it neither follows a link nor blocks on a planted FIFO.
        let entry = std::fs::symlink_metadata(&path).ok()?;
        if !entry.file_type().is_file() {
            eprintln!(
                "gnlqa: warning: ignoring the linker cache file {}: it is not a regular file",
                path.display()
            );
            return None;
        }
        let mut file = std::fs::File::open(&path).ok()?;
        let meta = file.metadata().ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let why = if (meta.dev(), meta.ino()) != (entry.dev(), entry.ino()) {
                Some("it was replaced while it was opened".to_string())
            } else if meta.uid() != effective_uid() {
                Some(format!(
                    "it is owned by user {}, not by this process's user {}",
                    meta.uid(),
                    effective_uid()
                ))
            } else if meta.permissions().mode() & 0o077 != 0 {
                Some(
                    "it is accessible to other users (gnlqa writes these files with mode 0600)"
                        .to_string(),
                )
            } else {
                None
            };
            if let Some(why) = why {
                eprintln!(
                    "gnlqa: warning: ignoring the linker cache file {}: {why}",
                    path.display()
                );
                return None;
            }
        }
        let age = meta.modified().ok().and_then(|t| t.elapsed().ok())?;
        if age >= self.max_age {
            return None;
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).ok()?;
        let c: CachedLabels = serde_json::from_slice(&bytes).ok()?;
        let sane =
            |v: &[(String, String)]| v.iter().all(|(u, _)| crate::schema::checked_iri(u).is_ok());
        (c.format == 1
            && c.fingerprint == fingerprint
            && sane(&c.labels.entities)
            && sane(&c.labels.predicates)
            && sane(&c.labels.types))
        .then_some(c.labels)
    }

    /// Store `labels` under `fingerprint` (write to a temporary file, then
    /// rename, so a reader never sees a partial file).
    ///
    /// Directories the cache has to create are made `0700`; an existing
    /// directory (e.g. a `GNLQA_LINKER_CACHE_DIR` shared with other data)
    /// keeps its permissions — the cache files themselves are `0600`.
    pub fn store(&self, fingerprint: &str, labels: &LinkerLabels) -> Result<()> {
        let io =
            |e: std::io::Error| Error::Config(format!("linker cache {}: {e}", self.dir.display()));
        let mut mkdir = std::fs::DirBuilder::new();
        mkdir.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            mkdir.mode(0o700);
        }
        mkdir.create(&self.dir).map_err(io)?;
        let body = serde_json::to_vec(&CachedLabels {
            format: 1,
            fingerprint: fingerprint.to_string(),
            labels: labels.clone(),
        })?;
        let tmp = self
            .dir
            .join(format!(".linker-{fingerprint}.{}.tmp", std::process::id()));
        // A fresh file every time (O_EXCL): a leftover temporary file, or a
        // symlink planted in a shared directory, is removed — never followed
        // or reused with its old permissions.
        let _ = std::fs::remove_file(&tmp);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let written = opts.open(&tmp).and_then(|mut f| {
            use std::io::Write;
            f.write_all(&body)?;
            f.sync_all()
        });
        if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, self.path(fingerprint))) {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(e));
        }
        Ok(())
    }
}

/// The process's effective user id (what owns the files it creates).
#[cfg(unix)]
fn effective_uid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid takes no arguments, cannot fail and touches no memory
    // of the caller.
    unsafe { geteuid() }
}

/// Build a `FILTER` disjunction over language tags: always allows no-language
/// (`""`) labels, plus each requested code. Codes are sanitized to `[A-Za-z0-9-]`
/// (dropping anything else) so a resolved/config language can't inject into the
/// SPARQL FILTER; empties are skipped.
fn lang_filter_clause(languages: &[&str]) -> String {
    let mut clauses = vec!["lang(?l) = \"\"".to_string()];
    for l in languages {
        let code: String = l
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect::<String>()
            .to_lowercase();
        if !code.is_empty() {
            clauses.push(format!("lang(?l) = \"{code}\""));
        }
    }
    clauses.join(" || ")
}

/// Build de-duplicated `(uri, label)` pairs from a 2-column SELECT (label = the
/// literal value). Skips blank-node subjects and de-duplicates on the *(uri,
/// label)* pair — so every distinct label of a URI is indexed (e.g. both
/// `"Beijing"@en` and `"北京"@zh`), which is the whole point of multilingual
/// linking. The vector index supports several rows per URI.
fn pairs_with_label(ans: &SparqlAnswer, uri_var: &str, label_var: &str) -> Vec<(String, String)> {
    let SparqlAnswer::Select { vars, rows } = ans else {
        return Vec::new();
    };
    let (Some(ui), Some(li)) = (
        vars.iter().position(|v| v == uri_var),
        vars.iter().position(|v| v == label_var),
    ) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let (Some(Some(ut)), Some(Some(lt))) = (r.get(ui), r.get(li)) else {
            continue;
        };
        if ut.kind == TermKind::Bnode {
            continue;
        }
        if seen.insert((ut.value.clone(), lt.value.clone())) {
            out.push((ut.value.clone(), lt.value.clone()));
        }
    }
    out
}

/// Build `(uri, local_name)` pairs from a 1-column SELECT of URIs.
fn uris_by_local_name(ans: &SparqlAnswer, var: &str) -> Vec<(String, String)> {
    ans.column_values(var)
        .into_iter()
        .map(|u| {
            let ln = local_name(&u);
            (u, ln)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use crate::kb::{MockKb, RdfTerm, TermKind};

    fn ent(uri: &str, label: &str) -> (String, String) {
        (uri.to_string(), label.to_string())
    }

    fn linker() -> Linker {
        Linker::from_labels(
            Box::new(HashEmbedder::new(128)),
            &[
                ent("http://ex/Berlin", "Berlin"),
                ent("http://ex/Paris", "Paris capital France"),
            ],
            &[
                ent("http://ex/capitalOf", "capital of"),
                ent("http://ex/population", "population"),
            ],
            &[
                ent("http://ex/City", "City"),
                ent("http://ex/Country", "Country"),
            ],
            0.0,
        )
        .unwrap()
    }

    #[test]
    fn local_name_strips_to_last_segment() {
        assert_eq!(local_name("http://ex/Berlin"), "Berlin");
        assert_eq!(local_name("http://x#name"), "name");
        assert_eq!(local_name("bare"), "bare");
    }

    #[test]
    fn links_entity_type_predicate() {
        let l = linker();
        assert_eq!(l.sizes(), (2, 2, 2));
        let e = l
            .link_mention(
                &Mention {
                    text: "Berlin".into(),
                    kind: MentionKind::Entity,
                },
                1,
            )
            .unwrap();
        assert_eq!(e[0].uri, "http://ex/Berlin");
        assert_eq!(e[0].kind, LinkKind::Entity);

        let t = l
            .link_mention(
                &Mention {
                    text: "City".into(),
                    kind: MentionKind::Type,
                },
                1,
            )
            .unwrap();
        assert_eq!(t[0].uri, "http://ex/City");

        let p = l.link_predicate("capital of", 1).unwrap();
        assert_eq!(p[0].uri, "http://ex/capitalOf");
        assert_eq!(p[0].to_term(), "<http://ex/capitalOf>");
    }

    #[test]
    fn literal_mention_passes_through() {
        let l = linker();
        let lit = l
            .link_mention(
                &Mention {
                    text: "42".into(),
                    kind: MentionKind::Literal,
                },
                3,
            )
            .unwrap();
        assert_eq!(lit[0].kind, LinkKind::Literal);
        assert_eq!(lit[0].to_term(), "\"42\"");
    }

    #[test]
    fn min_score_relaxes_to_best_when_all_pruned() {
        let l = Linker::from_labels(
            Box::new(HashEmbedder::new(128)),
            &[ent("http://ex/Berlin", "Berlin Germany capital")],
            &[],
            &[],
            0.99, // very high threshold
        )
        .unwrap();
        // A partial match ("Germany" shares a token with the label) scores below
        // the 0.99 threshold but has real evidence, so relaxation keeps it.
        let e = l
            .link_mention(
                &Mention {
                    text: "Germany".into(),
                    kind: MentionKind::Entity,
                },
                5,
            )
            .unwrap();
        assert_eq!(e.len(), 1);
        assert!(e[0].score > 0.0 && e[0].score < 0.99);
    }

    #[test]
    fn zero_evidence_mention_yields_no_candidate() {
        // No shared token with any label → no evidence → NO link (not the
        // lexicographically-first entity). Guards GraphRAG/analytics from bogus
        // seeds. A large bucket space keeps this deterministic (no hash-collision
        // between the distinctive mention token and the label tokens).
        let l = Linker::from_labels(
            Box::new(HashEmbedder::new(1 << 20)),
            &[
                ent("http://ex/Aardvark", "aardvark"),
                ent("http://ex/Berlin", "berlin"),
            ],
            &[],
            &[],
            0.0, // the shipped default: even here a zero-overlap hit must be dropped
        )
        .unwrap();
        let e = l
            .link_mention(
                &Mention {
                    text: "zzqxwv".into(),
                    kind: MentionKind::Entity,
                },
                5,
            )
            .unwrap();
        assert!(
            e.is_empty(),
            "zero-overlap mention must not link to an arbitrary entity"
        );
    }

    #[test]
    fn literal_to_term_escapes_quotes() {
        let l = linker();
        let c = l
            .link_mention(
                &Mention {
                    text: "say \"hi\"\nbye".into(),
                    kind: MentionKind::Literal,
                },
                1,
            )
            .unwrap();
        // no raw quote/newline can break out of the SPARQL literal
        assert_eq!(c[0].to_term(), "\"say \\\"hi\\\"\\nbye\"");
    }

    #[test]
    fn local_name_handles_urn_and_trailing_slash() {
        assert_eq!(local_name("http://ex/Berlin/"), "Berlin");
        assert_eq!(local_name("urn:isbn:0451450523"), "0451450523");
    }

    #[test]
    fn build_from_kb_runs_label_queries() {
        fn uri(v: &str) -> Option<RdfTerm> {
            Some(RdfTerm {
                kind: TermKind::Uri,
                value: v.into(),
                datatype: None,
                lang: None,
            })
        }
        fn lit(v: &str) -> Option<RdfTerm> {
            Some(RdfTerm {
                kind: TermKind::Literal,
                value: v.into(),
                datatype: None,
                lang: None,
            })
        }
        // Answers in query order: entities (?s ?l), predicates (?p), types (?t).
        let entities = SparqlAnswer::Select {
            vars: vec!["s".into(), "l".into()],
            rows: vec![vec![uri("http://ex/Berlin"), lit("Berlin")]],
        };
        let predicates = SparqlAnswer::Select {
            vars: vec!["p".into()],
            rows: vec![vec![uri("http://ex/capitalOf")]],
        };
        let types = SparqlAnswer::Select {
            vars: vec!["t".into()],
            rows: vec![vec![uri("http://ex/City")]],
        };
        let kb = MockKb::new(vec![entities, predicates, types]);
        let l = Linker::build_from_kb(&kb, Box::new(HashEmbedder::new(128)), 0.0, None).unwrap();
        assert_eq!(l.sizes(), (1, 1, 1));
        let p = l.link_predicate("capitalOf", 1).unwrap();
        assert_eq!(p[0].uri, "http://ex/capitalOf"); // local-name label matched
    }

    #[test]
    fn lang_filter_always_includes_empty_and_sanitizes() {
        let c = lang_filter_clause(&["en", "zh"]);
        assert!(c.contains(r#"lang(?l) = """#)); // no-language always allowed
        assert!(c.contains(r#"lang(?l) = "en""#));
        assert!(c.contains(r#"lang(?l) = "zh""#));
        // injection attempt is stripped to safe chars ([A-Za-z0-9-]) and
        // case-normalized, collapsing to one harmless code — no query structure
        // (quotes/braces/UNION/spaces) survives to break out of the FILTER.
        let evil = lang_filter_clause(&[r#"en") } UNION { ?s ?p ?o . FILTER("#]);
        assert!(evil.contains(r#"lang(?l) = "enunionspofilter""#));
        assert!(!evil.contains('}') && !evil.contains("UNION") && !evil.contains(" ?"));
    }

    #[test]
    fn build_from_kb_langs_filters_by_language() {
        fn uri(v: &str) -> Option<RdfTerm> {
            Some(RdfTerm {
                kind: TermKind::Uri,
                value: v.into(),
                datatype: None,
                lang: None,
            })
        }
        fn lit_lang(v: &str, l: &str) -> Option<RdfTerm> {
            Some(RdfTerm {
                kind: TermKind::Literal,
                value: v.into(),
                datatype: None,
                lang: Some(l.into()),
            })
        }
        // Entity labels in Chinese; the ?s ?l answer is what our FILTER'd query
        // would return. Ordering: entities, predicates, types.
        let entities = SparqlAnswer::Select {
            vars: vec!["s".into(), "l".into()],
            rows: vec![vec![uri("http://ex/Beijing"), lit_lang("北京", "zh")]],
        };
        let predicates = SparqlAnswer::Select {
            vars: vec!["p".into()],
            rows: vec![],
        };
        let types = SparqlAnswer::Select {
            vars: vec!["t".into()],
            rows: vec![],
        };
        let kb = MockKb::new(vec![entities, predicates, types]);
        let l =
            Linker::build_from_kb_langs(&kb, Box::new(HashEmbedder::new(128)), 0.0, None, &["zh"])
                .unwrap();
        assert_eq!(l.sizes().0, 1); // the Chinese-labeled entity was indexed
        let hits = l
            .link_mention(
                &Mention {
                    text: "北京".into(),
                    kind: MentionKind::Entity,
                },
                1,
            )
            .unwrap();
        assert_eq!(hits[0].uri, "http://ex/Beijing");
    }
}
