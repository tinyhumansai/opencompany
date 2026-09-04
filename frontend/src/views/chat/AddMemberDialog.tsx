import { useEffect, useState } from "react";
import { Mail } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { getInferenceStatus, type CognitionPath } from "@/api/inference";
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
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { addTeammateSurface, describedTeammateFields } from "@/lib/team-add-surface";
import { DescribeTeammate } from "@/views/team/DescribeTeammate";

export interface NewMemberFields {
  name: string;
  role: string;
  description: string;
  inbox?: boolean;
  /**
   * Land on the new teammate's detail page with its edit form open, rather than
   * staying where the dialog was opened from (issue #1989).
   *
   * Set only by the reduced dialog, and it is that dialog's second half: it
   * collects a name and a sentence, so the description, the persona, the budget
   * and the inbox are all still to be filled in — on the page this flag opens,
   * beside the copilot that drafts two of them. A caller with nowhere to
   * navigate to may ignore it; a caller whose write fell back to a local-only
   * row has no id to navigate to and must.
   */
  landOnProfile?: boolean;
}

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onAdd: (fields: NewMemberFields) => void;
  /**
   * For the cognition read that decides which dialog renders (issue #1989).
   * This dialog writes nothing itself — `onAdd` is still what creates.
   */
  client: OpenCompanyClient;
  company: string | null;
}

/**
 * Add teammate. Reached from the chat pane's member list and from the org
 * chart's desk cards.
 *
 * Two shapes since issue #1989, told apart by `addTeammateSurface`:
 *
 * - **Reduced** — a name and one box. Create writes the teammate and lands the
 *   operator on its detail page with the edit form open, where the copilot
 *   drafts the description and the persona. The role is derived from the
 *   sentence rather than asked for; see `roleFromDescription` for why it cannot
 *   simply be left blank.
 * - **Full** — this dialog's original Name / Role / What they do / inbox form,
 *   byte for byte, for a company whose copilot cannot draft. Hidden, never
 *   deleted: a company on the offline brain would otherwise be locked out of
 *   ever writing a description, since nothing downstream could draft one for it.
 */
export function AddMemberDialog({ open, onOpenChange, onAdd, client, company }: Props) {
  const [name, setName] = useState("");
  const [role, setRole] = useState("");
  const [description, setDescription] = useState("");
  const [inbox, setInbox] = useState(false);
  /** Everything the reduced dialog collects. */
  const [described, setDescribed] = useState({ name: "", description: "" });
  /**
   * Whether a Create found no role in that sentence, which retires the reduced
   * dialog for this open rather than writing a role-less teammate.
   */
  const [roleUnderivable, setRoleUnderivable] = useState(false);
  /**
   * The cognition path this company booted onto, read while the dialog is open.
   * `null` until the check settles and on a host without the route, which the
   * surface function reads as "can draft" — see `addTeammateSurface` for why
   * that is the right way to be wrong.
   */
  const [cognition, setCognition] = useState<CognitionPath | null>(null);

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

  const describing = addTeammateSurface({ cognition, roleUnderivable }) === "describe";
  /** Why the reduced dialog's Create is dead, or `null` when it is not. */
  const describeBlocked = !described.name.trim()
    ? "A name is required."
    : !described.description.trim()
      ? "Say what they should do."
      : null;

  function reset() {
    setName("");
    setRole("");
    setDescription("");
    setInbox(false);
    setDescribed({ name: "", description: "" });
    // The hand-over lasts for one open: the next add starts reduced again,
    // because the sentence that could not be read is gone with it.
    setRoleUnderivable(false);
  }

  function submit() {
    if (describing) {
      const fields = describedTeammateFields(described);
      if (!fields) {
        // Nothing in the sentence survived the clause split. Hand over the full
        // form carrying what WAS typed, rather than writing a teammate whose
        // blank role breaks its own system prompt and switches off the copilot
        // on the page this create was about to open.
        setName(described.name.trim());
        setDescription(described.description.trim());
        setRoleUnderivable(true);
        return;
      }
      onAdd({ ...fields, landOnProfile: true });
      reset();
      return;
    }
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
          <DialogTitle>Add teammate</DialogTitle>
          <DialogDescription>
            {describing
              ? "Name them and say what they should do. You can fill in the rest on their profile."
              : "Add a teammate to your company's roster."}
          </DialogDescription>
        </DialogHeader>
        {describing ? (
          <DescribeTeammate
            idPrefix="member-chat"
            name={described.name}
            description={described.description}
            onNameChange={(next) => setDescribed((d) => ({ ...d, name: next }))}
            onDescriptionChange={(next) =>
              setDescribed((d) => ({ ...d, description: next }))
            }
          />
        ) : (
          <>
            {/* Said only when the full form arrived by hand-over, so the
                operator knows why the dialog changed under them. Never shown on
                the no-model path, where this form is simply what the dialog is. */}
            {roleUnderivable && (
              <p className="text-2xs text-muted-foreground" data-testid="chat-add-handover">
                We couldn&apos;t read a role out of that description, so here are all the
                fields.
              </p>
            )}
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
                <Mail className="size-4 text-muted-foreground" /> Give this teammate an inbox
              </span>
              <Switch
                checked={inbox}
                onCheckedChange={setInbox}
                aria-label="Give this teammate an inbox"
              />
            </label>
          </>
        )}
        <DialogFooter className="items-center">
          {describing && describeBlocked && (
            <p className="mr-auto text-2xs text-muted-foreground" data-testid="chat-add-blocked">
              {describeBlocked}
            </p>
          )}
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button
            onClick={submit}
            disabled={describing ? Boolean(describeBlocked) : !name.trim() || !role.trim()}
          >
            Add teammate
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
