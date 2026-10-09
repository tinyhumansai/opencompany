// @vitest-environment jsdom

// Guards the Brain "Add learning" flow against the save-vs-reload masquerade:
// the write and the post-write reload are two separate operations, and only a
// failed WRITE may surface the "could not save the learning" toast or leave the
// operator looking at what they typed. A reload that hangs or fails must never
// be reported as a save failure — that is what invites a retry and a duplicate
// memory.
//
// # The form is inline now, not a dialog
//
// It was a dialog opened from a button in the Brain header. The header is
// shared by every Brain view, so the control stood on Overview too, while the one page whose whole job is adding had no visible way
// to do it by hand. The form is a panel on the Upload tab now.
//
// So "the dialog closed" is no longer the observable. The panel has nothing to
// close: it CLEARS on a confirmed write, and keeps what you typed on a failed
// one. Same two states, same reason — text left standing after a successful
// save reads as work that has not been saved, and text thrown away after a
// failed save is work the operator has to type again.

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { MemoryList, MemoryStatus } from "@/api/memory";
import type { OpenCompanyClient } from "@/api/client";

// Partial mock: keep every constant the view and its dialog render from
// (kinds, styles, labels), stub only the network calls the add flow and the
// initial load touch.
const createMemory = vi.fn();
const listMemory = vi.fn();
const memoryStatus = vi.fn();
const memoryAgents = vi.fn();
const brainSources = vi.fn();

vi.mock("@/api/memory", async (importActual) => {
  const actual = await importActual<typeof import("@/api/memory")>();
  return { ...actual, createMemory, listMemory, memoryStatus, memoryAgents, brainSources };
});

vi.mock("sonner", () => ({
  toast: { error: vi.fn(), success: vi.fn(), message: vi.fn() },
}));

// Imported after the mocks are registered.
const { MemoryView } = await import("@/views/MemoryView");
const { toast } = await import("sonner");

const EMPTY_LIST: MemoryList = { items: [] };
const ON: MemoryStatus = { root: "team:acme", on: true, engine: "local" };

// A client the view never reaches past the api mock; every call stays pending
// so nothing drives state we are not testing.
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
  memoryStatus.mockResolvedValue(ON);
  memoryAgents.mockResolvedValue({ root: "team:acme", agents: [] });
  brainSources.mockResolvedValue({ root: "team:acme", sources: [], unfiled: 0 });
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

/** What the text box currently holds — empty once a save is confirmed. */
function titleValue(): string | null {
  const el = query("memory-text") as HTMLTextAreaElement | null;
  return el ? el.value : null;
}

// Type a learning into the Upload tab's panel and click Save.
async function openAndSave(): Promise<void> {
  const title = query("memory-text") as HTMLTextAreaElement | null;
  expect(title).not.toBeNull();
  act(() => {
    const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
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
  it("clears the form the instant the write is confirmed, before the reload settles", async () => {
    // The write succeeds, but the reload never settles. An `await load()` before
    // the reset would hang here and leave the operator's text standing —
    // indistinguishable from a save that did not happen.
    createMemory.mockResolvedValue({ id: "m1" });
    listMemory.mockReturnValue(new Promise<MemoryList>(() => {})); // never settles

    act(() => {
      root.render(
        createElement(MemoryView, { client: stubClient(), company: "acme", sub: "upload" }),
      );
    });
    await settle();

    await openAndSave();

    expect(titleValue()).toBe("");
    expect(toast.error).not.toHaveBeenCalled();
  });

  it("keeps a newer learning typed while the earlier save is pending", async () => {
    let resolveSave!: (value: { id: string }) => void;
    createMemory.mockImplementation(
      () => new Promise<{ id: string }>((resolve) => {
        resolveSave = resolve;
      }),
    );
    listMemory.mockResolvedValue(EMPTY_LIST);
    act(() => {
      root.render(createElement(MemoryView, { client: stubClient(), company: "acme", sub: "upload" }));
    });
    await settle();
    await openAndSave();
    const title = query("memory-text") as HTMLTextAreaElement;
    act(() => {
      const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
      setter?.call(title, "Newer unsaved learning");
      title.dispatchEvent(new Event("input", { bubbles: true }));
    });
    await settle();
    act(() => {
      resolveSave({ id: "m1" });
    });
    await settle();
    expect(createMemory).toHaveBeenCalledWith(expect.anything(), "acme", {
      kind: "fact",
      text: "Client prefers Friday reviews",
    });
    expect(titleValue()).toBe("Newer unsaved learning");
  });

  it("resets only the fields that were saved, keeping an untouched edit", async () => {
    let resolveSave!: (value: { id: string }) => void;
    createMemory.mockImplementation(
      () => new Promise<{ id: string }>((resolve) => {
        resolveSave = resolve;
      }),
    );
    listMemory.mockResolvedValue(EMPTY_LIST);
    act(() => {
      root.render(createElement(MemoryView, { client: stubClient(), company: "acme", sub: "upload" }));
    });
    await settle();
    await openAndSave();
    // Change only the kind while the write is pending. The saved text must
    // still clear (it was saved), and the newer kind choice must survive.
    act(() => {
      document.getElementById("mem-kind")?.click();
    });
    await settle();
    act(() => {
      const item = Array.from(document.querySelectorAll("[role=option]")).find(
        (el) => el.textContent?.trim() === "Preference",
      ) as HTMLElement | undefined;
      item?.click();
    });
    await settle();
    act(() => {
      resolveSave({ id: "m1" });
    });
    await settle();
    expect(titleValue()).toBe("");
    expect(document.getElementById("mem-kind")?.textContent).toContain("Preference");
  });

  it("does not report a failed reload as a failed save", async () => {
    createMemory.mockResolvedValue({ id: "m1" });
    listMemory.mockRejectedValue(new Error("reload boom"));

    act(() => {
      root.render(
        createElement(MemoryView, { client: stubClient(), company: "acme", sub: "upload" }),
      );
    });
    await settle();

    await openAndSave();

    expect(titleValue()).toBe("");
    expect(toast.error).not.toHaveBeenCalledWith("could not save the learning");
  });

  it("still reports a genuine save failure, and keeps what was typed", async () => {
    // Reject with a non-Error so the dialog's fallback copy is used verbatim.
    createMemory.mockRejectedValue("write failed");
    listMemory.mockResolvedValue(EMPTY_LIST);

    act(() => {
      root.render(
        createElement(MemoryView, { client: stubClient(), company: "acme", sub: "upload" }),
      );
    });
    await settle();

    await openAndSave();

    expect(toast.error).toHaveBeenCalledWith("could not save the learning");
    // Kept, so Save is a retry rather than a re-type.
    expect(titleValue()).toBe("Client prefers Friday reviews");
  });
});
