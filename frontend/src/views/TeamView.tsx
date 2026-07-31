import { useCallback, useEffect, useState } from "react";
import { Mail, MoreHorizontal, Plus, Sparkles, UserPlus } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { setInboxEnabled } from "@/api/inbox";
import { ApiError, type TeamMemberDto } from "@/api/types";
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
import { Textarea } from "@/components/ui/textarea";
import {
  fromDto,
  initials,
  newMember,
  starterTeam,
  TEAM_TONES,
  type TeamMember,
} from "@/lib/team";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

type Load = "loading" | "ready";

/** The company's agents — showcased and operator-definable. */
export function TeamView({ client, company }: Props) {
  const [load, setLoad] = useState<Load>("loading");
  const [fromHost, setFromHost] = useState(false);
  const [members, setMembers] = useState<TeamMember[]>([]);
  const [addOpen, setAddOpen] = useState(false);

  /**
   * Give a teammate an inbox, or take it away, on the host — keyed by the
   * roster **agent id**, which is the `InboxStore` key the Inbox page reads and
   * the ingest webhook files mail under. Nothing is persisted client-side: if
   * the write fails the switch goes back, so the console never claims an inbox
   * the host doesn't have.
   *
   * Starter-team cards are locally-invented placeholders, not host records, so
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
      setLoad("ready");
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void boot();
  }, [boot]);

  async function addMember(fields: { name: string; role: string; description: string; inbox?: boolean }) {
    let created: TeamMemberDto | null = null;
    try {
      created = await client.addTeamMember(
        { name: fields.name, role: fields.role, description: fields.description || undefined },
        company,
      );
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) {
        // No team write plane on this host — keep the edit local-only. An inbox
        // needs a persisted teammate to hang off, so it can't be enabled here.
        setMembers((m) => [...m, newMember(fields)]);
        if (fields.inbox) toast.error("This host can't persist teammates, so no inbox was created.");
        setAddOpen(false);
        return;
      }
      toast.error(error instanceof Error ? error.message : "Couldn't add teammate.");
      return;
    }

    // Enable the inbox against the host's real agent id *before* refetching, so
    // the reloaded roster already reports the toggle as on.
    if (fields.inbox) {
      try {
        await setInboxEnabled(client, company, created.id, true);
      } catch {
        toast.error("Teammate added, but their inbox couldn't be switched on.");
      }
    }
    // Persisted on the host — refetch so the card reflects the real record
    // (id, merge order, inbox state) rather than a locally-guessed one.
    await boot();
    setAddOpen(false);
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
        toast.error("This teammate is defined in the company manifest and can't be removed here.");
      } else {
        toast.error(error instanceof Error ? error.message : "Couldn't remove teammate.");
      }
    }
  }

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-5xl space-y-6 px-4 py-6">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="space-y-1">
            <h2 className="text-2xl font-semibold tracking-tight">Team</h2>
            <p className="text-sm text-muted-foreground">
              The agents that make up your company. {fromHost ? "Defined by this company." : "Start from these and shape your own."}
            </p>
          </div>
          <Button onClick={() => setAddOpen(true)}>
            <UserPlus className="size-4" /> Add member
          </Button>
        </div>

        {load === "loading" ? (
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {Array.from({ length: 6 }).map((_, i) => (
              <Skeleton key={i} className="h-32 rounded-xl" />
            ))}
          </div>
        ) : (
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
            {members.map((m) => (
              <MemberCard
                key={m.id}
                member={m}
                inboxOn={m.inboxEnabled}
                onToggleInbox={() => void toggleMemberInbox(m)}
                onRemove={() => void removeMember(m)}
              />
            ))}
            <button
              onClick={() => setAddOpen(true)}
              className="flex min-h-32 flex-col items-center justify-center gap-2 rounded-xl border border-dashed text-sm text-muted-foreground transition-colors hover:border-primary/40 hover:bg-accent/40 hover:text-foreground"
            >
              <Plus className="size-5" />
              Define an agent
            </button>
          </div>
        )}
      </div>

      <AddMemberDialog open={addOpen} onOpenChange={setAddOpen} onAdd={addMember} />
    </div>
  );
}

function MemberCard({
  member,
  inboxOn,
  onToggleInbox,
  onRemove,
}: {
  member: TeamMember;
  inboxOn: boolean;
  onToggleInbox: () => void;
  onRemove: () => void;
}) {
  return (
    <Card data-testid="team-card">
      <CardContent className="flex h-full flex-col gap-3 py-4">
        <div className="flex items-start gap-3">
          <div
            className={cn(
              "flex size-11 shrink-0 items-center justify-center rounded-xl text-sm font-semibold",
              TEAM_TONES[member.tone] ?? "bg-muted text-muted-foreground",
            )}
            aria-hidden
          >
            {initials(member.name)}
          </div>
          <div className="min-w-0 flex-1">
            <p className="truncate font-medium">{member.name}</p>
            <p className="truncate text-xs text-muted-foreground">{member.role}</p>
          </div>
          <DropdownMenu>
            <DropdownMenuTrigger
              render={<Button variant="ghost" size="icon" className="-mr-1 -mt-1 size-7" aria-label="Member actions" />}
            >
              <MoreHorizontal className="size-4" />
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem variant="destructive" onClick={onRemove}>
                Remove
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
        {member.description && (
          <p className="line-clamp-3 text-sm text-muted-foreground">{member.description}</p>
        )}
        <div className="mt-auto flex items-center justify-between gap-2 border-t pt-3">
          <Badge variant="secondary" className="gap-1">
            <Sparkles className="size-3" /> Agent
          </Badge>
          <label className="flex cursor-pointer items-center gap-2 text-xs text-muted-foreground">
            <Mail className="size-3.5" />
            Inbox
            <Switch
              checked={inboxOn}
              onCheckedChange={onToggleInbox}
              aria-label="Give this agent an inbox"
              data-testid="team-inbox-toggle"
            />
          </label>
        </div>
      </CardContent>
    </Card>
  );
}

function AddMemberDialog({
  open,
  onOpenChange,
  onAdd,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  onAdd: (fields: { name: string; role: string; description: string; inbox?: boolean }) => void;
}) {
  const [name, setName] = useState("");
  const [role, setRole] = useState("");
  const [description, setDescription] = useState("");
  const [inbox, setInbox] = useState(false);

  function reset() {
    setName("");
    setRole("");
    setDescription("");
    setInbox(false);
  }

  function submit() {
    if (!name.trim() || !role.trim()) return;
    onAdd({ name, role, description, inbox });
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
          <DialogTitle>Define an agent</DialogTitle>
          <DialogDescription>Add a teammate to your company&apos;s roster.</DialogDescription>
        </DialogHeader>
        <div className="grid gap-2">
          <Label htmlFor="member-name">Name</Label>
          <Input
            id="member-name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            placeholder="e.g. Nova"
          />
        </div>
        <div className="grid gap-2">
          <Label htmlFor="member-role">Role</Label>
          <Input
            id="member-role"
            value={role}
            onChange={(e) => setRole(e.target.value)}
            placeholder="e.g. Growth Marketer"
          />
        </div>
        <div className="grid gap-2">
          <Label htmlFor="member-desc">What they do</Label>
          <Textarea
            id="member-desc"
            rows={3}
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            placeholder="e.g. Runs paid acquisition and reports on ROAS."
          />
        </div>
        <label className="flex items-center justify-between rounded-lg border p-3">
          <span className="flex items-center gap-2 text-sm">
            <Mail className="size-4 text-muted-foreground" /> Give this agent an inbox
          </span>
          <Switch checked={inbox} onCheckedChange={setInbox} aria-label="Give this agent an inbox" />
        </label>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button onClick={submit} disabled={!name.trim() || !role.trim()}>
            Add member
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
