import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";

/**
 * The reduced Add-teammate dialog's whole content (issue #1989): a name and one
 * box.
 *
 * Shared by both Add-teammate dialogs — the roster grid's
 * (`TeamView.AddMemberDialog`, which also offers a budget) and the chat/org
 * chart's (`views/chat/AddMemberDialog`) — because the reduction is the same
 * reduction, and two copies of it would drift the way the two full forms
 * already have. What each dialog keeps to itself is what it does with the
 * result: the org chart also places the teammate on a desk, and the roster grid
 * also refetches.
 *
 * ## Why a Name field beside the box, when the brief said one box
 *
 * Because nothing can derive a name. The copilot's `DraftableField` is
 * `description | instructions` and excludes `name` deliberately, so there is no
 * model in this path to name anybody; the only alternative is splitting the
 * sentence, and a teammate's name is not a phrase. "Runs paid acquisition"
 * would then be the name on every roster card, in every member list, and beside
 * every message that teammate sends. The role IS a phrase, so that split is
 * kept for the role — see `roleFromDescription`.
 *
 * ## Why there is no copilot control in here
 *
 * There is nothing for it to draft yet. `draftNewAgentField` drafts
 * `description` or `instructions` for a teammate that does not exist, but this
 * box IS the description, in the operator's own words, and the instructions are
 * the field the handoff exists to fill. Create lands on `#/team/<id>?edit`,
 * where `AgentDetailView` wires both fields to the copilot against a teammate
 * the host has actually stored — which is a better grounding than anything this
 * dialog could send, and is the reason the redirect is load-bearing rather than
 * a nicety.
 */
export function DescribeTeammate({
  idPrefix,
  name,
  description,
  onNameChange,
  onDescriptionChange,
  disabled,
}: {
  /** Namespaces the DOM ids, so two of these can be mounted at once. */
  idPrefix: string;
  name: string;
  description: string;
  onNameChange: (value: string) => void;
  onDescriptionChange: (value: string) => void;
  /** Held while a create is in flight. */
  disabled?: boolean;
}) {
  const nameId = `${idPrefix}-describe-name`;
  const descriptionId = `${idPrefix}-describe-text`;
  return (
    <div className="grid gap-4">
      <div className="grid gap-2">
        <Label htmlFor={nameId}>Name</Label>
        <Input
          id={nameId}
          value={name}
          disabled={disabled}
          onChange={(e) => onNameChange(e.target.value)}
          placeholder="e.g. Nova"
          data-testid="team-describe-name"
        />
      </div>
      <div className="grid gap-2">
        <Label htmlFor={descriptionId}>What should they do?</Label>
        <Textarea
          id={descriptionId}
          rows={4}
          value={description}
          disabled={disabled}
          onChange={(e) => onDescriptionChange(e.target.value)}
          placeholder="e.g. Runs paid acquisition and reports on ROAS every week."
          data-testid="team-describe-box"
        />
        {/* Says where the rest of the teammate comes from, because the fields
            this dialog used to ask for have not gone away — they are on the
            page Create lands on, with the copilot beside them. Without this the
            reduction reads as fields being taken away. */}
        <p className="text-2xs text-muted-foreground" data-testid="team-describe-hint">
          You&apos;ll land on their profile, where the copilot can draft their
          instructions and you can set a budget or an inbox.
        </p>
      </div>
    </div>
  );
}
