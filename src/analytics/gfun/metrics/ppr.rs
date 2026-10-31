//! Port of upstream's FORA single-source PPR (temporary stub).

use super::super::{CsrGraph, GFunCtx, GNode, PredFilter};
use crate::error::{GStoreError, Result};
use crate::model::id::EntityLiteralId;

pub(super) struct FilteredOut;

impl FilteredOut {
    pub(super) fn new(_g: &CsrGraph, _f: &PredFilter) -> FilteredOut {
        FilteredOut
    }
}

pub(super) fn ssppr(
    _ctx: &GFunCtx<'_>,
    _fwd: &FilteredOut,
    _src: &GNode,
    _ret_num: usize,
    _k: i32,
) -> Result<Vec<(EntityLiteralId, f64)>> {
    Err(GStoreError::Query("PPR: not ported yet".into()))
}
