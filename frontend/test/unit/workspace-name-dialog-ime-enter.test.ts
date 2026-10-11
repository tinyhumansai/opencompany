// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { WorkspaceView } from "@/views/WorkspaceView";

/**
 * The workspace name dialog (new note, new folder, rename) submits on Enter.
 * An Enter that confirms an IME candidate still carries `isComposing` on the
 * keydown, and the dialog used to read it as "submit" — creating or renaming
 * with the half-composed text and closing the dialog mid-word. The key must be
 * left to the composition; a settled Enter submits exactly as before.
 */

function node(over: { id: string; name: string; kind: "folder" | "file"; parentId?: string }) {
  return { ...over, updatedAt: 1 };
}

const TREE = [
  node({ id: "standards", name: "Standards", kind: "folder" }),
  node({ id: "eng", name: "Engineering", kind: "folder", parentId: "standards" }),
];

let container: HTMLDivElement;
let root: Root;
let client: {
  scopeFor: () => string;
  get: ReturnType<typeof vi.fn>;
  post: ReturnType<typeof vi.fn>;
};

function host() {
  return {
    scopeFor: () => "/api/v1/company/acme",
    get: vi.fn(async (path: string) => {
      if (path.includes("/workspace/file/")) {
        return { id: "note", name: "Plan.md", content: "", backlinks: [], updatedAt: 1 };
      }
      return TREE;
    }),
    post: vi.fn(async (_path: string, body: { name: string; kind: string }) =>
      node({ id: "new", name: body.name, kind: body.kind as "file" | "folder" }),
    ),
  };
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
});

async function render() {
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
}

function actionsButtonFor(name: string): HTMLButtonElement {
  const row = Array.from(container.querySelectorAll("div.group")).find((d) =>
    d.textContent?.includes(name),
  );
  const found = row?.querySelector('[aria-label="Actions"]');
  if (!found) throw new Error(`no Actions button for "${name}"`);
  return found as HTMLButtonElement;
}

function menuItem(label: string): HTMLElement {
  const found = Array.from(document.querySelectorAll('[role="menuitem"]')).find(
    (el) => el.textContent?.trim() === label,
  );
  if (!found) throw new Error(`no "${label}" menu item`);
  return found as HTMLElement;
}

async function openNewFolderDialog() {
  await render();
  await act(async () => {
    actionsButtonFor("Standards").click();
  });
  await act(async () => {
    menuItem("New folder here").click();
  });
  const input = document.querySelector("#fs-name") as HTMLInputElement | null;
  if (!input) throw new Error("the name dialog did not mount its field");
  return input;
}

function typeInto(input: HTMLInputElement, text: string) {
  // React tracks the DOM value node-side; only the prototype setter gives
  // `onChange` a change it can see.
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
  act(() => {
    setter?.call(input, text);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

function enter(input: HTMLInputElement, isComposing: boolean) {
  act(() => {
    input.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true, isComposing }),
    );
  });
}

const workspacePosts = () =>
  client.post.mock.calls.filter((c) => (c[0] as string).endsWith("/workspace"));

describe("the workspace name dialog and IME composition", () => {
  it("does not submit on an Enter that is still confirming a candidate", async () => {
    const input = await openNewFolderDialog();
    typeInto(input, "かな");
    enter(input, true);

    expect(workspacePosts()).toHaveLength(0);
    expect(document.querySelector("#fs-name")).not.toBeNull();

    enter(input, false);
    expect(workspacePosts()).toHaveLength(1);
    expect((workspacePosts()[0][1] as { name: string }).name).toBe("かな");
  });

  it("does not submit on Safari's completing Enter (compositionend, then a plain Enter)", async () => {
    // WebKit bug 165004: the completing keydown arrives after compositionend
    // with isComposing already false. Only the just-ended burst marks it.
    const input = await openNewFolderDialog();
    typeInto(input, "かな");
    act(() => {
      input.dispatchEvent(new Event("compositionend", { bubbles: true }));
    });
    enter(input, false);

    expect(workspacePosts()).toHaveLength(0);
    expect(document.querySelector("#fs-name")).not.toBeNull();
  });
});
