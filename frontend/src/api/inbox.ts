// The per-agent inbox helpers the generated client doesn't cover.
//
// The reads themselves live on `OpenCompanyClient` — `listInboxes`,
// `inboxMessages`, `markInboxRead` — against the host's `…/inboxes` routes
// (REST, camelCase over the wire), backed by the same `InboxStore` the ingest
// webhook, the IMAP poller, and every outbound send append to. This module adds
// the inbox *enable* write (which has no client method) plus the two pure
// helpers the Inbox view needs.
//
// Together they replace the client-side `lib/inbox` localStorage fixture, which
// fabricated the same four emails ("Priya Sharma", "Stripe", "Weekly Digest",
// "Figma") for every teammate and could never show mail that actually arrived
// (issue #173). There is deliberately no seed and no local fallback: an empty
// inbox renders as empty, so a backend failure can't be masked by fake data.

import type { OpenCompanyClient } from "./client";
import type { InboxDto } from "./types";

/** The `PUT …/team/{agentId}/inbox` acknowledgement. */
export interface InboxAck {
  key: string;
  address: string;
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
export function enabledInboxes(inboxes: InboxDto[]): InboxDto[] {
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
