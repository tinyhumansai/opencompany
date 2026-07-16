import type { OpenCompanyClient } from "@/api/client";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { FeedbackForm } from "@/components/feedback-form";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

/** Flag something that was wrong, in a modal. Wraps the shared FeedbackForm. */
export function FeedbackDialog({ client, company, open, onOpenChange }: Props) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Flag something</DialogTitle>
          <DialogDescription>
            Tell your company what was off. You&apos;ll see exactly what gets shared before it leaves
            your machine.
          </DialogDescription>
        </DialogHeader>
        {/* Remount per open so the form resets cleanly between sessions. */}
        {open && (
          <FeedbackForm client={client} company={company} onDone={() => onOpenChange(false)} />
        )}
      </DialogContent>
    </Dialog>
  );
}
