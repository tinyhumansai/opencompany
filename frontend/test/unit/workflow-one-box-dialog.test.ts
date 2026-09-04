// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import type { WorkflowGraph } from "@/api/workflows";
import { WorkflowCreateDialog } from "@/views/WorkflowCreateDialog";

/**
 * Creating a workflow is one description box and a Create button: no Name, no
 * Workflow ID, no Description, no Nodes, no Connections, and none of the
 * validation that serves them — including "Give the workflow an id.", which
 * used to fire on a dialog the operator had not finished reading.
 *
 * **On every company and every build.** The copilot's availability used to pick
 * between two dialogs; a host with no model configured got the full graph form,
 * which is the dialog this redesign exists to retire. Now it changes only what
 * Create *does* — draft, or fall back to the operator's own sentence — and what
 * the box says above itself. The `echo` cases below are the ones that prove it,
 * and they are the reason this file renders the component rather than trusting
 * `createSurface` (unit-tested next door): the branch has to reach the DOM.
 *
 * The manual form is unchanged and still reachable, by the one route that
 * needs it — a create the host refused, which names an id there is otherwise no
 * control to obey.
 */

const SCOPE = "/api/v1/companies/acme";

/** The controls that must NOT exist on the one-box dialog. */
const NAME_INPUT = 'input[placeholder="e.g. Campaign pipeline"]';
const ID_INPUT = 'input[placeholder="e.g. campaign_pipeline"]';
const DESCRIPTION_BOX = 'textarea[placeholder="What does this workflow do?"]';

/** A drafted graph the host would answer with. */
const DRAFTED: WorkflowGraph = {
  id: "weekly-digest",
  name: "Weekly digest",
  description: "Every Monday, draft the digest and email it.",
  version: null,
  nodes: [
    { id: "start", kind: "trigger", name: "Start", schedule: "0 9 * * 1" },
    { id: "write", kind: "agent", name: "Draft it", agent: "writer" },
  ],
  edges: [{ from: "start", to: "write" }],
};

interface Stub {
  /** What `POST …/workflows/draft-from-description` answers, or throws. */
  draft?: () => Promise<unknown>;
  /** Counts every draft attempt, so "it never asked" can be asserted. */
  drafts?: { count: number };
  /** What `POST …/workflows` answers, or throws. */
  create?: (body: unknown) => Promise<unknown>;
  /** The company's cognition path. `"hosted"` is a company that can draft. */
  cognition?: string;
}

/**
 * Stubs the verbs the dialog reaches. The GETs other than `/inference` are
 * optional picker sources that each degrade on failure, so one rejection stands
 * in for "this host offers none of them".
 */
function stubClient(opts: Stub): OpenCompanyClient {
  return {
    scopeFor: () => SCOPE,
    listTeam: () => Promise.reject(new Error("not offered by this host")),
    get: (path: string) =>
      path.endsWith("/inference")
        ? Promise.resolve({ cognition: opts.cognition ?? "hosted" })
        : Promise.reject(new Error("not offered by this host")),
    post: (path: string, body?: unknown) => {
      if (path.endsWith("/workflows/draft-from-description")) {
        if (opts.drafts) opts.drafts.count += 1;
        return (
          opts.draft?.() ??
          Promise.resolve({ automatable: true, summary: "a digest", workflow: DRAFTED })
        );
      }
      if (path.endsWith("/workflows/validate")) return Promise.resolve({ valid: true });
      if (path.endsWith("/workflows")) {
        return opts.create?.(body) ?? Promise.resolve(body as WorkflowGraph);
      }
      return Promise.reject(new Error(`unexpected POST ${path}`));
    },
    put: () => Promise.reject(new Error("no put expected")),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;
let onOpenChange: ReturnType<typeof vi.fn>;
let onCreated: ReturnType<typeof vi.fn>;

function inDialog<T extends Element>(selector: string): T | null {
  return document.querySelector<T>(`[data-slot="dialog-content"] ${selector}`);
}

function submitButton(): HTMLButtonElement {
  const el = inDialog<HTMLButtonElement>('[data-testid="workflow-dialog-submit"]');
  if (!el) throw new Error(`no submit button in:\n${document.body.innerHTML}`);
  return el;
}

function describeBox(): HTMLTextAreaElement | null {
  return inDialog<HTMLTextAreaElement>('[data-testid="workflow-describe-box"]');
}

/** Sets a controlled textarea the way a keystroke would. */
function typeDescription(value: string) {
  const box = describeBox();
  expect(box, "the one-box dialog should have a description box").toBeTruthy();
  const setter = Object.getOwnPropertyDescriptor(
    HTMLTextAreaElement.prototype,
    "value",
  )!.set!;
  setter.call(box, value);
  box!.dispatchEvent(new Event("input", { bubbles: true }));
}

/** The visible label text of every section heading and control label. */
function dialogText(): string {
  return document.querySelector('[data-slot="dialog-content"]')?.textContent ?? "";
}

async function open(client: OpenCompanyClient) {
  await act(async () => {
    root.render(
      createElement(WorkflowCreateDialog, {
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

describe("the New-workflow dialog when the copilot can draft", () => {
  it("is one description box — no Name, ID, Description, Nodes or Connections", async () => {
    await open(stubClient({ cognition: "hosted" }));

    expect(describeBox(), "the description box is the whole dialog").toBeTruthy();
    expect(inDialog(NAME_INPUT), "Name must not render").toBeNull();
    expect(inDialog(ID_INPUT), "Workflow ID must not render").toBeNull();
    expect(inDialog(DESCRIPTION_BOX), "the second Description box must not render").toBeNull();
    // The section headings, not just their controls: a heading with no rows
    // under it is the same clutter the redesign removes.
    expect(dialogText()).not.toContain("Nodes");
    expect(dialogText()).not.toContain("Connections");
    expect(dialogText()).not.toContain("Add node");
    expect(dialogText()).not.toContain("Add edge");
    // …and the separate "Draft it" button is gone with the two-step it served.
    expect(inDialog('[data-testid="workflow-copilot-draft"]')).toBeNull();
  });

  it("never raises the id complaint on a dialog that asks for no id", async () => {
    await open(stubClient({ cognition: "hosted" }));

    // Create with an empty box does nothing at all — the button is dead rather
    // than answering with a rule about a field nobody was shown.
    expect(submitButton().disabled, "Create is dead with an empty box").toBe(true);
    await act(async () => {
      submitButton().click();
    });
    expect(dialogText()).not.toContain("Give the workflow an id.");
    expect(inDialog('[data-testid="create-error"]')).toBeNull();
  });

  it("drafts, saves, and hands the canvas the graph and the host's notes", async () => {
    const posted: unknown[] = [];
    await open(
      stubClient({
        cognition: "hosted",
        draft: () =>
          Promise.resolve({
            automatable: true,
            summary: "a weekly digest",
            workflow: DRAFTED,
            notes: ["Matched “the writer” to teammate `writer`.", "   "],
          }),
        create: (body) => {
          posted.push(body);
          return Promise.resolve({ ...(body as WorkflowGraph), version: "v1" });
        },
      }),
    );

    await act(async () => {
      typeDescription("Every Monday, draft the digest and email it.");
    });
    await act(async () => {
      submitButton().click();
    });

    // One write, of the host's own drafted graph — not a round trip through
    // form state the dialog is no longer rendering.
    expect(posted).toHaveLength(1);
    expect((posted[0] as WorkflowGraph).id).toBe("weekly-digest");
    expect((posted[0] as WorkflowGraph).nodes).toHaveLength(2);
    // The canvas is where review happens now, so it is handed both the saved
    // graph and the corrections the host made on the way to it — blank notes
    // dropped, because an empty bullet is not a correction.
    expect(onCreated).toHaveBeenCalledTimes(1);
    expect(onCreated.mock.calls[0]![0].version).toBe("v1");
    expect(onCreated.mock.calls[0]![1]).toEqual([
      "Matched “the writer” to teammate `writer`.",
    ]);
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });

  it("shows a decline as advice, with a way past it", async () => {
    const posted: unknown[] = [];
    await open(
      stubClient({
        cognition: "hosted",
        draft: () =>
          Promise.resolve({
            automatable: false,
            reason: "This is a one-off — just do it once rather than building it.",
          }),
        create: (body) => {
          posted.push(body);
          return Promise.resolve(body as WorkflowGraph);
        },
      }),
    );

    await act(async () => {
      typeDescription("Email Priya the Q3 numbers, once.");
    });
    await act(async () => {
      submitButton().click();
    });

    // The reason is shown, and nothing was written.
    const declined = inDialog('[data-testid="workflow-draft-declined"]');
    expect(declined, "a decline must be shown, not swallowed").toBeTruthy();
    expect(declined!.textContent).toContain("This is a one-off");
    expect(posted, "a decline writes nothing on its own").toHaveLength(0);

    // …and the operator who disagrees is not blocked. The reason is advice.
    const anyway = inDialog<HTMLButtonElement>('[data-testid="workflow-create-anyway"]');
    expect(anyway, "a decline must offer a way past it").toBeTruthy();
    await act(async () => {
      anyway!.click();
    });
    expect(posted, "Create it anyway must actually create").toHaveLength(1);
    const graph = posted[0] as WorkflowGraph;
    // Named and described from the operator's own sentence, with the same
    // single trigger the blank form has always started from.
    expect(graph.name).toBe("Email Priya the Q3 numbers");
    expect(graph.id).toBe("email-priya-the-q3-numbers");
    expect(graph.description).toBe("Email Priya the Q3 numbers, once.");
    expect(graph.nodes.map((n) => n.kind)).toEqual(["trigger"]);
    expect(onCreated).toHaveBeenCalledTimes(1);
  });

  it("hands over the fields when the host refuses the minted id", async () => {
    // The refusal that actually happens: the host mints ids by deduping against
    // SAVED workflows only, so two similar descriptions drafted before either
    // is created mint the same id — and the second Create is told to pick a
    // different one, by a dialog with no id field.
    await open(
      stubClient({
        cognition: "hosted",
        create: () =>
          Promise.reject(
            new ApiError(
              409,
              "conflict",
              "A workflow with id `weekly-digest` already exists. Pick a different id.",
            ),
          ),
      }),
    );

    await act(async () => {
      typeDescription("Every Monday, draft the digest and email it.");
    });
    await act(async () => {
      submitButton().click();
    });

    // The fields come back, carrying the graph that was refused, so the
    // instruction in the message is one the operator can actually follow.
    expect(inDialog(ID_INPUT), "the id field must come back").toBeTruthy();
    expect(inDialog<HTMLInputElement>(ID_INPUT)!.value).toBe("weekly-digest");
    expect(inDialog<HTMLInputElement>(NAME_INPUT)!.value).toBe("Weekly digest");
    expect(dialogText()).toContain("Nodes");
    expect(inDialog('[data-testid="create-error"]')!.textContent).toContain(
      "Pick a different id",
    );
    expect(onCreated, "a refused write creates nothing").not.toHaveBeenCalled();
    expect(onOpenChange).not.toHaveBeenCalledWith(false);
  });

  it("keeps the box when the build turns out to have no copilot, and still creates", async () => {
    // A capability gap is a fact about the deployment, not about the sentence,
    // so it retires DRAFTING — not the dialog. The fields do not come back;
    // the notice above the box changes to the host's own words and Create
    // builds the workflow from what the operator wrote.
    const posted: unknown[] = [];
    await open(
      stubClient({
        cognition: "hosted",
        draft: () =>
          Promise.reject(new ApiError(404, "not_wired", "This build has no copilot wired.")),
        create: (body) => {
          posted.push(body);
          return Promise.resolve(body as WorkflowGraph);
        },
      }),
    );

    await act(async () => {
      typeDescription("Every Monday, draft the digest.");
    });
    await act(async () => {
      submitButton().click();
    });

    // The one box is still the whole dialog — this is the reversal.
    expect(describeBox(), "the box must survive a capability gap").toBeTruthy();
    expect(inDialog(NAME_INPUT), "the manual form must NOT come back").toBeNull();
    expect(inDialog(ID_INPUT)).toBeNull();
    expect(dialogText()).not.toContain("Nodes");
    // …saying what happened, in the host's own words, and what Create does now.
    const notice = inDialog('[data-testid="workflow-draft-unavailable"]');
    expect(notice, "the operator must be told the copilot could not draft").toBeTruthy();
    expect(notice!.textContent).toContain("This build has no copilot wired.");
    expect(notice!.textContent).toContain("empty canvas");
    expect(posted, "the failed draft writes nothing on its own").toHaveLength(0);

    // …and Create is not a dead button. The second press builds the workflow
    // from the sentence and lands on the canvas.
    await act(async () => {
      submitButton().click();
    });
    expect(posted, "Create must still create with no copilot").toHaveLength(1);
    const graph = posted[0] as WorkflowGraph;
    expect(graph.name).toBe("Every Monday");
    expect(graph.description).toBe("Every Monday, draft the digest.");
    expect(graph.nodes.map((n) => n.kind)).toEqual(["trigger"]);
    expect(onCreated).toHaveBeenCalledTimes(1);
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });
});

describe("the New-workflow dialog on a company with no model configured", () => {
  it("is the same one box — the graph form does not come back for `echo`", async () => {
    await open(stubClient({ cognition: "echo" }));

    // The reversal, asserted where the operator sees it. Every field the
    // one-box dialog removes stays removed on the offline brain, because a
    // company with no model is the LAST one to hand a graph editor to.
    expect(describeBox(), "the description box is the whole dialog").toBeTruthy();
    expect(inDialog(NAME_INPUT), "Name must not render").toBeNull();
    expect(inDialog(ID_INPUT), "Workflow ID must not render").toBeNull();
    expect(inDialog(DESCRIPTION_BOX), "the second Description box must not render").toBeNull();
    expect(dialogText()).not.toContain("Nodes");
    expect(dialogText()).not.toContain("Connections");
    expect(dialogText()).not.toContain("Add node");
    expect(dialogText()).not.toContain("Add edge");
    expect(inDialog('[data-testid="workflow-copilot-draft"]')).toBeNull();
  });

  it("says what Create will do before it is pressed, and where to fix it", async () => {
    await open(stubClient({ cognition: "echo" }));

    const notice = inDialog('[data-testid="workflow-draft-unavailable"]');
    expect(notice, "an operator must not be promised a draft that cannot happen").toBeTruthy();
    expect(notice!.textContent).toContain("no model configured");
    expect(notice!.textContent).toContain("Settings → Inference");
    expect(notice!.textContent).toContain("empty canvas");
    // NOT the copy this path used to carry, which pointed at a form that is no
    // longer under it.
    expect(dialogText()).not.toContain("build the graph by hand below");
  });

  it("creates from the sentence and lands on the canvas, without a doomed draft", async () => {
    const drafts = { count: 0 };
    const posted: unknown[] = [];
    await open(
      stubClient({
        cognition: "echo",
        drafts,
        create: (body) => {
          posted.push(body);
          return Promise.resolve({ ...(body as WorkflowGraph), version: "v1" });
        },
      }),
    );

    await act(async () => {
      typeDescription("Chase overdue invoices every Friday.");
    });
    await act(async () => {
      submitButton().click();
    });

    // No round trip that is already known to fail — the cognition read has
    // settled on the offline brain, so there is nothing to ask.
    expect(drafts.count, "the copilot must not be asked on the offline brain").toBe(0);
    // …and the same route "Create it anyway" takes: the sentence names it and
    // describes it, over the single trigger the blank form has always started
    // from.
    expect(posted, "Create must never be a button that cannot create").toHaveLength(1);
    const graph = posted[0] as WorkflowGraph;
    expect(graph.name).toBe("Chase overdue invoices every Friday");
    expect(graph.id).toBe("chase-overdue-invoices-every-friday");
    expect(graph.description).toBe("Chase overdue invoices every Friday.");
    expect(graph.nodes.map((n) => n.kind)).toEqual(["trigger"]);
    expect(onCreated).toHaveBeenCalledTimes(1);
    expect(onCreated.mock.calls[0]![0].version).toBe("v1");
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });

  it("hands over the fields when the sentence names nothing, rather than minting an empty id", async () => {
    // The id is the permanent join key and it is derived from the name, so a
    // sentence with no words in it has to be asked about — never guessed.
    const posted: unknown[] = [];
    await open(
      stubClient({
        cognition: "echo",
        create: (body) => {
          posted.push(body);
          return Promise.resolve(body as WorkflowGraph);
        },
      }),
    );

    await act(async () => {
      typeDescription("...");
    });
    await act(async () => {
      submitButton().click();
    });

    expect(posted, "an empty name must not be written").toHaveLength(0);
    expect(inDialog(NAME_INPUT), "the fields come back to be filled in").toBeTruthy();
    expect(inDialog<HTMLInputElement>(NAME_INPUT)!.value).toBe("");
    expect(inDialog<HTMLInputElement>(ID_INPUT)!.value).toBe("");
    expect(inDialog('[data-testid="create-error"]')!.textContent).toContain(
      "Give this workflow a name",
    );
  });
});
