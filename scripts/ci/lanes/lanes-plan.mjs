// The lane plan behind ci-fast.yml / ci-fast-hosted.yml (via ci-lanes.yml).
//
// Pure data: `buildPlan()` turns a profile and the changed-area flags into
// lanes of named checks; `lanes.mjs` executes them. Keeping the plan pure lets
// lanes.test.mjs pin its shape without running a single cargo command. Ported
// from OpenHuman's scripts/ci/self-hosted/lanes-plan.mjs (its #6537 CI Fast).
//
// Every check here is a step the old monolithic ci.yml ran, moved as-is. The
// rules that hold for all of them:
//
//   * An area flag decides whether a WHOLE suite runs. No command is ever
//     narrowed to changed files: commands are static strings that never
//     interpolate the diff (the self-test asserts it). OpenHuman tried
//     changed-file selection (#4486) and removed it (#6349) after it let
//     regressions in related code through.
//   * Every check in a lane runs even when an earlier one failed, and the lane
//     still fails. A red `fmt` used to hide every later step as "skipped"
//     (OpenHuman #6432, #6506). `needs` names only real data dependencies;
//     `after` orders without requiring success.
//   * scripts/ci/assert-feature-lanes.sh reads THIS file (plus ci-lanes.yml)
//     for the `cargo test --features X` / `run-scoped-suite.sh` lines that
//     prove a feature has a lane, so keep each command a literal string.

/** Changed-area flags, as exported by the workflow from dorny/paths-filter. */
export const AREA_ENV = {
  rust: "CI_AREA_RUST",
  frontend: "CI_AREA_FRONTEND",
  desktop: "CI_AREA_DESKTOP",
};

/** Read the area flags from an environment object. Missing means false. */
export function areasFromEnv(env) {
  const areas = {};
  for (const [key, name] of Object.entries(AREA_ENV)) {
    areas[key] = env[name] === "true";
  }
  return areas;
}

/**
 * Hosted-runner job groups: lanes that share one GitHub-hosted job. Each group
 * is one runner with its own `target/` and its own rust-cache entry, so two
 * lanes in a group must not compile different feature sets side by side —
 * `core` and `e2e` share one because `e2e` compiles nothing.
 */
export const HOSTED_GROUPS = [
  // Static checks and the console suite: no heavy compile, fit side by side.
  { group: "checks", lanes: ["static", "console"], maxParallel: 2 },
  // The default-feature graph, then the e2e suites against its binary. e2e
  // starts as soon as the binary is built and overlaps the test runs.
  { group: "core", lanes: ["core", "e2e"], maxParallel: 2 },
  // The vendored OpenHuman graph (the long pole), and the live-brain e2e
  // against the gated binary it builds first.
  { group: "gated", lanes: ["gated", "e2e-live"], maxParallel: 2 },
  { group: "desktop", lanes: ["desktop"], maxParallel: 1 },
];

/**
 * Build the lane plan.
 *
 * @param {object} opts
 * @param {"ex63"|"hosted"} opts.profile
 * @param {Record<string, boolean>} opts.areas  from areasFromEnv()
 * @param {object} [opts.env]  process environment (profile paths only)
 * @returns {{profile: string, lanes: Lane[]}}
 */
export function buildPlan({ profile, areas, env = {} }) {
  if (profile !== "ex63" && profile !== "hosted") {
    throw new Error(`unknown profile "${profile}" (expected ex63 or hosted)`);
  }
  const ex63 = profile === "ex63";
  const scratch = env.CI_SCRATCH_DIR;
  if (ex63 && !scratch) {
    throw new Error(
      "profile ex63 needs CI_SCRATCH_DIR (set by the microVM guest)",
    );
  }
  const rust = areas.rust;
  // The e2e lanes and the desktop package need binaries and a console build
  // whenever either side of them moved.
  const app = areas.rust || areas.frontend;
  const desktop = app || areas.desktop;

  // ex63: one throwaway target dir per lane, so lanes never queue on cargo's
  // build-dir lock; sccache warms each from the host's shared, capped store
  // (its keys include the target dir, so a lane shares with the same lane of
  // earlier jobs, not with the other lanes). hosted: cargo's default target
  // dir, which Swatinem/rust-cache restores per group.
  const targetDir = (lane) => (ex63 ? `${scratch}/target/${lane}` : null);
  const rustEnv = ex63 ? { RUSTC_WRAPPER: "sccache" } : {};
  // The guest's persistent cache disk: npm's cache and Playwright's browsers
  // survive from job to job there. Env values are expanded by lanes.mjs
  // (`${NAME}`, `${NAME:-default}`, and `${ROOT}` for the checkout root),
  // because a spawned process's environment is never shell-expanded. Hosted runners use actions/setup-node's
  // npm cache and download Chromium per job.
  const cacheDisk = "${CI_CACHE_DIR:-/cache}";
  const nodeEnv = ex63
    ? {
        npm_config_cache: `${cacheDisk}/npm`,
        PLAYWRIGHT_BROWSERS_PATH: `${cacheDisk}/ms-playwright`,
      }
    : {};
  // Binaries are copied out of the target dir as soon as they are built, so a
  // later cargo command in the same lane (which rewrites target/debug/
  // opencompany with other features) cannot change what e2e runs.
  const bin = "ci-out/bin";
  const copyBin = (name) =>
    `mkdir -p ${bin} && cp "\${CARGO_TARGET_DIR:-target}/debug/opencompany" ${bin}/${name}`;
  // The job user on the EX63 has no sudo; a user namespace gives the same
  // empty network namespace without it.
  const netns = ex63
    ? "unshare --user --map-root-user --net --"
    : "sudo --preserve-env=RUST_MIN_STACK,OPENCOMPANY_OFFLINE_LANE unshare --net --";
  // No apt on the EX63: GUEST_EXTRA_PACKAGES bakes Chromium's libraries into
  // the guest (tinyhumansai/gh-hosted-runner).
  const playwrightInstall = ex63
    ? "npx playwright install chromium"
    : "npx playwright install --with-deps chromium";
  // ex63: every lane shares one checkout, so npm ci and the console build run
  // once in the console lane and the others wait on them. hosted: each group
  // has its own checkout, so the lanes that need them install their own.
  const frontendPrep = (lane) =>
    ex63
      ? []
      : [
          {
            name: "npm-ci",
            when: lane === "desktop" ? desktop : app,
            run: "cd frontend && npm ci",
          },
          ...(lane === "desktop"
            ? [
                {
                  name: "console-build",
                  when: desktop,
                  needs: ["npm-ci"],
                  run: "cd frontend && npm run build",
                },
              ]
            : []),
        ];
  const npmCi = (lane) => (ex63 ? "console:npm-ci" : `${lane}:npm-ci`);

  /** @type {Lane[]} */
  const lanes = [
    {
      name: "static",
      checks: [
        {
          name: "actions-pinned",
          when: true,
          run: "scripts/ci/assert-actions-pinned.sh",
        },
        {
          name: "merge-group-wiring",
          when: true,
          run: "scripts/ci/assert-merge-group-workflow.sh",
        },
        {
          name: "desktop-features",
          when: true,
          run: "scripts/ci/assert-desktop-features.sh",
        },
        {
          name: "analytics-readback-script",
          when: true,
          run: "scripts/ci/test-verify-analytics-capture.sh",
        },
        {
          name: "version-sync",
          when: true,
          run: "node scripts/release/verify-version-sync.mjs",
        },
        {
          name: "markdown-cap",
          when: true,
          run: "./scripts/ci/assert-md-line-cap.sh",
        },
        {
          name: "rust-source-layout",
          when: true,
          run: "./scripts/ci/assert-rs-source-layout.sh tests",
        },
        {
          name: "ci-script-tests",
          when: true,
          run: "node --test scripts/ci/lanes/lanes.test.mjs scripts/ci/ci-gate.test.mjs",
        },
        {
          name: "toolchain-pin",
          when: rust,
          run: "scripts/ci/assert-toolchain-pin.sh",
        },
        {
          name: "vendored-deps-checked-out",
          when: rust,
          run: "scripts/ci/assert-vendored-deps-checked-out.sh",
        },
        {
          // A notice, never a failure (see the script).
          name: "openhuman-drift",
          when: rust,
          reportOnly: true,
          run: "scripts/ci/report-openhuman-drift.sh",
        },
        {
          name: "cargo-lock-sync",
          when: rust,
          run: "cargo metadata --locked --format-version 1 > /dev/null",
        },
        {
          name: "feature-lanes",
          when: rust,
          run: "scripts/ci/assert-feature-lanes.sh",
        },
        { name: "rust-fmt", when: rust, run: "cargo fmt --all -- --check" },
      ],
    },
    {
      // The vendored OpenHuman graph: the long pole, so first in line for a
      // heavy-compile slot. The gated binary is built first so e2e-live can
      // start while the rest of this lane runs.
      name: "gated",
      heavy: 0,
      targetDir: targetDir("gated"),
      env: rustEnv,
      checks: [
        {
          name: "gated-binary",
          when: app,
          run:
            "cargo build --locked -p opencompany-core --features openhuman,mcp,composio --bin opencompany" +
            ` && ${copyBin("opencompany-gated")}`,
        },
        {
          name: "build",
          when: rust,
          run: "cargo build --locked -p opencompany-core --features openhuman --all-targets",
        },
        {
          name: "clippy-openhuman",
          when: rust,
          run: "cargo clippy --locked -p opencompany-core --no-deps --features openhuman --all-targets -- -D warnings",
        },
        {
          name: "auth-matrix",
          when: rust,
          run: "scripts/ci/assert-auth-matrix.sh",
        },
        {
          name: "clippy-acp",
          when: rust,
          run: "cargo clippy --locked -p opencompany-core --no-deps --features acp,runner,tinymemory --all-targets -- -D warnings",
        },
        {
          name: "no-duplicated-openhuman",
          when: rust,
          run: "scripts/ci/assert-no-duplicated-openhuman.sh",
        },
        {
          name: "test-openhuman",
          when: rust,
          run: "cargo test --locked -p opencompany-core --features openhuman --tests",
        },
        {
          name: "runner",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "runner" acp,runner,tinymemory runner`,
        },
        // Four invocations, not one: run-scoped-suite.sh takes exactly one
        // filter (a second positional is libtest's, not a second filter).
        {
          name: "acp-server",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "acp server" acp,runner,tinymemory server::acp`,
        },
        {
          name: "acp-run-turn",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "acp run turn" acp,runner,tinymemory harness::acp::run_turn`,
        },
        {
          name: "acp-routes",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "acp routes" acp,runner,tinymemory server::routes`,
        },
        {
          name: "acp-lanes",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "acp lanes" acp,runner,tinymemory harness::lanes`,
        },
        {
          name: "routes-no-acp",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "routes no-acp" oauth,platform-jwt,documents,tinymemory console_does_not_shadow_unmatched_reserved_paths`,
        },
        {
          name: "chargebee",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "chargebee" openhuman,chargebee,paypal,composio chargebee`,
        },
        {
          name: "finance-read-plane",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "finance read plane" openhuman,chargebee,paypal,composio server::ops::finance`,
        },
        {
          name: "paypal",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "paypal" openhuman,chargebee,paypal,composio paypal`,
        },
        {
          name: "composio",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "composio" openhuman,chargebee,paypal,composio composio`,
        },
        {
          name: "integration-targets-run",
          when: rust,
          run: "scripts/ci/assert-integration-targets-run.sh openhuman",
        },
        {
          // The offline lane: the binary runs in an empty network namespace,
          // so anything that still reaches for the network fails here.
          name: "offline-e2e",
          when: rust,
          env: { OPENCOMPANY_OFFLINE_LANE: "1", RUST_MIN_STACK: "8388608" },
          run:
            "cargo test --locked -p opencompany-core --features openhuman --test offline_e2e --no-run" +
            ' && binary="$(cargo test --locked -p opencompany-core --features openhuman --test offline_e2e' +
            " --no-run --message-format=json 2>/dev/null" +
            " | jq -r 'select(.executable != null and (.target.name == \"offline_e2e\")) | .executable'" +
            ' | tail -n 1)"' +
            ' && test -n "$binary"' +
            ` && ${netns} sh -c "ip link set lo up && exec '$binary' --test-threads=1 --nocapture"`,
        },
        {
          name: "check-all-features",
          when: rust,
          run: "cargo check --locked --all-features --all-targets",
        },
        {
          name: "tool-belt-contract",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "tool-belt contract" openhuman,mcp,media harness::built_in::build::tests`,
        },
        {
          name: "media-toolbelt",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "media toolbelt" openhuman,mcp,media harness::built_in::toolbelt`,
        },
        {
          name: "mcp-oauth-state",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "mcp oauth state" openhuman,mcp,media app::types`,
        },
        {
          name: "hive-mcp-server",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "hive mcp server" openhuman,mcp,media hive::mcp_server`,
        },
        {
          name: "hive-mcp-tools",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "hive mcp tools" openhuman,mcp,media hive::tools`,
        },
        {
          name: "tinymemory-contract",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "tinymemory contract" acp,runner,tinymemory store::memory`,
        },
        {
          name: "tinymemory-selection",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "tinymemory selection" acp,runner,tinymemory store::select`,
        },
        {
          name: "bin-tests-tinymemory",
          when: rust,
          run: "cargo test --locked -p opencompany-core --features acp,runner,tinymemory --bin opencompany",
        },
        {
          name: "memory-provider-contract",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "memory provider contract" tinymemory store::memory`,
        },
        {
          name: "memory-selection",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "memory selection" tinymemory store::select`,
        },
        {
          name: "tinyplace",
          when: rust,
          run: "cargo test --locked -p opencompany-core --features tinyplace --lib",
        },
        {
          name: "webhooks",
          when: rust,
          run: "cargo test --locked -p opencompany-core --features webhooks --lib server::webhook",
        },
      ],
    },
    {
      // The default-feature graph. The host binary is built first so the e2e
      // lane can start while the tests run.
      name: "core",
      heavy: 1,
      targetDir: targetDir("core"),
      env: rustEnv,
      checks: [
        {
          name: "host-binary",
          when: app,
          run:
            "cargo build --locked -p opencompany-core --bin opencompany" +
            ` && ${copyBin("opencompany")}`,
        },
        {
          name: "clippy",
          when: rust,
          run: "cargo clippy --locked --all-targets -- -D warnings",
        },
        { name: "test", when: rust, run: "cargo test --locked" },
        {
          name: "sqlite",
          when: rust,
          run: "cargo test --locked -p opencompany-core --features sqlite --lib store::sqlite",
        },
        {
          name: "export-bundle",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "export bundle" export store::export`,
        },
        {
          name: "mail-imap",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "mail (imap)" imap,smtp server::ops::imap`,
        },
        {
          name: "sidecar-brain",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "sidecar brain" sidecar brain::sidecar`,
        },
        {
          name: "analytics",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "analytics" analytics analytics`,
        },
        {
          // The TinyHumans hub clients (feedback forwarding, the key
          // exchange), gated on `tinyhumans`; HTTP-level tests against a
          // local stub.
          name: "tinyhumans-feedback",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "tinyhumans feedback" tinyhumans feedback::tinyhumans`,
        },
        {
          name: "tinyhumans-hub-identity",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "tinyhumans hub identity" tinyhumans server::hub_identity`,
        },
        {
          name: "crash-reporting",
          when: rust,
          run: `scripts/ci/run-scoped-suite.sh "crash reporting" crash-reporting observability`,
        },
        {
          name: "tui-build",
          when: rust,
          run: "cargo build --locked -p opencompany-tui --bin opencompany-tui",
        },
        {
          name: "tui-harness",
          when: rust,
          run: "cargo test --locked -p opencompany-tui --features harness --lib",
        },
        {
          name: "tui-sqlite",
          when: rust,
          run: "cargo test --locked -p opencompany-tui --features sqlite --lib",
        },
      ],
    },
    {
      name: "console",
      env: nodeEnv,
      checks: [
        // ex63: also the one install every other lane's frontend work uses.
        {
          name: "npm-ci",
          when: ex63 ? desktop : areas.frontend,
          run: "cd frontend && npm ci",
        },
        {
          name: "typecheck",
          when: areas.frontend,
          needs: ["npm-ci"],
          run: "cd frontend && npm run typecheck",
        },
        {
          name: "typecheck-e2e",
          when: areas.frontend,
          needs: ["npm-ci"],
          run: "cd frontend && npm run typecheck:e2e",
        },
        {
          name: "typecheck-unit",
          when: areas.frontend,
          needs: ["npm-ci"],
          run: "cd frontend && npm run typecheck:unit",
        },
        {
          name: "vitest",
          when: areas.frontend,
          needs: ["npm-ci"],
          run: "cd frontend && npm test",
        },
        {
          // ex63: also the frontend the desktop package embeds.
          name: "console-build",
          when: ex63 ? desktop : areas.frontend,
          needs: ["npm-ci"],
          run: "cd frontend && npm run build",
        },
        {
          name: "pages-sdk-build",
          when: areas.frontend,
          needs: ["npm-ci"],
          run: "cd frontend && npm run build:pages-sdk",
        },
        {
          name: "design-tokens",
          when: areas.frontend,
          run: "./scripts/ci/assert-design-tokens.sh",
        },
        {
          name: "setup-inference-imports",
          when: areas.frontend,
          run: "./scripts/ci/assert-setup-inference-imports.sh",
        },
        {
          name: "single-tauri-app",
          when: areas.frontend,
          run: "./scripts/ci/assert-single-tauri-app.sh",
        },
        {
          name: "release-script-tests",
          when: areas.frontend,
          run:
            "node --test scripts/release/generate-release-notes.test.mjs deploy/entrypoint.test.mjs" +
            " scripts/release/bump-version.test.mjs scripts/release/prepare-tauri-config.test.mjs" +
            " scripts/release/publish-updater-manifest.test.mjs scripts/release/assert-latest-unchanged.test.mjs",
        },
        {
          name: "pnpm-lockfile",
          when: areas.frontend,
          run: "cd frontend && npx --yes pnpm@11 install --frozen-lockfile --lockfile-only --trust-lockfile",
        },
        {
          name: "pnpm-lockfile-floor",
          when: areas.frontend,
          run: "cd frontend && npx --yes pnpm@10 install --frozen-lockfile --lockfile-only",
        },
      ],
    },
    {
      // The console suites against the default-feature host binary.
      name: "e2e",
      env: { ...nodeEnv, PW_HOST_BINARY: "${ROOT}/ci-out/bin/opencompany" },
      checks: [
        ...frontendPrep("e2e"),
        {
          name: "playwright-install",
          when: app,
          needs: [npmCi("e2e")],
          run: `cd frontend && ${playwrightInstall}`,
        },
        {
          name: "e2e",
          when: app,
          needs: ["playwright-install", "core:host-binary"],
          run: "cd frontend && npm run e2e && npm run e2e:analytics",
        },
        {
          name: "e2e-first-run",
          when: app,
          needs: ["playwright-install", "core:host-binary"],
          run: "scripts/ci/assert-e2e-spec-ran.sh",
        },
      ],
    },
    {
      // The live-brain suite against the gated binary. On the EX63 it shares
      // the e2e lane's checkout, and the suites' managed host binds one port
      // per checkout, so it waits for e2e to finish (pass or fail).
      name: "e2e-live",
      env: {
        ...nodeEnv,
        PW_HOST_BINARY: "${ROOT}/ci-out/bin/opencompany-gated",
        PW_COMPOSIO: "1",
      },
      checks: [
        ...frontendPrep("e2e-live"),
        {
          name: "playwright-install",
          when: app,
          needs: [npmCi("e2e-live")],
          run: `cd frontend && ${playwrightInstall}`,
        },
        {
          name: "e2e-live",
          when: app,
          needs: ["playwright-install", "gated:gated-binary"],
          after: ex63 ? ["e2e:e2e-first-run"] : [],
          run: "cd frontend && npm run e2e:live",
        },
      ],
    },
    {
      // The Tauri shell: its own workspace and lockfile under
      // crates/opencompany-app, built with the release feature set
      // (scripts/ci/assert-desktop-features.sh keeps these in step).
      name: "desktop",
      heavy: 2,
      targetDir: targetDir("desktop"),
      env: rustEnv,
      checks: [
        ...frontendPrep("desktop"),
        {
          name: "fmt",
          when: desktop,
          run: "cargo fmt --manifest-path crates/opencompany-app/Cargo.toml --all -- --check",
        },
        {
          name: "clippy",
          when: desktop,
          run: "cargo clippy --manifest-path crates/opencompany-app/Cargo.toml --locked --all-targets --no-deps --features opencompany-core/acp,opencompany-core/composio -- -D warnings",
        },
        {
          name: "test",
          when: desktop,
          run: "cargo test --manifest-path crates/opencompany-app/Cargo.toml --locked --features opencompany-core/acp,opencompany-core/composio",
        },
        // Packaged from both directories the Tauri CLI can find the app from,
        // so a cwd-relative hook cannot come back unnoticed (see
        // docs/spec/runtime/desktop.md). The second run is nearly free: same
        // profile, same target dir, everything already built.
        {
          name: "package-from-root",
          when: desktop,
          needs: [ex63 ? "console:console-build" : "console-build"],
          run: "./frontend/node_modules/.bin/tauri build --debug --no-bundle",
        },
        {
          name: "package-from-app-dir",
          when: desktop,
          needs: [ex63 ? "console:console-build" : "console-build"],
          run: "cd crates/opencompany-app && ../../frontend/node_modules/.bin/tauri build --debug --no-bundle",
        },
      ],
    },
  ];

  for (const lane of lanes) {
    lane.checks = lane.checks.map((c) => ({
      ...c,
      when: Boolean(c.when),
      needs: c.needs ?? [],
      after: c.after ?? [],
    }));
    lane.active = lane.checks.some((c) => c.when);
  }
  return { profile, lanes };
}

/** Restrict a plan to the named lanes (hosted groups run a subset each). */
export function selectLanes(plan, names) {
  if (!names || names.length === 0) return plan;
  const known = new Set(plan.lanes.map((l) => l.name));
  for (const n of names) {
    if (!known.has(n)) throw new Error(`unknown lane "${n}"`);
  }
  return { ...plan, lanes: plan.lanes.filter((l) => names.includes(l.name)) };
}

/**
 * Hosted matrix: one entry per group with at least one active lane, so an
 * untouched area never spins up a runner.
 */
export function hostedMatrix(plan) {
  const active = new Set(plan.lanes.filter((l) => l.active).map((l) => l.name));
  return HOSTED_GROUPS.filter((g) => g.lanes.some((l) => active.has(l))).map(
    (g) => ({
      group: g.group,
      lanes: g.lanes.filter((l) => active.has(l)).join(","),
      "max-parallel": g.maxParallel,
    }),
  );
}

/**
 * Check every `needs` / `after` resolves to a check in the plan. Returns a list
 * of problems (empty when the plan is sound).
 */
export function validatePlan(plan) {
  const problems = [];
  const ids = new Set();
  for (const lane of plan.lanes) {
    for (const c of lane.checks) ids.add(`${lane.name}:${c.name}`);
  }
  for (const lane of plan.lanes) {
    for (const c of lane.checks) {
      for (const dep of [...c.needs, ...c.after]) {
        const id = dep.includes(":") ? dep : `${lane.name}:${dep}`;
        if (!ids.has(id))
          problems.push(`${lane.name}:${c.name} depends on unknown check ${id}`);
      }
    }
  }
  return problems;
}

/**
 * @typedef {object} Check
 * @property {string} name
 * @property {boolean} when      run it at all (area-selected)
 * @property {string} run        static bash command
 * @property {string[]} needs    checks (`name` or `lane:name`) that must succeed first
 * @property {string[]} after    checks that must have finished (any outcome) first
 * @property {object} [env]
 * @property {boolean} [reportOnly]  failure never fails the lane
 *
 * @typedef {object} Lane
 * @property {string} name
 * @property {Check[]} checks
 * @property {string|null} [targetDir]  CARGO_TARGET_DIR for this lane
 * @property {object} [env]
 * @property {number} [heavy]  compiles a big Rust graph; priority for a
 *   heavy-compile slot (lower first). Absent for light lanes.
 * @property {boolean} active
 */
