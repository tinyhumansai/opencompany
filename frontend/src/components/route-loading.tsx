import { PageHeader } from "@/components/page-header";

/**
 * What a route paints while its `lazy()` chunk is still in flight (codex
 * review, #1785).
 *
 * # The state this exists for
 *
 * Seven routes are code-split — Workspace, Observatory, Workflows, Pages,
 * Finance (the section, and its overview page) and Settings → Usage — and each
 * one's `<Suspense fallback>` was a bare centred `Loading …` line. On a cold
 * chunk that is the whole page: the routed component this repo's heading guards
 * hold to an `h1` is not mounted yet, so for as long as the network takes,
 * `#/workspace` is a page a screen reader cannot announce.
 *
 * It is the same defect the loading states inside those views had before this
 * PR, one level up, and the source scans could not see it: they read the *leaf*
 * files, and these fallbacks live in `app-shell.tsx`, `SettingsSection.tsx` and
 * `finance/FinanceSection.tsx`, which are not leaves.
 *
 * # Why `hidden`, and why the visible line is unchanged
 *
 * `hidden` is `PageHeader`'s existing answer for a state that must be named
 * without being painted, and it is the right one here for two reasons. The
 * honest one: a loading state does not know the header it is standing in for.
 * Workflows' bar reads "Workflows" or "Runs" depending on a tab this component
 * cannot see, and Observatory's reads "Run" on `#/observatory/<id>` — a bar
 * that guessed would flip to a different word the moment the chunk landed,
 * which is worse than no bar. The cheap one: `sr-only` is out of flow, so the
 * centred line below is laid out exactly as it was before this component
 * existed and no route's loading state moves by a pixel.
 *
 * `label` stays each route's own wording ("Loading canvas…" for Workflows)
 * because it is the visible text and it was already right; `title` is the
 * page's name, and it must match the name the loaded header settles on.
 *
 * # Enforcement
 *
 * `test/unit/lazy-route-named-while-loading.test.ts` fails on a `<Suspense>`
 * whose fallback carries no heading unless it has a documented row saying it is
 * a widget inside an already-named page. The next code-split route inherits
 * that on the day it is added rather than six weeks later.
 */
export function RouteLoading({ title, label }: { title: string; label: string }) {
  return (
    <div className="text-muted-foreground flex flex-1 items-center justify-center text-sm">
      <PageHeader hidden title={title} />
      {label}
    </div>
  );
}
