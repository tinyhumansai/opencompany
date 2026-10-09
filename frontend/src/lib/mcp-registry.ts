// What a merged MCP row is allowed to do, and how a directory failure reads
// (issue #1270).
//
// ## Why these are functions and not conditions in the row
//
// `GET …/mcp/servers` now returns two structurally different things in one list.
// A List A row — manifest, default, or an operator-typed runtime entry — is
// addressed by `name`, and every control the row has ever offered writes to a
// `…/mcp/servers/{name}/…` route that resolves that name against List A's
// declarations. A directory install has no such declaration: its `name` is a
// display slug the host mints for the merged view, so the *same* Switch, the
// same Test and the same Tools button answer `no MCP server named …` on it.
//
// So provenance does not merely pick a badge — it picks which half of the API
// the row may talk to at all. One function answering that for the whole row is
// what keeps the four decisions (toggle, probe, lifecycle, delete) from drifting
// apart into four inline conditions that each get it right on a different day.
//
// ## Provenance comes from `source`, never from `serverId`
//
// A server can be both: declared in `company.toml` *and* installed from the
// directory. The host reconciles those into **one row** carrying List A's
// provenance, because List A is the half the agents actually reach and the half
// whose delete guard is real — a manifest declaration cannot be deleted, only
// disabled, and a delete that uninstalled the directory copy would leave the
// declared server exactly where it was while reporting success. Such a row
// carries a `serverId`, so reading "has a serverId" as "is a registry row" is
// precisely the mistake that removes the wrong thing.

import { ApiError } from "@/api/types";
import type { McpHealth, McpServer, McpSource } from "@/api/types";

/**
 * How a row is removed, if at all.
 *
 * - `none` — a manifest or install-wide default. The declaration outlives any
 *   delete, so the row offers a disable instead.
 * - `index` — `DELETE …/mcp/servers/{name}`. The host removes the runtime index
 *   row *and* any directory install reconciled onto it, so one call is the whole
 *   removal even on a reconciled row.
 * - `install` — `DELETE …/mcp/registry/{serverId}`. The only delete a
 *   registry-badged row may use.
 */
export type McpRemoval =
  | { kind: "none" }
  | { kind: "index"; name: string }
  | { kind: "install"; serverId: string };

/**
 * Which connection control a row offers, and which way it points.
 *
 * Scoped to `registry` rows on purpose. A reconciled row *has* an install and
 * the connect routes would accept its `serverId`, but what the company's agents
 * reach through that row is List A's half — so a Disconnect there would drop a
 * session the row does not represent while its tools kept working. The row
 * already has the control that governs its own half: the Switch.
 */
export type McpLifecycle = "none" | "connect" | "disconnect";

/** Everything a merged row's right edge is entitled to offer. */
export interface McpRowControls {
  /** List A's enable/disable Switch (`PUT …/mcp/servers/{name}`). */
  toggle: boolean;
  /**
   * List A's on-demand probe and live tool discovery
   * (`…/mcp/servers/{name}/test` and `/tools`). Both resolve the name against
   * List A's declarations, which a registry install has none of.
   */
  probe: boolean;
  lifecycle: McpLifecycle;
  removal: McpRemoval;
}

/** Whether this row has a List A declaration behind its `name`. */
function declaredInListA(source: McpSource): boolean {
  return source !== "registry";
}

/**
 * How this row is removed.
 *
 * Reads `source` and only then `serverId`: the guard on a manifest or default
 * row holds whether or not a directory install reconciled onto it.
 */
export function mcpRemoval(server: McpServer): McpRemoval {
  switch (server.source) {
    case "manifest":
    case "default":
      return { kind: "none" };
    case "runtime":
      return { kind: "index", name: server.name };
    case "registry":
      // A `registry` row with no id is a host we cannot address. Offering a
      // delete we have no key for would remove nothing and report success.
      return server.serverId ? { kind: "install", serverId: server.serverId } : { kind: "none" };
  }
}

/**
 * Whether this row's only credential path is a browser sign-in the console
 * cannot start.
 *
 * A directory install is authorised by writing named env values to the host's
 * registry store. A server that answers `oauth_required` is asking for
 * something else entirely, and the registry routes expose no `oauth/start` —
 * so every credential control the console has is one that cannot authorise
 * this row.
 */
export function registryOauthUnsupported(
  server: Pick<McpServer, "source">,
  health: McpHealth | undefined,
): boolean {
  return (
    server.source === "registry" &&
    health?.status === "needs_config" &&
    health.authHint === "oauth_required"
  );
}

/** What the console says on a row {@link registryOauthUnsupported} holds true of. */
export const REGISTRY_OAUTH_UNSUPPORTED_NOTICE =
  "This console can't sign a directory install in, so this server can't be authorised here yet.";

/**
 * Which way a registry row's connection control points.
 *
 * `ok` is the only state that means a live session exists; every other state —
 * refused for a credential, failed, disabled, connecting, never dialled — is one
 * a Connect is the right offer for, except the one the host has already refused
 * for a credential this console cannot supply.
 */
export function mcpLifecycle(server: McpServer, health: McpHealth | undefined): McpLifecycle {
  if (server.source !== "registry" || !server.serverId) return "none";
  if (health?.status === "ok") return "disconnect";
  return registryOauthUnsupported(server, health) ? "none" : "connect";
}

/**
 * The whole control set for one merged row.
 *
 * `health` is the row's *effective* health — a live Test result this session
 * where there is one, else the persisted probe — so the connection control
 * agrees with the badge beside it.
 */
export function mcpRowControls(
  server: McpServer,
  health: McpHealth | undefined,
): McpRowControls {
  const listA = declaredInListA(server.source);
  return {
    toggle: listA,
    probe: listA,
    lifecycle: mcpLifecycle(server, health),
    removal: mcpRemoval(server),
  };
}

/** The source badge: the host's own word for the provenance, and its weight. */
export function mcpSourceBadge(source: McpSource): {
  label: McpSource;
  variant: "secondary" | "outline";
} {
  // The label is the host's vocabulary verbatim — the tab has badged `manifest`
  // and `runtime` since issue #50 and an operator reads them as the words they
  // are, so a fourth provenance joins as its own word rather than being folded
  // into one of the three.
  return { label: source, variant: source === "manifest" ? "secondary" : "outline" };
}

/**
 * Why the directory surface is not answering.
 *
 * - `unwired` — this build has no `mcp` feature, so the registry routes answer
 *   `404 not_wired`. A fact about the deployment, not a failure: the console
 *   says the feature is not in this build rather than showing an error.
 * - `error` — anything else. The directories are federated over the network and
 *   either of them can be down; that is an empty result with a reason, never a
 *   broken tab.
 *
 * **Total by construction.** Every failure the browse surface can see resolves
 * to one of these two, including a rejection that is not an `ApiError` at all,
 * because the one outcome this must never produce is an exception escaping into
 * the section that renders the company's server list. A directory being down
 * cannot be allowed to take List A's rows off the screen with it.
 */
export type McpRegistryOutage = { kind: "unwired" } | { kind: "error"; message: string };

export function registryOutage(err: unknown): McpRegistryOutage {
  if (err instanceof ApiError) {
    if (err.code === "not_wired") return { kind: "unwired" };
    if (err.code === "timeout") return { kind: "error", message: DIRECTORY_SLOW };
    if (OPAQUE_CODES.has(err.code)) return { kind: "error", message: DIRECTORY_DOWN };
    if (err.message.trim()) return { kind: "error", message: err.message };
  }
  return { kind: "error", message: DIRECTORY_DOWN };
}

const DIRECTORY_DOWN = "The MCP directory didn't answer. Try again in a moment.";
const DIRECTORY_SLOW =
  "The MCP directory is taking too long to answer. Try again in a moment.";

/** Codes whose message is internal detail, not prose for an operator. */
const OPAQUE_CODES = new Set(["harness_error", "internal", "unexpected_shape"]);

/** What the console says when the directory surface is missing from the build. */
export const REGISTRY_UNWIRED_NOTICE =
  "Browsing the MCP directory isn't enabled in this build (the host was compiled without the MCP feature). The servers above still work.";

/**
 * What a zero-row search means, in the operator's terms.
 *
 * Zero rows has two causes and only one of them is the operator's to fix — a
 * real miss, or a directory whose hosted entries this host cannot run — and the
 * honest surface is the one that names the difference. A bare "nothing matched"
 * reads as a broken search.
 */
export function directoryEmptyNotice(): string {
  return (
    "No hosted servers matched that. The directory only offers servers this host can dial over " +
    "HTTP — one that runs as a local subprocess is not listed."
  );
}

/**
 * The declared env keys an install is still missing a value for.
 *
 * Trimmed, because a key whose value is whitespace is a key the operator has
 * not filled in — and an install that silently stores it would come back
 * `unauthorized` with a credential the console reports as configured.
 */
export function missingEnvKeys(
  requiredKeys: readonly string[],
  values: Readonly<Record<string, string>>,
): string[] {
  return requiredKeys.filter((key) => !(values[key] ?? "").trim());
}

/**
 * What a row's provenance means, for the detail panel (issue #821).
 *
 * Four provenances, four sentences: the panel used to read `manifest` against
 * everything-else, which told an operator that a directory install "was added
 * from the console and lives in this company's runtime store" — true of neither
 * half of it.
 */
export function mcpProvenanceNote(source: McpSource): string {
  switch (source) {
    case "manifest":
      return "Declared in this company's company.toml, so it comes back on every boot — it can be turned off here but not deleted.";
    case "default":
      return "Shipped enabled by this installation rather than by this company, so it is present for every company here — it can be turned off but not deleted.";
    case "registry":
      return "Installed from an upstream MCP directory, so it lives in this host's registry store rather than in company.toml — it is connected and disconnected here, and uninstalling removes it.";
    case "runtime":
      return "Added from the console, so it lives in this company's runtime store rather than in company.toml.";
  }
}

/** What a removal reaches, for the detail panel. Mirrors {@link mcpRemoval}. */
export function mcpRemovalNote(source: McpSource): string {
  switch (source) {
    case "manifest":
      return "This server is declared in the company's bundle (company.toml or mcp.json), so it cannot be removed from the console — turning it off drops it from every agent's tool belt on the next turn, and it returns on the next boot unless the manifest changes.";
    case "default":
      return "This server ships with the installation, so it cannot be removed from the console — turning it off drops it from every agent's tool belt on the next turn.";
    case "registry":
      return "Uninstalling it removes the install from this host's registry store, closes its session and deletes the credential values stored for it.";
    case "runtime":
      return "Removing it drops it from every agent's tool belt on the next turn and deletes the credential stored here for it.";
  }
}

/**
 * Who publishes a directory entry: the host of its website, else the owner its
 * qualified name encodes (`com.notion/mcp` → `notion.com`,
 * `io.github.acme/server` → `acme`, `@acme/server` → `acme`).
 */
export function catalogPublisher(entry: {
  qualifiedName: string;
  websiteUrl?: string;
}): string | null {
  if (entry.websiteUrl) {
    try {
      const host = new URL(entry.websiteUrl).hostname.replace(/^www\./, "");
      if (host) return host;
    } catch {
      // Not a URL; fall through to the qualified name.
    }
  }
  const namespace = entry.qualifiedName.split("/")[0]?.trim() ?? "";
  if (namespace.startsWith("@")) return namespace.slice(1) || null;
  const parts = namespace.split(".").filter(Boolean);
  if (parts.length < 2) return null;
  if (parts[0] === "io" && parts[1] === "github" && parts.length > 2) {
    return parts.slice(2).join(".");
  }
  return parts.reverse().join(".");
}

/** The name the host saves a directory install under: its shown name as a slug. */
export function directoryServerName(displayName: string): string {
  return displayName
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

/** The name a server is shown under: its own title when it has one. */
export function mcpDisplayName(server: Pick<McpServer, "name" | "probedTitle">): string {
  return server.probedTitle?.trim() || server.name;
}
