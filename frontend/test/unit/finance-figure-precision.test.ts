import { readdirSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { formatUsd } from "@/lib/cost";
import { money } from "@/lib/language";

/**
 * Issue B-016: Finance said "Spend · US$0 this month" on the same screen that
 * listed that month's spend.
 *
 * The transactions beside it, all dated 2 Sept: `brand_designer −US$0.08`,
 * `brand_designer −US$0.06`, `researcher −US$0.02`. The Wallet balance tile:
 * −US$0.16. The Spend tile: US$0, and Net: −US$0 in red.
 *
 * Nothing was wrong with the numbers. The Spend/Revenue/Net tiles formatted to
 * whole dollars while the rows and the balance beside them formatted to cents,
 * so every real amount under fifty cents read as nothing — and a founder was
 * told they had spent nothing while they were being billed.
 *
 * The cause is not any one tile. The formatter took its precision as an
 * argument, so "how many decimals does money have here?" was a question each
 * call site answered for itself, and three of them answered zero. `formatUsd`
 * takes no such argument, which is what makes this unrepeatable rather than
 * fixed three times.
 */

describe("a USD figure the operator reads as money", () => {
  it("renders the founder's spend as the amount it is, not as zero", () => {
    // The exact numbers off the screenshot.
    expect(formatUsd(0.16)).toBe("$0.16");
    expect(formatUsd(-0.16)).toBe("-$0.16");
    expect(formatUsd(0.08)).toBe("$0.08");
    expect(formatUsd(0.02)).toBe("$0.02");
  });

  it("uses the same precision for a tile as for the rows beneath it", () => {
    // The whole bug in one assertion: the Spend tile and a transaction row are
    // now the same call, so they cannot disagree.
    const spend = 0.16;
    expect(formatUsd(spend)).toBe(formatUsd(0.08 + 0.06 + 0.02));
  });

  it("keeps trailing cents on a round amount, so the column stays aligned", () => {
    expect(formatUsd(5)).toBe("$5.00");
    expect(formatUsd(1234.5)).toBe("$1,234.50");
  });

  it("says nothing only when there is nothing", () => {
    expect(formatUsd(0)).toBe("$0.00");
  });

  it("never renders a real charge as $0.00", () => {
    // The same lie one order of magnitude down. An inference turn genuinely
    // costs fractions of a cent, and the Observatory reports costs like $0.019.
    expect(formatUsd(0.004)).toBe("<$0.01");
    expect(formatUsd(-0.004)).toBe("−<$0.01");
    expect(formatUsd(0.0001)).toBe("<$0.01");
  });

  it("does not put a minus sign in front of nothing", () => {
    // `revenue - spend` on a company with neither is `-0`, and `Intl` renders
    // that `-$0.00` — on the tile whose job is saying whether you are up or
    // down.
    expect(formatUsd(-0)).toBe("$0.00");
    expect(formatUsd(0 - 0)).toBe("$0.00");
  });

  it("refuses to invent a figure it was not given", () => {
    expect(formatUsd(Number.NaN)).toBe("—");
    expect(formatUsd(Number.POSITIVE_INFINITY)).toBe("—");
  });
});

describe("the console has one USD formatter, not several", () => {
  it("routes an approval's amount through the same one", () => {
    // `money()` was the second independent formatter and carried the same
    // failure in miniature: an approval for less than a cent rendered `$0.00`,
    // asking an operator to authorise a payment the card called free.
    expect(money(0.004)).toBe(formatUsd(0.004));
    expect(money(0.004)).not.toBe("$0.00");
    expect(money(12.5)).toBe("$12.50");
  });

  /**
   * Every console source, not one named file.
   *
   * # Why this sweeps (defect B-074)
   *
   * The guard this replaces read `views/FinancesView.tsx` and nothing else, and
   * looked for `Intl`'s `style: "currency"` / `maximumFractionDigits`. Both
   * halves of that were too narrow, and the same defect was live in two files
   * it never opened while it passed:
   * `ObservatoryView` and `AttemptCard` rendered `${cost.toFixed(3)}` — a third
   * money precision, and one that prints `$0.000` for real sub-tenth-cent
   * spend, which is B-016 exactly one order of magnitude down. Nine more sites
   * across Team, Chat, Usage, the task plan and the autonomy dialog were
   * building money by hand the same way.
   *
   * A guard scoped to where a bug was found can only ever prove that bug was
   * fixed. This one is scoped to the property: **no console source formats
   * currency for itself.** It fails on a file that has not been written yet,
   * which is the only version of this test worth having.
   */
  const here = dirname(fileURLToPath(import.meta.url));
  const SRC = resolve(here, "../../src");

  /**
   * The two modules that are allowed to turn a number into money, and why.
   *
   * `lib/cost.ts` is the formatter this whole test is about.
   * `views/finance/money.ts` renders **integer minor units** in an arbitrary
   * currency (`minorUnitDigits` asks `Intl` how many decimals JPY or a dinar
   * has). It is a different question from "how precise is a USD figure", it
   * cannot express the sub-cent amount that caused either defect, and folding
   * it into `formatUsd` would hard-code two decimals for currencies that do not
   * have two.
   */
  const ALLOWED = new Set(["lib/cost.ts", "views/finance/money.ts"]);

  function sources(dir: string, prefix = ""): string[] {
    const out: string[] = [];
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const rel = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.isDirectory()) out.push(...sources(resolve(dir, entry.name), rel));
      else if (/\.tsx?$/.test(entry.name) && !ALLOWED.has(rel)) out.push(rel);
    }
    return out;
  }

  const files = sources(SRC);

  it("sweeps the whole console, not the file the last defect was found in", () => {
    // The guard is worthless if the walk silently finds nothing — an `include`
    // that selects no file passes, which is the failure mode issue #475 is
    // about one level up. Both defect sites must be in the swept set.
    expect(files.length).toBeGreaterThan(200);
    expect(files).toContain("views/observatory/ObservatoryView.tsx");
    expect(files).toContain("views/observatory/AttemptCard.tsx");
    expect(files).toContain("views/FinancesView.tsx");
  });

  it("leaves no view formatting currency through Intl for itself", () => {
    const offenders = files.filter((f) =>
      readFileSync(resolve(SRC, f), "utf8").includes('style: "currency"'),
    );
    expect(offenders).toEqual([]);
  });

  it("leaves no view gluing a dollar sign onto a number it formatted itself", () => {
    // `$${x}` in a template, and `${x}` sitting in JSX text after a literal
    // `$` — the two ways every hand-built money string in this console was
    // written. What is inside the braces does not matter: the moment a view
    // owns the `$`, it owns the precision, and that is the decision `formatUsd`
    // exists to take away from it.
    const offenders = files.filter((f) => /\$\$\{/.test(readFileSync(resolve(SRC, f), "utf8")));
    expect(offenders).toEqual([]);
  });

  it("leaves no view rounding a money value to a precision of its own", () => {
    // `toFixed` on a line that also names money. This is the shape the previous
    // guard could not see: `${summary.costUsd.toFixed(3)}` contains no `Intl`
    // options at all, so a check written against `maximumFractionDigits` read
    // it as clean. Matched on the money words rather than on `toFixed` alone,
    // because `toFixed` is the right tool for a duration, a byte count or an
    // SVG coordinate — and this console uses it for all three.
    const MONEY = /\b(usd|cost|spend|spent|budget|amount|price|revenue|cap)\w*\b/i;
    const offenders: string[] = [];
    for (const f of files) {
      const lines = readFileSync(resolve(SRC, f), "utf8").split("\n");
      lines.forEach((line, i) => {
        if (line.includes(".toFixed(") && MONEY.test(line)) offenders.push(`${f}:${i + 1}`);
      });
    }
    expect(offenders).toEqual([]);
  });

  it("gives no caller a way to ask for fewer decimals", () => {
    // `formatUsd(x, 0)` must not typecheck or quietly work — a second argument
    // is how the whole-dollar tiles came about. Called with one and ignoring
    // anything else is the property; the type signature is the enforcement.
    const cost = readFileSync(resolve(SRC, "lib/cost.ts"), "utf8");
    expect(cost).toMatch(/export function formatUsd\(amount: number\): string/);
  });
});
