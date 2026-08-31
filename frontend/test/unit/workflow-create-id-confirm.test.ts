// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { WorkflowGraph } from "@/api/workflows";
import { WorkflowCreateDialog } from "@/views/WorkflowCreateDialog";

/**
 * The create-time id confirm (issue #1808).
 *
 * The workflow id is a permanent backend join key — it keys the overlay body,
 * the revision store, the scheduler's armed state, run history, and cross-graph
 * `sub_workflow` references — so a rename is a 400. Creation is the only moment
 * it can be set, and in create mode it is silently derived from the name, so a
 * name typo becomes a permanent id with no acknowledgement. This adds a confirm
 * step in create mode ONLY: the first Create opens the confirm showing the exact
 * id the write will send, and the confirm's own action runs the single write.
 *
 * These are claims about what the DOM does across a click sequence — a confirm
 * that must appear, a write that must NOT fire until it is accepted, and a save
 * path that must skip it entirely in edit mode — so they render the component
 * the same way `workflow-create-problems.test.ts` and `ledger-retire-confirm`
 * do, rather than a pure helper.
 */

/** The workflows write scope this fake client reports. */
const SCOPE = "/api/v1/companies/acme";

/**
 * Stubs the client verbs the dialog reaches. `post` serves BOTH the debounced
 * host pre-flight (`POST …/workflows/validate`) and the create write
 * (`POST …/workflows`); they are told apart by path so `onCreate` only ever
 * counts real creates. The optional-picker GETs and the roster reject, which
 * the dialog degrades to free-text fallbacks — none of it blocks authoring.
 */
function stubClient(opts: {
  onCreate?: (path: string, body: unknown) => void;
  put?: (path: string, body?: unknown) => Promise<unknown>;
}): OpenCompanyClient {
  return {
    scopeFor: () => SCOPE,
    get: () => Promise.reject(new Error("not offered by this host")),
    listTeam: () => Promise.reject(new Error("not offered by this host")),
    post: (path: string, body?: unknown) => {
      if (path.endsWith("/workflows/validate")) {
        return Promise.resolve({ valid: true });
      }
      opts.onCreate?.(path, body);
      return Promise.resolve(body as WorkflowGraph);
    },
    put: opts.put ?? (() => Promise.reject(new Error("no put expected"))),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;
let onOpenChange: ReturnType<typeof vi.fn>;

function inDialog<T extends Element>(selector: string): T | null {
  return document.querySelector<T>(`[data-slot="dialog-content"] ${selector}`);
}

/** The main form's Create/Save button. */
function submitButton(): HTMLButtonElement {
  return inDialog<HTMLButtonElement>('[data-testid="workflow-dialog-submit"]')!;
}

/** The confirm modal (portalled onto `document.body`), or null when it is closed. */
function confirmDialog(): HTMLElement | null {
  return document.querySelector<HTMLElement>('[data-testid="workflow-id-confirm"]');
}

function confirmValue(): string | null {
  return (
    document
      .querySelector('[data-testid="workflow-id-confirm-value"]')
      ?.textContent?.trim() ?? null
  );
}

/** A button in the confirm modal by its testid. */
function confirmButton(testid: string): HTMLButtonElement {
  const el = document.querySelector<HTMLButtonElement>(`[data-testid="${testid}"]`);
  if (!el) throw new Error(`no “${testid}” in:\n${document.body.innerHTML}`);
  return el;
}

/** Sets a controlled input the way a keystroke would. */
function type(selector: string, value: string) {
  const input = inDialog<HTMLInputElement>(selector);
  expect(input, `no input matching ${selector}`).toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(
    HTMLInputElement.prototype,
    "value",
  )!.set!;
  setter.call(input, value);
  input!.dispatchEvent(new Event("input", { bubbles: true }));
}

const NAME_INPUT = 'input[placeholder="e.g. Campaign pipeline"]';
const ID_INPUT = 'input[placeholder="e.g. campaign_pipeline"]';

function idFieldValue(): string {
  return inDialog<HTMLInputElement>(ID_INPUT)!.value;
}

async function openCreate(client: OpenCompanyClient) {
  await act(async () => {
    root.render(
      createElement(WorkflowCreateDialog, {
        open: true,
        onOpenChange,
        client,
        company: "acme",
      }),
    );
  });
}

async function openEditing(client: OpenCompanyClient, workflow: WorkflowGraph) {
  await act(async () => {
    root.render(
      createElement(WorkflowCreateDialog, {
        open: true,
        onOpenChange,
        client,
        company: "acme",
        workflow,
      }),
    );
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  Element.prototype.scrollIntoView = vi.fn();
  onOpenChange = vi.fn();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("create-time id confirm (issue #1808)", () => {
  it("opens the confirm on Create and does not write, showing the derived id", async () => {
    const onCreate = vi.fn();
    await openCreate(stubClient({ onCreate }));

    // A name derives the id (the field is untouched), so this is the exact
    // silent path the confirm exists to surface.
    await act(async () => {
      type(NAME_INPUT, "Weekly Digest");
    });
    const derived = idFieldValue();
    expect(derived, "the name should have derived a non-empty id").toBeTruthy();

    await act(async () => {
      submitButton().click();
    });

    // The write has NOT fired — the confirm stands between Create and the post.
    expect(onCreate).not.toHaveBeenCalled();
    expect(confirmDialog(), "the confirm modal did not open").toBeTruthy();
    // …and it surfaces the exact id that would be written.
    expect(confirmValue()).toBe(derived);
  });

  it("writes exactly once with the shown id when the confirm is accepted, then closes", async () => {
    const onCreate = vi.fn();
    await openCreate(stubClient({ onCreate }));

    await act(async () => {
      type(NAME_INPUT, "Weekly Digest");
    });
    const shown = idFieldValue();

    await act(async () => {
      submitButton().click();
    });
    expect(confirmValue()).toBe(shown);

    await act(async () => {
      confirmButton("workflow-id-confirm-create").click();
    });

    // Exactly one create, carrying the id the confirm displayed, and the dialog
    // closes on success.
    expect(onCreate).toHaveBeenCalledTimes(1);
    const posted = onCreate.mock.calls[0][1] as WorkflowGraph;
    expect(posted.id).toBe(shown);
    expect(onOpenChange).toHaveBeenCalledWith(false);
    expect(confirmDialog()).toBeNull();
  });

  it("Back cancels without writing; re-Create after editing the id shows the NEW id", async () => {
    const onCreate = vi.fn();
    await openCreate(stubClient({ onCreate }));

    await act(async () => {
      type(NAME_INPUT, "Weekly Digest");
    });
    await act(async () => {
      submitButton().click();
    });
    expect(confirmDialog()).toBeTruthy();

    // Back — no write, confirm dismissed.
    await act(async () => {
      confirmButton("workflow-id-confirm-back").click();
    });
    expect(onCreate).not.toHaveBeenCalled();
    expect(confirmDialog()).toBeNull();

    // Correct the id, then Create again — the confirm must show what would be
    // written now, not the stale preview it showed before Back.
    await act(async () => {
      type(ID_INPUT, "weekly_digest_v2");
    });
    await act(async () => {
      submitButton().click();
    });
    expect(confirmDialog()).toBeTruthy();
    expect(confirmValue()).toBe("weekly_digest_v2");
    expect(onCreate).not.toHaveBeenCalled();
  });

  it("edit mode saves directly through updateWorkflow with no confirm", async () => {
    const workflow: WorkflowGraph = {
      id: "greeter",
      name: "Greeter",
      version: "v1",
      nodes: [
        { id: "start", kind: "trigger", name: "Start" },
        { id: "greet", kind: "agent", name: "Greet", agent: "alice" },
      ],
      edges: [{ from: "start", to: "greet" }],
    };
    const put = vi.fn((_path: string, _body?: unknown) => Promise.resolve(workflow));
    await openEditing(stubClient({ put }), workflow);

    await act(async () => {
      submitButton().click();
    });

    // The id is fixed in edit mode — the confirm never renders, and Save writes
    // straight through.
    expect(confirmDialog()).toBeNull();
    expect(put).toHaveBeenCalledTimes(1);
    expect(put.mock.calls[0][0]).toContain(`/workflows/${workflow.id}`);
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });

  it("a client-validation failure blocks before any confirm or write", async () => {
    const onCreate = vi.fn();
    await openCreate(stubClient({ onCreate }));

    // No name and no id — `validate()` refuses at the id check, before the
    // confirm gate is ever reached.
    await act(async () => {
      submitButton().click();
    });

    expect(confirmDialog()).toBeNull();
    expect(onCreate).not.toHaveBeenCalled();
    const banner = inDialog<HTMLElement>('[data-testid="create-error"]');
    expect(banner, "the validation banner did not render").toBeTruthy();
    expect(banner!.textContent).toContain("id");
  });
});
