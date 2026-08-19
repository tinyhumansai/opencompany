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
