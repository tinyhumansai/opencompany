import { Button } from "@/components/ui/button";
import type { ChatMessage } from "@/lib/chat";
import {
  isBudgetPauseNotice,
  isBudgetPauseNoticeSuperseded,
  parseBudgetPauseAgent,
} from "@/hooks/use-events";

/**
 * The budget-pause notice's highlighted card with its "Add credits & resend"
 * CTA (issue #1846) — extracted from `MessageRow`'s `SystemPill` (issue #1846
 * review, Codex #3870168372) so `ThreadPanel` can render the SAME card for a
 * notice that answered a thread reply, instead of the plain system-message
 * text it fell back to.
 *
 * A budget-pause notice is journaled with the thread's `parent` set when the
 * exhausted turn was answering a reply, and `buildTimelineItems` (per
 * `model.ts`) routes any message with a `parentId` out of the main channel
 * timeline and into that thread. Before this, `MessageTimeline`/`MessageRow`
 * were the only place a notice's CTA ever rendered — so a thread-parented
 * notice was visible ONLY in `ThreadPanel`, which rendered every system line
 * (including this one) as a bare `<p>`, offering no way to redeem it at all.
 *
 * Returns `null` for a message that is not this notice, so a caller can
 * unconditionally try this component first and fall back to its own
 * plain-system-line rendering when it renders nothing.
 */
export function BudgetPauseNoticeCard({
  message,
  onRedeemBudgetPause,
  redeemingBudgetPauseAgent,
  latestBudgetPauseMessageIdByAgent,
}: {
  message: ChatMessage;
  /**
   * Carries `message.id` alongside the agent id (issue #1846 review, Codex
   * #3868962374) — see `ChatView.redeemBudgetPause`'s doc for why a live
   * re-read at click time cannot bind to the specific marker this card was
   * rendered from on its own.
   */
  onRedeemBudgetPause?: (agentId: string, noticeMessageId: string) => void;
  redeemingBudgetPauseAgent?: string | null;
  latestBudgetPauseMessageIdByAgent?: Map<string, string>;
}) {
  if (!isBudgetPauseNotice(message.text)) return null;

  const agentId = parseBudgetPauseAgent(message.text);
  const redeeming = agentId != null && redeemingBudgetPauseAgent === agentId;
  // Issue #1846 review (Codex #3864988184): the backend parks at most one
  // marker per agent, so a notice that is not the CURRENT one for its agent
  // must not offer a live CTA — clicking it would redeem whatever pause is
  // parked now, not the one this card shows. Disabled rather than hidden: a
  // card whose button silently vanished reads as a bug, not as "this one is
  // superseded".
  const superseded = isBudgetPauseNoticeSuperseded(
    agentId,
    message.id,
    latestBudgetPauseMessageIdByAgent,
  );

  return (
    <div className="flex justify-center px-4 py-1.5">
      <div className="flex max-w-lg flex-col gap-1.5 rounded-lg border border-status-blocked/30 bg-status-blocked-soft px-3.5 py-2.5 text-xs text-status-blocked-text">
        <p className="leading-5">{message.text}</p>
        {agentId != null && onRedeemBudgetPause && (
          <Button
            size="sm"
            variant="outline"
            className="w-fit border-status-blocked/40 bg-transparent text-xs hover:bg-status-blocked-soft"
            disabled={redeeming || superseded}
            title={
              superseded ? "A newer pause replaced this one — see the latest notice." : undefined
            }
            onClick={() => onRedeemBudgetPause(agentId, message.id)}
          >
            {redeeming
              ? "Resending…"
              : superseded
                ? "Superseded by a newer pause"
                : "Add credits & resend"}
          </Button>
        )}
      </div>
    </div>
  );
}
