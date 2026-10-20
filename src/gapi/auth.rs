//! Per-request authentication and privilege checks of the `/api` protocol.
//!
//! Upstream gStore has no sessions on this protocol: every request carries
//! `username` and `password` (`grpc.cpp` `checkRequest`), and with
//! `encryption=1` the password parameter is the **lowercase hex MD5** of the
//! password instead of the password. Both forms are verified against the one
//! stored slow hash of [`crate::http_users`] (PBKDF2 over the MD5 digest; see
//! its module docs) — an entry still in an older format can only be verified
//! (and is then migrated) by a plaintext request, and an `encryption=1`
//! request against it is refused with `1001` and a message saying so.
//!
//! Privileges use the server's existing **global** privilege model
//! ([`crate::http_users::Privilege`]) through the one function
//! [`check_privilege`], which maps each operation to the privilege upstream's
//! `APIUtil::check_privilege` (`src/Api/APIUtil.cpp` ~1197-1280) requires.
//! (Upstream grants privileges per database; a later package replaces this
//! function's body accordingly.)

use super::envelope::{GResp, Status};
use super::params::Params;
use super::{Ctx, SYSTEM_DB};
use crate::http_users::{CredentialCheck, PasswordForm, Privilege, ROOT_USER};

/// Which of upstream's two IP checks is being made (`check_access_ip` stage
/// `1`: every request; stage `2`: every operation but `login` /
/// `testConnect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpStage {
    Connect,
    Operation,
}

/// The IP access check (upstream `APIUtil::check_access_ip`). This core
/// package keeps no allow/deny lists, so every address passes; the
/// configuration package plugs its `ip_allow` / `ip_deny` lists and the
/// failed-login lockout in here (answering `1101 IPBlocked`).
pub fn check_ip(_ctx: &Ctx<'_>, _stage: IpStage) -> Result<(), GResp> {
    Ok(())
}

/// Record a failed authentication from the caller's address (upstream
/// `update_access_ip_error_num`); a hook for the configuration package's
/// lockout, a no-op here.
pub fn note_auth_failure(_ctx: &Ctx<'_>) {}

/// Clear the caller's failed-authentication count after a successful
/// `login` (upstream `reset_access_ip_error_num`); a hook, a no-op here.
pub fn note_login_success(_ctx: &Ctx<'_>) {}

/// Authenticate the request's `username` / `password` (`encryption=1`: the
/// password's MD5 hex digest) and record the user in [`Ctx::user`].
///
/// * either parameter missing (or the username empty) → `1003 "username or
///   password is empty"`;
/// * unknown user or wrong password → `1001`;
/// * `encryption=1` against an entry not yet migrated to the MD5-compatible
///   format → `1001`, with a message asking for one plaintext login.
///
/// An upstream *inner* request (`inner=true` from `127.0.0.1`) skips the
/// password check when [`GapiConfig::trust_inner_localhost`](super::GapiConfig)
/// is on; the user must still exist.
pub fn authenticate(ctx: &mut Ctx<'_>, params: &Params) -> Result<String, GResp> {
    let (Some(username), Some(password)) = (params.str("username"), params.str("password")) else {
        return Err(GResp::error(Status::ParamIsIllegal, "username or password is empty"));
    };
    if username.is_empty() {
        return Err(GResp::error(Status::ParamIsIllegal, "username or password is empty"));
    }
    let wrong = || GResp::error(Status::AuthenticationFailed, "Username or password is wrong.");
    if ctx.config.trust_inner_localhost && params.bool("inner", false) && ctx.remote_ip == "127.0.0.1"
    {
        if !ctx.state().users.exists(&username) {
            return Err(wrong());
        }
        ctx.inner_request = true;
        ctx.user = Some(username.clone());
        return Ok(username);
    }
    let form = if params.str("encryption").as_deref().map(str::trim) == Some("1") {
        PasswordForm::Md5Hex
    } else {
        PasswordForm::Plain
    };
    let outcome = ctx.state().users.authenticate(&username, &password, form);
    match outcome {
        CredentialCheck::Valid => {
            ctx.user = Some(username.clone());
            Ok(username)
        }
        CredentialCheck::Invalid => {
            note_auth_failure(ctx);
            Err(wrong())
        }
        CredentialCheck::NeedsPlaintextLogin => Err(GResp::error(
            Status::AuthenticationFailed,
            format!(
                "the stored password of user '{username}' predates encryption=1 support and \
                 cannot be checked against an MD5 digest yet: log in once with the plaintext \
                 password (without encryption=1) to migrate it, then retry"
            ),
        )),
    }
}

/// What an operation requires of a (non-root) caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Any authenticated user.
    Anyone,
    /// The given privilege.
    Privilege(Privilege),
    /// Only `root` (or an `admin`-privileged user): upstream's
    /// `check_privilege` grants every operation not listed to `root` alone.
    RootOnly,
}

/// The requirement of operation `op`, after upstream's
/// `APIUtil::check_privilege`.
pub fn requirement(op: &str) -> Requirement {
    match op {
        "login" | "testConnect" | "getCoreVersion" | "funquery" | "funcudb" | "funreview"
        | "check" | "show" | "userpassword" | "upload" | "download" => Requirement::Anyone,
        "query" | "monitor" => Requirement::Privilege(Privilege::Query),
        "update" | "batchInsert" | "batchRemove" | "begin" | "tquery" | "commit" | "rollback"
        | "checkpoint" => Requirement::Privilege(Privilege::Update),
        "load" => Requirement::Privilege(Privilege::Load),
        "unload" => Requirement::Privilege(Privilege::Unload),
        "restore" => Requirement::Privilege(Privilege::Restore),
        "backup" => Requirement::Privilege(Privilege::Backup),
        "export" => Requirement::Privilege(Privilege::Export),
        _ => Requirement::RootOnly,
    }
}

/// Whether the authenticated caller may run operation `op` (an upstream
/// operation name, or a privilege name such as `"update"` for an operation
/// that needs a second check) on `db_name`.
///
/// Like upstream: nobody may target the `system` database, `root` may do
/// everything else, and other users need the privilege [`requirement`] maps
/// `op` to — here from the global privilege model (an `admin`-privileged
/// user counts as `root`). This is the single seam a per-database privilege
/// model replaces.
pub fn check_privilege(ctx: &Ctx<'_>, op: &str, db_name: Option<&str>) -> bool {
    if db_name == Some(SYSTEM_DB) {
        return false;
    }
    let Some(user) = ctx.user.as_deref() else {
        return false;
    };
    if user == ROOT_USER {
        return true;
    }
    let st = ctx.state();
    match requirement(op) {
        Requirement::Anyone => st.users.exists(user),
        Requirement::Privilege(p) => st.users.has_priv(user, p),
        Requirement::RootOnly => st.users.has_priv(user, Privilege::Admin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirements_follow_upstream_check_privilege() {
        assert_eq!(requirement("login"), Requirement::Anyone);
        assert_eq!(requirement("show"), Requirement::Anyone);
        assert_eq!(requirement("query"), Requirement::Privilege(Privilege::Query));
        assert_eq!(requirement("monitor"), Requirement::Privilege(Privilege::Query));
        for op in ["tquery", "begin", "commit", "rollback", "checkpoint", "batchInsert", "update"] {
            assert_eq!(requirement(op), Requirement::Privilege(Privilege::Update), "{op}");
        }
        assert_eq!(requirement("export"), Requirement::Privilege(Privilege::Export));
        for op in ["stat", "build", "drop", "usermanage", "showuser", "ipmanage", "txnlog"] {
            assert_eq!(requirement(op), Requirement::RootOnly, "{op}");
        }
    }
}
