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
import { Check, Crown } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { DeskDto, TeamMemberDto } from "@/api/types";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { avatarRef } from "@/lib/avatar";
import { toneFor } from "@/lib/team";
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
  const [memberFilter, setMemberFilter] = useState("");
  const [submitting, setSubmitting] = useState(false);
  // Two kinds of message, deliberately kept apart (issue #1100). `nameError` is
  // about one field and renders at that field; `error` is the host's refusal of
  // the whole form and keeps the banner above the footer. Sharing one slot put
  // "Give the desk a name." underneath the entire roster picker — below the
  // fold on a real company, with nothing moving the operator to it.
  const [nameError, setNameError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const nameRef = useRef<HTMLInputElement>(null);
  // Keyed by member id, so `makeLead` can move focus to a control that
  // survives the promotion — see the comment there.
  const toggleRefs = useRef(new Map<string, HTMLButtonElement>());
  const formId = useId();
  const nameErrorId = `${formId}-name-error`;

  // Reload the roster (the member picker) and reset the draft each time the
  // dialog opens, so a prior attempt never leaks into the next one.
  useEffect(() => {
    if (!open) return;
    setName("");
    setDescription("");
    setMembers([]);
    setMemberFilter("");
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

  // Promote a selected teammate to the lead slot (index 0), which the whole
  // stack treats as the desk's lead (`members[0]` — see `chat/model.ts` and
  // `api/types.ts`). The others keep their relative order behind the new lead.
  // No-ops (returns the same reference) when the id is already the lead, and no
  // backend call — the choice is posted with the rest of the draft on Create.
  //
  // The row that owns the "Make lead" button the operator just activated
  // loses that button on this render — the promoted row switches to the
  // non-focusable "Lead" badge — so the focused element would otherwise be
  // ripped out of the DOM and focus would drop to `document.body`, stranding
  // a keyboard/AT user outside the dialog. The row's select/deselect toggle
  // is keyed by the same `member.id` and renders for every visible row
  // regardless of lead status, so it is the one control guaranteed to still
  // be mounted after the promotion; hand focus to it.
  function makeLead(id: string) {
    setMembers((current) =>
      current[0] === id ? current : [id, ...current.filter((m) => m !== id)],
    );
    toggleRefs.current.get(id)?.focus();
  }

  function memberLabel(id: string): string {
    const member = roster.find((r) => r.id === id);
    return member?.name ?? member?.role ?? id;
  }

  const memberFilterNeedle = memberFilter.trim().toLowerCase();
  const visibleRoster = roster.filter((member) => {
    const label = member.name ?? member.role ?? member.id;
    return !memberFilterNeedle || label.toLowerCase().includes(memberFilterNeedle);
  });

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
          <p className="text-xs text-muted-foreground">
            The top teammate leads the desk — add them in seniority order, or use “Make lead” to
            promote anyone.
          </p>
          {roster.length === 0 ? (
            <p className="rounded-lg border border-dashed p-3 text-center text-xs text-muted-foreground">
              No roster teammates to add — you can add them after the desk exists.
            </p>
          ) : (
            <div className="flex flex-col gap-1.5">
              {roster.length > 8 && (
                <Input
                  value={memberFilter}
                  onChange={(e) => setMemberFilter(e.target.value)}
                  placeholder="Filter teammates…"
                  aria-label="Filter teammates"
                  data-testid="desk-member-filter"
                  className="h-8 text-sm"
                />
              )}
              {visibleRoster.map((member) => {
                const order = members.indexOf(member.id);
                const selected = order !== -1;
                const memberName = member.name ?? member.role;
                return (
                  // A row, not a button: the select/deselect toggle and the
                  // "Make lead" promote control are siblings, because nesting an
                  // interactive control inside the toggle button is invalid HTML
                  // and would make a promote click also toggle selection.
                  <div
                    key={member.id}
                    className={cn(
                      "flex items-center gap-2 rounded-md border px-2.5 py-1.5 text-sm transition-colors",
                      selected
                        ? "border-primary/50 bg-primary/10"
                        : "hover:bg-muted/60",
                    )}
                  >
                    <button
                      type="button"
                      ref={(el) => {
                        if (el) toggleRefs.current.set(member.id, el);
                        else toggleRefs.current.delete(member.id);
                      }}
                      onClick={() => toggleMember(member.id)}
                      aria-pressed={selected}
                      className="flex min-w-0 flex-1 items-center gap-1.5 text-left"
                    >
                      <span
                        aria-hidden="true"
                        className={cn(
                          "flex size-4 shrink-0 items-center justify-center rounded-sm border",
                          selected
                            ? "border-primary bg-primary text-primary-foreground"
                            : "border-muted-foreground/50",
                        )}
                      >
                        {selected && <Check className="size-3" />}
                      </span>
                      <TeammateAvatar
                        name={memberName}
                        avatar={avatarRef(member.avatar, member.id ?? member.name ?? "")}
                        tone={toneFor(member.id ?? member.name ?? "")}
                        className="size-5 shrink-0"
                      />
                      {order === 0 && (
                        <Crown role="img" aria-label="Desk lead" className="size-3.5 shrink-0 text-muted-foreground" />
                      )}
                      <span className="truncate">{memberName}</span>
                    </button>
                    {order === 0 ? (
                      <span
                        data-testid="desk-lead-badge"
                        className="shrink-0 rounded-full bg-primary/15 px-2 py-0.5 text-xs font-medium text-primary"
                      >
                        Lead
                      </span>
                    ) : selected ? (
                      <span className="flex shrink-0 items-center gap-1.5">
                        <span
                          data-testid="desk-member-position"
                          className="text-xs text-muted-foreground"
                        >
                          {order + 1}
                        </span>
                        <button
                          type="button"
                          onClick={() => makeLead(member.id)}
                          data-testid="desk-make-lead"
                          aria-label={`Make ${memberName} the desk lead`}
                          className="shrink-0 rounded-md px-1.5 py-0.5 text-xs text-muted-foreground hover:bg-muted hover:text-foreground"
                        >
                          Make lead
                        </button>
                      </span>
                    ) : null}
                  </div>
                );
              })}
              {visibleRoster.length === 0 && (
                <p className="rounded-lg border border-dashed p-3 text-center text-xs text-muted-foreground">
                  No teammates match “{memberFilter.trim()}”.
                </p>
              )}
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
