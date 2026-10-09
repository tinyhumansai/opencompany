// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { DiscussionMessage } from "@/api/tasks";

/**
 * The Discussion tab's two silences.
 *
 * **A message is truncated at 4000 codepoints, host-side, with a `201`.**
 * `cap_discussion` in `src/ports/tasks.rs` takes the first 4000 characters and
 * stores them; the write succeeds either way. Nothing in the composer said so —
 * no counter, no cap — so an operator pasting a long note learned the limit
 * existed by reading their own echoed row and finding its tail gone, after the
 * words were unrecoverable.
 *
 * **Withdrawing a message had no busy state.** It is the only mutating control
 * on this screen that did not: `redact()` set no flag, so a second click fired
 * a second `DELETE`, and a third a third. The host is idempotent so nothing
 * ever broke — what it cost was the operator's confidence, because the only
 * answer to a click was the row changing whenever a request happened to land.
 */

const toasts = vi.hoisted(() => ({
  base: vi.fn(),
  success: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
  info: vi.fn(),
}));

vi.mock("sonner", () => {
  const toast = Object.assign(toasts.base, {
    success: toasts.success,
    error: toasts.error,
    warning: toasts.warning,
    info: toasts.info,
  });
  return { toast };
});

const { DiscussionTab, MAX_DISCUSSION_CHARS, countDiscussionChars } = await import(
  "@/views/TaskDetailView"
);

const MESSAGE: DiscussionMessage = {
  seq: 7,
  author: "ops",
  atMillis: new Date("2026-03-02T10:00:00Z").getTime(),
  text: "the invoice numbers do not line up",
};

/** Counts `DELETE`s and never settles them, so "in flight" is a real state. */
function stallingClient(seen: string[]): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    del: async (path: string) => {
      seen.push(path);
      return new Promise(() => {});
    },
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

async function render(client: OpenCompanyClient) {
  await act(async () => {
    root.render(
      createElement(DiscussionTab, {
        messages: [MESSAGE],
        hasMore: false,
        taskId: "task-1",
        client,
        company: "acme",
        onPosted: async () => {},
      }),
    );
  });
}

function textarea(): HTMLTextAreaElement {
  return container.querySelector("textarea") as HTMLTextAreaElement;
}

function post(): HTMLButtonElement {
  return container.querySelector('[data-testid="discussion-post"]') as HTMLButtonElement;
}

/** Records the bodies a post actually reached the client with. */
function postingClient(seen: string[]): OpenCompanyClient {
  return {
    scopeFor: (company: string | null) => `/api/v1/${company ?? "company"}`,
    post: async (_path: string, body: unknown) => {
      seen.push((body as { text: string }).text);
      return { seq: 8, author: "ops", atMillis: 0, text: (body as { text: string }).text };
    },
  } as unknown as OpenCompanyClient;
}

/**
 * The counter the composer names in `aria-describedby`.
 *
 * Found through that attribute rather than by class or position, because the
 * link is half of what is being claimed: a description the textarea does not
 * point at is one no screen reader reads. `getElementById` rather than a `#id`
 * selector — `useId` mints ids containing characters a CSS selector would need
 * escaped, and jsdom here has no escaping helper to do it with.
 */
function counter(): HTMLElement {
  const id = textarea().getAttribute("aria-describedby");
  expect(id).toBeTruthy();
  const found = document.getElementById(id!);
  expect(found).not.toBeNull();
  return found!;
}

/** React listens for `input` on its own value setter, not on `.value = …`. */
async function type(el: HTMLTextAreaElement, value: string) {
  const setter = Object.getOwnPropertyDescriptor(
    window.HTMLTextAreaElement.prototype,
    "value",
  )!.set!;
  await act(async () => {
    setter.call(el, value);
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
    true;
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  vi.clearAllMocks();
});

describe("countDiscussionChars", () => {
  it("counts what the operator typed", () => {
    expect(countDiscussionChars("a short note")).toBe(12);
    expect(countDiscussionChars("")).toBe(0);
  });

  it("counts codepoints, the way the host counts them", () => {
    // `String.length` is UTF-16 code units, so a message of astral-plane
    // characters would measure double the limit the host actually applies, and
    // the counter would be describing a different rule from the one the host
    // enforces. `cap_discussion` in `src/ports/tasks.rs` takes `chars()`.
    //
    // A `maxLength` on the textarea would have had the same flaw, which is why
    // there is not one — and it could not be undone either, which is the other
    // reason.
    expect("🙂".repeat(10).length).toBe(20);
    expect(countDiscussionChars("🙂".repeat(10))).toBe(10);
  });
});

describe("the composer surfaces the limit", () => {
  it("describes the box by a counter that is always there for a screen reader", async () => {
    await render(stallingClient([]));
    expect(counter().textContent).toContain(`of ${MAX_DISCUSSION_CHARS} characters`);
  });

  it("stays out of the way for a one-line note", async () => {
    await render(stallingClient([]));
    // A permanent "0 of 4000" under every note is noise that teaches nobody
    // anything, so it is hidden until the count starts to matter.
    expect(counter().className).toContain("sr-only");
  });

  it("shows itself as the limit comes into range", async () => {
    await render(stallingClient([]));
    await type(textarea(), "x".repeat(MAX_DISCUSSION_CHARS - 10));
    expect(counter().className).not.toContain("sr-only");
    expect(counter().textContent).toContain(`${MAX_DISCUSSION_CHARS - 10} of`);
  });

  it("says so at exactly the cap, without calling it an error", async () => {
    await render(stallingClient([]));
    await type(textarea(), "x".repeat(MAX_DISCUSSION_CHARS));
    expect(counter().textContent).toContain("the most a message can hold");
    expect(post().disabled).toBe(false);
  });

  /**
   * The overflow is refused, **not destroyed**.
   *
   * Clipping inside `onChange` reached for the same goal and cost more than the
   * truncation it was guarding against: pasting 4,200 characters silently threw
   * 200 of them away, past the reach of the browser's undo, and reported it
   * with a counter reading a number the operator had never typed. Keeping the
   * words and taking Post down says the same thing while the text is still on
   * screen to shorten.
   */
  it("keeps an over-long paste and refuses to send it", async () => {
    await render(stallingClient([]));
    const over = "x".repeat(MAX_DISCUSSION_CHARS + 200);
    await type(textarea(), over);

    expect(textarea().value).toBe(over);
    expect(countDiscussionChars(textarea().value)).toBe(MAX_DISCUSSION_CHARS + 200);
    expect(post().disabled).toBe(true);
    expect(counter().textContent).toContain("200 over the most a message can hold");
  });

  /**
   * Codepoints on the live path too, not only in the counting helper. A
   * `String.length` gate would have called 2,100 emoji — well inside the host's
   * limit — an overflow and refused a message the host would have taken whole.
   */
  it("measures a paste of emoji the way the host does", async () => {
    await render(stallingClient([]));
    // 2100 codepoints, but 4200 UTF-16 units: over the cap only if you count
    // the wrong thing.
    await type(textarea(), "🙂".repeat(2_100));
    expect(post().disabled).toBe(false);
    expect(counter().className).toContain("sr-only");
  });

  /**
   * Enter posts, and it does not consult the button's `disabled` attribute —
   * so the refusal has to live in `post()` as well, or the keyboard walks
   * straight past it into the truncating host.
   */
  it("refuses the same message on Enter as it does on the button", async () => {
    const seen: string[] = [];
    await render(postingClient(seen));
    await type(textarea(), "x".repeat(MAX_DISCUSSION_CHARS + 1));
    await act(async () => {
      textarea().dispatchEvent(
        new KeyboardEvent("keydown", { key: "Enter", bubbles: true }),
      );
    });
    expect(seen).toHaveLength(0);

    // The same gesture on a message that fits does post, so the assertion above
    // is about the length and not about the key never working.
    await type(textarea(), "looks good to me");
    await act(async () => {
      textarea().dispatchEvent(
        new KeyboardEvent("keydown", { key: "Enter", bubbles: true }),
      );
    });
    expect(seen).toHaveLength(1);
  });
});

describe("the composer keeps IME confirmation separate from posting", () => {
  it.each([
    { label: "composing Enter", isComposing: true, keyCode: 13, draft: "日本語を入力" },
    { label: "Safari's composition-ending Enter", isComposing: false, keyCode: 229, draft: "中文输入" },
  ])("keeps the draft on $label, then posts on an ordinary Enter", async ({ isComposing, keyCode, draft }) => {
    const seen: string[] = [];
    await render(postingClient(seen));
    await type(textarea(), draft);
    const confirmation = new KeyboardEvent("keydown", {
      key: "Enter", bubbles: true, cancelable: true, isComposing, keyCode,
    });
    await act(async () => { textarea().dispatchEvent(confirmation); });

    expect(seen).toEqual([]);
    expect(textarea().value).toBe(draft);
    expect(confirmation.defaultPrevented).toBe(false);

    const enter = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true });
    await act(async () => { textarea().dispatchEvent(enter); });
    expect(seen).toEqual([draft]);
    expect(textarea().value).toBe("");
    expect(enter.defaultPrevented).toBe(true);
  });

  it("leaves Shift+Enter to the textarea", async () => {
    const seen: string[] = [];
    await render(postingClient(seen));
    await type(textarea(), "a note");
    const newline = new KeyboardEvent("keydown", {
      key: "Enter", shiftKey: true, bubbles: true, cancelable: true,
    });
    await act(async () => { textarea().dispatchEvent(newline); });
    expect(seen).toEqual([]);
    expect(textarea().value).toBe("a note");
    expect(newline.defaultPrevented).toBe(false);
  });
});

describe("withdrawing a message has an in-flight state", () => {
  /** Presses Remove and confirms it in the portalled dialog. */
  async function withdraw() {
    const trigger = container.querySelector(
      '[data-testid="discussion-redact"]',
    ) as HTMLButtonElement;
    await act(async () => trigger.click());
    const confirm = [...document.querySelectorAll("button")].find(
      (b) => b.textContent?.trim() === "Remove it",
    ) as HTMLButtonElement;
    await act(async () => confirm.click());
  }

  it("sends one DELETE per confirmation, not one per click", async () => {
    const seen: string[] = [];
    await render(stallingClient(seen));
    await withdraw();
    expect(seen).toHaveLength(1);

    // The request never settles, so this is the repeat-click window exactly.
    const trigger = container.querySelector(
      '[data-testid="discussion-redact"]',
    ) as HTMLButtonElement;
    expect(trigger.disabled).toBe(true);
    await act(async () => trigger.click());
    expect(seen).toHaveLength(1);
  });

  it("shows which row is being withdrawn", async () => {
    await render(stallingClient([]));
    await withdraw();
    const trigger = container.querySelector('[data-testid="discussion-redact"]')!;
    expect(trigger.getAttribute("data-busy")).toBe("true");
    expect(trigger.querySelector(".animate-spin")).not.toBeNull();
  });

  it("leaves the control alone until something is actually in flight", async () => {
    await render(stallingClient([]));
    const trigger = container.querySelector(
      '[data-testid="discussion-redact"]',
    ) as HTMLButtonElement;
    expect(trigger.disabled).toBe(false);
    expect(trigger.getAttribute("data-busy")).toBeNull();
  });
});
