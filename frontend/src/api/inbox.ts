// The live per-agent inbox API: the console reads each teammate's *real*
// correspondence through the host's `…/inboxes` routes (REST, camelCase over the
// wire) — the same `InboxStore` the inbound ingest webhook and every outbound
// send append to.
//
// This replaces the client-side `lib/inbox` localStorage fixture, which
// fabricated the same four emails ("Priya Sharma", "Stripe", "Weekly Digest",
// "Figma") for every teammate and could never show mail that actually arrived
// (issue #173). There is deliberately no seed and no local fallback: an empty
// inbox renders as empty, so a backend failure can't be masked by fake data.

import type { OpenCompanyClient } from "./client";

/** One email in a teammate's inbox — inbound or outbound. Mirrors `EmailRecord`. */
export interface EmailMessage {
  id: string;
  /** The inbox key this message is filed under (the teammate's agent id). */
  inbox: string;
  /** The sender's display name; may be empty (ingest doesn't always supply one). */
  fromName: string;
  fromEmail: string;
  subject: string;
  /** The plain-text body. There is no server-side preview; see `preview()`. */
  body: string;
  /** When it arrived / was sent, epoch millis. */
  atMillis: number;
  read: boolean;
  /** True for mail the company sent, false for mail it received. */
  outbound: boolean;
}

/** One teammate inbox as `GET …/inboxes` lists it. */
export interface Inbox {
  /** The inbox key — the teammate's agent id / address local part. */
  key: string;
  name: string;
  /** The full address (`{key}@{domain}`); empty until a domain is configured. */
  address: string;
  /** Whether it is receiving mail (the Team page toggle). */
  enabled: boolean;
  /** Unread *received* mail; a sent copy never counts. */
  unread: number;
  /** Every message filed here, inbound and outbound. */
  total: number;
}

/** A page of one inbox's mail, newest first. */
export interface MessagesPage {
  items: EmailMessage[];
  /** The unpaginated message count. */
  total: number;
}

/** The `PUT …/team/{agentId}/inbox` acknowledgement. */
export interface InboxAck {
  key: string;
  address: string;
}

/** Every inbox this company owns, enabled or not, with its unread count. */
export function listInboxes(
  client: OpenCompanyClient,
  company: string | null,
): Promise<Inbox[]> {
  return client.get<Inbox[]>(`${client.scopeFor(company)}/inboxes`);
}

/** One teammate's mail, newest first. */
export function inboxMessages(
  client: OpenCompanyClient,
  company: string | null,
  key: string,
  opts?: { limit?: number; offset?: number },
): Promise<MessagesPage> {
  const params = new URLSearchParams();
  if (opts?.limit !== undefined) params.set("limit", String(opts.limit));
  if (opts?.offset !== undefined) params.set("offset", String(opts.offset));
  const qs = params.toString();
  return client.get<MessagesPage>(
    `${client.scopeFor(company)}/inboxes/${encodeURIComponent(key)}/messages${qs ? `?${qs}` : ""}`,
  );
}

/**
 * Mark mail read — the given `ids`, or the whole inbox when omitted. Returns the
 * count still unread, so the badge follows the host rather than local state.
 */
export function markInboxRead(
  client: OpenCompanyClient,
  company: string | null,
  key: string,
  ids?: string[],
): Promise<{ unread: number }> {
  return client.post<{ unread: number }>(
    `${client.scopeFor(company)}/inboxes/${encodeURIComponent(key)}/read`,
    ids ? { ids } : {},
  );
}

/** Give a teammate an inbox, or take it away. Keyed by the roster **agent id**. */
export function setInboxEnabled(
  client: OpenCompanyClient,
  company: string | null,
  agentId: string,
  enabled: boolean,
): Promise<InboxAck> {
  return client.put<InboxAck>(
    `${client.scopeFor(company)}/team/${encodeURIComponent(agentId)}/inbox`,
    { enabled },
  );
}

/** Inboxes the operator has switched on, name-sorted for the selector. */
export function enabledInboxes(inboxes: Inbox[]): Inbox[] {
  return inboxes.filter((i) => i.enabled).sort((a, b) => a.name.localeCompare(b.name));
}

/**
 * A one-line preview of a message body. The host stores the full body only —
 * the fixture's hand-written `preview` field had no real counterpart — so the
 * list derives its snippet here.
 */
export function preview(body: string, max = 110): string {
  const flat = body.replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max - 1).trimEnd()}…` : flat;
}
