import { useEffect, useMemo, useState } from "react";
import { ArrowLeft, Inbox as InboxIcon, Mail } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { cn } from "@/lib/utils";
import {
  type EmailMessage,
  enabledInboxes,
  type Inbox,
  loadInboxes,
  saveInboxes,
  slugify,
  unreadCount,
} from "@/lib/inbox";

interface Props {
  company: string | null;
}

/** An email inbox surface. Each agent with an inbox enabled gets its own. */
export function InboxView({ company }: Props) {
  const [store, setStore] = useState(() => loadInboxes(company));
  const inboxes = useMemo(() => enabledInboxes(store), [store]);
  const [activeKey, setActiveKey] = useState<string>(() => enabledInboxes(loadInboxes(company))[0]?.key ?? "");
  const [openId, setOpenId] = useState<string | null>(null);
  const [mobilePane, setMobilePane] = useState<"list" | "read">("list");

  useEffect(() => {
    saveInboxes(company, store);
  }, [company, store]);

  const active = inboxes.find((i) => i.key === activeKey) ?? inboxes[0];
  const openMsg = active?.messages.find((m) => m.id === openId) ?? null;

  function openMessage(inboxKey: string, id: string) {
    setOpenId(id);
    setMobilePane("read");
    setStore((s) => {
      const inbox = s[inboxKey];
      if (!inbox) return s;
      return {
        ...s,
        [inboxKey]: {
          ...inbox,
          messages: inbox.messages.map((m) => (m.id === id ? { ...m, read: true } : m)),
        },
      };
    });
  }

  if (inboxes.length === 0) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center gap-3 text-center text-muted-foreground">
        <InboxIcon className="size-8" />
        <div className="space-y-1">
          <p className="font-medium text-foreground">No inboxes yet</p>
          <p className="max-w-sm text-sm">
            Give an agent its own inbox from the <span className="font-medium">Team</span> page —
            flip on the inbox toggle for anyone who needs to receive email.
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-1 overflow-hidden">
      {/* Message list */}
      <section
        className={cn(
          "w-full shrink-0 flex-col border-r md:flex lg:w-96",
          mobilePane === "list" ? "flex" : "hidden",
        )}
      >
        <div className="flex items-center gap-2 border-b px-3 py-2.5">
          <Select value={active?.key} onValueChange={(v) => v && (setActiveKey(v), setOpenId(null))} items={Object.fromEntries(inboxes.map((i) => [i.key, i.name]))}>
            <SelectTrigger className="h-8 flex-1">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {inboxes.map((i) => (
                <SelectItem key={i.key} value={i.key}>
                  {i.name}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {active && unreadCount(active) > 0 && <Badge variant="secondary">{unreadCount(active)}</Badge>}
        </div>
        <div className="flex-1 overflow-y-auto">
          {active && active.messages.length > 0 ? (
            active.messages
              .slice()
              .sort((a, b) => b.at - a.at)
              .map((m) => (
                <MessageRow
                  key={m.id}
                  message={m}
                  active={m.id === openId}
                  onClick={() => openMessage(active.key, m.id)}
                />
              ))
          ) : (
            <div className="p-8 text-center text-sm text-muted-foreground">No messages.</div>
          )}
        </div>
      </section>

      {/* Reading pane */}
      <section className={cn("flex-1 flex-col overflow-hidden md:flex", mobilePane === "read" ? "flex" : "hidden")}>
        {openMsg && active ? (
          <Reading message={openMsg} inbox={active} onBack={() => setMobilePane("list")} />
        ) : (
          <div className="flex flex-1 flex-col items-center justify-center gap-2 text-center text-muted-foreground">
            <Mail className="size-8" />
            <p className="text-sm">Select a message to read.</p>
          </div>
        )}
      </section>
    </div>
  );
}

function MessageRow({
  message,
  active,
  onClick,
}: {
  message: EmailMessage;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      onClick={onClick}
      className={cn(
        "flex w-full items-start gap-3 border-b px-3 py-3 text-left transition-colors",
        active ? "bg-accent" : "hover:bg-accent/50",
      )}
    >
      <Avatar name={message.fromName} />
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline justify-between gap-2">
          <span className={cn("truncate text-sm", !message.read && "font-semibold")}>{message.fromName}</span>
          <span className="shrink-0 text-[11px] text-muted-foreground">{formatTime(message.at)}</span>
        </div>
        <p className={cn("truncate text-sm", message.read ? "text-muted-foreground" : "font-medium")}>
          {message.subject}
        </p>
        <p className="truncate text-xs text-muted-foreground">{message.preview}</p>
      </div>
      {!message.read && <span className="mt-1.5 size-2 shrink-0 rounded-full bg-primary" />}
    </button>
  );
}

function Reading({ message, inbox, onBack }: { message: EmailMessage; inbox: Inbox; onBack: () => void }) {
  return (
    <>
      <div className="flex items-center gap-2 border-b px-4 py-2.5">
        <Button variant="ghost" size="icon" className="size-8 md:hidden" onClick={onBack} aria-label="Back">
          <ArrowLeft className="size-4" />
        </Button>
        <span className="truncate text-sm font-medium">{message.subject}</span>
        <Badge variant="outline" className="ml-auto shrink-0 gap-1 text-xs">
          <InboxIcon className="size-3" /> {inbox.name}
        </Badge>
      </div>
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto max-w-2xl px-6 py-6">
          <div className="mb-4 flex items-center gap-3">
            <Avatar name={message.fromName} />
            <div className="min-w-0">
              <p className="text-sm font-medium">{message.fromName}</p>
              <p className="truncate text-xs text-muted-foreground">
                {message.fromEmail} · to {slugify(inbox.name)}@company
              </p>
            </div>
            <span className="ml-auto shrink-0 text-xs text-muted-foreground">{formatDateTime(message.at)}</span>
          </div>
          <div className="text-sm leading-relaxed whitespace-pre-wrap">{message.body}</div>
        </div>
      </div>
    </>
  );
}

function Avatar({ name }: { name: string }) {
  return (
    <span className="flex size-9 shrink-0 items-center justify-center rounded-full bg-muted text-xs font-semibold text-muted-foreground">
      {name
        .trim()
        .split(/\s+/)
        .slice(0, 2)
        .map((p) => p.charAt(0).toUpperCase())
        .join("")}
    </span>
  );
}

function formatTime(at: number): string {
  return new Date(at).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
}

function formatDateTime(at: number): string {
  return new Date(at).toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
}
