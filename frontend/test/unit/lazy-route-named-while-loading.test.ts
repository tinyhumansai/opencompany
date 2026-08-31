// @vitest-environment jsdom

import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { act, createElement, lazy, Suspense } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { financeFallbackTitle } from "@/components/app-shell";
import { RouteLoading } from "@/components/route-loading";
import { FINANCE_PAGES } from "@/views/finance/FinanceSection";

/**
 * A code-split route is named while its chunk is still in flight (codex review
 * on #1785).
 *
 * # The hole this closes
 *
 * The other two heading guards read the *leaf* files — the component a route
 * eventually renders. That is the wrong file for a cold chunk: for as long as
 * the network takes, the leaf is not mounted and what is on screen is the
 * `<Suspense fallback>`, which lives in `app-shell.tsx`,
 * `views/SettingsSection.tsx`, `views/finance/FinanceSection.tsx` or `App.tsx`.
 * Every one of those was a bare centred `Loading …` line with no `h1`, so
 * `#/workspace`, `#/observatory`, `#/workflows`, `#/pages`, `#/finances`,
 * `#/finances/overview`, `#/settings/usage` and `#/styleguide` each had a state
 * a screen reader could not announce, and both new guards passed anyway —
 * mapping only the eventual leaf makes them true of a component that is not on
 * screen yet.
 *
 * # Why the scan is over *every* Suspense, with an allowlist
 *
 * The tempting version derives "route-level" from the suspended component's
 * name and skips anything it does not recognise. That fails silently in the
 * direction that matters: the next code-split route is unrecognised, so it is
 * skipped, so it is unguarded — which is the exact shape of the defect being
 * fixed. Scanning every boundary and requiring a documented row instead makes a
 * new boundary fail closed, and the row has to say why a state has no name.
 *
 * `WIDGET_SUSPENSE` therefore holds only boundaries that are *not* the page:
 * a panel inside an already-named page, and an overlay drawn over one.
 */

const SRC = resolve(dirname(fileURLToPath(import.meta.url)), "../../src");

/**
 * Suspense boundaries that are not a page, keyed `file:SuspendedComponent`.
 *
 * A row is a claim that something else on screen already carries the `h1` while
 * this boundary shows its fallback. That is checkable by eye and is checked
 * below by `every row still names a boundary that exists`; it is not checkable
 * by this scan, which is why each row says where the name comes from.
 */
const WIDGET_SUSPENSE: Record<string, string> = {
  "views/Overview.tsx:KnowledgeGraph": [
    "One panel of the company overview, not the page. `Overview.tsx` renders",
    "`<PageHeader hidden title=\"Company overview\" />` above this boundary and",
    "outside it, so the page keeps its name for the whole time the graph's",
    "physics chunk is in flight.",
  ].join(" "),
  "tour/TourController.tsx:Joyride": [
    "The product tour is an overlay drawn over whatever page is mounted, and",
    "its fallback paints nothing. The page underneath is unchanged and keeps",
    "its own `h1`; a heading here would be a second one on the page.",
  ].join(" "),
};

/** Every `.tsx` under `src`, as paths relative to it. */
function sources(dir = SRC, prefix = ""): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) return sources(join(dir, entry.name), rel);
    return entry.name.endsWith(".tsx") ? [rel] : [];
  });
}

type Boundary = {
  /** `file:SuspendedComponent`, the key `WIDGET_SUSPENSE` is written in. */
  key: string;
  file: string;
  line: number;
  fallback: string;
};

/**
 * The `fallback={…}` expression, read by balancing braces from the `{` after
 * `fallback=`.
 *
 * A regex cannot do this: every real fallback in this repo contains nested
 * braces, and the lazy one (`[^}]*`) stops at the first `}` of a `className`
 * interpolation and reports an empty fallback for a boundary that has one.
 *
 * The *attribute* is matched with a regex, though, and with optional whitespace
 * around the `=`. A literal `"fallback="` search missed
 * `<Suspense fallback = {…}>` — which is valid JSX and which prettier has no
 * reason to touch inside a multi-line prop list — and a missed boundary is not
 * a missed assertion here: `boundaries()` simply would not yield it, so it
 * would skip every heading check silently rather than fail one.
 */
const FALLBACK_ATTR = /fallback\s*=\s*/g;

function fallbackAt(source: string, from: number): { text: string; end: number } | null {
  FALLBACK_ATTR.lastIndex = from;
  const match = FALLBACK_ATTR.exec(source);
  if (!match) return null;
  const at = match.index + match[0].length;
  const open = source.indexOf("{", at);
  if (open < 0) return null;
  let depth = 0;
  for (let i = open; i < source.length; i++) {
    if (source[i] === "{") depth += 1;
    else if (source[i] === "}") {
      depth -= 1;
      if (depth === 0) return { text: source.slice(open + 1, i), end: i };
    }
  }
  return null;
}

/**
 * The first component rendered *inside* the boundary — what is suspending.
 *
 * Read from the end of the `fallback` expression rather than from the tag,
 * because the fallback is JSX too: a scan that took the first `<Capitalized`
 * after `<Suspense` would name `RouteLoading` for every boundary and collapse
 * eight distinct keys into one.
 */
function suspendedName(source: string, fallbackEnd: number): string {
  const close = source.indexOf(">", fallbackEnd);
  const child = source.slice(close, close + 400).match(/<([A-Z][A-Za-z0-9_]*)/);
  return child?.[1] ?? "?";
}

function boundaries(): Boundary[] {
  return sources().flatMap((file) => {
    const source = readFileSync(join(SRC, file), "utf8");
    const found: Boundary[] = [];
    for (const match of source.matchAll(/<Suspense[\s>]/g)) {
      const at = match.index;
      const fallback = fallbackAt(source, at);
      if (fallback === null) continue;
      found.push({
        key: `${file}:${suspendedName(source, fallback.end)}`,
        file,
        line: source.slice(0, at).split("\n").length,
        fallback: fallback.text,
      });
    }
    return found;
  });
}

/** Whether a fallback puts a page heading on screen. */
function names(fallback: string): boolean {
  return (
    fallback.includes("<RouteLoading") ||
    fallback.includes("<PageHeader") ||
    fallback.includes("<h1")
  );
}

const BOUNDARIES = boundaries();

describe("a lazy route is named while its chunk loads (#1785)", () => {
  it("finds the Suspense boundaries at all, so a broken scan cannot pass silently", () => {
    // Eight route boundaries plus the two widget rows. A parser that stopped
    // finding fallbacks would report zero offenders and read as green, which is
    // the failure mode every guard in this directory is written against.
    expect(BOUNDARIES.length).toBeGreaterThanOrEqual(10);
    expect(BOUNDARIES.some((b) => b.file === "components/app-shell.tsx")).toBe(true);
    // And the brace balancer really read an expression, not an empty string.
    expect(BOUNDARIES.every((b) => b.fallback.trim().length > 0)).toBe(true);
    // Every key names a real component. A `?` would be a boundary nobody could
    // write an exemption row for, since the row is keyed on that name.
    expect(BOUNDARIES.filter((b) => b.key.endsWith(":?")).map((b) => b.file)).toEqual([]);
  });

  it("has no route-level fallback rendering without a page heading", () => {
    const offenders = BOUNDARIES.filter((b) => !(b.key in WIDGET_SUSPENSE))
      .filter((b) => !names(b.fallback))
      .map((b) => `${b.file}:${b.line} suspends <${b.key.split(":")[1]}> behind a fallback with no h1`);

    expect(
      offenders,
      `A route's lazy chunk paints a state with no page heading, so that route ` +
        `has no h1 for as long as the chunk is in flight and a screen reader ` +
        `cannot announce it.\n` +
        `Use <RouteLoading title="…" label="…" /> — src/components/route-loading.tsx. ` +
        `If the boundary is a widget inside a page that is already named, add a ` +
        `row to WIDGET_SUSPENSE saying where the name comes from.\n` +
        offenders.join("\n"),
    ).toEqual([]);
  });

  it("has every widget row still naming a boundary that exists", () => {
    // A row whose boundary was deleted or renamed is an exemption nobody is
    // being held to, and it would silently cover the next thing to take that
    // name.
    const stale = Object.keys(WIDGET_SUSPENSE)
      .filter((key) => !BOUNDARIES.some((b) => b.key === key))
      .map((key) => `${key} is exempt but no such Suspense boundary exists — drop the row`);

    expect(stale, stale.join("\n")).toEqual([]);
  });

  it("has every code-split route named by RouteLoading rather than one-off markup", () => {
    // The eight fallbacks were eight copies of the same centred line before
    // this PR, which is how the console got twelve heading styles in the first
    // place. One component means one decision about how a loading route looks.
    const routes = BOUNDARIES.filter((b) => !(b.key in WIDGET_SUSPENSE));
    expect(routes.length).toBeGreaterThanOrEqual(8);
    const oneOff = routes
      .filter((b) => !b.fallback.includes("<RouteLoading"))
      .map((b) => `${b.file}:${b.line} rolls its own loading state`);
    expect(oneOff, oneOff.join("\n")).toEqual([]);
  });
});

/**
 * The scan above reads text. This renders a real suspended boundary and asks
 * the DOM, for the same reason `settings-page-named-in-every-state.test.ts`
 * does: a fallback that *mentions* `RouteLoading` is not evidence that React
 * put an `h1` on screen while the child was suspended.
 */
describe("RouteLoading names the page it is standing in for", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as unknown as { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT =
      true;
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  it("puts exactly one h1 on screen while the chunk never arrives", async () => {
    // A chunk that is permanently in flight is the state being guarded: the
    // leaf never mounts, so the only thing that can carry the name is this.
    const Never = lazy(() => new Promise<never>(() => {}));

    await act(async () => {
      root.render(
        createElement(
          Suspense,
          {
            fallback: createElement(RouteLoading, {
              title: "Workspace",
              label: "Loading workspace…",
            }),
          },
          createElement(Never),
        ),
      );
    });

    const headings = container.querySelectorAll("h1");
    expect(headings.length, "a page has exactly one h1").toBe(1);
    expect(headings[0].textContent).toBe("Workspace");
    // The accessible name is not a substitute for the visible one, and the
    // visible line is what the operator sees.
    expect(container.textContent).toContain("Loading workspace…");
  });

  it("keeps the heading out of the flow, so the loading line stays centred", () => {
    // `sr-only` is `position: absolute`, which takes the h1 out of the flex
    // row. Without that, adding the name would have nudged every one of these
    // eight loading states off centre — a regression nobody would have filed
    // and nobody would have caught.
    act(() => {
      root.render(createElement(RouteLoading, { title: "Pages", label: "Loading pages…" }));
    });
    const h1 = container.querySelector("h1");
    expect(h1?.className).toContain("sr-only");
  });
});

/**
 * The scanner itself, held to the JSX it claims to read.
 *
 * `boundaries()` yielding nothing is indistinguishable from every boundary
 * passing — the suite goes green either way — so the one failure mode worth
 * asserting directly is the scanner missing a boundary it should have found.
 * A literal `"fallback="` search missed `<Suspense fallback = {…}>`, which is
 * valid JSX.
 */
describe("the fallback scanner", () => {
  it("finds the attribute with or without spaces around the `=`", () => {
    for (const spelling of [
      "fallback={<RouteLoading title=\"Wallet\" />}",
      "fallback = {<RouteLoading title=\"Wallet\" />}",
      "fallback ={<RouteLoading title=\"Wallet\" />}",
      "fallback= {<RouteLoading title=\"Wallet\" />}",
    ]) {
      const found = fallbackAt(`<Suspense ${spelling}>`, 0);
      expect(found, spelling).not.toBeNull();
      expect(found?.text, spelling).toContain("RouteLoading");
      expect(found?.text, spelling).toContain("Wallet");
    }
  });

  it("still balances braces rather than stopping at the first `}`", () => {
    const found = fallbackAt(
      '<Suspense fallback = {<RouteLoading className={`a ${b}`} title="Wallet" />}>',
      0,
    );
    expect(found?.text).toContain("Wallet");
  });

  it("answers null when there is no boundary at all", () => {
    expect(fallbackAt("<div>nothing here</div>", 0)).toBeNull();
  });
});

/**
 * The finance boundary names the subpage it is standing in for.
 *
 * On a cold direct visit to `#/finances/wallet` this boundary is the whole
 * page, so its `h1` is what a screen reader announces — and a single
 * "Finances" for every subpage announced the wrong page to someone who had
 * bookmarked one, correcting itself only once the chunk landed.
 *
 * The shell cannot import its titles from `FinanceSection`: a static import of
 * anything in that module pulls the chunk eagerly and there is no lazy boundary
 * left to name. So the map is spelled out there and reconciled here, where
 * importing both is free — a subpage added to `FINANCE_PAGES` without a title
 * fails this rather than quietly inheriting the section heading.
 */
describe("the finance fallback names every subpage", () => {
  it("matches each subpage's own label", () => {
    for (const page of FINANCE_PAGES) {
      if (page.id === "overview") continue;
      expect(financeFallbackTitle(page.id), page.id).toBe(page.label);
    }
  });

  it("falls back to the section for the overview and for no subpage at all", () => {
    expect(financeFallbackTitle(null)).toBe("Finances");
    expect(financeFallbackTitle("overview")).toBe("Finances");
    expect(financeFallbackTitle("not-a-page")).toBe("Finances");
  });
});

/**
 * The Observatory boundary names a run, for the same reason the finance one
 * names a subpage.
 *
 * `ObservatoryView` titles itself `runId ? "Run" : "Observatory"`, and a cold
 * direct visit to `#/observatory/<runId>` is exactly when the boundary *is* the
 * page — so the index heading was announced to someone who had bookmarked a
 * run, and only corrected itself once the chunk arrived.
 */
describe("the observatory fallback names a direct run", () => {
  it("follows the same rule the loaded view uses", () => {
    const shell = readFileSync(join(SRC, "components/app-shell.tsx"), "utf8");
    expect(shell).toContain('title={sub ? "Run" : "Observatory"}');
    expect(shell).not.toContain('<RouteLoading title="Observatory"');

    const view = readFileSync(join(SRC, "views/observatory/ObservatoryView.tsx"), "utf8");
    // The rule is copied, so it has to keep matching the view it stands in for.
    expect(view).toContain('title={runId ? "Run" : "Observatory"}');
  });
});
