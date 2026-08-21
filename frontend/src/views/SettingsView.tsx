import { useEffect, useState } from "react";
import { Compass, Flag, Globe, Pause, Play, Power, Archive as ArchiveIcon } from "lucide-react";
import { toast } from "sonner";

import type { LifecycleAction, OpenCompanyClient } from "@/api/client";
import { ApiError, type MemorySpec } from "@/api/types";
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
import { DevicePairing } from "@/components/device-pairing";
import { DomainSettings } from "@/components/domain-settings";
import { PolicySettings } from "@/components/policy-settings";
import { StatusPill } from "@/components/status-pill";
import { ThemeToggle } from "@/components/theme-toggle";
import type { CompanyFeed } from "@/hooks/use-company";
import { restartTour } from "@/tour/state";
import { preloadTour } from "@/tour/TourController";
import { useLocalScope } from "@/connections/ConnectionContext";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  feed: CompanyFeed;
  onFlag: () => void;
}

/** Connection details, lifecycle controls, and the feedback entry point. */
export function SettingsView({ client, company, feed, onFlag }: Props) {
  // Which (connection, company) this subtree's browser-local state belongs to.
  const scope = useLocalScope();
  const { status } = feed;
  const scoped = company ?? client.defaultCompany;

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl space-y-6 px-4 py-6">
        {/* This sub-page draws no visible title of its own — the sub-nav rail
            beside it already says "Settings" (issue #1221). */}
        <h1 className="sr-only">General settings</h1>
        {/* Pairing this machine. Renders nothing in a browser, where the
            session cookie already works. */}
        <DevicePairing />

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

        <MemoryEngineCard client={client} />

        {/* Lifecycle */}
        {scoped ? (
          <LifecycleControls client={client} company={scoped} feed={feed} />
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

        {/* Domain & email */}
        <DomainSettings client={client} company={company} />

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
            <CardDescription>Replay the guided walkthrough of the console.</CardDescription>
            <CardAction>
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

function LifecycleControls({
  client,
  company,
  feed,
}: {
  client: OpenCompanyClient;
  company: string;
  feed: CompanyFeed;
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
  const archived = state === "archived";
  const running = state === "running";
  const paused = state === "paused";

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

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Lifecycle</CardTitle>
        <CardDescription>Pause, resume, or retire this company.</CardDescription>
      </CardHeader>
      <CardContent className="flex flex-wrap gap-2">
        {running && (
          <Button variant="outline" disabled={busy} onClick={() => void run("pause")}>
            <Pause className="size-4" /> Pause
          </Button>
        )}
        {(paused || state === "suspended") && !archived && (
          <Button variant="outline" disabled={busy} onClick={() => void run("resume")}>
            <Play className="size-4" /> Resume
          </Button>
        )}
        {!archived && (
          <ConfirmAction
            trigger={
              <Button variant="outline" disabled={busy}>
                <Power className="size-4" /> Suspend
              </Button>
            }
            title="Suspend this company?"
            description="It will stop handling work until you resume it. In-flight tasks are paused, not lost."
            confirmLabel="Suspend"
            onConfirm={() => void run("suspend")}
          />
        )}
        {!archived && (
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
        {archived && (
          <p className="text-sm text-muted-foreground">This company is archived.</p>
        )}
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
 * Read-only: which memory engine this instance is bound to, from `/spec`.
 *
 * Deliberately carries no setter. Engine selection is instance-wide and
 * belongs to the infra operator — the `OPENCOMPANY_MEMORY*` variables, read
 * once at boot — so a console admin can see the engine but never repoint a
 * deployment's storage from here. The switch runbook lives in
 * `docs/spec/runtime/memory-engine.md`. Renders nothing on the `store`
 * default and on a host predating the `/spec` memory field.
 */
function MemoryEngineCard({ client }: { client: OpenCompanyClient }) {
  const [engine, setEngine] = useState<MemorySpec | undefined>(undefined);
  useEffect(() => {
    let live = true;
    setEngine(undefined);
    client
      .spec()
      .then((spec) => {
        if (live) setEngine(spec.memory);
      })
      .catch(() => {
        /* best-effort: the settings page works without /spec */
      });
    return () => {
      live = false;
    };
  }, [client]);

  if (!engine || engine.backend === "store") return null;
  const discarding = engine.backend === "null";
  return (
    <Card data-testid="settings-memory-engine">
      <CardHeader>
        <CardTitle className="text-base">Memory engine</CardTitle>
        <CardDescription>
          Set by the infra operator (<code className="text-xs">OPENCOMPANY_MEMORY*</code>, read
          at boot). Instance-wide; read-only here by design.
          {discarding &&
            " This engine accepts and discards every write — nothing this company is told will be remembered."}
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-0 divide-y">
        <InfoRow label="Engine">
          <span className="font-mono text-xs">{engine.driver_id ?? engine.backend}</span>
        </InfoRow>
        <InfoRow label="Mode">
          <span className="font-mono text-xs">{engine.backend}</span>
        </InfoRow>
        <InfoRow label="Capabilities">
          <span className="text-sm">
            {engine.capabilities.length > 0
              ? engine.capabilities.join(", ")
              : "not negotiated"}
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
