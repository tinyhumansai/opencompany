import { useState } from "react";
import { CheckCircle2, ExternalLink, ShieldAlert } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type FeedbackCategory, type FeedbackResponse } from "@/api/types";
import { FEEDBACK_CATEGORIES } from "@/lib/language";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";

const CATEGORY_LABELS = Object.fromEntries(
  FEEDBACK_CATEGORIES.map((c) => [c.value, c.label]),
) as Record<FeedbackCategory, string>;

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** Called when the flow is finished (submitted or dismissed). */
  onDone: () => void;
  /** Show a Cancel button (dialogs) vs. no Cancel (a standalone page). */
  showCancel?: boolean;
}

/** The scrub-then-preview feedback flow, reused by the dialog and the page. */
export function FeedbackForm({ client, company, onDone, showCancel = true }: Props) {
  const [category, setCategory] = useState<FeedbackCategory>("wrong-output");
  const [note, setNote] = useState("");
  const [result, setResult] = useState<FeedbackResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(preview: boolean) {
    if (!note.trim() || busy) return;
    setBusy(true);
    setError(null);
    try {
      const res = await client.feedback({ category, note: note.trim(), preview }, company);
      setResult(res);
    } catch (err) {
      setError(err instanceof ApiError ? err.message : "something went wrong");
    } finally {
      setBusy(false);
    }
  }

  if (result) {
    return <FeedbackResult result={result} onDone={onDone} onEdit={() => setResult(null)} />;
  }

  return (
    <div className="space-y-4">
      {error && (
        <Alert variant="destructive">
          <ShieldAlert className="size-4" />
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      )}
      <div className="grid gap-2">
        <Label htmlFor="feedback-category">What happened?</Label>
        <Select
          value={category}
          onValueChange={(v) => setCategory(v as FeedbackCategory)}
          items={CATEGORY_LABELS}
        >
          <SelectTrigger id="feedback-category" className="w-full">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {FEEDBACK_CATEGORIES.map((c) => (
              <SelectItem key={c.value} value={c.value}>
                {c.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>
      <div className="grid gap-2">
        <Label htmlFor="feedback-note">In your words</Label>
        <Textarea
          id="feedback-note"
          rows={4}
          value={note}
          onChange={(e) => setNote(e.target.value)}
          placeholder="e.g. The invoice it drafted had the wrong total."
        />
      </div>
      <div className="flex justify-end gap-2">
        {showCancel && (
          <Button variant="ghost" onClick={onDone}>
            Cancel
          </Button>
        )}
        <Button variant="outline" disabled={busy || !note.trim()} onClick={() => void submit(true)}>
          Preview
        </Button>
        <Button disabled={busy || !note.trim()} onClick={() => void submit(false)}>
          Send
        </Button>
      </div>
    </div>
  );
}

function FeedbackResult({
  result,
  onDone,
  onEdit,
}: {
  result: FeedbackResponse;
  onDone: () => void;
  onEdit: () => void;
}) {
  if (result.blocked) {
    return (
      <div className="space-y-4">
        <Alert variant="destructive">
          <ShieldAlert className="size-4" />
          <AlertTitle>Not shared</AlertTitle>
          <AlertDescription>
            {result.reason ?? "It contained something private"}. Your note stays on your machine.
          </AlertDescription>
        </Alert>
        <div className="flex justify-end">
          <Button onClick={onEdit}>Edit and try again</Button>
        </div>
      </div>
    );
  }

  if (result.preview_body) {
    return (
      <div className="space-y-4">
        <div className="grid gap-2">
          <Label htmlFor="feedback-preview">This is exactly what would be shared</Label>
          <Textarea
            id="feedback-preview"
            readOnly
            rows={8}
            value={result.preview_body}
            className="font-mono text-xs"
          />
        </div>
        {result.prefilled_url && (
          <p className="text-sm text-muted-foreground">
            Prefer to file it yourself?{" "}
            <a
              className="inline-flex items-center gap-1 font-medium text-foreground underline underline-offset-4"
              href={result.prefilled_url}
              target="_blank"
              rel="noreferrer"
            >
              Open a prefilled report <ExternalLink className="size-3" />
            </a>
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={onEdit}>
            Edit
          </Button>
          <Button onClick={onDone}>Done</Button>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <Alert>
        <CheckCircle2 className="size-4" />
        <AlertTitle>
          {result.filed
            ? result.deduped
              ? "Added to an existing report"
              : "Shared — thanks!"
            : "Captured locally"}
        </AlertTitle>
        <AlertDescription>
          {result.filed ? "You'll hear back if it ships." : "Your note is saved on this machine."}
        </AlertDescription>
      </Alert>
      {result.issue_url && (
        <a
          className="inline-flex items-center gap-1 text-sm font-medium underline underline-offset-4"
          href={result.issue_url}
          target="_blank"
          rel="noreferrer"
        >
          View the report <ExternalLink className="size-3" />
        </a>
      )}
      {!result.filed && result.prefilled_url && (
        <a
          className="inline-flex items-center gap-1 text-sm font-medium underline underline-offset-4"
          href={result.prefilled_url}
          target="_blank"
          rel="noreferrer"
        >
          File it yourself <ExternalLink className="size-3" />
        </a>
      )}
      <div className="flex justify-end">
        <Button onClick={onDone}>Done</Button>
      </div>
    </div>
  );
}
