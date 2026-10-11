import { useCallback, useRef } from "react";

/**
 * Whether a keydown belongs to an IME composition being confirmed, rather
 * than to the operator.
 *
 * Three signals, because no one of them holds across engines:
 *
 * - `isComposing` — true on the confirming keydown in Chrome and Firefox.
 * - `keyCode === 229` — browsers mark keydowns handled by the IME "Process"
 *   with 229; some engines report the confirming Enter that way instead.
 * - a compositionend within the last burst — Safari dispatches the completing
 *   Enter *after* compositionend, with `isComposing` already false and a
 *   normal keyCode (WebKit bug 165004; the ordering is now called out in
 *   MDN's compat data). The only signal that Enter is the composition's is
 *   that the composition ended an instant ago. The window costs one swallowed
 *   Enter only if the operator presses Enter within the window of a
 *   composition they ended some other way, which is the standard trade.
 */
export function useImeEnter(burstMs = 100) {
  const compositionEndAt = useRef(0);

  const onCompositionEnd = useCallback(() => {
    compositionEndAt.current = Date.now();
  }, []);

  const confirmsComposition = useCallback(
    (event: React.KeyboardEvent) => {
      const native = event.nativeEvent;
      return (
        native.isComposing ||
        native.keyCode === 229 ||
        Date.now() - compositionEndAt.current < burstMs
      );
    },
    [burstMs],
  );

  return { onCompositionEnd, confirmsComposition };
}
