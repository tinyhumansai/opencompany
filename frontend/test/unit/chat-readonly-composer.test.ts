// @vitest-environment jsdom

import { act, createElement, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { writeLastChannel } from "@/lib/last-channel";
import { TOUR } from "@/tour/steps";
import { RoomView } from "@/views/RoomView";

/**
 * The channel composer answers a read-only channel by not existing, and the
 * echo-brain notice sits next to the control it qualifies — and stays on the
 * feed that has no such control, where the attribution it corrects is the one
 * the reader cannot check by asking.
 *
 * # Why a render test and not a source scan
 *
 * Both facts are about what is *on screen*, and both were previously "true"
 * in a form that read as fixed and was not. The composer was `disabled`, which
 * is still a claim that the action exists: under a notice reading "There is
 * nothing to reply to here", a read-only channel drew a text input, three intent
 * chips, a mention button, a paperclip, a formatting toggle, a Send button and
 * an "Enter to send" hint. And the notice explaining that replies come from
 * the offline echo brain sat above the transcript, at the far end of the page
 * from the Send that provokes one.
 *
 * A grep cannot tell a rendered control from a removed one, so this mounts the
 * real `RoomView` against a stub client and asks the DOM.
 *
 * # The writable half is not optional
 *
 * Every read-only assertion here is an assertion of absence, and absence is
 * also what a `RoomView` that failed to mount produces. The writable cases
 * pin the same queries finding everything, off the same fixture — so a
 * mount that silently renders nothing fails rather than passing twice.
 */

const DESK_DTO = {
  id: "front-desk",
  name: "Front desk",
  description: "Where requests come in",
  members: [] as string[],
};

/** The writable desk channel, and the read-only `#general` archive. */
const WRITABLE = "front-desk";
const ARCHIVE = "main";
const READ_ONLY_NOTICE = "nothing can be posted here";

function stubClient(cognition: string | null): OpenCompanyClient {
  return {
    listDesks: vi.fn(async () => [DESK_DTO]),
    listTeam: vi.fn(async () => []),
    mentionables: vi.fn(async () => []),
    capabilityStatus: vi.fn(async () => ({ cognition })),
    chat: vi.fn(),
    reactToMessage: vi.fn(),
    getBudgetPause: vi.fn(async () => null),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;
let onNavigate: ReturnType<typeof vi.fn>;

beforeEach(() => {
  (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  // `useIsDesktop` reads `matchMedia`, which jsdom does not implement. A
  // desktop viewport keeps both panes mounted, which is the case under test.
  window.matchMedia = ((query: string) => ({
    matches: query.includes("min-width"),
    media: query,
    onchange: null,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    dispatchEvent: () => false,
  })) as unknown as typeof window.matchMedia;
  Object.defineProperty(window, "innerWidth", { value: 1440, writable: true });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  onNavigate = vi.fn();
  window.localStorage.clear();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

/**
 * One run for the in-flight bar.
 *
 * Every other test here passes no `inflightRuns` at all, which is why the
 * sibling-order tests below could once claim the banner and the composer are
 * adjacent: `RoomView` gates `InflightRunBar` on the prop being defined, so a
 * harness that omits it never renders the row that actually sits between them
 * (codex and CodeRabbit, both on PR #2159).
 */
const INFLIGHT_RUN = {
  taskId: "t-1",
  key: "run-1",
  kind: "task",
  title: "Weekly pipeline review",
  agentId: "pm",
  startedAt: 0,
  pendingAction: null,
} as const;

function tree(
  client: OpenCompanyClient,
  sub: string,
  typing: string[] = [],
  inflight = false,
): ReactNode {
  const view = createElement(RoomView, {
    client,
    company: "acme",
    sub,
    onNavigate,
    transcripts: {},
    setTranscripts: vi.fn(),
    // Who the shell says is at a keyboard in this channel. Empty by default;
    // the sibling-order test below supplies a name, because `TypingLine`
    // renders nothing at all when nobody is typing and the banner's placement
    // was only ever wrong when it renders something.
    resolveTypingNames: () => typing,
    // Undefined by default, because that is the shape most of these cases care
    // about — but a shell in production always passes both, so the in-flight
    // order test opts in.
    ...(inflight ? { inflightRuns: [INFLIGHT_RUN], onInflightSteered: vi.fn() } : {}),
    // The live-scope escape hatch `send` reads to decide whether a reply still
    // belongs to the company on screen. Nothing here sends.
    scopeRef: { current: { connection: "local", company: "acme", client } },
  });
  return createElement(ConnectionScopeProvider, {
    scope: { connection: "local", company: "acme" },
    children: view,
  });
}

/**
 * Render (or re-render) this root at `sub`, then let the reads settle.
 *
 * Re-rendering the same root with the same client is how the draft test walks
 * between channels: React reconciles `RoomView` in place, which is exactly the
 * production path an operator takes when they click another channel in the
 * rail. Remounting instead would discard the composer's state for reasons that
 * have nothing to do with the behaviour under test, and the test would pass
 * against any implementation.
 */
async function renderAt(
  client: OpenCompanyClient,
  sub: string,
  typing: string[] = [],
  inflight = false,
) {
  await act(async () => {
    root.render(tree(client, sub, typing, inflight));
  });
  // Let the desks / capability reads settle.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

async function mount(
  sub: string,
  cognition: string | null = null,
  typing: string[] = [],
  inflight = false,
) {
  const client = stubClient(cognition);
  await renderAt(client, sub, typing, inflight);
  return client;
}

/** The main channel composer's textarea — `MessageComposer` labels it. */
function composerInput() {
  return container.querySelector('textarea[aria-label^="Message "]');
}

function readOnlyComposerInput() {
  return container.querySelector('textarea[aria-label="This channel is read-only"]');
}

function banner() {
  return container.querySelector('[data-testid="chat-cognition-banner"]');
}

describe("a read-only channel renders no composer", () => {
  it("draws neither the composer nor its placeholder", async () => {
    await mount(ARCHIVE);

    expect(composerInput()).toBeNull();
    expect(readOnlyComposerInput()).toBeNull();
    expect(container.querySelector("textarea")).toBeNull();
  });

  it("draws no Send button and no intent chips", async () => {
    await mount(ARCHIVE);

    expect(container.querySelector('[aria-label="Send"]')).toBeNull();
    expect(container.querySelector('[aria-label="What this message is for"]')).toBeNull();
    for (const chip of ["Just chatting", "Do it once", "Build me the automation"]) {
      expect(container.textContent).not.toContain(chip);
    }
  });

  it("draws none of the mention, attach or formatting controls", async () => {
    await mount(ARCHIVE);

    for (const label of ["Mention someone", "Attach a file", "Formatting"]) {
      expect(container.querySelector(`[aria-label="${label}"]`)).toBeNull();
    }
  });

  it("drops the keyboard hint, which describes a send that cannot happen", async () => {
    await mount(ARCHIVE);

    expect(container.textContent).not.toContain("to send");
    expect(container.textContent).not.toContain("for a new line");
  });

  it("keeps the notice that explains why", async () => {
    await mount(ARCHIVE);

    expect(container.textContent).toContain(READ_ONLY_NOTICE);
  });

  it("offers neither empty-state card, since neither action exists here", async () => {
    await mount(ARCHIVE);

    // "Give the team a brief" prefills a composer this channel does not
    // render; "Add people" opens a members pane `RoomView` gates off on the
    // same flag. Both were dead controls under the notice.
    expect(container.textContent).not.toContain("Give the team a brief");
    expect(container.textContent).not.toContain("Add people");
  });
});

describe("a writable channel still renders the whole composer", () => {
  it("draws the input, the Send button and the controls", async () => {
    await mount(WRITABLE);

    expect(composerInput()).not.toBeNull();
    expect(container.querySelector('[aria-label="Send"]')).not.toBeNull();
    // The intent chips ("Just chatting" / "Do it once" / "Build me the
    // automation") are behind `COMPOSER_INTENT_HIDDEN`, so the control that
    // opened them is absent. Asserted rather than dropped, in the idiom
    // `product-scope-hidden-surfaces.test.ts` uses: a hidden surface coming
    // back by accident is the failure, and it looks like a feature.
    expect(container.querySelector('[aria-label="What this message is for"]')).toBeNull();
    for (const label of ["Mention someone", "Formatting"]) {
      expect(container.querySelector(`[aria-label="${label}"]`)).not.toBeNull();
    }
    expect(container.textContent).toContain("to send");
    expect(container.textContent).not.toContain(READ_ONLY_NOTICE);
  });

  it("still offers the empty-state cards", async () => {
    await mount(WRITABLE);

    expect(container.textContent).toContain("Give the team a brief");
    expect(container.textContent).toContain("Add people");
  });
});

describe("the harness-unavailable notice sits next to the composer, or without one", () => {
  it("renders the notice on a writable channel, saying all three things", async () => {
    await mount(WRITABLE, "unavailable");

    const strip = banner();
    expect(strip).not.toBeNull();
    expect(strip?.textContent).toContain(
      "This host cannot reach a model — no agent harness is available.",
    );
    // The sentence lost its directional word when the strip moved (see the
    // render site) and kept everything else: not the teammate they appear
    // under, from the offline echo brain, and no setting changes it.
    expect(strip?.textContent).toContain(
      "The replies in this conversation come from the offline echo brain rather than the " +
        "agent they appear under. No setting changes that: it takes a host built and " +
        "started with the harness.",
    );
  });

  it("shares the composer's own box, so nothing can come between them", async () => {
    await mount(WRITABLE, "unavailable");

    const strip = banner()!;
    const input = composerInput()!;
    expect(strip).not.toBeNull();
    expect(input).not.toBeNull();

    // This used to be an order assertion over the pane's flex column: the
    // notice was a full-bleed strip in the flow, and the claim was that it sat
    // after the transcript and before the composer. It kept needing more cases
    // — the typing line, then the in-flight run bar — because every new row in
    // that column was a new thing that could land between them.
    //
    // The notice hovers now: it and the composer are in one `relative` box, and
    // it anchors to that box with `absolute bottom-full`. So the adjacency is
    // structural rather than ordered, and the run bar can render between them
    // in the DOM without coming between them on screen.
    const box = strip.parentElement!;
    expect(box.className).toContain("relative");
    expect(box.contains(input), "the notice and the composer share one box").toBe(true);
    expect(strip.className).toContain("absolute");
    expect(strip.className).toContain("bottom-full");
  });

  it("overlaps the transcript rather than displacing it", async () => {
    await mount(WRITABLE, "unavailable");

    const strip = banner()!;
    // The trade the float makes, stated: it covers the last line of the
    // transcript instead of pushing it up. The transcript can be scrolled and
    // this cannot be missed, which is the right way round — but it is only
    // acceptable because the box takes no pointer events, so a click meant for
    // the message underneath still lands. The one thing here that IS clickable
    // puts them back on itself.
    expect(strip.className).toContain("pointer-events-none");
    const link = strip.querySelector("a");
    if (link) expect(strip.className).toContain("[&_a]:pointer-events-auto");
  });

  it("still hovers over the composer with a run in flight", async () => {
    // `InflightRunBar` renders inside the same box, between the notice's anchor
    // and the composer. That used to break the adjacency assertion; now it
    // cannot, and this is the case that proves it.
    await mount(WRITABLE, "unavailable", ["Jane"], true);

    const strip = banner()!;
    const input = composerInput()!;
    const bar = container.querySelector('[data-testid="inflight-run-bar"]');
    expect(bar).not.toBeNull();

    const box = strip.parentElement!;
    expect(box.contains(input)).toBe(true);
    expect(box.contains(bar!)).toBe(true);
    expect(strip.className).toContain("bottom-full");
  });

  /**
   * The read-only archive keeps it: the notice is about the replies already on
   * screen, not about sending.
   */
  it("stays on the read-only archive, which takes no replies", async () => {
    await mount(ARCHIVE, "unavailable");

    const strip = banner();
    expect(strip).not.toBeNull();
    expect(strip?.textContent).toContain(
      "This host cannot reach a model — no agent harness is available.",
    );
    expect(strip?.textContent).toContain(
      "The replies in this conversation come from the offline echo brain rather than the " +
        "agent they appear under. No setting changes that: it takes a host built and " +
        "started with the harness.",
    );

    // Restoring the notice restores nothing else: the channel is still
    // read-only, still says so, and still draws no composer.
    expect(container.textContent).toContain(READ_ONLY_NOTICE);
    expect(composerInput()).toBeNull();
    expect(readOnlyComposerInput()).toBeNull();
    expect(container.querySelector("textarea")).toBeNull();
  });

  it("sits after the read-only notice, with no composer between them", async () => {
    await mount(ARCHIVE, "unavailable");

    const strip = banner()!;
    // One level deeper than it used to be: the notice's parent is now the
    // `relative` box the banner anchors to, so the read-only notice is a
    // sibling of that box rather than of the banner itself.
    const box = strip.parentElement!;
    const column = box.parentElement!;
    const kids = Array.from(column.children);
    const notice = kids.find((el) => el.textContent?.includes(READ_ONLY_NOTICE));

    expect(notice).not.toBeUndefined();
    expect(kids.indexOf(notice!)).toBeLessThan(kids.indexOf(box));

    // Order relative to the read-only notice only — deliberately NOT "and it is
    // the last child of the column". `InflightRunBar` renders after this strip
    // in production, outside the read-only branch on purpose (see its comment
    // at the render site), and this harness passes no `inflightRuns`, so a
    // last-child assertion would pass here while being false on screen.
  });
});

/**
 * The draft an operator has half-written outlives a look at a read-only channel.
 *
 * This is the regression the read-only change nearly shipped (codex review on
 * PR #1984). `MessageComposer` holds the draft, the staged attachment, the
 * mentions and the intent in its own `useState`, and `RoomView` renders one
 * instance for every channel — so React reconciling it in place is the only
 * reason a draft has ever survived walking to another channel and back.
 * Gating the element on `!readOnly` unmounted it, and the operator came back
 * to an empty box. The fix keeps the element and renders nothing from it.
 */
describe("a trip to the read-only archive does not eat the draft", () => {
  /** Type into a controlled textarea the way a keystroke would. */
  function type(el: HTMLTextAreaElement, text: string) {
    const setter = Object.getOwnPropertyDescriptor(
      HTMLTextAreaElement.prototype,
      "value",
    )!.set!;
    act(() => {
      setter.call(el, text);
      el.dispatchEvent(new Event("input", { bubbles: true }));
    });
  }

  it("comes back with the text still in it", async () => {
    const client = stubClient(null);
    await renderAt(client, WRITABLE);

    const before = composerInput() as HTMLTextAreaElement;
    expect(before).not.toBeNull();
    type(before, "half-written thought");
    expect((composerInput() as HTMLTextAreaElement).value).toBe("half-written thought");

    await renderAt(client, ARCHIVE);
    // Still nothing on screen: the point is that it is unrendered, not that it
    // came back.
    expect(composerInput()).toBeNull();
    expect(container.querySelector("textarea")).toBeNull();

    await renderAt(client, WRITABLE);
    expect((composerInput() as HTMLTextAreaElement).value).toBe("half-written thought");
  });
});

/**
 * A General *spelling* opens whatever holds the legacy line: a blueprint desk
 * that claims it, else the read-only archive — never issue #370's "isn't a
 * channel here" notice.
 */
describe("a General address resolves to whichever channel holds the line", () => {
  function claimedClient(): OpenCompanyClient {
    return {
      // Two desks, and the one that claims the line is NOT the first — so
      // "resolved to the company line" and "fell through to the first channel"
      // are distinguishable answers. The claiming desk declares itself by NAME
      // while carrying its own id: the case `deskClaimsGeneralChannel` exists
      // for, and the one where neither `main` nor `general` names a channel
      // directly.
      listDesks: vi.fn(async () => [
        { id: "eng", name: "Engineering", description: "Ships it", members: [] as string[] },
        { id: "ops", name: "General", description: "The company line", members: [] as string[] },
      ]),
      listTeam: vi.fn(async () => []),
      mentionables: vi.fn(async () => []),
      capabilityStatus: vi.fn(async () => ({ cognition: null })),
      chat: vi.fn(),
      reactToMessage: vi.fn(),
      getBudgetPause: vi.fn(async () => null),
    } as unknown as OpenCompanyClient;
  }

  it("opens the desk that claimed the line, with no unknown-channel notice", async () => {
    await renderAt(claimedClient(), "main");

    // The claiming desk, not the first channel in the rail — which is what a
    // bare first-channel fallback would have landed on.
    expect(composerInput()?.getAttribute("aria-label")).toBe("Message #general");
    expect(container.textContent).not.toContain("isn't a channel here");
    expect(container.textContent).not.toContain(READ_ONLY_NOTICE);
  });

  it("opens the read-only archive in an ordinary company", async () => {
    await mount(ARCHIVE);

    expect(composerInput()).toBeNull();
    expect(container.textContent).toContain(READ_ONLY_NOTICE);
    expect(container.textContent).not.toContain("isn't a channel here");
  });
});

/**
 * The guided tour's composer stops land somewhere that has a composer.
 *
 * Two stops spotlight `[data-tour="chat-composer"]`, and one of them is the
 * closing "You're all set". They name no channel, so they open whatever a bare
 * `#/chat` restores. A missing anchor is skipped in silence, so both halves
 * are pinned: a bare entry renders a composer, and it does not restore onto
 * the read-only archive.
 */
describe("the tour's composer stops open a writable channel", () => {
  const composerStops = TOUR.filter((s) => s.target === '[data-tour="chat-composer"]');

  it("finds the two stops that spotlight the composer", () => {
    expect(composerStops.length).toBe(2);
    expect(composerStops.map((s) => s.title)).toEqual(["Talk to your company", "You're all set"]);
  });

  it("names no channel, so a bare #/chat decides", () => {
    for (const stop of composerStops) {
      expect(stop.view).toBe("chat");
      expect(stop.sub).toBeUndefined();
    }
  });

  it("mounts the spotlight anchor on a bare #/chat", async () => {
    await mount("");

    expect(container.querySelector('[data-tour="chat-composer"]')).not.toBeNull();
    expect(onNavigate).toHaveBeenCalledWith(WRITABLE);
  });

  it("does not restore a bare #/chat onto a remembered archive", async () => {
    writeLastChannel({ connection: "local", company: "acme" }, ARCHIVE);
    await mount("");

    expect(onNavigate).toHaveBeenCalledWith(WRITABLE);
    expect(onNavigate).not.toHaveBeenCalledWith(ARCHIVE);
  });

  it("still restores a remembered writable channel", async () => {
    writeLastChannel({ connection: "local", company: "acme" }, "dm:ada");
    await mount("");

    expect(onNavigate).toHaveBeenCalledWith("dm:ada");
  });

  it("mounts no anchor on the archive", async () => {
    await mount(ARCHIVE);

    expect(container.querySelector('[data-tour="chat-composer"]')).toBeNull();
  });
});
