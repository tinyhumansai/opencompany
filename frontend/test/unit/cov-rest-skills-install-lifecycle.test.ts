// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { RegistrySkill, Skill } from "@/api/skills";
import { SkillsView } from "@/views/SkillsView";

vi.mock("sonner", () => ({
  toast: { error: vi.fn(), success: vi.fn(), message: vi.fn() },
}));
const { toast } = await import("sonner");

/**
 * `install`/`uninstall` (`server/ops/skills.rs:370,440`) are
 * `AdminScopedCompany` — a skill's content lands in every agent's effective
 * prompt company-wide, so a member's write here would only ever earn a
 * 403 — and `SkillsView` mirrors that with its own `canManage` gate read
 * from `/auth/me`, withholding "Install" before the write route is ever
 * asked. What had no test at all was the lifecycle itself: install landing
 * moves a registry tile to "Installed", and uninstall a refusal must leave
 * the card in place with an honest toast rather than vanishing it on a
 * write that never actually landed.
 */

const REGISTRY: RegistrySkill = {
  id: "s-standup",
  name: "Standup writer",
  description: "Drafts a standup update.",
  category: "productivity",
  publisher: "opencompany",
};

const INSTALLED: Skill = {
  id: "s-standup",
  name: "Standup writer",
  description: "Drafts a standup update.",
  category: "productivity",
  enabled: true,
  source: "registry",
};

function clientAs(opts: {
  skills?: Skill[];
  role?: "admin" | "member";
  install?: () => Promise<Skill>;
  uninstall?: () => Promise<void>;
}): OpenCompanyClient {
  const install = opts.install ?? (() => Promise.resolve(INSTALLED));
  const uninstall = opts.uninstall ?? (() => Promise.resolve());
  const role = opts.role ?? "admin";
  return {
    scopeFor: () => "/api/v1/companies/acme",
    get: (path: string) => {
      if (path.includes("/skills/registry")) return Promise.resolve([REGISTRY]);
      if (path.endsWith("/auth/me"))
        return Promise.resolve({ id: "u1", email: "a@b.c", role, company: "acme", hasPassword: true });
      return Promise.resolve(opts.skills ?? []);
    },
    post: vi.fn((path: string) => {
      if (path.endsWith("/uninstall")) return uninstall();
      return install();
    }),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function show(element: React.ReactElement) {
  await act(async () => {
    root.render(element);
  });
}

function findButton(text: string): HTMLButtonElement | null {
  return (
    (Array.from(container.querySelectorAll("button")).find((b) =>
      b.textContent?.includes(text),
    ) as HTMLButtonElement | undefined) ?? null
  );
}

beforeEach(() => {
  // The tab a page opens on rides the address (`useHashTab`), so a test that
  // opened one leaves it set for the next. Reset it here rather than in each
  // test: the leak is invisible — the page renders, just on the wrong tab.
  window.location.hash = "";
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

describe("SkillsView, a member without canManage", () => {
  it("withholds Install on the registry tab, matching the host's AdminScopedCompany route", async () => {
    const client = clientAs({ skills: [], role: "member" });
    await show(createElement(SkillsView, { client, company: "acme" }));
    await act(async () => {});
    findButton("Registry")!.click();
    await act(async () => {});

    const card = container.querySelector('[data-testid="registry-card"]')!;
    expect(card.textContent).toContain("Standup writer");
    expect(card.querySelector("button")).toBeNull();
  });
});

describe("install lifecycle", () => {
  it("flips the registry tile from Install to Installed once the write actually lands", async () => {
    const client = clientAs({ skills: [] });
    await show(createElement(SkillsView, { client, company: "acme" }));
    await act(async () => {});
    findButton("Registry")!.click();
    await act(async () => {});

    const card = container.querySelector('[data-testid="registry-card"]')!;
    expect(card.textContent).toContain("Install");

    await act(async () => {
      (card.querySelector("button") as HTMLButtonElement).click();
    });

    expect(card.querySelector("button")).toBeNull();
    expect(card.textContent).toContain("Installed");
  });
});

describe("uninstall lifecycle", () => {
  it("leaves the card installed and says why when the host refuses the uninstall", async () => {
    const client = clientAs({
      skills: [INSTALLED],
      uninstall: () => Promise.reject(new Error("this skill is pinned by the manifest")),
    });
    await show(createElement(SkillsView, { client, company: "acme" }));
    await act(async () => {});

    expect(container.textContent).toContain("Standup writer");
    // Uninstall lives in the row's ⋮ menu, which renders through a portal.
    await act(async () => {
      container
        .querySelector<HTMLButtonElement>(
          '[data-testid="installed-card"] [data-testid="skill-row-menu"]',
        )!
        .click();
    });
    await act(async () => {
      document.querySelector<HTMLElement>('[data-testid="skill-menu-uninstall"]')!.click();
    });

    // Still on screen — the refusal must not have removed the card.
    expect(container.textContent).toContain("Standup writer");
    expect(toast.error).toHaveBeenCalledWith("this skill is pinned by the manifest");
  });
});
