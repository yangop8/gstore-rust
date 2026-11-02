# CI toolchain pin — CI on Rust 1.99.1, line-tables-only debuginfo, clippy 1.99 fixes

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
  With the default full debuginfo, the `test (gstore, --features zip
  pfn-wasm readline)` job filled the runner's disk while linking about 95
  executables (68 integration tests, the binaries and their test
  harnesses, the examples), each of which links gstore. rust-lld then died
  with SIGBUS (signal 7) while writing `tests/dt_upstream_parser_suite`.
  Cargo.toml's `[profile.release]` and `[profile.test]` set no `debug`, so
  nothing overrides these variables. bench.yml's `rocksdb-correctness` job
  sets the same two variables, so it keeps hitting the shared RocksDB
  cache. The release builds (bench smoke, reference) are unaffected.
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
- Without debuginfo (`CARGO_PROFILE_DEV_DEBUG=0`, as the local test runs
  below; cargo then also strips the standard library's debuginfo) the same
  binary is 13,943,752 bytes (13.3 MiB). Line tables add about 50 MiB to
  this binary; CI keeps them for file:line in backtraces.
- clippy with `-D warnings` is clean on 1.99.1 for `--workspace
  --all-targets` and for `-p gstore --all-targets` with each of `gzip`,
  `tls`, `zip`, `readline`, `pfn-wasm`, `zip pfn-wasm readline` and
  `rocksdb`, and on 1.97.0 for `--workspace --all-targets`.
  `cargo fmt --all -- --check` is clean on both.
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
