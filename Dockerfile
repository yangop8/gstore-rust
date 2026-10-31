# syntax=docker/dockerfile:1
#
# gstore-rust container image: gserver (the HTTP API server, upstream gStore
# 1.4's /api protocol on port 9000) plus the command-line tools.
#
#   docker build -t gstore-rust .
#   docker run -d --name gstore -p 9000:9000 \
#       -v gstore-data:/var/lib/gstore -v gstore-conf:/etc/gstore \
#       --stop-timeout 120 gstore-rust
#   curl 'http://127.0.0.1:9000/api?operation=check'
#
# Optional cargo features: --build-arg FEATURES="gzip tls zip" (pure Rust);
# "rocksdb" also works (the builder then installs clang for bindgen).
# See docs/DEPLOY.md.

# ---- builder ------------------------------------------------------------------
# The toolchain is the crate's MSRV (`rust-version` in Cargo.toml).
ARG RUST_VERSION=1.89
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

ARG FEATURES=""
WORKDIR /src

RUN if echo " ${FEATURES} " | grep -q " rocksdb "; then \
        apt-get update \
        && apt-get install -y --no-install-recommends clang libclang-dev \
        && rm -rf /var/lib/apt/lists/*; \
    fi

# The workspace (gstore + the gnlqa member, which cargo needs to resolve the
# workspace even though only gstore is built). .dockerignore keeps target/,
# tests and test data out of the context.
COPY Cargo.toml Cargo.lock ./
COPY gnlqa ./gnlqa
COPY src ./src
COPY conf ./conf

# Build the release binaries with cached registry and target directories, and
# copy them out of the cache mount in the same step.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p gstore --bins ${FEATURES:+--features "$FEATURES"} \
    && mkdir -p /out \
    && for b in gserver gconsole gbuild gquery gadd gsub gdrop gshow gbackup grestore gexport gmonitor gnode; do \
         cp "target/release/$b" /out/; \
       done

# ---- runtime ------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="gstore-rust" \
      org.opencontainers.image.description="gStore RDF graph database (Rust rewrite): gserver + CLI tools" \
      org.opencontainers.image.source="https://github.com/yangop8/gstore-rust" \
      org.opencontainers.image.licenses="BSD-3-Clause"

# curl for the health check; ca-certificates for https sources (LOAD, gbuild URLs).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# A dedicated unprivileged user owns the data.
RUN groupadd --system --gid 10001 gstore \
    && useradd --system --uid 10001 --gid gstore --home-dir /var/lib/gstore \
         --no-create-home --shell /usr/sbin/nologin gstore

COPY --from=builder /out/ /usr/local/bin/

# Configuration in /etc/gstore (conf.ini from the shipped example, the log4cplus
# properties examples); data in /var/lib/gstore, the working directory: every
# relative conf.ini path (dbhome/, backups/, logs/, upload/, ./conf/...) lives
# below it, as upstream's install root. /var/lib/gstore/conf points at
# /etc/gstore so conf.ini's ./conf/... paths (log_mode, ip_deny_path) resolve.
COPY conf/conf.ini.example /etc/gstore/conf.ini
COPY conf/slog.properties.example conf/slog.stdout.properties.example /etc/gstore/
RUN mkdir -p /var/lib/gstore \
    && ln -s /etc/gstore /var/lib/gstore/conf \
    && chown -R gstore:gstore /var/lib/gstore /etc/gstore \
    && chmod 0640 /etc/gstore/conf.ini

USER gstore
WORKDIR /var/lib/gstore
VOLUME ["/var/lib/gstore", "/etc/gstore"]

# Upstream's default port (conf.ini [server] port).
EXPOSE 9000

# A cheap, unauthenticated /api operation (upstream's `check`). Adjust the port
# if conf.ini changes it.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS 'http://127.0.0.1:9000/api?operation=check' | grep -q '"StatusCode":0' || exit 1

# gserver in the foreground (-s); SIGTERM (docker stop) shuts it down
# gracefully: in-flight requests finish, open transactions roll back,
# unsaved changes are saved. The listener binds every interface inside the
# container (conf.ini's listen_addr defaults to 127.0.0.1), so gserver will not
# start while root has the default password 123456: give the first start one
# with GSTORE_ROOT_PASSWORD_FILE (a mounted file or Docker secret; see
# docs/DEPLOY.md) or conf.ini's root_password. Its diagnostic log
# goes to standard error (docker logs) unless conf.ini / slog.properties say
# otherwise.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/gserver"]
CMD ["-s", "-c", "/etc/gstore/conf.ini", "--addr", "0.0.0.0"]
