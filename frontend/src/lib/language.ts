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

/** A parked effect kind → what the company wants to do, in plain language. */
const EFFECT_LABELS: Record<string, string> = {
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
};

export function effectAction(kind: string): string {
  return EFFECT_LABELS[kind] ?? titleCase(kind.replace(/[._]/g, " "));
}

/** A one-line, human summary of what needs approval. */
export function approvalSummary(a: ApprovalSummary): string {
  const action = effectAction(a.kind);
  if (a.amount_usd != null) return `${action} — ${money(a.amount_usd)}`;
  return action;
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

function titleCase(s: string): string {
  return s.replace(/\w\S*/g, (w) => w.charAt(0).toUpperCase() + w.slice(1).toLowerCase());
}
