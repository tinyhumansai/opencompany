import { useEffect, useState } from "react";
import { Loader2, Sparkles } from "lucide-react";

import {
  draftSkill,
  uploadSkills,
  type SkillDraftTurn,
  type SkillUploadRow,
} from "@/api/skills";
import type { OpenCompanyClient } from "@/api/client";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";

/**
 * Draft a skill by talking to the company's model.
 *
 * The transcript lives here and nowhere else: the host stores none of it, so
 * closing this dialog is the whole of "the conversation ended". Each turn sends
 * everything said so far, which is what lets "shorter" mean shorter than the
 * draft on screen.
 *
 * A turn that answers with a question rather than a document is a normal turn,
 * not a failure — asking is what makes this a conversation rather than a hint
 * box, and rendering it as an error would train the operator to distrust it.
 */
export function DraftSkillDialog({
  client,
  company,
  open,
  onOpenChange,
  onSaved,
}: {
  client: OpenCompanyClient;
  company: string | null;
  open: boolean;
  onOpenChange: (o: boolean) => void;
  /** Called with the stored skill once the operator saves a draft. */
  onSaved: (rows: SkillUploadRow[]) => void;
}) {
  const [messages, setMessages] = useState<SkillDraftTurn[]>([]);
  const [input, setInput] = useState("");
  const [draft, setDraft] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  function reset() {
    setMessages([]);
    setInput("");
    setDraft(null);
    setError(null);
  }

  // Closing is not always routed through this dialog's own handler — a
  // company switch or any parent that just stops passing `open` leaves the
  // last run's rows in state, and the next open shows the previous upload's
  // verdicts as if they were this one's.
  useEffect(() => {
    if (!open) reset();
    // `reset` only touches setters and a ref, both stable for the component's life.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  async function send() {
    const said = input.trim();
    const next: SkillDraftTurn[] = said
      ? [...messages, { role: "operator" as const, text: said }]
      : messages;
    const request: SkillDraftTurn[] =
      draft !== null
        ? [
            ...messages,
            { role: "copilot" as const, text: draft },
            ...(said ? [{ role: "operator" as const, text: said }] : []),
          ]
        : next;
    setMessages(next);
    setInput("");
    setBusy(true);
    setError(null);
    try {
      const answer = await draftSkill(client, company, request);
      if (answer.source === "unavailable") {
        setError(reasonText(answer.reason));
        return;
      }
      if (answer.reply) setMessages([...next, { role: "copilot", text: answer.reply }]);
      if (answer.text) setDraft(answer.text);
    } catch (e) {
      setError(e instanceof Error ? e.message : "the copilot could not be reached");
    } finally {
      setBusy(false);
    }
  }

  /**
   * Saves the draft through the upload route.
   *
   * The draft is a whole `SKILL.md`, and the route that stores a `SKILL.md` is
   * the upload route — which runs the validator and the scan exactly as a
   * typed-in skill would. Re-splitting the document into the four form fields
   * to post it to `POST …/skills` would mean a frontmatter parser on the client
   * whose disagreements with the host's are invisible until a save is refused.
   */
  async function save() {
    if (!draft) return;
    setBusy(true);
    setError(null);
    try {
      const file = new File([draft], "SKILL.md", { type: "text/markdown" });
      const answer = await uploadSkills(client, company, [file]);
      const refused = answer.results.find((row) => !row.ok);
      if (refused) {
        setError(refused.error ?? "that draft could not be saved");
        return;
      }
      onSaved(answer.results);
      onOpenChange(false);
      reset();
    } catch (e) {
      setError(e instanceof Error ? e.message : "that draft could not be saved");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) reset();
      }}
    >
      <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Draft a skill with a teammate</DialogTitle>
          <DialogDescription>
            Say what the skill is for. Nothing is saved until you save it.
          </DialogDescription>
        </DialogHeader>

        {messages.length > 0 && (
          <ul className="grid gap-2 text-sm" data-testid="skill-draft-transcript">
            {messages.map((turn, i) => (
              <li
                key={`${turn.role}-${i}`}
                className={
                  turn.role === "operator"
                    ? "rounded-md bg-muted p-2"
                    : "rounded-md border border-border p-2"
                }
              >
                {turn.text}
              </li>
            ))}
          </ul>
        )}

        <div className="grid gap-2">
          <Label htmlFor="skill-draft-input">What should it do?</Label>
          <Textarea
            id="skill-draft-input"
            rows={3}
            value={input}
            onChange={(e) => setInput(e.target.value)}
            placeholder="A weekly status report from recent work, for when someone asks for an update."
          />
        </div>

        {draft !== null && (
          <div className="grid gap-2">
            <Label htmlFor="skill-draft-doc">The draft</Label>
            <Textarea
              id="skill-draft-doc"
              data-testid="skill-draft-doc"
              rows={12}
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
            />
          </div>
        )}

        {error && (
          <p className="text-sm text-destructive" data-testid="skill-draft-error">
            {error}
          </p>
        )}

        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={busy}>
            Cancel
          </Button>
          <Button variant="outline" disabled={busy} onClick={() => void send()}>
            {busy ? (
              <Loader2 className="mr-1.5 size-4 animate-spin" />
            ) : (
              <Sparkles className="mr-1.5 size-4" />
            )}
            {messages.length === 0 ? "Draft one" : "Send"}
          </Button>
          <Button
            data-testid="skill-draft-save"
            disabled={!draft || busy}
            onClick={() => void save()}
          >
            Save this skill
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** What the operator should do about a refusal, per cause. */
function reasonText(reason?: string): string {
  switch (reason) {
    case "no_model":
      return "This company has no model wired up, so nothing can draft a skill for it.";
    case "budget_exhausted":
      return "This company is at its token ceiling for the period, so the draft was not run.";
    case "refused_by_scan":
      return "What came back was refused by the content scan, so it was not shown. Say it differently and try again.";
    case "unreadable":
      return "The answer could not be read as a draft. Say a little more about what the skill is for.";
    default:
      return "The model could not be reached. Try again in a moment.";
  }
}
