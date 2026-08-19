"use client"

import * as React from "react"
import { Select as SelectPrimitive } from "@base-ui/react/select"

import { cn } from "@/lib/utils"
import { ChevronDownIcon, CheckIcon, ChevronUpIcon } from "lucide-react"

const Select = SelectPrimitive.Root

function SelectGroup({ className, ...props }: SelectPrimitive.Group.Props) {
  return (
    <SelectPrimitive.Group
      data-slot="select-group"
      className={cn("scroll-my-1 p-1", className)}
      {...props}
    />
  )
}

function SelectValue({ className, ...props }: SelectPrimitive.Value.Props) {
  return (
    <SelectPrimitive.Value
      data-slot="select-value"
      className={cn("flex flex-1 text-left", className)}
      {...props}
    />
  )
}

function SelectTrigger({
  className,
  size = "default",
  children,
  ...props
}: SelectPrimitive.Trigger.Props & {
  size?: "sm" | "default"
}) {
  return (
    <SelectPrimitive.Trigger
      data-slot="select-trigger"
      data-size={size}
      className={cn(
        "flex w-fit items-center justify-between gap-1.5 rounded-lg border border-input bg-transparent py-2 pr-2 pl-2.5 text-sm whitespace-nowrap transition-colors outline-none select-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:opacity-50 aria-invalid:border-destructive aria-invalid:ring-3 aria-invalid:ring-destructive/20 data-placeholder:text-muted-foreground data-[size=default]:h-8 data-[size=sm]:h-7 data-[size=sm]:rounded-[min(var(--radius-md),10px)] *:data-[slot=select-value]:line-clamp-1 *:data-[slot=select-value]:flex *:data-[slot=select-value]:items-center *:data-[slot=select-value]:gap-1.5 dark:bg-input/30 dark:hover:bg-input/50 dark:aria-invalid:border-destructive/50 dark:aria-invalid:ring-destructive/40 [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4",
        className
      )}
      {...props}
    >
      {children}
      <SelectPrimitive.Icon
        render={
          <ChevronDownIcon className="pointer-events-none size-4 text-muted-foreground" />
        }
      />
    </SelectPrimitive.Trigger>
  )
}

/**
 * The select popup's own box.
 *
 * **Width is a floor, not a match** (issue #811). `--anchor-width` is the
 * trigger's width, and the triggers are `w-fit` — so binding `width` to it made
 * every popup exactly as wide as whatever short value happened to be selected
 * (`trigger`, `none`, `agent`) and clipped its own options mid-word. The
 * workflow node-kind picker was the worst case: each label explains what the
 * kind does, and the explanation is the half that got cut — `Trigger — starts
 * the w…`. An operator meeting that picker for the first time is reading
 * exactly the text that is missing.
 *
 * `min-w` keeps the popup from ever being *narrower* than its trigger, which is
 * the only thing the exact binding bought, while leaving it free to grow to its
 * content. The `max(9rem, …)` inside preserves the old `min-w-36` floor for a
 * trigger narrower than that.
 *
 * `max-w` then stops a long option pushing the popup off-screen:
 * `--available-width` is what the positioner measured to the viewport edge, and
 * 28rem is a readable line length, so it takes the smaller of the two.
 *
 * **The floor is capped by that same ceiling, and it has to be.** When the two
 * conflict CSS resolves in favour of `min-width` — measured, not assumed:
 * `min-width:max(9rem,40rem)` against `max-width:min(28rem,20rem)` lays out at
 * 640px, not 320px. So an uncapped floor would let a trigger wider than the
 * space beside it push the popup straight off the viewport, which is the one
 * thing `max-w` is here to prevent. Wrapping the floor in the same `min(…)`
 * makes the ceiling authoritative in every case.
 */
const SELECT_POPUP_CLASSES =
  "relative isolate z-50 max-h-(--available-height) max-w-[min(28rem,var(--available-width))] min-w-[min(max(9rem,var(--anchor-width)),28rem,var(--available-width))] origin-(--transform-origin) overflow-x-hidden overflow-y-auto rounded-lg bg-popover text-popover-foreground shadow-md ring-1 ring-foreground/10"

/**
 * The shared look of both scroll arrows (issue #975).
 *
 * # Why a gradient and not just a chevron
 *
 * An arrow was already here and already worked: Base UI mounts it exactly when
 * the list can scroll further, and a browser measurement confirms it is present
 * and visible at the moment of truncation. It was still missed in live QA, where
 * a tester recorded two of a company's eight workflows as *"not inspected due to
 * dropdown truncation"* — including the healthiest one on the tenant.
 *
 * The reason is that it was the **only** signal. Base UI adds
 * `base-ui-disable-scrollbar` to the list whenever scroll arrows are mounted, so
 * the native scrollbar is deliberately traded away for the arrow — and the arrow
 * was a small low-contrast chevron on a flat `bg-popover` strip, below a list
 * that ended on a clean, uncut row. A list that ends tidily reads as a list that
 * ended. There was nothing at the cut edge saying otherwise.
 *
 * The gradient is what makes the edge itself the affordance: the row under the
 * arrow fades out instead of stopping square, so the content visibly continues
 * past the boundary rather than finishing at it. `from-55%` keeps the chevron
 * sitting on solid `popover` so it loses no contrast, and only the half nearest
 * the content fades.
 *
 * A count beside the trigger (`Workflows 8`) was the issue's other suggestion
 * and is deliberately **not** what this does. A total tells you how many exist;
 * it does not tell you that the list in front of you is cut, and it would have
 * to be added to each of the twenty-odd selects in the console one at a time.
 * This is one change to the shared primitive and it lands at the point of
 * truncation, which is where the information was missing.
 *
 * `h-7` over the old `py-1`: the gradient needs vertical room to read as a fade
 * rather than a band, and a fixed height keeps both arrows the same size whether
 * or not their icon renders.
 */
const SELECT_SCROLL_ARROW_CLASSES =
  "z-10 flex h-7 w-full cursor-default justify-center text-muted-foreground [&_svg:not([class*='size-'])]:size-4"

/**
 * Exported for `select-scroll-affordance.test.ts`, which cannot reach them any
 * other way: Base UI mounts a scroll arrow only once it measures the list as
 * scrollable, and jsdom does no layout — so the arrows never render there, and
 * rendering the parts standalone throws for want of `SelectRootContext`. The
 * rule is therefore pinned at its source, and the behavioural proof (a real
 * Chromium, twelve entries, a constrained viewport) lives in the PR.
 */
export const SELECT_SCROLL_UP_ARROW_CLASSES = `${SELECT_SCROLL_ARROW_CLASSES} top-0 items-start bg-gradient-to-b from-popover from-30% to-transparent`

/** The down arrow's mirror. See {@link SELECT_SCROLL_UP_ARROW_CLASSES}. */
export const SELECT_SCROLL_DOWN_ARROW_CLASSES = `${SELECT_SCROLL_ARROW_CLASSES} bottom-0 items-end bg-gradient-to-t from-popover from-30% to-transparent`

function SelectContent({
  className,
  children,
  side = "bottom",
  sideOffset = 4,
  align = "center",
  alignOffset = 0,
  alignItemWithTrigger = true,
  ...props
}: SelectPrimitive.Popup.Props &
  Pick<
    SelectPrimitive.Positioner.Props,
    "align" | "alignOffset" | "side" | "sideOffset" | "alignItemWithTrigger"
  >) {
  return (
    <SelectPrimitive.Portal>
      <SelectPrimitive.Positioner
        side={side}
        sideOffset={sideOffset}
        align={align}
        alignOffset={alignOffset}
        alignItemWithTrigger={alignItemWithTrigger}
        className="isolate z-50"
      >
        <SelectPrimitive.Popup
          data-slot="select-content"
          data-align-trigger={alignItemWithTrigger}
          className={cn(SELECT_POPUP_CLASSES, "duration-100 data-[align-trigger=true]:animate-none data-[side=bottom]:slide-in-from-top-2 data-[side=inline-end]:slide-in-from-left-2 data-[side=inline-start]:slide-in-from-right-2 data-[side=left]:slide-in-from-right-2 data-[side=right]:slide-in-from-left-2 data-[side=top]:slide-in-from-bottom-2 data-open:animate-in data-open:fade-in-0 data-open:zoom-in-95 data-closed:animate-out data-closed:fade-out-0 data-closed:zoom-out-95", className )}
          {...props}
        >
          <SelectScrollUpButton />
          <SelectPrimitive.List className="p-1">{children}</SelectPrimitive.List>
          <SelectScrollDownButton />
        </SelectPrimitive.Popup>
      </SelectPrimitive.Positioner>
    </SelectPrimitive.Portal>
  )
}

function SelectLabel({
  className,
  ...props
}: SelectPrimitive.GroupLabel.Props) {
  return (
    <SelectPrimitive.GroupLabel
      data-slot="select-label"
      className={cn("px-1.5 py-1 text-xs text-muted-foreground", className)}
      {...props}
    />
  )
}

function SelectItem({
  className,
  children,
  ...props
}: SelectPrimitive.Item.Props) {
  return (
    <SelectPrimitive.Item
      data-slot="select-item"
      className={cn(
        "relative flex w-full cursor-default items-center gap-1.5 rounded-md py-1 pr-8 pl-1.5 text-sm outline-hidden select-none focus:bg-accent focus:text-accent-foreground not-data-[variant=destructive]:focus:**:text-accent-foreground data-disabled:pointer-events-none data-disabled:opacity-50 [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4 *:[span]:last:flex *:[span]:last:items-center *:[span]:last:gap-2",
        className
      )}
      {...props}
    >
      <SelectPrimitive.ItemText className="flex flex-1 shrink-0 gap-2 whitespace-nowrap">
        {children}
      </SelectPrimitive.ItemText>
      <SelectPrimitive.ItemIndicator
        render={
          <span className="pointer-events-none absolute right-2 flex size-4 items-center justify-center" />
        }
      >
        <CheckIcon className="pointer-events-none" />
      </SelectPrimitive.ItemIndicator>
    </SelectPrimitive.Item>
  )
}

function SelectSeparator({
  className,
  ...props
}: SelectPrimitive.Separator.Props) {
  return (
    <SelectPrimitive.Separator
      data-slot="select-separator"
      className={cn("pointer-events-none -mx-1 my-1 h-px bg-border", className)}
      {...props}
    />
  )
}

function SelectScrollUpButton({
  className,
  ...props
}: React.ComponentProps<typeof SelectPrimitive.ScrollUpArrow>) {
  return (
    <SelectPrimitive.ScrollUpArrow
      data-slot="select-scroll-up-button"
      className={cn(
        SELECT_SCROLL_UP_ARROW_CLASSES,
        className
      )}
      {...props}
    >
      <ChevronUpIcon
      />
    </SelectPrimitive.ScrollUpArrow>
  )
}

function SelectScrollDownButton({
  className,
  ...props
}: React.ComponentProps<typeof SelectPrimitive.ScrollDownArrow>) {
  return (
    <SelectPrimitive.ScrollDownArrow
      data-slot="select-scroll-down-button"
      className={cn(
        SELECT_SCROLL_DOWN_ARROW_CLASSES,
        className
      )}
      {...props}
    >
      <ChevronDownIcon
      />
    </SelectPrimitive.ScrollDownArrow>
  )
}

export {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectScrollDownButton,
  SelectScrollUpButton,
  SelectSeparator,
  SelectTrigger,
  SelectValue,
}
