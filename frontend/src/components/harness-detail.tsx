// One harness, in full: what this machine can say about it, what it can run,
// and which teammates are on it.
//
// A dialog rather than a page. There is no existing address for a harness to
// preserve — the panel this replaces was a card on Settings with no route of
// its own — and `lib/console-routes.ts` documents what adding one costs when it
// goes wrong. The content is a plain component underneath, so a route can be
// added later by rendering it somewhere else.
//
// Mounted from two places, the agent editor and the Providers page, with the
// same live-computed content either way. Neither owns a copy: the caller passes
// the row and the install seam from its own `useHarnessRows`, so the list a
// surface shows and the dialog opened from it cannot disagree.

import { useEffect, useState } from "react";
import { RefreshCw } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { cachedAcpModels, ensureAcpModels, type AcpHarnessModel } from "@/api/transport/desktop";
import type { TeamMemberDto } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { agentHref } from "@/lib/agent-profile";
import {
  boundAgents,
  boundLabel,
  desktopHarnessId,
  harnessAction,
  readinessNote,
  statusOf,
  type HarnessRow,
} from "@/lib/harnesses";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** The harness this is about, or `null` when nothing is open. */
  row: HarnessRow | null;
  /** The id of the harness marked `default = true`, for resolving unbound teammates. */
  defaultHarnessId: string | undefined;
  /**
   * Re-run the caller's own `GET {scope}/harnesses`.
   *
   * The survey has no re-check of its own by design: readiness is derived from
   * the declared list, so re-fetching that list is what re-surveys. A second
   * refresh path here could settle rows the caller's list no longer contains.
   */
  onRecheck: () => void;
  /** Whether that re-fetch, or the survey it starts, is still running. */
  checking: boolean;
  install: (row: HarnessRow) => Promise<string | null>;
  installing: Set<string>;
  installErrors: Record<string, string>;
  onOpenChange: (open: boolean) => void;
}

export function HarnessDetailDialog({
  client,
  company,
  row,
  defaultHarnessId,
  onRecheck,
  checking,
  install,
  installing,
  installErrors,
  onOpenChange,
}: Props) {
  return (
    <Dialog open={row !== null} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg" data-testid="harness-detail">
        {row && (
          <>
            <DialogHeader>
              <DialogTitle className="flex flex-wrap items-center gap-2">
                {row.label}
                <span className="font-mono text-xs font-normal text-muted-foreground">{row.id}</span>
                {row.isDefault && <Badge variant="secondary">Default</Badge>}
                {row.declared && <Badge variant="outline">In blueprint</Badge>}
              </DialogTitle>
              <DialogDescription>
                A coding engine a teammate here can be put on. Everything below is about this
                machine right now — nothing is stored.
              </DialogDescription>
            </DialogHeader>
            <HarnessStatusBlock
              row={row}
              failure={installErrors[row.id]}
              busy={installing.has(row.id)}
              checking={checking}
              onInstall={() => void install(row)}
              onRecheck={onRecheck}
            />
            <HarnessFacts row={row} />
            <DetectedModels row={row} />
            <BoundTeammates
              client={client}
              company={company}
              harnessId={row.id}
              defaultHarnessId={defaultHarnessId}
            />
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

/** The verdict, what to do about it, and the two controls that can change it. */
function HarnessStatusBlock({
  row,
  failure,
  busy,
  checking,
  onInstall,
  onRecheck,
}: {
  row: HarnessRow;
  failure: string | undefined;
  busy: boolean;
  checking: boolean;
  onInstall: () => void;
  onRecheck: () => void;
}) {
  const status = statusOf(row);
  const action = harnessAction(row);
  return (
    <section className="space-y-2 rounded-md border p-3" data-testid="harness-detail-status">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="inline-flex shrink-0 items-center gap-1.5 rounded-full border bg-card px-2.5 py-0.5 text-xs font-medium">
          <span className={cn("size-1.5 rounded-full", status.dot)} />
          {status.label}
        </span>
        <div className="flex items-center gap-1.5">
          <Button variant="outline" size="sm" disabled={checking} onClick={onRecheck}>
            <RefreshCw className={cn("size-4", checking && "animate-spin")} />
            Check again
          </Button>
          {action !== "none" && (
            <Button
              size="sm"
              variant={action === "install" ? "default" : "outline"}
              disabled={busy}
              onClick={onInstall}
              data-testid="harness-detail-install"
            >
              {busy ? "Installing…" : action === "install" ? "Install add-on" : "Update"}
            </Button>
          )}
        </div>
      </div>
      <p className={cn("text-xs", failure ? "text-destructive" : "text-muted-foreground")}>
        {failure ?? readinessNote(row)}
      </p>
    </section>
  );
}

/**
 * The facts behind the verdict — what kind of harness this is, and whatever the
 * readiness union carries for the state it settled on.
 *
 * Read off the same union the status word came from rather than re-derived, so
 * a row cannot show a path or a version the verdict did not produce.
 */
function HarnessFacts({ row }: { row: HarnessRow }) {
  const facts: [string, string][] = [
    ["Kind", row.kind === "acp" ? "Coding CLI (ACP)" : "Built in to this host"],
  ];
  if (row.transport) facts.push(["Runs", row.transport === "local" ? "On this machine" : "On a registered remote machine"]);
  if (row.agent && row.agent !== row.id) facts.push(["Drives", row.agent]);

  const readiness = row.readiness;
  if (readiness?.state === "adapterMissing") {
    facts.push(["CLI found at", readiness.cli]);
    facts.push(["Add-on needed", readiness.package]);
  }
  if (readiness?.state === "adapterOutdated") {
    facts.push(["Add-on installed", readiness.found]);
    facts.push(["Add-on expected", readiness.want]);
  }
  if (readiness?.state === "spawnFailed") facts.push(["Refused with", readiness.reason]);
  if (row.path) facts.push(["Adapter at", row.path]);

  return (
    <section className="space-y-1.5" data-testid="harness-detail-facts">
      <h4 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">Details</h4>
      <dl className="grid gap-x-3 gap-y-1 text-xs sm:grid-cols-[auto_1fr]">
        {facts.map(([term, value]) => (
          <div key={term} className="contents">
            <dt className="text-muted-foreground">{term}</dt>
            <dd className="font-mono break-all">{value}</dd>
          </div>
        ))}
      </dl>
    </section>
  );
}

/**
 * The models this harness has told us about.
 *
 * Read from the session cache first so an already-confirmed harness fills in
 * without a spawn, then refreshed. An empty list is "nothing has asked it yet",
 * not "it has no models" — a browser never asks — so it says which.
 */
function DetectedModels({ row }: { row: HarnessRow }) {
  const [models, setModels] = useState<AcpHarnessModel[]>([]);
  const desktopId = row.kind === "acp" ? desktopHarnessId(row) : undefined;

  useEffect(() => {
    if (!desktopId) {
      setModels([]);
      return;
    }
    let live = true;
    setModels(cachedAcpModels(desktopId));
    void ensureAcpModels(desktopId).then((found) => {
      if (live) setModels(found);
    });
    return () => {
      live = false;
    };
  }, [desktopId]);

  if (!desktopId) return null;
  return (
    <section className="space-y-1.5" data-testid="harness-detail-models">
      <h4 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
        Detected models
      </h4>
      {models.length === 0 ? (
        <p className="text-xs text-muted-foreground">
          Nothing has asked this harness for its models yet — it reports them once it starts.
        </p>
      ) : (
        <ul className="space-y-1">
          {models.map((model) => (
            <li key={model.value} className="flex flex-wrap items-baseline gap-1.5 text-xs">
              <span className="font-mono">{model.name ?? model.value}</span>
              {model.current && <Badge variant="secondary">Its default</Badge>}
              {model.description && (
                <span className="text-muted-foreground">{model.description}</span>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

/**
 * Which teammates run their turns here.
 *
 * Resolved from the roster read through `boundAgents`, which mirrors the host's
 * own lane rule: a teammate that declares no harness is on the default one and
 * on no other. Each row's Edit is a link to that teammate's own Model tab, not
 * a second place to change a binding — one editing surface, addressed from
 * here.
 */
function BoundTeammates({
  client,
  company,
  harnessId,
  defaultHarnessId,
}: {
  client: OpenCompanyClient;
  company: string | null;
  harnessId: string;
  defaultHarnessId: string | undefined;
}) {
  const [roster, setRoster] = useState<TeamMemberDto[] | null>(null);

  useEffect(() => {
    let live = true;
    // Best-effort: a roster this read cannot reach is a count we decline to
    // claim, not a reason for the rest of the dialog to fail.
    client
      .listTeam(company)
      .then((next) => {
        if (live) setRoster(next);
      })
      .catch(() => {
        if (live) setRoster([]);
      });
    return () => {
      live = false;
    };
  }, [client, company]);

  const bound = roster ? boundAgents(roster, harnessId, defaultHarnessId) : [];
  return (
    <section className="space-y-1.5" data-testid="harness-detail-bound">
      <h4 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
        Bound teammates
      </h4>
      {roster === null ? (
        <p className="text-xs text-muted-foreground">Reading the roster…</p>
      ) : bound.length === 0 ? (
        <p className="text-xs text-muted-foreground">
          Nobody runs on this harness. A teammate is put on one from its own page, under Model.
        </p>
      ) : (
        <>
          <p className="text-xs text-muted-foreground" data-testid="harness-detail-bound-count">
            {boundLabel(bound.length)}
          </p>
          <ul className="divide-y">
            {bound.map((member) => (
              <li key={member.id} className="flex items-center justify-between gap-3 py-1.5">
                <div className="min-w-0">
                  <p className="truncate text-sm">{member.name ?? member.role}</p>
                  {member.harness === undefined && (
                    <p className="text-xs text-muted-foreground">Inherits the default</p>
                  )}
                </div>
                <Button variant="ghost" size="sm" render={<a href={agentHref(member.id, { tab: "model" })} />}>
                  Edit
                </Button>
              </li>
            ))}
          </ul>
        </>
      )}
    </section>
  );
}
