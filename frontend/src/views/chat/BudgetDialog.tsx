import { useEffect, useState } from "react";

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
import type { TeamMember } from "@/lib/team";

/**
 * Enter a daily cap for one teammate. Ported from the retired Team page
 * (issue #360) so the member pane keeps the same budget affordance.
 *
 * Empty input is **not** submittable: "no cap" is the explicit "Remove cap"
 * action on the row, not a blank field, so an operator clearing the box and
 * saving can never silently uncap a teammate. `0` is allowed and means exactly
 * what it says — this teammate may not spend.
 */
export function BudgetDialog({
  member,
  onOpenChange,
  onSave,
}: {
  member: TeamMember | null;
  onOpenChange: (open: boolean) => void;
  onSave: (cap: number) => void;
}) {
  const [value, setValue] = useState("");

  useEffect(() => {
    setValue(member?.budgetUsdDaily !== undefined ? String(member.budgetUsdDaily) : "");
  }, [member]);

  const parsed = Number(value);
  const valid = value.trim() !== "" && Number.isFinite(parsed) && parsed >= 0;

  return (
    <Dialog open={member !== null} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Daily budget</DialogTitle>
          <DialogDescription>
            The most {member?.name ?? "this teammate"} may spend per day. It takes effect on their
            next task — no restart needed.
          </DialogDescription>
        </DialogHeader>
        <div className="grid gap-2">
          <Label htmlFor="member-budget">US dollars per day</Label>
          <Input
            id="member-budget"
            type="number"
            min={0}
            step="0.01"
            inputMode="decimal"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            placeholder="e.g. 5.00"
            data-testid="team-budget-input"
          />
          <p className="text-xs text-muted-foreground">
            $0 stops them spending entirely. To let them spend freely, use "Remove cap".
          </p>
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button
            onClick={() => valid && onSave(parsed)}
            disabled={!valid}
            data-testid="team-budget-save"
          >
            Save
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
