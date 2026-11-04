//! VS-tree signatures from the sorted triple streams.
//!
//! An entity's signature is the union of `encode_edge(p, o, Out)` over its
//! out-edges and `encode_edge(p, s, In)` over its in-edges — exactly what the
//! incremental build computes from `for_each_entity_adjacency`. The out
//! halves come from the SPO stream (grouped by subject) and are spooled in id
//! order; the in halves come from the OSP stream (grouped by entity object)
//! and are OR-joined with the spool by id. Each joined signature becomes a
//! [`SigRec`] in an external sorter ordered by (limbs, id) — the VS-tree
//! clustering order — from which `vstree.kv` is written streaming.

use crate::error::Result;
use crate::model::id::{is_literal_id, EntityLiteralId, PredId};
use crate::signature::{EdgeDir, Signature, SIG_BYTES};

use super::budget::Budget;
use super::extsort::{ExtSorter, Record, Spool, SpoolReader};
use super::scratch::Scratch;

/// 64-bit limbs of a signature.
pub(crate) const SIG_LIMBS: usize = SIG_BYTES / 8;

fn to_limbs(sig: &Signature) -> [u64; SIG_LIMBS] {
    let mut b = [0u8; SIG_BYTES];
    sig.write_le(&mut b);
    let mut l = [0u64; SIG_LIMBS];
    for (i, w) in l.iter_mut().enumerate() {
        *w = u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    }
    l
}

fn from_limbs(l: &[u64; SIG_LIMBS]) -> Signature {
    let mut b = [0u8; SIG_BYTES];
    for (i, w) in l.iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    Signature::read_le(&b)
}

fn put_limbs(l: &[u64; SIG_LIMBS], out: &mut [u8]) {
    for (i, w) in l.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
}

fn get_limbs(b: &[u8]) -> [u64; SIG_LIMBS] {
    let mut l = [0u64; SIG_LIMBS];
    for (i, w) in l.iter_mut().enumerate() {
        *w = u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    }
    l
}

/// A signature in the VS-tree clustering order: the raw limbs compared as
/// numbers, low limb first (`Signature::cmp_limbs` — the encoders never set
/// the padding bits past the signature width), then the id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SigRec {
    limbs: [u64; SIG_LIMBS],
    id: u32,
}

impl SigRec {
    /// The `(id, signature)` entry of `vstree.kv`.
    pub(crate) fn entry(&self) -> (EntityLiteralId, Signature) {
        (self.id, from_limbs(&self.limbs))
    }
}

impl Record for SigRec {
    const BYTES: usize = SIG_BYTES + 4;
    fn put(&self, out: &mut [u8]) {
        put_limbs(&self.limbs, out);
        out[SIG_BYTES..SIG_BYTES + 4].copy_from_slice(&self.id.to_le_bytes());
    }
    fn get(b: &[u8]) -> Self {
        SigRec {
            limbs: get_limbs(b),
            id: u32::from_le_bytes(b[SIG_BYTES..SIG_BYTES + 4].try_into().unwrap()),
        }
    }
}

/// An out-half signature keyed by entity id (spooled in id order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct IdSig {
    id: u32,
    limbs: [u64; SIG_LIMBS],
}

impl Record for IdSig {
    const BYTES: usize = SIG_BYTES + 4;
    fn put(&self, out: &mut [u8]) {
        out[0..4].copy_from_slice(&self.id.to_le_bytes());
        put_limbs(&self.limbs, &mut out[4..]);
    }
    fn get(b: &[u8]) -> Self {
        IdSig {
            id: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            limbs: get_limbs(&b[4..]),
        }
    }
}

/// Collects the out-halves from the SPO stream.
pub(crate) struct OutSigs<'a> {
    spool: Spool<'a, IdSig>,
    cur: Option<(EntityLiteralId, Signature)>,
}

impl<'a> OutSigs<'a> {
    pub(crate) fn new(budget: &'a Budget, scratch: &'a Scratch, share: usize) -> OutSigs<'a> {
        OutSigs {
            spool: Spool::new("outsig", budget, scratch, share),
            cur: None,
        }
    }

    /// The next SPO triple (ascending).
    #[inline]
    pub(crate) fn add(&mut self, s: EntityLiteralId, p: PredId, o: EntityLiteralId) -> Result<()> {
        match &mut self.cur {
            Some((id, sig)) if *id == s => sig.encode_edge(p, o, EdgeDir::Out),
            _ => {
                self.flush()?;
                let mut sig = Signature::new();
                sig.encode_edge(p, o, EdgeDir::Out);
                self.cur = Some((s, sig));
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if let Some((id, sig)) = self.cur.take() {
            self.spool.push(IdSig {
                id,
                limbs: to_limbs(&sig),
            })?;
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<SpoolReader<'a, IdSig>> {
        self.flush()?;
        self.spool.finish()
    }
}

/// Joins the in-halves from the OSP stream with the spooled out-halves and
/// sorts the result into clustering order.
pub(crate) struct JoinSigs<'a> {
    out: SpoolReader<'a, IdSig>,
    out_head: Option<IdSig>,
    cur: Option<(EntityLiteralId, Signature)>,
    sorter: ExtSorter<'a, SigRec>,
    entities: u64,
}

impl<'a> JoinSigs<'a> {
    pub(crate) fn new(
        mut out: SpoolReader<'a, IdSig>,
        budget: &'a Budget,
        scratch: &'a Scratch,
        share: usize,
    ) -> Result<JoinSigs<'a>> {
        let out_head = out.next()?;
        Ok(JoinSigs {
            out,
            out_head,
            cur: None,
            sorter: ExtSorter::new("sig", budget, scratch, share),
            entities: 0,
        })
    }

    /// The next OSP triple (ascending). Literal objects are skipped.
    #[inline]
    pub(crate) fn add(&mut self, o: EntityLiteralId, s: EntityLiteralId, p: PredId) -> Result<()> {
        if is_literal_id(o) {
            return Ok(());
        }
        match &mut self.cur {
            Some((id, sig)) if *id == o => sig.encode_edge(p, s, EdgeDir::In),
            _ => {
                self.flush_cur()?;
                let mut sig = Signature::new();
                sig.encode_edge(p, s, EdgeDir::In);
                self.cur = Some((o, sig));
            }
        }
        Ok(())
    }

    fn emit(&mut self, id: EntityLiteralId, limbs: [u64; SIG_LIMBS]) -> Result<()> {
        self.entities += 1;
        self.sorter.push(SigRec { limbs, id })
    }

    /// Emit the spooled out-halves below `id` alone, and return the one at
    /// `id` if there is one.
    fn advance_out(&mut self, id: EntityLiteralId) -> Result<Option<[u64; SIG_LIMBS]>> {
        while let Some(h) = self.out_head {
            if h.id > id {
                break;
            }
            self.out_head = self.out.next()?;
            if h.id == id {
                return Ok(Some(h.limbs));
            }
            self.emit(h.id, h.limbs)?;
        }
        Ok(None)
    }

    fn flush_cur(&mut self) -> Result<()> {
        if let Some((id, sig)) = self.cur.take() {
            let mut limbs = to_limbs(&sig);
            if let Some(out) = self.advance_out(id)? {
                for (a, b) in limbs.iter_mut().zip(out) {
                    *a |= b;
                }
            }
            self.emit(id, limbs)?;
        }
        Ok(())
    }

    /// Every entity's signature, sorted by (limbs, id); and the entity count.
    pub(crate) fn finish(mut self) -> Result<(ExtSorter<'a, SigRec>, u64)> {
        self.flush_cur()?;
        self.advance_out(EntityLiteralId::MAX)?;
        Ok((self.sorter, self.entities))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_order_like_cmp_limbs() {
        let mut a = Signature::new();
        a.encode_edge(3, 9, EdgeDir::Out);
        let mut b = Signature::new();
        b.encode_edge(4, 1, EdgeDir::In);
        let ra = SigRec {
            limbs: to_limbs(&a),
            id: 7,
        };
        let rb = SigRec {
            limbs: to_limbs(&b),
            id: 2,
        };
        assert_eq!(ra.cmp(&rb), a.cmp_limbs(&b).then(7.cmp(&2)));
        let mut buf = [0u8; 128];
        ra.put(&mut buf);
        assert_eq!(SigRec::get(&buf), ra);
        assert_eq!(ra.entry(), (7, a));
        let s = IdSig {
            id: 5,
            limbs: to_limbs(&b),
        };
        s.put(&mut buf);
        assert_eq!(IdSig::get(&buf), s);
    }

    /// The join equals signing each entity from its adjacency lists.
    #[test]
    fn join_equals_per_entity_signing() {
        let dir = std::env::temp_dir().join(format!("gstore_sigs_ut_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let scratch = Scratch::new(&dir);
        let budget = Budget::new(1 << 20);
        let lit = crate::model::id::LITERAL_FIRST_ID;
        let mut triples = vec![
            [1u32, 0, 2],
            [1, 1, lit],
            [2, 0, 5],
            [7, 2, 1],
            [9, 0, lit + 3],
            [4, 1, 9],
        ];
        triples.sort_unstable();
        // Tiny shares so the spool and the sorter spill.
        let mut out = OutSigs::new(&budget, &scratch, 0);
        for &[s, p, o] in &triples {
            out.add(s, p, o).unwrap();
        }
        let mut join = JoinSigs::new(out.finish().unwrap(), &budget, &scratch, 0).unwrap();
        let mut osp: Vec<[u32; 3]> = triples.iter().map(|&[s, p, o]| [o, s, p]).collect();
        osp.sort_unstable();
        for [o, s, p] in osp {
            join.add(o, s, p).unwrap();
        }
        let (sorter, n) = join.finish().unwrap();
        let mut sorted = sorter.finish().unwrap();
        let mut got = Vec::new();
        while let Some(r) = sorted.next().unwrap() {
            got.push(r.entry());
        }
        let mut want = Vec::new();
        let mut ids: Vec<u32> = triples
            .iter()
            .flat_map(|t| [t[0], t[2]])
            .filter(|&x| !is_literal_id(x))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        for e in ids {
            let mut sig = Signature::new();
            for &[s, p, o] in &triples {
                if s == e {
                    sig.encode_edge(p, o, EdgeDir::Out);
                }
                if o == e {
                    sig.encode_edge(p, s, EdgeDir::In);
                }
            }
            want.push((e, sig));
        }
        want.sort_unstable_by(|a, b| a.1.cmp_limbs(&b.1).then(a.0.cmp(&b.0)));
        assert_eq!(n as usize, want.len());
        assert_eq!(got, want);
        drop(sorted);
        drop(scratch);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
