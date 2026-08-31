import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

/**
 * A card has the same padding on all four sides, and no consumer adds a second
 * one on top of the primitive's (issue #1765).
 *
 * # The defect this stops
 *
 * `components/ui/card.tsx` is symmetric by construction, from one token:
 *
 * ```
 * Card         gap-(--card-spacing)  py-(--card-spacing)  [--card-spacing:--spacing(4)]
 * CardHeader   px-(--card-spacing)
 * CardContent  px-(--card-spacing)
 * ```
 *
 * The parent owns the vertical, the children own the horizontal, and left
 * untouched every card has 4 on every side.
 *
 * A `CardContent` that also writes `py-4` does not *replace* that — it sits
 * inside a parent that already applied one, so the two stack and the rendered
 * card has 8 above and below against 4 either side. Forty-four `Card*` elements
 * had picked one up, twenty-two of them the exact `py-4` the parent had already
 * paid, and the console's cards visibly read as taller-than-wide padded.
 *
 * # Why it needs a test rather than a note in the file
 *
 * `<CardContent className="space-y-2 py-4">` is unimpeachable in review. The
 * reviewer sees a card, sees a sensible padding, and has no way to see that the
 * parent applied one four lines up in a different file. That is how there came
 * to be forty-four. A grep can see it; a reviewer cannot.
 *
 * Same argument, and the same shape, as `scripts/ci/assert-design-tokens.sh`.
 */

const SRC = new URL("../../src", import.meta.url).pathname;

/**
 * Padding classes each slot may not carry, and why.
 *
 * The rule differs between the parent and its children because the *mechanism*
 * differs:
 *
 * - On `Card`, `cn` is `twMerge`, so a `py-*` written here **replaces** the
 *   primitive's rather than stacking. It cannot produce the asymmetry — but it
 *   does move one axis away from `--card-spacing` while the children keep
 *   following it, which lands in the same place by another road. `p-*` is fine:
 *   it sets all four together, and it is the right call for a card whose
 *   children are raw elements rather than `Card*` slots, where the token drives
 *   no horizontal padding to agree with.
 * - On `CardHeader` / `CardContent` / `CardFooter`, any vertical padding
 *   **adds** to what the parent already applied. Including `p-4`, which sets a
 *   horizontal that merely matches the token and a vertical that doubles it.
 *
 * A zero is always allowed. `p-0` on a card holding a table or a divided list
 * is the correct full-bleed idiom — it removes padding rather than adding a
 * second one, and the rows inside carry their own.
 */
const FORBIDDEN: Record<string, RegExp> = {
  Card: /\b(py|pt|pb)-(?!0\b)[\w.[\]()-]+/,
  CardHeader: /\b(p|py|pt|pb)-(?!0\b)[\w.[\]()-]+/,
  CardContent: /\b(p|py|pt|pb)-(?!0\b)[\w.[\]()-]+/,
  CardFooter: /\b(py|pt|pb)-(?!0\b)[\w.[\]()-]+/,
};

/**
 * Elements allowed to keep one anyway.
 *
 * Empty on purpose. Every one of the forty-four was either redundant with the
 * primitive or expressible through `--card-spacing`, so there is no exception
 * to carry — and an allowlist with nothing in it is the honest record of that.
 *
 * If a card ever genuinely needs different spacing, the answer is
 * `[--card-spacing:--spacing(n)]` on the `Card`, which moves all four sides at
 * once. A row here means that was tried and could not work; say why.
 */
const ALLOWED: Record<string, { count: number; why: string }> = {};

/** Every `.tsx` under `src`, as paths relative to it. */
function sources(dir = SRC, prefix = ""): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory()) return sources(join(dir, entry.name), rel);
    return entry.name.endsWith(".tsx") ? [rel] : [];
  });
}

type Offence = { file: string; line: number; slot: string; classes: string };

/**
 * Every `Card*` opening tag whose className carries a forbidden padding.
 *
 * The tag pattern spans lines because a `Card*` with several props is written
 * over several — `TeamView`'s and `FinancesView`'s both were — and a
 * line-oriented scan reported a clean tree with those two still in it.
 */
function offences(file: string, source: string): Offence[] {
  const found: Offence[] = [];
  const tags = source.matchAll(/<(Card|CardHeader|CardContent|CardFooter)\b([^>]*?)\/?>/gs);
  for (const tag of tags) {
    const [, slot, attrs] = tag;
    const forbidden = FORBIDDEN[slot];
    for (const cls of attrs.matchAll(/className=(?:"([^"]*)"|\{`([^`]*)`\})/gs)) {
      const classes = cls[1] ?? cls[2] ?? "";
      if (forbidden.test(classes)) {
        found.push({
          file,
          line: source.slice(0, tag.index).split("\n").length,
          slot,
          classes: classes.replace(/\s+/g, " ").trim(),
        });
      }
    }
  }
  return found;
}

const SOURCES = new Map(sources().map((rel) => [rel, readFileSync(join(SRC, rel), "utf8")]));

describe("cards pad every side the same (#1765)", () => {
  it("finds files to check at all, so a broken glob cannot pass silently", () => {
    expect(SOURCES.size).toBeGreaterThan(50);
    expect([...SOURCES.keys()]).toContain("components/ui/card.tsx");
  });

  it("has the primitive still owning vertical padding from the one token", () => {
    // If this stops being true the rule below is enforcing nothing: the whole
    // argument is that the parent already paid, so a child that pays again
    // doubles it.
    const card = SOURCES.get("components/ui/card.tsx") ?? "";
    expect(card).toContain("py-(--card-spacing)");
    expect(card).toContain("[--card-spacing:--spacing(4)]");
    expect(card).toContain('data-slot="card-content"');
  });

  it("has no consumer stacking a second vertical padding on it", () => {
    const found = [...SOURCES].flatMap(([rel, src]) => offences(rel, src));
    const budget = (rel: string, slot: string) => ALLOWED[`${rel} ${slot}`]?.count ?? 0;

    const offenders = found
      .filter((o) => budget(o.file, o.slot) === 0)
      .map((o) => `${o.file}:${o.line}  <${o.slot} className="${o.classes}">`);

    expect(
      offenders,
      `A Card* must not set its own vertical padding — the primitive already ` +
        `applies it, so the two stack and the card ends up taller than it is wide.\n` +
        `Drop the class. If the card genuinely needs different spacing, set ` +
        `[--card-spacing:--spacing(n)] on the <Card>, which moves all four sides ` +
        `together; size="sm" is the ready-made denser one.\n` +
        `${offenders.join("\n")}`,
    ).toEqual([]);
  });

  it("keeps p-0 usable, so the full-bleed idiom is not collateral damage", () => {
    // A card holding a divided list or a table zeroes its content padding and
    // lets the rows own it. That removes padding rather than adding a second,
    // so it is not what this guards — and a rule that broke it would be worse
    // than the drift.
    const feedback = SOURCES.get("views/FeedbackView.tsx") ?? "";
    expect(feedback).toContain('<CardContent className="p-0">');
    expect(offences("views/FeedbackView.tsx", feedback)).toEqual([]);
  });
});
