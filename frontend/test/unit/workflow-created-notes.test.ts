// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { WorkflowGraph } from "@/api/workflows";

/**
 * Where the copilot's corrections are read now that the dialog closes on them.
 *
 * The one-box New-workflow dialog drafts, saves and lands on the canvas in a
 * single gesture. The host's `notes[]` — "matched the teammate you named by
 * role to `writer`" — used to be read in the dialog, beside the form the draft
 * hydrated. With no such form and no such pause, they would be written, applied
 * and never shown: the graph would quietly not say what the operator asked for.
 *
 * The tricky half, and the reason this is a rendered test rather than a helper:
 * a create MOVES THE SELECTION, and the selection-change effect is what sweeps
 * every stale banner off the canvas. The notes are set and the selection is
 * moved in the same batch, so a sweep that could not tell "navigated away from
 * it" from "just this instant arrived on it" would erase them on the very
 * render meant to show them.
 */

vi.mock("sonner", () => ({
  toast: Object.assign(vi.fn(), {
    success: vi.fn(),
    error: vi.fn(),
    warning: vi.fn(),
    info: vi.fn(),
  }),
}));
vi.mock("next-themes", () => ({ useTheme: () => ({ resolvedTheme: "light" }) }));

// React Flow measures its container on mount; jsdom has no layout and no
// `ResizeObserver`. Same setup as `workflow-week1-nudge-view.test.ts`.
class NoopResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}
Object.assign(globalThis, {
  ResizeObserver: NoopResizeObserver,
  DOMMatrixReadOnly: class {
    m22 = 1;
  },
});
Object.defineProperties(globalThis.HTMLElement.prototype, {
  offsetHeight: { get: () => 400 },
  offsetWidth: { get: () => 800 },
});

const CREATED: WorkflowGraph = {
  id: "weekly-digest",
  name: "Weekly digest",
  description: "Every Monday, draft the digest.",
  nodes: [{ id: "start", kind: "trigger", name: "Start" }],
  edges: [],
  editable: true,
  enabled: true,
  version: "v1",
};

const NOTES = ["Matched “the writer” to teammate `writer`.", "Set the trigger to Mondays at 09:00."];

/** What `onCreated` is called with, chosen per test. */
let createdNotes: string[] | undefined;

// The real dialog runs the whole draft-then-write flow, which
// `workflow-one-box-dialog.test.ts` covers. Stubbed here down to the one call
// this suite is about: the create landing on the view with its notes.
vi.mock("@/views/WorkflowCreateDialog", () => ({
  WorkflowCreateDialog: (props: {
    open: boolean;
    workflow?: unknown;
    onCreated?: (graph: WorkflowGraph, notes?: string[]) => void;
  }) => {
    if (!props.open || props.workflow) return null;
    return createElement(
      "button",
      {
        "data-testid": "test-confirm-create",
        onClick: () => props.onCreated?.(CREATED, createdNotes),
      },
      "confirm create",
    );
  },
}));

const { WorkflowsView } = await import("@/views/WorkflowsView");

function graphFor(id: string): WorkflowGraph {
  return { ...CREATED, id, name: id };
}

function client(): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    get: async (path: string) => {
      // Both companies list the SAME id, which is the whole point of the
      // company-switch case below: ids are unique only within a company, so a
      // sweep keyed on the workflow alone still matches after the switch.
      if (path.endsWith("/workflows"))
        return [
          { id: "other", name: "Other", enabled: true },
          { id: CREATED.id, name: CREATED.name, enabled: true },
        ];
      if (path.includes("/workflows/tool-slugs")) return { slugs: [], unwired: [] };
      if (path.includes("/workflows/wired-channels")) return { channels: [] };
      if (path.includes("/workflows/runs")) return { runs: [], hasMore: false };
      const m = path.match(/\/workflows\/([^/?]+)$/);
      if (m) return graphFor(decodeURIComponent(m[1]));
      return null;
    },
    post: async () => ({}),
    del: async () => {},
    notifications: async () => ({ notifications: [], unread: 0 }),
    markNotificationsRead: async () => ({ unread: 0 }),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

const notesBanner = () => container.querySelector('[data-testid="workflow-created-notes"]');

async function render(c: OpenCompanyClient) {
  await act(async () => {
    root.render(createElement(WorkflowsView, { client: c, company: "acme", sub: null }));
  });
}

/** Click the stubbed dialog's create, which calls `onCreated`. */
async function create() {
  await act(async () => {
    container.querySelector<HTMLButtonElement>('[data-testid="test-confirm-create"]')!.click();
  });
}

/** Open the New-workflow dialog from the index CTA. */
async function openDialog() {
  await act(async () => {
    container.querySelector<HTMLButtonElement>('[data-testid="workflow-create"]')!.click();
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  createdNotes = undefined;
  vi.clearAllMocks();
});

afterEach(async () => {
  await act(async () => {
    root.unmount();
  });
  container.remove();
  window.location.hash = "";
});

describe("the copilot's corrections, after a one-box create", () => {
  it("survives the selection move the create itself performs", async () => {
    createdNotes = NOTES;
    window.location.hash = "#/workflows";
    await render(client());
    await openDialog();
    await create();

    const banner = notesBanner();
    expect(banner, "the corrections must reach the canvas the create lands on").not.toBeNull();
    for (const note of NOTES) {
      expect(banner!.textContent).toContain(note);
    }
  });

  it("is dismissible, and stays dismissed", async () => {
    createdNotes = NOTES;
    window.location.hash = "#/workflows";
    await render(client());
    await openDialog();
    await create();
    expect(notesBanner()).not.toBeNull();

    await act(async () => {
      container
        .querySelector<HTMLButtonElement>('[data-testid="workflow-created-notes-dismiss"]')!
        .click();
    });
    expect(notesBanner()).toBeNull();
  });

  it("says nothing when the draft needed no corrections", async () => {
    // An empty list is not a quiet banner — it is no banner. A heading with
    // nothing under it reads as a warning the operator cannot act on.
    createdNotes = [];
    window.location.hash = "#/workflows";
    await render(client());
    await openDialog();
    await create();
    expect(notesBanner()).toBeNull();

    createdNotes = undefined;
    await create();
    expect(notesBanner()).toBeNull();
  });

  it("does not follow the operator onto a different workflow", async () => {
    // The notes describe one graph. The selection-change sweep clears them the
    // moment the canvas is showing something else, the same way the version
    // conflict banner is cleared (issue #1704).
    createdNotes = NOTES;
    const c = client();
    window.location.hash = "#/workflows";
    await render(c);
    await openDialog();
    await create();
    expect(notesBanner()).not.toBeNull();

    // The router drives the selection through `sub`, so this is what "the
    // operator opened a different workflow" actually looks like here.
    await act(async () => {
      window.location.hash = "#/workflows/other";
      root.render(
        createElement(WorkflowsView, { client: c, company: "acme", sub: "other" }),
      );
    });
    expect(notesBanner(), "corrections belong to the workflow they were made on").toBeNull();
  });

  it("does not follow the operator into another company", async () => {
    // Ids are only unique within a company. Two companies with a
    // `weekly-digest` would otherwise carry one's corrections onto the other's
    // canvas — a sweep keyed on the workflow alone matches, and keeps them.
    createdNotes = NOTES;
    const c = client();
    window.location.hash = "#/workflows";
    await render(c);
    await openDialog();
    await create();
    expect(notesBanner()).not.toBeNull();

    await act(async () => {
      root.render(
        createElement(WorkflowsView, {
          client: c,
          company: "beta",
          sub: CREATED.id,
        }),
      );
    });
    expect(notesBanner(), "corrections belong to the company they were made in").toBeNull();
  });
});
