import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { AGENT_FIELDS, type AgentDraft, type AgentFieldKey } from "@/lib/agent";

/**
 * An agent's authored fields, rendered from one definition (issue #264).
 *
 * Used by both "Add teammate" and the detail view's edit form. They collect
 * the same three things, and rendering them from
 * [`AGENT_FIELDS`](@/lib/agent) is what keeps the labels, the placeholders and
 * the order the same in both places rather than the same by coincidence.
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
}: {
  /** Namespaces the DOM ids, so two of these can be mounted at once. */
  idPrefix: string;
  draft: AgentDraft;
  onChange: (key: AgentFieldKey, value: string) => void;
  /** Whether a given field is read-only. Defaults to all-editable. */
  readOnly?: (key: AgentFieldKey) => boolean;
}) {
  return (
    <>
      {AGENT_FIELDS.map((field) => {
        const id = `${idPrefix}-${field.key}`;
        const locked = readOnly?.(field.key) ?? false;
        return (
          <div key={field.key} className="grid gap-2">
            <Label htmlFor={id}>{field.label}</Label>
            {field.kind === "prose" ? (
              <Textarea
                id={id}
                rows={4}
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
                data-testid={`agent-field-${field.key}`}
              />
            )}
          </div>
        );
      })}
    </>
  );
}
