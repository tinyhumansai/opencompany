// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { MemoryView } from "@/views/MemoryView";

/**
 * The Add learning form stays enabled while a save is in flight. When that
 * save completes, only the learning it actually sent may reset: text typed
 * during the request is a newer, unsent learning and clearing it loses work.
 */

function clientWithPendingSave() {
  let resolveSave: (v: unknown) => void = () => {};
  const post = vi.fn(
    () =>
      new Promise((resolve) => {
        resolveSave = resolve;
      }),
  );
  const get = vi.fn((path: string) => {
    if (path.endsWith("/memory/status")) return Promise.resolve({ root: "team:acme", on: true, engine: "local" });
    if (path.endsWith("/memory/agents")) return Promise.resolve({ root: "team:acme", agents: [] });
    if (path.endsWith("/memory/brain")) return Promise.resolve({ root: "team:acme", sources: [], unfiled: 0 });
    if (path.includes("/memory")) return Promise.resolve({ items: [] });
    return Promise.reject(new Error(`unexpected GET ${path}`));
  });
  const client = {
    scopeFor: () => "/api/v1/company/acme",
    get,
    post,
    del: vi.fn(() => Promise.resolve({})),
  } as unknown as OpenCompanyClient;
  return { client, post, finishSave: () => resolveSave({ id: "m9" }) };
}

let container: HTMLDivElement;
let root: Root;

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
});

function textarea(): HTMLTextAreaElement {
  return container.querySelector('[data-testid="memory-text"]') as HTMLTextAreaElement;
}

async function type(text: string) {
  const el = textarea();
  const setter = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), "value")!.set!;
  await act(async () => {
    setter.call(el, text);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

describe("Add learning save race", () => {
  it("keeps text typed while an earlier save is in flight", async () => {
    const { client, finishSave } = clientWithPendingSave();
    await act(async () => {
      root.render(createElement(MemoryView, { client, company: "acme", sub: "upload" }));
    });
    await act(async () => {});

    await type("Client prefers Friday reviews");
    await act(async () => {
      (container.querySelector('[data-testid="memory-save"]') as HTMLButtonElement).click();
    });
    await type("Client prefers Friday reviews and async standups");

    await act(async () => {
      finishSave();
    });
    await act(async () => {});

    expect(textarea().value).toBe(" and async standups");
  });

  it("keeps a kind changed while an earlier save is in flight", async () => {
    const { client, finishSave } = clientWithPendingSave();
    await act(async () => {
      root.render(createElement(MemoryView, { client, company: "acme", sub: "upload" }));
    });
    await act(async () => {});

    await type("Client prefers Friday reviews");
    await act(async () => {
      (container.querySelector('[data-testid="memory-save"]') as HTMLButtonElement).click();
    });
    // Change the kind while the save is pending: the save's completion must
    // not reset a choice it did not send.
    await act(async () => {
      (container.querySelector("#mem-kind") as HTMLElement).click();
    });
    const option = Array.from(document.querySelectorAll('[role="option"]')).find(
      (o) => o.textContent === "Preference",
    );
    if (!option) throw new Error("no Preference option in the open popup");
    await act(async () => {
      (option as HTMLElement).click();
    });
    await act(async () => {
      finishSave();
    });
    await act(async () => {});

    const trigger = container.querySelector("#mem-kind") as HTMLElement;
    expect(trigger.textContent).toContain("Preference");
  });

  it("still clears the form when nothing changed during the save", async () => {
    const { client, finishSave } = clientWithPendingSave();
    await act(async () => {
      root.render(createElement(MemoryView, { client, company: "acme", sub: "upload" }));
    });
    await act(async () => {});

    await type("Client prefers Friday reviews");
    await act(async () => {
      (container.querySelector('[data-testid="memory-save"]') as HTMLButtonElement).click();
    });
    await act(async () => {
      finishSave();
    });
    await act(async () => {});

    expect(textarea().value).toBe("");
  });
});
