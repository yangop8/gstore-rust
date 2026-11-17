# r5-packaging — Docker/deb/rpm build and smoke in CI, systemd/RPM/Debian hardening, conformance harness accounting, MSRV/advisory metadata, deployment docs

## Changelog

Behaviour changes:

- **`scripts/conformance.sh`: a skipped suite fails the run** (review
  infra#11). The Python SDK suite, which cannot run without python3's
  `requests`, used to count as passed ("all suites passed", exit 0), so CI
  stayed green with the suite gone. It is now reported as `SKIP` and the
  run exits 1 (`conformance: FAILED: suite(s) skipped: python`) unless
  `--allow-skip` accepts it. CI's conformance job keeps its invocation, so
  it now fails if the runner's python3 cannot import `requests`.
- **`scripts/conformance.sh`: the server picks its own port** (review
  tests-b#17). The port came from a python3 probe socket closed before
  gserver bound it (a race; a fixed fallback port without python3), and
  the `sed` writing it into `conf.ini` was unchecked. `conf/conf.ini` now
  says `port=0`, gserver binds a port of the kernel's choice, and the
  script writes the port from the server's pid file into `conf/conf.ini`
  for the upstream scripts, failing unless the first `port=` line reads
  it. The compat-switch block (convention 4) is unchanged and was proved
  once more with a misspelt key (`conformance: compat switch
  anonymous_downlaod did not take effect`, exit 1).
- **systemd unit: hardening** (review infra#14): `UMask=0027`, an empty
  `CapabilityBoundingSet`, `ProtectKernelLogs`, `ProtectClock`,
  `ProtectHostname`, `ProtectProc=invisible`, `RestrictRealtime`,
  `RestrictNamespaces`, `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`,
  `MemoryDenyWriteExecute` (the `pfn-wasm` runtime interprets), `RemoveIPC`,
  `SystemCallArchitectures=native`, `SystemCallFilter=@system-service`
  without `@privileged @resources` and `SystemCallErrorNumber=EPERM`.
  `systemd-analyze security --offline` rates the unit 1.2 (was 6.8). For
  administrators: files gserver creates are no longer world-readable; a
  port below 1024 needs a drop-in granting `CAP_NET_BIND_SERVICE`;
  `ProtectHome=yes` (unchanged, now documented) hides `/home`, `/root` and
  `/run/user`, so `--load-dir`, `db_path` and `backup_path` cannot be there.
- **systemd unit: a crash loop ends** (review infra#12):
  `StartLimitIntervalSec=300`, `StartLimitBurst=5` (with `RestartSec=5s`
  systemd's default 5 starts / 10 s was never reached, so a failing server
  restarted forever) and `RestartPreventExitStatus=1 2` (gserver's error
  and usage exits — a refused configuration such as root's default
  password beyond loopback, an unbindable port — are not restarted).
- **deb: the unit's enablement survives a remove** (review infra#15).
  prerm ran `systemctl disable` on remove, so `apt remove gstore` +
  `apt install gstore` left a service the administrator had enabled
  disabled. The scripts follow dh_installsystemd's protocol for a unit the
  package does not enable: prerm only stops the service
  (`deb-systemd-invoke`), postrm masks the unit on remove and forgets it on
  purge (`deb-systemd-helper`), postinst unmasks it and re-enables it when
  its enable links are still there. The systemd steps and postrm's purge
  paths honour `DPKG_ROOT`.
- **rpm: conf.ini is installed with group `gstore`** (review infra#15). It
  was packaged `root:root` and re-grouped by `%post`, so `rpm -V gstore`
  reported a mismatch and `rpm --setugids gstore` made it unreadable to
  gserver. A new `%pre` (`packaging/rpm/pre_install.sh`) creates the
  gstore user and group first (`systemd-sysusers` with the sysusers.d line,
  else `groupadd` / `useradd`), the asset carries `group = "gstore"`, and
  `%post` only gives the (unowned) `/etc/gstore` its group and mode.
- **Container health check** (review infra#17): `HEALTHCHECK` ran
  `curl http://127.0.0.1:9000/api?operation=check`, so another port or
  TLS made the container unhealthy, an IP allow list without 127.0.0.1
  refused it, and every probe was an access-log record. It is now
  `/usr/local/bin/gstore-healthcheck`: `gserver -S` with PID 1's own
  arguments in its working directory, which finds the pid file and
  connects to the address the server bound, without a request. The image
  no longer installs `curl`.
- **Image and packages ship every binary target** (review infra#7): the
  Dockerfile installs them with `cargo install --path . --locked` (no list),
  the deb takes them from Cargo (`"$auto"`, which also brings the README to
  the same `/usr/share/doc/gstore/README.md`); the rpm list (cargo-generate-rpm
  has no automatic assets) is checked against the targets.
- **Packages built with the image's features** (review infra#19): `cargo
  deb` builds with `features = ["pfn-wasm"]`, both package descriptions say
  so, and DEPLOY.md's build commands use `--features pfn-wasm`. gnlqa's
  `rust-version` is the workspace MSRV 1.89 (it said 1.82 while depending
  on gstore's 1.89).

New files and tools:

- `scripts/package-smoke.sh` (`stage [--deb FILE]`, `installed
  [--service]`, `image IMAGE`): every binary target installed and
  answering `--help`, `systemd-analyze verify`, the first-start root
  password (the default refused), the image health check (no access-log
  record), `/api` check / login / build / load / INSERT DATA / SELECT,
  REST `/status`, a graceful stop, a second start without the password
  file; the image mode also a container on port 9100 turning healthy and a
  container without a password refusing to start (exit 1).
- `.github/workflows/packaging.yml`: `packages` (release build with
  `pfn-wasm`, `cargo deb` / `cargo generate-rpm` at pinned versions, the
  .deb's `/usr/bin` against `cargo metadata`, the stage smoke of the .deb's
  content, the .deb installed on the runner and smoked through systemd,
  enable → remove → install → purge, the .rpm installed and smoked in a
  Fedora container with `rpm -V`), `image` (`docker build`, which is also
  the MSRV build, then the image smoke) and `advisories` (`cargo audit`,
  `continue-on-error`). Push / PR (not for docs-only changes), weekly,
  manual.
- `packaging/rpm/pre_install.sh`, `packaging/docker/gstore-healthcheck`
  (`.dockerignore` lets it into the build context).
- Tests: `tests/conformance_harness.rs` (infra#11, tests-b#17: the script
  against this build with a python3 shim), `tests/packaging_scripts.rs`
  (infra#15: prerm/postrm/postinst with the real `deb-systemd-helper` in a
  `DPKG_ROOT`, skipped where not installed; the rpm `%pre` with shims),
  `tests/packaging_layout.rs` (infra#7/#12/#14/#15/#17/#19 guards: rpm and
  DEPLOY.md binary lists vs the targets, one MSRV, the packages' features,
  the rpm conf.ini group, the unit's start limit and hardening,
  `systemd-analyze verify` where installed, the health check, the Compose
  example), `tests/container_healthcheck.rs` (infra#17, against a server on
  a free port), `tests/package_smoke.rs` (infra#7: the stage smoke against
  the test build; skipped without `curl`). Every one fails on the
  unfixed files except `systemd_analyze_verifies_the_unit`, a
  no-regression guard (the old unit verified too).
- `tests/packaging_first_start.rs`: its postrm check now reads the purge
  line in its `DPKG_ROOT` form (review infra#15 comment; the purge itself
  is run by tests/packaging_scripts.rs).

## GAP_PLAN status line

§8 / §9 Phase 5 row (打包): Docker 镜像与 deb / rpm 包由
`.github/workflows/packaging.yml` 构建、安装并冒烟(`scripts/package-smoke.sh`;
deb 在 runner 上经 systemd 启动,rpm 在 Fedora 容器中 `rpm -V`,镜像在非默认
端口上 healthy),review infra#7。

## Docs notes

- README.md (review gap-doc-claims#9), line ~170, "HTTP server" bullet:
  replace "(the upstream SDKs work unchanged)" with "(upstream's Python SDK
  works unchanged, as `scripts/conformance.sh` checks; the other upstream
  SDKs speak the same protocol but are not tested)".
- README.md (review gap-doc-claims#9), `/api` section, line ~419: replace
  "so the upstream SDKs such as `GstoreConnector.py` work against it
  unchanged" with "so upstream's Python SDK (`GstoreConnector.py`) works
  against it unchanged (`scripts/conformance.sh`); upstream's Java, C++,
  Node.js, PHP and Go SDKs speak the same protocol but are not tested".
- README.md, testing section (line ~447, "`scripts/conformance.sh` starts
  the real `gserver`…"): add that a suite that cannot run (the Python one
  without python3's `requests`) fails the run unless `--allow-skip`, and
  that the server runs on a port the kernel picks.
- README.md, wherever the packages / Docker are introduced: point to
  docs/DEPLOY.md's new "Package smoke" section and to `packaging.yml`.
- docs/DEPLOY.md: written by this package (its own sections): the intro's
  SDK claim (gap-doc-claims#9), the image (cargo install, health check,
  no curl), the Compose example (a root password through a secret, the
  port published on loopback; infra#12), the systemd restart policy and
  hardening with what each setting means for an administrator (infra#12,
  infra#14), the package build commands (features, pinned tool versions,
  `CARGO_PROFILE_RELEASE_STRIP=symbols` instead of a hand-kept `strip`
  list), the binary list with gcompact, the first-start password, the
  enablement rules (infra#15) and "Package smoke" (infra#7). No new D1
  compat switch: nothing for the compat-switch table.
- CHANGELOG.md: the Changelog bullets above; and the Phase 5 "Not yet" item
  "The Docker image and the deb / rpm packages were built and linted but
  not yet installed and run on a live host in CI" is done by
  `packaging.yml` (review infra#7).
- docs/REFACTOR_BACKLOG.md I.7, the MSRV bullet ("没有 CI 作业在 MSRV(1.89)上构建,只有
  Docker 镜像用 MSRV"): the Docker image is now built in CI
  (`packaging.yml`'s `image` job, `rust:1.89`, `--features pfn-wasm`), so
  that combination is an MSRV CI build; the default feature set, the other
  features and the tests still are not built on 1.89 (review infra#19).
- docs/REFACTOR_BACKLOG.md, new items (review ids, reasons):
  - infra#17: the health check is a connection-level probe (`gserver -S`
    connects to the bound address and sends nothing). A server whose
    accept loop hangs while its socket stays open counts as healthy until
    the backlog fills. An HTTP-level probe that is neither logged nor
    filtered needs a gserver-side exemption or subcommand
    (`src/bin/gserver.rs`, the access-log path in `src/http_api.rs`), which
    this package does not own.
  - infra#15: cargo-generate-rpm 0.21.1's `dir = true` assets fail to
    build (`with_file("")`, ENOENT), so `/etc/gstore` is not an rpm entry;
    `%post` gives it group gstore and mode 0750 (it is unowned, so
    `rpm -V` / `--setperms` do not cover it). Own it once the tool can.
  - infra#15: the rpm has no `Requires(pre)` on the user tools (Fedora's
    `shadow-utils`, openSUSE's `shadow`); `%pre` warns when the group
    cannot be created, conf.ini then gets group root.
  - infra#15: the deb's `DPKG_ROOT` support covers the systemd steps and
    postrm's purge paths; postinst's user / tmpfiles / chgrp steps still
    act on the live system.
  - infra#7: the rpm's binary list stays in Cargo.toml (cargo-generate-rpm
    has no automatic binary assets); tests/packaging_layout.rs and
    packaging.yml keep it equal to the targets.
  - The unit is installed in `/lib/systemd/system` by the deb (current
    Debian prefers `/usr/lib/systemd/system`; cargo-deb's `systemd-units`
    could take over the unit and its maintainer-script snippets).
  - infra#12: `RestartPreventExitStatus=1 2` keys on gserver's general
    error status, which also covers failures a retry could cure (a
    `listen_addr` not yet configured at boot despite
    `After=network-online.target`, a port still held by another process):
    those now leave the service failed instead of retrying. A dedicated
    exit status for a refused configuration (src/bin/gserver.rs, outside
    this package) would let the unit prevent only that.

## Measured numbers

No hot path touched (convention 8): the bench smoke gate was not run.
Machine shared with three other implementers' builds.

- `systemd-analyze security --offline=yes` (systemd 255) on
  `packaging/systemd/gserver.service`: 6.8 MEDIUM before, 1.2 OK after.
- `scripts/conformance.sh --no-build` (test-profile binaries, all six
  suites, port 33225 chosen by the kernel): basic 5 s, update 2 s,
  transaction 2 s, api 31 s, parser 10 s (known 15 17), python 4 s
  (`sdk_check: 49 passed`); "all suites passed", exit 0.
- `cargo build --release --locked -p gstore --bins --features pfn-wasm`:
  4 min 54 s. `cargo deb -p gstore --no-build` (cargo-deb 3.8.0):
  `gstore_0.1.0-1_amd64.deb` 9.4 MB, 14 binaries in `/usr/bin`, conffiles
  conf.ini and the two properties examples. `cargo generate-rpm`
  (0.21.1, from an unstripped build): 11.5 MB; header dump: conf.ini
  `0100640 root:gstore`, `%pre` / `%post` / `%preun` present, 14 binaries.
- `scripts/package-smoke.sh stage` against target/release and
  `stage --deb` against that .deb: passed (about 1 s each);
  `tests/package_smoke.rs` against the test build: 1.0 s.
- Not run here: `docker build` and the image smoke (no Docker daemon in
  the sandbox), the `installed` smoke (it installs a package system-wide),
  the Fedora rpm smoke, `cargo audit` (advisory DB from GitHub), actionlint
  (not installed; running a downloaded binary was not permitted; the
  workflow was parsed with PyYAML and its `run` blocks with `bash -n`).
  The proof of those is the GitHub run of `packaging.yml`.
- Package review (same branch): the image's builder toolchain checked
  without Docker — `cargo check --locked -p gstore --bins --features
  pfn-wasm` and `cargo check --locked -p gnlqa` with rustc 1.89.0 (the
  Dockerfile's `RUST_VERSION`, the declared MSRV of every workspace
  member): both pass. The packages job's `.deb` binary-list step was
  fixed before its first run: its `sed` turned the archive's own
  `./usr/bin/` directory entry (cargo-deb writes one) into an empty name,
  so the list never matched (reproduced with a `dpkg-deb -b` package).
  Full `scripts/conformance.sh --no-build` again: six suites PASS, port
  picked by the kernel.

## Requests/Not yet

- Shared edits outside the owned files: `gnlqa/Cargo.toml` `rust-version`
  1.82 → 1.89 (one line; r5-gnlqa owns gnlqa/** this round and did not
  touch that line); `.dockerignore` one exception line
  (`!packaging/docker/gstore-healthcheck`); `tests/packaging_first_start.rs`
  `the_unit_hands_the_password_file_to_gserver`, one assertion (the
  purge line's `DPKG_ROOT` form, with a review infra#15 comment).
- `.github/workflows/ci.yml` is unchanged: the conformance job's
  invocation (`scripts/conformance.sh`, no `--allow-skip`) already gets
  the new skip rule.
- Final proof of infra#7 (and of the hardened unit under a real systemd,
  the rpm in Fedora, the image's health check and MSRV build) is the
  GitHub run of `packaging.yml` after the round is pushed.
- The deferrals with reasons are listed under Docs notes →
  REFACTOR_BACKLOG (infra#17 connection-level probe, the rpm's
  `/etc/gstore` entry and `Requires(pre)`, partial `DPKG_ROOT`, the rpm
  binary list, `/lib/systemd/system`).
