// One agent's whole session (the "one agent, one session" change).
//
// Every other tab on this page describes what a teammate *is* — its
// instructions, its toolbelt, its model — and `AgentRuns` says what it has
// *done*. This one says what it has **said and heard**: your DMs with it, its
// lines on every desk it sits on, the private asides it was party to, the
// questions it put to another desk, and the tool calls behind each answer, in
// the order it experienced them.
//
// # Why this is one stream and not a channel picker
//
// Because that is now what the agent itself has. Its in-memory history used to
// be cleared and re-seeded on every channel switch, so "the agent's session"
// was not a thing that existed — there were as many transcripts as there were
// desks, and no vantage point from which the teammate was one continuous
// participant. Splitting this view back into channels would be showing the old
// architecture on top of the new one.
//
// # Where the channel labels come from
//
// The host, not here. `GET {scope}/agents/{id}/session` stamps each row with
// the channel it was said on, resolved through the *same* function that decides
// which channels reach the agent's own context. A console-side merge of
// per-desk `chat/history` calls would be a second opinion about what a teammate
// can see, and the two would drift.
//
// # The operator sees more than the agent does
//
// Deliberately. Asides are narrowed for a peer agent and never for a person —
// privacy there is a deliberation device, not a security boundary — so this
// page shows every one in full. See `docs/spec/runtime/hivemind-asides.md`.

import { useCallback, useEffect, useRef, useState } from "react";
import { Braces, Loader2, MessageSquare, MessagesSquare } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { AgentSessionMessageDto } from "@/api/types";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useHashFlag } from "@/hooks/use-hash-flag";
import { fromHistory, type ChatMessage } from "@/lib/chat";
import { cn } from "@/lib/utils";
import { RawTurns } from "@/views/room/RawTurns";
import {
  AsideConversation,
  ReferralConversation,
  StepTimeline,
} from "@/views/room/StepTimeline";

/** How many lines one page of the session carries. */
const SESSION_PAGE = 200;

/** A line plus the channel it was said on. */
interface SessionLine {
  message: ChatMessage;
  channel: string;
  channelId: string;
  /**
   * The host's own row, kept beside the mapped message because the raw view
   * renders **this** and not `message`.
   *
   * `fromHistory` is a rendering decision: it resolves `from` against the
   * viewer, prefixes ids, and lifts referrals and asides onto the bubble. All
   * of that is exactly what somebody asking for the raw turns is asking to see
   * past. Rendering the raw view from the mapped shape would make it a second
   * opinion about the transcript rather than the transcript.
   */
  row: AgentSessionMessageDto;
}

type Load = "loading" | "ready" | "unsupported" | "error";

export function AgentSession({
  client,
  company,
  agentId,
  agentName,
}: {
  client: OpenCompanyClient;
  company: string | null;
  agentId: string;
  agentName: string;
}) {
  const [lines, setLines] = useState<SessionLine[]>([]);
  const [load, setLoad] = useState<Load>("loading");
  /**
   * Whether to show the turns as the agent received them rather than as chat.
   *
   * An address (`?tab=session&raw`), not local state, for the reason the Edit
   * form is one: the raw view is the thing you send to somebody else. "Look at
   * what it actually saw" is a link, and a link that lands on the rendered
   * bubbles and asks the reader to find a switch has lost the point of being
   * sent. `useHashFlag` also makes Back close it, which is the behaviour a
   * reader who opened it out of curiosity expects.
   */
  const [raw, setRaw] = useHashFlag("raw");
  // Incremented whenever the fetch effect restarts. A read that started before
  // a teammate switch captures the old value and discards its answer, so rows
  // fetched for one teammate can never be committed beneath another's name —
  // the same guard `AgentRuns` carries, for the same reason (issue #1671).
  const generationRef = useRef(0);

  const read = useCallback(async () => {
    const generation = ++generationRef.current;
    setLoad("loading");
    try {
      const rows: AgentSessionMessageDto[] = await client.agentSession(agentId, company, {
        limit: SESSION_PAGE,
      });
      if (generation !== generationRef.current) return;
      // `fromHistory` is the room's own mapping, reused whole: it is what
      // prefixes host ids, resolves `from`, and carries `referralConversation`
      // and `asideConversation` through untouched. Mapping these rows by hand
      // would be a second answer to "what is a chat line" that would drift from
      // the room's.
      //
      // Called once per row rather than once for the whole array (tinysweeper
      // review): `fromHistory` happens to be a 1:1, order-preserving `.map`
      // today, so correlating its output back to `rows` by array index was
      // correct — but that correlation held only by accident of the current
      // implementation, and a later filter or reorder inside `fromHistory`
      // would silently misattribute a `channel`/`channelId` to the wrong
      // message with no type error to catch it. Mapping each row through its
      // own `fromHistory([row])` call ties every message to its row
      // structurally instead of positionally.
      setLines(
        rows.map((row) => {
          const [message] = fromHistory([row]);
          return {
            message,
            channel: row.sessionChannel ?? "",
            channelId: row.sessionChannelId ?? "",
            row,
          };
        }),
      );
      setLoad("ready");
    } catch (error) {
      if (generation !== generationRef.current) return;
      // A host that predates the route is not an error to shout about — it is a
      // host without this surface, and saying so is more useful than a red box
      // that implies something broke.
      const status = (error as { status?: number } | null)?.status;
      setLoad(status === 404 ? "unsupported" : "error");
    }
  }, [client, company, agentId]);

  useEffect(() => {
    void read();
  }, [read]);

  if (load === "loading") {
    return (
      <Card>
        <CardContent className="flex items-center gap-2 text-sm text-muted-foreground">
          <Loader2 className="size-4 animate-spin" aria-hidden />
          Reading {agentName}&apos;s session…
        </CardContent>
      </Card>
    );
  }

  if (load === "unsupported") {
    return (
      <Card>
        <CardContent className="text-sm text-muted-foreground">
          This host does not keep a per-agent session yet, so there is nothing to
          show here. Each desk&apos;s own transcript is still on the Room page.
        </CardContent>
      </Card>
    );
  }

  if (load === "error") {
    return (
      <Card>
        <CardContent className="text-sm text-muted-foreground">
          {agentName}&apos;s session could not be read. It is still there — this
          is a failed request, not an empty history.
        </CardContent>
      </Card>
    );
  }

  if (lines.length === 0) {
    return (
      <Card>
        <CardContent className="flex items-center gap-2 text-sm text-muted-foreground">
          <MessageSquare className="size-4 shrink-0" aria-hidden />
          {agentName} has not said or heard anything yet.
        </CardContent>
      </Card>
    );
  }

  return (
    <Card>
      <CardContent className="space-y-4">
        <div className="flex items-center justify-between gap-3">
          <p className="text-xs text-muted-foreground">
            {raw
              ? "Every turn as the agent received it, in order, with each tool call unfolded."
              : "Everything this teammate has said and heard, across every channel it can read."}
          </p>
          <ViewToggle raw={raw} onChange={setRaw} />
        </div>
        {raw ? (
          // Channel badges on, unlike the DM: this stream interleaves rows from
          // every desk the teammate sits on, and without the badge two
          // teammates answering in two places are indistinguishable.
          <RawTurns
            rows={lines.map((line) => line.row)}
            agentId={agentId}
            showChannel
          />
        ) : (
          <ol className="space-y-4" data-testid="agent-session">
            {lines.map((line) => (
              <SessionRow key={line.message.id} line={line} agentId={agentId} />
            ))}
          </ol>
        )}
      </CardContent>
    </Card>
  );
}

/**
 * Chat or raw turns.
 *
 * Two buttons rather than a `Switch`, in the idiom the workflow index already
 * uses for Cards/List: a switch names one state and leaves the operator to
 * infer the other, and "Raw" is not a thing that is obviously on or off.
 */
function ViewToggle({
  raw,
  onChange,
}: {
  raw: boolean;
  onChange: (on: boolean) => void;
}) {
  return (
    <div className="flex shrink-0 items-center gap-1 rounded-lg border p-0.5">
      {(
        [
          { value: false, label: "Chat", Icon: MessagesSquare, id: "chat" },
          { value: true, label: "Raw turns", Icon: Braces, id: "raw" },
        ] as const
      ).map(({ value, label, Icon, id }) => (
        <Button
          key={id}
          size="sm"
          variant={raw === value ? "secondary" : "ghost"}
          className="h-7 px-2"
          onClick={() => onChange(value)}
          aria-pressed={raw === value}
          data-testid={`agent-session-view-${id}`}
        >
          <Icon className="mr-1.5 size-3.5" aria-hidden />
          {label}
        </Button>
      ))}
    </div>
  );
}

/** One line of the session, badged with where it was said. */
function SessionRow({ line, agentId }: { line: SessionLine; agentId: string }) {
  const { message, channel } = line;
  // Who is speaking, from the reader's point of view. `from` is resolved
  // host-side against the *viewer*, so "you" here means the operator reading
  // the page — which is why the agent's own lines are matched by author rather
  // than by `from`.
  const mine = message.from === "you";
  // CodeRabbit: `fromHistory` sets a system row's `from` to `"system"` and
  // carries no `channel` for it, so the fallback below (`message.channel ??
  // agentId`) landed on `agentId` — a structural marker (a dispatch
  // terminal, say) then read as if the teammate itself had said it.
  const author =
    mine ? "You" : message.from === "system" ? "System" : (message.channel ?? agentId);

  return (
    <li className="flex gap-3" data-testid="agent-session-row">
      <TeammateAvatar
        name={author}
        className="mt-0.5 size-6 shrink-0"
        markOnly
      />
      <div className="flex min-w-0 flex-1 flex-col gap-1">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs font-semibold">{author}</span>
          {channel && (
            <Badge
              variant="outline"
              className={cn("text-2xs font-normal text-muted-foreground")}
              data-testid="agent-session-channel"
            >
              {channel}
            </Badge>
          )}
        </div>
        <p className="text-sm leading-relaxed whitespace-pre-wrap">{message.text}</p>
        {/* The room's own collapses, reused rather than reimplemented — an
            agent-to-agent exchange has to read the same way here as it does in
            the channel it happened in, or the two surfaces disagree about what
            was said. */}
        {!!message.steps?.length && <StepTimeline steps={message.steps} />}
        {message.referralConversation && (
          <ReferralConversation crossing={message.referralConversation} />
        )}
        {message.asideConversation && (
          <AsideConversation aside={message.asideConversation} />
        )}
      </div>
    </li>
  );
}
