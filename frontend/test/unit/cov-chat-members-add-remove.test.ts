// @vitest-environment jsdom

import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { TeamMember } from "@/lib/team";
import { AddMemberDialog } from "@/views/room/AddMemberDialog";
import { MembersPane } from "@/views/room/MembersPane";

/**
 * Put an existing agent onto a channel's desk from the chat member pane
 * (issue #2224). `MembersPane` no longer creates or removes a teammate
 * itself — hiring lives on the empty-desk prompt and the Team page, and
 * dropping one from the roster entirely is a Team-page-only action now —
 * this pane's only mutation is `onAddExisting`, offered solely on an
 * "Everyone else" row when there is a real desk to add into.
 */

const MEMBER: TeamMember = {
  id: "m1",
  name: "Ada",
  role: "engineer",
  description: "",
  tone: "blue",
  avatar: "ada",
  inboxEnabled: false,
  effectiveTools: [],
  desks: [],
};

function paneProps(overrides: Record<string, unknown> = {}) {
  return {
    channelMembers: [],
    others: [MEMBER],
    people: [],
    loading: false,
    fromHost: true,
    onMessage: vi.fn(),
    onAddExisting: vi.fn(),
    ...overrides,
  };
}

let container: HTMLDivElement;
let root: Root;

/**
 * jsdom ships no `matchMedia`, and the mascot avatar swatch the avatar
 * picker now renders (`AvatarPicker`'s flavour grid,
 * `rendering-strategy.md`'s "12th tile") pulls in `@rive-app/react-canvas`,
 * whose own `useDevicePixelRatio` reaches for `matchMedia` unguarded — so
 * any dialog reaching that picker fails to mount without this. Same stub as
 * `chat-cognition-banner.test.ts` / `working-indicator.test.ts`.
 */
function stubMatchMedia() {
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
      onchange: null,
    }),
  });
}

beforeEach(() => {
  stubMatchMedia();
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

describe("MembersPane's add-existing action, on an Everyone-else row", () => {
  it("offers a + for Ada when there is a real desk to add her to", async () => {
    await act(async () => {
      root.render(createElement(MembersPane, paneProps()));
    });

    const add = container.querySelector('[aria-label="Add Ada to this channel"]') as HTMLButtonElement;
    expect(add).not.toBeNull();
  });

  it("wires the + straight to onAddExisting with the member's id", async () => {
    const onAddExisting = vi.fn();
    await act(async () => {
      root.render(createElement(MembersPane, paneProps({ onAddExisting })));
    });

    const add = container.querySelector('[aria-label="Add Ada to this channel"]') as HTMLButtonElement;
    await act(async () => add.click());

    expect(onAddExisting).toHaveBeenCalledWith("m1");
  });

  it("offers no + at all when the caller has no channel to add into (onAddExisting absent)", async () => {
    await act(async () => {
      root.render(createElement(MembersPane, paneProps({ onAddExisting: undefined })));
    });

    expect(container.querySelector('[aria-label="Add Ada to this channel"]')).toBeNull();
  });

  it("offers no + on a DM — real, non-null channelMembers that is not a desk", async () => {
    const onAddExisting = vi.fn();
    await act(async () => {
      // The caller (RoomView) gates `onAddExisting` on `activeIsDesk`, never on
      // `channelMembers` alone: a DM has real, non-null membership too (issue
      // #2224's DM regression). This pane must not re-derive that signal —
      // an absent `onAddExisting` renders no + no matter what channelMembers is.
      root.render(createElement(MembersPane, paneProps({ onAddExisting: undefined, channelMembers: [MEMBER] })));
    });

    expect(container.querySelector('[aria-label="Add Ada to this channel"]')).toBeNull();
    expect(onAddExisting).not.toHaveBeenCalled();
  });
});

describe("AddMemberDialog, when the write is refused", () => {
  function clientAs(): OpenCompanyClient {
    return {
      scopeFor: () => "/api/v1/companies/acme",
      get: (path: string) =>
        path.endsWith("/inference") ? Promise.resolve({ cognition: "echo" }) : Promise.resolve({}),
    } as unknown as OpenCompanyClient;
  }

  async function flush() {
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
  }

  it("keeps the dialog open and the typed fields intact for a retry, rather than closing on a write that never landed", async () => {
    const onAdd = vi.fn(async () => false);
    const onOpenChange = vi.fn();
    await act(async () => {
      root.render(
        createElement(AddMemberDialog, {
          open: true,
          onOpenChange,
          onAdd,
          client: clientAs(),
          company: "acme",
        }),
      );
    });
    await flush();

    const setInput = (el: HTMLInputElement, text: string) => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
      act(() => {
        setter.call(el, text);
        el.dispatchEvent(new Event("input", { bubbles: true }));
      });
    };
    setInput(document.body.querySelector("#agent-add-name") as HTMLInputElement, "Nova");
    setInput(document.body.querySelector("#agent-add-role") as HTMLInputElement, "Growth Marketer");

    const create = Array.from(document.body.querySelectorAll("button")).find(
      (b) => b.textContent === "Add agent" || b.textContent === "Adding…",
    ) as HTMLButtonElement;
    await act(async () => create.click());
    await flush();

    // `inbox` is gone with the per-agent inbox; `avatar` and `landOnProfile`
    // are what the reduced dialog adds. `avatar` is undefined because nobody
    // picked a face, which is not the same as picking the hashed mascot.
    expect(onAdd).toHaveBeenCalledWith({
      name: "Nova",
      role: "Growth Marketer",
      description: "",
      instructions: "",
      avatar: undefined,
      landOnProfile: true,
    });
    // Not closed on a failed write — the caller's own toast (RoomView.addMember)
    // is the visible error; this dialog's honest half is staying open and
    // retryable rather than claiming the write landed.
    expect(onOpenChange).not.toHaveBeenCalledWith(false);
    expect((document.body.querySelector("#agent-add-name") as HTMLInputElement).value).toBe("Nova");
    const retry = Array.from(document.body.querySelectorAll("button")).find(
      (b) => b.textContent === "Add agent",
    ) as HTMLButtonElement | undefined;
    expect(retry).not.toBeUndefined();
    expect(retry?.disabled).toBe(false);
  });
});
