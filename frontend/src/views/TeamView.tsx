import { useCallback, useEffect, useRef, useState } from "react";
import { Mail, MoreHorizontal, Network, Plus, Sparkles, UserPlus, Users } from "lucide-react";
import { toast } from "sonner";

import { listPeople, me as fetchMe, type Person } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { setInboxEnabled } from "@/api/inbox";
import { listTasks } from "@/api/tasks";
import { ApiError, type TeamMemberDto } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
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
import { emptyDraft, missingRequired, type AgentDraft, type AgentFieldKey } from "@/lib/agent";
import { draftNewAgentField } from "@/api/agent-copilot";
import { getInferenceStatus, type CognitionPath } from "@/api/inference";
import { fetchBoardColumns } from "@/lib/board-columns";
import { shouldPromptSetup } from "@/lib/company-setup";
import {
  addMemberFailure,
  addOutcome,
  reportAddMember,
  type MissedStep,
} from "@/lib/member-feedback";
import { fromDto, newMember, roleSubtitle, type TeamMember } from "@/lib/team";
import { workloadByAssignee, type Workload } from "@/lib/team-workload";
import { personName } from "@/lib/person";
import { cn } from "@/lib/utils";
import { AgentDetailView } from "@/views/team/AgentDetailView";
import { AgentFields } from "@/views/team/AgentFields";
import { FieldCopilot } from "@/views/team/FieldCopilot";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * The agent id in the hash (`#/team/<agentId>`), when one is addressed. The
   * detail view is a sub-page rather than a modal so an operator can link to an
   * agent, refresh onto it, and use Back (issue #264).
   */
  sub: string | null;
  /** Open an agent, or return to the roster with `null`. */
  onOpenAgent: (agentId: string | null) => void;
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
  const [nameQuery, setNameQuery] = useState("");
  const [workingOnly, setWorkingOnly] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  const [isAdmin, setIsAdmin] = useState(false);
  // Who set which cap. Only an admin may read the user directory, so this stays
  // empty for a member — and the attribution line degrades to "an admin"
  // rather than disappearing.
  const [people, setPeople] = useState<Person[]>([]);
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
   * Hiding the budget controls from a non-admin is **courtesy, not enforcement**.
   * The host refuses the write with a 403 whatever this says; showing an
   * operator a control they cannot use is the only thing this prevents.
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
    void loadViewer();
    void loadWorkload();
    // `refreshKey` re-runs the read after setup staffs the company; without it
    // the operator lands on the roster they had before their team was built.
  }, [boot, loadViewer, loadWorkload, refreshKey]);

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

  /** A human label for whoever set a cap — never a raw user id. */
  function whoSet(userId: string): string {
    const person = people.find((p) => p.id === userId);
    return person ? personName(person) : "an admin";
  }

  // Setting, changing and resetting a teammate's daily cap moved to the
  // teammate's own detail page (issue #1206), beside Inbox — see
  // `AgentDetailView`'s `Budget` section. This view keeps `whoSet`/`people`
  // above only to attribute the cap it still *displays* on the card via
  // `DailyBudgetLine`.

  async function addMember(fields: AddMemberFields) {
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
          // Omitted unless the operator typed one: an add that carries a cap is
          // admin-only on the host, while a plain add is open to any member.
          budgetUsdDaily: fields.budgetUsdDaily,
        },
        company,
      );
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        // No team write plane on this host — keep the edit local-only. An inbox
        // needs a persisted teammate to hang off, so it can't be enabled here.
        setMembers((m) => [...m, newMember(fields)]);
        reportAddMember({
          kind: "console-only",
          name: fields.name,
          note: fields.inbox ? "No inbox was created." : undefined,
        });
        setAddOpen(false);
        return;
      }
      reportAddMember(addMemberFailure(error));
      return;
    }

    const missed: MissedStep[] = [];
    // Enable the inbox against the host's real agent id *before* refetching, so
    // the reloaded roster already reports the toggle as on.
    if (fields.inbox) {
      try {
        await setInboxEnabled(client, company, created.id, true);
      } catch {
        missed.push({
          what: "their inbox couldn't be switched on",
          fix: "Turn it on from their actions menu.",
        });
      }
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
          error.message || "You can't remove your company's last teammate.",
        );
      } else {
        toast.error(error instanceof Error ? error.message : "Couldn't remove teammate.");
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
        Headed "Company", not "Team" (issue #1141). This grid is no longer a
        page of its own — bare `#/team` redirects to `#/company` — it is the
        Company page's Cards half, and the org chart beside it heads the same
        way. Two headings over one page's two halves is how an operator ends
        up believing they are on two different pages.

        Issue #1207 put the actions on the heading's row rather than on a row of
        their own; `PageHeader` is where that shape lives now (issue #1763), and
        `company-header` still names the row the two share.
      */}
      <PageHeader
        title="Company"
        width="5xl"
        rowTestId="company-header"
        description={
          <>
            The teammates that make up your company — what each does, and what
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
            <Button onClick={() => setAddOpen(true)}>
              <UserPlus className="size-4" /> Add teammate
            </Button>
          </>
        }
      />
      <div className="mx-auto min-h-0 w-full max-w-5xl flex-1 space-y-6 overflow-y-auto px-4 py-6">

        {/*
          The other half of "blocking but skippable": until somebody has staffed
          this company, keep a visible way back into setup. Skipping the dialog
          leaves an operator on a page with nothing of theirs on it, and burying
          the offer would make that a dead end.

          The copy says "not been set up" rather than "has no team", and that is
          load-bearing: this prompt now renders directly above the global
          baseline's teammates, who are real agents on the host (issue #1404).
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
                  Search teammates by name
                </Label>
                <Input
                  id="team-roster-search"
                  value={nameQuery}
                  onChange={(event) => setNameQuery(event.target.value)}
                  placeholder="Search teammates by name…"
                  data-testid="team-roster-search"
                />
              </div>
              <Label className="flex items-center gap-2 text-sm font-medium">
                <Switch
                  checked={workingOnly}
                  onCheckedChange={setWorkingOnly}
                  disabled={workload === null}
                  aria-label="Show working teammates only"
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
                  // Only a host-backed teammate can be opened: a starter-team
                  // card is a local placeholder with no record behind it, so its
                  // id would 404 and the detail view would report a teammate that
                  // was never removed.
                  onOpen={fromHost ? () => onOpenAgent(m.id) : undefined}
                  setByLabel={m.budgetSetBy ? whoSet(m.budgetSetBy) : undefined}
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
                  No teammates match these filters.
                </p>
              )}
              <button
                onClick={() => setAddOpen(true)}
                className="flex min-h-32 flex-col items-center justify-center gap-2 rounded-xl border border-dashed text-sm text-muted-foreground transition-colors hover:border-primary/40 hover:bg-accent/40 hover:text-foreground"
              >
                <Plus className="size-5" />
                Add teammate
              </button>
            </div>
          </>
        )}
      </div>

      <AddMemberDialog
        open={addOpen}
        onOpenChange={setAddOpen}
        onAdd={addMember}
        canSetBudget={isAdmin && fromHost}
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

/** The fields the add dialog collects. */
interface AddMemberFields {
  name: string;
  role: string;
  description: string;
  /**
   * The persona typed into the dialog's Instructions box.
   *
   * Collected since #264 put `instructions` in `AGENT_FIELDS`, and dropped on
   * the floor until #1776 noticed: the box was rendered, filled in, and never
   * sent. The host has accepted `instructions` at creation since #1530 and
   * `addTeamMember` has carried it since — this was the one link missing, so an
   * operator who wrote a persona in the add dialog watched it vanish.
   */
  instructions: string;
  inbox?: boolean;
  /** An optional daily cap. Undefined means "don't set one", never "$0". */
  budgetUsdDaily?: number;
}

function MemberCard({
  member,
  onRemove,
  onOpen,
  setByLabel,
  workload,
  onNavigateToDesk,
}: {
  member: TeamMember;
  onRemove: () => void;
  /** Open this agent's detail page. Undefined when the card has no host record. */
  onOpen?: () => void;
  /** Who set the current override, already resolved to something readable. */
  setByLabel?: string;
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
            language as chat, minus the mascot — so a teammate had a face in a DM
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
              className="-m-1 min-w-0 flex-1 rounded-sm p-1 text-left after:absolute after:inset-0 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
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
          {/* Above the title button's stretched click target (issue #1810). */}
          <div className="relative z-10">
            <DropdownMenu>
              <DropdownMenuTrigger
                render={<Button variant="ghost" size="icon" className="-mr-1 -mt-1 size-7" aria-label="Teammate actions" />}
              >
                <MoreHorizontal className="size-4" />
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end">
                {/*
                  Issue #1206: "View teammate" is gone — the card itself
                  navigates now, so a menu item doing the same thing was noise
                  that also implied (wrongly) that the card did not. The
                  budget-editing items ("Set/Change daily budget…", "Remove
                  cap", "Reset to company default") are gone too, for the same
                  reason the Inbox switch left the card in #1190: a card in a
                  grid of thirteen is for recognising a teammate, not
                  configuring one. Editing now lives on the teammate's own
                  detail page, beside Inbox — see `AgentDetailView`'s `Budget`
                  section. The card still *shows* the cap and today's spend
                  via `DailyBudgetLine` below; only the controls that write
                  moved.

                  That leaves exactly one item. It stays a menu rather than a
                  bare button: Remove is destructive, and a deliberate extra
                  click before it is worth keeping beside the title action.
                  Unlike "View teammate" it does
                  not duplicate the card's own action, and unlike Budget it is
                  not per-teammate configuration that reads better on a
                  detail page — it is the one roster-level action an operator
                  reaches for while scanning many cards deciding which to
                  prune, and moving it off the grid would trade a fast,
                  discoverable one-hop delete for an extra full-page
                  navigation with no offsetting benefit.
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
          The desks this teammate sits on, one chip per desk (issue #1440). The
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
          {workload && <WorkloadLine workload={workload} />}
          <DailyBudgetLine member={member} setByLabel={setByLabel} />
        </div>
        {/*
          The card's footer is gone with the Inbox switch it existed to hold
          (issue #1190).

          The switch was the only control on the card that *wrote* to the host,
          at the same weight as the name, on a grid of thirteen — a card is for
          recognising a teammate, and a mis-click while scanning silently
          changed a per-teammate setting with no confirmation. It moved to the
          teammate's own page, which already reported inbox state as a badge and
          offered no way to change it. See `AgentDetailView`.

          Its companion — a "Teammate" badge — went with it rather than being
          left behind a border rule on its own. On a page whose every card is a
          teammate it labelled nothing, and a bordered band holding one inert
          chip reads as something that failed to load.
        */}
      </CardContent>
    </Card>
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
 * The teammate's daily spend cap and what it has spent against it today.
 *
 * Renders nothing at all for an uncapped teammate: the host omits the fields
 * entirely rather than sending zeros, so absence means "spends freely" and must
 * not be drawn as "$0.00/day". Once spend reaches the cap the line turns
 * destructive — that teammate's dispatch is paused until 00:00 UTC, and the
 * card is where an operator will look to find out why it went quiet.
 */
function DailyBudgetLine({
  member,
  setByLabel,
}: {
  member: TeamMember;
  setByLabel?: string;
}) {
  const cap = member.budgetUsdDaily;
  const attribution =
    setByLabel && member.budgetSetAtMillis !== undefined ? (
      <p data-testid="team-budget-attribution" className="text-xs text-muted-foreground">
        {cap === undefined ? "Uncapped by" : "Set by"} {setByLabel} ·{" "}
        {new Date(member.budgetSetAtMillis).toLocaleDateString()}
      </p>
    ) : null;

  // No cap: render nothing but the attribution, if a human deliberately removed
  // one. "Uncapped by Ana" and "nobody ever capped this" are different facts,
  // and only the first has a line.
  if (cap === undefined) return attribution;

  const spent = member.spentTodayUsd ?? 0;
  const overBudget = spent >= cap;
  const usd = (n: number) => `$${n.toFixed(2)}`;
  return (
    <div className="space-y-0.5">
      <p
        data-testid="team-budget"
        className={cn("text-xs", overBudget ? "text-destructive" : "text-muted-foreground")}
      >
        {usd(cap)}/day · {usd(spent)} spent today
        {overBudget && " · paused until 00:00 UTC"}
      </p>
      {attribution}
    </div>
  );
}

// `BudgetDialog` — entering a daily cap — moved to `AgentDetailView.tsx`
// (issue #1206), alongside the editing controls it belongs to now.

function AddMemberDialog({
  open,
  onOpenChange,
  onAdd,
  canSetBudget,
  client,
  company,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  onAdd: (fields: AddMemberFields) => void;
  /** Whether to offer the cap field — setting one is admin-only on the host. */
  canSetBudget: boolean;
  /** For the copilot's draft call (issue #1776) — this dialog writes nothing. */
  client: OpenCompanyClient;
  company: string | null;
}) {
  // The same three authored fields the detail view edits, held in the same
  // shape (issue #264) so "Add teammate" and "Edit teammate" cannot drift into
  // two different sets of labels for one set of values.
  const [draft, setDraft] = useState<AgentDraft>(emptyDraft);
  const [inbox, setInbox] = useState(false);
  const [budget, setBudget] = useState("");
  /**
   * The cognition path this company booted onto (issue #1776), read while the
   * dialog is open so the copilot can say "no model is configured" rather than
   * offering a draft that can only come back refused. `null` until the check
   * settles and on a host without the route, which leaves it enabled — see
   * `AgentDetailView` for why that is the right way to be wrong.
   */
  const [cognition, setCognition] = useState<CognitionPath | null>(null);
  /**
   * The required fields still blank (issue #1776).
   *
   * Read from `AGENT_FIELDS` rather than re-spelled as
   * `!draft.name.trim() || !draft.role.trim()`, which is what this button
   * checked before: two forms deciding separately what a teammate needs is how
   * they drift, and the edit form asks the same question one import away.
   */
  const missing = missingRequired(draft);

  useEffect(() => {
    if (!open) return;
    let live = true;
    (async () => {
      try {
        const status = await getInferenceStatus(client, company);
        if (live) setCognition(status.cognition);
      } catch {
        if (live) setCognition(null);
      }
    })();
    return () => {
      live = false;
    };
  }, [open, client, company]);

  function reset() {
    setDraft(emptyDraft());
    setInbox(false);
    setBudget("");
  }

  const parsedBudget = Number(budget);
  // A blank field means "no cap", which is the default for a new teammate —
  // so it is left out of the request entirely rather than sent as 0.
  const budgetUsdDaily =
    budget.trim() !== "" && Number.isFinite(parsedBudget) && parsedBudget >= 0
      ? parsedBudget
      : undefined;
  const budgetInvalid = budget.trim() !== "" && budgetUsdDaily === undefined;

  function submit() {
    if (!draft.name.trim() || !draft.role.trim() || budgetInvalid) return;
    onAdd({
      name: draft.name,
      role: draft.role,
      description: draft.description,
      instructions: draft.instructions,
      inbox,
      budgetUsdDaily,
    });
    reset();
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) reset();
      }}
    >
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Add teammate</DialogTitle>
          <DialogDescription>Add a teammate to your company&apos;s roster.</DialogDescription>
        </DialogHeader>
        <AgentFields
          idPrefix="member"
          draft={draft}
          onChange={(key: AgentFieldKey, value) => setDraft((d) => ({ ...d, [key]: value }))}
          copilot={(key) =>
            key === "description" || key === "instructions" ? (
              <FieldCopilot
                field={key}
                // No id to address — this teammate does not exist yet — so the
                // fields being typed ride the request. Everything else the
                // draft is grounded in still comes from the record host-side.
                onTurn={(conversation) =>
                  draftNewAgentField(client, company, key, conversation, {
                    role: draft.role,
                    name: draft.name,
                    description: draft.description,
                    instructions: draft.instructions,
                  })
                }
                onAccept={(text) => setDraft((d) => ({ ...d, [key]: text }))}
                // A draft is written FROM the role, so there is nothing to
                // write one from until it is filled in — the same rule the
                // host enforces, said here before the operator meets it as a
                // refusal.
                disabled={!draft.role.trim() || cognition === "echo"}
                disabledNotice={
                  cognition === "echo"
                    ? "No model is configured, so the copilot can't draft yet."
                    : !draft.role.trim()
                      ? "Give this teammate a role first — the copilot drafts from it."
                      : undefined
                }
              />
            ) : null
          }
        />
        {canSetBudget && (
          <div className="grid gap-2">
            <Label htmlFor="member-budget-new">Daily budget (optional)</Label>
            <Input
              id="member-budget-new"
              type="number"
              min={0}
              step="0.01"
              inputMode="decimal"
              value={budget}
              onChange={(e) => setBudget(e.target.value)}
              placeholder="e.g. 5.00 — leave blank for no cap"
              data-testid="team-add-budget"
            />
          </div>
        )}
        <label className="flex items-center justify-between rounded-lg border p-3">
          <span className="flex items-center gap-2 text-sm">
            <Mail className="size-4 text-muted-foreground" /> Give this teammate an inbox
          </span>
          <Switch checked={inbox} onCheckedChange={setInbox} aria-label="Give this teammate an inbox" />
        </label>
        <DialogFooter className="items-center">
          {/* Why the button is dead, next to the button (issue #1776) — the
              same answer the edit form gives, from the same definition, so the
              two forms cannot come to disagree about what a teammate needs. */}
          {missing.length > 0 && (
            <p className="mr-auto text-2xs text-muted-foreground" data-testid="team-add-blocked">
              {missing.map((field) => field.label).join(" and ")}{" "}
              {missing.length > 1 ? "are" : "is"} required.
            </p>
          )}
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button
            onClick={submit}
            disabled={missing.length > 0 || budgetInvalid}
          >
            Add teammate
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
