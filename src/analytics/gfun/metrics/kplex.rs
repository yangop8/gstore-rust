//! Port of upstream's `MaxKPX` namespace (temporary stub).

use super::super::GFunCtx;
use super::super::PredFilter;
use crate::error::{GStoreError, Result};

pub(super) fn maximum_kplex(_ctx: &GFunCtx<'_>, _f: &PredFilter, _k: i32) -> Result<Vec<i32>> {
    Err(GStoreError::Query("maximumKplex: not ported yet".into()))
}

pub(super) fn core_truss(
    _ctx: &GFunCtx<'_>,
    _f: &PredFilter,
    _alpha: i32,
    _beta: i32,
) -> Result<Vec<i32>> {
    Err(GStoreError::Query("coreTruss: not ported yet".into()))
}
