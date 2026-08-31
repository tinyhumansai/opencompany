import { useState, type ReactElement } from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";
import type { Channel } from "./model";

interface Props {
  directMessages: Channel[];
  onSelect: (id: string) => void;
  trigger: ReactElement;
}

/** Pick any teammate to open a direct-message composer. */
export function NewMessageDialog({ directMessages, onSelect, trigger }: Props) {
  const [open, setOpen] = useState(false);

  function select(id: string) {
    onSelect(id);
    setOpen(false);
  }

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger render={trigger} />
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>New message</DialogTitle>
          <DialogDescription>Choose a teammate to start a direct message.</DialogDescription>
        </DialogHeader>
        <div className="flex max-h-80 flex-col gap-1 overflow-y-auto">
          {directMessages.map((channel) => (
            <Button
              key={channel.id}
              variant="ghost"
              className="h-auto justify-start px-3 py-2 text-left"
              onClick={() => select(channel.id)}
            >
              <span className="min-w-0">
                <span className="block truncate text-sm font-medium">{channel.name}</span>
                {channel.purpose && (
                  <span className="block truncate text-xs font-normal text-muted-foreground">
                    {channel.purpose}
                  </span>
                )}
              </span>
            </Button>
          ))}
        </div>
      </DialogContent>
    </Dialog>
  );
}
