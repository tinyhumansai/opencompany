import { useCallback, useEffect, useRef, useState } from "react";
import {
  AlertTriangle,
  Check,
  KeyRound,
  Loader2,
  Plug,
  Save,
  ShieldCheck,
  Trash2,
  Wallet,
} from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  getComposioStatus,
  setComposioApiKey,
  setComposioToken,
  type ComposioMode,
  type ComposioStatus,
} from "@/api/composio";
import { ApiError } from "@/api/types";
import { grantStanding } from "@/lib/provider-grid";
import { classifyLoadFailure } from "@/lib/section-load";
import { SectionUnreachable } from "@/views/connections/SectionUnreachable";
import { GrantNamespace } from "@/components/grant-namespace";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";
import { COMPOSIO_MANAGED_HIDDEN } from "@/product-scope";

/**
 * The routes to Composio.
 *
 * One table so the tile's label, its explanation and its billing line cannot
 * drift apart, and so the order below is the order they render. Every route stays
 * here even when it is not offered: this is what labels the route a company is
 * already on.
 */
const MODES: Record<ComposioMode, { label: string; blurb: string; billed: string }> = {
  managed: {
    label: "OpenHuman-managed",
    blurb:
      "Reached through OpenHuman, which holds the Composio account. Nothing to paste; the providers on offer are the ones OpenHuman permits.",
    billed: "Billed by OpenHuman",
  },
  byok: {
    label: "This company's own Composio account",
    blurb:
      "Calls go straight to Composio with this company's API key. Nothing is proxied here, and the providers on offer are whatever that account has.",
    billed: "Billed by Composio, to you",
  },
};

/** The routes in render order — also the order the arrow keys walk. */
const ALL_MODE_ORDER: ComposioMode[] = ["managed", "byok"];

/** The routes on offer. {@link MODES} keeps every descriptor; only this list is filtered. */
const MODE_ORDER: ComposioMode[] = ALL_MODE_ORDER.filter(
  (mode) => mode !== "managed" || !COMPOSIO_MANAGED_HIDDEN,
);

/** Whether this console offers `mode` as something to choose. */
function isOffered(mode: ComposioMode): boolean {
  return MODE_ORDER.includes(mode);
}

/**
 * The route the form works in.
 *
 * With one route on offer there is nothing to choose, so the form opens on it
 * rather than behind a picker: the operator lands on the credential field for
 * the only account this company can use. `persistedMode` still reports what the
 * host holds, which is what the switch confirmation keys on.
 */
function formModeFor(status: ComposioStatus | null): ComposioMode {
  return MODE_ORDER.length === 1 ? MODE_ORDER[0] : modeOf(status);
}

/**
 * What a route this console does not offer is called on screen.
 *
 * Says nothing about the route it stands in for. `MODES` still holds that
 * route's own name, which is a brand for a choice this console does not
 * present — so the tile that stands in for it says only what is true of every
 * value that lands there: nothing is configured here.
 */
const NOT_CONFIGURED = "Not configured";

/**
 * The route to render for whatever the host said.
 *
 * A host predating BYOK omits `mode`, and one from a future shape could name a
 * route this console has no copy for. Both read as `managed`: it is the only
 * route every host has, and the one whose controls are safe to offer when we do
 * not know. Narrowing here means nothing below can index {@link MODES} with a
 * key it does not hold.
 */
function modeOf(status: ComposioStatus | null): ComposioMode {
  const mode = status?.mode;
  return mode && mode in MODES ? mode : "managed";
}

/**
 * Whether the legacy managed-route token card belongs on screen.
 *
 * Requires the SELECTED tile (`mode`) and the PERSISTED route (`!onByok`) to
 * both be managed — not either alone. Either alone puts a second credential
 * surface on screen at exactly the moment an operator is switching between
 * them, and each direction breaks a different way:
 *
 * - Selected-only (`mode === "managed"`, ignoring `onByok`) shows this card to
 *   a BYOK company that has merely *clicked* the managed tile, alongside the
 *   real "Clear key & use OpenHuman-managed" control. This card's own Clear
 *   calls the legacy `setComposioToken("")`, which erases the *preserved*
 *   backend-token override (issue #586) without touching `composio/api_key`
 *   or `composio/mode` at all — a button that looks like the way back to
 *   managed but silently destroys a token the design keeps specifically to
 *   restore, while leaving the company on BYOK regardless.
 * - Persisted-only (`!onByok`, ignoring `mode`) shows it to a managed company
 *   that has clicked the BYOK tile, so a Composio API key field and this
 *   legacy backend-token field are both live with no way to tell which one
 *   the Save below belongs to.
 *
 * Requiring both is the only gate under which exactly one credential surface
 * is ever on screen, in either direction of the switch.
 */
export function showManagedTokenCard(input: {
  mode: ComposioMode;
  onByok: boolean;
  canManage: boolean;
  credentialed: boolean;
  showOverride: boolean;
  byoToken: boolean;
}): boolean {
  return (
    input.mode === "managed" &&
    !input.onByok &&
    input.canManage &&
    (!input.credentialed || input.showOverride || input.byoToken)
  );
}

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * Whether this viewer may change what the company connects through (issue
   * #403) — the credential its agents present, and which provider accounts
   * they act through.
   *
   * **Courtesy, not enforcement.** The host refuses both writes with a 403
   * whatever this says. What it prevents is offering a token field whose Save is
   * refused only after the operator has already pasted a live credential into
   * it.
   */
  canManage: boolean;
  /**
   * Called after the stored token changes.
   *
   * The provider grid's status and routing are downstream of which credential
   * this company reaches Composio with — setting or clearing a token here flips
   * `credentialSource`, and every tile's route with it. Without this the grid
   * would keep rendering the old answer while this section reported the new one:
   * the same two-surfaces-disagreeing failure #582 is about, arriving through
   * the credential rather than through the connection list.
   */
  onChanged: () => void;
}

/**
 * Which credential this company reaches Composio with (issue #110, Cell D).
 *
 * **Only that.** This section used to carry a second thing — a per-provider
 * "Sign in per provider" grid — and that grid was one of the two provider lists
 * issue #582 collapsed. The page now has a single grid, `ProvidersSection`,
 * built from the reconciled `GET …/connections` status; what is left here is the
 * credential layer, which is genuinely independent of it:
 *
 * - `attested` (hosted) — the instance already holds a platform identity, so
 *   there is nothing to paste and nothing stored here. A company that wants to
 *   use its OWN Composio account can still override.
 * - `company` (issue #586) — this company's own TinyHumans credential, set by
 *   its admin. Composio is brokered through it, so there is nothing to paste
 *   here either. The override still exists for a company that wants its own
 *   Composio account.
 * - `static` — a Composio token this company pasted, or a static instance key.
 * - `none` — no credential can be obtained, so there is nothing to authorize
 *   against and agents get no Composio tools.
 *
 * The pasted token is WRITE-ONLY: stored and never shown again. A set/clear
 * takes effect on the agents' next turn, no restart. Hidden entirely when the
 * feature is not in the build.
 */
export function ComposioSection({ client, company, canManage, onChanged }: Props) {
  const [load, setLoad] = useState<
    "loading" | "ready" | "unavailable" | "unconfigured" | "error"
  >("loading");
  const [status, setStatus] = useState<ComposioStatus | null>(null);
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState<"save" | "clear" | "route" | null>(null);
  // The route the picker shows, and the one actually stored. Kept apart so a
  // Save can tell "switch this company to BYOK" from "rotate the key it already
  // uses" — only the first needs the confirmation step below.
  const [mode, setMode] = useState<ComposioMode>("managed");
  const [persistedMode, setPersistedMode] = useState<ComposioMode>("managed");
  const [apiKey, setApiKey] = useState("");
  // The managed → BYOK confirmation. Not a modal: the warning belongs in the
  // same scroll context as the control that raised it, and what it warns about
  // — every provider connected through the managed route becoming invisible —
  // is not readable off a pair of tiles.
  const [confirmSwitch, setConfirmSwitch] = useState(false);
  // Only meaningful in the credentialled states, where the paste card is an
  // override rather than the way in.
  const [showOverride, setShowOverride] = useState(false);

  const requestGeneration = useRef(0);
  const modeButtons = useRef<(HTMLButtonElement | null)[]>([]);
  // Focus in and back out of the inline confirmation. It is `role="alertdialog"`
  // over a plain `<div>`, not a modal primitive with its own focus trap (see the
  // "Not a modal" comment above), so nothing does this for free: opening it
  // unmounts the "Save key" button that had focus, leaving focus on
  // `document.body` — invisible to a mouse user, but a screen reader or keyboard
  // user loses their place entirely.
  const confirmOpenerRef = useRef<HTMLElement | null>(null);
  const confirmPrimaryActionRef = useRef<HTMLButtonElement | null>(null);

  const refresh = useCallback(async () => {
    const generation = ++requestGeneration.current;
    try {
      const s = await getComposioStatus(client, company);
      if (generation !== requestGeneration.current) return;
      setStatus(s);
      setMode(formModeFor(s));
      setPersistedMode(modeOf(s));
      // Hide the whole section when the feature is not compiled into this build.
      setLoad(s.inBuild ? "ready" : "unavailable");
    } catch (err) {
      if (generation !== requestGeneration.current) return;
      // A 404 is a host with no Composio surface — hide it. Anything else is a
      // host that could not answer; keep the section rather than vanishing
      // (issue #1470).
      setLoad(classifyLoadFailure(err));
    }
  }, [client, company]);

  useEffect(() => {
    setStatus(null);
    setShowOverride(false);
    setApiKey("");
    setConfirmSwitch(false);
    setLoad("loading");
    void refresh();
  }, [refresh]);

  // Opening moves focus onto the confirmation's primary action; closing
  // returns it to whatever raised it — but only when focus is still exactly
  // where opening left it (`document.body`). A save that succeeded and moved
  // focus somewhere sensible on its own (the row that replaced this one) must
  // not be yanked back to a button that may no longer say what it said.
  useEffect(() => {
    if (confirmSwitch) {
      confirmPrimaryActionRef.current?.focus();
      return;
    }
    const opener = confirmOpenerRef.current;
    confirmOpenerRef.current = null;
    if (opener?.isConnected && document.activeElement === document.body) {
      opener.focus();
    }
  }, [confirmSwitch]);

  async function save() {
    if (!token.trim()) return;
    setBusy("save");
    try {
      const res = await setComposioToken(client, company, token.trim());
      setStatus(res.status);
      setToken("");
      toast.success(res.note);
      onChanged();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Could not save the token.");
    } finally {
      setBusy(null);
    }
  }

  async function clear() {
    setBusy("clear");
    try {
      const res = await setComposioToken(client, company, "");
      setStatus(res.status);
      setToken("");
      // Clearing an override falls back to whatever tier remains — the status
      // the host just returned says which, and the grid re-probes for itself.
      toast.success(res.note);
      onChanged();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Could not clear the token.");
    } finally {
      setBusy(null);
    }
  }

  /**
   * Store this company's own Composio API key and route it there.
   *
   * The key is sent once and never comes back — the field is blanked on success,
   * and what tells the operator it worked is the status the host returns, not
   * anything held here.
   */
  async function saveApiKey() {
    if (!apiKey.trim()) return;
    setBusy("route");
    try {
      const res = await setComposioApiKey(client, company, apiKey.trim());
      setStatus(res.status);
      setMode(formModeFor(res.status));
      setPersistedMode(modeOf(res.status));
      setApiKey("");
      setConfirmSwitch(false);
      toast.success(res.note);
      onChanged();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Could not save the Composio API key.");
    } finally {
      setBusy(null);
    }
  }

  /** Give the managed route back: clear the key, and the mode with it. */
  async function clearApiKey() {
    setBusy("route");
    try {
      const res = await setComposioApiKey(client, company, "");
      setStatus(res.status);
      setMode(formModeFor(res.status));
      setPersistedMode(modeOf(res.status));
      setApiKey("");
      setConfirmSwitch(false);
      toast.success(res.note);
      onChanged();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Could not clear the Composio API key.");
    } finally {
      setBusy(null);
    }
  }

  /**
   * A Save that would move this company off the managed route for the first
   * time. Gated on a confirmation because the consequence — the providers
   * connected through OpenHuman's Composio account are in *that* account and
   * vanish from the grid until they are connected again here — is not readable
   * off the tiles. Rotating a key already in use, and switching back, are not
   * gated: neither strands anything the operator cannot immediately undo.
   */
  function requestApiKeySave() {
    if (persistedMode === "managed") {
      confirmOpenerRef.current =
        document.activeElement instanceof HTMLElement ? document.activeElement : null;
      setConfirmSwitch(true);
      return;
    }
    void saveApiKey();
  }

  /**
   * Arrow keys move between the route tiles and select in the same step, the way
   * native radios behave.
   *
   * Without it every tile stays in the Tab order and no Arrow key moves between
   * them — a screen reader announces radiogroup controls whose keyboard behavior
   * does not exist. Same shape `policy-settings` uses for its approval tiers,
   * which states the reasoning at length. Navigates from the tile that has
   * FOCUS: the keydown bubbles from the focused button to the container, so
   * `event.target` is that button. Wraps at both ends rather than dead-ending.
   */
  function handleModeKeyDown(event: React.KeyboardEvent<HTMLDivElement>) {
    if (busy !== null) return;
    if (!["ArrowDown", "ArrowRight", "ArrowUp", "ArrowLeft"].includes(event.key)) return;
    const step = event.key === "ArrowDown" || event.key === "ArrowRight" ? 1 : -1;
    const focused = modeButtons.current.indexOf(event.target as HTMLButtonElement);
    if (focused === -1) return;
    event.preventDefault();
    const next = (focused + step + MODE_ORDER.length) % MODE_ORDER.length;
    setMode(MODE_ORDER[next]);
    setConfirmSwitch(false);
    modeButtons.current[next]?.focus();
  }

  if (load === "unavailable") return null;

  const attested = status?.credentialSource === "attested";
  // The company's own key already authorizes Composio (issue #586), so this
  // reads like `attested` everywhere the question is "is there anything to
  // paste?" — the difference is whose identity it is, which the copy states.
  const companyKey = status?.credentialSource === "company";
  const byoToken = status?.credentialSource === "static";
  // In the attested state the paste card is a deliberate override; everywhere
  // else it is the only way to connect, so it is always on screen.
  // Issue #403: the credential card is an admin's. A member still sees the
  // status line above ("token set" / "linked via cluster identity"), which is
  // what tells them why their agents can reach Gmail; what they do not get is a
  // field that invites them to paste a credential the host will refuse.
  // A company already brokered through its own key is in the same position as an
  // attested one: the paste card is a deliberate override, not the way in.
  const credentialed = attested || companyKey;
  // Which route this company is actually on, as opposed to what the picker shows.
  const onByok = persistedMode === "byok";
  // Everything below is about the managed route's credential tiers, and under
  // BYOK none of them is in play — the company's own Composio key is the whole
  // credential. Rendering the token card there would offer a control that
  // changes nothing about the calls being made. See `showManagedTokenCard`'s
  // own doc for why this needs both the selected tile AND the persisted route
  // to agree, not either alone.
  const showTokenCard = showManagedTokenCard({
    mode,
    onByok,
    canManage,
    credentialed,
    showOverride,
    byoToken,
  });
  // The composio-grant tri-state, narrowed the same way `ProvidersSection` does
  // (issue #1478): `undefined` reads as "unknown", never as "not granted", so
  // this badge and the grid a few inches below it cannot disagree on the same
  // field. `status` is non-null wherever this is read below.
  const grant = grantStanding(status?.granted);

  return (
    <section className="space-y-3">
      <div className="flex items-center gap-2">
        <Plug className="size-4 text-muted-foreground" />
        <h2 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          Composio credential
        </h2>
      </div>
      <p className="text-sm text-muted-foreground">
        {!canManage
          ? "Your teammates reach Gmail, Slack & GitHub through Composio. Which account they act through belongs to the company, so an admin manages it — this is what is wired today."
          : onByok
            ? "Your teammates reach providers through this company's own Composio account. Calls go straight to Composio with the API key stored here — nothing is proxied and nothing is billed elsewhere. Connect providers in the grid below; they are connected in that account."
            : // Two facts, and they had been collapsed into one sentence. The
              // route a company is on today is not what the form beneath does,
              // and while the form is fixed to this company's own account those
              // two can disagree — an operator was told there was nothing to
              // paste directly above a field asking them to paste it.
              //
              // So: name what is wired now, then say what saving would change.
              // A company already reaching providers is switching accounts, not
              // filling a blank, and the grid it is looking at belongs to the
              // account it is leaving.
              attested
              ? "Your teammates reach providers through Composio today, linked through this instance's own cluster identity — nothing is stored here for that. Saving an API key below moves this company onto its own Composio account instead: the providers connected now live in the account it is leaving, so the grid will look empty until they are connected again here."
              : companyKey
                ? "Your teammates reach providers through Composio today, on the account this company's stored credential authorizes. Saving an API key below moves it onto its own Composio account instead: the providers connected now live in the account it is leaving, so the grid will look empty until they are connected again here."
                : "Your teammates reach providers through Composio. Nothing is wired yet — paste this company's own Composio API key below, and it will act through its own Composio account."}
      </p>

      {load === "loading" ? (
        <Skeleton className="h-32 rounded-xl" />
      ) : load === "error" ? (
        <SectionUnreachable label="Couldn't read this company's Composio credential" />
      ) : (
        <>
          {status && (
            <div className="flex flex-wrap items-center gap-2">
              <Badge variant={grant === "granted" ? "secondary" : "outline"}>
                {grant === "granted"
                  ? "granted"
                  : grant === "not-granted"
                    ? "not granted"
                    : "grant unknown"}
              </Badge>
              {onByok ? (
                // BYOK with no key stored is a real state the resolver handles —
                // it withholds the tools rather than borrowing the platform's
                // identity — so the badge has to tell the two apart. Reporting
                // "using its own account" for a company whose agents have no
                // Composio at all is the confident-wrong-answer shape #886 was
                // filed about, one route over.
                status.credentialSource === "none" ? (
                  <span className="inline-flex items-center gap-1 text-xs text-status-blocked-text">
                    <AlertTriangle className="size-3" /> No API key — agents get no Composio tools
                  </span>
                ) : (
                  <span className="inline-flex items-center gap-1 text-xs text-status-done-text">
                    <KeyRound className="size-3" /> Using this company&apos;s own Composio account
                  </span>
                )
              ) : attested ? (
                <span className="inline-flex items-center gap-1 text-xs text-status-done-text">
                  <ShieldCheck className="size-3" /> Linked via cluster identity — nothing stored
                </span>
              ) : companyKey ? (
                <span className="inline-flex items-center gap-1 text-xs text-status-done-text">
                  <ShieldCheck className="size-3" /> Linked via this company&apos;s own credential
                </span>
              ) : byoToken ? (
                <span className="inline-flex items-center gap-1 text-xs text-status-done-text">
                  <Check className="size-3" /> token set
                </span>
              ) : (
                <span className="text-xs text-muted-foreground">not connected</span>
              )}
            </div>
          )}

          {/* Fires only on an explicit not-granted, never on an unchecked grant
              (issue #1478): telling an operator to widen a grant that may
              already be set, off a field that was never read, is the same false
              confidence the badge above used to show. */}
          {grant === "not-granted" && (
            <GrantNamespace
              client={client}
              company={company}
              namespace="composio"
              explanation="Teammates will not receive Composio tools even once connected."
              canManage={canManage}
              onGranted={async () => {
                await refresh();
                onChanged();
              }}
              testId="composio-not-granted"
            />
          )}

          {canManage && (
            <Card>
              <CardContent className="space-y-4">
                {/* Two tiles, not a dropdown. The choice is binary, consequential,
                    and both sides need a sentence — a select collapses the option
                    NOT currently chosen to nothing, which is exactly the half an
                    operator is trying to evaluate. Same radiogroup shape
                    `policy-settings` uses for approval tiers, roving tabindex
                    included. */}
                {MODE_ORDER.length > 1 && (
                <div
                  role="radiogroup"
                  aria-label="Which Composio account this company uses"
                  className={cn("grid gap-2", MODE_ORDER.length > 1 && "sm:grid-cols-2")}
                  onKeyDown={handleModeKeyDown}
                >
                  {/* A stored route the list does not offer still gets a tile.
                      Without one no radio in the group is checked — every tile
                      reports `aria-checked="false"` and the control claims the
                      company has chosen nothing, which is a different (and
                      wrong) statement from "it is on a route not offered here".

                      Disabled: it is the state the company is in, not a route to
                      go back to. */}
                  {!isOffered(mode) && (
                    <button
                      type="button"
                      role="radio"
                      aria-checked
                      tabIndex={0}
                      disabled
                      data-testid="composio-mode-unconfigured"
                      className="rounded-md border border-primary bg-primary/5 p-3 text-left disabled:cursor-not-allowed"
                    >
                      <span className="text-sm font-medium">{NOT_CONFIGURED}</span>
                      <p className="mt-1 text-xs text-muted-foreground">
                        No Composio account is configured for this company. Add an API key below to
                        connect one.
                      </p>
                    </button>
                  )}
                  {MODE_ORDER.map((m, index) => {
                    const active = mode === m;
                    return (
                      <button
                        key={m}
                        ref={(el) => {
                          modeButtons.current[index] = el;
                        }}
                        type="button"
                        role="radio"
                        aria-checked={active}
                        tabIndex={active ? 0 : -1}
                        disabled={busy !== null}
                        data-testid={`composio-mode-${m}`}
                        onClick={() => {
                          setMode(m);
                          setConfirmSwitch(false);
                        }}
                        className={cn(
                          "rounded-md border p-3 text-left transition-colors",
                          "disabled:cursor-not-allowed disabled:opacity-60",
                          active ? "border-primary bg-primary/5" : "hover:bg-muted/50",
                        )}
                      >
                        <div className="flex items-start gap-2">
                          <span className="flex-1 text-sm font-medium">{MODES[m].label}</span>
                          {persistedMode === m && (
                            <Badge variant="secondary" className="text-xs">
                              Current
                            </Badge>
                          )}
                        </div>
                        <p className="mt-1 text-xs text-muted-foreground">{MODES[m].blurb}</p>
                        <p className="mt-2 inline-flex items-center gap-1 text-xs text-muted-foreground">
                          <Wallet className="size-3 shrink-0" />
                          {MODES[m].billed}
                        </p>
                      </button>
                    );
                  })}
                </div>
                )}

                {/* The endpoint, so the routing claim above is checkable rather
                    than merely asserted — but only when the managed token card
                    below is not already printing it. Two copies of one URL on
                    one screen reads as two different facts. */}
                {status && !showTokenCard && isOffered(persistedMode) && (
                  <p className="truncate font-mono text-xs text-muted-foreground">
                    {status.backendUrl}
                  </p>
                )}

                {mode === "byok" && (
                  <div className="space-y-1.5">
                    <Label htmlFor="composio-api-key" className="text-xs">
                      Composio API key
                    </Label>
                    <Input
                      id="composio-api-key"
                      type="password"
                      autoComplete="off"
                      placeholder={onByok ? "stored — paste a new key to rotate" : "ak_…"}
                      value={apiKey}
                      onChange={(e) => setApiKey(e.target.value)}
                    />
                    <p className="text-xs text-muted-foreground">
                      From your Composio dashboard at app.composio.dev. Stored on this host, never
                      shown again.
                    </p>
                  </div>
                )}

                {/* Said before the switch, not after: what it costs is not
                    readable off the tiles. */}
                {confirmSwitch && (
                  <div
                    role="alertdialog"
                    aria-labelledby="composio-switch-warning"
                    className="space-y-3 rounded-md border border-status-blocked/40 bg-status-blocked-soft p-3"
                  >
                    <p
                      id="composio-switch-warning"
                      className="inline-flex items-center gap-2 text-xs font-medium"
                    >
                      <AlertTriangle className="size-3.5 shrink-0" />
                      Providers connected before this stay where they are
                    </p>
                    <p className="text-xs text-muted-foreground">
                      They live in the Composio account this company reached before, not in this
                      one, so the grid below will look empty until you connect them again here.
                      Clearing the key puts this company back where it is now.
                    </p>
                    <div className="flex flex-wrap gap-2">
                      <Button
                        ref={confirmPrimaryActionRef}
                        size="sm"
                        disabled={busy !== null}
                        onClick={() => void saveApiKey()}
                      >
                        {busy === "route" ? (
                          <Loader2 className="size-4 animate-spin" />
                        ) : (
                          <Save className="size-4" />
                        )}
                        Use this company&apos;s account
                      </Button>
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={busy !== null}
                        onClick={() => setConfirmSwitch(false)}
                      >
                        Cancel
                      </Button>
                    </div>
                  </div>
                )}

                {/* Nothing to act on for a managed company that has stored no
                    key: no Save (there is nothing to save) and no Clear (there
                    is nothing to clear). Rendering the row anyway left a band of
                    empty padding under the tiles that read as a missing
                    control. */}
                {!confirmSwitch && (mode === "byok" || onByok) && (
                  <div className="flex flex-wrap items-center gap-2">
                    {mode === "byok" ? (
                      <>
                        <Button
                          disabled={busy !== null || !apiKey.trim()}
                          onClick={requestApiKeySave}
                        >
                          {busy === "route" ? (
                            <Loader2 className="size-4 animate-spin" />
                          ) : (
                            <Save className="size-4" />
                          )}
                          {onByok ? "Rotate key" : "Save key"}
                        </Button>
                        {/* No Clear control here, deliberately.
                            Clearing is not "the key goes away": the host derives
                            the route from whether a key exists, so
                            `store_api_key("")` writes the mode back to the one
                            this console no longer offers, and a company with any
                            credential on that route resumes acting through it —
                            a different account, billed differently.
                            Reaching that used to require picking the other tile,
                            which named it. With no tile to pick, a button here
                            could only either say what it does — naming the route
                            — or not say it, which is the switch happening
                            silently. The status read carries no signal for
                            whether that route has a credential, so the control
                            cannot be offered only where clearing is inert
                            either. Rotating a key stays; removing one needs a
                            host that can express "no key, no route". */}
                      </>
                    ) : (
                      onByok && (
                        <Button disabled={busy !== null} onClick={() => void clearApiKey()}>
                          {busy === "route" ? (
                            <Loader2 className="size-4 animate-spin" />
                          ) : (
                            <Trash2 className="size-4" />
                          )}
                          {COMPOSIO_MANAGED_HIDDEN
                            ? "Clear key"
                            : "Clear key & use OpenHuman-managed"}
                        </Button>
                      )
                    )}
                  </div>
                )}
              </CardContent>
            </Card>
          )}

          {/* Gated on `credentialed`, not `attested`: a company brokered through
              its own TinyHumans key is equally "already credentialled", so it
              equally needs a way back to the BYO card. Gating this on `attested`
              alone left a company-key admin with the paste card hidden and no
              control to reveal it — the override became unreachable. */}
          {canManage && !onByok && credentialed && !showTokenCard && (
            <Button variant="outline" size="sm" onClick={() => setShowOverride(true)}>
              <KeyRound className="size-4" />
              Use your own Composio account instead
            </Button>
          )}

          {showTokenCard && (
            <Card>
              <CardContent className="space-y-4">
                {/* The explainer has to name what the token would displace,
                    and that differs by tier: an attested company falls back to
                    the pod's cluster identity, a company-key one falls back to
                    its own credential. Saying "cluster identity" to the latter
                    would describe a fallback it does not have. */}
                {companyKey ? (
                  <p className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
                    Optional. This company&apos;s own TinyHumans credential already authorizes
                    Composio. A token set here replaces it for Composio only — use it when the
                    company has a separate Composio account. Clear it to go back to the company
                    credential.
                  </p>
                ) : attested ? (
                  <p className="rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
                    Optional. A token set here overrides the instance identity for this company only
                    — use it when the company has its own Composio account. Clear it to go back to
                    the cluster identity.
                  </p>
                ) : null}
                <div className="space-y-1">
                  <Label htmlFor="composio-token" className="text-xs">
                    Composio token {byoToken ? "— set (paste a new value to rotate)" : ""}
                  </Label>
                  <Input
                    id="composio-token"
                    type="password"
                    autoComplete="off"
                    placeholder="paste the company's Composio OAuth token"
                    value={token}
                    onChange={(e) => setToken(e.target.value)}
                  />
                  {status && isOffered(persistedMode) && (
                    <p className="truncate text-xs text-muted-foreground">{status.backendUrl}</p>
                  )}
                </div>

                <div className="flex items-center gap-2">
                  <Button disabled={busy !== null || !token.trim()} onClick={() => void save()}>
                    {busy === "save" ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <Save className="size-4" />
                    )}
                    Save token
                  </Button>
                  {byoToken && (
                    <Button variant="outline" disabled={busy !== null} onClick={() => void clear()}>
                      {busy === "clear" ? (
                        <Loader2 className="size-4 animate-spin" />
                      ) : (
                        <Trash2 className="size-4" />
                      )}
                      Clear
                    </Button>
                  )}
                  <KeyRound className="ml-auto size-4 text-muted-foreground" />
                </div>
              </CardContent>
            </Card>
          )}
        </>
      )}
    </section>
  );
}
