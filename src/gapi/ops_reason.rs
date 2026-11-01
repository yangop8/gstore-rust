//! `reasonManage` — upstream gStore's reasoning-rule management
//! (`src/Server/ApiReasonOperation.cpp`, the response class
//! `MessageReasonManageResponse` in `MessageApiReason.cpp`).
//!
//! One operation, `db_name` plus `type`:
//!
//! | `type` | does | parameters | answer (besides `StatusCode` / `StatusMsg`) |
//! |---|---|---|---|
//! | `1` add | store a rule (`新建`) | `ruleinfo` (a JSON object) | — (`StatusMsg` names the file) |
//! | `2` list | every rule | — | `list` (the rule objects), `num` |
//! | `3` compile | compile (`已编译`) | `rulename` | `insert_sparql`, `delete_sparql`, `check_sparql` |
//! | `4` execute | infer (`已执行`) | `rulename` | `insert_sparql`, `select_sparql`, `AnsNum` (triples added) |
//! | `5` disable | retract (`已失效`) | `rulename` | `delete_sparql`, `AnsNum` (triples removed) |
//! | `6` show | one rule | `rulename` | `ruleinfo` |
//! | `7` delete | remove a rule that is not executed | `rulename` | — |
//! | `8` check | count matches (`已校验`) | `rulename` | `check_sparql`, `effectNum` |
//!
//! Only `root` (and `admin` users) may call it, as upstream
//! (`APIUtil::check_privilege` grants `reasonManage` to no other user:
//! `1004 "You have no reasonManage privilege"`). The lifecycle itself —
//! states, refusals, exact retraction — is [`crate::reason::manage`]'s.
//!
//! Like upstream, execute / disable / check first read the rule (its errors
//! come first), then need the database loaded (`1005 "database[<db>] is not
//! loaded yet"`). Execute and disable run as an autocommit update under the
//! database's lock — the query cache, the CSR, write-through, durability
//! (saved under the check_point policy) as for a `query` update; they are
//! registered as tasks (cancellable) and recorded in the query log — and are
//! refused (`1004`) while a transaction is open on the database (upstream
//! has no such case: its transactions are a separate manager). Check reads
//! a transaction's begin-time snapshot, as other non-transactional reads do.
//!
//! Rust differences: a `type` given as a JSON number is accepted (upstream
//! reads only strings); `ruleinfo` may also be a string holding the JSON
//! object (for form / query-string clients); add, list, compile, show and
//! delete on a database that does not exist answer `1005 "database[<db>]
//! does not exist"` (upstream creates `<db>.db/reason_rule_files/`); and a
//! rule name must be a safe file name (`1003` otherwise).

use std::path::PathBuf;
use std::sync::Arc;

use super::apilog::QueryLog;
use super::envelope::{GResp, Status};
use super::params::Params;
use super::{required, Ctx, OpSpec, Registry};
use crate::http_api::{DbHandle, TaskGuard};
use crate::json::{self, Json};
use crate::reason::manage::{self, ReasonError, ReasonErrorKind};
use crate::reason::store::RuleStore;
use crate::server::locked;

pub(super) fn register(reg: &mut Registry) {
    // Root-only through the default privilege mapping
    // (`crate::sysdb::requirement`: an operation upstream grants to nobody
    // else), synchronous like upstream's.
    reg.add(OpSpec::new("reasonManage", reason_manage).needs_db());
}

fn reason_manage(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let db = ctx.db_name.clone().unwrap_or_default();
    let ty = match required(p, "type") {
        Ok(t) => t,
        Err(r) => return r,
    };
    let name = p.str("rulename").unwrap_or_default();
    match ty.trim() {
        "1" => add(ctx, &db, p),
        "2" => list(ctx, &db),
        "3" => compile(ctx, &db, &name),
        "4" => execute(ctx, &db, &name),
        "5" => disable(ctx, &db, &name),
        "6" => show(ctx, &db, &name),
        "7" => delete(ctx, &db, &name),
        "8" => check(ctx, &db, &name),
        _ => GResp::error(Status::ParamIsIllegal, "The operation is not support."),
    }
}

/// A failed operation as upstream answers it.
fn reason_error(e: ReasonError) -> GResp {
    let status = match e.kind {
        ReasonErrorKind::Param => Status::ParamIsIllegal,
        ReasonErrorKind::Failed => Status::OperationFailed,
        ReasonErrorKind::Conflict => Status::OperationConditionsAreNotSatisfied,
    };
    GResp::error(status, e.message)
}

/// The database's handle (when loaded) and its directory.
fn target(ctx: &Ctx<'_>, db: &str) -> (Option<DbHandle>, PathBuf) {
    let st = ctx.state();
    (st.dbs.get(db).map(Arc::clone), st.db_dir(db))
}

/// The rule store of `db` for an operation that only touches rule files:
/// the database must exist (loaded, or on disk). It does not wait for the
/// database's lock (a long query may hold it): the rule directory's own lock
/// orders it with execute / disable / check, and with backups (which copy
/// the directory under it) and drop / restore (which remove or replace it
/// under it). Refused (`1007`) while a build / drop / rename / backup /
/// restore of the database is in progress.
fn file_store(ctx: &Ctx<'_>, db: &str) -> Result<RuleStore<'static>, GResp> {
    if ctx.state().busy.contains(db) {
        return Err(GResp::error(
            Status::LossOfLock,
            format!(
                "database[{db}] try write lock fail: another build/load/unload/drop/rename/\
                 backup/restore of it is in progress"
            ),
        ));
    }
    // A loaded database already keeps its rules in the directory it is
    // served from (`DbSlot::handle` tells it so), whose rules are these.
    let (handle, dir) = target(ctx, db);
    if handle.is_none() && !dir.is_dir() {
        return Err(GResp::error(
            Status::OperationFailed,
            format!("database[{db}] does not exist"),
        ));
    }
    Ok(RuleStore::dir(dir))
}

/// `type` 1.
fn add(ctx: &mut Ctx<'_>, db: &str, p: &Params) -> GResp {
    let ruleinfo = match p.raw("ruleinfo") {
        None | Some(Json::Null) => {
            return GResp::error(
                Status::ParamIsIllegal,
                "the data has not the rule information",
            )
        }
        // A string holding the object (form / query-string clients).
        Some(Json::Str(s)) => json::parse(s).unwrap_or_else(|_| Json::Str(s.clone())),
        Some(v) => v.clone(),
    };
    let mut store = match file_store(ctx, db) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let now = crate::sysdb::clock::now().datetime();
    match manage::add(&mut store, &ruleinfo, &now) {
        Ok(added) => GResp::success(&format!(
            "Add Reason Rule done.Save Successfully! the file path is {}",
            added.path
        )),
        Err(e) => reason_error(e),
    }
}

/// `type` 2.
fn list(ctx: &mut Ctx<'_>, db: &str) -> GResp {
    let store = match file_store(ctx, db) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match manage::list(&store) {
        Ok(rules) => {
            let num = rules.len();
            GResp::success("ok")
                .with("list", Json::Arr(rules))
                .with("num", num)
        }
        Err(e) => reason_error(e),
    }
}

/// `type` 3.
fn compile(ctx: &mut Ctx<'_>, db: &str, name: &str) -> GResp {
    let mut store = match file_store(ctx, db) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match manage::compile(&mut store, name) {
        // Upstream logs the compiled statements (`SLOG_CORE`).
        Ok(c) => {
            crate::gs_debug!(
                "reasonManage: rule '{name}' of database '{db}' compiled: insert_sparql: {}; \
                 delete_sparql: {}; check_sparql: {}",
                c.insert_sparql,
                c.delete_sparql,
                c.check_sparql
            );
            GResp::success("ok")
                .with("insert_sparql", c.insert_sparql)
                .with("delete_sparql", c.delete_sparql)
                .with("check_sparql", c.check_sparql)
        }
        Err(e) => reason_error(e),
    }
}

/// `type` 6.
fn show(ctx: &mut Ctx<'_>, db: &str, name: &str) -> GResp {
    let store = match file_store(ctx, db) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match manage::show(&store, name) {
        Ok(rule) => GResp::success("ok").with("ruleinfo", rule),
        Err(e) => reason_error(e),
    }
}

/// `type` 7.
fn delete(ctx: &mut Ctx<'_>, db: &str, name: &str) -> GResp {
    let mut store = match file_store(ctx, db) {
        Ok(s) => s,
        Err(r) => return r,
    };
    match manage::delete(&mut store, name) {
        Ok(msg) => GResp::success(&msg),
        Err(e) => reason_error(e),
    }
}

/// What execute / disable / check run on a loaded database.
enum DataOp {
    Execute,
    Disable,
    Check,
}

/// `type` 4.
fn execute(ctx: &mut Ctx<'_>, db: &str, name: &str) -> GResp {
    run_data_op(ctx, db, name, DataOp::Execute)
}

/// `type` 5.
fn disable(ctx: &mut Ctx<'_>, db: &str, name: &str) -> GResp {
    run_data_op(ctx, db, name, DataOp::Disable)
}

/// `type` 8.
fn check(ctx: &mut Ctx<'_>, db: &str, name: &str) -> GResp {
    run_data_op(ctx, db, name, DataOp::Check)
}

/// Execute / disable / check rule `name` on loaded database `db` (see the
/// module docs).
fn run_data_op(ctx: &mut Ctx<'_>, db: &str, name: &str, op: DataOp) -> GResp {
    let (handle, dir) = target(ctx, db);
    // Upstream reads the rule before it looks at the database.
    let field = match op {
        DataOp::Execute => "insert_sparql",
        DataOp::Disable => "delete_sparql",
        DataOp::Check => "check_sparql",
    };
    let sparql = match manage::compiled_statement(&RuleStore::dir(dir.clone()), name, field) {
        Ok(s) => s,
        Err(e) => return reason_error(e),
    };
    let Some(handle) = handle else {
        return not_loaded(db, dir.is_dir());
    };
    let query_log = QueryLog::start(ctx, db, &sparql, None);
    let (task_id, cancel) = {
        let mut st = ctx.state();
        ctx.inner.register_task(&mut st, db, &sparql, ctx.user())
    };
    let mut task = TaskGuard::new(ctx.inner, task_id);
    let mut slot = locked(&handle);
    if slot.retired {
        drop(slot);
        return not_loaded(db, dir.is_dir());
    }
    ctx.inner.sync_query_settings(&mut slot);
    let mut store = RuleStore::dir(slot.dir.clone());
    let resp = match op {
        DataOp::Check => {
            let checked = {
                let slot = &mut *slot;
                manage::check(&mut store, name, |s| {
                    if slot.txn.is_some() {
                        slot.read(s)
                    } else {
                        slot.db.query_cancellable(s, Arc::clone(&cancel))
                    }
                })
            };
            drop(slot);
            match checked {
                Ok(c) => GResp::success("ok")
                    .with("check_sparql", c.check_sparql)
                    .with("effectNum", c.effect_num),
                Err(e) => reason_error(e),
            }
        }
        DataOp::Execute | DataOp::Disable => {
            if let Some(open) = &slot.txn {
                let tid = open.tid;
                drop(slot);
                task.set_ok(false);
                return GResp::error(
                    Status::OperationConditionsAreNotSatisfied,
                    format!(
                        "database '{db}' has an open transaction (TID {tid}); reasoning rules \
                         cannot be executed or disabled until it is committed or rolled back"
                    ),
                );
            }
            let before = slot.db.mutation_count();
            let mut saved = false;
            let outcome = match op {
                DataOp::Execute => {
                    manage::execute(&mut slot.db, &mut store, name, Some(Arc::clone(&cancel))).map(
                        |x| {
                            crate::gs_info!(
                                "reasonManage: rule '{name}' of database '{db}' executed by '{}': \
                                 {} triple(s) inferred, {} owned",
                                ctx.user(),
                                x.inserted,
                                x.owned
                            );
                            GResp::success("ok")
                                .with("insert_sparql", x.insert_sparql)
                                .with("select_sparql", x.select_sparql)
                                .with("AnsNum", x.inserted)
                        },
                    )
                }
                _ => {
                    // The retraction is saved before the ledger is emptied
                    // (whatever the check_point policy; upstream saves here
                    // too): a crash never leaves inferred triples on disk
                    // that no ledger records.
                    let dir = slot.dir.clone();
                    let persist = |db: &mut crate::Database| {
                        crate::http_api::DbSlot::save_db(db, &dir)?;
                        saved = true;
                        Ok(())
                    };
                    manage::disable(
                        &mut slot.db,
                        &mut store,
                        name,
                        Some(Arc::clone(&cancel)),
                        persist,
                    )
                    .map(|x| {
                        crate::gs_info!(
                            "reasonManage: rule '{name}' of database '{db}' disabled by '{}': \
                             {} triple(s) retracted",
                            ctx.user(),
                            x.removed
                        );
                        GResp::success("ok")
                            .with("delete_sparql", x.delete_sparql)
                            .with("AnsNum", x.removed)
                    })
                }
            };
            // Data that changed (also by an update that failed part-way) is
            // saved like any other write — unless the disable just saved it.
            let warning = if saved {
                slot.dirty = false;
                None
            } else if slot.db.mutation_count() != before {
                slot.wrote(ctx.inner.save_writes())
            } else {
                None
            };
            drop(slot);
            match outcome {
                Ok(mut r) => {
                    if let Some(w) = warning {
                        // A Rust extension (as `query`'s): applied, not saved.
                        r.set("warning", w);
                    }
                    r
                }
                Err(e) => {
                    crate::gs_warn!("reasonManage: rule '{name}' of database '{db}': {e}");
                    reason_error(e)
                }
            }
        }
    };
    task.set_ok(resp.is_ok());
    query_log.finish(ctx, &resp);
    resp
}

/// Upstream's `validate_databaseinfo` refusals.
fn not_loaded(db: &str, on_disk: bool) -> GResp {
    let msg = if on_disk {
        format!("database[{db}] is not loaded yet")
    } else {
        format!("database[{db}] does not exist")
    };
    GResp::error(Status::OperationFailed, msg)
}

#[cfg(test)]
mod tests {
    use crate::sysdb::{requirement, Requirement};

    /// Upstream's metadata: root-only (`check_privilege` grants it to no
    /// other user), a database required, synchronous.
    #[test]
    fn reason_manage_is_registered_root_only_and_synchronous() {
        let spec = super::super::registry()
            .get("reasonManage")
            .expect("registered");
        assert!(spec.auth);
        assert!(spec.needs_db);
        assert!(!spec.async_capable);
        assert_eq!(requirement(spec.privilege), Requirement::RootOnly);
    }
}
