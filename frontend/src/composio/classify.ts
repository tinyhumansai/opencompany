// What to say about a failed Composio credential check.
//
// PURE, and **copy selection only**. The classifier itself is not here: the
// host decides the class and sends it, because the raw upstream error is what
// the classifier reads and that string must not reach the console at all.
// Reimplementing the decision on this side would give us two classifiers to
// keep in step for the sake of a value the console is already told.
//
// It exists as plain functions so the copy is selectable in a unit test rather
// than only reachable through a rendered banner.

import type { ComposioProbeClass } from "./types";

/**
 * How a failed check should be presented.
 *
 * `error` is reserved for the one class that actually failed to save. Every
 * other class **kept the key**, so colouring it red would be a lie about what
 * happened: the save succeeded and only reachability is in question. Those are
 * amber, and dismissible.
 */
export type ComposioAdvisoryTone = "error" | "warning";

/**
 * Whether meeting this class stored the key anyway.
 *
 * Exactly one class says no, and that is the point. The naive flow rolls the
 * credential back on any probe failure, and the naive flow **destroys valid
 * credentials**: a corporate proxy, a WAF and a rate limit all fail a probe
 * while the key is perfectly good.
 */
export function storesKey(probeClass: ComposioProbeClass): boolean {
  return probeClass !== "auth";
}

/** The tone a class is rendered in. Follows {@link storesKey} exactly. */
export function probeTone(
  probeClass: ComposioProbeClass,
): ComposioAdvisoryTone {
  return storesKey(probeClass) ? "warning" : "error";
}

/**
 * The console's own sentence for a probe class.
 *
 * **Nothing here interpolates the upstream error string**, and it structurally
 * cannot: this function takes no text. That is not squeamishness — the upstream
 * body can echo request headers or fragments of the key that was just written,
 * and this sentence lands in a banner an operator screenshots into a ticket.
 * The raw text belongs in a detail or console channel.
 *
 * The `auth` sentence says what was *not* done, because that is the half an
 * operator cannot see: a rejected write leaves the page exactly as it was, and
 * without the sentence there is no way to tell a refused save from one that
 * silently did nothing.
 */
export function probeCopy(probeClass: ComposioProbeClass): string {
  switch (probeClass) {
    case "auth":
      return "Composio rejected that credential. Nothing was stored.";
    case "endpoint":
      return "Saved, but nothing answered at Composio.";
    case "quota":
      return "Saved. The Composio account is out of credit.";
    case "timeout":
      return "Saved, but Composio did not answer in time.";
    case "unknown":
      return "Saved, but the check did not complete.";
  }
}

/**
 * The console's sentence for a probe class on a route that **stored nothing**.
 *
 * A second table rather than a reuse of {@link probeCopy}, and the reason is a
 * filed defect on the sibling LLM surface: its manual Test reuses the add-path
 * advisory copy, so five of its six classes open with "Saved" — a statement
 * about an event that did not happen. `POST …/composio/api-key/test` writes
 * nothing on any path, including `auth`, so none of these may claim a save.
 *
 * Only a fallback in practice — that route sends its own verdict copy, and
 * {@link verdictMessage} prefers it. This is what an older host, or a host that
 * classified without explaining, degrades to.
 *
 * Same class vocabulary as {@link probeCopy}; only the framing differs. A class
 * added to `ComposioProbeClass` needs a row in both, and the unit tests assert
 * that no sentence here says "Saved" and that the two tables never converge.
 */
export function verdictCopy(probeClass: ComposioProbeClass): string {
  switch (probeClass) {
    case "auth":
      return "Composio rejected this key. Check it at app.composio.dev.";
    case "endpoint":
      return "Nothing answered at Composio.";
    case "quota":
      return "The Composio account is out of credit, or is being rate-limited.";
    case "timeout":
      return "Composio did not answer in time.";
    case "unknown":
      return "The check did not complete.";
  }
}

/**
 * The sentence to show for a **check** — {@link verdictCopy}'s analogue of
 * {@link advisoryMessage}, and it applies the same rule for the same reason:
 * prefer the host's own sentence, except for `unknown`, whose text is by
 * definition the upstream string nobody classified.
 */
export function verdictMessage(
  probeClass: ComposioProbeClass | undefined,
  message: string | undefined,
): string {
  const cls = probeClass ?? "unknown";
  if (cls === "unknown") return verdictCopy("unknown");
  const own = message?.trim();
  return own && own.length > 0 ? own : verdictCopy(cls);
}

/**
 * The sentence to show for a probe result.
 *
 * Prefers the host's own `advisory` — it knows which toolkit, which endpoint
 * and which account, and the console does not — **except for `unknown`**, which
 * is the one class whose text is by definition the upstream string nobody
 * classified. There the console's own copy is used and the host's is dropped,
 * which is the same rule stated on {@link probeCopy} applied to the one place
 * it could otherwise be routed around.
 *
 * An absent class reads as `unknown` rather than as "no failure": a host that
 * sent an advisory without saying what kind has told us something went wrong
 * and nothing about whether its words are safe to print.
 */
export function advisoryMessage(
  probeClass: ComposioProbeClass | undefined,
  advisory: string | undefined,
): string {
  const cls = probeClass ?? "unknown";
  if (cls === "unknown") return probeCopy("unknown");
  const own = advisory?.trim();
  return own && own.length > 0 ? own : probeCopy(cls);
}

/**
 * What one attempt at storing a credential came back as.
 *
 * Two shapes and not a boolean, because the two differ in the only fact that
 * matters: `advisory` **kept the key** and `rejected` stored nothing. A single
 * "it failed" flag is what lets a page colour a successful save red, or offer a
 * retry for a write that already landed.
 */
export type ComposioSubmitOutcome =
  | {
      kind: "advisory";
      /** Absent when the host sent copy without saying what kind. */
      probeClass?: ComposioProbeClass;
      message: string;
    }
  | {
      kind: "rejected";
      /** The host's HTTP status, where there was one. */
      status?: number;
      message: string;
    };

/**
 * Whether "add anyway" should be offered after this attempt.
 *
 * Gated on **what actually happened**, never on a bare "it failed". Two kinds
 * of failure must not unlock it:
 *
 * - An **advisory**. The key was stored; there is nothing left to add, and a
 *   button offering to add it again would invite a second write of a credential
 *   that already landed.
 * - A **permission** refusal (401/403). The viewer may not write this
 *   credential at all, and skipping the check turns one refusal into two.
 *
 * What is left is a write the host refused on the strength of its own check —
 * including an `auth` class, because a Composio account behind a proxy that
 * rewrites 401s is exactly the operator who cannot otherwise get past a check
 * that is wrong about them. Callers clear the offer on every retry, so an
 * attempt that fails for an unrelated reason does not still offer to skip a
 * check.
 */
export function offersSkipVerify(
  outcome: ComposioSubmitOutcome | null,
): boolean {
  if (outcome?.kind !== "rejected") return false;
  return outcome.status !== 401 && outcome.status !== 403;
}
