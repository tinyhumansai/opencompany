import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle, ExternalLink, Loader2, Plug, Save } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  getComposioStatus,
  setComposioApiKey,
  setComposioToken,
  testComposioApiKey,
  type ComposioMutation,
  type ComposioStatus,
} from "@/api/composio";
import { ApiError } from "@/api/types";
import {
  advisoryMessage,
  offersSkipVerify,
  verdictMessage,
} from "@/composio/classify";
import type { ComposioSubmitOutcome } from "@/composio/classify";
import { ComposioRowList } from "@/composio/ComposioRowList";
import { ProbeAdvisory } from "@/composio/ProbeAdvisory";
import {
  composioForm,
  composioRows,
  credentialDialogBlurb,
  credentialDialogTitle,
  modeOf,
} from "@/composio/rows";
import type {
  ComposioPending,
  ComposioRow,
  ComposioRowId,
} from "@/composio/types";
import { grantStanding } from "@/lib/provider-grid";
import { classifyLoadFailure } from "@/lib/section-load";
import { SectionUnreachable } from "@/views/connections/SectionUnreachable";
import { GrantNamespace } from "@/components/grant-namespace";
import { Button, buttonVariants } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";

/**
 * Where a BYOK key comes from.
 *
 * The bare host the field's own copy names, not a deep link into the settings
 * page that mints the key: that path is the vendor's to move, and a stale one
 * strands the operator on a 404 *after* a sign-in that worked — which is worse
 * than the landing page they can navigate from themselves.
 */
const COMPOSIO_DASHBOARD_URL = "https://app.composio.dev";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * Whether this viewer may change what the company connects through (issue
   * #403) — the credential its agents present, and which provider accounts
   * they act through.
   *
   * **Courtesy, not enforcement.** The host refuses both writes with a 403
   * whatever this says. What it prevents is offering a credential field whose
   * Save is refused only after the operator has already pasted a live secret
   * into it.
   */
  canManage: boolean;
  /**
   * Called after the stored credential changes.
   *
   * The provider grid's status and routing are downstream of which credential
   * this company reaches Composio with — setting or clearing one here flips
   * `credentialSource`, and every tile's route with it. Without this the grid
   * would keep rendering the old answer while this section reported the new
   * one: the same two-surfaces-disagreeing failure #582 is about, arriving
   * through the credential rather than through the connection list.
   */
  onChanged: () => void;
}

/**
 * Which account this company reaches Composio through (issue #110, Cell D).
 *
 * # The shape, and why it changed
 *
 * A **Connected card of rows** — a mark, a name, one sub-line, controls on the
 * right — matching the reworked LLM page. What it replaces was two large tiles
 * under a four-branch paragraph that explained what each route meant, what
 * saving would change, and where the connected providers went; almost every
 * clause of it explained a control that was visible while it was being read.
 *
 * The one thing not carried over from the inference rows is the **per-row
 * toggle**. Those model providers that *coexist*; Composio's two modes are one
 * stored scalar and `resolve_access` reads exactly one branch, so a toggle per
 * row would make both-on and both-off reachable with nowhere to put them. The
 * rows are single-select instead: `composioRows` states the argument in full.
 *
 * # Where the decisions live
 *
 * Not here. `@/composio/rows` decides what each row says and which controls it
 * may offer; `@/composio/classify` decides what a failed check is called. This
 * component is layout and handlers, and everything it renders is a function of
 * those two.
 *
 * # The credential tiers, which the rows report rather than hide
 *
 * - `attested` (hosted) — the instance holds a platform identity, so there is
 *   nothing to paste and nothing stored.
 * - `company` (issue #586) — this company's own TinyHumans credential, set by
 *   its admin, brokering Composio.
 * - `static` — a Composio token this company pasted, or a static instance key.
 * - `none` — no credential can be obtained, so agents get no Composio tools.
 *
 * The managed row's sub-line is driven by `managedCredentialSource`, never by
 * "did somebody paste a token": that boolean is issue #886, it answers only
 * about the first of three tiers, and it is routinely false on a working hosted
 * tenant.
 *
 * Every credential here is WRITE-ONLY: stored and never shown again. A set or
 * clear takes effect on the agents' next turn, no restart. Hidden entirely when
 * the feature is not in the build.
 */
export function ComposioSection({
  client,
  company,
  canManage,
  onChanged,
}: Props) {
  const [load, setLoad] = useState<
    "loading" | "ready" | "unavailable" | "unconfigured" | "error"
  >("loading");
  const [status, setStatus] = useState<ComposioStatus | null>(null);
  const [busy, setBusy] = useState(false);
  // The credential form an operator asked for, as an intent rather than a
  // rendered state. `composioForm` re-checks it against the rows on every
  // render, so a form left standing by a status that moved underneath it — a
  // refresh, another admin — simply stops being returned.
  const [pending, setPending] = useState<ComposioPending | null>(null);
  const [secret, setSecret] = useState("");
  // What the last attempt at storing a credential came back as. Two shapes, not
  // a boolean: an advisory KEPT the key and a rejection stored nothing, and the
  // page must not colour a successful save red.
  const [outcome, setOutcome] = useState<ComposioSubmitOutcome | null>(null);
  // The managed → BYOK confirmation. It renders inside the credential dialog,
  // in place of that dialog's footer, rather than as a second modal over it:
  // what it warns about — every provider connected through the managed route
  // becoming invisible — is about the key in the field above it, and a second
  // overlay would hide the thing being decided about.
  const [confirmSwitch, setConfirmSwitch] = useState(false);
  // The check's verdict, kept apart from `outcome` on purpose. A check writes
  // nothing, so it must not reach `offersSkipVerify` — "add anyway" answers a
  // refused *write*, and offering it after a failed check would propose storing
  // a key that is already stored.
  const [testOutcome, setTestOutcome] = useState<ComposioSubmitOutcome | null>(
    null,
  );
  // Which row's check is in flight. Not folded into `busy`: `busy` disables the
  // controls that write, and a check changes nothing.
  const [testingRow, setTestingRow] = useState<ComposioRowId | null>(null);

  const requestGeneration = useRef(0);
  // Focus in and back out of the switch confirmation. It is a labelled group
  // inside the credential dialog rather than a popup of its own, so nothing
  // moves focus for free: showing it unmounts the footer's Save button, which
  // is where focus was, and the dialog's trap then leaves focus on the popup
  // itself — invisible to a mouse user, but a screen-reader or keyboard user
  // loses their place entirely.
  //
  // Both directions are by REF to a currently-rendered button, not by recording
  // the node that had focus. Recording it was the first shape and it cannot
  // work here: the node focus came from is the footer's Save button, and
  // showing the confirmation is exactly what unmounts it — so by the time
  // Cancel puts it back, the recorded node is detached and a restore onto it is
  // a no-op. The footer's Save button re-registers this ref on the way back,
  // which is the same button by role even though it is a different node.
  const confirmPrimaryActionRef = useRef<HTMLButtonElement | null>(null);
  const saveButtonRef = useRef<HTMLButtonElement | null>(null);
  // Whether the confirmation has been on screen during this dialog. Without it,
  // the first render of every dialog would count as a close and yank focus onto
  // Save before the operator has touched the field.
  const confirmWasOpen = useRef(false);

  const refresh = useCallback(async () => {
    const generation = ++requestGeneration.current;
    try {
      const s = await getComposioStatus(client, company);
      if (generation !== requestGeneration.current) return;
      setStatus(s);
      // Hide the whole section when the feature is not compiled into this build.
      setLoad(s.inBuild ? "ready" : "unavailable");
    } catch (err) {
      if (generation !== requestGeneration.current) return;
      // A 404 is a host with no Composio surface — hide it. Anything else is a
      // host that could not answer; keep the section rather than vanishing
      // (issue #1470).
      setLoad(classifyLoadFailure(err));
    }
  }, [client, company]);

  useEffect(() => {
    setStatus(null);
    setPending(null);
    setSecret("");
    setOutcome(null);
    setConfirmSwitch(false);
    setLoad("loading");
    void refresh();
  }, [refresh]);

  // Opening moves focus onto the confirmation's primary action; cancelling
  // hands it back to the Save button the confirmation replaced.
  //
  // On a save that SUCCEEDED there is nothing to hand back to — the whole
  // dialog unmounts — and `saveButtonRef` is null by then, so this does
  // nothing and Base UI returns focus to the row control that opened the
  // dialog. That is the right destination, and it is why this does not need a
  // guard for the difference.
  useEffect(() => {
    if (confirmSwitch) {
      confirmWasOpen.current = true;
      confirmPrimaryActionRef.current?.focus();
      return;
    }
    if (!confirmWasOpen.current) return;
    confirmWasOpen.current = false;
    saveButtonRef.current?.focus();
  }, [confirmSwitch]);

  const rows = composioRows(status);
  const form = composioForm(pending, rows);
  const persistedMode = modeOf(status);

  /**
   * Land a mutation's answer.
   *
   * A response can carry an advisory even though it succeeded — the key was
   * stored and only the check failed — so "did it throw" is not enough to
   * decide what the page says next. The dialog closes either way, because the
   * credential is written.
   *
   * **The advisory is toasted, not merely set.** `onChanged()` at the bottom of
   * this function bumps the generation `ComposioView` keys this section on, and
   * a changed `key` is an unmount — so the `outcome` set three lines earlier is
   * thrown away before it can paint. That remount is deliberate (issue #586:
   * the tier this section reports is downstream of the key just written), and
   * the clean branch survived it only because a toast lives outside the tree
   * that remounts. The advisory branch had no toast, so the one case an
   * operator must not be left guessing about — the key IS stored, the check did
   * not pass — said nothing at all. The inline `outcome` is kept for the paths
   * that do not remount; the toast is what makes this one reach anybody.
   */
  function settle(res: ComposioMutation) {
    setStatus(res.status);
    setSecret("");
    setPending(null);
    setConfirmSwitch(false);
    if (res.probeClass || res.advisory) {
      const message = advisoryMessage(res.probeClass, res.advisory);
      setOutcome({ kind: "advisory", probeClass: res.probeClass, message });
      // Amber, not red: the write landed. `toast.error` here would report a
      // stored credential as a failure, which is the miscolouring the two
      // outcome shapes exist to prevent.
      toast.warning(message);
    } else {
      setOutcome(null);
      toast.success(res.note);
    }
    onChanged();
  }

  /** Land a refusal. The credential was not stored, so the form stays open. */
  function reject(err: unknown, fallback: string) {
    setOutcome({
      kind: "rejected",
      status: err instanceof ApiError ? err.status : undefined,
      message: err instanceof ApiError ? err.message : fallback,
    });
  }

  async function run(call: () => Promise<ComposioMutation>, fallback: string) {
    setBusy(true);
    // Cleared on every attempt, so an "add anyway" offered after one failure
    // does not survive a retry that failed for an unrelated reason.
    setOutcome(null);
    try {
      settle(await call());
    } catch (err) {
      reject(err, fallback);
    } finally {
      setBusy(false);
    }
  }

  /**
   * Move this company onto the managed route.
   *
   * One call, because on this host the mode is a consequence of the key rather
   * than a separate control: `setComposioApiKey("")` clears the company's own
   * Composio key and the route derived from it in the same write. That is also
   * why the own-account row offers no "Remove key" — it would be this exact
   * call under a second name.
   */
  function useManaged() {
    void run(
      () => setComposioApiKey(client, company, ""),
      "Could not move this company to the TinyHumans-managed route.",
    );
  }

  /**
   * Check the credential stored for `row`, in place.
   *
   * Writes nothing, on any path — the host does not either, and this is the
   * console half of the same rule: the page is not refreshed, no status is
   * replaced, and a rejected key is left exactly where it is. `auth` is the one
   * class shown as an error, because it is the one class that is a statement
   * about the key; the rest are amber, since the key is plausibly fine and only
   * the connection is in question.
   */
  async function runTest(row: ComposioRow) {
    setTestingRow(row.id);
    setTestOutcome(null);
    try {
      const verdict = await testComposioApiKey(client, company);
      if (verdict.ok) {
        toast.success(
          `Composio accepted the ${row.keyNoun} stored for ${row.label}.`,
        );
        return;
      }
      const message = verdictMessage(verdict.probeClass, verdict.message);
      setTestOutcome(
        verdict.probeClass === "auth"
          ? { kind: "rejected", message }
          : { kind: "advisory", probeClass: verdict.probeClass, message },
      );
    } catch (err) {
      setTestOutcome({
        kind: "rejected",
        status: err instanceof ApiError ? err.status : undefined,
        message:
          err instanceof ApiError
            ? err.message
            : "Could not check the Composio API key.",
      });
    } finally {
      setTestingRow(null);
    }
  }

  /** Clear the Composio token stored for the managed route, falling back to whatever remains. */
  function clearManagedToken() {
    void run(
      () => setComposioToken(client, company, ""),
      "Could not clear the Composio token.",
    );
  }

  /**
   * Store what is in the field.
   *
   * `skipVerify` is passed only from the "add anyway" affordance, which is
   * offered only after a typed refusal — never as a standing option, and never
   * after an advisory, where the key already landed.
   */
  function submit(skipVerify = false) {
    const value = secret.trim();
    if (!form || !value) return;
    if (form.credential === "composio-api-key") {
      void run(
        () => setComposioApiKey(client, company, value, skipVerify),
        "Could not save the Composio API key.",
      );
    } else {
      void run(
        () => setComposioToken(client, company, value),
        "Could not save the token.",
      );
    }
  }

  /**
   * A Save that would move this company off the managed route for the first
   * time.
   *
   * Gated on a confirmation because the consequence — the providers connected
   * through the TinyHumans-managed Composio account are in *that* account and
   * vanish from the grid until they are connected again here — is not readable
   * off a row. Rotating a key already in use, and switching back, are not
   * gated: neither strands anything the operator cannot immediately undo.
   */
  function requestSubmit() {
    if (
      form?.credential === "composio-api-key" &&
      persistedMode === "managed"
    ) {
      setConfirmSwitch(true);
      return;
    }
    submit();
  }

  function openForm(row: ComposioRow, action: ComposioPending["action"]) {
    setPending({ row: row.id, action });
    setSecret("");
    setOutcome(null);
    setConfirmSwitch(false);
  }

  /**
   * Close the credential dialog, discarding what was typed into it.
   *
   * Every exit the operator can take runs through here — Cancel, the X,
   * Escape, a click on the backdrop — so none of them leaves a secret in state
   * behind a closed modal, or `confirmSwitch` armed for the next opening.
   *
   * One exit does not, and cannot: the dialog is derived from `composioForm`,
   * so a status that moves underneath it closes the dialog by making that
   * function return `null` (which is the point — see its doc). That path leaves
   * `pending` and `secret` set. It is reachable only from a refresh raised
   * behind the overlay, and the next `openForm` clears both, but this is a
   * discipline the shape does not enforce rather than one it guarantees.
   */
  function closeForm() {
    setPending(null);
    setSecret("");
    setOutcome(null);
    setConfirmSwitch(false);
  }

  if (load === "unavailable") return null;

  // The composio-grant tri-state, narrowed the same way `ProvidersSection` does
  // (issue #1478): `undefined` reads as "unknown", never as "not granted", so
  // this section and the grid a few inches below it cannot disagree on the same
  // field.
  //
  // It no longer paints a badge — see the heading below — but the narrowing is
  // load-bearing all the same: the call to action underneath fires on
  // `not-granted` only, and collapsing "unknown" into it is exactly what #1478
  // is about.
  const grant = grantStanding(status?.granted);
  // "Add anyway" answers a refused API-KEY write, and only that. `skipVerify`
  // is a parameter of `setComposioApiKey` alone — `submit(true)` on the managed
  // row's token drops it and re-sends a byte-identical request, so the button
  // there could only ever earn the same refusal again. The classifier cannot
  // see which credential is in the form, so the form says.
  const skipOffered =
    offersSkipVerify(outcome) && form?.credential === "composio-api-key";

  return (
    <section className="space-y-3">
      {/* The heading, and nothing beside it.

          A grant badge sat here reading "granted" / "not granted" / "grant
          unknown". Two of its three states say nothing an operator can act on
          — "granted" is the ordinary case, and "grant unknown" reports that a
          field was not read — so on almost every visit it was a chip of
          vocabulary ("grant") that belongs to the tool namespace rather than to
          the question this card answers, which is whose Composio account the
          company reaches.

          The third state is the one worth surfacing, and it already is, one
          element below: an explicit not-granted renders `GrantNamespace`, which
          says what is wrong in a sentence and offers the fix. The badge was the
          same fact with no verb. */}
      <div className="flex flex-wrap items-center gap-2">
        <Plug className="size-4 text-muted-foreground" />
        <h2 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          Connected
        </h2>
      </div>

      {load === "loading" ? (
        <Skeleton className="h-32 rounded-xl" />
      ) : load === "error" ? (
        <SectionUnreachable label="Couldn't read this company's Composio credential" />
      ) : (
        <>
          {/* Fires only on an explicit not-granted, never on an unchecked grant
              (issue #1478): telling an operator to widen a grant that may
              already be set, off a field that was never read, is the same false
              confidence a status badge used to show. */}
          {grant === "not-granted" && (
            <GrantNamespace
              client={client}
              company={company}
              namespace="composio"
              explanation="Agents will not receive Composio tools even once connected."
              canManage={canManage}
              onGranted={async () => {
                await refresh();
                onChanged();
              }}
              testId="composio-not-granted"
            />
          )}

          <Card className="py-0">
            <CardContent className="px-0">
              <ComposioRowList
                rows={rows}
                canManage={canManage}
                busy={busy}
                onSelect={(row) => {
                  if (row.id === "managed") useManaged();
                  // The own-account route cannot be chosen without the key that
                  // makes it resolve, so choosing it opens the field rather than
                  // writing anything.
                  else openForm(row, "add");
                }}
                onAddKey={(row) => openForm(row, "add")}
                onReplaceKey={(row) => openForm(row, "replace")}
                onRemoveKey={(row) => {
                  if (row.id === "managed") clearManagedToken();
                }}
                onTest={(row) => void runTest(row)}
                testingRow={testingRow}
              />
            </CardContent>
          </Card>

          {/* The one explanation that survives, because no control on the page
              says it: a credential that has landed and a credential that is in
              effect look identical, and here they differ by one turn. */}
          <p className="text-xs text-muted-foreground">
            A change here takes effect on the agents&apos; next turn. No
            restart.
          </p>

          {/* The outcome of an action taken from a ROW rather than from the
              dialog — "Use this" on the managed route, "Remove token" — which
              have no field to sit beside and no dialog to sit in.

              `!form` rather than a check on the kind, because what decides
              where a message goes is whether a dialog is open, not what the
              message says: behind a modal overlay, a sentence on the page is a
              sentence nobody can read, so anything raised while the dialog is
              up renders inside it instead.

              Note what does NOT arrive here: an advisory from `settle`. That
              path remounts this section (see `settle`), so its message is
              carried by a toast. */}
          {outcome && !form && (
            <ProbeAdvisory
              outcome={outcome}
              skipOffered={false}
              busy={busy}
              onSkip={() => submit(true)}
              onDismiss={() => setOutcome(null)}
            />
          )}

          {/* The check's verdict, with its own test-id namespace: it and a
              write's outcome are separate state and can be on screen together.
              Never offers "add anyway" — that answers a refused write, and this
              route wrote nothing to refuse. */}
          {testOutcome && (
            <ProbeAdvisory
              outcome={testOutcome}
              skipOffered={false}
              busy={testingRow !== null}
              onSkip={() => {}}
              onDismiss={() => setTestOutcome(null)}
              testIdPrefix="composio-test"
            />
          )}

          {/* The credential surface is a MODAL, and that is the fix rather
              than the decoration.

              It was an inline card appended to the bottom of this section —
              after the rows, after the "takes effect next turn" line, after two
              advisory slots. Clicking "Add a token" on a row near the top of a
              scrolling page therefore rendered a form roughly a screenful below
              the fold, with nothing scrolling to it: the operator pressed the
              button, the page did not visibly move, and the honest reading of
              that is "the button is broken". It was reported as exactly that.

              A modal also matches what the action is. Pasting the credential
              every agent in the company presents is not an edit alongside the
              rows — it is one decision taken to the exclusion of the page
              behind it, and it either lands or is refused before anything else
              can be touched. Which is also why the host's answer is rendered in
              here (`outcome`) instead of on the page underneath. */}
          {form && canManage && (
            <Dialog
              open
              onOpenChange={(next) => {
                // A write in flight holds the dialog open: dismissing it now
                // would take away the only place its answer is reported, while
                // the credential lands anyway.
                if (next || busy) return;
                closeForm();
              }}
            >
              <DialogContent
                className="sm:max-w-md"
                showCloseButton={!busy}
                data-testid="composio-form-dialog"
              >
                <DialogHeader>
                  <DialogTitle>{credentialDialogTitle(form)}</DialogTitle>
                  <DialogDescription>
                    {credentialDialogBlurb(form)}
                  </DialogDescription>
                </DialogHeader>

                <div className="space-y-1.5">
                  <Label
                    htmlFor={form.credential}
                    className="text-xs"
                    data-testid="composio-form-label"
                  >
                    {form.row === "byok" ? "Composio API key" : "Composio token"}
                    {form.rotating
                      ? " — stored; paste a new value to rotate"
                      : ""}
                  </Label>
                  <Input
                    id={form.credential}
                    type="password"
                    autoComplete="off"
                    disabled={busy}
                    placeholder={
                      form.row === "byok"
                        ? "ak_…"
                        : "paste the company's Composio token"
                    }
                    value={secret}
                    onChange={(e) => setSecret(e.target.value)}
                    // Enter submits, the idiom the console's other credential
                    // field already uses (`McpServersSection`). Not while the
                    // confirmation is up: there the keyboard belongs to the
                    // choice being put. Tab is NOT taken, so the field behind
                    // the confirmation is still reachable — deliberately, since
                    // the value it holds is what the confirmation is about.
                    onKeyDown={(e) => {
                      if (e.key !== "Enter") return;
                      if (busy || confirmSwitch || !secret.trim()) return;
                      e.preventDefault();
                      requestSubmit();
                    }}
                  />
                  {/* Where to get it, which is the one thing the field cannot
                      say for itself. What storing it *does* is the line under
                      the title, so it is not repeated here. */}
                  <p className="text-xs text-muted-foreground">
                    {form.row === "byok"
                      ? "From your Composio dashboard at app.composio.dev. Stored on this host, never shown again."
                      : "Stored on this host, never shown again."}
                  </p>
                  {/* The dashboard the line above names, as somewhere to go
                      rather than an address to retype. Deliberately the bare
                      host from that copy and not a guessed deep link: a
                      settings path that moves leaves the operator on a 404
                      after a sign-in that worked.

                      Only on the own-account row. The managed route's token
                      does not come from app.composio.dev at all — it is a
                      bearer the TinyHumans backend issues — so offering the
                      same errand there would send an operator to the wrong
                      vendor for the credential they were asked for. */}
                  {form.row === "byok" && (
                    <a
                      href={COMPOSIO_DASHBOARD_URL}
                      target="_blank"
                      rel="noreferrer"
                      data-testid="composio-open-dashboard"
                      className={cn(
                        buttonVariants({ variant: "outline", size: "sm" }),
                        "mt-1",
                      )}
                    >
                      Open Composio dashboard
                      <ExternalLink className="size-3.5" />
                    </a>
                  )}
                </div>

                {/* The refusal, where the operator is looking — and "add
                    anyway", which answers a refused write and so can only be
                    offered next to the field that was refused. */}
                {outcome && (
                  <ProbeAdvisory
                    outcome={outcome}
                    skipOffered={skipOffered}
                    busy={busy}
                    onSkip={() => submit(true)}
                    onDismiss={() => setOutcome(null)}
                  />
                )}

                {/* Said before the switch, not after: what it costs is not
                    readable off a row.

                    `role="group"`, NOT `role="alertdialog"`, which is what this
                    carried while it was a block on the page. It is inside a
                    `DialogContent` now — an element already announced as
                    `role="dialog" aria-modal="true"` — and a second dialog role
                    nested in a modal's own subtree is not a composition ARIA
                    defines: two elements claim one modal context and the inner
                    one has no modality, no focus containment and no boundary of
                    its own. A labelled, described group is the honest shape for
                    what this actually is — a titled block of the dialog it
                    lives in, whose text belongs to the button beneath it. */}
                {confirmSwitch ? (
                  <div
                    role="group"
                    aria-labelledby="composio-switch-warning"
                    aria-describedby="composio-switch-consequence"
                    className="space-y-3 rounded-md border border-status-blocked/40 bg-status-blocked-soft p-3"
                  >
                    <p
                      id="composio-switch-warning"
                      className="inline-flex items-center gap-2 text-xs font-medium"
                    >
                      <AlertTriangle className="size-3.5 shrink-0" />
                      Providers connected before this stay where they are
                    </p>
                    <p
                      id="composio-switch-consequence"
                      className="text-xs text-muted-foreground"
                    >
                      They live in the Composio account this company reached
                      before, not in this one, so the grid will look empty until
                      you connect them again here. Choosing TinyHumans-managed
                      again puts this company back where it is now.
                    </p>
                    <div className="flex flex-wrap gap-2">
                      <Button
                        ref={confirmPrimaryActionRef}
                        size="sm"
                        disabled={busy}
                        data-testid="composio-confirm-switch"
                        onClick={() => submit()}
                      >
                        {busy ? (
                          <Loader2 className="size-4 animate-spin" />
                        ) : (
                          <Save className="size-4" />
                        )}
                        Use this company&apos;s account
                      </Button>
                      <Button
                        variant="outline"
                        size="sm"
                        disabled={busy}
                        onClick={() => setConfirmSwitch(false)}
                      >
                        Cancel
                      </Button>
                    </div>
                  </div>
                ) : (
                  <DialogFooter>
                    <Button
                      variant="outline"
                      disabled={busy}
                      data-testid="composio-form-cancel"
                      onClick={closeForm}
                    >
                      Cancel
                    </Button>
                    <Button
                      ref={saveButtonRef}
                      disabled={busy || !secret.trim()}
                      data-testid="composio-form-save"
                      onClick={requestSubmit}
                    >
                      {busy ? (
                        <Loader2 className="size-4 animate-spin" />
                      ) : (
                        <Save className="size-4" />
                      )}
                      {form.rotating
                        ? `Rotate ${form.keyNoun}`
                        : `Save ${form.keyNoun}`}
                    </Button>
                  </DialogFooter>
                )}
              </DialogContent>
            </Dialog>
          )}
        </>
      )}
    </section>
  );
}

// `showManagedTokenCard` lived here and is gone. It gated the legacy
// managed-route token card on the SELECTED tile and the PERSISTED route
// agreeing, because either alone put two credential surfaces on screen at
// exactly the moment an operator was switching between them — and one of those
// directions offered a Clear that silently destroyed a preserved token (#586)
// while leaving the company where it was.
//
// The invariant survives; the predicate does not need to. There is one
// `pending` form at a time and `composioForm` returns at most one
// `ComposioForm`, so two credential surfaces are now unrepresentable rather
// than merely tested against. `composio/rows.ts` owns the check that a pending
// form is still permitted by the row it belongs to, which is the half that
// used to be spread across `mode`, `onByok` and `showOverride`.
//
// `modeOf` also moved, unchanged, to `composio/rows.ts`. Nothing is re-exported
// from here on its way out: a pure function reachable only through a `.tsx` is
// the shape that made the old predicate testable but not the rows around it.
