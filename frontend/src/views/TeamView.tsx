import { useCallback, useEffect, useRef, useState } from "react";
import {
  Cpu,
  MessageSquare,
  MoreHorizontal,
  Network,
  Plus,
  Sparkles,
  UserPlus,
  Users,
} from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { listTasks } from "@/api/tasks";
import { ApiError, type TeamMemberDto } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Badge } from "@/components/ui/badge";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { withHostParam } from "@/hooks/use-host-route";
import { fetchBoardColumns } from "@/lib/board-columns";
import { shouldPromptSetup } from "@/lib/company-setup";
import {
  addMemberFailure,
  addOutcome,
  reportAddMember,
  type MissedStep,
} from "@/lib/member-feedback";
import { fromDto, modelSummary, newMember, roleSubtitle, type TeamMember } from "@/lib/team";
import { workloadByAssignee, type Workload } from "@/lib/team-workload";
import { usd } from "@/lib/money";
import { cn } from "@/lib/utils";
import { dmChannelId } from "@/views/room/channels";
import { AgentDetailView } from "@/views/team/AgentDetailView";
import { AddMemberDialog, type NewMemberFields } from "@/views/room/AddMemberDialog";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * The agent id in the hash (`#/team/<agentId>`), when one is addressed. The
   * detail view is a sub-page rather than a modal so an operator can link to an
   * agent, refresh onto it, and use Back (issue #264).
   */
  sub: string | null;
  /**
   * Open an agent, or return to the roster with `null`.
   *
   * `edit` lands on `#/team/<id>?edit` — the detail page with its edit form
   * already open (issue #1989). That flag is not a convenience: the reduced
   * Add-teammate dialog collects a name and a sentence and nothing else, and
   * the copilot that fills in the rest lives inside that form. Landing beside
   * it rather than on the read-only profile is what makes the reduction a
   * handoff instead of a subtraction.
   */
  onOpenAgent: (agentId: string | null, options?: { edit?: boolean }) => void;
  /**
   * Bumped when first-run setup staffs the company, so this view re-reads a
   * roster that now has people on it (`docs/spec/runtime/company-setup.md`).
   */
  refreshKey?: number;
  /**
   * Reopen first-run setup. Rendered as an in-place prompt while the company has
   * nobody on it, so skipping the dialog is not a dead end.
   */
  onRunSetup?: () => void;
  /**
   * Go to the org chart — desks, seats, leads (issue #1193).
   *
   * The one way there from here, and a named destination rather than half of a
   * toggle: the chart is not another rendering of this roster, it is the only
   * surface that can create a desk or move somebody between two. Optional, so
   * this view still stands alone.
   */
  onManageDesks?: () => void;
  /**
   * Open a single desk from a card's desk chip. Same destination as the chart's
   * own desk links (`#/company/<deskId>`), and optional so the card stays inert
   * — desk chips render as text, not buttons — when the shell does not wire it.
   */
  onNavigateToDesk?: (deskId: string) => void;
}

type Load = "loading" | "ready";

/** The company's agents — showcased and operator-definable. */
export function TeamView({
  client,
  company,
  sub,
  onOpenAgent,
  refreshKey,
  onRunSetup,
  onManageDesks,
  onNavigateToDesk,
}: Props) {
  const [load, setLoad] = useState<Load>("loading");
  const [fromHost, setFromHost] = useState(false);
  /**
   * The host answered the roster read **and** nobody has staffed this company
   * (`docs/spec/runtime/company-setup.md`).
   *
   * Distinct from `!fromHost`, which also covers a host with no `…/team` surface
   * at all. Only the first case is a company waiting to be set up; offering
   * setup on the second would open a dialog whose first call 404s.
   *
   * Also distinct from "the host answered with nobody", which is what this used
   * to mean and is a state no company can be in: the global baseline puts
   * undeletable teammates on every roster (issue #1404). `shouldPromptSetup`
   * discounts those, so this is `true` on a company that has the baseline and
   * nothing else — which is exactly the company that needs the prompt.
   */
  const [hostEmpty, setHostEmpty] = useState(false);
  const [members, setMembers] = useState<TeamMember[]>([]);
  /**
   * Ids of rows this console appended itself, because the host has no team
   * write plane (`addMember`'s 404 branch below).
   *
   * `fromHost` cannot answer this. It is one flag for the whole roster, set by
   * the *read*, and a host that serves `GET …/team` and 404s the `POST` leaves
   * it true while a console-only row sits on the grid — so both of the card's
   * host-addressed controls would offer to open something no host holds. See
   * {@link hostBackedCard}.
   *
   * Emptied by every re-read: `boot` replaces the roster wholesale, so a marker
   * that outlived its row would suppress the controls on a real teammate who
   * happens to be minted at the same id.
   */
  const [consoleOnly, setConsoleOnly] = useState<ReadonlySet<string>>(NO_CONSOLE_ONLY);
  const [nameQuery, setNameQuery] = useState("");
  const [workingOnly, setWorkingOnly] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  /**
   * Open cards and running state per teammate (issue #1141), or `null` while
   * nothing has been read and for a host that cannot answer.
   *
   * `null` and an empty map are the same *rendering* — no dot, no count — and
   * that is the point: the alternative was a `0` on every card, which claims
   * every teammate is free on a host that never said so. See `lib/team-workload.ts`.
   */
  const [workload, setWorkload] = useState<Map<string, Workload> | null>(null);
  /**
   * A monotonic run id for the workload read. The effect below bumps it on every
   * re-read, and `loadWorkload` only commits a result whose run is still
   * current. Clearing `workload` alone is not enough: a superseded read still in
   * flight can resolve *after* a newer one and repopulate the state with a map
   * the roster no longer describes.
   */
  const workloadRun = useRef(0);


  /**
   * Re-read the roster. Answers whether it landed.
   *
   * The catch below is right to show nobody, and wrong to stay silent about
   * after a write we know the host took: a failed read here does not leave a
   * stale list, it empties the one the operator is about to be congratulated
   * over. `addMember` is the only caller that looks at the answer; the effects
   * and the Back handler still fire and forget.
   */
  const boot = useCallback(async (): Promise<boolean> => {
    try {
      const roster = await client.listTeam(company);
      // Every row the host holds is rendered, baseline teammates included: they
      // are real agents an operator can open, brief and cap. The setup prompt is
      // gated on a *different* question — whether anyone has been staffed here —
      // so the two are read separately rather than one inferred from the other
      // (issue #1404).
      setHostEmpty(shouldPromptSetup(roster));
      if (roster.length) {
        setMembers(roster.map(fromDto));
        setFromHost(true);
      } else {
        // NOT `starterTeam()`. The host answered, and answered with nobody — so
        // fabricating twelve agents here would put "Ops Lead", "Front Desk" and
        // ten more on screen that do not exist on the host, directly under a
        // prompt saying the company has no team. An honest empty state plus the
        // setup offer is the whole point of the flow
        // (`docs/spec/runtime/company-setup.md`).
        setMembers([]);
        setFromHost(false);
      }
      return true;
    } catch {
      // The roster read failed, so we never learned who is on this company.
      // Show nobody rather than a fabricated team: an operator cannot tell an
      // invented roster from a real one, and every action on a fake row fails.
      // NOT `hostEmpty` — that means "the host answered, with nobody".
      setMembers([]);
      setFromHost(false);
      setHostEmpty(false);
      return false;
    } finally {
      // Every branch above replaced the roster from the host — with its rows,
      // with nobody, or with nobody because the read failed. None of them can
      // still hold a row this console appended, so the markers go with them.
      setConsoleOnly(NO_CONSOLE_ONLY);
      setLoad("ready");
    }
  }, [client, company]);

  /**
   * The board, read for what it says about the people rather than the cards.
   *
   * Two reads, both best-effort and neither of them blocking: the roster is the
   * page, and a host with no `…/tasks` route — or a network that dropped — must
   * still render every teammate. Both failures land on `null`, which draws no
   * status line at all rather than a fabricated "idle · 0 open".
   *
   * The columns come with it because "open" is the host's word, not this
   * console's: `closed` is declared per column on the `tasks` ledger.
   */
  const loadWorkload = useCallback(async () => {
    if (!company) {
      setWorkload(null);
      return;
    }
    const run = workloadRun.current;
    const [tasks, columns] = await Promise.all([
      listTasks(client, company).catch(() => null),
      fetchBoardColumns(client, company).catch(() => null),
    ]);
    // Superseded: a newer read started while this one was in flight (the effect
    // re-ran on a `refreshKey` change, say), so this map must not overwrite the
    // newer read's answer — one read's board cannot determine another's roster.
    if (run !== workloadRun.current) return;
    // `columns.length === 0` is a *third* failure and the easiest to miss:
    // `fetchBoardColumns` resolves empty — it does not reject — for a host whose
    // ledger list carries no `tasks` ledger at all. Treating that as a known
    // vocabulary would put "Idle · 0 open tasks" on every card of a company
    // whose board this console never found, which is the exact false claim the
    // `null` state exists to prevent.
    setWorkload(tasks && columns?.length ? workloadByAssignee(tasks, columns) : null);
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    // Drop the previous read's workload before the new reads start. A stale
    // non-null map must never filter a roster it does not describe: on a
    // `refreshKey` re-run the new roster can land while `loadWorkload` is still
    // in flight, and one company's board cannot determine another's visible
    // roster. `null` also disables the Working switch, so the filter cannot
    // strand the roster mid-re-read.
    setWorkload(null);
    workloadRun.current += 1;
    void boot();
    void loadWorkload();
    // `refreshKey` re-runs the read after setup staffs the company; without it
    // the operator lands on the roster they had before their team was built.
  }, [boot, loadWorkload, refreshKey]);

  /**
   * A "Working" filter is only answerable while the workload is readable.
   *
   * If the workload read fails after the operator turned the filter on —
   * a re-run setup that hits a dropped network, say — every member reads as
   * not working, and the switch below is disabled while `workload` is null,
   * so the filter would hide the whole roster with no way to turn it off.
   * Reset it when the workload becomes unavailable so the roster always has a
   * way back.
   */
  useEffect(() => {
    if (workload === null) setWorkingOnly(false);
  }, [workload]);

  /**
   * Re-read the roster on the way back from the agent sub-page (issue #264).
   *
   * This view renders the detail as an early return, so opening an agent never
   * unmounts the roster and never re-runs `boot`. An edit saved in the panel
   * therefore landed on the host while these cards went on showing what they
   * held before it: press Back after renaming an agent and the old role is
   * still on the card, until a hard reload. The panel and the roster disagreed
   * about the same company, and the roster was the wrong one.
   *
   * Keyed on `sub` rather than on the Back button's callback, so the browser's
   * own Back — and a hand-edited hash — refresh too. The ref is what keeps the
   * first mount from fetching twice: the effect above already did.
   */
  const leftAgentPage = useRef(false);
  useEffect(() => {
    if (sub) {
      leftAgentPage.current = true;
      return;
    }
    if (!leftAgentPage.current) return;
    leftAgentPage.current = false;
    // Deliberately without `setLoad("loading")`: the cards on screen are the
    // right cards, only possibly stale, so they stay put until the new ones
    // land rather than blanking to a skeleton on every Back.
    void boot();
  }, [sub, boot]);


  // Setting, changing and resetting a teammate's daily cap moved to the
  // teammate's own detail page (issue #1206), beside Inbox — see
  // `AgentDetailView`'s `Budget` section. This view keeps `whoSet`/`people`
  // above only to attribute the cap it still *displays* on the card via
  // `DailyBudgetLine`.

  /**
   * Writes the teammate and answers whether the write landed (issue #1989).
   *
   * The boolean is what lets the dialog keep the operator's sentence and the
   * design the host was paid for when this fails — it used to be called
   * fire-and-forget and the dialog cleared itself regardless. `true` also
   * covers the console-only fallback below: nothing reached a host, but the
   * add is as complete as it is going to get and there is nothing to retry.
   */
  async function addMember(fields: NewMemberFields): Promise<boolean> {
    let created: TeamMemberDto | null = null;
    try {
      created = await client.addTeamMember(
        {
          name: fields.name,
          role: fields.role,
          description: fields.description || undefined,
          // Blank stays off the wire: at creation there is no blueprint to
          // override, so an empty box means "no persona", not "an empty one".
          instructions: fields.instructions || undefined,
        },
        company,
      );
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        // No team write plane on this host — keep the edit local-only.
        const local = newMember(fields);
        setMembers((m) => [...m, local]);
        // And say so per row, because the roster-wide `fromHost` still reads
        // true here: the read landed, only the write had nowhere to go. Without
        // this the card would offer to open a detail page and a DM against an
        // id the host has never heard of.
        setConsoleOnly((ids) => new Set(ids).add(local.id));
        reportAddMember({ kind: "console-only", name: fields.name });
        setAddOpen(false);
        return true;
      }
      reportAddMember(addMemberFailure(error));
      // The dialog keeps what it holds: this is the transient case, and a
      // retry must not cost a second design pass.
      return false;
    }

    const missed: MissedStep[] = [];
    // The face, against the host's real agent id — `addTeamMember` takes none.
    // Before the redirect, so the page the operator lands on already wears it.
    if (fields.avatar) {
      try {
        await client.updateAgent(created.id, { avatar: fields.avatar }, company);
      } catch {
        missed.push({
          what: "their icon couldn't be set",
          fix: "Pick one again from their profile.",
        });
      }
    }
    // The dialog's write is only half of its flow. It collects a name, a face
    // and a post, so the description and the persona are still to be written —
    // on the teammate's own page, where the copilot that drafts them lives.
    //
    // The redirect goes BEFORE the roster refetch on purpose. The operator is
    // being taken off the roster, so blocking the handoff on a read of the list
    // they are leaving delays it for nothing — and a read that failed would
    // raise "the roster couldn't be read back" over a page the roster is not on,
    // which is a sentence about a list nobody is looking at.
    if (fields.landOnProfile) {
      setAddOpen(false);
      onOpenAgent(created.id, { edit: true });
      reportAddMember(addOutcome(fields.name, missed));
      // Still re-read, so the roster is current when Back returns to it.
      void boot();
      return true;
    }
    // Persisted on the host — refetch so the card reflects the real record
    // (id, merge order, inbox state) rather than a locally-guessed one.
    if (!(await boot())) {
      missed.push({
        what: "the roster couldn't be read back",
        fix: "The roster below is empty because that read failed, not because your company is — reload to see them.",
      });
    }
    setAddOpen(false);
    // Announced after the refetch, not on the response, and only as a clean add
    // when that refetch actually landed: the roster the operator is looking at
    // is the one being claimed about, so a read that could not confirm the
    // write must not be toasted over as though it had.
    reportAddMember(addOutcome(fields.name, missed));
    return true;
  }

  async function removeMember(member: TeamMember) {
    try {
      await client.removeTeamMember(member.id, company);
      await boot();
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        // No team write plane on this host — drop it from local state only.
        setMembers((ms) => ms.filter((x) => x.id !== member.id));
      } else if (error instanceof ApiError && error.status === 409) {
        // The only 409 this route still answers: a company must keep at
        // least one teammate. The host's own message says which teammate and
        // what to do about it, so it is shown rather than restated.
        toast.error(
          error.message || "You can't remove your company's last agent.",
        );
      } else {
        toast.error(error instanceof Error ? error.message : "Couldn't remove agent.");
      }
    }
  }

  // `#/team/<agentId>` is the agent detail sub-page. Every hook above has
  // already run, so this early return keeps hook order stable across both
  // shapes of the view.
  if (sub) {
    return (
      <AgentDetailView
        client={client}
        company={company}
        agentId={sub}
        onBack={() => onOpenAgent(null)}
      />
    );
  }

  const normalizedNameQuery = nameQuery.trim().toLocaleLowerCase();
  const visibleMembers = members.filter((member) => {
    const matchesName = !normalizedNameQuery || member.name.toLocaleLowerCase().includes(normalizedNameQuery);
    const isWorking = workload?.get(member.id)?.status === "working";
    return matchesName && (!workingOnly || isWorking);
  });

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {/*
        Headed "Agents", not "Team" (issue #1141) and not "Company" any more.
        This grid is no longer a page of its own — bare `#/team` redirects to
        `#/company` — it is what the sidebar's **Agents** row leads to, and a
        page has to be called what the row that reaches it is called.

        "Company" was right while the row said Company. Under a section ALSO
        called Company it said the word twice and named nothing: the page is
        the roster, so it is the agents.

        Issue #1207 put the actions on the heading's row rather than on a row of
        their own; `PageHeader` is where that shape lives now (issue #1763), and
        `company-header` still names the row the two share.
      */}
      <PageHeader
        title="Agents"
        width="full"
        rowTestId="company-header"
        description={
          <>
            The agents that make up your company — what each does, and what
            they're on. {fromHost ? "Defined by this company." : "Start from these and shape your own."}
          </>
        }
        actions={
          <>
            {onManageDesks && (
              <Button variant="outline" onClick={onManageDesks} data-testid="company-manage-desks">
                <Network className="size-4" /> Manage desks
              </Button>
            )}
            {/*
              The activity graph is a deep-link destination rather than a fifth
              sidebar row (Rule 6): it is reached from the roster, which is the
              page an operator is already on when they ask who works with whom.
              An anchor, not a callback — it is a plain address, and one more
              navigation prop through this tree buys nothing.
            */}
            <Button
              variant="outline"
              render={<a href="#/company/comms" data-testid="company-activity" />}
            >
              <Network className="size-4" /> Activity
            </Button>
            <Button onClick={() => setAddOpen(true)}>
              <UserPlus className="size-4" /> Add agent
            </Button>
          </>
        }
      />
      <div className="min-h-0 w-full flex-1 space-y-6 overflow-y-auto px-4 py-6">

        {/*
          The other half of "blocking but skippable": until somebody has staffed
          this company, keep a visible way back into setup. Skipping the dialog
          leaves an operator on a page with nothing of theirs on it, and burying
          the offer would make that a dead end.

          The copy says "not been set up" rather than "has no team", and that is
          load-bearing: this prompt now renders directly above the global
          baseline's agents, who are real agents on the host (issue #1404).
          Claiming there is nobody here, over four cards, would be the same lie
          the fabricated starter roster was deleted for — pointing the other way.
        */}
        {load === "ready" && onRunSetup && hostEmpty && (
          <div
            className="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-dashed px-4 py-3"
            data-testid="setup-prompt"
          >
            <div className="space-y-0.5">
              <p className="text-sm font-medium">This company hasn't been set up yet</p>
              <p className="text-sm text-muted-foreground">
                Answer three questions and we'll build you a starting team.
              </p>
            </div>
            <Button variant="secondary" onClick={onRunSetup} data-testid="setup-prompt-run">
              <Sparkles className="size-4" /> Set up my company
            </Button>
          </div>
        )}

        {load === "loading" ? (
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {Array.from({ length: 6 }).map((_, i) => (
              <Skeleton key={i} className="h-32 rounded-xl" />
            ))}
          </div>
        ) : (
          <>
            <div className="flex flex-wrap items-center gap-3" data-testid="team-roster-filters">
              <div className="min-w-52 flex-1">
                <Label htmlFor="team-roster-search" className="sr-only">
                  Search agents by name
                </Label>
                <Input
                  id="team-roster-search"
                  value={nameQuery}
                  onChange={(event) => setNameQuery(event.target.value)}
                  placeholder="Search agents by name…"
                  data-testid="team-roster-search"
                />
              </div>
              <Label className="flex items-center gap-2 text-sm font-medium">
                <Switch
                  checked={workingOnly}
                  onCheckedChange={setWorkingOnly}
                  disabled={workload === null}
                  aria-label="Show working agents only"
                  data-testid="team-roster-working"
                />
                Working
              </Label>
            </div>
            <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
              {visibleMembers.map((m) => (
                <MemberCard
                  key={m.id}
                  member={m}
                  onRemove={() => void removeMember(m)}
                  // Only a host-backed teammate can be opened: a card with no
                  // record behind it would 404 on its id, and the detail view
                  // would report a teammate that was never removed.
                  onOpen={hostBackedCard(m, fromHost, consoleOnly) ? () => onOpenAgent(m.id) : undefined}
                  // The same gate again: only a row the host holds has a
                  // binding to report. A console-only placeholder runs on
                  // nothing yet, and "Company default" over it would describe a
                  // teammate that does not exist.
                  hostBacked={hostBackedCard(m, fromHost, consoleOnly)}
                  // The same gate, because it is the same question: a row no
                  // host holds has no DM either, and the room would answer with
                  // its unknown-channel fallback rather than a conversation.
                  messageHref={
                    hostBackedCard(m, fromHost, consoleOnly) ? agentDmHref(m) : undefined
                  }
                  // Looked up by roster id, so a card the board assigned to a
                  // *desk* is never attributed to the people on it.
                  //
                  // The two ways of having no entry are different facts and are
                  // kept apart here: the board answered and this teammate is on
                  // nothing (idle, zero — worth saying), versus the board never
                  // answered (undefined — the card says nothing at all).
                  workload={workload ? (workload.get(m.id) ?? IDLE) : undefined}
                  onNavigateToDesk={onNavigateToDesk}
                />
              ))}
              {visibleMembers.length === 0 && (
                <p className="col-span-full text-sm text-muted-foreground" data-testid="team-roster-empty">
                  No agents match these filters.
                </p>
              )}
              <button
                onClick={() => setAddOpen(true)}
                className="flex min-h-32 flex-col items-center justify-center gap-2 rounded-xl border border-dashed text-sm text-muted-foreground transition-colors hover:border-primary/40 hover:bg-accent/40 hover:text-foreground"
              >
                <Plus className="size-5" />
                Add agent
              </button>
            </div>
          </>
        )}
      </div>

      <AddMemberDialog
        open={addOpen}
        onOpenChange={setAddOpen}
        onAdd={addMember}
        client={client}
        company={company}
      />
    </div>
  );
}

/**
 * A teammate the board knows about and has given nothing to.
 *
 * Shared rather than rebuilt per card: it is a constant fact, and a fresh
 * object per render would change `MemberCard`'s props on every pass.
 */
const IDLE: Workload = { open: 0, status: "idle" };

/** No row is console-only — the state every roster read returns to. */
const NO_CONSOLE_ONLY: ReadonlySet<string> = new Set<string>();

/**
 * Whether this card's two host-addressed controls have anything to address.
 *
 * Both the title's detail link and the Message link resolve an **id against the
 * host**, so both are wrong in exactly the same states and are gated together
 * rather than separately — one of them silently surviving a narrowing of the
 * other is how they come to disagree.
 *
 * Two states, and `fromHost` alone only covers the first:
 *
 *  - **The roster is not the host's.** The read never landed, or landed with
 *    nobody, so every card on screen is a local placeholder.
 *  - **This row is not the host's**, on a roster that is. A host serving
 *    `GET …/team` and 404ing the `POST` leaves `fromHost` true while
 *    `addMember` appends a console-only row beside the real ones — the state
 *    `consoleOnly` exists to name. Reaching the detail page for such a row
 *    reports a teammate that was never removed; reaching its DM lands on the
 *    room's unknown-channel fallback.
 */
export function hostBackedCard(
  member: TeamMember,
  fromHost: boolean,
  consoleOnly: ReadonlySet<string>,
): boolean {
  return fromHost && !consoleOnly.has(member.id);
}

/**
 * The address of this teammate's direct conversation (issue #2252).
 *
 * Built on {@link dmChannelId} and **not** `dmThreadId`. The two are only the
 * same string for most of the roster: `dmChannelId` is always `dm:<id>`, the
 * console-local channel id the hash router resolves, while `dmThreadId` is the
 * bare id — the *host* thread a DM is addressed on — for everyone except a
 * teammate whose id itself spells General, where the host folds the bare key
 * onto the company-wide line. Routing on the thread id therefore sends an
 * operator who clicked that teammate to the company's General channel instead
 * of the DM they asked for (issue #1743).
 *
 * The same call, for the same reason, backs the console search results
 * (`search/sources.ts`).
 *
 * ## Why it carries the host scope
 *
 * Through {@link withHostParam} rather than as a bare `#/chat/…` fragment,
 * because this addresses an *anchor* — and an anchor is copied, middle-clicked
 * and Cmd-clicked as well as clicked, which is half of why it is an anchor at
 * all. A same-tab click survives a dropped scope, since `useHostAddress`
 * re-asserts it on the `hashchange` that follows. A new document has no
 * selection to repair from: `useHostRoute` falls back to the bootstrap or
 * embedded host and resolves this agent's id against whichever console that is
 * — an unknown DM, or a different company's agent wearing the same id.
 * `TaskCard`'s `detailsHref` carries the scope for exactly this reason.
 *
 * A console holding one host writes no scope at all, so this is the same
 * string it has always been there.
 */
export function agentDmHref(member: TeamMember): string {
  return withHostParam(`chat/${encodeURIComponent(dmChannelId(member))}`);
}

/**
 * One agent on the Agent board.
 *
 * The card is a scanning surface first: the title is a stretched link to the
 * agent's detail page (issue #1810), and the two controls that sit above that
 * click target are the ones an operator reaches for *without* leaving the grid
 * — Message on the face (issue #2252) and the destructive Remove behind an
 * overflow (issue #1206). Everything a card only *reports* — workload, desk,
 * daily budget — is read-only here and configured on the detail page.
 *
 * `onOpen` and `messageHref` are both undefined for a card with no host record,
 * for the same reason: a starter-team placeholder has no agent behind it, so
 * both addresses would resolve to nothing.
 */
function MemberCard({
  member,
  onRemove,
  onOpen,
  messageHref,
  workload,
  onNavigateToDesk,
  hostBacked,
}: {
  member: TeamMember;
  onRemove: () => void;
  /** Open this agent's detail page. Undefined when the card has no host record. */
  onOpen?: () => void;
  /**
   * Address of this agent's direct conversation ({@link agentDmHref}).
   * Undefined when the card has no host record, which is when a DM would
   * address a thread with no agent behind it — the menu item is then omitted.
   */
  messageHref?: string;
  /**
   * What this teammate is on and carrying, or undefined when the board could
   * not be read — in which case the card says nothing about either.
   */
  workload?: Workload;
  /**
   * Open one of this teammate's desks from its chip. Undefined when the shell
   * does not offer desk navigation; the chips then render as plain text.
   */
  onNavigateToDesk?: (deskId: string) => void;
  /**
   * Whether the host holds this row, and so whether it has a harness and model
   * binding to report at all.
   */
  hostBacked?: boolean;
}) {
  // Issue #1208: the role only earns its line when it is not the name again.
  // Every manifest-declared agent in the shipped companies resolves both to one
  // string, so this slot was the same words twice on every card — directly
  // above the description that actually says what the teammate does.
  const subtitle = roleSubtitle(member.name, member.role);
  return (
    <Card
      data-testid="team-card"
      className={cn(
        "relative transition-colors",
        onOpen && "cursor-pointer hover:border-primary/40 hover:shadow-sm",
      )}
    >
      <CardContent className="flex h-full flex-col gap-3">
        <div className="flex items-start gap-3">
          {/*
            The shared chat avatar, not a hand-rolled tile (issue #1181). This
            drew `initials()` over a `TEAM_TONES` background — the same visual
            language as chat, minus the mascot — so an agent had a face in a DM
            and letters on the page that is *about* them.

            44px, comfortably above the ~24px floor under which a mascot is a
            smudge and the bare tone tile is the honest fallback.
          */}
          <TeammateAvatar name={member.name} tone={member.tone} avatar={member.avatar} className="size-11 rounded-xl text-sm" />
          {onOpen ? (
            <button
              type="button"
              onClick={onOpen}
              // Issue #1810: stretch the title's native button over the card,
              // instead of turning a container with nested controls into a
              // button. The menu and desk links sit above this layer below.
              className="-m-1 min-w-0 flex-1 rounded-sm p-1 text-left after:absolute after:inset-0 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring transition-opacity hover:opacity-80"
              data-testid="team-card-open"
            >
              <span className="block truncate font-medium">{member.name}</span>
              {subtitle && (
                <span className="block truncate text-xs text-muted-foreground">{subtitle}</span>
              )}
              {member.global && (
                <Badge
                  variant="secondary"
                  className="mt-1 text-3xs"
                  data-testid="team-card-global"
                >
                  Global baseline
                </Badge>
              )}
            </button>
          ) : (
            <div className="min-w-0 flex-1" data-testid="team-card-open">
              <p className="truncate font-medium">{member.name}</p>
              {subtitle && (
                <p className="truncate text-xs text-muted-foreground">{subtitle}</p>
              )}
              {member.global && (
                <Badge
                  variant="secondary"
                  className="mt-1 text-3xs"
                  data-testid="team-card-global"
                >
                  Global baseline
                </Badge>
              )}
            </div>
          )}
          {/*
            Above the title button's stretched click target (issue #1810), and
            holding two controls rather than one since issue #2252.

            `items-center` aligns the pair to each other inside a header that is
            `items-start`; `shrink-0` keeps them at full size so the squeeze
            lands on the title's `min-w-0 flex-1` — which truncates — rather
            than on the buttons, which cannot.
          */}
          <div className="relative z-10 flex shrink-0 items-center gap-0.5">
            {/*
              Message sits on the card face, not in the overflow (issue #2252).

              It shipped inside the menu first. That put the thing an operator
              wants *while scanning the roster* — ask this one something — two
              clicks deep, behind a control whose only other item is
              destructive. On the face it is one click and always visible: no
              hover needed to discover it, which matters on a grid where the
              pointer is travelling between cards rather than resting on one.
              The menu is back to holding only Remove; two controls doing the
              same thing would be worse than either alone.

              Still an anchor, so the status bar previews the destination on
              hover and Cmd-click opens the DM in a new tab — neither of which
              a button can offer.

              Icon-only, so the name is mandatory and carries the agent:
              "Message Brand Designer", not a bare "Message" repeated on every
              card, which would leave a screen reader with thirteen
              indistinguishable controls. The label is spent twice — as the
              tooltip and as `aria-label` — the way `McpIconButton` does it.
            */}
            {messageHref && (
              <Tooltip>
                <TooltipTrigger
                  render={
                    <a
                      href={messageHref}
                      aria-label={`Message ${member.name}`}
                      data-testid="team-card-message"
                      className={buttonVariants({
                        variant: "ghost",
                        size: "icon",
                        className: "-mt-1 size-7",
                      })}
                    />
                  }
                >
                  <MessageSquare className="size-4" />
                </TooltipTrigger>
                <TooltipContent>{`Message ${member.name}`}</TooltipContent>
              </Tooltip>
            )}
            <DropdownMenu>
              <DropdownMenuTrigger
                render={<Button variant="ghost" size="icon" className="-mr-1 -mt-1 size-7" aria-label="Agent actions" />}
              >
                <MoreHorizontal className="size-4" />
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end">
                {/*
                  Issue #1206: "View agent" is gone — the card itself
                  navigates now, so a menu item doing the same thing was noise
                  that also implied (wrongly) that the card did not. The
                  budget-editing items ("Set/Change daily budget…", "Remove
                  cap", "Reset to company default") are gone too, for the same
                  reason the Inbox switch left the card in #1190: a card in a
                  grid of thirteen is for recognising an agent, not
                  configuring one. Editing now lives on the agent's own
                  detail page, beside Inbox — see `AgentDetailView`'s `Budget`
                  section. The card still *shows* the cap and today's spend
                  via `DailyBudgetLine` below; only the controls that write
                  moved.

                  That still leaves exactly one item. It stays a menu rather
                  than a bare button: Remove is destructive, and a deliberate
                  extra click before it is worth keeping beside the title
                  action. Unlike "View agent" it does
                  not duplicate the card's own action, and unlike Budget it is
                  not per-agent configuration that reads better on a
                  detail page — it is the one roster-level action an operator
                  reaches for while scanning many cards deciding which to
                  prune, and moving it off the grid would trade a fast,
                  discoverable one-hop delete for an extra full-page
                  navigation with no offsetting benefit.

                  Message (issue #2252) briefly sat here too, above a
                  separator. It moved to the card face — see the anchor beside
                  this trigger — because burying the roster's most-reached-for
                  action behind an overflow was the thing the menu is supposed
                  to protect against, not an instance of it. It is deliberately
                  not in both places: one affordance per action.
                */}
                <DropdownMenuItem variant="destructive" onClick={onRemove}>
                  Remove
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          </div>
        </div>
        {member.description && (
          <p className="line-clamp-3 text-sm text-muted-foreground" data-testid="team-card-description">
            {member.description}
          </p>
        )}
        {/*
          The desks this agent sits on, one chip per desk (issue #1440). The
          roster read already carries `desks` per member — the card just never
          drew it. A chip is the desk's name plus a "(lead)" marker for the desk
          it leads, and it links to that desk's own address (`#/company/<deskId>`),
          the same destination as the chart's desk nodes. When the host reports
          no desks the card says so outright rather than leaving a blank gap:
          "on no desk" is a fact an operator scanning a roster wants to see.
        */}
        <div className="flex flex-wrap gap-1" data-testid="team-card-desks">
          {member.desks.length === 0 ? (
            <p className="text-xs text-muted-foreground" data-testid="team-card-no-desks">
              Not on a desk
            </p>
          ) : (
            member.desks.map((desk) => (
              <Badge
                key={desk.id}
                variant="secondary"
                className={cn(
                  "gap-1 text-3xs",
                  onNavigateToDesk && "relative z-10 cursor-pointer",
                )}
                data-testid={`team-card-desk-${desk.id}`}
                onClick={
                  onNavigateToDesk
                    ? () => onNavigateToDesk(desk.id)
                    : undefined
                }
              >
                <Users className="size-2.5" aria-hidden />
                {desk.name}
                {desk.lead && <span className="text-3xs opacity-70">(lead)</span>}
              </Badge>
            ))
          )}
        </div>
        {/*
          Pinned to the bottom of the card, not left floating under whatever
          length the description happened to be.

          `CardContent` is a `h-full` column inside a stretched grid row, so
          every card in a row is the same height — but the content was all
          top-aligned, and the description is `line-clamp-3`. A one-line
          description therefore put this block ~36px higher than the two-line
          card beside it, and the status line is the one thing a roster is
          scanned for: "who is working, and how much is on them" was on two or
          three different baselines in every row, with dead space underneath
          each card.

          `mt-auto` takes the slack instead, so the running facts line up
          across a row and the card has no empty tail. Wrapped rather than
          applied to `WorkloadLine` directly because a host that cannot answer
          the board renders no workload at all (see `IDLE` and the `workload`
          prop) — the budget line has to inherit the same anchor, or the two
          shapes of card disagree again.
        */}
        <div className="mt-auto space-y-1.5 empty:hidden">
          {hostBacked && <ModelLine member={member} />}
          {workload && <WorkloadLine workload={workload} />}
          {member.budgetUsdDaily !== undefined && (
            <DailyBudgetLine
              budgetUsdDaily={member.budgetUsdDaily}
              spentTodayUsd={member.spentTodayUsd ?? 0}
            />
          )}
        </div>
        {/*
          The card's footer is gone with the Inbox switch it existed to hold
          (issue #1190).

          The switch was the only control on the card that *wrote* to the host,
          at the same weight as the name, on a grid of thirteen — a card is for
          recognising an agent, and a mis-click while scanning silently
          changed a per-agent setting with no confirmation. It moved to the
          agent's own page, which already reported inbox state as a badge and
          offered no way to change it. See `AgentDetailView`.

          Its companion — a "Agent" badge — went with it rather than being
          left behind a border rule on its own. On a page whose every card is a
          agent it labelled nothing, and a bordered band holding one inert
          chip reads as something that failed to load.
        */}
      </CardContent>
    </Card>
  );
}

/**
 * Which model this teammate runs on, and the harness it runs it through.
 *
 * Read-only here like the desk chips and the budget line: the picker lives on
 * the teammate's own page. The label comes from {@link modelSummary}, which
 * never names a model the roster read did not send — an unpinned teammate
 * inherits, and saying so is the honest answer a resolved-looking name would
 * not be.
 *
 * The harness rides along as a chip rather than another `·` segment: it is a
 * different kind of fact from the model, it is what tells an operator a
 * teammate runs through their own CLI rather than the built-in harness, and a
 * chip keeps its width off the model text, which is what truncates.
 */
function ModelLine({ member }: { member: TeamMember }) {
  const summary = modelSummary(member);
  return (
    <p className="flex items-center gap-1.5 text-xs text-muted-foreground" data-testid="team-card-model">
      <Cpu className="size-3 shrink-0" aria-hidden />
      <span
        className={cn("min-w-0 truncate", summary.inherited && "italic")}
        title={summary.label}
        data-testid="team-card-model-label"
      >
        {summary.label}
      </span>
      {summary.harness && (
        <Badge variant="outline" className="text-3xs" data-testid="team-card-harness">
          {summary.harness}
        </Badge>
      )}
    </p>
  );
}

/**
 * What a teammate is on, and how much of it (issue #1141).
 *
 * One line for two facts an operator scanning the roster is actually asking:
 * is anybody working on my behalf right now, and how much is queued behind
 * them. Neither is a host field — both are derived from the board, and
 * `lib/team-workload.ts` carries the reasoning.
 *
 * Coloured through the console's status vocabulary rather than a palette step,
 * so `working` is the same cyan as a running workflow node and `idle` the same
 * neutral as everything that is asking nothing of anyone. Both themes come from
 * the tokens.
 */
function WorkloadLine({ workload }: { workload: Workload }) {
  const working = workload.status === "working";
  return (
    <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
      <span
        className={cn(
          "size-2 shrink-0 rounded-full",
          working ? "bg-status-running" : "bg-status-idle",
        )}
        aria-hidden
      />
      <span
        className={cn(
          "font-medium",
          working ? "text-status-running-text" : "text-status-idle-text",
        )}
        data-testid="team-card-status"
      >
        {working ? "Working" : "Idle"}
      </span>
      <span aria-hidden>·</span>
      <span data-testid="team-card-tasks">
        {workload.open === 1 ? "1 open task" : `${workload.open} open tasks`}
      </span>
    </p>
  );
}

/**
 * The card's read half of a teammate's daily spend cap (issue #304) — the
 * edit half moved to the teammate's own page beside Inbox (issue #1206), and
 * this line is what is left to show for it here.
 *
 * Only rendered when `budgetUsdDaily` is present: absence IS the uncapped
 * signal (`TeamMemberDto.budgetUsdDaily`'s own doc), so a `0` cap would be a
 * different, wrong claim — nothing to show is not the same as a $0.00 cap.
 */
function DailyBudgetLine({
  budgetUsdDaily,
  spentTodayUsd,
}: {
  budgetUsdDaily: number;
  spentTodayUsd: number;
}) {
  const paused = spentTodayUsd >= budgetUsdDaily;
  return (
    <p className="flex items-center gap-1.5 text-xs text-muted-foreground" data-testid="team-budget">
      <span className={cn("font-medium", paused && "text-status-idle-text")}>
        {usd(budgetUsdDaily)}/day
      </span>
      <span aria-hidden>·</span>
      <span>{usd(spentTodayUsd)} spent today</span>
      {paused && (
        <>
          <span aria-hidden>·</span>
          <span className="font-medium text-status-idle-text">paused</span>
        </>
      )}
    </p>
  );
}
