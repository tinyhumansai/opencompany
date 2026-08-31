// The company org chart (issue #311): the surface that replaces the removed
// Desks page rather than restoring it.
//
// #302 closed the way in to desk management and said so plainly — "desk
// creation and membership editing become unreachable ... editable by hand in
// the manifest and nowhere else" — and accepted that as temporary. This is the
// destination. Creation, deletion, membership and the lead are all routed
// through the hierarchy, so there is one structure surface and not a flat list
// beside it.
//
// The tree is three levels — company, desk, seat — and that cap is structural,
// not checked: see `lib/org.ts`. The DOM says so too. Every node carries an
// `aria-level`, and no code path here can emit `aria-level="4"`, which is what
// the e2e spec asserts.

import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type DragEvent,
  type ReactNode,
} from "react";
import {
  ChevronDown,
  ChevronRight,
  ChevronUp,
  Crown,
  Lock,
  Plus,
  Trash2,
  UserPlus,
  Users,
  X,
} from "lucide-react";
import { toast } from "sonner";

import { listPeople } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type DeskDto, type TeamMemberDto } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { TeammateAvatar } from "@/components/teammate-avatar";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Skeleton } from "@/components/ui/skeleton";
import { personName } from "@/lib/person";
import { roleSubtitle, toneFor, type TeamMember } from "@/lib/team";
import {
  addMemberFailure,
  addOutcome,
  NO_TEAM_WRITE_PLANE,
  reportAddMember,
  type AddMemberOutcome,
  type MissedStep,
} from "@/lib/member-feedback";
import {
  addableTo,
  buildOrgTree,
  canDragAcrossDesks,
  reorderedIds,
  reorderedIdsAfterDrop,
  summarize,
  type OrgDesk,
  type OrgPerson,
  type OrgSeat,
  type OrgTree,
  type Provenance,
} from "@/lib/org";
import { cn } from "@/lib/utils";
import { DeskCreateDialog } from "@/views/company/DeskCreateDialog";
import {
  AddMemberDialog,
  type NewMemberFields,
} from "@/views/chat/AddMemberDialog";

const SEAT_MIME = "application/x-opencompany-seat";

/**
 * The seat currently being dragged, if any — lifted to `Chart` (issue #1227).
 *
 * `draggingIndex` used to live inside each `DeskNode`, which meant the desk
 * you dropped *onto* had never heard of a seat coming from a *different*
 * desk's `DeskNode` instance — that silence was the whole bug. Lifting this
 * one level, to the common ancestor of every `DeskNode`, is what lets a
 * target desk answer "what's being dragged, and can I take it" instead of
 * "I don't recognise this drop."
 *
 * `provenance` travels with it because that answer differs by desk: a
 * same-desk reorder only ever calls `setDeskOrder`, which is fine for a
 * blueprint seat, but a cross-desk move calls `removeDeskMember` on the
 * source — and the host refuses that for a blueprint seat. The gate needs to
 * know which kind of seat this is before a drop is even accepted, not after
 * the host says no.
 */
interface DragSeat {
  deskId: string;
  index: number;
  seatId: string;
  seatName: string;
  provenance: Provenance;
}

/**
 * Where a teammate named on this chart opens: `#/team/<agentId>`, the sub-page
 * `TeamView` already routes to `AgentDetailView` (issue #1102).
 *
 * A **link**, not a click handler on a `div`. The console routes on the hash,
 * so an `<a href>` is the real address: middle-click and cmd-click open a
 * second console, the browser shows the target on hover, and the keyboard
 * reaches it without this file re-implementing Enter/Space and a focus ring.
 *
 * The id is the one the desk itself names — `OrgSeat.id` is `DeskDto.members[i]`
 * and `TeamMember.id` is the roster id, and `buildOrgTree` resolves the seat by
 * matching those two. That is exactly the id `AgentDetailView` asks the host
 * for, so no translation is needed here — and none should be invented.
 *
 * `null` for an id that is blank or missing, which is the whole point of
 * routing through this function: a teammate with no usable id must render as
 * plain text rather than as a link to `#/team/undefined`, which is a page that
 * cannot exist and would report the teammate as deleted.
 */
function teamHref(agentId: string | null | undefined): string | null {
  const id = agentId?.trim();
  return id ? `#/team/${encodeURIComponent(id)}` : null;
}

/**
 * Why a cross-desk drag was refused, for a blueprint seat (issue #1227).
 *
 * A blueprint seat is declared in the manifest, and the host refuses to
 * remove a manifest-declared member from its desk — that is a real backend
 * invariant, not a frontend bug to work around. The old behaviour was to say
 * nothing at all when a cross-desk drop landed anywhere; saying *why* here is
 * the whole fix for that seat's half of the issue, not a workaround for the
 * refusal itself.
 */
function blueprintMoveRefusal(seatName: string): string {
  return `${seatName} is a blueprint member of their current desk — the manifest still declares them there, so they can't be moved to another desk. Same-desk reordering still works.`;
}

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * A desk to bring into view on arrival — the second segment of
   * `#/company/<deskId>`, which the chat member pane links to (issue #485).
   *
   * Best-effort on purpose. `useHashView` hands the segment back unvalidated,
   * this chart loads over the network, and a link outlives the desk it names —
   * so an id this chart does not draw is a **silent no-op**, never an error. A
   * stale bookmark should show the company, not a banner about it.
   *
   * The hash is never rewritten from here either: `#/company/<deskId>` is a
   * shareable address, and canonicalising it away would break the link the
   * operator just followed.
   */
  focusDeskId?: string | null;
  /**
   * Return to the roster at `#/company` (issue #1193).
   *
   * The chart is a destination under the Company page rather than a mode of it,
   * so it owes the operator a way back — the same debt any sub-page has.
   * Optional, so the chart still stands alone.
   */
  onBack?: () => void;
}

type Load = "loading" | "ready" | "error";

export function OrgChartView({ client, company, focusDeskId, onBack }: Props) {
  const [load, setLoad] = useState<Load>("loading");
  const [tree, setTree] = useState<OrgTree | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  const [addMemberOpen, setAddMemberOpen] = useState(false);
  const [addMemberDeskId, setAddMemberDeskId] = useState<string | null>(null);
  // A generation token so a response for a company we have navigated away from
  // cannot overwrite the one on screen. Same guard `SkillsView` uses.
  const gen = useRef(0);
  /** Which desk id the arriving link has already been honoured for. */
  const focused = useRef<string | null>(null);
  /** The desk currently wearing the arrival ring, if any. */
  const [focusMark, setFocusMark] = useState<string | null>(null);
  const chartRef = useRef<HTMLDivElement | null>(null);

  /**
   * Re-read the whole chart. Answers whether it landed.
   *
   * A read superseded by a newer one counts as landed: another `boot` owns the
   * screen and will settle it, so reporting a reload failure for it would warn
   * about a chart nobody is looking at. Only the catch is a real miss, and
   * `addMember` is the only caller that asks — everything else fires and
   * forgets, which is why this reports rather than rejects.
   */
  const boot = useCallback(async (): Promise<boolean> => {
    const mine = ++gen.current;
    try {
      // Desks are the only required half — they are the chart. The roster and
      // the people list are best-effort: a host that 404s `/team` still has a
      // structure worth drawing, it just cannot name who fills the seats, and
      // `buildOrgTree` marks those seats unknown rather than dropping them.
      const [desks, roster, people, status] = await Promise.all([
        client.listDesks(company) as Promise<DeskDto[]>,
        client.listTeam(company).catch(() => [] as TeamMemberDto[]),
        listPeople(client, company)
          .then((rows) =>
            rows.map((p): OrgPerson => ({
              id: p.id,
              // Through `personName`, so a person is called the same thing
              // here as in chat and in the mail the host sends them.
              name: personName(p),
              email: p.email,
              role: p.role,
            })),
          )
          .catch(() => [] as OrgPerson[]),
        client.status(company).catch(() => null),
      ]);
      if (mine !== gen.current) return true;
      // Cleared here rather than at the top of the write that triggered this
      // read: since #1099 the banner belongs to the load alone, so a chart that
      // loads is the only thing that can retire the message saying it did not.
      setError(null);
      setTree(
        buildOrgTree(
          status?.name || company || "This company",
          desks,
          roster,
          people,
        ),
      );
      setLoad("ready");
      return true;
    } catch (e) {
      if (mine !== gen.current) return true;
      // A failed `/desks` is a real error, not an empty company. Inventing an
      // empty chart here would tell the operator their desks are gone.
      setError(
        e instanceof Error ? e.message : "Could not load the org chart.",
      );
      setLoad("error");
      return false;
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void boot();
    return () => {
      gen.current++;
    };
  }, [boot]);

  /**
   * Bring the linked-to desk into view, once per id.
   *
   * Once, because every write here refetches the whole chart: re-running this
   * on each refetch would yank the page back to the linked desk while the
   * operator is editing a different one. `focused` records the id already
   * honoured rather than a boolean, so following a second link still works.
   *
   * The desk is also given DOM focus, not just a scroll position. A ring is
   * invisible to a screen reader, and "where did that link land me" is exactly
   * the question focus answers; `preventScroll` keeps `scrollIntoView`'s
   * centring rather than letting the browser re-scroll to its own idea of
   * visible.
   *
   * An id with no node — a deleted desk, a typo, a chart that 404'd — falls
   * through without marking itself honoured, so a desk that appears later
   * (created in another tab, or a refetch that finally succeeds) is still
   * caught.
   */
  /**
   * Forget the honoured id when the route target itself changes.
   *
   * `focused` exists to survive a *refetch* (same target, new tree), not a
   * *navigation*. Without this, leaving `#/company/<id>` for the bare chart or
   * a stale id and then coming back to the same desk hits the `focused.current
   * === focusDeskId` guard and the link silently does nothing the second time.
   * Clearing the mark here also stops an unknown target from inheriting the
   * previous desk's ring.
   *
   * Keyed on the target only — `tree` is deliberately absent, so a refetch
   * still cannot yank the operator back to the linked desk mid-edit.
   */
  useEffect(() => {
    focused.current = null;
    setFocusMark(null);
  }, [company, focusDeskId]);

  useEffect(() => {
    if (load !== "ready" || !focusDeskId || focused.current === focusDeskId)
      return;
    const node = chartRef.current?.querySelector<HTMLElement>(
      `[data-desk-id="${CSS.escape(focusDeskId)}"]`,
    );
    if (!node) return;
    focused.current = focusDeskId;
    node.scrollIntoView({ block: "center", behavior: "smooth" });
    node.focus({ preventScroll: true });
    setFocusMark(focusDeskId);
  }, [load, focusDeskId, tree]);

  /**
   * Drop the ring on the operator's first move, rather than after a guessed
   * number of milliseconds.
   *
   * A timer has to be picked against a chart that loads over the network: too
   * short and the ring expires before a slow load painted it, too long and it
   * outstays the moment it was drawn for. A click or a keypress is the actual
   * signal that the desk has been found.
   */
  useEffect(() => {
    if (!focusMark) return;
    const clear = () => setFocusMark(null);
    window.addEventListener("pointerdown", clear, { once: true });
    window.addEventListener("keydown", clear, { once: true });
    return () => {
      window.removeEventListener("pointerdown", clear);
      window.removeEventListener("keydown", clear);
    };
  }, [focusMark]);

  /**
   * Run a write, then re-read the whole chart.
   *
   * Refetch rather than patch in place: every write here changes something the
   * host derives (the effective member union, the lead, the overlay subset), so
   * a local edit would be a second implementation of rules the host already
   * owns — and the two would drift.
   */
  async function mutate(key: string, run: () => Promise<unknown>) {
    setBusy(key);
    try {
      await run();
      await boot();
    } catch (e) {
      // Toasted, not banked in the banner above the chart (issue #1099). The
      // banner is the page's own state — it belongs to a chart that could not
      // be *loaded*, and it sits with the Retry button that clears it. A write
      // the operator just attempted is an action, and every other action in
      // this console answers in a toast.
      toast.error(
        e instanceof Error ? e.message : "Something went wrong. Try again.",
      );
    } finally {
      setBusy(null);
    }
  }

  /**
   * Company creation is durable structure, so a host without the team write
   * plane must explain the refusal rather than borrow ChatView's local-only
   * fallback. A local row could not be placed on a desk and would vanish on
   * the next chart read.
   */
  async function addMember(fields: NewMemberFields) {
    const deskId = addMemberDeskId;
    setBusy("add-member");
    // Whether the host has the teammate, which decides whether the chart needs
    // re-reading on the way out. A desk add that fails after the teammate is
    // created leaves the two disagreeing: the roster has someone the chart has
    // never heard of, so the message telling the operator to place them by hand
    // would point at a dropdown that does not list them yet.
    let createdOnHost = false;
    // What to say once the chart has been re-read — decided here, raised after
    // `boot()`, so a refetch that contradicts the write contradicts the message
    // too rather than arriving a beat behind it.
    let outcome: AddMemberOutcome;
    // Every step of the ask that did not land, in the order the operator met
    // them. Empty is the only thing that earns "Added <name>.".
    const missed: MissedStep[] = [];
    try {
      let created: TeamMemberDto;
      try {
        created = await client.addTeamMember(
          {
            name: fields.name,
            role: fields.role,
            description: fields.description || undefined,
          },
          company,
        );
        createdOnHost = true;
      } catch (e) {
        if (e instanceof ApiError && e.status === 404) {
          // No local-only fallback here, unlike the roster and the chat empty
          // state: a console-only teammate has no host id to place on a desk
          // and would vanish on the next chart read.
          throw new Error(
            `${NO_TEAM_WRITE_PLANE} They can't be created from the Company page.`,
          );
        }
        throw e;
      }
      if (deskId) {
        try {
          await client.addDeskMember(deskId, created.id, company);
        } catch (e) {
          // Created but unplaced — a real half-landing, and the operator has
          // to know which half, because the fix is on the chart in front of
          // them rather than in the dialog they just closed.
          missed.push({
            what: `they couldn't be added to that desk: ${e instanceof Error ? e.message : "unknown error"}`,
            fix: "They're on the roster — drag them onto the desk from the chart.",
          });
        }
      }
      if (!(await boot())) {
        // The chart is on its error state behind this toast. Congratulating the
        // operator over a banner saying the chart could not be loaded is the
        // contradiction #1099 set out to remove, not one to add.
        missed.push({
          what: "the chart couldn't be read back",
          fix: "Retry to see where they landed.",
        });
      }
      outcome = addOutcome(fields.name, missed);
      setAddMemberOpen(false);
    } catch (e) {
      setAddMemberOpen(false);
      outcome = addMemberFailure(e, "Could not create teammate.");
      if (createdOnHost) {
        await boot();
      }
    } finally {
      setBusy(null);
    }
    reportAddMember(outcome);
  }

  return (
    <div ref={chartRef} className="flex min-h-0 flex-1 flex-col">
      {/*
        Issue #1207 put the actions on the heading's row rather than on a row of
        their own; `PageHeader` is where that shape lives now (issue #1763), and
        `desks-header` still names the row the two share.

        The breadcrumb rides in `eyebrow`, above the title inside the same bar:
        a sub-page of Company says where it is and offers the way back (issue
        #1193), and that belongs with the page's name rather than floating over
        the content beneath it.
      */}
      <PageHeader
        title="Desks"
        width="4xl"
        rowTestId="desks-header"
        eyebrow={
          onBack && (
            <nav aria-label="Breadcrumb">
            <ol className="flex flex-wrap items-center gap-1 text-sm">
              <li>
                <Button
                  variant="ghost"
                  size="sm"
                  className="-ml-2 h-7 px-2 text-muted-foreground"
                  onClick={onBack}
                  data-testid="desks-breadcrumb-company"
                >
                  Company
                </Button>
              </li>
              <li aria-hidden className="text-muted-foreground">
                <ChevronRight className="size-3.5" />
              </li>
              <li aria-current="page" className="min-w-0 truncate font-medium">
                Desks
              </li>
            </ol>
            </nav>
          )
        }
        description={
          <>
            How your company is organised: the desks it works from and who
            staffs each one. Add a desk, move someone between desks, or change
            who leads.
          </>
        }
        actions={
          <>
              <Button
                size="sm"
                variant="outline"
                disabled={load === "loading"}
                onClick={() => setCreateOpen(true)}
              >
                <Plus className="mr-1.5 size-4" />
                New desk
              </Button>
              <Button
                size="sm"
                variant="outline"
                disabled={load === "loading"}
                onClick={() => {
                  setAddMemberDeskId(null);
                  setAddMemberOpen(true);
                }}
              >
                <UserPlus className="mr-1.5 size-4" />
                Add teammate
              </Button>
          </>
        }
      />
      <div className="mx-auto min-h-0 w-full max-w-4xl flex-1 space-y-6 overflow-y-auto px-4 py-6">

        {error && (
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        {load === "loading" ? (
          <div className="space-y-3">
            {Array.from({ length: 3 }).map((_, i) => (
              <Skeleton key={i} className="h-24 rounded-xl" />
            ))}
          </div>
        ) : load === "error" ? (
          <div className="flex min-h-40 flex-col items-center justify-center gap-3 rounded-xl border border-dashed text-sm text-muted-foreground">
            The org chart could not be loaded.
            <Button size="sm" variant="outline" onClick={() => void boot()}>
              Retry
            </Button>
          </div>
        ) : (
          tree && (
            <>
              <Chart
                tree={tree}
                busy={busy}
                focusMark={focusMark}
                onCreate={() => setCreateOpen(true)}
                onCreateMember={(desk) => {
                  setAddMemberDeskId(desk.id);
                  setAddMemberOpen(true);
                }}
                onAdd={(desk, agentId) =>
                  void mutate(`${desk.id}:${agentId}`, () =>
                    client.addDeskMember(desk.id, agentId, company),
                  )
                }
                onRemove={(desk, agentId) =>
                  void mutate(`${desk.id}:${agentId}`, () =>
                    client.removeDeskMember(desk.id, agentId, company),
                  )
                }
                onMove={(desk, index, direction) => {
                  const next = reorderedIds(desk, index, direction);
                  if (!next) return;
                  void mutate(`${desk.id}:${desk.seats[index].id}`, () =>
                    client.setDeskOrder(desk.id, next, company),
                  );
                }}
                onReorder={(desk, fromIndex, toIndex) => {
                  const next = reorderedIdsAfterDrop(desk, fromIndex, toIndex);
                  if (!next) return;
                  void mutate(`drag:${desk.id}`, () =>
                    client.setDeskOrder(desk.id, next, company),
                  );
                }}
                onMoveAcrossDesks={(fromDesk, seatId, toDesk) =>
                  void mutate(`move:${seatId}`, async () => {
                    // The host has no "move" verb, only add and remove — so a
                    // cross-desk move is those two calls plus one refetch
                    // (`mutate` already does the refetch). The add is not
                    // wrapped in its own try/catch: nothing has changed on the
                    // host yet if it fails, so `mutate`'s own catch-and-toast
                    // is the right place for that failure to land.
                    await client.addDeskMember(toDesk.id, seatId, company);
                    try {
                      await client.removeDeskMember(
                        fromDesk.id,
                        seatId,
                        company,
                      );
                    } catch (e) {
                      // Half-landed: the teammate is now on both desks, which
                      // is a real, visible inconsistency the operator has to
                      // resolve by hand — silently swallowing this would be
                      // exactly the kind of no-op #1227 is about.
                      throw new Error(
                        `Added to ${toDesk.name}, but couldn't remove them from ${fromDesk.name}: ${
                          e instanceof Error ? e.message : "unknown error"
                        }. They're on both desks now — remove them from ${fromDesk.name} by hand.`,
                      );
                    }
                  })
                }
                onDelete={(desk) =>
                  void mutate(`delete:${desk.id}`, () =>
                    client.deleteDesk(desk.id, company),
                  )
                }
              />
              <Unplaced tree={tree} />
            </>
          )
        )}
      </div>

      <DeskCreateDialog
        client={client}
        company={company}
        open={createOpen}
        onOpenChange={setCreateOpen}
        onCreated={() => void boot()}
      />
      <AddMemberDialog
        open={addMemberOpen}
        onOpenChange={setAddMemberOpen}
        onAdd={(fields) => void addMember(fields)}
      />
    </div>
  );
}

/**
 * The tree proper.
 *
 * `role="tree"` with an explicit `aria-level` on every node, rather than nested
 * lists left to the reader: the level is the whole point of this surface, so it
 * is stated rather than implied by indentation a screen reader cannot see.
 */
function Chart({
  tree,
  busy,
  focusMark,
  onCreate,
  onCreateMember,
  onAdd,
  onRemove,
  onMove,
  onReorder,
  onMoveAcrossDesks,
  onDelete,
}: {
  tree: OrgTree;
  busy: string | null;
  /** The desk a `#/company/<deskId>` link landed on, ringed until first input. */
  focusMark: string | null;
  onCreate: () => void;
  onCreateMember: (desk: OrgDesk) => void;
  onAdd: (desk: OrgDesk, agentId: string) => void;
  onRemove: (desk: OrgDesk, agentId: string) => void;
  onMove: (desk: OrgDesk, index: number, direction: "up" | "down") => void;
  onReorder: (desk: OrgDesk, fromIndex: number, toIndex: number) => void;
  onMoveAcrossDesks: (
    fromDesk: OrgDesk,
    seatId: string,
    toDesk: OrgDesk,
  ) => void;
  onDelete: (desk: OrgDesk) => void;
}) {
  // The drag source, lifted here rather than into `DeskNode` — see `DragSeat`.
  // This is what lets a *different* desk's drop handlers know a seat is being
  // dragged at all, which is the fix for #1227's cross-desk silent no-op.
  const [dragSeat, setDragSeat] = useState<DragSeat | null>(null);
  return (
    <div role="tree" aria-label="Company org chart" className="space-y-3">
      <div
        role="treeitem"
        aria-level={1}
        aria-expanded="true"
        aria-selected="false"
      >
        <div className="rounded-xl border bg-muted/40 px-4 py-3">
          <p className="font-medium">{tree.companyName}</p>
          <p className="text-xs text-muted-foreground">{summarize(tree)}</p>
        </div>

        {tree.desks.length === 0 ? (
          <div className="mt-3 flex min-h-32 flex-col items-center justify-center gap-3 rounded-xl border border-dashed text-sm text-muted-foreground">
            <Users className="size-5" />
            This company has no desks yet.
            <Button size="sm" variant="outline" onClick={onCreate}>
              <Plus className="mr-1.5 size-4" />
              Create your first desk
            </Button>
          </div>
        ) : (
          <div role="group" className="mt-3 space-y-3 border-l pl-4">
            {tree.desks.map((desk) => (
              <DeskNode
                key={desk.id}
                desk={desk}
                addable={addableTo(tree, desk)}
                busy={busy}
                focused={focusMark === desk.id}
                dragSeat={dragSeat}
                onAdd={(agentId) => onAdd(desk, agentId)}
                onCreateMember={() => onCreateMember(desk)}
                onRemove={(agentId) => onRemove(desk, agentId)}
                onMove={(index, direction) => onMove(desk, index, direction)}
                onSeatDragStart={(seat, index) =>
                  setDragSeat({
                    deskId: desk.id,
                    index,
                    seatId: seat.id,
                    seatName: seat.name,
                    provenance: seat.provenance,
                  })
                }
                onSeatDragEnd={() => setDragSeat(null)}
                onReorder={(fromIndex, toIndex) =>
                  onReorder(desk, fromIndex, toIndex)
                }
                onMoveIn={() => {
                  if (!dragSeat || dragSeat.deskId === desk.id) return;
                  const fromDesk = tree.desks.find(
                    (d) => d.id === dragSeat.deskId,
                  );
                  if (!fromDesk) return;
                  onMoveAcrossDesks(fromDesk, dragSeat.seatId, desk);
                }}
                onDelete={() => onDelete(desk)}
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

/** One desk and its seats — levels 2 and 3. */
function DeskNode({
  desk,
  addable,
  busy,
  focused,
  dragSeat,
  onCreateMember,
  onAdd,
  onRemove,
  onMove,
  onSeatDragStart,
  onSeatDragEnd,
  onReorder,
  onMoveIn,
  onDelete,
}: {
  desk: OrgDesk;
  addable: TeamMember[];
  busy: string | null;
  /** This desk is the one a `#/company/<deskId>` link asked for. */
  focused: boolean;
  /** The seat currently being dragged anywhere on the chart, if any. */
  dragSeat: DragSeat | null;
  onCreateMember: () => void;
  onAdd: (agentId: string) => void;
  onRemove: (agentId: string) => void;
  onMove: (index: number, direction: "up" | "down") => void;
  onSeatDragStart: (seat: OrgSeat, index: number) => void;
  onSeatDragEnd: () => void;
  onReorder: (fromIndex: number, toIndex: number) => void;
  /** A seat from another desk has been dropped onto this one. */
  onMoveIn: () => void;
  onDelete: () => void;
}) {
  const locked = busy !== null;
  // Whether any seat on this desk was added at runtime rather than declared by
  // the manifest. Only on such a "mixed provenance" desk does the Blueprint
  // badge still earn its place — everywhere else the whole desk is blueprint,
  // so a muted lock says it with far less noise than a badge on every seat.
  const hasOverlaySeats = desk.seats.some(
    (s) => s.provenance === "overlay",
  );
  // Whether the seat currently being dragged (from anywhere on the chart)
  // could land on *this* desk: it must come from a different desk, and it
  // must be an overlay seat — the host refuses to remove a blueprint member
  // from its desk, so accepting the drop here would only fail one step later
  // with a 409 the operator never asked to see (issue #1227).
  const crossDeskDropAllowed =
    dragSeat !== null &&
    dragSeat.deskId !== desk.id &&
    canDragAcrossDesks({ provenance: dragSeat.provenance });
  const crossDeskDropBlocked =
    dragSeat !== null &&
    dragSeat.deskId !== desk.id &&
    !canDragAcrossDesks({ provenance: dragSeat.provenance });
  /**
   * Whether this desk has already told the operator, for the drag in
   * progress, that it cannot take a blueprint seat.
   *
   * Fired on `dragenter` rather than `drop`: whether a browser lets `drop`
   * fire at all depends on whether *any* element preventDefaulted `dragover`
   * during the gesture, and this desk deliberately never does that for a
   * blocked seat (that's what draws the native "not allowed" cursor). A toast
   * that only fired from a `drop` handler could end up as silent as the bug
   * this fixes, on a browser that honours the cursor and never sends `drop`
   * at all. `dragenter` needs no such cooperation — it always fires.
   */
  const warnedRef = useRef(false);
  useEffect(() => {
    warnedRef.current = false;
  }, [dragSeat]);
  function dragEnterDesk() {
    if (crossDeskDropBlocked && dragSeat && !warnedRef.current) {
      warnedRef.current = true;
      toast.error(blueprintMoveRefusal(dragSeat.seatName));
    }
  }
  /**
   * Accept a drop landing on the desk's open space rather than on a specific
   * seat row — the only way an empty desk (or the space below its last seat)
   * can ever be a drop target, since there is no `Seat` there to catch the
   * event otherwise.
   */
  function dragOverGroup(event: DragEvent<HTMLDivElement>) {
    if (!crossDeskDropAllowed) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "move";
  }
  function dropOnGroup(event: DragEvent<HTMLDivElement>) {
    event.preventDefault();
    if (crossDeskDropAllowed) onMoveIn();
  }
  return (
    <div
      role="treeitem"
      aria-level={2}
      aria-expanded="true"
      aria-selected="false"
      // The desk's anchor for `#/company/<deskId>` (issue #485). A data
      // attribute rather than an `id`: the desk id is the host's, and minting
      // document-wide ids from it would collide with anything else on the page
      // that names the same desk.
      data-desk-id={desk.id}
      data-desk-focused={focused ? "true" : undefined}
      // Programmatically focusable, not tab-reachable: the arrival focus is
      // how a screen reader is told where the link landed, but a desk wrapper
      // is not a control and does not belong in the tab order.
      tabIndex={-1}
      // The desk-wide warning that a blueprint seat can't land here — see
      // `dragEnterDesk`. Placed on the whole wrapper, not just the seat
      // list, so entering over the header or the border counts too.
      onDragEnter={dragEnterDesk}
      className={cn(
        "scroll-mt-4 rounded-xl outline-none",
        focused && "ring-2 ring-primary ring-offset-2 ring-offset-background",
      )}
    >
      <div className="rounded-xl border">
        <div className="flex items-start justify-between gap-2 px-3 py-2.5">
          <div className="min-w-0">
            <p className="flex items-center gap-2 truncate font-medium">
              {desk.name}
              {desk.provenance === "blueprint" &&
                (hasOverlaySeats ? (
                  <Badge variant="secondary" className="shrink-0 text-3xs">
                    Blueprint
                  </Badge>
                ) : (
                  <Lock
                    role="img"
                    aria-label="Part of the company blueprint"
                    className="size-3.5 shrink-0 text-muted-foreground"
                  />
                ))}
            </p>
            {desk.description && (
              <p className="line-clamp-2 text-xs text-muted-foreground">
                {desk.description}
              </p>
            )}
          </div>
          {/* Only an operator-created desk can be deleted at runtime. A
              blueprint desk lives in version control, and the host refuses —
              so no button is offered rather than one that always fails. */}
          {desk.provenance === "overlay" && (
            <Button
              variant="ghost"
              size="icon"
              className="size-7 shrink-0 text-muted-foreground hover:text-destructive"
              aria-label={`Delete ${desk.name}`}
              disabled={locked}
              onClick={onDelete}
            >
              <Trash2
                className={cn(
                  "size-3.5",
                  busy === `delete:${desk.id}` && "opacity-50",
                )}
              />
            </Button>
          )}
        </div>

        <div
          role="group"
          className="space-y-1 border-t px-3 py-2"
          // The empty-space fallback drop target: an empty desk (or the space
          // below its last seat) has no `Seat` row to catch the event, so it
          // needs its own handlers to ever be a valid cross-desk drop target
          // (issue #1227).
          onDragOver={dragOverGroup}
          onDrop={dropOnGroup}
        >
          {desk.seats.length === 0 && (
            <p className="py-1 text-xs text-muted-foreground">
              Nobody staffs this desk yet.
            </p>
          )}
          {desk.seats.map((seat, index) => (
            <Seat
              key={seat.id}
              seat={seat}
              index={index}
              deskId={desk.id}
              deskName={desk.name}
              deskHasOverlaySeats={hasOverlaySeats}
              first={index === 0}
              last={index === desk.seats.length - 1}
              busy={busy === `${desk.id}:${seat.id}`}
              locked={locked}
              dragSeat={dragSeat}
              onUp={() => onMove(index, "up")}
              onDown={() => onMove(index, "down")}
              onRemove={() => onRemove(seat.id)}
              onDragStart={() => onSeatDragStart(seat, index)}
              onDragEnd={onSeatDragEnd}
              onReorderDrop={(fromIndex, toIndex) =>
                onReorder(fromIndex, toIndex)
              }
              onCrossDeskDrop={onMoveIn}
            />
          ))}

          {/*
            One control, two ways to staff a desk.

            This was two adjacent controls: a full-width "Add teammate" button
            that seated somebody already on the roster, and — flush against it,
            with no label — a `UserPlus` icon that *created* a teammate here.
            Three problems, all of them the same problem:

            - the labelled one said "Add teammate" and meant "add an existing
              one", while the page header's "New teammate" wore the identical
              icon to the unlabelled one beside it. "Add teammate" named two
              different actions on the same screen;
            - an icon button with no visible label, touching a button that
              already says the words, is not discoverable. Nobody looking for
              "define a new teammate on this desk" finds a bare glyph;
            - when every roster teammate was already seated, the labelled
              control went disabled and read "Everyone is on this desk" — so
              the only remaining way in was the affordance nobody can see.

            Now the button always says "Add teammate", is never disabled, and
            its menu carries both: whoever is left on the roster, then
            "New teammate…". "Everyone on the roster is already here" is a
            piece of information inside the menu rather than a dead trigger.
          */}
          <div className="pt-1">
            <DropdownMenu>
              <DropdownMenuTrigger
                render={
                  <Button
                    variant="outline"
                    size="sm"
                    className="w-full"
                    // Only an in-flight write holds this shut. A fully-staffed
                    // desk does not: creating a teammate here is still a thing
                    // an operator can do.
                    disabled={locked}
                  />
                }
              >
                <Plus className="size-4" />
                Add teammate
              </DropdownMenuTrigger>
              {/*
                A fixed width, not the trigger's. The trigger is full-bleed
                across a desk card — over a thousand pixels at 1440 — and the
                menu inherited it, so ten short names sat down the left edge of
                an enormous empty panel.
              */}
              <DropdownMenuContent align="start" className="w-64">
                {addable.length > 0 ? (
                  // Grouped, because `DropdownMenuLabel` is Base UI's
                  // `Menu.GroupLabel` and it throws outside a `Menu.Group` —
                  // a blank page, not a warning.
                  // The *group* scrolls, not the whole menu. Put the cap on
                  // the popup and "New teammate…" falls below the fold on any
                  // company with a roster — which is the one item that had to
                  // become findable for merging the two controls to be worth
                  // anything.
                  <DropdownMenuGroup className="max-h-64 overflow-y-auto">
                    <DropdownMenuLabel>On the roster</DropdownMenuLabel>
                    {addable.map((member) => (
                      <DropdownMenuItem
                        key={member.id}
                        onClick={() => onAdd(member.id)}
                      >
                        <TeammateAvatar
                          name={member.name}
                          avatar={member.avatar}
                          tone={member.tone}
                          className="size-5 shrink-0"
                        />
                        <span className="truncate">{member.name}</span>
                      </DropdownMenuItem>
                    ))}
                  </DropdownMenuGroup>
                ) : (
                  // Plain text, not a `GroupLabel`: there is no group here to
                  // label, and this is a statement about the roster rather
                  // than a heading over items.
                  <p className="px-1.5 py-1 text-xs text-muted-foreground">
                    Everyone on the roster is already here.
                  </p>
                )}
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  onClick={onCreateMember}
                  aria-label={`Add teammate to ${desk.name}`}
                >
                  <UserPlus className="size-4" />
                  New teammate…
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          </div>
        </div>
      </div>
    </div>
  );
}

/** One seat — level 3, the deepest a path here can go. */
function Seat({
  seat,
  index,
  deskId,
  deskName,
  deskHasOverlaySeats,
  first,
  last,
  busy,
  locked,
  dragSeat,
  onUp,
  onDown,
  onRemove,
  onDragStart,
  onDragEnd,
  onReorderDrop,
  onCrossDeskDrop,
}: {
  seat: OrgSeat;
  index: number;
  deskId: string;
  deskName: string;
  /** Whether this seat's desk mixes blueprint and overlay members. */
  deskHasOverlaySeats: boolean;
  first: boolean;
  last: boolean;
  busy: boolean;
  locked: boolean;
  /** The seat currently being dragged anywhere on the chart, if any. */
  dragSeat: DragSeat | null;
  onUp: () => void;
  onDown: () => void;
  onRemove: () => void;
  onDragStart: () => void;
  onDragEnd: () => void;
  /** Same-desk reorder: the dragged seat's own index, and where it landed. */
  onReorderDrop: (fromIndex: number, toIndex: number) => void;
  /** A seat from another desk landed on this desk (issue #1227). */
  onCrossDeskDrop: () => void;
}) {
  function startDrag(event: DragEvent<HTMLDivElement>) {
    onDragStart();
    event.dataTransfer.effectAllowed = "move";
    event.dataTransfer.setData(SEAT_MIME, `${deskId}:${index}`);
    event.dataTransfer.setData("text/plain", `${deskId}:${index}`);
  }

  /**
   * Whether dropping the seat currently in flight, here, is something this
   * row would honour: a same-desk reorder always is, and a cross-desk
   * landing only is when the source is an overlay seat — the host refuses to
   * remove a blueprint member from its desk, so a blueprint source can never
   * land anywhere but back where it started (issue #1227).
   */
  function dropAllowed(): boolean {
    if (!dragSeat) return false;
    return (
      dragSeat.deskId === deskId ||
      canDragAcrossDesks({ provenance: dragSeat.provenance })
    );
  }

  function dragOver(event: DragEvent<HTMLDivElement>) {
    if (!dropAllowed()) return; // no preventDefault: the browser draws its
    // own "not allowed" cursor for the rest of the gesture — the visible
    // refusal a blueprint-sourced cross-desk drag gets instead of silence.
    event.preventDefault();
    event.dataTransfer.dropEffect = "move";
  }

  function drop(event: DragEvent<HTMLDivElement>) {
    if (!dropAllowed() || !dragSeat) return;
    event.preventDefault();
    // Stop the desk's own fallback handler (on the seat-list container, for
    // the empty-desk case) from also seeing this same drop and repeating the
    // write it is about to trigger.
    event.stopPropagation();
    if (dragSeat.deskId === deskId) {
      onReorderDrop(dragSeat.index, index);
    } else {
      // Cross-desk: which row it lands on doesn't matter. The host has an
      // add verb, not an insert-at-position verb, so the whole desk is the
      // drop target and every row on it lands the seat the same way.
      onCrossDeskDrop();
    }
  }

  // Where this seat opens, or `null` when it opens nowhere. A seat the roster
  // cannot resolve is deliberately *not* a link: `#/team/<id>` for an id the
  // host has never heard of lands on the detail view's "no such teammate"
  // state, so offering the link would send the operator to a dead end to
  // discover what the badge beside the name already says.
  const href = seat.known ? teamHref(seat.id) : null;
  // Issue #1208: only when the role is not the name over again. A seat's two
  // strings come from one roster row, and the console's own name fallback
  // (`fromDto`) makes them identical for every agent a manifest declares
  // without a display name — which was every seat on this chart.
  const subtitle = roleSubtitle(seat.name, seat.role);

  const label = (
    <>
      <TeammateAvatar
        name={seat.name}
        avatar={seat.avatar}
        tone={toneFor(seat.id)}
        className="size-5 shrink-0"
      />
      {seat.lead && (
        <Crown
          role="img"
          aria-label="Desk lead"
          className="size-3.5 shrink-0 text-muted-foreground"
        />
      )}
      <span className={cn("truncate", !seat.known && "text-muted-foreground")}>
        {seat.name}
      </span>
      {subtitle && (
        <span className="truncate text-xs text-muted-foreground">
          {subtitle}
        </span>
      )}
      {/* A seat naming somebody the roster no longer has. Shown, not hidden:
          it is a fact about the structure only the operator can fix. */}
      {!seat.known && (
        <Badge variant="outline" className="shrink-0 text-3xs">
          Not on the roster
        </Badge>
      )}
    </>
  );

  return (
    <div
      role="treeitem"
      aria-level={3}
      aria-selected="false"
      draggable={!locked}
      data-seat-id={seat.id}
      onDragStart={startDrag}
      onDragEnd={onDragEnd}
      onDragOver={dragOver}
      onDrop={drop}
      className={cn(
        "flex cursor-grab items-center justify-between gap-2 rounded-md border px-3 py-2 text-sm active:cursor-grabbing",
        busy && "opacity-50",
      )}
    >
      {href ? (
        <a
          href={href}
          title={`Open ${seat.name}`}
          // The name is the target, not the whole row: the row is the drag
          // handle for re-ordering, and a full-row link would make every
          // attempt to drag a seat read as a click on it.
          //
          // `draggable={false}` so a drag that starts on the name is not the
          // browser's own drag-a-link gesture — it falls through to the row,
          // which is the draggable ancestor, and re-ordering keeps working
          // from anywhere on the seat.
          draggable={false}
          className="-mx-1 flex min-w-0 cursor-pointer items-center gap-1.5 rounded-md px-1 outline-none hover:bg-muted focus-visible:ring-3 focus-visible:ring-ring/50"
        >
          {label}
          <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />
        </a>
      ) : (
        <span className="flex min-w-0 items-center gap-1.5">{label}</span>
      )}
      <span className="flex shrink-0 items-center gap-0.5">
        {/* Moving the second seat up is how the lead changes: `members[0]` IS
            the lead, so there is no separate set-lead call to make. */}
        <Button
          variant="ghost"
          size="icon"
          className="size-6 text-muted-foreground hover:text-foreground"
          aria-label={`Move ${seat.name} up in ${deskName}`}
          // Global lock, not per-row: any in-flight write means the order on
          // screen is about to be replaced, and a second PUT computed from it
          // would be built on a stale list.
          disabled={locked || first}
          onClick={onUp}
        >
          <ChevronUp className="size-3.5" />
        </Button>
        <Button
          variant="ghost"
          size="icon"
          className="size-6 text-muted-foreground hover:text-foreground"
          aria-label={`Move ${seat.name} down in ${deskName}`}
          disabled={locked || last}
          onClick={onDown}
        >
          <ChevronDown className="size-3.5" />
        </Button>
        {seat.provenance === "overlay" ? (
          <Button
            variant="ghost"
            size="icon"
            className="size-6 text-muted-foreground hover:text-destructive"
            aria-label={`Remove ${seat.name} from ${deskName}`}
            disabled={locked}
            onClick={onRemove}
          >
            <X className="size-3.5" />
          </Button>
        ) : deskHasOverlaySeats ? (
          <Badge variant="secondary" className="shrink-0 text-3xs">
            Blueprint
          </Badge>
        ) : (
          <Lock
            role="img"
            aria-label="Part of the company blueprint"
            className="size-3.5 shrink-0 text-muted-foreground"
          />
        )}
      </span>
    </div>
  );
}

/**
 * Everyone the chart does not place: roster teammates on no desk, and the
 * humans who can sign in.
 *
 * Outside the tree on purpose. Neither has a position the company declares, and
 * putting them under a node would be inventing structure — which is the failure
 * the Overview graph already documents about its own derived departments.
 */
function Unplaced({ tree }: { tree: OrgTree }) {
  if (tree.unassigned.length === 0 && tree.people.length === 0) return null;
  return (
    <div className="space-y-4">
      <h2 className="sr-only">People outside desks</h2>
      {tree.unassigned.length > 0 && (
        <section className="space-y-2">
          <h3 className="text-sm font-medium text-muted-foreground">Not on a desk</h3>
          <p className="text-xs text-muted-foreground">
            Roster teammates the company has not staffed anywhere. Add them to a
            desk above.
          </p>
          <ul className="flex flex-wrap gap-1.5">
            {tree.unassigned.map((member) => {
              // These chips name roster teammates, so they open the same page
              // a seat does (issue #1102) — they were the worse half of that
              // bug, bordered pills that read as controls and did nothing.
              const href = teamHref(member.id);
              return (
                <li key={member.id}>
                  {href ? (
                    <a
                      href={href}
                      title={`Open ${member.name}`}
                      className="flex items-center gap-1.5 rounded-md border px-2 py-1 text-xs outline-none hover:bg-muted focus-visible:ring-3 focus-visible:ring-ring/50"
                    >
                      <TeammateAvatar
                        name={member.name}
                        avatar={member.avatar}
                        tone={member.tone}
                        className="size-5 shrink-0"
                      />
                      {member.name}
                      <ChevronRight className="size-3 shrink-0 text-muted-foreground" />
                    </a>
                  ) : (
                    // No usable id, so there is nothing to open. Rendered flat
                    // rather than as a pill: the border is what made the inert
                    // version of this chip a lie.
                    <InertChip title="This teammate has no id, so their page can't be opened.">
                      <TeammateAvatar
                        name={member.name}
                        avatar={member.avatar}
                        tone={member.tone}
                        className="size-5 shrink-0"
                      />
                      {member.name}
                    </InertChip>
                  )}
                </li>
              );
            })}
          </ul>
        </section>
      )}
      {tree.people.length > 0 && (
        <section className="space-y-2">
          <h3 className="text-sm font-medium text-muted-foreground">People</h3>
          <p className="text-xs text-muted-foreground">
            The humans who can sign in. Desks staff teammates, so the company
            declares no desk for a person, and this chart does not guess one.
          </p>
          <ul className="flex flex-wrap gap-1.5">
            {/* Deliberately inert, and styled to say so (issue #1102). A person
                is a console user, not an agent: `#/team/<id>` resolves against
                the roster, so pointing a person's id at it would 404 every
                time. There is no person detail page to link to instead, so the
                pill treatment is dropped rather than left promising one. */}
            {tree.people.map((person) => (
              <li key={person.id}>
                <InertChip title="People sign in to the console. Desks staff agents, so a person has no teammate page.">
                  {person.name}
                  <span className="ml-1.5">{person.role}</span>
                </InertChip>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}

/**
 * A name that is only a name.
 *
 * Filled and borderless, with the muted foreground a caption uses: the outlined
 * pill it replaces was indistinguishable from an outline button, which is why
 * #1102 reports these as clicked and inert. Nothing here reacts to a pointer —
 * no hover, no cursor change, no focus ring — because nothing here happens.
 */
function InertChip({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <span
      title={title}
      className="inline-block rounded-md bg-muted px-2 py-1 text-xs text-muted-foreground"
    >
      {children}
    </span>
  );
}
