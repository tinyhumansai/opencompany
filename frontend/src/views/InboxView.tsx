import { useCallback, useEffect, useMemo, useState } from "react";
import { ArrowLeft, Inbox as InboxIcon, Mail } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { InboxDto, InboxMessageDto } from "@/api/types";
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

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/** A short one-line preview derived from the plain-text body. */
function preview(body: string): string {
  return body.replace(/\s+/g, " ").trim().slice(0, 120);
}

/** The email inbox surface — reads a teammate's mail from the server (both the
 * ingest webhook and the IMAP poller file into the same store). */
export function InboxView({ client, company }: Props) {
  const [inboxes, setInboxes] = useState<InboxDto[]>([]);
  const [loadingInboxes, setLoadingInboxes] = useState(true);
  const [activeKey, setActiveKey] = useState<string>("");
  const [messages, setMessages] = useState<InboxMessageDto[]>([]);
  const [loadingMessages, setLoadingMessages] = useState(false);
  const [openId, setOpenId] = useState<string | null>(null);
  const [mobilePane, setMobilePane] = useState<"list" | "read">("list");

  // Load the inbox list on mount / company change. A host without the route
  // (older build) 404s — treat that as "no inboxes" rather than an error wall.
  useEffect(() => {
    let live = true;
    setLoadingInboxes(true);
    client
      .listInboxes(company)
      .then((list) => {
        if (!live) return;
        setInboxes(list);
        setActiveKey((k) => (list.some((i) => i.key === k) ? k : (list[0]?.key ?? "")));
      })
      .catch(() => {
        if (live) setInboxes([]);
      })
      .finally(() => {
        if (live) setLoadingInboxes(false);
      });
    return () => {
      live = false;
    };
  }, [client, company]);

  // Load the active inbox's messages whenever it changes.
  useEffect(() => {
    if (!activeKey) {
      setMessages([]);
      return;
    }
    let live = true;
    setLoadingMessages(true);
    setOpenId(null);
    client
      .inboxMessages(activeKey, company)
      .then((msgs) => {
        if (live) setMessages(msgs);
      })
      .catch(() => {
        if (live) setMessages([]);
      })
      .finally(() => {
        if (live) setLoadingMessages(false);
      });
    return () => {
      live = false;
    };
  }, [client, company, activeKey]);

  const active = useMemo(
    () => inboxes.find((i) => i.key === activeKey) ?? inboxes[0],
    [inboxes, activeKey],
  );
  const unread = useMemo(
    () => messages.filter((m) => !m.outbound && !m.read).length,
    [messages],
  );
  const openMsg = messages.find((m) => m.id === openId) ?? null;

  const openMessage = useCallback(
    (id: string) => {
      setOpenId(id);
      setMobilePane("read");
      const target = messages.find((m) => m.id === id);
      if (!target || target.read || !active) return;
      // Optimistically mark read; persist to the server (fire-and-forget).
      setMessages((ms) => ms.map((m) => (m.id === id ? { ...m, read: true } : m)));
      client.markInboxRead(active.key, [id], company).catch(() => {
        /* leave the optimistic state; a reload reconciles */
      });
    },
    [messages, active, client, company],
  );

  if (loadingInboxes) {
    return (
      <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
        Loading inboxes…
      </div>
    );
  }

  if (inboxes.length === 0) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center gap-3 text-center text-muted-foreground">
        <InboxIcon className="size-8" />
        <div className="space-y-1">
          <p className="font-medium text-foreground">No inboxes yet</p>
          <p className="max-w-sm text-sm">
            Give an agent its own inbox from the <span className="font-medium">Team</span> page —
            flip on the inbox toggle for anyone who needs to receive email. Mail sent to that
            address shows up here.
          </p>
        </div>
      </div>
    );
  }

  const sorted = messages.slice().sort((a, b) => b.atMillis - a.atMillis);

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
          <Select
            value={active?.key}
            onValueChange={(v) => v && (setActiveKey(v), setOpenId(null))}
            items={Object.fromEntries(inboxes.map((i) => [i.key, i.name]))}
          >
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
          {unread > 0 && <Badge variant="secondary">{unread}</Badge>}
        </div>
        <div className="flex-1 overflow-y-auto">
          {loadingMessages ? (
            <div className="p-8 text-center text-sm text-muted-foreground">Loading…</div>
          ) : sorted.length > 0 ? (
            sorted.map((m) => (
              <MessageRow
                key={m.id}
                message={m}
                active={m.id === openId}
                onClick={() => openMessage(m.id)}
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
  message: InboxMessageDto;
  active: boolean;
  onClick: () => void;
}) {
  const who = message.outbound ? "You" : message.fromName || message.fromEmail;
  return (
    <button
      onClick={onClick}
      className={cn(
        "flex w-full items-start gap-3 border-b px-3 py-3 text-left transition-colors",
        active ? "bg-accent" : "hover:bg-accent/50",
      )}
    >
      <Avatar name={who} />
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline justify-between gap-2">
          <span className={cn("truncate text-sm", !message.read && !message.outbound && "font-semibold")}>{who}</span>
          <span className="shrink-0 text-[11px] text-muted-foreground">{formatTime(message.atMillis)}</span>
        </div>
        <p className={cn("truncate text-sm", message.read || message.outbound ? "text-muted-foreground" : "font-medium")}>
          {message.subject}
        </p>
        <p className="truncate text-xs text-muted-foreground">{preview(message.body)}</p>
      </div>
      {!message.read && !message.outbound && <span className="mt-1.5 size-2 shrink-0 rounded-full bg-primary" />}
    </button>
  );
}

function Reading({ message, inbox, onBack }: { message: InboxMessageDto; inbox: InboxDto; onBack: () => void }) {
  const who = message.outbound ? "You" : message.fromName || message.fromEmail;
  const addr = inbox.address || `${inbox.key}@company`;
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
            <Avatar name={who} />
            <div className="min-w-0">
              <p className="text-sm font-medium">{who}</p>
              <p className="truncate text-xs text-muted-foreground">
                {message.fromEmail} · {message.outbound ? "from" : "to"} {addr}
              </p>
            </div>
            <span className="ml-auto shrink-0 text-xs text-muted-foreground">{formatDateTime(message.atMillis)}</span>
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
