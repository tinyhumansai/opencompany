// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { DropZone } from "@/views/memory/DropZone";

vi.mock("sonner", () => ({
  toast: { success: vi.fn(), error: vi.fn(), warning: vi.fn() },
}));

import { toast } from "sonner";

/**
 * A folder whose directory read fails partway through must not be announced
 * as fully remembered: `readEntries` errors resolve as an empty batch, which
 * looks exactly like the end of a directory, and `file()` errors resolve as
 * null files that drop out of the upload set silently.
 */

function client() {
  const postForm = vi.fn(async (_path: string, _form: FormData) => ({
    items: [{ name: "ok.txt", status: "stored" }],
  }));
  const c = {
    scopeFor: (company: string | null) => `/api/v1/company/${company}`,
    postForm,
  } as unknown as OpenCompanyClient;
  return { client: c, postForm };
}

function transferWithFailingDirectory(): DataTransfer {
  const file = new File(["hello"], "ok.txt");
  let batches = 0;
  const dirEntry = {
    isFile: false,
    isDirectory: true,
    name: "docs",
    createReader: () => ({
      readEntries: (ok: (b: unknown[]) => void, fail: (e: unknown) => void) => {
        batches += 1;
        if (batches === 1) {
          ok([
            {
              isFile: true,
              isDirectory: false,
              name: "ok.txt",
              file: (cb: (f: File) => void) => cb(file),
            },
          ]);
        } else {
          // The second page fails: the first file already uploaded, so this
          // is a partial read that must surface.
          fail(new Error("read failed"));
        }
      },
    }),
  };
  return {
    items: [{ kind: "file", webkitGetAsEntry: () => dirEntry }],
    getData: () => "",
    files: [],
  } as unknown as DataTransfer;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  vi.clearAllMocks();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("DropZone partial reads", () => {
  it("warns instead of claiming full success when a directory read fails partway", async () => {
    const { client: c, postForm } = client();
    await act(async () => {
      root.render(
        createElement(DropZone, {
          client: c,
          company: "acme",
          off: false,
          onIngested: () => {},
        }),
      );
    });
    const zone = container.querySelector('[data-testid="memory-dropzone"]') as HTMLElement;
    await act(async () => {
      const event = new Event("drop", { bubbles: true, cancelable: true }) as DragEvent;
      Object.defineProperty(event, "dataTransfer", { value: transferWithFailingDirectory() });
      zone.dispatchEvent(event);
      await new Promise((r) => setTimeout(r, 0));
    });

    expect(toast.warning).toHaveBeenCalledTimes(1);
    expect(vi.mocked(toast.warning).mock.calls[0][0]).toContain("skipped");
    expect(toast.success).not.toHaveBeenCalled();
    // The readable file really went up: the partial read was not a total one.
    expect(postForm).toHaveBeenCalledTimes(1);
    const form = vi.mocked(postForm).mock.calls[0][1];
    expect(form.getAll("file")).toHaveLength(1);
  });
});
