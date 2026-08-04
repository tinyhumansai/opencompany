import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { setInboxEnabled } from "@/api/inbox";
import { ApiError } from "@/api/types";
import { type ChatMessage, makeMessage } from "@/lib/chat";
import { fromDto, newMember, starterTeam, type TeamMember } from "@/lib/team";
import { cn } from "@/lib/utils";
import { AddMemberDialog, type NewMemberFields } from "./chat/AddMemberDialog";
import { ChannelRail } from "./chat/ChannelRail";
import { ChatHeader } from "./chat/ChatHeader";
import { MembersPane } from "./chat/MembersPane";
import { MessageComposer } from "./chat/MessageComposer";
import { MessageTimeline } from "./chat/MessageTimeline";
import { ThreadPanel } from "./chat/ThreadPanel";
import {
  buildChannels,
  buildTimeline,
  channelTitle,
  dmChannelId,
  findChannel,
  toggleReaction,
} from "./chat/model";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** The hash's second segment — the channel id, e.g. `main` in `#/chat/main`. */
  sub: string | null;
  onNavigate: (channelId: string) => void;
  /** Called after a reply lands, so the shell can refresh approvals/status. */
  onReply?: () => void;
  /**
   * Append-only system lines raised elsewhere in the console (an approval
   * decision, say). They land in the main channel so the transcript still
   * records what happened outside it.
   */
  notices: string[];
}

/** Every channel's transcript, keyed by channel id. */
type Transcripts = Record<string, ChatMessage[]>;

const DEFAULT_CHANNEL = "main";

/**
 * The chat workspace.
 *
 * One screen replaces what used to be three: the Conversation page's thread
 * list, the Team page's roster, and the desks those two shared without ever
 * being connected. Here the desks are channels, every teammate has a DM, and
 * the roster sits in a pane you can open beside the transcript.
 *
 * Every channel posts to the same company chat endpoint — a channel scopes a
 * transcript and fixes the company side's identity, it is not a separate
 * backend. Threads and reactions are console-local for the same reason: the
 * host has no surface for either yet.
 */
export function ChatView({ client, company, sub, onNavigate, onReply, notices }: Props) {
  const [members, setMembers] = useState<TeamMember[]>([]);
  const [loadingTeam, setLoadingTeam] = useState(true);
  const [fromHost, setFromHost] = useState(false);
  const [transcripts, setTranscripts] = useState<Transcripts>({});
  const [sending, setSending] = useState(false);
  const [openThreadId, setOpenThreadId] = useState<string | null>(null);
  const [membersOpen, setMembersOpen] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  const [mobilePane, setMobilePane] = useState<"rail" | "chat">("chat");

  const boot = useCallback(async () => {
    try {
      const roster = await client.listTeam(company);
      if (roster.length) {
        setMembers(roster.map(fromDto));
        setFromHost(true);
      } else {
        setMembers(starterTeam());
        setFromHost(false);
      }
    } catch {
      // No roster surface on this host yet — start from an editable team.
      setMembers(starterTeam());
      setFromHost(false);
    } finally {
      setLoadingTeam(false);
    }
  }, [client, company]);

  useEffect(() => {
    setLoadingTeam(true);
    void boot();
  }, [boot]);

  // Drain by index rather than by identity: `notices` only ever grows, so the
  // count of what has already been folded in is the whole bookmark.
  const drained = useRef(0);
  useEffect(() => {
    if (notices.length <= drained.current) return;
    const fresh = notices.slice(drained.current);
    drained.current = notices.length;
    setTranscripts((t) => ({
      ...t,
      [DEFAULT_CHANNEL]: [
        ...(t[DEFAULT_CHANNEL] ?? []),
        ...fresh.map((line) => makeMessage("system", line)),
      ],
    }));
  }, [notices]);

  const sections = useMemo(() => buildChannels(members), [members]);
  const channel = findChannel(sections, sub) ?? findChannel(sections, DEFAULT_CHANNEL);

  const messages = channel ? (transcripts[channel.id] ?? []) : [];
  const entries = useMemo(
    () => (channel ? buildTimeline(messages, channel) : []),
    [messages, channel],
  );

  // An open thread only makes sense while its parent is on screen; switching
  // channels closes it rather than leaving a panel pointing at nothing.
  useEffect(() => {
    setOpenThreadId(null);
  }, [channel?.id]);

  if (!channel) return null;
  // A local the closures below can capture as non-null: TypeScript hoists
  // function declarations, so the guard above does not narrow inside them.
  const active = channel;

  const append = (channelId: string, ...added: ChatMessage[]) =>
    setTranscripts((t) => ({ ...t, [channelId]: [...(t[channelId] ?? []), ...added] }));

  /**
   * Post a line and thread the company's answer back into the same place.
   * `parentId` set means the exchange stays inside the thread panel.
   */
  async function send(text: string, parentId?: string) {
    if (sending) return;
    const target = active.id;
    append(target, makeMessage("you", text, { parentId }));
    setSending(true);
    try {
      const reply = await client.chat(text, company);
      const replies = reply.responses.length
        ? reply.responses.map((r) => makeMessage("company", r.text, { channel: r.channel, parentId }))
        : [makeMessage("system", "(no reply)", { parentId })];
      append(target, ...replies);
      onReply?.();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "something went wrong";
      append(target, makeMessage("system", `Couldn't send — ${msg}`, { parentId }));
    } finally {
      setSending(false);
    }
  }

  function react(messageId: string, emoji: string) {
    setTranscripts((t) => ({
      ...t,
      [active.id]: (t[active.id] ?? []).map((m) =>
        m.id === messageId ? { ...m, reactions: toggleReaction(m.reactions, emoji) } : m,
      ),
    }));
  }

  /**
   * Give a teammate an inbox, or take it away, on the host — keyed by the
   * roster **agent id**, which is the `InboxStore` key the Inbox page reads and
   * the ingest webhook files mail under. Nothing is persisted client-side: if
   * the write fails the switch goes back, so the console never claims an inbox
   * the host doesn't have (issue #173).
   *
   * Starter-roster rows are locally-invented placeholders, not host records, so
   * their ids are not real inbox keys — refuse rather than file mail under one.
   */
  async function toggleMemberInbox(member: TeamMember) {
    if (!fromHost) {
      toast.error("Add this teammate to your company first — an inbox needs a saved teammate.");
      return;
    }
    const next = !member.inboxEnabled;
    const apply = (enabled: boolean) =>
      setMembers((ms) => ms.map((m) => (m.id === member.id ? { ...m, inboxEnabled: enabled } : m)));
    apply(next);
    try {
      await setInboxEnabled(client, company, member.id, next);
    } catch (error) {
      apply(!next);
      toast.error(
        error instanceof ApiError && error.status === 404
          ? "This host doesn't offer teammate inboxes yet."
          : error instanceof Error
            ? error.message
            : "Couldn't change the inbox.",
      );
    }
  }

  function addMember(fields: NewMemberFields) {
    setMembers((m) => [...m, newMember(fields)]);
    // A locally-added teammate has no host record yet, so there is no agent id
    // to hang an inbox off — say so rather than silently dropping the request.
    if (fields.inbox) toast.error("Save this teammate on the host before giving them an inbox.");
    setAddOpen(false);
  }

  function selectChannel(id: string) {
    onNavigate(id);
    setMobilePane("chat");
  }

  const parent = openThreadId ? messages.find((m) => m.id === openThreadId) : undefined;
  const threadReplies = parent ? messages.filter((m) => m.parentId === parent.id) : [];

  return (
    <div className="flex min-h-0 flex-1">
      <ChannelRail
        sections={sections}
        activeId={channel.id}
        // Nothing arrives in a channel you are not looking at yet — every
        // reply answers a line you just sent. The rail renders unread counts
        // already, so this is the one seam to fill when the host starts
        // pushing messages of its own.
        unread={{}}
        onSelect={selectChannel}
        className={cn("md:flex", mobilePane === "rail" ? "flex" : "hidden")}
      />

      <div
        className={cn(
          "min-w-0 flex-1 flex-col",
          mobilePane === "chat" ? "flex" : "hidden md:flex",
        )}
      >
        <ChatHeader
          channel={channel}
          memberCount={members.length}
          membersOpen={membersOpen}
          onToggleMembers={() => setMembersOpen((o) => !o)}
          onOpenRail={() => setMobilePane("rail")}
        />

        <div className="flex min-h-0 flex-1">
          <div className="flex min-w-0 flex-1 flex-col">
            <MessageTimeline
              channel={channel}
              entries={entries}
              openThreadId={openThreadId}
              typing={sending && !openThreadId}
              onOpenThread={setOpenThreadId}
              onReact={react}
            />
            <MessageComposer
              placeholder={`Message ${channelTitle(channel)}`}
              disabled={sending}
              onSend={(text) => void send(text)}
            />
          </div>

          {parent && (
            <ThreadPanel
              channel={channel}
              parent={parent}
              replies={threadReplies}
              sending={sending}
              onSend={(text) => void send(text, parent.id)}
              onClose={() => setOpenThreadId(null)}
            />
          )}

          {membersOpen && (
            <MembersPane
              members={members}
              loading={loadingTeam}
              fromHost={fromHost}
              onToggleInbox={(m) => void toggleMemberInbox(m)}
              onRemove={(id) => setMembers((ms) => ms.filter((m) => m.id !== id))}
              onAdd={() => setAddOpen(true)}
              onMessage={(m) => selectChannel(dmChannelId(m))}
            />
          )}
        </div>
      </div>

      <AddMemberDialog open={addOpen} onOpenChange={setAddOpen} onAdd={addMember} />
    </div>
  );
}
