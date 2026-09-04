export interface CostValue {
  amountUsd?: number;
  hidden?: boolean;
}

/**
 * Formats source-currency USD without ever making positive spend look free.
 * Timeline lines use four decimals; aggregate totals use two.
 */
export function formatUsdCost(
  cost: CostValue | undefined,
  precision: "line" | "total",
): string | null {
  if (!cost) return null;
  if (cost.hidden) return "Cost hidden";
  const amount = cost.amountUsd;
  if (amount === undefined || amount <= 0) return null;
  const floor = precision === "line" ? 0.0001 : 0.01;
  if (amount < floor) return `<$${floor.toFixed(precision === "line" ? 4 : 2)}`;
  const digits = precision === "line" ? 4 : 2;
  return amount.toLocaleString("en-US", {
    style: "currency",
    currency: "USD",
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
}

/**
 * One USD amount, for anywhere the console states a figure an operator will
 * read as money: a balance, a total, a budget, an approval's amount, a
 * transaction row.
 *
 * # Why this exists (issue B-016)
 *
 * Finance told a founder "Spend · US$0 this month" on the same screen that
 * listed that month's spend transactions — `brand_designer −US$0.08`,
 * `brand_designer −US$0.06`, `researcher −US$0.02` — and a Wallet balance of
 * −US$0.16. Nothing was wrong with the numbers. The Spend, Revenue and Net
 * tiles were formatted to whole dollars while the rows and the balance beside
 * them were formatted to cents, so every real amount under fifty cents read as
 * nothing. A founder was told they had spent nothing while they were being
 * billed.
 *
 * The tiles were not individually wrong — the formatter took the precision as
 * an argument, so "how many decimals does money have here?" was a question each
 * call site answered for itself, and one of them answered zero. This function
 * takes no such argument. There is one precision for a money figure in this
 * console and it is cents, because that is the precision the ledger keeps.
 *
 * # Sub-cent amounts
 *
 * `$0.00` is reserved for actually nothing. A charge of $0.004 is real money
 * and rounding it to `$0.00` is the same lie one order of magnitude down, so it
 * renders `<$0.01` — the convention {@link formatUsdCost} already uses, and for
 * the same reason.
 *
 * # Locale
 *
 * The operator's, not `en-US`: the tiles and rows this replaces already
 * rendered through the browser's locale (`US$0.16` for the founder who filed
 * this), and changing the currency's presentation is not what the bug asked
 * for. {@link formatUsdCost} pins `en-US` because a per-turn cost sits inside
 * English prose; a balance does not.
 */
/** The `toLocaleString` options `formatUsd` renders every amount through. */
const USD_TWO_DECIMALS: Intl.NumberFormatOptions = {
  style: "currency",
  currency: "USD",
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
};

export function formatUsd(amount: number): string {
  if (!Number.isFinite(amount)) return "—";
  // `-0` is what `revenue - spend` produces on a company that has neither, and
  // `Intl` renders it `-$0.00` — a minus sign in front of nothing, on the tile
  // whose whole job is saying whether the company is up or down. `+ 0`
  // normalises it; `-0 === 0` is true, so no comparison below can catch it.
  const value = amount === 0 ? 0 : amount;
  // `$0.00` is reserved for actually nothing. Half a cent still rounds to it,
  // which is this bug one order of magnitude down.
  if (value !== 0 && Math.abs(value) < 0.005) {
    // CodeRabbit review, PR #2054: rendered through the same locale-aware
    // formatter as every other amount below, rather than a hardcoded
    // "$0.01" — a `de-DE` or `fr-FR` operator reads the ordinary amounts on
    // this same tile in their own locale's decimal and currency placement,
    // and this threshold is the one figure here that did not follow.
    const threshold = (0.01).toLocaleString(undefined, USD_TWO_DECIMALS);
    return value < 0 ? `−<${threshold}` : `<${threshold}`;
  }
  return value.toLocaleString(undefined, USD_TWO_DECIMALS);
}
