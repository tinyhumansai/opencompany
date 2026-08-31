import type { CognitionState } from "@/api/types";
import type { ChatMessage } from "@/lib/chat";
import type { Sender } from "./model";

/**
 * The marker that says a line was produced by the offline echo brain rather
 * than the teammate whose name and avatar it is rendered under (issue #1734).
 *
 * Lives in its own module because two surfaces render an author line: the
 * channel transcript (`MessageRow`) and the thread panel (`ThreadPanel`, which
 * has its own compact `Line`). A reply read inside a thread is exactly the same
 * false attribution as one read in the channel, and the first cut of this fix
 * marked only the first of them — so the marker, and the sentence behind it,
 * are one copy that both import rather than two that can drift.
 */

/**
 * The cognition states that mean this company's replies came from the echo
 * brain, or `null` when they did not — or when the host never said.
 *
 * `undefined` (an older host, or one that could not answer) is **unknown**, and
 * unknown is not an echo. Asserting one on a host that cannot be asked would be
 * the same unfounded claim this fix removes, pointed the other way.
 */
export function echoCause(cognition: CognitionState | null | undefined): CognitionState | null {
  return cognition && cognition !== "configured" ? cognition : null;
}

/**
 * Whether to mark this row, and with which cause — the single predicate both
 * author-line surfaces use.
 *
 * Two rows must never be marked, and they are easy to miss because both arrive
 * as `from: "company"`:
 *
 * 1. **The reader's own line.** They wrote it and know it; marking it would be
 *    the same misattribution pointed the other way.
 * 2. **Another signed-in person's line.** In a multi-user company `fromHistory`
 *    maps every `mine: false` message to `from: "company"`, so a collaborator's
 *    own words land on the company side beside the agent replies. Marking those
 *    would have the console tell one colleague that the echo brain wrote another
 *    colleague's message — a *fabricated* attribution, which is worse than the
 *    missing one this PR set out to fix (codex, PR #1740).
 *
 * The second is read off `byPerson`, which the host projects, and **not** off
 * `channel === "operator"`. That was the first attempt and it is a trap worth
 * naming: the offline echo brain names its own outbound channel `operator` too
 * (`brain::echo`), so the label matches a canned reply and a human's message
 * alike, and splitting on it suppressed the marker on exactly the replies this
 * feature exists for. A real browser against a live host caught it; no unit
 * test here would have.
 *
 * `undefined` — a host that predates the field — marks as before rather than
 * silently trusting a guess.
 *
 * `system` never reaches either call site: both short-circuit it to a centred
 * pill above.
 */
export function echoMarkerFor(
  message: Pick<ChatMessage, "byPerson">,
  sender: Sender,
  cognition: CognitionState | null | undefined,
): CognitionState | null {
  if (sender.kind === "you") return null;
  if (message.byPerson) return null;
  return echoCause(cognition);
}

/**
 * Why this line is not the named teammate's words, in the operator's terms.
 *
 * The two causes get different sentences because they have different remedies,
 * which is the whole reason the host reports a discriminated state instead of a
 * boolean. A tooltip that says "no model configured" on a host with no harness
 * contradicts the banner directly above it, which is telling that same operator
 * that no setting will help.
 */
function reason(author: string, cause: CognitionState): string {
  switch (cause) {
    case "unconfigured":
      return `${author} did not write this. The company has no model configured, so the offline echo brain answered instead.`;
    case "restart-required":
      return `${author} did not write this. A provider is configured but not live yet, so the offline echo brain answered instead.`;
    case "unavailable":
      return `${author} did not write this. No agent harness is available on this host, so the offline echo brain answered instead.`;
    default:
      // `undetermined`: the host cannot read its own inference configuration,
      // so it cannot say why. Naming a cause here would invent one.
      return `${author} did not write this. The company is on the offline echo brain, and the host could not read its inference configuration to say why.`;
  }
}

/**
 * The chip itself. Short, because it sits on every company-side row; the
 * sentence is the tooltip, following the `disabledReason` idiom `MessageRow`
 * uses — a label that just changes shape reads as a bug, so the reason is
 * available without leaving the row.
 */
export function EchoPlaceholder({ author, cause }: { author: string; cause: CognitionState }) {
  return (
    <span
      data-testid="chat-echo-placeholder"
      title={reason(author, cause)}
      className="shrink-0 rounded-full bg-muted px-1.5 py-px text-2xs font-medium text-muted-foreground"
    >
      Placeholder
    </span>
  );
}
