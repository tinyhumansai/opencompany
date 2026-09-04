// @vitest-environment jsdom

import { act, createElement, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { isGeneralChannel } from "@/lib/chat";
import { TOUR } from "@/tour/steps";
import { ChatView } from "@/views/ChatView";

/**
 * The channel composer answers a read-only channel by not existing, and the
 * echo-brain notice sits next to the control it qualifies.
 *
 * # Why a render test and not a source scan
 *
 * Both facts are about what is *on screen*, and both were previously "true"
 * in a form that read as fixed and was not. The composer was `disabled`, which
 * is still a claim that the action exists: under a notice reading "There is
 * nothing to reply to here", `#Operator` drew a text input, three intent
 * chips, a mention button, a paperclip, a formatting toggle, a Send button and
 * an "Enter to send" hint. And the notice explaining that replies come from
 * the offline echo brain sat above the transcript, at the far end of the page
 * from the Send that provokes one.
 *
 * A grep cannot tell a rendered control from a removed one, so this mounts the
 * real `ChatView` against a stub client and asks the DOM.
 *
 * # The writable half is not optional
 *
 * Every read-only assertion here is an assertion of absence, and absence is
 * also what a `ChatView` that failed to mount produces. The writable cases
 * pin the same queries finding everything, off the same fixture — so a
 * mount that silently renders nothing fails rather than passing twice.
 */

const OPERATOR_DTO = {
  id: "operator",
  name: "Operator",
  description: "Workflow reports and notifications",
};

const DESK_DTO = {
  id: "main",
  name: "main",
  description: "The main channel",
  members: [] as string[],
};

function stubClient(cognition: string | null): OpenCompanyClient {
  return {
    listDesks: vi.fn(async () => [DESK_DTO]),
    listTeam: vi.fn(async () => []),
    mentionables: vi.fn(async () => []),
    getOperatorChannel: vi.fn(async () => OPERATOR_DTO),
    capabilityStatus: vi.fn(async () => ({ cognition })),
    chat: vi.fn(),
    reactToMessage: vi.fn(),
    getBudgetPause: vi.fn(async () => null),
  } as unknown as OpenCompanyClient;
}

let container: HTMLDivElement;
let root: Root;

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
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

function tree(client: OpenCompanyClient, sub: string, typing: string[] = []): ReactNode {
  const view = createElement(ChatView, {
    client,
    company: "acme",
    sub,
    onNavigate: vi.fn(),
    transcripts: {},
    setTranscripts: vi.fn(),
    // Who the shell says is at a keyboard in this channel. Empty by default;
    // the sibling-order test below supplies a name, because `TypingLine`
    // renders nothing at all when nobody is typing and the banner's placement
    // was only ever wrong when it renders something.
    resolveTypingNames: () => typing,
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
 * between channels: React reconciles `ChatView` in place, which is exactly the
 * production path an operator takes when they click another channel in the
 * rail. Remounting instead would discard the composer's state for reasons that
 * have nothing to do with the behaviour under test, and the test would pass
 * against any implementation.
 */
async function renderAt(client: OpenCompanyClient, sub: string, typing: string[] = []) {
  await act(async () => {
    root.render(tree(client, sub, typing));
  });
  // Let the desks / operator / capability reads settle.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

async function mount(sub: string, cognition: string | null = null, typing: string[] = []) {
  const client = stubClient(cognition);
  await renderAt(client, sub, typing);
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
    await mount("operator");

    expect(composerInput()).toBeNull();
    expect(readOnlyComposerInput()).toBeNull();
    expect(container.querySelector("textarea")).toBeNull();
  });

  it("draws no Send button and no intent chips", async () => {
    await mount("operator");

    expect(container.querySelector('[aria-label="Send"]')).toBeNull();
    expect(container.querySelector('[aria-label="What this message is for"]')).toBeNull();
    for (const chip of ["Just chatting", "Do it once", "Build me the workflow"]) {
      expect(container.textContent).not.toContain(chip);
    }
  });

  it("draws none of the mention, attach or formatting controls", async () => {
    await mount("operator");

    for (const label of ["Mention someone", "Attach a file", "Formatting"]) {
      expect(container.querySelector(`[aria-label="${label}"]`)).toBeNull();
    }
  });

  it("drops the keyboard hint, which describes a send that cannot happen", async () => {
    await mount("operator");

    expect(container.textContent).not.toContain("to send");
    expect(container.textContent).not.toContain("for a new line");
  });

  it("keeps the notice that explains why", async () => {
    await mount("operator");

    expect(container.textContent).toContain("There is nothing to reply to here");
  });

  it("offers neither empty-state card, since neither action exists here", async () => {
    await mount("operator");

    // "Give the team a brief" prefills a composer this channel does not
    // render; "Add people" opens a members pane `ChatView` gates off on the
    // same flag. Both were dead controls under the notice.
    expect(container.textContent).not.toContain("Give the team a brief");
    expect(container.textContent).not.toContain("Add people");
  });
});

describe("a writable channel still renders the whole composer", () => {
  it("draws the input, the Send button, the chips and the controls", async () => {
    await mount("main");

    expect(composerInput()).not.toBeNull();
    expect(container.querySelector('[aria-label="Send"]')).not.toBeNull();
    expect(container.querySelector('[aria-label="What this message is for"]')).not.toBeNull();
    for (const label of ["Mention someone", "Formatting"]) {
      expect(container.querySelector(`[aria-label="${label}"]`)).not.toBeNull();
    }
    expect(container.textContent).toContain("to send");
    expect(container.textContent).not.toContain("There is nothing to reply to here");
  });

  it("still offers the empty-state cards", async () => {
    await mount("main");

    expect(container.textContent).toContain("Give the team a brief");
    expect(container.textContent).toContain("Add people");
  });
});

describe("the harness-unavailable notice sits next to the composer", () => {
  it("renders the notice on a writable channel, saying all three things", async () => {
    await mount("main", "unavailable");

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
        "teammate they appear under. No setting changes that: it takes a host built and " +
        "started with the harness.",
    );
  });

  it("places it below the transcript and above the composer", async () => {
    await mount("main", "unavailable");

    const strip = banner()!;
    const input = composerInput()!;
    expect(strip).not.toBeNull();
    expect(input).not.toBeNull();

    // `MessageTimeline`'s root is the scrolling viewport. The notice, the
    // scroller and the composer are all direct children of the same flex
    // column, so their order in that column is the order on screen — which is
    // the entire claim being made: the notice qualifies the Send below it, not
    // the transcript above it.
    const column = strip.parentElement!;
    const kids = Array.from(column.children);
    const scroller = column.querySelector(":scope > div.overflow-y-auto")!;
    const composerRoot = kids.find((el) => el.contains(input))!;

    expect(scroller).not.toBeNull();
    expect(composerRoot).not.toBeUndefined();
    expect(kids.indexOf(scroller)).toBeLessThan(kids.indexOf(strip));
    expect(kids.indexOf(strip)).toBeLessThan(kids.indexOf(composerRoot));
  });

  it("stays directly above the composer with somebody typing", async () => {
    // The order was asserted with nobody typing, which is the one case where
    // `TypingLine` renders nothing — so `["TRANSCRIPT", "BANNER", "COMPOSER"]`
    // read correct while the shipped order was TRANSCRIPT, BANNER, TYPING,
    // COMPOSER for anyone mid-conversation (CodeRabbit review on PR #1984).
    // Proximity to the composer is the entire reason the strip moved, so the
    // case with a row competing for that gap is the case worth pinning.
    await mount("main", "unavailable", ["Jane"]);

    const strip = banner()!;
    const input = composerInput()!;
    const typing = container.querySelector('[data-testid="typing-line"]');
    expect(strip).not.toBeNull();
    expect(input).not.toBeNull();
    expect(typing).not.toBeNull();

    const column = strip.parentElement!;
    const kids = Array.from(column.children);
    const composerRoot = kids.find((el) => el.contains(input))!;

    // Adjacency, not just order: nothing at all between the notice and the
    // control it qualifies.
    expect(kids.indexOf(typing!)).toBeLessThan(kids.indexOf(strip));
    expect(kids.indexOf(composerRoot)).toBe(kids.indexOf(strip) + 1);
  });

  it("still states the false attribution on a read-only feed", async () => {
    // It was suppressed here for one commit, on the reasoning that a caveat
    // about sending has nothing to qualify where nothing can be sent. The
    // sentence is not about sending: `#Operator` renders company-authored
    // reports under a teammate's name, `MessageRow` marks each of them from
    // this same state, and with the strip gone the only explanation left was
    // `EchoPlaceholder`'s `title` — invisible to touch and keyboard (codex
    // review on PR #1984). A feed the reader cannot reply to is the last place
    // to drop it.
    await mount("operator", "unavailable");

    const strip = banner();
    expect(strip).not.toBeNull();
    expect(strip?.textContent).toContain(
      "come from the offline echo brain rather than the teammate they appear under",
    );
    // Still no composer, and the read-only notice still explains the channel.
    expect(composerInput()).toBeNull();
    expect(container.textContent).toContain("There is nothing to reply to here");
  });

  it("sits under the read-only notice, as the bottom strip of the pane", async () => {
    await mount("operator", "unavailable");

    const strip = banner()!;
    const column = strip.parentElement!;
    const kids = Array.from(column.children);
    const notice = kids.find((el) => el.textContent?.includes("There is nothing to reply to here"))!;

    expect(notice).not.toBeUndefined();
    expect(kids.indexOf(notice)).toBeLessThan(kids.indexOf(strip));
    // Nothing after it: the composer that would normally follow is not here.
    expect(kids.indexOf(strip)).toBe(kids.length - 1);
  });
});

/**
 * The draft an operator has half-written outlives a look at `#Operator`.
 *
 * This is the regression the read-only change nearly shipped (codex review on
 * PR #1984). `MessageComposer` holds the draft, the staged attachment, the
 * mentions and the intent in its own `useState`, and `ChatView` renders one
 * instance for every channel — so React reconciling it in place is the only
 * reason a draft has ever survived walking to another channel and back.
 * Gating the element on `!readOnly` unmounted it, and the operator came back
 * to an empty box. The fix keeps the element and renders nothing from it.
 */
describe("a trip to the read-only feed does not eat the draft", () => {
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
    await renderAt(client, "main");

    const before = composerInput() as HTMLTextAreaElement;
    expect(before).not.toBeNull();
    type(before, "half-written thought");
    expect((composerInput() as HTMLTextAreaElement).value).toBe("half-written thought");

    await renderAt(client, "operator");
    // Still nothing on screen: the point is that it is unrendered, not that it
    // came back.
    expect(composerInput()).toBeNull();
    expect(container.querySelector("textarea")).toBeNull();

    await renderAt(client, "main");
    expect((composerInput() as HTMLTextAreaElement).value).toBe("half-written thought");
  });
});

/**
 * A General *spelling* opens the company-wide line whichever way the company
 * declared it.
 *
 * The guided tour's two composer stops address `#/chat/main` outright so they
 * cannot inherit the read-only Operator feed (`tour/steps.ts`). That address
 * has to resolve in a grandfathered company too — one whose blueprint put a
 * desk on the company line, where `buildChannels` adds no built-in `#general`
 * beside it — or the tour would spotlight a composer under issue #370's
 * "isn't a channel here" notice.
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
      getOperatorChannel: vi.fn(async () => OPERATOR_DTO),
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
    expect(container.textContent).not.toContain("There is nothing to reply to here");
  });

  it("still opens the built-in channel in an ordinary company", async () => {
    await mount("main");

    expect(composerInput()).not.toBeNull();
    expect(container.textContent).not.toContain("isn't a channel here");
  });
});

/**
 * The guided tour's composer stops land somewhere that has a composer.
 *
 * Two of the eight stops spotlight `[data-tour="chat-composer"]`, and one of
 * them is the closing "You're all set". A stop that names only `view: "chat"`
 * inherits whichever channel was last open there — `app-shell`'s remembered
 * sub-segment, or `ChatView`'s remembered channel on a cold start — which can
 * be the read-only Operator feed. Since PR #1984 that feed renders no composer,
 * so the anchor never mounts, `waitForTarget` times out, and the stop is
 * **skipped in silence**: a missing anchor degrades rather than errors, so the
 * tour teaches less and nothing reports it. Neither half of that is visible
 * from a passing suite, which is why both halves are pinned here.
 */
describe("the tour's composer stops address a writable channel", () => {
  const composerStops = TOUR.filter((s) => s.target === '[data-tour="chat-composer"]');

  it("finds the two stops that spotlight the composer", () => {
    expect(composerStops.length).toBe(2);
    expect(composerStops.map((s) => s.title)).toEqual(["Talk to your company", "You're all set"]);
  });

  it("names a channel outright rather than inheriting the last one", () => {
    for (const stop of composerStops) {
      expect(stop.view).toBe("chat");
      expect(stop.sub).toBeTruthy();
      // A General spelling: the company-wide line exists in every company and
      // is writable in all of them, and `ChatView` folds every spelling of it
      // onto whichever channel actually holds the line.
      expect(isGeneralChannel(stop.sub!)).toBe(true);
    }
  });

  it("mounts the spotlight anchor at that address, with an Operator feed present", async () => {
    for (const stop of composerStops) {
      await renderAt(stubClient(null), stop.sub!);
      expect(container.querySelector('[data-tour="chat-composer"]')).not.toBeNull();
    }
  });

  it("mounts no anchor on the feed those stops must not inherit", async () => {
    // The other half of the claim: the address matters because the remembered
    // one would have failed. Without this the test above passes on a console
    // where every channel renders a composer.
    await mount("operator");

    expect(container.querySelector('[data-tour="chat-composer"]')).toBeNull();
  });
});
