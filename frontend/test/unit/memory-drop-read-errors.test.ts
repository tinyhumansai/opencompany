// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { toast } from "sonner";
import { DropZone } from "@/views/memory/DropZone";

/**
 * With memory off the host refuses every ingest (`409 not_configured`), but
 * the console's own lever is `off`, and `DropZone` is where it has to hold: a
 * raw drag-and-drop bypasses the two buttons' `disabled` prop entirely, so the
 * guard inside `onDrop` is the only thing standing between an operator's drop
 * and a request that can only fail.
 */

vi.mock("sonner", () => ({ toast: { error: vi.fn(), success: vi.fn() } }));

function dataTransferWith(files: File[]): DataTransfer {
  return {
    items: files.map((file) => ({
      kind: "file",
      webkitGetAsEntry: () => ({
        isFile: true,
        isDirectory: false,
        name: file.name,
        file: (cb: (f: File) => void) => cb(file),
      }),
    })),
    getData: () => "",
    files,
  } as unknown as DataTransfer;
}

let container: HTMLDivElement;
let root: Root;

async function show(client: OpenCompanyClient, off: boolean) {
  await act(async () => {
    root.render(
      createElement(DropZone, {
        client,
        company: "acme",
        off,
        onIngested: () => {},
      }),
    );
  });
}

function dropzone(): HTMLElement {
  return container.querySelector('[data-testid="memory-dropzone"]') as HTMLElement;
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.restoreAllMocks();
  vi.clearAllMocks();
});

describe("a drop while memory is off ingests nothing", () => {
  it("refuses a raw drop, which the two disabled buttons cannot stop", async () => {
    const postForm = vi.fn(() => Promise.resolve({ items: [] }));
    const client = {
      scopeFor: () => "/api/v1/company/acme",
      postForm,
      post: vi.fn(),
    } as unknown as OpenCompanyClient;
    await show(client, true);

    const file = new File(["hello"], "note.txt", { type: "text/plain" });
    await act(async () => {
      const event = new Event("drop", { bubbles: true, cancelable: true }) as unknown as Event & {
        dataTransfer: DataTransfer;
        preventDefault: () => void;
      };
      Object.defineProperty(event, "dataTransfer", { value: dataTransferWith([file]) });
      dropzone().dispatchEvent(event);
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(postForm).not.toHaveBeenCalled();
  });

  it("ingests normally once memory is on", async () => {
    const postForm = vi.fn(() => Promise.resolve({ items: [{ source: "note.txt", status: "stored" }] }));
    const client = {
      scopeFor: () => "/api/v1/company/acme",
      postForm,
      post: vi.fn(),
    } as unknown as OpenCompanyClient;
    await show(client, false);

    const file = new File(["hello"], "note.txt", { type: "text/plain" });
    await act(async () => {
      const event = new Event("drop", { bubbles: true, cancelable: true }) as unknown as Event & {
        dataTransfer: DataTransfer;
      };
      Object.defineProperty(event, "dataTransfer", { value: dataTransferWith([file]) });
      dropzone().dispatchEvent(event);
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(postForm).toHaveBeenCalled();
  });
});


describe("folder traversal failures", () => {
  it("reports unreadable entries rather than claiming every collected file was remembered", async () => {
    const postForm = vi.fn(async () => ({ items: [{ source: "folder/ok.txt", status: "stored" }] }));
    const client = { scopeFor: () => "/api/v1/company/acme", postForm, post: vi.fn() } as unknown as OpenCompanyClient;
    await show(client, false);
    const ok = new File(["hello"], "ok.txt", { type: "text/plain" });
    let page = 0;
    const folder = {
      isDirectory: true, isFile: false, name: "folder",
      createReader: () => ({ readEntries: (success: (value: unknown[]) => void, failure: (error: DOMException) => void) => {
        if (page++ === 0) success([{ isFile: true, isDirectory: false, name: "ok.txt", file: (cb: (file: File) => void) => cb(ok) }]);
        else failure(new DOMException("Permission denied", "NotReadableError"));
      } }),
    };
    await act(async () => {
      const event = new Event("drop", { bubbles: true, cancelable: true });
      Object.defineProperty(event, "dataTransfer", { value: { items: [{ kind: "file", webkitGetAsEntry: () => folder }], files: [], getData: () => "" } });
      dropzone().dispatchEvent(event);
      for (let i = 0; i < 15; i++) await Promise.resolve();
    });
    expect(toast.error).toHaveBeenCalledWith(expect.stringContaining("folder"));
    expect(toast.success).not.toHaveBeenCalled();
    expect(postForm).not.toHaveBeenCalled();
  });
  it("reports a file-read error before uploading any other files", async () => {
    const postForm = vi.fn(async () => ({ items: [] }));
    const client = { scopeFor: () => "/api/v1/company/acme", postForm, post: vi.fn() } as unknown as OpenCompanyClient;
    await show(client, false);
    await act(async () => {
      const event = new Event("drop", { bubbles: true, cancelable: true });
      Object.defineProperty(event, "dataTransfer", { value: {
        items: [{ kind: "file", webkitGetAsEntry: () => ({ isFile: true, isDirectory: false, name: "secret.txt", file: (_success: unknown, failure: (error: DOMException) => void) => failure(new DOMException("Permission denied", "NotReadableError")) }) }],
        files: [], getData: () => "",
      } });
      dropzone().dispatchEvent(event);
      for (let i = 0; i < 15; i++) await Promise.resolve();
    });
    expect(toast.error).toHaveBeenCalledWith(expect.stringContaining("secret.txt"));
    expect(postForm).not.toHaveBeenCalled();
  });

  it("does not traverse ignored directories", async () => {
    const read = vi.fn();
    const client = { scopeFor: () => "/api/v1/company/acme", postForm: vi.fn(), post: vi.fn() } as unknown as OpenCompanyClient;
    await show(client, false);
    await act(async () => {
      const event = new Event("drop", { bubbles: true, cancelable: true });
      Object.defineProperty(event, "dataTransfer", { value: {
        items: [{ kind: "file", webkitGetAsEntry: () => ({ isFile: false, isDirectory: true, name: ".git", createReader: read }) }],
        files: [], getData: () => "",
      } });
      dropzone().dispatchEvent(event);
      for (let i = 0; i < 15; i++) await Promise.resolve();
    });
    expect(read).not.toHaveBeenCalled();
    expect(toast.error).toHaveBeenCalledWith("nothing in that drop could be read");
  });

});
