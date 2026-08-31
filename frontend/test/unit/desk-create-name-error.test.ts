// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { DeskDto, TeamMemberDto } from "@/api/types";
import { DeskCreateDialog } from "@/views/company/DeskCreateDialog";

/**
 * Where the desk creator puts its refusal (issue #1100).
 *
 * Before this, "Give the desk a name." was set into the same state the host's
 * refusal uses, and that state renders once — after the Name field, the
 * Description field and the whole roster picker, inside a
 * `max-h-[85vh] overflow-y-auto` dialog. On a company with a real roster the
 * message therefore landed below the fold while the empty field sat at the top,
 * nothing scrolled to it and nothing focused the input, so pressing Create
 * looked like pressing nothing.
 *
 * These assertions are all about a mounted component: which node holds the
 * message, where that node sits relative to the field, what the input's ARIA
 * says, and where focus went. None of that survives extraction into a helper,
 * which is why this earns a jsdom render.
 */

const ROSTER: TeamMemberDto[] = Array.from({ length: 12 }, (_, i) => ({
  id: `member-${i}`,
  name: `Teammate ${i}`,
  role: "engineer",
}));

function stubClient(createDesk: () => Promise<DeskDto>) {
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

function rosterButton(name: string): HTMLButtonElement {
  const match = Array.from(
    document.querySelectorAll<HTMLButtonElement>('[data-slot="dialog-content"] [aria-pressed]'),
  ).find((button) => button.textContent?.includes(name));
  expect(match, `no roster button for ${name}`).toBeTruthy();
  return match as HTMLButtonElement;
}

/** Sets a controlled input the way a keystroke would: through the native value
 * setter, so React's own descriptor sees the change and `onChange` fires. */
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

/** Presses Create and lets the post-commit frame run, which is where the focus
 * and scroll move live. */
async function pressCreate() {
  await act(async () => {
    createButton().click();
  });
  await act(async () => {
    await new Promise((r) => requestAnimationFrame(r));
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  // jsdom does no layout and ships no `scrollIntoView`; the component calls it
  // on the frame after the refusal renders, where a throw would surface as an
  // unhandled error rather than a test failure. Stubbed here rather than
  // guarded in the component — the call is correct in a browser, and WHERE the
  // container ends up scrolled is a layout property the e2e suite owns.
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

describe("the desk creator when the name is empty", () => {
  it("puts the message at the Name field, focuses it, and never posts", async () => {
    const createDesk = vi.fn(() => Promise.reject(new Error("must not be called")));
    await open(stubClient(createDesk));
    // The picker really is long enough for the old bottom slot to be off-screen
    // in a browser — this is the geometry the issue is about.
    expect(document.querySelectorAll('[data-slot="dialog-content"] [aria-pressed]').length).toBe(
      ROSTER.length,
    );

    await pressCreate();

    expect(createDesk).not.toHaveBeenCalled();
    expect(onOpenChange).not.toHaveBeenCalled();

    const message = inDialog<HTMLElement>('[data-testid="desk-name-error"]');
    expect(message, "the refusal did not render").toBeTruthy();
    expect(message!.textContent).toBe("Give the desk a name.");

    // Adjacent to the field it is about: same field group, immediately after
    // the input — not the bottom of the form, past the whole roster.
    const input = nameInput();
    expect(input.nextElementSibling).toBe(message);
    expect(message!.parentElement).toBe(input.parentElement);
    const firstRosterButton = inDialog<HTMLElement>("[aria-pressed]")!;
    expect(
      message!.compareDocumentPosition(firstRosterButton) &
        Node.DOCUMENT_POSITION_FOLLOWING,
      "the message renders after the roster picker",
    ).toBeTruthy();
    // ...and the whole-form banner, which belongs to host refusals, stayed away.
    expect(inDialog('[data-testid="desk-create-error"]')).toBeNull();

    // Announced, not merely rendered: without these a screen-reader operator
    // gets exactly the silence a sighted one used to get.
    expect(message!.getAttribute("role")).toBe("alert");
    expect(input.getAttribute("aria-invalid")).toBe("true");
    expect(input.getAttribute("aria-describedby")).toBe(message!.id);
    expect(message!.id).toBeTruthy();

    // ...and the operator is taken to the problem rather than left looking at
    // an unchanged screen.
    expect(document.activeElement).toBe(input);
    expect(Element.prototype.scrollIntoView).toHaveBeenCalled();
  });

  it("retires the message on the first keystroke", async () => {
    await open(stubClient(() => Promise.reject(new Error("must not be called"))));
    await pressCreate();
    expect(inDialog('[data-testid="desk-name-error"]')).toBeTruthy();

    await act(async () => {
      type(nameInput(), "Engineering");
    });

    expect(inDialog('[data-testid="desk-name-error"]')).toBeNull();
    expect(nameInput().getAttribute("aria-invalid")).toBe("false");
    expect(nameInput().getAttribute("aria-describedby")).toBeNull();
  });
});

describe("the desk creator when the host refuses", () => {
  it("keeps that in the whole-form banner, away from the Name field", async () => {
    const createDesk = vi.fn(() =>
      Promise.reject(new Error("a desk named “Engineering” already exists")),
    );
    await open(stubClient(createDesk));

    await act(async () => {
      type(nameInput(), "Engineering");
    });
    await pressCreate();

    expect(createDesk).toHaveBeenCalledTimes(1);
    const banner = inDialog<HTMLElement>('[data-testid="desk-create-error"]');
    expect(banner, "the host's refusal did not render").toBeTruthy();
    expect(banner!.textContent).toContain("already exists");
    expect(banner!.getAttribute("role")).toBe("alert");
    // It is about the form, not the field, so the field is not marked invalid.
    expect(inDialog('[data-testid="desk-name-error"]')).toBeNull();
    expect(nameInput().getAttribute("aria-invalid")).toBe("false");
    // The draft survives, and the form is live again to retry.
    expect(nameInput().value).toBe("Engineering");
    expect(onOpenChange).not.toHaveBeenCalled();
    expect(createButton().disabled).toBe(false);
  });
});

describe("the desk creator teammate picker", () => {
  it("filters a long roster and makes the chosen order visible", async () => {
    await open(stubClient(() => Promise.reject(new Error("must not be called"))));

    const filter = inDialog<HTMLInputElement>('[data-testid="desk-member-filter"]');
    expect(filter, "long rosters need a name filter").toBeTruthy();
    expect(rosterButton("Teammate 0").getAttribute("aria-pressed")).toBe("false");

    await act(async () => {
      rosterButton("Teammate 0").click();
      rosterButton("Teammate 2").click();
    });

    const lead = rosterButton("Teammate 0");
    const second = rosterButton("Teammate 2");
    expect(lead.getAttribute("aria-pressed")).toBe("true");
    // The lead carries the "Lead" badge (a row sibling of the toggle), the
    // non-lead carries a "Make lead" promote control instead.
    expect(lead.parentElement!.querySelector('[data-testid="desk-lead-badge"]')).toBeTruthy();
    expect(lead.querySelector(".lucide-check")).toBeTruthy();
    expect(second.getAttribute("aria-pressed")).toBe("true");
    expect(second.parentElement!.querySelector('[data-testid="desk-make-lead"]')).toBeTruthy();

    await act(async () => {
      type(filter!, "Teammate 2");
    });

    expect(rosterButton("Teammate 2")).toBeTruthy();
    expect(
      Array.from(document.querySelectorAll('[data-slot="dialog-content"] [aria-pressed]')).some((button) =>
        button.textContent?.includes("Teammate 0"),
      ),
    ).toBe(false);
  });
});
