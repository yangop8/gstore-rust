//! Upstream's server-lifecycle path routes: `/shutdown` and `/restart` (also
//! under the documented `/grpc/` prefix), `GET` or `POST` — upstream
//! `shutdown()` / `restart()` in `src/Main/grpc.cpp`.
//!
//! Like every `/api` answer they are HTTP 200 with the gStore envelope
//! `{"StatusCode":…,"StatusMsg":…}`. The layer's IP check runs first (as
//! upstream's `check_access_ip` level 0), then:
//!
//! * `1101 "server is stopping, please waitting !"` once a shutdown began;
//! * `1003` for a missing `username` / `password` — read from the request
//!   parameters (URL query, JSON / form / multipart body) or, failing that,
//!   request headers (upstream's `/shutdown` reads headers);
//! * `1001 "You have no rights to stop the server."` for a user other than
//!   `root` or `conf.ini`'s `system_username`;
//! * `1001` for wrong credentials, checked by the layer's one credential check
//!   [`auth::verify`]: the `system` user with this run's system password (what
//!   upstream's `gserver -t` sends, read from the pid file), or `root` with its
//!   password — either one also as its MD5 hex digest with `encryption=1`.
//!   (Upstream's `/restart` accepts any valid user; here both routes are
//!   `root` / `system` only.)
//!
//! On success the response is written first, then the server shuts down
//! gracefully exactly as on `SIGTERM` (in-flight requests finish, open
//! transactions roll back, dirty databases are saved); `/restart` also sets
//! [`ShutdownHandle::restart_requested`](crate::server::ShutdownHandle::restart_requested),
//! on which `gserver` re-executes itself.

use super::auth::{self, Credentials};
use super::envelope::{GResp, Status};
use super::params::Params;
use super::{Ctx, PathRoute, Registry};
use crate::http_users::ROOT_USER;

const METHODS: &[&str] = &["GET", "POST"];

pub(super) fn register(reg: &mut Registry) {
    for path in ["/shutdown", "/grpc/shutdown"] {
        reg.add_path(PathRoute {
            name: "shutdown",
            path,
            prefix: false,
            methods: METHODS,
            handler: shutdown,
        });
    }
    for path in ["/restart", "/grpc/restart"] {
        reg.add_path(PathRoute {
            name: "restart",
            path,
            prefix: false,
            methods: METHODS,
            handler: restart,
        });
    }
}

fn shutdown(ctx: &mut Ctx<'_>, params: &Params) -> GResp {
    stop(ctx, params, false)
}

fn restart(ctx: &mut Ctx<'_>, params: &Params) -> GResp {
    stop(ctx, params, true)
}

fn stop(ctx: &mut Ctx<'_>, params: &Params, restart: bool) -> GResp {
    if ctx.inner.stopping() {
        // Upstream answers StatusIPBlocked (1101) with this message.
        return GResp::error(Status::IpBlocked, "server is stopping, please waitting !");
    }
    // The failed-login lockout holds here too: these routes check the root
    // and system passwords, and must not be a way around it.
    if let Err(r) = auth::check_ip(ctx, auth::IpStage::Operation) {
        return r;
    }
    let creds = Credentials::from_params_or_headers(params, ctx.req);
    for (name, value) in [("username", &creds.username), ("password", &creds.password)] {
        if value.is_empty() {
            return GResp::error(
                Status::ParamIsIllegal,
                format!("the value of {name} can not be empty!"),
            );
        }
    }
    if creds.username != ctx.inner.config().system_username && creds.username != ROOT_USER {
        let action = if restart { "restart" } else { "stop" };
        return GResp::error(
            Status::AuthenticationFailed,
            format!("You have no rights to {action} the server."),
        );
    }
    if let Err(denied) = auth::verify(ctx, &creds, true) {
        return denied;
    }
    ctx.inner.request_stop(restart);
    GResp::success(if restart {
        "Server restarted successfully."
    } else {
        "Server stopped successfully."
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Request;

    fn parse(raw: &str) -> (Params, Credentials) {
        let req = Request::parse(&mut raw.as_bytes()).unwrap().unwrap();
        let params = Params::from_request(&req).unwrap();
        let creds = Credentials::from_params_or_headers(&params, &req);
        (params, creds)
    }

    fn creds_of(raw: &str) -> Credentials {
        parse(raw).1
    }

    #[test]
    fn credentials_come_from_query_json_form_or_headers() {
        // A JSON body (what the SDKs send): escapes and non-ASCII decode, a
        // nested object's fields are not top-level ones, the query string
        // fills what the body lacks, headers fill what both lack.
        let json = r#"{"nested":{"username":"no","a":[1,{"b":"}"}]},"username":"r\"oöt","n":42,"t":true,"z":null}"#;
        let (params, c) = parse(&format!(
            "POST /shutdown?password=q HTTP/1.1\r\npassword: h\r\nencryption: 1\r\n\
             Content-Length: {}\r\n\r\n{json}",
            json.len()
        ));
        assert_eq!(c.username, "r\"oöt");
        assert_eq!(c.password, "q", "the query string before a header");
        assert!(c.md5, "header fallback");
        // Numbers and booleans read as written; `null` as absent.
        assert_eq!(params.str("n").as_deref(), Some("42"));
        assert_eq!(params.str("t").as_deref(), Some("true"));
        assert_eq!(params.str("z"), None);
        // A form body.
        let c = creds_of("POST /shutdown HTTP/1.1\r\nContent-Length: 15\r\n\r\nusername=a%20b&");
        assert_eq!(c.username, "a b");
        assert_eq!(c.password, "");
        assert!(!c.md5);
        // Headers only (upstream's `/shutdown`), surrogate-pair escapes in JSON.
        let c = creds_of("GET /shutdown HTTP/1.1\r\nusername: system\r\npassword: 123\r\n\r\n");
        assert_eq!(
            (c.username.as_str(), c.password.as_str()),
            ("system", "123")
        );
        let body = "{\"username\" : \"\\ud83d\\ude00\"}";
        let c = creds_of(&format!(
            "POST /restart HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
        assert_eq!(c.username, "😀");
    }

    #[test]
    fn the_lifecycle_routes_are_registered() {
        let reg = super::super::registry();
        for (path, name) in [
            ("/shutdown", "shutdown"),
            ("/grpc/shutdown", "shutdown"),
            ("/restart", "restart"),
            ("/grpc/restart", "restart"),
        ] {
            for method in ["GET", "POST"] {
                let route = reg.path_route(method, path).expect(path);
                assert_eq!(route.name, name, "{method} {path}");
            }
            assert!(reg.path_route("PUT", path).is_none(), "{path}");
        }
    }
}
