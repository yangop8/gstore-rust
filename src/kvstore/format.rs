//! The kvstore format registry: the single owner of every on-disk version
//! marker, header root slot and feature bit of a [`Pager`](super::pager::Pager)
//! file (`kvstore.kv`, and `vstree.kv`, which is a pager file too).
//!
//! No other module invents a magic, a header slot or a layout version: a new
//! structure that needs one claims it here (docs/PHASE6_PLAN.md §3.1).
//!
//! ## Header (page 0)
//! ```text
//! [magic: 8 bytes][page_count: u32 LE][free_head: u32 LE][root slot × NROOTS: u64 LE] [zero …]
//! ```
//! Only bytes `0 .. 16 + NROOTS * 8` are ever non-zero. [`NROOTS`] grew from 24
//! to 32 in Phase 6; a file written with 24 slots reads its extra slots as `0`,
//! and a V1 file written by current code has the very same header bytes as one
//! written before the change (slots 21..32 are zero in a V1 file).
//!
//! ## Versions
//! * [`MAGIC_V1`] (`GSTOREKV`): no optional feature is in use — [`FEATURES`] is 0.
//! * [`MAGIC_V2`] (`GSTOREK2`): at least one feature bit is set. Binaries older
//!   than the registry refuse such a file ("not a gStore KV file"), which is
//!   the intent: they would misread or corrupt the structures behind the bit.
//!
//! Rules (enforced by [`check_header`] on every open):
//! * `FEATURES == 0` ⇔ `MAGIC_V1`. A V2 file without features and a V1 file
//!   with features are both refused as inconsistent.
//! * Unknown bits — anything outside [`SUPPORTED_FEATURES`] of this binary —
//!   are refused.
//! * The first use of a feature sets its bit **and** `MAGIC_V2` in the same
//!   pager flush as the first structure that needs it
//!   ([`Pager::enable_feature`](super::pager::Pager::enable_feature) only
//!   marks the in-memory header; the next committed flush carries both).
//! * `vstree.kv` must have `FEATURES == 0`.
//! * Node, IvArray, overflow and WAL formats do not change in Phase 6.
//!
//! ## Root-slot map of `kvstore.kv`
//! | Slots | Use |
//! |---|---|
//! | 0–20 | SPO/POS/OSP, the six dictionary structures, counters, SP2O_VLIST, COMPACTED, PRED_STATS, distinct counts, GSPO, quad count, VSTREE_STALE |
//! | 21 | [`FEATURES`] bitmask |
//! | 22 | [`FREE_IDS`] B+tree root (R4) |
//! | 23–25 | [`FREE_COUNT_E`] / [`FREE_COUNT_L`] / [`FREE_COUNT_P`] (R4) |
//! | 26–31 | reserved |
//!
//! ## Feature bits
//! | Bit | Feature | Status |
//! |---|---|---|
//! | 0 | [`FEATURE_FREE_IDS`] | R4-RECLAIM (not supported by this binary yet) |
//! | 1 | [`FEATURE_IVARRAY_WIDE`] | reserved (lifts the 33.5M IvArray cap) |
//! | 2 | [`FEATURE_LONG_KEYS`] | reserved (dictionary keys > 4 KB) |
//! | 3–63 | — | reserved |

use crate::error::{GStoreError, Result};

/// Magic of a file that uses no optional feature (every pre-Phase-6 file).
pub const MAGIC_V1: &[u8; 8] = b"GSTOREKV";
/// Magic of a file with at least one feature bit set.
pub const MAGIC_V2: &[u8; 8] = b"GSTOREK2";

/// Number of `u64` root slots in the header.
pub const NROOTS: usize = 32;
/// Root slots that existed before Phase 6. A V1 header written by current code
/// is byte-identical to one written with this many slots.
pub const NROOTS_V1: usize = 24;

// ---- kvstore.kv root slots (DiskStore) ------------------------------------

/// SPO triple index (12-byte `s|p|o` keys).
pub(crate) const SPO: usize = 0;
/// POS triple index (12-byte `p|o|s` keys).
pub(crate) const POS: usize = 1;
/// OSP triple index (12-byte `o|s|p` keys).
pub(crate) const OSP: usize = 2;
/// Entity dictionary key → id (B+tree).
pub(crate) const ENTITY2ID: usize = 3;
/// Literal dictionary key → public literal id (B+tree).
pub(crate) const LITERAL2ID: usize = 4;
/// Predicate dictionary key → id (B+tree).
pub(crate) const PREDICATE2ID: usize = 5;
/// Entity id → key (IvArray).
pub(crate) const ID2ENTITY: usize = 6;
/// Local literal index (id − `LITERAL_FIRST_ID`) → key (IvArray).
pub(crate) const ID2LITERAL: usize = 7;
/// Predicate id → key (IvArray).
pub(crate) const ID2PREDICATE: usize = 8;
/// Default-graph triple count.
pub(crate) const ROOT_TRIPLE_COUNT: usize = 9;
/// Entity id high-water mark (next entity id).
pub(crate) const ROOT_ENTITY_COUNT: usize = 10;
/// Literal index high-water mark.
pub(crate) const ROOT_LITERAL_COUNT: usize = 11;
/// Predicate id high-water mark.
pub(crate) const ROOT_PRED_COUNT: usize = 12;
/// Root of the optional VList-compressed `(sub, pred) → objects` index.
pub(crate) const SP2O_VLIST: usize = 13;
/// `1` once `DiskStore::compact` has built [`SP2O_VLIST`]; cleared by any write.
pub(crate) const ROOT_COMPACTED: usize = 14;
/// Per-predicate statistics IvArray: pred id → `[num][dsub][dobj]` (u32 LE each).
pub(crate) const PRED_STATS: usize = 15;
/// Exact number of distinct subjects.
pub(crate) const ROOT_DISTINCT_SUBJ: usize = 16;
/// Exact number of distinct objects.
pub(crate) const ROOT_DISTINCT_OBJ: usize = 17;
/// Named-graph quad index (16-byte `g|s|p|o` keys).
pub(crate) const GSPO: usize = 18;
/// Number of quads in [`GSPO`].
pub(crate) const ROOT_QUAD_COUNT: usize = 19;
/// `1` once a default-graph mutation made `vstree.kv` stale.
pub(crate) const ROOT_VSTREE_STALE: usize = 20;
/// The feature bitmask (see the module docs). Owned by the pager: it is read
/// and written through [`Pager::features`](super::pager::Pager::features) /
/// [`Pager::enable_feature`](super::pager::Pager::enable_feature), never
/// through `set_root`.
pub const FEATURES: usize = 21;
/// Free-id B+tree root (R4-RECLAIM, behind [`FEATURE_FREE_IDS`]).
#[allow(dead_code)] // reserved: R4-RECLAIM
pub(crate) const FREE_IDS: usize = 22;
/// Number of free entity ids (R4-RECLAIM).
#[allow(dead_code)] // reserved: R4-RECLAIM
pub(crate) const FREE_COUNT_E: usize = 23;
/// Number of free literal ids (R4-RECLAIM).
#[allow(dead_code)] // reserved: R4-RECLAIM
pub(crate) const FREE_COUNT_L: usize = 24;
/// Number of free predicate ids (R4-RECLAIM).
#[allow(dead_code)] // reserved: R4-RECLAIM
pub(crate) const FREE_COUNT_P: usize = 25;

// ---- feature bits -----------------------------------------------------------

/// Bit 0: free-id list in [`FREE_IDS`] / `FREE_COUNT_*` (R4-RECLAIM).
pub const FEATURE_FREE_IDS: u64 = 1 << 0;
/// Bit 1 (reserved, deferred): wider IvArray page tables.
pub const FEATURE_IVARRAY_WIDE: u64 = 1 << 1;
/// Bit 2 (reserved, deferred): dictionary keys longer than one page.
pub const FEATURE_LONG_KEYS: u64 = 1 << 2;
/// Every bit the registry defines (implemented or reserved).
pub const DEFINED_FEATURES: u64 = FEATURE_FREE_IDS | FEATURE_IVARRAY_WIDE | FEATURE_LONG_KEYS;
/// The bits this binary implements and may therefore open or enable. Empty
/// until R4-RECLAIM implements [`FEATURE_FREE_IDS`].
pub const SUPPORTED_FEATURES: u64 = 0;

/// The magic a header with `features` must carry.
pub fn magic_for(features: u64) -> &'static [u8; 8] {
    if features == 0 {
        MAGIC_V1
    } else {
        MAGIC_V2
    }
}

/// Validate a header's `magic` and `features` against the rules above, for a
/// binary that implements `supported`.
pub fn check_header(magic: &[u8], features: u64, supported: u64) -> Result<()> {
    let v1 = magic == MAGIC_V1;
    let v2 = magic == MAGIC_V2;
    if !v1 && !v2 {
        return Err(GStoreError::Database("not a gStore KV file".into()));
    }
    if v1 && features != 0 {
        return Err(GStoreError::Database(format!(
            "corrupt gStore KV header: a version-1 file with feature bits {features:#x}"
        )));
    }
    if v2 && features == 0 {
        return Err(GStoreError::Database(
            "corrupt gStore KV header: a version-2 file without feature bits".into(),
        ));
    }
    let unknown = features & !supported;
    if unknown != 0 {
        return Err(GStoreError::Database(format!(
            "this gStore KV file uses feature bits {unknown:#x} that this binary does not \
             support (written by a newer gStore); open it with that version"
        )));
    }
    Ok(())
}

/// Validate a single feature bit for
/// [`Pager::enable_feature`](super::pager::Pager::enable_feature): exactly one
/// defined bit that `supported` includes.
pub fn check_enable(bit: u64, supported: u64) -> Result<()> {
    if bit.count_ones() != 1 || bit & DEFINED_FEATURES == 0 {
        return Err(GStoreError::Database(format!(
            "{bit:#x} is not a gStore KV feature bit"
        )));
    }
    if bit & supported == 0 {
        return Err(GStoreError::Database(format!(
            "gStore KV feature bit {bit:#x} is not implemented by this binary"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_rules() {
        assert!(check_header(MAGIC_V1, 0, SUPPORTED_FEATURES).is_ok());
        assert!(check_header(b"GSTOREXX", 0, u64::MAX).is_err());
        // V1 with bits, V2 without bits: inconsistent.
        assert!(check_header(MAGIC_V1, FEATURE_FREE_IDS, u64::MAX).is_err());
        assert!(check_header(MAGIC_V2, 0, u64::MAX).is_err());
        // Unknown bits are refused, supported ones accepted.
        assert!(check_header(MAGIC_V2, FEATURE_FREE_IDS, 0).is_err());
        assert!(check_header(MAGIC_V2, FEATURE_FREE_IDS, FEATURE_FREE_IDS).is_ok());
        assert!(check_header(MAGIC_V2, 1 << 40, DEFINED_FEATURES).is_err());
        assert_eq!(magic_for(0), MAGIC_V1);
        assert_eq!(magic_for(FEATURE_LONG_KEYS), MAGIC_V2);
    }

    #[test]
    fn enable_rules() {
        assert!(check_enable(FEATURE_FREE_IDS, FEATURE_FREE_IDS).is_ok());
        assert!(check_enable(FEATURE_FREE_IDS, 0).is_err(), "not implemented yet");
        assert!(check_enable(FEATURE_FREE_IDS | FEATURE_LONG_KEYS, u64::MAX).is_err());
        assert!(check_enable(1 << 9, u64::MAX).is_err(), "undefined bit");
        assert!(check_enable(0, u64::MAX).is_err());
    }

    #[test]
    fn slots_fit_and_are_distinct() {
        let slots = [
            SPO,
            POS,
            OSP,
            ENTITY2ID,
            LITERAL2ID,
            PREDICATE2ID,
            ID2ENTITY,
            ID2LITERAL,
            ID2PREDICATE,
            ROOT_TRIPLE_COUNT,
            ROOT_ENTITY_COUNT,
            ROOT_LITERAL_COUNT,
            ROOT_PRED_COUNT,
            SP2O_VLIST,
            ROOT_COMPACTED,
            PRED_STATS,
            ROOT_DISTINCT_SUBJ,
            ROOT_DISTINCT_OBJ,
            GSPO,
            ROOT_QUAD_COUNT,
            ROOT_VSTREE_STALE,
            FEATURES,
            FREE_IDS,
            FREE_COUNT_E,
            FREE_COUNT_L,
            FREE_COUNT_P,
        ];
        for (i, &s) in slots.iter().enumerate() {
            assert_eq!(s, i, "slots are assigned densely in registry order");
            assert!(s < NROOTS);
        }
        assert!(16 + NROOTS * 8 <= super::super::pager::PAGE_SIZE);
    }
}
