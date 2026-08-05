import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type Dispatch,
  type ReactNode,
  type SetStateAction,
} from "react";
import { TriangleAlert } from "lucide-react";
import { toast } from "sonner";

import { listPeople, me as fetchMe, type Person } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { setInboxEnabled } from "@/api/inbox";
import { ApiError, type ApprovalSummary, type TeamMemberDto, type TurnStep, type Verdict } from "@/api/types";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { type ChatMessage, makeMessage } from "@/lib/chat";
import { defaultDesks, type Desk } from "@/lib/desks";
import { fromDto, newMember, starterTeam, type TeamMember } from "@/lib/team";
import { cn } from "@/lib/utils";
import { useAskerNames } from "@/components/approval-card";
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
  buildTimelineItems,
  channelMembers,
  channelTitle,
  deskFromDto,
  dmChannelId,
  findChannel,
  firstChannel,
  toggleReaction,
  type DecidedApproval,
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
  /**
   * Called around the awaited chat POST with the **host thread id** it was sent
   * on, so the shell can suppress the SSE echo of our own turn while it is in
   * flight. Without this bracket the shell's live injection and the awaited
   * reply below both render and the bubble doubles — the exact duplicate-bubble
   * race the Conversation surface already brackets against.
   */
  onSendStart?: (threadId: string) => void;
  onSendEnd?: (threadId: string) => void;
  /**
   * The in-flight tool timeline the shell folds out of the live turn frames,
   * keyed by **host thread id** — so this view has to resolve its channel to a
   * thread to read it (see `activeThreadId`). Covers turns this console never
   * started, which is most of what issue #367 is about.
   */
  liveStepsByThread?: Record<string, TurnStep[]>;
  /** Channel id → unread count, for the rail's badges. Owned by the shell. */
  unread?: Record<string, number>;
  /**
   * Reports the channel actually on screen — which the hash need not name,
   * since it may have been resolved by the first-channel fallback. The shell
   * clears that channel's unread count and remembers it as where an
   * unaddressed line belongs after this view is gone (issue #368).
   */
  onChannelViewed?: (channelId: string) => void;
  /**
   * Every approval currently awaiting the operator, straight off the shell's
   * feed, plus the host thread → channel map that places them (#379).
   *
   * Passed whole and filtered here rather than pre-filtered upstream, because
   * the filter needs the channel actually on screen — which this view resolves,
   * not the shell. An approval whose `thread` names no channel this company has
   * (a workflow delivery, a scheduler tick, anything parked before #379) simply
   * matches nothing and stays on the Approvals page, which is the whole
   * additive contract.
   */
  approvals?: ApprovalSummary[];
  chatChannelByThread?: Record<string, string>;
  /** Now, for a card's "waiting N minutes" line. */
  now?: number;
  /**
   * Decide an approval from inside the conversation. Owned by the shell so the
   * witnessed verdict survives this view unmounting — the operator can walk to
   * Approvals and back mid-turn.
   */
  onDecideApproval?: (approval: ApprovalSummary, verdict: Verdict) => void;
  /** The verdict each card is waiting on, and the ones already witnessed. */
  decidingApprovals?: ReadonlyMap<string, Verdict>;
  decidedApprovals?: Record<string, DecidedApproval>;
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
  onSendStart,
  onSendEnd,
  liveStepsByThread,
  unread,
  onChannelViewed,
  approvals,
  chatChannelByThread,
  now,
  onDecideApproval,
  decidingApprovals,
  decidedApprovals,
}: Props) {
  const [members, setMembers] = useState<TeamMember[]>([]);
  const [loadingTeam, setLoadingTeam] = useState(true);
  const [fromHost, setFromHost] = useState(false);
  /**
   * The company's channels — `null` until `/desks` has answered.
   *
   * Seeding this with `defaultDesks()` is what made every deep link flash
   * `#general`: the first render of every mount resolved the hash against the
   * fabricated `main`/`strategy`/`creative`/`frontdesk` set, then swapped under
   * the operator once the real desks landed (issue #370). `null` means "not
   * answered yet", so nothing resolves against a set the company doesn't have.
   */
  const [desks, setDesks] = useState<Desk[] | null>(null);
  /** Set when `/desks` failed for a reason that isn't "this host has none". */
  const [desksError, setDesksError] = useState<string | null>(null);
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

  // Only the newest load may write. Two loads can be in flight at once — a
  // company switch, or a Retry over a request that is merely slow rather than
  // dead — and a stale answer landing last would replace the current company's
  // channels with the previous one's.
  const desksRun = useRef(0);

  /**
   * The company's real desks, when the host exposes them — a company with its
   * own desks gets its own channels instead of the generic strategy/creative/
   * front-desk trio.
   *
   * Two outcomes are *not* failures and fall back to the static defaults: a
   * host with no `.../desks` route at all (404, the pre-#53 shape the
   * Conversation path also tolerates) and a host that answers with no desks.
   * Both mean "this company has no desks surface", which the defaults exist
   * for. Anything else — a 500, a timeout, an offline tab — is a genuine
   * failure, and pinning the fabricated desks on top of it is what made a
   * broken `/desks` permanently show `#general` while the URL claimed a real
   * desk (issue #370). Those surface as an error the operator can retry.
   */
  const loadDesks = useCallback(async () => {
    const run = ++desksRun.current;
    setDesks(null);
    setDesksError(null);
    try {
      const dtos = await client.listDesks(company);
      if (run !== desksRun.current) return;
      setDesks(dtos.length ? dtos.map(deskFromDto) : defaultDesks());
    } catch (error) {
      if (run !== desksRun.current) return;
      if (error instanceof ApiError && error.status === 404) {
        setDesks(defaultDesks());
        return;
      }
      setDesksError(
        error instanceof Error ? error.message : "Couldn't load this company's channels.",
      );
    }
  }, [client, company]);

  useEffect(() => {
    void loadDesks();
  }, [loadDesks]);

  // No channels exist until the host has answered. Resolving against a
  // half-built list is exactly the first-paint swap issue #370 describes.
  const sections = useMemo(
    () => (desks ? buildChannels(members, desks) : []),
    [members, desks],
  );
  // The hash's channel, else the first one that exists. There used to be a
  // literal "main" between the two — an id only the *fallback* desks carry, so
  // it matched nothing once a company's real desks loaded and matched the same
  // channel `firstChannel` returns when they hadn't. It never selected anything
  // the line below wouldn't; it only made "main" look like a real channel id
  // (issue #368).
  const channel = desks ? (findChannel(sections, sub) ?? firstChannel(sections)) : null;
  /**
   * The hash named a channel this company doesn't have, and the first-channel
   * fallback answered instead.
   *
   * Only meaningful once the desks are in: before that, *every* id looks
   * unknown. Derived rather than stored, so it clears itself the moment the
   * hash changes — there is no stale banner to dismiss.
   */
  const unknownChannel = desks && sub && !findChannel(sections, sub) ? sub : null;

  /**
   * Who is in the channel on screen — `null` when it names no membership, in
   * which case the pane falls back to the whole roster (issue #369).
   *
   * A desk's membership comes from the host. A DM's is the one teammate on the
   * other end: it has no `memberIds` (nothing in the model claims a DM has a
   * roster), so the two-person case is stated here rather than faked upstream.
   */
  const inChannel = useMemo(() => {
    if (!channel) return null;
    if (channel.kind === "dm") return channel.member ? [channel.member] : null;
    return channelMembers(channel, members);
  }, [channel, members]);

  const outsideChannel = useMemo(() => {
    if (!inChannel) return members;
    const inside = new Set(inChannel.map((m) => m.id));
    return members.filter((m) => !inside.has(m.id));
  }, [inChannel, members]);

  const messages = channel ? (transcripts[channel.id] ?? []) : [];
  const entries = useMemo(
    () => (channel ? buildTimeline(messages, channel) : []),
    [messages, channel],
  );

  /**
   * The approvals raised in the channel on screen (#379).
   *
   * **Derived, never appended.** The cards come from server state on every
   * render, so a pending one survives a reload — better than the transcripts it
   * sits among, which are still console-local (#364), and deliberately not
   * dependent on that being fixed.
   *
   * An approval with no `thread`, or one naming a thread this company has no
   * channel for, resolves to `null` and matches nothing. That is how a workflow
   * delivery or a scheduler tick stays Approvals-page-only.
   *
   * `desks` gates the derivation for the same reason it gates the channel list
   * (#393): before `/desks` answers, every thread id looks unknown, and placing
   * cards against a half-built channel set is the first-paint swap #370
   * describes one surface over.
   */
  const channelApprovals = useMemo(() => {
    if (!desks || !channel || !approvals?.length) return [];
    const byThread = chatChannelByThread ?? {};
    return approvals.filter((a) => a.thread && byThread[a.thread] === channel.id);
  }, [desks, channel, approvals, chatChannelByThread]);

  /**
   * Cards the operator has already decided, which the feed no longer carries.
   *
   * The host drops a resolved approval from `GET …/approvals` at once, so these
   * cannot be re-derived from `approvals` — they come from the shell's witnessed
   * map, which keeps the last-seen summary precisely so a decided card can
   * settle in place rather than blinking out of the thread.
   */
  const settledApprovals = useMemo(() => {
    if (!decidedApprovals || !channel || !desks) return [];
    const live = new Set(channelApprovals.map((a) => a.id));
    const byThread = chatChannelByThread ?? {};
    return Object.values(decidedApprovals)
      .map((d) => d.approval)
      .filter((a) => !live.has(a.id))
      .filter((a) => a.thread && byThread[a.thread] === channel.id);
  }, [decidedApprovals, channel, desks, channelApprovals, chatChannelByThread]);

  const askerNames = useAskerNames(client, company, channelApprovals);

  const items = useMemo(
    () =>
      buildTimelineItems(
        entries,
        [...channelApprovals, ...settledApprovals],
        decidedApprovals ?? {},
      ),
    [entries, channelApprovals, settledApprovals, decidedApprovals],
  );

  // An open thread only makes sense while its parent is on screen; switching
  // channels closes it rather than leaving a panel pointing at nothing.
  useEffect(() => {
    setOpenThreadId(null);
  }, [channel?.id]);

  // Whoever owns the unread counts needs to know what is actually being looked
  // at. Re-runs as the open channel's transcript grows, not only on a switch:
  // a reply that lands while you are reading the channel is read, and should
  // not leave a badge on the channel you are sitting in.
  useEffect(() => {
    if (channel) onChannelViewed?.(channel.id);
  }, [channel?.id, messages.length, onChannelViewed]);

  // Three ways to have no channel on screen, which used to be one blank pane.
  // Which one it is, is the whole point: "still loading" and "this company has
  // nothing" are different facts and only one of them is worth acting on.
  if (desksError) {
    return (
      <EmptyPane
        title="Couldn't load this company's channels"
        body={desksError}
        action={{ label: "Retry", onClick: () => void loadDesks() }}
      />
    );
  }
  if (!desks) return <LoadingPane />;
  if (!channel) {
    return (
      <EmptyPane
        title="No channels yet"
        body="This company has no desks and nobody on its roster, so there is nothing to talk to. Add a teammate and their direct message shows up here."
        action={{ label: "Add a teammate", onClick: () => setAddOpen(true) }}
        after={
          <AddMemberDialog
            open={addOpen}
            onOpenChange={setAddOpen}
            onAdd={(fields) => void addMember(fields)}
          />
        }
      />
    );
  }
  // A local the closures below can capture as non-null: TypeScript hoists
  // function declarations, so the guard above does not narrow inside them.
  const active = channel;
  // The host thread this channel is addressed on. A real desk channel's id
  // doubles as its thread id (`deskFromDto`), so addressing by it routes to
  // that desk's lead. A DM's id is console-local (`dmChannelId`), not a host
  // thread — but `chat` also accepts a roster teammate id directly
  // (`responder_for` in `src/harness/brain.rs`), which is exactly what a DM's
  // `member.id` is, so a DM addresses that teammate the same way a desk
  // addresses its lead. It is also the id every live turn frame carries.
  const activeThreadId = active.kind === "channel" ? active.id : active.member?.id;
  const liveSteps = activeThreadId ? liveStepsByThread?.[activeThreadId] : undefined;
  /**
   * The count beside the channel title.
   *
   * A DM is stated as 2 rather than derived: it is a two-person conversation,
   * but the operator has no roster row, so counting rows would say 1 and
   * inventing a "You" row to make the arithmetic work would be worse. A desk
   * counts its own members; a channel with no membership of its own still
   * counts the company, which is all it can honestly claim.
   */
  const headerCount = active.kind === "dm" ? 2 : (inChannel?.length ?? members.length);

  const append = (channelId: string, ...added: ChatMessage[]) =>
    setTranscripts((t) => ({ ...t, [channelId]: [...(t[channelId] ?? []), ...added] }));

  /**
   * Post a line and thread the company's answer back into the same place.
   * `parentId` set means the exchange stays inside the thread panel.
   */
  async function send(text: string, parentId?: string) {
    if (sending) return;
    const target = active.id;
    const chatId = activeThreadId;
    append(target, makeMessage("you", text, { parentId }));
    setSending(true);
    // Claim the thread for the duration of the POST. The backend journals an
    // `AgentReply` for our own turn too and pushes it over SSE mid-await, so
    // without this the shell injects that echo *and* the awaited reply lands
    // below — two bubbles for one turn.
    if (chatId) onSendStart?.(chatId);
    try {
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
      if (chatId) onSendEnd?.(chatId);
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
        unread={unread ?? {}}
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
          memberCount={headerCount}
          membersOpen={membersOpen}
          onToggleMembers={() => setMembersOpen((o) => !o)}
          onOpenRail={() => setMobilePane("rail")}
        />

        <div className="flex min-h-0 flex-1">
          <div className="flex min-w-0 flex-1 flex-col">
            {unknownChannel && (
              <p
                role="status"
                className="flex shrink-0 items-center gap-1.5 border-b bg-muted/50 px-3 py-1.5 text-xs text-muted-foreground"
              >
                <TriangleAlert className="size-3.5 shrink-0" aria-hidden />
                <span className="min-w-0 truncate">
                  <span className="font-medium text-foreground">#{unknownChannel}</span> isn&apos;t a
                  channel here — showing {channelTitle(active)} instead.
                </span>
              </p>
            )}
            <MessageTimeline
              channel={channel}
              items={items}
              openThreadId={openThreadId}
              typing={sending && !openThreadId}
              liveSteps={openThreadId ? undefined : liveSteps}
              onOpenThread={setOpenThreadId}
              onReact={react}
              now={now}
              askerNames={askerNames}
              decidingApprovals={decidingApprovals}
              onDecideApproval={onDecideApproval}
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
              channelMembers={inChannel}
              others={outsideChannel}
              leadId={active.kind === "channel" ? active.memberIds?.[0] : undefined}
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

/**
 * The pane while `/desks` is still out.
 *
 * Shaped like the workspace it is about to become — a header bar and message
 * rows — so the real channel does not arrive as a jump. It exists to say "not
 * yet", which is the one thing the old blank pane could not distinguish from
 * "never".
 */
function LoadingPane() {
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-13 shrink-0 items-center gap-2 border-b px-3">
        <Skeleton className="size-4 rounded" />
        <Skeleton className="h-4 w-32 rounded" />
      </div>
      <div className="flex-1 space-y-3 p-4">
        {Array.from({ length: 5 }).map((_, i) => (
          <Skeleton key={i} className="h-12 rounded-lg" />
        ))}
      </div>
      <span className="sr-only">Loading channels…</span>
    </div>
  );
}

/** A whole-pane state: a headline, a sentence, and at most one thing to do. */
function EmptyPane({
  title,
  body,
  action,
  after,
}: {
  title: string;
  body: string;
  action?: { label: string; onClick: () => void };
  /** Rendered alongside — a dialog the action opens, which needs to mount. */
  after?: ReactNode;
}) {
  return (
    <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 p-8 text-center">
      <div className="max-w-sm space-y-1.5">
        <h2 className="text-base font-semibold tracking-tight">{title}</h2>
        <p className="text-sm text-muted-foreground">{body}</p>
      </div>
      {action && (
        <Button variant="outline" size="sm" onClick={action.onClick}>
          {action.label}
        </Button>
      )}
      {after}
    </div>
  );
}
