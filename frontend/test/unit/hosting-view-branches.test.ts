// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { HostingView } from "@/views/HostingView";

/**
 * The conditional surfaces of `HostingView`, which are where its whole job is.
 *
 * Three separate things can each stop a teammate deploying, and two of them are
 * invisible from this form's own fields: the company not granting `hosting`, and
 * the host being built without the tools at all. A single "Connected" badge
 * would be green for both and send an operator hunting through a form that is
 * already correct — so each is reported on its own terms, and the ordering
 * between them matters: granting `hosting` fixes nothing on a host compiled
 * without the harness.
 */

const HOSTING_OK = {
  apiKeyConfigured: true,
  provider: "vercel",
  team: "team_abc",
  granted: true,
  inBuild: true,
  supportedProviders: ["vercel"],
};

/**
 * A client answering the hosting read with a value or a rejection.
 *
 * `/auth/me` is answered separately, and as an admin by default: since #1796
 * this page resolves the viewer's role to decide whether to offer the grant
 * control, and a client that rejected every `GET` alike would leave every test
 * here asserting a non-admin's view by accident. `role` makes that explicit.
 */
function clientWith(answer: unknown, role: "admin" | "member" = "admin"): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/companies/acme",
    get: (path: string) =>
      path.endsWith("/auth/me")
        ? Promise.resolve({ id: "u1", email: "a@b.c", role, company: "acme", hasPassword: true })
        : answer instanceof Error
          ? Promise.reject(answer)
          : Promise.resolve(answer ?? HOSTING_OK),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function show(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(HostingView, { client, company: "acme" }));
  });
}

function at(testid: string): HTMLElement | null {
  return container.querySelector<HTMLElement>(`[data-testid="${testid}"]`);
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
});

describe("HostingView status surfaces", () => {
  it("renders the connected badge and neither alert on the ordinary path", async () => {
    // The control: the assertions below are only worth having if the working
    // case does not produce either alert.
    await show(clientWith(HOSTING_OK));

    expect(at("hosting-view")).not.toBeNull();
    expect(at("hosting-connected")).not.toBeNull();
    expect(at("hosting-not-granted")).toBeNull();
    expect(at("hosting-not-in-build")).toBeNull();
  });

  it("names the grant, not the form, when the company does not grant hosting", async () => {
    // A token stored and still nothing reaches a teammate, so saying "not
    // connected" here would send the operator back through a form that is
    // already correct.
    await show(clientWith({ ...HOSTING_OK, granted: false }));

    expect(at("hosting-not-granted")?.textContent).toContain("hosting");
    expect(at("hosting-not-in-build")).toBeNull();
    // The token is fully configured (HOSTING_OK) and the badge must still not
    // agree with a page that just said this integration reaches nobody.
    expect(at("hosting-connected")).toBeNull();
  });

  it("offers to grant the namespace rather than dead-ending (issue #1796)", async () => {
    // This page used to end the sentence with "it cannot be fixed from this
    // page" — true when written, and the whole of the bug: on a hosted tenant
    // the manifest is a read-only boot snapshot, so the operator had nowhere
    // left to go and the integration read "Connected" forever.
    await show(clientWith({ ...HOSTING_OK, granted: false }));

    const action = at("hosting-not-granted-action");
    expect(action).not.toBeNull();
    expect(action?.textContent).toContain("Grant hosting");
    expect(at("hosting-not-granted")?.textContent).not.toContain(
      "cannot be fixed from this page",
    );
  });

  it("offers a non-admin the explanation and no control", async () => {
    // Every write behind the control is admin-only, so a member gets told what
    // is wrong and who can fix it. Offering the button anyway would trade the
    // old dead end for a button whose only possible outcome is a 403 toast.
    await show(clientWith({ ...HOSTING_OK, granted: false }, "member"));

    const warning = at("hosting-not-granted");
    expect(warning).not.toBeNull();
    expect(warning?.textContent).toContain("An admin has to grant it");
    expect(at("hosting-not-granted-action")).toBeNull();
  });

  it("says the host lacks the tools, and says only that", async () => {
    // Not-in-build outranks not-granted: granting `hosting` in the manifest
    // fixes nothing on a host compiled without the harness, and showing both
    // alerts gives two remedies for one problem.
    await show(clientWith({ ...HOSTING_OK, granted: false, inBuild: false }));

    expect(at("hosting-not-in-build")).not.toBeNull();
    expect(at("hosting-not-granted")).toBeNull();
    expect(at("hosting-connected")).toBeNull();
  });

  it("shows the page-level error when the status cannot be read", async () => {
    await show(clientWith(new Error("store unreachable")));

    expect(at("hosting-load-error")?.textContent).toContain("store unreachable");
    expect(at("hosting-api-key")).toBeNull();
  });

  it("never renders a stored token, and offers to replace rather than reveal it", async () => {
    // The token is write-only end to end: the host does not return it, so the
    // field stays empty with a placeholder. An input pre-filled with dots would
    // invite an operator to "correct" a value they cannot see.
    await show(clientWith(HOSTING_OK));

    const key = at("hosting-api-key") as HTMLInputElement | null;
    expect(key?.value).toBe("");
    expect(key?.getAttribute("placeholder")).toContain("Configured");
    expect(key?.getAttribute("type")).toBe("password");
  });

  it("seeds the team box with what is stored so a typo can be corrected", async () => {
    // The one non-secret field. Leaving it empty would make an operator retype
    // it to change anything else.
    await show(clientWith(HOSTING_OK));

    expect((at("hosting-team") as HTMLInputElement | null)?.value).toBe("team_abc");
  });

  it("offers no disconnect button before anything is stored", async () => {
    await show(clientWith({ ...HOSTING_OK, apiKeyConfigured: false, team: null }));

    expect(at("hosting-connected")).toBeNull();
    expect(at("hosting-clear")).toBeNull();
    expect(at("hosting-save")).not.toBeNull();
  });
});
