// Sample finances for the Finances page. The console has no ledger API yet, so
// these are illustrative, deterministic figures. Swap for a real wallet/ledger
// feed when the host meters spend and revenue.

export interface Transaction {
  id: string;
  /** ISO day. */
  date: string;
  description: string;
  category: string;
  amountUsd: number;
  direction: "in" | "out";
}

export interface CategorySpend {
  category: string;
  amount: number;
}

export interface FinanceData {
  balanceUsd: number;
  budgetUsd: number;
  spentUsd: number;
  revenueUsd: number;
  netUsd: number;
  byCategory: CategorySpend[];
  transactions: Transaction[];
}

function isoDay(offsetDays: number, todayMs: number): string {
  return new Date(todayMs - offsetDays * 86_400_000).toISOString().slice(0, 10);
}

/** Build an illustrative month of finances ending today. */
export function buildFinance(todayMs: number): FinanceData {
  const byCategory: CategorySpend[] = [
    { category: "Model inference", amount: 612 },
    { category: "Paid ads", amount: 340 },
    { category: "Tools & APIs", amount: 128 },
    { category: "Subscriptions", amount: 96 },
    { category: "Design assets", amount: 54 },
  ].sort((a, b) => b.amount - a.amount);

  const spentUsd = byCategory.reduce((a, c) => a + c.amount, 0);
  const budgetUsd = 2000;
  const revenueUsd = 3450;
  const netUsd = revenueUsd - spentUsd;
  const balanceUsd = 8420.55;

  const transactions: Transaction[] = [
    tx("t1", 0, "Retainer — Acme Co.", "Revenue", 2500, "in", todayMs),
    tx("t2", 1, "Anthropic — inference", "Model inference", 214.32, "out", todayMs),
    tx("t3", 2, "Meta Ads — spring push", "Paid ads", 180.0, "out", todayMs),
    tx("t4", 3, "Skill sale — SEO audit", "Revenue", 250, "in", todayMs),
    tx("t5", 4, "Figma — team seats", "Subscriptions", 48.0, "out", todayMs),
    tx("t6", 6, "Google Ads — retargeting", "Paid ads", 160.0, "out", todayMs),
    tx("t7", 8, "Stock imagery", "Design assets", 29.0, "out", todayMs),
    tx("t8", 9, "Skill sale — landing page", "Revenue", 700, "in", todayMs),
  ];

  return { balanceUsd, budgetUsd, spentUsd, revenueUsd, netUsd, byCategory, transactions };
}

function tx(
  id: string,
  offset: number,
  description: string,
  category: string,
  amountUsd: number,
  direction: "in" | "out",
  todayMs: number,
): Transaction {
  return { id, date: isoDay(offset, todayMs), description, category, amountUsd, direction };
}

export function usd(n: number, maxFrac = 2): string {
  return n.toLocaleString(undefined, { style: "currency", currency: "USD", maximumFractionDigits: maxFrac });
}
