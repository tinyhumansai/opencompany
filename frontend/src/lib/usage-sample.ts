// Sample usage analytics for the Usage page. The console has no metering API
// yet, so these are illustrative, deterministic series (seeded, so they don't
// jump on every render). Swap for a real `.../usage` feed when the host meters.

export interface DayUsage {
  /** ISO day, e.g. "2026-07-16". */
  date: string;
  inputTokens: number;
  outputTokens: number;
}

export interface AgentUsage {
  name: string;
  tokens: number;
}

export interface ProviderUsage {
  provider: string;
  calls: number;
}

export interface UsageData {
  series: DayUsage[];
  byAgent: AgentUsage[];
  byProvider: ProviderUsage[];
  totals: {
    inputTokens: number;
    outputTokens: number;
    tokens: number;
    costUsd: number;
    oauthCalls: number;
    connections: number;
  };
}

// Blended per-million-token prices, for an illustrative cost estimate.
const INPUT_PER_M = 3;
const OUTPUT_PER_M = 15;

function makeRng(seed: number): () => number {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 4294967296;
  };
}

function isoDay(offsetDays: number, todayMs: number): string {
  return new Date(todayMs - offsetDays * 86_400_000).toISOString().slice(0, 10);
}

/** Build `days` of usage ending today. `todayMs` is injected for stable output. */
export function buildUsage(days: number, todayMs: number): UsageData {
  const rng = makeRng(1337 + days);
  const series: DayUsage[] = [];
  for (let i = days - 1; i >= 0; i--) {
    // A gentle weekly rhythm + noise; weekends dip.
    const dow = new Date(todayMs - i * 86_400_000).getUTCDay();
    const weekend = dow === 0 || dow === 6 ? 0.55 : 1;
    const base = 90_000 * weekend * (0.75 + rng() * 0.5);
    const inputTokens = Math.round(base);
    const outputTokens = Math.round(base * (0.35 + rng() * 0.25));
    series.push({ date: isoDay(i, todayMs), inputTokens, outputTokens });
  }

  const inputTokens = series.reduce((a, d) => a + d.inputTokens, 0);
  const outputTokens = series.reduce((a, d) => a + d.outputTokens, 0);
  const tokens = inputTokens + outputTokens;
  const costUsd = (inputTokens * INPUT_PER_M + outputTokens * OUTPUT_PER_M) / 1_000_000;

  // Split the token total across desks by fixed weights (stable identity).
  const agentWeights: [string, number][] = [
    ["Strategy desk", 0.28],
    ["Creative studio", 0.34],
    ["Front desk", 0.14],
    ["Analyst", 0.16],
    ["Researcher", 0.08],
  ];
  const byAgent = agentWeights
    .map(([name, w]) => ({ name, tokens: Math.round(tokens * w) }))
    .sort((a, b) => b.tokens - a.tokens);

  // OAuth calls by provider, scaled to the window length.
  const providerBase: [string, number][] = [
    ["Gmail", 41],
    ["Slack", 33],
    ["Google Drive", 22],
    ["Notion", 18],
    ["Stripe", 7],
  ];
  const byProvider = providerBase
    .map(([provider, perDay]) => ({ provider, calls: Math.round(perDay * days * (0.85 + rng() * 0.3)) }))
    .sort((a, b) => b.calls - a.calls);

  const oauthCalls = byProvider.reduce((a, p) => a + p.calls, 0);

  return {
    series,
    byAgent,
    byProvider,
    totals: {
      inputTokens,
      outputTokens,
      tokens,
      costUsd,
      oauthCalls,
      connections: byProvider.length,
    },
  };
}

/** Compact token/number formatting: 1.2M, 340K, 5.1K. */
export function compact(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n >= 10_000_000 ? 0 : 1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(n >= 100_000 ? 0 : 1)}K`;
  return `${n}`;
}

export function usd(n: number): string {
  return n.toLocaleString(undefined, { style: "currency", currency: "USD", maximumFractionDigits: n < 100 ? 2 : 0 });
}
