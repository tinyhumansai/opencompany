import { useEffect, useRef, useState } from "react";
import { Plus } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import type { ProbeResult } from "@/api/inference";
import { TEST_RESULT_MS } from "./classify";
import type { TestState } from "./classify";
import type { ProbeClass } from "./types";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { SectionUnreachable } from "@/views/connections/SectionUnreachable";
import { LocalHarnesses } from "./LocalHarnesses";
import { AddProviderDialog } from "./AddProviderDialog";
import { DefaultModelDialog } from "./DefaultModelDialog";
import { ProviderConnectDialog } from "./ProviderConnectDialog";
import type { ConnectDraft, ModelAsk } from "./ProviderConnectDialog";
import { MANAGED_SLUG, NO_CREDENTIAL_RESOLVES, ProviderList } from "./ProviderList";
import { RemoveProviderDialog } from "./RemoveProviderDialog";
import type { RemovalIntent } from "./RemoveProviderDialog";
import { isAzureEndpoint } from "./catalogue";
import { MANAGED_OPTION_SLUG, defaultBrokenCopy, defaultNeedsModel, modelAskFromProbe, probeEndpoint } from "./connect";
import { MANAGED_TARGET_LABEL, managedFallbackNote, nothingCanAnswer } from "./managed-copy";
import { hasUsedBy, removalImpact } from "./removal";
import { RoutesNotCarriedBanner } from "./RoutesNotCarriedBanner";
import type { InferenceActions, InferenceState } from "./use-inference";
import type { Provider } from "./types";

/**
 * LLM Providers: what this company can reach a model through, and how to add one.
 *
 * One page (per-workload routing is gone, keys rework issue #2306 phase 5b) —
 * a company has one default `{provider, model}` and agents may pin their own
 * (Team → the agent → Harness & model). Every provider connects the same way:
 * add the key or endpoint, choose a model from what it actually publishes,
 * save. There is no state in which a row is "connected" but has no model.
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
  /** Which option the connect dialog is open on, if any. */
  const [connecting, setConnecting] = useState<string | null>(null);
  /** The provider the connect dialog is editing, if it is editing one. */
  const [editing, setEditing] = useState<Provider | null>(null);
  const [adding, setAdding] = useState(false);
  /**
   * The action awaiting confirmation, if any (keys rework, issue #2306's
   * confirmation contract: every destructive action and every on/off toggle
   * confirms — `disable` and `enable` both included now).
   */
  const [confirming, setConfirming] = useState<{
    intent: RemovalIntent;
    provider: Provider;
    /**
     * The legacy Managed row's confirm, synthesised from no real provider
     * record (round-2 review, P0-1): both directions route through
     * `actions.setManagedOn` rather than a provider write against a slug with
     * no row behind it — detected by this flag, never by comparing
     * `provider.slug` to {@link MANAGED_SLUG}, which a real `tinyhumans` row
     * would collide with.
     */
    legacy?: boolean;
  } | null>(null);
  /**
   * A fresher `usedBy` and message than `confirming.provider.usedBy`, from a
   * `409 in_use` this same confirm already hit once. The dialog stays open and
   * re-renders with these instead of closing on the refusal — a stale UI is
   * the one case `confirmInUse: true` is not sent blind.
   */
  const [confirmRefusal, setConfirmRefusal] = useState<{ message: string; usedBy?: Provider["usedBy"] } | null>(null);
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
   * The endpoint's catalogue, once the details step has been submitted. `null`
   * until then. Unlike the pre-rework flow this always opens once set — there
   * is no longer a "this endpoint resolves tiers itself, skip the model" case
   * (D-model, 2d).
   */
  const [modelAsk, setModelAsk] = useState<ModelAsk | null>(null);
  /**
   * What each row's Test is doing, keyed by slug.
   *
   * **Per row, not per page.** A single result under the card says nothing about
   * which of several providers was tested, which was the bug: two providers must
   * be able to show two different answers at once without ambiguity.
   */
  const [tests, setTests] = useState<Record<string, TestState>>({});
  /**
   * The row the "Set as default" dialog is open on, if any.
   */
  const [settingDefault, setSettingDefault] = useState<Provider | null>(null);
  const [defaultBusy, setDefaultBusy] = useState(false);
  const [defaultError, setDefaultError] = useState<string | null>(null);
  /**
   * Invalidates a stale connect attempt (round-2 review, P0-2).
   *
   * Bumped on every submit and every time the connect dialog is opened,
   * closed, or moved back a step. A probe or write started under an earlier
   * value checks it again once its promise settles and drops the result if it
   * has changed — otherwise cancelling mid-probe and opening a **different**
   * provider let the first one's late answer land in the second one's dialog
   * (the previous provider's catalogue and error, an empty hidden key field,
   * and a submit that could save the wrong model against the new provider).
   */
  const attempt = useRef(0);

  // Cleared on unmount, so a result that resolves after the page is gone does
  // not set state on a component nobody is looking at.
  const timers = useRef<Record<string, ReturnType<typeof setTimeout>>>({});
  useEffect(
    () => () => {
      for (const timer of Object.values(timers.current)) clearTimeout(timer);
    },
    [],
  );

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

  /**
   * Bumps {@link attempt} and clears everything a stale result could still
   * write into.
   *
   * Clears `busy` too, not just the error/model-ask fields. `submitConnect`'s
   * own `finally` only resets `busy` when `myAttempt === attempt.current` —
   * deliberately, so a stale async response from a superseded attempt cannot
   * clear the spinner a *newer* attempt is showing. But a *successful* submit
   * calls `closeConnect` (which calls this) from inside that same attempt's
   * try block, before its own `finally` runs — so by the time `finally`
   * checks, this call has already bumped `attempt` out from under it, and the
   * guard skips `setBusy(false)`. A dialog opened again after that (a second
   * provider in the same session, `openConnect` also routes through here)
   * inherited `busy: true` from the previous, already-finished attempt and
   * rendered its first submit permanently disabled on "Reading models…" —
   * exactly the shape the loop in "a second provider holds a credential of
   * its own" hit on its second iteration.
   */
  const resetConnectState = () => {
    attempt.current += 1;
    setBusy(false);
    setError(null);
    setProbeFailure(null);
    setModelAsk(null);
  };

  const closeConnect = () => {
    resetConnectState();
    setConnecting(null);
    setEditing(null);
  };

  /**
   * Opens the connect dialog fresh — every caller that sets
   * `connecting`/`editing` goes through this.
   *
   * Also closes the "Add a provider" list dialog (found twice, independently:
   * live lane 1's KR-L1-02, and upstream's "handle missing provider in
   * inference tab"). `AddProviderDialog`'s own `onChoose` calls straight into
   * this without ever setting `adding` back to `false`, so the outer picker
   * — a separate `open` boolean from the inner connect dialog — stayed
   * mounted and open *underneath* the connect dialog it had just opened. Both
   * are Base UI portalled dialogs at the same z-index, so the connect dialog
   * (rendered later in the DOM) covered it while open — but the moment the
   * connect dialog closed (submit success, Escape, or an overlay click), the
   * still-open picker resurfaced with its own overlay (marking the whole page
   * `aria-hidden` again) and ate every click after it, including a later
   * `inference-add-open` click meant to open a fresh one. A loop that
   * connects two providers in one test hit this on the second iteration.
   * Opening the connect dialog always closes the picker, whether or not it
   * happened to be open.
   */
  const openConnect = (next: { connecting: string | null; editing: Provider | null }) => {
    resetConnectState();
    setAdding(false);
    setConnecting(next.connecting);
    setEditing(next.editing);
  };

  async function submitConnect(draft: ConnectDraft) {
    const myAttempt = ++attempt.current;
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
          model: draft.model,
        });
        if (myAttempt !== attempt.current) return;
      } else if (!modelAsk) {
        // **Ask before writing, not after refusing.** A model is always
        // required (D-model), and the only honest moment to ask is with that
        // endpoint's own catalogue in hand. This runs for every kind now —
        // TinyHumans included (decision X6: the console no longer calls the
        // deprecated `PUT …/inference/managed/key`; it goes through this same
        // add flow like everything else) — never conditionally on whether the
        // endpoint "needs" one.
        const url = probeEndpoint(draft.kind, draft.baseUrl);
        const probe = url
          ? await actions.probeDraftEndpoint({ baseUrl: url, key: draft.key, kind: draft.kind })
          : null;
        // Round-2 review, P0-2: a cancel (Escape, overlay click, or opening a
        // different provider) bumps `attempt` — drop this probe's answer
        // rather than let it land in whatever dialog is open by the time it
        // resolves.
        if (myAttempt !== attempt.current) return;
        // Round-2 review, P1-8: a rejected key must not advance to the model
        // step. The old behaviour opened it anyway, in free text, and only
        // discovered the same rejection a second time when the add itself
        // ran — by which point "Add anyway" was on offer for a credential
        // that was never going to work. Stop here, on the field the operator
        // can actually fix.
        if (probe && !probe.ok && probe.class === "auth") {
          setError(probe.message ? stripEnvelopePrefix(probe.message) : "The credential was rejected.");
          setProbeFailure("auth");
          return;
        }
        setModelAsk(modelAskFromProbe(url, probe, isAzureEndpoint));
        return;
      } else {
        await actions.add({
          kind: draft.kind,
          label: draft.label,
          baseUrl: draft.baseUrl,
          key: draft.key,
          model: draft.model ?? "",
          addAnyway: draft.addAnyway,
        });
        if (myAttempt !== attempt.current) return;
      }
      closeConnect();
    } catch (err) {
      if (myAttempt !== attempt.current) return;
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
      if (myAttempt === attempt.current) setBusy(false);
    }
  }

  /**
   * Runs a confirmed provider action.
   *
   * `confirmInUse` is decided by the caller from what the open dialog actually
   * showed (round-2 review, P1-1) — never sent blind. Sending it unconditionally
   * made the `409 in_use` re-open path below unreachable: the host always
   * proceeds once told to, so a row whose `usedBy` changed after the dialog
   * opened (an agent pinned in another tab, say) was stranded with no warning
   * at all, exactly the case this contract exists to catch.
   *
   * A `409 in_use` this still hits — because the open dialog showed nothing
   * used, or the host's `usedBy` changed again between this click and the
   * request landing — re-opens the dialog with the refusal's own message and
   * fresh `usedBy` rather than closing on it, so confirming there sends
   * `confirmInUse: true` against what the operator was just told. Every other
   * failure closes and toasts, the same as any other write.
   */
  async function confirmedWrite(run: () => Promise<unknown>) {
    try {
      await run();
      setConfirming(null);
      setConfirmRefusal(null);
    } catch (err) {
      if (err instanceof ApiError && err.status === 409 && err.code === "in_use") {
        setConfirmRefusal({ message: stripEnvelopePrefix(err.message), usedBy: err.usedBy });
        return;
      }
      // Any other failure: the toast from `write()` already said so. Close,
      // the same as before this contract existed.
      setConfirming(null);
      setConfirmRefusal(null);
    }
  }

  const openDefaultDialog = (provider: Provider) => {
    setDefaultError(null);
    setSettingDefault(provider);
  };

  /** Opens a confirm dialog on a real row, re-reading status so its `usedBy` is fresh (round-2 review, P1-1). */
  const openConfirm = (intent: RemovalIntent, provider: Provider) => {
    setConfirmRefusal(null);
    setConfirming({ intent, provider });
    void actions.reload().catch(() => {});
  };

  /** The legacy Managed row's own confirm — see the `confirming.legacy` doc above. */
  const openManagedConfirm = (intent: RemovalIntent, provider: Provider) => {
    setConfirmRefusal(null);
    setConfirming({ intent, provider, legacy: true });
  };

  // Round-2 review, P1-1: the `usedBy` actually shown — the row's own, freshened
  // by the `reload()` `openConfirm` fired when the dialog opened, or (once a
  // 409 has already hit this same confirm) the refusal's own fresher answer.
  // `confirmInUse` is sent only when this is non-empty, which is also exactly
  // the condition under which the dialog has anything to show at all.
  const shownUsedBy =
    confirmRefusal?.usedBy ??
    (confirming ? (state.providers.find((p) => p.slug === confirming.provider.slug)?.usedBy ?? confirming.provider.usedBy) : undefined);
  const confirmInUseNow = hasUsedBy(shownUsedBy);

  const brokenDefault = defaultBrokenCopy(state.status?.defaultChoice, state.providers);
  const defaultRow = state.providers.find((p) => p.slug === state.status?.defaultChoice?.provider);

  return (
    <div className="space-y-4">
      {/* Phase 5a: a routing table the boot-time carry could not fold into a
          single default. One release's bridge; nothing here reads or writes
          routing, which is gone (phase 5b). */}
      <RoutesNotCarriedBanner
        rows={state.status?.routesNotCarried}
        canManage={canManage}
        onChooseDefault={() => {
          // The first row naming a provider this company still has and has
          // enabled is the best guess at "choose a model for the row that
          // already resolves here"; otherwise fall back to Add.
          const named = (state.status?.routesNotCarried ?? [])
            .map((r) => {
              const slug = r.route.split(":")[0]?.trim();
              // The legacy `managed` chain resolves through the `tinyhumans`
              // provider now (round-2 review, P3-4) — the route grammar's own
              // slug for it never matched a row, so this always fell back to
              // Add for a company whose unset routes named managed.
              return slug === "managed" ? MANAGED_OPTION_SLUG : slug;
            })
            .find((slug) => slug && state.providers.some((p) => p.slug === slug && p.enabled));
          const target = named ? state.providers.find((p) => p.slug === named) : undefined;
          if (target) openDefaultDialog(target);
          else setAdding(true);
        }}
      />

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

      {/* Decision Q1/2c: a bare-slug default (a provider chosen before this
          rework, no model) never resolves a turn. Said plainly, with the one
          action that fixes it. */}
      {defaultNeedsModel(state.status?.defaultChoice) && (
        <Card data-testid="inference-default-needs-model-banner">
          <CardContent className="flex flex-wrap items-center justify-between gap-3">
            <p className="text-sm">Your default provider has no model. Choose one.</p>
            {canManage && defaultRow && (
              <Button
                type="button"
                variant="outline"
                data-testid="inference-default-needs-model-choose"
                onClick={() => openDefaultDialog(defaultRow)}
              >
                Choose a model
              </Button>
            )}
          </CardContent>
        </Card>
      )}

      {/* Decision X14: disabling or deleting the default's provider never
          clears the stored default — it just stops resolving. This is the
          durable notice for that state, with the exact wording the agent
          editor's own fallback line repeats for every agent with no pin. */}
      {brokenDefault && (
        <Card data-testid="inference-default-broken-banner">
          <CardContent className="flex flex-wrap items-center justify-between gap-3">
            <p className="text-sm text-status-blocked-text">{brokenDefault}</p>
            {canManage && (
              <Button
                type="button"
                variant="outline"
                data-testid="inference-default-broken-choose"
                onClick={() => {
                  // Round-2 review, P2-3: this opened the Add dialog, which
                  // lists only providers this company does not have yet — so
                  // an operator whose already-connected row should simply
                  // become the default again had no way to say so from here.
                  // The first enabled row is the same best guess the routes
                  // banner above makes; "Set as default" on any other row's
                  // own menu remains the way to choose a different one.
                  const target = state.providers.find((p) => p.enabled);
                  if (target) openDefaultDialog(target);
                  else setAdding(true);
                }}
              >
                Choose a default
              </Button>
            )}
          </CardContent>
        </Card>
      )}

      <Card>
        <CardContent className="px-0">
          <h3 className="px-4 pb-2 text-xs font-medium tracking-wide text-muted-foreground uppercase">
            Connected
          </h3>
          <ProviderList
            providers={state.providers}
            managed={state.status?.managed}
            defaultChoice={state.status?.defaultChoice}
            canManage={canManage}
            busySlug={state.busySlug}
            // Decision X3: every toggle confirms now, both directions.
            onToggle={(p, enabled) => openConfirm(enabled ? "enable" : "disable", p)}
            onEdit={(p) => openConnect({ connecting: p.kind, editing: p })}
            // Round-2 review, P1-7c: entry zero and the row a bare-slug
            // default names cannot go through the ordinary edit dialog — the
            // host refuses entry zero's edit outright, and neither has
            // anywhere else to write a model except `set_default`. Route
            // those two through "Set as default" instead of `onEdit`.
            onChooseModel={(p) => {
              const choice = state.status?.defaultChoice;
              if (p.origin === "entryZero" || (defaultNeedsModel(choice) && choice?.provider === p.slug)) {
                openDefaultDialog(p);
              } else {
                openConnect({ connecting: p.kind, editing: p });
              }
            }}
            onTest={(p) => runTest(p.slug, () => actions.test(p.slug))}
            onRemove={(p) => openConfirm("provider", p)}
            onRemoveKey={(p) => openConfirm("key", p)}
            // The same dialog the add flow opens, in edit mode: adding a key and
            // replacing one are one code path.
            onReplaceKey={(p) => openConnect({ connecting: p.kind, editing: p })}
            onMakeDefault={(p) => openDefaultDialog(p)}
            // The same handler the header's button uses, passed down rather
            // than reimplemented: one way to add a provider, not two.
            onAdd={() => setAdding(true)}
            onManagedToggle={(enabled) =>
              openManagedConfirm(enabled ? "enable" : "disable", {
                // @deprecated keys-rework #2306: the legacy managed row has no
                // provider record — synthesised just enough for the confirm
                // dialog's copy. `onConfirm` below routes on `confirming.legacy`
                // to `actions.setManagedOn`, never a real provider write
                // against this synthesised slug (round-2 review, P0-1).
                id: MANAGED_SLUG, slug: MANAGED_SLUG, label: MANAGED_TARGET_LABEL, kind: "tinyhumans", baseUrl: "", models: {}, enabled: true, keyConfigured: true,
              })
            }
            onManagedTest={() => runTest(MANAGED_SLUG, actions.testManagedChain)}
            // Decision X6: opens the ordinary TinyHumans catalogue add/edit
            // flow, never the deprecated `PUT …/inference/managed/key` route
            // directly.
            onManagedReplaceKey={() => openConnect({ connecting: MANAGED_OPTION_SLUG, editing: null })}
            testState={(slug) => tests[slug] ?? { kind: "idle" }}
          />
        </CardContent>
      </Card>

      {/* The coding CLIs a teammate can be put on, beside the providers it can
          reach a model through. Renders nothing when this build declares none,
          which is what a hosted one does. */}
      <LocalHarnesses client={client} company={company} />

      {/* @deprecated keys-rework #2306: describes only the legacy managed
          fallback chain's transitional pre-row state — see `managed-copy.ts`. */}
      {managedFallbackNote(state.status?.managed) && (
        <p className="text-xs text-muted-foreground" data-testid="inference-managed-fallback">
          {managedFallbackNote(state.status?.managed)}
        </p>
      )}

      {state.providers.length > 0 &&
        nothingCanAnswer(state.providers, state.status?.managed?.configured) && (
          <p className="text-xs text-status-blocked-text" data-testid="inference-providers-dead-end">
            {NO_CREDENTIAL_RESOLVES}. Switch one of these back on, or connect a provider.
          </p>
        )}

      <AddProviderDialog
        open={adding}
        onOpenChange={setAdding}
        providers={state.providers}
        onChoose={(option) => openConnect({ connecting: option, editing: null })}
      />
      {/* Keyed so the dialog is a fresh component per open. Its fields seed at
          mount from this row; without the key React would keep the previous
          open's state and the seeding would have to be an effect, which runs
          after paint and races with anything typed before it. */}
      <ProviderConnectDialog
        key={`${connecting ?? "closed"}:${editing?.slug ?? "new"}`}
        client={client}
        company={company}
        optionSlug={connecting}
        providers={state.providers}
        editing={editing}
        busy={busy}
        error={error}
        offerAddAnyway={probeFailure !== null}
        modelAsk={modelAsk}
        // Decision X1 (round-2 review, P2-2): keyed on whether a default is
        // already stored, not on whether this is the company's first row —
        // a company with rows but no chosen default still gets the "this
        // becomes the default" note on its next add.
        noDefaultYet={state.status?.defaultChoice == null}
        replacesKey={
          connecting === MANAGED_OPTION_SLUG && editing === null && state.status?.managed?.source === "provider_key"
        }
        onCancel={closeConnect}
        onBack={resetConnectState}
        onSubmit={(draft) => void submitConnect(draft)}
      />
      <DefaultModelDialog
        key={settingDefault?.slug ?? "none"}
        client={client}
        company={company}
        provider={settingDefault}
        providers={state.providers}
        defaultChoice={state.status?.defaultChoice}
        busy={defaultBusy}
        error={defaultError}
        onCancel={() => setSettingDefault(null)}
        onSubmit={(model) => {
          if (!settingDefault) return;
          const slug = settingDefault.slug;
          setDefaultBusy(true);
          setDefaultError(null);
          void actions
            .makeDefault(slug, model)
            .then((result) => {
              // P1-2 (round-2 review): the backend at review time silently
              // dropped this write — no body extractor on `set_default` — and
              // a success toast still showed. Once `defaultChoice` ships this
              // catches any host that still accepts the request and does not
              // apply it, rather than reporting success on a no-op.
              if (result.status.defaultChoice && (result.status.defaultChoice.provider !== slug || result.status.defaultChoice.model !== model)) {
                setDefaultError("Saved, but the company default still does not show this pair. Try again.");
                return;
              }
              setSettingDefault(null);
            })
            .catch((err) =>
              setDefaultError(err instanceof ApiError ? stripEnvelopePrefix(err.message) : "That did not work."),
            )
            .finally(() => setDefaultBusy(false));
        }}
      />
      <RemoveProviderDialog
        intent={confirming?.intent ?? null}
        label={confirming?.provider.label ?? ""}
        impact={
          confirming
            ? { ...removalImpact(confirming.provider, state.providers), usedBy: shownUsedBy }
            : { lastEnabled: false }
        }
        busy={busy}
        serverError={confirmRefusal?.message}
        // Offered only where it is genuinely the softer answer: turning a
        // provider off keeps its endpoint and its credential, which is what
        // somebody removing one usually wants. It is not an alternative to
        // clearing a credential. Never offered for the legacy row, which has
        // no provider record to disable.
        onDisable={
          confirming?.intent === "provider" && confirming.provider.enabled && !confirming.legacy
            ? () => {
                const provider = confirming.provider;
                setBusy(true);
                void confirmedWrite(() => actions.setEnabled(provider.slug, false, confirmInUseNow)).finally(() =>
                  setBusy(false),
                );
              }
            : undefined
        }
        onCancel={() => {
          setConfirming(null);
          setConfirmRefusal(null);
        }}
        onConfirm={() => {
          if (!confirming) return;
          const { intent, provider, legacy } = confirming;
          setBusy(true);
          void confirmedWrite(() => {
            // Round-2 review, P0-1: the legacy Managed row has no real
            // provider record — both directions of its toggle go through the
            // managed-chain action, never a provider write against a slug
            // with no row behind it.
            if (legacy) return actions.setManagedOn(intent === "enable");
            switch (intent) {
              case "disable":
                return actions.setEnabled(provider.slug, false, confirmInUseNow);
              case "enable":
                return actions.setEnabled(provider.slug, true, confirmInUseNow);
              case "key":
                return actions.edit(provider.slug, { key: "", confirmInUse: confirmInUseNow });
              case "provider":
                return actions.remove(provider.slug, confirmInUseNow);
            }
          }).finally(() => setBusy(false));
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
