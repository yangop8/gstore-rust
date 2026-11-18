# Deploying gstore-rust

This guide covers running `gserver` — the HTTP API server that speaks
upstream gStore 1.4's `/api` protocol (upstream's Python SDK works against
it unmodified, as `scripts/conformance.sh` checks; the other upstream SDKs
speak the same protocol but are not tested; `gconsole` is gstore-rust's
own) — as a container, as a systemd service, and from `.deb` / `.rpm`
packages; then its configuration, logs and the command-line tools.

| What | Where (package / systemd) | Where (container) |
|------|---------------------------|-------------------|
| binaries | `/usr/bin/g*` | `/usr/local/bin/g*` |
| configuration | `/etc/gstore/conf.ini` | `/etc/gstore/conf.ini` (volume) |
| working directory: databases (`dbhome/`), `backups/`, `logs/`, `upload/` | `/var/lib/gstore` | `/var/lib/gstore` (volume) |
| PFN functions (`pfn_base_path`; default `<db_home>/pfn`) | `/var/lib/gstore/dbhome/pfn` | `/var/lib/gstore/dbhome/pfn` (in the volume) |
| `./conf` (for `conf.ini`'s `./conf/...` paths) | `/var/lib/gstore/conf → /etc/gstore` | same |
| port | 9000 (`conf.ini` `[server] port`) | 9000 |
| runs as | user `gstore` | user `gstore` (uid 10001) |

Every relative path in `conf.ini` (`db_home="./dbhome/"`,
`backup_path="./backups/"`, `upload_path="./upload/"`,
`log_mode="./conf/slog.properties"`, …) is relative to the working
directory, as upstream's are to its install root — except `querylog_path`
and `accesslog_path`, which are relative to `db_home`: with the shipped
`conf.ini` the query and access logs are in
`/var/lib/gstore/dbhome/logs/endpoint/` and `…/dbhome/logs/ipaccess/`.

**Before exposing a server**: the `root` user's password is `conf.ini`'s
`root_password` (default `123456`) when the user store is first created —
set it before the first start (or pass `--password-file FILE`, or
`--password`), or change it later with the `/api` `userPassword` operation or
`gconsole`'s `setpswd`. While root still has the default password, `gserver`
(API mode) refuses to start — on every address, loopback included; the log
names both remedies. The compat switch `[server]
allow_default_root_password=on` lets it start on a loopback address only,
and accepts the default password from local clients only (see [Secure
defaults and upstream-compat
switches](#secure-defaults-and-upstream-compat-switches)). *Upgrade note:*
an API server whose stored root password is still `123456` refuses to start
after the upgrade (earlier versions only warned on loopback): change the
password first, or start it once on `127.0.0.1` with
`allow_default_root_password=on`, change the password (`gconsole` `setpswd`,
`/api` `userPassword`) and remove the switch.

The legacy **single-database mode** (`gserver DIR`, without `--api`) has no
authentication and no IP lists at all: anyone who reaches it may update the
database. It refuses to listen beyond loopback unless started with
`--allow-unauthenticated`, and refuses (`403`) a request that a loopback
reverse proxy forwards (one carrying `Forwarded`, `X-Forwarded-For` or
`X-Real-IP`) unless `trust_proxy_headers=on`; serve a database to the
network through the multi-database API instead.

**`--allow-service`** lets SPARQL `SERVICE` contact remote endpoints, and
lets users with the update privilege make the server download
`http(s)://` sources (`/api` `build` / `batchInsert` / `batchRemove` with
`remote=true`) and keep what they hold in a database — never from (or to) a
host-local address, but from any other host the server can reach, internal
ones included (the private networks `10/8`, `172.16/12` and `192.168/16`
stay reachable by design). Host-local means an endpoint that resolves only
to loopback, `0.0.0.0/8`, link-local, multicast or broadcast addresses,
`100.64.0.0/10` (cloud metadata such as `100.100.100.200`),
`198.18.0.0/15`, `240.0.0.0/4`, unique-local `fc00::/7` (IPv6 metadata
such as `fd00:ec2::254`), site-local `fec0::/10`, IPv4-compatible `::/96`,
or the NAT64 (`64:ff9b::/96`) and 6to4 (`2002::/16`) forms of a host-local
IPv4 address: it is refused before anything connects, and a `SERVICE` to it
fails the query with an error naming the refusal, `SILENT` or not. A
`SERVICE` IRI holding whitespace or a control character is refused the same
way. `--allow-local-hosts` (only together with `--allow-service`; alone it
only warns that it has no effect) lifts the host-local rule for `SERVICE`
and `remote=true` downloads alike and prints a startup warning — unsafe on
a shared server: clients can then reach services listening on the server's
loopback and cloud metadata endpoints. Asynchronous-operation callbacks
(any user holding the load, update, backup or restore privilege on a
database may name a callback URL) are not gated by `--allow-service`: they
refuse host-local addresses (the same set) unless the library setting
`TaskConfig::callback_allow_local` is on, but reach other internal hosts —
restrict the server's outbound connections with a firewall if that
matters (review sec#13).

## Docker

The `Dockerfile` at the repository root builds a two-stage image: the
crate's MSRV toolchain (`rust:1.89-slim-bookworm`) compiles and installs
every binary of the gstore package (`cargo install --path .`); the runtime
is `debian:bookworm-slim` with the binaries, the health check
(`/usr/local/bin/gstore-healthcheck`) and `ca-certificates`, running as the
unprivileged user `gstore`.

```bash
docker build -t gstore-rust .
# FEATURES defaults to "pfn-wasm" (PFN functions compile and run); add
# others — gzip / tls / zip / readline are pure Rust; rocksdb makes the
# builder install clang for bindgen — or pass FEATURES="" for the lean set
docker build --build-arg FEATURES="pfn-wasm gzip tls zip" -t gstore-rust .

# root's password, read once when the user store is created (the server
# refuses to start while root has the default 123456); the file must be
# readable by the container's uid 10001
printf '%s\n' 'a-long-random-password' > gstore_root
docker run -d --name gstore -p 9000:9000 \
    -v gstore-data:/var/lib/gstore \
    -v gstore-conf:/etc/gstore \
    -v "$PWD/gstore_root:/run/secrets/gstore_root:ro" \
    -e GSTORE_ROOT_PASSWORD_FILE=/run/secrets/gstore_root \
    --stop-timeout 120 \
    gstore-rust

curl 'http://127.0.0.1:9000/api?operation=check'          # {"StatusCode":0,...}
docker logs gstore                                         # the diagnostic log
docker exec -it gstore gconsole -u root                    # the console
```

* **Features.** The image is built with `--locked` and the `pfn-wasm`
  feature (the `FEATURES` build argument, default `"pfn-wasm"`): users'
  PFN functions compile and run in the server's WebAssembly sandbox. A
  plain `cargo build` leaves it out, as it does every optional feature.
* **Entry point.** `ENTRYPOINT ["/usr/local/bin/gserver"]` with
  `CMD ["-s", "-c", "/etc/gstore/conf.ini", "--addr", "0.0.0.0"]`: the server
  in the foreground, reading `/etc/gstore/conf.ini`, listening on every
  interface of the container (`conf.ini`'s `listen_addr` defaults to
  `127.0.0.1`). Arguments given to `docker run … gstore-rust ARGS` replace
  the `CMD`. The server will not start while `root` has the default
  password (and, listening beyond loopback, not even with
  `allow_default_root_password=on`): give the first start one through
  `GSTORE_ROOT_PASSWORD_FILE` (a Docker secret or a mounted file, as above),
  `--password-file FILE`, or `root_password` in the `/etc/gstore` volume's
  `conf.ini` — not `--password`, which `docker inspect` and `ps` show.
* **Volumes.** `/var/lib/gstore` holds the databases (with their
  reasoning rules), backups, uploads, the users' PFN functions
  (`dbhome/pfn/`) and the query/access/transaction logs; `/etc/gstore`
  holds `conf.ini` (and,
  if you add them, `slog.properties`, `ipDeny.config`, `ipAllow.config`).
  Named volumes are initialised from the image (including the
  `/var/lib/gstore/conf` link). With a **bind mount** for `/var/lib/gstore`,
  create the link yourself (`ln -s /etc/gstore <dir>/conf`) or `conf.ini`'s
  `./conf/...` paths will not resolve (the log then goes to standard error
  only, the IP lists are empty); the directory must be writable by uid 10001.
* **Stopping.** `docker stop` sends `SIGTERM`: in-flight requests finish
  (up to 10 s), open transactions roll back and unsaved changes are saved
  before the process exits. Give it time (`--stop-timeout 120`, or
  `stop_grace_period: 2m` in Compose) so a large database can be saved.
* **Health check.** `gstore-healthcheck` every 30 s: healthy while the
  server runs and accepts connections on the address it listens on. It runs
  `gserver -S` with the server's own arguments (the container's main
  process), which finds the server's pid file and connects to the address
  recorded there — whatever port `conf.ini` or `-p` chose, HTTP or HTTPS
  alike. The probe sends no request, so it is not an access-log record and
  the IP lists do not apply to it. (Earlier images probed
  `curl http://127.0.0.1:9000/api?operation=check`, which failed on any
  other port or with TLS and logged every probe; the image no longer ships
  `curl`.) If PID 1 is not `gserver` (`docker run --init`), it checks the
  server of `/etc/gstore/conf.ini`.
* **Logs.** With the shipped `conf.ini`, `log_mode` names
  `./conf/slog.properties`, which does not exist: the diagnostic log goes to
  standard error (`docker logs`). Copy `slog.properties.example` to
  `/etc/gstore/slog.properties` to also write `logs/all.log` (rolled daily).
  The query and access logs are files below `/var/lib/gstore/dbhome/logs/`
  (`querylog_path` / `accesslog_path` are relative to `db_home`).

A Compose service. The image listens on every interface of the container,
so `gserver` will not start until root has a password of its own: the
secret below is read once, when the user store is created (without it the
container exits with "refusing to start" and `restart: unless-stopped`
restarts it in a loop). The port is published on the host's loopback
address only; publish it more widely (`"9000:9000"`) deliberately, behind
a firewall or a TLS proxy.

```yaml
services:
  gstore:
    build: .
    ports: ["127.0.0.1:9000:9000"]
    environment:
      GSTORE_ROOT_PASSWORD_FILE: /run/secrets/gstore_root
    secrets: [gstore_root]
    volumes:
      - gstore-data:/var/lib/gstore
      - gstore-conf:/etc/gstore
    stop_grace_period: 2m
    restart: unless-stopped
secrets:
  gstore_root:
    # printf '%s\n' 'a-long-random-password' > gstore_root
    # (readable by the container's uid 10001)
    file: ./gstore_root
volumes:
  gstore-data:
  gstore-conf:
```

Once root's password has been changed (or the user store exists), the
secret is no longer read; a `GSTORE_ROOT_PASSWORD_FILE` naming a missing
file is skipped with a warning (earlier versions stopped `gserver`).

## systemd

`packaging/systemd/gserver.service` runs `/usr/bin/gserver -s -c
/etc/gstore/conf.ini` as user `gstore` in `/var/lib/gstore` (installed by
the packages below; to install by hand, copy the binaries to `/usr/bin`,
`conf/conf.ini.example` to `/etc/gstore/conf.ini`, and
`packaging/sysusers/gstore.conf` / `packaging/tmpfiles/gstore.conf` to
`/usr/lib/sysusers.d/` / `/usr/lib/tmpfiles.d/`, then run
`systemd-sysusers` and `systemd-tmpfiles --create`).

```bash
sudo systemctl enable --now gserver
systemctl status gserver
journalctl -u gserver -f          # the diagnostic log (standard error)
sudo systemctl restart gserver
```

* `Type=simple`: `gserver -s` stays in the foreground.
* Stopping sends `SIGTERM` to the main process only (`KillMode=mixed`) —
  exactly what `gserver -t` sends — which shuts the server down gracefully
  (see Docker above); `TimeoutStopSec=180` leaves time to save.
* `Restart=on-failure` restarts a server that crashed (a signal, or a
  panic's exit status 101): leave `conf.ini`'s `deamon=off` (its supervisor
  would duplicate what systemd does). `RestartPreventExitStatus=1 2`: a
  server that exits with gserver's error status (a configuration it
  refuses, such as root's still-default password, or a port it cannot
  bind) or its usage status (`2`: invalid arguments) is not
  restarted — the reason is in `journalctl -u gserver`. A server that keeps
  crashing stops being restarted after 5 starts within 5 minutes
  (`StartLimitIntervalSec=300`, `StartLimitBurst=5`; systemd's default,
  5 starts in 10 s, was never reached with `RestartSec=5s`); after fixing
  the cause, `systemctl reset-failed gserver` and start it again. Manual
  `systemctl start` / `restart` count toward the limit too: a sixth
  (re)start within 5 minutes is refused ("start request repeated too
  quickly") until `systemctl reset-failed gserver`.
* `LimitNOFILE=65535`, as upstream raises it. A RocksDB database keeps up
  to `[storage] rocksdb_max_open_files` (default 4096) files open, per
  loaded database: size `LimitNOFILE` above the number of RocksDB databases
  loaded at once × `rocksdb_max_open_files`, plus the connection limit
  (`thread_num`, `queue_size`).
* Hardening (`systemd-analyze security gserver` rates the unit about 1.2,
  "OK"): the server writes only below `/var/lib/gstore`
  (`ProtectSystem=strict`, `ReadWritePaths=/var/lib/gstore`) — the PFN
  functions too (`pfn_base_path`, by default `<db_home>/pfn`, i.e.
  `/var/lib/gstore/dbhome/pfn`; set it elsewhere and add that directory to
  `ReadWritePaths` with a drop-in). `/etc/gstore` is read-only
  to it — to manage the IP lists with the `ipmanage` operation, point
  `ip_deny_path` / `ip_allow_path` at files below `/var/lib/gstore`. What
  else the unit restricts, and what it means for an administrator:
  * `ProtectHome=yes`: `/home`, `/root` and `/run/user` are empty and
    inaccessible to the server. A `--load-dir` there, a `/api` `build` / `batchInsert`
    `db_path` or a `backup_path` below a home directory fails as "not
    exist": copy the files below `/var/lib/gstore` (for example into
    `upload/`), or relax it in a drop-in (`systemctl edit gserver`:
    `ProtectHome=read-only`).
  * `UMask=0027`: the databases, backups, exports and logs it creates are
    not readable by other users (group `gstore` may read them).
  * `CapabilityBoundingSet=` (none) and `NoNewPrivileges=yes`: a `port`
    below 1024 needs a drop-in with
    `AmbientCapabilities=CAP_NET_BIND_SERVICE` and
    `CapabilityBoundingSet=CAP_NET_BIND_SERVICE`.
  * `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`,
    `SystemCallFilter=@system-service` without `@privileged @resources`
    (`SystemCallErrorNumber=EPERM`: a filtered call fails instead of
    killing the server), `SystemCallArchitectures=native`,
    `MemoryDenyWriteExecute=yes` (the `pfn-wasm` runtime interprets
    WebAssembly and generates no machine code), `RestrictNamespaces=yes`,
    `RestrictRealtime=yes`, `LockPersonality=yes`, `RemoveIPC=yes`,
    `PrivateTmp=yes`, `PrivateDevices=yes` and `Protect{KernelTunables,
    KernelModules,KernelLogs,ControlGroups,Clock,Hostname}=yes`,
    `ProtectProc=invisible`.
* `gserver -S -c /etc/gstore/conf.ini` (run as `gstore`, in
  `/var/lib/gstore`) reports the server's status from its pid file
  `dbhome/gserver.pid`.

## Debian / RPM packages

`Cargo.toml` carries [cargo-deb](https://crates.io/crates/cargo-deb) and
[cargo-generate-rpm](https://crates.io/crates/cargo-generate-rpm) metadata
(`[package.metadata.deb]`, `[package.metadata.generate-rpm]`):

```bash
cargo install --locked cargo-deb@3.8.0 cargo-generate-rpm@0.21.1   # the versions CI uses

# The packages' feature set is the container image's: pfn-wasm. Stripped
# binaries (cargo-deb strips its own; cargo-generate-rpm packages them as
# they are).
CARGO_PROFILE_RELEASE_STRIP=symbols cargo build --release --locked -p gstore --features pfn-wasm
cargo deb -p gstore --no-build           # target/debian/gstore_<version>-1_<arch>.deb
cargo generate-rpm                       # target/generate-rpm/gstore-<version>-1.<arch>.rpm

sudo apt install ./target/debian/gstore_*.deb      # or: sudo dnf install ./target/generate-rpm/gstore-*.rpm
sudo cat /etc/gstore/root_password                 # root's first-start password (see below)
sudo systemctl enable --now gserver
```

Built with other features (`--features "pfn-wasm zip tls"`, …), the same
commands package those binaries; the packages built by CI
(`.github/workflows/packaging.yml`) and the container image have
`pfn-wasm` only, and the package descriptions say so.

The packages install:

* the binaries `gserver gconsole gbuild gquery gadd gsub gdrop gshow
  gbackup grestore gexport gmonitor gcompact gnode` in `/usr/bin` — every
  binary target of the gstore package (the deb takes them from Cargo, the
  rpm lists them; `tests/packaging_layout.rs` keeps the rpm list, this one
  and the Cargo targets in step); of these, `gnode cluster` (the
  experimental Raft replica) requires `--state-dir DIR` (its term, vote and
  log journaled there, so it can be restarted) or `--ephemeral` (in memory;
  never restart it under the same id) — see the README's *Experimental:
  cluster*;
* `conf/conf.ini.example` as `/etc/gstore/conf.ini` (a configuration file:
  your edits survive upgrades — a dpkg conffile, an rpm
  `%config(noreplace)`), readable by the `gstore` group only (`0640`; the
  rpm's header carries the group, its `%pre` creating the `gstore` user
  before the files are laid down, so `rpm -V gstore` finds nothing to
  report), and the log4cplus properties examples next to it;
* the unit as `/lib/systemd/system/gserver.service` (deb) or
  `/usr/lib/systemd/system/gserver.service` (rpm);
* `/usr/lib/sysusers.d/gstore.conf` (the `gstore` system user) and
  `/usr/lib/tmpfiles.d/gstore.conf` (`/var/lib/gstore`, mode `0750`, and its
  `conf → /etc/gstore` link), applied by the install scripts
  (`packaging/debian/postinst`; the rpm's `packaging/rpm/pre_install.sh`
  and `post_install.sh`), which fall back to `adduser` / `groupadd` and
  `useradd` without systemd;
* `DEPLOY.md` and `README.md` in `/usr/share/doc/gstore/`.

A fresh install writes a random root password to `/etc/gstore/root_password`
(`0640`, group `gstore`); the unit hands it to `gserver`, which sets it when
it creates the user store at its first start. Change it after the first
start (`gconsole` `setpswd`, or the `/api` `userPassword` operation), then
delete the file. The file is written (32 hex digits, beside it first, then
synced and renamed into place) only when no user store exists under
`/var/lib/gstore` and `/etc/gstore/conf.ini` still names the default
password; upgrades, existing stores and passwords already set are left
alone. `conf.ini` itself stays as shipped (byte-identical), so later
upgrades raise no conffile prompt. The unit names the file with
`Environment=GSTORE_ROOT_PASSWORD_FILE=/etc/gstore/root_password`; once it
is deleted (or on an upgrade) `gserver` skips the missing file with a
warning — a new user store would then be seeded from `conf.ini`'s
`root_password`, whose default `gserver` refuses. A `--password-file` given
on the command line must be readable.

The service is not enabled or started on install; an upgrade restarts a
running server (`try-restart`). Removing the package stops the service
first (a graceful stop: unsaved changes are saved). Whether the service is
enabled is the administrator's choice and is kept: the deb follows
`dh_installsystemd`'s protocol (`deb-systemd-helper`), so a service that was
enabled before `apt remove gstore` is enabled again by the next
`apt install gstore` (it starts at the next boot, or with
`systemctl start gserver`); `apt purge gstore` forgets it, and also removes
`/etc/gstore/root_password` and the `/var/lib/gstore/conf` link. Erasing
the rpm disables the service, as Fedora's `%systemd_preun` does. The
databases, their reasoning rules and the PFN functions in `/var/lib/gstore`
are never removed by the package, not even on purge.

### Package smoke

`scripts/package-smoke.sh` installs-and-runs what the packages and the
image ship; `.github/workflows/packaging.yml` runs it against the .deb
(installed on the runner, the server started by systemd with the unit's
hardening), the .rpm (installed in a Fedora container) and the image, and
`tests/package_smoke.rs` runs its `stage` mode in every test run:

```bash
scripts/package-smoke.sh stage                       # the .deb layout from target/release, as you (no root, no Docker)
scripts/package-smoke.sh stage --deb target/debian/gstore_*.deb   # the built .deb's own content
sudo scripts/package-smoke.sh installed [--service]  # the installed package (--service: through systemd)
scripts/package-smoke.sh image gstore-rust           # the container image
```

Each checks that every binary target is installed and answers `--help`,
that `systemd-analyze verify` accepts the unit, that the first start takes
root's password from the first-start file (the default `123456` is
refused), the health check, a build, an update and a query, a graceful
stop, and a second start without the password file; the image mode also
checks a container on another port turns healthy without an access-log
record and that one without a root password refuses to start.

## Secure defaults and upstream-compat switches

gstore-rust keeps upstream gStore 1.4's wire format (the `/api` protocol
answers HTTP 200 with its `StatusCode`s), but where upstream's behaviour is
unsafe on a network the default here is the secure one. Each switch below
restores upstream's behaviour for deployments that need it. A `conf.ini`
switch set to its upstream value makes `gserver` print one startup line
`upstream-compat: <key>=<value> (…)` (on standard error before it detaches,
and in the server log); `refreshconf` applies `upstream_login_lockout` at
once and prints the line when it turns the switch on.
`conf/conf.ini.example` shows each switch commented out with its secure
default in its home section (`txn_idle_timeout`, a tuning key, is an active
line with its default `600`).

`scripts/conformance.sh` turns on exactly two switches, because upstream's
scripts and SDK log in as `root` / `123456` and `api_test.sh` downloads its
export anonymously with `wget`:
`compat_switch server allow_default_root_password on` and
`compat_switch upload anonymous_download on`. After the server has started
it checks that `gserver` reported both (`check_compat_switches`; a misspelt
key fails the run).

| Switch (`conf.ini` key) | Secure default | Upstream value | Risk with the upstream value | `conformance.sh` |
|---|---|---|---|---|
| `[server] cors_allow_origin` | empty: no response carries `Access-Control-Allow-Origin`; a preflight from any origin gets `403` without CORS headers (one without `Origin`: `200` without ACAO). Listed origins (`scheme://host[:port]`, separated by `\|`, `,` or spaces) get their own origin echoed with `Vary: Origin`; `cors=off` still turns CORS processing off | `*` (`Access-Control-Allow-Origin: *` on every response) | any web page reads the server's answers and sends it writes through a visitor's browser (any non-empty value prints the startup line, naming the origins) | no |
| `[server] allowed_hosts` | empty: a request arriving over loopback must name in `Host` an IP literal, `localhost` / `*.localhost`, a dotless name or `listen_addr` (when a name); any other name is answered `403` (`/api`: `1101`); a request without `Host` and non-loopback peers are not checked | `*` (no Host check) | DNS rebinding: a web page whose name re-resolves to `127.0.0.1` reaches a loopback server | no |
| `[server] allow_get_update` | `off`: `GET /sparql` with an UPDATE answers `400`; `POST /update` with `text/plain` or `multipart/form-data` answers `415` | `on` (GET runs updates; `/update` takes any body type) | `<img src=…/sparql?query=DROP ALL>` or a cross-site `text/plain` form wipes data (CSRF) | no |
| `[server] trust_proxy_headers` | `off`: a loopback peer forwarding `Forwarded`, `X-Forwarded-For` or `X-Real-IP` is a reverse proxy, not a local client | `on` (every loopback peer is local) | a same-host reverse proxy hands loopback privileges — the unauthenticated single-database server, root's default password, `/api`'s `trust_inner_localhost`, the pre-body credential check — to the internet | no |
| `[server] allow_default_root_password` | `off`: `gserver` (API mode) refuses to start while root has `123456`, on every address | `on` — upstream accepts `123456` anywhere; here the switch allows it on a loopback address only, from local clients only | anyone who can reach the server as a local client is root, including any web page open in a browser on that machine (`/api` `GET` operations carry credentials in the URL, and the Origin guard covers state-changing methods only) | **yes** |
| `[server] upstream_login_lockout` | `off`: failed logins counted per account and client address, with an expiring back-off, on every password path (see [Login lockout](#login-lockout)) | `on`: upstream's rule — per address only; `login`, `testConnect` and REST `/login` exempt and still evaluating guesses; any successful login resets the address; locked until a restart | unlimited online password guessing through `login` / `testConnect`, one PBKDF2 hash per guess (an unauthenticated CPU amplifier); behind a proxy or NAT any client's failures lock every client out until a restart | no |
| `[system] txn_idle_timeout` | `600` (seconds): an open transaction without a statement in flight for that long is rolled back (see [Transactions](#transactions)) | `0` (never expires) | a client that vanishes after `begin` blocks its database's writes, maintenance and further begins until root rolls the transaction back or the server restarts; on RocksDB it also pins a full copy of the store | no |
| `[upload] anonymous_download` | `off`: `/file/download` needs credentials and passes the owner check; an anonymous request gets `401` with `WWW-Authenticate: Basic` and a `1001` envelope (see [Downloads and exports](#downloads-and-exports)) | `on` (a public export directory: every file of it to anyone) | anyone who reaches the port and learns a file name downloads whole-database exports and `format=file` query results (the names carry 128 random bits, so it takes a leaked name); where the CORS policy lets an origin in, a page from it reads the file through a visitor's browser | **yes** |

Two switches outside `conf.ini`, which print no `upstream-compat:` line:

| Switch | Secure default | Value restoring the old behaviour | Risk | `conformance.sh` |
|---|---|---|---|---|
| `gserver --allow-local-hosts` (command-line flag) | absent: with `--allow-service`, `SERVICE` and `/api` `remote=true` downloads refuse host-local endpoints | given (only with `--allow-service`): the earlier `--allow-service` reach of host-local endpoints; upstream gStore has no `SERVICE`, so this is not an upstream-compat switch; `gserver` prints its own startup warning | clients reach services listening on the server's loopback and cloud metadata endpoints (unsafe on a shared server) | no (no upstream script uses `SERVICE`) |
| `GNLQA_HTTP_ALLOW_UNAUTHENTICATED` (`gnlqa` environment / `gnlqa.conf` key) | `off`: `gnlqa serve` refuses to start unless `GNLQA_HTTP_TOKEN` is set (clients send `Authorization: Bearer <token>`) | `on` (no upstream gnlqa behaviour; it partly restores the earlier open server): loopback clients served without a token, others `403`, a non-loopback bind refused, a startup warning | every local user or process runs LLM-chosen read-only SPARQL against the KB with the server's gStore credentials and spends the operator's LLM credit (the rate limit, cost budget and connection caps still apply) | no |

Limits with secure defaults — tuning keys, not compat switches (no
startup line; `conformance.sh` sets none of them):

| Key | Default | Upstream | Effect of raising it (or `0`) |
|---|---|---|---|
| `[upload] upload_max_body_size` | `268435456` (256 MiB), default and shipped | `1073741824` (1 GiB) | larger request bodies held in memory per authenticated upload; raise `request_body_budget` with it |
| `[upload] preauth_max_body_size` | `67108864` (64 MiB) before authentication, on every route | the whole upload cap before authentication (set it equal to `upload_max_body_size`) | every unauthenticated client may make the server buffer that much per request (bounded in total by `request_body_budget`) |
| `[upload] request_body_budget` | `536870912` (512 MiB) of request bodies in memory per server, never below `upload_max_body_size` | none | concurrent bodies take more memory; beyond the budget requests get `503` |
| `[server] min_body_rate` | `65536` bytes/s on average after a 30 s grace (streamed downloads likewise after the write timeout) | none (`0`: the per-read timeout alone) | with `0`, one byte every 29 s holds a worker indefinitely, and `thread_num`+1 such connections disable the server; slow download readers hold download slots |
| `[server] max_conns_per_ip` | `64` open connections per address (waiting ones included) | none (`0`) | with `0` one address can occupy every worker and the queue; behind a proxy or NAT the default may refuse legitimate clients (raise it there) |
| `[server] query_timeout` | `3600` s of evaluation per statement | none (`0` = off) | a runaway read holds its database (and a writer queued behind it) longer |
| `[server] max_result_bytes` | `1073741824` (1 GiB) per answer | none (`0` = off) | larger answers serialized in memory |
| `[server] login_lockout_failures`, `login_lockout_max_seconds` | `4` (the 5th failure locks), `900` s | `errornum > 4`, locked until a restart (with `upstream_login_lockout=on`) | more guesses per account and address before a lock, shorter locks |
| `[upload] download_max_concurrent` | `8` streaming downloads at once; further ones `503` + `Retry-After: 1` | none (`0`) | each download holds a worker of `thread_num` |
| `[upload] file_retention_hours` | `168`: recorded uploads, exports and query results removed after a week | `0` (kept forever) | with `0` exports, results and uploads accumulate on disk and stay readable longer; with the default, copy the exports you want to keep |
| `[upload] upload_user_quota` | `0` (no quota) | none | — (bytes of retained uploads per user; beyond it `1005 "Upload quota exceeded: …"`) |
| `[upload] sparql_max_size` | `16777216` (16 MiB) per SPARQL query or update | none (`0`) | longer statements tokenized and parsed in memory |
| `[upload] rdf_max_token_size` | `67108864` (64 MiB) per token of a streaming RDF reader | none (`0`) | an unterminated string or a newline-free line held whole |
| `[pfn] pfn_fuel_per_query` | `10000000000` (ten calls' worth of `pfn_fuel_per_call`) per request | none (upstream runs PFN as native code) | one request's PFN calls together may run longer (with `0`, up to 1,000,000 calls of `pfn_fuel_per_call` each) |

### Browser-facing defaults and reverse proxies

Three guards protect a server that a browser can reach — directly, or
because the server listens on the loopback address of a machine where
someone browses:

* **CSRF.** `GET` never runs a SPARQL UPDATE; `POST /update` refuses the
  content types a page can send cross-site without a preflight
  (`text/plain`, `multipart/form-data`: `415`); a state-changing request
  (any method but `GET`, `HEAD`, `OPTIONS`) whose `Origin` names neither the
  server itself (`http(s)://` + the request's own `Host`) nor an origin of
  `cors_allow_origin` is answered `403` (`/api`: `1101`), on both servers.
* **DNS rebinding.** A request arriving over loopback must name a local
  host in `Host` (`allowed_hosts`).
* **CORS reads.** No `Access-Control-Allow-Origin` unless the origin is
  listed in `cors_allow_origin`.

Clients that send no `Origin` (curl, the SDKs, upstream's scripts,
`gconsole`) are unaffected; the bundled web console works when opened at
the server's own address. A front end served from another origin
(upstream's Workbench) needs its origin in `cors_allow_origin`.

Behind a reverse proxy:

* A same-host proxy is not a local client **only when it adds
  `Forwarded`, `X-Forwarded-For` or `X-Real-IP`**. Such a request loses
  loopback privileges: the single-database server refuses it (`403`),
  root's default password is refused from it (REST Basic and `/login`:
  `401`; `/api`: `1001`, plaintext or `encryption=1`; the pre-body
  credential check leaves its credentials unchecked, so its body is held to
  `preauth_max_body_size`), and `/api`'s `trust_inner_localhost` no longer
  applies. The server cannot detect a proxy that adds none — nginx without
  `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;` (its
  default `proxy_pass` sends none), HAProxy without `option forwardfor`,
  stunnel, ssh or socat tunnels: configure the proxy to add one, and never
  put the unauthenticated single-database mode or
  `allow_default_root_password=on` behind a proxy that does not.
* Prefer the API mode behind a proxy: the single-database mode refuses
  proxied requests unless `trust_proxy_headers=on`.
* A proxy that passes the client's `Host` (Caddy; nginx with
  `proxy_set_header Host $host;`) needs the public name in `allowed_hosts`.
  Behind a proxy that rewrites `Host` (nginx's default), the public `Origin`
  of every browser page — the bundled web console included — matches no
  `Host`, so its writes are refused: list that origin in
  `cors_allow_origin`, or keep the client's `Host` and list it in
  `allowed_hosts`.
* The server keys `max_conns_per_ip`, the login lockout and the IP lists
  on the TCP peer, so every client behind a proxy (or a NAT) shares the
  proxy's address: set `max_conns_per_ip` at least to the proxy's upstream
  connection count (or `0`), and expect failed logins of different clients
  to count against one address.

### Login lockout

Failed logins are counted per account and client address (an IPv6 client
by its /64, an IPv4-mapped one as its IPv4 address) on every path that
checks a password: `/api` `login`, `testConnect` and every authenticated
operation (`/shutdown` and `/restart` too), the pre-body credential check,
REST `/login` and HTTP Basic credentials, and the single-database server's
Basic authentication. After more than `login_lockout_failures` failures
(default `4`, so the fifth locks, as upstream's `errornum > 4`) that account
is locked for that address — 1 s at first, each further failure doubling
the lock up to `login_lockout_max_seconds` (default `900`); the counts are
forgotten after an hour without a failure. The lock is asked before the
PBKDF2 hash runs, so a refused attempt costs no hash; it is answered `/api`
`1101`, REST and single-database `429 Too Many Requests` with
`Retry-After`. A success clears only its own account's count at that
address; a lock refuses only that account's password checks (other users,
other operations and logged-in sessions from the address keep working).
More than 8 × `login_lockout_failures` (default 32) failures of any accounts
from one address lock that address's password checks too ("too many failed
logins from this address"), except for accounts that authenticated
successfully from it within the last hour. `refreshconf` applies the three
lockout keys at once (the counts are kept). An `/api` request that presents
a session token instead of a password (see [HTTPS, sessions and
`gconsole`](#https-sessions-and-gconsole)) checks no password, so it neither
counts toward the lockout nor clears it.

### Request and resource limits

* **Body caps.** Before authentication a request body may be
  `preauth_max_body_size` (64 MiB); only a request authenticated from its
  head on a payload route may carry up to `upload_max_body_size` (256 MiB;
  upstream's 1 GiB). Upstream routes (`/file/upload`, `/api`) count only
  `username` / `password` in the URL query string
  (`/file/upload?username=…&password=…`; an `Authorization` header does not
  count there); the REST payload routes (`/upload`, `/build`, `/update`,
  `/tquery`, `/batchInsert`, `/batchRemove`) count only HTTP Basic or a
  session token (`Authorization: Bearer`, `X-Session-Token`, `session=` /
  `token=` in the URL; URL `username=` / `password=` do not count). The
  upstream clients put credentials in the body (the SDK's `/api` JSON,
  Workbench's `/file/upload` form), so their uploads stop at 64 MiB unless
  the credentials go in the URL or `preauth_max_body_size` is raised. A
  larger `Content-Length` is answered `413` before a byte of the body is
  read, a chunked body is cut off at its cap (`413`; its framing may take
  64 KiB plus a sixteenth of the cap besides the data), and `Expect:
  100-continue` is answered only after the decision. Credentials in the head
  that fail are answered `401` before the body (not the envelope's `1001`);
  a locked-out or blocked address is not checked and is held to the
  pre-auth cap. An upstream inner request (`inner=true`, honoured only with
  `trust_inner_localhost`) is never authenticated from its head: its body is
  held to `preauth_max_body_size` (raise that key for larger inner uploads).
  For large data, raise `upload_max_body_size` and `request_body_budget`, or
  load server-side files (`build` / `batchInsert` from a `--load-dir`
  directory) or use `gbuild` / `gadd`.
* **Body budget.** All request bodies in memory share `request_body_budget`
  (512 MiB), charged as their bytes arrive and held until the response is
  written. A request whose announced body does not fit is answered `503`
  before its body is read, one that runs out part-way `503` too, and the
  connection is closed (clients retry later). Size it to the memory the
  server may spend on bodies; it is raised to `upload_max_body_size` when
  set lower.
* **Slow clients.** A request body must arrive at `min_body_rate` (64 KiB/s)
  on average after a 30 s grace from the end of its head — the allowance is
  earned by bytes received, never by the declared `Content-Length` — or it is
  answered `400 bad request: the request did not arrive in time` and the
  connection closed. A client that drips its body is cut off after about
  30 s; holding a worker longer takes really sending at least
  `min_body_rate` (one worker per 64 KiB/s of the client's bandwidth). No
  request holds a worker longer than
  `30 s + (max_body + 64 KiB + max_body / 16) / min_body_rate` (the last
  two terms: a chunked body's framing allowance), and no file download
  longer than `write_timeout + size / min_body_rate` (a reader that falls
  behind is disconnected, freeing its worker and its
  `download_max_concurrent` slot). Under TLS the handshake and every record
  of the head count against the head deadline, and the idle wait between
  kept-alive requests is bounded the same way. Lower
  `min_body_rate` (or set `0`, the per-read timeout alone) for clients on
  very slow links.
* **Connections per address.** `max_conns_per_ip` (64): a further
  connection from an address that already has that many open (waiting for
  a worker included) is closed as soon as it is accepted, logged at most
  once a second (`refused a connection from <ip>: it already has <n> open
  ([server] max_conns_per_ip)`). IPv4-mapped IPv6 peers count as their IPv4
  address; IPv6 clients are keyed by their full address, so a client
  rotating within its prefix is not held to one count — pair the key with a
  firewall limit. Keep it below `thread_num` where untrusted clients with
  fast links connect.
* **Statements.** `query_timeout` (seconds, fractions allowed; `3600`, `0` =
  off) stops a statement still running — REST `/sparql`, `/update` and
  `/tquery`, `/api` `query` (synchronous or `async`) and `tquery`, the
  single-database `/sparql` and `/update` — at its next cancellation check:
  REST and single-database `503 {"error":"query timed out after …
  (query_timeout) …"}`, `/api` `1005 "Query fail: query timed out …"`. It
  measures the statement's evaluation only: neither the wait for its
  database (behind a write, a save or a backup) nor the save after an
  UPDATE counts. It bounds the case where a long read and a writer queued
  behind it freeze a database: lower it on shared servers. A timed-out
  transaction statement leaves the transaction open, with the operations a
  multi-operation UPDATE applied before it was stopped (the answer names
  them; roll back to discard them). `max_result_bytes` (1 GiB, `0` = off)
  refuses an answer as soon as its serialization passes it
  (`503 {"error":"result too large: …"}` / `1005`; a streamed
  single-database answer is cut off without its closing chunk);
  `format=file` / `n-triple-file` results, written to disk, are exempt.
* **SPARQL and RDF sizes.** See `sparql_max_size` and `rdf_max_token_size`
  under [Configuration and logs](#configuration-and-logs). `/api` bodies are
  parsed within bounds: a JSON body holds at most 65,536 values, a query
  string, form or multipart body at most 65,536 parameters or parts
  (`1003`); a multipart boundary must follow RFC 2046 (1 to 70 characters).
* **Strict request framing** (RFC 9112): `Content-Length` together with
  `Transfer-Encoding`, differing or non-decimal `Content-Length` values
  (`+5`, `0x5`), any `Transfer-Encoding` other than exactly `chunked`
  (`identity`, `gzip`, …) or any `Transfer-Encoding` in HTTP/1.0, chunk sizes
  that are not bare hex, header lines without a colon or with whitespace
  before it, obs-fold continuations, and control characters other than tab
  in a request line, header, chunk line or trailer are answered `400` and
  the connection closed. No known upstream client (curl, Python `requests`,
  the upstream SDKs and scripts) sends any of them; `scripts/conformance.sh`
  stays green.

### Downloads and exports

`/file/download` (and `/grpc/file/download`) needs credentials:
`username` / `password` parameters (`?username=…&password=…`, or a POST
body), HTTP Basic (`curl -u user:pass`, `wget --user … --password …`), or a
session token with a `username`. Without them it answers `401` with
`WWW-Authenticate: Basic` and a `1001` envelope, before anything is
resolved. Every file the server writes for a client (upload, export,
`format=file` / `n-triple-file` query result) gets an owner record in
`<db_home>/file_owners/`, and a user other than root / admin reads it —
by download, or as a `build` / `batchInsert` / `batchRemove` source — only
when the record allows: an export with the `export` privilege on its
database, a query result with `query` on it, an upload only by the user who
uploaded it.
Files without a record (put there by the operator, or by older versions)
are root's and admin's alone. A refused download answers upstream's
`FileNotFound` (StatusCode 11), a refused source `1003`. Back the owner
records up with the server's root: without its record an export or upload
is readable by root / admin only.

Export and query-result file names carry 128 random bits
(`<db>_<YYYYmmddHHMMSS>_<32 hex>.nt|nq|zip`,
`<db>_<threadId>_<UTC YYYYmmddHHMMSS>_<32 hex>.json|txt`); clients use the
reported `filepath` / `FileName`. Downloads are streamed in 64 KiB chunks
with their `Content-Length` and never compressed — with the `gzip` feature,
only buffered responses (the JSON envelopes, query results) are.
`[upload] download_max_concurrent` (8; `0` = no limit) bounds the streaming
downloads at once, each of which holds a worker of `thread_num`; further
ones are answered `503` with `Retry-After: 1`.
`[upload] file_retention_hours` (168; `0` = keep forever) removes recorded uploads,
exports and query results older than that, with their records (operator
files are never touched), and spilled asynchronous results under
`<root>/tasks` that belong to no task of the running server; the sweep is
lazy (on uploads, exports, downloads, `format=file` queries and asynchronous
submissions, at most once a minute). **Exports and query results are
therefore deleted after a week by default**: copy what must be kept out of
the export directory. `[upload] upload_user_quota` (bytes of retained
uploads per user, `0` = none) refuses an upload beyond it with `1005
"Upload quota exceeded: …"`.

REST `/export` writes an unnamed scratch file in the export directory
(`[upload]`'s, by default `<root>/export`) under the database's shared
guard and streams it once the guard is released: the directory needs room
for one export per concurrent `/export`.

### HTTPS, sessions and `gconsole`

`gserver --tls-cert FILE --tls-key FILE` serves HTTPS in a build with the
`tls` feature. A `gserver` built without it refuses both options with an
argument error and exit status `2` (it used to warn and serve plaintext
HTTP). `gconsole --tls --ca-file <the CA of gserver's certificate>` (also a
`tls` build) talks to such a server, verifying its certificate against
`--ca-file`, else `$SSL_CERT_FILE`, else the system CA bundle; a certificate
the bundle does not vouch for is a connection failure and nothing is sent.
Without TLS, use the console on the server's machine or through an SSH
tunnel: a plain-HTTP `gconsole` connection to a non-loopback host prints a
warning that the password (at login) and every statement cross the network
in clear text.

`gconsole` logs in once (`POST /login`) and sends the session token, not
the password, with every `/api` request. That is a gstore-rust extension of
the upstream protocol: an `/api` request without a `password` may present a
live session token as the REST endpoints take it (`Authorization: Bearer`,
`X-Session-Token`, `session=` / `token=`), also for `/file/upload` and, with
a `username`, `/file/download`; a `username` given must name the session's
user, and an unknown, expired or revoked token is answered `1001`. A token
is as good as the password until it expires (24 h), the user logs out, or
the user's password changes. A request carrying a password is checked
exactly as before, so upstream clients are unaffected; there is no compat
switch. REST session tokens are read from the request head or URL only,
never from a body parameter.

### gNLQA server

`gnlqa serve` requires `GNLQA_HTTP_TOKEN` (see the table above); `/health`
stays open. Each client address (an IPv6 client per /64) may send
`GNLQA_HTTP_RATE_PER_MIN` (30) question requests a minute, counted before
authentication (`429` with `Retry-After` beyond); a question over
`GNLQA_MAX_QUESTION_BYTES` (4096) gets `413`; one request may make at most
`GNLQA_MAX_LLM_CALLS` (16) LLM calls, read `GNLQA_MAX_KB_ROWS` (200000) KB
rows and run `GNLQA_REQUEST_TIMEOUT_SECS` (120 s) — an exhausted budget
answers `503` (`limit_exceeded`). The server takes 64 connections, at most 8
from one client (`429` beyond; behind a reverse proxy every client is the
proxy's address). Error answers carry a generic message, a stable `code`
and a `request_id`; the full error goes to standard error under that id.
`gnlqa` reads its configuration only from `$GNLQA_CONFIG`, else
`$XDG_CONFIG_HOME/gnlqa/gnlqa.conf`, else `~/.config/gnlqa/gnlqa.conf` (a
`gnlqa.conf` in the working directory is ignored with a note;
`GNLQA_CONFIG=./gnlqa.conf` opts in) and refuses a world-writable file.
Details: [`gnlqa/README.md`](../gnlqa/README.md).

## Backups

**Databases.** `/api` `backup` (and `compress=true` for a zip), `gconsole`'s
`backup` and `gbackup` (`-z 1` for a zip) copy a database's whole
directory, as upstream does — its data and its **reasoning rules** with
their ownership records (`reason_rule_files/`, see the README's
Reasoning section). `restore` / `grestore` (a backup directory or a zip)
bring both back, so a restored executed rule still retracts exactly the
triples it inferred. `drop` with `is_backup=true` (the default) keeps the
directory, rules included, as `<db>.db.bak`. `export` / `gexport` write the
data only, as upstream's export: the inferred `<Rule:…>` triples are in it
as ordinary triples, the rules are not (a database rebuilt from an export
holds those triples as asserted ones, owned by no rule). `gbackup` holds the
database's directory lock for the whole copy, so it refuses a database
another process has open — back up a served database through the server
(`/api` `backup`, REST `/backup`, `gconsole`'s `backup`). `gbackup` refuses
a destination that is the database directory or lies inside it (exit 1,
nothing touched). A REST or `/api` backup directory holding
`BACKUP_OF.partial` is a backup that did not finish (a crash, a panic, a
failure part-way): `/restore` and `/api` `restore` refuse it (`409` /
"incomplete"), and the next backup of the same database into it is allowed
and completes it.

**File ownership records.** `<db_home>/file_owners/` records who may read
each upload, export and query result the server wrote (see [Downloads and
exports](#downloads-and-exports)); back it up with the server's root —
restored without its record, an export or upload is readable by root and
admin only.

**PFN functions** belong to users, not to databases: they live under
`pfn_base_path` (by default `<db_home>/pfn` — `/var/lib/gstore/dbhome/pfn`
in the packaged and container layouts), one directory per user:

```text
<pfn_base_path>/src/<user>/data.json     every function's definition (one JSON object per line)
<pfn_base_path>/src/<user>/<name lower-cased>.wat    the generated WebAssembly text
<pfn_base_path>/lib/<user>/lib<name lower-cased><md5(lastTime)>.wasm   the compiled module
```

No database backup includes them. Back them up by archiving
`pfn_base_path` (e.g. `tar -C /var/lib/gstore/dbhome -czf pfn.tgz pfn`,
while no `funcudb` is running); `src/` is enough — `buildpfn` / `funcudb`
type `4` recompiles a function from it. To restore, stop `gserver`, unpack
the archive in place and start it again. Deleting a user (`/api`
`usermanage` type `2`) deletes that user's functions.

## Configuration and logs

`conf.ini` is upstream's file key for key (`conf/conf.ini.example`
documents every key and the gstore-rust extensions). Command-line flags win
over it: `-c FILE`, `-p/--port`, `--addr`, `--password`, `--password-file`, `--db-home`,
`--thread-num`, `--no-checkpoint`, `--load-dir`, … (`gserver -h`).

**Precedence.** Command-line flag > `conf.ini` > built-in default — except
that the environment variables `GSTORE_DISK_QUERY_MODE`,
`GSTORE_DISK_CACHE_MB`, `GSTORE_VSTREE_ON_OPEN`, `GSTORE_ID_RECLAIM` and
`GSTORE_ROCKS_BLOCK_CACHE_MB` override their `conf.ini` keys whenever a
database is opened (a variable left in a service's environment wins over the
file); `gserver` logs the effective values at startup. The tools without a
`conf.ini` (`gadd`, `gsub`, `gquery`, …) follow the variables only.

**Inline comments.** A `#` or `;` starts an inline comment only when it
stands as a word of its own — whitespace before it, whitespace or the end
of the line after it — after some of the value (`port=9000  # http` reads
`9000`). A marker glued to more text stays in the value, as upstream reads
it; nothing inside double quotes is a comment; `root_password` is never
cut; and every cut is a startup warning ("… is read as a comment (upstream
gStore reads it as part of the value; quote the value to keep it)"). Quote a
value that must contain a `#` or `;` standing as a word. *Upgrade notes:* a
`root_password` line with a note after the value
(`root_password="pw"  # …`) now sets the whole text, as upstream does (it
seeds a new user store only; the startup warning does not quote it) —
remove such notes; and for every key, a note glued to its marker
(`db_home=/srv/gstore #primary`, `backup_path=./backups ;nightly`,
`ip_deny_path=./conf/ipDeny.config #blocked`) was cut by earlier
gstore-rust versions and is now part of the value, as upstream reads it —
so a database home, backup directory or IP list path can move on upgrade.
Each such line is a startup warning naming line, key and kept text; put a
space after the marker, or the note on a line of its own. Upstream's files,
which have no inline comments, parse as before. `gserver` also warns at startup about a configured `ip_allow_path` /
`ip_deny_path` it cannot read (an empty allow list admits nobody, an empty
deny list denies nobody; the default `./conf/ipDeny.config` missing is not
reported), and `gbuild` prints `conf.ini` warnings and read errors on
standard error ("gbuild: conf.ini warning: …").

**Keys accepted but not acted on:** `tcp_port`, `min_memory`,
`min_million_disk`, `min_million_memory`, `resource_check` (`on` is a
startup warning: gstore-rust performs no resource check), `db_suffix`
(always `.db`; `gserver` warns about another value) and `root_username`
(always `root`; `gserver` warns).

**Query evaluation** (`[query]`, a gstore-rust section upstream has no keys
for): `integer_division` (`decimal`, SPARQL 1.1's `7/2` = `3.5`, or
`truncate`, upstream's `3`), `query_cache_size` (each database's read-query
cache budget in bytes, `0` = off, default 64 MiB), `early_termination`,
`topk_bounded_heap`, `topk_dpb` (Top-K evaluation of `LIMIT` / `ORDER BY …
LIMIT`) and `gfun_max_output_bytes` (the most JSON the graph-function and
`PFN.name` calls of one query may produce together — every call, each
`GROUP BY` group and `GRAPH` block included — default 64 MiB; a query past
it fails with `<function>: more than N bytes of output in this query`). The
iterative functions (`PR`, `louvain`, `labelProp`) also stop at a fixed work
bound (`gfun::MAX_ITERATION_WORK`, 2^34 vertex / edge / community visits
beyond the first 1024 sweeps, so `maxIter` up to 1024 always runs to the
end; typically hit with `tol < 0` or `increase < 0` and a huge `maxIter`),
and every function polls `/cancel` inside its loops. `maximumKplex`'s search
holds up to about 136 MiB per call at its 8192-vertex ego-network limit (one
call per worker). These keys apply to every database the server loads,
builds, creates or restores, and `refreshconf` re-applies them to the
loaded ones. The graph functions (`shortestPath`, `PR`, `louvain`, …)
run on a per-database CSR built on the first such query after a change;
`/api` `loadcsr` (or `load` with `csr=1`) builds it ahead of time — as a
task `/tasklist` lists and `/cancel` stops — and `show` reports whether it
is built (`csr`). Any write discards it; a cancelled build caches nothing.

**PFN functions** (`[pfn]`, a gstore-rust section; see
[`docs/PFN.md`](PFN.md)): `pfn_fuel_per_call` (instructions per call,
default 10^9, `0` = unlimited), `pfn_fuel_per_query` (the fuel all the PFN
calls of one request may use together — every request starts from zero and
the operations of one UPDATE share one budget; default 10^10, `0` =
unlimited), `pfn_max_memory_pages` (64 KiB pages, default 1024),
`pfn_max_module_bytes` (default 1 MiB) and `pfn_max_functions` (per user,
default 100) bound the sandbox; like `[query]`, `refreshconf` re-applies
them. Fuel counts instructions, host bytes and each call's instantiation
(its initial memory and data section, one unit per 8 bytes); a request past
`pfn_fuel_per_query` fails with `run f fail: the instruction budget of N for
the PFN calls of one query (pfn_fuel_per_query) is exhausted`. Deployments
with very large PFN queries (hundreds of thousands of calls, or calls near
the per-call budget) raise it or set `0`; it is a limit, not an
upstream-compat switch (upstream runs PFN as native code without limits).
`[system] pfn_base_path`
(upstream's key; default `<db_home>/pfn`) is where the functions are kept —
read at startup only, so moving them needs a restart (and the files moved
along, see [Backups](#backups)). Compiling and calling functions needs a
build with the `pfn-wasm` feature (the container image has it, see
[Docker](#docker)); without it they can still be listed, created, updated
and deleted. Reasoning rules (`reasonManage`) have no keys.

**Storage, builds and id reclamation** (gstore-rust keys; the README's
*Configuration* and *Storage layouts and query modes* sections have the
details). `[storage]`: `disk_query_mode` (`memory`, the default, loads an
on-disk database's working set at open; `stream` queries its B+ trees
directly), `disk_cache_mb` (16), `dict_cache_mb` (80), `vstree_on_open`
(off), `default_layout` (the layout of `gbuild`, REST `/build` and `/api
build` when none is named: `snapshot` — the default — `disk` or
`rocksdb`), `rocksdb_block_cache_mb` (256), `rocksdb_bloom` (10) and
`rocksdb_max_open_files` (4096, the most files one RocksDB database keeps
open; `0` = unlimited, RocksDB's own default — size `LimitNOFILE` for it,
see [systemd](#systemd)). `gserver` installs them at startup and again on
`refreshconf` (a loaded database keeps the query mode it was opened in). A
RocksDB database also keeps four old info logs and bounds its write-ahead
logs to 256 MiB; a `check_point` save of it is a commit point (an fsync of
RocksDB's write-ahead log), and its memtables are flushed by `gcompact`, a
backup and close. `[system] id_reclaim`
(`on`, the default) reuses the ids of deleted terms; with it on, a new
on-disk database is created in `kvstore.kv` version 2, which binaries older
than Phase 6 refuse (`id_reclaim=off` keeps creating version 1).
`[system] build_memory_mb` (1024), `build_threads` (0 = every core;
`build_multi_thread=off` means 1, and another `build_threads` is then a
startup warning), `build_tmp_dir` and `build_fill_factor` (90) bound the
bulk build of `gbuild --disk` / `--rocksdb` and of server builds with
`layout=disk` / `rocksdb`. The shipped `conf.ini.example` leaves `build_multi_thread` at its
default (`on`, shown commented out; upstream's file has `off`, which earlier
examples copied); existing installations keep their `conf.ini`. `gbuild`
reads `./conf/conf.ini` when it exists (`default_layout`, `id_reclaim`, the
build and RocksDB keys); `gquery`,
`gadd`, `gsub` and the other tools that open a database follow the
environment only. Environment variables win over the file:
`GSTORE_DISK_QUERY_MODE=stream|memory`, `GSTORE_DISK_CACHE_MB`,
`GSTORE_VSTREE_ON_OPEN`, `GSTORE_ID_RECLAIM=on|off` and
`GSTORE_ROCKS_BLOCK_CACHE_MB` (a plain number of MiB, the block cache a
server's RocksDB databases share; `0` = RocksDB's own per-column-family
caches; another value is logged and ignored). `GSTORE_BULK_CRASH=<step>` is
a crash-test failpoint, live in release binaries: while it is set, every
bulk build of the process aborts right after that install step (and says so
on standard error; a value naming no step is reported and ignored) — never
set it outside crash tests.

**Zip packages** (a build with the `zip` feature): `build`, `batchInsert`
and `batchRemove` read a `.zip` of RDF files (each entry needs an extension
from `upload_allow_extensions`), `backup
compress=true` writes `<db>.db_<timestamp>.zip`, `restore` takes such an
archive, and `export compress=true` zips the export file. A package is
checked in full before anything is unpacked: unsafe entry names (absolute,
`..`) are refused, and `[upload] unzip_max_entries` (default 10000) and
`unzip_max_size` (default 10 GiB, counted on the bytes actually written)
bound what it may unpack to — all the packages of one request together; a
package named twice in a request is unpacked once. `unzip_max_entries` is
enforced from the end-of-central-directory records, before the central
directory is decoded. Without the feature these requests answer `1003`,
naming it.

**RDF and SPARQL size limits** (`[upload]`, gstore-rust keys; limits, not
upstream-compat switches — upstream has none): `rdf_max_token_size`
(default 64 MiB, `0` = no limit) is the longest token the streaming RDF
readers hold (a Turtle / TriG string, IRI, name, number or language tag)
and the longest N-Triples / N-Quads line; a longer one fails its statement,
which a tolerant build skips and logs. `sparql_max_size` (default 16 MiB,
`0` = no limit) is the longest SPARQL query or update `gserver` (and
`gbuild`) accepts, refused before it is tokenized ("the request is N bytes
long, more than the limit of M bytes ([upload] sparql_max_size)"); `gquery`,
`gconsole --local`, library callers and `gnode` read no configuration and
have no limit. A deployment sending `INSERT DATA` bodies over 16 MiB raises
it or sets `0`, but large loads belong in `build` / `batchInsert`.

**Diagnostic log** (errors, warnings, startup and shutdown events — the
analogue of upstream's log4cplus `SLOG_*` log):

* `[log] log_mode` names a log4cplus properties file, as upstream
  (`conf/slog.properties.example` is upstream's: level `INFO`, the console
  and `./logs/all.log`). Honoured: the root level, `ConsoleAppender`
  (standard error) and one `DailyRollingFileAppender` (`File`,
  `Schedule=DAILY`) with an optional `Threshold`. As upstream's log4cplus
  (whose `MaxBackupIndex` only limits one day's `.1`, `.2` … backups), the
  rolled daily files are all kept unless `MaxHistory` (days) or `log_days`
  sets a retention. When the file does not exist, the log goes to standard error
  at level `info`.
* gstore-rust extension keys override it: `log_level`
  (`trace`/`debug`/`info`/`warn`/`error`/`off`), `log_file` (a path, or
  `off`), `log_days` (days of rolled files kept, `0` = all; `30` when
  `log_file` sets the file) and `log_stderr`
  (`on`/`off`); the `GSTORE_LOG` environment variable (a level) overrides
  the level once more, e.g. `GSTORE_LOG=debug` logs every request.
* The file rolls over at local midnight into `<file>.YYYY-MM-DD`; rolled
  files older than the retention are deleted. Each record is
  `YYYY-MM-DD HH:MM:SS.mmm [LEVEL] [module] message`.
* The startup lines `gserver` prints on standard output (where it listens,
  its pid file, …) are recorded in the log file too.
* Reasoning and PFN events, as upstream logs them: a rule executed or
  disabled (`info`, with the triples inferred / retracted; a failure
  `warn`), its compiled statements (`debug`); a PFN function compiled or
  failing to compile (`info`), each call (`debug`) and a failed call
  (`warn`); a stale PFN file that cannot be removed or an unreadable
  `data.json` line (`warn` / `error`).
* `/api` `refreshconf` re-reads `log_mode`'s file and these keys and
  reconfigures the log (another file or level takes effect at once), as
  upstream's `refreshconf` re-runs log4cplus.

**Query and access logs** (as upstream; read with the `/api` operations
`querylog`, `querylogdate`, `accesslog`, `accesslogdate`):

* `querylog_mode` / `querylog_path` / `querylog_days`: one JSON record per
  SPARQL statement — `/api` `query` and the REST endpoints `/sparql`,
  `/update`, `/tquery` alike, PFN calls included, and for a
  `reasonManage` execute / disable / check the rule's compiled statement
  as upstream logs it (for a disable, upstream's nominal `delete where`:
  a rule with a ledger retracts exactly its ledger's triples, whose number
  is the record's `ansNum`) — with the client IP
  (`remoteIP`), the number of answers (`ansNum`), the latency in ms
  (`queryTime`), the status, the format and the database, in a daily file
  `<querylog_path>/YYYYMMDD.log`. Failed statements are recorded too, with
  their real `statusCode` / `statusMsg` — `/api` query errors, unknown
  formats, timeouts and too-large answers (upstream records successes only)
  as well as the REST endpoints' failures; a write applied in memory but not
  saved is recorded with `statusCode` 500.
* `accesslog_mode` / `accesslog_path` / `accesslog_days`: one JSON record
  per `/api` operation (`reasonManage`, `funcudb`, `funquery`, `funreview`
  included) with the client IP. Every HTTP request (REST and
  `/api`) is also appended to `<db_home>/access.log` — time, method, path,
  user, status and client IP, tab-separated (`GET /accesslog`).
* The statements are also appended, as text, to `<db_home>/query.log` —
  time, database, user, statement, tab-separated (`GET /querylog`).
  `query.log` follows `querylog_mode` and `access.log` `accesslog_mode`;
  both roll over daily into `<file>.YYYY-MM-DD`, keeping `querylog_days` /
  `accesslog_days` days (`GET /querylog` / `/accesslog` read today's file).
* A relative `querylog_path` / `accesslog_path` is taken relative to
  `db_home` (upstream: to its working directory).
* Each line of `access.log`, `query.log` and `txn.log` is written with one
  `write(2)`, so concurrent requests no longer interleave them.

**Transaction log** (`<db_home>/logs/transaction.json`, read with the
`/api` `txnlog` operation): it is rotated at 32 MiB into
`transaction.json.1` (one older generation is kept, hard-linked before
the rename so the log is never missing); the records of transactions still
running are carried into the new file, and `txnlog` lists the current
generation, reading only what was appended since its previous call. A torn
last line of the transaction, query or access log (a crash, a full disk, a
zero-filled tail after a power loss) is ended before the next append, so it
stays one unparsable line, which readers skip, and the next record is kept.
At startup, transactions a previous process left `RUNNING` are marked
`ABORTED`.

## Operations

### Transactions

A database has at most one open transaction (REST `/begin` … `/commit` /
`/rollback`, the `/api` operations); while it is open, a second `begin` and
every other write to the database are refused (`/api` `1008` / `1004`, REST
`409`).

* **Idle timeout.** `[system] txn_idle_timeout` (seconds, default 600;
  `0` = never, upstream's behaviour and an upstream-compat value) rolls back a
  transaction that has had no statement — and no `/commit` or `/rollback` —
  in flight for that long, logging it `ABORTED` (`txn.log`: `aborted`). It
  is reaped by the next `begin` on its database, by any request once the
  earliest idle deadline passed, or by a background pass every
  `min(txn_idle_timeout / 4, 30 s)` (which also frees a RocksDB database's
  copy); none of them waits for a busy database. `/tquery`, `/commit` or
  `/rollback` of an expired transaction answers "not running (rolled back
  after txn_idle_timeout)" (`409` / `1005`). `refreshconf` applies a new
  timeout at once to `begin` and the request path, but the background pass
  keeps the interval it was started with — and is not started at all when
  the server started with `0` — until a restart.
* **Deleted owners.** A transaction of a deleted user is rolled back
  whatever the timeout ("rolled back because its owner was deleted") by the
  next `begin` on its database, by the background pass, or by the next
  request after one of its transaction requests found it orphaned; a plain
  write in between is still refused until one of those runs.
* **Who may end one.** The owner or any superuser (`root`, or a user with
  the `admin` privilege) may `/tquery`, `/commit` or `/rollback` it;
  `rollback` needs no update privilege (committing does). Upstream lets
  only `root` end another user's transaction.
* **Transaction ids.** REST and `/api` take a positive integer or the
  transaction log's `<begin>_<tid>` form, whose begin time must be the
  transaction's (REST no longer accepts a sign, `+2`): a stale id gets the
  remembered end of the transaction it names ("not running (already
  committed …)") or "Wrong transaction id" (REST `404`, `/api` `1005`),
  never another transaction with the same number.
* **Monitoring.** REST `/show` and `/monitor` and `/api` `show` / `monitor`
  report each database's open transaction as a `transaction` object
  (`TID`, `txnlog_TID`, `user`, `isolation`, `begin_time`, `idle_seconds`,
  `running_statements`).
* **Commit failures.** A commit whose commit point fails is either rolled
  back — REST `500` / `/api` `1005` `"commit failed and transaction N was
  rolled back: <cause>"` (on a disk database whose store the failure broke,
  plus that it must be unloaded and loaded before writing again) — or, when
  part of it may already have reached the store, answered as not durable:
  `"applied in memory but not durable: the commit point of transaction N
  failed: …; the database needs a reload (unload, then load)"`. Both are
  logged `ABORTED` (`txn.log`: `commit-failed-rolled-back` /
  `commit-failed-not-durable`); a retried commit is told which. After a
  not-durable answer, unload and load the database: the reload keeps what
  the write-ahead log committed.

These are gstore-rust extensions of upstream's transactions: the idle
timeout, rollback without the update privilege, superusers ending any
transaction, the checked `<begin>_<tid>` form and the `transaction` object.

### Write failures and `storage failed`

A write is never acknowledged before it is saved: a write whose save fails
(disk full, a quota, an I/O error) is answered as a failure — HTTP `500`
from REST and the single-database server, StatusCode `1005` from `/api` —
with the message `"applied in memory but not durable: <cause>"`. Clients
must treat such an answer as "not committed". The write stays applied in
memory: on the snapshot layout the next successful save, `/checkpoint`,
`/unload` or graceful shutdown saves it (it is lost if the process dies
first); on the on-disk layouts a failed commit point breaks the database
until it is reopened, which discards those writes.

A reply or log line `storage failed: …` (or `the on-disk store is broken by
an earlier write error …`) means a commit of that on-disk database failed —
typically the disk filled up or an I/O error occurred. The database then
refuses every further write (in Stream mode every statement; REST and the
single-database server answer `500`) until it is reopened; other databases
are unaffected. The file on disk is consistent: the last commit whose
write-ahead log was complete is replayed when the database is opened again.
Free the space (or fix the I/O problem), then `unload` and `load` the
database (`/api` operations, or REST `/unload` and `/load`), or just `load`
it, which reopens a broken loaded database in place (one busy with a
request at that moment answers "already loaded": repeat). The answer's
`warning` and the server log name the failure and how many changes since
the last successful commit point are discarded; clients were told of each
failed update when it happened. In the single-database mode (`gserver DIR`),
restart the server. A database whose `kvstore.kv.wal` exists but cannot be
read is refused at open (`cannot read the write-ahead log …`): fix its
permissions or the disk error rather than deleting it — it may hold the
last committed batch. A `kvstore.kv` (or `vstree.kv`) shorter than its
header says is refused at open as torn or truncated, and a database whose
`vstree.kv` is empty or corrupt fails to open naming the file (remove it to
rebuild the index).

### Monitoring

* HTTP `500` or StatusCode `1005` with "applied in memory but not durable"
  means saves are failing (disk full, a quota, EIO).
* HTTP status codes of REST and the single-database server: `409` a lock
  another process holds (a database directory or a paged file another
  `gserver`, `gquery`, `gconsole --local` or bulk build has open) or a
  transaction conflict; `500` a storage or persistence failure, including a
  store read error during a query and an I/O error on a lock file; `503` a
  statement cancelled, timed out (`query_timeout`, also a single-database
  UPDATE stopped part-way), too large (`max_result_bytes`), the body budget
  exhausted or the server shutting down; `400` a syntax or evaluation error.
  `/api` keeps HTTP 200 and its StatusCodes.
* At startup, the warnings "cannot list the database directories" and "…
  stays registered" mean the server root, or a database directory or the
  volume it lives on, was unreachable: the registry was not reconciled, or
  that database kept its registration, creator and grants. Only a database
  directory that is confirmed gone is unregistered.
* `refused a connection from <ip>: it already has <n> open` — see
  `max_conns_per_ip`.

### Leftovers of interrupted operations

All safe to delete while no tool or server works on the database:

* `<name>.db.drop-old` beside the databases: a drop (`gdrop`, REST
  `/drop`, `/api` `drop` with `is_backup=false`) renames the directory
  aside durably before deleting it, so an interrupted drop (a crash, a power
  loss, an I/O error while deleting) leaves this. It is not listed, not
  registered and holds no data anyone reads; the next drop of the name
  removes it too.
* `<dest>.part-<pid>-<n>` beside a `gquery -f` or `gexport` output (see
  [Command-line tools](#command-line-tools)).
* `<db>.db.unzip-tmp-<pid>-<random>`: the scratch directory of a zip
  package unpacked by `gbuild` / `gadd` / `gsub`.
* A backup directory holding `BACKUP_OF.partial` (see [Backups](#backups))
  — rather than deleting it, back up into it again.

### Asynchronous operations

Callbacks of asynchronous operations are delivered by their own bounded
pool, and the queue of pending callbacks is bounded too; queued callbacks
hold spilled results by file name and read them at delivery (library
settings `TaskConfig::callback_workers`, default 2,
`callback_queue_bytes`, 64 MiB, and `callback_pending_per_user`, 32; no
`conf.ini` key). Spilled results under `<root>/tasks` that belong to no
task of the running server are removed by the retention sweep (see
[Downloads and exports](#downloads-and-exports)).

## Command-line tools

The tools work on the databases of their **current directory** (`<name>` is
`./<name>.db`) — run them in the server's `db_home`
(`/var/lib/gstore/dbhome`, as user `gstore`) to work on its databases, and
only while the server does not have the database loaded. Besides their own
arguments they accept upstream 1.3's option spellings:

```bash
gbuild -db lubm -f lubm.nt         # gbuild lubm lubm.nt; no -f: an empty database
gbuild --force -db lubm -f new.nt  # gbuild refuses to overwrite a database without --force
gquery -db lubm -q query.sql [-f result.txt]
gadd -db lubm -f more.nt           # or -dir DIR
gsub -db lubm -f old.nt
gshow                              # every database here (or --db-home DIR); gshow lubm: its counts
gmonitor -db lubm
gexport -db lubm [-f DIR]          # writes DIR/lubm_<timestamp>.nt (.nq with named graphs)
gexport -db lubm -z 1              # lubm_<timestamp>.zip holding the export (feature zip)
gexport -db lubm --format nq       # N-Quads: the default graph, then every named graph
gbackup -db lubm [-p ./backups/]   # creates ./backups/lubm.db_<timestamp>
gbackup -db lubm -z 1              # ./backups/lubm.db_<timestamp>.zip (feature zip)
grestore -l lubm                   # lists those backups, zipped ones included
grestore -db lubm -p ./backups/lubm.db_<timestamp>   # = grestore lubm <backup>; a .zip too
gbuild -db lubm2 -f lubm.zip       # the RDF files of a zip package (feature zip)
gdrop -db lubm --yes               # the confirmation is required (upstream asks none)
gbuild --disk -db lubm -f lubm.nt  # the on-disk layout, built in bulk (--rocksdb: RocksDB)
gcompact -db lubm                  # the id spaces; --reclaim frees unused ids, --check
                                   # verifies them, --enable-reclaim upgrades an older
                                   # kvstore.kv (older binaries then refuse it)
```

`grestore` takes the database first and the backup second, as upstream
(`-db`, `-p`); earlier gstore-rust releases took them the other way round.
Every tool answers `-h`/`--help`; the exit status is `0` on success, `1`
when the operation failed and `2` for invalid arguments — `gserver`
included (it used to exit `1` on invalid arguments; `--tls-cert` /
`--tls-key` in a build without the `tls` feature are invalid arguments).
`gbuild` also exits `3` when the build completed but tolerant mode skipped
statements (each listed in `<db>.db/parse_error.log`): check for it in
migration scripts (`gexport` → `gbuild`), and build with `--strict` to stop
at the first error instead.

* **`gbuild` holds its directory** from start to end: it creates and claims
  the database directory before it writes anything (the parse error log
  included), so a `gserver` or another tool holding the directory makes it
  fail at once with status `1` ("cannot build '…': … locked by another
  process"), touching nothing. Without `--force`, a name that was new when
  `gbuild` checked it is checked again once claimed. A failed build removes
  only a directory it created and still holds, and only while it holds no
  database; a build that succeeded keeps its database even when its summary
  cannot be printed (status `1`, "built '…', but its summary could not be
  printed"). `gbuild --force`, a bulk rebuild and REST `/build` over an
  existing database remove the replaced database's reasoning rules under
  the rule lock, renaming them aside before deleting them. Within one
  process a bulk build's claim refuses other threads' opens, saves, drops
  and restores of that directory (a lock conflict, REST `409`).
* **Outputs are replaced whole.** `gquery -f FILE` and `gexport` (also with
  `-z`) write `<dest>.part-<pid>-<n>` beside the destination (private,
  `0600`, until complete), sync it and rename it over the destination: a
  failure leaves the previous file as it was. A crash may leave such a
  `.part-*` file behind (safe to delete). A symbolic link keeps pointing at
  its file (which is replaced, keeping its mode and, where possible, its
  owner); a FIFO or a device (`/dev/null`) is written in place; a replaced
  file loses extra hard links (it is a new inode).
* **Zip packages** of `gbuild` / `gadd` / `gsub` are unpacked into a fresh
  `<db>.db.unzip-tmp-<pid>-<random>` (mode `0700`), removed when the tool
  ends; a crash may leave one behind (safe to delete when no tool runs).
* **`gdrop`** renames the directory to `<name>.db.drop-old` and syncs its
  parent before deleting it (see [Leftovers of interrupted
  operations](#leftovers-of-interrupted-operations)); **`gbackup`** refuses a
  destination inside the database.

## Upgrade notes

Behaviour an upgraded server or script may notice (the
[`CHANGELOG`](../CHANGELOG.md) has the full list):

* **Root's default password.** An API server whose root still has
  `123456` refuses to start, on loopback too: see [Before exposing a
  server](#deploying-gstore-rust) at the top of this guide.
* **`conf.ini` inline comments** follow a narrower rule, and a note glued to
  its marker is now part of the value: see [Configuration and
  logs](#configuration-and-logs).
* **Browser clients.** No CORS header unless `cors_allow_origin` lists the
  origin, `Host` and `Origin` checks on loopback and state-changing
  requests, `GET` never updates, `POST /update` refuses `text/plain` and
  multipart bodies (`415`), and REST session tokens are no longer read from
  a body parameter: see [Browser-facing defaults and reverse
  proxies](#browser-facing-defaults-and-reverse-proxies).
* **Uploads** by clients that send their credentials in the body (the
  upstream SDKs, Workbench) stop at 64 MiB, and every upload at 256 MiB
  (was 1 GiB): see [Request and resource limits](#request-and-resource-limits).
* **Downloads** need credentials, and exports and query results are deleted
  after a week: see [Downloads and exports](#downloads-and-exports).
* **Idle transactions** are rolled back after 10 minutes
  (`txn_idle_timeout`), and **failed logins** lock the account for the
  client's address with a back-off: see [Transactions](#transactions) and
  [Login lockout](#login-lockout).
* **Deleting blank-node triples.** Blank nodes are refused at parse time in
  `DELETE DATA`, `DELETE WHERE` and DELETE templates, so a client deleting
  the upstream way (`DELETE DATA { <a> <p> _:x }`,
  `DELETE WHERE { <a> <p> _:x }`) gets a parse error. The exact replacement
  is `DELETE { <a> <p> ?o } WHERE { <a> <p> ?o FILTER(sameTerm(?o, _:x)) }`
  (`_:x` is the label a query result shows; `FILTER isBlank(?o)` would
  delete the triples of every matching blank node). No switch restores the
  upstream reading: this is a SPARQL 1.1 Update conformance change
  (§3.1.2, §3.1.3) with no security side, pinned on every layout by
  `tests/delete_one_stored_blank_node.rs`; replaying an `update.log`
  written by an earlier version keeps the old reading.
* **Terms loaded by older versions.** Data an older version loaded with a
  malformed language tag or blank-node label (`"x"@zh_CN`, a JSON-LD
  `_:a b`) is stored as it was, but the readers now normalise the tag (`zh-CN`)
  and hash the label, so removing such data by re-reading its source —
  `gsub`, `/api` `batchRemove`, REST `/batchRemove` — does not match it: the
  command succeeds, counting those statements as read but not removed, and
  the legacy terms stay. Remove them by their stored form, e.g.
  `DELETE { ?s ?p ?o } WHERE { ?s ?p ?o FILTER(lang(?o) = "zh_CN") }`
  (`LANG()` returns the stored tag), or export, normalise and rebuild.
  Likewise a database built from RDF/XML, JSON-LD or RDFa with relative
  references under a base (`../x`, `//host/x`, `?q`) keeps the IRIs the
  older readers resolved wrongly until it is rebuilt.
* **`gbuild` exit status `3`** (built, statements skipped) and **`gserver`
  exit status `2`** (invalid arguments): see [Command-line
  tools](#command-line-tools).
* **`gnode cluster`** needs `--state-dir DIR` or `--ephemeral`; **`gnlqa`**
  reads no `gnlqa.conf` from the working directory any more (move it to
  `~/.config/gnlqa/gnlqa.conf` or name it with `GNLQA_CONFIG`), and
  `gnlqa serve` needs `GNLQA_HTTP_TOKEN`.
