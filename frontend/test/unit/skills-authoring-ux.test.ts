// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { SKILL_DESCRIPTION_MAX_CHARS, skillDescriptionCount } from "@/lib/skills";
import { SkillsView } from "@/views/SkillsView";

/**
 * The three authoring affordances the Skills page gained, each asserted for the
 * thing that can silently go wrong:
 *
 * - the description counter, because a counter that disagrees with the host
 *   either stops the operator short of a description that would have been
 *   accepted, or reads green while the save is refused;
 * - the upload dialog's per-file rows, because a single failure line would
 *   throw away the files that worked and name none of them;
 * - the draft control's visibility, because a host with no drafter can only
 *   answer `no_model` and a button that answers nothing else is worse than no
 *   button.
 */

interface Posted {
  path: string;
  form: FormData;
}

function clientWith(options: {
  designsProfiles?: boolean;
  inferenceFails?: boolean;
  uploadResults?: unknown[];
  posted?: Posted[];
}): OpenCompanyClient {
  return {
    scopeFor: () => "/api/v1/companies/acme",
    get: (path: string) => {
      if (path.endsWith("/auth/me")) {
        return Promise.resolve({
          id: "u1",
          email: "a@b.c",
          role: "admin",
          company: "acme",
          hasPassword: true,
        });
      }
      if (path.endsWith("/inference")) {
        return options.inferenceFails
          ? Promise.reject(new Error("unreachable"))
          : Promise.resolve({
              designsProfiles: options.designsProfiles,
              canRebuildInPlace: false,
            });
      }
      if (path.endsWith("/skills/registry")) return Promise.resolve([]);
      if (path.endsWith("/skills")) return Promise.resolve([]);
      return Promise.reject(new Error(`unexpected GET ${path}`));
    },
    postForm: (path: string, form: FormData) => {
      options.posted?.push({ path, form });
      return Promise.resolve({ results: options.uploadResults ?? [] });
    },
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function show(client: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(SkillsView, { client, company: "acme" }));
  });
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

function click(el: Element | null | undefined) {
  return act(async () => {
    (el as HTMLElement).dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

function button(label: string, within: ParentNode = document.body): HTMLButtonElement {
  const found = [...within.querySelectorAll("button")].find((b) => b.textContent?.includes(label));
  if (!found) throw new Error(`no button labelled ${label}`);
  return found;
}

function testid(id: string): HTMLElement | null {
  return document.body.querySelector<HTMLElement>(`[data-testid="${id}"]`);
}

async function type(id: string, value: string) {
  const field = document.body.querySelector<HTMLInputElement | HTMLTextAreaElement>(`#${id}`);
  if (!field) throw new Error(`no field #${id}`);
  const proto =
    field instanceof HTMLTextAreaElement
      ? HTMLTextAreaElement.prototype
      : HTMLInputElement.prototype;
  Object.getOwnPropertyDescriptor(proto, "value")!.set!.call(field, value);
  await act(async () => {
    field.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

beforeEach(() => {
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

describe("skill description guidance", () => {
  it("counts what is typed against the host's own limit", async () => {
    await show(clientWith({ designsProfiles: true }));
    await click(button("Add skill"));

    expect(testid("skill-desc-count")?.textContent).toContain(`0 / ${SKILL_DESCRIPTION_MAX_CHARS}`);
    await type("skill-desc", "Pitch a story. Use when asked for press.");
    expect(testid("skill-desc-count")?.textContent).toContain(
      `40 / ${SKILL_DESCRIPTION_MAX_CHARS}`,
    );
  });

  it("says what a description is for, under the field", async () => {
    await show(clientWith({}));
    await click(button("Add skill"));

    const hint = testid("skill-desc-hint");
    expect(hint?.textContent).toContain("when an agent should use it");
  });

  it("refuses to submit a description past the limit rather than spending a round trip", async () => {
    await show(clientWith({}));
    await click(button("Add skill"));

    await type("skill-name", "Press Outreach");
    await type("skill-desc", "a".repeat(SKILL_DESCRIPTION_MAX_CHARS + 1));

    const dialog = document.body.querySelector('[role="dialog"]')!;
    expect(button("Add skill", dialog).disabled).toBe(true);
  });

  it("counts an astral character once, the way the host's chars().count() does", () => {
    // `.length` would say two: the operator would be stopped short of a
    // description the host would have taken.
    expect(skillDescriptionCount("🙂")).toBe(1);
  });
});

describe("skill upload dialog", () => {
  it("renders one row per file, so a bad file does not hide the good ones", async () => {
    const posted: Posted[] = [];
    await show(
      clientWith({
        posted,
        uploadResults: [
          {
            file: "good.md",
            ok: true,
            skill: { id: "press-outreach", name: "Press Outreach", scan: { findings: [] } },
          },
          { file: "broken.md", ok: false, error: "that file has no `name` in its frontmatter." },
        ],
      }),
    );
    await click(button("Upload"));

    const input = testid("skill-upload-input") as HTMLInputElement;
    const files = [
      new File(["---\nname: a\ndescription: b\n---\n"], "good.md"),
      new File(["nope"], "broken.md"),
    ];
    Object.defineProperty(input, "files", { value: files, configurable: true });
    await act(async () => {
      input.dispatchEvent(new Event("change", { bubbles: true }));
    });

    const dialog = document.body.querySelector('[role="dialog"]')!;
    await click(button("Upload", dialog));

    const rows = [...document.body.querySelectorAll('[data-testid="skill-upload-row"]')];
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain("press-outreach");
    expect(rows[1].textContent).toContain("no `name` in its frontmatter");
    expect(posted[0].path).toContain("/skills/upload");
    const sent = posted[0].form.getAll("file").map((f) => (f as File).name);
    expect(sent).toEqual(["good.md", "broken.md"]);
  });
});

describe("draft control visibility", () => {
  it("is offered when the host says it can draft", async () => {
    await show(clientWith({ designsProfiles: true }));

    expect(testid("skills-draft-trigger")).not.toBeNull();
  });

  it("is hidden when the host says it cannot, rather than answering no_model", async () => {
    await show(clientWith({ designsProfiles: false }));

    expect(testid("skills-draft-trigger")).toBeNull();
  });

  it("stays offered when the host did not say, since unknown is not no", async () => {
    // An older host omits the field, and a failed read says nothing either. A
    // network blip must not remove a working feature.
    await show(clientWith({ inferenceFails: true }));

    expect(testid("skills-draft-trigger")).not.toBeNull();
  });
});
