// The shapes the inference surfaces pass around.
//
// Types only — no values, no behaviour. Kept apart from `routing.ts` and
// `classify.ts` so those stay readable as *decisions*, which is the whole reason
// they are separate from the components that render them.
//
// The credential rule, restated here because this is where someone would add a
// field to one of these: **no shape in this file carries a key.** The wire
// carries `keyConfigured: boolean` and nothing else. Four independent mechanisms
// keep credentials off the wire in this subsystem, and the easiest way to break
// all four at once is to put one on a record for convenience.
//
// `baseUrl` is the field that made that sentence briefly untrue. A URL can carry
// userinfo (`http://user:password@host/v1`), and this shape comes back from a
// `ScopedCompany` route every console reader can call. The host now refuses such
// an endpoint everywhere one can be set and redacts it everywhere one is said
// (`catalogue::endpoint_has_credentials` / `redact_endpoint`), so what arrives
// here is `http://***@host/v1` at worst — but a `baseUrl` is still a place a
// credential can hide, which is why it is called out rather than trusted.

/** How a provider expects its credential presented. */
export type AuthStyle = "bearer" | "anthropic" | "none";

/** Which of the three questions a provider answers. */
export type ProviderCategory = "cloud" | "local" | "cli";

/**
 * One configured way for this company to reach a model.
 *
 * `id` is identity and survives a rename; `slug` is the address an operator
 * reads and hand-edits in a routing entry. They are two fields because they
 * answer two questions.
 */
export interface Provider {
  /** Stable, opaque. Never shown. */
  id: string;
  /** Routing key. Unique per company. What a routing entry names. */
  slug: string;
  /** Display label. Never used in routing. */
  label: string;
  /** Provider kind — a catalogue slug, or a legacy manifest kind. */
  kind: string;
  /** Resolved OpenAI-compatible base URL. */
  baseUrl: string;
  /** Abstract tier → concrete model id. */
  models: Record<string, string>;
  /** Whether this is available for routing. Distinct from deleted. */
  enabled: boolean;
  /** Whether a credential is stored — **never the credential**. */
  keyConfigured: boolean;
  /**
   * Whether an **unset** workload goes through this one.
   *
   * The resolved answer rather than the raw marker: a company that has never
   * said which provider is its default reports its first enabled one here,
   * because that is what it has always resolved to. So a row can say "Default"
   * without the console knowing whether it was chosen or inherited — and the
   * operator sees the same answer either way.
   *
   * Optional because an older host does not send it.
   */
  isDefault?: boolean;
  /** The last thing the system learnt about reaching it, if anything. */
  health?: ProviderHealth;
}

/**
 * What the system last learnt about reaching a provider.
 *
 * Sourced from things that already happen — the add-time probe, the manual
 * Test, and the turn path's own 401 — rather than from a poller. A poller costs
 * a request per provider per interval across every company on the host, to learn
 * something the next real turn learns for free.
 */
export interface ProviderHealth {
  /** `ok`, or the probe class of the last failure. */
  state: "ok" | ProbeClass;
  /** When it was learnt, ISO-8601. */
  at: string;
}

/** What a failed check means. See `classify.ts` for the copy each one gets. */
export type ProbeClass = "auth" | "model" | "quota" | "endpoint" | "timeout" | "unknown";

/** A workload that owns a routing row. */
export type Workload = "chat" | "reasoning" | "agentic" | "vision";

/**
 * What one routing row points at.
 *
 * `managed` and `default` are different states on purpose: one is a choice, the
 * other is an absence. Collapsing them loses the ability to say "this row is
 * deliberately managed" as distinct from "this row was never set".
 */
export type ProviderRef =
  | { kind: "managed" }
  | { kind: "default" }
  | { kind: "cloud"; providerSlug: string; model?: string }
  | { kind: "local"; model?: string }
  | { kind: "claudeCode"; model?: string };

/** Workload → what it routes through. A workload absent from the map is unset. */
export type RoutingMap = Partial<Record<Workload, ProviderRef>>;

/** The three routing modes. Inferred from the map, never stored. */
export type RoutingMode = "managed" | "own" | "advanced";
