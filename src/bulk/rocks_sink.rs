//! The RocksDB target of the bulk build ([`BulkTarget::RocksDb`](super::BulkTarget)):
//! the sorted streams of the R2 pipeline written as SST files, one set per
//! column family, and ingested into a fresh store.
//!
//! 1. **parse + encode** — [`source`] parses; the quads are encoded as the
//!    memory build (`Database::build_rocksdb_*`) encodes them — subject,
//!    predicate, object, then the graph, first seen first — so ids, and
//!    with them every key and value, equal the incremental build's. Only
//!    the id-space bounds are checked (with `checked_*_id`'s errors):
//!    RocksDB has none of the disk layout's key-size limits.
//! 2. **spo** — the merged SPO stream becomes the `spo` SST files and, as
//!    it passes, the statistics taps ([`Stats`]), the `s|s` counters, the
//!    POS / OSP sorters and the `(p, s, count)` re-sort for `x|p|s`; then the
//!    GSPO stream becomes the `gspo` files.
//! 3. **index** — four independent tasks, on their own threads when
//!    `threads` allows: POS (with `y|p|o` and the per-predicate distinct
//!    objects), OSP (with `o|o` and the distinct objects), the `x|p|s`
//!    counters, and the dictionary (`tag ‖ be32(id) → key`).
//! 4. **ingest** — the global and `c|`/`u|`/`v|` counters; a fresh store in
//!    `.bulk-tmp/rocksdb` ingests every CF's files (moved, not copied), the
//!    dictionary first; the `meta` name and then `layout = "1"` are written
//!    last; flush, WAL sync, close; the store is reopened and its counters
//!    checked against the build's; every file is synced.
//!
//! Every SST file holds ascending keys and is cut at [`SST_SPLIT_BYTES`];
//! the files of one column family never overlap, so the ingest places
//! them straight into the bottom level. Each is written with exactly the
//! options [`cf_options`] gives its column family (compression, bloom
//! filter). A counter that would be zero is absent, as the incremental
//! writers leave it.
//!
//! The install ([`super::ROCKS_CRASH_POINTS`]): the build holds the
//! directory's [`DirLock`] and [`BuildGuard`] and refuses a directory whose
//! RocksDB store is open anywhere; the new store replaces `rocksdb/`
//! through `fsutil::install_staged_dir` (its rename into place is the
//! commit point), then the replaced database's reasoning rules, the other
//! layouts and the staging area are removed.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

use rocksdb::{Options, SstFileWriter};

use crate::backend::rocks::{
    cf_options, key3, key4, tag1, tag2, RocksStore, RocksTuning, TermSpace, CF_DICT, CF_GSPO,
    CF_OSP, CF_POS, CF_SPO, CF_STATS, K_OBJECTS, K_PREDS, K_SUBJECTS, K_TRIPLES, LAYOUT_VERSION,
    META_LAYOUT_KEY, META_NAME_KEY, T_CARD, T_OBJ, T_PO, T_POBJ, T_PS, T_PSUBJ, T_SUB,
};
use crate::db::dirlock::{BuildGuard, DirLock};
use crate::db::fsutil::{self, remove_dir_if_exists, sync_dir};
use crate::db::persist::remove_other_layouts;
use crate::db::StorageKind;
use crate::dict::{checked_entity_id, checked_literal_id, checked_predicate_id};
use crate::error::{GStoreError, Result};
use crate::model::id::{PredId, LITERAL_FIRST_ID};
use crate::model::Term;
use crate::parser::import::{ErrorLog, ImportOptions, ImportReport, BNODE_GRAPH_PREFIX};
use crate::parser::nquads::Quad;
use crate::store::TripleSource;

use super::budget::Budget;
use super::commit::{refuse_open_store, remove_rule_dir, CRASH_ENV, STAGING_CLEANUP, STAGING_DIR};
use super::encode::{render_key, Dict, Space};
use super::extsort::{ExtSorter, Sorted};
use super::scratch::{self, Scratch};
use super::source;
use super::stats::Stats;
use super::termmap::{hash_key, TermMap};
use super::{BulkInput, BulkOptions, BulkReport, PhaseTime, ROCKS_CRASH_POINTS};

/// The RocksDB layout's directory inside a database directory.
const ROCKS_SUBDIR: &str = "rocksdb";
/// Where the SST files are written inside the staging directory.
const SST_DIR: &str = "sst";
/// SST files are cut once they reach this size.
pub(crate) const SST_SPLIT_BYTES: u64 = 256 << 20;
/// A test hook: the environment variable overriding [`SST_SPLIT_BYTES`]
/// (tests cut many small files; the bytes stored are the same).
const SST_SPLIT_ENV: &str = "GSTORE_BULK_SST_SPLIT_BYTES";
/// Keys between two checks of an SST file's size.
const SIZE_CHECK_EVERY: u32 = 4096;

// ---- crash points (tests) ---------------------------------------------------

/// Per-directory simulated crash points (see [`debug_crash_at`]).
fn hooks() -> &'static Mutex<Vec<(PathBuf, String)>> {
    static HOOKS: OnceLock<Mutex<Vec<(PathBuf, String)>>> = OnceLock::new();
    HOOKS.get_or_init(|| Mutex::new(Vec::new()))
}

/// The RocksDB install's half of [`super::debug_crash_at`].
pub(crate) fn debug_crash_at(dir: &Path, point: Option<&str>) {
    let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut h = hooks().lock().unwrap_or_else(PoisonError::into_inner);
    h.retain(|(d, _)| *d != dir);
    if let Some(p) = point {
        h.push((dir, p.to_string()));
    }
}

/// The crash points armed for one install.
struct Failpoints {
    abort_at: Option<String>,
    fail_at: Option<String>,
    crashed: bool,
}

impl Failpoints {
    fn load(dir: &Path) -> Failpoints {
        let canonical = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let fail_at = hooks()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(d, _)| *d == canonical)
            .map(|(_, p)| p.clone());
        Failpoints {
            abort_at: std::env::var(CRASH_ENV).ok().filter(|s| !s.is_empty()),
            fail_at,
            crashed: false,
        }
    }

    fn hit(&mut self, point: &str) -> Result<()> {
        debug_assert!(ROCKS_CRASH_POINTS.contains(&point));
        if self.abort_at.as_deref() == Some(point) {
            eprintln!("{CRASH_ENV}: aborting after bulk install step '{point}'");
            std::process::abort();
        }
        if self.fail_at.as_deref() == Some(point) {
            self.crashed = true;
            return Err(GStoreError::Database(format!(
                "bulk build: simulated crash after install step '{point}'"
            )));
        }
        Ok(())
    }
}

// ---- encoding ---------------------------------------------------------------

/// The graph key the memory build files a quad under (`graph_iri` + an IRI
/// dictionary key); `false` = the default graph.
fn render_graph(g: &Option<Term>, out: &mut Vec<u8>) -> bool {
    out.clear();
    let label = match g {
        Some(Term::Iri(s)) => s.as_str(),
        Some(Term::Blank(l)) => {
            out.extend_from_slice(b"<");
            out.extend_from_slice(BNODE_GRAPH_PREFIX.as_bytes());
            out.extend_from_slice(l.as_bytes());
            out.push(b'>');
            return true;
        }
        Some(Term::Literal { .. }) | None => return false,
    };
    out.push(b'<');
    out.extend_from_slice(label.as_bytes());
    out.push(b'>');
    true
}

/// Intern `key` in the term map of `space`, checking a new term's id against
/// the space's bound exactly as the in-memory dictionary does; the public id.
fn intern(map: &mut TermMap, space: Space, key: &[u8]) -> Result<u32> {
    let h = hash_key(key);
    let local = match map.find(key, h) {
        Some(id) => id,
        None => {
            let next = map.len() as u64;
            match space {
                Space::Entity => checked_entity_id(next).map(drop)?,
                Space::Literal => checked_literal_id(next).map(drop)?,
                Space::Predicate => checked_predicate_id(next).map(drop)?,
            }
            map.intern(key, h).0
        }
    };
    Ok(if space == Space::Literal {
        LITERAL_FIRST_ID + local
    } else {
        local
    })
}

/// Encodes quads the way the memory build does (see the module docs).
struct Encoder<'a> {
    dict: Dict,
    spo: ExtSorter<'a, [u32; 3]>,
    gspo: ExtSorter<'a, [u32; 4]>,
    /// Reused key buffers: subject, predicate, object, graph.
    keys: [Vec<u8>; 4],
}

impl Encoder<'_> {
    fn add(&mut self, batch: &[Quad]) -> Result<()> {
        for q in batch {
            self.add_quad(q)?;
        }
        Ok(())
    }

    fn add_quad(&mut self, q: &Quad) -> Result<()> {
        let [ks, kp, ko, kg] = &mut self.keys;
        let d = &mut self.dict;
        render_key(&q.subject, ks);
        render_key(&q.predicate, kp);
        render_key(&q.object, ko);
        let s = intern(&mut d.entities, Space::Entity, ks)?;
        let p = intern(&mut d.predicates, Space::Predicate, kp)?;
        let o = if q.object.is_literal() {
            intern(&mut d.literals, Space::Literal, ko)?
        } else {
            intern(&mut d.entities, Space::Entity, ko)?
        };
        if render_graph(&q.graph, kg) {
            let g = intern(&mut d.entities, Space::Entity, kg)?;
            self.gspo.push([g, s, p, o])
        } else {
            self.spo.push([s, p, o])
        }
    }
}

// ---- SST output ---------------------------------------------------------------

fn sst_error(e: rocksdb::Error) -> GStoreError {
    GStoreError::Database(format!("rocksdb SST write: {e}"))
}

/// Where and how the SST files are written.
#[derive(Clone, Copy)]
struct Out<'a> {
    dir: &'a Path,
    tuning: &'a RocksTuning,
    /// Cut a file at this size ([`SST_SPLIT_BYTES`]).
    split: u64,
}

impl Out<'_> {
    /// The options column family `cf`'s files are written with.
    fn opts(&self, cf: &str) -> Options {
        cf_options(cf, self.tuning)
    }
}

/// The SST files of one column family (or one key range of `stats`),
/// written in ascending key order and cut at [`SST_SPLIT_BYTES`].
struct SstOut<'o> {
    opts: &'o Options,
    dir: &'o Path,
    split: u64,
    name: &'static str,
    writer: Option<SstFileWriter<'o>>,
    files: Vec<PathBuf>,
    since_check: u32,
    keys: u64,
}

impl<'o> SstOut<'o> {
    fn new(opts: &'o Options, out: Out<'o>, name: &'static str) -> SstOut<'o> {
        SstOut {
            opts,
            dir: out.dir,
            split: out.split,
            name,
            writer: None,
            files: Vec::new(),
            since_check: 0,
            keys: 0,
        }
    }

    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        if self.writer.is_none() {
            let path = self
                .dir
                .join(format!("{}-{:04}.sst", self.name, self.files.len()));
            let w = SstFileWriter::create(self.opts);
            w.open(&path).map_err(sst_error)?;
            self.files.push(path);
            self.writer = Some(w);
        }
        let w = self.writer.as_mut().expect("opened above");
        w.put(key, value).map_err(sst_error)?;
        self.keys += 1;
        self.since_check += 1;
        if self.since_check >= SIZE_CHECK_EVERY {
            self.since_check = 0;
            if w.file_size() >= self.split {
                self.cut()?;
            }
        }
        Ok(())
    }

    /// A counter: `key → be64(n)`.
    fn counter(&mut self, key: &[u8], n: u64) -> Result<()> {
        self.put(key, &n.to_be_bytes())
    }

    /// Finish the file being written, if any.
    fn cut(&mut self) -> Result<()> {
        if let Some(mut w) = self.writer.take() {
            w.finish().map_err(sst_error)?;
        }
        Ok(())
    }

    /// The finished files, in key order.
    fn finish(mut self) -> Result<Vec<PathBuf>> {
        self.cut()?;
        Ok(self.files)
    }
}

/// Counts runs of equal keys in a sorted stream, handing each finished run
/// to its consumer.
struct Runs<K: PartialEq + Copy> {
    current: Option<(K, u64)>,
}

impl<K: PartialEq + Copy> Runs<K> {
    fn new() -> Runs<K> {
        Runs { current: None }
    }

    /// The next key; returns the run it ended, if it ended one.
    #[inline]
    fn next(&mut self, key: K) -> Option<(K, u64)> {
        match &mut self.current {
            Some((k, n)) if *k == key => {
                *n += 1;
                None
            }
            cur => cur.replace((key, 1)),
        }
    }

    /// The last run.
    fn finish(self) -> Option<(K, u64)> {
        self.current
    }
}

// ---- the image ------------------------------------------------------------------

/// What the pipeline needs besides its input.
struct Ctx<'a> {
    budget: &'a Budget,
    scratch: &'a Scratch,
    threads: usize,
    tuning: RocksTuning,
    /// Where SST files are cut ([`SST_SPLIT_BYTES`], or [`SST_SPLIT_ENV`]).
    split: u64,
}

/// What [`build_store`] wrote.
#[derive(Debug, Default)]
struct Image {
    import: ImportReport,
    triples: u64,
    quads: u64,
    entities: u64,
    literals: u64,
    predicates: u64,
    term_map_bytes: usize,
    store_bytes: u64,
    phases: Vec<PhaseTime>,
}

fn phase(phases: &mut Vec<PhaseTime>, name: &'static str, since: Instant) {
    phases.push(PhaseTime {
        name,
        millis: since.elapsed().as_secs_f64() * 1e3,
    });
}

fn millis(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1e3
}

/// Run the tasks, on threads of their own when `parallel`; results in order.
fn run_all<T: Send>(parallel: bool, tasks: Vec<Box<dyn FnOnce() -> T + Send + '_>>) -> Vec<T> {
    if !parallel {
        return tasks.into_iter().map(|t| t()).collect();
    }
    std::thread::scope(|s| {
        let handles: Vec<_> = tasks.into_iter().map(|t| s.spawn(t)).collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    })
}

/// The output of one index task.
struct TaskOut {
    /// `(column family, files)`.
    files: Vec<(&'static str, Vec<PathBuf>)>,
    /// The POS task's per-predicate distinct objects.
    pos_stats: Option<Stats>,
    /// The OSP task's distinct objects.
    distinct_obj: Option<u32>,
    millis: f64,
}

/// POS: the `pos` files, the `y|p|o` counters, the per-predicate distinct
/// objects.
fn task_pos(mut sorted: Sorted<'_, [u32; 3]>, out: Out<'_>, npreds: usize) -> Result<TaskOut> {
    let t = Instant::now();
    let (pos_opts, stats_opts) = (out.opts(CF_POS), out.opts(CF_STATS));
    let mut pos = SstOut::new(&pos_opts, out, "pos");
    let mut y = SstOut::new(&stats_opts, out, "stats-y");
    let mut st = Stats::new(npreds);
    let mut runs = Runs::new();
    while let Some([p, o, s]) = sorted.next()? {
        pos.put(&key3(p, o, s), b"")?;
        st.on_pos(p, o, s);
        if let Some(((rp, ro), n)) = runs.next((p, o)) {
            y.counter(&tag2(T_PO, rp, ro), n)?;
        }
    }
    if let Some(((rp, ro), n)) = runs.finish() {
        y.counter(&tag2(T_PO, rp, ro), n)?;
    }
    Ok(TaskOut {
        files: vec![(CF_POS, pos.finish()?), (CF_STATS, y.finish()?)],
        pos_stats: Some(st),
        distinct_obj: None,
        millis: millis(t),
    })
}

/// OSP: the `osp` files, the `o|o` counters, the distinct objects.
fn task_osp(mut sorted: Sorted<'_, [u32; 3]>, out: Out<'_>) -> Result<TaskOut> {
    let t = Instant::now();
    let (osp_opts, stats_opts) = (out.opts(CF_OSP), out.opts(CF_STATS));
    let mut osp = SstOut::new(&osp_opts, out, "osp");
    let mut ocount = SstOut::new(&stats_opts, out, "stats-o");
    let mut st = Stats::new(0);
    let mut runs = Runs::new();
    while let Some([o, s, p]) = sorted.next()? {
        osp.put(&key3(o, s, p), b"")?;
        st.on_osp(o, s, p);
        if let Some((ro, n)) = runs.next(o) {
            ocount.counter(&tag1(T_OBJ, ro), n)?;
        }
    }
    if let Some((ro, n)) = runs.finish() {
        ocount.counter(&tag1(T_OBJ, ro), n)?;
    }
    Ok(TaskOut {
        files: vec![(CF_OSP, osp.finish()?), (CF_STATS, ocount.finish()?)],
        pos_stats: None,
        distinct_obj: Some(st.distinct_obj),
        millis: millis(t),
    })
}

/// The `x|p|s` counters from the re-sorted `(p, s, count)` records.
fn task_ps(mut sorted: Sorted<'_, [u32; 3]>, out: Out<'_>) -> Result<TaskOut> {
    let t = Instant::now();
    let stats_opts = out.opts(CF_STATS);
    let mut x = SstOut::new(&stats_opts, out, "stats-x");
    while let Some([p, s, n]) = sorted.next()? {
        x.counter(&tag2(T_PS, p, s), u64::from(n))?;
    }
    Ok(TaskOut {
        files: vec![(CF_STATS, x.finish()?)],
        pos_stats: None,
        distinct_obj: None,
        millis: millis(t),
    })
}

/// The dictionary: `tag ‖ be32(id) → key` for every term, spaces in tag
/// order (`E`, `L`, `P`), ids ascending.
fn task_dict(dict: &Dict, out: Out<'_>) -> Result<TaskOut> {
    let t = Instant::now();
    let opts = out.opts(CF_DICT);
    let mut dict_out = SstOut::new(&opts, out, "dict");
    for (space, map, offset) in [
        (TermSpace::Entity, &dict.entities, 0),
        (TermSpace::Literal, &dict.literals, LITERAL_FIRST_ID),
        (TermSpace::Predicate, &dict.predicates, 0),
    ] {
        for id in 0..map.len() as u32 {
            dict_out.put(&tag1(space.tag(), offset + id), map.key(id))?;
        }
    }
    Ok(TaskOut {
        files: vec![(CF_DICT, dict_out.finish()?)],
        pos_stats: None,
        distinct_obj: None,
        millis: millis(t),
    })
}

/// Build the new store in `staging/rocksdb` from `input` (see the module
/// docs).
fn build_store(
    staging: &Path,
    name: &str,
    input: &BulkInput<'_>,
    opts: &ImportOptions,
    log: &mut dyn ErrorLog,
    ctx: &Ctx<'_>,
) -> Result<Image> {
    let (budget, scratch, tuning) = (ctx.budget, ctx.scratch, &ctx.tuning);
    let sst_dir = staging.join(SST_DIR);
    fs::create_dir_all(&sst_dir)?;
    let out = Out {
        dir: &sst_dir,
        tuning,
        split: ctx.split,
    };
    let mut img = Image::default();

    // 1. Parse and encode.
    let t = Instant::now();
    let mut enc = Encoder {
        dict: Dict::default(),
        spo: ExtSorter::new("spo", budget, scratch, budget.share(2)),
        gspo: ExtSorter::new("gspo", budget, scratch, budget.share(2)),
        keys: Default::default(),
    };
    let mut sink = |b: &mut Vec<Quad>| enc.add(b);
    img.import = source::import(input, opts, log, ctx.threads, &mut sink)?;
    let Encoder {
        mut dict,
        spo,
        gspo,
        ..
    } = enc;
    img.term_map_bytes = dict.heap_bytes();
    dict.entities.drop_table();
    dict.literals.drop_table();
    dict.predicates.drop_table();
    img.entities = dict.entities.len() as u64;
    img.literals = dict.literals.len() as u64;
    img.predicates = dict.predicates.len() as u64;
    phase(&mut img.phases, "parse+encode", t);

    // 2. SPO (feeding the statistics, s|, POS, OSP and the x| re-sort), GSPO.
    let t = Instant::now();
    let npreds = dict.predicates.len();
    let mut stats = Stats::new(npreds);
    let share3 = budget.share(3);
    let mut pos = ExtSorter::<[u32; 3]>::new("pos", budget, scratch, share3);
    let mut osp = ExtSorter::<[u32; 3]>::new("osp", budget, scratch, share3);
    let mut ps = ExtSorter::<[u32; 3]>::new("ps", budget, scratch, share3);
    let mut files: Vec<(&'static str, Vec<PathBuf>)> = Vec::new();
    {
        let (spo_opts, stats_opts) = (out.opts(CF_SPO), out.opts(CF_STATS));
        let mut spo_out = SstOut::new(&spo_opts, out, "spo");
        let mut s_out = SstOut::new(&stats_opts, out, "stats-s");
        let mut subjects = Runs::new();
        let mut pairs = Runs::new();
        let mut sorted = spo.finish()?;
        while let Some([s, p, o]) = sorted.next()? {
            spo_out.put(&key3(s, p, o), b"")?;
            stats.on_spo(s, p, o);
            pos.push([p, o, s])?;
            osp.push([o, s, p])?;
            if let Some((rs, n)) = subjects.next(s) {
                s_out.counter(&tag1(T_SUB, rs), n)?;
            }
            if let Some(((rs, rp), n)) = pairs.next((s, p)) {
                ps.push([rp, rs, pair_count(n)?])?;
            }
        }
        drop(sorted);
        if let Some((rs, n)) = subjects.finish() {
            s_out.counter(&tag1(T_SUB, rs), n)?;
        }
        if let Some(((rs, rp), n)) = pairs.finish() {
            ps.push([rp, rs, pair_count(n)?])?;
        }
        files.push((CF_SPO, spo_out.finish()?));
        files.push((CF_STATS, s_out.finish()?));

        let gspo_opts = out.opts(CF_GSPO);
        let mut gspo_out = SstOut::new(&gspo_opts, out, "gspo");
        let mut sorted = gspo.finish()?;
        while let Some([g, s, p, o]) = sorted.next()? {
            gspo_out.put(&key4(g, s, p, o), b"")?;
        }
        img.quads = gspo_out.keys;
        files.push((CF_GSPO, gspo_out.finish()?));
    }
    phase(&mut img.phases, "spo", t);

    // 3. POS / OSP / x| / dictionary.
    let t = Instant::now();
    let parallel = ctx.threads > 1;
    let dict_ref = &dict;
    type Task<'t> = Box<dyn FnOnce() -> Result<TaskOut> + Send + 't>;
    let tasks: Vec<Task<'_>> = vec![
        Box::new(move || task_pos(pos.finish()?, out, npreds)),
        Box::new(move || task_osp(osp.finish()?, out)),
        Box::new(move || task_ps(ps.finish()?, out)),
        Box::new(move || task_dict(dict_ref, out)),
    ];
    let outs = run_all(parallel, tasks)
        .into_iter()
        .collect::<Result<Vec<TaskOut>>>()?;
    phase(&mut img.phases, "index", t);
    for (name, task) in ["  pos", "  osp", "  x-counts", "  dictionary"]
        .into_iter()
        .zip(&outs)
    {
        img.phases.push(PhaseTime {
            name,
            millis: task.millis,
        });
    }
    for task in outs {
        if let Some(st) = &task.pos_stats {
            stats.merge_pos(st);
        }
        if let Some(n) = task.distinct_obj {
            stats.distinct_obj = n;
        }
        files.extend(task.files);
    }

    // 4. The global and per-predicate counters; ingest; check.
    let t = Instant::now();
    files.extend(write_pred_counters(&stats, out)?);
    let store_dir = staging.join(ROCKS_SUBDIR);
    {
        let store = RocksStore::open_with(&store_dir, tuning)?;
        // The dictionary first: a term is in the store before any triple
        // that uses it.
        for cf in [CF_DICT, CF_SPO, CF_POS, CF_OSP, CF_GSPO, CF_STATS] {
            let mut of_cf: Vec<PathBuf> = files
                .iter()
                .filter(|(c, _)| *c == cf)
                .flat_map(|(_, f)| f.iter().cloned())
                .collect();
            of_cf.sort();
            store.ingest_sst(cf, &of_cf)?;
        }
        store.put_meta(META_NAME_KEY, name.as_bytes())?;
        store.put_meta(META_LAYOUT_KEY, LAYOUT_VERSION)?;
        store.flush()?;
        store.sync_wal()?;
    }
    check_counters(&store_dir, tuning, &stats)?;
    img.store_bytes = sync_tree(&store_dir)?;
    sync_dir(staging)?;
    let _ = fs::remove_dir(&sst_dir);
    phase(&mut img.phases, "ingest", t);
    img.triples = stats.triples;
    Ok(img)
}

/// The count of an `(s, p)` run as the re-sort record's third field (it
/// counts distinct objects, so it fits).
fn pair_count(n: u64) -> Result<u32> {
    u32::try_from(n).map_err(|_| {
        GStoreError::Database(format!(
            "bulk build: {n} objects under one subject and predicate"
        ))
    })
}

/// The global counters and `c|p` (one file: `O`, `P`, `S`, `T`, `c…`), then
/// `u|p` and `v|p` (another): two key ranges that the `o|`, `s|`, `x|` and
/// `y|` files do not overlap. Nothing at all for an empty store.
fn write_pred_counters(stats: &Stats, out: Out<'_>) -> Result<Vec<(&'static str, Vec<PathBuf>)>> {
    if stats.triples == 0 {
        return Ok(Vec::new());
    }
    let opts = out.opts(CF_STATS);
    let live = || {
        stats
            .preds
            .iter()
            .enumerate()
            .filter(|(_, c)| c.0 > 0)
            .map(|(p, c)| (p as PredId, *c))
    };
    let mut a = SstOut::new(&opts, out, "stats-a");
    // Byte order of the one-byte global keys: `O` < `P` < `S` < `T`.
    a.counter(K_OBJECTS, u64::from(stats.distinct_obj))?;
    a.counter(K_PREDS, live().count() as u64)?;
    a.counter(K_SUBJECTS, u64::from(stats.distinct_subj))?;
    a.counter(K_TRIPLES, stats.triples)?;
    for (p, (n, _, _)) in live() {
        a.counter(&tag1(T_CARD, p), u64::from(n))?;
    }
    let mut b = SstOut::new(&opts, out, "stats-b");
    for (p, (_, ds, _)) in live() {
        b.counter(&tag1(T_PSUBJ, p), u64::from(ds))?;
    }
    for (p, (_, _, dob)) in live() {
        b.counter(&tag1(T_POBJ, p), u64::from(dob))?;
    }
    Ok(vec![(CF_STATS, a.finish()?), (CF_STATS, b.finish()?)])
}

/// Reopen the new store and check that it reports the counts the build
/// computed.
fn check_counters(store_dir: &Path, tuning: &RocksTuning, stats: &Stats) -> Result<()> {
    let store = RocksStore::open_with(store_dir, tuning)?;
    let live: Vec<(PredId, (u32, u32, u32))> = stats
        .preds
        .iter()
        .enumerate()
        .filter(|(_, c)| c.0 > 0)
        .map(|(p, c)| (p as PredId, *c))
        .collect();
    let mut ok = store.triple_count() == stats.triples
        && store.num_predicates() == live.len()
        && store.distinct_subjects() == stats.distinct_subj as usize
        && store.distinct_objects() == stats.distinct_obj as usize
        && store.predicates() == live.iter().map(|(p, _)| *p).collect::<Vec<_>>();
    for (p, (n, ds, dob)) in &live {
        ok &= store.pred_card(*p) == *n as usize
            && store.pred_distinct_subj(*p) == *ds as usize
            && store.pred_distinct_obj(*p) == *dob as usize;
    }
    ok &= store.get_meta(META_LAYOUT_KEY)?.as_deref() == Some(LAYOUT_VERSION);
    if !ok {
        return Err(GStoreError::Database(format!(
            "bulk build: the ingested RocksDB store in '{}' does not reopen with the counts \
             it was built with ({} triples, {} predicates; it reports {} and {})",
            store_dir.display(),
            stats.triples,
            live.len(),
            store.triple_count(),
            store.num_predicates()
        )));
    }
    Ok(())
}

/// `fsync` every file directly in `dir` and the directory itself; the
/// total size of the files.
fn sync_tree(dir: &Path) -> Result<u64> {
    let mut bytes = 0u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_file() {
            bytes += meta.len();
            File::open(entry.path())?.sync_all()?;
        }
    }
    sync_dir(dir)?;
    Ok(bytes)
}

// ---- build + install ----------------------------------------------------------

/// Build a RocksDB database in `dir` (see [`crate::bulk::build`]).
pub(crate) fn build_rocksdb(
    dir: &Path,
    name: &str,
    input: &BulkInput<'_>,
    opts: &ImportOptions,
    log: &mut dyn ErrorLog,
    bulk: &BulkOptions,
) -> Result<BulkReport> {
    let started = Instant::now();
    fs::create_dir_all(dir)?;
    let _guard = BuildGuard::enter(dir)?;
    if DirLock::held_here(dir) {
        // A database of this process has the directory open: replacing its
        // files under it would strand its later writes.
        return Err(GStoreError::Database(format!(
            "database directory '{}' is open in this process; close it before rebuilding it",
            dir.display()
        )));
    }
    // Another process holding the directory (a server, a tool) is refused
    // here; an older binary serving the RocksDB store (it takes only the
    // store's own lock, not the directory's) is refused below, through
    // that lock.
    let _lock = DirLock::acquire(dir)?;
    // Finish a swap a crash interrupted (as every open does) before looking
    // at the store: its `rocksdb.new` may be the committed database.
    fsutil::recover_dir_swap(&dir.join(ROCKS_SUBDIR))?;
    refuse_open_store(dir)?;
    let staging = dir.join(STAGING_DIR);
    if staging.exists() {
        // What a crashed build left: never part of a database.
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    if let Some(t) = &bulk.temp_dir {
        fs::create_dir_all(t)?;
        scratch::sweep_stale(t);
    }
    let threads = bulk.effective_threads();
    let budget = Budget::new(bulk.memory_budget);
    let scratch = Scratch::new(bulk.temp_dir.as_deref().unwrap_or(&staging));
    let ctx = Ctx {
        budget: &budget,
        scratch: &scratch,
        threads,
        tuning: RocksTuning::from_env(),
        split: std::env::var(SST_SPLIT_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(SST_SPLIT_BYTES),
    };
    let built = build_store(&staging, name, input, opts, log, &ctx);
    drop(scratch);
    let img = match built {
        Ok(img) => img,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    let t = Instant::now();
    let mut fp = Failpoints::load(dir);
    let warnings = match install(dir, &staging, &mut fp) {
        Ok(w) => w,
        Err(e) => {
            if !fp.crashed {
                let _ = fs::remove_dir_all(&staging);
            }
            return Err(e);
        }
    };
    let mut phases = img.phases;
    phases.push(PhaseTime {
        name: "install",
        millis: millis(t),
    });
    Ok(BulkReport {
        import: img.import,
        triples: img.triples,
        quads: img.quads,
        entities: img.entities,
        literals: img.literals,
        predicates: img.predicates,
        vstree_entities: 0,
        kv_bytes: img.store_bytes,
        vstree_bytes: 0,
        term_map_bytes: img.term_map_bytes as u64,
        budget_bytes: budget.total() as u64,
        budget_peak_bytes: budget.peak() as u64,
        spilled_runs: budget.spilled_runs(),
        spilled_bytes: budget.spilled_bytes(),
        threads,
        phases,
        millis: millis(started),
        warnings,
    })
}

/// Record `r`'s error, if any, as a warning about the cleanup step `what`.
fn note(warnings: &mut Vec<String>, what: &str, r: Result<()>) {
    if let Err(e) = r {
        warnings.push(format!("{what}: {e}"));
    }
}

/// The install steps ([`super::ROCKS_CRASH_POINTS`]). An error before the
/// commit point leaves the old database and is returned; from there on the
/// remaining steps all run and their errors come back as warnings.
fn install(dir: &Path, staging: &Path, fp: &mut Failpoints) -> Result<Vec<String>> {
    let staged = staging.join(ROCKS_SUBDIR);
    fp.hit("staged")?;
    // Again, right before the swap: an older binary that opened the store
    // while the input was parsed and sorted (it takes no directory lock)
    // would keep writing to the replaced files and lose every write.
    refuse_open_store(dir)?;

    let target = dir.join(ROCKS_SUBDIR);
    let mut warnings = Vec::new();
    if let Err(e) = fsutil::install_staged_dir(&staged, &target) {
        // Where did the swap stop? `staged` → `rocksdb.new` → (`rocksdb` →
        // `rocksdb.old`) → `rocksdb`.
        if staged.exists() {
            return Err(e);
        }
        let new = fsutil::with_suffix(&target, ".new");
        if new.is_dir() && target.is_dir() {
            // The old store is still in place (the next open would discard
            // the new one): undo.
            let _ = remove_dir_if_exists(&new);
            return Err(e);
        }
        // The new store is `rocksdb/`, or `rocksdb.new` which every open
        // moves into place: the commit point is passed.
        warnings.push(format!("swapping the new store in: {e}"));
        note(
            &mut warnings,
            "completing the swap",
            fsutil::recover_dir_swap(&target),
        );
    }
    fp.hit("swapped")?;

    // The replaced database's reasoning rules: their ledgers name the old
    // data's triples.
    note(
        &mut warnings,
        "removing the replaced database's reasoning rules",
        remove_rule_dir(dir),
    );
    fp.hit("rules-removed")?;

    note(
        &mut warnings,
        "removing the other layouts",
        remove_other_layouts(dir, StorageKind::RocksDb),
    );
    fp.hit("layouts-removed")?;

    note(
        &mut warnings,
        STAGING_CLEANUP,
        fs::remove_dir_all(staging)
            .map_err(GStoreError::from)
            .and_then(|()| sync_dir(dir)),
    );
    fp.hit("cleaned")?;
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_count_equal_neighbours() {
        let mut r = Runs::new();
        let mut out = Vec::new();
        for k in [1, 1, 2, 3, 3, 3] {
            out.extend(r.next(k));
        }
        out.extend(r.finish());
        assert_eq!(out, vec![(1, 2), (2, 1), (3, 3)]);
    }

    #[test]
    fn graph_keys_follow_the_memory_build() {
        let mut buf = Vec::new();
        assert!(render_graph(&Some(Term::iri("http://g")), &mut buf));
        assert_eq!(buf, Term::iri("http://g").dict_key().as_bytes());
        assert!(render_graph(&Some(Term::blank("x")), &mut buf));
        assert_eq!(
            buf,
            Term::iri(format!("{BNODE_GRAPH_PREFIX}x"))
                .dict_key()
                .as_bytes()
        );
        assert!(!render_graph(&None, &mut buf));
        assert!(!render_graph(&Some(Term::plain_literal("l")), &mut buf));
    }

    #[test]
    fn interning_checks_only_the_id_space() {
        let mut map = TermMap::new();
        // A key far over the disk layout's 2038-byte limit is fine here.
        let long = vec![b'x'; 10_000];
        assert_eq!(intern(&mut map, Space::Entity, &long).unwrap(), 0);
        assert_eq!(intern(&mut map, Space::Entity, b"<a>").unwrap(), 1);
        assert_eq!(intern(&mut map, Space::Entity, &long).unwrap(), 0);
        let mut lits = TermMap::new();
        assert_eq!(
            intern(&mut lits, Space::Literal, b"\"l\"").unwrap(),
            LITERAL_FIRST_ID
        );
    }
}
