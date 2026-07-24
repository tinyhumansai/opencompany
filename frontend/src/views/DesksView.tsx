import { useCallback, useEffect, useState } from "react";
import { Crown, Plus, Users, X } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { DeskDto, TeamMemberDto } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

type Load = "loading" | "ready" | "empty";

/**
 * The company's desks (group chats) and their membership. Manifest members are
 * shown read-only; operator-added overlay members carry a remove control, and
 * every desk offers a picker of roster teammates not yet on it (issue #72).
 */
export function DesksView({ client, company }: Props) {
  const [load, setLoad] = useState<Load>("loading");
  const [desks, setDesks] = useState<DeskDto[]>([]);
  const [roster, setRoster] = useState<TeamMemberDto[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const boot = useCallback(async () => {
    try {
      // The roster is best-effort — a host without `/team` still lists desks.
      const [desksRes, rosterRes] = await Promise.all([
        client.listDesks(company),
        client.listTeam(company).catch(() => [] as TeamMemberDto[]),
      ]);
      setDesks(desksRes);
      setRoster(rosterRes);
      setLoad(desksRes.length === 0 ? "empty" : "ready");
    } catch {
      // No desks surface on this host yet.
      setLoad("empty");
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void boot();
  }, [boot]);

  function displayName(id: string): string {
    const member = roster.find((r) => r.id === id);
    return member?.name ?? member?.role ?? id;
  }

  async function mutate(key: string, run: () => Promise<void>) {
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
      <div className="mx-auto w-full max-w-5xl space-y-6 px-4 py-6">
        <div className="space-y-1">
          <h2 className="text-2xl font-semibold tracking-tight">Desks</h2>
          <p className="text-sm text-muted-foreground">
            The desks your company works from. Add or remove teammates to change who staffs each one.
          </p>
        </div>

        {error && (
          <div className="rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
            {error}
          </div>
        )}

        {load === "loading" ? (
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {Array.from({ length: 3 }).map((_, i) => (
              <Skeleton key={i} className="h-40 rounded-xl" />
            ))}
          </div>
        ) : load === "empty" ? (
          <div className="flex min-h-40 flex-col items-center justify-center gap-2 rounded-xl border border-dashed text-sm text-muted-foreground">
            <Users className="size-5" />
            This company has no desks yet.
          </div>
        ) : (
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {desks.map((desk) => (
              <DeskCard
                key={desk.id}
                desk={desk}
                roster={roster}
                busy={busy}
                displayName={displayName}
                onAdd={(agentId) =>
                  mutate(`${desk.id}:${agentId}`, () =>
                    client.addDeskMember(desk.id, agentId, company),
                  )
                }
                onRemove={(agentId) =>
                  mutate(`${desk.id}:${agentId}`, () =>
                    client.removeDeskMember(desk.id, agentId, company),
                  )
                }
              />
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

function DeskCard({
  desk,
  roster,
  busy,
  displayName,
  onAdd,
  onRemove,
}: {
  desk: DeskDto;
  roster: TeamMemberDto[];
  busy: string | null;
  displayName: (id: string) => string;
  onAdd: (agentId: string) => void;
  onRemove: (agentId: string) => void;
}) {
  const overlay = new Set(desk.overlayMembers ?? []);
  // Roster teammates not already on this desk are the ones we can add.
  const available = roster.filter((r) => !desk.members.includes(r.id));

  return (
    <Card>
      <CardContent className="flex h-full flex-col gap-3 py-4">
        <div className="min-w-0">
          <p className="truncate font-medium">{desk.name}</p>
          {desk.description && (
            <p className="line-clamp-2 text-xs text-muted-foreground">{desk.description}</p>
          )}
        </div>

        <ul className="space-y-1">
          {desk.members.length === 0 && (
            <li className="text-xs text-muted-foreground">No teammates on this desk yet.</li>
          )}
          {desk.members.map((id, i) => {
            const isOverlay = overlay.has(id);
            const isBusy = busy === `${desk.id}:${id}`;
            return (
              <li
                key={id}
                className={cn(
                  "flex items-center justify-between gap-2 rounded-md border px-2 py-1.5 text-sm",
                  isBusy && "opacity-50",
                )}
              >
                <span className="flex min-w-0 items-center gap-1.5">
                  {i === 0 && <Crown className="size-3.5 shrink-0 text-amber-500" aria-label="Desk lead" />}
                  <span className="truncate">{displayName(id)}</span>
                </span>
                {isOverlay ? (
                  <Button
                    variant="ghost"
                    size="icon"
                    className="size-6 shrink-0 text-muted-foreground hover:text-destructive"
                    aria-label={`Remove ${displayName(id)} from ${desk.name}`}
                    disabled={isBusy}
                    onClick={() => onRemove(id)}
                  >
                    <X className="size-3.5" />
                  </Button>
                ) : (
                  <Badge variant="secondary" className="shrink-0 text-[10px]">
                    Blueprint
                  </Badge>
                )}
              </li>
            );
          })}
        </ul>

        <div className="mt-auto border-t pt-3">
          <DropdownMenu>
            <DropdownMenuTrigger
              render={
                <Button
                  variant="outline"
                  size="sm"
                  className="w-full"
                  disabled={available.length === 0 || busy !== null}
                />
              }
            >
              <Plus className="size-4" />
              {available.length === 0 ? "Everyone's on this desk" : "Add teammate"}
            </DropdownMenuTrigger>
            {available.length > 0 && (
              <DropdownMenuContent align="start" className="max-h-64 overflow-y-auto">
                {available.map((member) => (
                  <DropdownMenuItem key={member.id} onClick={() => onAdd(member.id)}>
                    <span className="truncate">{member.name ?? member.role}</span>
                  </DropdownMenuItem>
                ))}
              </DropdownMenuContent>
            )}
          </DropdownMenu>
        </div>
      </CardContent>
    </Card>
  );
}
