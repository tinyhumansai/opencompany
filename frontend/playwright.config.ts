import { defineConfig } from "@playwright/test";
import { mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

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
 * meaning (unset ⇒ no `globalSetup` ⇒ no sign-in). Unchanged contract.
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
 * CI still does not run it. That needs the vendored submodules, a Rust
 * toolchain and a built binary, none of which the Node-only `Console` job has,
 * so it is a lane of its own rather than something smuggled in here.
 */

const providedBaseURL = process.env.PW_BASE_URL;

/** Whether this config is responsible for the host, as opposed to driving yours. */
const managesHost = !providedBaseURL;

const baseURL = providedBaseURL || "http://127.0.0.1:8080";

/**
 * Where the shared signed-in session lands. Defaulted only when we manage the
 * host: against a host we started, every spec needs a session and there is no
 * reason to make the caller name a path for it. Against yours, an unset
 * variable keeps its existing meaning.
 */
const storageState =
  process.env.PW_STORAGE_STATE ||
  (managesHost ? resolve(here, "../target/e2e/storage-state.json") : undefined);

// `global-setup.ts` writes that file but does not create its directory, and
// `target/e2e/` does not exist on a clean checkout.
if (storageState) {
  mkdirSync(dirname(storageState), { recursive: true });
}

export default defineConfig({
  testDir: "./test/e2e",
  globalSetup: storageState ? "./test/e2e/global-setup.ts" : undefined,
  fullyParallel: false,
  workers: 1,
  timeout: 60_000,
  reporter: [["list"]],
  use: {
    baseURL,
    storageState,
    trace: "on-first-retry",
    screenshot: "only-on-failure",
  },
  webServer: managesHost
    ? {
        command: "./test/e2e/host.sh",
        url: `${baseURL}/healthz`,
        // A host already listening on the default bind is almost always the one
        // you are developing against, so drive it rather than fight it for the
        // port. In CI that would mean silently testing something unknown.
        reuseExistingServer: !process.env.CI,
        // Covers a cold `npm run build` for the console bundle plus the host's
        // own boot, with room to spare.
        timeout: 180_000,
        stdout: "pipe",
        stderr: "pipe",
        env: {
          PW_HOST_BIND: new URL(baseURL).host,
        },
      }
    : undefined,
});
