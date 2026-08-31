// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { WorkspaceView } from "@/views/WorkspaceView";

/**
 * PR #1498 review (CodeRabbit, thread 3829432141): `RepairDialog.onReveal` is
 * documented "Show a residual in the tree. Never writes — it expands and
 * scrolls." — but a *file* residual fell through to the ordinary `open()`
 * route, and `open()` starts with `await flush()`. If the editor held a
 * staged draft when the operator clicked "show me" on a file residual, that
 * draft got written to the host as a side effect of a control promised as
 * reveal-only.
 *
 * This pins the fix at the one place that could actually observe it: with a
 * *different* note open and dirty, revealing a file residual must not touch
 * the network, and the dirty note's draft must still read as unsaved
 * afterward — not "saving" or "saved" out from under the operator.
 *
 * # Why this file owns the clock (issue #1783)
 *
 * A dirty draft is the precondition here, and a dirty draft arms the editor's
 * 800ms autosave debounce (`AUTOSAVE_DELAY_MS` in `WorkspaceView`). That
 * autosave is correct and wanted — issue #1372 exists to make sure typing
 * reaches the host — but it writes through the very same `client.put` these
 * tests watch, so on real timers the file was a race: every `expect(...).not
 * .toHaveBeenCalled()` below was really asserting "the reveal did not write
 * *and* this test finished in under 800ms of wall clock".
 *
 * It did not always finish in time. Measured on `upstream/main` at 27b42eda0
 * with no source changes, the first test reached its assertion 729ms after the
 * keystroke in one full-suite run (green, 91% of the budget) and overran it in
 * the next (red, one `put` to `/workspace/file/note-1` carrying the draft) —
 * the autosave firing, not the reveal. Run alone the file finished in ~400ms
 * and always passed, which is why it read as a `main`-only failure.
 *
 * So the timers are faked and only advanced deliberately. Nothing about the
 * invariant is relaxed: the regression this file guards is `open()`'s
 * `await flush()`, a direct call on the click path that no clock can hide.
 * Freezing the clock removes the *other* writer, so a `put` here can only mean
 * the click made it.
 */

const ENG = "eng";
const SPECS_FOLDER = "specs-folder";
const SPECS_NOTE = "specs-note";
const OPEN_NOTE = "note-1";

const TREE = [
  { id: ENG, name: "Engineering", kind: "folder", updatedAt: 1 },
  { id: SPECS_FOLDER, name: "Specs", kind: "folder", parentId: ENG, updatedAt: 1 },
  { id: SPECS_NOTE, name: "Specs", kind: "file", parentId: ENG, updatedAt: 1 },
  { id: OPEN_NOTE, name: "Notes.md", kind: "file", updatedAt: 1 },
];

/** The repair preview: a residual-only outcome naming both the folder and the file above. */
const RESIDUAL_ONLY = {
  residuals: [
    { id: SPECS_FOLDER, name: "Specs", parentId: ENG, cause: "fileSharesTheName" },
    { id: SPECS_NOTE, name: "Specs", parentId: ENG, cause: "fileSharesTheName" },
  ],
};

/** The one host each test drives, so its write methods can be asserted on. */
function host() {
  return {
    scopeFor: () => "/api/v1/company/acme",
    get: vi.fn(async (path: string) => {
      if (path.includes("/workspace/file/")) {
        return {
          id: OPEN_NOTE,
          name: "Notes.md",
          content: "saved body",
          backlinks: [],
          updatedAt: 1,
        };
      }
      return TREE;
    }),
    post: vi.fn().mockResolvedValue(RESIDUAL_ONLY),
    patch: vi.fn(),
    del: vi.fn(),
    // The write route a residual reveal must never reach (`writeFile` calls
    // `client.put`) — asserted on directly rather than inferred from state.
    put: vi.fn().mockResolvedValue({ updatedAt: 2 }),
  };
}

let client: ReturnType<typeof host>;
let container: HTMLDivElement;
let root: Root;

/** `AUTOSAVE_DELAY_MS` in `WorkspaceView`. Only ever advanced past on purpose. */
const AUTOSAVE_DELAY_MS = 800;

beforeEach(() => {
  // Only the two functions the debounce uses. Faking `Date`, `setInterval` or
  // `queueMicrotask` as well would buy nothing here and would put React's
  // scheduler and the mocked host's promises on a clock this file has no
  // reason to drive.
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  Element.prototype.scrollIntoView = vi.fn();
  localStorage.clear();
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.useRealTimers();
});

function button(label: string): HTMLButtonElement {
  const match = Array.from(container.querySelectorAll("button")).find(
    (b) => b.textContent?.trim() === label,
  );
  expect(match, `no button labeled "${label}"`).toBeTruthy();
  return match as HTMLButtonElement;
}

function saveStateAttr(): string | null {
  return document.querySelector('[data-testid="workspace-save-state"]')?.getAttribute("data-state") ?? null;
}

function residualRows(): HTMLElement[] {
  return Array.from(
    document.querySelectorAll('[data-testid="workspace-repair-residual"]'),
  ) as HTMLElement[];
}

/**
 * Open `Notes.md`, switch it to Edit, and type a paragraph the host has never
 * seen — the exact "staged draft" precondition the finding names — then open
 * the repair dialog on top of it. The keystroke arms the autosave debounce and
 * deliberately leaves it armed: the clock is frozen (see the file header), so
 * it stays pending until a test advances past it on purpose.
 */
async function openRepairWithADirtyNoteBehindIt() {
  client = host();
  await act(async () => {
    root.render(
      createElement(ConnectionScopeProvider, {
        scope: { connection: "c1", company: "acme" },
        children: createElement(WorkspaceView, {
          client: client as unknown as OpenCompanyClient,
          company: "acme",
        }),
      }),
    );
  });

  await act(async () => {
    button("Notes").click();
  });

  await act(async () => {
    (
      Array.from(document.querySelectorAll('[role="tab"]')).find(
        (t) => t.textContent?.trim() === "Edit",
      ) as HTMLElement
    ).click();
  });

  const editor = document.querySelector('[data-testid="workspace-editor"]') as HTMLTextAreaElement;
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
    setter?.call(editor, "a paragraph the host has never seen");
    editor.dispatchEvent(new Event("input", { bubbles: true }));
  });
  expect(saveStateAttr()).toBe("dirty");

  await act(async () => {
    (container.querySelector('[data-testid="workspace-repair"]') as HTMLButtonElement).click();
  });
}

describe("a residual reveal never flushes a dirty draft (PR #1498 review)", () => {
  it("does not call the write route when the residual is a file", async () => {
    await openRepairWithADirtyNoteBehindIt();
    const rows = residualRows();
    expect(rows).toHaveLength(2);

    // rows()[1] is SPECS_NOTE — the file residual, and the one `open()` used
    // to swallow. rows()[0] (the folder) already went through `revealFolder`,
    // which never wrote.
    await act(async () => {
      rows[1].click();
    });

    expect(client.put).not.toHaveBeenCalled();
    expect(client.patch).not.toHaveBeenCalled();
    expect(client.del).not.toHaveBeenCalled();

    // …and the silence above has to be the click's doing, not an empty buffer's.
    // Release the frozen autosave: the draft is still staged, so it writes now.
    // If a future change ever stopped staging it — or stopped arming the
    // debounce — the three assertions above would pass for the wrong reason and
    // this file would guard nothing. That is the failure mode #1783 was really
    // about, so it gets an assertion rather than a comment.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(AUTOSAVE_DELAY_MS);
    });
    expect(client.put).toHaveBeenCalledWith("/api/v1/company/acme/workspace/file/note-1", {
      content: "a paragraph the host has never seen",
    });
  });

  it("leaves the open note's draft reading as unsaved, not saving or saved", async () => {
    await openRepairWithADirtyNoteBehindIt();
    const rows = residualRows();

    await act(async () => {
      rows[1].click();
    });

    // If the reveal had gone through `open()` -> `flush()`, this would read
    // "saving" (or "saved", once the mocked `put` resolved) instead — the
    // operator's still-open, still-unsaved note would look like it had been
    // written out from under them by a click that named a different file.
    expect(saveStateAttr()).toBe("dirty");
  });

  it("still does the reveal: closes the dialog and does not disturb the open note", async () => {
    await openRepairWithADirtyNoteBehindIt();
    const rows = residualRows();

    await act(async () => {
      rows[1].click();
    });

    expect(document.querySelector('[data-testid="workspace-repair-residual"]')).toBeNull();
    // The open note is still Notes.md, still in Edit, with the same unwritten
    // text — the reveal changed the tree's selection, not the open pane.
    const editor = document.querySelector('[data-testid="workspace-editor"]') as HTMLTextAreaElement;
    expect(editor).not.toBeNull();
    expect(editor.value).toBe("a paragraph the host has never seen");
  });
});
