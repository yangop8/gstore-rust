#!/usr/bin/env bash
# Cross-version format compatibility against the pre-Phase-6 binaries
# (review gap-doc-claims#8; docs/STORAGE_BACKEND.md "兼容矩阵").
#
#   scripts/compat-fixtures.sh [--generate] [--offline] [--keep] [--work DIR]
#
# Builds the pinned commit eaf5320 (OLD_REV below) in a scratch git worktree
# with its own CARGO_TARGET_DIR, and then checks every direction of the
# compatibility matrix with real binaries of both versions:
#
#  1. the OLD gbuild/gquery write a snapshot (snapshot) and an on-disk
#     version-1 database (disk-v1) from testdata/compat/data.nt, add named
#     graphs (named.ru) and delete a triple (delete.ru); the CURRENT code
#     opens and queries them (tests/format_compat.rs, run against these
#     fresh fixtures and against the checked-in testdata/compat/eaf5320);
#  2. the OLD gquery reads a snapshot and a version-1 disk database
#     (GSTORE_ID_RECLAIM=off) that the CURRENT gbuild wrote;
#  3. the OLD gquery refuses a version-2 disk database (the default
#     id_reclaim=on) with "not a gStore KV file";
#  4. the OLD library's restore_incremental fails on an increment holding
#     a CLEAR record that the CURRENT code wrote ("unknown redo.log op 'C'").
#
# --generate also replaces testdata/compat/eaf5320/ with the fresh fixtures
# (LOCK files left out). --offline builds with `cargo --offline`. --keep
# keeps the scratch worktree and build directories (a rerun with the same
# --work DIR is then incremental). CARGO (default `cargo`) is the cargo
# command, e.g. CARGO="cargo +1.99.1". Run weekly by .github/workflows/compat.yml.
set -euo pipefail

OLD_REV=eaf53202cb9cf2e81493ed8a49480bf7fbf81d83
CARGO=${CARGO:-cargo}
GENERATE=0
OFFLINE=()
KEEP=0
WORK=""
while [ $# -gt 0 ]; do
    case "$1" in
        --generate) GENERATE=1 ;;
        --offline) OFFLINE=(--offline) ;;
        --keep) KEEP=1 ;;
        --work) WORK=$2; shift ;;
        -h | --help) sed -n '2,29p' "$0"; exit 0 ;;
        *) echo "compat-fixtures: unknown argument '$1'" >&2; exit 2 ;;
    esac
    shift
done

REPO=$(cd "$(dirname "$0")/.." && pwd)
DATA="$REPO/testdata/compat"
OWN_WORK=0
if [ -z "$WORK" ]; then
    WORK=$(mktemp -d "${TMPDIR:-/tmp}/gstore-compat.XXXXXX")
    OWN_WORK=1
fi
mkdir -p "$WORK"
WORK=$(cd "$WORK" && pwd)
OLD_SRC="$WORK/old-src"
OLD_TARGET="$WORK/target-old"

say() { echo "compat-fixtures: $*"; }
fail() { echo "compat-fixtures: FAIL: $*" >&2; exit 1; }

cleanup() {
    if [ "$KEEP" = 0 ]; then
        git -C "$REPO" worktree remove --force "$OLD_SRC" >/dev/null 2>&1 || true
        # A --work DIR given by the caller keeps everything but what this
        # script put there; a directory it made itself goes as a whole.
        rm -rf "$OLD_SRC" "$OLD_TARGET" "$WORK/old-run" "$WORK/fixtures" "$WORK/new" "$WORK/export"
        if [ "$OWN_WORK" = 1 ]; then rm -rf "$WORK"; else rmdir "$WORK" 2>/dev/null || true; fi
    fi
}
trap cleanup EXIT

# ---- the old binaries -------------------------------------------------------
if ! git -C "$REPO" cat-file -e "$OLD_REV^{commit}" 2>/dev/null; then
    say "fetching $OLD_REV"
    git -C "$REPO" fetch --no-tags origin "$OLD_REV"
fi
if [ ! -e "$OLD_SRC/.git" ]; then
    git -C "$REPO" worktree add --detach "$OLD_SRC" "$OLD_REV" >/dev/null
fi
[ "$(git -C "$OLD_SRC" rev-parse HEAD)" = "$OLD_REV" ] || fail "$OLD_SRC is not at $OLD_REV"
say "building $OLD_REV (gbuild, gquery)"
(cd "$OLD_SRC" && CARGO_TARGET_DIR="$OLD_TARGET" $CARGO build "${OFFLINE[@]}" --locked \
    --bin gbuild --bin gquery)
OLD_BIN="$OLD_TARGET/debug"

# ---- the current binaries ---------------------------------------------------
say "building the current gbuild, gquery"
(cd "$REPO" && $CARGO build "${OFFLINE[@]}" --bin gbuild --bin gquery)
NEW_BIN="${CARGO_TARGET_DIR:-$REPO/target}/debug"

# ---- 1. fixtures written by the old binaries --------------------------------
# The old binaries run in $RUN (they write <name>.db there); the fixtures are
# then laid out as in testdata/compat/eaf5320: snapshot/ and disk-v1/ (not
# *.db, which .gitignore leaves out).
RUN="$WORK/old-run"
GEN="$WORK/fixtures"
rm -rf "$RUN" "$GEN"
mkdir -p "$RUN" "$GEN"
for kind in snap disk; do
    flag=()
    [ "$kind" = disk ] && flag=(--disk)
    (cd "$RUN" \
        && "$OLD_BIN/gbuild" "${flag[@]}" "$kind" "$DATA/data.nt" \
        && "$OLD_BIN/gquery" "$kind" "$DATA/named.ru" \
        && "$OLD_BIN/gquery" "$kind" "$DATA/delete.ru") >"$RUN/$kind.log" 2>&1 \
        || { cat "$RUN/$kind.log" >&2; fail "the old binaries could not write $kind.db"; }
    # The old binary's own answer, for the record.
    (cd "$RUN" && "$OLD_BIN/gquery" "$kind" -e "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }") \
        >>"$RUN/$kind.log" 2>&1
    grep -q '"23"' "$RUN/$kind.log" || { cat "$RUN/$kind.log" >&2; fail "old $kind.db: not 23 triples"; }
    rm -f "$RUN/$kind.db/LOCK"
done
cp -R "$RUN/snap.db" "$GEN/snapshot"
cp -R "$RUN/disk.db" "$GEN/disk-v1"
head -c 8 "$GEN/disk-v1/kvstore.kv" | grep -q '^GSTOREKV$' || fail "the old disk database is not version 1"
say "old binaries wrote: $(cd "$GEN" && du -b snapshot/* disk-v1/* | tr '\t' ' ' | tr '\n' ';')"

if [ "$GENERATE" = 1 ]; then
    rm -rf "$DATA/eaf5320"
    mkdir -p "$DATA/eaf5320"
    cp -R "$GEN/snapshot" "$GEN/disk-v1" "$DATA/eaf5320/"
    say "testdata/compat/eaf5320 regenerated by $OLD_REV"
fi

say "current code against the fresh fixtures"
(cd "$REPO" && GSTORE_COMPAT_FIXTURES="$GEN" $CARGO test "${OFFLINE[@]}" --test format_compat)
say "current code against the checked-in fixtures"
(cd "$REPO" && $CARGO test "${OFFLINE[@]}" --test format_compat)

# ---- 2./3. databases written by the current gbuild, read by the old gquery ---
NEW="$WORK/new"
rm -rf "$NEW"
mkdir -p "$NEW"
old_count() { # <db name> -> the old gquery's output for COUNT(*)
    (cd "$NEW" && "$OLD_BIN/gquery" "$1" -e "SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }" 2>&1) || true
}
(cd "$NEW" \
    && "$NEW_BIN/gbuild" --layout snapshot nsnap "$DATA/data.nt" \
    && GSTORE_ID_RECLAIM=off "$NEW_BIN/gbuild" --disk nv1 "$DATA/data.nt" \
    && "$NEW_BIN/gbuild" --disk nv2 "$DATA/data.nt") >"$NEW/build.log" 2>&1 \
    || { cat "$NEW/build.log" >&2; fail "the current gbuild failed"; }
head -c 8 "$NEW/nv1.db/kvstore.kv" | grep -q '^GSTOREKV$' || fail "GSTORE_ID_RECLAIM=off did not write version 1"
head -c 8 "$NEW/nv2.db/kvstore.kv" | grep -q '^GSTOREK2$' || fail "the default build did not write version 2"
for db in nsnap nv1; do
    out=$(old_count "$db")
    echo "$out" | grep -q '"24"' || { echo "$out" >&2; fail "the old gquery cannot read the current $db"; }
    say "old gquery reads the current $db"
done
out=$(old_count nv2)
if echo "$out" | grep -q '"24"'; then
    fail "the old gquery read a version-2 kvstore.kv"
fi
echo "$out" | grep -q "not a gStore KV file" \
    || { echo "$out" >&2; fail "the old gquery did not refuse version 2 with 'not a gStore KV file'"; }
say "old gquery refuses version 2: $(echo "$out" | head -n 1)"

# ---- 4. redo-log downgrade --------------------------------------------------
EXPORT="$WORK/export"
rm -rf "$EXPORT"
mkdir -p "$EXPORT"
(cd "$REPO" && GSTORE_COMPAT_EXPORT="$EXPORT" $CARGO test "${OFFLINE[@]}" --test format_compat \
    -- --ignored --exact export_a_clear_increment_for_old_binaries)
grep -q '^REDO C ' "$EXPORT/inc1/incr.log" || fail "the increment holds no CLEAR record"
mkdir -p "$OLD_SRC/tests"
cat >"$OLD_SRC/tests/zz_compat_restore.rs" <<'RS'
// Written by scripts/compat-fixtures.sh: eaf5320's restore_incremental on
// an increment the current code wrote.
#[test]
fn restore() {
    let dir = std::path::PathBuf::from(std::env::var_os("GSTORE_COMPAT_EXPORT").unwrap());
    match gstore::Database::restore_incremental(dir.join("base"), &[dir.join("inc1")]) {
        Ok(db) => println!("COMPAT-RESTORE: ok, {} triples", db.triple_num()),
        Err(e) => println!("COMPAT-RESTORE: err: {e}"),
    }
}
RS
out=$(cd "$OLD_SRC" && GSTORE_COMPAT_EXPORT="$EXPORT" CARGO_TARGET_DIR="$OLD_TARGET" \
    $CARGO test "${OFFLINE[@]}" --locked --test zz_compat_restore -- --nocapture 2>&1) \
    || { echo "$out" >&2; fail "the old restore test did not run"; }
rm -f "$OLD_SRC/tests/zz_compat_restore.rs"
line=$(echo "$out" | grep 'COMPAT-RESTORE:' || true)
echo "$line" | grep -q "unknown redo.log op 'C'" \
    || { echo "$out" >&2; fail "the old restore_incremental did not fail on the CLEAR record: $line"; }
say "old restore_incremental refuses the CLEAR record: ${line#COMPAT-RESTORE: }"

say "OK"
