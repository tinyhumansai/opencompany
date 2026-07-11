import { useState } from "react";

import type { OpenCompanyClient } from "../api/client";
import { ApiError, type FeedbackCategory, type FeedbackResponse } from "../api/types";
import { FEEDBACK_CATEGORIES } from "../lib/language";

interface Props {
  client: OpenCompanyClient;
  company: string;
  onClose: () => void;
}

/** Flag something that was wrong. Mirrors the scrub-then-preview gate: the
 *  operator previews the exact final body before anything is filed. */
export function FeedbackDialog({ client, company, onClose }: Props) {
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

  return (
    <div className="overlay" onClick={onClose}>
      <div className="dialog card" onClick={(e) => e.stopPropagation()}>
        <h2>Flag something</h2>
        <div className="muted">Tell your company what was off. You'll see exactly what gets shared before it leaves your machine.</div>

        {error && <div className="banner error">{error}</div>}

        {result ? (
          <FeedbackResult result={result} onDone={onClose} onEdit={() => setResult(null)} />
        ) : (
          <>
            <label className="field">
              <span>What happened?</span>
              <select value={category} onChange={(e) => setCategory(e.target.value as FeedbackCategory)}>
                {FEEDBACK_CATEGORIES.map((c) => (
                  <option key={c.value} value={c.value}>
                    {c.label}
                  </option>
                ))}
              </select>
            </label>
            <label className="field">
              <span>In your words</span>
              <textarea
                rows={4}
                value={note}
                onChange={(e) => setNote(e.target.value)}
                placeholder="e.g. The invoice it drafted had the wrong total."
              />
            </label>
            <div className="row">
              <button className="btn" onClick={onClose}>
                Cancel
              </button>
              <button className="btn" disabled={busy || !note.trim()} onClick={() => void submit(true)}>
                Preview
              </button>
              <button className="btn primary" disabled={busy || !note.trim()} onClick={() => void submit(false)}>
                Send
              </button>
            </div>
          </>
        )}
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
      <>
        <div className="banner error">
          Not shared — {result.reason ?? "it contained something private"}. Your note stays on your machine.
        </div>
        <div className="row">
          <button className="btn primary" onClick={onEdit}>
            Edit and try again
          </button>
        </div>
      </>
    );
  }
  if (result.preview_body) {
    return (
      <>
        <div className="muted">This is exactly what would be shared:</div>
        <label className="field">
          <textarea readOnly rows={8} value={result.preview_body} />
        </label>
        {result.prefilled_url && (
          <div className="muted">
            Prefer to file it yourself?{" "}
            <a href={result.prefilled_url} target="_blank" rel="noreferrer">
              Open a prefilled report
            </a>
            .
          </div>
        )}
        <div className="row">
          <button className="btn" onClick={onEdit}>
            Edit
          </button>
          <button className="btn primary" onClick={onDone}>
            Done
          </button>
        </div>
      </>
    );
  }
  return (
    <>
      <div className="banner ok">
        {result.filed
          ? result.deduped
            ? "Added to an existing report. Thanks!"
            : "Shared. Thanks — you'll hear back if it ships."
          : "Captured locally."}
      </div>
      {result.issue_url && (
        <div className="muted">
          <a href={result.issue_url} target="_blank" rel="noreferrer">
            View the report
          </a>
        </div>
      )}
      {!result.filed && result.prefilled_url && (
        <div className="muted">
          <a href={result.prefilled_url} target="_blank" rel="noreferrer">
            File it yourself
          </a>
        </div>
      )}
      <div className="row">
        <button className="btn primary" onClick={onDone}>
          Done
        </button>
      </div>
    </>
  );
}
