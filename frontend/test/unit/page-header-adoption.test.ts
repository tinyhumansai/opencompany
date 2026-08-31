import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

import { VIEWS as ROUTED_VIEWS, type View } from "@/lib/console-routes";
import { FINANCE_NAMED_BY, NAMED_BY, SETTINGS_NAMED_BY, type Leaf } from "./support/routed-views";

/**
 * Every page's title comes from `PageHeader`. A view that hand-rolls an `<h1>`
 * fails here (issue #1763).
 *
 * # Why this is a test rather than a convention
 *
 * The console reached twelve distinct heading styles without anyone deciding
 * to have twelve. Each one was reasonable where it was written — `text-xl`
 * because the page felt smaller, `text-lg font-medium` because it was a
 * sub-page, `text-sm` because it lived in a toolbar — and none of them is
 * visible as drift until you put four screens side by side, which no reviewer
 * of any single PR ever does.
 *
 * So this guards the *mechanism* rather than the values: a page cannot invent a
 * thirteenth style, because a page cannot write a heading at all. It is the
 * same argument `scripts/ci/assert-design-tokens.sh` makes about raw hex —
 * a grep is cheaper than the argument, and it does not get tired.
 *
 * # What it does not check
 *
 * Nothing about how `PageHeader` looks. Its type scale, its bar and its
 * hairline are decided in one file; if they are wrong they are wrong once,
 * which is the entire point of moving them there.
 *
 * `h2` and below are `page-section-heading-level.test.ts`'s business.
 */

const VIEWS = new URL("../../src/views", import.meta.url).pathname;

/**
 * Files allowed to open an `<h1>` of their own, and how many.
 *
 * The count is load-bearing. A bare allowlist would let `WorkflowsView` — which
 * legitimately keeps one — quietly grow a second, which is exactly the drift
 * this test exists to stop. Every entry below is a heading that names *the open
 * item* or lives *outside the console shell*, not a page title that could have
 * been a `PageHeader` and was not.
 *
 * Adding a row is a design decision, not a formality: say why the heading
 * cannot be a page header, in the same register as the rows already here.
 */
const HAND_ROLLED: Record<string, { count: number; why: string }> = {
  "Login.tsx": {
    count: 1,
    why:
      "Sign-in, outside the console shell. A hero heading centred in a `max-w-md` " +
      "column with no page around it — there is no bar for a bar-shaped header to sit in.",
  },
  "setup/SetupWizard.tsx": {
    count: 2,
    why:
      "The first-run flow, outside the console shell. These head a wizard *step* " +
      "and its completion screen, neither of which is a page an address reaches.",
  },
  "setup/AddHostPage.tsx": {
    count: 1,
    why: "Also the first-run flow, for the same reason.",
  },
  "WorkflowsView.tsx": {
    count: 1,
    why:
      "The workflow detail identity row (#1135/#1138), pinned by " +
      "`workflow-toolbar-layout.test.ts`: two rows, because identity-and-state and " +
      "act-on-it are different questions. It names the open workflow, not the page — " +
      "the page's own header is the index's, and that one is a `PageHeader`.",
  },
  "chat/ChatHeader.tsx": {
    count: 1,
    why:
      "The channel bar. It names the open channel and changes as you switch, and its " +
      "title sits inside a `group/title` whose hover reveals the copy control beside " +
      "it — an affordance that only works while the heading and the button share a " +
      "parent this file owns.",
  },
  "TaskDetailView.tsx": {
    count: 1,
    why:
      "The card's title inside the Work detail pane, above the compressed metadata " +
      "row #1347/#1348/#1349 cut 190px of preamble down to. A bar with a hairline " +
      "over it is the chrome those issues removed.",
  },
  "team/AgentDetailView.tsx": {
    count: 1,
    why:
      "The teammate profile block: a 56px avatar that is itself the control for " +
      "changing it (#1181), the name, the role, and a row of desk and tier badges. " +
      "It also renders only once the teammate has loaded, so it cannot be hoisted " +
      "to a header that has to exist through the loading and error states too.",
  },
};

/** Every `.tsx` under `src/views`, as paths relative to it. */
function views(dir = VIEWS, prefix = ""): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) return views(join(dir, entry.name), rel);
    return entry.name.endsWith(".tsx") ? [rel] : [];
  });
}

/**
 * `<h1` outside comments.
 *
 * A doc comment that *names* the anti-pattern it is warning about must not
 * count as the anti-pattern — the same trap `assert-design-tokens.sh` documents
 * having fallen into. Block comments are stripped whole (they span lines);
 * `//` only when it opens the line, so a `//` inside a string literal on a line
 * of real code cannot blind the scan to an `<h1>` before it.
 */
function handRolledCount(source: string): number {
  const code = source
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/^[ \t]*\/\/.*$/gm, "");
  return code.match(/<h1[\s>]/g)?.length ?? 0;
}

const SOURCES = new Map(views().map((rel) => [rel, readFileSync(join(VIEWS, rel), "utf8")]));

describe("page headers come from PageHeader (#1763)", () => {
  it("finds views to check at all, so a broken glob cannot pass silently", () => {
    expect(SOURCES.size).toBeGreaterThan(20);
  });

  it("has no view hand-rolling a page heading outside the allowlist", () => {
    const offenders = [...SOURCES]
      .filter(([rel, src]) => !(rel in HAND_ROLLED) && handRolledCount(src) > 0)
      .map(([rel, src]) => `${rel} opens ${handRolledCount(src)} <h1> of its own`);

    expect(
      offenders,
      `Use <PageHeader> instead — src/components/page-header.tsx.\n` +
        `A heading that genuinely cannot be one needs a row in HAND_ROLLED ` +
        `saying why.\n${offenders.join("\n")}`,
    ).toEqual([]);
  });

  it("holds every allowlisted file to exactly the count it is allowed", () => {
    const offenders = [...Object.entries(HAND_ROLLED)]
      .map(([rel, { count }]) => {
        const src = SOURCES.get(rel);
        if (src === undefined) return `${rel} is allowlisted but no longer exists`;
        const found = handRolledCount(src);
        return found === count ? null : `${rel} opens ${found} <h1>, allowed ${count}`;
      })
      .filter((line): line is string => line !== null);

    expect(
      offenders,
      `An allowlisted file grew or lost a heading. If it grew one, it is a new ` +
        `page header and belongs in <PageHeader>; if it lost one, drop the row.\n` +
        `${offenders.join("\n")}`,
    ).toEqual([]);
  });

  it("has every view with a visible page header importing the component", () => {
    // A view that renders a title without importing PageHeader is a view that
    // found some other way to draw one — which is the thing being prevented.
    const drawn = [...SOURCES].filter(([, src]) => src.includes("<PageHeader"));
    const missing = drawn
      .filter(([, src]) => !src.includes('from "@/components/page-header"'))
      .map(([rel]) => rel);

    expect(missing, `render <PageHeader> without importing it: ${missing.join(", ")}`).toEqual([]);
    expect(drawn.length).toBeGreaterThan(15);
  });
});

/** The file a leaf points at, whichever kind it is. */
function fileOf(leaf: Leaf): string {
  return "pageHeader" in leaf ? leaf.pageHeader : leaf.handRolled;
}

/** Every (route, leaf) pair, flattened — a dispatching route contributes several. */
function leaves(): [View, Leaf][] {
  return (Object.entries(NAMED_BY) as [View, readonly Leaf[]][]).flatMap(([view, list]) =>
    list.map((leaf): [View, Leaf] => [view, leaf]),
  );
}

describe("every routed view is named by something (#1763)", () => {
  it("has a row for every view the router can reach, and no stale ones", () => {
    // The compile-time `Record<View, …>` already forbids a missing row. This
    // is the runtime half: `VIEWS` (imported as `ROUTED_VIEWS`, since this
    // file already has a `VIEWS` of its own) is derived from `ROUTABLE`, so if the two
    // ever disagree the disagreement is visible here rather than silent.
    expect([...ROUTED_VIEWS].sort()).toEqual([...(Object.keys(NAMED_BY) as View[])].sort());
    expect(ROUTED_VIEWS.length).toBeGreaterThan(15);
  });

  it("has every routed view's named files actually exist", () => {
    const missing = leaves()
      .map(([view, leaf]) => [view, fileOf(leaf)] as const)
      .filter(([, file]) => !SOURCES.has(file))
      .map(([view, file]) => `${view} names ${file}, which is not under src/views`);

    expect(missing, missing.join("\n")).toEqual([]);
  });

  it("has every leaf without a documented exception rendering PageHeader", () => {
    const offenders = leaves()
      .filter((entry): entry is [View, { pageHeader: string }] => "pageHeader" in entry[1])
      .filter(([, leaf]) => !(SOURCES.get(leaf.pageHeader) ?? "").includes("<PageHeader"))
      .map(([view, leaf]) => `${view} is named by ${leaf.pageHeader}, which renders no <PageHeader>`);

    expect(
      offenders,
      `A routed view lost its page header. A page with no header is a page a ` +
        `screen reader cannot announce — which is the state Workspace and the ` +
        `unknown-route page were in before #1763.\n` +
        `Render <PageHeader> there (use hidden if the page is its own content), ` +
        `or move the leaf to handRolled and add the file to HAND_ROLLED with a ` +
        `reason.\n${offenders.join("\n")}`,
    ).toEqual([]);
  });

  it("has every handRolled exception carrying its reason in HAND_ROLLED", () => {
    // One reason, in one place. A second copy here is a second thing to keep
    // true, and the whole argument of this file is that nobody notices when a
    // second copy stops being true.
    const undocumented = leaves()
      .filter((entry): entry is [View, { handRolled: string }] => "handRolled" in entry[1])
      .filter(([, leaf]) => !(leaf.handRolled in HAND_ROLLED))
      .map(([view, leaf]) => `${view} names ${leaf.handRolled} as hand-rolled, but it has no HAND_ROLLED row`);

    expect(undocumented, undocumented.join("\n")).toEqual([]);
  });

  it("has every section page rendering a PageHeader too", () => {
    // Settings and Finance are one routed view each over ten and three
    // bookmarkable addresses. A `Record` keyed on their own tables means a new
    // page with no row is a compile error; this is the runtime half.
    const offenders = [
      ...Object.entries(SETTINGS_NAMED_BY).map(([id, f]) => [`settings/${id}`, f] as const),
      ...Object.entries(FINANCE_NAMED_BY).map(([id, f]) => [`finances/${id}`, f] as const),
    ]
      .filter(([, file]) => !(SOURCES.get(file) ?? "").includes("<PageHeader"))
      .map(([page, file]) => `${page} is named by ${file}, which renders no <PageHeader>`);

    expect(offenders, offenders.join("\n")).toEqual([]);
  });

  it("has a leaf for every route, and none of them empty", () => {
    // A route mapped to `[]` would satisfy every `flatMap` above by having
    // nothing to check — the emptiest possible way to pass.
    const empty = (Object.entries(NAMED_BY) as [View, readonly Leaf[]][])
      .filter(([, list]) => list.length === 0)
      .map(([view]) => `${view} names nothing`);
    expect(empty, empty.join("\n")).toEqual([]);
  });
});

/**
 * A page header sits in the same gutter as the body it heads.
 *
 * `PageHeader` defaults to `px-4`, which is right for a page whose content
 * wrapper is `p-4`. A page whose body is `p-6` has to say so, or the title and
 * its actions sit 8px outside the content they belong to — which is what
 * happened to Lists: before it had a `PageHeader` at all, its heading lived
 * inside the same `p-6` wrapper and lined up.
 *
 * Checked for the two pages that opt out, rather than inferred across every
 * view: a body's padding is not always a single literal on one wrapper, and a
 * guard that has to guess would be a guard nobody trusts.
 */
describe("a header's gutter matches its body", () => {
  const source = (rel: string) => readFileSync(join(VIEWS, rel), "utf8");

  it("gives the p-6 pages the px-6 gutter, in every state", () => {
    for (const rel of ["LedgersView.tsx", "company/ManageListsView.tsx"]) {
      const text = source(rel);
      expect(text, rel).toContain('gutter="px-6"');
      const headers = text.match(/<PageHeader/g) ?? [];
      const gutters = text.match(/gutter="px-6"/g) ?? [];
      expect(gutters.length, `${rel}: every PageHeader needs the gutter`).toBe(
        headers.length,
      );
    }
  });

  /**
   * And the Observatory names the run in *every* state, not only when ready.
   *
   * The lazy fallback was corrected first, so a direct `#/observatory/<runId>`
   * visit announced "Run" — and then the chunk landed and the loading branch
   * replaced it with "Observatory" until the run request settled. A heading that
   * corrects itself and then un-corrects itself is worse than one that was
   * always wrong.
   */
  it("names an Observatory run in the load, unavailable and error states too", () => {
    const text = source("observatory/ObservatoryView.tsx");
    expect(text).not.toContain('<PageHeader title="Observatory"');
    const named = text.match(/title=\{runId \? "Run" : "Observatory"\}/g) ?? [];
    const headers = text.match(/<PageHeader/g) ?? [];
    expect(named.length, "every state header follows the same rule").toBe(
      headers.length,
    );
  });
});
