import { MessageSquareWarning, PanelLeftClose, PanelLeftOpen, Settings } from "lucide-react";

import type { View } from "@/components/app-shell";

import { Button } from "@/components/ui/button";
import { useSidebar } from "@/components/ui/sidebar";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { DiscordIcon } from "@/components/discord-icon";
import { DISCORD_INVITE_URL } from "@/lib/links";
import { cn } from "@/lib/utils";

/**
 * A sidebar row at rest: dimmed until you reach for it.
 *
 * The sidebar is standing furniture, on screen behind every view — holding the
 * whole list at full strength makes ten equal-weight rows compete with the
 * content beside them. Hover, keyboard focus, and the active row all come back
 * to full, so nothing is ever dimmed at the moment you are using it.
 */
// `data-active` is a bare boolean attribute on these buttons, not
// `data-active="true"` — match it the same way the sidebar's own styles do.
export const RESTING_ROW =
  "opacity-60 transition-opacity hover:opacity-100 focus-visible:opacity-100 data-active:opacity-100";

// Discord's brand blurple, lifted a step in dark mode so it clears the
// sidebar's surface instead of sinking into it. Named tokens rather than raw
// hex — the colour is deliberately not ours, and saying so in the token name
// is what stops it being "fixed" into the palette later. See `--brand-discord`
// in index.css.
const DISCORD_BLURPLE =
  "text-(--brand-discord-on-light) dark:text-(--brand-discord-on-dark)";

/**
 * Shared footprint for every button on the utility bar.
 *
 * Lifted from {@link SidebarCollapseButton}, which had it first and is now one
 * of four: 28px square under the column, 32px on the rail so it lands on the
 * same rhythm as the nav icons; the resting dim through the ink's alpha rather
 * than `RESTING_ROW`'s `opacity-60`, because opacity dims the focus ring too
 * and the ring is the only thing saying where the keyboard is on an unlabelled
 * button. The three hover classes each replace exactly one of `ghost`'s, so
 * tailwind-merge drops the original instead of leaving the two to race.
 */
const UTILITY_BUTTON = cn(
  "shrink-0 text-sidebar-foreground/60",
  "hover:bg-sidebar-accent hover:text-sidebar-accent-foreground dark:hover:bg-sidebar-accent",
  "focus-visible:ring-sidebar-ring/50",
  "group-data-[collapsible=icon]:size-8",
);

/**
 * One button on the utility bar: icon, tooltip, and a name a screen reader can
 * read.
 *
 * `label` is the accessible name AND the tooltip text, deliberately the same
 * string. These are icon-only in both sidebar states — unlike a nav row, which
 * carries its own label once the column is open — so the tooltip is the only
 * place the word appears on screen, and a tooltip is a visual affordance that
 * cannot be relied on for the name.
 */
function UtilityButton({
  label,
  icon,
  active,
  onClick,
  href,
  className,
  ...rest
}: {
  label: string;
  icon: React.ReactNode;
  /** Marks the button while its own page is open, the way a nav row does. */
  active?: boolean;
  onClick?: () => void;
  /** An external destination — renders an anchor instead of a button. */
  href?: string;
  className?: string;
  "data-tour"?: string;
  "data-testid"?: string;
}) {
  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <Button
            type={href ? undefined : "button"}
            variant="ghost"
            size="icon-sm"
            aria-label={label}
            // `aria-current`, not `data-active`: these are destinations, and
            // this is the same thing the nav rows announce when their page is
            // the one open. Absent — not `false` — when it is not, because
            // `aria-current="false"` is announced by some readers.
            aria-current={active ? "page" : undefined}
            onClick={onClick}
            className={cn(UTILITY_BUTTON, active && "text-sidebar-foreground", className)}
            render={href ? <a href={href} target="_blank" rel="noreferrer" /> : undefined}
            {...rest}
          />
        }
      >
        {icon}
      </TooltipTrigger>
      {/*
        The raw tooltip primitive rather than `SidebarMenuButton`'s `tooltip`
        prop, which hides its content unless the sidebar is collapsed. That is
        right for a nav row and wrong here: expanded is exactly the state in
        which a reader of this bar has never seen the word.
      */}
      <TooltipContent side="right">{label}</TooltipContent>
    </Tooltip>
  );
}

/**
 * The utility bar: one thin row of icons directly under the company switcher.
 *
 * Settings, Feedback, Discord and Collapse. Three of them go somewhere and one
 * changes the chrome, but none is a place an operator *works* — they are
 * reached once a session, if that. As full-width rows they cost four rows of a
 * column whose remaining rows are the actual destinations, and Feedback and
 * Discord pushed the company's own state and its switcher further down the
 * footer every time the list grew.
 *
 * This is the shape OpenHuman's shell already uses for the same four
 * (`vendor/openhuman/app/src/components/layout/shell/SidebarHeader.tsx`):
 * a header band of icon-only tertiary buttons, tooltipped, with the collapse
 * control at the end of the row. Settings keeps its `data-tour` anchor, so the
 * guided tour's "Connect your tools" stop still has something to spotlight.
 *
 * ## The collapsed rail
 *
 * The rail is `--sidebar-width-icon` (3rem) less `SidebarHeader`'s `p-2` — 32px
 * of content box, one icon wide. The row therefore becomes a column there, the
 * same way the switcher row above it already does, and each button grows to
 * 32px so the whole header reads as one stack with the nav icons below it.
 */
export function SidebarUtilityBar({
  view,
  onNavigate,
}: {
  /** The active view, so Settings and Feedback can show as current. */
  view: View;
  onNavigate: (view: View) => void;
}) {
  const { isMobile, setOpenMobile } = useSidebar();

  const navigate = (next: View) => {
    onNavigate(next);
    // The sheet is the whole screen on a phone; leaving it open would hide the
    // page just navigated to. Same rule the nav rows follow.
    if (isMobile) setOpenMobile(false);
  };

  return (
    // `role="group"` so the bar has a name of its own. It sits in the sidebar's
    // header, which is outside the `Main navigation` landmark on purpose — the
    // landmark is the destinations an operator works out of, and these are the
    // utilities that act on the console itself.
    <div
      role="group"
      aria-label="Console utilities"
      data-testid="sidebar-utilities"
      // Centred, not left-aligned under the switcher's glyph. Four icons in a
      // 13.5rem column left no honest edge to align to: flush left they hung
      // under the nameplate's text with a ragged right, and the row reads as
      // one object rather than four list items when it is centred in the
      // column it belongs to. On the rail it is already the full width, so
      // centring is what the column does anyway.
      className="flex items-center justify-center gap-1 group-data-[collapsible=icon]:flex-col"
    >
      <UtilityButton
        label="Settings"
        icon={<Settings />}
        active={view === "settings"}
        onClick={() => navigate("settings")}
        data-tour="nav-settings"
      />
      <UtilityButton
        label="Feedback"
        icon={<MessageSquareWarning />}
        active={view === "feedback"}
        onClick={() => navigate("feedback")}
      />
      {/* Deliberately NOT dimmed with the others.

          The resting dim is safe for near-white text and destroys a mid-tone
          hue: the blurple measures 6.36:1 at full strength and 3.04:1 dimmed.
          Recovering that inside the dim would mean lightening the blurple until
          it is a pale lavender that no longer reads as Discord's colour. The
          hue already sets it apart without help from the property doing the
          damage. */}
      <UtilityButton
        label="Join our Discord"
        icon={<DiscordIcon className="size-4" />}
        href={DISCORD_INVITE_URL}
        className={cn(
          DISCORD_BLURPLE,
          "hover:text-(--brand-discord-on-light) dark:hover:text-(--brand-discord-on-dark)",
        )}
      />
      <SidebarCollapseButton />
    </div>
  );
}

/**
 * Show or hide the sidebar. A button in the header, not a row in the nav.
 *
 * ## Why it is not a row (issue #1177)
 *
 * It used to be a `SidebarMenuButton` — full width, icon then label, `h-8`,
 * `bg-sidebar-accent` on hover — sitting directly under the host switcher and
 * directly above Overview. That is the nav row shape exactly, so the eye filed
 * it as the first destination in the list. It is not a destination: everything
 * else in that column takes you somewhere, and this one changes the chrome and
 * leaves you where you are.
 *
 * Colouring it differently would not have fixed that; the shape is what says
 * "row". So it stops using the row primitive altogether and becomes the
 * console's ordinary icon button, in the sidebar's header — which is the part
 * of the column that talks about the panel rather than about the company.
 * `SidebarContent` below it is the destinations, and the header/content
 * boundary now means something.
 *
 * ## Why it does not crowd the host switcher (issue #1174)
 *
 * The switcher beside it is `h-12`, carries a filled glyph, a two-line
 * nameplate and the cross-host status dot. This is 28px, ghost, and dimmed at
 * rest. They also sit in separate elements, so hovering one never lights the
 * other — which is what stops the pair reading as a single control with a
 * chevron at one end and a panel glyph at the other.
 *
 * ## The collapsed rail
 *
 * The rail is `--sidebar-width-icon` (3rem) and `SidebarHeader` is `p-2`, so
 * there are 32px of content box — exactly the switcher's glyph, and no room
 * for anything beside it. The header row therefore becomes a column on the
 * rail (see `app-shell.tsx`), and this button grows to `size-8` there so it
 * lands on the same 32px rhythm as every nav icon below it.
 *
 * It deliberately drops the `bg-primary` fill it used to take when collapsed.
 * That fill existed to make it findable in a column of identical nav icons; up
 * here it has only the switcher for company, and the switcher's glyph is
 * *already* a filled primary square. Two of those stacked would read as one
 * control, which is the failure the paragraph above exists to prevent.
 */
export function SidebarCollapseButton() {
  const { toggleSidebar, state, isMobile } = useSidebar();
  // `state` tracks the DESKTOP open flag; the sheet has its own (`openMobile`).
  // Reading it unguarded labels an open sheet "Expand sidebar" whenever the
  // desktop state happens to be collapsed — which, since issue #1176 stopped
  // the sidebar auto-collapsing, is now a state an operator can leave behind
  // and come back to on a phone.
  const collapsed = !isMobile && state === "collapsed";
  const label = collapsed ? "Expand sidebar" : "Collapse sidebar";
  const Icon = collapsed ? PanelLeftOpen : PanelLeftClose;

  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <Button
            type="button"
            variant="ghost"
            size="icon-sm"
            // The accessible name, and the only name this control has — an
            // icon-only button with no label is otherwise announced as
            // "button". The tooltip says the same words, but a tooltip is a
            // visual affordance and cannot be relied on for the name.
            aria-label={label}
            // Deliberately NO `aria-expanded`, and not an oversight.
            //
            // The name already carries the state: it says "Collapse sidebar"
            // while the column is showing and "Expand sidebar" once it is a
            // rail, so a reader is told what pressing does and, by the change,
            // what happened. `aria-expanded` on top of that announces the
            // state twice ("Expand sidebar, collapsed") — and `ghost` styles
            // the attribute as "the popup under me is open", which is what it
            // means on the dropdown triggers that variant was written for. On
            // this button it painted a pressed chip for as long as the sidebar
            // was open, and Tailwind sorts `aria-expanded:` after `hover:`, so
            // overriding the chip also swallowed the hover feedback. A second
            // channel saying the same thing is not worth either.
            data-testid="sidebar-collapse"
            onClick={toggleSidebar}
            className={cn(
              // The same resting dim as the rows below, reached through the
              // ink's alpha rather than `RESTING_ROW`'s `opacity-60`: opacity
              // dims the whole box, focus ring included, and the ring on an
              // unlabelled button is the only thing saying where the keyboard
              // is. (`RESTING_ROW` also carries `data-active:opacity-100`,
              // which is a nav row's business and never this one's.)
              "shrink-0 text-sidebar-foreground/60",
              // Three classes replacing exactly one of `ghost`'s each, so
              // tailwind-merge drops the original rather than leaving the two
              // to race: `hover:bg-muted`, `hover:text-foreground` and
              // `dark:hover:bg-muted/50`. The muted tint is tuned against the
              // canvas; this button is on the sidebar's surface, which is a
              // different rung and moving again in issue #1178. The accent is
              // also what every row in this column already hovers to.
              "hover:bg-sidebar-accent hover:text-sidebar-accent-foreground dark:hover:bg-sidebar-accent",
              "focus-visible:ring-sidebar-ring/50",
              // 28px beside a 48px nameplate, 32px on the rail. See the note
              // on the collapsed rail above.
              "group-data-[collapsible=icon]:size-8",
            )}
          />
        }
      >
        <Icon />
      </TooltipTrigger>
      {/*
        The raw tooltip primitive rather than `SidebarMenuButton`'s `tooltip`
        prop, which renders its content with `hidden={state !== "collapsed"}`.
        That is right for a nav row — expanded, the row already carries its
        label — and wrong here: this button is icon-only in BOTH states, and
        expanded is the state in which a reader has never seen the word.

        `side="right"` in both states, matching every other tooltip in this
        column, and the one side that is clear of the sidebar either way.
      */}
      <TooltipContent side="right">{label}</TooltipContent>
    </Tooltip>
  );
}
