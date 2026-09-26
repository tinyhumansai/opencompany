import { useEffect, useRef, useState } from "react";
import { Loader2, Upload } from "lucide-react";

import { uploadSkills, type SkillUploadRow } from "@/api/skills";
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

/**
 * Upload `.md` / `.zip` / `.skill` files as skills.
 *
 * The host answers with one row per file, so this renders one row per file:
 * an operator who drops five and mistypes one keeps the four that worked and
 * is told what was wrong with the fifth. A single "upload failed" line would
 * throw away the good files and name none of them.
 *
 * Blocked-by-scan rows get a second chance behind an explicit override, never
 * a silent retry — the point of the verdict is that somebody reads it.
 */
export function UploadSkillDialog({
  client,
  company,
  open,
  onOpenChange,
  onUploaded,
}: {
  client: OpenCompanyClient;
  company: string | null;
  open: boolean;
  onOpenChange: (o: boolean) => void;
  /** Called with every skill this dialog stored, so the list can take them. */
  onUploaded: (rows: SkillUploadRow[]) => void;
}) {
  const input = useRef<HTMLInputElement>(null);
  const [files, setFiles] = useState<File[]>([]);
  const [rows, setRows] = useState<SkillUploadRow[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  function reset() {
    setFiles([]);
    setRows([]);
    setError(null);
    if (input.current) input.current.value = "";
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

  const isBlocked = (row: SkillUploadRow) => !row.ok && row.scanBlocked === true;

  async function send(force: boolean) {
    const indices = force
      ? rows.flatMap((row, i) => (isBlocked(row) ? [i] : []))
      : files.map((_, i) => i);
    if (indices.length === 0) return;
    setBusy(true);
    setError(null);
    try {
      const answer = await uploadSkills(
        client,
        company,
        indices.map((i) => files[i]),
        force,
      );
      setRows((prev) => {
        if (!force) return answer.results;
        const next = [...prev];
        indices.forEach((originalIndex, k) => {
          next[originalIndex] = answer.results[k];
        });
        return next;
      });
      onUploaded(answer.results.filter((row) => row.ok));
    } catch (e) {
      setError(e instanceof Error ? e.message : "the upload could not be sent");
    } finally {
      setBusy(false);
    }
  }

  const blocked = rows.some(isBlocked);

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
          <DialogTitle>Upload skills</DialogTitle>
          <DialogDescription>
            A <code>SKILL.md</code>, or an archive holding one. Several at once — each gets its
            own answer.
          </DialogDescription>
        </DialogHeader>
        <div className="grid gap-2">
          <Label htmlFor="skill-upload-files">Files</Label>
          <input
            ref={input}
            id="skill-upload-files"
            data-testid="skill-upload-input"
            type="file"
            multiple
            accept=".md,.zip,.skill"
            className="text-sm file:mr-3 file:rounded-md file:border file:border-border file:bg-muted file:px-3 file:py-1.5 file:text-sm"
            onChange={(e) => {
              setFiles([...(e.target.files ?? [])]);
              setRows([]);
            }}
          />
          <p className="text-xs text-muted-foreground">
            An archive must carry one <code>SKILL.md</code> and nothing else. Bundled files have
            nowhere to be stored yet, so an archive holding them is refused rather than having
            them dropped.
          </p>
        </div>

        {error && (
          <p className="text-sm text-destructive" data-testid="skill-upload-error">
            {error}
          </p>
        )}

        {rows.length > 0 && (
          <ul className="grid gap-2" data-testid="skill-upload-results">
            {rows.map((row, index) => (
              <li
                key={`${index}:${row.file}`}
                data-testid="skill-upload-row"
                className="rounded-md border border-border p-2 text-sm"
              >
                <span className="font-medium">{row.file}</span>{" "}
                <span className="text-muted-foreground">
                  {row.ok ? `stored as ${row.skill?.id}` : "not stored"}
                </span>
                {row.error && <p className="mt-1 text-xs text-destructive">{row.error}</p>}
                {row.ok && row.skill?.scan && row.skill.scan.findings.length > 0 && (
                  <ul className="mt-1 list-disc pl-4 text-xs text-muted-foreground">
                    {row.skill.scan.findings.map((finding) => (
                      <li key={finding}>{finding}</li>
                    ))}
                  </ul>
                )}
              </li>
            ))}
          </ul>
        )}

        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={busy}>
            Close
          </Button>
          {blocked && (
            <Button
              variant="outline"
              data-testid="skill-upload-force"
              disabled={busy}
              onClick={() => void send(true)}
            >
              Upload anyway
            </Button>
          )}
          <Button disabled={files.length === 0 || busy} onClick={() => void send(false)}>
            {busy ? <Loader2 className="mr-1.5 size-4 animate-spin" /> : <Upload className="mr-1.5 size-4" />}
            Upload
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
