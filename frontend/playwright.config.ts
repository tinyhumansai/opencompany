import { defineConfig } from "@playwright/test";
import { createHash } from "node:crypto";
import { mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  COMPOSIO,
  COMPOSIO_FIXTURE_BIND,
  EULER,
  EULER_COMPANY,
  FIRST_RUN,
  FIRST_RUN_COMPANY,
  LIVE_BRAIN,
  LIVE_LLM,
  LIVE_LLM_BIND,
  MCP_FIXTURE_BIND,
  MOCK_BRAIN_BIND,
  VISUAL,
} from "./test/e2e/capabilities";
import { MANAGED_HOST_HOME } from "./test/e2e/host-identity";

// `package.json` is `"type": "module"`, so this file is ESM and `__dirname`
// does not exist here — it type-checks against `@types/node` and then throws at
// load. Which is the whole lesson of #406 in one line.
const here = dirname(fileURLToPath(import.meta.url));

/**
 * Playwright config for the operator-console end-to-end suite.
 *
 * The suite drives a *running* OpenCompany host — the Rust binary serving the
 * built `frontend/dist` via `OPENCOMPANY_CONSOLE_DIR`. There are two ways to
 * get one, and `PW_BASE_URL` alone decides which applies:
 *
 * **`PW_BASE_URL` set** — you brought your own host and this config touches
 * nothing else: no `webServer`, and `PW_STORAGE_STATE` keeps its exact former
 * meaning (unset ⇒ no sign-in). Unchanged contract. `globalSetup` does run
 * either way now, to identify what is on the address before the suite trusts
 * it (issue #1773); it still authenticates only when a storage state is
 * configured.
 *
 * **`PW_BASE_URL` unset** — the config brings a host up itself through
 * `test/e2e/host.sh` and authenticates against it, so `npm run e2e` is the
 * whole command. It needs `target/debug/opencompany` to exist; `host.sh` says
 * so, and says how, rather than failing obscurely.
 *
 * That second path exists because of issue #406. This suite is typechecked by
 * CI (`npm run typecheck:e2e`) and executed by nobody, which is how
 * `workflow-edit-delete.spec.ts` came to reference a fixture that was never
 * committed and stay red indefinitely without one report. Typechecking a spec
 * proves it compiles, not that it holds. Running it was possible before this —
 * but only if you already knew which four environment variables to set, and a
 * suite that is hard to start is a suite nobody starts.
 *
 * CI runs it in two lanes (#428, #467): `Console E2E` against a default-feature
 * host, and `Console E2E (live brain)` against a feature-gated one with the
 * fixtures below standing behind it.
 *
 * # `PW_LIVE_BRAIN=1` — the second lane
 *
 * Four specs need an agent that actually executes, which needs a host built
 * with `--features openhuman,mcp` **and** something for that harness
 * to think with. When we manage the host, this config supplies the second half:
 * it starts `test/e2e/mock-brain.mjs` and `test/e2e/mcp-server.mjs` alongside
 * the host, and hands the host the inference endpoint through
 * `PW_HOST_PASSTHROUGH` — the escape hatch `host.sh`'s allowlist keeps for
 * exactly this. The binary is still yours to supply (`PW_HOST_BINARY`, or
 * `target/debug/opencompany` built with those features); nothing here can tell
 * a feature-gated host from a default one, which is why the flag is a
 * declaration rather than a probe. See `test/e2e/capabilities.ts`.
 *
 * Against a host you brought yourself (`PW_BASE_URL`), the flag still enables
 * the four specs but the fixtures are yours to start too — this config will not
 * reconfigure a host it did not launch.
 *
 * # `PW_LIVE_LLM=1` — the lane with a real model in it
 *
 * `npm run e2e:live-llm`. Same feature-gated binary, but the inference behind
 * it is `test/e2e/live-brain-proxy.mjs` in front of a real router rather than
 * `mock-brain.mjs`, and the selection below narrows the run to
 * `orchestration-live.spec.ts` — the one spec whose claim is that a *model*,
 * given a goal and this company's real tool descriptions, delegates it and
 * closes it out. Every other spec asserts on the mock's scripted answers and
 * cannot pass against it. Not run by CI: it spends tokens and its verdict
 * depends on a model's judgement. See `LIVE_LLM` in `test/e2e/capabilities.ts`.
 */

/**
 * The one spec that needs a host serving a company nobody has staffed
 * (`companies/e2e_setup`), and which therefore cannot share a host with the
 * rest of the suite. See `FIRST_RUN` in `test/e2e/capabilities.ts` for why this
 * is a second run rather than a skip inside the spec — issue #1404.
 */
const FIRST_RUN_SPEC = /company-setup\.spec\.ts$/;

/**
 * The one spec that needs a host thinking with a **real model** rather than
 * with the scripted mock, and which therefore cannot share a host with the rest
 * of the suite either. See `LIVE_LLM` in `test/e2e/capabilities.ts`.
 */
const LIVE_LLM_SPEC = /orchestration-live\.spec\.ts$/;

/**
 * The one spec whose verdict is a **published integer** rather than a shape on
 * the board, and which therefore needs a host serving the lab that computes it
 * (`companies/agentic_math_lab`). See `EULER` in `test/e2e/capabilities.ts`.
 */
const EULER_SPEC = /euler-live\.spec\.ts$/;

/**
 * The one spec that compares **pixels** rather than named quantities, and which
 * therefore runs on its own so a page still settling cannot be attributed to it
 * — and so it never sits in the way of a merge. Same default-feature host as an
 * ordinary run; the separation is about the kind of verdict, not the kind of
 * host. See `VISUAL` in `test/e2e/capabilities.ts`.
 */
const VISUAL_SPEC = /visual\.spec\.ts$/;

const providedBaseURL = process.env.PW_BASE_URL;

/**
 * Whether this config is responsible for the host, as opposed to driving yours.
 *
 * Derived from `MANAGED_HOST_HOME` rather than from `providedBaseURL` directly,
 * even though the two say the same thing (`!process.env.PW_BASE_URL`). One
 * source, so the address this config manages and the data root behind it can
 * never disagree about whether there is a host of ours at all.
 */
const managesHost = MANAGED_HOST_HOME !== undefined;

/**
 * Where a host *we* manage listens.
 *
 * The default is derived from this checkout's own path. A fixed default
 * collides across worktrees, and it collides SILENTLY: `reuseExistingServer`
 * below is on outside CI, so a second run does not fail with "port in use" —
 * it adopts the host the first run started and reports on that binary, that
 * console bundle and that data directory. Hashing the checkout path gives
 * every worktree its own port, stable across runs (so a host you left up is
 * still reused by the next run in the SAME worktree, which is what
 * `reuseExistingServer` is for) and distinct from every other worktree's.
 * `test/e2e/host.sh` derives the identical default from the same path, so
 * running it directly agrees with running it through Playwright.
 *
 * 8100-16899 avoids 8080 itself and stays below the ephemeral range
 * (net.ipv4.ip_local_port_range starts at 32768), so the kernel cannot hand a
 * derived port to something else first. The width matters: this repository has
 * ~200 worktrees, and birthday collisions over 800 ports put ~23 of them on a
 * shared number. Over 8800 it is closer to two, and two worktrees that do
 * collide are still only as broken as every worktree is today.
 *
 * `PW_HOST_BIND` names the bind explicitly when the derived default is not the
 * one you want — `PW_HOST_BIND=127.0.0.1:8123 npm run e2e` is a run that
 * cannot collide with anyone. (`PW_BASE_URL` moves the port too, but by
 * handing the host over to you entirely.)
 */
const repoRoot = resolve(here, "..");
const derivedPort =
  8100 +
  (parseInt(createHash("sha256").update(repoRoot).digest("hex").slice(0, 8), 16) %
    8800);

const managedBind = process.env.PW_HOST_BIND || `127.0.0.1:${derivedPort}`;

const baseURL = providedBaseURL || `http://${managedBind}`;

/**
 * Where the shared signed-in session lands. Defaulted only when we manage the
 * host: against a host we started, every spec needs a session and there is no
 * reason to make the caller name a path for it. Against yours, an unset
 * variable keeps its existing meaning.
 */
const storageState =
  process.env.PW_STORAGE_STATE ||
  (managesHost
    ? resolve(
        here,
        // A path of its own per lane that signs into a different company on a
        // different data root: a shared file would hand one run the other's
        // session, which reads as a mysterious sign-in loop rather than as the
        // collision it is.
        FIRST_RUN
          ? "../target/e2e/first-run-storage-state.json"
          : EULER
            ? "../target/e2e/euler-storage-state.json"
            : "../target/e2e/storage-state.json",
      )
    : undefined);

// `global-setup.ts` writes that file but does not create its directory, and
// `target/e2e/` does not exist on a clean checkout.
if (storageState) {
  mkdirSync(dirname(storageState), { recursive: true });
}

/** Whether this run also brings up the live-brain fixtures (issue #467). */
const managesFixtures = managesHost && LIVE_BRAIN;

/**
 * What the host is told about inference, and which of those names `host.sh` is
 * allowed to forward.
 *
 * The bearer is a placeholder and nothing checks it: the host needs *a*
 * credential only because a configured credential is what makes it choose a
 * live harness over the offline echo brain (`harness_inference_from_env`).
 *
 * `PW_HOST_PASSTHROUGH` is the load-bearing line. `host.sh` starts the host
 * from an empty environment and copies in an allowlist, so a variable set here
 * and not named there reaches nothing.
 */
/** Whether this run also brings up the real-model proxy (the live-LLM lane). */
const managesLiveLlm = managesHost && LIVE_LLM;

const inferenceEnv: Record<string, string> = managesLiveLlm
  ? {
      // The bearer is still a placeholder and still unchecked — the *upstream*
      // credential belongs to the proxy, which is the only process that talks
      // to the router. The host needs one only because a configured credential
      // is what makes it choose a live harness over the offline echo brain.
      OPENCOMPANY_INFERENCE_KEY: "live-brain-proxy",
      OPENCOMPANY_INFERENCE_URL: `http://${LIVE_LLM_BIND}/v1`,
    }
  : managesFixtures
    ? {
        OPENCOMPANY_INFERENCE_KEY: "mock-brain",
        OPENCOMPANY_INFERENCE_URL: `http://${MOCK_BRAIN_BIND}/v1`,
      }
    : {};

/** Whether this run also brings up the Composio fixture backend (issue #820). */
const managesComposio = managesHost && COMPOSIO;

/**
 * Where the host's Composio calls go, when this run is standing a fixture up.
 *
 * The same `PW_HOST_PASSTHROUGH` caveat applies as above and is the reason the
 * two blocks are joined below rather than each setting the variable: `host.sh`
 * copies an allowlist into an empty environment, so a second assignment here
 * would quietly replace the first and the inference URL would never arrive.
 */
const composioEnv: Record<string, string> = managesComposio
  ? { OPENCOMPANY_COMPOSIO_BACKEND_URL: `http://${COMPOSIO_FIXTURE_BIND}` }
  : {};

/**
 * What a first-run run tells `test/e2e/host.sh` to serve.
 *
 * The company is the whole point: `companies/e2e_setup` declares no `[[agent]]`,
 * so it is the only company this repository ships that first-run setup can open
 * on. The data root is separate for the same reason it is wiped — the spec
 * creates a team, and a second run against a root still holding the first one's
 * team would find the company already staffed and the gate correctly shut.
 *
 * Both are read by `host.sh` itself rather than by the host binary, so they do
 * not go through `PW_HOST_PASSTHROUGH`.
 */
const firstRunEnv: Record<string, string> =
  // `MANAGED_HOST_HOME !== undefined` rather than `managesHost`, which is the
  // same test: TypeScript narrows the data root away from `undefined` only
  // through the comparison itself, not through a boolean aliasing it.
  MANAGED_HOST_HOME !== undefined && FIRST_RUN
    ? {
        PW_HOST_COMPANY: resolve(here, "..", FIRST_RUN_COMPANY),
        PW_HOST_DATA_DIR: MANAGED_HOST_HOME,
      }
    : {};

/**
 * What a Project Euler run tells `test/e2e/host.sh` to serve.
 *
 * The company is the point: `companies/agentic_math_lab` is the roster whose
 * split — decide, program, break — and whose *withheld* grants (no `web`, no
 * `search`) are what the spec's verdict rests on. The data root is separate so
 * the answers ledger read at the end of a run cannot be holding the previous
 * run's row.
 *
 * Both are read by `host.sh` itself rather than by the host binary, so they do
 * not go through `PW_HOST_PASSTHROUGH`.
 */
const eulerEnv: Record<string, string> =
  // See `firstRunEnv` for why this is not `managesHost`.
  MANAGED_HOST_HOME !== undefined && EULER
    ? {
        PW_HOST_COMPANY: resolve(here, "..", EULER_COMPANY),
        PW_HOST_DATA_DIR: MANAGED_HOST_HOME,
      }
    : {};

const passthrough = [...Object.keys(inferenceEnv), ...Object.keys(composioEnv)];
const hostEnv: Record<string, string> = {
  ...inferenceEnv,
  ...composioEnv,
  ...firstRunEnv,
  ...eulerEnv,
  ...(passthrough.length > 0 ? { PW_HOST_PASSTHROUGH: passthrough.join(" ") } : {}),
};

/**
 * One `webServer` entry per fixture, ahead of the host.
 *
 * Both are plain Node scripts with no dependencies, and both answer `/healthz`,
 * which is what lets Playwright wait for them rather than leaving the first
 * agent turn to discover a backend that is not up yet. Ordering is a courtesy
 * only — the host reads its inference URL at boot but does not dial it until a
 * turn runs, well after every server here is ready.
 */
const fixtureServers = [
  ...(managesLiveLlm
    ? [
        {
          command: `node ./test/e2e/live-brain-proxy.mjs --bind ${LIVE_LLM_BIND}`,
          url: `http://${LIVE_LLM_BIND}/healthz`,
          reuseExistingServer: !process.env.CI,
          timeout: 30_000,
          stdout: "pipe" as const,
          stderr: "pipe" as const,
        },
      ]
    : []),
  ...(managesComposio
    ? [
        {
          command: `node ./test/e2e/composio-backend.mjs --bind ${COMPOSIO_FIXTURE_BIND}`,
          url: `http://${COMPOSIO_FIXTURE_BIND}/healthz`,
          reuseExistingServer: !process.env.CI,
          timeout: 30_000,
          stdout: "pipe" as const,
          stderr: "pipe" as const,
        },
      ]
    : []),
  ...(managesFixtures
  ? [
      {
        command: `node ./test/e2e/mock-brain.mjs --bind ${MOCK_BRAIN_BIND}`,
        url: `http://${MOCK_BRAIN_BIND}/healthz`,
        reuseExistingServer: !process.env.CI,
        timeout: 30_000,
        stdout: "pipe" as const,
        stderr: "pipe" as const,
      },
      {
        command: `node ./test/e2e/mcp-server.mjs --bind ${MCP_FIXTURE_BIND}`,
        url: `http://${MCP_FIXTURE_BIND}/healthz`,
        reuseExistingServer: !process.env.CI,
        timeout: 30_000,
        stdout: "pipe" as const,
        stderr: "pipe" as const,
      },
      ]
    : []),
];

export default defineConfig({
  testDir: "./test/e2e",
  // The two runs are disjoint **by selection**, not by a guard inside a spec.
  // A first-run run drives a host serving an unstaffed company and can only run
  // the first-run spec; every other run drives the harness company, against
  // which that spec is unpassable. Expressing it here means neither run can be
  // pointed at a host it cannot pass against, and — because Playwright exits
  // non-zero when a selection matches nothing — an empty selection is a
  // failure rather than a silent zero (issue #1404).
  // Three disjoint selections, one per kind of host. A first-run run drives a
  // host serving an unstaffed company; a live-LLM run drives one thinking with
  // a real model; an ordinary run drives the harness company on the scripted
  // mock. Each spec is unpassable against the other two hosts, so the selection
  // — rather than a guard inside a spec — is what keeps a run from being
  // pointed at a host it cannot pass against. Playwright exits non-zero when a
  // selection matches nothing, so an empty one is a failure rather than a
  // silent zero (issue #1404).
  //
  // Four disjoint selections now: the Project Euler lane is a live-LLM run
  // against a different company, so it is checked *before* `LIVE_LLM` — both
  // flags are set for it, and the more specific lane wins.
  //
  // Five now: the visual lane is the fifth, and it is selected the same way for
  // a different reason — its host is an ordinary one, but a run that mixed
  // pixel comparison in with the rest would attribute a page still settling to
  // whichever spec happened to be next.
  ...(FIRST_RUN
    ? { testMatch: FIRST_RUN_SPEC }
    : EULER
      ? { testMatch: EULER_SPEC }
      : LIVE_LLM
        ? { testMatch: LIVE_LLM_SPEC }
        : VISUAL
          ? { testMatch: VISUAL_SPEC }
          : { testIgnore: [FIRST_RUN_SPEC, LIVE_LLM_SPEC, EULER_SPEC, VISUAL_SPEC] }),
  // UNCONDITIONAL, and it was not always (issue #1773). `global-setup.ts` runs
  // after every `webServer` above has resolved, which makes it the only hook
  // that sees the server Playwright *adopted* rather than the one it was
  // configured to start — and `reuseExistingServer` will adopt anything
  // answering 2xx, including a console dev server whose SPA fallback answers
  // 200 for `/healthz`. So it now identifies the server before doing anything
  // else, and that has to happen on every run, not only the ones that sign in.
  //
  // The sign-in contract is unchanged: `global-setup.ts` reads the resolved
  // `storageState` off the config and returns before authenticating when there
  // is none, exactly as an absent `globalSetup` did.
  globalSetup: "./test/e2e/global-setup.ts",
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  // The visual lane compares pixels of a d3 physics sim in headless Chromium,
  // whose frame clock can occasionally stall (`requestAnimationFrame` stops
  // firing and the graph never ticks — `settleKnowledgeGraph` in visual.spec.ts
  // now fails loudly on that). One retry absorbs the stall on a fresh page
  // while still failing on a real diff, which survives two consecutive stalls
  // only 1-in-80 times. Every other lane keeps Playwright's default of zero.
  retries: VISUAL ? 1 : 0,
  reporter: [["list"]],
  use: {
    baseURL,
    storageState,
    trace: "on-first-retry",
    screenshot: "only-on-failure",
    // The visual lane's baselines are only meaningful at the size and density
    // they were recorded at, so both are pinned here rather than inherited.
    // Playwright's own defaults are the same two values today; naming them
    // means a future change to them is a decision about this lane instead of a
    // silent invalidation of every committed PNG.
    // `reducedMotion` is not belt-and-braces on top of `animations: "disabled"`
    // — that option freezes CSS animations at their end state, and Overview's
    // knowledge graph is a d3 simulation driven from `requestAnimationFrame`,
    // which no CSS switch reaches. The graph reads the media query itself
    // (`KnowledgeGraph.tsx` has a `prefers-reduced-motion` block) so this is the
    // lever it was built to respond to.
    ...(VISUAL
      ? { viewport: { width: 1280, height: 720 }, deviceScaleFactor: 1, reducedMotion: "reduce" as const }
      : {}),
  },
  webServer: managesHost
    ? [
        ...fixtureServers,
        {
          command: "./test/e2e/host.sh",
          url: `${baseURL}/healthz`,
          // A host already listening on the default bind is almost always the
          // one you are developing against, so drive it rather than fight it
          // for the port. In CI that would mean silently testing something
          // unknown.
          reuseExistingServer: !process.env.CI,
          // Covers a cold `npm run build` for the console bundle plus the
          // host's own boot, with room to spare.
          timeout: 180_000,
          stdout: "pipe" as const,
          stderr: "pipe" as const,
          env: {
            PW_HOST_BIND: new URL(baseURL).host,
            ...hostEnv,
          },
        },
      ]
    : undefined,
});
