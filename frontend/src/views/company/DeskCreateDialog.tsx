// The desk creator: a plain form that builds a `CreateDeskInput` and posts it
// via `client.createDesk`. Desks are created through the operator overlay — the
// company blueprint is never rewritten — and start with any roster teammates the
// operator picks (the first selected becomes the desk lead). Mirrors the
// `WorkflowCreateDialog` create-flow shape.
//
// Reached from the org chart (issue #311). It used to be reached from the flat
// Desks page, which #302 unmounted; between then and now nothing rendered this
// dialog at all, so creating a desk was impossible outside the manifest.

import { useEffect, useId, useRef, useState } from "react";
import { Crown } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { DeskDto, TeamMemberDto } from "@/api/types";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { avatarFor, toneFor } from "@/lib/team";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";

export function DeskCreateDialog({
  client,
  company,
  open,
  onOpenChange,
  onCreated,
}: {
  client: OpenCompanyClient;
  company: string | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onCreated: (desk: DeskDto) => void;
}) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  // The chosen member ids, in selection order — the first is the desk lead.
  const [members, setMembers] = useState<string[]>([]);
  const [roster, setRoster] = useState<TeamMemberDto[]>([]);
  const [submitting, setSubmitting] = useState(false);
  // Two kinds of message, deliberately kept apart (issue #1100). `nameError` is
  // about one field and renders at that field; `error` is the host's refusal of
  // the whole form and keeps the banner above the footer. Sharing one slot put
  // "Give the desk a name." underneath the entire roster picker — below the
  // fold on a real company, with nothing moving the operator to it.
  const [nameError, setNameError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const nameRef = useRef<HTMLInputElement>(null);
  const formId = useId();
  const nameErrorId = `${formId}-name-error`;

  // Reload the roster (the member picker) and reset the draft each time the
  // dialog opens, so a prior attempt never leaks into the next one.
  useEffect(() => {
    if (!open) return;
    setName("");
    setDescription("");
    setMembers([]);
    setNameError(null);
    setError(null);
    let live = true;
    (async () => {
      try {
        const team = await client.listTeam(company);
        if (live) setRoster(team);
      } catch {
        // No roster surface on this host — the desk can still be created empty
        // and gain members later through the desk card's add control.
        if (live) setRoster([]);
      }
    })();
    return () => {
      live = false;
    };
  }, [open, client, company]);

  function toggleMember(id: string) {
    setMembers((current) =>
      current.includes(id) ? current.filter((m) => m !== id) : [...current, id],
    );
  }

  function memberLabel(id: string): string {
    const member = roster.find((r) => r.id === id);
    return member?.name ?? member?.role ?? id;
  }

  async function submit() {
    if (!name.trim()) {
      setNameError("Give the desk a name.");
      setError(null);
      // The frame AFTER the state change: the message does not exist in the DOM
      // until this render commits, and the input can be scrolled far above the
      // Create button the operator just pressed. `preventScroll` so the focus
      // move does not jump the container out from under the smooth scroll.
      requestAnimationFrame(() => {
        nameRef.current?.focus({ preventScroll: true });
        nameRef.current?.scrollIntoView({ block: "center", behavior: "smooth" });
      });
      return;
    }
    setNameError(null);
    setSubmitting(true);
    setError(null);
    try {
      const created = await client.createDesk(
        {
          name: name.trim(),
          description: description.trim() || undefined,
          members: members.length > 0 ? members : undefined,
        },
        company,
      );
      onCreated(created);
      onOpenChange(false);
    } catch (e) {
      setError(e instanceof Error ? e.message : "could not create the desk");
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !submitting && onOpenChange(o)}>
      <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>New desk</DialogTitle>
          <DialogDescription>
            A desk is a group chat you talk to. Pick who staffs it — the first teammate you choose
            leads it.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-2">
          <Label htmlFor={`${formId}-name`}>Name</Label>
          <Input
            id={`${formId}-name`}
            ref={nameRef}
            value={name}
            onChange={(e) => {
              setName(e.target.value);
              // The complaint was about the field being empty, so the first
              // keystroke answers it; leaving it up would outlive its subject.
              setNameError(null);
            }}
            aria-invalid={Boolean(nameError)}
            aria-describedby={nameError ? nameErrorId : undefined}
            placeholder="e.g. Engineering"
          />
          {nameError && (
            // Announced as well as rendered: the focus move above is what a
            // sighted operator notices, `role="alert"` is what a screen reader
            // gets, and `aria-describedby` ties it to the field it is about.
            <p
              id={nameErrorId}
              role="alert"
              data-testid="desk-name-error"
              className="text-xs text-destructive"
            >
              {nameError}
            </p>
          )}
        </div>
        <div className="grid gap-2">
          <Label htmlFor={`${formId}-desc`}>Description</Label>
          <Textarea
            id={`${formId}-desc`}
            rows={2}
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            placeholder="What is this desk for?"
          />
        </div>

        <div className="grid gap-2">
          <Label>Teammates</Label>
          {roster.length === 0 ? (
            <p className="rounded-lg border border-dashed p-3 text-center text-xs text-muted-foreground">
              No roster teammates to add — you can add them after the desk exists.
            </p>
          ) : (
            <div className="flex flex-col gap-1.5">
              {roster.map((member) => {
                const order = members.indexOf(member.id);
                const selected = order !== -1;
                return (
                  <button
                    type="button"
                    key={member.id}
                    onClick={() => toggleMember(member.id)}
                    aria-pressed={selected}
                    className={cn(
                      "flex items-center justify-between gap-2 rounded-md border px-2.5 py-1.5 text-left text-sm transition-colors",
                      selected
                        ? "border-primary/50 bg-primary/10"
                        : "hover:bg-muted/60",
                    )}
                  >
                    <span className="flex min-w-0 items-center gap-1.5">
                      <TeammateAvatar
                        name={member.name ?? member.role}
                        avatar={avatarFor(member.id ?? member.name ?? "")}
                        tone={toneFor(member.id ?? member.name ?? "")}
                        className="size-5 shrink-0"
                      />
                      {order === 0 && (
                        <Crown role="img" aria-label="Desk lead" className="size-3.5 shrink-0 text-muted-foreground" />
                      )}
                      <span className="truncate">{member.name ?? member.role}</span>
                    </span>
                    {selected && (
                      <span className="shrink-0 text-xs text-muted-foreground">
                        {order === 0 ? "lead" : order + 1}
                      </span>
                    )}
                  </button>
                );
              })}
            </div>
          )}
          {members.length > 0 && (
            <p className="text-xs text-muted-foreground">
              Lead: <span className="font-medium">{memberLabel(members[0])}</span>
            </p>
          )}
        </div>

        {/* Whole-form failures only — what the host said when it refused the
            create. Field-level complaints render at their field, above.
            `Alert` carries `role="alert"` itself. */}
        {error && (
          <Alert variant="destructive" data-testid="desk-create-error">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={submitting}>
            Cancel
          </Button>
          <Button onClick={() => void submit()} disabled={submitting}>
            {submitting ? "Creating…" : "Create desk"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
