// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import type { WorkflowGraph } from "@/api/workflows";
import { WorkflowCreateDialog } from "@/views/WorkflowCreateDialog";

/**
 * Issue #1006: the edit dialog must not destroy unsaved graph edits, and must
 * not be able to lock itself shut.
 *
 * Both claims are about a mounted dialog reacting to a click — what a Cancel
 * press does when the form has been touched, and whether Cancel still works
 * after a failed save. Neither is reachable from a pure helper, so this earns
 * the same jsdom exception as `setup-wizard-finish-gate`: mount it, click it,
 * assert what came back out.
 *
 * The serialisation half is driven through a mocked `configFromDraft` on
 * purpose. Its failure branch means the form's validation and the serializer
 * DISAGREED, which by construction no keystroke can produce — so the only
 * honest way to pin "and then the operator can still get out" is to make them
 * disagree.
 */

const { configFromDraft } = vi.hoisted(() => ({ configFromDraft: vi.fn() }));

vi.mock("@/lib/workflow-node-config", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/workflow-node-config")>();
  configFromDraft.mockImplementation(actual.configFromDraft);
  return { ...actual, configFromDraft };
});

/** A saved graph the dialog hydrates from: valid, so `validate()` passes and
 * Save reaches the serialisation step. */
function savedGraph(): WorkflowGraph {
  return {
    id: "weekly_report",
    name: "Weekly report",
    description: "Assemble and send the Monday summary.",
    version: "v1",
    nodes: [
      { id: "start", kind: "trigger", name: "Start" },
      { id: "search", kind: "tool_call", name: "Search", config: { slug: "web_search" } },
    ],
    edges: [{ from: "start", to: "search" }],
  } as WorkflowGraph;
}

/** A host that answers every read the dialog makes on open, and records writes.
 * `put` rejects if it is ever called: a save that failed to serialise must not
 * have reached the host. */
function stubClient(): OpenCompanyClient & { puts: unknown[] } {
  const puts: unknown[] = [];
  return {
    puts,
    scopeFor: () => "/api/companies/acme",
    listTeam: async () => [],
    get: async (path: string) =>
      path.endsWith("/wired-channels") ? { channels: [] } : [],
    // The dialog debounces a `/workflows/validate` preflight while mounted and
    // the callback can fire after a test body has already returned — the same
    // stray-callback class as the `scrollIntoView` shim above. `savedGraph` is
    // valid, so answering the preflight with `valid: true` is what a real host
    // would say, and no assertion in this file reads preflight state.
    post: async () => ({ valid: true }),
    put: async (_path: string, body: unknown) => {
      puts.push(body);
      return savedGraph();
    },
  } as unknown as OpenCompanyClient & { puts: unknown[] };
}

/**
 * jsdom implements no scrolling at all, so `Element.prototype.scrollIntoView`
 * simply does not exist. The dialog scrolls its error banner into view from a
 * `requestAnimationFrame` after a failed save, which puts the resulting
 * TypeError outside every test body — Vitest reports 899 passing tests and
 * then fails the run on one unhandled error. Supplying the missing method is
 * the honest fix: the production call is correct in a browser, and guarding it
 * in `WorkflowCreateDialog` would only be guarding against jsdom.
 */
const originalScrollIntoView = Object.getOwnPropertyDescriptor(
  Element.prototype,
  "scrollIntoView",
);

if (!Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = () => {};
}

let container: HTMLDivElement;
let root: Root;
let confirms: string[];

/**
 * The discard confirm, or `null` when the dialog is not asking (defect B-081).
 *
 * Looked up by test id rather than by button label so a reworded prompt does
 * not silently stop testing the guard.
 */
function discardAsk(): HTMLElement | null {
  return document.body.querySelector<HTMLElement>('[data-testid="workflow-discard-confirm"]');
}

/** Answer the discard confirm. */
async function answerDiscard(answer: "leave" | "keep") {
  expect(discardAsk(), "the discard confirm is not open").toBeTruthy();
  await click(find(`[data-testid="workflow-discard-${answer === "leave" ? "leave" : "keep"}"]`));
}

/** The dialog portals into `document.body`, not into the mount container. */
function find<T extends Element>(selector: string): T {
  const el = document.body.querySelector<T>(selector);
  expect(el, `no element matching ${selector}`).toBeTruthy();
  return el as T;
}

function button(label: string): HTMLButtonElement {
  const buttons = Array.from(document.body.querySelectorAll("button"));
  const match = buttons.find((b) => b.textContent?.trim() === label);
  expect(match, `no button labeled "${label}"`).toBeTruthy();
  return match as HTMLButtonElement;
}

/** Type into a controlled `<input>`/`<textarea>` the way React reads it. */
async function type(el: HTMLInputElement | HTMLTextAreaElement, value: string) {
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(
      el instanceof HTMLTextAreaElement
        ? HTMLTextAreaElement.prototype
        : HTMLInputElement.prototype,
      "value",
    )!.set!;
    setter.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function click(el: HTMLElement) {
  await act(async () => {
    el.click();
  });
}

/** `workflow` defaults to a saved graph (edit mode); pass `null` for create. */
async function open(
  client: OpenCompanyClient,
  onOpenChange: (o: boolean) => void,
  workflow: WorkflowGraph | null = savedGraph(),
) {
  await act(async () => {
    root.render(
      createElement(WorkflowCreateDialog, {
        client,
        company: "acme",
        open: true,
        onOpenChange,
        workflow,
      }),
    );
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  confirms = [];
  // Defect B-081: the trapping environment, made the default for every test in
  // this file. A `window.confirm` that answers `false` without asking anybody
  // is what Chrome does after "prevent this page from creating additional
  // dialogs", what an automation-driven browser does, and what the `src-tauri`
  // webview does — and under it every exit from this dialog silently did
  // nothing. Stubbing it to `true`, as this file used to, is a world in which
  // the bug cannot happen; the guard has to hold in the world where it can.
  vi.stubGlobal("confirm", (message: string) => {
    confirms.push(message);
    return false;
  });
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
  configFromDraft.mockClear();
});

// The shim stays up for the whole FILE, not per test: a failed save scrolls the
// error banner from a `requestAnimationFrame`, and the callback can fire after
// a test body has already returned. Removing the shim in `afterEach` strands
// that callback with no `scrollIntoView` and Vitest reports an unhandled error.
// `afterAll` keeps it present throughout the file and still restores the
// prototype so it does not leak into a later jsdom test sharing this worker
// (same discipline as `chat-scroll-anchor.test.ts`).
afterAll(() => {
  if (originalScrollIntoView) {
    Object.defineProperty(Element.prototype, "scrollIntoView", originalScrollIntoView);
  } else {
    delete (Element.prototype as unknown as Record<string, unknown>).scrollIntoView;
  }
});

describe("unsaved graph edits are not thrown away silently (#1006)", () => {
  it("names the workflow being edited, so a swapped selection is visible", async () => {
    await open(stubClient(), () => {});
    expect(document.body.textContent).toContain("Weekly report");
  });

  it("closes without asking when nothing has been edited", async () => {
    const closes: boolean[] = [];
    await open(stubClient(), (o) => closes.push(o));

    await click(button("Cancel"));

    expect(confirms).toEqual([]);
    expect(closes).toEqual([false]);
  });

  it("asks before Cancel discards an edit, and stays open when declined", async () => {
    const closes: boolean[] = [];
    await open(stubClient(), (o) => closes.push(o));
    const name = find<HTMLInputElement>('input[id$="-name"]');
    await type(name, "Weekly report v2");

    await click(button("Cancel"));

    // The ask is the console's own, so it is on screen and answerable.
    expect(discardAsk()).toBeTruthy();
    expect(closes).toEqual([]);

    await answerDiscard("keep");
    // Declined: not closed, the ask is gone, and the edit is still in the
    // form, which is the whole point.
    expect(discardAsk()).toBeNull();
    expect(closes).toEqual([]);
    expect(find<HTMLInputElement>('input[id$="-name"]').value).toBe("Weekly report v2");

    await click(button("Cancel"));
    await answerDiscard("leave");
    expect(closes).toEqual([false]);
  });

  it("asks before Esc discards an edit", async () => {
    const closes: boolean[] = [];
    await open(stubClient(), (o) => closes.push(o));
    await type(find<HTMLTextAreaElement>('textarea[id$="-desc"]'), "changed");

    await act(async () => {
      document.body
        .querySelector('[data-slot="dialog-content"]')!
        .dispatchEvent(
          new KeyboardEvent("keydown", { key: "Escape", bubbles: true }),
        );
    });

    expect(discardAsk()).toBeTruthy();
    expect(closes).toEqual([]);

    await answerDiscard("leave");
    expect(closes).toEqual([false]);
  });

  it("guards a reload while dirty, and stops guarding once it is not", async () => {
    await open(stubClient(), () => {});
    const beforeUnload = () => {
      const e = new Event("beforeunload", { cancelable: true });
      window.dispatchEvent(e);
      return e.defaultPrevented;
    };
    // A pristine form has nothing to warn about.
    expect(beforeUnload()).toBe(false);

    await type(find<HTMLInputElement>('input[id$="-name"]'), "Weekly report v2");
    expect(beforeUnload()).toBe(true);

    // Typed back to what was saved: no longer dirty, so no warning.
    await type(find<HTMLInputElement>('input[id$="-name"]'), "Weekly report");
    expect(beforeUnload()).toBe(false);
  });
});

describe("a serialisation failure leaves the dialog closable (#1006)", () => {
  it("reports it, keeps the draft, sends nothing, and lets Cancel out", async () => {
    const client = stubClient();
    const closes: boolean[] = [];
    await open(client, (o) => closes.push(o));
    await type(find<HTMLInputElement>('input[id$="-name"]'), "Weekly report v2");

    configFromDraft.mockReturnValue({ ok: false, error: "Arguments must be valid JSON." });
    await click(find<HTMLButtonElement>('[data-testid="workflow-dialog-submit"]'));

    // The failure is reported against the node that caused it…
    const banner = find('[data-testid="create-error"]');
    expect(banner.textContent).toContain("Search");
    expect(banner.textContent).toContain("Arguments must be valid JSON.");
    // …nothing was written…
    expect(client.puts).toEqual([]);
    // …the draft survived…
    expect(find<HTMLInputElement>('input[id$="-name"]').value).toBe("Weekly report v2");
    // …and the way out is not disabled, which is what `submitting` stuck true
    // used to do. Before the fix this button stayed disabled forever.
    const cancel = button("Cancel");
    expect(cancel.disabled).toBe(false);

    await click(cancel);
    await answerDiscard("leave");
    expect(closes).toEqual([false]);
  });
});

/**
 * Defect B-081: a rejected save must not leave the dialog inescapable.
 *
 * The reported repro is a 409 on a duplicate workflow id, but the 409 leaves no
 * state of its own — it lands in the same generic error branch a 500 does. What
 * it does is force an operator who has necessarily typed something (so: dirty)
 * to reach for Cancel, which is the first moment anyone meets the guard. So the
 * property under test is the one that actually failed: **on a dirty form, in an
 * environment whose `window.confirm` answers nobody, every exit still works.**
 *
 * `beforeEach` stubs `confirm` to return `false` for the whole file, which is
 * that environment. Before the fix, each assertion below found the dialog still
 * open with nothing drawn and nothing logged.
 */
describe("a dirty dialog is escapable without window.confirm (B-081)", () => {
  /** Dirty a New-workflow form and answer the host's create with a 409. */
  async function afterRejectedSave(closes: boolean[]) {
    const client = stubClient();
    const preflight = client.post.bind(client);
    // The create POST, not the `/workflows/validate` preflight beside it.
    (client as { post: unknown }).post = async (path: string, body: unknown) =>
      path.endsWith("/workflows")
        ? Promise.reject(
            new ApiError(409, "conflict", "a workflow with this ID already exists", true),
          )
        : preflight(path, body);

    await open(client, (o) => closes.push(o), null);
    await type(find<HTMLInputElement>('input[id$="-name"]'), "Weekly report");
    await click(find<HTMLButtonElement>('[data-testid="workflow-dialog-submit"]'));
    // Create mode asks the operator to confirm the permanent id first (#1808).
    if (document.body.querySelector('[data-testid="workflow-id-confirm"]')) {
      await click(find('[data-testid="workflow-id-confirm-create"]'));
    }
    expect(find('[data-testid="create-error"]').textContent).toContain("already exists");
  }

  it("never consults window.confirm for the discard question at all", async () => {
    const closes: boolean[] = [];
    await afterRejectedSave(closes);

    await click(button("Cancel"));

    // The whole defect in one line: the answer is asked for in the console,
    // where it can be given, not through a primitive that answers `false` on
    // the operator's behalf.
    expect(confirms).toEqual([]);
    expect(discardAsk()).toBeTruthy();
  });

  it.each([
    ["Cancel", async () => void (await click(button("Cancel")))],
    [
      "Escape",
      async () =>
        void (await act(async () => {
          document.body
            .querySelector('[data-slot="dialog-content"]')!
            .dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
        })),
    ],
  ])("lets %s out after a 409", async (_label, exit) => {
    const closes: boolean[] = [];
    await afterRejectedSave(closes);

    await exit();
    await answerDiscard("leave");

    expect(closes).toEqual([false]);
  });

  it("keeps the draft when the operator decides to stay", async () => {
    const closes: boolean[] = [];
    await afterRejectedSave(closes);

    await click(button("Cancel"));
    await answerDiscard("keep");

    expect(closes).toEqual([]);
    expect(find<HTMLInputElement>('input[id$="-name"]').value).toBe("Weekly report");
  });

  it("reads a dismissed ask as 'keep editing', never as consent to discard", async () => {
    const closes: boolean[] = [];
    await afterRejectedSave(closes);

    await click(button("Cancel"));
    await act(async () => {
      discardAsk()!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    });

    // Escaping the *question* is not answering it. The graph survives.
    expect(closes).toEqual([]);
    expect(find<HTMLInputElement>('input[id$="-name"]').value).toBe("Weekly report");
  });
});
