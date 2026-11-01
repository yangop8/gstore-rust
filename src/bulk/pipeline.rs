//! The bulk build pipeline: input → `kvstore.kv` and `vstree.kv` page images.
//!
//! 1. **parse + encode** — the input is parsed ([`source`](super::source))
//!    and encoded ([`Encoder`]) into the term maps and the SPO / GSPO
//!    sorters.
//! 2. **spo** — the merged SPO stream is written as the SPO tree while it
//!    feeds the statistics, the POS and OSP sorters and the out-signature
//!    spool; then the GSPO stream becomes the GSPO tree.
//! 3. **index** — three independent tasks, on three threads when `threads`
//!    allows, each into its own pre-assigned page range: the POS tree
//!    (per-predicate distinct objects), the OSP tree (distinct objects, the
//!    signature join and `vstree.kv`), and the dictionary (`*2id` trees and
//!    `id2*` arrays, placed last in the file because its size is only known
//!    once its keys are sorted).
//! 4. **finish** — the per-predicate statistics array, the header (page 0),
//!    one `fsync`.
//!
//! Page ranges are assigned in a fixed order and every task writes only its
//! own, so the file is byte-identical whatever the thread count.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::Instant;

use crate::error::Result;
use crate::kvstore::format::{
    self, ENTITY2ID, GSPO, ID2ENTITY, ID2LITERAL, ID2PREDICATE, LITERAL2ID, NROOTS, OSP, POS,
    PREDICATE2ID, PRED_STATS, ROOT_DISTINCT_OBJ, ROOT_DISTINCT_SUBJ, ROOT_ENTITY_COUNT,
    ROOT_LITERAL_COUNT, ROOT_PRED_COUNT, ROOT_QUAD_COUNT, ROOT_TRIPLE_COUNT, ROOT_VSTREE_STALE,
    SPO,
};
use crate::kvstore::store::{key3, key4};
use crate::kvstore::PageId;
use crate::model::id::LITERAL_FIRST_ID;
use crate::parser::import::{ErrorLog, ImportOptions, ImportReport};
use crate::signature::VsTree;

use super::budget::Budget;
use super::encode::{Dict, Encoder, Space};
use super::extsort::{ExtSorter, Sorted};
use super::kvimage::{
    fixed_tree_pages, header_page, ivarray_pages, write_header, write_ivarray, FixedTreeWriter,
    PageAlloc, PageOut, VarTreePlan,
};
use super::scratch::Scratch;
use super::sigs::{IdSig, JoinSigs, OutSigs};
use super::source;
use super::stats::Stats;
use super::{BulkInput, PhaseTime};

/// What the pipeline needs besides its input.
pub(crate) struct Ctx<'a> {
    pub(crate) budget: &'a Budget,
    pub(crate) scratch: &'a Scratch,
    pub(crate) fill_percent: u8,
    pub(crate) threads: usize,
}

/// What the pipeline wrote.
#[derive(Debug, Default)]
pub(crate) struct Image {
    pub(crate) import: ImportReport,
    pub(crate) triples: u64,
    pub(crate) quads: u64,
    pub(crate) entities: u64,
    pub(crate) literals: u64,
    pub(crate) predicates: u64,
    pub(crate) vstree_entities: u64,
    pub(crate) kv_pages: u64,
    pub(crate) vstree_pages: u64,
    pub(crate) term_map_bytes: usize,
    pub(crate) phases: Vec<PhaseTime>,
}

fn create(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?)
}

fn phase(phases: &mut Vec<PhaseTime>, name: &'static str, since: Instant) {
    phases.push(PhaseTime {
        name,
        millis: since.elapsed().as_secs_f64() * 1e3,
    });
}

/// Run three tasks, concurrently when `parallel`; results in task order.
fn run3<A: Send, B: Send, C: Send>(
    parallel: bool,
    a: impl FnOnce() -> A + Send,
    b: impl FnOnce() -> B + Send,
    c: impl FnOnce() -> C,
) -> (A, B, C) {
    if !parallel {
        let ra = a();
        let rb = b();
        return (ra, rb, c());
    }
    std::thread::scope(|s| {
        let ha = s.spawn(a);
        let hb = s.spawn(b);
        let rc = c();
        let ra = ha.join().unwrap_or_else(|p| std::panic::resume_unwind(p));
        let rb = hb.join().unwrap_or_else(|p| std::panic::resume_unwind(p));
        (ra, rb, rc)
    })
}

/// Build the page images of `kv_path` and `vs_path` from `input`.
pub(crate) fn build_image(
    kv_path: &Path,
    vs_path: &Path,
    input: &BulkInput<'_>,
    opts: &ImportOptions,
    log: &mut dyn ErrorLog,
    ctx: &Ctx<'_>,
) -> Result<Image> {
    let (budget, scratch, fill) = (ctx.budget, ctx.scratch, ctx.fill_percent);
    let mut img = Image::default();

    // 1. Parse and encode.
    let t = Instant::now();
    let mut enc = Encoder::new(
        ExtSorter::new("spo", budget, scratch, budget.share(2)),
        ExtSorter::new("gspo", budget, scratch, budget.share(2)),
    );
    img.import = source::import(input, opts, log, ctx.threads, &mut |b: &mut Vec<
        crate::parser::nquads::Quad,
    >| enc.add(b))?;
    let (mut dict, spo, gspo) = enc.into_parts();
    img.term_map_bytes = dict.heap_bytes();
    dict.entities.drop_table();
    dict.literals.drop_table();
    dict.predicates.drop_table();
    img.entities = dict.entities.len() as u64;
    img.literals = dict.literals.len() as u64;
    img.predicates = dict.predicates.len() as u64;
    phase(&mut img.phases, "parse+encode", t);

    // 2. SPO (feeding POS, OSP, the statistics and the out-signatures), GSPO.
    let t = Instant::now();
    let file = create(kv_path)?;
    let mut alloc = PageAlloc::new();
    let mut stats = Stats::new(dict.predicates.len());
    let share3 = budget.share(3);
    let mut pos = ExtSorter::<[u32; 3]>::new("pos", budget, scratch, share3);
    let mut osp = ExtSorter::<[u32; 3]>::new("osp", budget, scratch, share3);
    let mut outsig = OutSigs::new(budget, scratch, share3);
    let mut roots = [0u64; NROOTS];
    {
        let mut out = PageOut::new(&file);
        let mut sorted = spo.finish()?;
        let mut w = FixedTreeWriter::new(alloc.next(), 12, fill);
        while let Some([s, p, o]) = sorted.next()? {
            w.push(&mut out, &key3(s, p, o))?;
            stats.on_spo(s, p, o);
            pos.push([p, o, s])?;
            osp.push([o, s, p])?;
            outsig.add(s, p, o)?;
        }
        drop(sorted);
        let (root, pages) = w.finish(&mut out)?;
        alloc.reserve(pages)?;
        roots[SPO] = u64::from(root);

        let mut sorted = gspo.finish()?;
        let mut w = FixedTreeWriter::new(alloc.next(), 16, fill);
        while let Some([g, s, p, o]) = sorted.next()? {
            w.push(&mut out, &key4(g, s, p, o))?;
        }
        stats.quads = w.keys();
        let (root, pages) = w.finish(&mut out)?;
        alloc.reserve(pages)?;
        roots[GSPO] = u64::from(root);
        out.flush()?;
    }
    let outsig = outsig.finish()?;
    phase(&mut img.phases, "spo", t);

    // 3. POS / OSP + VS-tree / dictionary, each in its own page range.
    let t = Instant::now();
    let n = stats.triples;
    let pos_base = alloc.reserve(fixed_tree_pages(n, 12, fill))?;
    let osp_base = alloc.reserve(fixed_tree_pages(n, 12, fill))?;
    let pred_records = stats.pred_records();
    let ps_pages = ivarray_pages(pred_records.iter().map(|(p, r)| (*p, r.len())));
    let ps_base = alloc.reserve(ps_pages)?;
    let dict_base = alloc.next();
    let npreds = dict.predicates.len();
    let file_ref = &file;
    let dict_ref = &dict;
    let parallel = ctx.threads > 1;
    let (pos_r, osp_r, dict_r) = run3(
        parallel,
        move || -> Result<(PageId, Stats, f64)> {
            let t = Instant::now();
            let sorted = pos.finish()?;
            let (root, st) = write_pos(file_ref, pos_base, sorted, npreds, fill)?;
            Ok((root, st, t.elapsed().as_secs_f64() * 1e3))
        },
        move || -> Result<(PageId, u32, (u64, u64), f64)> {
            let t = Instant::now();
            let sorted = osp.finish()?;
            let r = write_osp(
                file_ref, osp_base, sorted, outsig, vs_path, budget, scratch, fill,
            )?;
            Ok((r.0, r.1, r.2, t.elapsed().as_secs_f64() * 1e3))
        },
        move || -> Result<(DictRoots, f64)> {
            let t = Instant::now();
            let r = write_dict(file_ref, dict_base, dict_ref, fill)?;
            Ok((r, t.elapsed().as_secs_f64() * 1e3))
        },
    );
    let (pos_root, pos_stats, pos_ms) = pos_r?;
    let (osp_root, distinct_obj, vs, osp_ms) = osp_r?;
    let (dr, dict_ms) = dict_r?;
    phase(&mut img.phases, "index", t);
    img.phases.push(PhaseTime {
        name: "  pos",
        millis: pos_ms,
    });
    img.phases.push(PhaseTime {
        name: "  osp+vstree",
        millis: osp_ms,
    });
    img.phases.push(PhaseTime {
        name: "  dictionary",
        millis: dict_ms,
    });
    stats.merge_pos(&pos_stats);
    stats.distinct_obj = distinct_obj;
    roots[POS] = u64::from(pos_root);
    roots[OSP] = u64::from(osp_root);

    // 4. Statistics array, header, fsync.
    let t = Instant::now();
    {
        let mut out = PageOut::new(&file);
        let records = stats.pred_records();
        let (root, used) = write_ivarray(
            &mut out,
            ps_base,
            records.iter().map(|(p, r)| (*p, r.as_slice())),
        )?;
        debug_assert_eq!(used, ps_pages);
        roots[PRED_STATS] = u64::from(root);
        out.flush()?;
    }
    roots[ENTITY2ID] = u64::from(dr.trees[0]);
    roots[LITERAL2ID] = u64::from(dr.trees[1]);
    roots[PREDICATE2ID] = u64::from(dr.trees[2]);
    roots[ID2ENTITY] = u64::from(dr.arrays[0]);
    roots[ID2LITERAL] = u64::from(dr.arrays[1]);
    roots[ID2PREDICATE] = u64::from(dr.arrays[2]);
    roots[ROOT_TRIPLE_COUNT] = stats.triples;
    roots[ROOT_ENTITY_COUNT] = img.entities;
    roots[ROOT_LITERAL_COUNT] = img.literals;
    roots[ROOT_PRED_COUNT] = img.predicates;
    roots[ROOT_DISTINCT_SUBJ] = u64::from(stats.distinct_subj);
    roots[ROOT_DISTINCT_OBJ] = u64::from(stats.distinct_obj);
    roots[ROOT_QUAD_COUNT] = stats.quads;
    // vstree.kv is built from this very data: fresh.
    roots[ROOT_VSTREE_STALE] = 0;
    debug_assert_eq!(roots[format::FEATURES], 0);
    alloc.reserve(dr.pages)?;
    let page_count = alloc.next();
    write_header(&file, &header_page(page_count, 0, &roots))?;
    file.sync_all()?;
    phase(&mut img.phases, "finish", t);

    img.triples = stats.triples;
    img.quads = stats.quads;
    (img.vstree_entities, img.vstree_pages) = vs;
    img.kv_pages = u64::from(page_count);
    Ok(img)
}

/// Write the POS tree; returns its root and the per-predicate distinct
/// objects.
fn write_pos(
    file: &File,
    base: PageId,
    mut sorted: Sorted<'_, [u32; 3]>,
    npreds: usize,
    fill: u8,
) -> Result<(PageId, Stats)> {
    let mut out = PageOut::new(file);
    let mut st = Stats::new(npreds);
    let mut w = FixedTreeWriter::new(base, 12, fill);
    while let Some([p, o, s]) = sorted.next()? {
        w.push(&mut out, &key3(p, o, s))?;
        st.on_pos(p, o, s);
    }
    let (root, _) = w.finish(&mut out)?;
    out.flush()?;
    Ok((root, st))
}

/// Write the OSP tree and, from the joined signatures, `vstree.kv`; returns
/// the OSP root, the distinct-object count and the VS-tree file's numbers.
#[allow(clippy::too_many_arguments)]
fn write_osp<'a>(
    file: &File,
    base: PageId,
    mut sorted: Sorted<'a, [u32; 3]>,
    outsig: super::extsort::SpoolReader<'a, IdSig>,
    vs_path: &Path,
    budget: &'a Budget,
    scratch: &'a Scratch,
    fill: u8,
) -> Result<(PageId, u32, (u64, u64))> {
    let mut out = PageOut::new(file);
    let mut st = Stats::new(0);
    let mut join = JoinSigs::new(outsig, budget, scratch, budget.share(2))?;
    let mut w = FixedTreeWriter::new(base, 12, fill);
    while let Some([o, s, p]) = sorted.next()? {
        w.push(&mut out, &key3(o, s, p))?;
        st.on_osp(o, s, p);
        join.add(o, s, p)?;
    }
    drop(sorted);
    let (root, _) = w.finish(&mut out)?;
    out.flush()?;
    let (sigs, _) = join.finish()?;
    let mut sigs = sigs.finish()?;
    let entries = std::iter::from_fn(|| match sigs.next() {
        Ok(Some(r)) => Some(Ok(r.entry())),
        Ok(None) => None,
        Err(e) => Some(Err(e)),
    });
    let vs = VsTree::write_disk_sorted(vs_path, entries)?;
    Ok((root, st.distinct_obj, vs))
}

/// The roots and size of the dictionary region.
struct DictRoots {
    /// `entity2id`, `literal2id`, `predicate2id`.
    trees: [PageId; 3],
    /// `id2entity`, `id2literal`, `id2predicate`.
    arrays: [PageId; 3],
    pages: u64,
}

/// Write the dictionary trees and arrays from page `base` on.
fn write_dict(file: &File, base: PageId, dict: &Dict, fill: u8) -> Result<DictRoots> {
    let mut out = PageOut::new(file);
    let mut at = base;
    let mut trees = [0; 3];
    let mut arrays = [0; 3];
    let spaces = [Space::Entity, Space::Literal, Space::Predicate];
    for (i, space) in spaces.into_iter().enumerate() {
        let map = dict.map(space);
        let offset = if space == Space::Literal {
            LITERAL_FIRST_ID
        } else {
            0
        };
        let sorted = map.sorted_ids();
        let plan = VarTreePlan::new(sorted.len(), 4, fill, |j| map.key_len(sorted[j]));
        trees[i] = plan.write(
            &mut out,
            at,
            |j| map.key(sorted[j]),
            |j| (sorted[j] + offset).to_be_bytes(),
        )?;
        at += plan.pages() as PageId;
    }
    for (i, space) in spaces.into_iter().enumerate() {
        let map = dict.map(space);
        let n = map.len() as u32;
        let (root, used) = write_ivarray(&mut out, at, (0..n).map(|id| (id, map.key(id))))?;
        arrays[i] = root;
        at += used as PageId;
    }
    out.flush()?;
    Ok(DictRoots {
        trees,
        arrays,
        pages: u64::from(at - base),
    })
}
