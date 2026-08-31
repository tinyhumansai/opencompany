import { useCallback, useEffect, useRef, useState } from "react";
import type { FocusEvent } from "react";

/**
 * Holds a polled list's on-screen order stable while the operator is
 * interacting with it (issue #1414).
 *
 * The Approvals queue is replaced wholesale every 5s poll (`use-company.ts`),
 * and the view maps it straight to cards in host order. So the list mutates
 * under the pointer whenever a card is decided elsewhere, the host sweeper
 * retires an expired one, or an agent turn parks new ones above the card being
 * read. Because every card's Approve button sits at the same fixed x on the
 * right edge, removing a card *above* the pointer slides the next card's button
 * to exactly where the previous one was — and a click already in flight lands
 * on a different request. There is no undo: the host journals the verdict
 * durably before anything slow happens.
 *
 * The fix (option 1 in the issue, the cheapest and the one with no false
 * positives): freeze the rendered order — and hold removals — for as long as
 * the operator is interacting with the queue, then reconcile to the latest poll
 * the instant they are not. "Interacting" is the pointer being over the queue
 * OR keyboard focus being inside it, so both a mouse user clicking down a column
 * and a keyboard user tabbing through it are protected. When nothing is
 * happening the freeze is invisible: `frozen` is `null` and the live list flows
 * straight through.
 *
 * Content updates to cards that survive are deliberately held too while frozen:
 * a card resolving its own in-flight state is driven by the caller's own
 * `deciding` map keyed on id, not by this snapshot, so the button a hovering
 * operator is about to press does not move, yet still shows its spinner.
 *
 * Spread {@link StableList.containerProps} onto the element wrapping the
 * interaction region — the permissions and rows together in the approvals
 * view — whose bounds define "over the queue". It also carries a `ref`:
 * see the note on `focusInsideNow` below for why the thaw check needs the
 * actual container node rather than trusting the focus flag alone.
 */
export interface StableList<T> {
  /** The rows to render, in a pointer-stable order. */
  items: T[];
  /**
   * True while the list is held — the pointer is over the container or focus is
   * inside it. A caller that renders other content ABOVE the held rows (the
   * standing-permissions section above the Approvals queue) reads this to defer
   * layout changes of its own for the same reason the rows are frozen: a section
   * that appears above the queue shifts every approve/decline control while the
   * pointer is already aiming at one (#1593).
   *
   * The hold is interaction-scoped, not row-scoped: entering over an *empty*
   * queue still holds (so the above-queue section defers), while the rows
   * themselves are not frozen — there is nothing to protect, and freezing an
   * empty snapshot would keep a just-arrived approval invisible.
   */
  holding: boolean;
  /** Handlers (and a `ref`) for the queue container that detect operator interaction. */
  containerProps: {
    onPointerEnter: () => void;
    onPointerLeave: () => void;
    onFocusCapture: () => void;
    onBlurCapture: (event: FocusEvent) => void;
    ref: (node: HTMLElement | null) => void;
  };
}

export function useStableList<T>(live: T[]): StableList<T> {
  // The held snapshot, or `null` when the list is flowing live. Kept as the
  // actual rows rather than a set of ids so a card removed by the host stays on
  // screen — in place — until the operator moves away, instead of vanishing and
  // pulling the queue up under the pointer.
  const [frozen, setFrozen] = useState<T[] | null>(null);
  // The interaction hold itself, independent of whether there was anything to
  // freeze. Entering the container over a *empty* queue must not freeze an
  // empty snapshot — a stationary cursor would then hold a just-arrived
  // approval off screen until it is auto-declined (#1593) — but the hold still
  // stands, so content rendered above the queue (the standing-permissions
  // section) keeps deferring its own layout changes for the whole interaction.
  const [held, setHeld] = useState(false);

  // Live is read inside the interaction callbacks, which are memoised and must
  // not close over a stale render. A ref updated every render is the current
  // value without re-creating the handlers.
  const liveRef = useRef(live);
  liveRef.current = live;

  // Two independent reasons to be frozen; the list thaws only when BOTH clear.
  const pointerInside = useRef(false);
  const focusInside = useRef(false);

  // The container node, so the thaw check can ask the DOM directly rather
  // than trust `focusInside` alone — see `focusInsideNow`.
  const containerRef = useRef<HTMLElement | null>(null);
  const setContainerRef = useCallback((node: HTMLElement | null) => {
    containerRef.current = node;
  }, []);

  const freeze = useCallback(() => {
    // Never freeze an empty queue: there is nothing to protect, and a frozen
    // empty snapshot would keep a just-arrived approval invisible — until its
    // deadline — for as long as the pointer stays over the empty area.
    if (liveRef.current.length === 0) return;
    // Capture the list as it is *now* — which equals live, because a frozen
    // list never reaches here twice (the guard `?? liveRef.current` keeps the
    // first snapshot for the whole interaction).
    setFrozen((prev) => prev ?? liveRef.current);
  }, []);

  /**
   * Whether focus is *actually* still inside the container right now.
   *
   * `focusInside` is set from `onFocusCapture`/`onBlurCapture`, but disabling
   * a focused element clears `document.activeElement` — to `<body>` — without
   * firing `blur`/`focusout` at all (verified against Chromium; the browser
   * drops focus as a side effect of the `disabled` attribute, not as a focus
   * transition). This queue is the textbook trigger: every decide button sets
   * its own `disabled` the instant it is clicked (the caller's in-flight
   * `deciding` state), and clicking focuses the button first. So the row the
   * operator just decided freezes on click, `onBlurCapture` never runs to
   * clear `focusInside`, and — because nothing else ever calls it again —
   * the queue stayed frozen permanently, surviving even the pointer leaving.
   * Re-deriving from the live DOM here, at every thaw check, closes that gap
   * without depending on an event that this specific case never sends. A
   * disabled active element is treated as outside too: jsdom retains focus on
   * it, while Chromium moves focus to `<body>`.
   */
  const focusInsideNow = useCallback(() => {
    const active = document.activeElement;
    if (active instanceof HTMLElement && active.matches(":disabled"))
      return false;
    return (
      active !== null &&
      containerRef.current !== null &&
      containerRef.current.contains(active)
    );
  }, []);

  const thawIfIdle = useCallback(() => {
    if (pointerInside.current) return;
    if (focusInsideNow()) return;
    focusInside.current = false;
    setFrozen(null);
    setHeld(false);
  }, [focusInsideNow]);

  const onPointerEnter = useCallback(() => {
    pointerInside.current = true;
    setHeld(true);
    freeze();
  }, [freeze]);

  const onPointerLeave = useCallback(() => {
    pointerInside.current = false;
    thawIfIdle();
  }, [thawIfIdle]);

  const onFocusCapture = useCallback(() => {
    focusInside.current = true;
    setHeld(true);
    freeze();
  }, [freeze]);

  const onBlurCapture = useCallback(
    (event: FocusEvent) => {
      // Focus moving between two cards inside the queue is not leaving it —
      // only a blur whose destination is outside the container thaws.
      if (event.currentTarget.contains(event.relatedTarget as Node | null))
        return;
      focusInside.current = false;
      thawIfIdle();
    },
    [thawIfIdle],
  );

  // Items arriving while the interaction is already underway must freeze now:
  // the operator is aiming at the (now nonempty) queue, and a poll that
  // reorders it is exactly the #1414 bug. The enter handler skipped earlier
  // because nothing was on screen to aim at then.
  useEffect(() => {
    if (live.length > 0 && (pointerInside.current || focusInsideNow())) {
      freeze();
    }
  }, [live, freeze, focusInsideNow]);

  // Disabling the focused decide button clears `document.activeElement` without
  // dispatching blur/focusout in Chromium. The parent render that disables it
  // is the one reliable observation point left, so check after every commit;
  // `thawIfIdle` still refuses to thaw while the pointer remains inside.
  useEffect(() => {
    thawIfIdle();
  });

  return {
    items: frozen ?? live,
    holding: held,
    containerProps: {
      onPointerEnter,
      onPointerLeave,
      onFocusCapture,
      onBlurCapture,
      ref: setContainerRef,
    },
  };
}
