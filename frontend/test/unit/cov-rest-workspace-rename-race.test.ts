// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { WorkspaceView } from "@/views/WorkspaceView";

/**
 * WS-001: the two-writer race on rename/move, never independently checked.
 *
 * The Rename dialog fires-and-forgets (`void rename(...)`, then closes
 * immediately) rather than waiting on the `PATCH`, so an operator — or a
 * second tab on the same node — can start a second rename before the first
 * one's response lands. `rename()` applies whichever response resolves last,
 * unconditionally (`setNodes(all => all.map(n => n.id === updated.id ?
 * updated : n))`), with no per-node request generation the way every other
 * network-racy surface in this codebase (`InboxView`, `ArtifactsTab`,
 * `PagesView`, `TaskDetailView`) carries. This test proves which behaviour
 * the code actually has.
 */

function node(over: { id: string; name: string; kind: "folder" | "file"; parentId?: string }) {
  return { ...over, updatedAt: 1 };
}

const TREE = [node({ id: "note", name: "Plan.md", kind: "file" })];

let container: HTMLDivElement;
let root: Root;

function menuItem(label: string): HTMLElement {
  const found = Array.from(document.querySelectorAll('[role="menuitem"]')).find(
    (el) => el.textContent?.trim() === label,
  );
  if (!found) throw new Error(`no "${label}" menu item`);
  return found as HTMLElement;
}

function actionsButtonFor(name: string): HTMLButtonElement {
  const row = Array.from(container.querySelectorAll("div.group")).find((d) =>
    d.textContent?.includes(name),
  );
  const found = row?.querySelector('[aria-label="Actions"]');
  if (!found) throw new Error(`no Actions button for "${name}"`);
  return found as HTMLButtonElement;
}

async function openRename() {
  await act(async () => {
    actionsButtonFor("Plan").click();
  });
  await act(async () => {
    menuItem("Rename").click();
  });
}

async function submitRename(name: string) {
  const input = document.getElementById("fs-name") as HTMLInputElement;
  await act(async () => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
    setter?.call(input, name);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await act(async () => {
    (Array.from(document.querySelectorAll("button")).find(
      (b) => b.textContent?.trim() === "Rename",
    ) as HTMLButtonElement).click();
  });
}

beforeEach(() => {
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
  localStorage.clear();
});

describe("two overlapping renames of the same node race on the network, not on the operator's intent", () => {
  // Fixed in `rename()`: a per-node generation counter — the gate InboxView,
  // ArtifactsTab, PagesView and TaskDetailView already carry for their racy
  // reads — so a late, superseded response can no longer overwrite the newer
  // name, and a stale failure cannot toast over the newer attempt's outcome.
  it("keeps the operator's second, newer rename even when the first request's response arrives after it", async () => {
    let resolveFirst: ((node: unknown) => void) | null = null;
    let resolveSecond: ((node: unknown) => void) | null = null;
    let call = 0;
    const patch = vi.fn(() => {
      call += 1;
      if (call === 1) return new Promise((resolve) => (resolveFirst = resolve));
      return new Promise((resolve) => (resolveSecond = resolve));
    });
    const client = {
      scopeFor: () => "/api/v1/company/acme",
      get: vi.fn().mockResolvedValue(TREE),
      patch,
      listTeam: vi.fn().mockResolvedValue([]),
    } as unknown as OpenCompanyClient;

    await act(async () => {
      root.render(
        createElement(ConnectionScopeProvider, {
          scope: { connection: "c1", company: "acme" },
          children: createElement(WorkspaceView, { client, company: "acme" }),
        }),
      );
    });

    await openRename();
    await submitRename("First.md");
    // The dialog closed on submit, fire-and-forget — the operator can already
    // rename again before the first PATCH has answered.
    await openRename();
    await submitRename("Second.md");

    expect(patch).toHaveBeenCalledTimes(2);

    // Resolve out of order: the SECOND (newer) request's response lands
    // first, then the stale first request's response lands after it.
    await act(async () => {
      resolveSecond?.(node({ id: "note", name: "Second.md", kind: "file" }));
      await Promise.resolve();
    });
    await act(async () => {
      resolveFirst?.(node({ id: "note", name: "First.md", kind: "file" }));
      await Promise.resolve();
    });

    const label = container.querySelector('[data-testid="workspace-tree-name"]');
    // The tree renders `titleOf` (the extension-stripped title — the row is
    // found by "Plan" above, not "Plan.md"), so the newer name proves itself
    // as "Second"; the stale response would show "First".
    expect(label?.textContent).toBe("Second");
  });
});
