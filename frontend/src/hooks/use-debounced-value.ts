import { useEffect, useState } from "react";

/**
 * The value, but only after it has held still for `delayMs`.
 *
 * Every change restarts the window, so a burst of rapid updates — a search box
 * taking keystrokes — collapses to a single settled value once typing pauses.
 * Bind the input to the raw value for instant echo and feed this debounced copy
 * to whatever is expensive (a network read), so the request fires once per
 * pause rather than once per keystroke.
 *
 * The value itself is debounced, not a callback: the consumers that must stay
 * immediate (an explicit refresh after a mutation) keep reading the raw source
 * and are untouched.
 */
export function useDebouncedValue<T>(value: T, delayMs: number): T {
  const [debounced, setDebounced] = useState(value);

  useEffect(() => {
    const id = setTimeout(() => setDebounced(value), delayMs);
    return () => clearTimeout(id);
  }, [value, delayMs]);

  return debounced;
}
