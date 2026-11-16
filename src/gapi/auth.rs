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
//! ([`locked_account`], [`note_auth_failure`], [`note_login_success`]) are
//! those of [`super::ipfilter`]: by default a password check of an account
//! locked at the caller's address is refused (`1101`) before the password
//! hash runs, on every path that checks one — [`verify`] (every
//! authenticated operation, `login` and `testConnect` included, and the
//! lifecycle routes) and the pre-body check (`check_before_body`, see
//! `crate::preauth`).

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
/// [`IpStage::Operation`] upstream's per-address lockout (see
/// [`super::ipfilter`]; only under `upstream_login_lockout` — the default
/// lockout refuses a locked account's password check in [`verify`]).
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

/// The `1101` answer to a password check of `account` from the caller's
/// address that the default lockout refuses (none under
/// `upstream_login_lockout`). Ask it before the password hash runs.
pub fn locked_account(ctx: &Ctx<'_>, account: &str) -> Option<GResp> {
    let refusal = ctx
        .inner
        .ipfilter
        .auth_refusal_detail(account, &ctx.remote_ip)?;
    Some(GResp::error(Status::IpBlocked, refusal.message()))
}

/// Record a failed authentication of `account` from the caller's address:
/// against that account at that address, or — under
/// `upstream_login_lockout` — against the address (upstream
/// `update_access_ip_error_num`). Enough of them lock it out.
pub fn note_auth_failure(ctx: &Ctx<'_>, account: &str) {
    ctx.inner
        .ipfilter
        .note_auth_failure(account, &ctx.remote_ip);
}

/// Record a successful `login` of the caller: clears its account's
/// failed-authentication count at the caller's address — or, under
/// `upstream_login_lockout`, the address's count (upstream
/// `reset_access_ip_error_num`).
pub fn note_login_success(ctx: &Ctx<'_>) {
    ctx.inner
        .ipfilter
        .note_login_success(ctx.user(), &ctx.remote_ip);
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

/// The upstream credentials a request carries in its URL query string
/// (`username`, `password`, `encryption`), read before its body: `None` when
/// the query lacks a non-empty `username` or a `password` (they may be in
/// the body, which is not read yet).
pub(crate) fn credentials_in_query(query: Option<&str>) -> Option<Credentials> {
    let params = Params::from_parts(query, None, b"").ok()?;
    params.str("password")?;
    let creds = Credentials::from_params(&params);
    (!creds.username.is_empty()).then_some(creds)
}

/// The outcome of [`check_before_body`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreAuth {
    /// The credentials are valid.
    Valid,
    /// They are not (counted towards the address's lockout).
    Invalid,
    /// Not checked: the address is blocked or locked out, the request is an
    /// upstream *inner* request, or the stored password can only be checked
    /// by a plaintext login (the request is then served as unauthenticated,
    /// and its handler answers it).
    Unchecked,
}

/// Check `creds` (`md5`: the password is its MD5 hex digest) against the
/// user store before the request's body is read — the pre-body hook's check
/// of credentials in a request head (see `crate::preauth`) — under the same
/// IP rules as [`authenticate`]: a blocked or locked-out address, and an
/// account the lockout refuses at this address, are not checked at all (no
/// password hash runs), and a wrong password counts towards the lockout
/// ([`note_auth_failure`]). A success clears only its own account's count
/// at this address (under `upstream_login_lockout` nothing: only a `login`
/// resets an address) and records no user: the request's handler
/// authenticates it again. An upstream *inner* request (`creds.inner`) is not checked
/// either: whether its password is checked at all is [`verify`]'s decision
/// (`trust_inner_localhost`), and a failure counted here could lock out the
/// local tools that send it. Root's default password from a client that is
/// not local (`local_trusted`, the rule of [`Ctx::local_trusted`]) is not
/// checked either: [`verify`] refuses it after the body without counting it,
/// so it must not earn the upload cap before the body.
pub(crate) fn check_before_body(
    inner: &crate::http_api::ApiInner,
    remote_ip: &str,
    local_trusted: bool,
    creds: &Credentials,
) -> PreAuth {
    if creds.username.is_empty()
        || creds.inner
        || !inner.ipfilter.permits(remote_ip)
        || inner.ipfilter.locked_out(remote_ip)
        || (!local_trusted
            && creds.username == crate::http_users::ROOT_USER
            && is_default_password(creds))
    {
        return PreAuth::Unchecked;
    }
    // A locked account is not checked (no hash runs): its handler refuses
    // the request after the body, which is held to the pre-auth cap.
    if inner
        .ipfilter
        .auth_refusal(&creds.username, remote_ip)
        .is_some()
    {
        return PreAuth::Unchecked;
    }
    let form = if creds.md5 {
        PasswordForm::Md5Hex
    } else {
        PasswordForm::Plain
    };
    // The slow hash runs with the state lock released, as in `verify`.
    let attempt = crate::server::locked(&inner.state).users.begin_check(
        &creds.username,
        &creds.password,
        form,
    );
    let checked = attempt.verify();
    let outcome = crate::server::locked(&inner.state)
        .users
        .finish_check(checked);
    match outcome {
        CredentialCheck::Valid => {
            inner.ipfilter.note_auth_success(&creds.username, remote_ip);
            PreAuth::Valid
        }
        CredentialCheck::Invalid => {
            inner.ipfilter.note_auth_failure(&creds.username, remote_ip);
            PreAuth::Invalid
        }
        CredentialCheck::NeedsPlaintextLogin => PreAuth::Unchecked,
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
/// * otherwise the user store: an account the lockout refuses at the
///   caller's address → `1101` before any password hash runs
///   ([`locked_account`]); unknown user or wrong password → `1001`
///   (recorded with [`note_auth_failure`]; a success clears the account's
///   count at that address); `encryption=1` against an entry not yet
///   migrated to the MD5-compatible format → `1001`, with a message asking
///   for one plaintext login.
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
        if let Some(refused) = locked_account(ctx, &username) {
            return Err(refused);
        }
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
            ctx.inner
                .ipfilter
                .note_auth_success(&username, &ctx.remote_ip);
            ctx.user = Some(username.clone());
            return Ok(username);
        }
        // Only this run's system password admits the system user here: a
        // stored user of that name (which can no longer be created, but may
        // predate the rule) must not stop or restart the server.
        note_auth_failure(ctx, &username);
        return Err(wrong());
    }
    // Only a local client: a reverse proxy on this machine (a loopback peer
    // forwarding Forwarded / X-Forwarded-For / X-Real-IP) speaks for remote
    // ones unless `trust_proxy_headers` (review sec#9 / gapi#22).
    if ctx.config.trust_inner_localhost
        && creds.inner
        && ctx.remote_ip == "127.0.0.1"
        && ctx.local_trusted
    {
        if !ctx.state().users.exists(&username) {
            return Err(wrong());
        }
        ctx.inner_request = true;
        ctx.user = Some(username.clone());
        return Ok(username);
    }
    // Root's default password is a loopback privilege: refused from a
    // client that is not local, whether or not root still has it (a constant
    // comparison: nothing about root's actual password is revealed, so it is
    // not counted towards the lockout either — against a proxy's address
    // that would lock out every client behind it).
    if !ctx.local_trusted && username == crate::http_users::ROOT_USER && is_default_password(creds)
    {
        return Err(GResp::error(
            Status::AuthenticationFailed,
            "the default root password is accepted only from a local client (not from a remote \
             one, nor through a proxy): change root's password",
        ));
    }
    // A locked account is refused before the slow hash: guessing gets no
    // further, and costs the server nothing (review gapi#5 / sec#7).
    if let Some(refused) = locked_account(ctx, &username) {
        return Err(refused);
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
            ctx.inner
                .ipfilter
                .note_auth_success(&username, &ctx.remote_ip);
            ctx.user = Some(username.clone());
            Ok(username)
        }
        CredentialCheck::Invalid => {
            note_auth_failure(ctx, &username);
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

/// Whether `creds` present upstream's default root password (as such, or
/// with `encryption=1` as its MD5 hex digest).
fn is_default_password(creds: &Credentials) -> bool {
    let default = crate::config::DEFAULT_ROOT_PASSWORD;
    if creds.md5 {
        ct_eq(
            creds.password.trim().to_ascii_lowercase().as_bytes(),
            md5_hex(default).as_bytes(),
        )
    } else {
        ct_eq(creds.password.as_bytes(), default.as_bytes())
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
        // review gap-txn-lifecycle#5: rollback is open to every user (the
        // transaction handlers let only the owner or a superuser end a
        // transaction, so an owner whose update privilege was revoked can
        // still roll back); upstream requires the update privilege.
        assert_eq!(requirement("rollback"), Requirement::Anyone);
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
