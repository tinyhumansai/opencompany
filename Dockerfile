# syntax=docker/dockerfile:1

# ── builder ────────────────────────────────────────────────────────────────
# Compiles the `opencompany` host binary. `FEATURES` selects optional cargo
# features (e.g. "medulla tinyplace sqlite"); empty = the small default build.
FROM rust:1-slim-bookworm AS builder
ARG FEATURES=""
WORKDIR /build

# `--features openhuman` transitively needs system libraries at build time that
# the small default/mongodb builds don't:
#   * OpenSSL headers — native-tls → openssl-sys (via motosan-ai-oauth →
#     hyper-tls in the vendored openhuman tree).
#   * X11 dev libs — openhuman's unconditional `rdev`/`arboard` deps (input +
#     clipboard) link against libX11/libXi/libXtst/libxcb/libxkbcommon even in
#     a headless server build; the matching .so runtime libs are added to the
#     runtime stage so the dynamic linker is satisfied (the code paths never
#     run here). These are no-ops for the default/mongodb builds.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       pkg-config ca-certificates libssl-dev \
       libx11-dev libxi-dev libxtst-dev libxrandr-dev libxcb1-dev libxkbcommon-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy only inputs read by the Rust build, rather than the whole checkout.
# Keeping frontend, docs, scripts, and deployment files out of this layer lets
# their changes reuse the compiled Cargo cache.
#
# `build.rs` embeds the shipped company agents and `src/desktop.rs` embeds each
# preset manifest, so the complete companies tree remains a real build input.
# `vendor/` backs the path dependencies and Cargo patch table.
COPY Cargo.toml Cargo.lock rust-toolchain.toml build.rs ./
COPY src ./src
COPY benches ./benches
COPY tests ./tests
COPY examples ./examples
COPY vendor ./vendor
COPY companies ./companies

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    set -eux; \
    if [ -n "$FEATURES" ]; then \
      cargo build --release --bin opencompany --features "$FEATURES"; \
    else \
      cargo build --release --bin opencompany; \
    fi; \
    install -Dm755 target/release/opencompany /out/opencompany

# ── console builder ────────────────────────────────────────────────────────
# Builds the Vite/React operator console (frontend/) into a static bundle that
# the runtime image serves at `/` (same-origin, no separate nginx container).
# Matches the node version frontend/Dockerfile pins.
#
# `build:pages-sdk` builds `@opencompany/site` — the component subset +
# postMessage client agent-authored dashboard pages import — into
# `dist/pages-sdk/`, nested inside the same `dist/` the console itself builds
# to, so the single `COPY --from=console /console/dist /app/console` below
# already carries it into the runtime image with no second COPY needed.
FROM node:22-slim AS console
WORKDIR /console
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build && npm run build:pages-sdk

# ── local development ─────────────────────────────────────────────────────
# Used by docker-compose.dev.yml. The repository is bind-mounted over
# /workspace; cargo-watch rebuilds and restarts the host after local edits.
FROM rust:1-slim-bookworm AS development
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && cargo install cargo-watch --locked
WORKDIR /workspace
ENV OPENCOMPANY_BIND=0.0.0.0:8080 \
    OPENCOMPANY_DATA_DIR=/data

# ── runtime ────────────────────────────────────────────────────────────────
# The TinyMemory loadable module, fetched from its pinned release and digest-
# verified in this stage so the runtime image needs neither curl nor network.
# Baked rather than downloaded at boot for three measured reasons (issue
# #1524): the tenant pod runs uid 10001 on a read-only root filesystem, the
# PVC mount is fsGroup-writable (tinybus refuses to dlopen under a
# group-writable ancestor), and a rollback must never depend on network state.
#
# The platform bucket is ubuntu-22.04: the runtime below is bookworm-slim
# (glibc 2.36), inside the 22.04 archive's >=2.35 <2.39 window — CI runners
# are ~24.04 and reaching for that bucket is a symbol-version dlopen failure
# at first memory call, not at build. VERSION, PLATFORM and SHA256 move
# together or the digest check fails the build; take SHA256 verbatim from the
# release's checksum.toml, never from a local build.
FROM debian:bookworm-slim AS module
ARG TINYMEMORY_MODULE_VERSION=1.1.0
ARG TINYMEMORY_MODULE_PLATFORM=ubuntu-22.04-x86_64
ARG TINYMEMORY_MODULE_SHA256=66bd6c0e138e4af9c6819992bd27f66f4f49539295f7baa60603e3ee87090aa1
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN set -eu; \
    archive="tinymemory-module-${TINYMEMORY_MODULE_VERSION}-${TINYMEMORY_MODULE_PLATFORM}.tar.gz"; \
    curl -fsSL -o "/tmp/${archive}" \
      "https://github.com/tinyhumansai/tinymemory/releases/download/v${TINYMEMORY_MODULE_VERSION}/${archive}"; \
    echo "${TINYMEMORY_MODULE_SHA256}  /tmp/${archive}" | sha256sum -c -; \
    mkdir -p /module; \
    tar -xzf "/tmp/${archive}" -C /module; \
    test -f /module/modules.toml

FROM debian:bookworm-slim AS runtime
# libssl3 + the X11 runtime shared libraries satisfy the dynamic linker for the
# openssl-sys/native-tls and rdev/arboard links the `openhuman` feature pulls in
# (the desktop-automation code paths never run in a headless tenant, but the .so
# files must be present to load the binary). No-ops for the small builds.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates curl libssl3 \
       libx11-6 libxi6 libxtst6 libxrandr2 libxcb1 libxkbcommon0 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

COPY --from=builder /out/opencompany /usr/local/bin/opencompany
# The company definitions the switch chooses from at runtime.
COPY companies ./companies
# The shared skill library. The host derives `skills_root` from the loaded
# company's directory (`companies/<name>` → `<repo>/skills`), so without this
# the registry resolves to a missing directory and serves nothing: the console's
# registry tab would be empty and installs would fall back to client metadata.
COPY skills ./skills
# The built operator console, served at `/` by the host. World-readable so the
# unprivileged runtime user (uid 10001 under the platform's securityContext)
# can read it even with a read-only root filesystem.
COPY --from=console /console/dist /app/console
# The TinyMemory module and its digest allowlist, side by side — tinybus reads
# `modules.toml` from the library's own directory at attach time. root:root
# 0755 the whole chain: tinybus refuses any group/other-writable ancestor, and
# /app under the platform's securityContext satisfies its walk where /data
# (fsGroup-mounted) never could.
COPY --from=module /module /app/modules
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh && mkdir -p /data

# The switch: which example company this container runs. Override at deploy time.
# `OPENCOMPANY_CONSOLE_DIR` points the host at the baked console bundle so a
# hosted tenant serves its own UI at `/` instead of 404ing.
ENV OPENCOMPANY_COMPANY=agentic_marketing_agency \
    OPENCOMPANY_BIND=0.0.0.0:8080 \
    OPENCOMPANY_DATA_DIR=/data \
    OPENCOMPANY_DISCOVERABLE=false \
    OPENCOMPANY_CONSOLE_DIR=/app/console \
    OPENCOMPANY_MEMORY_MODULE_PATH=/app/modules/libtinymemory_module.so

EXPOSE 8080
HEALTHCHECK --interval=15s --timeout=3s --start-period=8s --retries=5 \
  CMD curl -fsS http://localhost:8080/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
