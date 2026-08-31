// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { WorkflowCreateDialog } from "@/views/WorkflowCreateDialog";

/**
 * What the create dialog says while it is working, and when it refuses (#1005).
 *
 * The three claims here are all about a moment that does not exist in any pure
 * helper: the window between the click and the host's answer. `validate()` can
 * be tested without a document because it maps a draft to a string; "the form
 * looks busy", "the draft survives a rejection" and "two clicks post once" are
 * properties of a mounted component holding an unresolved promise, so they earn
 * a jsdom render for the same reason `setup-wizard-finish-gate` does.
 *
 * They are also the three the operator actually notices. Before #1005 the
 * dialog's entire in-flight vocabulary was the word "Creating…" on one button,
 * and a host rejection landed in a banner that nothing scrolled to and no
 * screen reader announced — a create that failed was indistinguishable from a
 * create that was still going.
 */

/** The one call this suite cares about. `createWorkflow` is a thin wrapper over
 * `client.post`, so stubbing the client rather than the module keeps the real
 * request path — including the id/name trimming `submit()` does — in the test. */
function stubClient(post: (path: string, body?: unknown) => Promise<unknown>) {
  return {
    scopeFor: () => "/api/companies/acme",
    // Every GET the dialog fires on open is an optional picker source (roster,
    // sub-workflow list, wired channels, cognition path) and each has a
    // degrade-on-failure path, so one rejection stands in for "this host offers
    // none of them" and leaves the form fully authorable.
    get: () => Promise.reject(new Error("not offered by this host")),
    listTeam: () => Promise.reject(new Error("not offered by this host")),
    post,
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;
let onOpenChange: ReturnType<typeof vi.fn>;

/** The dialog renders through a portal, so it is on `document`, not `container`. */
function inDialog<T extends Element>(selector: string): T | null {
  return document.querySelector<T>(`[data-slot="dialog-content"] ${selector}`);
}

function dialogContent(): HTMLElement {
  const el = document.querySelector<HTMLElement>('[data-slot="dialog-content"]');
  expect(el, "the dialog did not render").toBeTruthy();
  return el as HTMLElement;
}

function button(label: string): HTMLButtonElement {
  const match = Array.from(
    document.querySelectorAll<HTMLButtonElement>('[data-slot="dialog-content"] button'),
  ).find((b) => b.textContent?.trim().startsWith(label));
  expect(match, `no button labeled "${label}"`).toBeTruthy();
  return match as HTMLButtonElement;
}

function submitButton(): HTMLButtonElement {
  return inDialog<HTMLButtonElement>('[data-testid="workflow-dialog-submit"]')!;
}

/** Sets a controlled input the way a keystroke would: through the native value
 * setter, so React's own descriptor sees the change and `onChange` fires. */
function type(selector: string, value: string, nth = 0) {
  const input = Array.from(
    document.querySelectorAll<HTMLInputElement>(`[data-slot="dialog-content"] ${selector}`),
  )[nth];
  expect(input, `no input matching ${selector} at index ${nth}`).toBeTruthy();
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
      createElement(WorkflowCreateDialog, {
        open: true,
        onOpenChange,
        client,
        company: "acme",
        workflow: null,
      }),
    );
  });
}

/** The smallest draft `validate()` accepts: the starter trigger node already
 * carries an id and a name, so only the graph's own id and name are missing. */
async function fillValidDraft() {
  await act(async () => {
    type('[placeholder="e.g. campaign_pipeline"]', "campaign_pipeline");
  });
  await act(async () => {
    type('[placeholder="e.g. Campaign pipeline"]', "Campaign pipeline");
  });
}

/** The confirm's own Create action (#1808), portalled onto `document.body`. */
function confirmCreateButton(): HTMLButtonElement {
  const el = document.querySelector<HTMLButtonElement>(
    '[data-testid="workflow-id-confirm-create"]',
  );
  expect(el, "the id confirm did not open").toBeTruthy();
  return el as HTMLButtonElement;
}

/** Create now confirms the permanent id first (#1808): the form's Create opens a
 * confirm, and the confirm's own action runs the write. This drives both, for
 * the tests below whose subject is the write, not the confirm step itself. */
async function submitThroughConfirm() {
  await act(async () => {
    submitButton().click();
  });
  await act(async () => {
    confirmCreateButton().click();
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  // jsdom does no layout and ships no `scrollIntoView`; `showError` calls it on
  // the next frame, where a throw would be an unhandled rejection rather than a
  // test failure. Stubbed, not asserted — WHERE the page scrolls is a browser
  // property and belongs to the e2e suite.
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

describe("the create dialog while a create is in flight", () => {
  it("shows a spinner and stops the form being edited under the request", async () => {
    // Never settles: the assertions below are about the window between the
    // click and the answer, which is the whole point.
    const post = vi.fn(() => new Promise<never>(() => {}));
    await open(stubClient(post));
    await fillValidDraft();

    expect(dialogContent().getAttribute("aria-busy")).toBe("false");
    expect(submitButton().querySelector(".animate-spin")).toBeNull();

    // Create opens the id confirm (#1808); the confirm's action runs the write,
    // which is what puts the form in flight.
    await submitThroughConfirm();

    expect(post).toHaveBeenCalledTimes(1);
    expect(dialogContent().getAttribute("aria-busy")).toBe("true");
    expect(submitButton().querySelector(".animate-spin")).toBeTruthy();
    expect(submitButton().disabled).toBe(true);
    // The graph being posted is the one on screen, so every control that would
    // change it is off for the duration.
    expect(button("Add node").disabled).toBe(true);
    expect(button("Add edge").disabled).toBe(true);
    expect(inDialog<HTMLButtonElement>('[data-testid="workflow-copilot-draft"]')!.disabled).toBe(
      true,
    );
    // ...including the fields themselves, which the `fieldset` covers. Matched
    // with `:disabled` rather than `.disabled`, deliberately: a control inside a
    // disabled fieldset IS disabled — it matches the pseudo-class and receives
    // no events — but its own IDL property stays false, because the `disabled`
    // content attribute is on the ancestor. Asserting `.disabled` here would
    // read false and hide a working lock.
    for (const [what, selector] of [
      ["the workflow id", '[placeholder="e.g. campaign_pipeline"]'],
      ["the workflow name", '[placeholder="e.g. Campaign pipeline"]'],
      ["the description", '[placeholder="What does this workflow do?"]'],
      ["a node id", '[aria-label="Node id"]'],
      ["a node name", '[placeholder="display name"]'],
      ["the node kind select", '[aria-label="Node kind"]'],
      ["the remove-node button", '[aria-label="Remove node"]'],
    ] as const) {
      const el = inDialog<HTMLElement>(selector);
      expect(el, `no control matching ${selector}`).toBeTruthy();
      expect(el!.matches(":disabled"), `${what} stayed live under the request`).toBe(true);
    }

    // Not merely styled: the row really cannot be removed from the graph being
    // posted. A disabled fieldset swallows the event rather than dispatching it.
    const rowsBefore = document.querySelectorAll('[aria-label="Remove node"]').length;
    await act(async () => {
      inDialog<HTMLButtonElement>('[aria-label="Remove node"]')!.click();
    });
    expect(document.querySelectorAll('[aria-label="Remove node"]').length).toBe(rowsBefore);
  });

  it("posts once when the operator clicks the confirm Create twice in one tick", async () => {
    const post = vi.fn(() => new Promise<never>(() => {}));
    await open(stubClient(post));
    await fillValidDraft();

    // Create opens the id confirm (#1808); the write is behind the confirm's
    // own action, so the re-entrancy guard now lives on that click.
    await act(async () => {
      submitButton().click();
    });

    // BOTH clicks inside ONE `act`, deliberately. React commits state at the
    // end of the block, so the button is still enabled for the second one and
    // `create()` is genuinely re-entered — which is the only way to reach the
    // guard. Split across two `act`s the first commit has landed, `disabled` is
    // set, React drops the event on the disabled fiber, and the assertion below
    // passes for a reason that has nothing to do with `create()`.
    //
    // So this is the assertion that pins `submittingRef`: against a guard
    // reading the `submitting` STATE both calls see the pre-commit `false` and
    // this fails with two posts.
    await act(async () => {
      confirmCreateButton().click();
      confirmCreateButton().click();
    });

    expect(post).toHaveBeenCalledTimes(1);
  });
});

describe("the create dialog when the host refuses", () => {
  it("keeps the dialog open with the draft intact and announces the reason", async () => {
    const post = vi.fn(() =>
      Promise.reject(new ApiError(409, "conflict", "a workflow named “Campaign pipeline” already exists", true)),
    );
    await open(stubClient(post));
    await fillValidDraft();

    await submitThroughConfirm();

    const banner = inDialog<HTMLElement>('[data-testid="create-error"]');
    expect(banner).toBeTruthy();
    expect(banner!.textContent).toContain("already exists");
    // Announced, not merely rendered: the banner can be well below the fold on
    // a long graph, so being in the DOM is not the same as being reported.
    expect(banner!.getAttribute("role")).toBe("alert");
    expect(banner!.getAttribute("aria-live")).toBe("assertive");
    // ...and taken to, on the frame after it renders. This is the treatment the
    // client-validation branch already had and the server branch did not, which
    // is the whole of #1005's "a failed create is never announced": on a long
    // graph the banner renders below the fold and nothing moves to it.
    await act(async () => {
      await new Promise((r) => requestAnimationFrame(r));
    });
    expect(Element.prototype.scrollIntoView).toHaveBeenCalled();

    // The dialog never asked to close, and what the operator typed is still
    // there to fix — a rejected create must not cost them the graph.
    expect(onOpenChange).not.toHaveBeenCalled();
    expect(inDialog<HTMLInputElement>('[placeholder="e.g. campaign_pipeline"]')!.value).toBe(
      "campaign_pipeline",
    );
    expect(inDialog<HTMLInputElement>('[placeholder="e.g. Campaign pipeline"]')!.value).toBe(
      "Campaign pipeline",
    );

    // ...and the form is live again, so the fix can be made and retried.
    expect(dialogContent().getAttribute("aria-busy")).toBe("false");
    expect(submitButton().disabled).toBe(false);
  });

  /** Rejects, so the banner is up and the form is live again to edit. */
  async function submitAndGetRefused() {
    await open(
      stubClient(() =>
        Promise.reject(new ApiError(400, "bad_request", "that id is taken", true)),
      ),
    );
    await fillValidDraft();
    await submitThroughConfirm();
    expect(inDialog('[data-testid="create-error"]')).toBeTruthy();
  }

  // The complaint was about the graph as it was, so any edit to that graph
  // retires it; leaving it up would make the next failure indistinguishable
  // from this one. Each case below is a separate handler in the component —
  // the id and the name are top-level state, a node row goes through
  // `updateNode` — so they are covered separately rather than assumed.
  for (const [what, selector] of [
    ["the id", '[placeholder="e.g. campaign_pipeline"]'],
    ["the name", '[placeholder="e.g. Campaign pipeline"]'],
    ["a node", '[placeholder="display name"]'],
  ] as const) {
    it(`drops the stale banner as soon as the operator edits ${what}`, async () => {
      await submitAndGetRefused();

      await act(async () => {
        type(selector, "something_else");
      });

      expect(inDialog('[data-testid="create-error"]')).toBeNull();
    });
  }

  // Structural edits too, which is the case that actually bites: the banner
  // routinely names something an add or a remove is the fix for ("Add at least
  // one node.", a duplicate id, an edge pointing at nothing), so leaving it up
  // through exactly the action that answers it is the worst version of a stale
  // message — the operator fixes the problem and the complaint does not move.
  it("drops the stale banner as soon as the operator adds a node", async () => {
    await submitAndGetRefused();

    await act(async () => {
      button("Add node").click();
    });

    expect(inDialog('[data-testid="create-error"]')).toBeNull();
  });

  it("drops the stale banner as soon as the operator adds an edge", async () => {
    await submitAndGetRefused();

    // Add edge is `disabled` until the graph has two nodes to connect, so the
    // second node is authored first — and that add clears the banner on its
    // own, hence the fresh refusal below to put it back.
    await act(async () => {
      button("Add node").click();
    });
    await act(async () => {
      type('[aria-label="Node id"]', "second_node", 1);
    });
    await act(async () => {
      type('[placeholder="display name"]', "Second node", 1);
    });
    // The freshly-added agent node has no assignee yet, so `validate()` refuses
    // client-side and the banner is back BEFORE the id-confirm gate — a plain
    // submit is enough here (no confirm opens on a failed validate).
    await act(async () => {
      submitButton().click();
    });
    expect(inDialog('[data-testid="create-error"]')).toBeTruthy();
    expect(button("Add edge").disabled).toBe(false);

    await act(async () => {
      button("Add edge").click();
    });

    expect(inDialog('[data-testid="create-error"]')).toBeNull();
  });

  it("drops the stale banner as soon as the operator removes a node", async () => {
    await submitAndGetRefused();

    await act(async () => {
      inDialog<HTMLButtonElement>('[aria-label="Remove node"]')!.click();
    });

    expect(inDialog('[data-testid="create-error"]')).toBeNull();
  });
});
