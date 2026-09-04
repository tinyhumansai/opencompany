import { useEffect, useState } from "react";
import {
  Compass,
  Flag,
  Globe,
  Pause,
  Play,
  Power,
  RotateCcw,
  TriangleAlert,
  Archive as ArchiveIcon,
} from "lucide-react";
import { toast } from "sonner";

import { me as fetchMe, type UserRole } from "@/api/auth";
import type { LifecycleAction, OpenCompanyClient } from "@/api/client";
import { memoryEngine, type MemoryEngineState } from "@/api/memory";
import { ApiError } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription } from "@/components/ui/alert";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { DomainSettings } from "@/components/domain-settings";
import { ExternalHarnesses } from "@/components/external-harnesses";
import { PolicySettings } from "@/components/policy-settings";
import { StatusPill } from "@/components/status-pill";
import { ThemeToggle } from "@/components/theme-toggle";
import type { CompanyFeed } from "@/hooks/use-company";
import { withHostParam } from "@/hooks/use-host-route";
import { restartTour } from "@/tour/state";
import { preloadTour } from "@/tour/TourController";
import { useLocalScope } from "@/connections/ConnectionContext";
import { lifecycleAffordances } from "@/lib/lifecycle-controls";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  feed: CompanyFeed;
  onFlag: () => void;
  /** Start the reset (archive + start clean) flow for the active company (#1807). */
  onResetCompany?: (id: string, name: string) => void;
}

// These are the optional capability families closest to the mandatory core /
// recall / portability path. Remote providers commonly omit them, so merely
// listing what answered leaves an operator to infer a material limitation.
const MANDATORY_ADJACENT_MEMORY_FAMILIES = [
  "tree",
  "entities",
  "graph",
  "diff",
  "goals",
  "tool_memory",
];

/** Connection details, lifecycle controls, and the feedback entry point. */
export function SettingsView({ client, company, feed, onFlag, onResetCompany }: Props) {
  // Which (connection, company) this subtree's browser-local state belongs to.
  const scope = useLocalScope();
  const { status } = feed;
  const scoped = company ?? client.defaultCompany;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {/*
        This page used to hide its own title above `lg` (issue #1221), on the
        reasoning that the sub-nav rail beside it already says "Settings".

        Issue #1763 makes it visible at every width, because that reasoning
        stopped being true of only this page: Brain, Skills, People, Hosting,
        Search, OAuth, MCP, Inference and Usage all sit beside the same rail and
        all show a title. General was the one settings page that did not, so the
        rail argument had become an argument for an exception rather than for a
        rule — and the rail says "Settings", which is the section, while this
        says "General settings", which is the page.
      */}
      <PageHeader title="General settings" width="3xl" />
      <div className="mx-auto min-h-0 w-full max-w-3xl flex-1 space-y-6 overflow-y-auto px-4 py-6">
        {/* Device pairing was here. Sessions are the frontend client's own
            business now — the desktop app holds its session the same way the
            browser does — so there is no machine for this page to pair. */}

        {/* Every coding engine a teammate can be bound to, joined against
            whether it can actually run on this machine (issue #1245). Works in
            a browser; the installed-here half only fills in on the desktop. */}
        <ExternalHarnesses client={client} company={company} />

        {/* Approvals: the autonomy tier and the always-ask list (issue #562).
            High in the page on purpose — an operator who comes to settings
            because they are drowning in approval cards is here for this. */}
        <PolicySettings client={client} company={company} />

        {/* Connection */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Connection</CardTitle>
            <CardDescription>Where this console is pointed.</CardDescription>
          </CardHeader>
          <CardContent className="space-y-0 divide-y">
            <InfoRow label="Host">
              <span className="inline-flex items-center gap-1.5 font-mono text-xs">
                <Globe className="size-3.5 text-muted-foreground" />
                {client.baseUrl || "same origin"}
              </span>
            </InfoRow>
            <InfoRow label="Company">
              <span className="font-mono text-xs">{status.id}</span>
            </InfoRow>
            {status.template_provenance ? (
              <InfoRow label="Template">
                <span className="text-sm">
                  <span className="font-mono text-xs">
                    {status.template_provenance.source_id}
                  </span>
                  {status.template_provenance.version
                    ? ` (${status.template_provenance.version})`
                    : ""}
                </span>
              </InfoRow>
            ) : null}
            <InfoRow label="Mode">
              <span className="text-sm">
                {client.isSingleCompany ? "Single-company" : "Multi-company (platform)"}
              </span>
            </InfoRow>
            <InfoRow label="Current state">
              <StatusPill lifecycle={status.lifecycle} />
            </InfoRow>
          </CardContent>
        </Card>

        <MemoryEngineCard client={client} company={company} />

        {/* Lifecycle */}
        {scoped ? (
          <LifecycleControls
            client={client}
            company={scoped}
            feed={feed}
            onReset={
              onResetCompany
                ? () => onResetCompany(scoped, feed.status.name)
                : undefined
            }
          />
        ) : (
          <Card>
            <CardHeader>
              <CardTitle className="text-base">Lifecycle</CardTitle>
              <CardDescription>
                Pause, resume, or retire the company. Available on platform hosts with a company id.
              </CardDescription>
            </CardHeader>
          </Card>
        )}

        {/* Domain & email.

            `key` is load-bearing, not cosmetic: this card holds a
            typed-but-unsaved SMTP password in React state, and without a
            remount a company switch would carry a credential typed for one
            company into another company's Save. `SettingsSection` remounts
            `BillingView`/`HostingView` for exactly this reason. */}
        <DomainSettings key={company ?? "self"} client={client} company={company} />

        {/* Appearance.

            The trailing control goes in `CardAction`, not a bare child:
            `CardHeader` is a grid, so `flex-row justify-between` on it is inert
            and the control drops onto a row of its own below the description.
            `CardAction` is what switches the header to `grid-cols-[1fr_auto]`
            and parks the control at the top right. */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Appearance</CardTitle>
            <CardDescription>Switch between light, dark, and system themes.</CardDescription>
            <CardAction>
              <ThemeToggle />
            </CardAction>
          </CardHeader>
        </Card>

        {/* Product tour */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Product tour</CardTitle>
            <CardDescription>Replay the guided walkthrough or set up your company again.</CardDescription>
            <CardAction>
              <div className="flex flex-wrap gap-2">
                <Button
                  variant="outline"
                  // The tour's code is lazily loaded, so without this the download
                  // starts on the click and the button appears to do nothing until
                  // it lands. Pointing at it is intent enough to fetch.
                  onPointerEnter={preloadTour}
                  onFocus={preloadTour}
                  onClick={() => {
                    restartTour(scope);
                    toast.success("Starting the product tour.");
                  }}
                >
                  <Compass className="size-4" /> Replay tour
                </Button>
                {/* The route is `#/setup`, but the anchor has to say so with the
                    host scope carried: a Ctrl/Cmd-click opens a new tab, and a
                    tab has no `useHostAddress` repairer — it boots with the
                    address as written. Without the param the new tab would pick
                    its bootstrap/default host and setup could staff the wrong
                    company in a multi-host console (issue #1417 review). */}
                <Button variant="outline" render={<a href={withHostParam("setup")} />}>
                  Set up company
                </Button>
              </div>
            </CardAction>
          </CardHeader>
        </Card>

        {/* Feedback */}
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Something off?</CardTitle>
            <CardDescription>
              Flag a wrong result or a missing capability. You&apos;ll preview exactly what gets
              shared first.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <Button variant="outline" onClick={onFlag}>
              <Flag className="size-4" /> Flag something
            </Button>
          </CardContent>
        </Card>
      </div>
    </div>
  );
}

/**
 * Exported (rather than kept view-local) so the Reset / Start clean button's
 * gating and click wiring can be rendered and asserted on directly, without
 * pulling in every other card `SettingsView` composes
 * (`settings-lifecycle-reset-button.test.ts`).
 */
export function LifecycleControls({
  client,
  company,
  feed,
  onReset,
}: {
  client: OpenCompanyClient;
  company: string;
  feed: CompanyFeed;
  /** Open the reset (archive + start clean) flow for this company (#1807). */
  onReset?: () => void;
}) {
  const [busy, setBusy] = useState(false);
  /**
   * The state this control just asked for, shown until the feed confirms it.
   *
   * Without this the buttons lag the click by two round trips: the action
   * itself, and then the `refresh` that re-reads the company. `busy` cleared as
   * soon as the action returned, so in between the operator saw re-enabled
   * buttons still offering "Pause" on a company they had just paused — which
   * reads as a click that did not register, and invites a second one.
   *
   * Cleared in `finally` rather than on success, so a *failed* action reverts
   * to whatever the feed says instead of leaving the console asserting a state
   * the host rejected.
   */
  const [pending, setPending] = useState<string | null>(null);
  const state = pending ?? feed.status.lifecycle;

  /**
   * The signed-in caller's role, or `null` when the console found no session.
   *
   * `null` is not "non-admin" — `resolve_principal` prefers a resolved
   * session over a platform bearer whenever both are present, so whether a
   * session exists at all changes which credential `pause` / `resume`
   * actually authorize against. Defaults to `null` so an unresolved read
   * never renders an enabled Pause/Resume, matching the closed-by-default
   * pattern every other admin-gated view uses (`HostingView`, `TeamView`, ...).
   */
  const [session, setSession] = useState<UserRole | null>(null);
  useEffect(() => {
    let live = true;
    void (async () => {
      let role: UserRole | null = null;
      try {
        role = (await fetchMe(client, company)).role;
      } catch {
        // No user plane on this host, or not signed in — no session.
      }
      if (live) setSession(role);
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  async function run(action: LifecycleAction) {
    if (busy) return;
    setBusy(true);
    // Before the await, so the buttons move on the click rather than on the
    // response. This is the whole point of tracking it separately.
    setPending(resultOf(action));
    try {
      await client.lifecycle(action, company);
      toast.success(`Company ${labelFor(action)}.`);
      // Awaited, unlike before: `pending` is dropped in `finally`, and dropping
      // it while the refresh was still in flight would flip the buttons back to
      // the stale lifecycle for exactly as long as that request took.
      await feed.refresh();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "something went wrong";
      toast.error(`Couldn't ${action} — ${msg}`);
    } finally {
      setPending(null);
      setBusy(false);
    }
  }

  const platform = client.carriesPlatformBearer;
  const { actions, explainPlatformOnly, explainPlatformSuspended, explainAdminOnly, archived } =
    lifecycleAffordances(state, session, platform);
  const offers = (action: LifecycleAction) => actions.includes(action);

  return (
    <Card>
      <CardHeader>
        {/* Four verbs, and the card used to name three of them. `Pause` and
          `Suspend` in particular were indistinguishable to a reader — the
          suspend dialog described what anyone would assume pause did. Only the
          controls this console can actually reach are named here. */}
        <CardTitle className="text-base">Lifecycle</CardTitle>
        <CardDescription>
          Pause stops this company taking new work; resume starts it again.
          {platform ? " Suspend and archive are the platform's own, heavier stops." : ""}
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {/* Said before the buttons, in the register Billing and Hosting use for
          the same problem: a control that cannot work here explains itself in
          the page rather than failing after the click. */}
        {explainPlatformOnly && (
          <Alert data-testid="lifecycle-platform-only">
            <TriangleAlert className="size-4" />
            <AlertDescription>
              Suspending and archiving a company are the hosting platform&rsquo;s to do, not this
              console&rsquo;s. They need a platform credential, which a person signed in here never
              holds — so the controls are left out rather than shown failing. Pause is the
              reversible stop that is yours.
            </AlertDescription>
          </Alert>
        )}
        {explainPlatformSuspended && (
          <Alert data-testid="lifecycle-platform-suspended">
            <TriangleAlert className="size-4" />
            <AlertDescription>
              The platform suspended this company. Only the platform can lift a suspension — an
              admin here cannot resume it, so there is no Resume button to offer.
            </AlertDescription>
          </Alert>
        )}
        {explainAdminOnly && (
          <Alert data-testid="lifecycle-admin-only">
            <TriangleAlert className="size-4" />
            <AlertDescription>
              Pausing and resuming a company need admin authority here — ask one of this
              company&rsquo;s admins, since a member&rsquo;s session cannot reach these controls.
            </AlertDescription>
          </Alert>
        )}
        <div className="flex flex-wrap gap-2">
          {offers("pause") && (
            <Button variant="outline" disabled={busy} onClick={() => void run("pause")}>
              <Pause className="size-4" /> Pause
            </Button>
          )}
          {offers("resume") && (
            <Button variant="outline" disabled={busy} onClick={() => void run("resume")}>
              <Play className="size-4" /> Resume
            </Button>
          )}
          {offers("suspend") && (
            <ConfirmAction
              trigger={
                <Button variant="outline" disabled={busy}>
                  <Power className="size-4" /> Suspend
                </Button>
              }
              title="Suspend this company?"
              // The old copy promised "until you resume it". The handler
              // refuses exactly that: a suspension is a platform-forced pause,
              // and neither an owner token nor the company's own admins may
              // lift it. Saying so is the difference between a heavy stop and
              // a trap.
              description="It stops handling work, and only the platform can start it again — the company's own admins cannot. For a stop you can undo, use Pause."
              confirmLabel="Suspend"
              onConfirm={() => void run("suspend")}
            />
          )}
          {offers("archive") && (
            <ConfirmAction
              trigger={
                <Button variant="destructive" disabled={busy}>
                  <ArchiveIcon className="size-4" /> Archive
                </Button>
              }
              title="Archive this company?"
              description="Archiving retires the company. This is meant to be permanent — you won't be able to operate it afterward."
              confirmLabel="Archive"
              destructive
              onConfirm={() => void run("archive")}
            />
          )}
          {/* Reset = archive this company (data retained, not deleted) and
              provision a fresh empty one in its place — the only truthful
              "start clean" the host offers, since there is no purge route.
              Platform-scoped like archive, so it rides the same `platform`
              gate and is left out entirely for a magic-link operator. */}
          {onReset && platform && !archived && (
            <Button variant="destructive" disabled={busy} onClick={onReset}>
              <RotateCcw className="size-4" /> Reset / Start clean
            </Button>
          )}
          {archived && (
            <p className="text-sm text-muted-foreground">This company is archived.</p>
          )}
        </div>
      </CardContent>
    </Card>
  );
}

function ConfirmAction({
  trigger,
  title,
  description,
  confirmLabel,
  destructive,
  onConfirm,
}: {
  trigger: React.ReactElement;
  title: string;
  description: string;
  confirmLabel: string;
  destructive?: boolean;
  onConfirm: () => void;
}) {
  return (
    <AlertDialog>
      <AlertDialogTrigger render={trigger} />
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>{description}</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction
            onClick={onConfirm}
            className={destructive ? "bg-destructive text-white hover:bg-destructive/90" : undefined}
          >
            {confirmLabel}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

/**
 * Read-only: which memory engine this instance is bound to, from the
 * `…/memory/engine` surface.
 *
 * Deliberately carries no setter. Engine selection is instance-wide and
 * belongs to the infra operator or the company configuration, depending on
 * the reported layer. A console admin can see the engine but never repoint a
 * deployment's storage from here. The switch runbook lives in
 * `docs/spec/runtime/memory-engine.md`. Renders nothing on the `store`
 * default and on a host predating the engine route.
 */
function MemoryEngineCard({
  client,
  company,
}: {
  client: OpenCompanyClient;
  company: string | null;
}) {
  const [engine, setEngine] = useState<MemoryEngineState | undefined>(undefined);
  useEffect(() => {
    let live = true;
    setEngine(undefined);
    memoryEngine(client, company)
      .then((state) => {
        if (live) setEngine(state);
      })
      .catch(() => {
        /* best-effort: the settings page works without the engine route */
      });
    return () => {
      live = false;
    };
  }, [client, company]);

  if (!engine || engine.active === "store") return null;
  const discarding = engine.active === "null";
  const unservedFamilies = MANDATORY_ADJACENT_MEMORY_FAMILIES.filter(
    (family) => !engine.capabilities.includes(family),
  );
  return (
    <Card data-testid="settings-memory-engine">
      <CardHeader>
        <CardTitle className="text-base">Memory engine</CardTitle>
        <CardDescription>
          {engine.editable ? (
            engine.layer === "config.toml" ? (
              <>Selected in the company configuration. You can change it here.</>
            ) : (
              <>Using the default engine. You can change it here.</>
            )
          ) : (
            <>
              Set by the infra operator (<code className="text-xs">OPENCOMPANY_MEMORY*</code>, read
              at boot). Instance-wide; read-only here by design.
            </>
          )}
          {discarding &&
            " This engine accepts and discards every write — nothing this company is told will be remembered."}
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-0 divide-y">
        <InfoRow label="Engine">
          <span className="font-mono text-xs">{engine.active}</span>
        </InfoRow>
        <InfoRow label="Layer">
          <span className="font-mono text-xs">{engine.layer}</span>
        </InfoRow>
        <InfoRow label="Capabilities">
          <span className="text-sm">
            {engine.capabilities.length > 0
              ? engine.capabilities.join(", ")
              : "not negotiated"}
          </span>
        </InfoRow>
        <InfoRow label="Not served">
          <span className="text-sm">
            {unservedFamilies.length > 0 ? unservedFamilies.join(", ") : "none in this set"}
          </span>
        </InfoRow>
        <InfoRow label="Boot probe">
          <span className="text-sm">
            {engine.healthy === true
              ? "reachable"
              : engine.healthy === false
                ? "unreachable — check the endpoint and credential"
                : "not probed"}
          </span>
        </InfoRow>
        {/*
          Distinct from "Not served" above, which is derived client-side from
          what the driver *claims*. This is what the engine actually answered
          when read at boot: a family can be advertised, pass the bind-time
          audit, and still return nothing.
        */}
        <InfoRow label="Refused at probe">
          <span className="text-sm">
            {engine.unreachableFamilies === undefined
              ? "not probed"
              : engine.unreachableFamilies.length === 0
                ? // Naming what was probed matters: portability is mandatory and
                  // deliberately never probed, so a bare "none" would imply more
                  // coverage than there is.
                  "none — core and recall both answered (portability is not probed)"
                : `${engine.unreachableFamilies.join(", ")} — reads against these will fail`}
          </span>
        </InfoRow>
        {engine.slowFamilies !== undefined && engine.slowFamilies.length > 0 && (
          <InfoRow label="Slow at probe">
            <span className="text-sm">
              {`${engine.slowFamilies.join(", ")} — did not answer in time; the engine may just be loaded`}
            </span>
          </InfoRow>
        )}
      </CardContent>
    </Card>
  );
}

function InfoRow({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 py-3 first:pt-0 last:pb-0">
      <span className="text-sm text-muted-foreground">{label}</span>
      {children}
    </div>
  );
}

/**
 * The lifecycle an action lands the company in.
 *
 * Separate from {@link labelFor} even though three of the four strings match:
 * that one writes a sentence to a person ("Company resumed.") and this one has
 * to equal what the host reports in `status.lifecycle`, which is why `resume`
 * differs. Sharing one function would make the two meanings drift into each
 * other the first time either wording changed.
 */
function resultOf(action: LifecycleAction): string {
  switch (action) {
    case "pause":
      return "paused";
    case "resume":
      return "running";
    case "suspend":
      return "suspended";
    case "archive":
      return "archived";
  }
}

function labelFor(action: LifecycleAction): string {
  switch (action) {
    case "pause":
      return "paused";
    case "resume":
      return "resumed";
    case "suspend":
      return "suspended";
    case "archive":
      return "archived";
  }
}
