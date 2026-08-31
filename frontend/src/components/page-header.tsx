import type { ReactNode } from "react";

import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";

/**
 * The console's one page header (issue #1763).
 *
 * # What it replaces
 *
 * Twelve distinct hand-rolled heading styles, one per view that happened to
 * need a title: `text-2xl font-semibold tracking-tight` on fourteen pages,
 * `text-xl` on four, `text-lg` on three, `text-sm` on Workflows, `text-base` on
 * the chat channel bar, plus twelve `sr-only`. Beyond type, the *structure*
 * drifted too — some titles sat in a bordered bar, most floated in the page's
 * content column; some carried a count, most did not; Workspace had no header
 * at all and opened straight into an `EXPLORER` toolbar.
 *
 * # The shape, and why this one
 *
 * The operator named **Workflows** as the reference, and what makes that header
 * read as designed is structural rather than typographic:
 *
 *   - it is a **contained bar** anchored to the content surface, with a
 *     hairline beneath it, so the page's name is chrome rather than a paragraph
 *     that happens to be bold;
 *   - the **count** rides inline with the title, so "how many of these are
 *     there" is answered where the question is asked;
 *   - the **actions are right-aligned on the title row**, against the surface's
 *     own edge — the shape #1207 argued for on Company and Desks, where an
 *     actions row of its own was anchored to nothing.
 *
 * So this component adopts the Workflows *layout* wholesale, and does **not**
 * adopt its `text-sm` type. That size is a consequence of Workflows' header
 * being a toolbar — a strip that also carries a segmented control and a filled
 * button, where 14px keeps the title from towering over 32px controls. At page
 * level it costs the console the top of its type hierarchy: "Company" set at
 * 14px is the same size as the body text under it and the same size as the
 * button labels beside it, so the page's own name reads as no more important
 * than a control. `TITLE` below is the compromise the console had already
 * arrived at twice without saying so — the chat channel bar is `text-base`, and
 * a bar heading wants to be a heading without being a hero.
 *
 * # `hidden`: the twelve `sr-only` headings are a variant, not a bug
 *
 * Chat, Conversation, Inbox, Pages, Approvals and the Overview are their own
 * content: the first thing on the page is the thing you came for, and a title
 * bar over it would be chrome for its own sake. They keep an accessible name —
 * a page with no `h1` is a page a screen reader cannot announce — and paint
 * nothing. That is `hidden`, and it is why the accessible name and the visible
 * bar are one component rather than two: a page cannot acquire the first
 * without deciding about the second.
 *
 * # Enforcement
 *
 * `test/unit/page-header-adoption.test.ts` fails if a view hand-rolls an `<h1>`
 * outside this component. A convention nobody can re-break by accident is the
 * difference between fixing this once and fixing it again in six weeks — the
 * same reason `scripts/ci/assert-design-tokens.sh` rejects raw hex rather than
 * asking people not to write it.
 */

/**
 * `text-lg` (18px), not `text-2xl` (24px) and not Workflows' `text-sm`.
 *
 * 24px is a hero size that reads correctly only with air above it — which is
 * exactly what the fourteen pages using it had, and exactly what a bar does not
 * give. Dropped into a `py-3` strip beside `size="sm"` buttons it makes the bar
 * ~72px tall and the buttons look like an afterthought pinned to a banner.
 *
 * 18px sits a full step above the 14px body text and the 14px control labels,
 * so the page's name still reads first, while leaving the bar the same height
 * as the controls it carries.
 */
const TITLE = "min-w-0 truncate text-lg font-semibold tracking-tight";

/**
 * The bar. `py-3` is Workflows' own padding, kept so the reference page is
 * unchanged by adopting the component that was derived from it. The horizontal
 * gutter rides on the inner column rather than here, so that the bar and its
 * hairline still span the surface while the title sits on the page's own
 * column.
 *
 * That gutter is `GUTTER` below by default and the caller may override it,
 * because it has to equal whatever the page body uses. A fixed `px-4` was
 * *stated* to make "the title land on the same vertical as the first word
 * beneath it" and did not: measured in Chromium at 1440px, the Overview body
 * (`p-5 sm:p-8`) put its content 16px inside the title, and the Styleguide
 * (`px-6`) 8px — both of which had been aligned before adopting this
 * component (codex review, #1785). A promise the code does not keep is worse
 * than no promise, so the code now keeps it.
 *
 * The hairline is the load-bearing half: it is what separates "the page's name"
 * from "the page", and it is what every floating title was missing.
 */
const BAR = "shrink-0 border-b py-3";

/**
 * The default horizontal gutter: Workflows' own, and right for the many pages
 * whose body is `px-4`.
 *
 * A page whose body uses anything else passes its own through `gutter`. There
 * is no clever derivation available here — the body is a sibling this
 * component never sees — so the caller states it, and the rule is simply that
 * the two must be the same.
 */
const GUTTER = "px-4";

/**
 * The width of the header's *row*, which must match the width of the column the
 * page's body uses.
 *
 * The bar and its hairline always span the surface — a rule that stops short of
 * the edge reads as a divider inside the content rather than as the frame of
 * the page. What this decides is where the title and the actions sit on that
 * rule: `full` puts them against the surface's own gutter, and a `max-w-*`
 * token centres them on the same column the content below is centred on, so
 * the title lands on the same vertical as the first word beneath it.
 *
 * Named for the Tailwind token rather than for an intent ("page", "narrow")
 * because the only correct value is whatever the body already uses, and a name
 * that abstracts that away is a name a caller has to guess at.
 */
export type PageHeaderWidth = "full" | "3xl" | "4xl" | "5xl" | "6xl";

const COLUMN: Record<PageHeaderWidth, string | false> = {
  full: false,
  "3xl": "mx-auto w-full max-w-3xl",
  "4xl": "mx-auto w-full max-w-4xl",
  "5xl": "mx-auto w-full max-w-5xl",
  "6xl": "mx-auto w-full max-w-6xl",
};

export type PageHeaderProps = {
  /** The page's name. Rendered as its one `h1`. */
  title: ReactNode;
  /**
   * How many of the thing the page is a list of. Inline with the title, in the
   * Workflows idiom — omitted rather than shown as `0` is the caller's choice,
   * because "no workflows yet" and "this page does not count" are different.
   */
  count?: number;
  /** One line under the title, for what the page is for. */
  description?: ReactNode;
  /** Right-aligned on the title row (#1207). */
  actions?: ReactNode;
  /**
   * Before the title, inside the row — a back control, an icon, a breadcrumb.
   * Kept in the row so it reads as part of the heading rather than above it.
   */
  leading?: ReactNode;
  /**
   * One quiet line *above* the title, naming the context the page sits in —
   * the company on the Overview, "OpenCompany design system" on the Styleguide.
   *
   * Rendered as a `div` rather than a `p` on purpose: `PageHeader`'s one `p` is
   * the description, and `company-header-layout.test.ts` finds that description
   * by taking the first `p` in the header. A second one above it would silently
   * become the thing that test asserts about.
   */
  eyebrow?: ReactNode;
  /** After the title and count — status chips that qualify the page itself. */
  trailing?: ReactNode;
  /**
   * Beneath the description, still inside the bar.
   *
   * For a header whose "what this page is" is a *disclosure* rather than a
   * sentence — Ledgers keeps the engine's description of a list behind an
   * "About this list" `<details>` (issue #1349), and a `<details>` cannot live
   * inside the `<p>` `description` renders into.
   */
  children?: ReactNode;
  /**
   * The page is its own content: keep the accessible name, paint nothing.
   * Everything else on this component is ignored, which is deliberate — an
   * invisible header with actions in it would be actions nobody can reach.
   */
  hidden?: boolean;
  width?: PageHeaderWidth;
  /**
   * The row's horizontal padding, which **must match the page body's**.
   *
   * Defaults to `px-4`. Pass the body's own classes when it differs —
   * `gutter="px-5 sm:px-8"` for a `p-5 sm:p-8` body — so the title and the
   * first word beneath it share a vertical. Responsive values are why this is
   * a class string rather than a token: the Overview's gutter changes at `sm`.
   */
  gutter?: string;
  /** On the bar, for a page that needs to find its own header in a test. */
  "data-testid"?: string;
  /**
   * On the title row rather than the bar. Two pages pin the row itself —
   * `company-header` and `desks-header`, from #1207 — because what they assert
   * is that the heading and the actions share it.
   */
  rowTestId?: string;
  /** On the `h1`. */
  titleTestId?: string;
  className?: string;
};

export function PageHeader({
  title,
  count,
  description,
  actions,
  leading,
  trailing,
  eyebrow,
  children,
  hidden = false,
  width = "full",
  gutter = GUTTER,
  rowTestId,
  titleTestId,
  className,
  "data-testid": testId,
}: PageHeaderProps) {
  if (hidden) {
    return (
      <h1 className="sr-only" data-testid={titleTestId}>
        {title}
      </h1>
    );
  }

  return (
    <div className={cn(BAR, className)} data-testid={testId}>
      <div className={cn(gutter, COLUMN[width])}>
        {eyebrow && <div className="text-sm text-muted-foreground">{eyebrow}</div>}
        {/*
          `ml-auto` on the actions rather than `justify-between` on the row: with
          a count badge between the title and the actions, `justify-between`
          spaces all three evenly and strands the badge in the middle of the bar.
          `flex-wrap` is what makes the narrow case survive — a header carrying a
          count, a description and two actions is the one that wraps, and it
          wraps the actions onto their own line still right-aligned.
        */}
        <div
          className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1.5"
          data-testid={rowTestId}
        >
          {leading}
          <h1 className={TITLE} data-testid={titleTestId}>
            {title}
          </h1>
          {count !== undefined && <Badge variant="secondary">{count}</Badge>}
          {trailing}
          {actions && (
            <div className="ml-auto flex min-w-0 flex-wrap items-center justify-end gap-2">
              {actions}
            </div>
          )}
        </div>
        {/*
          A sibling of the row, never inside it — #1207's shape, and what
          `company-header-layout.test.ts` pins: the heading and its actions on
          one line, the description as its own line beneath.
        */}
        {description && (
          <p className="mt-1 text-sm text-balance text-muted-foreground">
            {description}
          </p>
        )}
        {children}
      </div>
    </div>
  );
}
