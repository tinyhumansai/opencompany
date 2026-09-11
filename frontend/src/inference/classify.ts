// What to say about a failed provider check.
//
// Copy selection only. **The classifier itself is not here** — the host decides
// the class and sends it, because the raw upstream error is what the classifier
// reads and that string must not reach the console at all. Reimplementing the
// decision on this side would give us two classifiers to keep in step for the
// sake of a value the console is already told.
//
// The split is deliberate on the host side too (`inference::probe`): `classify`
// decides, `describe` says. This is the console's half of `describe`, and it
// exists as a plain function so the copy is selectable in a unit test rather
// than only reachable through a rendered banner.

import type { ProbeClass } from "./types";

/**
 * How a failed check should be presented.
 *
 * `error` is reserved for the one class that actually failed to save. Every
 * other class kept the record **and** the credential, so colouring it red would
 * be a lie about what happened: the save succeeded, and only reachability is in
 * question. Those are amber, and dismissible.
 */
export type AdvisoryTone = "error" | "warning";

/** What to show for a probe class. */
export interface ProbeAdvisory {
  tone: AdvisoryTone;
  /** One sentence. Never carries the upstream error string. */
  message: string;
  /** Whether the row was created at all. */
  rowCreated: boolean;
  /** Whether the credential was kept. */
  keyKept: boolean;
}

/**
 * Whether meeting this class should roll back the credential just written.
 *
 * Exactly one class says yes, and that is the point. The naive add flow rolls
 * everything back on any probe failure, and the naive flow **destroys valid
 * credentials**: a corporate proxy, a WAF, a rate limit and a mistyped model id
 * all fail a probe while the key is perfectly good.
 */
export function destroysCredential(probeClass: ProbeClass): boolean {
  return probeClass === "auth";
}

/**
 * The advisory for a probe class.
 *
 * `provider` is the label to name — for `auth` and `endpoint` the sentence is
 * about a specific thing and naming it is the difference between a dead end and
 * a next step. The other classes are about the account or the check, so they
 * name neither.
 *
 * **Nothing here interpolates the raw upstream string.** It can echo request
 * material — headers, fragments of a key — and this sentence lands in a banner
 * someone screenshots into a ticket. The raw text belongs in a detail or console
 * channel.
 */
export function describeProbe(probeClass: ProbeClass, provider: string): ProbeAdvisory {
  switch (probeClass) {
    case "auth":
      return {
        tone: "error",
        message: `Could not reach ${provider}: the provider rejected the credential.`,
        rowCreated: false,
        keyKept: false,
      };
    case "endpoint":
      return {
        tone: "warning",
        message: `Saved, but nothing answered at ${provider}.`,
        rowCreated: true,
        keyKept: true,
      };
    case "model":
      return {
        tone: "warning",
        message: "Saved. The endpoint did not recognise that model id.",
        rowCreated: true,
        keyKept: true,
      };
    case "quota":
      return {
        tone: "warning",
        message: "Saved. The account is out of credit.",
        rowCreated: true,
        keyKept: true,
      };
    case "timeout":
      return {
        tone: "warning",
        message: `Saved, but ${provider} did not answer in time.`,
        rowCreated: true,
        keyKept: true,
      };
    case "unknown":
      return {
        tone: "warning",
        message: "Saved, but the check did not complete.",
        rowCreated: true,
        keyKept: true,
      };
  }
}

/** The short state a connected row shows beside its endpoint. */
export function healthLabel(state: "ok" | ProbeClass): string {
  switch (state) {
    case "ok":
      return "ok";
    case "auth":
      return "key rejected";
    case "endpoint":
      return "unreachable";
    case "model":
      return "model not recognised";
    case "quota":
      return "out of credit";
    case "timeout":
      return "slow to answer";
    case "unknown":
      return "unchecked";
  }
}

/**
 * Whether "add anyway" should be offered.
 *
 * A provider that does not serve an OpenAI-shaped `{base}/models` listing is
 * still perfectly usable for inference, so blocking creation on the probe leaves
 * those operators unable to reach the model field at all.
 *
 * But it is gated on a **typed probe failure**, never on a boolean: a slug
 * collision or a failed key write must not unlock it, because neither is
 * evidence that the endpoint is fine. And callers clear it on every retry, so an
 * attempt that fails for an unrelated reason does not still offer to skip
 * verification.
 */
export function offersAddAnyway(failure: { kind: "probe"; probeClass: ProbeClass } | { kind: string }): boolean {
  return failure.kind === "probe";
}


/** How long a finished test result stays on the row before it clears itself. */
export const TEST_RESULT_MS = 10_000;

/** What a row shows while and after a test. */
export type TestState =
  | { kind: "idle" }
  | { kind: "testing" }
  | { kind: "done"; ok: boolean; message: string };

/**
 * What a finished test reads as on the row.
 *
 * A tone of its own rather than [`AdvisoryTone`]: that one has no success case,
 * deliberately — it describes what happened to a *save*, where the only
 * question is how bad the news is. A test can simply be good news.
 *
 * The message is the **host's**, chosen by the same `describe` this module
 * mirrors, so a 407 behind a corporate proxy reads differently from a rejected
 * key. That distinction is the whole reason the classifier exists, and
 * collapsing it to "failed" here would throw it away at the last step.
 *
 * Different tones as well as different words, because a result that clears
 * itself after ten seconds is glanced at rather than read.
 */
export function testOutcome(state: TestState): { tone: "ok" | "error"; message: string } | null {
  if (state.kind !== "done") return null;
  return { tone: state.ok ? "ok" : "error", message: state.message };
}

/**
 * What a finished provider check actually established.
 *
 * **The control has to answer the question it is asked.** The routing dialog's
 * Test promised "one real completion, your provider may charge for it" and sent
 * `GET /models` — so a row pinned to `this-model-does-not-exist` came back
 * "Reached the provider.", a true sentence about a question nobody asked. The
 * check is a catalogue read, it is free, and it is now said to be one; what it
 * can genuinely settle beyond reachability is whether the endpoint publishes the
 * id the row has chosen.
 *
 * A model the catalogue does not list is a **caution, not a failure**: an Azure
 * deployment name is never published by design, and a catalogue can be stale
 * anywhere. Reporting it as a failure would make the honest case unreachable.
 */
export function checkOutcome(result: {
  ok: boolean;
  message?: string;
  modelCount: number;
  modelKnown?: boolean;
}): { ok: boolean; message: string } {
  if (!result.ok) return { ok: false, message: result.message ?? "The check did not complete." };
  if (result.modelKnown === false) {
    return {
      ok: true,
      message:
        "Reached the provider, but it does not publish that model id. That is expected at an Azure deployment, and worth a second look anywhere else.",
    };
  }
  if (result.modelKnown === true) {
    return { ok: true, message: "Reached the provider, and it publishes that model." };
  }
  return { ok: true, message: "Reached the provider." };
}
