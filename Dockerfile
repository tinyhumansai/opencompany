# syntax=docker/dockerfile:1

# ── builder ────────────────────────────────────────────────────────────────
# Compiles the `opencompany` host binary. `FEATURES` selects optional cargo
# features (e.g. "medulla tinyplace sqlite"); empty = the small default build.
FROM rust:1-slim-bookworm AS builder
ARG FEATURES=""
WORKDIR /build

# The whole workspace is copied (examples/*/Cargo.toml load the workspace;
# vendor/tinyagents backs the [patch.crates-io] entry). vendor/openhuman,
# target/, and node_modules are excluded via .dockerignore.
COPY . .

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
FROM node:22-slim AS console
WORKDIR /console
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build

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
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

COPY --from=builder /out/opencompany /usr/local/bin/opencompany
# The company definitions the switch chooses from at runtime.
COPY companies ./companies
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
