// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterAll, afterEach, beforeEach, describe, expect, it } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { PrefilledDraft, WorkflowGraph } from "@/api/workflows";
import { WorkflowCreateDialog } from "@/views/WorkflowCreateDialog";

/**
 * Issue #1053: the id derives itself from the name — and stops the moment the
 * id is somebody's.
 *
 * `workflow-id.test.ts` pins the two helpers. This pins the WIRING, which is
 * where the bug actually lives: which handler calls the slugger, when the
 * `idTouched` latch closes, and that edit mode never derives. None of that is
 * reachable from a pure helper — a latch that never closes and a latch that
 * closes on open both leave `slugifyWorkflowId` perfectly correct, and both
 * ship the bug.
 *
 * The dialog earned a jsdom harness in #1006, so the honest test is now
 * available: mount it, type into it, read the id field back out. Before that
 * landed, this file could not have been written, and the PR said so rather
 * than claiming the coverage.
 */

/** A saved graph for edit mode: its id must survive every name keystroke. */
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

/** A host that answers every read the dialog makes on open. */
function stubClient(): OpenCompanyClient {
  return {
    scopeFor: () => "/api/companies/acme",
    listTeam: async () => [],
    // Creating a workflow is one description box now, on every company —
    // `echo`, the offline brain, included. This suite is about the manual
    // Name/ID/Description/Nodes/Connections form, which `openCreateForm` below
    // reaches the way an operator does. See `createSurface` in
    // `@/lib/workflow-create-surface`.
    get: async (path: string) => {
      if (path.endsWith("/inference")) return { cognition: "echo" };
      return path.endsWith("/wired-channels") ? { channels: [] } : [];
    },
  } as unknown as OpenCompanyClient;
}

// Same jsdom gap `workflow-editor-unsaved-work` documents: `scrollIntoView`
// does not exist, and the dialog calls it from a `requestAnimationFrame` that
// can outlive a test body. Kept up for the whole file, restored in `afterAll`.
const originalScrollIntoView = Object.getOwnPropertyDescriptor(
  Element.prototype,
  "scrollIntoView",
);

if (!Element.prototype.scrollIntoView) {
  Element.prototype.scrollIntoView = () => {};
}

let container: HTMLDivElement;
let root: Root;

/** The dialog portals into `document.body`, not into the mount container. */
function field(suffix: string): HTMLInputElement {
  const el = document.body.querySelector<HTMLInputElement>(`input[id$="-${suffix}"]`);
  expect(el, `no input matching id$="-${suffix}"`).toBeTruthy();
  return el as HTMLInputElement;
}

/** Type into a controlled `<input>` the way React reads it. */
async function type(el: HTMLInputElement, value: string) {
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      "value",
    )!.set!;
    setter.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

async function render(opts: {
  open: boolean;
  workflow?: WorkflowGraph | null;
  prefilledDraft?: PrefilledDraft | null;
}) {
  await act(async () => {
    root.render(
      createElement(WorkflowCreateDialog, {
        client: stubClient(),
        company: "acme",
        open: opts.open,
        onOpenChange: () => {},
        workflow: opts.workflow ?? null,
        prefilledDraft: opts.prefilledDraft ?? null,
      }),
    );
  });
}

async function open(opts: {
  workflow?: WorkflowGraph | null;
  prefilledDraft?: PrefilledDraft | null;
} = {}) {
  await render({ ...opts, open: true });
}

/** Sets a controlled textarea the way a keystroke would. */
function typeDescription(value: string) {
  const box = document.body.querySelector<HTMLTextAreaElement>(
    '[data-testid="workflow-describe-box"]',
  );
  expect(box, "the create dialog should open as one description box").toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(
    HTMLTextAreaElement.prototype,
    "value",
  )!.set!;
  setter.call(box, value);
  box!.dispatchEvent(new Event("input", { bubbles: true }));
}

/**
 * Opens the create dialog **on its manual form**, by the route an operator
 * takes to it.
 *
 * Create mode opens as one description box. The fields come back when the
 * dialog cannot finish without them, and the shortest such case is driven here:
 * this company cannot draft (`echo`), and a sentence with no words in it
 * derives no name — so rather than mint an empty id, which is the permanent
 * join key this whole suite is about, the dialog hands over the fields and asks
 * for a name. Crucially it hands them over **derivable**: an operator who is
 * being asked for a name must get an id out of typing one.
 */
async function openCreateForm() {
  await open();
  await act(async () => {
    typeDescription("...");
  });
  await act(async () => {
    document
      .body
      .querySelector<HTMLButtonElement>('[data-testid="workflow-dialog-submit"]')!
      .click();
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

afterAll(() => {
  if (originalScrollIntoView) {
    Object.defineProperty(Element.prototype, "scrollIntoView", originalScrollIntoView);
  } else {
    delete (Element.prototype as unknown as Record<string, unknown>).scrollIntoView;
  }
});

describe("the create form derives the id from the name (#1053)", () => {
  it("starts with the human name, then explains the editable permanent machine id", async () => {
    await openCreateForm();

    const name = field("name");
    const id = field("id");
    expect(name.compareDocumentPosition(id) & Node.DOCUMENT_POSITION_FOLLOWING).not.toBe(0);
    expect(document.body.textContent).toContain("Generated from the name");
    expect(document.body.textContent).toContain("permanent machine ID");

    for (const label of ["Node ID", "Kind", "Step name", "Summary"]) {
      expect(document.body.textContent).toContain(label);
    }
    expect(
      Array.from(document.body.querySelectorAll("button")).some(
        (button) => button.textContent?.trim() === "Connect in order",
      ),
    ).toBe(true);
  });

  it("fills the id in as the name is typed, so a bare name is not rejected", async () => {
    await openCreateForm();

    await type(field("name"), "Weekly digest");

    // The reported bug verbatim: this used to be "" and the form answered
    // "Give the workflow an id."
    expect(field("id").value).toBe("weekly-digest");
  });

  it("keeps deriving while the id is nobody's, so it tracks the name", async () => {
    await openCreateForm();
    const name = field("name");

    await type(name, "Weekly");
    expect(field("id").value).toBe("weekly");
    await type(name, "Weekly digest v2");
    expect(field("id").value).toBe("weekly-digest-v2");
  });

  it("leaves the field alone when the name derives to nothing", async () => {
    await openCreateForm();

    await type(field("name"), "Campaign pipeline");
    expect(field("id").value).toBe("campaign-pipeline");
    // "???" has nothing usable in it. Writing the empty derivation would blank
    // a good id on a keystroke — the clobber the guard exists to prevent.
    await type(field("name"), "???");
    expect(field("id").value).toBe("campaign-pipeline");
  });
});

describe("deriving stops the moment the id is somebody's (#1053)", () => {
  it("never writes over an id the operator typed", async () => {
    await openCreateForm();

    await type(field("id"), "chosen-by-hand");
    await type(field("name"), "Weekly digest");

    expect(field("id").value).toBe("chosen-by-hand");
  });

  it("treats clearing the id back to empty as a decision too", async () => {
    await openCreateForm();

    // Derive one, then take it away. An operator who empties the field meant
    // to; the next keystroke in Name must not quietly refill it.
    await type(field("name"), "Weekly digest");
    expect(field("id").value).toBe("weekly-digest");
    await type(field("id"), "");
    await type(field("name"), "Weekly digest final");

    expect(field("id").value).toBe("");
  });

  it("starts derivable again on the next open, so the latch is not one-way", async () => {
    await openCreateForm();
    await type(field("id"), "the-first-one");
    await type(field("name"), "First workflow");
    expect(field("id").value).toBe("the-first-one");

    // The latch is deliberately sticky WITHIN an open — clobbering a chosen id
    // is the worse bug. That makes failing to reset it on the next open the
    // matching failure: the second workflow an operator creates in a session
    // would silently stop deriving, and only the second one.
    await render({ open: false });
    await openCreateForm();

    await type(field("name"), "Second workflow");
    expect(field("id").value).toBe("second-workflow");
  });

  it("never derives over the id a copilot correction came back to", async () => {
    // A copilot correction (fix-from-run, issue #840) is the one thing that
    // hands the dialog a graph it did not load itself. It always arrives
    // alongside the saved workflow — `WorkflowsView` renders the dialog with
    // `open={editOpen && editGraph !== null}` — so this is the production
    // shape, and the id is already somebody's twice over: the saved graph's own
    // id wins even over the one the correction minted, because it keys the
    // overlay body, the schedule and the run history.
    await open({
      workflow: savedGraph(),
      prefilledDraft: { workflow: { ...savedGraph(), id: "copilot-minted-this" } },
    });
    expect(field("id").value).toBe("weekly_report");

    // The correction renames the workflow, which is exactly the keystroke that
    // would re-slug an unlatched id — and a re-slug here is a rename the host
    // answers 400 to.
    await type(field("name"), "Renamed by the copilot");

    expect(field("id").value).toBe("weekly_report");
  });
});

describe("edit mode never derives (#1053)", () => {
  it("leaves the saved id alone however the name is edited", async () => {
    await open({ workflow: savedGraph() });
    expect(field("id").value).toBe("weekly_report");

    await type(field("name"), "Weekly report v2");

    // Re-slugging here would be a rename, and the id keys the saved graph, its
    // schedule and its run history. The field is `readOnly` in edit mode, but
    // that is the UI's guard; this pins the handler's own.
    expect(field("id").value).toBe("weekly_report");
  });
});
