// The Settings section's sub-page table, and the helpers that read it.
//
// It lives in its own module rather than beside the section that renders it so
// that anything *pointing at* a sub-page can name one without importing the
// section — which imports every view under it, and would import itself back
// through them. `device-pairing.tsx` is the case that forced it: it tells a
// desktop user where to go, and for one release it told them to go somewhere
// that did not exist (issue #1476). Directions read from this table cannot say
// that again — a page id that is not here is a type error.

import {
  Activity,
  MessageSquareWarning,
  Palette,
  ShieldCheck,
  ChartColumnBig,
  type LucideIcon,
  Settings2,
  UserCog,
} from "lucide-react";

/** The sub-pages that live under Settings. The id is the hash's second segment. */
export const SETTINGS_PAGES = [
  {
    id: "general",
    label: "General",
    icon: Settings2,
    hint: "Approvals, connection, lifecycle, domain, mail",
    group: "identity",
  },
  {
    id: "people",
    label: "People",
    icon: UserCog,
    hint: "Who can sign in, and as what",
    group: "identity",
  },
  // The autonomy tier and the always-ask list. It was the second card on
  // General, high on the page because it is what an operator drowning in
  // approval cards comes here for — which is a reason to be a row, not a
  // scroll position.
  //
  // NOT `#/approvals`, the sidebar row: that is the queue of decisions waiting
  // right now, and this is the standing rule that decides what reaches it. The
  // hints below are written to keep the two apart.
  {
    id: "approvals",
    label: "Approvals",
    icon: ShieldCheck,
    hint: "The standing rule for what agents may do unattended",
    group: "capability",
  },
  // One question per page. "Connections" carried five — third-party accounts,
  // MCP servers, inference, channels, repositories — so each was something an
  // operator scrolled past on the way to another. The first three became pages;
  // the last two left the product.
  //
  // Two of those three then left this rail entirely: third-party accounts (as
  // **Apps**) and MCP servers are the Connections section now, at
  // `#/connections/apps` and `#/connections/mcp`, because what the company can
  // act through is something an operator reads repeatedly rather than
  // configures once. Both `#/settings/oauth` and `#/settings/mcp` still
  // resolve, rewritten onto the section by `console-route-rewrites.ts`, so
  // every link minted while they lived here works.
  //
  // Inference followed them, and this file used to say it would not. The
  // reason given — a credential form belongs beside the one thing it unlocks —
  // was answering the wrong question: this rail is not "beside the model", it
  // is configuration an operator visits once, and the model a company thinks
  // with is the most-read, most-changed thing that was on it. It is
  // `#/connections/inference` now, rewritten from here so every link minted
  // while it lived on this rail still works. Skills went with it, for the
  // matching reason on the capability group below.
  //
  // Hosting and Search went too, which emptied the Integrations group and
  // retired it. This file argued for years that a credential form belongs
  // beside what it unlocks; what settled it is that the thing a deploy token
  // and a search key each unlock IS the connection, so "beside what it
  // unlocks" was always an argument for the Connections section rather than
  // against it. Both resolve from their old addresses. See
  // `connection-pages.ts`.
  //
  // What is left on this rail is what Settings is for: who can sign in, how
  // the company behaves, what it did, and what it spends. No row below has an
  // outside service at the other end of it, and a new row that does belongs in
  // Connections rather than here.
  // Skills is NOT here any more: it is `#/connections/skills`, rewritten from
  // this rail so every link minted while it lived here works. Installing a
  // skill is the same act as connecting an app — granting the company a
  // capability it did not have a minute ago — and it is read far more often
  // than it is set, which is the test the Connections section applies. See
  // `connection-pages.ts`.
  //
  // The run observatory: what the company's agents actually did, run by run.
  //
  // It had a nav row of its own and lost it to the four-section restructure.
  // Filed here rather than parked, because this is where an operator goes to
  // ask a question *about* the company rather than to work in it. It used to
  // sit beside Skills as the other half of a pair — what teammates are told to
  // do, and what they did — and keeps its place now that Skills has moved:
  // what a company DID is a question about it, not a capability you grant it.
  //
  // This row is a doorway, not the address. `#/settings/observatory` is
  // rewritten straight back onto `#/observatory` by `console-route-rewrites.ts`
  // — the Observatory reads four query keys off the hash and keys them on its
  // head being `observatory`, so under `#/settings/…` its analytics tab and its
  // agent/turn selection stop being addressable. A single run keeps the same
  // top-level shape, `#/observatory/<runId>`, because workflow rows, approval
  // cards and chat all link straight to one — burying that behind a settings
  // rail would break every link that names a run.
  { id: "observatory", label: "Observatory", icon: Activity, hint: "What your agents actually did", group: "capability" },
  // A fact about this browser rather than about the company: the theme is
  // stored per client, and changing it changes nothing for anyone else who
  // signs in. That is what separates it from every card left on General.
  {
    id: "appearance",
    label: "Appearance",
    icon: Palette,
    hint: "Theme and accent",
    group: "console",
  },
  // Brain is NOT here: it has its own nav row (`#/brain`). It was the one page
  // on this rail an operator came to *read* rather than to change — settings
  // are configuration, and what the company remembers is not configuration.
  // `#/settings/brain` still resolves, rewritten onto the row by
  // `console-route-rewrites.ts`, so every link minted while it lived here works.
  // Feedback was a glyph in the window's title row, beside Settings — which
  // made it chrome, on a par with "where you are" and "what the agents may do".
  // It is not that: it is a page you visit rarely and deliberately, which is
  // what this rail is a list of. Filed under "This console" because that is
  // exactly what it is about — the product, not the company running in it.
  { id: "feedback", label: "Feedback", icon: MessageSquareWarning, hint: "Tell us what is wrong or missing", group: "console" },
  { id: "usage", label: "Usage", icon: ChartColumnBig, hint: "What this company is spending", group: "spend" },
] as const satisfies readonly { id: string; label: string; icon: LucideIcon; hint: string; group: string }[];

export type SettingsPage = (typeof SETTINGS_PAGES)[number]["id"];

/** The settings rail groups related sub-pages without changing their routes. */
export const SETTINGS_PAGE_GROUPS = [
  { id: "identity", label: "Identity & lifecycle" },
  { id: "capability", label: "Capability" },
  { id: "console", label: "This console" },
  { id: "spend", label: "Spend" },
] as const satisfies readonly { id: (typeof SETTINGS_PAGES)[number]["group"]; label: string }[];

export const DEFAULT_SETTINGS_PAGE: SettingsPage = "general";

/**
 * The readable measure a **form** keeps inside a full-width settings page.
 *
 * Every sub-page under this rail used to centre its body on a `max-w-*` column
 * — `3xl` on General and People, `5xl` on Inference, Hosting, Search and
 * Skills, `6xl` on Usage. Issue #2131 removes that: on a 1920px window the
 * settings pane is about 1400px wide, so General's 768px column left roughly
 * 600px of empty margin either side of cards that had nothing to do with
 * prose, and the rail beside them made the whole page read as a narrow strip.
 *
 * What the column was actually protecting is *fields*, not cards. A card is a
 * container — its header, its alerts, its DNS and member tables and its
 * provider grids all get better with width. A text input does not: a single
 * one stretched across a 27" monitor is a worse control than the constraint
 * being removed, because the label at the left and the caret at the right are
 * a head-turn apart.
 *
 * So the page is full width and this is the cap on the field grids inside it.
 * `4xl` (896px) rather than a wider one because these grids are two-column at
 * `sm` and up, which puts each control at roughly 430px — the width they
 * already had inside the old `5xl` body, so no field the operator uses today
 * changes size.
 *
 * A class string rather than a component: the grids that need it already exist
 * and differ (`sm:grid-cols-2`, `sm:items-end`, a nested pair), and wrapping
 * each in a layout component to pass one number would be a bigger change than
 * the number.
 */
export const SETTINGS_FIELD_COLUMN = "max-w-4xl";

/** Whether a hash segment names a real sub-page. */
export function isSettingsPage(sub: string | null): sub is SettingsPage {
  return SETTINGS_PAGES.some((page) => page.id === sub);
}

/** Whether a hash segment names a real sub-page. */
export function resolveSettingsPage(sub: string | null): SettingsPage {
  return isSettingsPage(sub) ? sub : DEFAULT_SETTINGS_PAGE;
}

/**
 * What the sub-nav calls a page, for prose that sends someone to it.
 *
 * Typed to `SettingsPage`, so directions written against this cannot outlive
 * the page they name: renaming a page rewrites the sentence, and removing one
 * stops the build.
 */
export function settingsPageLabel(page: SettingsPage): string {
  return SETTINGS_PAGES.find((p) => p.id === page)!.label;
}

/**
 * The console hash a link to one Settings sub-page needs.
 *
 * Typed for the same reason `settingsPageLabel` is: a link written against this
 * cannot outlive the page it points at.
 *
 * `#/settings/connections` was the standing counter-example: hard-coded in four
 * places across `SetupController` and `SetupDialog`, naming a page that stopped
 * existing when Connections was split into OAuth / MCP / Inference, and so
 * repaired onto General — a dead link that looked like a working one, which is
 * the failure issue #1476 was filed for a release earlier. All four are typed
 * calls to this function now, and all four turned out to mean Inference: every
 * one of them is reached because the company has no usable model. The address
 * itself also answers again, rewritten onto the Connections section.
 */
export function settingsHref(page: SettingsPage): string {
  return `#/settings/${page}`;
}
