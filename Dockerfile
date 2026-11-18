# syntax=docker/dockerfile:1
#
# gstore-rust container image: gserver (the HTTP API server, upstream gStore
# 1.4's /api protocol on port 9000) plus the command-line tools.
#
#   docker build -t gstore-rust .
#   printf '%s\n' 'a-long-random-password' > gstore_root   # root's password
#   docker run -d --name gstore -p 9000:9000 \
#       -v gstore-data:/var/lib/gstore -v gstore-conf:/etc/gstore \
#       -v "$PWD/gstore_root:/run/secrets/gstore_root:ro" \
#       -e GSTORE_ROOT_PASSWORD_FILE=/run/secrets/gstore_root \
#       --stop-timeout 120 gstore-rust
#   curl 'http://127.0.0.1:9000/api?operation=check'
#
# Cargo features: FEATURES defaults to "pfn-wasm" -- the sandboxed
# WebAssembly runtime of PFN user functions (pure Rust: wasmi + wat), so the
# image compiles and calls them; the default `cargo build` stays without it.
# The deb/rpm packages are built with the same feature (Cargo.toml's package
# metadata). Add others with --build-arg FEATURES="pfn-wasm gzip tls zip
# readline" (all pure Rust; readline only changes gconsole); "rocksdb" also
# works (the builder then installs clang for bindgen). FEATURES="" builds the
# lean set. See docs/DEPLOY.md.
#
# .github/workflows/packaging.yml builds this image and runs
# scripts/package-smoke.sh against it (review infra#7).

# ---- builder ------------------------------------------------------------------
# The toolchain is the crate's MSRV (`rust-version` in Cargo.toml; every
# workspace member declares the same, tests/packaging_layout.rs checks it), so
# the image build is also the MSRV build.
ARG RUST_VERSION=1.89
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

ARG FEATURES="pfn-wasm"
WORKDIR /src

RUN if echo " ${FEATURES} " | grep -q " rocksdb "; then \
        apt-get update \
        && apt-get install -y --no-install-recommends clang libclang-dev \
        && rm -rf /var/lib/apt/lists/*; \
    fi

# The workspace (gstore + the gnlqa and bench members, which cargo needs to
# resolve the workspace even though only gstore is built). .dockerignore keeps
# target/, tests and test data out of the context.
COPY Cargo.toml Cargo.lock ./
COPY gnlqa ./gnlqa
COPY bench ./bench
COPY src ./src
COPY conf ./conf

# Build and install every binary target of the gstore package (whatever
# [[bin]] targets Cargo.toml has: no list to keep in step, review infra#7)
# into /out/bin, with cached registry and target directories.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo install --path . --locked --root /out --target-dir /src/target \
        ${FEATURES:+--features "$FEATURES"} \
    && rm -f /out/.crates.toml /out/.crates2.json

# ---- runtime ------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="gstore-rust" \
      org.opencontainers.image.description="gStore RDF graph database (Rust rewrite): gserver + CLI tools" \
      org.opencontainers.image.source="https://github.com/yangop8/gstore-rust" \
      org.opencontainers.image.licenses="BSD-3-Clause"

# ca-certificates for https sources (LOAD, gbuild URLs, SERVICE).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# A dedicated unprivileged user owns the data.
RUN groupadd --system --gid 10001 gstore \
    && useradd --system --uid 10001 --gid gstore --home-dir /var/lib/gstore \
         --no-create-home --shell /usr/sbin/nologin gstore

COPY --from=builder /out/bin/ /usr/local/bin/
COPY --chmod=0755 packaging/docker/gstore-healthcheck /usr/local/bin/gstore-healthcheck

# Configuration in /etc/gstore (conf.ini from the shipped example, the log4cplus
# properties examples); data in /var/lib/gstore, the working directory: every
# relative conf.ini path (dbhome/, backups/, logs/, upload/, ./conf/...) lives
# below it, as upstream's install root -- the users' PFN functions too
# (pfn_base_path, by default <db_home>/pfn = /var/lib/gstore/dbhome/pfn), so
# the data volume keeps them. /var/lib/gstore/conf points at
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

# Healthy while gserver runs and accepts connections on the address it
# listens on, whatever port or scheme conf.ini and the command line chose
# (packaging/docker/gstore-healthcheck: `gserver -S` with the server's own
# arguments). The probe sends no request: it is not logged as client traffic
# and the IP lists do not apply to it (review infra#17).
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/gstore-healthcheck"]

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
