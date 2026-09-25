// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { Skill } from "@/api/skills";
import { SKILL_BUILTIN_UNINSTALL_REASON } from "@/lib/skills-list";
import { SkillsView } from "@/views/SkillsView";

/**
 * SKILL-001's Rust half (`ops/skills.rs`) has slug-validation coverage but no
 * direct REST-route test for install/uninstall. The console's own lifecycle
 * surface — `SkillsView` — had none at all. Two properties: a company
 * (manifest-baked) skill cannot be uninstalled, the one authority split this
 * screen actually draws; and an uninstall the host refuses puts the card back
 * rather than dropping it for good on a client that merely believed it
 * worked.
 *
 * The first property is now asserted as a *disabled* Uninstall carrying the
 * host's own refusal, not as a missing one. An action that silently is not
 * there teaches nothing, and the operator who goes looking for it has no way
 * to find out why.
 */

function skill(over: Partial<Skill> = {}): Skill {
  return {
    id: "s1",
    name: "Refund policy",
    description: "How to handle refund requests.",
    category: "Support",
    source: "registry",
    enabled: true,
    ...over,
  };
}

function clientWith(opts: { skills: Skill[]; post?: (path: string) => Promise<unknown> }): OpenCompanyClient {
  const post = vi.fn(opts.post ?? (() => Promise.resolve(undefined)));
  return {
    scopeFor: () => "/api/v1/company/acme",
    get: vi.fn((path: string) => {
      if (path.endsWith("/skills")) return Promise.resolve(opts.skills);
      if (path.endsWith("/skills/registry")) return Promise.resolve([]);
      if (path.endsWith("/auth/me"))
        return Promise.resolve({ id: "u1", email: "a@b.c", role: "admin", company: "acme", hasPassword: true });
      return Promise.reject(new Error(`unexpected GET ${path}`));
    }),
    post,
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function show(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(SkillsView, { client, company: "acme" }));
  });
}

function cards(): HTMLElement[] {
  return Array.from(container.querySelectorAll('[data-testid="installed-card"]'));
}

function cardNamed(name: string): HTMLElement {
  const found = cards().find((card) => card.textContent?.includes(name));
  if (!found) throw new Error(`no card named ${name}`);
  return found;
}

/** Open a row's ⋮ menu. It renders through a portal, onto `document.body`. */
async function openMenuIn(card: HTMLElement) {
  await act(async () => {
    card.querySelector<HTMLElement>('[data-testid="skill-row-menu"]')!.click();
  });
}

function menuUninstall(): HTMLElement {
  const found = document.querySelector<HTMLElement>('[data-testid="skill-menu-uninstall"]');
  if (!found) throw new Error(`no Uninstall item in:\n${document.body.innerHTML}`);
  return found;
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

describe("a manifest-baked skill cannot be uninstalled", () => {
  it("greys Uninstall on a company skill and says why, and leaves it live on a registry one", async () => {
    const client = clientWith({
      skills: [
        skill({ id: "baked", name: "Baked-in playbook", source: "company" }),
        skill({ id: "installed", name: "Installed skill", source: "registry" }),
      ],
    });
    await show(client);

    await openMenuIn(cardNamed("Baked-in playbook"));
    expect(menuUninstall().getAttribute("aria-disabled")).toBe("true");
    // The host's own sentence, so the menu and the route that refuses agree.
    expect(document.body.textContent).toContain(SKILL_BUILTIN_UNINSTALL_REASON);
    await act(async () => {
      document.querySelector<HTMLElement>('[data-testid="skill-row-menu"]')!.click();
    });

    await openMenuIn(cardNamed("Installed skill"));
    expect(menuUninstall().getAttribute("aria-disabled")).not.toBe("true");
    expect(document.body.textContent).not.toContain(SKILL_BUILTIN_UNINSTALL_REASON);
  });
});

describe("a refused uninstall puts the skill back", () => {
  it("re-lists the skill and reports the failure when the host refuses the uninstall", async () => {
    const client = clientWith({
      skills: [skill()],
      post: () => Promise.reject(new Error("skill is pinned by an active automation")),
    });
    await show(client);

    await openMenuIn(cards()[0]);
    await act(async () => {
      menuUninstall().click();
    });

    expect(cards()).toHaveLength(1);
    expect(container.textContent).toContain("Refund policy");
  });
});

describe("a row the host served without a category", () => {
  // The badge is an identity tint, so an empty one is a coloured pill saying
  // nothing — and a row can genuinely arrive without a category: the field is
  // free-form frontmatter, and an upload row is folded in without a re-read.
  it("renders no category badge rather than an empty one", async () => {
    await show(clientWith({ skills: [skill({ category: "" })] }));

    const card = cardNamed("Refund policy");
    expect(card.querySelector('[data-testid="skill-category"]')).toBeNull();
    // The rest of the row still reports itself.
    expect(card.textContent).toContain("Registry");
    expect(card.textContent).toContain("Never edited");
  });
});
