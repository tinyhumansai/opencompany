import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

/**
 * A page's top-level sections all head at `h2`, directly under its one `h1`.
 *
 * Issue #1392 is a family of the same mistake: a page grew an `h1`, and the
 * sections beneath it kept heading at `h3` or `h4`. Each component is
 * unimpeachable read on its own — nothing is wrong with an `h3` until you know
 * what it renders beside — so the defect only exists at the level of the
 * assembled page, which is the level this guards.
 *
 * Two shapes of it were reported, and both are pinned below:
 *
 * - `#/settings/connections` headed all eight of its sections at `h3` under one
 *   `h1`. axe reported `heading-order` against MCP alone, because MCP is the
 *   first section on the page — but every one of them skipped 1→3, so promoting
 *   only the flagged one would have satisfied axe while making the other seven
 *   read as subsections *of* MCP Servers.
 * - `#/feedback` left `FeedbackBoard` at `h3` when the page title became an
 *   `h1`, so the board appeared nested beneath a section that does not exist.
 *
 * What this cannot check is a component's *position*. A section that heads at
 * `h2` correctly can still be rendered before the `h1` — see
 * `nav-rail-headings.test.ts`, which covers the navigation rails where that
 * actually happened.
 *
 * A source guard in the idiom of `dialog-width-override`.
 */
const VIEWS = new URL("../../src/views", import.meta.url).pathname;

/**
 * Per page: the view holding its `h1`, and the components it renders directly
 * that each contribute one top-level section heading to it.
 *
 * Only components whose heading lands in the *page's* outline belong here. A
 * component rendered into a dialog is deliberately absent — `ProviderDetail`
 * heads at `h4`, but it renders inside a `Sheet`, which carries its own
 * `SheetTitle` and its own outline, so it is not a peer of the page's sections.
 */
const PAGES = [
  {
    view: "OAuthView",
    sections: [
      "connections/CompanyCredentialCard",
      "connections/ComposioSection",
      "connections/ProvidersSection",
      "connections/AccountChoiceSection",
    ],
  },
  {
    // The Connections split gave MCP and inference pages of their own, and a
    // page's outline is exactly what that changes: each section that used to
    // sit under the accounts `h1` now sits under its own. Pinned here so the
    // split cannot decay back into a page whose `h2` heads nothing.
    view: "McpServersView",
    sections: ["connections/McpServersSection"],
  },
  {
    view: "InferenceView",
    sections: ["connections/InferenceSection"],
  },
  {
    view: "FeedbackView",
    sections: ["feedback/FeedbackBoard"],
  },
] as const;

/**
 * The heading tags a file opens, in source order.
 *
 * `<PageHeader` counts as an `h1`: since issue #1763 a page's title comes from
 * that component rather than from a heading tag the view writes itself, so a
 * scan that only looked for `<h1` would report every page as having none — and
 * would then read "this page's sections are orphaned" as "this page is fine".
 */
function headingLevels(source: string): number[] {
  return [...source.matchAll(/<(?:h([1-6])|(PageHeader))[\s>/]/g)].map(
    ([, level, pageHeader]) => (pageHeader ? 1 : Number(level)),
  );
}

const read = (name: string) => readFileSync(`${VIEWS}/${name}.tsx`, "utf8");

/** The bare component name, for a path that may carry a directory. */
const componentName = (section: string) => section.split("/").at(-1) ?? section;

describe.each(PAGES)("$view", ({ view, sections }) => {
  it("renders every section this pins, so the list cannot go stale", () => {
    const source = read(view);
    const missing = sections.filter((s) => !source.includes(`<${componentName(s)}`));

    expect(missing, `no longer rendered by ${view}: ${missing.join(", ")}`).toEqual([]);
  });

  it("holds the page's one h1 itself", () => {
    expect(headingLevels(read(view))).toEqual([1]);
  });

  it("has each section head at level two, as peers of one another", () => {
    const offenders: string[] = [];
    for (const section of sections) {
      // A section may carry sub-headings of its own; only its shallowest is a
      // peer of the other sections'.
      const top = Math.min(...headingLevels(read(section)));
      if (top !== 2) offenders.push(`${componentName(section)} heads its section with h${top}`);
    }

    expect(
      offenders,
      `these sections disagree with the ${view} outline:\n${offenders.join("\n")}`,
    ).toEqual([]);
  });
});
