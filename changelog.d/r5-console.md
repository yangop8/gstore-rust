# r5-console — gconsole correctness, TLS and session login, deterministic console tests

## Changelog

Behaviour changes first:

- **`gconsole --local` keeps the updates it acknowledged (review ext#5).** A
  snapshot-layout database — and a new in-memory one — is persisted only by
  a save, and the local console used to drop the updates it had answered
  `[updated N triple(s)]` to when it left the database: at `quit` / `exit`,
  at the end of the input and on `load <other>`. It now tracks the
  database's mutation count since it was opened or last saved and saves it
  before leaving it (to its directory; a new in-memory database to
  `<name>.db`, where `save` writes it), printing `saved to <dir>`. When that
  save fails, `load <other>` keeps the current database, an interactive
  `quit` stays in the console (a second `quit` leaves without saving), and
  at the end of the input the failure is reported (exit code 1). On-disk
  (`kvstore.kv`) and RocksDB databases write each update through and are
  not saved. The banner of a new database and `help` say so.
- **`gconsole --local` ends statements like the remote console (review
  ext#9).** A statement used to end at any line ending in `;`, so a
  multi-line SELECT with a `;` predicate-object list, a long (`"""…"""`)
  literal whose first line ends in `;`, and two statements on one line
  failed to parse. A `;` now ends a statement only outside `{…}` groups,
  string literals, `<IRI>`s and `#` comments (the remote console's scanner,
  `console::syntax::next_sparql_statement`); consecutive update operations
  completed together (`op ; op;` on a line) still run as one request
  sharing their prologue, and a script's final statement may lack its `;`
  (it used to be dropped silently; an interactive one still is).
- **gconsole logs in once and sends a session, not the password, with every
  request (review ext#12).** The console now logs in with `POST /login` and
  sends the session token (`Authorization: Bearer`) with every `/api`
  request instead of the user's password; the password crosses the network
  once per login. When the session ends (it expired — 24 h by default — or
  the user's password changed, which ends their sessions) the server answers
  `1001`, and the console logs in again and repeats the request once; at the
  end it logs out (`POST /logout`). It checks a new session with `/api`
  `login`; against a server without `POST /login` (an upstream gserver), or
  whose `/api` does not take the session (a gstore-rust gserver older than
  this change, which has `POST /login`; the session is then ended), it
  falls back to upstream's `/api` `login` and the per-request password.
  Wrong credentials still print upstream's
  `login failed: Username or password is wrong.`.
- **`/api` accepts a `/login` session in place of the password (review
  ext#12; a gstore-rust extension of the upstream protocol).** A request
  without a `password` parameter may authenticate with a live session token
  of a `POST /login`, presented as the REST endpoints take it
  (`Authorization: Bearer`, `X-Session-Token`, `session=` / `token=` in the
  URL); the same holds for upstream's `/file/upload` and, when it names a
  `username`, `/file/download`. A `username` given must name the session's
  user; an unknown,
  expired, logged-out or revoked token is answered `1001 "The session is
  invalid or has expired: log in again."`. A request that carries a
  password is checked exactly as before (the token is then ignored), so
  nothing changes for upstream clients. No password is checked on this
  path, so it adds nothing to the failed-login lockout — the `/login` that
  issued the token is under it — and the IP allow/deny lists and
  upstream-lockout check run before it as for every operation.
- **`gconsole --tls [--ca-file FILE]` (review ext#12; the `tls` cargo
  feature).** The console can talk to a gserver serving HTTPS
  (`--tls-cert` / `--tls-key`): each exchange runs over rustls, the server's
  certificate verified against `--ca-file` (else `$SSL_CERT_FILE`, else the
  system CA bundle). A certificate the bundle does not vouch for is a
  connection failure (`Could not connect to server…`, the reason on stderr)
  and nothing is sent. The handshake is bounded (30 s); Ctrl-C still drops
  the connection. Without the feature `--tls` is refused (exit 2), as is
  `--ca-file` without `--tls`. A plain-HTTP connection to a host that is not
  loopback prints a warning that the password (at login) and every
  statement cross the network in clear text (use `--tls`, or an SSH
  tunnel).
- **`setpriv` validates its answer and changes privileges as one step
  (review ext#11).** An answer that is not a list of privilege numbers — a
  typo (`q`), an out-of-range number, an empty line — is refused with a
  message naming it and `Privilege set failed.`, and nothing is sent (it
  used to revoke every privilege of the user on the database and print
  `Privilege set successfully.`). A lone `0` sets no privilege (the
  explicit form of what an empty answer did); `0` with other numbers is
  refused. The console now grants what is missing first and then revokes
  what is left over (upstream: revoke everything, then grant); when the
  revoke fails it takes the grant back and prints
  `The privileges of <user> on <db> are unchanged.` (or, should that fail
  too, which privileges the user now also holds).
- **Server text cannot drive the operator's terminal (review ext#10).**
  Besides `StatusMsg` and table cells, every server field gconsole prints
  is printed with its control characters escaped (`\u001B` …): the paths
  of `backup` / `export`, the counts and times of queries, updates and
  imports (`AnsNum`, `QueryTime`, `success_num`, `failed_num`), a login
  refusal's message and the quoted text of an answer that is not an `/api`
  envelope (stderr).
- **A Ctrl-C typed at a prompt is never lost (review ext#13).** The console
  took its Ctrl-C mark inside the read, after showing the prompt, and saw a
  Ctrl-C only as an interrupted blocking `read`, so one arriving between the
  prompt and the read was lost (the statement being typed was not
  discarded). The mark is now taken before the prompt is shown
  (`console::term::LineReader::read_line_since`), and the reader waits for
  stdin with `poll` and a 100 ms timeout, checking the Ctrl-C counter
  before each wait.

Developers and tests:

- `http_client::tls_client_stream` (feature `tls`, new, additive): a rustls
  client session over a caller's stream, verifying the server against a
  CA bundle — the `https://` side of `post_with` for callers that run
  their own exchange.
- New tests: `tests/api_session_tokens.rs` (the `/api` session path);
  in `tests/dt_gconsole.rs`
  `the_local_mode_saves_acknowledged_updates_when_it_leaves_a_database`,
  `the_local_mode_keeps_imports_writes_disk_databases_through_and_reports_a_failed_save`,
  `the_local_mode_ends_statements_like_the_remote_console`,
  `server_fields_cannot_drive_the_terminal`,
  `setpriv_refuses_invalid_numbers_and_sets_exactly_the_given_ones`,
  `setpriv_rolls_back_its_grant_when_the_revoke_fails`,
  `a_server_whose_api_takes_no_session_gets_the_password` (a fake
  gserver whose `POST /login` gives a session its `/api` refuses),
  `the_password_crosses_the_network_once_per_login` (a relay records what
  the client sends), `the_console_talks_to_a_tls_gserver` (feature `tls`:
  nothing crosses in clear text), `tls_needs_the_tls_feature` (default
  features), `a_gserver_is_ready_when_it_says_so_not_when_its_port_answers`;
  unit tests in `console::syntax`, `console::client`, `console::term`.
- `ctrl_c_cancels_the_running_query_and_the_statement_being_typed` no
  longer orders its signals with fixed sleeps (review ext#13, tests-a#14):
  it runs gconsole with `-i` and sends each Ctrl-C once the prompt it is
  meant for is visible, going on once the console has answered it (a fresh
  prompt, `Query cancelled.`). With `-i` the run's exit status is
  interactive (0); that a cancelled command fails a script stays asserted
  by `ctrl_c_cancels_the_servers_query_task` (review note in the test).
- The console tests no longer pick a port by binding `127.0.0.1:0` and
  freeing it, nor take any listener on it for a ready gserver (review
  tests-a#7): gserver binaries start with `-p 0` and the test reads the
  bound port from gserver's own startup line; the README-flow test, which
  must keep conf.ini's port, checks that gserver announces exactly that
  port and retries on another when gserver exits because the port was
  taken; "nothing listening" uses port 0 (also in the client unit test).
- Corrected test (D6): `console::commands::tests::privilege_numbers_and_bits`
  pinned the silent skipping of unknown words (`"9 x 0 7"` → `1 << 7`); it
  now expects the error (review ext#11 note in the test).

## GAP_PLAN status line

none

## Docs notes

- README.md (gconsole section): the console logs in once and sends a
  session token, not the password, with every request (falls back to
  upstream's per-request password against a server without `POST /login`
  or whose `/api` does not take its sessions);
  new options `--tls` and `--ca-file FILE` (feature `tls`) and the
  cleartext warning for a non-loopback host; `gconsole --local` saves a
  snapshot / new in-memory database with unsaved updates when it leaves it
  (quit, end of input, `load <other>`), and ends statements by the remote
  console's rule; `setpriv` accepts `0` alone for "no privilege" and
  refuses anything that is not a privilege number.
- README.md / docs/DEPLOY.md (upstream `/api` protocol, authentication): a
  request without `password` may present a `POST /login` session token
  (`Authorization: Bearer`, `X-Session-Token`, `session=` / `token=`) — a
  gstore-rust extension; a token is as good as the password until it
  expires (24 h), the user logs out, or the user's password changes. No
  compat switch: upstream clients always send a password, which is checked
  as before.
- docs/DEPLOY.md (TLS): `gconsole --tls --ca-file <the CA of gserver's
  certificate>` talks to `gserver --tls-cert … --tls-key …`; without TLS,
  use the console on the server's machine or through an SSH tunnel.
- docs/REFACTOR_BACKLOG.md: nothing deferred by this package.

## Measured numbers

- No hot path changed (the console, one branch of `/api` authentication
  taken only by requests without a password, an additive client function),
  so no `gstore-bench ci` run.
- The rewritten Ctrl-C test: 18 consecutive runs green, 8 of them with
  three CPU-bound busy loops on the 4-vCPU machine (other agents' builds
  running too); the reviewer's failure needed the console throttled to
  2 % of a CPU.
- `the_console_talks_to_a_tls_gserver` takes about 0.2 s. A plain-HTTP
  request to a gserver serving TLS was measured holding the client about
  31 s per request (see Requests/Not yet), so the test does not exercise
  that case.
- Suites on this branch (machine shared with other agents), toolchain
  1.99.1: `cargo test --workspace` with `GSTORE_DISK_QUERY_MODE=memory`
  and with `=stream`: 2952 passed, 0 failed, 8 ignored each;
  `cargo test -p gstore --features tls`: 2734 passed, 0 failed, 7 ignored;
  `cargo test -p gstore --features readline`: 2724 passed, 0 failed, 7 ignored;
  `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo clippy -p gstore --all-targets --features tls|readline -- -D
  warnings`: clean; `RUSTDOCFLAGS=-D warnings cargo doc -p gstore
  --no-deps` (default and `--features tls`): clean. (Re-run by the
  package's reviewer after its two commits — the ext#12 session check and
  the ext#5 guard test — on the final head.)

## Requests/Not yet

- Edits outside this package's files: `src/gapi/auth.rs` —
  `authenticate` (one branch at its start) and a new private
  `session_user` after it (the `/api` side of ext#12; nobody owns
  `gapi/auth.rs` in round 5); `src/http_client.rs` — new
  `tls_client_stream` (additive, before `DeadlineStream`; its `#[cfg(test)]`
  module untouched).
- For the owner of `src/server.rs`'s TLS accept path: a plain-HTTP request
  to a gserver serving TLS is not refused at once — the client waits about
  31 s (the read timeout) for the connection to close. Not a correctness
  problem; a prompt close (or an HTTP 400 in clear) would help users who
  forget `--tls`.
- Possible follow-up (not needed for ext#12): the pre-body admission of the
  upstream `/api` payload route (`src/preauth.rs`) takes credentials from
  the URL query only, not a session token, so a session-authenticated
  `/api` request body stays under `preauth_max_body` (64 MiB) — as the
  console's bodies with the password inside always were.
- tests-a#7's other sites, `free_port()` in `tests/dt_gserver_lifecycle.rs`,
  are not in this package's files; as the verifier noted they fail loudly
  (gserver `-b` exits non-zero and the port is read back from the pid file),
  so only the `dt_gconsole.rs` sites could run against the wrong server.
- Not in this package's ids, seen while here: two unit tests in
  `src/console` (`commands::tests::history_is_kept_and_capped`,
  `tests::ports_come_from_the_configuration`) use per-process names under
  `std::env::temp_dir()` and remove them only when they pass.
