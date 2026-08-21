import { useState } from "react";
import { Mail } from "lucide-react";

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

export interface NewMemberFields {
  name: string;
  role: string;
  description: string;
  inbox?: boolean;
}

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onAdd: (fields: NewMemberFields) => void;
}

/** Add teammate. Reached from the team pane's "Add" button. */
export function AddMemberDialog({ open, onOpenChange, onAdd }: Props) {
  const [name, setName] = useState("");
  const [role, setRole] = useState("");
  const [description, setDescription] = useState("");
  const [inbox, setInbox] = useState(false);

  function reset() {
    setName("");
    setRole("");
    setDescription("");
    setInbox(false);
  }

  function submit() {
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
          <DialogDescription>Add a teammate to your company&apos;s roster.</DialogDescription>
        </DialogHeader>
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
          <Switch checked={inbox} onCheckedChange={setInbox} aria-label="Give this teammate an inbox" />
        </label>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button onClick={submit} disabled={!name.trim() || !role.trim()}>
            Add teammate
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
