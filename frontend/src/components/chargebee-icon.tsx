/**
 * Chargebee's symbol mark (lucide-react in this build ships no brand icons).
 *
 * The symbol, not the wordmark: it sits beside a label that already reads
 * "Chargebee", and the primary logo is a 730×110 lockup that would say the name
 * twice at seven times the width.
 *
 * `currentColor`, so the one place that names the colour is the call site's
 * `--brand-chargebee` token rather than five `fill` attributes here.
 */
export function ChargebeeIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 100 100" fill="currentColor" className={className} aria-hidden="true">
      <path d="M33.7401 50.0067L99.7959 34.2536V0H65.5424L33.7401 50.0067Z" />
      <path d="M1 49.4036C1 53.5014 1.49951 57.479 2.44303 61.2901L33.7365 50.0048L2.18403 38.6178C1.41626 42.0866 1 45.6942 1 49.3943V49.4036Z" />
      <path d="M12.6361 17.5588L33.7266 50.0086L43.4486 0.492188C31.1365 2.22198 20.286 8.49363 12.6361 17.5588V17.5588Z" />
      <path d="M33.7401 50.0022L99.7959 65.7461V99.9996H65.5424L33.7401 50.0022Z" />
      <path d="M12.6397 82.4438L33.7302 49.994L43.443 99.5012C31.1309 97.7714 20.2804 91.4998 12.6305 82.4345L12.6397 82.4438Z" />
    </svg>
  );
}
