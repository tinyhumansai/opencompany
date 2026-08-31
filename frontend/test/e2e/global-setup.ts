import {
  request,
  type APIRequestContext,
  type APIResponse,
  type FullConfig,
} from "@playwright/test";

import {
  EXPECTED_INSTANCE_ID,
  MANAGED_HOST_HOME,
  SPEC_PATH,
  identityFailure,
  readHomeInstanceId,
} from "./host-identity";

const ADMIN_EMAIL = "harness-e2e@tinyhumans.ai";
const REQUEST_PATH = "/api/v1/company/auth/request";
const VERIFY_PATH = "/api/v1/company/auth/verify";

/**
 * Identifies the server Playwright adopted, then authenticates once and shares
 * the session with every spec through Playwright storage state, so the suite
 * signs in a single time.
 *
 * Every failure here aborts the whole run before a single spec executes, so the
 * message has to carry enough to diagnose it without a second run: the endpoint,
 * the status, and the body the host actually returned. `auth/request` answers
 * `202 {"sent": true}` for *every* outcome by design — it refuses to say whether
 * an address is a member — so a missing `dev_code` is otherwise indistinguishable
 * from a broken host, which is exactly what issue #271 sent people chasing.
 *
 * The identity check comes first, and ahead of the storage-state early return,
 * because this hook runs after `webServer` has resolved and is therefore the
 * only place that sees which server was actually adopted rather than which one
 * was configured — issue #1773, and `host-identity.ts` for the whole story. A
 * run with no sign-in still gets it: what is on the port is worth knowing even
 * when nothing is about to log in to it.
 */
export default async function globalSetup(config: FullConfig) {
  const baseURL = config.projects[0]?.use.baseURL as string | undefined;
  if (!baseURL) {
    throw new Error(
      "[e2e global-setup] no baseURL is configured. Set PW_BASE_URL to the " +
        "running OpenCompany host, e.g. PW_BASE_URL=http://127.0.0.1:8080.",
    );
  }

  const identity = await request.newContext({ baseURL });
  try {
    await identifyServer(identity, baseURL);
  } finally {
    await identity.dispose();
  }

  // Read the RESOLVED path off the config, not `process.env.PW_STORAGE_STATE`.
  // The config now defaults it when it is the one bringing the host up (issue
  // #406), and reading the raw variable meant this returned early in exactly
  // that case — writing no session, and leaving every spec to fail on a
  // storage-state file nobody had created. The env var still wins where it is
  // set: the config honours it first.
  const storageState = config.projects[0]?.use.storageState as string | undefined;
  if (!storageState) return;

  const context = await request.newContext({ baseURL });
  try {
    const requested = await post(context, baseURL, REQUEST_PATH, {
      email: ADMIN_EMAIL,
    });
    if (!requested.response.ok()) {
      throw new Error(
        `[e2e global-setup] ${describe(requested)}\n` +
          "The host is reachable but rejected the sign-in request.",
      );
    }

    const devCode = readDevCode(requested);
    if (!devCode) {
      throw new Error(
        `[e2e global-setup] ${describe(requested)}\n` +
          `No dev_code came back, so the suite cannot sign in as ${ADMIN_EMAIL}.\n` +
          "The host answers this route identically whatever happened, so check, " +
          "in order:\n" +
          `  1. The host serves a company whose [users] admins lists ${ADMIN_EMAIL} ` +
          "(companies/e2e_harness). An address the host does not recognise gets " +
          'this exact {"sent": true} answer.\n' +
          `  2. The host binds loopback (127.0.0.1 / localhost) and has no ` +
          "OPENCOMPANY_PUBLIC_URL set. A host that looks routable never echoes a " +
          "login code, even with no mail configured.\n" +
          "  3. No OPENCOMPANY_MAIL_* transport is configured. With one wired the " +
          "code is mailed instead of echoed, and this bootstrap cannot read it " +
          "at all — a throttled resend is not the difference, the mailing is.\n" +
          "Re-running the suite inside 60s is NOT a cause: the resend throttle no " +
          "longer applies where the code is echoed rather than mailed (issue #271, " +
          "see RESEND_INTERVAL_MILLIS in src/server/users/routes.rs). If the host " +
          "predates that fix, a second run within the minute does fail here.",
      );
    }

    const verified = await post(context, baseURL, VERIFY_PATH, {
      code: devCode,
    });
    if (!verified.response.ok()) {
      throw new Error(
        `[e2e global-setup] ${describe(verified)}\n` +
          "The dev_code from auth/request was refused, so no session was minted. " +
          "A login code is single-use and expires 15 minutes after it is minted; " +
          "a 401 here means it was already spent or is stale.",
      );
    }
    await context.storageState({ path: storageState });
  } finally {
    await context.dispose();
  }
}

/**
 * Asks `/spec` who answered, and throws unless it is this run's host.
 *
 * The `instance-id` read happens **after** the request and not before: the id
 * is minted lazily on first use, so answering us is what creates that file
 * under the responder's own data root. Read in this order, a root of ours with
 * no file is proof the responder does not serve it. See `host-identity.ts`.
 */
async function identifyServer(context: APIRequestContext, baseURL: string): Promise<void> {
  const url = `${baseURL.replace(/\/$/, "")}${SPEC_PATH}`;

  let response: APIResponse;
  try {
    response = await context.get(SPEC_PATH);
  } catch (error) {
    throw new Error(
      `[e2e global-setup] GET ${url} did not answer: ${String(error)}\n` +
        "Either nothing is serving this address, or something is holding the " +
        "connection open without ever replying — a wedged process still owns " +
        "the port. Either way the suite has no host to drive.",
    );
  }

  const failure = identityFailure({
    url,
    status: response.status(),
    contentType: response.headers()["content-type"] ?? null,
    // Text, not JSON: a dev server's HTML fallback is exactly the body worth
    // quoting back, and `.json()` would throw over it before it could be shown.
    body: await response.text().catch(() => "<body could not be read>"),
    expectedInstanceId: EXPECTED_INSTANCE_ID,
    home: MANAGED_HOST_HOME,
    homeInstanceId: MANAGED_HOST_HOME ? readHomeInstanceId(MANAGED_HOST_HOME) : undefined,
  });

  if (failure) throw new Error(`[e2e global-setup] ${failure}`);
}

/** A response paired with the request that produced it, for reporting. */
type Attempt = {
  method: string;
  url: string;
  response: APIResponse;
  body: string;
};

async function post(
  context: APIRequestContext,
  baseURL: string,
  path: string,
  data: Record<string, unknown>,
): Promise<Attempt> {
  const response = await context.post(path, { data });
  return {
    method: "POST",
    url: `${baseURL.replace(/\/$/, "")}${path}`,
    response,
    // Read as text, not JSON: an HTML error page or an empty body is exactly
    // the case worth reporting verbatim, and `.json()` would throw over it.
    body: await response.text().catch(() => "<body could not be read>"),
  };
}

/** One line naming the request, its status, and what came back. */
function describe(attempt: Attempt): string {
  const { method, url, response, body } = attempt;
  const shown = body.length > 500 ? `${body.slice(0, 500)}…` : body;
  return `${method} ${url} → ${response.status()} ${response.statusText()}; body: ${shown || "<empty>"}`;
}

/** The echoed login code, if the body is JSON and carries one. */
function readDevCode(attempt: Attempt): string | undefined {
  try {
    const parsed = JSON.parse(attempt.body) as { dev_code?: unknown };
    return typeof parsed.dev_code === "string" && parsed.dev_code
      ? parsed.dev_code
      : undefined;
  } catch {
    return undefined;
  }
}
