// Issue #302: unmounted from the console — hidden, not retired. The host's
// inbox routes, per-agent store and tests are unchanged; re-listing "inbox" in
// `app-shell.tsx`'s `View`/`NAV` brings this surface straight back. Do not
// delete it as dead code.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ArrowLeft, Inbox as InboxIcon, Info, Mail, Send } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { enabledInboxes, preview } from "@/api/inbox";
import type { InboxDto, InboxMessageDto } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

type Load = "loading" | "ready" | "error";

/**
 * An email inbox surface. Each teammate with an inbox enabled gets its own, read
 * live from the host's `InboxStore` through `client.listInboxes()` /
 * `client.inboxMessages()` — never a client-side fixture, so two teammates show
 * two different sets of mail (issue #173). Both inbound paths file into that
 * store, so ingest-webhook and IMAP-polled mail both land here.
 */
export function InboxView({ client, company }: Props) {
  const [load, setLoad] = useState<Load>("loading");
  const [error, setError] = useState<string | null>(null);
  const [inboxes, setInboxes] = useState<InboxDto[]>([]);
  const [activeKey, setActiveKey] = useState("");
  const [messages, setMessages] = useState<InboxMessageDto[]>([]);
  const [messagesLoading, setMessagesLoading] = useState(false);
  const [messagesError, setMessagesError] = useState<string | null>(null);
  const [messagesReload, setMessagesReload] = useState(0);
  const [openId, setOpenId] = useState<string | null>(null);
  const [mobilePane, setMobilePane] = useState<"list" | "read">("list");

  const listed = useMemo(() => enabledInboxes(inboxes), [inboxes]);
  const active = listed.find((i) => i.key === activeKey) ?? listed[0];
  const openMsg = messages.find((m) => m.id === openId) ?? null;

  /**
   * Generation counter for the roster fetch. Bumped on every load and on
   * teardown, so a response that resolves after the company changed — or after
   * a second "Try again" — is dropped instead of overwriting the newer roster,
   * active key, and unread counts.
   */
  const rosterRun = useRef(0);

  // The inbox roster: which teammates have an inbox, and how much is unread.
  const loadRoster = useCallback(async () => {
    const run = ++rosterRun.current;
    try {
      const rows = await client.listInboxes(company);
      if (run !== rosterRun.current) return;
      setInboxes(rows);
      setActiveKey((current) =>
        rows.some((i) => i.enabled && i.key === current)
          ? current
          : (enabledInboxes(rows)[0]?.key ?? ""),
      );
      setError(null);
      setLoad("ready");
    } catch (cause) {
      // A stale failure must not bury a newer roster either — the error path is
      // gated on the same generation as the success path.
      if (run !== rosterRun.current) return;
      // No fixture fallback: a host that can't serve inboxes says so rather than
      // render invented mail.
      setInboxes([]);
      setError(cause instanceof Error ? cause.message : "Couldn't load inboxes.");
      setLoad("error");
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void loadRoster();
    // Invalidate the in-flight load when the company changes or the view
    // unmounts, so its response can never land on the next company's state.
    return () => {
      rosterRun.current += 1;
    };
  }, [loadRoster]);

  // The selected inbox's mail. Refetched whenever the selection changes, so
  // switching teammates always shows that teammate's own correspondence. The
  // host returns append order (oldest first); the reader wants newest first.
  const activeInboxKey = active?.key;
  useEffect(() => {
    if (!activeInboxKey) {
      setMessages([]);
      setMessagesError(null);
      return;
    }
    let cancelled = false;
    setMessagesLoading(true);
    setMessagesError(null);
    void (async () => {
      try {
        const mail = await client.inboxMessages(activeInboxKey, company);
        if (!cancelled) setMessages(mail.slice().sort((a, b) => b.atMillis - a.atMillis));
      } catch (cause) {
        // A failed read is NOT an empty inbox. Rendering "no messages yet" here
        // would be the fixture bug in a new costume: the console would state
        // something about this teammate's mail that it does not know.
        if (!cancelled) {
          setMessages([]);
          setMessagesError(
            cause instanceof Error ? cause.message : "Couldn't load this inbox.",
          );
        }
      } finally {
        if (!cancelled) setMessagesLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [client, company, activeInboxKey, messagesReload]);

  async function openMessage(inboxKey: string, message: InboxMessageDto) {
    setOpenId(message.id);
    setMobilePane("read");
    if (message.read) return;
    // Optimistic locally, authoritative from the host: the response carries the
    // remaining unread count the badge renders.
    setMessages((ms) => ms.map((m) => (m.id === message.id ? { ...m, read: true } : m)));
    try {
      const { unread } = await client.markInboxRead(inboxKey, [message.id], company);
      setInboxes((is) => is.map((i) => (i.key === inboxKey ? { ...i, unread } : i)));
    } catch {
      // Leave the row read locally; the next roster load reconciles with the host.
    }
  }

  if (load === "loading") {
    return (
      <div className="flex flex-1 flex-col overflow-hidden">
        <InboxParkingNotice />
        <div className="flex flex-1 flex-col gap-2 p-4">
          <PageHeader hidden title="Inbox" />
          {Array.from({ length: 6 }).map((_, i) => (
            <Skeleton key={i} className="h-16 rounded-lg" />
          ))}
        </div>
      </div>
    );
  }

  if (load === "error") {
    return (
      <div className="flex flex-1 flex-col overflow-hidden">
        <InboxParkingNotice />
        <div className="flex flex-1 flex-col items-center justify-center gap-3 text-center text-muted-foreground">
          <PageHeader hidden title="Inbox" />
          <InboxIcon className="size-8" />
          <div className="space-y-1">
            <p className="font-medium text-foreground">Inboxes unavailable</p>
            <p className="max-w-sm text-sm">{error}</p>
          </div>
          <Button variant="outline" size="sm" onClick={() => void loadRoster()}>
            Try again
          </Button>
        </div>
      </div>
    );
  }

  if (listed.length === 0) {
    return (
      <div className="flex flex-1 flex-col overflow-hidden">
        <InboxParkingNotice />
        <div className="flex flex-1 flex-col items-center justify-center gap-3 text-center text-muted-foreground">
          <PageHeader hidden title="Inbox" />
          <InboxIcon className="size-8" />
          <div className="space-y-1">
            <p className="font-medium text-foreground">No inboxes yet</p>
            <p className="max-w-sm text-sm">
              Give a teammate its own inbox from the{" "}
              <a className="font-medium text-foreground underline-offset-4 hover:underline" href="#/company">
                Company page
              </a>{" "}
              — open a teammate to flip on the inbox toggle for anyone who needs to receive email.
              Mail sent to that address shows up here.
            </p>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-1 flex-col overflow-hidden">
      <InboxParkingNotice />
      <div className="flex min-h-0 flex-1 overflow-hidden">
        <PageHeader hidden title="Inbox" />
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
              items={Object.fromEntries(listed.map((i) => [i.key, i.name]))}
            >
              <SelectTrigger className="h-8 flex-1" data-testid="inbox-select">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {listed.map((i) => (
                  <SelectItem key={i.key} value={i.key}>
                    {i.name}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            {active && active.unread > 0 && <Badge variant="secondary">{active.unread}</Badge>}
          </div>
          <div className="flex-1 overflow-y-auto" data-testid="inbox-list">
            {messagesLoading ? (
              <div className="space-y-2 p-3">
                {Array.from({ length: 4 }).map((_, i) => (
                  <Skeleton key={i} className="h-14 rounded-lg" />
                ))}
              </div>
            ) : messagesError ? (
              <div
                className="flex flex-col items-center gap-3 p-8 text-center text-sm text-muted-foreground"
                data-testid="inbox-messages-error"
              >
                <Mail className="size-6" />
                <div className="space-y-1">
                  <p className="font-medium text-foreground">Couldn't load this inbox</p>
                  <p className="max-w-xs">{messagesError}</p>
                </div>
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setMessagesReload((n) => n + 1)}
                >
                  Try again
                </Button>
              </div>
            ) : messages.length > 0 ? (
              messages.map((m) => (
                <MessageRow
                  key={m.id}
                  message={m}
                  active={m.id === openId}
                  onClick={() => active && void openMessage(active.key, m)}
                />
              ))
            ) : (
              <div className="p-8 text-center text-sm text-muted-foreground" data-testid="inbox-empty">
                No messages yet. Mail sent to{" "}
                <span className="font-medium">{active?.address || active?.key}</span> lands here.
              </div>
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
    </div>
  );
}

/** A persistent explanation for Inbox's deliberately direct-URL-only state. */
function InboxParkingNotice() {
  // The Alert base supplies `w-full`, so a horizontal margin on the alert itself
  // would make it 2rem wider than its `overflow-hidden` parent and get its right
  // edge clipped. Pad the wrapper instead and keep the alert at full width.
  return (
    <div className="shrink-0 px-4 pt-4">
      <Alert data-testid="inbox-parked-notice">
        <Info className="size-4" />
        <AlertTitle>Inbox is not in the console navigation right now</AlertTitle>
        <AlertDescription>
          This page still works and shows live email data, but nothing in the console links to it.
          Reach it with a direct link to this address.
        </AlertDescription>
      </Alert>
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
  const unread = !message.read && !message.outbound;
  return (
    <button
      onClick={onClick}
      data-testid="inbox-message"
      className={cn(
        "flex w-full items-start gap-3 border-b px-3 py-3 text-left transition-colors",
        active ? "bg-accent" : "hover:bg-accent/50",
      )}
    >
      <Avatar name={sender(message)} />
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline justify-between gap-2">
          <span className={cn("truncate text-sm", unread && "font-semibold")}>{sender(message)}</span>
          <span className="shrink-0 text-2xs text-muted-foreground">{formatTime(message.atMillis)}</span>
        </div>
        <p className={cn("truncate text-sm", unread ? "font-medium" : "text-muted-foreground")}>
          {message.outbound && <Send className="mr-1 inline size-3 align-[-1px]" aria-label="Sent" />}
          {message.subject || "(no subject)"}
        </p>
        <p className="truncate text-xs text-muted-foreground">{preview(message.body)}</p>
      </div>
      {unread && <span className="mt-1.5 size-2 shrink-0 rounded-full bg-primary" />}
    </button>
  );
}

function Reading({
  message,
  inbox,
  onBack,
}: {
  message: InboxMessageDto;
  inbox: InboxDto;
  onBack: () => void;
}) {
  const box = inbox.address || inbox.key;
  return (
    <>
      <div className="flex items-center gap-2 border-b px-4 py-2.5">
        <Button variant="ghost" size="icon" className="size-8 md:hidden" onClick={onBack} aria-label="Back">
          <ArrowLeft className="size-4" />
        </Button>
        <span className="truncate text-sm font-medium">{message.subject || "(no subject)"}</span>
        <Badge variant="outline" className="ml-auto shrink-0 gap-1 text-xs">
          <InboxIcon className="size-3" /> {inbox.name}
        </Badge>
      </div>
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto max-w-2xl px-6 py-6">
          <div className="mb-4 flex items-center gap-3">
            <Avatar name={sender(message)} />
            <div className="min-w-0">
              <p className="text-sm font-medium">{sender(message)}</p>
              <p className="truncate text-xs text-muted-foreground">
                {/* A sent record carries the sending box's own address and no
                    recipient, so outbound reads as "sent from this box". */}
                {message.outbound ? `Sent from ${box}` : `${message.fromEmail} · to ${box}`}
              </p>
            </div>
            <span className="ml-auto shrink-0 text-xs text-muted-foreground">
              {formatDateTime(message.atMillis)}
            </span>
          </div>
          <div className="text-sm leading-relaxed whitespace-pre-wrap">{message.body}</div>
        </div>
      </div>
    </>
  );
}

/**
 * The name to show for a message — ingest often supplies only an address, and a
 * sent copy comes from the operator's own company rather than a correspondent.
 */
function sender(message: InboxMessageDto): string {
  if (message.outbound) return "You";
  return message.fromName.trim() || message.fromEmail || "Unknown sender";
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
