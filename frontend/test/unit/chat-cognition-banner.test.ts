// @vitest-environment jsdom

import { Fragment, act, createElement, createRef, useLayoutEffect } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { OpenCompanyClient } from "@/api/client";
import type { CapabilityStatusDto, CognitionState } from "@/api/types";
import { ConnectionScopeProvider } from "@/connections/ConnectionContext";
import { ChatView } from "@/views/ChatView";

/**
 * Issues #1734 / #1735 — chat says so, before the first echo, when this company
 * has no model behind it.
 *
 * The bug is a silent degrade that reaches all the way to the transcript: with
 * no inference configured the runtime falls back to `EchoBrain`, whose replies
 * render under the teammate's own name, and no surface anywhere mentions it.
 * The remedy is one settings page away, so the banner has to *name* it — an
 * "unavailable" that does not say what to do sends the operator looking for a
 * new build.
 *
 * The two causes need different copy, which is why the host reports a
 * discriminated state rather than a boolean: only `unconfigured` is fixable in
 * the app. Both are pinned here, along with the two silences that must not
 * produce a banner at all.
 */

let container: HTMLDivElement;
let root: Root;

/**
 * A client that answers the capability read, and answers everything else with
 * an empty list.
 *
 * A `Proxy` rather than an enumerated stub on purpose. `ChatView` boots eight
 * unrelated reads — roster, viewer, people, desks, mentionables, history,
 * read-state, presence — and naming each one here would make this test a
 * standing record of that list, failing on the next read anyone adds for a
 * reason that has nothing to do with the banner. An empty answer is a state the
 * view already handles everywhere (it is what a company with no teammates and
 * no history looks like), so the fixture stays about the one read it is for.
 */
function clientWith(
  cognition: CognitionState | undefined | "reject" | "pending",
): OpenCompanyClient {
  const capabilityStatus = vi.fn(() => {
    if (cognition === "reject") {
      return Promise.reject(new Error("no capability surface on this host"));
    }
    // A read that never settles — the state a company the operator has only
    // just switched to is in, and the one the stale-scope test needs to hold
    // open rather than race.
    if (cognition === "pending") {
      return new Promise<CapabilityStatusDto>(() => {});
    }
    return Promise.resolve({ configured: false, cognition } as CapabilityStatusDto);
  });
  const named: Record<string, unknown> = {
    capabilityStatus,
    scopeFor: () => "/api/v1/company",
  };
  return new Proxy(named, {
    get: (target, prop: string) => target[prop] ?? (() => Promise.resolve([])),
  }) as unknown as OpenCompanyClient;
}

// `createElement` rather than JSX because the unit suite's vitest `include` is
// `*.test.ts` — a `.tsx` file is silently not collected, which reads as a
// passing suite.
async function render(cognition: CognitionState | undefined | "reject"): Promise<void> {
  const client = clientWith(cognition);
  const scopeRef = createRef<{
    connection: string;
    company: string | null;
    client: OpenCompanyClient;
  }>() as { current: { connection: string; company: string | null; client: OpenCompanyClient } };
  scopeRef.current = { connection: "c1", company: "acme", client };
  await act(async () => {
    root.render(
      createElement(ConnectionScopeProvider, {
        scope: { connection: "c1", company: "acme" },
        children: createElement(ChatView, {
          client,
          company: "acme",
          sub: "main",
          onNavigate: () => {},
          transcripts: {},
          setTranscripts: () => {},
          scopeRef,
        }),
      }),
    );
    // Two microtask drains: the capability read, then the state it sets.
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

function banner(): HTMLElement | null {
  return container.querySelector('[data-testid="chat-cognition-banner"]');
}

/**
 * jsdom ships no `matchMedia`, and `useIsDesktop` reaches for it unguarded — so
 * without this the whole view fails to mount and every assertion below would be
 * about a blank container. Same stub as `working-indicator.test.ts`.
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
});

describe("the chat cognition banner", () => {
  it("says what is wrong and where to fix it when no model is configured", async () => {
    await render("unconfigured");

    const notice = banner();
    expect(notice).not.toBeNull();
    // What is wrong, in the operator's terms rather than the runtime's.
    expect(notice!.textContent).toContain("Teammates can't think yet.");
    // Why the replies below are not what they look like.
    expect(notice!.textContent).toContain("offline echo brain");
    // And the remedy, as a link that actually goes there — the whole point of
    // the issue is that this is one settings page away and nothing said so.
    const link = notice!.querySelector("a");
    expect(link).not.toBeNull();
    expect(link!.getAttribute("href")).toBe("#/settings/inference");
    expect(link!.textContent).toContain("Settings → Inference");
  });

  it("names the host, not a setting, when no harness is available", async () => {
    await render("unavailable");

    const notice = banner();
    expect(notice).not.toBeNull();
    // Worded for the remedy rather than one of the two mechanisms behind it:
    // this state is reported both for a binary with no harness compiled in and
    // for one whose runtimes were never handed a pool, and naming only the
    // first would be false on the second (codex, PR #1740).
    expect(notice!.textContent).toContain(
      "This host cannot reach a model — no agent harness is available.",
    );
    // No settings link here: offering one would be the switch-that-does-nothing
    // this whole surface exists to stop.
    expect(notice!.querySelector("a")).toBeNull();
  });

  /**
   * The state with no remedy to name (codex, PR #1740).
   *
   * A harness is reachable, but the host could not *read* this company's
   * inference configuration, so it cannot say why the echo brain answered. An
   * unreadable config is no evidence that saving one would help — the same #266
   * doctrine that stops the workflow-run route answering `inference_required`
   * on this exact runtime — so the banner must offer no link, and must not
   * borrow the harness copy either, because a harness *is* attached.
   */
  it("names no remedy when the host cannot read its own inference config", async () => {
    await render("undetermined");

    const notice = banner();
    expect(notice).not.toBeNull();
    expect(notice!.textContent).toContain("this host can't say why");
    expect(notice!.textContent).toContain("could not be read");
    // No settings link: that is the promise the host declines to make.
    expect(notice!.querySelector("a")).toBeNull();
    // And not the harness story, which would be a plain falsehood here.
    expect(notice!.textContent).not.toContain("no agent harness is available");
  });

  /**
   * A provider is saved and resolves; the runtime just predates it (codex, PR
   * #1740). Telling this operator that no model is configured sends them back
   * to the page they came from to redo work they did correctly.
   */
  it("names the restart, not another provider choice, when one is already saved", async () => {
    await render("restart-required");

    const notice = banner();
    expect(notice).not.toBeNull();
    expect(notice!.textContent).toContain("the model isn't live");
    expect(notice!.textContent).toContain("A provider is configured");
    // Not the unconfigured story, which is the whole point.
    expect(notice!.textContent).not.toContain("has no model configured");
    // The link goes to the card that owns the restart — but the copy stops
    // short of promising a button, which is `canRebuildInPlace`'s to report.
    expect(notice!.querySelector("a")!.getAttribute("href")).toBe("#/settings/inference");
  });

  /**
   * Two cognition reads can be in flight at once — the mount's and a visibility
   * refresh's — and nothing guarantees they settle in the order they were
   * issued. A slow *older* read landing last would put back the state the newer
   * one had just corrected, and the scope stamp cannot catch it because both
   * carry the same scope (codex, PR #1740).
   *
   * Here the first read is held open, the tab is brought forward, the second
   * read answers `configured` and clears the banner — and only then does the
   * first answer `unconfigured`. The banner must stay down.
   */
  it("ignores a slow older read that lands after a newer one", async () => {
    const settles: Array<(dto: CapabilityStatusDto) => void> = [];
    const capabilityStatus = vi.fn(
      () => new Promise<CapabilityStatusDto>((resolve) => settles.push(resolve)),
    );
    const named: Record<string, unknown> = {
      capabilityStatus,
      scopeFor: () => "/api/v1/company",
    };
    const client = new Proxy(named, {
      get: (target, prop: string) => target[prop] ?? (() => Promise.resolve([])),
    }) as unknown as OpenCompanyClient;

    const scopeRef = createRef<{
      connection: string;
      company: string | null;
      client: OpenCompanyClient;
    }>() as { current: { connection: string; company: string | null; client: OpenCompanyClient } };
    scopeRef.current = { connection: "c1", company: "acme", client };
    await act(async () => {
      root.render(
        createElement(ConnectionScopeProvider, {
          scope: { connection: "c1", company: "acme" },
          children: createElement(ChatView, {
            client,
            company: "acme",
            sub: "main",
            onNavigate: () => {},
            transcripts: {},
            setTranscripts: () => {},
            scopeRef,
          }),
        }),
      );
      await Promise.resolve();
    });
    expect(settles).toHaveLength(1);

    // The tab comes forward; the refresh is issued while the first is still open.
    await act(async () => {
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "visible",
      });
      document.dispatchEvent(new Event("visibilitychange"));
      await Promise.resolve();
    });
    expect(settles).toHaveLength(2);

    // The newer read answers first: somebody configured a provider.
    await act(async () => {
      settles[1]({ configured: false, cognition: "configured" } as CapabilityStatusDto);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(banner()).toBeNull();

    // And now the stale one lands with the answer from before that change.
    await act(async () => {
      settles[0]({ configured: false, cognition: "unconfigured" } as CapabilityStatusDto);
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(banner(), "a stale read must not resurrect the banner").toBeNull();
  });

  it("stays down when the company has a model", async () => {
    await render("configured");

    expect(banner()).toBeNull();
  });

  it("stays down when the host does not report cognition", async () => {
    // An older host. Silence is not evidence of an echo, and a banner raised on
    // it would be the same unfounded claim in the other direction.
    await render(undefined);

    expect(banner()).toBeNull();
  });

  it("stays down when the capability read fails", async () => {
    await render("reject");

    expect(banner()).toBeNull();
  });

  /**
   * The answer can go stale under a console doing nothing at all: another admin,
   * or this operator in a second window, configures inference and rebuilds the
   * runtime while this chat sits open (codex, PR #1740). The operator's own trip
   * to Settings already re-reads — the shell mounts and unmounts `ChatView` per
   * route — but nothing covered the cross-session case, and a standing banner
   * insisting that a company which now thinks perfectly well cannot is the same
   * class of wrong claim this surface exists to remove.
   */
  it("re-reads cognition when the tab comes back to the foreground", async () => {
    let answer: CognitionState = "unconfigured";
    const capabilityStatus = vi.fn(() =>
      Promise.resolve({ configured: false, cognition: answer } as CapabilityStatusDto),
    );
    const named: Record<string, unknown> = {
      capabilityStatus,
      scopeFor: () => "/api/v1/company",
    };
    const client = new Proxy(named, {
      get: (target, prop: string) => target[prop] ?? (() => Promise.resolve([])),
    }) as unknown as OpenCompanyClient;

    const scopeRef = createRef<{
      connection: string;
      company: string | null;
      client: OpenCompanyClient;
    }>() as { current: { connection: string; company: string | null; client: OpenCompanyClient } };
    scopeRef.current = { connection: "c1", company: "acme", client };
    await act(async () => {
      root.render(
        createElement(ConnectionScopeProvider, {
          scope: { connection: "c1", company: "acme" },
          children: createElement(ChatView, {
            client,
            company: "acme",
            sub: "main",
            onNavigate: () => {},
            transcripts: {},
            setTranscripts: () => {},
            scopeRef,
          }),
        }),
      );
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(banner()).not.toBeNull();

    // Somebody else configures a provider. Nothing in this tab changed.
    answer = "configured";
    await act(async () => {
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "visible",
      });
      document.dispatchEvent(new Event("visibilitychange"));
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(capabilityStatus.mock.calls.length).toBeGreaterThan(1);
    expect(banner()).toBeNull();
  });

  /**
   * A company switch must not show the previous company's verdict, not even for
   * the frame before the new read lands (CodeRabbit review of PR #1740).
   *
   * `ChatView` stays mounted when `company` changes, and its capability read is
   * a passive effect — which runs *after* React has committed the DOM and the
   * browser has painted. Clearing the state inside that effect is therefore too
   * late by construction: the operator sees company A's "teammates can't think"
   * banner and its Placeholder chips over company B's transcript first. Binding
   * the value to the scope that produced it makes the stale answer unreadable
   * rather than merely short-lived.
   *
   * The assertion has to be taken at **commit** time for the same reason.
   * `act()` flushes passive effects before returning, so a post-`act` DOM query
   * would find the cleared state and pass either way — the classic vacuous
   * regression test. The probe below records what was actually committed, from
   * a layout effect, which React runs after every DOM mutation of a commit and
   * before any passive effect of it.
   */
  it("never shows the previous company's banner over the next company", async () => {
    const committed: boolean[] = [];
    function Probe() {
      useLayoutEffect(() => {
        committed.push(container.querySelector('[data-testid="chat-cognition-banner"]') !== null);
      });
      return null;
    }

    const clientA = clientWith("unconfigured");
    // Company B's read never settles, which is the whole window under test: the
    // console has been told nothing about B yet.
    const clientB = clientWith("pending");

    function show(client: OpenCompanyClient, company: string) {
      const scopeRef = createRef<{
        connection: string;
        company: string | null;
        client: OpenCompanyClient;
      }>() as { current: { connection: string; company: string | null; client: OpenCompanyClient } };
      scopeRef.current = { connection: "c1", company, client };
      root.render(
        createElement(ConnectionScopeProvider, {
          scope: { connection: "c1", company },
          children: createElement(Fragment, null, [
            createElement(ChatView, {
              key: "chat",
              client,
              company,
              sub: "main",
              onNavigate: () => {},
              transcripts: {},
              setTranscripts: () => {},
              scopeRef,
            }),
            createElement(Probe, { key: "probe" }),
          ]),
        }),
      );
    }

    await act(async () => {
      show(clientA, "acme");
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    });
    // The premise: A really is showing the banner. Without this the assertion
    // below would pass on a view that never renders one at all.
    expect(banner()).not.toBeNull();

    committed.length = 0;
    await act(async () => {
      show(clientB, "beta");
    });

    expect(committed.length).toBeGreaterThan(0);
    expect(committed).not.toContain(true);
    expect(banner()).toBeNull();
  });
});
