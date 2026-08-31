/**
 * Choosing where this company's memory lives.
 *
 * The panel this replaces was read-only by design, on the grounds that engine
 * selection belonged to whoever controls the process environment. That is
 * still true of a *hosted* tenant — the control plane injects
 * `OPENCOMPANY_MEMORY*` and the host refuses a write that would be silently
 * ignored — and this section renders exactly the old read-only panel in that
 * case, saying who owns the choice. What changed is the self-hosted operator,
 * for whom "edit a unit file and restart" was the only way to try Supermemory
 * or mem0: the host now persists the choice to `config.toml` and rebinds it
 * live, so the picker below is the whole flow.
 *
 * ## What is asserted, and what is only saved
 *
 * A hosted engine is probed before it is bound, and a probe that fails refuses
 * the change rather than swapping a working engine for a dead one. So the
 * green dot after an apply means the engine answered, not that a file was
 * written.
 */

import { useEffect, useState } from "react";
import { AlertTriangle, Check, Loader2 } from "lucide-react";
import { toast } from "sonner";

import {
  applyMemoryEngine,
  memoryEngine,
  testMemoryEngine,
  type EngineOption,
  type MemoryEngineState,
} from "@/api/memory";
import type { OpenCompanyClient } from "@/api/client";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";

import { EngineMark } from "./EngineMark";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** Called after an engine change lands, so the page can re-read memory. */
  onApplied?: (state: MemoryEngineState) => void;
}

/** The probe line under the picker. */
function healthLabel(healthy: boolean | undefined): string {
  if (healthy === true) return "answering";
  if (healthy === false) return "not answering — check the endpoint and key";
  return "no probe — this engine has no health check to ask";
}

export function EngineSection({ client, company, onApplied }: Props) {
  const [state, setState] = useState<MemoryEngineState | null>(null);
  const [error, setError] = useState<string | null>(null);
  // What the operator has picked but not yet applied. `null` means "showing
  // what is saved", which is why this is not initialised from `state`.
  const [draft, setDraft] = useState<string | null>(null);
  const [url, setUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [busy, setBusy] = useState<"testing" | "applying" | null>(null);

  useEffect(() => {
    let live = true;
    setState(null);
    setDraft(null);
    memoryEngine(client, company)
      .then((next) => {
        if (!live) return;
        setState(next);
        setUrl(next.url ?? "");
        setError(null);
      })
      .catch((e: unknown) => {
        if (!live) return;
        setError(e instanceof Error ? e.message : "could not read the memory engine");
      });
    return () => {
      live = false;
    };
  }, [client, company]);

  if (error) {
    return (
      <Alert variant="destructive">
        <AlertDescription>{error}</AlertDescription>
      </Alert>
    );
  }
  if (!state) return <Skeleton className="h-40 rounded-xl" />;

  const chosen = draft ?? state.selected;
  const option = state.options.find((o) => o.id === chosen);
  const dirty =
    chosen !== state.selected ||
    (option?.requiresUrl && url.trim() !== (state.url ?? "")) ||
    apiKey.trim().length > 0;

  /** The body a test or an apply sends. */
  function choice() {
    return {
      engine: chosen,
      url: option?.requiresUrl ? url.trim() : undefined,
      // Omitted rather than sent empty: the host reads absence as "keep the
      // stored credential" and an empty string as "clear it", and a console
      // that always sent a value would wipe a key on an endpoint edit.
      apiKey: apiKey.trim() ? apiKey.trim() : undefined,
    };
  }

  async function test() {
    setBusy("testing");
    try {
      const probe = await testMemoryEngine(client, company, choice());
      if (probe.healthy) {
        toast.success(
          probe.capabilities.length
            ? `${option?.label ?? chosen} answered · ${probe.capabilities.join(", ")}`
            : `${option?.label ?? chosen} answered`,
        );
      } else {
        toast.error(probe.detail ?? "the engine did not answer");
      }
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not reach the engine");
    } finally {
      setBusy(null);
    }
  }

  async function apply() {
    setBusy("applying");
    try {
      const applied = await applyMemoryEngine(client, company, choice());
      setState(applied.engineState);
      setDraft(null);
      setApiKey("");
      setUrl(applied.engineState.url ?? "");
      if (applied.restartRequiredFor.length > 0) {
        // Named, never assumed: these companies are still running on the
        // previous engine, and saying "restart required" without saying which
        // would leave an operator unsure whether the change took at all.
        toast.warning(
          `Saved, but ${applied.restartRequiredFor.join(", ")} could not be rebuilt — restart the host for ${applied.restartRequiredFor.length > 1 ? "them" : "it"}.`,
        );
      } else {
        toast.success(`Memory is now on ${applied.engine}.`);
      }
      onApplied?.(applied.engineState);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not change the memory engine");
    } finally {
      setBusy(null);
    }
  }

  return (
    <Card data-testid="memory-engine-panel">
      <CardHeader>
        <CardTitle className="text-base">Memory engine</CardTitle>
        <CardDescription>
          {state.editable ? (
            <>
              Where this company&rsquo;s memory is stored. Instance-wide — every company on this
              host shares it — and saved to{" "}
              <code
                className="inline-block max-w-56 truncate align-bottom text-xs"
                data-visual-volatile
                title={state.configPath}
              >
                {state.configPath}
              </code>
              .
            </>
          ) : (
            <>
              Set by this deployment&rsquo;s environment (
              <code className="text-xs">OPENCOMPANY_MEMORY*</code>), which outranks the console.
              Shown here so you can see what is bound.
            </>
          )}
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex flex-wrap items-center gap-x-6 gap-y-1 text-sm">
          <span className="inline-flex items-center gap-1.5" data-testid="memory-engine-health">
            <span
              className={cn(
                "size-2 rounded-full",
                // The null engine never goes green: it answers every probe and
                // retains nothing, and a green dot beside a write button would
                // say the opposite of what is true.
                state.active === "null"
                  ? "bg-status-blocked"
                  : state.healthy === true
                    ? "bg-status-done"
                    : state.healthy === false
                      ? "bg-status-failed"
                      : "bg-muted-foreground/40",
              )}
            />
            <span className="font-medium">{state.active}</span>
            <span className="text-muted-foreground">· {healthLabel(state.healthy)}</span>
          </span>
          {state.capabilities.length > 0 && (
            <span className="flex flex-wrap items-center gap-1.5">
              <span className="text-muted-foreground">Capabilities</span>
              {state.capabilities.map((family) => (
                <Badge key={family} variant="outline" className="text-xs">
                  {family}
                </Badge>
              ))}
            </span>
          )}
        </div>

        {state.active === "null" && (
          <Alert variant="destructive" data-testid="memory-engine-discard">
            <AlertDescription>
              This engine accepts and discards every write — nothing this company is told will be
              remembered, and every read comes back empty, indistinguishable from a company that
              simply hasn&rsquo;t learned anything yet.
            </AlertDescription>
          </Alert>
        )}

        <div className="grid gap-2 sm:grid-cols-2 lg:grid-cols-3">
          {state.options.map((o) => (
            <EngineTile
              key={o.id}
              option={o}
              selected={chosen === o.id}
              active={state.active === o.id}
              disabled={!state.editable || !o.available}
              onPick={() => {
                setDraft(o.id);
                if (o.id === state.selected) setUrl(state.url ?? "");
              }}
            />
          ))}
        </div>

        {state.editable && option && (option.requiresUrl || option.requiresKey) && (
          <div className="grid gap-3 sm:grid-cols-2">
            {option.requiresUrl && (
              <div className="space-y-1.5">
                <Label htmlFor="memory-engine-url">Endpoint</Label>
                <Input
                  id="memory-engine-url"
                  value={url}
                  onChange={(e) => setUrl(e.target.value)}
                  placeholder="https://api.example.com"
                  autoComplete="off"
                />
              </div>
            )}
            {option.requiresKey && (
              <div className="space-y-1.5">
                <Label htmlFor="memory-engine-key">API key</Label>
                <Input
                  id="memory-engine-key"
                  type="password"
                  value={apiKey}
                  onChange={(e) => setApiKey(e.target.value)}
                  // The stored key never comes back from the host, so the
                  // field cannot be pre-filled — and saying so beats an empty
                  // box that looks like a key nobody set.
                  placeholder={
                    state.apiKeySet && chosen === state.selected
                      ? "stored — leave blank to keep it"
                      : "paste the key"
                  }
                  autoComplete="off"
                />
              </div>
            )}
          </div>
        )}

        {state.editable && (
          <div className="flex flex-wrap items-center gap-2">
            <Button onClick={() => void apply()} disabled={!dirty || busy !== null}>
              {busy === "applying" ? (
                <Loader2 className="size-4 animate-spin" />
              ) : (
                <Check className="size-4" />
              )}
              Use this engine
            </Button>
            {(option?.requiresUrl || option?.requiresKey) && (
              <Button variant="outline" onClick={() => void test()} disabled={busy !== null}>
                {busy === "testing" && <Loader2 className="size-4 animate-spin" />}
                Test connection
              </Button>
            )}
            {dirty && (
              <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
                <AlertTriangle className="size-3.5" />
                Switching engines starts empty — nothing is migrated from the current one.
              </span>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

/** One selectable engine. */
function EngineTile({
  option,
  selected,
  active,
  disabled,
  onPick,
}: {
  option: EngineOption;
  selected: boolean;
  /** Whether this engine is the one currently bound. */
  active: boolean;
  disabled: boolean;
  onPick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onPick}
      disabled={disabled}
      // The reason a disabled tile is disabled, on the element that still takes
      // a hover.
      title={option.available ? undefined : option.unavailableReason}
      data-testid={`engine-tile-${option.id}`}
      className={cn(
        "flex items-start gap-3 rounded-xl border p-3 text-left transition-colors",
        selected ? "border-primary bg-primary/5" : "hover:bg-muted/50",
        disabled && "cursor-not-allowed opacity-50",
      )}
    >
      <EngineMark engine={option.id} className="shrink-0" />
      <span className="min-w-0 space-y-1">
        <span className="flex flex-wrap items-center gap-1.5">
          <span className="font-medium">{option.label}</span>
          {active && (
            <Badge variant="outline" className="text-3xs">
              in use
            </Badge>
          )}
          {!option.durable && (
            <Badge variant="outline" className="border-status-blocked/40 text-3xs">
              keeps nothing
            </Badge>
          )}
        </span>
        <span className="block text-xs leading-snug text-muted-foreground">
          {option.available ? option.description : option.unavailableReason}
        </span>
      </span>
    </button>
  );
}
