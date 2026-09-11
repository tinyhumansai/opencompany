import { useEffect, useRef, useState } from "react";
import { Plus } from "lucide-react";

import { ApiError } from "@/api/types";
import type { ProbeResult } from "@/api/inference";
import { TEST_RESULT_MS } from "./classify";
import type { TestState } from "./classify";
import type { ProbeClass } from "./types";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { SectionUnreachable } from "@/views/connections/SectionUnreachable";
import { AddProviderDialog } from "./AddProviderDialog";
import { ProviderConnectDialog } from "./ProviderConnectDialog";
import type { ConnectDraft } from "./ProviderConnectDialog";
import { MANAGED_SLUG, ProviderList } from "./ProviderList";
import { RemoveProviderDialog } from "./RemoveProviderDialog";
import type { RemovalIntent } from "./RemoveProviderDialog";
import { categoryOf } from "./catalogue";
import { MANAGED_OPTION_SLUG } from "./connect";
import { WORKLOADS, WORKLOAD_TIER, managedFallbackNote, parseRef, removalImpact } from "./routing";
import type { RoutingMap } from "./types";
import type { InferenceActions, InferenceState } from "./use-inference";
import type { Provider } from "./types";

/**
 * LLM Providers: what this company can reach a model through, and how to add one.
 *
 * Two cards and one line. The page this replaces opened with four paragraphs —
 * what bring-your-own-key means, what Test costs, what Reset does, what Remove
 * key does — above a form. Every one of them explained a control that was
 * visible while they were being read.
 *
 * What survives the cut is what an operator cannot infer from the control
 * itself: the restart notice, because a save that has landed and is not yet in
 * effect looks exactly like one that is, and the cost warning on a real
 * completion, which lives on the button it applies to rather than above the fold.
 */
/**
 * Drops the error envelope's own prefix from a message meant for a person.
 *
 * The host answers a refusal as `invalid request: <sentence>`, and the prefix is
 * machine vocabulary: it says which *kind* of error this is to a caller that
 * might branch on it, and says nothing at all to the operator standing in front
 * of the field they have to correct. The sentence after it is already written
 * for them.
 */
export function stripEnvelopePrefix(message: string): string {
  return message.replace(/^(invalid request|conflict|not found):\s*/i, "");
}

/**
 * Fires a write whose only failure surface is the toast `write` already raised.
 *
 * `void promise` on a rejecting call is an unhandled rejection, and these are
 * the row controls — a switch, a default marker, a restart — with no form open
 * to show an error in. Swallowing here is deliberate and narrow: the toast has
 * already been raised by the time this runs, so the alternative is not "report
 * it better", it is "report it twice, once as a console error nobody reads".
 */
function fireAndForget(run: Promise<unknown>): void {
  void run.catch(() => {});
}

export function ProvidersTab({
  state,
  actions,
  canManage,
}: {
  state: InferenceState;
  actions: InferenceActions;
  canManage: boolean;
}) {
  /** Which option the connect dialog is open on, if any. */
  const [connecting, setConnecting] = useState<string | null>(null);
  /** The provider the connect dialog is editing, if it is editing one. */
  const [editing, setEditing] = useState<Provider | null>(null);
  const [adding, setAdding] = useState(false);
  /**
   * The removal awaiting confirmation, if any.
   *
   * Both removals go through one piece of state because they are one decision
   * with two answers, and holding them apart would be two ways to have a
   * confirmation open at once.
   */
  const [confirming, setConfirming] = useState<{
    intent: RemovalIntent;
    provider: Provider;
  } | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /**
   * Whether the last failure was a **probe** failure.
   *
   * Gated on the class rather than on a boolean, and cleared on every attempt:
   * a slug collision or a failed key write must not offer to skip verification,
   * because neither is evidence that the endpoint is fine.
   */
  const [probeFailure, setProbeFailure] = useState<ProbeClass | null>(null);
  /**
   * What each row's Test is doing, keyed by slug.
   *
   * **Per row, not per page.** A single result under the card says nothing about
   * which of several providers was tested, which was the bug: two providers must
   * be able to show two different answers at once without ambiguity.
   */
  const [tests, setTests] = useState<Record<string, TestState>>({});
  // Cleared on unmount, so a result that resolves after the page is gone does
  // not set state on a component nobody is looking at.
  const timers = useRef<Record<string, ReturnType<typeof setTimeout>>>({});
  useEffect(
    () => () => {
      for (const timer of Object.values(timers.current)) clearTimeout(timer);
    },
    [],
  );

  /**
   * The routes as refs, so a removal can say which workloads it would reset.
   *
   * Read from the same `state.routes` the Routing tab renders, rather than from
   * a second fetch: the confirmation has to name what the write will actually
   * scrub, and two reads are two chances to disagree about it.
   */
  const routingMap: RoutingMap = Object.fromEntries(
    WORKLOADS.map((w) => [w, parseRef(state.routes[WORKLOAD_TIER[w]] ?? "")]),
  ) as RoutingMap;

  /** Runs a test for one row and lands its answer on that row. */
  const runTest = (slug: string, run: () => Promise<ProbeResult>) => {
    clearTimeout(timers.current[slug]);
    setTests((prev) => ({ ...prev, [slug]: { kind: "testing" } }));
    void run()
      .then((result) =>
        setTests((prev) => ({
          ...prev,
          [slug]: {
            kind: "done",
            ok: result.ok,
            // The host's own sentence, so a proxy failure reads differently
            // from a rejected key. "Failed" would throw that away at the last
            // step.
            message: result.ok
              ? "Reached the provider."
              : (result.message ?? "The check did not complete."),
          },
        })),
      )
      .catch((err) =>
        setTests((prev) => ({
          ...prev,
          [slug]: {
            kind: "done",
            ok: false,
            message: err instanceof ApiError ? err.message : "The check did not complete.",
          },
        })),
      )
      .finally(() => {
        timers.current[slug] = setTimeout(
          () => setTests((prev) => ({ ...prev, [slug]: { kind: "idle" } })),
          TEST_RESULT_MS,
        );
      });
  };

  if (state.load === "unavailable") return null;
  if (state.load === "loading") return <Skeleton className="h-64 rounded-xl" />;
  if (state.load === "error") {
    return <SectionUnreachable label="Couldn't read this company's model providers" />;
  }

  const closeConnect = () => {
    setConnecting(null);
    setEditing(null);
    setError(null);
    setProbeFailure(null);
  };

  async function submitConnect(draft: ConnectDraft) {
    setBusy(true);
    // Cleared on every retry, so an attempt that fails for an unrelated reason
    // does not still offer to skip verification.
    setError(null);
    setProbeFailure(null);
    try {
      if (editing) {
        await actions.edit(editing.slug, {
          label: draft.label,
          baseUrl: draft.baseUrl,
          key: draft.key,
        });
      } else if (draft.kind === MANAGED_OPTION_SLUG) {
        // Managed has no provider record — it resolves from a chain — so its
        // credential goes to its own route rather than through `add`.
        await actions.saveManagedKey(draft.key ?? "");
      } else {
        const result = await actions.add(draft);
        // A non-destructive probe failure saved the row and kept the key. The
        // dialog closes on it, because the save succeeded — the advisory is the
        // page's note, not an error in a form that is still open.
        if (result.probe && !result.probe.ok && result.probe.class) {
          setProbeFailure(result.probe.class);
        }
      }
      closeConnect();
    } catch (err) {
      // The envelope's own `invalid request: ` prefix is machine vocabulary and
      // this sentence is read by a person standing in front of the field they
      // have to correct.
      setError(err instanceof ApiError ? stripEnvelopePrefix(err.message) : "That did not work.");
      // The host refuses an add on exactly one probe class, and it is the only
      // refusal that unlocks "add anyway".
      if (err instanceof ApiError && err.message.includes("rejected the credential")) {
        setProbeFailure("auth");
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-4">
      {state.status?.restartRequired && (
        <RestartNotice
          canRestart={canManage && state.status.canRebuildInPlace}
          onRestart={() => fireAndForget(actions.restart())}
        />
      )}

      <Card>
        <CardContent className="flex flex-wrap items-center justify-between gap-3">
          <div className="grid gap-0.5">
            <h2 className="text-sm font-medium">LLM Providers</h2>
            <p className="text-xs text-muted-foreground">
              Add and configure language model providers.
            </p>
          </div>
          <Button
            type="button"
            disabled={!canManage}
            data-testid="inference-add-open"
            onClick={() => setAdding(true)}
          >
            <Plus className="size-4" />
            Add a provider
          </Button>
        </CardContent>
      </Card>

      <Card>
        <CardContent className="px-0">
          <h3 className="px-4 pb-2 text-xs font-medium tracking-wide text-muted-foreground uppercase">
            Connected
          </h3>
          <ProviderList
            providers={state.providers}
            managed={state.status?.managed}
            canManage={canManage}
            busySlug={state.busySlug}
            onToggle={(p, enabled) => fireAndForget(actions.setEnabled(p.slug, enabled))}
            onEdit={(p) => {
              setEditing(p);
              setConnecting(p.kind);
            }}
            onTest={(p) => runTest(p.slug, () => actions.test(p.slug))}
            // Both removals are confirmed rather than performed, because one
            // deletes a record and its routes and the other only clears a
            // credential — and they sit one menu item apart.
            onRemove={(p) => setConfirming({ intent: "provider", provider: p })}
            onRemoveKey={(p) => setConfirming({ intent: "key", provider: p })}
            // The same dialog the add flow opens, in edit mode: adding a key and
            // replacing one are one code path.
            onReplaceKey={(p) => {
              setEditing(p);
              setConnecting(p.kind);
            }}
            onMakeDefault={(p) => fireAndForget(actions.makeDefault(p.slug))}
            // The same handler the header's button uses, passed down rather
            // than reimplemented: one way to add a provider, not two.
            onAdd={() => setAdding(true)}
            onManagedToggle={(enabled) => fireAndForget(actions.setManagedOn(enabled))}
            onManagedTest={() => runTest(MANAGED_SLUG, actions.testManagedChain)}
            // The same dialog the add flow opens on the managed option, so
            // adding a key and replacing one are one code path.
            onManagedReplaceKey={() => {
              setEditing(null);
              setConnecting(MANAGED_OPTION_SLUG);
            }}
            // An empty key is how the store clears a value — it has no delete —
            // and it removes step 1 alone. The response re-reads the chain, so
            // the row immediately says whichever step answers next.
            onManagedRemoveKey={() => fireAndForget(actions.saveManagedKey(""))}
            testState={(slug) => tests[slug] ?? { kind: "idle" }}
          />
        </CardContent>
      </Card>

      {/* Said only when it is worth saying, and never more than once.
          "Managed is always available as a fallback" was **not true** here —
          managed needs a credential and can resolve to nothing — and when it
          does resolve, the row above already names the step that answers and
          who it bills. A line repeating that is duplication; a line claiming
          "always" is a lie. So this speaks in exactly the two cases the row
          cannot cover on its own.

          No navigation in either: the action is the button at the top of this
          same page, and telling an operator to go where they already are has
          stopped reading its own surroundings. */}
      {managedFallbackNote(state.status?.managed) && (
        <p className="text-xs text-muted-foreground" data-testid="inference-managed-fallback">
          {managedFallbackNote(state.status?.managed)}
        </p>
      )}

      <AddProviderDialog
        open={adding}
        onOpenChange={setAdding}
        providers={state.providers}
        managed={state.status?.managed}
        onChoose={(option) => {
          setAdding(false);
          setEditing(null);
          setConnecting(option);
        }}
      />
      <ProviderConnectDialog
        optionSlug={connecting}
        providers={state.providers}
        busy={busy}
        error={error}
        offerAddAnyway={probeFailure !== null}
        onCancel={closeConnect}
        onSubmit={(draft) => void submitConnect(draft)}
      />
      <RemoveProviderDialog
        intent={confirming?.intent ?? null}
        label={confirming?.provider.label ?? ""}
        impact={
          confirming
            ? removalImpact(confirming.provider, state.providers, routingMap, categoryOf)
            : { routed: [], isDefault: false, lastEnabled: false, defaultMovesTo: null }
        }
        busy={busy}
        // Offered only where it is genuinely the softer answer: switching a
        // provider off keeps its endpoint, its credential and its routes, which
        // is what somebody removing one usually wants. It is not an alternative
        // to clearing a credential, and it is not one for a provider that is
        // already off.
        onDisable={
          confirming?.intent === "provider" && confirming.provider.enabled
            ? () => {
                const provider = confirming.provider;
                setConfirming(null);
                fireAndForget(actions.setEnabled(provider.slug, false));
              }
            : undefined
        }
        onCancel={() => setConfirming(null)}
        onConfirm={() => {
          if (!confirming) return;
          const { intent, provider } = confirming;
          setConfirming(null);
          // An empty key is how the store clears a value — it has no delete.
          fireAndForget(
            intent === "key"
              ? actions.edit(provider.slug, { key: "" })
              : actions.remove(provider.slug),
          );
        }}
      />
    </div>
  );
}

/**
 * The one explanation that survives the deletion pass.
 *
 * A saved configuration that has landed and is not yet in effect looks exactly
 * like one that is — there is no control on the page whose appearance differs —
 * so this is the case where prose is carrying information rather than repeating
 * a button. Which brain a company runs is chosen when its runtime is built, so a
 * company that started with no model keeps echoing however the config changes
 * underneath it.
 *
 * The button appears only where the host said it can actually rebuild. Naming a
 * remedy and handing over a control that cannot perform it is worse than naming
 * the remedy alone.
 */
function RestartNotice({
  canRestart,
  onRestart,
}: {
  canRestart: boolean;
  onRestart: () => void;
}) {
  return (
    <Card data-testid="inference-restart-required">
      <CardContent className="flex flex-wrap items-center justify-between gap-3">
        <div className="grid min-w-0 flex-1 gap-1">
          <p className="text-sm">
            Restart required. This company booted without a model, so agents are still on the
            offline brain and the saved configuration is not yet in effect.
          </p>
          {/* The remedy, in both spellings the host could mean. The capability
              comes from the host, which does not know which shell it is
              packaged in — and this is the case where the operator cannot infer
              the next step from any control on the page, because there is no
              control for it. */}
          {!canRestart && (
            <p className="text-xs text-muted-foreground" data-testid="inference-restart-manual">
              This host cannot rebuild a company runtime in place: quit and reopen the app, or
              restart the server process.
            </p>
          )}
        </div>
        {canRestart && (
          <Button
            type="button"
            variant="outline"
            data-testid="inference-restart-now"
            onClick={onRestart}
          >
            Restart now
          </Button>
        )}
      </CardContent>
    </Card>
  );
}
