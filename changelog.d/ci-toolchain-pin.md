# CI toolchain pin — CI on Rust 1.99.1, line-tables-only debuginfo, one build per feature job, clippy 1.99 fixes

## Changelog

- **CI pins its Rust toolchain.** `.github/workflows/ci.yml` and
  `.github/workflows/bench.yml` set `RUST_TOOLCHAIN: "1.99.1"` and install
  it with `dtolnay/rust-toolchain@master` (`toolchain:
  ${{ env.RUST_TOOLCHAIN }}`), instead of `dtolnay/rust-toolchain@stable`.
  When `@stable` moved to 1.99.1 (2026-10-31), its clippy brought new lints
  (`clippy::chunks_exact_to_as_chunks`, and a new
  `clippy::needless_borrows_for_generic_args` finding), so every
  `-D warnings` clippy step failed. In the feature matrix, clippy runs
  before the tests, so the `gzip`, `tls`, `zip`, `readline`, `pfn-wasm`,
  `zip pfn-wasm readline` and `rocksdb` tests stopped running in CI too.
  A new stable release no longer changes CI without a commit. Bumping the
  pin means fixing the new version's clippy and rustfmt findings in the
  same commit. Both workflows must name the same version, because
  bench.yml's `rocksdb-correctness` job shares ci.yml's RocksDB cache
  (`shared-key: rocksdb`), and rust-cache hashes the rustc version and
  every `CARGO*`/`RUST*` environment variable into the key.
- **Dev and test builds in CI keep line tables only.** ci.yml sets
  `CARGO_PROFILE_DEV_DEBUG` and `CARGO_PROFILE_TEST_DEBUG` to
  `line-tables-only` for every job. Backtraces still show file:line.
  With the default full debuginfo, the build step of the `test (gstore,
  --features zip pfn-wasm readline)` job filled the runner's disk while
  linking about 95 executables (68 integration tests, the binaries and
  their test harnesses, the examples), each of which links gstore. rust-lld
  then died with SIGBUS (signal 7) while writing
  `tests/dt_upstream_parser_suite`. Cargo.toml's `[profile.release]` and
  `[profile.test]` set no `debug`, so nothing overrides these variables.
  bench.yml's `rocksdb-correctness` job sets the same two variables, so it
  keeps hitting the shared RocksDB cache. The release builds (bench smoke,
  reference) are unaffected.
- **The feature-matrix jobs build each target once.** The `build` step of
  ci.yml's `features` job runs `cargo build --locked --profile test -p
  gstore --all-targets`, the profile its `test` step's `cargo test` uses,
  instead of the dev profile. Cargo.toml's `[profile.test]` sets
  `opt-level = 1`, so with the dev profile every unit got a different
  hash, and the test step compiled and linked gstore, its dependencies and
  every executable a second time, next to the dev set in `target/debug`.
  Now `cargo test` compiles nothing and only runs the tests, and the job's
  target directory holds one set of builds instead of two (measured
  below). The build step still checks `--locked` and builds every target.
- **Code changes for clippy 1.99, behaviour unchanged.**
  `src/query/hash.rs` (MD5, SHA-1, SHA-256 `update`, SHA-512/384, and
  `hex_decode`) and the rule parser in `src/reason/mod.rs` iterate
  `as_chunks::<N>()` instead of `chunks_exact(N)`. `as_chunks` is stable
  since Rust 1.88, within the crate's MSRV of 1.89. `Sha256::update` takes
  the remainder from the same call and drops its
  `try_into().expect(..)`. `Parser::parse_gfn_args`
  (`src/parser/sparql/mod.rs`) passes its error-context closure to
  `map_err` by value. No `#[allow]` was added, local or crate-wide.

## GAP_PLAN status line

none (CI maintenance).

## Docs notes

- README.md (the line saying `cargo clippy --workspace --all-targets --
  -D warnings` is clean): this holds on the toolchain CI pins, Rust 1.99.1
  (`RUST_TOOLCHAIN` in `.github/workflows/ci.yml`). A newer local clippy
  may report lints that CI does not see until the pin is bumped.
- bench/README.md (CI section, the `rocksdb-correctness` bullet saying
  its RocksDB build cache is shared with ci.yml's rocksdb job): the cache
  hits only while both workflows set the same `CARGO*`/`RUST*` environment,
  today `RUST_TOOLCHAIN`, `CARGO_PROFILE_DEV_DEBUG`,
  `CARGO_PROFILE_TEST_DEBUG`, `CARGO_TERM_COLOR` and `RUST_BACKTRACE`.
  Changing one of them in one workflow only makes the shared cache miss,
  and that job then builds librocksdb from source.
- docs/DEPLOY.md: no change. The Dockerfile's builder stays on the MSRV
  image (`rust:1.89-slim-bookworm`); it builds releases, not CI.

## Measured numbers

Measured on the 4 vCPU development VM, with the benchmark lock
(`flock /tmp/gstore-bench.lock`) held for each build.

- `CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only
  CARGO_PROFILE_TEST_DEBUG=line-tables-only cargo +1.99.1 build -p gstore
  --all-targets --features "zip pfn-wasm readline"` (1 min 14 s):
  `du -sh target/debug` grew from 791M (clippy metadata and the RocksDB
  build-script output) to 6.3G,
  so the build itself wrote 5.5 GiB. Its 95 linked executables take
  5,098 MiB: median 62.8 MiB, largest 97.5 MiB.
- One binary with full debuginfo, the default CI used before this change
  (`CARGO_PROFILE_DEV_DEBUG=full CARGO_PROFILE_TEST_DEBUG=full cargo
  +1.99.1 build -p gstore --features "zip pfn-wasm readline" --test
  dt_upstream_parser_suite`): `dt_upstream_parser_suite` is 164,603,976
  bytes (157 MiB). With line tables only it is 66,214,616 bytes (63 MiB,
  of which 34.6 MiB are `.debug_*` sections), 2.5 times smaller.
  Scaling the 95 executables by that ratio gives about 12.4 GiB with full
  debuginfo, before the dependencies and the restored rust-cache. GitHub
  documents 14 GB of SSD storage for its standard Linux runners, which
  explains the SIGBUS. The full-debuginfo build of all targets was not run.
- The whole `features` job for `zip pfn-wasm readline` after this change:
  its build, clippy and test steps in order, with CI's environment
  (`CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only
  CARGO_PROFILE_TEST_DEBUG=line-tables-only`, `cargo +1.99.1`, `TMPDIR`
  outside `/tmp`). Before it, `cargo clean -p gstore` left the target
  directory at 2,054,312,795 bytes (1.91 GiB: the RocksDB build-script
  output and dependencies built for other profiles and toolchains). Sizes
  from `du -sb target`:
  - `cargo build --locked --profile test -p gstore --all-targets
    --features "zip pfn-wasm readline"` (5 min 54 s, including the
    test-profile dependencies): +6,590,202,639 bytes (6.14 GiB). It links
    97 executables (13 binaries, their 13 unit-test harnesses, the
    library's, 68 integration tests, 2 examples; each file counted once,
    although cargo also hard-links the binaries and examples under a
    second name): 5,941 MiB, median 71.9 MiB, largest 93.8 MiB.
    `dt_upstream_parser_suite` is 75,940,592 bytes with the test profile
    (opt-level 1) and 66,214,616 bytes with the dev profile.
  - `cargo clippy -p gstore --all-targets --features "zip pfn-wasm
    readline" -- -D warnings` (34 s, clean): +64,302,701 bytes (61 MiB).
    It reused the dependencies' check metadata from earlier 1.99.1 clippy
    runs, so a cold job writes more in this step.
  - `cargo test -p gstore --features "zip pfn-wasm readline"` (1 min
    38 s): +15,772 bytes. It printed no `Compiling` line (`Finished` after
    0.14 s) and passed 1,911 tests (0 failed, 83 suites).
  - Afterwards the target directory held 8,708,833,907 bytes (8.11 GiB).
    That is an upper bound for the job, since 1.91 GiB of it was there
    before. rust-cache restores the dependencies' artifacts into the same
    directory (it removes the workspace crates' artifacts before saving),
    so the restored cache is part of these numbers, not added on top of
    them.
- The same job before this change: the dev-profile build step wrote
  5.5 GiB (first bullet), then `cargo test` built the test-profile set
  (6.14 GiB above) next to it, about 11.6 GiB of builds even with line
  tables only, against 6.2 GiB now. Reproduced on one target: after the
  test-profile build above, `cargo +1.99.1 build --locked -p gstore
  --features "zip pfn-wasm readline" --test dt_upstream_parser_suite`
  compiled gstore again (no `-C opt-level`, `-C
  metadata=1a00e1091ae235ed`) and linked
  `dt_upstream_parser_suite-acc62d9b3444a811` next to the test profile's
  `dt_upstream_parser_suite-e5df1ecd9a8d989a`, which `cargo test --no-run`
  reports as its executable.
- Without debuginfo (`CARGO_PROFILE_DEV_DEBUG=0`, as the local test runs
  below; cargo then also strips the standard library's debuginfo) the same
  binary is 13,943,752 bytes (13.3 MiB). Line tables add about 50 MiB to
  this binary; CI keeps them for file:line in backtraces.
- clippy with `-D warnings` is clean on 1.99.1 for `--workspace
  --all-targets` and for `-p gstore --all-targets` with each of `gzip`,
  `tls`, `zip`, `readline`, `pfn-wasm`, `zip pfn-wasm readline` and
  `rocksdb`, and on 1.97.0 for `--workspace --all-targets`.
  `cargo fmt --all -- --check` is clean on both. Rechecked after the
  features job's build step moved to the test profile (no Rust code
  changed): clippy on 1.99.1 for `zip pfn-wasm readline` (above), on
  1.97.0 for `--workspace --all-targets`, and `cargo fmt --all -- --check`
  on both are clean.
- Tests on 1.99.1 (`TMPDIR` outside `/tmp`, same lock):
  `cargo +1.99.1 test --workspace` passed 2,077 tests (1 ignored, 0
  failed, 92 suites); `cargo +1.99.1 test -p gstore --features "zip
  pfn-wasm readline"` passed 1,911 (0 failed, 83 suites).

## Requests/Not yet

- No CI job builds on the MSRV (1.89). The Docker image build is the only
  MSRV build. clippy reads `rust-version`, so it does not suggest APIs
  newer than 1.89, but a dependency bump could still raise the real MSRV
  unnoticed.
- The pin does not move by itself: a maintainer bumps `RUST_TOOLCHAIN` in
  both workflows, together with the fixes for the new version's clippy and
  rustfmt findings.
- For the integrator: this fragment is CI maintenance, not a Phase 6 work
  package, so it is not named `phase6-<ID>.md`. When R4-CLOSURE folds
  `changelog.d/*.md` into CHANGELOG.md, file its Changelog bullets under
  "Unreleased" as CI notes (for example a `#### CI` subsection), not in
  the Phase 6 section.
