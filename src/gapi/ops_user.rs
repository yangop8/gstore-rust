//! User management, IP-list and log operations: `showuser`, `usermanage`,
//! `userprivilegemanage`, `userpassword` (upstream
//! `src/Server/ApiUserOperation.cpp`), `ipmanage` (see [`super::ipfilter`])
//! and `querylogdate`, `querylog`, `accesslogdate`, `accesslog`, `txnlog`
//! (see [`super::apilog`]).
//!
//! Users, their per-database privileges and the database registry are the
//! system store of [`crate::sysdb`]. Every operation here except
//! `userpassword` is reserved to `root` by the dispatcher's privilege check
//! (upstream's `check_privilege`); `userpassword` changes the caller's own
//! password. `root` can be neither deleted nor have its privileges changed.

use std::collections::BTreeSet;

use super::envelope::{GResp, Status};
use super::params::Params;
use super::{apilog, check_db_name, ipfilter, Ctx, OpSpec, Registry};
use crate::http_users::{Privilege, ROOT_USER};
use crate::json::Json;
use crate::sysdb::{privilege_list, PRIVILEGE_FIELDS};

pub(super) fn register(reg: &mut Registry) {
    reg.add(OpSpec::new("showuser", showuser));
    reg.add(OpSpec::new("usermanage", usermanage));
    reg.add(OpSpec::new("userprivilegemanage", userprivilegemanage));
    reg.add(OpSpec::new("userpassword", userpassword));
    reg.add(OpSpec::new("ipmanage", ipfilter::ipmanage));
    reg.add(OpSpec::new("querylogdate", apilog::querylogdate));
    reg.add(OpSpec::new("querylog", apilog::querylog));
    reg.add(OpSpec::new("accesslogdate", apilog::accesslogdate));
    reg.add(OpSpec::new("accesslog", apilog::accesslog));
    reg.add(OpSpec::new("txnlog", apilog::txnlog));
}

/// What upstream sends as a user's `password` in `showuser`. Upstream stores
/// passwords in plaintext and returns them; this server keeps only a slow
/// salted hash, so the field is masked (kept for clients that expect it).
const MASKED_PASSWORD: &str = "******";

/// A string parameter, trimmed (`""` when absent).
fn param(p: &Params, name: &str) -> String {
    p.str(name)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Upstream's `check_param_value` for a plain parameter: `1003 "the value
/// of <name> can not be empty!"`.
fn non_empty(p: &Params, name: &str) -> Result<String, GResp> {
    let v = param(p, name);
    if v.is_empty() {
        Err(GResp::error(
            Status::ParamIsIllegal,
            format!("the value of {name} can not be empty!"),
        ))
    } else {
        Ok(v)
    }
}

/// `showuser` — every user with the databases each privilege is held on
/// (upstream `show_users`): `ResponseBody` is an array of `{username,
/// password, query_privilege, update_privilege, load_privilege,
/// unload_privilege, backup_privilege, restore_privilege,
/// export_privilege}`, each list `"db1,db2,"` (`"all"` for `root`). (Upstream
/// sends the unload list as `load_privilege` too — a bug not reproduced.)
fn showuser(ctx: &mut Ctx<'_>, _p: &Params) -> GResp {
    let mut body = Vec::new();
    {
        let st = ctx.state();
        for u in st.users.list() {
            let mut item = Json::obj()
                .with("username", u.name.as_str())
                .with("password", MASKED_PASSWORD);
            for (field, privilege) in PRIVILEGE_FIELDS {
                item.set(field, privilege_list(u, privilege));
            }
            body.push(item);
        }
    }
    if body.is_empty() {
        return GResp::success("No Users");
    }
    GResp::json(
        Json::obj()
            .with("StatusCode", 0)
            .with("StatusMsg", "success")
            .with("ResponseBody", body),
    )
}

/// `usermanage` — `type` 1 adds user `op_username` with password
/// `op_password` (no privileges), 2 deletes it (never `root`), 3 sets its
/// password (upstream `user_manage`).
fn usermanage(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let op_username = match non_empty(p, "op_username") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let kind = param(p, "type");
    // The new password is taken verbatim (spaces are characters too).
    let op_password = p.str("op_password").unwrap_or_default();
    if kind != "2" && op_password.is_empty() {
        return GResp::error(
            Status::ParamIsIllegal,
            "the value of op_password can not be empty!",
        );
    }
    let max_users = ctx.inner.config().max_user_num;
    let mut st = ctx.state();
    match kind.as_str() {
        "1" => {
            if st.users.count() >= max_users {
                return GResp::error(
                    Status::OperationConditionsAreNotSatisfied,
                    "The total number of users more than max_user_num.",
                );
            }
            if st.users.exists(&op_username) {
                return GResp::error(
                    Status::OperationFailed,
                    "Username already existed, add user failed.",
                );
            }
            if op_username == ctx.inner.config().system_username {
                return GResp::error(
                    Status::ParamIsIllegal,
                    format!(
                        "add user failed: '{op_username}' is the system user's name (system_username)"
                    ),
                );
            }
            match st.users.add(&op_username, &op_password, BTreeSet::new()) {
                Ok(()) => GResp::success("Add user done."),
                Err(e) => GResp::error(Status::ParamIsIllegal, format!("add user failed: {e}")),
            }
        }
        "2" => {
            if op_username == ROOT_USER {
                return GResp::error(
                    Status::OperationFailed,
                    "You cannot delete root, delete user failed.",
                );
            }
            if !st.users.exists(&op_username) {
                return GResp::error(
                    Status::OperationFailed,
                    "Username not exist, delete user failed.",
                );
            }
            let dropped = st.users.drop_user(&op_username);
            drop(st);
            match dropped {
                // Its PFN functions go with it: an account created later
                // under the name must not inherit them.
                Ok(()) => match ctx.inner.pfn.drop_user(&op_username) {
                    Ok(()) => GResp::success("Delete user done."),
                    Err(e) => GResp::error(
                        Status::OperationFailed,
                        format!("delete user done, but removing its PFN functions failed: {e}"),
                    ),
                },
                Err(e) => GResp::error(Status::OperationFailed, format!("delete user failed: {e}")),
            }
        }
        "3" => {
            if !st.users.exists(&op_username) {
                return GResp::error(
                    Status::OperationFailed,
                    "Username not exist, change password failed.",
                );
            }
            match st.users.set_password(&op_username, &op_password) {
                Ok(()) => GResp::success("Change password done."),
                Err(e) => GResp::error(
                    Status::OperationFailed,
                    format!("change password failed: {e}"),
                ),
            }
        }
        _ => GResp::error(Status::ParamIsIllegal, "The operation is not support."),
    }
}

/// The privileges named by `privileges` (upstream codes `1`..`7`,
/// comma-separated; names such as `query` are accepted too). Unknown items
/// are skipped, as upstream.
fn parse_privileges(list: &str) -> Vec<Privilege> {
    let mut out = Vec::new();
    for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match Privilege::parse(item) {
            Some(p) if p != Privilege::Admin && !out.contains(&p) => out.push(p),
            _ => {}
        }
    }
    out
}

/// `userprivilegemanage` — `type` 1 grants, 2 revokes `privileges` on
/// `db_name` to / from `op_username`; 3 clears all of the user's privileges
/// (upstream `user_privilege_manage`).
fn userprivilegemanage(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let kind = match non_empty(p, "type") {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !matches!(kind.as_str(), "1" | "2" | "3") {
        return GResp::error(
            Status::ParamIsIllegal,
            format!("The type {kind} is not support."),
        );
    }
    let op_username = match non_empty(p, "op_username") {
        Ok(v) => v,
        Err(r) => return r,
    };
    {
        let st = ctx.state();
        if !st.users.exists(&op_username) {
            return GResp::error(
                Status::OperationConditionsAreNotSatisfied,
                "The username is not exists.",
            );
        }
    }
    if op_username == ROOT_USER {
        return GResp::error(
            Status::OperationConditionsAreNotSatisfied,
            "You can't change privileges for root user.",
        );
    }
    if kind == "3" {
        return match ctx.state().users.clear_privileges(&op_username) {
            Ok(()) => GResp::success("Clear the all privileges for the user successfully!"),
            Err(_) => GResp::error(
                Status::OperationFailed,
                "Clear the all privileges for the user fail.",
            ),
        };
    }
    let db_name = param(p, "db_name");
    if let Err(msg) = check_db_name(&db_name) {
        return GResp::error(Status::ParamIsIllegal, msg);
    }
    if !database_exists(ctx, &db_name) {
        return GResp::error(
            Status::OperationConditionsAreNotSatisfied,
            format!("Database[{db_name}] does not exist."),
        );
    }
    let privileges = match non_empty(p, "privileges") {
        Ok(v) => v,
        Err(r) => return r,
    };
    let privs = parse_privileges(&privileges);
    if privs.is_empty() {
        return GResp::error(
            Status::ParamIsIllegal,
            "not match any valid privilege, valid values between 1 and 7.",
        );
    }
    let names = privs
        .iter()
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let set: BTreeSet<Privilege> = privs.into_iter().collect();
    let (verb, result) = {
        let mut st = ctx.state();
        if kind == "1" {
            ("add", st.users.grant_on(&op_username, &db_name, &set))
        } else {
            ("delete", st.users.revoke_on(&op_username, &db_name, &set))
        }
    };
    match result {
        Ok(()) => GResp::success(&format!("{verb} privilege {names} successfully.")),
        Err(_) => GResp::error(
            Status::OperationFailed,
            format!("{verb} privilege {names} failed."),
        ),
    }
}

/// Whether database `name` exists: registered in the system store, on disk,
/// or loaded (upstream `check_db_built`).
fn database_exists(ctx: &Ctx<'_>, name: &str) -> bool {
    let st = ctx.state();
    st.sysdb.contains(name) || st.dbs.contains_key(name) || st.db_dir(name).is_dir()
}

/// `userpassword` — change the caller's own password to `op_password`
/// (upstream `user_passworrd`).
fn userpassword(ctx: &mut Ctx<'_>, p: &Params) -> GResp {
    let op_password = p.str("op_password").unwrap_or_default();
    if op_password.is_empty() {
        return GResp::error(
            Status::ParamIsIllegal,
            "the value of op_password can not be empty!",
        );
    }
    let user = ctx.user().to_string();
    let mut st = ctx.state();
    if !st.users.exists(&user) {
        return GResp::error(Status::ParamIsIllegal, "Username does not exist.");
    }
    match st.users.set_password(&user, &op_password) {
        Ok(()) => GResp::success("Change password done."),
        Err(e) => GResp::error(
            Status::OperationFailed,
            format!("Change password fail: {e}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privilege_lists_parse_upstream_codes() {
        assert_eq!(
            parse_privileges("1,2,3"),
            vec![Privilege::Query, Privilege::Load, Privilege::Unload]
        );
        assert_eq!(
            parse_privileges("4, 9,,update,8,admin"),
            vec![Privilege::Update],
            "unknown codes, duplicates and admin are skipped"
        );
        assert!(parse_privileges("0,x").is_empty());
    }

    #[test]
    fn the_operations_are_registered_root_only_but_userpassword() {
        let reg = super::super::registry();
        for op in [
            "showuser",
            "usermanage",
            "userprivilegemanage",
            "ipmanage",
            "querylogdate",
            "querylog",
            "accesslogdate",
            "accesslog",
            "txnlog",
        ] {
            assert!(reg.get(op).is_some(), "{op}");
            assert_eq!(
                crate::sysdb::requirement(op),
                crate::sysdb::Requirement::RootOnly,
                "{op}"
            );
        }
        assert_eq!(
            crate::sysdb::requirement("userpassword"),
            crate::sysdb::Requirement::Anyone
        );
    }
}
