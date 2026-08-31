// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CreateDeskInput, DeskDto, TeamMemberDto } from "@/api/types";
import { DeskCreateDialog } from "@/views/company/DeskCreateDialog";

/**
 * Choosing the desk lead (issue #1802).
 *
 * The whole stack treats `members[0]` as the desk's lead (`chat/model.ts`,
 * `api/types.ts`). The creator used to encode that purely as selection order
 * with a Crown and a terse "1 · Lead" string, and the only way to change the
 * lead was to deselect everyone and re-pick. This makes the lead explicit: the
 * top row wears a "Lead" badge, every other selected row offers "Make lead",
 * and the summary names the current lead — all while keeping `members[0]` as
 * the single source of truth, which is what the posted payload must reflect.
 *
 * These are assertions about a mounted component — which row holds the badge,
 * what the summary says, and what actually gets posted — so they earn a jsdom
 * render rather than a unit test of a helper.
 */

const ROSTER: TeamMemberDto[] = [
  { id: "ada", name: "Ada", role: "engineer" },
  { id: "grace", name: "Grace", role: "engineer" },
  { id: "linus", name: "Linus", role: "engineer" },
];

function stubClient(createDesk: (...args: never[]) => Promise<DeskDto>) {
  return {
    listTeam: () => Promise.resolve(ROSTER),
    createDesk,
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;
let onOpenChange: ReturnType<typeof vi.fn>;
let onCreated: ReturnType<typeof vi.fn>;

/** The dialog renders through a portal, so it is on `document`, not `container`. */
function inDialog<T extends Element>(selector: string): T | null {
  return document.querySelector<T>(`[data-slot="dialog-content"] ${selector}`);
}

/** The select/deselect toggle for a teammate — the row's `[aria-pressed]` button. */
function toggle(name: string): HTMLButtonElement {
  const match = Array.from(
    document.querySelectorAll<HTMLButtonElement>('[data-slot="dialog-content"] [aria-pressed]'),
  ).find((button) => button.textContent?.includes(name));
  expect(match, `no roster toggle for ${name}`).toBeTruthy();
  return match as HTMLButtonElement;
}

/** The "Make lead" promote control in a teammate's row (a sibling of the toggle). */
function makeLead(name: string): HTMLButtonElement | null {
  const row = toggle(name).parentElement!;
  return row.querySelector<HTMLButtonElement>('[data-testid="desk-make-lead"]');
}

/** True when the named teammate's row is the one wearing the "Lead" badge. */
function isLead(name: string): boolean {
  return Boolean(toggle(name).parentElement!.querySelector('[data-testid="desk-lead-badge"]'));
}

/**
 * The numeric hierarchy position shown next to a selected non-lead teammate's
 * "Make lead" control, or `null` if the row does not render one (unselected,
 * or the lead itself — which wears the "Lead" badge instead).
 */
function position(name: string): string | null {
  const el = toggle(name).parentElement!.querySelector('[data-testid="desk-member-position"]');
  return el?.textContent ?? null;
}

/** The "Lead: <name>" summary line under the picker. */
function leadSummary(): string {
  const line = Array.from(
    document.querySelectorAll<HTMLElement>('[data-slot="dialog-content"] p'),
  ).find((p) => p.textContent?.startsWith("Lead:"));
  return line?.textContent ?? "";
}

function nameInput(): HTMLInputElement {
  const el = inDialog<HTMLInputElement>('[placeholder="e.g. Engineering"]');
  expect(el, "the Name field did not render").toBeTruthy();
  return el as HTMLInputElement;
}

function createButton(): HTMLButtonElement {
  const match = Array.from(
    document.querySelectorAll<HTMLButtonElement>('[data-slot="dialog-content"] button'),
  ).find((b) => b.textContent?.trim().startsWith("Create desk"));
  expect(match, 'no button labeled "Create desk"').toBeTruthy();
  return match as HTMLButtonElement;
}

/** Sets a controlled input the way a keystroke would, so React's `onChange` fires. */
function type(input: HTMLInputElement, value: string) {
  const setter = Object.getOwnPropertyDescriptor(
    HTMLInputElement.prototype,
    "value",
  )!.set!;
  setter.call(input, value);
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

async function open(client: OpenCompanyClient) {
  await act(async () => {
    root.render(
      createElement(DeskCreateDialog, {
        open: true,
        onOpenChange,
        onCreated,
        client,
        company: "acme",
      }),
    );
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  Element.prototype.scrollIntoView = vi.fn();
  onOpenChange = vi.fn();
  onCreated = vi.fn();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the desk creator lead picker", () => {
  it("makes the first-selected teammate the lead", async () => {
    await open(stubClient(() => Promise.reject(new Error("must not be called"))));

    await act(async () => {
      toggle("Ada").click();
      toggle("Grace").click();
    });

    // The first pick leads: it wears the badge and no "Make lead" of its own.
    expect(isLead("Ada")).toBe(true);
    expect(makeLead("Ada")).toBeNull();
    // The other selected teammate offers promotion instead.
    expect(isLead("Grace")).toBe(false);
    expect(makeLead("Grace")).toBeTruthy();
    // ...and the summary names the current lead.
    expect(leadSummary()).toContain("Ada");
  });

  it("keeps each non-lead teammate's hierarchy position visible alongside Make lead", async () => {
    // Regression for the Codex P2 finding on #1827: with 3+ selected, every
    // non-lead row used to show only "Make lead", with no way to tell Grace
    // (2nd) from Linus (3rd) — the seniority order the hint text asks the
    // operator to build stopped being verifiable the moment a third teammate
    // joined the selection.
    await open(stubClient(() => Promise.reject(new Error("must not be called"))));

    await act(async () => {
      toggle("Ada").click();
      toggle("Grace").click();
      toggle("Linus").click();
    });

    expect(isLead("Ada")).toBe(true);
    expect(position("Ada")).toBeNull();
    expect(position("Grace")).toBe("2");
    expect(position("Linus")).toBe("3");

    // Promoting Linus reshuffles who is 2nd vs 3rd, not just who is lead.
    await act(async () => {
      makeLead("Linus")!.click();
    });

    expect(isLead("Linus")).toBe(true);
    expect(position("Linus")).toBeNull();
    expect(position("Ada")).toBe("2");
    expect(position("Grace")).toBe("3");
  });

  it("keeps focus on the promoted teammate's row after Make lead", async () => {
    // Regression for the Codex accessibility finding on #1827: clicking
    // "Make lead" removes that very button from the DOM (the promoted row
    // switches to the non-focusable "Lead" badge), which used to drop focus
    // to document.body — stranding a keyboard/AT user outside the dialog in
    // a long roster. Focus should land on the promoted row's persistent
    // select/deselect toggle instead.
    await open(stubClient(() => Promise.reject(new Error("must not be called"))));

    await act(async () => {
      toggle("Ada").click();
      toggle("Grace").click();
    });
    expect(isLead("Ada")).toBe(true);

    const promoteButton = makeLead("Grace")!;
    await act(async () => {
      promoteButton.focus();
      promoteButton.click();
    });

    expect(isLead("Grace")).toBe(true);
    // Focus followed the promotion to Grace's row, not out to document.body.
    expect(document.activeElement).toBe(toggle("Grace"));
  });

  it("moves the lead when a non-lead is promoted", async () => {
    await open(stubClient(() => Promise.reject(new Error("must not be called"))));

    await act(async () => {
      toggle("Ada").click();
      toggle("Grace").click();
    });
    expect(isLead("Ada")).toBe(true);

    await act(async () => {
      makeLead("Grace")!.click();
    });

    // The badge and the summary both move to Grace; Ada now offers "Make lead".
    expect(isLead("Grace")).toBe(true);
    expect(isLead("Ada")).toBe(false);
    expect(makeLead("Ada")).toBeTruthy();
    expect(leadSummary()).toContain("Grace");
  });

  it("posts the promoted teammate as members[0]", async () => {
    let captured: CreateDeskInput | undefined;
    const createDesk = vi.fn((input: CreateDeskInput) => {
      captured = input;
      return Promise.resolve({ id: "desk-1", name: input.name } as DeskDto);
    });
    await open(stubClient(createDesk as (...args: never[]) => Promise<DeskDto>));

    await act(async () => {
      type(nameInput(), "Engineering");
    });
    await act(async () => {
      toggle("Ada").click();
      toggle("Grace").click();
      toggle("Linus").click();
    });
    await act(async () => {
      makeLead("Linus")!.click();
    });
    await act(async () => {
      createButton().click();
    });

    expect(createDesk).toHaveBeenCalledTimes(1);
    expect(captured?.members?.[0]).toBe("linus");
    // The rest are still there, just behind the new lead.
    expect(captured?.members).toContain("ada");
    expect(captured?.members).toContain("grace");
  });

  it("promotes the next teammate when the current lead is deselected", async () => {
    await open(stubClient(() => Promise.reject(new Error("must not be called"))));

    await act(async () => {
      toggle("Ada").click();
      toggle("Grace").click();
    });
    expect(isLead("Ada")).toBe(true);

    // Deselecting the lead hands the slot to the next-in-line.
    await act(async () => {
      toggle("Ada").click();
    });

    expect(isLead("Grace")).toBe(true);
    expect(leadSummary()).toContain("Grace");
    // Ada is no longer selected, so it has neither badge nor promote control.
    expect(toggle("Ada").getAttribute("aria-pressed")).toBe("false");
    expect(makeLead("Ada")).toBeNull();
  });
});
