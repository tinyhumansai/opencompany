// The task edit form, extracted from the board (#184) so both the Kanban board
// and the Task Detail screen open the same dialog without a circular import.
// This is an *edit* form — title / note / column / priority / assignee plus a
// delete — unchanged from its original home in `TasksView`.

import { useEffect, useState } from "react";
import { Loader2, Trash2 } from "lucide-react";

import { deleteTask, patchTask, type PatchTask, type Task } from "@/api/tasks";
import type { OpenCompanyClient } from "@/api/client";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Textarea } from "@/components/ui/textarea";
import { TASK_COLUMNS } from "@/lib/tasks-sample";
import { toast } from "sonner";
import { AssigneeSelect } from "./AssigneeSelect";

const PRIORITIES = ["low", "medium", "high"] as const;

/**
 * Edit a card (or delete it). Open when `task` is non-null; `onClose` fires on
 * dismiss, `onSaved`/`onDeleted` hand the reconciled row back to the caller so
 * the board or detail screen can update its own state.
 */
export function TaskEditDialog({
  task,
  onClose,
  onSaved,
  onDeleted,
  client,
  company,
}: {
  task: Task | null;
  onClose: () => void;
  onSaved: (t: Task) => void;
  onDeleted: (id: string) => void;
  client: OpenCompanyClient;
  company: string | null;
}) {
  const [draft, setDraft] = useState<PatchTask>({});
  const [busy, setBusy] = useState(false);

  // Reset the edit draft each time a different card is opened.
  useEffect(() => {
    if (task) {
      setDraft({
        title: task.title,
        note: task.note ?? "",
        column: task.column,
        priority: task.priority,
        assignee: task.assignee,
      });
    }
  }, [task]);

  if (!task) return null;

  /**
   * The fields the operator actually changed, and only those.
   *
   * A `PatchTask` is applied field-by-field on the host, and every field it
   * *receives* is re-validated — `assignee` included, which re-runs
   * `assignee::resolve` against the current roster. Sending the whole seeded
   * draft therefore made a card uneditable the moment its stored assignee left
   * the roster (a teammate removed, a desk deleted): renaming the title
   * resubmitted the stale assignee, which came back `Unknown`, and the save
   * failed with a `400` about a field the operator never touched. Diffing means
   * an untouched field is never re-validated, so the only assignee the host is
   * ever asked to resolve is one just picked from the roster.
   */
  function changedFields(current: Task): PatchTask {
    const patch: PatchTask = {};
    if ((draft.title ?? "") !== current.title) patch.title = draft.title ?? "";
    if ((draft.note ?? "") !== (current.note ?? "")) patch.note = draft.note ?? "";
    if (draft.column !== undefined && draft.column !== current.column) patch.column = draft.column;
    if (draft.priority !== undefined && draft.priority !== current.priority) {
      patch.priority = draft.priority;
    }
    if ((draft.assignee ?? "") !== current.assignee) patch.assignee = draft.assignee ?? "";
    return patch;
  }

  async function save() {
    if (!task) return;
    const patch = changedFields(task);
    if (Object.keys(patch).length === 0) {
      // Nothing to write. Saying so beats a round-trip that reports "Saved."
      // for an edit that never happened.
      toast.success("No changes to save.");
      onSaved(task);
      return;
    }
    setBusy(true);
    try {
      const saved = await patchTask(client, company, task.id, patch);
      onSaved(saved);
      toast.success("Saved.");
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not save");
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    if (!task) return;
    setBusy(true);
    try {
      await deleteTask(client, company, task.id);
      onDeleted(task.id);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not delete");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog open={!!task} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Edit task</DialogTitle>
          <DialogDescription>
            Edit the card, or drop it into “In progress” on the board to dispatch it.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-3">
          <div className="grid gap-1.5">
            <Label htmlFor="task-title">Title</Label>
            <Input
              id="task-title"
              value={draft.title ?? ""}
              onChange={(e) => setDraft((d) => ({ ...d, title: e.target.value }))}
            />
          </div>

          <div className="grid gap-1.5">
            <Label htmlFor="task-note">Note / result</Label>
            <Textarea
              id="task-note"
              rows={8}
              className="font-mono text-xs"
              value={draft.note ?? ""}
              onChange={(e) => setDraft((d) => ({ ...d, note: e.target.value }))}
            />
          </div>

          <div className="grid grid-cols-3 gap-3">
            <div className="grid gap-1.5">
              <Label htmlFor="task-column">Column</Label>
              {/* Fall back to the card's own value rather than `undefined`.
                  `draft` starts empty and is seeded a tick later by the effect
                  above, so a bare `draft.column` hands Base UI `undefined` on
                  the first render — which latches the select as *uncontrolled*
                  and makes it ignore the seeded value, leaving the trigger
                  blank for the whole life of the dialog. */}
              <Select
                value={draft.column ?? task.column}
                onValueChange={(v) => setDraft((d) => ({ ...d, column: v ?? undefined }))}
              >
                <SelectTrigger id="task-column">
                  {/* The trigger renders the raw value unless told otherwise,
                      and a column's id is not its label (`in_progress` vs "In
                      progress"). */}
                  <SelectValue>
                    {(selected) =>
                      TASK_COLUMNS.find((c) => c.id === selected)?.label ?? String(selected ?? "")
                    }
                  </SelectValue>
                </SelectTrigger>
                <SelectContent>
                  {TASK_COLUMNS.map((c) => (
                    <SelectItem key={c.id} value={c.id}>
                      {c.label}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="grid gap-1.5">
              <Label htmlFor="task-priority">Priority</Label>
              {/* Same seeding hazard as Column above. A priority is its own
                  label, so only the casing the items carry needs restating. */}
              <Select
                value={draft.priority ?? task.priority}
                onValueChange={(v) => setDraft((d) => ({ ...d, priority: v ?? undefined }))}
              >
                <SelectTrigger id="task-priority">
                  <SelectValue className="capitalize" />
                </SelectTrigger>
                <SelectContent>
                  {PRIORITIES.map((p) => (
                    <SelectItem key={p} value={p} className="capitalize">
                      {p}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            {/* `min-w-0`: a grid item's automatic minimum size is its content's
                min-content width, so the assignee's long ids would otherwise
                widen this track and squeeze Column and Priority away. */}
            <div className="grid min-w-0 gap-1.5">
              <Label htmlFor="task-assignee">Assignee</Label>
              {/* Issue #263: picked from the roster, not typed. An assignee the
                  roster no longer carries still renders — flagged — so a save
                  that does not touch it can never quietly rewrite it. */}
              <AssigneeSelect
                id="task-assignee"
                client={client}
                company={company}
                value={draft.assignee ?? ""}
                onChange={(next) => setDraft((d) => ({ ...d, assignee: next }))}
              />
            </div>
          </div>
        </div>

        <DialogFooter className="justify-between sm:justify-between">
          <AlertDialog>
            <AlertDialogTrigger
              render={
                <Button variant="ghost" size="sm" disabled={busy}>
                  <Trash2 className="mr-1.5 size-4" />
                  Delete
                </Button>
              }
            />
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>Delete “{task.title}”?</AlertDialogTitle>
                <AlertDialogDescription>
                  This permanently removes the task and can’t be undone.
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>Keep task</AlertDialogCancel>
                <AlertDialogAction
                  onClick={() => void remove()}
                  className="bg-destructive text-white hover:bg-destructive/90"
                >
                  Delete task
                </AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
          <div className="flex gap-2">
            <Button variant="outline" onClick={onClose} disabled={busy}>
              Cancel
            </Button>
            <Button onClick={() => void save()} disabled={busy}>
              {busy && <Loader2 className="mr-1.5 size-4 animate-spin" />}
              Save
            </Button>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
