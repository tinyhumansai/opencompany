// @vitest-environment jsdom

import { describe, expect, it } from "vitest";

import {
  SELECT_SCROLL_DOWN_ARROW_CLASSES,
  SELECT_SCROLL_UP_ARROW_CLASSES,
} from "@/components/ui/select";

/**
 * The select popup's truncation affordance (issue #975).
 *
 * Live QA on a company with eight workflows recorded two of them as *"not
 * inspected due to dropdown truncation"* — including Feature Pipeline, the
 * healthiest workflow on the tenant. The control under-reported the company's
 * own contents to someone whose job was to enumerate them.
 *
 * # What was actually wrong, which is not what it looked like
 *
 * A scroll arrow was already there and already worked. Base UI mounts one
 * exactly when the list can scroll further, and a Chromium measurement confirms
 * it is present and visible at the moment of truncation. Two things made it
 * useless anyway:
 *
 * 1. Base UI puts `base-ui-disable-scrollbar` on the list whenever scroll arrows
 *    are mounted — the native scrollbar is deliberately traded away for the
 *    arrow, so the arrow is the *only* remaining signal.
 * 2. The arrow was a low-contrast chevron on a flat, **opaque** `bg-popover`
 *    strip — and that strip painted over the half-clipped next row underneath
 *    it. The list therefore ended on a clean, uncut row. A list that ends tidily
 *    reads as a list that ended.
 *
 * The clipped row was the affordance all along; the opaque strip was hiding it.
 * So the fix is a gradient that fades to transparent instead of a band that
 * covers: the next row shows through, faded, and the boundary reads as a cut.
 *
 * # Why this asserts strings
 *
 * jsdom does no layout, so Base UI never measures the list as scrollable and the
 * arrows never mount — and rendering the parts standalone throws for want of
 * `SelectRootContext`. There is no rendered element to interrogate. What can be
 * pinned is the rule, and the rule is the whole defect: a revert to an opaque
 * strip fails here rather than shipping and being missed by a tester again.
 */
describe("the select popup's scroll affordance", () => {
  for (const [name, cls] of [
    ["down", SELECT_SCROLL_DOWN_ARROW_CLASSES],
    ["up", SELECT_SCROLL_UP_ARROW_CLASSES],
  ] as const) {
    describe(`the ${name} arrow`, () => {
      it("fades to transparent, so the clipped row underneath shows through", () => {
        expect(cls).toContain("to-transparent");
      });

      it("does not paint an opaque band over the list edge", () => {
        // The exact regression: `bg-popover` as a flat fill covered the very
        // half-row that says the list continues.
        expect(cls).not.toMatch(/(^|\s)bg-popover(\s|$)/);
      });

      it("keeps its chevron on the solid end of the fade", () => {
        // The gradient is solid for the first 30% at the outer edge and the
        // chevron is pinned there, so making the strip see-through costs the
        // icon no contrast.
        expect(cls).toMatch(/from-30%/);
        expect(cls).toMatch(name === "down" ? /items-end/ : /items-start/);
      });
    });
  }

  it("fades each arrow away from its own edge", () => {
    // Direction matters: the down arrow must fade upward into the content above
    // it and the up arrow downward. Swapping them would fade the popup's own
    // outer edge into the page and leave the covered row still covered.
    expect(SELECT_SCROLL_DOWN_ARROW_CLASSES).toContain("bg-gradient-to-t");
    expect(SELECT_SCROLL_UP_ARROW_CLASSES).toContain("bg-gradient-to-b");
  });

  it("still pins each arrow to its edge", () => {
    // Guards the guard: every assertion above would pass against an empty
    // string, so prove the constants are the arrows' real classes.
    expect(SELECT_SCROLL_DOWN_ARROW_CLASSES).toMatch(/(^|\s)bottom-0(\s|$)/);
    expect(SELECT_SCROLL_UP_ARROW_CLASSES).toMatch(/(^|\s)top-0(\s|$)/);
  });
});
