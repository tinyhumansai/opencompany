import { useEffect, useState } from "react";
import { AlertTriangle, BadgeCheck, Check, Loader2, Plus, RotateCw } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import {
  getMcpRegistryEntry,
  type McpCatalogueDetail,
  type McpCatalogueEntry,
} from "@/api/mcp-registry";
import type { McpServer } from "@/api/types";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";
import {
  catalogPublisher,
  REGISTRY_UNWIRED_NOTICE,
  directoryServerName,
  registryOutage,
} from "@/lib/mcp-registry";
import { useMcpDirectory } from "@/hooks/use-mcp-directory";
import {
  McpServerIcon,
  openFromItem,
} from "@/views/connections/McpServerTable";

/** The name this company already holds a directory entry under, if it does. */
export function installedAs(
  servers: McpServer[],
  entry: McpCatalogueEntry,
): string | null {
  const byQualified = servers.find(
    (s) =>
      s.qualifiedName === entry.qualifiedName ||
      s.name.trim() === entry.qualifiedName,
  );
  if (byQualified) return byQualified.name;
  const slug = directoryServerName(entry.displayName);
  const byName = servers.find((s) => s.name.trim().toLowerCase() === slug);
  return byName?.name ?? null;
}

interface EntryProps {
  entry: McpCatalogueEntry;
  installedAs: string | null;
  installing: boolean;
  canManage: boolean;
  onInstall: (entry: McpCatalogueEntry) => void;
  onOpen: (entry: McpCatalogueEntry) => void;
}

function Verified({ official }: { official: boolean }) {
  if (!official) return null;
  return (
    <BadgeCheck
      className="size-4 shrink-0 text-status-done"
      aria-label="Verified publisher"
      data-testid="mcp-discover-verified"
    />
  );
}

function InstallControl({
  entry,
  installedAs,
  installing,
  canManage,
  onInstall,
}: Omit<EntryProps, "onOpen">) {
  if (installedAs !== null) {
    return (
      <span
        className="flex size-8 items-center justify-center rounded-md border border-status-done-text/30 bg-status-done-text/10 text-status-done-text"
        aria-label={`${entry.displayName} is installed`}
        data-testid="mcp-discover-installed"
      >
        <Check className="size-4" />
      </span>
    );
  }
  if (!canManage) return null;
  return (
    <Button
      size="icon-sm"
      variant="outline"
      disabled={installing}
      aria-label={`Install ${entry.displayName}`}
      data-testid="mcp-discover-install"
      onClick={() => onInstall(entry)}
    >
      {installing ? (
        <Loader2 className="size-4 animate-spin" />
      ) : (
        <Plus className="size-4" />
      )}
    </Button>
  );
}

export function McpDirectoryCard(props: EntryProps) {
  const { entry, onOpen } = props;
  const publisher = catalogPublisher(entry);
  return (
    <div
      data-testid="mcp-discover-card"
      onClick={(event) => openFromItem(event, () => onOpen(entry))}
      className="flex cursor-pointer gap-3 rounded-xl border border-border p-4 transition-colors select-none hover:bg-muted/40"
    >
      <McpServerIcon
        iconUrl={entry.iconUrl}
        name={entry.displayName}
        className="size-10"
      />
      <div className="min-w-0 flex-1 space-y-0.5">
        <div className="flex min-w-0 items-center gap-1.5">
          <span className="truncate text-sm font-medium">
            {entry.displayName}
          </span>
          <Verified official={entry.official} />
        </div>
        {entry.description && (
          <p className="line-clamp-2 text-xs text-muted-foreground">
            {entry.description}
          </p>
        )}
        {publisher && (
          <p className="truncate text-xs text-muted-foreground/80">
            by {publisher}
          </p>
        )}
      </div>
      <div className="shrink-0">
        <InstallControl {...props} />
      </div>
    </div>
  );
}

export function McpDirectoryRow(props: EntryProps) {
  const { entry, onOpen } = props;
  const publisher = catalogPublisher(entry);
  return (
    <tr
      data-testid="mcp-discover-row"
      onClick={(event) => openFromItem(event, () => onOpen(entry))}
      className="cursor-pointer transition-colors select-none hover:bg-muted/40"
    >
      <td className="border-b border-border py-3 pr-3 pl-4 align-middle">
        <div className="flex min-w-0 items-center gap-2">
          <McpServerIcon iconUrl={entry.iconUrl} name={entry.displayName} />
          <span className="truncate text-sm font-medium">
            {entry.displayName}
          </span>
          <Verified official={entry.official} />
        </div>
      </td>
      <td className="hidden border-b border-border px-3 py-3 align-middle text-xs text-muted-foreground md:table-cell">
        <span className="block truncate">{publisher ?? "—"}</span>
      </td>
      <td className="border-b border-border py-3 pr-4 pl-3 align-middle">
        <div className="flex justify-end">
          <InstallControl {...props} />
        </div>
      </td>
    </tr>
  );
}

function DirectoryTable({ children }: { children: React.ReactNode }) {
  return (
    <div className="overflow-hidden rounded-lg border border-border">
      <table className="w-full table-fixed border-collapse text-sm">
        <thead>
          <tr>
            <th className="border-b border-border py-2.5 pr-3 pl-4 text-left text-3xs font-medium tracking-wide text-muted-foreground uppercase">
              Connector
            </th>
            <th className="hidden w-48 border-b border-border px-3 py-2.5 text-left text-3xs font-medium tracking-wide text-muted-foreground uppercase md:table-cell">
              Publisher
            </th>
            <th className="w-20 border-b border-border py-2.5 pr-4 pl-3" />
          </tr>
        </thead>
        <tbody>{children}</tbody>
      </table>
    </div>
  );
}

/**
 * One directory entry, before it is installed: what it is, who publishes it,
 * and whether this host can install it.
 */
function McpDirectoryDialog({
  client,
  company,
  entry,
  installedAs,
  installing,
  canManage,
  onInstall,
  onClose,
}: {
  client: OpenCompanyClient;
  company: string | null;
  entry: McpCatalogueEntry | null;
  installedAs: string | null;
  installing: boolean;
  canManage: boolean;
  onInstall: (entry: McpCatalogueEntry) => void;
  onClose: () => void;
}) {
  const [lookup, setLookup] = useState(0);
  const [detail, setDetail] = useState<
    | { kind: "loading" }
    | { kind: "ready"; detail: McpCatalogueDetail }
    | { kind: "failed"; message: string }
  >({ kind: "loading" });

  useEffect(() => {
    if (!entry) return;
    const controller = new AbortController();
    setDetail({ kind: "loading" });
    getMcpRegistryEntry(client, company, entry.qualifiedName, {
      signal: controller.signal,
    })
      .then((found) => {
        if (!controller.signal.aborted) setDetail({ kind: "ready", detail: found });
      })
      .catch((err) => {
        if (controller.signal.aborted) return;
        const outage = registryOutage(err);
        setDetail({
          kind: "failed",
          message:
            outage.kind === "unwired" ? REGISTRY_UNWIRED_NOTICE : outage.message,
        });
      });
    return () => controller.abort();
  }, [client, company, entry, lookup]);

  if (!entry) return null;
  const publisher = catalogPublisher(entry);
  const refusal =
    detail.kind === "ready" && !detail.detail.installable
      ? detail.detail.refusal
      : undefined;

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="sm:max-w-lg" data-testid="mcp-discover-detail">
        <DialogHeader>
          <div className="flex items-center gap-3">
            <McpServerIcon
              iconUrl={entry.iconUrl}
              name={entry.displayName}
              className="size-12"
            />
            <div className="min-w-0">
              <DialogTitle className="flex items-center gap-1.5">
                <span className="truncate">{entry.displayName}</span>
                <Verified official={entry.official} />
              </DialogTitle>
              {publisher && (
                <DialogDescription>by {publisher}</DialogDescription>
              )}
            </div>
          </div>
        </DialogHeader>
        <div className="space-y-3 text-sm">
          <p className="text-muted-foreground">
            {entry.description ?? "The directory listing carries no description."}
          </p>
          {detail.kind === "loading" ? (
            <Skeleton className="h-8 rounded-md" />
          ) : detail.kind === "failed" ? (
            <div
              className="flex items-start justify-between gap-2"
              data-testid="mcp-discover-detail-failed"
            >
              <p className="text-xs text-destructive">{detail.message}</p>
              <Button
                size="sm"
                variant="outline"
                data-testid="mcp-discover-detail-retry"
                onClick={() => setLookup((n) => n + 1)}
              >
                <RotateCw className="size-4" /> Retry
              </Button>
            </div>
          ) : detail.detail.endpoint ? (
            <code className="block truncate rounded-md border border-border bg-muted/40 px-2 py-1 font-mono text-xs select-text">
              {detail.detail.endpoint}
            </code>
          ) : null}
          {refusal && (
            <p className="text-xs text-status-blocked-text" data-testid="mcp-discover-refusal">
              {refusal}
            </p>
          )}
          {!entry.official && (
            <p
              className="flex items-start gap-2 rounded-md border border-status-blocked-text/30 bg-status-blocked-text/10 px-2 py-1 text-xs text-status-blocked-text"
              data-testid="mcp-directory-unverified"
            >
              <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
              <span>
                Not a verified publisher. Every tool it exposes still starts
                un-granted until somebody sets it.
              </span>
            </p>
          )}
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            Close
          </Button>
          {installedAs !== null ? (
            <Button disabled data-testid="mcp-discover-detail-installed">
              <Check className="size-4" /> Installed
            </Button>
          ) : (
            canManage && (
              <Button
                data-testid="mcp-discover-detail-install"
                disabled={
                  installing || refusal !== undefined || detail.kind === "failed"
                }
                onClick={() => onInstall(entry)}
              >
                {installing ? (
                  <Loader2 className="size-4 animate-spin" />
                ) : (
                  <Plus className="size-4" />
                )}
                Install
              </Button>
            )
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** Discover: the directory as cards or a list, browsed before anything is typed. */
export function McpDiscover({
  client,
  company,
  query,
  layout,
  servers,
  installing,
  canManage,
  onInstall,
}: {
  client: OpenCompanyClient;
  company: string | null;
  query: string;
  layout: "cards" | "list";
  servers: McpServer[];
  installing: string | null;
  canManage: boolean;
  onInstall: (entry: McpCatalogueEntry) => Promise<boolean> | void;
}) {
  const { state, matches, loadMore, retry } = useMcpDirectory(client, company, query);
  const [previewing, setPreviewing] = useState<McpCatalogueEntry | null>(null);
  const term = query.trim();
  const searching = term !== "";

  if (state.kind === "outage") {
    return state.outage.kind === "unwired" ? (
      <p className="text-xs text-muted-foreground" data-testid="mcp-registry-unwired">
        {REGISTRY_UNWIRED_NOTICE}
      </p>
    ) : (
      <div className="flex flex-wrap items-center gap-2" data-testid="mcp-registry-outage">
        <p className="text-xs text-status-blocked-text" data-testid="mcp-registry-error">
          <strong className="font-medium">
            {searching ? `Couldn't search for “${term}”.` : "The directory isn't answering."}
          </strong>{" "}
          {state.outage.message} Your own servers are unaffected.
        </p>
        <Button
          size="sm"
          variant="outline"
          data-testid="mcp-registry-retry"
          onClick={retry}
        >
          <RotateCw className="size-4" /> Retry
        </Button>
      </div>
    );
  }

  const instant = state.kind === "loading" && searching && matches.length > 0;
  const shown =
    state.kind === "loading"
      ? instant
        ? matches
        : state.previous
      : state.entries;
  const stale = state.kind === "loading" && !instant && shown.length > 0;

  const itemProps = (entry: McpCatalogueEntry): EntryProps => ({
    entry,
    installedAs: installedAs(servers, entry),
    installing: installing === entry.qualifiedName,
    canManage,
    onInstall,
    onOpen: setPreviewing,
  });

  return (
    <section className="space-y-3" data-testid="mcp-discover">
      <div className="flex items-center gap-2">
        <h3 className="text-sm font-medium">
          {searching ? "Results" : "Top connectors"}
        </h3>
        {state.kind === "loading" && (
          <span
            className="flex items-center gap-1 text-xs text-muted-foreground"
            role="status"
            data-testid="mcp-discover-searching"
          >
            <Loader2 className="size-3.5 animate-spin" />
            {searching ? `Searching for “${term}”…` : "Loading…"}
          </span>
        )}
      </div>

      {state.kind === "fallback" && (
        <div className="flex flex-wrap items-center gap-2" data-testid="mcp-discover-fallback">
          <p className="text-xs text-muted-foreground">
            Showing popular matches — the MCP directory is slow right now.
          </p>
          <Button
            size="sm"
            variant="outline"
            data-testid="mcp-discover-fallback-retry"
            onClick={retry}
          >
            <RotateCw className="size-4" /> Retry
          </Button>
        </div>
      )}

      {state.kind === "loading" && shown.length === 0 ? (
        layout === "cards" ? (
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            {Array.from({ length: 4 }, (_, i) => (
              <Skeleton key={i} className="h-24 rounded-xl" />
            ))}
          </div>
        ) : (
          <div className="space-y-2">
            {Array.from({ length: 4 }, (_, i) => (
              <Skeleton key={i} className="h-12 rounded-md" />
            ))}
          </div>
        )
      ) : shown.length === 0 ? (
        <p className="text-sm text-muted-foreground" data-testid="mcp-search-nothing">
          {searching
            ? "The directory has no listing for that. A server running inside your own network is added as a custom server."
            : "The directory returned nothing to show."}
        </p>
      ) : (
        <div
          className={cn("transition-opacity", stale && "opacity-50")}
          aria-busy={stale || undefined}
          data-testid="mcp-discover-results"
        >
          {layout === "cards" ? (
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
              {shown.map((entry) => (
                <McpDirectoryCard key={entry.qualifiedName} {...itemProps(entry)} />
              ))}
            </div>
          ) : (
            <DirectoryTable>
              {shown.map((entry) => (
                <McpDirectoryRow key={entry.qualifiedName} {...itemProps(entry)} />
              ))}
            </DirectoryTable>
          )}
        </div>
      )}

      {state.kind === "ready" && state.page < state.totalPages && (
        <div className="flex flex-col items-center gap-2">
          {state.moreFailed && (
            <p
              className="text-xs text-status-blocked-text"
              data-testid="mcp-discover-more-error"
            >
              {state.moreFailed}
            </p>
          )}
          <Button
            variant="outline"
            size="sm"
            disabled={state.loadingMore}
            data-testid="mcp-discover-more"
            onClick={loadMore}
          >
            {state.loadingMore && <Loader2 className="size-4 animate-spin" />}
            {state.moreFailed ? "Retry" : "Show more"}
          </Button>
        </div>
      )}

      <McpDirectoryDialog
        client={client}
        company={company}
        entry={previewing}
        installedAs={previewing ? installedAs(servers, previewing) : null}
        installing={previewing !== null && installing === previewing.qualifiedName}
        canManage={canManage}
        onInstall={(entry) => {
          void Promise.resolve(onInstall(entry)).then((installed) => {
            if (installed) setPreviewing(null);
          });
        }}
        onClose={() => setPreviewing(null)}
      />
    </section>
  );
}
