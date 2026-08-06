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

import { useCallback, useEffect, useRef, useState } from "react";
import { ChevronDown, ChevronUp, Crown, Plus, Trash2, Users, X } from "lucide-react";

import { listPeople } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import type { DeskDto, TeamMemberDto } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Skeleton } from "@/components/ui/skeleton";
import {
  addableTo,
  buildOrgTree,
  reorderedIds,
  summarize,
  type OrgDesk,
  type OrgPerson,
  type OrgSeat,
  type OrgTree,
} from "@/lib/org";
import { cn } from "@/lib/utils";
import { DeskCreateDialog } from "@/views/company/DeskCreateDialog";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

type Load = "loading" | "ready" | "error";

export function OrgChartView({ client, company }: Props) {
  const [load, setLoad] = useState<Load>("loading");
  const [tree, setTree] = useState<OrgTree | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  // A generation token so a response for a company we have navigated away from
  // cannot overwrite the one on screen. Same guard `SkillsView` uses.
  const gen = useRef(0);

  const boot = useCallback(async () => {
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
            rows.map(
              (p): OrgPerson => ({
                id: p.id,
                name: p.displayName?.trim() || p.email.split("@")[0],
                email: p.email,
                role: p.role,
              }),
            ),
          )
          .catch(() => [] as OrgPerson[]),
        client.status(company).catch(() => null),
      ]);
      if (mine !== gen.current) return;
      setTree(buildOrgTree(status?.name || company || "This company", desks, roster, people));
      setLoad("ready");
    } catch (e) {
      if (mine !== gen.current) return;
      // A failed `/desks` is a real error, not an empty company. Inventing an
      // empty chart here would tell the operator their desks are gone.
      setError(e instanceof Error ? e.message : "Could not load the org chart.");
      setLoad("error");
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
   * Run a write, then re-read the whole chart.
   *
   * Refetch rather than patch in place: every write here changes something the
   * host derives (the effective member union, the lead, the overlay subset), so
   * a local edit would be a second implementation of rules the host already
   * owns — and the two would drift.
   */
  async function mutate(key: string, run: () => Promise<unknown>) {
    setBusy(key);
    setError(null);
    try {
      await run();
      await boot();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Something went wrong. Try again.");
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-4xl space-y-6 px-4 py-6">
        <div className="flex items-start justify-between gap-4">
          <div className="space-y-1">
            <h2 className="text-2xl font-semibold tracking-tight">Company</h2>
            <p className="text-sm text-muted-foreground">
              How your company is organised: the desks it works from and who staffs each one. Add a
              desk, move someone between desks, or change who leads.
            </p>
          </div>
          <Button
            size="sm"
            variant="outline"
            className="shrink-0"
            disabled={load === "loading"}
            onClick={() => setCreateOpen(true)}
          >
            <Plus className="mr-1.5 size-4" />
            New desk
          </Button>
        </div>

        {error && (
          <div
            role="alert"
            className="rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive"
          >
            {error}
          </div>
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
                onCreate={() => setCreateOpen(true)}
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
                onDelete={(desk) =>
                  void mutate(`delete:${desk.id}`, () => client.deleteDesk(desk.id, company))
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
  onCreate,
  onAdd,
  onRemove,
  onMove,
  onDelete,
}: {
  tree: OrgTree;
  busy: string | null;
  onCreate: () => void;
  onAdd: (desk: OrgDesk, agentId: string) => void;
  onRemove: (desk: OrgDesk, agentId: string) => void;
  onMove: (desk: OrgDesk, index: number, direction: "up" | "down") => void;
  onDelete: (desk: OrgDesk) => void;
}) {
  return (
    <div role="tree" aria-label="Company org chart" className="space-y-3">
      <div role="treeitem" aria-level={1} aria-expanded="true" aria-selected="false">
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
                onAdd={(agentId) => onAdd(desk, agentId)}
                onRemove={(agentId) => onRemove(desk, agentId)}
                onMove={(index, direction) => onMove(desk, index, direction)}
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
  onAdd,
  onRemove,
  onMove,
  onDelete,
}: {
  desk: OrgDesk;
  addable: { id: string; name: string }[];
  busy: string | null;
  onAdd: (agentId: string) => void;
  onRemove: (agentId: string) => void;
  onMove: (index: number, direction: "up" | "down") => void;
  onDelete: () => void;
}) {
  const locked = busy !== null;
  return (
    <div role="treeitem" aria-level={2} aria-expanded="true" aria-selected="false">
      <div className="rounded-xl border">
        <div className="flex items-start justify-between gap-2 px-3 py-2.5">
          <div className="min-w-0">
            <p className="flex items-center gap-2 truncate font-medium">
              {desk.name}
              {desk.provenance === "blueprint" && (
                <Badge variant="secondary" className="shrink-0 text-[10px]">
                  Blueprint
                </Badge>
              )}
            </p>
            {desk.description && (
              <p className="line-clamp-2 text-xs text-muted-foreground">{desk.description}</p>
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
              <Trash2 className={cn("size-3.5", busy === `delete:${desk.id}` && "opacity-50")} />
            </Button>
          )}
        </div>

        <div role="group" className="space-y-1 border-t px-3 py-2">
          {desk.seats.length === 0 && (
            <p className="py-1 text-xs text-muted-foreground">Nobody staffs this desk yet.</p>
          )}
          {desk.seats.map((seat, index) => (
            <Seat
              key={seat.id}
              seat={seat}
              deskName={desk.name}
              first={index === 0}
              last={index === desk.seats.length - 1}
              busy={busy === `${desk.id}:${seat.id}`}
              locked={locked}
              onUp={() => onMove(index, "up")}
              onDown={() => onMove(index, "down")}
              onRemove={() => onRemove(seat.id)}
            />
          ))}

          <div className="pt-1">
            <DropdownMenu>
              <DropdownMenuTrigger
                render={
                  <Button
                    variant="outline"
                    size="sm"
                    className="w-full"
                    disabled={addable.length === 0 || locked}
                  />
                }
              >
                <Plus className="size-4" />
                {addable.length === 0 ? "Everyone is on this desk" : "Add teammate"}
              </DropdownMenuTrigger>
              {addable.length > 0 && (
                <DropdownMenuContent align="start" className="max-h-64 overflow-y-auto">
                  {addable.map((member) => (
                    <DropdownMenuItem key={member.id} onClick={() => onAdd(member.id)}>
                      <span className="truncate">{member.name}</span>
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuContent>
              )}
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
  deskName,
  first,
  last,
  busy,
  locked,
  onUp,
  onDown,
  onRemove,
}: {
  seat: OrgSeat;
  deskName: string;
  first: boolean;
  last: boolean;
  busy: boolean;
  locked: boolean;
  onUp: () => void;
  onDown: () => void;
  onRemove: () => void;
}) {
  return (
    <div
      role="treeitem"
      aria-level={3}
      aria-selected="false"
      className={cn(
        "flex items-center justify-between gap-2 rounded-md border px-2 py-1.5 text-sm",
        busy && "opacity-50",
      )}
    >
      <span className="flex min-w-0 items-center gap-1.5">
        {seat.lead && (
          <Crown role="img" aria-label="Desk lead" className="size-3.5 shrink-0 text-amber-500" />
        )}
        <span className={cn("truncate", !seat.known && "text-muted-foreground")}>{seat.name}</span>
        {seat.role && <span className="truncate text-xs text-muted-foreground">{seat.role}</span>}
        {/* A seat naming somebody the roster no longer has. Shown, not hidden:
            it is a fact about the structure only the operator can fix. */}
        {!seat.known && (
          <Badge variant="outline" className="shrink-0 text-[10px]">
            Not on the roster
          </Badge>
        )}
      </span>
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
        ) : (
          <Badge variant="secondary" className="shrink-0 text-[10px]">
            Blueprint
          </Badge>
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
      {tree.unassigned.length > 0 && (
        <section className="space-y-2">
          <h3 className="text-sm font-medium">Not on a desk</h3>
          <p className="text-xs text-muted-foreground">
            Roster teammates the company has not staffed anywhere. Add them to a desk above.
          </p>
          <ul className="flex flex-wrap gap-1.5">
            {tree.unassigned.map((member) => (
              <li key={member.id} className="rounded-md border px-2 py-1 text-xs">
                {member.name}
              </li>
            ))}
          </ul>
        </section>
      )}
      {tree.people.length > 0 && (
        <section className="space-y-2">
          <h3 className="text-sm font-medium">People</h3>
          <p className="text-xs text-muted-foreground">
            The humans who can sign in. Desks staff agents, so the company declares no desk for a
            person, and this chart does not guess one.
          </p>
          <ul className="flex flex-wrap gap-1.5">
            {tree.people.map((person) => (
              <li key={person.id} className="rounded-md border px-2 py-1 text-xs">
                {person.name}
                <span className="ml-1.5 text-muted-foreground">{person.role}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
