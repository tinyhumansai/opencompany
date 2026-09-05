// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import { DomainSettings } from "@/components/domain-settings";
import { PolicySettings } from "@/components/policy-settings";
import { useCanManage, useCanManagePolicy } from "@/hooks/use-can-manage";

/**
 * The two admin-only cards on Settings → General, by role.
 *
 * Neither asked the viewer's role at all before this: the autonomy tiers were
 * clickable, the domain box typable and the SMTP password field offered, while
 * `PUT …/policy` calls `require_admin` and `PUT …/domain` and the SMTP writes
 * are `AdminScopedCompany`. Every one of them answers a member with
 * `403 only an admin can do that`.
 *
 * Lifecycle is deliberately absent from this file. `pause` and `resume` take
 * `CompanyAuth` and never ask for a role, so a member's Pause really does stop
 * the company — a console gate there would be a second lie, not a fix. The
 * missing guard is the host's.
 */

const POLICY = {
  mode: "full",
  manifestMode: "auto",
  overridden: true,
  setBy: "someone@acme.test",
  alwaysApprove: [],
  approvalDeadlineHours: 24,
  tiers: [
    { value: "readonly", label: "Read-only", description: "Look, change nothing." },
    { value: "supervised", label: "Supervised", description: "Conservative." },
    { value: "auto", label: "Auto", description: "Balanced." },
    { value: "full", label: "Full", description: "Broadest." },
  ],
};

const DOMAIN = { domain: "mail.acme.test", verified: true, records: [], checks: [] };
const SMTP = {
  configured: true,
  host: "smtp.acme.test",
  port: 587,
  security: "starttls",
  username: "apikey",
  from_name: "Acme",
  from_email: "hello@acme.test",
};
const SMTP_UNCONFIGURED = { configured: false };

/** A client answering every read this pair makes, with `/auth/me` as `role`. */
function clientAs(
  role: "admin" | "member",
  overrides: { smtp?: unknown } = {},
): OpenCompanyClient {
  const get = (path: string) => {
    if (path.endsWith("/auth/me")) {
      return Promise.resolve({ id: "u1", email: "a@b.c", role, company: "acme" });
    }
    if (path.endsWith("/policy")) return Promise.resolve(POLICY);
    if (path.endsWith("/domain")) return Promise.resolve(DOMAIN);
    if (path.endsWith("/smtp")) return Promise.resolve(overrides.smtp ?? SMTP);
    // The policy card also reads the wired tool slugs, to offer them as
    // suggestions on the always-ask box. An empty set is the shape a host with
    // no wired tools sends, and it degrades to a plain input.
    return Promise.resolve({ slugs: [] });
  };
  return {
    scopeFor: () => "/api/v1/companies/acme",
    get,
    defaultCompany: "acme",
  } as unknown as OpenCompanyClient;
}

/**
 * The platform bearer (`?token=` / `VITE_OC_TOKEN`), with no human session
 * behind it — `carriesPlatformBearer: true` and every `/auth/me` refused.
 */
function bearerClient(): OpenCompanyClient {
  const get = (path: string) => {
    if (path.endsWith("/auth/me")) {
      return Promise.reject(new ApiError(401, "unauthorized", "not signed in", true));
    }
    if (path.endsWith("/policy")) return Promise.resolve(POLICY);
    if (path.endsWith("/domain")) return Promise.resolve(DOMAIN);
    if (path.endsWith("/smtp")) return Promise.resolve(SMTP);
    return Promise.resolve({ slugs: [] });
  };
  return {
    scopeFor: () => "/api/v1/companies/acme",
    carriesPlatformBearer: true,
    get,
    defaultCompany: "acme",
  } as unknown as OpenCompanyClient;
}

/**
 * Wires the two hooks exactly as `SettingsView` does — Policy off
 * {@link useCanManagePolicy}, Domain/SMTP off the broader {@link useCanManage}
 * — so a mismatch between what a principal is granted and what each card is
 * told can only pass if the wiring, not just each hook in isolation, is right.
 */
function SettingsGeneralSlice({ client }: { client: OpenCompanyClient }) {
  const canManage = useCanManage(client, "acme");
  const canManagePolicy = useCanManagePolicy(client, "acme");
  return createElement(
    "div",
    null,
    createElement(PolicySettings, { client, company: "acme", canManage: canManagePolicy }),
    createElement(DomainSettings, { client, company: "acme", canManage }),
  );
}

let container: HTMLDivElement;
let root: Root;

async function show(element: React.ReactElement) {
  await act(async () => {
    root.render(element);
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

describe("Settings → General → Approvals, by role", () => {
  it("leaves a member the tiers to read but not to pick", async () => {
    await show(
      createElement(PolicySettings, {
        client: clientAs("member"),
        company: "acme",
        canManage: false,
      }),
    );

    // Every tier is still shown — which tier the company is on is worth
    // knowing to a member, because it decides what their teammates may do.
    const full = at("policy-tier-full");
    expect(full).not.toBeNull();
    expect(full?.hasAttribute("disabled")).toBe(true);

    const notice = at("policy-read-only");
    expect(notice).not.toBeNull();
    expect(notice?.textContent).toContain("Only an admin");
  });

  it("leaves an admin every tier live and shows no notice", async () => {
    await show(
      createElement(PolicySettings, {
        client: clientAs("admin"),
        company: "acme",
        canManage: true,
      }),
    );

    expect(at("policy-read-only")).toBeNull();
    expect(at("policy-tier-full")?.hasAttribute("disabled")).toBe(false);
  });
});

describe("Settings → General → Domain and SMTP, by role", () => {
  it("offers a member no domain write and no SMTP credential field", async () => {
    await show(
      createElement(DomainSettings, {
        client: clientAs("member"),
        company: "acme",
        canManage: false,
      }),
    );

    expect(at("domain-remove")).toBeNull();
    expect(at("domain-input")).toBeNull();
    // The mutations go — password, Save, and Test, since all three are
    // `AdminScopedCompany` — but the routing itself is member-readable
    // (`GET …/smtp`), so it stays on screen.
    expect(at("smtp-save")).toBeNull();
    expect(at("smtp-password")).toBeNull();
    expect(at("smtp-host")?.textContent).toBe("smtp.acme.test");
    expect(at("smtp-port")?.textContent).toBe("587");
    expect(at("smtp-security")?.textContent).toBe("STARTTLS");
    expect(at("smtp-username")?.textContent).toBe("apikey");
    expect(at("smtp-from-name")?.textContent).toBe("Acme");
    expect(at("smtp-from-email")?.textContent).toBe("hello@acme.test");
  });

  it("falls back to the plain summary for a member when nothing is configured", async () => {
    await show(
      createElement(DomainSettings, {
        client: clientAs("member", { smtp: SMTP_UNCONFIGURED }),
        company: "acme",
        canManage: false,
      }),
    );

    expect(at("smtp-routing")).toBeNull();
    expect(at("smtp-member-summary")?.textContent).toContain("No outbound mail server");
  });

  it("tells a member why both cards are read-only", async () => {
    await show(
      createElement(DomainSettings, {
        client: clientAs("member"),
        company: "acme",
        canManage: false,
      }),
    );

    expect(at("domain-read-only")?.textContent).toContain("Only an admin");
    expect(at("smtp-read-only")?.textContent).toContain("Only an admin");
  });

  it("still lets a member re-check DNS, which the host allows", async () => {
    // `POST …/domain/verify` is `ScopedCompany` on purpose — it re-reads DNS
    // for a domain only an admin could have set and changes nothing a member
    // could not already read. Withholding it would over-correct.
    await show(
      createElement(DomainSettings, {
        client: clientAs("member"),
        company: "acme",
        canManage: false,
      }),
    );

    expect(at("domain-verify")).not.toBeNull();
  });

  it("offers an admin both forms and no notice", async () => {
    await show(
      createElement(DomainSettings, {
        client: clientAs("admin"),
        company: "acme",
        canManage: true,
      }),
    );

    expect(at("domain-read-only")).toBeNull();
    expect(at("smtp-read-only")).toBeNull();
    expect(at("domain-remove")).not.toBeNull();
    expect(at("smtp-save")).not.toBeNull();
  });
});

describe("Settings → General, a platform bearer with no human session", () => {
  it("gets Domain/SMTP management but not the policy controls", async () => {
    await show(createElement(SettingsGeneralSlice, { client: bearerClient() }));

    // `AdminScopedCompany` (`scope.rs`) admits the bearer directly.
    expect(at("domain-read-only")).toBeNull();
    expect(at("smtp-read-only")).toBeNull();
    expect(at("domain-remove")).not.toBeNull();
    expect(at("smtp-save")).not.toBeNull();

    // `set_policy` calls `require_admin` off the request headers and refuses
    // a bearer with no session behind it as unauthenticated.
    expect(at("policy-read-only")).not.toBeNull();
    expect(at("policy-tier-full")?.hasAttribute("disabled")).toBe(true);
  });
});
