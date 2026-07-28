import { Compass } from "lucide-react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";

/**
 * The non-blocking welcome moment shown once to a new operator. "Take the tour"
 * starts the guided spotlight; "Skip for now" records the tour as seen. Closing
 * by Esc/backdrop just dismisses (no decision recorded) so it re-offers on the
 * next load.
 */
export function WelcomeDialog({
  open,
  onOpenChange,
  onStart,
  onSkip,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onStart: () => void;
  onSkip: () => void;
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent showCloseButton={false} className="sm:max-w-md">
        <DialogHeader>
          <div className="mb-1 flex size-11 items-center justify-center rounded-xl bg-primary/10 text-primary">
            <Compass className="size-5" />
          </div>
          <DialogTitle>Welcome to your company</DialogTitle>
          <DialogDescription>
            This is your operator console — where your AI staff, their desks, workflows, and
            connections all live. Take a two-minute tour to see where everything is.
          </DialogDescription>
        </DialogHeader>
        <DialogFooter>
          <Button variant="ghost" onClick={onSkip}>
            Skip for now
          </Button>
          <Button onClick={onStart}>Take the tour</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
