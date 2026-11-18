//! review ext#12 (server side): the upstream `/api` protocol accepts a
//! session token from `POST /login` — in `Authorization: Bearer` — in place
//! of the password (a gstore-rust extension), so a client such as
//! `gconsole` logs in once instead of sending the password with every
//! request. A request that carries a password is checked as before; a
//! token that is unknown, logged out, revoked by a password change, or
//! presented with another user's name is refused (`1001`).

#[path = "upstream_scripts/harness.rs"]
mod harness;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use gstore::json::{self, Json};
use harness::{code, data, ok, Server, PASSWORD};

/// One HTTP exchange: (status, JSON body).
fn exchange(
    addr: &SocketAddr,
    target: &str,
    headers: &[(&str, &str)],
    content_type: &str,
    body: &str,
) -> (u16, Json) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let mut head = format!(
        "POST {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").expect("a response");
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status");
    let body = json::parse(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"));
    (status, body)
}

/// `POST /login` as `user`: the session token.
fn login(srv: &Server, user: &str, password: &str) -> String {
    let (status, j) = exchange(
        &srv.addr,
        "/login",
        &[],
        "application/x-www-form-urlencoded",
        &format!("username={user}&password={password}"),
    );
    assert_eq!(status, 200, "{j:?}");
    j.get("session")
        .and_then(Json::as_str)
        .expect("a session token")
        .to_string()
}

/// An `/api` operation with `params` (no password) and `token` as bearer.
fn api_with_token(srv: &Server, token: &str, params: &[(&str, &str)]) -> Json {
    let mut body = Json::obj();
    for (k, v) in params {
        body.set(*k, *v);
    }
    let bearer = format!("Bearer {token}");
    let (status, j) = exchange(
        &srv.addr,
        "/api",
        &[("Authorization", &bearer)],
        "application/json",
        &body.to_string(),
    );
    assert_eq!(status, 200, "{j:?}");
    j
}

#[test]
fn the_upstream_api_takes_a_login_session_in_place_of_the_password() {
    let srv = Server::start("api_session");
    ok(&srv.build("small", &data("small/small.nt")));
    srv.load("small");
    let token = login(&srv, "root", PASSWORD);

    // Every operation, without a password.
    let j = api_with_token(&srv, &token, &[("operation", "show"), ("username", "root")]);
    ok(&j);
    let j = api_with_token(
        &srv,
        &token,
        &[
            ("operation", "query"),
            ("username", "root"),
            ("db_name", "small"),
            ("sparql", "select (count(*) as ?n) where { ?s ?p ?o }"),
        ],
    );
    ok(&j);
    assert!(j.to_string().contains("\"25\""), "{j:?}");
    // The user name may be left out: the session's.
    ok(&api_with_token(
        &srv,
        &token,
        &[("operation", "checkpoint"), ("db_name", "small")],
    ));

    // A user is held to their own privileges.
    ok(&srv.op(
        "usermanage",
        &[
            ("type", "1"),
            ("op_username", "bob"),
            ("op_password", "bobpw"),
        ],
    ));
    let bob = login(&srv, "bob", "bobpw");
    let j = api_with_token(
        &srv,
        &bob,
        &[
            ("operation", "query"),
            ("username", "bob"),
            ("db_name", "small"),
            ("sparql", "select * where { ?s ?p ?o }"),
        ],
    );
    assert_ne!(code(&j), 0, "bob has no query privilege: {j:?}");
    // Another user's name with bob's token.
    let j = api_with_token(&srv, &bob, &[("operation", "show"), ("username", "root")]);
    assert_eq!(code(&j), 1001, "{j:?}");
    // An unknown token.
    let j = api_with_token(
        &srv,
        "not-a-session",
        &[("operation", "show"), ("username", "root")],
    );
    assert_eq!(code(&j), 1001, "{j:?}");
    // No token and no password: as before.
    let (_, j) = exchange(
        &srv.addr,
        "/api",
        &[],
        "application/json",
        r#"{"operation":"show","username":"root"}"#,
    );
    assert_eq!(code(&j), 1003, "{j:?}");
    // A password, when given, is what is checked (a wrong one fails even
    // with a valid token).
    let (_, j) = exchange(
        &srv.addr,
        "/api",
        &[("Authorization", &format!("Bearer {token}"))],
        "application/json",
        r#"{"operation":"show","username":"root","password":"wrong"}"#,
    );
    assert_eq!(code(&j), 1001, "{j:?}");

    // A password change ends the user's sessions; so does a logout.
    ok(&api_with_token(
        &srv,
        &bob,
        &[
            ("operation", "userpassword"),
            ("username", "bob"),
            ("op_password", "newpw"),
        ],
    ));
    let j = api_with_token(&srv, &bob, &[("operation", "show"), ("username", "bob")]);
    assert_eq!(code(&j), 1001, "{j:?}");
    let (status, _) = exchange(
        &srv.addr,
        "/logout",
        &[("Authorization", &format!("Bearer {token}"))],
        "application/x-www-form-urlencoded",
        "",
    );
    assert_eq!(status, 200);
    let j = api_with_token(&srv, &token, &[("operation", "show"), ("username", "root")]);
    assert_eq!(code(&j), 1001, "{j:?}");
}
