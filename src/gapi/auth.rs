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
//! [`verify`] is the **single credential check** of the server's upstream
//! protocol: [`authenticate`] (every authenticated operation) and the
//! lifecycle path routes `/shutdown` / `/restart` (see `ops_lifecycle`, which
//! also accept the per-run `system` user) both go through it.
//!
//! Privileges are granted **per database**, as upstream: [`check_privilege`]
//! maps each operation to the privilege upstream's `APIUtil::check_privilege`
//! (`src/Api/APIUtil.cpp` ~1197-1280) requires and checks the caller's grants
//! on the target database through the system store ([`crate::sysdb::allowed`],
//! which the REST endpoints use too).
//!
//! The IP checks ([`check_ip`]) and the failed-login lockout
//! ([`note_auth_failure`], [`note_login_success`]) are those of
//! [`super::ipfilter`].

use super::envelope::{GResp, Status};
use super::ipfilter;
use super::params::Params;
use super::{Ctx, SYSTEM_DB};
use crate::http_users::{CredentialCheck, PasswordForm};
use crate::query::hash::md5_hex;
use crate::server::{ct_eq, Request};

/// Which of upstream's two IP checks is being made (`check_access_ip` stage
/// `1`: every request; stage `2`: every operation but `login` /
/// `testConnect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpStage {
    Connect,
    Operation,
}

/// The IP access check (upstream `APIUtil::check_access_ip`), answering
/// `1101 IPBlocked`: at [`IpStage::Connect`] the allow / deny lists, at
/// [`IpStage::Operation`] the failed-login lockout (see [`super::ipfilter`]).
pub fn check_ip(ctx: &Ctx<'_>, stage: IpStage) -> Result<(), GResp> {
    let filter = &ctx.inner.ipfilter;
    let (blocked, msg) = match stage {
        IpStage::Connect => (!filter.permits(&ctx.remote_ip), ipfilter::BLOCKED_MSG),
        IpStage::Operation => (filter.locked_out(&ctx.remote_ip), ipfilter::LOCKED_MSG),
    };
    if blocked {
        Err(GResp::error(Status::IpBlocked, msg))
    } else {
        Ok(())
    }
}

/// Record a failed authentication from the caller's address (upstream
/// `update_access_ip_error_num`); enough of them lock the address out.
pub fn note_auth_failure(ctx: &Ctx<'_>) {
    ctx.inner.ipfilter.note_failure(&ctx.remote_ip);
}

/// Clear the caller's failed-authentication count after a successful
/// `login` (upstream `reset_access_ip_error_num`).
pub fn note_login_success(ctx: &Ctx<'_>) {
    ctx.inner.ipfilter.reset(&ctx.remote_ip);
}

/// The credentials a request presents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
    /// `encryption=1`: [`password`](Self::password) is the password's
    /// lowercase hex MD5 digest.
    pub md5: bool,
    /// Upstream's `inner=true` (honoured only with
    /// [`GapiConfig::trust_inner_localhost`](super::GapiConfig)).
    pub inner: bool,
}

impl Credentials {
    /// `username`, `password`, `encryption` and `inner` from the request
    /// parameters (absent values are empty / off).
    pub fn from_params(params: &Params) -> Credentials {
        Credentials {
            username: params.str("username").unwrap_or_default(),
            password: params.str("password").unwrap_or_default(),
            md5: params.str("encryption").as_deref().map(str::trim) == Some("1"),
            inner: params.bool("inner", false),
        }
    }

    /// [`from_params`](Self::from_params), each of `username`, `password` and
    /// `encryption` falling back to a request header of the same name when
    /// the parameters lack it: upstream's `/shutdown` reads its credentials
    /// from headers (and its docs say parameters — both work).
    pub(crate) fn from_params_or_headers(params: &Params, req: &Request) -> Credentials {
        let get = |key: &str| {
            params
                .str(key)
                .or_else(|| req.header(key).map(str::to_string))
        };
        Credentials {
            username: get("username").unwrap_or_default(),
            password: get("password").unwrap_or_default(),
            md5: get("encryption").as_deref().map(str::trim) == Some("1"),
            inner: false,
        }
    }
}

/// Authenticate the request's `username` / `password` (`encryption=1`: the
/// password's MD5 hex digest) and record the user in [`Ctx::user`].
///
/// * either parameter missing (or the username empty) → `1003 "username or
///   password is empty"`;
/// * otherwise [`verify`] (the system user is not accepted here).
pub fn authenticate(ctx: &mut Ctx<'_>, params: &Params) -> Result<String, GResp> {
    if params.str("username").is_none() || params.str("password").is_none() {
        return Err(GResp::error(
            Status::ParamIsIllegal,
            "username or password is empty",
        ));
    }
    let creds = Credentials::from_params(params);
    if creds.username.is_empty() {
        return Err(GResp::error(
            Status::ParamIsIllegal,
            "username or password is empty",
        ));
    }
    verify(ctx, &creds, false)
}

/// The credential check: verify `creds` and record the user in
/// [`Ctx::user`].
///
/// * with `accept_system`, `conf.ini`'s `system_username` with this run's
///   system password (the one `gserver` writes to its pid file; with
///   `encryption=1` its MD5 hex digest) is accepted — what upstream's
///   `/shutdown` and `/restart` take — and that name is checked against
///   nothing else;
/// * an upstream *inner* request (`inner=true` from `127.0.0.1`) skips the
///   password check when [`GapiConfig::trust_inner_localhost`](super::GapiConfig)
///   is on; the user must still exist;
/// * otherwise the user store: unknown user or wrong password → `1001`
///   (recorded with [`note_auth_failure`]); `encryption=1` against an entry
///   not yet migrated to the MD5-compatible format → `1001`, with a message
///   asking for one plaintext login.
pub fn verify(
    ctx: &mut Ctx<'_>,
    creds: &Credentials,
    accept_system: bool,
) -> Result<String, GResp> {
    let username = creds.username.clone();
    let wrong = || {
        GResp::error(
            Status::AuthenticationFailed,
            "Username or password is wrong.",
        )
    };
    if accept_system && username == ctx.inner.config().system_username {
        let expected = ctx.inner.system_password();
        let ok = !expected.is_empty()
            && if creds.md5 {
                ct_eq(
                    creds.password.trim().to_ascii_lowercase().as_bytes(),
                    md5_hex(expected).as_bytes(),
                )
            } else {
                ct_eq(creds.password.as_bytes(), expected.as_bytes())
            };
        if ok {
            ctx.user = Some(username.clone());
            return Ok(username);
        }
        // Only this run's system password admits the system user here: a
        // stored user of that name (which can no longer be created, but may
        // predate the rule) must not stop or restart the server.
        note_auth_failure(ctx);
        return Err(wrong());
    }
    if ctx.config.trust_inner_localhost && creds.inner && ctx.remote_ip == "127.0.0.1" {
        if !ctx.state().users.exists(&username) {
            return Err(wrong());
        }
        ctx.inner_request = true;
        ctx.user = Some(username.clone());
        return Ok(username);
    }
    let form = if creds.md5 {
        PasswordForm::Md5Hex
    } else {
        PasswordForm::Plain
    };
    // The slow password hash runs with the state lock released (only the
    // lookup and the bookkeeping take it), so bad credentials cannot stall
    // other requests behind it.
    let attempt = ctx
        .state()
        .users
        .begin_check(&username, &creds.password, form);
    let checked = attempt.verify();
    let outcome = ctx.state().users.finish_check(checked);
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

pub use crate::sysdb::{requirement, Requirement};

/// Whether the authenticated caller may run operation `op` (an upstream
/// operation name, or a privilege name such as `"update"` for an operation
/// that needs a second check) on `db_name`.
///
/// Like upstream: nobody may target the `system` database, `root` may do
/// everything else, and other users need the privilege [`requirement`] maps
/// `op` to **on `db_name`** — granted on that database, or on every database
/// (an `admin`-privileged user counts as `root`). See
/// [`crate::sysdb::allowed`], which decides.
pub fn check_privilege(ctx: &Ctx<'_>, op: &str, db_name: Option<&str>) -> bool {
    if db_name == Some(SYSTEM_DB) {
        return false;
    }
    let Some(user) = ctx.user.as_deref() else {
        return false;
    };
    crate::sysdb::allowed(&ctx.state().users, user, op, db_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_users::Privilege;

    #[test]
    fn requirements_follow_upstream_check_privilege() {
        assert_eq!(requirement("login"), Requirement::Anyone);
        assert_eq!(requirement("show"), Requirement::Anyone);
        assert_eq!(
            requirement("query"),
            Requirement::Privilege(Privilege::Query)
        );
        assert_eq!(
            requirement("monitor"),
            Requirement::Privilege(Privilege::Query)
        );
        for op in [
            "tquery",
            "begin",
            "commit",
            "rollback",
            "checkpoint",
            "batchInsert",
            "update",
        ] {
            assert_eq!(
                requirement(op),
                Requirement::Privilege(Privilege::Update),
                "{op}"
            );
        }
        assert_eq!(
            requirement("export"),
            Requirement::Privilege(Privilege::Export)
        );
        for op in [
            "stat",
            "build",
            "drop",
            "usermanage",
            "showuser",
            "ipmanage",
            "txnlog",
        ] {
            assert_eq!(requirement(op), Requirement::RootOnly, "{op}");
        }
    }
}
