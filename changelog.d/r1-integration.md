# r1-integration — Round 1 merge record: cross-package fixes and wiring

## Changelog

Behaviour changes first:

- **REST `/unload` and the reopen-in-place of `/load` of a database whose
  on-disk store failed answer success again** (HTTP 200, `StatusCode` 0, a
  `warning` naming the discarded changes), as r1-storage-failstate
  specified (review M5: gap-txn-lifecycle#4). After the merge they were
  answered HTTP 500 `"applied in memory but not durable"`: r1-durable-ack
  turned `Resp::ok_warn` into the answer to a *write* (review
  durability#2), and r1-storage-failstate's two replies used it. They now
  use a new `Resp::ok_with_warning`; every write keeps `ok_warn`'s
  non-durable failure. Covered by r1-storage-failstate's
  `tests/server_broken_store_recovery.rs` (two cases failed on the merge).
- **The pre-body credential check follows the loopback rule for root's
  default password** (review C-2: sec#1, gapi#1; H-10: sec#8, root's
  default password; sec#9 / gapi#22, proxy trust). r1-ingest-caps' check of credentials in a request head
  (`gapi::auth::check_before_body`) accepted `root` / `123456` from any
  client, while r1-http-surface's full check after the body (`verify`)
  refuses it from a client that is not local — a loopback reverse proxy
  forwarding `Forwarded` / `X-Forwarded-For` / `X-Real-IP`, unless
  `trust_proxy_headers=on`. A remote client behind such a proxy (on a
  loopback server started with `allow_default_root_password=on`) could thus
  send bodies up to the upload cap with the publicly known password. The
  pre-body hook now decides locality by the same rule (new
  `server::FORWARDING_HEADERS`, shared with `Request::is_forwarded`) and
  leaves those credentials unchecked — the body is held to the
  pre-authentication cap — without counting a failure, as `verify` does.
  Test: `tests/preauth_follows_the_credential_policy.rs`.
- r1-download's standalone `upstream-compat: anonymous_download=on` startup
  line is folded into `GStoreConfig::upstream_compat_warnings()`
  (convention 13c); a config unit test
  (`every_upstream_compat_switch_reports_one_startup_warning`) checks that
  each of round 1's six compat switches yields exactly one such line and
  the secure defaults none.
- `scripts/conformance.sh`: one copy of the compat helper, both round-1
  switches (`[server] allow_default_root_password=on`, `[upload]
  anonymous_download=on`) and one `check_compat_switches || exit 1` after
  the server start (r1-http-surface's authoritative position). Proved once
  locally: with the key misspelled (`anonymous_downlaod`) the run stops
  with `conformance: compat switch anonymous_downlaod did not take effect`.
- Docs next to the code: `config`'s module docs list `min_body_rate`,
  `max_conns_per_ip` and r1-download's `[upload]` download keys;
  `read_request`'s docs mention `min_body_rate`; gserver's
  `install_open_options` names `rocksdb_max_open_files` (as the
  r1-ingest-framing and r1-rocksdb fragments requested).

## GAP_PLAN status line

none

## Docs notes

- docs/DEPLOY.md (secure defaults table, closing step): the conformance
  harness turns on exactly two compat switches in round 1,
  `allow_default_root_password=on` and `anonymous_download=on`; both print
  their `upstream-compat:` startup line.
- docs/DEPLOY.md (behind a reverse proxy): root's default password is
  refused from a forwarded client both before the body (no upload cap) and
  after it (`1001`), unless `trust_proxy_headers=on`.

## Measured numbers

- Bench smoke gate at 8d2ccab (`target/release/gstore-bench ci --baseline
  bench/baselines/smoke.json`, `GSTORE_BENCH_CACHE` warm): PASS, 456 gates
  checked, 0 failed (148 table rows, all pass). Machine state: a feature
  test run (zip) was in the other build slot, load average 8.0 on 4 CPUs,
  so the figures are pessimistic; no gate came near its 3.00 bound.

## Requests/Not yet

- None of the packages' requests is left open: every standalone
  compat-warning line is folded, the conformance compat block is deduped,
  the doc-comment requests are applied. The requests addressed to later
  rounds (r2-txn, r2-limits-auth, r3-rest-parity, r3-sparql-joins,
  r4-cli-config) stay in the package fragments.
