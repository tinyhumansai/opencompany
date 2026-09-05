import { useCallback, useEffect, useState } from "react";
import { Check, Loader2, TriangleAlert } from "lucide-react";
import { toast } from "sonner";

import {
  clearHosting,
  getHosting,
  saveHosting,
  type HostingStatus,
} from "@/api/hosting";
import type { OpenCompanyClient } from "@/api/client";
import { AdminOnlyNotice } from "@/components/admin-only-notice";
import { PageHeader } from "@/components/page-header";
import { useCanManage } from "@/hooks/use-can-manage";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { GrantNamespace } from "@/components/grant-namespace";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
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
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/** The message out of a rejected request, whatever it was rejected with. */
function reason(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/**
 * Settings → Hosting: the company's hosting provider connection.
 *
 * With a key stored here and the `hosting` grant in the manifest, a teammate can
 * put a site in this company's workspace on the public internet — with a managed
 * database wired into it, environment variables set before the build, and a
 * custom domain attached.
 *
 * # Why three states and not a "Connected ✓" badge
 *
 * Three separate things can each be missing, and two of them are invisible from
 * this form's own fields:
 *
 *   - no key       — teammates have no hosting tools at all
 *   - not granted  — the key is stored and STILL nothing reaches a teammate,
 *                    because the manifest does not grant `hosting`. The fix is
 *                    `company.toml`, not this page.
 *   - not in build — the running host was compiled without the harness, so no
 *                    amount of configuring will do anything.
 *
 * A single "Connected" badge would be green for the last two and send an
 * operator hunting through this form for a problem that is not in it.
 *
 * # The key is write-only
 *
 * It is never returned by the host, so it is never rendered. A stored key shows
 * as "Configured" and the input stays empty with a placeholder saying that
 * typing replaces it — an input pre-filled with dots invites an operator to
 * "correct" a value they cannot see, and submitting the dots would store the
 * dots.
 */
export function HostingView({ client, company }: Props) {
  const [status, setStatus] = useState<HostingStatus | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Whether this viewer may change what this company deploys through.
  //
  // Governs the whole page, not just the grant control: `PUT …/hosting` and
  // `DELETE …/hosting/key` are both `AdminScopedCompany`, so for a member the
  // token field, Save and Disconnect could each only ever produce a refusal.
  const canManage = useCanManage(client, company);

  const [apiKey, setApiKey] = useState("");
  const [team, setTeam] = useState("");

  // Every piece of state here belongs to ONE company — including the
  // typed-but-unsaved API key. `SettingsSection` renders this with
  // `key={company}` so a company switch remounts rather than carrying a key
  // typed for one company into another company's Save. See the same note in
  // the Finance section's provider forms.
  const load = useCallback(async () => {
    try {
      const next = await getHosting(client, company);
      setStatus(next);
      setLoadError(null);
      // Seed the team box with what is stored — it is the one non-secret field,
      // and an operator correcting a typo should not have to retype it.
      setTeam(next.team ?? "");
    } catch (err) {
      setLoadError(reason(err));
    }
  }, [client, company]);

  useEffect(() => {
    void load();
  }, [load]);

  async function onSave() {
    // Send only what was actually entered. The host treats this as a patch, so
    // an untouched key keeps its stored value rather than being cleared.
    const body: Record<string, string> = {};
    if (apiKey.trim()) body.apiKey = apiKey.trim();
    if (team.trim() && team.trim() !== (status?.team ?? "")) body.team = team.trim();

    if (Object.keys(body).length === 0) {
      toast.info("Nothing to save — fill in a field first.");
      return;
    }

    setBusy(true);
    try {
      const next = await saveHosting(client, company, body);
      setStatus(next);
      // Clear the secret input on success: leaving a key sitting in a form field
      // after it has been stored is one stray screen-share from a leak.
      setApiKey("");
      toast.success("Hosting settings saved.");
    } catch (err) {
      toast.error(reason(err));
    } finally {
      setBusy(false);
    }
  }

  async function onClear() {
    setBusy(true);
    try {
      const next = await clearHosting(client, company);
      setStatus(next);
      setApiKey("");
      setTeam("");
      toast.success("Hosting credentials cleared.");
    } catch (err) {
      toast.error(reason(err));
    } finally {
      setBusy(false);
    }
  }

  /*
    Hoisted above the state conditionals for the same reason as `SearchView`'s
    (codex review, #1785) — this page is the other half of that copy-paste
    pair, and had the same two unnamed states.
  */
  const header = (
    <PageHeader
      title="Hosting"
      width="5xl"
      description={
        <>
          Connect a hosting provider so your teammates can put a site from this
          company&rsquo;s workspace on the internet — with a managed database
          behind it, and a custom domain in front.
        </>
      }
    />
  );

  if (loadError) {
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        {header}
        <div className="mx-auto w-full max-w-5xl px-4 py-6">
          <Alert variant="destructive" data-testid="hosting-load-error">
            <TriangleAlert className="size-4" />
            <AlertDescription>Could not load hosting settings: {loadError}</AlertDescription>
          </Alert>
        </div>
      </div>
    );
  }

  if (!status) {
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        {header}
        <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
          <Loader2 className="mr-2 size-4 animate-spin" /> Loading hosting…
        </div>
      </div>
    );
  }

  const connected = status.inBuild && status.granted && status.apiKeyConfigured;

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="hosting-view">
      {header}
      <div className="mx-auto min-h-0 w-full max-w-5xl flex-1 space-y-6 overflow-y-auto px-4 py-6">

        {!canManage && (
          <AdminOnlyNotice
            testId="hosting-read-only"
            title="Only an admin can change where this company deploys"
          >
            This token deploys the company&rsquo;s files to the public internet under its
            own account, and a database provisioned through it is a bill that account
            pays &mdash; so an admin holds it. You can see what is connected.
          </AdminOnlyNotice>
        )}

        {/* The two problems this form cannot fix, said before the form so an
          operator does not fill it in and wonder why nothing happened. */}
        {!status.inBuild ? (
          <Alert data-testid="hosting-not-in-build">
            <TriangleAlert className="size-4" />
            <AlertDescription>
              This host was built without the hosting tools, so these settings
              will be stored and have no effect. Rebuild with the{" "}
              <code>openhuman</code> feature.
            </AlertDescription>
          </Alert>
        ) : null}

        {status.inBuild && !status.granted ? (
          <GrantNamespace
            client={client}
            company={company}
            namespace="hosting"
            canManage={canManage}
            explanation="No teammate will get the deployment tools even once a key is saved."
            onGranted={load}
            testId="hosting-not-granted"
          />
        ) : null}

        <Card>
          <CardContent className="space-y-4">
            <div className="flex items-center justify-between">
              <div className="space-y-1">
                <p className="text-sm font-medium capitalize">{status.provider}</p>
                <p className="text-xs text-muted-foreground">
                  {connected
                    ? status.team
                      ? `Connected, deploying under ${status.team}.`
                      : "Connected, deploying under the personal account."
                    : "Not connected yet."}
                </p>
              </div>
              {connected ? (
                <Badge variant="secondary" data-testid="hosting-connected">
                  <Check className="mr-1 size-3" /> Connected
                </Badge>
              ) : null}
            </div>

            <div className="grid gap-4 sm:grid-cols-2">
              {/* Withheld from a member rather than disabled. A disabled box is
                still somewhere to aim a paste, and the thing being pasted is a
                live deployment credential — the one field on this page where
                learning you were not allowed *after* the fact has a cost that
                outlives the click. The Team box below stays, read-only: it is
                not a secret, and which account this company deploys under is
                worth being able to read. */}
              {canManage ? (
                <div className="space-y-2">
                  <Label htmlFor="hosting-key">API token</Label>
                  <Input
                    id="hosting-key"
                    data-testid="hosting-api-key"
                    type="password"
                    autoComplete="off"
                    placeholder={
                      status.apiKeyConfigured ? "Configured — type to replace" : "vercel_…"
                    }
                    value={apiKey}
                    onChange={(e) => setApiKey(e.target.value)}
                  />
                  <p className="text-xs text-muted-foreground">
                    Create one at vercel.com → Account Settings → Tokens. Stored
                    write-only: it is never shown again, here or anywhere else.
                  </p>
                </div>
              ) : null}

              <div className="space-y-2">
                <Label htmlFor="hosting-team">Team (optional)</Label>
                <Input
                  id="hosting-team"
                  data-testid="hosting-team"
                  placeholder="team_…"
                  value={team}
                  disabled={!canManage}
                  onChange={(e) => setTeam(e.target.value)}
                />
                <p className="text-xs text-muted-foreground">
                  The team or organization to deploy under. Leave empty for a
                  personal account.
                </p>
              </div>
            </div>

            <div className="flex items-center gap-2">
              {canManage ? (
                <Button onClick={() => void onSave()} disabled={busy} data-testid="hosting-save">
                  {busy ? <Loader2 className="mr-2 size-4 animate-spin" /> : null}
                  Save
                </Button>
              ) : null}
              {canManage && status.apiKeyConfigured ? (
                <AlertDialog>
                  <AlertDialogTrigger
                    render={
                      <Button variant="outline" disabled={busy} data-testid="hosting-clear">
                        Disconnect
                      </Button>
                    }
                  />
                  <AlertDialogContent>
                    <AlertDialogHeader>
                      <AlertDialogTitle>Disconnect {status.provider}?</AlertDialogTitle>
                      <AlertDialogDescription>
                        This clears the write-only hosting token and team setting. They cannot be
                        recovered; reconnect with a new token before teammates can deploy again.
                      </AlertDialogDescription>
                    </AlertDialogHeader>
                    <AlertDialogFooter>
                      <AlertDialogCancel>Cancel</AlertDialogCancel>
                      <AlertDialogAction variant="destructive" onClick={() => void onClear()}>
                        Disconnect hosting
                      </AlertDialogAction>
                    </AlertDialogFooter>
                  </AlertDialogContent>
                </AlertDialog>
              ) : null}
            </div>
          </CardContent>
        </Card>

        <p className="text-xs text-muted-foreground">
          A deployment publishes the files in a teammate&rsquo;s workspace to the
          public internet under this account, and provisioning a database is a
          bill this account pays. Nothing else in this app can read the
          connection string a database produces — the provider injects it into
          the site&rsquo;s own environment.
        </p>
      </div>
    </div>
  );
}
