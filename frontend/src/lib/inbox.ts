// Per-agent inboxes: give an agent its own email inbox. Toggled on the Team
// page, surfaced on the Inbox page. Persisted per company in localStorage,
// keyed by a stable name slug (agent ids aren't stable across renders).

export interface EmailMessage {
  id: string;
  fromName: string;
  fromEmail: string;
  subject: string;
  preview: string;
  body: string;
  at: number;
  read: boolean;
}

export interface Inbox {
  key: string;
  name: string;
  enabled: boolean;
  messages: EmailMessage[];
}

export type InboxStore = Record<string, Inbox>;

export function slugify(name: string): string {
  return name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "");
}

const KEY = (company: string | null) => `oc-inboxes:${company ?? "single"}`;

export function loadInboxes(company: string | null): InboxStore {
  try {
    const raw = localStorage.getItem(KEY(company));
    if (raw) return JSON.parse(raw) as InboxStore;
  } catch {
    /* fall through to seed */
  }
  return seedInboxes();
}

export function saveInboxes(company: string | null, store: InboxStore): void {
  try {
    localStorage.setItem(KEY(company), JSON.stringify(store));
  } catch {
    /* storage unavailable */
  }
}

/** Enable or disable an agent's inbox, seeding messages on first enable. */
export function toggleInbox(store: InboxStore, name: string): InboxStore {
  const key = slugify(name);
  const existing = store[key];
  if (existing) {
    return { ...store, [key]: { ...existing, enabled: !existing.enabled } };
  }
  return { ...store, [key]: { key, name, enabled: true, messages: seedMessages(name) } };
}

export function isInboxEnabled(store: InboxStore, name: string): boolean {
  return Boolean(store[slugify(name)]?.enabled);
}

export function enabledInboxes(store: InboxStore): Inbox[] {
  return Object.values(store)
    .filter((i) => i.enabled)
    .sort((a, b) => a.name.localeCompare(b.name));
}

export function unreadCount(inbox: Inbox): number {
  return inbox.messages.filter((m) => !m.read).length;
}

let n = 0;
const genId = () => `msg-${Date.now().toString(36)}-${n++}`;

function daysAgo(d: number): number {
  return Date.now() - d * 86_400_000;
}

function seedMessages(name: string): EmailMessage[] {
  const handle = slugify(name);
  return [
    {
      id: genId(),
      fromName: "Priya Sharma",
      fromEmail: "priya@acme.co",
      subject: "Re: Spring campaign timeline",
      preview: "Thanks for the update — Friday works for the review. Can you also…",
      body: "Hi,\n\nThanks for the update — Friday works for the review. Can you also share the draft taglines beforehand so I can loop in the wider team?\n\nBest,\nPriya",
      at: daysAgo(0),
      read: false,
    },
    {
      id: genId(),
      fromName: "Stripe",
      fromEmail: "receipts@stripe.com",
      subject: "Your invoice has been paid",
      preview: "Invoice #INV-2043 for $2,500.00 was paid by Acme Co.",
      body: "Invoice #INV-2043 for $2,500.00 was paid by Acme Co. No action needed.",
      at: daysAgo(1),
      read: false,
    },
    {
      id: genId(),
      fromName: "Weekly Digest",
      fromEmail: "digest@marketingbrew.com",
      subject: "5 trends shaping paid acquisition",
      preview: "This week: creative fatigue, first-party data, and the rise of…",
      body: "This week: creative fatigue, first-party data, and the rise of retail media. Read on for the full breakdown.",
      at: daysAgo(2),
      read: true,
    },
    {
      id: genId(),
      fromName: "Figma",
      fromEmail: "team@figma.com",
      subject: `${name} was mentioned in “Spring hero”`,
      preview: `A teammate left a comment for ${handle}@ on the hero design.`,
      body: `A teammate left a comment for ${handle}@ on the “Spring hero” file. Open Figma to reply.`,
      at: daysAgo(3),
      read: true,
    },
  ];
}

/** Seed one inbox on by default so the Inbox page isn't empty out of the box. */
function seedInboxes(): InboxStore {
  const name = "Front Desk";
  const key = slugify(name);
  return { [key]: { key, name, enabled: true, messages: seedMessages(name) } };
}
