// @vitest-environment jsdom

// Guards the Brain "New memory" flow against the save-vs-reload masquerade:
// the write and the post-write reload are two separate operations, and only a
// failed WRITE may surface the dialog's "could not save the memory" toast or
// keep the dialog open. A reload that hangs or fails must never be reported as
// a save failure, and must never strand the dialog open — an open dialog is
// what invites the operator to retry and write a duplicate memory.

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { MemoryList, MemoryStats } from "@/api/memory";
import type { OpenCompanyClient } from "@/api/client";

// Partial mock: keep every constant the view and its dialog render from
// (kinds, styles, labels, origins, documentSlug), stub only the three network
// calls the add flow and the initial load touch.
const createMemory = vi.fn();
const listMemory = vi.fn();
const memoryStats = vi.fn();

vi.mock("@/api/memory", async (importActual) => {
  const actual = await importActual<typeof import("@/api/memory")>();
  return { ...actual, createMemory, listMemory, memoryStats };
});

vi.mock("sonner", () => ({
  toast: { error: vi.fn(), success: vi.fn(), message: vi.fn() },
}));

// Imported after the mocks are registered.
const { MemoryView } = await import("@/views/MemoryView");
const { toast } = await import("sonner");

const EMPTY_LIST: MemoryList = { items: [], totalContext: 0, contextTruncated: false };
const EMPTY_STATS: MemoryStats = {
  facts: 0,
  factsUpdatedAtMillis: 0,
  lastUpdatedAtMillis: 0,
  totalItems: 0,
  teammateMemory: 0,
  documentMemory: 0,
  taskOutcomes: 0,
};

// A client whose only reachable call from this view (after the api mock) is
// EngineSection's `memoryEngine` → `client.get`; leave it pending so the panel
// sits in its skeleton and never drives state we are not testing.
function stubClient(): OpenCompanyClient {
  const pending = () => new Promise<never>(() => {});
  return {
    scopeFor: () => "/companies/acme",
    get: vi.fn(pending),
    post: vi.fn(pending),
    put: vi.fn(pending),
    del: vi.fn(pending),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement("div");
  document.body.appendChild(container);
  act(() => {
    root = createRoot(container);
  });
  vi.clearAllMocks();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

async function settle(): Promise<void> {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

function query(testid: string): HTMLElement | null {
  return document.querySelector<HTMLElement>(`[data-testid="${testid}"]`);
}

function dialogEl(): HTMLElement | null {
  return document.querySelector<HTMLElement>('[role="dialog"]');
}

// Open the add dialog, type a title, and click Save.
async function openAndSave(): Promise<void> {
  act(() => {
    query("memory-add")?.click();
  });
  await settle();

  const title = query("memory-title") as HTMLInputElement | null;
  expect(title).not.toBeNull();
  act(() => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
    setter?.call(title, "Client prefers Friday reviews");
    title?.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await settle();

  act(() => {
    query("memory-save")?.click();
  });
  await settle();
}

describe("MemoryView add: save vs reload", () => {
  it("closes the dialog the instant the write is confirmed, before the reload settles", async () => {
    // RED-FIRST: the write succeeds, but the reload never settles. The old
    // `await load()` before `setAddOpen(false)` would hang here and leave the
    // dialog open — the reorder closes it on the confirmed write instead.
    createMemory.mockResolvedValue({ id: "m1" });
    listMemory.mockReturnValue(new Promise<MemoryList>(() => {})); // never settles
    memoryStats.mockReturnValue(new Promise<MemoryStats>(() => {}));

    act(() => {
      root.render(
        createElement(MemoryView, { client: stubClient(), company: "acme" }),
      );
    });
    await settle();

    await openAndSave();

    expect(dialogEl()).toBeNull();
    expect(query("memory-save")).toBeNull();
    expect(toast.error).not.toHaveBeenCalled();
  });

  it("does not report a failed reload as a failed save", async () => {
    createMemory.mockResolvedValue({ id: "m1" });
    listMemory.mockRejectedValue(new Error("reload boom"));
    memoryStats.mockResolvedValue(EMPTY_STATS);

    act(() => {
      root.render(
        createElement(MemoryView, { client: stubClient(), company: "acme" }),
      );
    });
    await settle();

    await openAndSave();

    expect(dialogEl()).toBeNull();
    expect(query("memory-save")).toBeNull();
    expect(toast.error).not.toHaveBeenCalledWith("could not save the memory");
  });

  it("still reports a genuine save failure and keeps the dialog open", async () => {
    // Reject with a non-Error so the dialog's fallback copy is used verbatim.
    createMemory.mockRejectedValue("write failed");
    listMemory.mockResolvedValue(EMPTY_LIST);
    memoryStats.mockResolvedValue(EMPTY_STATS);

    act(() => {
      root.render(
        createElement(MemoryView, { client: stubClient(), company: "acme" }),
      );
    });
    await settle();

    await openAndSave();

    expect(toast.error).toHaveBeenCalledWith("could not save the memory");
    expect(dialogEl()).not.toBeNull();
    expect(query("memory-save")).not.toBeNull();
  });
});
