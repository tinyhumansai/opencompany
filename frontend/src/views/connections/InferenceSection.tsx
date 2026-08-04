import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, BrainCircuit, Check, Loader2, RotateCcw, Save, Zap } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  getInferenceStatus,
  revertInference,
  setInference,
  testInference,
  type InferenceMutation,
  type InferenceProvider,
  type InferenceStatus,
  type UsageMetering,
} from "@/api/inference";
import { ApiError } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";

/** The abstract cognition tiers the tenant model table maps. */
const TIERS = ["chat-v1", "reasoning-v1", "agentic-v1", "vision-v1"] as const;
type Tier = (typeof TIERS)[number];

const PROVIDER_LABELS: Record<InferenceProvider, string> = {
  managed: "Managed (TinyHumans)",
  openrouter: "OpenRouter",
  ollama: "Ollama (local)",
  openai_compatible: "Custom (OpenAI-compatible)",
};

/**
 * What the live cognition path's metering mode means for the Usage view — so a
 * zero token/cost reading is legible instead of alarming (issue #174).
 */
const METERING_NOTES: Record<UsageMetering, string> = {
  perTurn: "usage metered per turn",
  perCycle: "usage metered per cycle, from what the provider reports",
  none: "no model runs on this path, so Usage stays at zero",
};

/** Per-provider form defaults applied when the operator picks a provider. */
function presetFor(provider: InferenceProvider): {
  baseUrl: string;
  models: Partial<Record<Tier, string>>;
} {
  switch (provider) {
    case "openrouter":
      return {
        baseUrl: "",
        // OpenRouter's recommended DeepSeek pairing, prefilled.
        models: { "chat-v1": "deepseek/deepseek-chat", "reasoning-v1": "deepseek/deepseek-r1" },
      };
    case "ollama":
      return { baseUrl: "http://localhost:11434/v1", models: { "chat-v1": "llama3.1" } };
    case "openai_compatible":
      return { baseUrl: "", models: {} };
    case "managed":
      return { baseUrl: "", models: {} };
  }
}

type Load = "loading" | "ready" | "unavailable";
type TestState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ok"; note: string }
  | { kind: "error"; message: string };

/**
 * Bring-Your-Own-Key inference (issue #56). Shows the company's effective
 * provider (with a source badge + tier→model rows + a "key set" indicator), a
 * live "Test" probe, and a switch form with per-provider presets. The key input
 * is **write-only** — it is sent on Save, stored server-side, and never read
 * back.
 *
 * A switch takes effect on the agents' next turn with no restart — *except* the
 * not-configured → configured transition, where the running brain was already
 * chosen without one. The host reports that as `restartRequired`, and this
 * section says so in the toast and keeps saying it in the status card until the
 * restart happens (issue #266).
 */
export function InferenceSection({
  client,
  company,
}: {
  client: OpenCompanyClient;
  company: string | null;
}) {
  const [load, setLoad] = useState<Load>("loading");
  const [status, setStatus] = useState<InferenceStatus | null>(null);
  const [busy, setBusy] = useState<"save" | "reset" | "test" | null>(null);
  const [test, setTest] = useState<TestState>({ kind: "idle" });

  // Switch form.
  const [provider, setProvider] = useState<InferenceProvider>("managed");
  const [baseUrl, setBaseUrl] = useState("");
  const [models, setModels] = useState<Partial<Record<Tier, string>>>({});
  const [key, setKey] = useState("");

  const refresh = useCallback(async () => {
    try {
      setStatus(await getInferenceStatus(client, company));
      setLoad("ready");
    } catch {
      setLoad("unavailable");
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void refresh();
  }, [refresh]);

  function pickProvider(next: InferenceProvider) {
    setProvider(next);
    const preset = presetFor(next);
    setBaseUrl(preset.baseUrl);
    setModels(preset.models);
    setTest({ kind: "idle" });
  }

  function setModel(tier: Tier, value: string) {
    setModels((m) => ({ ...m, [tier]: value }));
  }

  async function save() {
    if (busy) return;
    // Managed is a revert, and a revert cannot carry a credential — so a key
    // typed under a different provider and left behind by a switch would be
    // dropped by the save below while the toast claimed success (issue #265).
    // Refuse instead: a save that reports success must never have discarded
    // what the operator typed. The Save button is disabled in this state; this
    // is the guard that makes the invariant hold regardless of the button.
    if (provider === "managed" && key.trim()) {
      toast.error(
        "Managed uses the platform credential and can't store a key. Choose OpenRouter or Custom (OpenAI-compatible) to save it, or discard it first.",
      );
      return;
    }
    setBusy("save");
    try {
      // "Managed" means "use the platform default" — that's a revert, not a
      // runtime override with an empty credential.
      let result: InferenceMutation;
      if (provider === "managed") {
        result = await revertInference(client, company);
      } else {
        const cleanModels = Object.fromEntries(
          Object.entries(models)
            .map(([t, v]) => [t, (v ?? "").trim()])
            .filter(([, v]) => v.length > 0),
        );
        result = await setInference(client, company, {
          provider,
          baseUrl: baseUrl.trim() || undefined,
          models: Object.keys(cleanModels).length ? cleanModels : undefined,
          key: key.trim() || undefined,
        });
      }
      // Issue #266: only the host knows whether the *running* brain can act on
      // what was just saved. Which brain a company runs is fixed when it is
      // built, so a company that started with no inference source keeps echoing
      // no matter what lands here — "agents use it on their next turn" was a
      // promise the runtime could not keep for exactly the transition an
      // operator makes first. Follow the response instead of asserting.
      if (result.status.restartRequired) {
        toast.warning("Inference saved — restart the company for agents to use it.", {
          description: result.note,
        });
      } else {
        toast.success("Inference updated. Agents use it on their next turn.");
      }
      setKey("");
      setTest({ kind: "idle" });
      await refresh();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't update inference.");
    } finally {
      setBusy(null);
    }
  }

  async function reset() {
    if (busy) return;
    setBusy("reset");
    try {
      await revertInference(client, company);
      toast.success("Reverted to the managed configuration.");
      pickProvider("managed");
      setKey("");
      await refresh();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Couldn't revert inference.");
    } finally {
      setBusy(null);
    }
  }

  async function probe() {
    if (busy) return;
    setBusy("test");
    setTest({ kind: "loading" });
    try {
      const result = await testInference(client, company);
      if (result.ok) {
        setTest({ kind: "ok", note: result.note ?? "Reached the provider." });
      } else {
        setTest({ kind: "error", message: result.error ?? "The provider did not respond." });
      }
    } catch (err) {
      setTest({
        kind: "error",
        message: err instanceof ApiError ? err.message : "The probe failed.",
      });
    } finally {
      setBusy(null);
    }
  }

  if (load === "unavailable") return null;

  const modelRows = status ? Object.entries(status.models) : [];
  // A credential typed under a BYOK provider survives a switch to managed (the
  // input is hidden, the state is not). Managed has nowhere to put it, so this
  // is the one combination that would silently lose operator input on save.
  const managedWouldDiscardKey = provider === "managed" && key.trim().length > 0;

  return (
    <section className="space-y-3">
      <div className="flex items-center gap-2">
        <BrainCircuit className="size-4 text-muted-foreground" />
        <h3 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          Inference (BYOK)
        </h3>
      </div>
      <p className="text-sm text-muted-foreground">
        Choose which model provider your agents think with. Bring your own key for OpenRouter, a
        custom OpenAI-compatible endpoint, or a local Ollama server — the key is stored securely and
        never shown again. Switching provider or model takes effect on the agents' next turn. Giving
        inference to a company that started without any does not: the brain is chosen at startup, so
        that first setup needs a restart.
      </p>

      {load === "loading" ? (
        <Skeleton className="h-40 rounded-xl" />
      ) : (
        <Card>
          <CardContent className="space-y-4 py-4">
            {/* Status card. */}
            {status && (
              <div className="space-y-2">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium">{PROVIDER_LABELS[status.provider as InferenceProvider] ?? status.provider}</span>
                  <Badge variant={status.source === "runtime" ? "outline" : "secondary"}>
                    {status.source}
                  </Badge>
                  {status.keyConfigured && (
                    <span className="inline-flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
                      <Check className="size-3" /> key set
                    </span>
                  )}
                  <Button
                    variant="ghost"
                    size="sm"
                    className="ml-auto"
                    disabled={busy !== null}
                    onClick={() => void probe()}
                  >
                    {busy === "test" ? <Loader2 className="size-4 animate-spin" /> : <Zap className="size-4" />}
                    Test
                  </Button>
                </div>
                <p className="truncate text-xs text-muted-foreground">{status.baseUrl}</p>
                {/* Issue #174: config resolving to a provider does not mean the
                    company booted onto it. Say which cognition path is live and
                    whether its usage is metered, so a zero Usage reading reads as
                    "nothing was spent" rather than "accounting is broken". */}
                <p className="text-xs text-muted-foreground">
                  Cognition: <span className="font-mono">{status.cognition}</span> ·{" "}
                  {METERING_NOTES[status.usageMetering] ?? "usage metering unknown"}
                </p>
                {/* Issue #266: a saved config the running brain cannot act on.
                    The toast that says so is gone in seconds — and an operator
                    who reloads the page, or comes back tomorrow, sees only a
                    correct-looking provider next to agents that still echo. This
                    is the surface that stays until the restart happens. */}
                {status.restartRequired && (
                  <div
                    className="flex items-start gap-1.5 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-600 dark:text-amber-400"
                    data-testid="inference-restart-required"
                  >
                    <RotateCcw className="mt-px size-3.5 shrink-0" />
                    <span>
                      <span className="font-medium">Restart required.</span> This company started
                      with no inference source, so it is running the offline echo brain and its
                      scheduled workflows cannot fire. The brain is chosen at startup — this
                      configuration is saved, but agents keep echoing until the company is
                      restarted.
                    </span>
                  </div>
                )}
                {modelRows.length > 0 && (
                  <ul className="space-y-1 rounded-md bg-muted/40 p-2">
                    {modelRows.map(([tier, model]) => (
                      <li key={tier} className="text-xs">
                        <span className="font-mono font-medium">{tier}</span>
                        <span className="text-muted-foreground"> → {model}</span>
                      </li>
                    ))}
                  </ul>
                )}
                {test.kind === "ok" && (
                  <p className="flex items-center gap-1 text-xs text-emerald-600 dark:text-emerald-400">
                    <Check className="size-3" /> {test.note}
                  </p>
                )}
                {test.kind === "error" && <p className="text-xs text-destructive">{test.message}</p>}
                {test.kind === "loading" && (
                  <p className="flex items-center gap-1 text-xs text-muted-foreground">
                    <Loader2 className="size-3 animate-spin" /> Probing the provider…
                  </p>
                )}
              </div>
            )}

            {/* Switch form. */}
            <div className="space-y-3 border-t border-border pt-3">
              <div className="grid gap-2 sm:grid-cols-2 sm:items-end">
                <div className="space-y-1">
                  <Label htmlFor="inference-provider" className="text-xs">
                    Provider
                  </Label>
                  <Select
                    value={provider}
                    onValueChange={(v) => pickProvider(v as InferenceProvider)}
                    items={PROVIDER_LABELS}
                  >
                    <SelectTrigger id="inference-provider" className="w-full">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {(Object.keys(PROVIDER_LABELS) as InferenceProvider[]).map((p) => (
                        <SelectItem key={p} value={p}>
                          {PROVIDER_LABELS[p]}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>
                {(provider === "ollama" || provider === "openai_compatible") && (
                  <div className="space-y-1">
                    <Label htmlFor="inference-base-url" className="text-xs">
                      Base URL
                    </Label>
                    <Input
                      id="inference-base-url"
                      value={baseUrl}
                      placeholder="https://host/v1"
                      onChange={(e) => setBaseUrl(e.target.value)}
                    />
                  </div>
                )}
              </div>

              {provider === "managed" ? (
                <div className="space-y-2">
                  <p className="text-xs text-muted-foreground" data-testid="inference-managed-note">
                    Managed runs on the platform credential, so there is no key to paste here. To
                    bring your own key, choose OpenRouter or Custom (OpenAI-compatible).
                  </p>
                  {managedWouldDiscardKey && (
                    <div
                      className="space-y-2 rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive"
                      data-testid="inference-key-conflict"
                    >
                      <p className="flex items-start gap-1.5">
                        <AlertTriangle className="mt-px size-3.5 shrink-0" />
                        <span>
                          You typed a key, and managed can&apos;t store one — saving now would throw
                          it away. Choose OpenRouter or Custom (OpenAI-compatible) to save it, or
                          discard it to stay on managed.
                        </span>
                      </p>
                      <Button
                        variant="outline"
                        size="sm"
                        data-testid="inference-discard-key"
                        onClick={() => setKey("")}
                      >
                        Discard key
                      </Button>
                    </div>
                  )}
                </div>
              ) : (
                <>
                  <div className="grid gap-2 sm:grid-cols-2">
                    {TIERS.map((tier) => (
                      <div key={tier} className="space-y-1">
                        <Label htmlFor={`inference-model-${tier}`} className="text-xs">
                          {tier}
                        </Label>
                        <Input
                          id={`inference-model-${tier}`}
                          value={models[tier] ?? ""}
                          placeholder="provider model id"
                          onChange={(e) => setModel(tier, e.target.value)}
                        />
                      </div>
                    ))}
                  </div>
                  {provider !== "ollama" && (
                    <div className="space-y-1">
                      <Label htmlFor="inference-key" className="text-xs">
                        API key {status?.keyConfigured ? "(leave blank to keep)" : ""}
                      </Label>
                      <Input
                        id="inference-key"
                        type="password"
                        value={key}
                        placeholder="write-only"
                        autoComplete="off"
                        onChange={(e) => setKey(e.target.value)}
                      />
                    </div>
                  )}
                </>
              )}

              <div className="flex items-center gap-2">
                <Button
                  data-testid="inference-save"
                  disabled={busy !== null || managedWouldDiscardKey}
                  onClick={() => void save()}
                >
                  {busy === "save" ? <Loader2 className="size-4 animate-spin" /> : <Save className="size-4" />}
                  Save
                </Button>
                <Button variant="outline" disabled={busy !== null} onClick={() => void reset()}>
                  {busy === "reset" ? <Loader2 className="size-4 animate-spin" /> : <RotateCcw className="size-4" />}
                  Reset to managed
                </Button>
              </div>
            </div>
          </CardContent>
        </Card>
      )}
    </section>
  );
}
