#!/usr/bin/env bash
# Package smoke test (review infra#7): install-and-run checks of the .deb /
# .rpm packages and of the container image, as .github/workflows/packaging.yml
# runs them.
#
#   scripts/package-smoke.sh stage [--bin-dir DIR | --deb FILE] [--bins "NAMES"] [--keep]
#   scripts/package-smoke.sh installed [--service] [--bins "NAMES"]
#   scripts/package-smoke.sh image IMAGE [--bins "NAMES"]
#
# stage      Lay out, in a temporary directory, the root the .deb installs:
#            the [package.metadata.deb] assets of Cargo.toml, the binaries
#            ("$auto") from DIR (default: target/release), tmpfiles.d's
#            /var/lib/gstore and its conf link; run the first-start password
#            block of packaging/debian/postinst against it; then smoke it as
#            the current user (no root, no Docker, nothing outside the
#            temporary directory, which --keep keeps). With --deb, the root
#            is the content of the built .deb FILE (`dpkg-deb -x`) and the
#            block is taken from its own postinst.
# installed  Smoke the package installed on this machine, as root (after
#            `apt-get install ./gstore_*.deb` or `dnf install ./gstore-*.rpm`):
#            also the gstore user, conf.ini's group and the first-start
#            password file's owner. The server is started as the systemd
#            unit starts it (its User=, WorkingDirectory=, Environment= and
#            ExecStart= arguments); with --service by systemd itself
#            (`systemctl start gserver`), so the unit's hardening is
#            exercised too.
# image      Smoke the container image IMAGE with Docker: a container with a
#            root password file (an explicit password: gserver refuses the
#            default one), on a port other than 9000, must turn healthy
#            without a single access-log record, answer the requests below
#            and stop gracefully (exit 0); a container without a password
#            must refuse to start (exit 1).
#
# The smoke (stage / installed): every binary target of the gstore package
# (--bins, else `cargo metadata`) is installed and answers --help; the unit
# passes `systemd-analyze verify` (when available); the first start seeds
# root's password from the first-start file (/etc/gstore/root_password) and
# the default 123456 is refused; /api check, login, build, load, an INSERT
# DATA and a SELECT, REST /status; a graceful stop; a second start without
# the password file, on the user store the first one created. Before the
# requests, the container image's health check must find the server healthy
# without leaving an access-log record.
# Exit status 0 when every check passed; the first failure is reported.
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
MODE=${1:-}
[ $# -gt 0 ] && shift
BIN_DIR="$REPO/target/release"
DEB=""
BINS=""
KEEP=0
SERVICE=0
IMAGE=""
case "$MODE" in
    stage | installed | image) ;;
    -h | --help | "")
        awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"
        [ -n "$MODE" ]
        exit
        ;;
    *) echo "package-smoke: unknown mode '$MODE' (try --help)" >&2; exit 2 ;;
esac
if [ "$MODE" = image ]; then
    IMAGE=${1:-}
    [ -n "$IMAGE" ] || { echo "package-smoke: image needs an IMAGE" >&2; exit 2; }
    shift
fi
while [ $# -gt 0 ]; do
    case "$1" in
        --bin-dir) BIN_DIR=$2; shift 2 ;;
        --deb) DEB=$(cd "$(dirname "$2")" && pwd)/$(basename "$2"); shift 2 ;;
        --bins) BINS=$2; shift 2 ;;
        --keep) KEEP=1; shift ;;
        --service) SERVICE=1; shift ;;
        *) echo "package-smoke: unknown argument '$1' (try --help)" >&2; exit 2 ;;
    esac
done

say() { printf '==> %s\n' "$*"; }
fail() { echo "package-smoke: FAILED: $*" >&2; exit 1; }

# The binary targets of the gstore package, from Cargo's own view of the
# manifest (the package's [[bin]] targets and src/bin files alike).
if [ -z "$BINS" ]; then
    meta=$(cd "$REPO" && cargo metadata --no-deps --format-version 1) \
        || fail "cargo metadata failed (pass --bins)"
    if command -v jq >/dev/null 2>&1; then
        BINS=$(printf '%s' "$meta" | jq -r '.packages[] | select(.name == "gstore")
            | .targets[] | select(.kind | index("bin")) | .name')
    else
        BINS=$(printf '%s' "$meta" | python3 -c 'import json, sys
for p in json.load(sys.stdin)["packages"]:
    if p["name"] == "gstore":
        print("\n".join(t["name"] for t in p["targets"] if "bin" in t["kind"]))')
    fi
fi
BINS=$(printf '%s\n' $BINS | sort | tr '\n' ' ')
[ -n "${BINS// /}" ] || fail "no binary targets"

command -v curl >/dev/null 2>&1 || fail "curl is needed"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/gstore-package-smoke.XXXXXX")
CONTAINERS=()
STOP_CMD=""
cleanup() {
    local c
    for c in "${CONTAINERS[@]}"; do docker rm -f -v "$c" >/dev/null 2>&1 || :; done
    if [ -n "$STOP_CMD" ]; then eval "$STOP_CMD" >/dev/null 2>&1 || :; fi
    if [ "$KEEP" = 1 ]; then
        echo "package-smoke: kept $WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# ---- requests ------------------------------------------------------------------
API=""     # http://host:port of the server under test
PW=""      # root's password
api() { # <json body>: POST /api, the answer on stdout
    curl -sS --max-time 60 -X POST "$API/api" -H 'Content-Type: application/json' -d "$1"
}
expect() { # <what> <answer> <pattern>
    case "$2" in
        *"$3"*) ;;
        *) fail "$1: expected '$3' in: $(printf '%s' "$2" | head -c 400)" ;;
    esac
}
requests() { # the request round of every smoke, against $API with root/$PW
    local cred="\"username\":\"root\",\"password\":\"$PW\"" out
    out=$(curl -sS --max-time 60 "$API/api?operation=check") || fail "check: no answer"
    expect check "$out" '"StatusCode":0'
    out=$(api '{"operation":"login","username":"root","password":"123456"}')
    case "$out" in *'"StatusCode":0'*) fail "root logs in with the default password 123456" ;; esac
    expect login "$(api "{\"operation\":\"login\",$cred}")" '"StatusCode":0'
    expect build "$(api "{\"operation\":\"build\",\"db_name\":\"smoke\",$cred}")" '"StatusCode":0'
    expect load "$(api "{\"operation\":\"load\",\"db_name\":\"smoke\",$cred}")" '"StatusCode":0'
    expect insert "$(api "{\"operation\":\"query\",\"db_name\":\"smoke\",$cred,\"sparql\":\"INSERT DATA { <http://example.org/smoke> <http://example.org/says> \\\"package-smoke\\\" . }\"}")" '"StatusCode":0'
    out=$(api "{\"operation\":\"query\",\"db_name\":\"smoke\",\"format\":\"json\",$cred,\"sparql\":\"SELECT ?o WHERE { <http://example.org/smoke> <http://example.org/says> ?o }\"}")
    expect select "$out" '"value":"package-smoke"'
    out=$(curl -sS --max-time 60 -u "root:$PW" "$API/status") || fail "/status: no answer"
    expect /status "$out" '"StatusMsg":"running"'
    say "requests: check, login (default password refused), build, load, insert, select, /status"
}
relogin() { # the second start: root's password is still the first start's
    expect "login after restart" "$(api "{\"operation\":\"login\",\"username\":\"root\",\"password\":\"$PW\"}")" '"StatusCode":0'
    say "second start without the password file: root's password kept"
}

# ---- image ---------------------------------------------------------------------
if [ "$MODE" = image ]; then
    command -v docker >/dev/null 2>&1 || fail "docker is needed"
    name="gstore-smoke-$$"
    say "binaries in $IMAGE"
    have=$(docker run --rm --entrypoint /bin/sh "$IMAGE" -c 'ls /usr/local/bin' | sort | tr '\n' ' ')
    for b in $BINS gstore-healthcheck; do
        case " $have " in *" $b "*) ;; *) fail "the image has no /usr/local/bin/$b (it has: $have)" ;; esac
    done
    docker run --rm "$IMAGE" -h >/dev/null || fail "gserver -h failed in the image"
    # An explicit root password (the image's user, uid 10001, reads it).
    PW="smoke-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
    printf '%s\n' "$PW" >"$WORK/root_password"
    chmod 0644 "$WORK/root_password"
    CONTAINERS+=("$name")
    # Port 9100, not the image's 9000: the health check must find the port
    # the server listens on (review infra#17).
    docker run -d --name "$name" -p 127.0.0.1::9100 \
        -v "$WORK/root_password:/run/secrets/gstore_root:ro" \
        -e GSTORE_ROOT_PASSWORD_FILE=/run/secrets/gstore_root \
        --health-interval 2s \
        "$IMAGE" -s -c /etc/gstore/conf.ini --addr 0.0.0.0 -p 9100 >/dev/null
    deadline=$((SECONDS + 180))
    while :; do
        health=$(docker inspect -f '{{.State.Health.Status}}' "$name")
        [ "$health" = healthy ] && break
        [ "$(docker inspect -f '{{.State.Running}}' "$name")" = true ] \
            || { docker logs "$name" >&2; fail "the container stopped before it was healthy"; }
        [ $SECONDS -lt $deadline ] || { docker logs "$name" >&2; fail "not healthy after 180 s ($health)"; }
        sleep 1
    done
    say "healthy on port 9100"
    # Every probe so far was the health check: none may be in the access log.
    docker exec "$name" sh -c '! test -s dbhome/access.log' \
        || fail "the health check is logged as client traffic: $(docker exec "$name" cat dbhome/access.log | head -3)"
    say "the health check left no access-log record"
    API="http://$(docker port "$name" 9100/tcp | head -n 1)"
    requests
    docker stop -t 120 "$name" >/dev/null
    [ "$(docker inspect -f '{{.State.ExitCode}}' "$name")" = 0 ] \
        || { docker logs "$name" >&2; fail "gserver did not exit 0 on docker stop"; }
    # The logs are read whole before the search: `docker logs | grep -q`
    # under pipefail fails when grep stops reading early (SIGPIPE).
    logs=$(docker logs "$name" 2>&1) || fail "docker logs $name"
    case "$logs" in *'shut down'*) ;; *) fail "no graceful shutdown in the log" ;; esac
    say "docker stop: graceful, exit 0"
    # Without a password the image's default command (listening on 0.0.0.0)
    # must refuse root's default password.
    bare="$name-nopw"
    CONTAINERS+=("$bare")
    docker run -d --name "$bare" "$IMAGE" >/dev/null
    rc=$(timeout 120 docker wait "$bare") || fail "the container without a password did not exit"
    [ "$rc" = 1 ] || fail "the container without a password exited $rc, not 1"
    logs=$(docker logs "$bare" 2>&1) || fail "docker logs $bare"
    case "$logs" in
        *'refusing to start'*) ;;
        *) printf '%s\n' "$logs" >&2; fail "no default-password refusal in the log" ;;
    esac
    say "without a root password: refused (exit 1)"
    echo "package smoke: passed (image $IMAGE)"
    exit 0
fi

# ---- stage / installed: the layout -------------------------------------------
if [ "$MODE" = stage ]; then
    ROOT="$WORK/root"
    mkdir -p "$ROOT"
    POSTINST="$REPO/packaging/debian/postinst"
    if [ -n "$DEB" ]; then
        say "staging the content of $DEB in $ROOT"
        dpkg-deb -x "$DEB" "$ROOT" || fail "dpkg-deb -x $DEB"
        dpkg-deb -e "$DEB" "$WORK/DEBIAN" || fail "dpkg-deb -e $DEB"
        POSTINST="$WORK/DEBIAN/postinst"
        # Exactly the binary targets in /usr/bin.
        have=$(cd "$ROOT/usr/bin" && printf '%s\n' * | sort | tr '\n' ' ')
        [ "$have" = "$BINS" ] || fail "the package's /usr/bin holds: $have; the binary targets are: $BINS"
    else
        say "staging the .deb layout in $ROOT (binaries from $BIN_DIR)"
        for b in $BINS; do
            [ -x "$BIN_DIR/$b" ] || fail "no $BIN_DIR/$b"
            mkdir -p "$ROOT/usr/bin"
            ln -s "$BIN_DIR/$b" "$ROOT/usr/bin/$b"
        done
        # The other assets: ["source", "dest", "mode"] lines of
        # [package.metadata.deb] (a dest ending in / is a directory).
        awk '/^\[package\.metadata\.deb\]/ { on = 1; next } /^\[/ { on = 0 } on' "$REPO/Cargo.toml" \
            | sed -n 's/^[[:space:]]*\["\([^"]*\)", "\([^"]*\)", "\([0-7]*\)"\],\{0,1\}[[:space:]]*$/\1 \2 \3/p' \
            | while read -r src dest mode; do
                case "$dest" in */) dest="$dest$(basename "$src")" ;; esac
                mkdir -p "$(dirname "$ROOT/$dest")"
                install -m "$mode" "$REPO/$src" "$ROOT/$dest"
            done
    fi
    # tmpfiles.d: the working directory and its ./conf link.
    while read -r type path mode _ _ _ arg; do
        case "$type" in
            d) mkdir -p "$ROOT$path"; chmod "$mode" "$ROOT$path" ;;
            L) ln -s "$ROOT$arg" "$ROOT$path" ;;
            '' | '#'*) ;;
            *) fail "tmpfiles type $type is not staged" ;;
        esac
    done <"$ROOT/usr/lib/tmpfiles.d/gstore.conf"
    UNIT="$ROOT/lib/systemd/system/gserver.service"
    # The first-start block of the deb's postinst (as a fresh install).
    block=$(sed -n '/# >>> first-start root password/,/# <<< first-start root password/p' "$POSTINST")
    [ -n "$block" ] || fail "no first-start block in $POSTINST"
    GSTORE_PKG_CONF="$ROOT/etc/gstore/conf.ini" GSTORE_PKG_DATA="$ROOT/var/lib/gstore" \
        GSTORE_PKG_PWFILE="$ROOT/etc/gstore/root_password" GSTORE_PKG_GROUP="$(id -gn)" \
        sh -c "$block" postinst configure "" >"$WORK/postinst.log" 2>&1 \
        || { cat "$WORK/postinst.log" >&2; fail "the first-start block failed"; }
else
    ROOT=""
    [ "$(id -u)" = 0 ] || fail "installed: run as root"
    UNIT=""
    for u in /lib/systemd/system/gserver.service /usr/lib/systemd/system/gserver.service; do
        [ -f "$u" ] && UNIT=$u && break
    done
    [ -n "$UNIT" ] || fail "no gserver.service installed"
fi

say "binaries: $BINS"
for b in $BINS; do
    [ -x "$ROOT/usr/bin/$b" ] || fail "/usr/bin/$b is not installed"
    "$ROOT/usr/bin/$b" --help >/dev/null 2>&1 || fail "$b --help failed"
done

# The unit's settings (the first occurrence of each key).
unit_key() { sed -n "s/^$1=//p" "$UNIT" | head -n 1; }
USER_=$(unit_key User)
WORKDIR=$ROOT$(unit_key WorkingDirectory)
EXEC=$(unit_key ExecStart)
PWFILE=$(sed -n 's/^Environment=GSTORE_ROOT_PASSWORD_FILE=//p' "$UNIT" | head -n 1)
[ -n "$EXEC" ] && [ -n "$PWFILE" ] && [ -n "$USER_" ] || fail "the unit lacks ExecStart, User or the password file"
PWFILE=$ROOT$PWFILE
# ExecStart's arguments, absolute paths moved into the staged root.
read -r -a EXEC_ARGS <<<"$EXEC"
GSERVER=$ROOT${EXEC_ARGS[0]}
ARGS=()
for a in "${EXEC_ARGS[@]:1}"; do
    case "$a" in
        -s | --start) ;; # started with -b below: it returns once the server answers
        /*) ARGS+=("$ROOT$a") ;;
        *) ARGS+=("$a") ;;
    esac
done
CONF=""
for ((i = 0; i < ${#ARGS[@]}; i++)); do
    [ "${ARGS[$i]}" = -c ] && CONF=${ARGS[$((i + 1))]}
done
[ -n "$CONF" ] || fail "the unit's ExecStart names no -c conf.ini"

if command -v systemd-analyze >/dev/null 2>&1; then
    # ExecStart's binary must exist where verify looks: in the staged root.
    sed "s#^ExecStart=/#ExecStart=$ROOT/#" "$UNIT" >"$WORK/gserver.service"
    out=$(systemd-analyze verify "$WORK/gserver.service" 2>&1) || fail "systemd-analyze verify: $out"
    [ -z "$out" ] || fail "systemd-analyze verify: $out"
    say "systemd-analyze verify: clean"
else
    say "systemd-analyze verify: skipped (not installed)"
fi

# The configuration and the first-start password file.
mode_of() { stat -c '%a' "$1"; }
group_of() { stat -c '%G' "$1"; }
[ "$(mode_of "$CONF")" = 640 ] || fail "$CONF has mode $(mode_of "$CONF"), not 640"
[ -f "$PWFILE" ] || fail "no first-start password file $PWFILE"
[ "$(mode_of "$PWFILE")" = 640 ] || fail "$PWFILE has mode $(mode_of "$PWFILE"), not 640"
PW=$(head -n 1 "$PWFILE")
printf '%s' "$PW" | grep -Eq '^[0-9a-f]{32}$' || fail "$PWFILE does not hold a random password"
grep -q '^root_password="123456"' "$CONF" || fail "conf.ini was changed (it is a conffile)"
if [ "$MODE" = installed ]; then
    getent passwd "$USER_" >/dev/null || fail "no user $USER_"
    for f in "$CONF" "$PWFILE" "$(dirname "$CONF")"; do
        [ "$(group_of "$f")" = "$USER_" ] || fail "$f has group $(group_of "$f"), not $USER_"
    done
    [ "$(mode_of "$(dirname "$CONF")")" = 750 ] || fail "$(dirname "$CONF") is not 0750"
    [ "$(stat -c '%U %a' "$WORKDIR")" = "$USER_ 750" ] || fail "$WORKDIR: $(stat -c '%U %a' "$WORKDIR")"
fi
say "configuration: conf.ini 0640 unchanged, first-start password file 0640"

# ---- start, requests, stop -------------------------------------------------------
as_user() { # run "$@" in the unit's working directory as the unit's user
    if [ "$MODE" = stage ]; then
        (cd "$WORKDIR" && "$@")
    elif command -v runuser >/dev/null 2>&1; then
        (cd "$WORKDIR" && runuser -u "$USER_" -- "$@")
    else
        (cd "$WORKDIR" && setpriv --reuid "$USER_" --regid "$USER_" --init-groups -- "$@")
    fi
}
PIDFILE="$WORKDIR/dbhome/gserver.pid"
start_server() {
    if [ "$SERVICE" = 1 ]; then
        systemctl start gserver.service || { journalctl -u gserver --no-pager | tail -30 >&2; fail "systemctl start"; }
        STOP_CMD="systemctl stop gserver.service"
        local deadline=$((SECONDS + 120)) st
        while :; do
            st=$(as_user "$GSERVER" -S "${ARGS[@]}" 2>/dev/null || :)
            case "$st" in *"Address: http://"*) case "$st" in *"(not answering)"*) ;; *) break ;; esac ;; esac
            systemctl is-active --quiet gserver.service \
                || { journalctl -u gserver --no-pager | tail -30 >&2; fail "gserver.service stopped"; }
            [ $SECONDS -lt $deadline ] || fail "gserver.service did not answer within 120 s"
            sleep 1
        done
    else
        # As the unit: its user, directory, environment and arguments (-b
        # instead of -s, and any free port: -p 0).
        as_user env GSTORE_ROOT_PASSWORD_FILE="$PWFILE" "$GSERVER" -b "${ARGS[@]}" -p 0 \
            >"$WORK/start.log" 2>&1 || { cat "$WORK/start.log" >&2; fail "gserver did not start"; }
        STOP_CMD="as_user \"\$GSERVER\" -k \"\${ARGS[@]}\""
    fi
    local addr
    addr=$(sed -n 4p "$PIDFILE")
    [ -n "$addr" ] || fail "no address in $PIDFILE"
    API="http://${addr/0.0.0.0/127.0.0.1}"
}
stop_server() {
    if [ "$SERVICE" = 1 ]; then
        systemctl stop gserver.service
        [ "$(systemctl show -p ExecMainStatus --value gserver.service)" = 0 ] \
            || fail "gserver.service exited $(systemctl show -p ExecMainStatus --value gserver.service)"
    else
        as_user "$GSERVER" -t "${ARGS[@]}" >"$WORK/stop.log" 2>&1 \
            || { cat "$WORK/stop.log" >&2; fail "gserver -t failed"; }
    fi
    STOP_CMD=""
    [ ! -e "$PIDFILE" ] || fail "the pid file is still there after the stop"
}

say "first start (as the unit: user $USER_, in $WORKDIR)"
start_server
say "gserver listens on $API"
# The container image's health check (packaging/docker/gstore-healthcheck)
# against this server, on its non-default port: healthy, and no request
# recorded (the access log is only created by a request).
GSTORE_HEALTHCHECK_PID=$(sed -n 1p "$PIDFILE") GSTORE_GSERVER=$GSERVER \
    "$REPO/packaging/docker/gstore-healthcheck" >"$WORK/health.log" 2>&1 \
    || { cat "$WORK/health.log" >&2; fail "the health check reports the running server unhealthy"; }
[ ! -s "$WORKDIR/dbhome/access.log" ] || fail "the health check is logged as client traffic"
say "health check: healthy, no access-log record"
requests
stop_server
say "graceful stop"
# The password has been set; the file goes, as docs/DEPLOY.md advises. The
# unit still names it: gserver skips the missing file.
rm -f "$PWFILE"
start_server
relogin
stop_server
echo "package smoke: passed ($MODE)"
