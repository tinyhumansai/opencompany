import { useCallback, useEffect, useMemo, useState, type Dispatch, type SetStateAction } from "react";
import { toast } from "sonner";

import { listPeople, me as fetchMe, type Person } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { setInboxEnabled } from "@/api/inbox";
import { ApiError, type TeamMemberDto } from "@/api/types";
import { type ChatMessage, makeMessage } from "@/lib/chat";
import { defaultDesks, type Desk } from "@/lib/desks";
import { fromDto, newMember, starterTeam, type TeamMember } from "@/lib/team";
import { cn } from "@/lib/utils";
import { AddMemberDialog, type NewMemberFields } from "./chat/AddMemberDialog";
import { BudgetDialog } from "./chat/BudgetDialog";
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
  DEFAULT_CHANNEL,
  deskFromDto,
  dmChannelId,
  findChannel,
  toggleReaction,
  type Transcripts,
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
   * Every channel's transcript, keyed by channel id, and its setter — owned by
   * `AppShell` rather than here so a transcript survives this component
   * unmounting when the operator navigates to another view and back (the shell
   * mounts and unmounts `ChatView` per route; component-local state would be
   * discarded on every trip away from Chat).
   */
  transcripts: Transcripts;
  setTranscripts: Dispatch<SetStateAction<Transcripts>>;
}

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
export function ChatView({
  client,
  company,
  sub,
  onNavigate,
  onReply,
  transcripts,
  setTranscripts,
}: Props) {
  const [members, setMembers] = useState<TeamMember[]>([]);
  const [loadingTeam, setLoadingTeam] = useState(true);
  const [fromHost, setFromHost] = useState(false);
  const [desks, setDesks] = useState<Desk[]>(defaultDesks());
  const [sending, setSending] = useState(false);
  const [openThreadId, setOpenThreadId] = useState<string | null>(null);
  const [membersOpen, setMembersOpen] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  const [mobilePane, setMobilePane] = useState<"rail" | "chat">("chat");
  const [isAdmin, setIsAdmin] = useState(false);
  // Who set which cap (issue #360, ported from the retired Team page). Only
  // an admin may read the user directory, so this stays empty for a member —
  // the attribution line degrades to "an admin" rather than disappearing.
  const [people, setPeople] = useState<Person[]>([]);
  // The member whose budget dialog is open, if any.
  const [budgetFor, setBudgetFor] = useState<TeamMember | null>(null);

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

  /**
   * Hiding the budget controls from a non-admin is **courtesy, not
   * enforcement**. The host refuses the write with a 403 whatever this says;
   * showing an operator a control they cannot use is the only thing this
   * prevents.
   */
  const loadViewer = useCallback(async () => {
    let admin = false;
    try {
      admin = (await fetchMe(client, company)).role === "admin";
    } catch {
      // No user plane on this host, or not signed in — treat as non-admin.
    }
    setIsAdmin(admin);
    if (!admin) {
      setPeople([]);
      return;
    }
    try {
      setPeople(await listPeople(client, company));
    } catch {
      // Attribution falls back to "an admin"; not worth a toast.
      setPeople([]);
    }
  }, [client, company]);

  useEffect(() => {
    setLoadingTeam(true);
    void boot();
    void loadViewer();
  }, [boot, loadViewer]);

  /** A human label for whoever set a cap — never a raw user id. */
  function whoSet(userId: string): string {
    const person = people.find((p) => p.id === userId);
    return person?.displayName?.trim() || person?.email || "an admin";
  }

  const budgetError = (error: unknown, fallback: string): string => {
    if (error instanceof ApiError) {
      if (error.status === 404) return "This host doesn't support console budgets yet.";
      return error.message;
    }
    return error instanceof Error ? error.message : fallback;
  };

  /**
   * Set, change, or remove a teammate's daily cap.
   *
   * `cap` is `null` to remove the cap and a number to set one — `0` included,
   * which caps the teammate at nothing. The two are different states on the
   * host and must stay different here, which is why this takes `number |
   * null` and never an optional.
   */
  async function applyBudget(member: TeamMember, cap: number | null) {
    try {
      const row = await client.setTeamBudget(member.id, cap, company);
      // Update the one card from the host's answer rather than refetching the
      // roster: the response IS the new state, so a refetch could only disagree.
      setMembers((ms) => ms.map((m) => (m.id === member.id ? { ...m, ...fromDto(row) } : m)));
      toast.success(cap === null ? "Daily cap removed." : `Daily cap set to $${cap.toFixed(2)}.`);
    } catch (error) {
      toast.error(budgetError(error, "Couldn't change the daily cap."));
    }
  }

  /** Drop the override so the company's own default applies again. */
  async function resetBudget(member: TeamMember) {
    try {
      const row = await client.clearTeamBudgetOverride(member.id, company);
      setMembers((ms) => ms.map((m) => (m.id === member.id ? { ...m, ...fromDto(row) } : m)));
      toast.success("Reset to the company default.");
    } catch (error) {
      toast.error(budgetError(error, "Couldn't reset the daily cap."));
    }
  }

  // The company's real desks, when the host exposes them — a company with its
  // own desks gets its own channels instead of the generic strategy/creative/
  // front-desk trio. Hosts without `.../desks` yet 404; the static defaults
  // still work then (the existing Conversation path has the same fallback).
  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const dtos = await client.listDesks(company);
        if (live) setDesks(dtos.length ? dtos.map(deskFromDto) : defaultDesks());
      } catch {
        if (live) setDesks(defaultDesks());
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  const sections = useMemo(() => buildChannels(members, desks), [members, desks]);
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
      // A real desk channel's id doubles as its thread id (`deskFromDto`), so
      // addressing by it routes to that desk's lead. A DM's id is
      // console-local (`dmChannelId`), not a host thread — but `chat` also
      // accepts a roster teammate id directly (`responder_for` in
      // `src/harness/brain.rs`), which is exactly what a DM's `member.id`
      // is, so a DM addresses that teammate the same way a desk addresses
      // its lead.
      const chatId = active.kind === "channel" ? active.id : active.member?.id;
      const reply = await client.chat(text, company, chatId);
      const replies = reply.responses.length
        ? reply.responses.map((r) =>
            makeMessage("company", r.text, { channel: r.channel, parentId, steps: r.steps, taskId: r.taskId }),
          )
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

  /**
   * Persist a new teammate through the host (issue #360's Team-page add path),
   * falling back to a local-only add for a host without the write plane yet —
   * the same 404 fallback `boot` uses for the roster read itself.
   */
  async function addMember(fields: NewMemberFields) {
    let created: TeamMemberDto | null = null;
    try {
      created = await client.addTeamMember(
        { name: fields.name, role: fields.role, description: fields.description || undefined },
        company,
      );
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        // No team write plane on this host — keep the add local-only.
        setMembers((m) => [...m, newMember(fields)]);
      } else {
        toast.error(error instanceof Error ? error.message : "Couldn't add teammate.");
        return;
      }
    }
    if (created) {
      const member = fromDto(created);
      setMembers((m) => [...m, member]);
      // A successful host add proves the write plane exists, even for a
      // company that opened on the starter roster (fromHost still false from
      // `boot`) — flip it so this and later actions (inbox, budget) target
      // the host instead of refusing on a now-stale local-only guard.
      setFromHost(true);
      // A host-backed add has a real agent id, so the inbox request can go
      // straight through rather than waiting for a second save.
      if (fields.inbox) {
        try {
          await setInboxEnabled(client, company, member.id, true);
          setMembers((ms) => ms.map((m) => (m.id === member.id ? { ...m, inboxEnabled: true } : m)));
        } catch {
          toast.error("Couldn't enable the inbox — add it from the member's actions menu.");
        }
      }
    } else if (fields.inbox) {
      // A locally-added teammate has no host record yet, so there is no agent
      // id to hang an inbox off — say so rather than silently dropping it.
      toast.error("Save this teammate on the host before giving them an inbox.");
    }
    setAddOpen(false);
  }

  /**
   * Drop a teammate from the roster through the host when it has a record of
   * them; a manifest teammate can't be removed (409) and a starter-roster row
   * has no host record at all, so both fall back to a local-only removal.
   */
  async function removeMember(member: TeamMember) {
    if (!fromHost) {
      setMembers((ms) => ms.filter((m) => m.id !== member.id));
      return;
    }
    try {
      await client.removeTeamMember(member.id, company);
      setMembers((ms) => ms.filter((m) => m.id !== member.id));
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) {
        toast.error("This teammate is defined in the company manifest and can't be removed here.");
      } else {
        toast.error(error instanceof Error ? error.message : "Couldn't remove teammate.");
      }
    }
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
              onRemove={(id) => {
                const member = members.find((m) => m.id === id);
                if (member) void removeMember(member);
              }}
              onAdd={() => setAddOpen(true)}
              onMessage={(m) => selectChannel(dmChannelId(m))}
              canEditBudget={isAdmin && fromHost}
              onEditBudget={setBudgetFor}
              onRemoveCap={(m) => void applyBudget(m, null)}
              onResetBudget={(m) => void resetBudget(m)}
              setByLabel={(m) => (m.budgetSetBy ? whoSet(m.budgetSetBy) : undefined)}
            />
          )}
        </div>
      </div>

      <AddMemberDialog open={addOpen} onOpenChange={setAddOpen} onAdd={(fields) => void addMember(fields)} />
      <BudgetDialog
        member={budgetFor}
        onOpenChange={(open) => {
          if (!open) setBudgetFor(null);
        }}
        onSave={(cap) => {
          const target = budgetFor;
          setBudgetFor(null);
          if (target) void applyBudget(target, cap);
        }}
      />
    </div>
  );
}
