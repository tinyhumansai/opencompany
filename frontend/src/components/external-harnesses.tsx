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
import { acpHarnesses, confirmAcpHarness, installAcpHarness } from "@/api/transport/desktop";
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
  desktopHarnessId,
  harnessAction,
  isChecking,
  isUsableHere,
  joinHarnesses,
  readinessNote,
  withReadiness,
  type HarnessRow,
} from "@/lib/harnesses";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/**
 * The status dot and word for a row.
 *
 * The word carries the meaning and the dot is shorthand — the same rule
 * `host-switcher.tsx` follows, because hue alone tells someone that something
 * differs without saying what.
 */
function statusOf(row: HarnessRow): { label: string; dot: string } {
  if (row.kind === "built_in") return { label: "Managed", dot: "bg-status-done" };
  if (row.transport === "runner") return { label: "Remote", dot: "bg-muted-foreground/50" };
  switch (row.readiness?.state) {
    case "checking":
      return { label: "Checking…", dot: "bg-status-running animate-pulse" };
    case "ready":
      return { label: "Ready", dot: "bg-status-done" };
    case "notSignedIn":
      return { label: "Not signed in", dot: "bg-status-blocked" };
    case "notInstalled":
      return { label: "Not installed", dot: "bg-muted-foreground/50" };
    case "adapterMissing":
      // Not "Not installed": the CLI *is* installed, and this app is one small
      // add-on away from being able to drive it. Saying otherwise is what sent
      // people to reinstall software they already had.
      return { label: "Add-on needed", dot: "bg-status-blocked" };
    case "adapterOutdated":
      return { label: "Update available", dot: "bg-status-blocked" };
    case "nodeMissing":
      return { label: "Needs Node.js", dot: "bg-muted-foreground/50" };
    case "spawnFailed":
      return { label: "Won't start", dot: "bg-destructive" };
    default:
      // Nothing probed this machine — a browser, or a desktop shell predating
      // `oc_acp_harnesses`. Deliberately not "Not installed": those are
      // different facts, and saying the second here would tell someone to
      // reinstall a CLI already sitting on their machine, unseen from a tab.
      return { label: "Desktop only", dot: "bg-muted-foreground/50" };
  }
}

export function ExternalHarnesses({ client, company }: Props) {
  const [rows, setRows] = useState<HarnessRow[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [unsupported, setUnsupported] = useState(false);

  /**
   * Which load a confirmation belongs to.
   *
   * Bumped on every `load`, and captured by each in-flight probe. A probe from
   * a superseded run resolves against a stale generation and is dropped — so
   * hitting "Check again" while a slow CLI is still starting cannot have that
   * older answer land on the newer list.
   */
  const generation = useRef(0);

  /**
   * Which harness is installing, and what went wrong last time, by id.
   *
   * Per-row rather than one page-level flag: installs are independent, and a
   * single "installing" state would disable Codex's button while Claude's
   * fetch ran.
   */
  const [installing, setInstalling] = useState<Set<string>>(new Set());
  const [installErrors, setInstallErrors] = useState<Record<string, string>>({});

  const load = useCallback(async () => {
    const run = ++generation.current;
    setLoading(true);
    try {
      // The company half is required; the machine half is best-effort. A
      // browser has no local probe at all, and an older desktop shell has no
      // `oc_acp_harnesses` — both yield `null`, which the join renders as
      // "can't say from here" rather than as "not installed".
      const declared: HarnessDto[] = await client.listHarnesses(company);
      const local = isDesktopRuntime() ? await acpHarnesses().catch(() => null) : null;
      if (generation.current !== run) return;

      const joined = joinHarnesses(declared, local);
      setRows(joined);
      setUnsupported(false);
      setLoading(false);

      // Phase two, and the only phase with an answer in it. Every row arrives
      // `checking` — the host looks nothing up — so each one is started here
      // and settled by what it does. Fired in parallel and applied as each
      // lands, so a slow CLI delays only its own row.
      await Promise.all(
        joined.filter(isChecking).map(async (row) => {
          const confirmation = await confirmAcpHarness(desktopHarnessId(row));
          // `null` means nothing could answer; the row stays on `Checking`
          // rather than being given a verdict nobody reached. The models the
          // confirmation carried are cached inside `confirmAcpHarness` for
          // the agent picker to read, so nothing is threaded through here.
          if (!confirmation || generation.current !== run) return;
          setRows((current) =>
            current
              ? withReadiness(current, row.id, confirmation.readiness, confirmation.path)
              : current,
          );
        }),
      );
    } catch (error) {
      // A host predating `GET {scope}/harnesses` renders nothing rather than
      // an error — matching how `DevicePairing`/`localInstances` degrade for
      // an older host: absent, not broken.
      // Generation-checked like every other write in this component, and it
      // was the one that was not. Settings is reused across company changes,
      // so a 404 from a *superseded* load — an older host that lacked the
      // route — arrived after the new company had already populated its rows
      // and hid a panel that was working.
      if (generation.current !== run) return;
      if (error instanceof ApiError && error.status === 404) setUnsupported(true);
      setLoading(false);
    }
  }, [client, company]);

  /**
   * Fetches one harness's adapter, then re-settles that row on its own.
   *
   * Re-confirms only the row that changed rather than reloading the pane: a
   * full `load()` would restart every other harness's CLI for an event that
   * cannot have altered them, and would reset rows the operator is reading.
   *
   * The generation guard applies here too. An install started before a "Check
   * again" must not apply its result to the newer list — the row it was about
   * may not even be in it.
   */
  const install = useCallback(async (row: HarnessRow) => {
    // Two ids in play: `row.id` keys this page's own state, `desktopHarnessId`
    // addresses the shell's catalogue. Conflating them is what made a
    // declared `laptop` unaddressable.
    const id = row.id;
    const desktopId = desktopHarnessId(row);
    const run = generation.current;
    setInstalling((current) => new Set(current).add(id));
    setInstallErrors(({ [id]: _dropped, ...rest }) => rest);

    const failure = await installAcpHarness(desktopId);
    const settle = () =>
      setInstalling((current) => {
        const next = new Set(current);
        next.delete(id);
        return next;
      });

    if (generation.current !== run) return settle();
    if (failure) {
      // npm's own words, kept verbatim. A rewritten message would be this
      // layer guessing at a failure it did not diagnose.
      setInstallErrors((current) => ({ ...current, [id]: failure }));
      return settle();
    }

    // Straight back to `checking`, because that is now true again: something
    // was installed and nothing has started it yet. Leaving the old verdict up
    // while the handshake runs would show "Add-on needed" on a row that just
    // got its add-on.
    setRows((current) => (current ? withReadiness(current, id, { state: "checking" }) : current));
    const confirmation = await confirmAcpHarness(desktopId);
    if (confirmation && generation.current === run) {
      setRows((current) =>
        current ? withReadiness(current, id, confirmation.readiness, confirmation.path) : current,
      );
    }
    settle();
  }, []);

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
          The coding engines a teammate here can run on. A teammate is put on one from its
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
