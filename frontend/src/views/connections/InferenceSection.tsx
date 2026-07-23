import { useCallback, useEffect, useState } from "react";
import { BrainCircuit, Check, Loader2, RotateCcw, Save, Zap } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  getInferenceStatus,
  revertInference,
  setInference,
  testInference,
  type InferenceProvider,
  type InferenceStatus,
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
 * back. A switch takes effect on the agents' next turn with no restart.
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
    setBusy("save");
    try {
      // "Managed" means "use the platform default" — that's a revert, not a
      // runtime override with an empty credential.
      if (provider === "managed") {
        await revertInference(client, company);
      } else {
        const cleanModels = Object.fromEntries(
          Object.entries(models)
            .map(([t, v]) => [t, (v ?? "").trim()])
            .filter(([, v]) => v.length > 0),
        );
        await setInference(client, company, {
          provider,
          baseUrl: baseUrl.trim() || undefined,
          models: Object.keys(cleanModels).length ? cleanModels : undefined,
          key: key.trim() || undefined,
        });
      }
      toast.success("Inference updated. Agents use it on their next turn.");
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
        never shown again. A switch takes effect on the next turn, no restart.
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

              {provider !== "managed" && (
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
                <Button disabled={busy !== null} onClick={() => void save()}>
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
