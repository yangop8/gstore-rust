//! Statistics taps on the sorted streams: the counters and per-predicate
//! records the incremental writers maintain (`triple_count`, distinct
//! subjects and objects, `pre2num` / `pre2sub` / `pre2obj`, the quad count),
//! computed as the SPO, POS, OSP and GSPO streams pass by.

use crate::model::id::{EntityLiteralId, PredId};

/// The statistics of one build (see the module docs).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Stats {
    pub(crate) triples: u64,
    pub(crate) quads: u64,
    /// Per predicate id: `(num, distinct subjects, distinct objects)`.
    pub(crate) preds: Vec<(u32, u32, u32)>,
    pub(crate) distinct_subj: u32,
    pub(crate) distinct_obj: u32,
    last_spo: Option<[u32; 3]>,
    last_pos: Option<[u32; 3]>,
    last_osp: Option<[u32; 3]>,
}

impl Stats {
    pub(crate) fn new(predicates: usize) -> Stats {
        Stats {
            preds: vec![(0, 0, 0); predicates],
            ..Stats::default()
        }
    }

    /// The next SPO triple (ascending, unique).
    #[inline]
    pub(crate) fn on_spo(&mut self, s: EntityLiteralId, p: PredId, o: EntityLiteralId) {
        self.triples += 1;
        let st = &mut self.preds[p as usize];
        st.0 += 1;
        match self.last_spo {
            Some([ls, lp, _]) if ls == s => {
                if lp != p {
                    st.1 += 1;
                }
            }
            _ => {
                self.distinct_subj += 1;
                st.1 += 1;
            }
        }
        self.last_spo = Some([s, p, o]);
    }

    /// The next POS triple (ascending, unique).
    #[inline]
    pub(crate) fn on_pos(&mut self, p: PredId, o: EntityLiteralId, s: EntityLiteralId) {
        if !matches!(self.last_pos, Some([lp, lo, _]) if lp == p && lo == o) {
            self.preds[p as usize].2 += 1;
        }
        self.last_pos = Some([p, o, s]);
    }

    /// The next OSP triple (ascending, unique).
    #[inline]
    pub(crate) fn on_osp(&mut self, o: EntityLiteralId, s: EntityLiteralId, p: PredId) {
        if !matches!(self.last_osp, Some([lo, _, _]) if lo == o) {
            self.distinct_obj += 1;
        }
        self.last_osp = Some([o, s, p]);
    }

    /// Fold in the per-predicate distinct-object counts a separate POS pass
    /// computed (the POS tree may be written on another thread).
    pub(crate) fn merge_pos(&mut self, pos: &Stats) {
        for (mine, theirs) in self.preds.iter_mut().zip(&pos.preds) {
            mine.2 += theirs.2;
        }
    }

    /// The `pred_stats` records: `(pred, [num][dsub][dobj] LE)` for every
    /// predicate with at least one triple.
    pub(crate) fn pred_records(&self) -> Vec<(u32, [u8; 12])> {
        self.preds
            .iter()
            .enumerate()
            .filter(|(_, st)| st.0 > 0)
            .map(|(p, &(n, ds, dob))| {
                let mut rec = [0u8; 12];
                rec[0..4].copy_from_slice(&n.to_le_bytes());
                rec[4..8].copy_from_slice(&ds.to_le_bytes());
                rec[8..12].copy_from_slice(&dob.to_le_bytes());
                (p as u32, rec)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn taps_equal_set_counts() {
        let mut triples: Vec<[u32; 3]> = Vec::new();
        for i in 0..500u32 {
            triples.push([i % 37, i % 5, (i * 7) % 61]);
        }
        triples.sort_unstable();
        triples.dedup();
        let mut st = Stats::new(5);
        for &[s, p, o] in &triples {
            st.on_spo(s, p, o);
        }
        let mut pos: Vec<[u32; 3]> = triples.iter().map(|&[s, p, o]| [p, o, s]).collect();
        pos.sort_unstable();
        let mut pst = Stats::new(5);
        for [p, o, s] in pos {
            pst.on_pos(p, o, s);
        }
        st.merge_pos(&pst);
        let mut osp: Vec<[u32; 3]> = triples.iter().map(|&[s, p, o]| [o, s, p]).collect();
        osp.sort_unstable();
        for [o, s, p] in osp {
            st.on_osp(o, s, p);
        }
        assert_eq!(st.triples, triples.len() as u64);
        let subs: HashSet<u32> = triples.iter().map(|t| t[0]).collect();
        let objs: HashSet<u32> = triples.iter().map(|t| t[2]).collect();
        assert_eq!(st.distinct_subj as usize, subs.len());
        assert_eq!(st.distinct_obj as usize, objs.len());
        for p in 0..5u32 {
            let of_p: Vec<&[u32; 3]> = triples.iter().filter(|t| t[1] == p).collect();
            let ds: HashSet<u32> = of_p.iter().map(|t| t[0]).collect();
            let dob: HashSet<u32> = of_p.iter().map(|t| t[2]).collect();
            assert_eq!(
                st.preds[p as usize],
                (of_p.len() as u32, ds.len() as u32, dob.len() as u32)
            );
        }
        assert_eq!(st.pred_records().len(), 5);
    }
}
