// External harnesses (issue #1245's detected-harness follow-up): every coding
// engine a teammate here can be bound to, and — on the desktop — whether each
// can actually run on this machine.
//
// This replaces two cards that sat next to each other answering half the
// question each: one listed what the company declared and could not say
// whether any of it worked, the other reported what was installed on this
// machine and could not say whether the company could use it. Neither alone
// answers "can I put a teammate on Claude Code?", which is the only question
// anyone opens this page with.
//
// There is deliberately **no connect action**. A local coding CLI is usable
// exactly when it is installed and signed in on this machine — there is no
// state in between for a button to move it through, and a "connected" flag
// stored anywhere would be a second source of truth that could disagree with
// the CLI actually being there.
//
// There *is* an install action, and it is a different thing. It does not
// connect anything: it fetches the ACP adapter, which is this app's own
// dependency rather than the operator's. Somebody installed Claude Code; they
// did not install `@agentclientprotocol/claude-agent-acp` and have no reason
// to know it exists. Explicit rather than automatic — it is a network fetch
// that writes executables, and an app should ask before doing that.

import { useCallback, useEffect, useRef, useState } from "react";
import { Cpu, RefreshCw, Server } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { isDesktopRuntime } from "@/api/transport";
import { ApiError, type HarnessDto } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardAction,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import {
  harnessAction,
  isChecking,
  isUsableHere,
  readinessNote,
  statusOf,
} from "@/lib/harnesses";
import { useHarnessRows } from "@/lib/use-harness-rows";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

export function ExternalHarnesses({ client, company }: Props) {
  const [declared, setDeclared] = useState<HarnessDto[] | null>(null);
  const [fetching, setFetching] = useState(true);
  const [unsupported, setUnsupported] = useState(false);

  /**
   * Which fetch an answer belongs to.
   *
   * Settings is reused across company changes, so a 404 from a *superseded*
   * load — an older host that lacked the route — could arrive after the new
   * company had already populated its rows and hide a panel that was working.
   */
  const generation = useRef(0);
  /** The company `declared` currently answers for, so a company switch can drop stale rows. */
  const declaredFor = useRef<string | null>(null);

  const load = useCallback(async () => {
    const run = ++generation.current;
    if (declaredFor.current !== company) setDeclared(null);
    setFetching(true);
    try {
      // The company half is required; the machine half is the survey's, and
      // best-effort.
      const next: HarnessDto[] = await client.listHarnesses(company);
      if (generation.current !== run) return;
      setDeclared(next);
      declaredFor.current = company;
      setUnsupported(false);
      setFetching(false);
    } catch (error) {
      // A host predating `GET {scope}/harnesses` renders nothing rather than
      // an error — matching how `DevicePairing`/`localInstances` degrade for
      // an older host: absent, not broken.
      if (generation.current !== run) return;
      if (error instanceof ApiError && error.status === 404) setUnsupported(true);
      setFetching(false);
    }
  }, [client, company]);

  const { rows, surveying, install, installing, installErrors } = useHarnessRows(declared);
  // One flag for the whole round trip, as before: the pane is loading from the
  // moment the fetch starts until the joined rows are on screen, and "Check
  // again" stays held for that whole span rather than re-arming mid-survey.
  const loading = fetching || surveying;

  useEffect(() => {
    void load();
  }, [load]);

  if (unsupported) return null;

  const usableCount = rows?.filter(isUsableHere).length ?? 0;
  const stillChecking = rows?.some(isChecking) ?? false;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">External harnesses</CardTitle>
        <CardDescription>
          The coding engines an agent here can run on. An agent is put on one from its
          own page, under Harness &amp; model.
        </CardDescription>
        <CardAction>
          <Button variant="outline" size="sm" disabled={loading} onClick={() => void load()}>
            <RefreshCw className={cn("size-4", loading && "animate-spin")} />
            Check again
          </Button>
        </CardAction>
      </CardHeader>
      <CardContent className="space-y-0 divide-y">
        {loading && !rows ? (
          <p className="py-3 text-sm text-muted-foreground">Checking…</p>
        ) : (
          rows?.map((row) => {
            const status = statusOf(row);
            const action = harnessAction(row);
            const busy = installing.has(row.id);
            const failure = installErrors[row.id];
            return (
              <div
                key={row.id}
                className="flex items-center justify-between gap-4 py-3 first:pt-0 last:pb-0"
                data-testid="harness-row"
              >
                <div className="flex min-w-0 items-center gap-2.5">
                  {row.kind === "acp" ? (
                    <Cpu className="size-4 shrink-0 text-muted-foreground" />
                  ) : (
                    <Server className="size-4 shrink-0 text-muted-foreground" />
                  )}
                  <div className="min-w-0">
                    <p className="truncate text-sm font-medium">
                      {row.label}
                      <span className="ml-1.5 font-mono text-xs text-muted-foreground">
                        {row.id}
                      </span>
                    </p>
                    <p
                      className={cn(
                        "mt-0.5 truncate text-xs",
                        failure ? "text-destructive" : "text-muted-foreground",
                      )}
                      title={failure ?? readinessNote(row)}
                    >
                      {failure ?? readinessNote(row)}
                    </p>
                  </div>
                </div>
                <div className="flex shrink-0 items-center gap-1.5">
                  {row.isDefault && <Badge variant="secondary">Default</Badge>}
                  {row.declared && <Badge variant="outline">In blueprint</Badge>}
                  {action !== "none" && (
                    <Button
                      size="sm"
                      variant={action === "install" ? "default" : "outline"}
                      disabled={busy}
                      onClick={() => void install(row)}
                    >
                      {busy
                        ? "Installing…"
                        : action === "install"
                          ? "Install add-on"
                          : "Update"}
                    </Button>
                  )}
                  <span className="inline-flex shrink-0 items-center gap-1.5 rounded-full border bg-card px-2.5 py-0.5 text-xs font-medium">
                    <span className={cn("size-1.5 rounded-full", status.dot)} />
                    {status.label}
                  </span>
                </div>
              </div>
            );
          })
        )}
      </CardContent>
      {rows && rows.length > 0 && (
        <CardContent>
          <p className="text-xs text-muted-foreground">
            {/* No count while answers are still arriving: "1 of 3 can run a
                turn" is a claim, and it would be wrong for as long as the
                slowest CLI takes to start. */}
            {stillChecking
              ? "Starting the installed coding CLIs to confirm they work…"
              : usableCount === 0
                ? "Nothing here can run a turn on this machine yet."
                : `${usableCount} of ${rows.length} can run a turn here.`}
            {!isDesktopRuntime() &&
              " Installed coding CLIs can only be checked from the desktop app."}
          </p>
        </CardContent>
      )}
    </Card>
  );
}
