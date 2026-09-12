// What to say about a failed search-provider check.
//
// Copy selection only. **The classifier itself is not here** — the host decides
// the class and sends it, because the raw upstream error is what the classifier
// reads and that string must not reach the console at all: it can echo request
// material, including fragments of a key, and it lands in a banner somebody
// screenshots into a ticket.
//
// The split is deliberate on the host side too (`company::search::probe`):
// `classify` decides, `describe` says. This is the console's half of `describe`,
// and it exists as a plain function so the copy is selectable in a unit test
// rather than only reachable through a rendered banner.

import type { ConfirmTarget, ProbeClass } from "./types";

/** How a failed check should be presented. */
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
 * Whether meeting this class rolls back the credential just written.
 *
 * Exactly one class says yes, and that is the point. The naive add flow rolls
 * everything back on any probe failure, and the naive flow **destroys valid
 * credentials**: a corporate proxy, a WAF, a rate limit and a SearXNG instance
 * with JSON output turned off all fail a check while the credential — where
 * there is one at all — is perfectly good.
 */
export function destroysCredential(probeClass: ProbeClass): boolean {
  return probeClass === "auth";
}

/**
 * The advisory for a probe class.
 *
 * `provider` is the label to name — for `auth`, `endpoint` and `timeout` the
 * sentence is about a specific thing and naming it is the difference between a
 * dead end and a next step. The others are about the account or the check, so
 * they name neither.
 */
export function describeProbe(
  probeClass: ProbeClass,
  provider: string,
): ProbeAdvisory {
  switch (probeClass) {
    case "auth":
      return {
        tone: "error",
        message: `Could not reach ${provider}: the provider rejected the credential.`,
        rowCreated: false,
        keyKept: false,
      };
    case "format":
      return {
        tone: "warning",
        message:
          "Saved. The instance has JSON output turned off — add `json` to `search.formats` in its settings.yml.",
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
    case "endpoint":
      return {
        tone: "warning",
        message: `Saved, but nothing answered at ${provider}.`,
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
    // `unknown` and anything this console has never heard of. `ProbeClass` is a
    // compile-time union and the value is a string off the wire, so a host one
    // version ahead can send a seventh class — and without this the switch falls
    // out returning `undefined` against a signature that says it does not.
    // `SearchView` then reads `.tone` off it and throws inside the `try` that
    // wraps the save, so a save that SUCCEEDED reports an error toast.
    //
    // Falling through to `unknown` is the honest answer anyway: a class this
    // build cannot name is a check whose result it cannot interpret.
    case "unknown":
    default:
      return {
        tone: "warning",
        message: "Saved, but the check did not complete.",
        rowCreated: true,
        keyKept: true,
      };
  }
}

/** The short state a connected row shows when a check has failed. */
export function healthLabel(state: ProbeClass): string {
  switch (state) {
    case "auth":
      return "key rejected";
    case "format":
      return "JSON output off";
    case "quota":
      return "out of credit";
    case "endpoint":
      return "unreachable";
    case "timeout":
      return "slow to answer";
    // See `describeProbe`: the value is a string off the wire, so an unnamed
    // class must land somewhere rather than returning `undefined` into a
    // signature that promises a string.
    case "unknown":
    default:
      return "unchecked";
  }
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
 * A tone of its own rather than `AdvisoryTone`: that one has no success case,
 * deliberately — it describes what happened to a *save*, where the only question
 * is how bad the news is. A test can simply be good news.
 */
export function testOutcome(
  state: TestState,
): { tone: "ok" | "error"; message: string } | null {
  if (state.kind !== "done") return null;
  return { tone: state.ok ? "ok" : "error", message: state.message };
}

/**
 * What a test result says, given the class the host sent.
 *
 * `describeProbe`'s sentences are save-flavoured ("Saved, but …") and a test
 * changes nothing, so they would be wrong here. The distinction between the
 * classes is still the whole point — a 407 behind a corporate proxy must not
 * read like a rejected key — so this is a second table rather than a collapse
 * to "failed".
 */
export function describeTest(probeClass: ProbeClass, provider: string): string {
  switch (probeClass) {
    case "auth":
      return `${provider} rejected the credential.`;
    case "format":
      return "Reached it, but JSON output is turned off.";
    case "quota":
      return "The account is out of credit.";
    case "endpoint":
      return `Nothing answered at ${provider}.`;
    case "timeout":
      return `${provider} did not answer in time.`;
    // See `describeProbe`.
    case "unknown":
    default:
      return "The check did not complete.";
  }
}

/**
 * What a destructive confirmation asks.
 *
 * A function rather than three ternaries inside the dialog, so the wording is
 * unit-testable and so the component stays layout. It tolerates `null` because
 * the dialog stays mounted through its own close animation, and reading copy off
 * a target that has already been cleared would otherwise blank the text mid-fade.
 */
export function confirmCopy(target: ConfirmTarget | null): {
  title: string;
  body: string;
  action: string;
} {
  switch (target?.kind) {
    case "remove":
      return {
        title: `Remove ${target.label}?`,
        body: `Its stored key is cleared with it. Re-connecting later means pasting a new key — this one is never shown back, so it cannot be recovered from this page.`,
        action: "Remove",
      };
    case "remove-key":
      return {
        title: `Remove the ${target.label} key?`,
        body: `${target.label} stays in the list but stops answering searches, and teammates fall back to the included account. The key is never shown back, so it cannot be recovered from this page.`,
        action: "Remove key",
      };
    case "disconnect-all":
      return {
        title: "Disconnect every provider?",
        body: "Every connected provider is removed and every stored key is cleared. Teammates fall back to the included account, which is metered and capped.",
        action: "Disconnect all",
      };
    default:
      return { title: "", body: "", action: "Confirm" };
  }
}
