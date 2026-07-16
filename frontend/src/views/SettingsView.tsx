import { useState } from "react";
import { Flag, Globe, Pause, Play, Power, Archive as ArchiveIcon } from "lucide-react";
import { toast } from "sonner";

import type { LifecycleAction, OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
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
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { DomainSettings } from "@/components/domain-settings";
import { StatusPill } from "@/components/status-pill";
import { ThemeToggle } from "@/components/theme-toggle";
import type { CompanyFeed } from "@/hooks/use-company";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  feed: CompanyFeed;
  onFlag: () => void;
}

/** Connection details, lifecycle controls, and the feedback entry point. */
export function SettingsView({ client, company, feed, onFlag }: Props) {
  const { status } = feed;
  const scoped = company ?? client.defaultCompany;

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl space-y-6 px-4 py-6">
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
        <DomainSettings company={company} />

        {/* Appearance */}
        <Card>
          <CardHeader className="flex-row items-center justify-between space-y-0">
            <div className="space-y-1">
              <CardTitle className="text-base">Appearance</CardTitle>
              <CardDescription>Switch between light, dark, and system themes.</CardDescription>
            </div>
            <ThemeToggle />
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
  const state = feed.status.lifecycle;
  const archived = state === "archived";
  const running = state === "running";
  const paused = state === "paused";

  async function run(action: LifecycleAction) {
    if (busy) return;
    setBusy(true);
    try {
      await client.lifecycle(action, company);
      toast.success(`Company ${labelFor(action)}.`);
      void feed.refresh();
    } catch (err) {
      const msg = err instanceof ApiError ? err.message : "something went wrong";
      toast.error(`Couldn't ${action} — ${msg}`);
    } finally {
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

function InfoRow({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-4 py-3 first:pt-0 last:pb-0">
      <span className="text-sm text-muted-foreground">{label}</span>
      {children}
    </div>
  );
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
