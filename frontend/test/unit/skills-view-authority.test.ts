// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import { SkillsView } from "@/views/SkillsView";

/**
 * Install, uninstall, toggle and custom-authoring are all `AdminScopedCompany`
 * on the host — a skill's content lands in every agent's effective prompt,
 * company-wide, so these are decisions for the company rather than for the
 * caller alone (codex review). Before this, the console had no role check at
 * all: a member saw every write control enabled and learned only from a 403
 * toast that pasting one in was never going to work.
 */

const INSTALLED: Array<{
  id: string;
  name: string;
  description: string;
  category: string;
  source: string;
  enabled: boolean;
}> = [
  {
    id: "seo-audit",
    name: "SEO audit",
    description: "Checks a site's on-page SEO.",
    category: "Marketing",
    source: "registry",
    enabled: true,
  },
];

const REGISTRY: Array<{
  id: string;
  name: string;
  description: string;
  category: string;
  publisher: string;
}> = [
  {
    id: "cold-outreach",
    name: "Cold outreach",
    description: "Drafts a cold email sequence.",
    category: "Marketing",
    publisher: "OpenCompany",
  },
];

/**
 * A client answering the two reads with fixed data.
 *
 * `session` and `carriesPlatformBearer` are independent, matching
 * `resolve_principal` on the host: a hub console can carry both a platform
 * bearer and a signed-in member (`authHeaders`'s own doc comment), and the
 * host tries the session first, falling back to the bearer only when there is
 * none at all. `session: "none"` makes `/auth/me` reject the way the host's
 * own `no_session()` does — a real `ApiError` from its `{error, code}`
 * envelope — for an unauthenticated or bearer-only caller. `session: "error"`
 * rejects with a plain network-style error, the ambiguous case coderabbit
 * flagged: not a confirmed absence of a session, so it must not be read as
 * one.
 */
function clientWith(
  session: "admin" | "member" | "none" | "error" = "admin",
  carriesPlatformBearer = false,
): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/companies/acme",
    carriesPlatformBearer,
    get: (path: string) => {
      if (path.endsWith("/auth/me")) {
        if (session === "none") {
          return Promise.reject(new ApiError(401, "unauthorized", "not signed in", true));
        }
        if (session === "error") {
          return Promise.reject(new Error("network down"));
        }
        return Promise.resolve({ id: "u1", email: "a@b.c", role: session, company: "acme", hasPassword: true });
      }
      if (path.endsWith("/skills/registry")) {
        return Promise.resolve(REGISTRY);
      }
      return Promise.resolve(INSTALLED);
    },
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function show(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(SkillsView, { client, company: "acme" }));
  });
  // `refresh` resolves both reads as one `Promise.allSettled`, and canManage
  // resolves through its own `/auth/me` round trip — each lands a tick after
  // the initial render, so give React those ticks rather than assuming one
  // flush covers every source.
  await act(async () => {});
  await act(async () => {});
}

function at(testid: string): HTMLElement | null {
  return container.querySelector<HTMLElement>(`[data-testid="${testid}"]`);
}

/** Switches to the Registry tab, whose panel is not mounted until active. */
async function openRegistryTab() {
  const tab = Array.from(container.querySelectorAll<HTMLElement>('[role="tab"]')).find((t) =>
    t.textContent?.includes("Registry"),
  );
  await act(async () => {
    tab?.click();
  });
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

describe("SkillsView authority", () => {
  it("offers a member no way to add, install, enable, or uninstall a skill", async () => {
    await show(clientWith("member"));

    expect(at("skills-admin-only")?.textContent).toContain("Only an admin");
    // The page-level "Add skill" action is gone, not merely disabled.
    expect(
      Array.from(container.querySelectorAll("button")).some((b) => b.textContent?.includes("Add skill")),
    ).toBe(false);

    const toggle = at("installed-card")?.querySelector('[aria-label="Enable skill"]');
    expect(toggle).not.toBeNull();
    expect(toggle?.getAttribute("aria-disabled")).toBe("true");

    expect(at("installed-card")?.querySelector('[aria-label="Uninstall"]')).toBeNull();

    // Not a blank page: the installed skill's name and reach are still shown.
    // Checked before switching tabs — the Installed panel unmounts once the
    // Registry tab takes its place.
    expect(container.textContent).toContain("SEO audit");
    expect(container.textContent).toContain("Available for your agents to read");

    await openRegistryTab();
    expect(
      Array.from(at("registry-card")?.querySelectorAll("button") ?? []).some((b) =>
        b.textContent?.includes("Install"),
      ),
    ).toBe(false);
  });

  it("offers an admin every control, with no read-only notice", async () => {
    await show(clientWith("admin"));

    expect(at("skills-admin-only")).toBeNull();
    expect(
      Array.from(container.querySelectorAll("button")).some((b) => b.textContent?.includes("Add skill")),
    ).toBe(true);

    const toggle = at("installed-card")?.querySelector('[aria-label="Enable skill"]');
    expect(toggle?.getAttribute("aria-disabled")).not.toBe("true");

    await openRegistryTab();
    expect(
      Array.from(at("registry-card")?.querySelectorAll("button") ?? []).some((b) =>
        b.textContent?.includes("Install"),
      ),
    ).toBe(true);
  });

  it("offers a bearer-only caller every control once /auth/me finds no session", async () => {
    await show(clientWith("none", true));

    expect(at("skills-admin-only")).toBeNull();
    expect(
      Array.from(container.querySelectorAll("button")).some((b) => b.textContent?.includes("Add skill")),
    ).toBe(true);
  });

  it("stays read-only on an ambiguous /auth/me failure, even with a bearer present", async () => {
    // A network error, a timeout, or a 5xx is not a confirmed absence of a
    // session — a member's session could still be live and would still take
    // precedence on the host (coderabbit review). Only the host's own
    // no_session() answer may be read as "the bearer is what's left".
    await show(clientWith("error", true));

    expect(at("skills-admin-only")?.textContent).toContain("Only an admin");
    expect(
      Array.from(container.querySelectorAll("button")).some((b) => b.textContent?.includes("Add skill")),
    ).toBe(false);
  });

  it("defers to a member session even when a platform bearer is also present", async () => {
    // A hub console can carry both credentials at once (`authHeaders`), and
    // resolve_principal tries the session first — so a bearer must never
    // paper over a member's own 403 (codex review).
    await show(clientWith("member", true));

    expect(at("skills-admin-only")?.textContent).toContain("Only an admin");
    expect(
      Array.from(container.querySelectorAll("button")).some((b) => b.textContent?.includes("Add skill")),
    ).toBe(false);
  });

  it("defers to an admin session when a platform bearer is also present", async () => {
    await show(clientWith("admin", true));

    expect(at("skills-admin-only")).toBeNull();
    expect(
      Array.from(container.querySelectorAll("button")).some((b) => b.textContent?.includes("Add skill")),
    ).toBe(true);
  });
});
