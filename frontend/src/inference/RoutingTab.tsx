import { useState } from "react";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { checkOutcome } from "./classify";
import type { TestState } from "./classify";
import { cn } from "@/lib/utils";
import { SectionUnreachable } from "@/views/connections/SectionUnreachable";
import { ModelField } from "./ModelField";
import { WorkloadModelDialog } from "./WorkloadModelDialog";
import {
  ADVANCED_INTRO,
  MODE_COPY,
  OWN_MODE_EMPTY,
  OWN_MODE_SCOPE,
  WORKLOADS,
  WORKLOAD_COPY,
  WORKLOAD_TIER,
  MANAGED_NOT_SET_UP_ELSEWHERE,
  applyToEveryWorkload,
  managedModeBadge,
  formatRef,
  modelAfterProviderChange,
  ownModeDraft,
  parseRef,
  routingTargets,
  rowValue,
} from "./routing";
import type { InferenceActions, InferenceState } from "./use-inference";
import type { ProviderRef, RoutingMap, RoutingMode, Workload } from "./types";

/**
 * Routing: which provider serves which workload.
 *
 * Three modes, and **the mode is inferred from the routes, never stored**. A
 * mode field would be a fifth thing that can disagree with the four routes, and
 * the routes are the truth.
 *
 * ## Four editable rows, not five
 *
 * The design this ports lists `coding` beside `agentic`. Here they are the same
 * abstract tier — `agentic-v1` — so a fifth editable row would write one tier's
 * route under two names, and setting one would silently change the other. That
 * is the inheritance bug the same design forbids, wearing a different hat.
 * `coding` gets a read-only row that says where it actually resolves, which is
 * more honest than either omitting it or making it editable.
 */
export function RoutingTab({
  client,
  company,
  state,
  actions,
  canManage,
}: {
  client: OpenCompanyClient;
  company: string | null;
  state: InferenceState;
  actions: InferenceActions;
  canManage: boolean;
}) {
  const [editing, setEditing] = useState<Workload | null>(null);
  const [testing, setTesting] = useState(false);
  /** The dialog's own check result, with a tone — success and failure looked identical as prose. */
  const [testResult, setTestResult] = useState<TestState>({ kind: "idle" });
  const [error, setError] = useState<string | null>(null);
  /** Which mode the operator has selected, when it differs from the inferred one. */
  const [chosenMode, setChosenMode] = useState<RoutingMode | null>(null);
  /**
   * The Own-mode form, **only once it has been edited**.
   *
   * `null` means "show what is saved", so the form hydrates from the routing
   * table rather than starting blank over a table that already names a provider
   * — the gap that made a correct save read as a lost one. An override rather
   * than an effect that copies the saved value into state: a refetch landing
   * mid-edit would otherwise overwrite what is being typed, which is the same
   * class of bug as stripping a model id mid-keystroke.
   */
  const [ownDraft, setOwnDraft] = useState<{ slug: string; model: string } | null>(null);

  if (state.load === "unavailable") return null;
  if (state.load === "loading") return <Skeleton className="h-64 rounded-xl" />;
  if (state.load === "error") {
    return <SectionUnreachable label="Couldn't read this company's routing" />;
  }

  const routing: RoutingMap = Object.fromEntries(
    WORKLOADS.map((w) => [w, parseRef(state.routes[WORKLOAD_TIER[w]] ?? "")]),
  ) as RoutingMap;
  const mode = chosenMode ?? state.mode;
  const targets = routingTargets(state.providers);
  const ownSaved = ownModeDraft(routing);
  const ownSlug = ownDraft?.slug ?? ownSaved.slug;
  const ownModel = ownDraft?.model ?? ownSaved.model;

  async function save(next: RoutingMap) {
    setError(null);
    const wire: Record<string, string> = {};
    for (const workload of WORKLOADS) {
      wire[WORKLOAD_TIER[workload]] = formatRef(next[workload] ?? { kind: "default" });
    }
    try {
      await actions.saveRoutes(wire);
      setChosenMode(null);
      // Back to following the saved table, so the form reads what was written
      // rather than a draft that now says the same thing by coincidence.
      setOwnDraft(null);
    } catch (err) {
      setError(err instanceof ApiError ? err.message : "That routing could not be saved.");
    }
  }

  return (
    <div className="space-y-4">
      <Card>
        <CardContent className="space-y-3">
          {/* The page's one h1 is its title; this section and the Providers tab's
              are peers under it, not one nested in the other. */}
          <h2 className="text-sm font-medium">Routing mode</h2>
          {/* Here the navigation is not circular: the action lives on the other
              tab, and this is the page where "Managed is always available as a
              fallback" would otherwise be read as true. */}
          {state.status?.managed?.configured === false && (
            <p className="text-xs text-muted-foreground" data-testid="inference-managed-not-set-up">
              {MANAGED_NOT_SET_UP_ELSEWHERE}
            </p>
          )}
          {(["managed", "own", "advanced"] as const).map((option) => (
            <ModeRow
              key={option}
              option={option}
              selected={mode === option}
              managedConfigured={state.status?.managed?.configured}
              disabled={!canManage}
              onSelect={() => {
                setChosenMode(option);
                // Managed is a whole-table statement, so it saves on selection.
                // Own and Advanced need a choice before there is anything to
                // write.
                if (option === "managed") {
                  void save(
                    Object.fromEntries(
                      WORKLOADS.map((w) => [w, { kind: "managed" } as ProviderRef]),
                    ) as RoutingMap,
                  );
                }
              }}
            />
          ))}
        </CardContent>
      </Card>

      {mode === "own" && (
        <Card>
          <CardContent className="space-y-3">
            {targets.length === 0 ? (
              <p className="text-sm text-muted-foreground">{OWN_MODE_EMPTY}</p>
            ) : (
              <>
                <div className="grid gap-3 sm:grid-cols-2">
                  <div className="grid gap-1.5">
                    <Label htmlFor="inference-own-provider">Provider</Label>
                    <Select
                      value={ownSlug || null}
                      disabled={!canManage}
                      onValueChange={(v) => {
                        if (!v) return;
                        const slug = String(v);
                        // Same rule as the per-workload dialog: an id chosen at
                        // one provider is not a value at another.
                        setOwnDraft({
                          slug,
                          model: modelAfterProviderChange(ownModel, ownSlug, slug),
                        });
                      }}
                    >
                      <SelectTrigger id="inference-own-provider" className="w-full">
                        <SelectValue placeholder="Choose a provider…" />
                      </SelectTrigger>
                      <SelectContent>
                        {targets.map((p) => (
                          <SelectItem key={p.slug} value={p.slug}>
                            {p.label}
                          </SelectItem>
                        ))}
                      </SelectContent>
                    </Select>
                  </div>
                  <ModelField
                    client={client}
                    company={company}
                    slug={ownSlug || null}
                    id="inference-own-model"
                    value={ownModel}
                    disabled={!canManage}
                    onChange={(next) => setOwnDraft({ slug: ownSlug, model: next })}
                  />
                </div>
                <p className="text-xs text-muted-foreground">{OWN_MODE_SCOPE}</p>
                <Button
                  type="button"
                  disabled={!canManage || !ownSlug}
                  data-testid="inference-own-save"
                  onClick={() =>
                    void save(applyToEveryWorkload(ownSlug, ownModel.trim() || undefined))
                  }
                >
                  Save
                </Button>
              </>
            )}
          </CardContent>
        </Card>
      )}

      {mode === "advanced" && (
        <Card>
          <CardContent className="space-y-1">
            <p className="pb-2 text-xs text-muted-foreground">{ADVANCED_INTRO}</p>
            {WORKLOADS.map((workload) => (
              <WorkloadRow
                key={workload}
                workload={workload}
                ref_={routing[workload] ?? { kind: "default" }}
                state={state}
                canManage={canManage}
                onEdit={() => {
                  setTestResult({ kind: "idle" });
                  setEditing(workload);
                }}
              />
            ))}
            {/* Read-only, and present rather than omitted. Coding resolves
                through the agentic route; saying so is more honest than a row
                that is not there, and safer than one that can be set. */}
            <div
              className="flex flex-wrap items-center gap-x-3 gap-y-1 border-t border-border pt-3"
              data-testid="inference-workload-coding"
            >
              <span className="grid min-w-0 flex-1 leading-tight">
                <span className="truncate text-sm font-medium">Coding</span>
                <span className="truncate text-xs text-muted-foreground">
                  Code generation and refactor passes
                </span>
              </span>
              <span className="text-xs text-muted-foreground">
                Follows Agentic — one tier, two names
              </span>
            </div>
          </CardContent>
        </Card>
      )}

      {state.orphaned.length > 0 && (
        <p className="text-xs text-status-blocked-text" data-testid="inference-orphaned-routes">
          {state.orphaned
            .map(([tier, slug]) => `${tier} names ${slug}, which this company has no provider for.`)
            .join(" ")}
        </p>
      )}
      {/* The form's own failure stays in the form, where the thing to correct
          is. Everything else a write says is a toast — see `write`. */}
      {error && <p className="text-xs text-status-blocked-text">{error}</p>}

      <WorkloadModelDialog
        client={client}
        company={company}
        workload={editing}
        providers={state.providers}
        current={editing ? (routing[editing] ?? { kind: "default" }) : { kind: "default" }}
        testing={testing}
        testResult={testResult}
        onTest={(ref) => {
          const slug = ref.kind === "cloud" ? ref.providerSlug : null;
          if (!slug) return;
          setTesting(true);
          setTestResult({ kind: "testing" });
          // The chosen model goes with the request, so the check answers "will
          // this row work" rather than "is this endpoint up" — see
          // `checkOutcome`.
          void actions
            .test(slug, ref.kind === "cloud" ? ref.model : undefined)
            .then((result) => setTestResult({ kind: "done", ...checkOutcome(result) }))
            .catch(() =>
              setTestResult({ kind: "done", ok: false, message: "The check did not complete." }),
            )
            .finally(() => setTesting(false));
        }}
        onCancel={() => setEditing(null)}
        onApply={(ref) => {
          const next = { ...routing, [editing as Workload]: ref };
          setEditing(null);
          void save(next);
        }}
      />
    </div>
  );
}

/**
 * One mode.
 *
 * Managed carries a **badge, not a disabled toggle**. A locked switch reads as
 * switchable-but-broken and invites a fight the operator cannot win.
 */
function ModeRow({
  option,
  selected,
  managedConfigured,
  disabled,
  onSelect,
}: {
  option: RoutingMode;
  selected: boolean;
  /** Whether the managed chain resolves — only the managed row reads it. */
  managedConfigured: boolean | undefined;
  disabled: boolean;
  onSelect: () => void;
}) {
  const copy = MODE_COPY[option];
  return (
    <button
      type="button"
      disabled={disabled}
      aria-pressed={selected}
      data-testid={`inference-mode-${option}`}
      onClick={onSelect}
      className={cn(
        "flex w-full items-start gap-3 rounded-md border px-3 py-2 text-left",
        selected ? "border-primary bg-accent" : "border-border",
        disabled && "opacity-60",
      )}
    >
      <span className="grid min-w-0 flex-1 gap-0.5">
        <span className="text-sm font-medium">{copy.label}</span>
        <span className="text-xs text-muted-foreground">{copy.description}</span>
      </span>
      {option === "managed" && (
        <Badge
          variant="outline"
          className={cn(managedConfigured && "border-status-done text-status-done-text")}
          data-testid="inference-mode-managed-state"
        >
          {managedModeBadge(managedConfigured)}
        </Badge>
      )}
    </button>
  );
}

function WorkloadRow({
  workload,
  ref_,
  state,
  canManage,
  onEdit,
}: {
  workload: Workload;
  ref_: ProviderRef;
  state: InferenceState;
  canManage: boolean;
  onEdit: () => void;
}) {
  const copy = WORKLOAD_COPY[workload];
  // `rowValue` decides both the text and the button's word; this file decides
  // neither. An unset row names the primary it will actually resolve through.
  const { value, action } = rowValue(ref_, state.providers);
  return (
    <div
      className="flex flex-wrap items-center gap-x-3 gap-y-1 border-t border-border py-3 first:border-t-0"
      data-testid={`inference-workload-${workload}`}
    >
      <span className="grid min-w-0 flex-1 leading-tight">
        <span className="truncate text-sm font-medium">{copy.label}</span>
        <span className="truncate text-xs text-muted-foreground">{copy.description}</span>
      </span>
      <span className="truncate text-xs text-muted-foreground">{value}</span>
      <Button type="button" variant="outline" size="sm" disabled={!canManage} onClick={onEdit}>
        {action}
      </Button>
    </div>
  );
}
