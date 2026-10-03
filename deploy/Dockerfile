# syntax=docker/dockerfile:1

# ── builder ────────────────────────────────────────────────────────────────
# Compiles the `opencompany` host binary. `FEATURES` selects optional cargo
# features (e.g. "medulla sqlite"); empty = the small default build.
FROM docker.io/library/rust:1-slim-bookworm AS builder
ARG FEATURES=""
# The revision this image is built from, read by `build.rs` (see
# `src/build_stamp.rs`). It has to be passed in here: `deploy/Dockerfile.dockerignore` excludes
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
       pkg-config ca-certificates curl libssl-dev \
       libx11-dev libxi-dev libxtst-dev libxrandr-dev libxcb1-dev libxkbcommon-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy only inputs read by the Rust build, rather than the whole checkout.
# Keeping frontend, docs, scripts, and deployment files out of this layer lets
# their changes reuse the compiled Cargo cache.
#
# `build.rs` embeds the shipped company agents and `src/desktop.rs` (both under
# `crates/opencompany-core/`) embeds each
# preset manifest, so the complete companies tree remains a real build input;
# it also embeds the global baseline (`companies/_globals/`, its skills
# included), which a hosted tenant has no checkout to read from disk.
# `vendor/` backs the path dependencies and Cargo patch table.
#
# The root `Cargo.toml` is a virtual workspace whose members are the manifests
# under `crates/`. Cargo refuses to load a workspace with a listed member
# missing, so the tui crate is copied even though only the host binary is
# built. The host crate — manifest, `build.rs`, `src/`, `tests/`, `benches/`,
# `examples/` — lives under `crates/opencompany-core/`.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/opencompany-core ./crates/opencompany-core
COPY crates/opencompany-tui ./crates/opencompany-tui
COPY vendor ./vendor
COPY companies ./companies
COPY scripts/upload-sentry-symbols.sh ./scripts/upload-sentry-symbols.sh

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    set -eux; \
    if [ -n "$FEATURES" ]; then \
      cargo build --release -p opencompany-core --bin opencompany --features "$FEATURES"; \
    else \
      cargo build --release -p opencompany-core --bin opencompany; \
    fi; \
    install -Dm755 target/release/opencompany /out/opencompany; \
    mkdir -p /out/modules; \
    case ",$FEATURES," in \
      *,openhuman,*) \
        CARGO_TARGET_DIR=/build/target/tinysearch \
          cargo build --release --lib \
            --manifest-path vendor/openhuman/vendor/tinysearch/crates/tinysearch/Cargo.toml; \
        install -Dm644 /build/target/tinysearch/release/libtinysearch.so \
          /out/modules/libtinysearch.so; \
        ;; \
    esac

# Upload symbols from the shared target cache. The token is a BuildKit secret,
# never a build argument, image layer, or final-image environment value.
ARG SENTRY_URL=""
ARG SENTRY_ORG=""
ARG SENTRY_PROJECT_RUST=""
RUN --mount=type=cache,target=/build/target \
    --mount=type=secret,id=sentry_auth_token \
    set -eu; \
    if [ -s /run/secrets/sentry_auth_token ] && [ -n "$SENTRY_URL" ] && [ -n "$SENTRY_ORG" ] && [ -n "$SENTRY_PROJECT_RUST" ]; then \
      curl --fail --silent --show-error --location \
        https://github.com/getsentry/sentry-cli/releases/download/3.4.1/sentry-cli-Linux-x86_64 \
        --output /usr/local/bin/sentry-cli; \
      chmod +x /usr/local/bin/sentry-cli; \
      SENTRY_AUTH_TOKEN="$(cat /run/secrets/sentry_auth_token)" \
      SENTRY_PROJECT="$SENTRY_PROJECT_RUST" \
        scripts/upload-sentry-symbols.sh target/release; \
      rm /usr/local/bin/sentry-cli; \
    fi

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
FROM docker.io/library/node:22-slim AS console
WORKDIR /console

# `node:22-slim` ships no CA bundle. Node itself carries its own roots, but the
# native `sentry-cli` that `@sentry/vite-plugin` runs during `npm run build`
# uses the system store, and without it every call (`releases new`, sourcemap
# upload) dies with "unable to get local issuer certificate" and fails the build.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Console crash reporting (`docs/spec/runtime/crash-reporting.md`).
#
# These are read by Vite at BUILD time and inlined into the bundle, so unlike
# the host's `OPENCOMPANY_SENTRY_DSN` they cannot be injected into the running
# container: by the time k8s sets env, the bundle is already built. Without
# them there is no path for a console DSN to reach a hosted tenant at all, and
# setting one on the StatefulSet is a silent no-op.
#
# Both default to empty. An empty `VITE_SENTRY_DSN` makes the production bundle
# report to the compiled-in TinyHumans console project
# (`DEFAULT_CONSOLE_SENTRY_DSN`); pass `VITE_SENTRY_DSN=off` for a silent one
# (`deploy/docker-compose.yml` does). `sentryRelease()` falls back to the bare
# version without a commit.
#
# `VITE_SENTRY_ENVIRONMENT` has to be passed for the same reason, and is easy to
# miss because both surfaces default to something plausible and different: the
# host falls back to its deployment kind (`hosted-tenant`), a browser bundle
# cannot know that and falls back to `production`. Left unset, one tenant's
# reports split across two `environment` values and no filter shows both. The
# manager's `OPENCOMPANY_SENTRY_ENVIRONMENT` is the value to match.
#
# `VITE_SENTRY_TRACES_SAMPLE_RATE` is here for the same build-time reason, and
# is deliberately given NO default: absent means `0`, which installs no tracing
# integration at all. That is the shipped choice, not an oversight — errors and
# transactions bill separately, and a transaction is emitted per page load
# rather than only when something breaks, so a rate chosen on an operator's
# behalf is a recurring bill they did not ask for.
#
# `VITE_BUILD_COMMIT` is the frontend's spelling of the builder stage's
# `OPENCOMPANY_BUILD_COMMIT`, not a second source of truth — `vite.config.ts`
# shortens it to the same twelve characters `src/build_stamp.rs` normalizes to,
# so both surfaces compute one `opencompany@<version>+<commit>` release string.
# Passed the same value, a console event and a host event from this image line
# up in one filter; passed different ones, they silently never join.
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci

# Declared AFTER `npm ci` on purpose. `ENV` invalidates every layer beneath it
# and `VITE_BUILD_COMMIT` changes on every commit, so hoisting these above the
# install would re-run a full `npm ci` for each build — the same cache argument
# the builder stage makes for `OPENCOMPANY_BUILD_COMMIT`, which is cheap there
# only because the cargo `target` mount survives it. Nothing survives a lost
# `node_modules`.
ARG VITE_SENTRY_DSN=""
ARG VITE_BUILD_COMMIT=""
ARG VITE_SENTRY_ENVIRONMENT=""
ARG VITE_SENTRY_TRACES_SAMPLE_RATE=""
ARG SENTRY_URL=""
ARG SENTRY_ORG=""
ARG SENTRY_PROJECT_REACT=""
ENV VITE_SENTRY_DSN=$VITE_SENTRY_DSN \
    VITE_BUILD_COMMIT=$VITE_BUILD_COMMIT \
    VITE_SENTRY_ENVIRONMENT=$VITE_SENTRY_ENVIRONMENT \
    VITE_SENTRY_TRACES_SAMPLE_RATE=$VITE_SENTRY_TRACES_SAMPLE_RATE

COPY frontend/ ./
RUN --mount=type=secret,id=sentry_auth_token \
    set -eu; \
    if [ -s /run/secrets/sentry_auth_token ]; then \
      export SENTRY_AUTH_TOKEN="$(cat /run/secrets/sentry_auth_token)"; \
    fi; \
    export SENTRY_PROJECT="$SENTRY_PROJECT_REACT"; \
    npm run build
RUN npm run build:pages-sdk

# ── local development ─────────────────────────────────────────────────────
# Used by deploy/docker-compose.dev.yml. The repository is bind-mounted over
# /workspace; cargo-watch rebuilds and restarts the host after local edits.
FROM docker.io/library/rust:1-slim-bookworm AS development
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && cargo install cargo-watch --locked
WORKDIR /workspace
ENV OPENCOMPANY_BIND=0.0.0.0:8080 \
    OPENCOMPANY_DATA_DIR=/data

# ── runtime ────────────────────────────────────────────────────────────────
FROM docker.io/library/debian:bookworm-slim AS runtime
# libssl3 + the X11 runtime shared libraries satisfy the dynamic linker for the
# openssl-sys/native-tls and rdev/arboard links the `openhuman` feature pulls in
# (the desktop-automation code paths never run in a headless tenant, but the .so
# files must be present to load the binary). No-ops for the small builds.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
       ca-certificates curl git libssl3 \
       libx11-6 libxi6 libxtst6 libxrandr2 libxcb1 libxkbcommon0 \
    && git --version \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app

COPY --from=builder /out/opencompany /usr/local/bin/opencompany
# The company definitions the switch chooses from at runtime. They are also
# the skill registry: the host derives `skills_root` from the loaded company's
# directory (`companies/<name>` → `companies/`) and lists every bundle's
# `skills/`, so the console's registry tab and server-authoritative installs
# come from this tree.
COPY companies ./companies
# The built operator console, served at `/` by the host. World-readable so the
# unprivileged runtime user (uid 10001 under the platform's securityContext)
# can read it even with a read-only root filesystem.
COPY --from=console /console/dist /app/console
# The TinySearch module: the native library `search_byo` loads to run a search
# on a company's **own** provider key (`search/provider/<slug>/key` in the
# secret store). The engines moved into this module in OpenHuman v0.64.10, so
# reaching them is the only way to honour a BYO key — and the key is delivered
# to the module as private configuration, never written to disk.
#
# Built from `vendor/`, not downloaded from the pinned release, for three
# reasons. It is the same revision this binary's `tinysearch-bus` was compiled
# against, so the configuration sent and the configuration read cannot drift.
# The runtime user is unprivileged against a read-only root filesystem and may
# have no writable cache directory for the installer to extract into. And an API
# server fetching a library over the network on a tenant's first search is
# egress nobody asked for.
#
# The directory is created unconditionally by the builder, so a build without
# the `openhuman` feature copies it empty; `OPENCOMPANY_TINYSEARCH_MODULE` then
# names a file that is not there, which the host reports and falls back from
# rather than failing to start.
COPY --from=builder /out/modules/ /app/modules/
ENV OPENCOMPANY_TINYSEARCH_MODULE=/app/modules/libtinysearch.so
COPY deploy/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh && mkdir -p /data

# Self-hosted containers default to the marketing-agency example. The platform
# explicitly injects an empty value for a new tenant so it reaches the
# first-run setup wizard; that routable path is authorized by its SSO bootstrap
# token, not opened to anonymous visitors.
# `OPENCOMPANY_CONSOLE_DIR` points the host at the baked console bundle so a
# hosted tenant serves its own UI at `/` instead of 404ing.
ENV OPENCOMPANY_COMPANY="marketing_agency" \
    OPENCOMPANY_BIND=0.0.0.0:8080 \
    OPENCOMPANY_DATA_DIR=/data \
    OPENCOMPANY_CONSOLE_DIR=/app/console

EXPOSE 8080
HEALTHCHECK --interval=15s --timeout=3s --start-period=8s --retries=5 \
  CMD curl -fsS http://localhost:8080/healthz || exit 1
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
