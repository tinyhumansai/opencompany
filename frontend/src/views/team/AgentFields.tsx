import type { ReactNode } from "react";

import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import {
  AGENT_FIELDS,
  draftHasContent,
  type AgentDraft,
  type AgentFieldKey,
} from "@/lib/agent";

/**
 * An agent's authored fields, rendered from one definition (issue #264).
 *
 * Used by both "Add teammate" and the detail view's edit form. They collect
 * the same three things, and rendering them from
 * [`AGENT_FIELDS`](@/lib/agent) is what keeps the labels, the placeholders and
 * the order the same in both places rather than the same by coincidence.
 *
 * A **required** field says so, and a blank one is marked invalid once the form
 * holds anything at all (issue #1776). Both surfaces disable their submit while
 * a required field is empty, and until this the button simply sat there dead:
 * on a manifest teammate — which carries no name of its own, and is shown
 * everywhere by its role — the edit form opened with Name blank and Save
 * already disabled, with nothing saying which box was responsible. The rule was
 * never wrong; it was invisible.
 *
 * `copilot` renders the drafting control under a field (issue #1776). A render
 * prop rather than a set of handlers, because the two surfaces ask the host
 * different questions — the detail view addresses a teammate by id, the
 * Add-teammate form sends the role being typed — and neither difference is this
 * component's business. What IS its business is the rule about *where* the
 * control may appear, and that rule lives here so it cannot be half-applied:
 * only under a `prose` field, and never under a locked one. Offering to draft
 * text into a box the host will refuse to store is a dead end, and drafting a
 * `name` or a `role` is deliberately not on the table — a role is what
 * delegation grounds on, so a drafted one would change who the company routes
 * work to.
 *
 * `readOnly` is a predicate rather than a boolean because editability is
 * per-field and decided by the **host**: the detail response carries an
 * `editable` list, and a manifest teammate's fields are not in it. A locked
 * field is still rendered, because "you cannot change this here" is information
 * an operator needs; hiding it would just recreate the dead end.
 *
 * Locked means the native `readOnly`, NOT `disabled`. A disabled input is
 * removed from the tab order and from the accessibility tree's interactive
 * surface, so the very values this screen exists to show would be unreachable
 * by keyboard and awkward to select or copy. `readOnly` refuses the edit and
 * keeps the value reachable, which is the behaviour a read-only field wants.
 */
export function AgentFields({
  idPrefix,
  draft,
  onChange,
  readOnly,
  copilot,
}: {
  /** Namespaces the DOM ids, so two of these can be mounted at once. */
  idPrefix: string;
  draft: AgentDraft;
  onChange: (key: AgentFieldKey, value: string) => void;
  /** Whether a given field is read-only. Defaults to all-editable. */
  readOnly?: (key: AgentFieldKey) => boolean;
  /**
   * The drafting control for one field, when this surface offers one. Called
   * only for an editable `prose` field; omit it and the fields render exactly
   * as they did before.
   */
  copilot?: (key: AgentFieldKey) => ReactNode;
}) {
  // Whether a blank required field is shown as an error yet. See
  // `draftHasContent`: an edit form always holds something, so it marks the gap
  // immediately; a fresh Add form stays quiet until the operator types.
  const touched = draftHasContent(draft);
  return (
    <>
      {AGENT_FIELDS.map((field) => {
        const id = `${idPrefix}-${field.key}`;
        const locked = readOnly?.(field.key) ?? false;
        // A locked field cannot be the reason a save is refused — the host is
        // not being sent it — so it is never marked as one.
        const missing = Boolean(field.required) && !locked && draft[field.key].trim() === "";
        return (
          <div key={field.key} className="grid gap-2">
            <Label htmlFor={id} className="gap-1.5">
              {field.label}
              {field.required && !locked && (
                <span
                  className={
                    missing && touched ? "text-2xs text-destructive" : "text-2xs text-muted-foreground"
                  }
                  data-testid={`agent-field-required-${field.key}`}
                >
                  Required
                </span>
              )}
            </Label>
            {field.kind === "prose" ? (
              <Textarea
                id={id}
                rows={field.rows ?? 4}
                value={draft[field.key]}
                readOnly={locked}
                onChange={(e) => onChange(field.key, e.target.value)}
                placeholder={field.placeholder}
                data-testid={`agent-field-${field.key}`}
              />
            ) : (
              <Input
                id={id}
                value={draft[field.key]}
                readOnly={locked}
                onChange={(e) => onChange(field.key, e.target.value)}
                placeholder={field.placeholder}
                // The codebase's own error idiom (`aria-invalid` styling lives
                // in `ui/input.tsx`), so this reads as every other invalid
                // field does and is announced to a screen reader rather than
                // being colour alone.
                aria-invalid={missing && touched}
                data-testid={`agent-field-${field.key}`}
              />
            )}
            {field.kind === "prose" && !locked && copilot?.(field.key)}
          </div>
        );
      })}
    </>
  );
}
