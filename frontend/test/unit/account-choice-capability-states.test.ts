// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { AccountChoiceSection } from "@/views/connections/AccountChoiceSection";

/**
 * Issue #2081 — "Which account teammates act as" showed a reload-me error on
 * every load of the Apps page.
 *
 * `GET …/composio/connections` is registered in every build and answers 409 for
 * both of its capability states: `not_in_build` on a binary compiled without
 * the `composio` feature (the default set), and `not_configured` when the
 * company has no Composio credential. Neither is a failed read, and neither is
 * cleared by reloading — but with only a 404 counted as absence, both landed in
 * the transient bucket and the section drew "the host could not answer …
 * Reload to try again" over a question that simply has no answer here.
 *
 * These pin the four states apart, because the two that were wrong are only
 * wrong RELATIVE to the two that were right: a fix that hides the section for
 * everything would also hide the genuinely-unknown case #1470 exists to keep
 * visible.
 */

/** The reload-me copy `SectionUnreachable` renders. Its presence is the bug. */
const RELOAD_COPY = "Reload to try again";

function hostThatFails(err: unknown): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) =>
      company === null ? "/api/v1/company" : `/api/v1/companies/${company}`,
    get: async () => {
      throw err;
    },
  } as unknown as OpenCompanyClient;
}

/** Two accounts under one toolkit — the only shape this section renders at all. */
function hostWithTwoAccounts(): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) =>
      company === null ? "/api/v1/company" : `/api/v1/companies/${company}`,
    get: async () => [
      {
        toolkit: "gmail",
        connected: true,
        accounts: [
          { id: "ca_ops", status: "ACTIVE", connected: true, account: "ops@acme.test" },
          { id: "ca_billing", status: "ACTIVE", connected: true, account: "billing@acme.test" },
        ],
      },
    ],
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
});

async function show(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(AccountChoiceSection, { client, company: "acme", canManage: true }));
  });
}

describe("AccountChoiceSection capability states", () => {
  it("renders nothing when the build has no Composio (not_in_build)", async () => {
    await show(
      hostThatFails(
        new ApiError(409, "not_in_build", "Composio is not compiled into this build", true),
      ),
    );
    expect(container.textContent).not.toContain(RELOAD_COPY);
    expect(container.querySelector('[data-testid="account-choice"]')).toBeNull();
  });

  it("renders nothing when the company has no Composio credential (not_configured)", async () => {
    await show(
      hostThatFails(
        new ApiError(
          409,
          "not_configured",
          "no Composio credential is available for this company",
          true,
        ),
      ),
    );
    expect(container.textContent).not.toContain(RELOAD_COPY);
    expect(container.querySelector('[data-testid="account-choice"]')).toBeNull();
  });

  /**
   * The half that must NOT change. A host that could not answer leaves the
   * choice unknown, and an unknown list rendered as an empty one is the #1470
   * failure that sent operators looking for a rebuild.
   */
  it("still says the host could not answer on a genuinely transient failure", async () => {
    await show(hostThatFails(new TypeError("fetch failed")));
    expect(container.textContent).toContain(RELOAD_COPY);
  });

  it("still says the host could not answer on a 5xx", async () => {
    await show(hostThatFails(new ApiError(503, "quiescing", "being rebuilt", true)));
    expect(container.textContent).toContain(RELOAD_COPY);
  });

  /**
   * A 409 that is NOT a capability code is an ordinary conflict — a paused
   * company, a lost race — and stays unknown. This is the guard against
   * "fixing" the bug by reading the status instead of the code.
   */
  it("still says the host could not answer on a non-capability 409", async () => {
    await show(hostThatFails(new ApiError(409, "lifecycle_conflict", "company is paused", true)));
    expect(container.textContent).toContain(RELOAD_COPY);
  });

  it("renders the chooser when the host does answer with two accounts", async () => {
    await show(hostWithTwoAccounts());
    expect(container.textContent).toContain("ops@acme.test");
    expect(container.textContent).toContain("billing@acme.test");
    expect(container.textContent).not.toContain(RELOAD_COPY);
  });
});
