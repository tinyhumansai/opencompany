# syntax=docker/dockerfile:1

# ── builder ────────────────────────────────────────────────────────────────
# Compiles the `opencompany` host binary. `FEATURES` selects optional cargo
# features (e.g. "medulla tinyplace sqlite"); empty = the small default build.
FROM rust:1-slim-bookworm AS builder
ARG FEATURES=""
# The revision this image is built from, read by `build.rs` (see
# `src/build_stamp.rs`). It has to be passed in here: `.dockerignore` excludes
# `.git`, so the build script has no repository to ask, and `GITHUB_SHA` does
# not cross into a build context either. Left unset, the binary stamps the
# honest string `unknown` — which is what a hosted tenant would otherwise
# report forever, since `version` has read 0.1.0 for thousands of commits.
#
#   docker build --build-arg OPENCOMPANY_BUILD_COMMIT="$(git rev-parse --short=12 HEAD)" .
#
# A changing value invalidates the layer below, but not the compile: the cargo
# `target` cache mount survives it, so only the final crate is rebuilt.
ARG OPENCOMPANY_BUILD_COMMIT=""
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
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh && mkdir -p /data

# The switch: which example company this container runs. Override at deploy time.
# `OPENCOMPANY_CONSOLE_DIR` points the host at the baked console bundle so a
# hosted tenant serves its own UI at `/` instead of 404ing.
ENV OPENCOMPANY_COMPANY=agentic_marketing_agency \
    OPENCOMPANY_BIND=0.0.0.0:8080 \
    OPENCOMPANY_DATA_DIR=/data \
    OPENCOMPANY_DISCOVERABLE=false \
    OPENCOMPANY_CONSOLE_DIR=/app/console

EXPOSE 8080
HEALTHCHECK --interval=15s --timeout=3s --start-period=8s --retries=5 \
  CMD curl -fsS http://localhost:8080/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
