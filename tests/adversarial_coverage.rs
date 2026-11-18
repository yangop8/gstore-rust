//! The adversarial-coverage audit (review sec#20, http#20, robust#17,
//! gapi#21): every denial-of-service and hostile-input category those
//! findings named has a named regression test. Most of them landed with the
//! fixes of rounds 1-4; this file holds the ones still missing, and the map
//! below says where each category is pinned (the same table is in the
//! fragment `changelog.d/r5-test-audit.md`).
//!
//! | category | regression test |
//! |---|---|
//! | request head dripped byte by byte | `server::tests::a_dripping_client_cannot_hold_a_worker` |
//! | body dripped after a complete head, one worker | `http_framing_deadlines::a_dripped_body_is_cut_off_and_the_worker_serves_the_next_request`, `…_gains_nothing_by_declaring_a_large_length`, `a_dripped_chunked_body_is_cut_off_too`, `a_dripped_api_body_cannot_hold_the_api_server` |
//! | TLS handshake / head / body dripped (feature `tls`) | `http_framing_deadlines::tls::a_dripped_tls_handshake_cannot_hold_the_worker`, `…_request_head_…`, `…_body_…` |
//! | connections per address | `http_framing_deadlines::the_per_address_cap_refuses_only_the_address_over_it` |
//! | pre-authentication body cap | `http_preauth_limits::an_unauthenticated_body_over_the_preauth_cap_is_refused_before_it_is_read`, `a_chunked_unauthenticated_body_is_cut_off_at_the_preauth_cap`, `the_single_database_server_holds_unauthenticated_bodies_to_the_preauth_cap` |
//! | memory of all bodies at once | `http_preauth_limits::the_body_budget_bounds_all_bodies_in_memory` |
//! | pre-authentication parse amplification (JSON values, form pairs, multipart parts) | `http_preauth_limits::a_wide_json_body_is_refused_before_its_tree_is_built`; `gapi::params::tests::form_bodies_and_query_strings_have_a_bounded_number_of_pairs`, `multipart_parts_are_bounded`; `json::tests::limits_bound_values_and_depth` |
//! | multipart worst case (64 KiB boundary, megabytes of near-delimiters) | `http_preauth_limits::a_multipart_boundary_longer_than_rfc_2046_allows_is_refused`, `a_64_character_boundary_over_megabytes_parses_in_linear_time`; `gapi::params::tests::the_boundary_search_is_linear_in_the_boundary_and_the_body` |
//! | framing violations (CL+TE, duplicate CL, codings, chunk sizes, obs-fold) | `http_framing_deadlines::ambiguous_framing_is_refused_and_the_connection_closed`; `chunked_framing_counts_against_the_body_cap::chunk_extensions_count_against_the_body_cap`, `trailer_fields_count_against_the_body_cap` |
//! | control characters in header lines | `http_framing_deadlines::control_characters_in_header_lines_are_refused` |
//! | header count and header line length (`MAX_HEADERS`, `MAX_HEADER_LINE`) | **here**: [`more_than_256_header_fields_are_refused_before_authentication`], [`a_header_line_over_64_kib_is_refused_before_authentication`] |
//! | raw SPARQL bodies holding `&query=` / `&update=` | `http_request_semantics::a_raw_sparql_update_body_is_never_parsed_as_a_form`, `a_raw_sparql_query_body_is_never_parsed_as_a_form`, `a_raw_rest_body_never_switches_the_database_or_the_statement` |
//! | GET running an update; CORS; DNS rebinding | `http_browser_surface::get_never_runs_an_update_unless_the_compat_switch_is_on`, `cors_headers_go_only_to_listed_origins`, `preflights_follow_the_origin_list`, `a_loopback_request_must_name_a_local_host` |
//! | lockout on every authentication path (REST Basic, `/login`, `/api` login and testConnect, pre-body) | `login_lockout::api_login_and_test_connect_are_refused_for_a_locked_account_without_hashing`, `rest_login_and_basic_credentials_are_refused_for_a_locked_account`, `failed_basic_credentials_count_towards_the_lockout`, `the_pre_body_check_refuses_a_locked_account_before_hashing`; `http_preauth_limits::wrong_head_credentials_are_refused_before_the_body_and_count_towards_the_lockout` |
//! | `/api` with a `/login` session token in place of the password (round 5, ext#12: no password is checked, so it neither counts towards the lockout nor clears it — a session `login` leaves the failed-login count and an active lock alone; an unknown, logged-out or revoked token, or one presented with another user's name, is refused) | `api_session_tokens::the_upstream_api_takes_a_login_session_in_place_of_the_password`, `a_session_login_leaves_the_failed_login_lockout_alone` |
//! | session growth and expiry | `user_store_consistency::a_user_flooding_logins_ends_only_their_own_oldest_sessions`, `expired_sessions_are_swept_on_login` |
//! | deeply nested `SERVICE` reply | `service_hardening::a_deeply_nested_reply_is_a_parse_error`, `a_service_endpoint_replying_100k_nested_brackets_does_not_abort` |
//! | surrogate pairs in a `SERVICE` reply | `service_hardening::surrogate_pair_escapes_in_service_results_decode`, `a_surrogate_pair_from_a_service_endpoint_joins_with_local_data` |
//! | `SERVICE` to loopback / host-local endpoints | `service_hardening::service_refuses_host_local_endpoints_under_the_allow_service_policy`, `the_rest_server_refuses_host_local_service_endpoints`; `dt_security_review` (corrected in round 1, review tests-b#1) |
//! | CR/LF and control characters in endpoint IRIs | `service_hardening::crlf_in_a_service_endpoint_iri_is_refused_before_connecting`, `sparql_post_refuses_whitespace_and_control_characters` |
//! | callback transport error, retry, `callback_allow_local` | `gapi::tasks::tests::callbacks_report_delivery_failures` (corrected here, review gapi#21); `dt_gapi_tasks::callback_receives_the_result_and_failures_are_tolerated` |
//! | a failed query reaches the query log | `query_limits::failed_api_queries_are_in_the_query_log_with_their_status` |
//! | short `ipmanage` entries | `ipmanage_round_trip::short_ipv6_entries_round_trip_through_save_and_restart` |
//! | registry reconcile against an unlistable root / unreadable database | `sysdb::tests::an_unlistable_root_keeps_registrations_and_grants`; `sysdb_reconcile_read_errors::a_database_behind_a_dangling_symlink_keeps_its_registration_and_grants` |
//! | corrupt / truncated / unreadable `named.bin` | `snapshot_read_errors::a_truncated_named_bin_fails_the_open_and_gcompact_reclaim_refuses` and its neighbours |
//! | failed redo-log append | `redo_log_durability::a_failed_append_is_reported_cut_off_and_retried_in_order` |
//! | query time and answer size budgets | `query_limits::a_runaway_rest_query_times_out_and_a_queued_writer_then_completes`, `a_runaway_api_query_times_out_with_1005_and_is_logged`, `a_rest_answer_over_the_byte_budget_is_refused`; `api_result_budget_counts_escapes` |
//! | anonymous download of exports | `gapi_download_policy::anonymous_downloads_are_refused_by_default` (`dt_gapi_db`'s anonymous-download test corrected in round 1) |
//! | `ConcurrentDb` version-chain growth | `concurrent_db::version_chain_gc_work_stays_flat_after_20k_commits`, `churned_keys_leave_no_chains` |
//!
//! Tests that codified the risky behaviour were corrected where the fix
//! landed: the lockout exemption of `/api` login / testConnect is now only
//! the `upstream_login_lockout` switch's (`dt_gapi_users`), loopback
//! `SERVICE` under `allow_remote` is refused (`dt_security_review`),
//! anonymous export downloads need `anonymous_download` (`dt_gapi_db`), and
//! `callbacks_report_delivery_failures` no longer passes on the host-local
//! refusal alone.
//!
//! No assertion here depends on elapsed time; the read timeouts are hang
//! guards.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::config::GStoreConfig;
use gstore::http_api::ApiServer;
use gstore::server::{HttpOptions, Server};
use gstore::Database;

/// The only timeout here: a hang guard, never what a test asserts.
const HANG_GUARD: Duration = Duration::from_secs(60);

/// What `server.rs` allows: header fields per request, and bytes per line
/// (the terminator included).
const MAX_HEADERS: usize = 256;
const MAX_HEADER_LINE: usize = 64 * 1024;

/// The servers' head deadline: short, so that a server that waited for the
/// rest of a refused head (instead of refusing it as it arrives) answers
/// with its timeout message soon — which the tests tell apart from the
/// refusal ([`refused`]).
fn opts() -> HttpOptions {
    HttpOptions {
        read_timeout: Duration::from_secs(5),
        ..HttpOptions::default()
    }
}

/// A temporary directory, removed when dropped (also when the test fails).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "gstore-adversarial-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A server on a thread, shut down and joined when dropped.
struct Running {
    addr: SocketAddr,
    stop: Box<dyn Fn()>,
    serving: Option<thread::JoinHandle<()>>,
    /// A request every server answers `200` without credentials, ending in
    /// `Connection: close` and the blank line; the header lines under test
    /// go in front of it.
    probe: (&'static str, &'static str),
    _root: Option<TmpDir>,
}

impl Drop for Running {
    fn drop(&mut self) {
        (self.stop)();
        if let Some(t) = self.serving.take() {
            let _ = t.join();
        }
    }
}

/// The single-database server (no authentication at all).
fn single() -> Running {
    let db =
        Database::build_from_str("adv", "<http://ex/a> <http://ex/p> <http://ex/b> .\n").unwrap();
    let server = Arc::new(
        Server::bind(db, "127.0.0.1:0")
            .unwrap()
            .with_http_options(opts()),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || {
        s2.serve_forever();
    });
    Running {
        addr,
        stop: Box::new(move || server.shutdown()),
        serving: Some(serving),
        probe: (
            "GET /status HTTP/1.1\r\nHost: localhost\r\n",
            "Connection: close\r\n\r\n",
        ),
        _root: None,
    }
}

/// The `/api` server, asked without credentials: the head limits hold before
/// authentication.
fn api() -> Running {
    let root = TmpDir::new("api");
    let mut config = GStoreConfig::default();
    config.ip_deny_path = None;
    let server = Arc::new(
        ApiServer::bind(&root.0, "adversarial-pw", "127.0.0.1:0")
            .unwrap()
            .with_config(config)
            .with_http_options(opts()),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || {
        s2.serve_forever();
    });
    Running {
        addr,
        stop: Box::new(move || server.shutdown()),
        serving: Some(serving),
        probe: (
            "POST /api HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n",
            "Content-Length: 22\r\nConnection: close\r\n\r\n{\"operation\":\"check\"}\n",
        ),
        _root: Some(root),
    }
}

/// Write `raw` on a fresh connection and read until the server closes it
/// (a reset ends the reading too).
fn exchange(addr: SocketAddr, raw: &[u8]) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(HANG_GUARD)).unwrap();
    // A refused request may be answered before all of it is written.
    let _ = s.write_all(raw);
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) => {
                assert!(
                    !matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ),
                    "the server neither answered nor closed the connection"
                );
                break;
            }
        }
    }
    String::from_utf8_lossy(&got).into_owned()
}

/// A `400` refusing the head as malformed — not the head deadline's answer
/// ("the request did not arrive in time"), which a server waiting for the
/// rest of an over-long head would give.
fn refused(resp: &str) -> bool {
    let body = resp.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    // The single-database server answers the message as text, the /api
    // server as `{"error":"…"}`.
    status_of(resp) == 400 && (body == "bad request" || body == r#"{"error":"bad request"}"#)
}

fn status_of(resp: &str) -> u16 {
    resp.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// The probe request with `extra` header lines in front of its last lines.
fn probe_with(srv: &Running, extra: &str) -> Vec<u8> {
    format!("{}{extra}{}", srv.probe.0, srv.probe.1).into_bytes()
}

/// `n` distinct `X-Pad-<i>: v` header lines.
fn pad_headers(n: usize) -> String {
    (0..n).map(|i| format!("X-Pad-{i}: v\r\n")).collect()
}

/// How many header lines the probe request carries itself.
fn own_headers(srv: &Running) -> usize {
    let lines = |s: &str| s.matches("\r\n").count();
    // The request line and the blank line are not fields.
    lines(srv.probe.0) - 1 + lines(srv.probe.1) - 1
}

/// review http#20 (`MAX_HEADERS`): a request may carry 256 header fields; one
/// more is refused with `400` and the connection is closed, before any
/// authentication — and the worker serves the next request.
#[test]
fn more_than_256_header_fields_are_refused_before_authentication() {
    for (name, srv) in [("single-database", single()), ("/api", api())] {
        let fits = MAX_HEADERS - own_headers(&srv);
        let resp = exchange(srv.addr, &probe_with(&srv, &pad_headers(fits)));
        assert_eq!(status_of(&resp), 200, "{name}: 256 fields: {resp:?}");

        // One field too many. The request stops right after it (no blank
        // line), so the server has read all of it when it answers: the
        // 400 cannot be lost to a reset over unread bytes.
        let mut raw = format!("{}{}", srv.probe.0, pad_headers(MAX_HEADERS));
        raw.push_str("X-One-Too-Many: v\r\n");
        let resp = exchange(srv.addr, raw.as_bytes());
        assert!(refused(&resp), "{name}: 257 fields: {resp:?}");
        assert_eq!(resp.matches("HTTP/1.1 ").count(), 1, "{name}: {resp:?}");

        let resp = exchange(srv.addr, &probe_with(&srv, ""));
        assert_eq!(status_of(&resp), 200, "{name}: served after: {resp:?}");
    }
}

/// review http#20 (`MAX_HEADER_LINE`): a header line of 64 KiB (its CRLF
/// included) is read; a longer one is refused with `400` before the server
/// buffers more of it, before any authentication — and the worker serves
/// the next request.
#[test]
fn a_header_line_over_64_kib_is_refused_before_authentication() {
    for (name, srv) in [("single-database", single()), ("/api", api())] {
        let name_part = "X-Long: ";
        let longest = format!(
            "{name_part}{}\r\n",
            "a".repeat(MAX_HEADER_LINE - name_part.len() - 2)
        );
        assert_eq!(longest.len(), MAX_HEADER_LINE);
        let resp = exchange(srv.addr, &probe_with(&srv, &longest));
        assert_eq!(status_of(&resp), 200, "{name}: a 64 KiB line: {resp:?}");

        // 64 KiB without a line end: the server stops reading there (the
        // client sends nothing more, so no unread byte can turn its answer
        // into a reset).
        let raw = format!(
            "{}{name_part}{}",
            srv.probe.0,
            "a".repeat(MAX_HEADER_LINE - name_part.len())
        );
        let resp = exchange(srv.addr, raw.as_bytes());
        assert!(refused(&resp), "{name}: an over-long line: {resp:?}");

        // The request line is bounded the same way.
        let raw = format!("GET /{} HTTP/1.1", "a".repeat(MAX_HEADER_LINE));
        let resp = exchange(srv.addr, &raw.as_bytes()[..MAX_HEADER_LINE]);
        assert!(refused(&resp), "{name}: an over-long target: {resp:?}");

        let resp = exchange(srv.addr, &probe_with(&srv, ""));
        assert_eq!(status_of(&resp), 200, "{name}: served after: {resp:?}");
    }
}
