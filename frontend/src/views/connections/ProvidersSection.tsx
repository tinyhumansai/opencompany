import { useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  Check,
  ChevronRight,
  Loader2,
  LogIn,
  Search,
  ShieldCheck,
  Unplug,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import {
  availableCategories,
  permissionHint,
  visibleProviderRows,
  type ProviderCategory,
} from "@/lib/composio-catalog";
import {
  accountSummary,
  connectedProviderCount,
  grantStanding,
  tallyAccounts,
  tileDelivers,
} from "@/lib/provider-grid";
import type { GridProvider } from "@/lib/provider-grid";
import { cn } from "@/lib/utils";

interface Props {
  /** Every provider this company can reach, connected first (issue #582). */
  providers: GridProvider[];
  /** Whether this viewer may change what the company connects through (#403). */
  canManage: boolean;
  /** The `providerId` whose connect/disconnect is in flight, if any. */
  busy: string | null;
  /** Nothing to authorize against — no credential of any tier resolves. */
  noCredential: boolean;
  /**
   * Whether the company grants the `composio` tool namespace, as a tri-state
   * (issue #1478): `true` granted, `false` not granted, `undefined` couldn't
   * check (an older host, or response-shape drift through an unvalidated cast).
   *
   * A caveat, never a gate (issue #582). A connection made without it is real —
   * the account is linked and the tile is honest to say so — but no agent
   * receives its tools until the grant exists, and the operator has to be told
   * that where they can see the connected badge, not only in a section above.
   * `undefined` must not render as `false`: an unchecked grant is not a denied
   * one.
   */
  granted: boolean | undefined;
  /**
   * The Composio catalog probe timed out (issue #1478). Distinct from an empty
   * catalog: the host did not answer, so "no providers to offer" would be a
   * confident claim assembled from a request that never completed.
   */
  probeFailed: boolean;
  /** Open mode: any slug the backend permits is reachable, so offer the hatch. */
  openMode: boolean;
  /** Why the catalog on screen is not the backend's real one, when it is not. */
  degraded: string | null;
  /** The host has not answered yet. */
  loading: boolean;
  /**
   * Grant the `composio` tool namespace (issue #1796).
   *
   * A callback rather than a write of its own, for the reason stated below: this
   * grid decides nothing and calls nothing. What it contributes is the *place* —
   * the operator reading "connected" next to a tile is the one who needs to know
   * their agents still cannot use it, and the fix belongs where the complaint
   * is, not only in a section further up the page.
   */
  onGrant?: () => void;
  /** Whether that grant is in flight, so the control can say so. */
  granting?: boolean;
  onConnect: (provider: GridProvider) => void;
  onDisconnect: (provider: GridProvider) => void;
  /** Open a connected provider's detail view (issue #404). */
  onOpen: (provider: GridProvider) => void;
  onConnectSlug: (slug: string) => void;
}

/**
 * The OAuth page's one provider grid (issue #582).
 *
 * The page used to carry two: this tile grid (then inside `ComposioSection`,
 * fed by `GET …/composio/connections`) and a categorised grid of eleven
 * hardcoded tiles fed by `GET …/connections`. They disagreed — routinely and by
 * construction, not as a race — so Gmail could show "connected" in one and an
 * actionable Connect button in the other, on one screen.
 *
 * What survives is this grid, because it is the one that was already right: the
 * providers come from the backend's live catalog rather than a list compiled
 * into the console, and #600 gave it the names, logos, descriptions and
 * categories that made the hardcoded tiles worth keeping in the first place.
 * What it gained is the other grid's job — the reconciled status from
 * `GET …/connections`, the native self-hosted route, and Disconnect — so the
 * page has one list and that list has one answer.
 *
 * Presentational on purpose: it owns the filter chips and the search box, and
 * nothing else. Connected state, the route decision and both writes belong to
 * `ConnectionsView`, which holds the host state they are derived from. A tile
 * that decided its own status is how the page came to have two of them.
 */
export function ProvidersSection({
  providers,
  canManage,
  busy,
  noCredential,
  granted,
  probeFailed,
  openMode,
  degraded,
  loading,
  onGrant,
  granting = false,
  onConnect,
  onDisconnect,
  onOpen,
  onConnectSlug,
}: Props) {
  const [query, setQuery] = useState("");
  const [category, setCategory] = useState<ProviderCategory>("All");
  const [otherToolkit, setOtherToolkit] = useState("");

  const categories = useMemo(() => availableCategories(providers), [providers]);
  const visible = useMemo(
    () => visibleProviderRows(providers, category, query),
    [providers, category, query],
  );

  // A chip can go away under the operator — a company narrows its manifest, or a
  // refresh returns a shorter catalog. Falling back to All beats leaving them
  // staring at an empty grid filtered by a bucket that no longer exists.
  useEffect(() => {
    if (!categories.includes(category)) setCategory("All");
  }, [categories, category]);

  // One count, shared with the header badge in ConnectionsView (issue #1407).
  const connectedCount = connectedProviderCount(providers);
  const grant = grantStanding(granted);

  if (loading) {
    return (
      <section className="space-y-3">
        <SectionHeading count={null} />
        <Skeleton className="h-64 rounded-xl" />
      </section>
    );
  }

  return (
    <section className="space-y-3">
      <SectionHeading count={connectedCount} />
      <Card>
        <CardContent className="space-y-2">
          {noCredential && (
            <p className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
              {canManage
                ? "No credential is available for this company yet, so there is nothing to authorize against. Set the company's TinyHumans credential — one key authorizes the providers the platform brokers — or paste a Composio token below to use a Composio account of your own."
                : "No credential is available for this company yet, so there is nothing to authorize against. An admin has to set the company's credential before providers can be connected."}
            </p>
          )}
          {grant === "not-granted" && connectedCount > 0 && (
            // Stated here rather than only in the credential section above,
            // because this is where the connected badges are: the operator
            // reading "connected" is the one who needs to know their agents
            // still cannot use it. The connection itself is real — the grant
            // governs the tool belt, not the handshake (issue #582). Fires only
            // on an explicit not-granted, never on an unchecked grant (#1478).
            <div
              className="flex flex-col gap-2 rounded-md bg-muted/40 p-2 text-xs text-muted-foreground"
              data-testid="providers-not-granted"
            >
              <span className="flex items-start gap-2">
                <AlertTriangle className="mt-px size-3 shrink-0" />
                <span>
                  These accounts are connected, but this company does not grant the{" "}
                  <span className="font-mono">composio</span> tool namespace, so its teammates will
                  not receive their tools yet.
                </span>
              </span>
              {canManage && onGrant ? (
                <div>
                  <Button
                    size="sm"
                    variant="outline"
                    disabled={granting}
                    onClick={onGrant}
                    data-testid="providers-not-granted-action"
                  >
                    {granting ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <ShieldCheck className="size-4" />
                    )}
                    Grant composio
                  </Button>
                </div>
              ) : null}
            </div>
          )}
          {grant === "unknown" && connectedCount > 0 && (
            // Couldn't read the grant (issue #1478). Neither assert it is granted
            // nor tell the operator to widen a grant that may already be set —
            // say only that this could not be checked.
            <p className="flex items-start gap-2 rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
              <AlertTriangle className="mt-px size-3 shrink-0" />
              <span>
                Couldn&apos;t check whether this company grants the{" "}
                <span className="font-mono">composio</span> tool namespace, so whether teammates
                receive these tools is unknown.
              </span>
            </p>
          )}
          {degraded && (
            // The host could not read Composio's catalog. Say so — a built-in
            // list rendered like a fetched one is a claim we cannot back (#397).
            <p className="flex items-start gap-2 rounded-md bg-status-blocked-soft p-2 text-xs text-status-blocked-text">
              <AlertTriangle className="mt-px size-3 shrink-0" />
              <span>{degraded}</span>
            </p>
          )}
          {!degraded && openMode && (
            <p className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
              This company allows <span className="font-medium">any</span> provider Composio offers
              — {providers.length} in total, connected first. Filter by category or search by name,
              slug, or what a provider does.
            </p>
          )}

          {providers.length > 8 && (
            <div className="relative">
              <Search className="absolute top-1/2 left-2 size-3.5 -translate-y-1/2 text-muted-foreground" />
              <Input
                aria-label="Search providers"
                autoComplete="off"
                className="pl-7"
                placeholder={`Search ${providers.length} providers by name or what they do…`}
                value={query}
                onChange={(e) => setQuery(e.target.value)}
              />
            </div>
          )}

          {categories.length > 2 && (
            // Chips only earn their row when there is more than one real bucket
            // to choose between — `availableCategories` always includes "All",
            // so two means one bucket, which is not a choice.
            <div
              role="group"
              aria-label="Filter providers by category"
              className="flex gap-1.5 overflow-x-auto pb-1"
            >
              {categories.map((c) => (
                <Button
                  key={c}
                  type="button"
                  size="sm"
                  variant={c === category ? "secondary" : "ghost"}
                  aria-pressed={c === category}
                  className={cn(
                    "h-7 shrink-0 rounded-full px-3 text-xs",
                    c !== category && "text-muted-foreground",
                  )}
                  onClick={() => setCategory(c)}
                >
                  {c}
                </Button>
              ))}
            </div>
          )}

          <ul
            className="grid gap-2"
            style={{
              // Uniform rows so a grid of 123 tiles reads as a grid and not as
              // ragged masonry — the tile is a fixed slot, and both the label
              // and the access disclosure clamp (issue #1474).
              gridTemplateColumns: "repeat(auto-fill, minmax(8.5rem, 1fr))",
              gridAutoRows: "8.5rem",
            }}
          >
            {visible.map((row) => (
              <ProviderTile
                key={row.slug}
                row={row}
                canManage={canManage}
                busy={busy === row.providerId}
                anyBusy={busy !== null}
                noCredential={noCredential}
                granted={granted}
                onConnect={() => onConnect(row)}
                onDisconnect={() => onDisconnect(row)}
                onOpen={() => onOpen(row)}
              />
            ))}
          </ul>

          {probeFailed && (
            // The catalog probe timed out (issue #1478): the host did not answer,
            // so this is "couldn't check", not "no providers". Shown in place of
            // the empty-catalog copy below, which would otherwise assemble a
            // confident instruction — possibly to set a credential already set —
            // out of a request that never completed.
            <p
              className="flex items-start gap-2 rounded-md bg-status-blocked-soft p-2 text-xs text-status-blocked-text"
              data-testid="providers-probe-failed"
            >
              <AlertTriangle className="mt-px size-3 shrink-0" />
              <span>
                Couldn&apos;t reach this company&apos;s provider catalog in time, so what it offers
                is unknown. Reload to try again — anything already connected still appears here.
              </span>
            </p>
          )}
          {providers.length === 0 && !probeFailed && (
            // The honest empty state (issue #822). This grid used to fall back
            // to eleven hardcoded tiles whenever the backend offered no catalog,
            // so a host with Composio switched off looked like a page full of
            // connectable providers — and every one of those Connects stored a
            // credential no agent reads (#396). With the fallback gone, a host
            // with no catalog has nothing to show, and saying why beats a bare
            // "No provider in All." Withheld when the probe merely timed out
            // (issue #1478) — that is unknown, not empty.
            <p className="py-2 text-xs text-muted-foreground" data-testid="providers-empty">
              This host has no providers to offer yet. They come from Composio,
              which runs the sign-in and turns the result into tools your teammates
              actually receive
              {canManage
                ? " — set the company's credential above to see its catalog here."
                : " — ask an admin to set the company's credential."}{" "}
              Anything this company has already connected still appears here.
            </p>
          )}

          {visible.length === 0 && providers.length > 0 && (
            <p className="py-2 text-xs text-muted-foreground">
              {query.trim() !== "" ? (
                <>
                  No provider matches “{query.trim()}”
                  {category !== "All" && <> in {category}</>}. Composio&apos;s slug may differ from
                  the product name — try another category, or connect it by slug below.
                </>
              ) : (
                <>No provider in {category}.</>
              )}
            </p>
          )}

          {openMode && canManage && (
            <div className="flex items-end gap-2 border-t border-border pt-3">
              <div className="flex-1 space-y-1">
                <Label htmlFor="providers-other-toolkit" className="text-xs">
                  Connect by slug
                </Label>
                <Input
                  id="providers-other-toolkit"
                  autoComplete="off"
                  placeholder="composio toolkit slug, e.g. hubspot"
                  value={otherToolkit}
                  disabled={noCredential}
                  onChange={(e) => setOtherToolkit(e.target.value)}
                />
              </div>
              <Button
                variant="outline"
                disabled={busy !== null || noCredential || !otherToolkit.trim()}
                onClick={() => {
                  const slug = otherToolkit.trim().toLowerCase();
                  setOtherToolkit("");
                  onConnectSlug(slug);
                }}
              >
                <LogIn className="size-4" />
                Sign in
              </Button>
            </div>
          )}
        </CardContent>
      </Card>
    </section>
  );
}

function SectionHeading({ count }: { count: number | null }) {
  return (
    <div className="space-y-1">
      <h2 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
        Providers
      </h2>
      <p className="text-sm text-muted-foreground">
        Every account this company can act through, and which are wired.
        {count !== null && count > 0 && ` ${count} connected.`}
      </p>
    </div>
  );
}

/**
 * One tile.
 *
 * The whole tile is the affordance when there is exactly one thing to do with
 * it. An 8.5rem tile has no room for a label AND a button, and a tile that looks
 * clickable but is not would be worse than either.
 *
 * A tile Composio can reach opens its detail view instead (issue #404),
 * connected or not: there is an object behind it — accounts, statuses, dates, a
 * per-account revoke, and for a disconnected one what it is and how to connect
 * it — and a single icon button cannot stand for that. It costs the connect
 * flow a click, which is the trade OpenHuman already makes and this issue asks
 * us to inherit rather than re-derive.
 *
 * It is also why the inline Disconnect is gone from those tiles rather than
 * kept alongside: a toolkit can hold two accounts, and a control that names
 * neither has nothing to revoke.
 *
 * The natively-connected tile keeps the old shape — a small Disconnect control
 * in the glyph slot, so the destructive action is never the whole surface an
 * operator brushes past while scanning. It gets no detail view: the native
 * catalog's credential is read by nothing (#396), and a detail view is the one
 * place that inertness would look most like health.
 */
function ProviderTile({
  row,
  canManage,
  busy,
  anyBusy,
  noCredential,
  granted,
  onConnect,
  onDisconnect,
  onOpen,
}: {
  row: GridProvider;
  canManage: boolean;
  busy: boolean;
  anyBusy: boolean;
  noCredential: boolean;
  granted: boolean | undefined;
  onConnect: () => void;
  onDisconnect: () => void;
  onOpen: () => void;
}) {
  // Whether this connected tile's tools actually reach teammates (issue #1407).
  // A real connection whose `composio` grant is explicitly absent is
  // connected-but-not-delivering, so it must not wear the success colour — the
  // banner above already explains it. An unchecked grant is NOT demoted.
  const delivers = tileDelivers(row, granted);
  // `managed` and `unavailable` render no action at all — that is the whole
  // point of routing the tile (issue #599): a button that could only 400 is
  // never drawn. Composio is the only remaining kind that draws one, since the
  // native hatch stopped being offered (issue #822) — a Connect that succeeds
  // and confers nothing is no better than one that fails.
  const connectable = canManage && !row.connected && row.route.kind === "composio";
  // Openable for everyone who can see the page, not only an admin (issue #403
  // gates the writes inside, not the reading): "which account is Gmail wired
  // to, and since when" is exactly what a member opens this page to learn.
  //
  // Connected or not — the issue's own wording, and what OpenHuman does. The
  // one exclusion is a provider Composio reports as connected while the host
  // answered without `accounts` (it predates #696): the panel would open on a
  // connection it cannot name, describe, date or release, which is a worse
  // answer than the tile's badge.
  const openable =
    row.route.kind === "composio" && (!row.connected || row.accounts.length > 0);
  // One rule for counting accounts, shared with the detail panel (issue #923).
  // Two reads used to meet here and disagree: the count was
  // `row.accounts.length` — every account, whatever its state — while the badge
  // gating it was `row.connected`, which the host defines as *at least one
  // account ACTIVE*. So a Gmail holding one live account and five mid-handshake
  // ones said "6 accounts connected", and a Notion holding three mid-handshake
  // ones and no live one said "not connected" two inches above the three
  // accounts it holds. Both now count through `tallyAccounts`.
  const { live, pending } = tallyAccounts(row.accounts);
  // The connected wording, before the delivery caveat. A tile can be genuinely
  // connected and still not deliver (issue #1407), so the "not delivered" suffix
  // is appended to whichever of these applies rather than replacing it.
  const connectedState =
    live > 1
      ? // Only the live ones. A host predating #404 sends no accounts at all
        // while still reporting the toolkit connected, which is why the `via`
        // wording below stays the answer for "connected, nothing to count".
        (accountSummary(row.accounts) ?? "connected")
      : row.via.length > 0
        ? `connected via ${row.via.join(" + ")}`
        : "connected";
  const state = row.connected
    ? delivers
      ? connectedState
      : `${connectedState} · tools not delivered`
    : busy
      ? "signing in"
      : row.unverified
        ? "could not check"
        : row.route.kind === "managed"
          ? "managed by the platform"
          : row.route.kind === "unavailable"
            ? "not available here"
            : pending > 0
              ? // Accounts held, none of them usable. "not connected" here is
                // what contradicted the list below — an account mid-handshake
                // is not the absence of an account.
                (accountSummary(row.accounts) ?? "not connected")
              : "not connected";

  const shell = cn(
    "flex size-full flex-col items-start justify-between gap-1 rounded-lg border p-2.5 text-left",
    row.connected
      ? // A connected-but-not-delivering tile drops the success colour so it
        // does not contradict the "tools reach nobody" banner (issue #1407).
        delivers
        ? "border-status-done/30 bg-status-done-soft"
        : "border-border bg-muted/40"
      : "border-border bg-card",
  );
  // The connected glyph's tone follows delivery too, so a demoted tile is not
  // green-checked under a neutral shell.
  const connectedTone = delivers ? "text-status-done-text" : "text-muted-foreground";

  const glyph = row.connected ? (
    openable ? (
      // Non-interactive: the whole tile is the control that opens it, and a
      // button inside that button is not a thing the DOM allows.
      <ChevronRight className={cn("size-3.5 shrink-0", connectedTone)} aria-hidden="true" />
    ) : canManage && row.canDisconnect ? (
      <button
        type="button"
        disabled={anyBusy}
        onClick={onDisconnect}
        title={`Disconnect ${row.label}`}
        aria-label={`Disconnect ${row.label}`}
        className={cn(
          "shrink-0 rounded p-0.5 text-status-done-text",
          "hover:bg-status-done/20 hover:text-foreground",
          "focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none",
          "disabled:cursor-not-allowed disabled:opacity-60",
        )}
      >
        {busy ? <Loader2 className="size-3.5 animate-spin" /> : <Unplug className="size-3.5" />}
      </button>
    ) : (
      <Check
        className={cn("size-3.5 shrink-0", connectedTone)}
        // Connected, with nothing this console can address: either the viewer
        // cannot manage it, or the host answered the connection list without
        // `accounts` (it predates #696), so there is no id to revoke and no
        // object to open. Saying where the connection lives beats a button that
        // reports success and changes nothing.
        aria-hidden="true"
      />
    )
  ) : busy ? (
    <Loader2 className="size-3.5 shrink-0 animate-spin text-muted-foreground" />
  ) : connectable ? (
    <LogIn className="size-3.5 shrink-0 text-muted-foreground" />
  ) : openable ? (
    // Openable but not connectable: a member, who can read the panel and act on
    // nothing in it. A sign-in glyph would promise them the one thing #403
    // takes away.
    <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />
  ) : null;

  const body = (
    <>
      <div className="flex w-full items-start justify-between gap-1">
        <ProviderLogo row={row} />
        {glyph}
      </div>
      <div className="w-full min-w-0">
        <span className="line-clamp-2 text-xs leading-tight font-medium">{row.label}</span>
        <span
          className={cn(
            "block truncate text-3xs",
            // Delivery-aware, same as the glyph: a connected tile that reaches
            // nobody drops the success colour rather than green-texting its
            // account line under the "tools not delivered" state (issue #1407).
            connectedTone,
          )}
        >
          {row.account ?? state}
        </span>
      </div>
    </>
  );

  const title = openable
    ? row.connected
      ? `Open ${row.label} — accounts, status, and disconnect.`
      : `Open ${row.label} — what it is, and how to connect it.`
    : row.connected && !row.canDisconnect
      ? `${row.label} is connected through Composio; manage or revoke it there.`
      : row.description || undefined;

  return (
    // Keyed by the Composio slug, which is what the host calls it and what every
    // other surface in this issue reconciles on — so a spec that names a tile
    // names the same thing the backend does.
    <li className="min-w-0" data-testid={`provider-${row.slug}`}>
      {openable ? (
        <button
          type="button"
          onClick={onOpen}
          title={title}
          aria-label={`Open ${row.label}. ${state}. Typical access: ${permissionHint(row.category)}.`}
          // Deliberately NOT prefixed `provider-`: `connections-one-list.spec.ts`
          // counts `[data-testid^='provider-']` nodes to prove a provider
          // renders exactly one tile, and a nested node sharing that prefix
          // would make this control look like a second tile.
          data-testid={`open-provider-${row.slug}`}
          className={cn(
            shell,
            "transition-colors hover:border-foreground/20",
            // Hover follows delivery too: a connected-but-not-delivering tile
            // hovers neutral, matching its demoted shell, instead of flashing the
            // success tint the "tools reach nobody" banner contradicts (#1407).
            delivers
              ? "hover:bg-status-done/10"
              : row.connected
                ? "hover:bg-muted/70"
                : "hover:bg-accent",
            "focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none",
          )}
        >
          {body}
          {/* The hint is a broad category-derived guess — Composio decides the
              real consent scopes and does not publish them here. Saying
              "Permission requested" would present that guess as the actual
              grant; "Typical access" labels it as the general shape instead
              (issue #1474). */}
          <p className="mt-2 line-clamp-2 text-left text-xs text-muted-foreground">
            Typical access: {permissionHint(row.category)}.
          </p>
        </button>
      ) : connectable ? (
        <button
          type="button"
          disabled={anyBusy || (row.route.kind === "composio" && noCredential)}
          onClick={onConnect}
          title={row.description || undefined}
          aria-label={`Connect ${row.label}. ${state}. Typical access: ${permissionHint(row.category)}.`}
          className={cn(
            shell,
            "transition-colors hover:border-foreground/20 hover:bg-accent",
            "focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none",
            "disabled:cursor-not-allowed disabled:opacity-60 disabled:hover:border-border disabled:hover:bg-card",
          )}
        >
          {body}
        </button>
      ) : (
        // Connected, unroutable, or a viewer who cannot manage: the tile itself
        // is not a control. Rendered as a div rather than a disabled button so
        // it stays in the reading order — "Gmail, connected" is exactly what a
        // member opened this page to learn, and a disabled button is
        // unfocusable.
        <div className={shell} title={title} aria-label={`${row.label}. ${state}.`}>
          {body}
        </div>
      )}
    </li>
  );
}

function ProviderLogo({ row }: { row: GridProvider }) {
  const [failed, setFailed] = useState(false);
  // A company can repoint at a different backend, which re-keys the logo. Reset
  // on URL change so one dead image does not poison the slot for good.
  useEffect(() => setFailed(false), [row.logoUrl]);

  if (failed) {
    return (
      <span
        aria-hidden="true"
        className="flex size-8 items-center justify-center rounded-lg bg-muted text-xs font-semibold text-muted-foreground"
      >
        {row.label.charAt(0).toUpperCase()}
      </span>
    );
  }
  return (
    <img
      src={row.logoUrl}
      alt=""
      aria-hidden="true"
      loading="lazy"
      className="size-8 rounded-lg object-contain"
      onError={() => setFailed(true)}
    />
  );
}
