//! The full question-answering pipeline (C10): understand → link → ground →
//! generate N candidates → self-repair each → rank → answer. This is the
//! multi-candidate, data-driven disambiguation gAnswer pioneered, here driven by
//! the LLM and validated against gStore.

use crate::error::{Error, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::generate::generate_candidates;
use crate::ground::{explain, gather_citations};
use crate::intent::{extract_intent, QType, QuestionIntent};
use crate::kb::KbClient;
use crate::link::{LinkKind, Linker};
use crate::llm::{LlmClient, LlmRequest};
use crate::pipeline::{answer_values, render_answer, Answer, Provenance};
use crate::repair::{is_empty_answer, solve_with_repair, RepairOutcome};
use crate::schema::SchemaContext;

/// Score a solved outcome for ranking: a non-empty answer dominates an empty
/// one; each repair round is a small penalty (prefer queries that worked sooner).
/// The penalty is clamped so a non-empty answer always outranks an empty one,
/// regardless of `max_rounds`. This is a *ranking* heuristic reused as a rough
/// confidence — it is NOT a calibrated correctness probability.
pub fn score_outcome(o: &RepairOutcome) -> f32 {
    let base = if is_empty_answer(&o.answer) { 0.1 } else { 1.0 };
    let penalty = (0.05 * o.rounds as f32).min(0.89);
    base - penalty
}

/// Order-preserving de-duplication of URIs.
fn dedup(uris: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    uris.into_iter()
        .filter(|u| seen.insert(u.clone()))
        .collect()
}

/// Solve every candidate (with repair) and return the highest-scoring outcome.
/// A per-candidate query/repair failure is skipped (try the next), but a
/// transport failure ([`Error::Http`] — the endpoint is unreachable) or an
/// exhausted cost budget ([`Error::Limit`]) is fatal and propagated so the caller reports the outage instead of masking it as "no
/// candidate produced an answer".
#[allow(clippy::too_many_arguments)]
pub fn best_of(
    llm: &dyn LlmClient,
    kb: &dyn KbClient,
    question: &str,
    links: &str,
    schema: &str,
    candidates: &[String],
    max_rounds: usize,
    model: Option<&str>,
) -> Result<Option<RepairOutcome>> {
    let mut best: Option<(RepairOutcome, f32)> = None;
    for c in candidates {
        match solve_with_repair(llm, kb, question, links, schema, c, max_rounds, model) {
            Ok(o) => {
                let s = score_outcome(&o);
                if best.as_ref().is_none_or(|(_, bs)| s > *bs) {
                    best = Some((o, s));
                }
            }
            // Endpoint unreachable or the request's cost budget is spent — no
            // point trying the remaining candidates.
            Err(e) if e.is_fatal() => return Err(e),
            // Invalid/unanswerable for this candidate — try the next one.
            Err(_) => {}
        }
    }
    Ok(best.map(|(o, _)| o))
}

/// Default byte budget of the answer cache (review cluster-nlqa#19).
pub const DEFAULT_CACHE_MAX_BYTES: usize = 64 << 20;

/// Occupancy of the answer cache (see [`SolveEngine::cache_usage`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheUsage {
    pub entries: usize,
    /// Estimated bytes held (see [`answer_bytes`]).
    pub bytes: usize,
    pub max_entries: usize,
    pub max_bytes: usize,
}

/// A rough estimate of the heap bytes an [`Answer`] keeps alive (strings plus
/// a per-allocation overhead) — what the cache's byte bound counts.
pub fn answer_bytes(a: &Answer) -> usize {
    const PER_STRING: usize = 32;
    let s = |t: &str| t.len() + PER_STRING;
    let mut n = 256 + s(&a.text);
    n += a.sparql.as_deref().map_or(0, s);
    n += a.explanation.as_deref().map_or(0, s);
    n += a.values.iter().map(|v| s(v)).sum::<usize>();
    n += a
        .citations
        .iter()
        .map(|c| s(&c.subject) + s(&c.predicate) + s(&c.object))
        .sum::<usize>();
    n
}

/// A small bounded answer cache (FIFO eviction). gNLQA does not mutate the KB,
/// but the KB may change externally, so entries can carry a TTL (staleness bound)
/// and the whole cache can be cleared (e.g. on a chat `:reset`).
///
/// Bounded by entry count **and** by bytes (review cluster-nlqa#19): an answer
/// larger than an eighth of the byte budget is not cached at all, and the
/// oldest entries are evicted while the total exceeds the budget. Expired
/// entries are dropped, not just skipped, so they stop holding memory.
struct Cache {
    cap: usize,
    max_bytes: usize,
    bytes: usize,
    /// Optional entry lifetime; `None` = no expiry (bounded only by capacity).
    ttl: Option<Duration>,
    /// key → (answer, inserted at, estimated bytes)
    map: HashMap<String, (Answer, Instant, usize)>,
    /// Keys in insertion order (oldest first), so expired entries are at the front.
    order: VecDeque<String>,
}

impl Cache {
    fn new(cap: usize) -> Cache {
        Cache {
            cap: cap.max(1),
            max_bytes: DEFAULT_CACHE_MAX_BYTES,
            bytes: 0,
            ttl: None,
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }
    fn expired(&self, at: Instant, now: Instant) -> bool {
        self.ttl
            .is_some_and(|ttl| now.saturating_duration_since(at) > ttl)
    }
    fn remove(&mut self, k: &str) {
        if let Some((_, _, b)) = self.map.remove(k) {
            self.bytes -= b;
            if let Some(i) = self.order.iter().position(|o| o == k) {
                self.order.remove(i);
            }
        }
    }
    fn get(&mut self, k: &str) -> Option<Answer> {
        self.get_at(k, Instant::now())
    }
    fn get_at(&mut self, k: &str, now: Instant) -> Option<Answer> {
        let (v, at, _) = self.map.get(k)?;
        // An expired entry is a miss and is dropped, so a long-running server
        // neither serves pre-update answers nor keeps holding their memory.
        if self.expired(*at, now) {
            self.remove(k);
            return None;
        }
        Some(v.clone())
    }
    fn put(&mut self, k: String, v: Answer) {
        self.put_at(k, v, Instant::now())
    }
    fn put_at(&mut self, k: String, v: Answer, now: Instant) {
        self.remove(&k); // a refresh goes to the back with its new size
        let size = answer_bytes(&v) + k.len();
        if size > self.max_bytes / 8 {
            return; // too big to be worth a slot: never cached
        }
        // drop expired entries (oldest first)
        while let Some(front) = self.order.front() {
            match self.map.get(front) {
                Some((_, at, _)) if self.expired(*at, now) => {
                    let f = front.clone();
                    self.remove(&f);
                }
                _ => break,
            }
        }
        self.bytes += size;
        self.map.insert(k.clone(), (v, now, size));
        self.order.push_back(k);
        while self.map.len() > self.cap || self.bytes > self.max_bytes {
            let Some(old) = self.order.front().cloned() else {
                break;
            };
            self.remove(&old);
        }
    }
    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
    }
    fn usage(&self) -> CacheUsage {
        CacheUsage {
            entries: self.map.len(),
            bytes: self.bytes,
            max_entries: self.cap,
            max_bytes: self.max_bytes,
        }
    }
}

/// Which answering path(s) the user allows for a question. This is a
/// privacy/trust control as much as a routing one (see [`Provenance`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Structured SPARQL, then a GraphRAG fallback; never pure LLM. (default)
    #[default]
    Auto,
    /// Structured SPARQL / graph-analytics only — the **result data** (answer
    /// rows, supporting triples) stays on the machine; abstain if it can't be
    /// answered that way. To *build* the query the LLM still receives the
    /// question, the linked entity/type/predicate URIs and their labels,
    /// small schema samples (predicates and types around the linked terms)
    /// and, in the repair loop, gStore's error text for a failing query and
    /// whether a query returned no rows (review cluster-nlqa#21). The
    /// follow-up rewrite and [`SolveEngine::with_explain`] send no KB answer
    /// in this mode.
    Structured,
    /// Retrieve a private subgraph from gStore and let the LLM answer from it.
    GraphRag,
    /// Answer purely from the LLM's general knowledge; ignore the KB entirely.
    Open,
}

impl Mode {
    /// Parse a mode name (for the `/mode` command / `GNLQA_MODE`).
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_lowercase().as_str() {
            "auto" => Some(Mode::Auto),
            "structured" | "sparql" | "strict" => Some(Mode::Structured),
            "graphrag" | "rag" | "local" => Some(Mode::GraphRag),
            "open" | "llm" | "direct" => Some(Mode::Open),
            _ => None,
        }
    }
    /// Lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Structured => "structured",
            Mode::GraphRag => "graphrag",
            Mode::Open => "open",
        }
    }
}

/// The full QA engine: LLM + KB + (optional) linker.
pub struct SolveEngine {
    llm: Box<dyn LlmClient>,
    kb: Box<dyn KbClient>,
    linker: OnceLock<Linker>,
    /// How to (re)build the linker when it is missing (see
    /// [`with_linker_builder`](Self::with_linker_builder)).
    linker_source: Option<LinkerSource>,
    candidates: usize,
    max_rounds: usize,
    sample: usize,
    link_k: usize,
    cite: bool,
    explain: bool,
    abstain_below: f32,
    model: Option<String>,
    fast_model: Option<String>,
    cache: Option<Mutex<Cache>>,
    graphrag: bool,
    rag: crate::graphrag::RetrievalCfg,
    analytics: bool,
    analytics_cfg: crate::analytics::AnalyticsCfg,
    progress: bool,
    /// The mode used by [`ask`](Self::ask) (and every server surface that calls
    /// it: HTTP `/ask`, `/gSolve`, MCP `ask_kg`). Defaults to [`Mode::Auto`].
    default_mode: Mode,
}

/// A deferred linker build that is retried until it succeeds (review
/// cluster-nlqa#16: a KB that is unreachable at startup no longer leaves the
/// engine without a linker for its whole life, silently).
struct LinkerSource {
    build: Box<dyn Fn() -> Result<Linker> + Send + Sync>,
    retry_every: Duration,
    /// When the last failed attempt started; the mutex also serializes builds.
    last_failure: Mutex<Option<Instant>>,
}

/// Heuristic confidence for a GraphRAG answer: plausible-from-context, but not
/// KB-verified the way a SELECT result is. Kept below the structured-answer band.
const RAG_CONFIDENCE: f32 = 0.5;

/// Confidence for a graph-analytics answer: a deterministic computation, but over
/// a capped edge sample — high, not certain.
const ANALYTICS_CONFIDENCE: f32 = 0.85;

impl SolveEngine {
    pub fn new(llm: Box<dyn LlmClient>, kb: Box<dyn KbClient>) -> SolveEngine {
        SolveEngine {
            // Every LLM call and KB query the engine makes is charged against
            // the per-request budget a server surface installs (review
            // cluster-nlqa#13); with no budget installed this is a pass-through.
            llm: Box::new(crate::budget::MeteredLlm(llm)),
            kb: Box::new(crate::budget::MeteredKb(kb)),
            linker: OnceLock::new(),
            linker_source: None,
            candidates: 4,
            max_rounds: 2,
            sample: 30,
            link_k: 3,
            cite: true,
            explain: false,
            abstain_below: 0.0,
            model: None,
            fast_model: None,
            cache: None,
            graphrag: true,
            rag: crate::graphrag::RetrievalCfg::default(),
            analytics: true,
            analytics_cfg: crate::analytics::AnalyticsCfg::default(),
            progress: false,
            default_mode: Mode::Auto,
        }
    }
    /// Emit short stage logs to stderr (what's executing: model vs gStore), so an
    /// interactive user has an explicit "waiting target". Off by default.
    pub fn with_progress(mut self, on: bool) -> SolveEngine {
        self.progress = on;
        self
    }

    /// Emit a progress note when enabled.
    fn note(&self, msg: &str) {
        if self.progress {
            eprintln!("[gnlqa] {msg}");
        }
    }
    /// Enable/disable graph-analytics routing for `analytics`-typed questions
    /// (shortest path, centrality, PageRank, communities, triangles; default on).
    pub fn with_analytics(mut self, on: bool) -> SolveEngine {
        self.analytics = on;
        self
    }
    /// Cap on edges pulled from the KB for an analytics run.
    pub fn with_analytics_max_edges(mut self, max_edges: usize) -> SolveEngine {
        self.analytics_cfg.max_edges = max_edges.max(1);
        self
    }
    /// Enable/disable the GraphRAG fallback for open questions and empty
    /// structured results (default on).
    pub fn with_graphrag(mut self, on: bool) -> SolveEngine {
        self.graphrag = on;
        self
    }
    /// BFS depth for GraphRAG subgraph retrieval (≥1).
    pub fn with_rag_hops(mut self, hops: usize) -> SolveEngine {
        self.rag.hops = hops.max(1);
        self
    }
    /// Cheaper model for simple questions (factoid/list/count/boolean); complex
    /// ones (analytics/path/compare/open) still use the primary model.
    pub fn with_fast_model(mut self, model: impl Into<String>) -> SolveEngine {
        self.fast_model = Some(model.into());
        self
    }
    /// Enable a bounded answer cache of `capacity` questions (0 disables).
    pub fn with_cache(mut self, capacity: usize) -> SolveEngine {
        self.cache = (capacity > 0).then(|| Mutex::new(Cache::new(capacity)));
        self
    }

    /// Bound the answer cache by estimated bytes (default
    /// [`DEFAULT_CACHE_MAX_BYTES`]); an answer above an eighth of it is not
    /// cached. Has no effect without [`with_cache`](Self::with_cache).
    pub fn with_cache_max_bytes(self, max_bytes: usize) -> SolveEngine {
        if let Some(c) = &self.cache {
            c.lock().unwrap_or_else(|e| e.into_inner()).max_bytes = max_bytes.max(1);
        }
        self
    }

    /// The answer cache's occupancy (`None` if caching is off).
    pub fn cache_usage(&self) -> Option<CacheUsage> {
        self.cache
            .as_ref()
            .map(|c| c.lock().unwrap_or_else(|e| e.into_inner()).usage())
    }

    /// Give cached answers a time-to-live in seconds (0 = no expiry). Bounds
    /// staleness when the KB can change under a long-running server.
    pub fn with_cache_ttl(self, secs: u64) -> SolveEngine {
        if secs > 0 {
            if let Some(c) = &self.cache {
                c.lock().unwrap_or_else(|e| e.into_inner()).ttl = Some(Duration::from_secs(secs));
            }
        }
        self
    }

    /// The default answering mode for [`ask`](Self::ask) (and the server surfaces
    /// that call it). See [`Mode`].
    pub fn with_mode(mut self, mode: Mode) -> SolveEngine {
        self.default_mode = mode;
        self
    }

    /// Drop all cached answers (e.g. on a chat `:reset`, or after the KB changed).
    pub fn clear_cache(&self) {
        if let Some(c) = &self.cache {
            c.lock().unwrap_or_else(|e| e.into_inner()).clear();
        }
    }

    /// Pick the model for generation/repair based on question difficulty.
    fn route_model(&self, intent: &QuestionIntent) -> Option<String> {
        let hard = matches!(
            intent.qtype,
            QType::Analytics | QType::Path | QType::Compare | QType::Open
        );
        if hard {
            self.model.clone()
        } else {
            self.fast_model.clone().or_else(|| self.model.clone())
        }
    }
    /// Abstain (return a low-confidence "not sure" answer) when the winning
    /// outcome's confidence is below `threshold` (default 0.0 = never abstain).
    pub fn with_abstain_below(mut self, threshold: f32) -> SolveEngine {
        self.abstain_below = threshold;
        self
    }
    /// Attach supporting triples to the answer (default on).
    pub fn with_citations(mut self, on: bool) -> SolveEngine {
        self.cite = on;
        self
    }
    /// Have the LLM phrase a grounded natural-language answer (default off).
    ///
    /// Data-egress note: enabling this sends the answer's supporting **KB triples**
    /// to the LLM (to phrase the explanation) on the `auto` structured path,
    /// whose provenance is [`Provenance::GStore`](crate::pipeline::Provenance::GStore).
    /// [`Mode::Structured`] never does: its "result data stays local"
    /// guarantee wins and the explanation is skipped (review cluster-nlqa#21).
    pub fn with_explain(mut self, on: bool) -> SolveEngine {
        self.explain = on;
        self
    }
    pub fn with_linker(mut self, linker: Linker) -> SolveEngine {
        self.linker = OnceLock::from(linker);
        self
    }

    /// Build the linker lazily with `build`: on first use and, after a
    /// failure, again on a later request once `retry_every` has passed. Each
    /// failure is logged. The build runs outside any request budget (it
    /// serves every later request) and at most one runs at a time; requests
    /// that arrive meanwhile proceed without a linker.
    pub fn with_linker_builder(
        mut self,
        retry_every: Duration,
        build: impl Fn() -> Result<Linker> + Send + Sync + 'static,
    ) -> SolveEngine {
        self.linker_source = Some(LinkerSource {
            build: Box::new(build),
            retry_every,
            last_failure: Mutex::new(None),
        });
        self
    }

    /// Try to make the linker available now (e.g. at server start, so the
    /// first request does not pay for the build). Returns whether it is.
    pub fn prepare_linker(&self) -> bool {
        self.linker().is_some()
    }
    pub fn with_model(mut self, model: impl Into<String>) -> SolveEngine {
        self.model = Some(model.into());
        self
    }
    pub fn candidates(mut self, n: usize) -> SolveEngine {
        self.candidates = n.max(1);
        self
    }
    pub fn with_max_rounds(mut self, n: usize) -> SolveEngine {
        self.max_rounds = n;
        self
    }
    pub fn with_sample(mut self, n: usize) -> SolveEngine {
        self.sample = n.max(1);
        self
    }
    /// Top-k candidates to retrieve per linked mention/relation.
    pub fn with_link_k(mut self, k: usize) -> SolveEngine {
        self.link_k = k.max(1);
        self
    }

    /// The KB client, for direct SPARQL / analytics access (e.g. the MCP server).
    pub fn kb(&self) -> &dyn KbClient {
        self.kb.as_ref()
    }
    /// The entity linker, if one is configured or a deferred build (see
    /// [`with_linker_builder`](Self::with_linker_builder)) has succeeded —
    /// this call may run that build.
    pub fn linker(&self) -> Option<&Linker> {
        if let Some(l) = self.linker.get() {
            return Some(l);
        }
        let src = self.linker_source.as_ref()?;
        // One build at a time; a concurrent caller goes on without a linker.
        // A build that panicked leaves the lock poisoned, not the engine
        // without a linker for good: its state is still a valid timestamp.
        let mut last = match src.last_failure.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        };
        if let Some(l) = self.linker.get() {
            return Some(l); // built while we waited for the lock
        }
        if last.is_some_and(|t| t.elapsed() < src.retry_every) {
            return None;
        }
        let started = Instant::now();
        match crate::budget::unmetered(|| (src.build)()) {
            Ok(l) => {
                let _ = self.linker.set(l);
                *last = None;
                self.linker.get()
            }
            Err(e) => {
                *last = Some(started);
                eprintln!(
                    "gnlqa: warning: building the entity linker failed (answers are not \
                     grounded in the KB vocabulary until it succeeds; retrying after {} s): {e}",
                    src.retry_every.as_secs()
                );
                None
            }
        }
    }
    /// The analytics retrieval configuration.
    pub fn analytics_cfg(&self) -> crate::analytics::AnalyticsCfg {
        self.analytics_cfg
    }

    /// Answer a question end-to-end in the engine's [`default_mode`](Self::with_mode)
    /// (cache-aware). Used by every server surface (HTTP `/ask`, `/gSolve`, MCP
    /// `ask_kg`), so `GNLQA_MODE` applies there too — not just in `chat`.
    pub fn ask(&self, question: &str) -> Result<Answer> {
        self.ask_with_mode(question, self.default_mode)
    }

    /// Answer a question, constraining which path(s) are allowed (cache-aware).
    /// The cache key includes the mode, so the same question in different modes
    /// doesn't collide.
    pub fn ask_with_mode(&self, question: &str, mode: Mode) -> Result<Answer> {
        let key = format!("{}\u{1f}{}", mode.name(), question.trim());
        if let Some(cache) = &self.cache {
            if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
                return Ok(hit);
            }
        }
        let answer = self.solve_inner(question, mode)?;
        // Don't cache abstentions (a borderline "not sure" shouldn't be sticky).
        if let Some(cache) = &self.cache {
            if !answer.abstained {
                let val = answer.clone(); // clone outside the lock
                cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .put(key, val);
            }
        }
        Ok(answer)
    }

    /// Rewrite a follow-up into a standalone question using prior `(question,
    /// answer)` turns (routed to the fast model), falling back to the raw
    /// question on any error. Exposed so a [`Session`](crate::session::Session)
    /// can record the *resolved* question rather than the elliptical one.
    pub fn rewrite_followup_question(
        &self,
        history: &[(String, String)],
        question: &str,
    ) -> String {
        let m = self.fast_model.clone().or_else(|| self.model.clone());
        crate::session::rewrite_followup(self.llm.as_ref(), history, question, m.as_deref())
            .unwrap_or_else(|_| question.to_string())
    }

    /// Answer a follow-up given prior `(question, answer)` turns: rewrite the
    /// (possibly elliptical) question into a standalone one, then [`ask`](Self::ask).
    ///
    /// Data-egress: every `(question, answer)` pair in `history` is sent to the
    /// LLM as given. A caller that keeps KB answers in it must drop the ones
    /// the mode does not allow to leave the machine;
    /// [`Session`](crate::session::Session) does that per answer provenance
    /// (review cluster-nlqa#14).
    pub fn ask_followup(&self, history: &[(String, String)], question: &str) -> Result<Answer> {
        let standalone = self.rewrite_followup_question(history, question);
        self.ask(&standalone)
    }

    fn solve_inner(&self, question: &str, mode: Mode) -> Result<Answer> {
        // Open mode: answer purely from the LLM's general knowledge, no KB.
        if mode == Mode::Open {
            return self.answer_direct(question);
        }
        // 1) Understand. Intent is a cheap classification → run it on the fast
        // model. A malformed reply shouldn't kill the query — degrade, but say
        // so. A provider outage (after the client's retries) or an exhausted
        // budget ends the request instead: generating ungrounded SPARQL with
        // more calls to the same failing provider cannot help (review
        // cluster-nlqa#16).
        self.note("understanding question… (model)");
        let intent_model = self.fast_model.clone().or_else(|| self.model.clone());
        let intent = match extract_intent(self.llm.as_ref(), question, intent_model.as_deref()) {
            Ok(i) => i,
            Err(e) if e.is_fatal() => return Err(e),
            Err(e) => {
                eprintln!("gnlqa: intent extraction failed, continuing without one: {e}");
                QuestionIntent::default()
            }
        };
        // Resolve the answer language (LLM tag → script fallback → English) so
        // LLM-generated answers come back in the user's language.
        let lang = crate::lang::resolve_lang(&intent.lang, question);
        // Route generation/repair to the fast or primary model by difficulty.
        let model_owned = self.route_model(&intent);
        let model = model_owned.as_deref();

        // 2) Link + 3) ground (only if a linker is configured).
        self.note("linking entities…");
        let mut links = String::new();
        let mut ent_uris = Vec::new();
        let mut pred_uris = Vec::new();
        // One endpoint per DISTINCT mention (its top-1 entity/type candidate), used
        // for analytics shortest-path so the two endpoints are two different
        // mentions — not two disambiguation candidates of the same one (which the
        // flat `ent_uris` list would make seeds[0]/seeds[1] on a link_k > 1 run).
        let mut analytics_seeds: Vec<String> = Vec::new();
        if let Some(linker) = self.linker() {
            for m in &intent.mentions {
                let cands = linker.link_mention(m, self.link_k)?;
                if !cands.is_empty() {
                    links.push_str(&format!("{:?} '{}' -> ", m.kind, m.text));
                    links.push_str(
                        &cands
                            .iter()
                            .map(|c| c.to_term())
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                    links.push('\n');
                }
                if let Some(rep) = cands.iter().find(|c| {
                    matches!(c.kind, LinkKind::Entity | LinkKind::Type) && !c.uri.is_empty()
                }) {
                    if !analytics_seeds.contains(&rep.uri) {
                        analytics_seeds.push(rep.uri.clone());
                    }
                }
                for c in cands {
                    match c.kind {
                        LinkKind::Entity if !c.uri.is_empty() => ent_uris.push(c.uri),
                        LinkKind::Type if !c.uri.is_empty() => ent_uris.push(c.uri),
                        _ => {}
                    }
                }
            }
            for r in &intent.relations {
                let cands = linker.link_predicate(&r.phrase, self.link_k)?;
                if !cands.is_empty() {
                    links.push_str(&format!("relation '{}' -> ", r.phrase));
                    links.push_str(
                        &cands
                            .iter()
                            .map(|c| c.to_term())
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                    links.push('\n');
                }
                for c in cands {
                    if !c.uri.is_empty() {
                        pred_uris.push(c.uri);
                    }
                }
            }
        }
        // De-duplicate before grounding so we don't run identical schema queries
        // (and bloat the prompt) for a URI linked by several mentions.
        let ent_uris = dedup(ent_uris);
        let pred_uris = dedup(pred_uris);

        // GraphRag mode: skip Text-to-SPARQL entirely — retrieve a private
        // subgraph and let the LLM answer from it (abstain if it can't).
        if mode == Mode::GraphRag {
            // Honor the abstain policy: a fixed-confidence RAG answer that couldn't
            // clear the threshold must be withheld here too (as the Auto-mode
            // fallback and the structured path already do), not returned unabstained.
            if RAG_CONFIDENCE < self.abstain_below {
                return Ok(self.abstain_answer(&lang, None, RAG_CONFIDENCE));
            }
            self.note("retrieving subgraph & composing answer… (gStore + model)");
            if !ent_uris.is_empty() {
                if let Some(ans) = self.graphrag_answer(question, &ent_uris, model, &lang)? {
                    return Ok(ans);
                }
            }
            return Ok(self.abstain_answer(&lang, None, RAG_CONFIDENCE));
        }

        // Graph-analytics routing: shortest path / centrality / PageRank /
        // communities / triangles aren't a single BGP — run gStore's analytics
        // engine over a retrieved edge sample. Gated by the abstain threshold for
        // the same reason as the GraphRAG fallback. On a non-fatal error, fall
        // through to the normal Text-to-SPARQL pipeline.
        if self.analytics
            && intent.qtype == QType::Analytics
            && ANALYTICS_CONFIDENCE >= self.abstain_below
        {
            self.note("running graph analytics… (gStore)");
            match crate::analytics::run_analytics(
                self.kb.as_ref(),
                question,
                &analytics_seeds,
                self.analytics_cfg,
            ) {
                Ok(res) => {
                    let text = if res.truncated {
                        format!(
                            "{}\n(computed over a capped sample of {} edges / {} nodes)",
                            res.text, res.edge_count, res.node_count
                        )
                    } else {
                        res.text
                    };
                    return Ok(Answer {
                        text,
                        values: Vec::new(),
                        sparql: None,
                        rounds: 0,
                        citations: Vec::new(),
                        explanation: None,
                        confidence: ANALYTICS_CONFIDENCE,
                        abstained: false,
                        provenance: Provenance::GStore,
                    });
                }
                // A KB outage or a spent request budget ends the request: the
                // Text-to-SPARQL path would only pay for an LLM call and fail
                // the same way (review cluster-nlqa#16).
                Err(e) if e.is_fatal() => return Err(e),
                // Don't mask the failure silently — surface it, then fall through
                // to the Text-to-SPARQL path (which may still answer or abstain).
                Err(e) => eprintln!("gnlqa: analytics routing failed, falling back to SPARQL: {e}"),
            }
        }

        let schema = if ent_uris.is_empty() && pred_uris.is_empty() {
            String::new()
        } else {
            self.note("gathering schema… (gStore)");
            SchemaContext::gather(self.kb.as_ref(), &ent_uris, &pred_uris, self.sample)?.render()
        };

        // 4) Generate N candidates, 5) repair-solve each, 6) rank. If generation
        // produced nothing parser-valid, fail honestly rather than fabricating an
        // answer from a wildcard query.
        self.note("generating SPARQL… (model)");
        let candidates = generate_candidates(
            self.llm.as_ref(),
            question,
            &links,
            &schema,
            self.candidates,
            model,
        )?;
        self.note("querying gStore…");
        // `?` propagates a transport outage (endpoint unreachable) as a real error
        // instead of letting it look like "no candidate produced an answer".
        let best = best_of(
            self.llm.as_ref(),
            self.kb.as_ref(),
            question,
            &links,
            &schema,
            &candidates,
            self.max_rounds,
            model,
        )?;

        // GraphRAG fallback: when the structured path produced no non-empty
        // answer, retrieve a subgraph around the linked entities and let the LLM
        // answer from it (open/relational questions a single BGP can't express).
        let structured_ok = best.as_ref().is_some_and(|o| !is_empty_answer(&o.answer));
        // Auto mode only: when the structured path produced no non-empty answer,
        // fall back to GraphRAG. (Structured mode never leaves the SPARQL path.)
        // Skip when its fixed confidence couldn't clear the caller's abstain
        // threshold — otherwise we'd present a RAG answer the structured path
        // would have withheld. Falling through lets `match best` abstain.
        if mode == Mode::Auto
            && !structured_ok
            && self.graphrag
            && !ent_uris.is_empty()
            && RAG_CONFIDENCE >= self.abstain_below
        {
            self.note("no direct result — trying GraphRAG… (gStore + model)");
            if let Some(ans) = self.graphrag_answer(question, &ent_uris, model, &lang)? {
                return Ok(ans);
            }
        }

        match best {
            Some(o) => {
                let values = answer_values(&o.answer);
                let confidence = score_outcome(&o).clamp(0.0, 1.0);

                // Abstain when too unsure: keep the SPARQL (transparency) but
                // don't present a possibly-wrong answer.
                if confidence < self.abstain_below {
                    let mut a = self.abstain_answer(&lang, Some(o.sparql), confidence);
                    a.rounds = o.rounds;
                    return Ok(a);
                }

                let text = render_answer(&o.answer, &values);
                // Citations are supporting evidence, not the answer: a failure
                // (or an exhausted budget) drops them, but not silently.
                let citations = if self.cite {
                    match gather_citations(self.kb.as_ref(), &o.answer, 5, 10, 30) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("gnlqa: gathering citations failed, answering without: {e}");
                            Vec::new()
                        }
                    }
                } else {
                    Vec::new()
                };
                // Grounded prose is exposed via `explanation` (only when it could
                // be grounded on citations); `text` stays the rendered answer.
                // The explanation sends the supporting KB triples to the LLM,
                // which structured mode promises not to do (review
                // cluster-nlqa#21): it is skipped there even when enabled.
                let explanation = if self.explain && mode != Mode::Structured {
                    explain(self.llm.as_ref(), question, &citations, model, &lang)
                } else {
                    None
                };
                Ok(Answer {
                    text,
                    values,
                    sparql: Some(o.sparql),
                    rounds: o.rounds,
                    citations,
                    explanation,
                    confidence,
                    abstained: false,
                    provenance: Provenance::GStore,
                })
            }
            None => Err(Error::Sparql(
                "no candidate query produced an answer".into(),
            )),
        }
    }

    /// A "not confident enough" answer (values withheld, SPARQL kept for
    /// transparency). Provenance is `GStore`: it came from evaluating the local
    /// structured path and deciding not to answer.
    fn abstain_answer(&self, lang: &str, sparql: Option<String>, confidence: f32) -> Answer {
        Answer {
            text: crate::lang::abstain_message(lang).to_string(),
            values: Vec::new(),
            sparql,
            rounds: 0,
            citations: Vec::new(),
            explanation: None,
            confidence,
            abstained: true,
            provenance: Provenance::GStore,
        }
    }

    /// Answer purely from the LLM's general knowledge (Mode::Open) — no KB, no
    /// grounding. Marked `Provenance::Llm` and confidence 0 (not KB-verified).
    fn answer_direct(&self, question: &str) -> Result<Answer> {
        self.note("open answer… (model, no KB)");
        let sys = "You are a helpful assistant. Answer the user's question directly \
            and concisely. If you are not sure, say so rather than inventing facts.";
        let mut req = LlmRequest::prompt(question.to_string())
            .system(sys)
            .max_tokens(1024);
        if let Some(m) = &self.model {
            req = req.model(m.clone());
        }
        let text = self.llm.complete(&req)?;
        Ok(Answer {
            text: text.trim().to_string(),
            values: Vec::new(),
            sparql: None,
            rounds: 0,
            citations: Vec::new(),
            explanation: None,
            confidence: 0.0, // ungrounded world knowledge
            abstained: false,
            provenance: Provenance::Llm,
        })
    }

    /// Retrieve a subgraph around `seeds` and let the LLM answer from it. Returns
    /// `Ok(None)` when nothing was retrieved or the model declined ("I don't
    /// know") or failed in a recoverable way (logged), so the caller can fall
    /// through to the honest structured result/error. A KB/LLM outage or an
    /// exhausted budget is returned as the error it is (review
    /// cluster-nlqa#16), not reported as "no candidate produced an answer".
    fn graphrag_answer(
        &self,
        question: &str,
        seeds: &[String],
        model: Option<&str>,
        lang: &str,
    ) -> Result<Option<Answer>> {
        use crate::graphrag::{
            answer_from_subgraph, is_dont_know, render_subgraph, try_retrieve_subgraph,
        };
        let triples = try_retrieve_subgraph(self.kb.as_ref(), seeds, self.rag)?;
        if triples.is_empty() {
            return Ok(None);
        }
        let subgraph = render_subgraph(&triples);
        let raw = match answer_from_subgraph(self.llm.as_ref(), question, &subgraph, model, lang) {
            Ok(raw) => raw,
            Err(e) if e.is_fatal() => return Err(e),
            Err(e) => {
                eprintln!("gnlqa: GraphRAG answer failed: {e}");
                return Ok(None);
            }
        };
        if is_dont_know(&raw) {
            return Ok(None);
        }
        let citations = if self.cite {
            triples.iter().take(30).map(|t| t.to_citation()).collect()
        } else {
            Vec::new()
        };
        Ok(Some(Answer {
            text: raw.trim().to_string(),
            values: Vec::new(),
            sparql: None, // GraphRAG has no single query to expose
            rounds: 0,
            citations,
            explanation: None,
            confidence: RAG_CONFIDENCE,
            abstained: false,
            provenance: Provenance::GraphRag,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::{MockKb, RdfTerm, SparqlAnswer, TermKind};
    use crate::llm::MockLlm;
    use crate::repair::RepairOutcome;

    fn nonempty() -> SparqlAnswer {
        SparqlAnswer::Select {
            vars: vec!["x".into()],
            rows: vec![vec![Some(RdfTerm {
                kind: TermKind::Uri,
                value: "http://ex/a".into(),
                datatype: None,
                lang: None,
            })]],
        }
    }
    fn empty() -> SparqlAnswer {
        SparqlAnswer::Select {
            vars: vec!["x".into()],
            rows: vec![],
        }
    }

    #[test]
    fn score_prefers_nonempty_and_fewer_rounds() {
        let a = RepairOutcome {
            sparql: "q".into(),
            answer: nonempty(),
            rounds: 0,
        };
        let b = RepairOutcome {
            sparql: "q".into(),
            answer: nonempty(),
            rounds: 2,
        };
        let c = RepairOutcome {
            sparql: "q".into(),
            answer: empty(),
            rounds: 0,
        };
        assert!(score_outcome(&a) > score_outcome(&b));
        assert!(score_outcome(&b) > score_outcome(&c));
    }

    #[test]
    fn best_of_picks_nonempty_candidate() {
        // candidate 1 → empty, candidate 2 → nonempty. best_of must pick #2.
        let llm = MockLlm::fixed("unused");
        let kb = MockKb::new(vec![empty(), nonempty()]);
        let cands = vec![
            "SELECT ?x WHERE { ?x <http://ex/p1> ?o }".to_string(),
            "SELECT ?x WHERE { ?x <http://ex/p2> ?o }".to_string(),
        ];
        let best = best_of(&llm, &kb, "q", "", "", &cands, 0, None)
            .unwrap()
            .unwrap();
        assert!(!is_empty_answer(&best.answer));
        assert!(best.sparql.contains("p2"));
    }

    /// A KB whose every query fails with a transport error (endpoint unreachable).
    struct DeadKb;
    impl KbClient for DeadKb {
        fn query(&self, _sparql: &str) -> Result<SparqlAnswer> {
            Err(Error::Http(
                "gStore endpoint unreachable: Connection refused".into(),
            ))
        }
    }

    #[test]
    fn best_of_propagates_transport_outage_without_repair_storm() {
        use std::sync::Arc;
        let llm = Arc::new(MockLlm::fixed("SELECT ?x WHERE { ?x <http://ex/p> ?o }"));
        let cands = vec!["SELECT ?x WHERE { ?x <http://ex/p> ?o }".to_string()];
        // A dead endpoint must surface as Err(Http), not None, and must NOT burn
        // repair LLM calls trying to "fix" a query against a dead socket.
        let err = best_of(&Arc::clone(&llm), &DeadKb, "q", "", "", &cands, 3, None).unwrap_err();
        assert!(matches!(err, Error::Http(_)));
        assert_eq!(
            llm.call_count(),
            0,
            "no repair calls against an unreachable endpoint"
        );
    }

    #[test]
    fn ask_reports_endpoint_outage_instead_of_no_answer() {
        let llm = MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]);
        let engine = SolveEngine::new(Box::new(llm), Box::new(DeadKb));
        let err = engine.ask("which x?").unwrap_err();
        assert!(
            matches!(err, Error::Http(_)),
            "outage must propagate, got {err:?}"
        );
    }

    #[test]
    fn graphrag_mode_respects_abstain_threshold() {
        use crate::embed::HashEmbedder;
        use crate::link::Linker;
        let linker = Linker::from_labels(
            Box::new(HashEmbedder::new(64)),
            &[("http://ex/Alien".to_string(), "Alien".to_string())],
            &[],
            &[],
            0.0,
        )
        .unwrap();
        // Explicit graphrag mode + abstain_below above RAG_CONFIDENCE (0.5): the
        // fixed-confidence RAG answer must be withheld (the LLM must not be called).
        let llm = MockLlm::new(vec![
            r#"{"qtype":"open","mentions":[{"text":"Alien","kind":"entity"}]}"#.to_string(),
            "SHOULD NOT BE CALLED — RAG must abstain first".to_string(),
        ]);
        let kb = MockKb::new(vec![nonempty()]);
        let a = SolveEngine::new(Box::new(llm), Box::new(kb))
            .with_linker(linker)
            .with_abstain_below(0.7)
            .ask_with_mode("tell me about Alien", Mode::GraphRag)
            .unwrap();
        assert!(a.abstained, "sub-threshold graphrag answer must abstain");
    }

    #[test]
    fn clear_cache_forces_recompute() {
        use std::sync::Arc;
        let llm = Arc::new(MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
            // second full solve after the cache is cleared
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]));
        let kb = MockKb::new(vec![nonempty()]);
        let engine = SolveEngine::new(Box::new(Arc::clone(&llm)), Box::new(kb))
            .with_citations(false)
            .with_cache(8);
        let _ = engine.ask("q").unwrap();
        assert_eq!(llm.call_count(), 2);
        let _ = engine.ask("q").unwrap(); // cached — no new calls
        assert_eq!(llm.call_count(), 2);
        engine.clear_cache();
        let _ = engine.ask("q").unwrap(); // recomputed after clear
        assert_eq!(llm.call_count(), 4);
    }

    #[test]
    fn default_mode_applies_to_ask() {
        // ask() honors the engine default mode: Open answers from the LLM, no KB.
        let llm = MockLlm::fixed("SPARQL is a query language for RDF.");
        let kb = MockKb::new(vec![]);
        let a = SolveEngine::new(Box::new(llm), Box::new(kb))
            .with_mode(Mode::Open)
            .ask("what is SPARQL?")
            .unwrap();
        assert_eq!(a.provenance, Provenance::Llm);
    }

    #[test]
    fn engine_ask_end_to_end_without_linker() {
        // intent JSON, then the generation array, then queries answered.
        let llm = MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]);
        let kb = MockKb::new(vec![nonempty()]);
        let engine = SolveEngine::new(Box::new(llm), Box::new(kb));
        let a = engine.ask("which x?").unwrap();
        assert_eq!(a.values, vec!["http://ex/a"]);
        assert!(a.sparql.unwrap().contains("<http://ex/p>"));
    }

    #[test]
    fn confidence_high_for_nonempty_and_abstains_when_unsure() {
        let llm = MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]);
        let kb = MockKb::new(vec![nonempty()]);
        let a = SolveEngine::new(Box::new(llm), Box::new(kb))
            .with_citations(false)
            .ask("q")
            .unwrap();
        assert!(a.confidence > 0.9 && !a.abstained);

        // empty result + high abstain threshold → abstain, but SPARQL stays visible
        let llm2 = MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]);
        let kb2 = MockKb::new(vec![empty()]);
        let a2 = SolveEngine::new(Box::new(llm2), Box::new(kb2))
            .with_max_rounds(0)
            .with_abstain_below(0.5)
            .ask("q")
            .unwrap();
        assert!(a2.abstained && a2.confidence < 0.5);
        assert!(a2.sparql.is_some());
    }

    #[test]
    fn cache_serves_repeat_without_recompute() {
        use std::sync::Arc;
        let llm = Arc::new(MockLlm::new(vec![
            r#"{"qtype":"list"}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
        ]));
        let kb = MockKb::new(vec![nonempty()]);
        let engine = SolveEngine::new(Box::new(Arc::clone(&llm)), Box::new(kb))
            .with_citations(false)
            .with_cache(8);
        let a1 = engine.ask("q").unwrap();
        let a2 = engine.ask("q").unwrap(); // from cache
        assert_eq!(a1.values, a2.values);
        assert_eq!(llm.call_count(), 2, "second ask must be served from cache");
    }

    #[test]
    fn model_routing_by_difficulty() {
        use crate::intent::{QType, QuestionIntent};
        let e = SolveEngine::new(Box::new(MockLlm::fixed("x")), Box::new(MockKb::new(vec![])))
            .with_model("opus-primary")
            .with_fast_model("sonnet-fast");
        let factoid = QuestionIntent {
            qtype: QType::Factoid,
            ..Default::default()
        };
        let analytics = QuestionIntent {
            qtype: QType::Analytics,
            ..Default::default()
        };
        assert_eq!(e.route_model(&factoid).as_deref(), Some("sonnet-fast"));
        assert_eq!(e.route_model(&analytics).as_deref(), Some("opus-primary"));
    }

    #[test]
    fn engine_fails_honestly_when_no_valid_candidate() {
        // generation yields only an invalid candidate, GraphRAG disabled → no
        // fabricated answer
        let llm = MockLlm::new(vec![
            r#"{"qtype":"factoid"}"#.to_string(),
            r#"["not sparql"]"#.to_string(),
        ]);
        let kb = MockKb::new(vec![nonempty()]);
        let engine = SolveEngine::new(Box::new(llm), Box::new(kb)).with_graphrag(false);
        assert!(engine.ask("q").is_err());
    }

    #[test]
    fn graphrag_fallback_when_no_structured_answer() {
        use crate::embed::HashEmbedder;
        use crate::link::Linker;
        // linker maps "Alien" → a URI so the entity becomes a GraphRAG seed
        let linker = Linker::from_labels(
            Box::new(HashEmbedder::new(64)),
            &[("http://ex/Alien".to_string(), "Alien".to_string())],
            &[],
            &[],
            0.0,
        )
        .unwrap();
        // LLM: intent (open + Alien mention), generation (no valid query), RAG answer.
        let llm = MockLlm::new(vec![
            r#"{"qtype":"open","mentions":[{"text":"Alien","kind":"entity"}]}"#.to_string(),
            r#"["not a query"]"#.to_string(),
            "Ridley Scott directed Alien.".to_string(),
        ]);
        // Every KB query returns a (p, o) row (schema gather + GraphRAG outgoing).
        let po = SparqlAnswer::Select {
            vars: vec!["p".into(), "o".into()],
            rows: vec![vec![
                Some(RdfTerm {
                    kind: TermKind::Uri,
                    value: "http://ex/directed".into(),
                    datatype: None,
                    lang: None,
                }),
                Some(RdfTerm {
                    kind: TermKind::Uri,
                    value: "http://ex/RidleyScott".into(),
                    datatype: None,
                    lang: None,
                }),
            ]],
        };
        let kb = MockKb::new(vec![po]);
        let engine = SolveEngine::new(Box::new(llm), Box::new(kb))
            .with_linker(linker)
            .with_model("primary");
        let a = engine.ask("tell me about Alien").unwrap();
        assert_eq!(a.text, "Ridley Scott directed Alien.");
        assert!(a.sparql.is_none()); // GraphRAG path exposes no single query
        assert!(!a.citations.is_empty()); // grounded on the retrieved triple
        assert!((a.confidence - 0.5).abs() < 1e-6);
    }

    #[test]
    fn graphrag_respects_abstain_threshold() {
        use crate::embed::HashEmbedder;
        use crate::link::Linker;
        let linker = Linker::from_labels(
            Box::new(HashEmbedder::new(64)),
            &[("http://ex/Alien".to_string(), "Alien".to_string())],
            &[],
            &[],
            0.0,
        )
        .unwrap();
        // A *valid* query that returns empty → structured Some(empty), conf ~0.1.
        // RAG_CONFIDENCE (0.5) is below abstain_below (0.7), so the fallback must
        // be skipped and the engine must abstain rather than emit a RAG answer.
        let llm = MockLlm::new(vec![
            r#"{"qtype":"open","mentions":[{"text":"Alien","kind":"entity"}]}"#.to_string(),
            r#"["SELECT ?x WHERE { ?x <http://ex/p> ?o }"]"#.to_string(),
            "SHOULD NOT BE CALLED — RAG must be skipped".to_string(),
        ]);
        let kb = MockKb::new(vec![empty()]);
        let a = SolveEngine::new(Box::new(llm), Box::new(kb))
            .with_linker(linker)
            .with_max_rounds(0)
            .with_abstain_below(0.7)
            .ask("tell me about Alien")
            .unwrap();
        assert!(
            a.abstained,
            "sub-threshold RAG must not override the abstain policy"
        );
        assert!(a.confidence < 0.7);
    }

    fn sized_answer(bytes: usize) -> Answer {
        Answer {
            text: "x".repeat(bytes),
            values: Vec::new(),
            sparql: None,
            rounds: 0,
            citations: Vec::new(),
            explanation: None,
            confidence: 1.0,
            abstained: false,
            provenance: Provenance::GStore,
        }
    }

    #[test]
    fn cache_is_bounded_by_bytes() {
        // review cluster-nlqa#19
        let mut c = Cache::new(1000);
        c.max_bytes = 80_000;
        let t = Instant::now();
        for i in 0..50 {
            c.put_at(format!("q{i}"), sized_answer(5_000), t);
            assert!(c.bytes <= c.max_bytes, "bytes {} over budget", c.bytes);
        }
        assert!(c.map.len() < 50, "old entries were evicted");
        assert!(c.get_at("q49", t).is_some(), "the newest survives");
        assert!(c.get_at("q0", t).is_none(), "the oldest went first");
        // an answer above an eighth of the budget is not cached at all
        c.put_at("big".into(), sized_answer(20_000), t);
        assert!(c.get_at("big", t).is_none());
        let u = c.usage();
        assert_eq!(u.entries, c.order.len());
    }

    #[test]
    fn expired_entries_are_dropped_and_free_their_bytes() {
        let mut c = Cache::new(10);
        c.ttl = Some(Duration::from_secs(60));
        let t0 = Instant::now();
        c.put_at("a".into(), sized_answer(1000), t0);
        let held = c.bytes;
        assert!(held > 1000);
        assert!(c.get_at("a", t0 + Duration::from_secs(59)).is_some());
        assert!(c.get_at("a", t0 + Duration::from_secs(61)).is_none());
        assert_eq!(c.bytes, 0, "an expired entry stops holding memory");
        assert!(c.map.is_empty() && c.order.is_empty());
        // a put also sweeps expired entries from the front
        c.put_at("b".into(), sized_answer(10), t0);
        c.put_at("c".into(), sized_answer(10), t0 + Duration::from_secs(120));
        assert_eq!(c.map.len(), 1);
        assert!(c.map.contains_key("c"));
    }

    #[test]
    fn re_put_refreshes_age_and_order() {
        let mut c = Cache::new(2);
        let t = Instant::now();
        c.put_at("a".into(), sized_answer(1), t);
        c.put_at("b".into(), sized_answer(1), t);
        c.put_at("a".into(), sized_answer(1), t); // a is now the newest
        c.put_at("c".into(), sized_answer(1), t); // evicts b, not a
        assert!(c.get_at("a", t).is_some());
        assert!(c.get_at("b", t).is_none());
        assert_eq!(c.order.len(), 2);
    }
}
