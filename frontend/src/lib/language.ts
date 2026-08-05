// Prosumer-facing language. The spec's glossary is normative: product/UI text
// never exposes runtime internals ("agent graph", "tier", "dispatch", "cycle",
// "checkpoint", "A2A"). Everything a person sees goes through this layer.

import type { ApprovalSummary, FeedbackCategory } from "../api/types";

/** A company's lifecycle state, in plain language, with a status tone. */
export function lifecycle(state: string): { label: string; tone: "live" | "idle" | "stopped" } {
  switch (state) {
    case "running":
      return { label: "Live", tone: "live" };
    case "onboarding":
      return { label: "Setting up", tone: "idle" };
    case "drafted":
      return { label: "Draft", tone: "idle" };
    case "paused":
      return { label: "Paused", tone: "idle" };
    case "suspended":
      return { label: "Suspended", tone: "stopped" };
    case "archived":
      return { label: "Archived", tone: "stopped" };
    default:
      return { label: titleCase(state), tone: "idle" };
  }
}

/**
 * A parked effect kind → what the company wants to do, in plain language.
 *
 * Deliberately un-annotated so its keys stay literal: {@link EFFECT_DONE_LABELS}
 * is checked against them, which is what stops the two tables drifting.
 */
const EFFECT_LABELS = {
  "payment.send": "Send a payment",
  "subscription.start": "Start a subscription",
  "email.send": "Send an email",
  "dm.external": "Message someone new",
  "filing.submit": "Submit a filing",
  "contract.accept": "Accept a contract",
  "external.publish": "Publish something publicly",
  "website.deploy": "Deploy a website change",
  "handle.register": "Claim a public handle",
  "handle.renew": "Renew a public handle",
  "key.rotate": "Rotate its security key",
  // A tool call the company parked mid-conversation. The effect kind is the
  // tool's own name, which is a runtime internal — the glossary rule above says
  // an operator never sees one, so the gated tools get plain-language labels
  // rather than the title-cased fallback ("Composio Execute").
  composio_authorize: "Connect one of its accounts",
  composio_execute: "Act in one of its connected accounts",
  mcp_registry_tool_call: "Use a connected tool",
  media_generate_image: "Generate an image",
  media_generate_video: "Generate a video",
};

export function effectAction(kind: string): string {
  return labelFor(EFFECT_LABELS, kind) ?? titleCase(kind.replace(/[._]/g, " "));
}

/**
 * The same effects in the past tense (#351) — what a task ALREADY did.
 *
 * A separate table rather than a suffix rule on {@link EFFECT_LABELS}: every
 * entry there is phrased as a request the company is making ("Send a payment"),
 * and a retry warning has to state a fact ("Sent a payment"). Bending one into
 * the other mechanically produces sentences no editor would sign off on.
 *
 * `satisfies` makes the mirror exhaustive **both ways** — a kind added to
 * {@link EFFECT_LABELS} and not here is a typecheck failure rather than a
 * silently title-cased effect key, and a key here that is not a real effect kind
 * is one too. That is also why entries stay for kinds no path currently reaches
 * the retry dialog with: the table's contract is "every kind the glossary
 * names", not "every kind observed in a journal", and pruning to the latter
 * would break the moment a classification changed.
 */
const EFFECT_DONE_LABELS = {
  "payment.send": "Sent a payment",
  "subscription.start": "Started a subscription",
  "email.send": "Sent an email",
  "dm.external": "Messaged someone new",
  "filing.submit": "Submitted a filing",
  "contract.accept": "Accepted a contract",
  "external.publish": "Published something publicly",
  "website.deploy": "Deployed a website change",
  "handle.register": "Claimed a public handle",
  "handle.renew": "Renewed a public handle",
  "key.rotate": "Rotated its security key",
  composio_authorize: "Connected one of its accounts",
  composio_execute: "Acted in one of its connected accounts",
  mcp_registry_tool_call: "Used a connected tool",
  media_generate_image: "Generated an image",
  media_generate_video: "Generated a video",
} satisfies Record<keyof typeof EFFECT_LABELS, string>;

/**
 * What a company already did, in plain language, with the amount when there is
 * one: "Sent a payment of $2,400.00" (#351).
 *
 * An unmapped kind falls back to a generic sentence rather than to a
 * title-cased key. The kind of an effect projected from a tool call *is* the
 * tool's own name, so title-casing it puts a runtime internal
 * ("Slack Post Message") in front of an operator — which the glossary rule at
 * the top of this file forbids, and which reads as a system leak in exactly the
 * dialog that has to be trusted. Saying less is the better failure.
 */
export function effectDone(kind: string, amountUsd?: number | null): string {
  const action = labelFor(EFFECT_DONE_LABELS, kind);
  if (action) return amountUsd != null ? `${action} of ${money(amountUsd)}` : action;
  return amountUsd != null
    ? `Did something that cannot be undone, involving ${money(amountUsd)}`
    : "Did something that cannot be undone";
}

/**
 * Gated **tool** names → what using that tool means, in plain language (#372).
 *
 * Kept apart from {@link EFFECT_LABELS} on purpose. That table's entries are
 * business effects, which are self-describing at kind level: `payment.send`
 * without its amount still tells an operator what is about to happen. A tool
 * name does not — `shell` without its command is meaningless — so these labels
 * only ever appear *above* the payload block that says what the tool will do.
 * Keeping them out of `EFFECT_LABELS` also leaves the
 * `EFFECT_LABELS`/`EFFECT_DONE_LABELS` `satisfies` mirror undisturbed.
 */
const TOOL_LABELS: Readonly<Record<string, string>> = {
  shell: "Run a terminal command",
  glob: "Search files in its workspace",
};

/**
 * What an approval is asking for, in plain language (#372).
 *
 * Resolution order, and why the last two rungs differ:
 *
 * 1. the effect glossary — a business effect, phrased as it always was;
 * 2. {@link TOOL_LABELS} — a gated tool we have words for;
 * 3. an unmapped kind **with** an `agent` — it came from a tool call, so we can
 *    at least say a teammate wants to use a tool;
 * 4. an unmapped kind with no agent — a native effect nobody has named.
 *
 * The title-cased fallback in {@link effectAction} is never reached from here.
 * That fallback is what put "Glob" and "Shell" — raw runtime identifiers — in
 * front of an operator, which is the bug #372 opens with and which the glossary
 * rule at the top of this file forbids. `effectAction` itself is deliberately
 * left alone so no other surface shifts under this change.
 */
export function approvalAction(a: ApprovalSummary): string {
  return (
    labelFor(EFFECT_LABELS, a.kind) ??
    labelFor(TOOL_LABELS, a.kind) ??
    (a.agent ? "Use one of its tools" : "Do something that needs your sign-off")
  );
}

/**
 * What a tool does, in plain language, from its identifier alone (#374).
 *
 * The Standing permissions list has no approval to hand to
 * {@link approvalAction} — the card it came from was resolved and is gone — but
 * the glossary rule at the top of this file still applies: an operator must
 * never be shown `workspace_write` and asked to reason about it. Same first two
 * rungs as {@link approvalAction}, with a fallback that names a tool without
 * pretending to know which one.
 */
export function toolAction(kind: string): string {
  return labelFor(EFFECT_LABELS, kind) ?? labelFor(TOOL_LABELS, kind) ?? "Use one of its tools";
}

/** A one-line, human summary of what needs approval. */
export function approvalSummary(a: ApprovalSummary): string {
  const action = approvalAction(a);
  if (a.amount_usd != null) return `${action} — ${money(a.amount_usd)}`;
  return action;
}

/** One line of an approval's payload preview: a label and its value. */
export interface PayloadLine {
  label: string;
  value: string;
}

/**
 * The payload the host sent, as lines an operator can read (#372).
 *
 * The values are already redacted and bounded host-side, so this function's
 * only job is presentation. Two shapes:
 *
 * * a kind we know the argument names of (`shell`, `glob`) gets its meaningful
 *   arguments in a fixed, readable order — the command first, because that is
 *   the thing being consented to;
 * * anything else falls back to `key: value` over the payload's own top-level
 *   entries, which is still concrete and still safe.
 *
 * Nested objects and arrays are re-serialized as compact JSON rather than
 * dropped: an operator approving a structured call needs to see the structure.
 * Returns `[]` when there is nothing to show, which is also what an **old
 * host** produces (it omits `payload` entirely) — callers render the pre-#372
 * one-line card in that case.
 */
export function payloadLines(a: ApprovalSummary): PayloadLine[] {
  const payload = a.payload;
  if (payload == null) return [];
  if (typeof payload !== "object") return [{ label: "value", value: renderValue(payload) }];
  if (Array.isArray(payload)) return [{ label: "items", value: renderValue(payload) }];

  const entries = Object.entries(payload as Record<string, unknown>);
  if (entries.length === 0) return [];

  // Preferred ordering for the kinds whose argument names we know. Unlisted
  // arguments still follow — this promotes, it never hides.
  const preferred = PAYLOAD_KEY_ORDER[a.kind] ?? [];
  const rank = (key: string) => {
    const i = preferred.indexOf(key);
    return i === -1 ? preferred.length : i;
  };
  return entries
    .filter(([, value]) => value != null && value !== "")
    .sort(([a1], [b1]) => rank(a1) - rank(b1))
    .map(([label, value]) => ({ label, value: renderValue(value) }));
}

/** Which arguments lead the preview, per tool. Presentation only. */
const PAYLOAD_KEY_ORDER: Readonly<Record<string, string[]>> = {
  shell: ["command", "cwd", "timeout"],
  glob: ["pattern", "path"],
};

function renderValue(value: unknown): string {
  if (typeof value === "string") return value;
  return JSON.stringify(value) ?? String(value);
}

export function money(usd: number): string {
  return usd.toLocaleString(undefined, { style: "currency", currency: "USD" });
}

/** Feedback categories, phrased the way an operator would think about them. */
export const FEEDBACK_CATEGORIES: { value: FeedbackCategory; label: string }[] = [
  { value: "wrong-output", label: "This was wrong" },
  { value: "bug", label: "Something broke" },
  { value: "missing-capability", label: "It can't do something I need" },
  { value: "approval-friction", label: "It asks too much / too little" },
  { value: "template-gap", label: "The team is missing a role" },
  { value: "docs", label: "The docs are unclear" },
];

/** A short relative time like "2m ago", "3h ago", "just now". */
export function timeAgo(atMillis: number, now: number): string {
  const secs = Math.max(0, Math.floor((now - atMillis) / 1000));
  if (secs < 45) return "just now";
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

/**
 * Indexes a label table by a kind that arrives at runtime and may not be in it.
 *
 * The tables keep literal key types so they can be checked against each other;
 * this is the one place that widens them back to a lookup, so the widening is
 * deliberate rather than an annotation that quietly disables the check.
 */
function labelFor(table: Readonly<Record<string, string>>, kind: string): string | undefined {
  return table[kind];
}

function titleCase(s: string): string {
  return s.replace(/\w\S*/g, (w) => w.charAt(0).toUpperCase() + w.slice(1).toLowerCase());
}
