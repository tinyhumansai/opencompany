import { useCallback, useEffect, useState } from "react";
import {
  KeyRound,
  Loader2,
  LogOut,
  MailPlus,
  MoreHorizontal,
  ShieldCheck,
  UserMinus,
  UserPlus,
} from "lucide-react";

import {
  invite as sendInvite,
  isManifestInvite,
  listInvites,
  listPeople,
  me as fetchMe,
  resetPassword,
  revokeInvite,
  revokeSessions,
  updatePerson,
  type Invite,
  type Me,
  type Person,
  type UserRole,
} from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription } from "@/components/ui/alert";
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
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { personName } from "@/lib/person";
import { Skeleton } from "@/components/ui/skeleton";
import { toast } from "sonner";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/**
 * The people who can sign in to this company, and the invites that let them.
 *
 * Distinct from Team, which is the company's *agents*. These are humans.
 *
 * Admin-only. The view hides controls a member cannot use, but that is
 * courtesy — the backend refuses them regardless, and this must never be
 * mistaken for the enforcement.
 */
export function PeopleView({ client, company }: Props) {
  const [me, setMe] = useState<Me | null>(null);
  const [people, setPeople] = useState<Person[]>([]);
  const [invites, setInvites] = useState<Invite[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [inviteOpen, setInviteOpen] = useState(false);
  const [resetting, setResetting] = useState<Person | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      const who = await fetchMe(client, company);
      setMe(who);
      // A member gets the header and nothing else; asking for the roster would
      // only 403.
      if (who.role !== "admin") {
        setLoading(false);
        return;
      }
      const [roster, pending] = await Promise.all([
        listPeople(client, company),
        listInvites(client, company),
      ]);
      setPeople(roster);
      setInvites(pending);
    } catch (err) {
      setError(err instanceof ApiError ? err.message : "Couldn't load people.");
    } finally {
      setLoading(false);
    }
  }, [client, company]);

  useEffect(() => {
    void load();
  }, [load]);

  const isAdmin = me?.role === "admin";
  // Only invites nobody has redeemed yet are still "pending".
  const pending = invites.filter((i) => !i.acceptedAtMillis);
  const activeAdmins = people.filter((p) => p.role === "admin" && p.status === "active");

  async function act(what: string, fn: () => Promise<unknown>) {
    try {
      await fn();
      await load();
      toast.success(what);
    } catch (err) {
      // Surface the server's reason — "this is the company's last admin"
      // is worth reading, not swallowing.
      toast.error(err instanceof ApiError ? err.message : `Couldn't ${what.toLowerCase()}.`);
    }
  }

  /**
   * Invites someone and reports what actually reached them (issue #584).
   *
   * Deliberately not routed through `act`, which toasts unconditional success
   * on any 2xx. That is right for revoking or suspending, where the write is
   * the whole outcome, and wrong here: the grant landing and the person being
   * told are two different things, and a host with no mail transport does the
   * first without the second. Reporting them as one is what sent QA looking
   * for an email nobody had sent.
   *
   * Every outcome reloads, because the invite exists in all of them — only the
   * follow-up the operator owes differs.
   */
  async function inviteAndReport(email: string, role: UserRole) {
    try {
      // This view's dialog collects only an email address — it has no wallet
      // input — so it only ever invites in email mode, exactly as before
      // `mode` became a required argument.
      const result = await sendInvite(client, company, email, role, "email");
      await load();
      if (result.delivery === "sent") {
        toast.success(`Invited ${email}`, {
          description: "They've been emailed a link to sign in.",
        });
      } else if (result.delivery === "no_transport") {
        toast.warning(`${email} can now sign in — but nothing was emailed`, {
          description:
            "This host has no email set up, so you'll need to tell them yourself. " +
            "They sign in with this address on the login page.",
        });
      } else {
        toast.warning(`${email} was invited, but the email didn't send`, {
          description:
            "They can still sign in with this address — tell them, or revoke and " +
            "re-invite to try sending again.",
        });
      }
    } catch (err) {
      // A real refusal: already a member, already invited, or not an address.
      // The server's wording is the useful part.
      toast.error(err instanceof ApiError ? err.message : `Couldn't invite ${email}.`);
    }
  }

  /*
    One header for all three states, hoisted above the loading return (codex
    review, #1785): the skeleton used to run first, so a fresh
    `#/settings/people` exposed nothing but anonymous skeletons to a screen
    reader — no page name at all.

    Every prop keys off `isAdmin`, which is derived from `me` and is false
    until that read lands — so the header is driven by *what is known*, not by
    `loading`. `Invite` therefore appears the moment the role is known, which
    may be while the member list is still arriving (the two reads are one
    `Promise.all`, but `me` can settle first): offering it then is correct, and
    it is never offered before the role is known, which is the part that
    matters.

    The width is the one thing `loading` does decide. It stays `4xl` while the
    skeletons show so the common path — an admin who came here to manage
    access — has no reflow when the rows arrive; a non-admin sees the column
    narrow once, at the same moment the content replaces the skeletons.
  */
  const header = (
    <PageHeader
      title="People"
      width={loading || isAdmin ? "4xl" : "3xl"}
      description={
        isAdmin
          ? "The humans who can sign in. Access is invite-only."
          : "The humans who can sign in to this company."
      }
      actions={
        isAdmin ? (
          <Button size="sm" variant="outline" onClick={() => setInviteOpen(true)}>
            <UserPlus className="mr-1.5 size-4" />
            Invite
          </Button>
        ) : null
      }
    />
  );

  if (loading) {
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        {header}
        <div className="mx-auto w-full max-w-4xl space-y-3 px-4 py-6">
          <Skeleton className="h-8 w-48" />
          <Skeleton className="h-24 w-full" />
          <Skeleton className="h-24 w-full" />
        </div>
      </div>
    );
  }

  if (!isAdmin) {
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        {header}
        <div className="mx-auto min-h-0 w-full max-w-3xl flex-1 overflow-y-auto px-4 py-6">
        <Alert>
          <ShieldCheck className="size-4" />
          <AlertDescription>
            Only an admin can manage people. Ask one of them to invite someone or
            change access.
          </AlertDescription>
        </Alert>
        </div>
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {header}
      <div className="mx-auto min-h-0 w-full max-w-4xl flex-1 space-y-6 overflow-y-auto px-4 py-6">

      {error ? (
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      ) : null}

      <section className="space-y-3">
        <h2 className="text-sm font-medium text-muted-foreground">
          Members · {people.length}
        </h2>
        <Card>
          <CardContent className="divide-y p-0">
            {people.map((person) => (
              <PersonRow
                key={person.id}
                person={person}
                isSelf={person.id === me?.id}
                // Stripping the last admin would lock the company out of its
                // own directory, and there is no operator token to recover
                // with. The server refuses too; this just avoids offering it.
                isLastAdmin={
                  person.role === "admin" &&
                  person.status === "active" &&
                  activeAdmins.length === 1
                }
                onRole={(role) =>
                  act(`${person.email} is now ${role}`, () =>
                    updatePerson(client, company, person.id, { role }),
                  )
                }
                onStatus={(status) =>
                  act(
                    status === "suspended"
                      ? `Suspended ${person.email}`
                      : `Reactivated ${person.email}`,
                    () => updatePerson(client, company, person.id, { status }),
                  )
                }
                onSignOut={() =>
                  act(`Signed ${person.email} out everywhere`, () =>
                    revokeSessions(client, company, person.id),
                  )
                }
                onReset={() => setResetting(person)}
              />
            ))}
            {people.length === 0 ? (
              <p className="p-6 text-sm text-muted-foreground">
                Nobody has signed in yet.
              </p>
            ) : null}
          </CardContent>
        </Card>
      </section>

      <section className="space-y-3">
        <h2 className="text-sm font-medium text-muted-foreground">
          Pending invites · {pending.length}
        </h2>
        <Card>
          <CardContent className="divide-y p-0">
            {pending.map((invitation) => (
              <InviteRow
                key={invitation.id}
                invite={invitation}
                onRevoke={() =>
                  act(`Revoked the invite for ${invitation.email}`, () =>
                    revokeInvite(client, company, invitation.id),
                  )
                }
              />
            ))}
            {pending.length === 0 ? (
              <p className="p-6 text-sm text-muted-foreground">
                No invites outstanding.
              </p>
            ) : null}
          </CardContent>
        </Card>
      </section>
      </div>

      <InviteDialog
        open={inviteOpen}
        onOpenChange={setInviteOpen}
        onInvite={async (email, role) => {
          await inviteAndReport(email, role);
          setInviteOpen(false);
        }}
      />

      <ResetPasswordDialog
        person={resetting}
        onOpenChange={(open) => !open && setResetting(null)}
        onReset={async (password) => {
          if (!resetting) return;
          await act(`Set a temporary password for ${resetting.email}`, () =>
            resetPassword(client, company, resetting.id, password),
          );
          setResetting(null);
        }}
      />
    </div>
  );
}

function PersonRow({
  person,
  isSelf,
  isLastAdmin,
  onRole,
  onStatus,
  onSignOut,
  onReset,
}: {
  person: Person;
  isSelf: boolean;
  isLastAdmin: boolean;
  onRole: (role: UserRole) => void;
  onStatus: (status: "active" | "suspended") => void;
  onSignOut: () => void;
  onReset: () => void;
}) {
  const suspended = person.status === "suspended";
  return (
    <div className="flex items-center gap-3 p-4">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          {/* The name they chose, else the one derived from their address —
              the same one every other surface calls them by. The address itself
              is on the line below, where an admin needs it and it is not
              standing in for a name. */}
          <span className="truncate text-sm font-medium">{personName(person)}</span>
          {person.role === "admin" ? <Badge variant="secondary">Admin</Badge> : null}
          {suspended ? <Badge variant="destructive">Suspended</Badge> : null}
          {person.mustChangePassword ? (
            <Badge variant="outline">Must change password</Badge>
          ) : null}
          {isSelf ? <span className="text-xs text-muted-foreground">you</span> : null}
        </div>
        <p className="truncate text-xs text-muted-foreground">
          {`${person.email} · `}
          {person.hasPassword ? "password set" : "magic link only"}
          {/* "signed in", not "seen": this is stamped when a session is minted,
              not on every request. Saying "last seen" would imply activity
              tracking that deliberately does not happen — it would cost a store
              write per authenticated call. */}
          {person.lastSeenAtMillis
            ? ` · signed in ${relative(person.lastSeenAtMillis)}`
            : ""}
        </p>
      </div>

      <DropdownMenu>
        <DropdownMenuTrigger
          render={<Button variant="ghost" size="icon" aria-label={`Manage ${person.email}`} />}
        >
          <MoreHorizontal className="size-4" />
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          {person.role === "member" ? (
            <DropdownMenuItem onClick={() => onRole("admin")}>
              <ShieldCheck className="mr-2 size-4" />
              Make admin
            </DropdownMenuItem>
          ) : (
            <DropdownMenuItem disabled={isLastAdmin} onClick={() => onRole("member")}>
              <UserMinus className="mr-2 size-4" />
              {isLastAdmin ? "Last admin — promote someone first" : "Make member"}
            </DropdownMenuItem>
          )}
          <DropdownMenuItem onClick={onReset}>
            <KeyRound className="mr-2 size-4" />
            Set a temporary password
          </DropdownMenuItem>
          <DropdownMenuItem onClick={onSignOut}>
            <LogOut className="mr-2 size-4" />
            Sign out everywhere
          </DropdownMenuItem>
          <DropdownMenuSeparator />
          {suspended ? (
            <DropdownMenuItem onClick={() => onStatus("active")}>
              Reactivate
            </DropdownMenuItem>
          ) : (
            <DropdownMenuItem
              variant="destructive"
              disabled={isLastAdmin}
              onClick={() => onStatus("suspended")}
            >
              {isLastAdmin ? "Last admin — promote someone first" : "Suspend"}
            </DropdownMenuItem>
          )}
        </DropdownMenuContent>
      </DropdownMenu>
    </div>
  );
}

function InviteRow({ invite, onRevoke }: { invite: Invite; onRevoke: () => void }) {
  const fromManifest = isManifestInvite(invite);
  return (
    <div className="flex items-center gap-3 p-4">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate text-sm font-medium">{invite.email}</span>
          {invite.role === "admin" ? <Badge variant="secondary">Admin</Badge> : null}
          {fromManifest ? <Badge variant="outline">From manifest</Badge> : null}
        </div>
        <p className="truncate text-xs text-muted-foreground" data-testid="invite-meta">
          {fromManifest
            ? "Listed in the company manifest — remove them from [users].admins there"
            : `Invited ${relative(invite.createdAtMillis)} · expires ${relative(invite.expiresAtMillis)} · ${
                // Whether anyone actually told them. A row that said only
                // "invited" reads as "we contacted them", which is the
                // assumption issue #584 is about; on a host with no mail the
                // follow-up is the operator's and they need to know it.
                invite.notifiedAtMillis
                  ? "invite email sent"
                  : "no invite email sent — let them know"
              }`}
        </p>
      </div>
      {/* A manifest invite has no record to delete, and the manifest would
          re-grant it on the next sign-in. Offering the button would be a lie. */}
      {fromManifest ? null : (
        <Button variant="ghost" size="sm" onClick={onRevoke}>
          Revoke
        </Button>
      )}
    </div>
  );
}

function InviteDialog({
  open,
  onOpenChange,
  onInvite,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onInvite: (email: string, role: UserRole) => Promise<void>;
}) {
  const [email, setEmail] = useState("");
  const [role, setRole] = useState<UserRole>("member");
  const [busy, setBusy] = useState(false);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Invite someone</DialogTitle>
          <DialogDescription>
            We'll email them a link to the sign-in page, and they'll be able to
            sign in with this address. If this host has no email set up, we'll
            tell you so you can pass it on yourself.
          </DialogDescription>
        </DialogHeader>
        <form
          className="space-y-4"
          onSubmit={async (e) => {
            e.preventDefault();
            setBusy(true);
            try {
              await onInvite(email, role);
              setEmail("");
              setRole("member");
            } finally {
              setBusy(false);
            }
          }}
        >
          <div className="space-y-2">
            <Label htmlFor="invite-email">Email</Label>
            <Input
              id="invite-email"
              type="email"
              required
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              placeholder="them@company.com"
            />
          </div>
          <div className="space-y-2">
            <Label htmlFor="invite-role">Role</Label>
            <Select value={role} onValueChange={(v) => setRole(v as UserRole)}>
              <SelectTrigger id="invite-role">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="member">Member — use the company</SelectItem>
                <SelectItem value="admin">Admin — also manage people</SelectItem>
              </SelectContent>
            </Select>
          </div>
          <DialogFooter>
            <Button type="submit" disabled={busy}>
              {busy ? <Loader2 className="mr-2 size-4 animate-spin" /> : (
                <MailPlus className="mr-2 size-4" />
              )}
              Invite
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

function ResetPasswordDialog({
  person,
  onOpenChange,
  onReset,
}: {
  person: Person | null;
  onOpenChange: (open: boolean) => void;
  onReset: (password: string) => Promise<void>;
}) {
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);

  return (
    <Dialog open={person !== null} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Set a temporary password</DialogTitle>
          <DialogDescription>
            For {person?.email}. You'll have to tell them what it is, so send it
            somewhere private — or skip this and let them sign in with a magic
            link instead.
          </DialogDescription>
        </DialogHeader>
        <form
          className="space-y-4"
          onSubmit={async (e) => {
            e.preventDefault();
            setBusy(true);
            try {
              await onReset(password);
              setPassword("");
            } finally {
              setBusy(false);
            }
          }}
        >
          <div className="space-y-2">
            <Label htmlFor="temp-password">Temporary password</Label>
            <Input
              id="temp-password"
              type="text"
              required
              minLength={12}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder="at least 12 characters"
            />
          </div>
          <Alert>
            <AlertDescription className="text-xs">
              This signs them out everywhere and requires them to set a new
              password before they can do anything else.
            </AlertDescription>
          </Alert>
          <DialogFooter>
            <Button type="submit" variant="destructive" disabled={busy}>
              {busy ? <Loader2 className="mr-2 size-4 animate-spin" /> : null}
              Set it
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}

/** A coarse "3 days ago" / "in 2 weeks". Precision is not the point here. */
function relative(atMillis: number): string {
  const delta = atMillis - Date.now();
  const future = delta > 0;
  const mins = Math.round(Math.abs(delta) / 60_000);
  const say = (n: number, unit: string) =>
    future ? `in ${n} ${unit}${n === 1 ? "" : "s"}` : `${n} ${unit}${n === 1 ? "" : "s"} ago`;
  if (mins < 1) return future ? "shortly" : "just now";
  if (mins < 60) return say(mins, "minute");
  const hours = Math.round(mins / 60);
  if (hours < 24) return say(hours, "hour");
  return say(Math.round(hours / 24), "day");
}
