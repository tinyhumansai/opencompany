import { useCallback, useEffect, useRef, useState } from "react";
import { ChevronDown, ChevronRight, Loader2, RotateCcw } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import {
  type ApprovalMode,
  type ToolPolicyDocument,
  type ToolPolicyPatch,
  type ToolPolicyRow,
  type ToolTier,
  policyTarget,
  readToolPolicy,
  resetToolPolicy,
  writeToolPolicy,
} from "@/api/mcp-tool-policy";
import { ApiError, type McpServer } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { MODE_LABELS, ModeChoice } from "@/views/mcp/McpToolPermissionsControl";

/**
 * The value the tier control carries when nothing is stored for that tier.
 *
 * A sentinel rather than an absent value: the control has to be able to say
 * "nothing is set here" and to be set back to it, and a `Select` with no value
 * can do neither.
 */
const UNSET = "unset";

/** The tier control's own vocabulary: the three modes, plus "nothing set". */
const TIER_DEFAULT_LABELS: Record<string, string> = {
  [UNSET]: "Not set",
  ...MODE_LABELS,
};

/** The tiers, in the order they escalate. */
const TIERS: readonly ToolTier[] = ["read_only", "interactive", "write_delete"];

/** The order the sections are read in: what can do the most damage, first. */
const SECTION_ORDER: readonly ToolTier[] = [
  "write_delete",
  "interactive",
  "read_only",
];

const TIER_LABELS: Record<ToolTier, string> = {
  read_only: "Read-only",
  interactive: "Interactive",
  write_delete: "Write & delete",
};

/** How a suggestion is written on a row, in the brief's shorthand. */
const SUGGESTION_LABELS: Record<ToolTier, string> = {
  read_only: "read-only",
  interactive: "interactive",
  write_delete: "write/delete",
};

const SECTION_TITLES: Record<ToolTier, string> = {
  read_only: "Read-only tools",
  interactive: "Interactive tools",
  write_delete: "Write & delete tools",
};

/** How many unremarkable rows a section shows before it offers the rest. */
const VISIBLE_CAP = 8;

/**
 * Why a tool the panel lists is unreachable regardless of what its row says.
 *
 * `allowedTools` / `disallowedTools` are a separate gate, enforced where the
 * server is attached to an agent rather than at the approval ladder — so a row
 * can read "Needs approval" while the transport refuses the call outright.
 */
function exclusion(server: McpServer, tool: string): string | null {
  if (server.disallowedTools.includes(tool))
    return "Not sent — on the deny list";
  if (server.allowedTools.length > 0 && !server.allowedTools.includes(tool)) {
    return "Not sent — off the allow list";
  }
  return null;
}

/**
 * The patch a choice in the tier control means on the wire.
 *
 * The sentinel and the absence it stands for are two vocabularies: a tier
 * cleared back to unset has to arrive as `null`, because the host reads a
 * missing key as "leave it alone".
 */
export function tierPatch(tier: ToolTier, value: string): ToolPolicyPatch {
  return {
    tierDefaults: { [tier]: value === UNSET ? null : (value as ApprovalMode) },
  };
}

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  server: McpServer;
  /** Writes are an admin's. The host answers 403 whatever this says. */
  canManage: boolean;
  /**
   * Bumped by the page when a probe re-ran against this server.
   *
   * A probe rewrites the stored inventory, and the inventory is what a tier
   * default resolves against.
   */
  reloadKey?: number;
  /** Scroll this into view once it has something to show. */
  focus?: boolean;
}

type State =
  | { kind: "loading" }
  | { kind: "ready"; doc: ToolPolicyDocument }
  /** The stored document cannot be parsed. Clearing it is the way back. */
  | { kind: "unreadable"; message: string }
  | { kind: "failed"; message: string };

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function McpToolPermissions({
  client,
  company,
  server,
  canManage,
  reloadKey = 0,
  focus = false,
}: Props) {
  const [state, setState] = useState<State>({ kind: "loading" });
  const [busy, setBusy] = useState(false);
  const [writeError, setWriteError] = useState<string | null>(null);
  const [opened, setOpened] = useState<Partial<Record<ToolTier, boolean>>>({});
  const [showAll, setShowAll] = useState<Partial<Record<ToolTier, boolean>>>(
    {},
  );
  const root = useRef<HTMLDivElement | null>(null);

  const tools = state.kind === "ready" ? state.doc.tools : [];
  const openByDefault =
    SECTION_ORDER.find((tier) =>
      tools.some((row) => row.effectiveTier === tier),
    ) ?? SECTION_ORDER[0];

  const target = policyTarget(server);
  const targetKey = target
    ? target.kind === "registry"
      ? `registry:${target.serverId}`
      : `declared:${target.name}`
    : null;
  // Read by `apply`/`reset` after their await resolves, so a write started
  // against one server never lands on another's panel if the selection moves
  // to a different server while the request is in flight.
  const targetKeyRef = useRef(targetKey);
  targetKeyRef.current = targetKey;

  useEffect(() => {
    if (!target) {
      setState({
        kind: "failed",
        message:
          "This row has no install behind it, so it carries no permissions.",
      });
      return;
    }
    let live = true;
    setState({ kind: "loading" });
    void (async () => {
      try {
        const doc = await readToolPolicy(client, company, target);
        if (live) setState({ kind: "ready", doc });
      } catch (err) {
        if (!live) return;
        setState(
          err instanceof ApiError && err.code === "policy_unreadable"
            ? { kind: "unreadable", message: err.message }
            : { kind: "failed", message: message(err) },
        );
      }
    })();
    return () => {
      live = false;
    };
    // `target` is rebuilt every render from the row; `targetKey` is the value
    // this effect actually depends on.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client, company, targetKey, reloadKey]);

  useEffect(() => {
    if (focus && state.kind !== "loading")
      root.current?.scrollIntoView({ block: "nearest" });
  }, [focus, state.kind]);

  const apply = useCallback(
    async (patch: ToolPolicyPatch) => {
      if (!target) return;
      const key = targetKey;
      setBusy(true);
      setWriteError(null);
      try {
        const doc = await writeToolPolicy(client, company, target, patch);
        if (targetKeyRef.current === key) setState({ kind: "ready", doc });
      } catch (err) {
        if (targetKeyRef.current === key) setWriteError(message(err));
      } finally {
        setBusy(false);
      }
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [client, company, targetKey],
  );

  const reset = useCallback(async () => {
    if (!target) return;
    const key = targetKey;
    setBusy(true);
    setWriteError(null);
    try {
      const doc = await resetToolPolicy(client, company, target);
      if (targetKeyRef.current === key) setState({ kind: "ready", doc });
    } catch (err) {
      if (targetKeyRef.current === key) setWriteError(message(err));
    } finally {
      setBusy(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client, company, targetKey]);

  return (
    <div className="space-y-3" data-testid="mcp-tool-permissions" ref={root}>
      <div className="space-y-0.5">
        <p className="text-sm font-medium">Tool permissions</p>
        <p className="text-xs text-muted-foreground">
          Suggested tiers below are a starting guess, not enforced — set your
          own to override. &ldquo;Block&rdquo; refuses the call outright: no
          approver can wave it through.
        </p>
      </div>

      {state.kind === "loading" && (
        <p className="flex items-center gap-1 text-xs text-muted-foreground">
          <Loader2 className="size-3 animate-spin" /> Reading this server&apos;s
          permissions…
        </p>
      )}

      {state.kind === "failed" && (
        <p
          className="text-xs text-destructive"
          data-testid="mcp-permissions-failed"
        >
          {state.message}
        </p>
      )}

      {state.kind === "unreadable" && (
        <div className="space-y-2" data-testid="mcp-permissions-unreadable">
          <p className="text-xs text-destructive">{state.message}</p>
          {canManage && (
            <Button
              size="sm"
              variant="outline"
              disabled={busy}
              onClick={() => void reset()}
              data-testid="mcp-permissions-clear"
            >
              {busy ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                "Clear stored permissions"
              )}
            </Button>
          )}
        </div>
      )}

      {state.kind === "ready" && (
        <>
          {state.doc.tools.length === 0 && (
            <p
              className="text-xs text-muted-foreground"
              data-testid="mcp-permissions-empty"
            >
              No tools are listed for this server yet. A tier default below
              still applies to every tool in it; re-check the server to list
              what it actually has.
            </p>
          )}

          <div className="space-y-3">
            {SECTION_ORDER.map((tier) => (
              <TierSection
                key={tier}
                tier={tier}
                server={server}
                rows={state.doc.tools.filter(
                  (row) => row.effectiveTier === tier,
                )}
                bulk={state.doc.tierDefaults[tier]}
                canManage={canManage}
                busy={busy}
                open={opened[tier] ?? tier === openByDefault}
                showAll={showAll[tier] === true}
                onToggleOpen={() =>
                  setOpened((was) => ({
                    ...was,
                    [tier]: !(was[tier] ?? tier === openByDefault),
                  }))
                }
                onToggleShowAll={() =>
                  setShowAll((was) => ({
                    ...was,
                    [tier]: !(was[tier] ?? false),
                  }))
                }
                apply={apply}
              />
            ))}
          </div>

          {writeError && (
            <p
              className="text-xs text-destructive"
              data-testid="mcp-permissions-write-error"
            >
              {writeError}
            </p>
          )}
        </>
      )}
    </div>
  );
}

/**
 * Which rows a section shows.
 *
 * The cap is applied to the unremarkable rows only. A row an operator decided,
 * or one the transport will never send, is shown whatever the cap says —
 * otherwise blocking the twelfth read-only tool hides that decision behind
 * "8 more" and the section reads as though it was never made.
 */
function visibleRows(
  server: McpServer,
  rows: ToolPolicyRow[],
  showAll: boolean,
): { shown: ToolPolicyRow[]; hidden: number } {
  if (showAll) return { shown: rows, hidden: 0 };
  const pinned = (row: ToolPolicyRow) =>
    row.isOverride || exclusion(server, row.tool) !== null;
  let budget = VISIBLE_CAP;
  const shown = rows.filter((row) => {
    if (pinned(row)) return true;
    if (budget === 0) return false;
    budget -= 1;
    return true;
  });
  return { shown, hidden: rows.length - shown.length };
}

function TierSection({
  tier,
  server,
  rows,
  bulk,
  canManage,
  busy,
  open,
  showAll,
  onToggleOpen,
  onToggleShowAll,
  apply,
}: {
  tier: ToolTier;
  server: McpServer;
  rows: ToolPolicyRow[];
  bulk: { mode: ApprovalMode; stored: boolean };
  canManage: boolean;
  busy: boolean;
  open: boolean;
  showAll: boolean;
  onToggleOpen: () => void;
  onToggleShowAll: () => void;
  apply: (patch: ToolPolicyPatch) => void;
}) {
  const bodyId = `tier-body-${tier}`;
  const { shown, hidden } = visibleRows(server, rows, showAll);

  return (
    <section
      className="space-y-2 rounded-md border border-border p-2"
      data-testid={`mcp-tier-section-${tier}`}
    >
      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          onClick={onToggleOpen}
          aria-expanded={open}
          aria-controls={bodyId}
          className="flex flex-1 items-center gap-1 text-left text-xs font-medium"
          data-testid={`mcp-tier-toggle-${tier}`}
        >
          {open ? (
            <ChevronDown className="size-3.5" />
          ) : (
            <ChevronRight className="size-3.5" />
          )}
          {SECTION_TITLES[tier]}
          <span
            className="text-muted-foreground"
            data-testid={`mcp-tier-count-${tier}`}
          >
            ({rows.length})
          </span>
        </button>
        <Select
          value={bulk.stored ? bulk.mode : UNSET}
          onValueChange={(v) => v && apply(tierPatch(tier, v))}
          items={TIER_DEFAULT_LABELS}
          disabled={!canManage || busy}
        >
          <SelectTrigger
            id={`tier-${tier}`}
            aria-label={`Default for ${SECTION_TITLES[tier].toLowerCase()}`}
            className="w-40"
          >
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={UNSET}>Not set</SelectItem>
            {(Object.keys(MODE_LABELS) as ApprovalMode[]).map((mode) => (
              <SelectItem key={mode} value={mode}>
                {MODE_LABELS[mode]}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
      </div>

      {open && (
        <div id={bodyId} className="space-y-2">
          {rows.length === 0 ? (
            <p className="text-xs text-muted-foreground">
              No tool here yet. The default above still applies to any this
              server turns out to have.
            </p>
          ) : (
            <ul className="space-y-2">
              {shown.map((row) => (
                <ToolRow
                  key={row.tool}
                  row={row}
                  server={server}
                  canManage={canManage}
                  busy={busy}
                  apply={apply}
                />
              ))}
            </ul>
          )}
          {(hidden > 0 || showAll) && rows.length > 0 && (
            <button
              type="button"
              onClick={onToggleShowAll}
              className="text-xs text-muted-foreground underline"
              data-testid={`mcp-tier-more-${tier}`}
            >
              {showAll ? "Show fewer" : `${hidden} more`}
            </button>
          )}
        </div>
      )}
    </section>
  );
}

function ToolRow({
  row,
  server,
  canManage,
  busy,
  apply,
}: {
  row: ToolPolicyRow;
  server: McpServer;
  canManage: boolean;
  busy: boolean;
  apply: (patch: ToolPolicyPatch) => void;
}) {
  const note = exclusion(server, row.tool);

  return (
    <li className="space-y-1" data-testid="mcp-permission-row">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-mono text-xs">{row.tool}</span>
        {row.suggestedTier && (
          <span className="text-3xs text-muted-foreground">
            suggested: {SUGGESTION_LABELS[row.suggestedTier]}
          </span>
        )}
        {note && (
          <Badge variant="outline" className="text-3xs text-muted-foreground">
            {note}
          </Badge>
        )}
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <ModeChoice
          value={row.mode}
          label={`What happens when ${row.tool} is called`}
          disabled={!canManage}
          onChange={(mode) => {
            if (!busy) apply({ tools: [{ tool: row.tool, mode }] });
          }}
        />
        <Select
          value={row.effectiveTier}
          onValueChange={(v) =>
            v && apply({ tools: [{ tool: row.tool, tier: v as ToolTier }] })
          }
          items={TIER_LABELS}
          disabled={!canManage || busy}
        >
          <SelectTrigger
            aria-label={`Tier for ${row.tool}`}
            className="h-7 w-36"
          >
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {TIERS.map((tier) => (
              <SelectItem key={tier} value={tier}>
                {TIER_LABELS[tier]}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>
        {row.isOverride && canManage && (
          <Button
            size="sm"
            variant="ghost"
            disabled={busy}
            aria-label={`Clear the decision on ${row.tool}`}
            data-testid="mcp-permission-clear-row"
            onClick={() => apply({ tools: [{ tool: row.tool }] })}
          >
            <RotateCcw className="size-3.5" />
          </Button>
        )}
      </div>
    </li>
  );
}
