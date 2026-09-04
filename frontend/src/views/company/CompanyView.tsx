// The Company page (issue #1141): the teammates, with the desk hierarchy one
// click away at its own address.
//
// # Why this exists
//
// Everything on this page already existed and none of it was reachable. `#/team`
// rendered the teammate card grid and `#/team/<agentId>` opened a full detail
// sub-page — but `team` was routable *without a nav entry*, so no operator ever
// arrived at either. Meanwhile the one nav entry that leads here, Company,
// opened the org chart: a tree of desks, which is the company's filing system
// rather than the company's people.
//
// # Cards is the page; the chart is a destination (issue #1193)
//
// This shipped as a Cards ⇄ Org chart toggle, matching the Workflows index.
// That was wrong, and wrong in a way worth recording: a toggle says *two
// renderings of one thing*, and these are not that. The cards are the roster;
// the chart is the only surface in the console that can create a desk, delete
// one, move somebody between desks, reorder seats or change a lead — the way in
// that issue #302 closed and #311 deliberately reopened.
//
// Presenting it as the other half of a switch made desk management look like a
// view preference, and — because the preference was remembered — could open
// Company on the org chart for an operator who just wanted to see their team.
//
// So there is no mode and nothing is remembered. The **route** decides:
//
//   `#/company`            the teammates
//   `#/company/desks`      the org chart
//   `#/company/<deskId>`   the org chart, arriving at that desk (issue #485)
//   `#/company/graph`      the knowledge graph (issue #1321)
//
// which means the chart survives a reload, can be linked, and is reached by
// asking for it rather than by flipping a switch and hoping.
//
// Manage Lists (issue #1284) is deliberately NOT a segment of this page — an
// early cut put it here, "parallel to Manage Desks", and that analogy was
// wrong: desks are company *structure*, so managing them belongs here; lists
// are work records, reached almost entirely from the title switcher on Work's
// own screen. See `docs/spec/runtime/ledgers-console-ia.md`'s Rule 2 and
// `LedgersView.MANAGE_SEGMENT`.

import type { OpenCompanyClient } from "@/api/client";
import { Overview } from "@/views/Overview";
import { OrgChartView } from "@/views/company/OrgChartView";
import { TeamView } from "@/views/TeamView";

/**
 * The sub-page segment that names the chart itself rather than a desk on it.
 *
 * Reserved, and cheaply so: `OrgChartView` mints desk ids by slugifying a name
 * to `[a-z0-9_]`, so a *created* desk can only collide by being named literally
 * "Desks". A company whose manifest declares that id reaches its chart with no
 * desk brought into view — the same silent no-op an unknown id already gets
 * (see `focusDeskId`), never an error or a missing page.
 */
export const DESKS_SEGMENT = "desks";
/**
 * The declared-structure graph, moved off the operator landing page (#1321).
 *
 * Reserved like {@link DESKS_SEGMENT}, and with the same accepted collision:
 * `graph` is a legal desk id (`[a-z0-9_]+`), so a company whose manifest
 * declares a desk literally named `graph` cannot focus it through its
 * `#/company/graph` link — the address renders the graph instead. That is the
 * same tradeoff `desks` already makes, and resolving it would cost an async
 * desk read in this routing switch (delaying every graph paint, or flashing
 * the wrong page for the rare company that declares the id), so the collision
 * is documented rather than engineered around.
 */
export const GRAPH_SEGMENT = "graph";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * The hash's second segment: nothing, {@link DESKS_SEGMENT}, or a desk id.
   *
   * Handed back unvalidated, as `useHashView` documents — only the chart knows
   * which desks exist, so an id naming none is its own silent no-op rather than
   * something this component guesses at.
   */
  sub: string | null;
  /** Go to the chart, a desk, or back to the roster. */
  onNavigate: (sub: string | null) => void;
  /**
   * Open a teammate, or return to this page with `null`.
   *
   * The roster's detail page is `#/team/<agentId>`, an address of its own
   * (issue #264) rather than a second segment of `#/company` — so this page
   * never renders it, and always hands the roster `sub={null}`.
   *
   * `edit` opens that page with its edit form already showing (issue #1989),
   * which is where the reduced Add-teammate dialog lands: it collects a name
   * and a sentence, and the copilot that drafts the rest lives in that form.
   *
   * Spelled out here rather than left to structural typing. A callback taking
   * only `agentId` is assignable to one taking an optional second argument, so
   * a hop that forgot the option would compile, drop the flag, and land the
   * operator on a read-only profile with no copilot in sight — the redesign
   * failing silently, which is the failure this whole change has to avoid.
   */
  onOpenAgent: (agentId: string | null, options?: { edit?: boolean }) => void;
  /** Bumped when first-run setup staffs the company, so the roster re-reads. */
  refreshKey?: number;
  /** Reopen first-run setup, so skipping it is not a dead end. */
  onRunSetup?: () => void;
  /**
   * The company's real name — `feed.status.name` in the shell (issue #1219).
   * The graph's core node should name the company the way the rest of the
   * console does, not fall back to the slug; optional so this component still
   * stands alone, matching `Overview`'s own fallback chain.
   */
  companyName?: string;
}

export function CompanyView({
  client,
  company,
  sub,
  onNavigate,
  onOpenAgent,
  refreshKey,
  onRunSetup,
  companyName,
}: Props) {
  if (sub === GRAPH_SEGMENT) {
    return <Overview client={client} company={company} companyName={companyName} />;
  }

  if (sub) {
    return (
      <OrgChartView
        client={client}
        company={company}
        // The reserved segment names the chart, not a desk on it.
        focusDeskId={sub === DESKS_SEGMENT ? null : sub}
        onBack={() => onNavigate(null)}
        // The chart's own Add-teammate dialog lands a created teammate on its
        // detail page (issue #1989), the same `#/team/<agentId>` address the
        // roster half opens — not a segment of this view.
        onOpenAgent={onOpenAgent}
      />
    );
  }

  return (
    <TeamView
      client={client}
      company={company}
      sub={null}
      onOpenAgent={onOpenAgent}
      refreshKey={refreshKey}
      onRunSetup={onRunSetup}
      onManageDesks={() => onNavigate(DESKS_SEGMENT)}
      // A desk chip on a card opens that desk's own address (issue #1440), the
      // same destination as the chart's desk nodes.
      onNavigateToDesk={(deskId) => onNavigate(deskId)}
    />
  );
}
