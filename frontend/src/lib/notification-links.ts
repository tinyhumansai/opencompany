// Where a notification row sends you, given the subject the host named.
//
// `NotificationDto` carries `subjectKind` (`task` / `run` / `approval` /
// `workflow` / `message`) and `subjectId` in that subject's own id space. Every
// one of those has a console address already; this module is the one place that
// knows which, so the Activity list does not grow a switch statement of its own
// and drift from the router.
//
// **`null` is a legitimate answer and not a failure.** The host's `kind` field
// is free-form by design (`src/server/ops/notifications.rs`: "No kind
// allowlist"), so a future producer can write a subject kind this console has
// never heard of. That row still renders — it has a `title`, which is the line
// a person actually reads — it just is not a link. Guessing an address for an
// unknown subject would send an operator somewhere that is not about their
// notification, which is worse than leaving the row inert.

import type { NotificationDto } from "@/api/types";
import { hostMessageId } from "@/lib/chat";
import { renderedChannelIdForContext } from "@/lib/mention-badge";

/**
 * The hash a row links to, or `null` when nothing in the console is about it.
 *
 * `message` and `workflow`'s `workflow_report` kind are the two that cannot be
 * answered from `subjectId` alone: a `message` id is a chat message id, and a
 * `workflow_report`'s `subjectId` is the workflow's own id, neither of which is
 * a channel — the console addresses *channels*. Both carry the channel in
 * `context` instead (the DM `report_to_operator` journals the report into),
 * resolved through [`renderedChannelIdForContext`] rather than used raw — the
 * same resolution the mention badge and the shell's thread re-read already
 * share, so a legacy general-chat spelling lands on the rendered main channel
 * here too instead of on a channel id that does not exist (issue #65). Every
 * other `workflow` notification still opens the workflows list.
 */
export function notificationHref(
  notification: NotificationDto,
  channels: {
    /** The channel ids the rail is actually rendering. */
    rendered: ReadonlySet<string>;
    /** The rendered main channel, for a legacy general-chat `context`. */
    mainChannelId: string | undefined;
  },
): string | null {
  const id = notification.subjectId;
  switch (notification.subjectKind) {
    case "task":
      // The card detail — the timeline, the plan brief, the attempts. Not the
      // board: a notification is about one card.
      return id ? `#/tasks/${encodeURIComponent(id)}` : null;
    case "run":
      // The observatory reads its run id off the second segment.
      return id ? `#/observatory/${encodeURIComponent(id)}` : null;
    case "approval":
      // Deliberately the queue rather than `#/approvals/<id>`: that segment
      // narrows on a **board task** id (issue #883), and an approval id is not
      // one. A narrowed queue matching nothing renders "this card is clear",
      // which would be a lie about the row that sent you there.
      return "#/approvals";
    case "workflow": {
      if (notification.kind === "workflow_report") {
        const channel = renderedChannelIdForContext(
          notification.context,
          channels.mainChannelId,
          channels.rendered,
        );
        return channel ? `#/chat/${channel}` : null;
      }
      return "#/workflows";
    }
    case "message": {
      const channel = renderedChannelIdForContext(
        notification.context,
        channels.mainChannelId,
        channels.rendered,
      );
      if (!channel) return null;
      // The line, not just the room. A mention's `subject.id` is the host's own
      // sequence for the message (`company/runtime.rs` writes
      // `message_seq.value()`), and `MessageRow` keys its rows on the *console*
      // id — `h`-prefixed, which is what `hostMessageId` makes. `RoomView`
      // consumes `?m=`, scrolls the row into view and strips the key; a row it
      // cannot find after a second of polling it simply gives up on, so the
      // worst case is the channel opening exactly as it did before. This is the
      // conversion `search/sources.ts` already does, for the same reason (Codex).
      const anchor = id ? `?m=${encodeURIComponent(hostMessageId(id))}` : "";
      // The channel segment is **unescaped**, unlike that query value, and the
      // difference is the point. `RoomView` reads `?m=` through
      // `URLSearchParams`, which decodes; the path segment reaches
      // `readSegments`, which splits the hash on `/` and decodes nothing —
      // `decodeURIComponent` appears nowhere on that path. A DM channel id is
      // `dm:<member-id>` (`room/channels.ts`), so encoding it turns the `:`
      // into `%3A` and yields a segment no channel matches: every DM mention
      // opened nothing. `channels.ts` documents the addressable form as
      // `#/chat/dm:ada-1f3k` for exactly this reason (tinysweeper).
      return `#/chat/${channel}${anchor}`;
    }
    default:
      return null;
  }
}

/** Newest first, the order the host documents its own feed in. */
export function byNewestFirst(
  notifications: readonly NotificationDto[],
): NotificationDto[] {
  // Copied rather than sorted in place: the array handed here is the shell's
  // own polled state, and sorting it would mutate React state behind its back.
  // The host already returns newest-first, so this is a guarantee rather than a
  // repair — but the list must not depend on a remote promise for its order.
  return [...notifications].sort((a, b) => b.createdAt - a.createdAt);
}
