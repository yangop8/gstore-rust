#!/usr/bin/env bash
# Upstream conformance suite: run upstream gStore 1.4's own test scripts and
# Python SDK against the Rust `gserver`.
#
#   scripts/conformance.sh [--release] [--keep] [--no-build] [SUITE...]
#
# SUITE is any of: basic update transaction api parser python (default: all).
#   --release   build and run the release binaries (default: the dev profile)
#   --keep      keep the temporary server home (and its logs) afterwards
#   --no-build  use the binaries already built
#
# What it does:
#   1. builds the Rust binaries (`cargo build --bins`);
#   2. lays out a temporary GSTORE_HOME the way upstream's scripts expect it
#      (they run from GSTORE_HOME and use relative paths): conf/conf.ini (from
#      conf/conf.ini.example, with a free port), bin/ (the built binaries),
#      data/ (testdata/upstream/data, plus testdata/lubm/lubm.nt),
#      scripts/test/ (tests/upstream_scripts/*.sh and the parser_test data),
#      api/http/python/ (tests/upstream_scripts/python);
#   3. starts `bin/gserver -b` there (background mode, as upstream's
#      scripts/test/test.sh does), allowed to read data/ and scripts/test/;
#   4. runs each suite from GSTORE_HOME and checks what it printed (the
#      upstream scripts print the server's answers but mostly do not judge
#      them; the checks below do, see check_<suite>);
#   5. stops the server (`bin/gserver -t`) and prints a pass/fail summary.
#      The exit status is non-zero when a suite failed.
#
# The upstream scripts are copied verbatim from upstream commit bebe1ea
# (tests/upstream_scripts/README.md lists the adaptations: none for the shell
# scripts, which find the port in conf/conf.ini and their data relative to
# GSTORE_HOME; two for the Python examples). The Python suite is skipped when
# python3 or its `requests` module is missing.
set -uo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
PROFILE=dev
KEEP=0
BUILD=1
SUITES=()
for arg in "$@"; do
    case "$arg" in
        --release) PROFILE=release ;;
        --keep) KEEP=1 ;;
        --no-build) BUILD=0 ;;
        -h|--help) sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        basic|update|transaction|api|parser|python) SUITES+=("$arg") ;;
        *) echo "conformance: unknown argument '$arg' (try --help)" >&2; exit 2 ;;
    esac
done
[ ${#SUITES[@]} -eq 0 ] && SUITES=(basic update transaction api parser python)

TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"
if [ "$PROFILE" = release ]; then BIN_DIR="$TARGET_DIR/release"; else BIN_DIR="$TARGET_DIR/debug"; fi

say() { printf '\n\033[1;34m==> %s\033[0m\n' "$*"; }

# ---- 1. build ------------------------------------------------------------------
if [ "$BUILD" = 1 ]; then
    say "building the gStore binaries ($PROFILE)"
    if [ "$PROFILE" = release ]; then
        (cd "$REPO" && cargo build --bins --release) || exit 1
    else
        (cd "$REPO" && cargo build --bins) || exit 1
    fi
fi
[ -x "$BIN_DIR/gserver" ] || { echo "conformance: no $BIN_DIR/gserver" >&2; exit 1; }

# ---- 2. the server home --------------------------------------------------------
free_port() {
    if command -v python3 >/dev/null 2>&1; then
        python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
        return
    fi
    local p
    for _ in $(seq 1 50); do
        p=$((20000 + RANDOM % 40000))
        if ! (exec 3<>"/dev/tcp/127.0.0.1/$p") 2>/dev/null; then echo "$p"; return; fi
    done
    echo 19000
}

GHOME=$(mktemp -d "${TMPDIR:-/tmp}/gstore-conformance.XXXXXX")
GHOME=$(cd "$GHOME" && pwd -P)
LOGS="$GHOME/conformance-logs"
PORT=$(free_port)
mkdir -p "$GHOME/conf" "$GHOME/bin" "$GHOME/data/lubm" "$GHOME/scripts/test" "$GHOME/api/http" "$LOGS"
sed -e "s/^port=9000\$/port=$PORT/" "$REPO/conf/conf.ini.example" >"$GHOME/conf/conf.ini"
for b in "$BIN_DIR"/g*; do
    [ -x "$b" ] && [ -f "$b" ] && ln -s "$b" "$GHOME/bin/$(basename "$b")"
done
cp -R "$REPO/testdata/upstream/data/bbug" "$REPO/testdata/upstream/data/small" \
    "$REPO/testdata/upstream/data/num" "$GHOME/data/"
cp "$REPO/testdata/upstream/data/update_test.nt" "$REPO/testdata/upstream/data/all.sql" "$GHOME/data/"
# Upstream's data/lubm/lubm.nt is testdata/lubm/lubm.nt (byte-identical).
cp "$REPO/testdata/lubm/lubm.nt" "$REPO"/testdata/upstream/data/lubm/*.rq "$GHOME/data/lubm/"
cp "$REPO"/tests/upstream_scripts/*.sh "$GHOME/scripts/test/"
cp -R "$REPO/testdata/upstream/scripts/test/parser_test" "$GHOME/scripts/test/"
cp -R "$REPO/tests/upstream_scripts/python" "$GHOME/api/http/python"

stop_server() {
    if [ -f "$GHOME/dbhome/gserver.pid" ]; then
        (cd "$GHOME" && bin/gserver -t) >>"$LOGS/gserver-stop.log" 2>&1 \
            || (cd "$GHOME" && bin/gserver -k) >>"$LOGS/gserver-stop.log" 2>&1
    fi
}
cleanup() {
    stop_server
    if [ "$KEEP" = 1 ]; then
        echo "conformance: kept the server home $GHOME (logs in $LOGS)"
    else
        rm -rf "$GHOME"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# ---- 3. start the server -------------------------------------------------------
say "starting gserver -b on port $PORT in $GHOME"
if ! (cd "$GHOME" && bin/gserver -b --load-dir data --load-dir scripts/test) >"$LOGS/gserver-start.log" 2>&1; then
    cat "$LOGS/gserver-start.log"
    exit 1
fi
cat "$LOGS/gserver-start.log"

# ---- 4. the suites -------------------------------------------------------------
declare -A RESULT
declare -A DETAIL
FAILED=0

strip_colors() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }
# Every "StatusCode":N of a log, in order, space-separated.
status_codes() { grep -o '"StatusCode":-\{0,1\}[0-9]*' "$1" | cut -d: -f2 | tr '\n' ' ' | sed 's/ $//'; }
repeat() { local out="" i; for ((i = 0; i < $2; i++)); do out+="$1 "; done; echo "${out% }"; }

# Run scripts/test/<script> from GSTORE_HOME (stdout+stderr to the suite log).
run_script() {
    local suite=$1 script=$2
    shift 2
    (cd "$GHOME" && bash "scripts/test/$script" "$@") >"$LOGS/$suite.log" 2>&1
}

# basic_test.sh: build / batchRemove / batchInsert / small_add counts are
# judged by the script (✅ 一致 / ❌ 不一致); its query lines print ✅
# unconditionally, so each "查询 N: 实际=A 预期=B" must have A = B — except
# num's first three: the script records 0 0 0 (its extract_json_num default
# for an answer without AnsNum), the SPARQL answers are 8 1 4
# (tests/upstream_basic.rs).
check_basic() {
    local log=$LOGS/basic.log
    run_script basic basic_test.sh || { echo "basic_test.sh exited non-zero"; return 1; }
    strip_colors "$log" | awk '
        /===== 处理数据库：/ { ds = $0; sub(/.*处理数据库：/, "", ds); sub(/_[0-9]+ =====.*/, "", ds) }
        /✅ 一致/ { same++ }
        /❌/ { print "mismatch: " ds ": " prev; bad++ }
        /^查询 [0-9]+: 实际=/ {
            n = $0; sub(/^查询 /, "", n); sub(/:.*/, "", n)
            a = $0; sub(/.*实际=/, "", a); sub(/ .*/, "", a)
            e = $0; sub(/.*预期=/, "", e); sub(/ .*/, "", e)
            if (ds == "num" && n == 1) e = 8
            if (ds == "num" && n == 2) e = 1
            if (ds == "num" && n == 3) e = 4
            queries++
            if (a != e) { print "query " ds " #" n ": AnsNum " a ", expected " e; bad++ }
        }
        { prev = $0 }
        END {
            if (same != 13) { print "expected 13 matching build/batch counts, saw " same; bad++ }
            if (queries != 37) { print "expected 37 queries, saw " queries; bad++ }
            exit (bad > 0 ? 1 : 0)
        }'
}

# update_test.sh (default mode): build lubm, load, 10 single-triple inserts
# (each "update query returns true." with AnsNum 1), unload, drop.
check_update() {
    local log=$LOGS/update.log codes inserts
    run_script update update_test.sh || { echo "update_test.sh exited non-zero"; return 1; }
    codes=$(status_codes "$log")
    [ "$codes" = "$(repeat 0 14)" ] || { echo "StatusCodes: $codes (expected 14 × 0)"; return 1; }
    grep -q '"success_num":100543' "$log" || { echo "lubm build did not report 100543 triples"; return 1; }
    inserts=$(grep -c '"AnsNum":1,.*update query returns true' "$log")
    [ "$inserts" = 10 ] || { echo "$inserts of 10 inserts answered AnsNum 1"; return 1; }
}

# transaction_test.sh: an empty database, six transactions, 31 tquery
# statements. Every printed answer is StatusCode 0 (41: the script keeps the
# six begin answers to itself, printing their TIDs, which must be 1..6); each
# update changes the expected number of triples (U<n>: AnsNum) and each read
# answers the expected number of rows (R<n>: counted from the bindings, since
# upstream's tquery leaves AnsNum at 0 for a read).
check_transaction() {
    local log=$LOGS/transaction.log codes tids steps
    run_script transaction transaction_test.sh || { echo "transaction_test.sh exited non-zero"; return 1; }
    codes=$(status_codes "$log")
    [ "$codes" = "$(repeat 0 41)" ] || { echo "StatusCodes: $codes (expected 41 × 0)"; return 1; }
    tids=$(strip_colors "$log" | grep -o '获取事务ID: [0-9]*' | cut -d' ' -f2 | tr '\n' ' ')
    [ "$tids" = "1 2 3 4 5 6 " ] || { echo "TIDs: $tids (expected 1 2 3 4 5 6)"; return 1; }
    steps=$(awk '
        /Transaction query success/ { a = $0; sub(/.*"AnsNum":/, "", a); sub(/[^0-9].*/, "", a); printf "U%s ", a }
        /"StatusMsg":"success"/ && /"bindings"/ {
            v = $0; sub(/.*"vars":\[/, "", v); sub(/\].*/, "", v); nv = gsub(/"[^"]*"/, "", v)
            t = $0; nt = gsub(/"type":/, "", t)
            printf "R%d ", (nv ? nt / nv : 0)
        }' "$log")
    local want="U0 R0 U1 U1 R2 U1 R1 R1 U0 R1 U1 R0 U1 R0 U4 R4 U4 R0 U1 R1 U1 R2 U1 R3 R2 U0 U1 R3 U1 U1 R3 "
    [ "$steps" = "$want" ] || { echo "statements: $steps"; echo "expected:   $want"; return 1; }
}

# api_test.sh (dataset lubm): 28 operations. Every answer is StatusCode 0
# except the first drop (of a database that does not exist yet: 1005); the
# export and backup paths are reported, the export downloads (wget, 200) and
# the transactions are TID 1 and 2.
check_api() {
    local log=$LOGS/api.log codes want
    rm -f "$GHOME"/lubm_*.nt
    run_script api api_test.sh || { echo "api_test.sh exited non-zero"; return 1; }
    codes=$(status_codes "$log")
    want="0 0 1005 $(repeat 0 26)"
    [ "$codes" = "$want" ] || { echo "StatusCodes: $codes"; echo "expected:    $want"; return 1; }
    grep -q '^filepath：..*' "$log" || { echo "export reported no filepath"; return 1; }
    grep -q '^backupfilepath：..*' "$log" || { echo "backup reported no backupfilepath"; return 1; }
    grep -q 'awaiting response... 200' "$log" || { echo "the export download did not answer 200"; return 1; }
    ls "$GHOME"/lubm_*.nt >/dev/null 2>&1 || { echo "the export was not downloaded"; return 1; }
    [ "$(grep -c '"TID":"1"' "$log")" = 1 ] && [ "$(grep -c '"TID":"2"' "$log")" = 1 ] \
        || { echo "the transactions were not TID 1 and 2"; return 1; }
}

# parser_test.sh: 50 cases (build count + AnsNum). The cases that may fail
# are known: 15 and 17 (the script records AnsNum 0 where its own result files
# and SPARQL give 4 and 1), 33-35 (phase 2: graph functions), 42 and 50
# (phase 3: UCASE keeps the language tag; a variable joined across predicate
# and subject/object positions). Any other failing case fails the suite.
check_parser() {
    local log=$LOGS/parser.log failing unexpected=""
    run_script parser parser_test.sh || { echo "parser_test.sh exited non-zero"; return 1; }
    failing=$(strip_colors "$log" | grep -o '❌ 用例 #[0-9]* 校验失败' | grep -o '[0-9]*' | tr '\n' ' ')
    for n in $failing; do
        case " 15 17 33 34 35 42 50 " in
            *" $n "*) ;;
            *) unexpected+="$n " ;;
        esac
    done
    echo "failing cases: ${failing:-none} (known: 15 17 33 34 35 42 50)"
    [ -z "$unexpected" ] || { echo "unexpected failures: $unexpected"; return 1; }
    [ "$(strip_colors "$log" | grep -c '用例 #[0-9]* 校验')" = 50 ] || { echo "not all 50 cases ran"; return 1; }
}

# The Python SDK: upstream's GET and POST examples (every printed answer is
# StatusCode 0; fquery, which is not an upstream operation, saves 1100), then
# tests/upstream_scripts/python/sdk_check.py, which checks the answers of
# every GstoreConnector method it calls.
check_python() {
    local ex="$GHOME/api/http/python/example" log=$LOGS/python.log n codes
    : >"$log"
    for e in GET-example.py POST-example.py; do
        echo "---- $e" >>"$log"
        rm -f "$ex/res.txt"
        (cd "$ex" && GSTORE_PORT=$PORT python3 "$e") >"$LOGS/python-$e.log" 2>&1 \
            || { cat "$LOGS/python-$e.log" >>"$log"; echo "$e failed"; return 1; }
        cat "$LOGS/python-$e.log" >>"$log"
        n=$(grep -c '^{' "$LOGS/python-$e.log")
        codes=$(status_codes "$LOGS/python-$e.log")
        [ "$n" = 10 ] && [ "$codes" = "$(repeat 0 10)" ] \
            || { echo "$e: $n answers, StatusCodes: $codes (expected 10 × 0)"; return 1; }
        grep -q '"StatusCode":1100' "$ex/res.txt" || { echo "$e: fquery saved: $(head -c 200 "$ex/res.txt")"; return 1; }
    done
    echo "---- sdk_check.py" >>"$log"
    (cd "$GHOME" && python3 api/http/python/sdk_check.py "$PORT") >>"$log" 2>&1 \
        || { grep '^FAIL\|sdk_check' "$log"; return 1; }
    grep 'sdk_check:' "$log"
}

for suite in "${SUITES[@]}"; do
    if [ "$suite" = python ] && ! { command -v python3 >/dev/null 2>&1 && python3 -c 'import requests' 2>/dev/null; }; then
        say "suite python: SKIPPED (python3 with the requests module is not available)"
        RESULT[$suite]=SKIP
        continue
    fi
    say "suite $suite"
    start=$(date +%s)
    out=$("check_$suite" 2>&1)
    rc=$?
    secs=$(($(date +%s) - start))
    [ -n "$out" ] && echo "$out"
    if [ $rc -eq 0 ]; then
        RESULT[$suite]=PASS
    else
        RESULT[$suite]=FAIL
        FAILED=1
        echo "---- last lines of $LOGS/$suite.log:"
        tail -n 15 "$LOGS/$suite.log" 2>/dev/null | strip_colors /dev/stdin | cut -c1-300
    fi
    DETAIL[$suite]="${secs}s"
done

# ---- 5. summary ----------------------------------------------------------------
say "summary (gserver on port $PORT)"
for suite in "${SUITES[@]}"; do
    printf '  %-12s %-5s %s\n' "$suite" "${RESULT[$suite]}" "${DETAIL[$suite]:-}"
done
if [ "$FAILED" = 1 ]; then
    echo "conformance: FAILED"
    KEEP=1
    exit 1
fi
echo "conformance: all suites passed"
exit 0
