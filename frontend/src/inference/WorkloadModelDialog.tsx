import { useEffect, useState } from "react";

import type { OpenCompanyClient } from "@/api/client";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { testOutcome } from "./classify";
import type { TestState } from "./classify";
import { cn } from "@/lib/utils";
import { ModelField } from "./ModelField";
import { overrideIsSendable } from "./proxy-compat";
import {
  UNSET_TARGET,
  WORKLOAD_COPY,
  formatRef,
  modelAfterProviderChange,
  modelTarget,
  primaryLabel,
  refForTarget,
  routingOptions,
  targetForRef,
  targetLabel,
} from "./routing";
import type { Provider, ProviderRef, Workload } from "./types";

/**
 * Choosing what one workload runs on.
 *
 * The **only** place in this surface that tests with a real completion rather
 * than a catalog listing, and correctly so: the add flow asks "is this
 * reachable", a routing row asks "will this model actually answer". Those are
 * different questions and a catalog listing cannot answer the second.
 *
 * The dialog carries the workload's recommendation hint, because it is the part
 * that makes this screen usable by someone who has never chosen a model before.
 * It is the first thing a rewrite drops as verbose; it is not verbose, it is the
 * feature — and it is *here*, on the row it applies to, rather than in a
 * paragraph above the table.
 *
 * The model is free text. A select sourced from the provider's catalog would
 * make the only correct value unreachable at an Azure endpoint, where the
 * request keys on a **deployment name** while `/models` publishes base model
 * ids — and a typed id is honoured verbatim at every endpoint anyway.
 */
export function WorkloadModelDialog({
  client,
  company,
  workload,
  providers,
  current,
  testing,
  testResult,
  onTest,
  onCancel,
  onApply,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /** The workload being edited, or `null` when the dialog is closed. */
  workload: Workload | null;
  providers: readonly Provider[];
  current: ProviderRef;
  testing: boolean;
  /** What the last check said, with its tone. */
  testResult: TestState;
  onTest: (ref: ProviderRef) => void;
  onCancel: () => void;
  onApply: (ref: ProviderRef) => void;
}) {
  const [target, setTarget] = useState<string>(UNSET_TARGET);
  const [model, setModel] = useState("");

  useEffect(() => {
    if (!workload) return;
    setTarget(targetForRef(current));
    setModel(current.kind === "cloud" || current.kind === "local" ? (current.model ?? "") : "");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workload]);

  if (!workload) return null;
  const copy = WORKLOAD_COPY[workload];
  const options = routingOptions(providers);
  /** Whose catalog the model field reads, and `null` when the row takes no id. */
  const modelSlug = modelTarget(target, providers);
  const outcome = testOutcome(testResult);
  const ref: ProviderRef = refForTarget(target, model, providers);

  return (
    <Dialog open onOpenChange={(next) => !next && onCancel()}>
      <DialogContent className="sm:max-w-md" data-testid="inference-workload-dialog">
        <DialogHeader>
          <DialogTitle>{copy.label}</DialogTitle>
          <DialogDescription>{copy.description}</DialogDescription>
        </DialogHeader>

        <div className="grid gap-4">
          <p className="text-xs text-muted-foreground">Recommended: {copy.hint}</p>

          <div className="grid gap-1.5">
            <Label htmlFor="inference-workload-provider">Provider</Label>
            <Select
              value={target}
              onValueChange={(v) => {
                if (!v) return;
                const next = String(v);
                // A model id belongs to the provider it was chosen from — see
                // `modelAfterProviderChange`.
                setModel((current) => modelAfterProviderChange(current, target, next));
                setTarget(next);
              }}
            >
              <SelectTrigger id="inference-workload-provider" className="w-full">
                {/* The trigger renders the raw value unless told otherwise, and
                    these values are sentinels — `__unset__` is not a thing to
                    show an operator. The same trap `TaskEditDialog` documents
                    for a column id versus its label. */}
                <SelectValue>{() => targetLabel(target, providers)}</SelectValue>
              </SelectTrigger>
              <SelectContent>
                {/* Providers only. "Follow the default" is this row's unset
                    state, not a fourth provider — listing it here named the
                    primary twice, once under its own label and once as
                    `Primary (…)`, and the two behaved differently. */}
                {options.map((option) => (
                  <SelectItem key={option.slug} value={option.slug}>
                    {option.label}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            {/* Getting back to unset is an action. It clears the model with it:
                a model pins the row, so leaving one behind would put it straight
                back where it was. */}
            {target !== UNSET_TARGET && (
              <Button
                type="button"
                variant="link"
                size="sm"
                className="h-auto justify-self-start p-0 text-xs"
                data-testid="inference-workload-follow-default"
                onClick={() => {
                  setTarget(UNSET_TARGET);
                  setModel("");
                }}
              >
                Follow the default — {primaryLabel(providers)}
              </Button>
            )}
          </div>

          {/* Present or absent by the **kind of provider**, never by which
              synonym was chosen: `modelTarget` resolves an unset row to the
              provider it actually uses, so `Primary (OpenRouter)` and
              `OpenRouter` produce the same field. */}
          {modelSlug && (
            <ModelField
              client={client}
              company={company}
              slug={modelSlug}
              id="inference-workload-model"
              value={model}
              onChange={setModel}
            />
          )}

          {/* Said out loud rather than discovered after saving: the grammar has
              no "follow the default, but with this model" form, so choosing one
              is a decision to stop following it. */}
          {target === UNSET_TARGET && model.trim() && modelSlug && (
            <p className="text-xs text-muted-foreground" data-testid="inference-workload-pins">
              Choosing a model pins this workload to {targetLabel(modelSlug, providers)}. It will
              stay there if the company default moves.
            </p>
          )}

          {/* Judged on a SETTLED value, never on a keystroke: six of this
              rule's nine recorded regressions are about dropping a value while
              it was still being typed. It applies to the platform proxy alone —
              every other provider takes a typed id verbatim. */}
          {model.trim() && modelSlug && !overrideIsSendable(modelSlug, model) && (
            <p className="text-xs text-status-blocked-text" data-testid="inference-model-incompatible">
              The managed endpoint resolves tier names and its own
              <code className="px-1 font-mono">openrouter/author/model</code> form. This id would
              be rejected, so it will not be saved.
            </p>
          )}

          {/* A tone as well as words. It was grey prose either way, so a
              failure and a success read identically on the one control whose
              whole job is to tell them apart. Always mounted and polite, because
              a live region that appears with its text is frequently missed. */}
          <p
            aria-live="polite"
            className={cn(
              "text-xs",
              outcome?.tone === "ok" ? "text-status-done-text" : "text-status-blocked-text",
              !outcome && "text-muted-foreground",
            )}
            data-testid="inference-workload-test"
          >
            {testing ? "Checking…" : (outcome?.message ?? "")}
          </p>
        </div>

        <DialogFooter>
          <Button type="button" variant="outline" onClick={onCancel}>
            Cancel
          </Button>
          {/* Says what it does. It promised "one real completion, your provider
              may charge for it" and sent a catalogue read — so it cost nothing,
              charged nothing, and could not tell a bogus model id from a good
              one. The check is free and is now described as free; what it
              settles is reachability plus whether the endpoint publishes this
              id. */}
          <Button
            type="button"
            variant="outline"
            disabled={testing || ref.kind === "default" || ref.kind === "managed"}
            title="Reads this provider's model list. Free, and it does not send a turn."
            data-testid="inference-workload-test-button"
            onClick={() => onTest(ref)}
          >
            Test
          </Button>
          <Button
            type="button"
            data-testid="inference-workload-apply"
            onClick={() => onApply(ref)}
          >
            Save
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** The route string a ref writes, exported so the tab can compare drafts. */
export const refToString = formatRef;
