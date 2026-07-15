// The catalog of third-party accounts a company can act through. This is the
// console's view of what *can* be connected; whether a host can actually run
// the OAuth handshake depends on its `/connections` surface (see the client).

export type ConnectionCategory =
  | "Communication"
  | "Productivity"
  | "Developer"
  | "Finance"
  | "Social"
  | "Storage";

export interface ConnectionProvider {
  id: string;
  name: string;
  description: string;
  category: ConnectionCategory;
  /** Brand-ish color for the provider's monogram tile. */
  color: string;
  /** Short glyph for the tile (1–2 chars). Falls back to the name initial. */
  glyph?: string;
}

export const CONNECTION_PROVIDERS: ConnectionProvider[] = [
  {
    id: "gmail",
    name: "Gmail",
    description: "Send and read email from a connected inbox.",
    category: "Communication",
    color: "#EA4335",
    glyph: "M",
  },
  {
    id: "slack",
    name: "Slack",
    description: "Post updates and take requests from your workspace.",
    category: "Communication",
    color: "#4A154B",
    glyph: "#",
  },
  {
    id: "google-calendar",
    name: "Google Calendar",
    description: "Schedule and read events on a shared calendar.",
    category: "Productivity",
    color: "#4285F4",
    glyph: "31",
  },
  {
    id: "notion",
    name: "Notion",
    description: "Read and write docs and databases.",
    category: "Productivity",
    color: "#0F0F0F",
    glyph: "N",
  },
  {
    id: "google-drive",
    name: "Google Drive",
    description: "Store and retrieve files and deliverables.",
    category: "Storage",
    color: "#1FA463",
    glyph: "△",
  },
  {
    id: "dropbox",
    name: "Dropbox",
    description: "Sync assets and shared folders.",
    category: "Storage",
    color: "#0061FF",
    glyph: "▽",
  },
  {
    id: "github",
    name: "GitHub",
    description: "Open issues and pull requests in your repos.",
    category: "Developer",
    color: "#181717",
    glyph: "GH",
  },
  {
    id: "stripe",
    name: "Stripe",
    description: "Create invoices and read payment activity.",
    category: "Finance",
    color: "#635BFF",
    glyph: "S",
  },
  {
    id: "hubspot",
    name: "HubSpot",
    description: "Sync contacts and deals in your CRM.",
    category: "Finance",
    color: "#FF7A59",
    glyph: "H",
  },
  {
    id: "x",
    name: "X",
    description: "Publish posts and read mentions.",
    category: "Social",
    color: "#000000",
    glyph: "X",
  },
  {
    id: "linkedin",
    name: "LinkedIn",
    description: "Publish updates and manage your page.",
    category: "Social",
    color: "#0A66C2",
    glyph: "in",
  },
];

export const CONNECTION_CATEGORY_ORDER: ConnectionCategory[] = [
  "Communication",
  "Productivity",
  "Developer",
  "Finance",
  "Social",
  "Storage",
];
