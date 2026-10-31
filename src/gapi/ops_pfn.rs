//! The PFN (personalized function) operations: `funquery`, `funcudb`,
//! `funreview` (upstream `src/Server/ApiPFNOperation.cpp`, the request and
//! response classes in `src/Server/MessageProtocol/MessageApiPFN.*`, the
//! dispatch in `src/Main/grpc.cpp` `fun_*_task`), over the server's
//! [`PfnRegistry`](crate::pfn::PfnRegistry).
//!
//! Every user may call them (upstream grants them to every new user,
//! `API_USAGE.md` "新增的用户默认具备的接口权限"; [`crate::sysdb::requirement`]),
//! and each acts on **the caller's own** functions only — upstream keys
//! everything by the request's `username`. No `db_name` is involved.
//!
//! * `funquery` — `funInfo` (optional: `funName`, `funStatus` filters) →
//!   `{"StatusCode":0,"StatusMsg":"success","list":[PFNInfo…]}`, each item
//!   upstream's six fields. *Rust extension*: with `pageNo` / `pageSize`
//!   the list is one page (in creation order) and the answer adds
//!   `totalSize`, `totalPage`, `pageNo`, `pageSize` like the log operations;
//!   a page past the end is empty.
//! * `funcudb` — `type` `"1"` create, `"2"` update, `"3"` delete, `"4"`
//!   compile; `funInfo` (`funName`, `funDesc`, `funBody`, `funSubs`).
//!   Answers `"Function create|update|delete|build success."`; a failure is
//!   `1005 "Function <op> fail: <reason>"`, except a compilation error,
//!   which is `1005` with the compiler's message alone (upstream returns the
//!   `g++` log as is). Missing `type` → `1003 "the value of type can not be
//!   empty!"`; neither `funName` nor `funBody` → `1003 "Neither funName nor
//!   funBody can be empty!"`; another `type` → `1003 "The type is invalid,
//!   please look up the api document."`.
//! * `funreview` — `funInfo` → `{"StatusCode":0,"StatusMsg":"success",
//!   "result":<the generated module text, URL-encoded>}` (upstream encodes
//!   with `StringUtil::url_encode`; its docs spell the key `Result`, its code
//!   `result`).
//!
//! The `query` operation answers a projected `PFN.name(…)` in upstream's
//! shape ([`PfnShape`]): the document is `{"results": <the value, parsed
//! as JSON when it is JSON>}` plus the status fields.

use super::envelope::{GResp, Status};
use super::params::Params;
use super::{required, Ctx, OpSpec, Registry};
use crate::error::GStoreError;
use crate::json::Json;
use crate::parser::sparql::ast::{Expr, Projection, Query, SelectItem};
use crate::pfn::PfnInfo;
use crate::query::ResultSet;

pub(super) fn register(reg: &mut Registry) {
    reg.add(OpSpec::new("funquery", funquery));
    reg.add(OpSpec::new("funcudb", funcudb));
    reg.add(OpSpec::new("funreview", funreview));
}

/// A registry error's text (without the `query error:` prefix).
fn reason(e: GStoreError) -> String {
    match e {
        GStoreError::Query(m) => m,
        other => other.to_string(),
    }
}

/// The request's `funInfo` (absent = empty).
fn fun_info(p: &Params) -> Result<PfnInfo, GResp> {
    match p.raw("funInfo") {
        None => Ok(PfnInfo::default()),
        Some(v) => PfnInfo::from_json(v).map_err(|m| GResp::error(Status::ParamIsIllegal, m)),
    }
}

const EMPTY_INFO: &str = "Neither funName nor funBody can be empty!";

/// `funquery` — the caller's functions (see the module docs).
fn funquery(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let info = match fun_info(p) {
        Ok(i) => i,
        Err(r) => return r,
    };
    let filter = |s: &str| Some(s.to_string()).filter(|s| !s.is_empty());
    let (name, status) = (filter(&info.name), filter(&info.status));
    let list = match ctx
        .inner
        .pfn
        .list(ctx.user(), name.as_deref(), status.as_deref())
    {
        Ok(l) => l,
        Err(e) => {
            return GResp::error(
                Status::OperationFailed,
                format!("Fun query fail: {}", reason(e)),
            )
        }
    };
    let paged = p.has("pageNo") || p.has("pageSize");
    let mut body = Json::obj()
        .with("StatusCode", 0)
        .with("StatusMsg", "success");
    if paged {
        let bad = |e: String| GResp::error(Status::ParamIsIllegal, e);
        let page_no = match p.int("pageNo") {
            Ok(v) => v.unwrap_or(1).max(1),
            Err(e) => return bad(e),
        };
        let page_size = match p.int("pageSize") {
            Ok(v) => v.filter(|&n| n >= 1).unwrap_or(10),
            Err(e) => return bad(e),
        };
        let size = usize::try_from(page_size).unwrap_or(usize::MAX);
        let skip = usize::try_from(page_no - 1)
            .unwrap_or(usize::MAX)
            .saturating_mul(size);
        let total = list.len();
        let items: Vec<Json> = list
            .iter()
            .skip(skip)
            .take(size)
            .map(PfnInfo::to_json)
            .collect();
        body.set("totalSize", total);
        body.set("totalPage", total.div_ceil(size));
        body.set("pageNo", page_no);
        body.set("pageSize", page_size);
        body.set("list", Json::Arr(items));
    } else {
        body.set(
            "list",
            Json::Arr(list.iter().map(PfnInfo::to_json).collect()),
        );
    }
    GResp::json(body)
}

/// `funcudb` — create / update / delete / compile one of the caller's
/// functions (see the module docs).
fn funcudb(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let kind = match required(p, "type") {
        Ok(t) => t.trim().to_string(),
        Err(r) => return r,
    };
    let info = match fun_info(p) {
        Ok(i) => i,
        Err(r) => return r,
    };
    if info.is_empty() {
        return GResp::error(Status::ParamIsIllegal, EMPTY_INFO);
    }
    let user = ctx.user().to_string();
    let reg = &ctx.inner.pfn;
    let (op, outcome) = match kind.as_str() {
        "1" => ("create", reg.create(&user, info).map(|_| ())),
        "2" => ("update", reg.update(&user, info).map(|_| ())),
        "3" => ("delete", reg.delete(&user, &info.name)),
        "4" => {
            return match reg.compile(&user, &info.name) {
                Ok(Ok(_)) => GResp::success("Function build success."),
                // Upstream answers the compiler's log as is.
                Ok(Err(log)) => GResp::error(Status::OperationFailed, log),
                Err(e) => GResp::error(
                    Status::OperationFailed,
                    format!("Function build fail: {}", reason(e)),
                ),
            };
        }
        _ => {
            return GResp::error(
                Status::ParamIsIllegal,
                "The type is invalid, please look up the api document.",
            )
        }
    };
    match outcome {
        Ok(()) => GResp::success(&format!("Function {op} success.")),
        Err(e) => GResp::error(
            Status::OperationFailed,
            format!("Function {op} fail: {}", reason(e)),
        ),
    }
}

/// `funreview` — the module text `funInfo` compiles from, URL-encoded.
fn funreview(_ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let info = match fun_info(p) {
        Ok(i) => i,
        Err(r) => return r,
    };
    if info.is_empty() {
        return GResp::error(Status::ParamIsIllegal, EMPTY_INFO);
    }
    let source = crate::pfn::generate_source(&info);
    GResp::json(
        Json::obj()
            .with("StatusCode", 0)
            .with("StatusMsg", "success")
            .with("result", crate::pfn::url_encode(&source)),
    )
}

/// How a `query` whose SELECT projects `PFN.name(…)` is answered, as
/// upstream's `ResultSet::to_JSON` does (ResultSet.cpp:540-591): the PFN
/// value replaces the whole JSON document with `{"results": <value>}` —
/// the value parsed as JSON when it is JSON, else the text. With several
/// rows, the last shown row's first PFN cell wins (upstream overwrites as it
/// goes); when no shown row has one, the usual `head` / `results` document
/// stays.
pub(super) struct PfnShape {
    /// The result columns holding PFN values.
    cols: Vec<String>,
    /// The statement starts with `select (pfn.` (any case): upstream then
    /// rewrites it to `SELECT (PFN …` and drops `AnsNum`, `OutputLimit` and
    /// `ThreadId` from the answer (`ApiHandler::repalce_pfn_query`,
    /// `MessageQueryResponse::is_pfn`).
    pub(super) bare: bool,
}

impl PfnShape {
    /// The shape of `q` (`sparql` its text), if it projects a PFN call.
    pub(super) fn of(q: &Query, sparql: &str) -> Option<PfnShape> {
        let Query::Select(s) = q else { return None };
        let Projection::Items(items) = &s.projection else {
            return None;
        };
        let cols: Vec<String> = items
            .iter()
            .filter_map(|it| match it {
                SelectItem::Expr(Expr::Pfn { .. }, alias) => Some(alias.clone()),
                _ => None,
            })
            .collect();
        if cols.is_empty() {
            return None;
        }
        let bare =
            sparql.len() >= 13 && sparql.as_bytes()[..12].eq_ignore_ascii_case(b"select (pfn.");
        Some(PfnShape { cols, bare })
    }

    /// The `{"results": …}` document of the first `shown` rows of `rs`, if
    /// one of them has a PFN value.
    pub(super) fn results_json(&self, rs: &ResultSet, shown: Option<usize>) -> Option<Json> {
        let pfn_col: Vec<bool> = rs.vars.iter().map(|v| self.cols.contains(v)).collect();
        let mut data = None;
        for row in rs.rows.iter().take(shown.unwrap_or(usize::MAX)) {
            let cell = row.iter().enumerate().find_map(|(j, c)| {
                c.as_deref()
                    .filter(|c| pfn_col.get(j) == Some(&true) && !c.is_empty())
            });
            if let Some(c) = cell {
                data = Some(c);
            }
        }
        let cell = data?;
        let binding = crate::sparql_results::upstream_binding(cell);
        let text = binding
            .get("value")
            .and_then(Json::as_str)
            .unwrap_or(cell)
            .to_string();
        Some(Json::obj().with("results", crate::pfn::call::output_value(&text)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(q: &str) -> Option<PfnShape> {
        PfnShape::of(&crate::parser::sparql::parse(q).unwrap(), q)
    }

    #[test]
    fn pfn_projections_answer_upstreams_results_document() {
        assert!(shape("SELECT ?s WHERE { ?s ?p ?o }").is_none());
        let s = shape(r#"select (PFN.f({"a":"b"}) AS ?v) WHERE {}"#).unwrap();
        assert!(s.bare);
        let s = shape(r#"SELECT ?s (PFN f({}) AS ?v) WHERE { ?s ?p ?o }"#).unwrap();
        assert!(!s.bare);
        let rs = ResultSet {
            vars: vec!["s".into(), "v".into()],
            rows: vec![
                vec![Some("<http://ex/a>".into()), None],
                vec![
                    Some("<http://ex/b>".into()),
                    Some(r#""[{\"n\":1},\"x\"]""#.into()),
                ],
            ],
        };
        assert_eq!(
            s.results_json(&rs, None).unwrap().to_string(),
            r#"{"results":[{"n":1},"x"]}"#
        );
        // Outside the shown rows: the usual document.
        assert!(s.results_json(&rs, Some(1)).is_none());
        // Not JSON: the text.
        let rs = ResultSet {
            vars: vec!["v".into()],
            rows: vec![vec![Some(r#""plain text""#.into())]],
        };
        let s = shape(r#"SELECT (PFN.f({}) AS ?v) WHERE {}"#).unwrap();
        assert_eq!(
            s.results_json(&rs, None).unwrap().to_string(),
            r#"{"results":"plain text"}"#
        );
    }

    #[test]
    fn pfn_operations_are_registered_for_every_user() {
        use crate::sysdb::{requirement, Requirement};
        for op in ["funquery", "funcudb", "funreview"] {
            let spec = super::super::registry().get(op).unwrap();
            assert!(spec.auth && !spec.needs_db && !spec.async_capable, "{op}");
            assert_eq!(requirement(spec.privilege), Requirement::Anyone, "{op}");
        }
    }
}
