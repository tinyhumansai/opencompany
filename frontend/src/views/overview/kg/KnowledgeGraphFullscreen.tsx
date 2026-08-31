'use client';

// SPDX-License-Identifier: GPL-3.0-or-later
import { useEffect, useState } from 'react';
import { ArrowLeft, ChevronLeft, ChevronRight } from 'lucide-react';
import type { ToolWiki } from './agent-wiki';
import { ToolDetailCard, type DeptLite } from './KnowledgeDetail';

/**
 * How much of the graph card the detail sheet may take at or below 820px —
 * and, in the second string, where its top edge therefore is (issue #1664).
 *
 * A **percentage of the card**, not `vh`. The sheet is absolutely positioned
 * inside the graph surface, which sits on the console's inset content card and
 * is shorter than the viewport by the shell's frame. Measured at 430x932 the
 * card is 854px tall, so the old `62vh` was 578px — 68% of the box the sheet
 * actually lives in, not 62% — and every offset derived from it (a "38vh band"
 * above it, a paddle centred at `19vh`) was reasoning with a ruler the sheet
 * does not use. `Overview.tsx` made this same correction once already, when the
 * graph claiming `h-svh` inside that card laid itself out taller than its
 * clipping box and cropped its own legend.
 *
 * 55%, not the 62% that number converts to, because the band above the sheet
 * now has to hold the legend as well as the desk selector and the paddles.
 * Measured at 700x800 — the worst of the widths checked — that band is 325px
 * against a desk selector of 50px, a paddle of 80px, and a legend of 140px with
 * its caveat open. At 62% the legend and the paddle overlapped by 21px, which
 * is a paddle drawn across the caveat text. The sheet gives the strip back; its
 * content already scrolls, so what it loses is height and not reach.
 *
 * These two must stay in agreement: the second is the first plus a gap, and it
 * is what holds the legend clear of the sheet. Tailwind scans source for
 * literal class names, so they are strings rather than a computed value.
 */
const SHEET_CAP = 'max-[820px]:max-h-[55%]';
const LEGEND_ABOVE_SHEET = 'max-[820px]:bottom-[calc(55%+0.5rem)]';

/**
 * The chrome around the graph in its fullscreen (only) mode: the desk
 * selector and side paddles for stepping through desks, the vault
 * search/legend slots, and a detail panel that overlays rather than resizes
 * the canvas — so opening or closing a card never reflows the graph. Owns
 * ←/→ and Escape; typing in the vault search suppresses them so the query
 * can use those keys.
 */
export function KnowledgeGraphFullscreen({
  deptList, currentTeamId, currentDept,
  toolWiki, extraDetail, coreOpen = false, onCollapseCore, searchSlot, legendSlot, statusSlot,
  onNavDept, onBack, covered = false, emptyState = false, noDesks = false, children,
}: {
  deptList: DeptLite[];
  currentTeamId: string | null;
  currentDept: DeptLite | null;
  toolWiki: ToolWiki | null;
  /** task / human detail card rendered by the graph (SOP chain nodes) */
  extraDetail?: React.ReactNode;
  /** the Notes vault is expanded — Escape collapses it (via
      onCollapseCore) instead of exiting fullscreen; doing both at once
      stacked two heavy transitions and glitched the exit */
  coreOpen?: boolean;
  onCollapseCore?: () => void;
  /** vault search chip, rendered top-left while the vault is open */
  searchSlot?: React.ReactNode;
  /** compact kind legend, rendered bottom-left */
  legendSlot?: React.ReactNode;
  /** the snapshot line and its Refresh control, rendered top-right */
  statusSlot?: React.ReactNode;
  /** an outage overlay covers the shell; the graph must not answer the
      keyboard at all (issue #1314) */
  covered?: boolean;
  /**
   * The graph is bare — nothing but the company's core node — so the canvas is
   * replaced by an explanation rather than left looking broken.
   *
   * This used to mean "the company has no desks", which suppressed the canvas
   * for every deskless company, including ones with a roster, tools, saved
   * workflows and a memory constellation to draw. Those hang off the core in
   * the model and always had; only this view refused to render them.
   */
  emptyState?: boolean;
  /**
   * The company declares no desks, so the graph has no pillars. The graph is
   * drawn regardless — this only adds the note that says so.
   */
  noDesks?: boolean;
  onNavDept: (teamId: string) => void;
  onBack: () => void;
  children: React.ReactNode;
}) {
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);
  const hasDetail = !!(toolWiki || extraDetail);
  /**
   * How the right-edge chrome gets out of the detail rail's way (issue #1307).
   *
   * The rail is an absolute overlay on purpose — resizing the canvas under it
   * reflowed the graph on every open and close, which is the glitch the
   * comment on the `<aside>` below is about. So the canvas keeps its size and
   * the two controls that live on the right edge move instead: `right-2` is
   * inside the 300px rail, and the rail is `z-30`, so without this they are
   * both covered *and* unclickable rather than merely obscured.
   *
   * Above 820px the rail is that 300px column, and the offset is its width
   * plus the inset the control already had. At or below it the rail is a
   * bottom sheet ([`SHEET_CAP`], anchored bottom) — the right edge is clear
   * again, so the offset is reverted and the paddles rise into the band the
   * sheet leaves instead of staying centred underneath it.
   */
  const clearOfRail = hasDetail
    ? 'right-[316px] max-[820px]:right-3 max-[639px]:right-2'
    : 'right-2 max-[899px]:right-3 max-[639px]:right-2';
  /**
   * Mid-height normally; above the bottom sheet *and* above the legend when
   * there is one (issues #1307, #1664).
   *
   * The band the sheet leaves has to hold three things at or below 820px: the
   * desk selector at the top, these paddles, and the legend — which used to be
   * absent from this sum because the sheet simply covered it outright. 17% is
   * what is left between the other two: below the desk chips (which wrap to
   * several lines on a company with many desks), above the strip the legend now
   * takes immediately over the sheet. Measured clearances with a card open, on
   * the widths this was checked at: 13px / 22px / 27px above, 15px / 28px / 45px
   * below, at 700x800, 800x800 and 430x932.
   *
   * It is not the midpoint of the band any more, and cannot be: at 430x932 the
   * band is 384px against a desk selector of 70px, a paddle of 56px and a legend
   * of 158px with its caveat open. A midpoint would put the paddle through one
   * of the other two.
   *
   * A percentage, not `vh`, for the reason [`SHEET_CAP`] gives.
   */
  const paddleTop = hasDetail ? 'top-1/2 max-[820px]:top-[17%]' : 'top-1/2';
  /**
   * The desk selector's half of the same rule the legend follows (review of
   * #1752).
   *
   * With a card open at or below 820px the paddles leave mid-height and rise
   * into the band above the sheet, which is where the selector already sits.
   * They are `z-40` over its `z-20`, so an overlap is not merely untidy: the
   * paddle covers the first desk chip and wins its clicks. Measured at 700x400
   * the Previous-desk paddle spans y 28..108 against the selector's 33..83.
   *
   * Moving it down is the obvious answer and the wrong one: how far down
   * depends on how many lines the chips wrap to, which depends on the company.
   * So the selector steps out of the paddle's COLUMN instead — 40px wide at
   * this breakpoint, 32px below 640px — which holds at any height and any
   * number of desks. The band's rule, once the sheet is open, is that the outer
   * columns belong to the paddles and every other overlay keeps out of them;
   * the legend already follows it.
   *
   * `left-5` here is unconditional rather than a `sm:` variant, so unlike the
   * legend's case these overrides are not outranked.
   */
  const deskSelectorClearOfPaddles = hasDetail
    ? 'max-[820px]:left-16 max-[639px]:left-12'
    : '';
  /**
   * How the legend gets out of the detail panel's way (issue #1664).
   *
   * It did not, until now: at `z-10` under a `z-30` panel, both anchored to the
   * bottom edge at or below 820px, the sheet covered all seven kind labels and
   * the workflow-placement caveat outright — the caveat #1318 exists to make
   * reachable without a hover. `z-40` above puts it on the level the status slot
   * and the paddles already use, so no later move of the sheet can re-bury it.
   *
   * The panel is two different things either side of 820px, so the legend
   * answers it two different ways:
   *
   * - **Above 820px** the panel is the 300px right rail and the legend stays at
   *   the bottom-left corner, but its width now stops short of the rail (300px
   *   plus the 16px gap the status slot already leaves, hence 21rem). Measured
   *   at 900x800 the legend ran 280px *under* the rail; while it was `z-10`
   *   that read as clipped, and raising it to `z-40` without this would have
   *   traded being covered for covering, which is no better.
   * - **At or below 820px** the panel is the bottom sheet, so the legend lifts
   *   to sit on top of it — [`LEGEND_ABOVE_SHEET`] is the sheet's own cap plus a
   *   gap, so the two cannot drift apart — and is capped at the band that is
   *   left, scrolling rather than climbing into the desk selector.
   *
   *   That cap is `min(26%,calc(45%-6rem))` rather than a flat 26% (review of
   *   #1752). 45% is the band the 55% sheet leaves; 6rem is what stands in it
   *   above the legend — the desk selector at `top-5`, about 50px of chips, and
   *   a gap either side — so the second term is what is genuinely left. It only
   *   binds on a SHORT card: it is 239px against the 26% term's 188px at
   *   700x800 and does nothing, and 49px against 84px at 700x400, where a flat
   *   26% put the legend 16px over the desk selector. The 18px it leaves spare
   *   at that size is one more wrapped line of desk chips; past that the legend
   *   would touch the selector again, and `overview-responsive-chrome.spec.ts`
   *   is what would say so rather than a reader.
   *
   * Two things about how these classes are written are load-bearing rather than
   * stylistic, and both were caught by measuring a real browser rather than by
   * reading:
   *
   * - **No `sm:` in the open-panel list.** Tailwind emits `sm:` (min-width 640)
   *   *after* `max-[820px]`, so a `sm:bottom-5` alongside these wins at 700px
   *   and the lift silently does not happen. It was written that way first, and
   *   the 700px measurement is what caught it. Unconditional utilities are
   *   emitted before every variant, so `max-[820px]` beats those. This is why
   *   `left` moved out of the shared class list and into both branches: the
   *   `sm:left-5` that used to sit there outranked the open-panel indent below
   *   in exactly the same way, and the 800px measurement is what caught THAT.
   * - **The rail clearance is the unconditional value, overridden downward**,
   *   rather than a `min-[821px]` counterpart to `max-[820px]`. A viewport
   *   whose CSS width lands between the two — 820.5px, which is what Playwright
   *   reports for a "820px" viewport on a fractional device scale — matches
   *   neither, and the legend fell back to full width under the rail. There is
   *   no such gap when one side is the default.
   *
   * ## And out of the paddles' columns, not merely below them (review of #1752)
   *
   * At or below 820px the legend and the paddles share one band, and the first
   * version of this kept them apart **vertically** — the legend under the
   * paddles, the paddles at 17%. That holds only while the band is tall enough
   * to stack them, and the band is a percentage of a card that is itself
   * shorter than the window. Measured with a card open at 700x600 the card is
   * 522px: the sheet takes 287, the legend's 26% cap takes 136, and what is
   * left between the desk selector and the legend is 21px for an 80px paddle.
   * There is no percentage that fits, because nothing in that sum scales with
   * the card. The overlap ran from 700x800 downwards — 38px at 700x600, 40px at
   * 700x500 and 800x420, 42px at 700x400.
   *
   * So they are separated on the axis where there IS room. The paddles hug the
   * left and right edges and are 40px wide (32px below 640px); the legend now
   * starts inside the left one and stops short of the right one, and the two
   * are side by side rather than stacked. Vertical position stops mattering,
   * which is what makes this hold at every height rather than at the ones that
   * happened to be measured. The legend wraps to more lines for the width it
   * gives up — and it already scrolls at its cap, so what it loses is width
   * rather than reach.
   *
   * Only in the `hasDetail` branch, and only at or below 820px: with no panel
   * open the legend keeps the full width it has always had, and above 820px the
   * paddles are at mid-height and nowhere near it.
   */
  const legendClearOfDetail = hasDetail
    ? `bottom-5 min-[821px]:left-5 max-w-[calc(100%-21rem)] ${LEGEND_ABOVE_SHEET} max-[820px]:max-h-[min(26%,calc(45%-6rem))] max-[820px]:overflow-y-auto max-[820px]:left-16 max-[820px]:max-w-[calc(100%-8rem)] max-[639px]:left-12 max-[639px]:max-w-[calc(100%-6rem)]`
    : 'bottom-3 left-3 max-w-[calc(100%-1.5rem)] sm:bottom-5 sm:left-5 sm:max-w-[calc(100%-2.5rem)]';
  const idx = deptList.findIndex((d) => d.teamId === currentTeamId);
  const step = (dir: number) => {
    if (deptList.length === 0) return;
    const next = idx < 0 ? (dir > 0 ? 0 : deptList.length - 1) : (idx + dir + deptList.length) % deptList.length;
    onNavDept(deptList[next].teamId);
  };

  useEffect(() => {
    // While the outage overlay covers the shell, the graph must not answer
    // the keyboard at all: `inert` on the covered subtree cannot suppress a
    // `window` listener, so the handler is simply not registered (issue
    // #1314).
    if (covered) return;
    const onKey = (e: KeyboardEvent) => {
      // typing in the vault search (or any input) must not drive navigation
      const tag = (e.target as HTMLElement | null)?.tagName;
      if (tag === 'INPUT' || tag === 'TEXTAREA' || (e.target as HTMLElement | null)?.isContentEditable) return;
      if (e.key === 'Escape') {
        if (hasDetail) onBack();
        else if (coreOpen) onCollapseCore?.(); // close the vault, stay fullscreen
        // Nothing left to close: the graph is the page.
      } else if (e.key === 'ArrowLeft') step(-1);
      else if (e.key === 'ArrowRight') step(1);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hasDetail, coreOpen, idx, deptList, covered]);

  if (!mounted) return null;

  return (
    // Fills its container rather than covering the window: the graph IS the
    // page, so it sits inside the console's chrome instead of over it.
    <div className="flex h-full min-h-0 w-full min-w-0 bg-os-bg">
      {/* the graph fills the field — same view as the inline "demo": every
          department in its spot in the circle, the active one bloomed into its
          tree with its colour glow, the rest dimmed in the background. */}
      <div className="relative min-w-0 flex-1 overflow-hidden bg-os-surface">
        {!emptyState && children}

        {/* vault search — top-left while the Notes core is open */}
        {searchSlot && <div className="absolute left-5 top-5 z-10">{searchSlot}</div>}

        {/* desk selector — compact, TOP-LEFT: convenient, not in the
            graph's way. One named chip per desk (issue #1309).

            It used to be three 10px dots at 50% opacity under the words "Pick
            a desk", and the names existed only in each dot's `title` — so
            the control that exists to choose a desk refused to say which desk
            was which, while the graph named all three in their own colours a
            few inches away. You had to click a blind dot to learn what it was.

            The chips wrap rather than scroll or truncate the row: a company
            with ten desks gets three short lines in the corner, which is a
            legible answer, where a clipped row is not. The colour is the same
            one the desk's node and label carry, so the chip and the desk are
            visibly the same thing. */}
        {!coreOpen && !emptyState && (
          <div
            data-testid="kg-desk-selector"
            className={`absolute left-5 top-5 z-20 flex max-w-[min(34rem,45vw)] flex-col gap-1 rounded-sm-t border border-os-border-strong bg-os-bg/85 px-2.5 py-1.5 backdrop-blur transition-[left] duration-200 ease-standard ${deskSelectorClearOfPaddles}`}
          >
            <span className="font-mono text-3xs uppercase tracking-[0.14em] text-os-dim">
              {/* Names the group rather than instructing. "Pick a desk" was
                  an imperative with no visible object, and at zero desks it
                  asked for something the page made impossible. */}
              {deptList.length > 0 ? 'Desks' : 'No desks yet'}
            </span>
            {/* With no pillars the graph still draws — teammates, tools and
                workflows hang off the core — so this corner is where the fact
                is stated and where the one control that changes it lives. It
                was previously the empty-state overlay's job, and that overlay
                took the whole canvas with it. Only on an answered read: an
                unread `/desks` has `deptList` empty too, and must not be
                offered as a company that has none. */}
            {deptList.length === 0 && noDesks && (
              <a
                href="#/company/desks"
                className="mt-0.5 inline-flex w-fit rounded-sm-t border border-os-border-strong bg-os-surface px-2 py-0.5 text-2xs font-medium text-os-text transition-colors hover:bg-os-bg"
              >
                Create a desk
              </a>
            )}
            {deptList.length > 0 && (
              <div className="flex flex-wrap items-center gap-x-1 gap-y-0.5">
                {deptList.map((d) => {
                  const active = d.teamId === currentTeamId;
                  return (
                    <button
                      key={d.teamId}
                      onClick={() => onNavDept(d.teamId)}
                      title={`${d.name} — bring this desk forward`}
                      aria-current={active ? 'true' : undefined}
                      className={`flex items-center gap-1.5 rounded-sm-t px-1.5 py-0.5 text-2xs leading-tight transition-colors duration-200 ease-standard hover:bg-os-surface hover:text-os-text ${
                        active ? 'font-bold' : 'text-os-muted'
                      }`}
                      style={active ? { color: d.color } : undefined}
                    >
                      <span
                        aria-hidden
                        className={`h-2 w-2 shrink-0 rounded-full transition-all duration-200 ${
                          active ? '' : 'opacity-60'
                        }`}
                        style={{
                          background: d.color,
                          boxShadow: active ? `0 0 8px ${d.color}` : undefined,
                        }}
                      />
                      <span className="max-w-[12rem] truncate">{d.name}</span>
                    </button>
                  );
                })}
              </div>
            )}
          </div>
        )}

        {/* the snapshot line — top-right, clear of the detail rail (issue
            #1307). `z-40` so it stays above the rail even when the offset
            above puts it beside rather than behind it, and `top-5`/`right-5`
            so it sits on the same 20px inset as the desk selector and the
            legend rather than the 12px one it used to carry alone. */}
        {statusSlot && (
          <div
            className={`absolute top-5 z-40 flex flex-col items-end gap-1.5 transition-[right] duration-200 ease-standard ${
              hasDetail ? 'right-[316px] max-[820px]:right-5' : 'right-5'
            }`}
          >
            {statusSlot}
          </div>
        )}

        {/* A graph with no desks has no kind to explain. */}
        {!emptyState && legendSlot && (
          <div
            data-testid="kg-legend"
            className={`absolute left-3 z-40 transition-[bottom,left] duration-200 ease-standard ${legendClearOfDetail}`}
          >
            {legendSlot}
          </div>
        )}

        {/* A newly provisioned company has only its core node. The graph is
            useful once there is something to draw, so say that plainly and lead
            to the one place that can start it, instead of leaving inert graph
            controls around an empty canvas (issue #1313).

            Drawn only when the field really is bare. It used to be drawn for
            every deskless company, over a canvas that was itself suppressed —
            so a company with a roster, tools and saved workflows was told it
            had nothing, while the graph that could have shown all three was
            never rendered. */}
        {emptyState && (
          <div className="absolute inset-0 z-20 grid place-items-center p-5">
            <section
              aria-labelledby="overview-empty-title"
              className="max-w-md rounded-sm-t border border-os-border-strong bg-os-bg/90 px-6 py-5 text-center shadow-lg backdrop-blur"
            >
              <p className="font-mono text-3xs uppercase tracking-[0.14em] text-os-dim">Company overview</p>
              <h2 id="overview-empty-title" className="mt-2 text-lg font-semibold text-os-text">
                {noDesks ? 'No desks yet' : 'Nothing to draw yet'}
              </h2>
              <p className="mt-2 text-sm leading-6 text-os-muted">
                This graph shows how your company&apos;s desks, teammates, work, and workflows connect.
                {noDesks
                  ? ' Create a desk to add its first pillar.'
                  : ' Nothing has been declared for it to draw.'}
              </p>
              <a
                href="#/company/desks"
                className="mt-4 inline-flex rounded-sm-t border border-os-border-strong bg-os-surface px-3 py-1.5 text-sm font-medium text-os-text transition-colors hover:bg-os-bg"
              >
                Create a desk
              </a>
            </section>
          </div>
        )}

        {/* side paddles: slim, hugging the canvas edges at mid-height — you
            turn the wheel from where you're already looking, never the top.
            The right paddle steps aside when the detail panel is open — see
            `clearOfRail`, which is what finally made that sentence true
            (issue #1307). */}
        {!coreOpen && !emptyState && deptList.length > 0 && (
          <>
            <button
              onClick={() => step(-1)}
              aria-label="Previous desk"
              title="Previous desk (←)"
              className={`absolute left-2 z-40 flex h-32 w-12 -translate-y-1/2 items-center justify-center rounded-sm-t border border-os-border bg-os-bg/70 text-os-muted backdrop-blur transition-all duration-200 ease-standard hover:border-os-border-strong hover:text-os-text max-[899px]:left-3 max-[899px]:h-20 max-[899px]:w-10 max-[639px]:left-2 max-[639px]:h-14 max-[639px]:w-8 ${paddleTop}`}
            >
              <ChevronLeft className="h-7 w-7 max-[899px]:h-6 max-[899px]:w-6 max-[639px]:h-5 max-[639px]:w-5" />
            </button>
            <button
              onClick={() => step(1)}
              aria-label="Next desk"
              title="Next desk (→)"
              className={`absolute z-40 flex h-32 w-12 -translate-y-1/2 items-center justify-center rounded-sm-t border border-os-border bg-os-bg/70 text-os-muted backdrop-blur transition-all duration-200 ease-standard hover:border-os-border-strong hover:text-os-text max-[899px]:h-20 max-[899px]:w-10 max-[639px]:h-14 max-[639px]:w-8 ${clearOfRail} ${paddleTop}`}
            >
              <ChevronRight className="h-7 w-7 max-[899px]:h-6 max-[899px]:w-6 max-[639px]:h-5 max-[639px]:w-5" />
            </button>
          </>
        )}
      </div>

      {/* detail panel — an absolute overlay so opening/closing a card never
          resizes the graph area (that reflow was the back-and-forth glitch) */}
      {hasDetail && (
        <aside className={`absolute right-0 top-0 z-30 flex h-full w-[300px] flex-col border-l border-os-border-strong bg-os-bg/95 shadow-lg backdrop-blur max-[820px]:inset-x-0 max-[820px]:bottom-0 max-[820px]:top-auto max-[820px]:w-full max-[820px]:rounded-t-lg-t max-[820px]:border-l-0 max-[820px]:border-t ${SHEET_CAP}`}>
          {/* the trail: node → desk (this) → home. Same affordance inline. */}
          <button
            onClick={onBack}
            aria-label={`Back to the ${currentDept?.name ?? 'graph'} desk`}
            className="flex shrink-0 items-center gap-1.5 border-b border-os-border px-3 py-2 text-left font-mono text-3xs uppercase tracking-[0.14em] text-os-dim transition-colors hover:text-os-text"
          >
            <ArrowLeft className="h-3 w-3 shrink-0" />
            <span className="truncate">
              Back · <span style={currentDept ? { color: currentDept.color } : undefined}>{currentDept?.name ?? 'graph'}</span>
            </span>
          </button>
          {toolWiki ? (
            <ToolDetailCard wiki={toolWiki} onClose={onBack} />
          ) : (
            extraDetail ?? null
          )}
        </aside>
      )}
    </div>
  );
}
