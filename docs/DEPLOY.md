# Deploying gstore-rust

This guide covers running `gserver` — the HTTP API server that speaks
upstream gStore 1.4's `/api` protocol (so the upstream SDKs, the Workbench
and `gconsole` work against it) — as a container, as a systemd service, and
from `.deb` / `.rpm` packages; then its configuration, logs and the
command-line tools.

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
`--password`), or change it later with the `/api` `userpassword` operation or
`gconsole`. While root still has the default password, `gserver` refuses to
listen on anything but a loopback address (and warns on loopback).

The legacy **single-database mode** (`gserver DIR`, without `--api`) has no
authentication and no IP lists at all: anyone who reaches it may update the
database. It refuses to listen beyond loopback unless started with
`--allow-unauthenticated`; serve a database to the network through the
multi-database API instead.

**`--allow-service`** lets SPARQL `SERVICE` contact remote endpoints, and
lets users with the update privilege make the server download
`http(s)://` sources (`/api` `build` / `batchInsert` / `batchRemove` with
`remote=true`) and keep what they hold in a database — never from a
loopback, link-local or other host-local address, but from any other host
the server can reach, internal ones included.

## Docker

The `Dockerfile` at the repository root builds a two-stage image: the
crate's MSRV toolchain (`rust:1.89-slim-bookworm`) compiles the release
binaries; the runtime is `debian:bookworm-slim` with the binaries, `curl`
(for the health check) and `ca-certificates`, running as the unprivileged
user `gstore`.

```bash
docker build -t gstore-rust .
# FEATURES defaults to "pfn-wasm" (PFN functions compile and run); add
# others — gzip / tls / zip / readline are pure Rust; rocksdb makes the
# builder install clang for bindgen — or pass FEATURES="" for the lean set
docker build --build-arg FEATURES="pfn-wasm gzip tls zip" -t gstore-rust .

# root's password, read once when the user store is created (the server
# refuses to listen on 0.0.0.0 while root has the default 123456); the file
# must be readable by the container's uid 10001
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
  the `CMD`. Because it listens beyond loopback, the server will not start
  while `root` has the default password: give the first start one through
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
* **Health check.** `curl http://127.0.0.1:9000/api?operation=check` —
  upstream's cheap, unauthenticated `check` operation — every 30 s. If you
  change the port in `conf.ini`, override it (`--health-cmd`).
* **Logs.** With the shipped `conf.ini`, `log_mode` names
  `./conf/slog.properties`, which does not exist: the diagnostic log goes to
  standard error (`docker logs`). Copy `slog.properties.example` to
  `/etc/gstore/slog.properties` to also write `logs/all.log` (rolled daily).
  The query and access logs are files below `/var/lib/gstore/dbhome/logs/`
  (`querylog_path` / `accesslog_path` are relative to `db_home`).

A Compose service:

```yaml
services:
  gstore:
    build: .
    ports: ["9000:9000"]
    volumes:
      - gstore-data:/var/lib/gstore
      - gstore-conf:/etc/gstore
    stop_grace_period: 2m
    restart: unless-stopped
volumes:
  gstore-data:
  gstore-conf:
```

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
* `Restart=on-failure` restarts a server that crashed: leave `conf.ini`'s
  `deamon=off` (its supervisor would duplicate what systemd does).
* `LimitNOFILE=65535`, as upstream raises it.
* Hardening (`ProtectSystem=strict`, `ReadWritePaths=/var/lib/gstore`, …):
  the server writes only below `/var/lib/gstore` — the PFN functions too
  (`pfn_base_path`, by default `<db_home>/pfn`, i.e.
  `/var/lib/gstore/dbhome/pfn`; set it elsewhere and add that directory to
  `ReadWritePaths` with a drop-in). `/etc/gstore` is read-only
  to it — to manage the IP lists with the `ipmanage` operation, point
  `ip_deny_path` / `ip_allow_path` at files below `/var/lib/gstore`.
* `gserver -S -c /etc/gstore/conf.ini` (run as `gstore`, in
  `/var/lib/gstore`) reports the server's status from its pid file
  `dbhome/gserver.pid`.

## Debian / RPM packages

`Cargo.toml` carries [cargo-deb](https://crates.io/crates/cargo-deb) and
[cargo-generate-rpm](https://crates.io/crates/cargo-generate-rpm) metadata
(`[package.metadata.deb]`, `[package.metadata.generate-rpm]`):

```bash
cargo install cargo-deb cargo-generate-rpm

cargo build --release -p gstore          # add --features … as needed
cargo deb -p gstore --no-build           # target/debian/gstore_<version>_<arch>.deb
strip target/release/g{server,console,build,query,add,sub,drop,show,backup,restore,export,monitor,node}
cargo generate-rpm                       # target/generate-rpm/gstore-<version>-1.<arch>.rpm

sudo apt install ./target/debian/gstore_*.deb      # or: sudo dnf install ./target/generate-rpm/gstore-*.rpm
sudo editor /etc/gstore/conf.ini                   # root_password, port, …
sudo systemctl enable --now gserver
```

The packages install:

* the binaries `gserver gconsole gbuild gquery gadd gsub gdrop gshow
  gbackup grestore gexport gmonitor gnode` in `/usr/bin`;
* `conf/conf.ini.example` as `/etc/gstore/conf.ini` (a configuration file:
  your edits survive upgrades — a dpkg conffile, an rpm
  `%config(noreplace)`), readable by the `gstore` group only (`0640`), and
  the log4cplus properties examples next to it;
* the unit as `/lib/systemd/system/gserver.service` (deb) or
  `/usr/lib/systemd/system/gserver.service` (rpm);
* `/usr/lib/sysusers.d/gstore.conf` (the `gstore` system user) and
  `/usr/lib/tmpfiles.d/gstore.conf` (`/var/lib/gstore`, mode `0750`, and its
  `conf → /etc/gstore` link), applied by the post-install script
  (`packaging/debian/postinst`, `packaging/rpm/post_install.sh`), which falls
  back to `adduser`/`useradd` without systemd;
* `DEPLOY.md` and `README.md` in `/usr/share/doc/gstore/`.

The service is not enabled or started on install; an upgrade restarts a
running server (`try-restart`). Removing the package stops and disables
the service first (a graceful stop: unsaved changes are saved). The
databases, their reasoning rules and the PFN functions in `/var/lib/gstore`
are never removed by the package, not even on purge.

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
holds those triples as asserted ones, owned by no rule).

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

**Query evaluation** (`[query]`, a gstore-rust section upstream has no keys
for): `integer_division` (`decimal`, SPARQL 1.1's `7/2` = `3.5`, or
`truncate`, upstream's `3`), `query_cache_size` (each database's read-query
cache budget in bytes, `0` = off, default 64 MiB), `early_termination`,
`topk_bounded_heap`, `topk_dpb` (Top-K evaluation of `LIMIT` / `ORDER BY …
LIMIT`) and `gfun_max_output_bytes` (the most JSON one graph-function call
may produce, default 64 MiB). They apply to every database the server
loads, builds, creates or restores, and `refreshconf` re-applies them to
the loaded ones. The graph functions (`shortestPath`, `PR`, `louvain`, …)
run on a per-database CSR built on the first such query after a change;
`/api` `loadcsr` (or `load` with `csr=1`) builds it ahead of time, and
`show` reports whether it is built (`csr`). Any write discards it.

**PFN functions** (`[pfn]`, a gstore-rust section; see
[`docs/PFN.md`](PFN.md)): `pfn_fuel_per_call` (instructions per call,
default 10^9, `0` = unlimited), `pfn_max_memory_pages` (64 KiB pages,
default 1024), `pfn_max_module_bytes` (default 1 MiB) and
`pfn_max_functions` (per user, default 100) bound the sandbox; like
`[query]`, `refreshconf` re-applies them. `[system] pfn_base_path`
(upstream's key; default `<db_home>/pfn`) is where the functions are kept —
read at startup only, so moving them needs a restart (and the files moved
along, see [Backups](#backups)). Compiling and calling functions needs a
build with the `pfn-wasm` feature (the container image has it, see
[Docker](#docker)); without it they can still be listed, created, updated
and deleted. Reasoning rules (`reasonManage`) have no keys.

**Zip packages** (a build with the `zip` feature): `build`, `batchInsert`
and `batchRemove` read a `.zip` of RDF files (each entry needs an extension
from `upload_allow_extensions`), `backup
compress=true` writes `<db>.db_<timestamp>.zip`, `restore` takes such an
archive, and `export compress=true` zips the export file. A package is
checked in full before anything is unpacked: unsafe entry names (absolute,
`..`) are refused, and `[upload] unzip_max_entries` (default 10000) and
`unzip_max_size` (default 10 GiB, counted on the bytes actually written)
bound what it may unpack to — all the packages of one request together; a
package named twice in a request is unpacked once. Without the feature
these requests answer `1003`, naming it.

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
  `<querylog_path>/YYYYMMDD.log`. The `/api` `query` operation records the
  statements that ran (a statement that failed is not recorded, as
  upstream's); the REST endpoints record failed ones too.
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
```

`grestore` takes the database first and the backup second, as upstream
(`-db`, `-p`); earlier gstore-rust releases took them the other way round.
Every tool answers `-h`/`--help`; the exit status is `0` on success, `1`
when the operation failed and `2` for invalid arguments.
