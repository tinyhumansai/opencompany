/**
 * Whether the server the suite is about to drive is *this run's* host — not
 * merely *a* server that answered (issue #1773).
 *
 * # The failure this exists to stop
 *
 * On 2026-08-25 an end-to-end suite **passed** against the wrong server. A
 * sibling agent's Vite dev server held the port; `webServer.reuseExistingServer`
 * adopted it; the whole suite ran against another worktree's bundle serving a
 * different company, and every assertion passed. It was caught only because a
 * stack trace named unbundled `/src/...` paths and the company read `acme`
 * rather than the harness company. Hours later a second agent found `/healthz`
 * answering `{"status":"ok"}` on its own port *from another process entirely*,
 * while its own host had exited.
 *
 * A green suite against a stranger's server is worse than a red one, because it
 * is reported as evidence.
 *
 * # Why the readiness check cannot catch it
 *
 * Two properties combine badly, and both are load-bearing:
 *
 *  1. **Playwright's `url` check reads the status code and nothing else.** Its
 *     `isURLAvailable` resolves `statusCode >= 200 && statusCode < 404` and
 *     calls `res.resume()` on the body without looking at it — so a Vite SPA
 *     fallback, which answers `200 text/html` for *any* path including
 *     `/healthz`, satisfies a check written for a Rust host. A 302 or a 403
 *     satisfies it too.
 *  2. **`/healthz` names no instance.** It returns a hardcoded
 *     `{"status":"ok"}` (`src/server/routes.rs`), so even a real host is
 *     indistinguishable from a *different* real host.
 *
 * The readiness check answers "something is listening" when the question is
 * "is *my* server listening".
 *
 * # Where the check has to live
 *
 * In `globalSetup`, which Playwright runs **after** every `webServer` plugin
 * has resolved (`createGlobalSetupTasks` orders plugin setup ahead of the
 * global-setup files). That makes it the only hook that observes the server
 * Playwright actually adopted, as opposed to the one it was configured to
 * start.
 *
 * There is precedent one layer down: `scripts/desktop-dev.sh` refuses to adopt
 * a console dev server unless the response body contains `OpenCompany Console`,
 * "so it never reuses a stranger's". This is the same idea with an identifier
 * instead of a page title.
 *
 * # Type, and then identity
 *
 * Catching "this is not an OpenCompany host" is the easy half and stops the
 * first incident: `/spec` is JSON and names the crate. Catching "this is a
 * *different* OpenCompany host" is the second incident, and needs something
 * that differs between two hosts. That is `instance_id` — a random, stable,
 * per-data-root id the host already serves unauthenticated on `/spec` (see
 * `src/app/instance.rs`).
 *
 * Knowing which id to *expect* is the whole trick, and there are two answers:
 *
 *  - **You named one.** `PW_EXPECTED_INSTANCE_ID` is checked whenever it is
 *    set, including against a host you brought yourself with `PW_BASE_URL`.
 *    This is the seam for tooling that already pinned the id when it claimed
 *    the port.
 *
 *  - **We started the host, so we know its data root.** A host mints its id
 *    into `<data-root>/instance-id` and serves that same value, so the file is
 *    the host's own signature on the root we told it to serve. The read happens
 *    **after** the `/spec` request on purpose: the id is minted lazily on first
 *    use, so a freshly booted host has written no file until something asks —
 *    verified on 2026-08-25 against a host on a fresh root, where the file was
 *    absent after `/healthz` answered and present, matching, immediately after
 *    `/spec` did. Read in that order the absence of the file is itself
 *    conclusive: had the responder been serving our root, answering us would
 *    have created it.
 *
 * Nothing here is pinned, cached or written. The expectation is derived fresh
 * from the run's own configuration every time, so a host legitimately restarted
 * between runs — which `test/e2e/host.sh` does on every managed run, wiping the
 * root — is not mistaken for an impostor.
 */

import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { EULER, FIRST_RUN } from "./capabilities";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "../../..");

/** The unauthenticated handshake that names the crate and the instance. */
export const SPEC_PATH = "/spec";

/** The file a host mints its identity into, directly under its data root. */
const INSTANCE_ID_FILE = "instance-id";

/**
 * The instance data root a host **we** manage is told to serve, or `undefined`
 * when `PW_BASE_URL` hands the host over to the caller entirely.
 *
 * Mirrors `test/e2e/host.sh`'s own resolution exactly, for the same reason
 * `playwright.config.ts` mirrors its port derivation: the script computes a
 * default when Playwright does not pass one, and the two must agree or a
 * standalone run and a managed run would disagree about which root is in play.
 *
 * The lane defaults come first because `playwright.config.ts` *sets*
 * `PW_HOST_DATA_DIR` for those lanes — it overrides whatever the caller had, so
 * reading the caller's value here would name a root the host was never given.
 * Outside those lanes the caller's value is what `host.sh` receives, so it wins.
 */
export const MANAGED_HOST_HOME: string | undefined = process.env.PW_BASE_URL
  ? undefined
  : FIRST_RUN
    ? join(repoRoot, "target/e2e/first-run-data")
    : EULER
      ? join(repoRoot, "target/e2e/euler-data")
      : process.env.PW_HOST_DATA_DIR || join(repoRoot, "target/e2e/data");

/**
 * The instance id the caller says this run must be talking to.
 *
 * The escape hatch for a host you brought yourself: `PW_BASE_URL` tells this
 * config nothing about which host is at that address, but whoever claimed the
 * port already read its `instance_id` and can say so here.
 */
export const EXPECTED_INSTANCE_ID: string | undefined = process.env
  .PW_EXPECTED_INSTANCE_ID?.trim()
  ? process.env.PW_EXPECTED_INSTANCE_ID.trim()
  : undefined;

/**
 * The identity a host has already recorded under `home`, if any.
 *
 * `undefined` covers every way of not knowing — no file, an unreadable one, a
 * root that does not exist — because they all mean the same thing to the caller
 * and none of them is worth a distinct failure mode.
 */
export function readHomeInstanceId(home: string): string | undefined {
  try {
    const recorded = readFileSync(join(home, INSTANCE_ID_FILE), "utf8").trim();
    return recorded || undefined;
  } catch {
    return undefined;
  }
}

/** Everything observed about the server that answered, in one place. */
export type HostObservation = {
  /** The absolute URL that was asked, so a failure needs no re-run to place. */
  url: string;
  status: number;
  contentType: string | null;
  /** The raw body, read as text — an HTML error page is the case worth quoting. */
  body: string;
  /** {@link EXPECTED_INSTANCE_ID}, when the caller named one. */
  expectedInstanceId?: string;
  /** {@link MANAGED_HOST_HOME}, when this run brought the host up itself. */
  home?: string;
  /** {@link readHomeInstanceId} over {@link home}, read AFTER the request. */
  homeInstanceId?: string;
};

/**
 * Why `seen` is not this run's host, or `undefined` when it is.
 *
 * A pure function of the observation, so the verdicts are unit-testable without
 * a server (`test/unit/host-identity.test.ts`). Every message names the URL,
 * the status, the content type and the first bytes of the body, because this
 * aborts the run before a single spec executes and a person reading it has
 * nothing else to go on.
 */
export function identityFailure(seen: HostObservation): string | undefined {
  const evidence =
    `GET ${seen.url} → ${seen.status}; content-type: ${seen.contentType ?? "<none>"}; ` +
    `body: ${preview(seen.body)}`;

  if (seen.status < 200 || seen.status >= 300) {
    return (
      `${evidence}\n` +
      "An OpenCompany host answers /spec with 200 and JSON. Whatever is on " +
      "this address either is not one, or is not ready."
    );
  }

  if (!/^application\/json\b/i.test(seen.contentType ?? "")) {
    return `${evidence}\n${NOT_A_HOST}`;
  }

  let spec: { name?: unknown; instance_id?: unknown };
  try {
    spec = JSON.parse(seen.body) as typeof spec;
  } catch {
    return (
      `${evidence}\n` +
      "The content type said JSON but the body does not parse as JSON.\n" +
      NOT_A_HOST
    );
  }

  if (spec.name !== "opencompany") {
    return (
      `${evidence}\n` +
      `/spec parsed, but its "name" is ${JSON.stringify(spec.name)} rather ` +
      `than "opencompany".\n${NOT_A_HOST}`
    );
  }

  const instanceId = typeof spec.instance_id === "string" ? spec.instance_id : undefined;

  // An explicit expectation outranks a derived one: the caller who set it knows
  // something this config does not, and is the reason the variable exists.
  if (seen.expectedInstanceId) {
    if (instanceId !== seen.expectedInstanceId) {
      return (
        `${evidence}\n` +
        `This is an OpenCompany host, but NOT the one this run was told to ` +
        `drive.\n` +
        `  PW_EXPECTED_INSTANCE_ID: ${seen.expectedInstanceId}\n` +
        `  the host that answered:  ${instanceId ?? "<no instance_id in /spec>"}\n` +
        `${WRONG_HOST_ADVICE}`
      );
    }
    return undefined;
  }

  // No expectation to check against: a host we did not start and were not told
  // how to recognise is as far as this can go, and saying so is better than
  // implying a check that did not happen.
  if (!seen.home) return undefined;

  if (!instanceId) {
    return (
      `${evidence}\n` +
      "/spec carries no instance_id, so this host cannot be told apart from " +
      "any other. A host predating src/app/instance.rs would do this — check " +
      "that PW_HOST_BINARY is not a stale build."
    );
  }

  if (!seen.homeInstanceId) {
    return (
      `${evidence}\n` +
      `This is an OpenCompany host, but it is NOT serving the data root this ` +
      `run manages:\n` +
      `  data root this run manages: ${seen.home}\n` +
      `  the host that answered:     instance ${instanceId}\n` +
      "Answering /spec is what mints that root's `instance-id` file, so a root " +
      "with no file after the request above was never served by the responder. " +
      "Something else is on this address.\n" +
      `${WRONG_HOST_ADVICE}`
    );
  }

  if (seen.homeInstanceId !== instanceId) {
    return (
      `${evidence}\n` +
      `This is an OpenCompany host, but a DIFFERENT one from the host this ` +
      `run manages:\n` +
      `  ${join(seen.home, INSTANCE_ID_FILE)} records: ${seen.homeInstanceId}\n` +
      `  the host that answered:                       ${instanceId}\n` +
      `${WRONG_HOST_ADVICE}`
    );
  }

  return undefined;
}

/** Said whenever the responder is not an OpenCompany host at all. */
const NOT_A_HOST =
  "This is not an OpenCompany host. `webServer.reuseExistingServer` adopts " +
  "anything answering 2xx on the port — a console dev server (`npm run dev`) " +
  "answers 200 for every path through its SPA fallback, and satisfies the " +
  "/healthz readiness check exactly as a host would (issue #1773).\n" +
  "Free the port, or point this run somewhere else with PW_BASE_URL.";

/** Said whenever the responder is a host, but the wrong one. */
const WRONG_HOST_ADVICE =
  "Running the suite against it would report on another host's companies, " +
  "bundle and data. Free the port, or name the host you meant with " +
  "PW_BASE_URL / PW_HOST_BIND.";

/**
 * The first 200 bytes of a body, on one line.
 *
 * Whitespace is collapsed rather than preserved: the bodies worth quoting here
 * are HTML pages whose first 200 bytes are mostly newlines and indentation, and
 * a doctype followed by twelve blank lines identifies nothing.
 */
function preview(body: string): string {
  const flattened = body.replace(/\s+/g, " ").trim();
  if (!flattened) return "<empty>";
  return flattened.length > 200 ? `${flattened.slice(0, 200)}…` : flattened;
}
