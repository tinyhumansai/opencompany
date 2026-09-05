import { useCallback, useEffect, useRef, useState } from "react";
import { Loader2, RotateCcw, ShieldCheck } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  getPolicy,
  isPolicyStatus,
  NOT_A_POLICY,
  type PolicyStatus,
  resetPolicy,
  setPolicy,
} from "@/api/policy";
import { listWorkflowToolSlugs } from "@/api/workflows";
import { AdminOnlyNotice } from "@/components/admin-only-notice";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
// The title row's readers, so a policy written HERE reaches the pill without
// waiting for its 30s poll. Not a cycle: `use-autonomy` imports only
// `@/api/policy` and `@/lib/visible-poll`.
import { applyAutonomy } from "@/hooks/use-autonomy";
import { usd } from "@/lib/money";
import { cn } from "@/lib/utils";

/**
 * Tools worth naming as an *example* of something to always ask about, most
 * consequential first (issue #1226).
 *
 * A placeholder is a suggestion, so it should suggest a gate an operator might
 * actually want. Taking the host's list in its own order put
 * `read_workspace_state` — a read — in the worked example, which is a valid
 * entry and a pointless one. This orders the candidates; the datalist under the
 * field still carries the deployment's full set in the host's order, because
 * that is a lookup rather than a recommendation.
 *
 * Not a validator and not a filter: anything wired here is offered, and a tool
 * absent from this list simply sorts after the ones on it.
 */
const WORTH_GATING = [
  "publish_artifact",
  "shell",
  "http_request",
  "curl",
  "git_operations",
  "apply_patch",
  "web_fetch",
];

/**
 * Up to three worked examples, drawn from what this deployment wired.
 *
 * Falls back to real tool names when the host served nothing — a host predating
 * `…/workflows/tool-slugs` still deserves an example that would work, and the
 * one this field used to give (`payment.send, filing.submit, external.publish`)
 * is the one issue #684 deleted for gating nothing.
 */
export function alwaysAskPlaceholder(wired: string[]): string {
  if (wired.length === 0) return "shell, http_request, publish_artifact";
  const rank = (slug: string) => {
    const at = WORTH_GATING.indexOf(slug);
    return at === -1 ? WORTH_GATING.length : at;
  };
  return [...wired]
    .sort((a, b) => rank(a) - rank(b) || wired.indexOf(a) - wired.indexOf(b))
    .slice(0, 3)
    .join(", ");
}

/**
 * Whether moving through the host-provided tier order gives agents more
 * autonomy. `from`/`to` are host tier values; an unknown value is never "from"
 * (nothing is known about a move it starts from) and never "to" (there is no
 * ordering to move to).
 */
export function widensAutonomy(
  tiers: PolicyStatus["tiers"],
  from: string,
  to: string,
): boolean {
  const fromIndex = tiers.findIndex((tier) => tier.value === from);
  const toIndex = tiers.findIndex((tier) => tier.value === to);
  return fromIndex !== -1 && toIndex > fromIndex;
}

/**
 * The words the widening confirmation is made of, exported because there are
 * now **two** ways to reach the same decision.
 *
 * The tier is also changeable from the window's title row (`AutonomyPill`), and
 * a second confirmation written there would be a second set of words free to
 * drift from these — one dialog saying "Give teammates more autonomy?" and
 * another saying something else about the identical act. The comparison itself
 * is already shared ([`widensAutonomy`]); this shares the sentence that explains
 * it, so the two entry points cannot disagree about what is being agreed to.
 *
 * Literals and a function rather than a shared component: this page reaches the
 * same dialog from three different decisions (a tier, a spend-cap raise, a
 * reset) and only the tier one is shared, so a component would have to carry
 * all three.
 */
export const AUTONOMY_CONFIRM_TITLE = "Give teammates more autonomy?";

/** The cancel label. It names the outcome, not the gesture: nothing changes. */
export const AUTONOMY_CONFIRM_CANCEL = "Keep current setting";

/** The confirm label for a tier widening. */
export const AUTONOMY_CONFIRM_ACTION = "Give more autonomy";

/**
 * The standing note under a tier widening.
 *
 * True in this build and load-bearing: the gate runs with policy HITL disabled
 * (`src/runtime/builder.rs`), so what still stops an agent is an explicit
 * `request_approval` rather than the tier. An operator agreeing to a wider tier
 * is entitled to read that wherever they can agree to it.
 */
export const AUTONOMY_PROMPTS_NOTE =
  "Approval prompts remain explicit through request_approval.";

/**
 * What changes, in the host's own words on both sides of the move.
 *
 * Both descriptions are the host's (`TIER_TEXT`, `src/server/ops/policy.rs`),
 * never a paraphrase — that prose is server-side precisely so it tracks the gate
 * it describes. `current` is optional because a console running against a newer
 * host can be sitting on a mode it has no text for; the sentence then names only
 * what the operator is moving *to*, which is the half that still matters.
 */
export function tierWideningExplanation(
  current: string | undefined,
  next: PolicyStatus["tiers"][number],
): string {
  return `Instead of: ${current ?? ""} With ${next.label}: ${next.description} They will use the ${next.label} setting on their next turn.`;
}

/** Whether replacing the current spend cap with the manifest cap loosens it. */
export function widensSpendCap(
  current: number | null,
  manifest: number | null,
): boolean {
  // `null` is the stricter state: with no cap every spend parks, and a finite
  // cap lets sub-cap payments through on their own (`Some(None)` in
  // `PolicyOverride` is a real, deliberate "no cap" choice). So loosening is
  // null → finite, or a finite cap raised to a higher finite cap.
  if (current === null) return manifest !== null;
  if (manifest === null) return false;
  return manifest > current;
}

/**
 * Whether a tier change gives the company more freedom than it has now.
 *
 * Same order comparison as [`widensAutonomy`]; kept under the pre-#1423 name
 * because the always-ask vocabulary test pins it that way.
 */
export function isAutonomyEscalation(
  tiers: PolicyStatus["tiers"],
  currentMode: string,
  nextMode: string,
): boolean {
  return widensAutonomy(tiers, currentMode, nextMode);
}

/**
 * Whether an `always_approve` entry gates a target under the backend's matcher
 * (`src/policy/always_approve.rs`).
 *
 * The matcher accepts more than an exact tool name: the comparison is
 * ASCII-case-insensitive (a full-Unicode fold would accept a case confusable
 * the host's `eq_ignore_ascii_case` does not — `worKspace_write` lowercases
 * to `workspace_write` but never gates), and a leading dotted segment gates
 * the rest, so `SHELL` is the wired `shell` tool and `invoice` covers
 * `invoice.send`. The "is not a tool" warning under the field must not
 * contradict the gate it describes — an entry the backend would match is a
 * valid fence, not a mistake — so the same two rules decide whether an entry
 * counts as known.
 */
export function alwaysApproveGates(entry: string, target: string): boolean {
  const e = entry.trim();
  const t = target.trim();
  if (e === "") return false;
  if (asciiEqualsIgnoreCase(t, e)) return true;
  // Leading dotted segment: `invoice` gates `invoice.send`, but a bare prefix
  // (`pay` for `payroll.export`) does not — the segment boundary is load
  // bearing, exactly as it is in the backend.
  return (
    t.length > e.length &&
    t[e.length] === "." &&
    asciiEqualsIgnoreCase(t.slice(0, e.length), e)
  );
}

/**
 * ASCII-only case-insensitive equality, mirroring `str::eq_ignore_ascii_case`.
 *
 * `String.prototype.toLowerCase()` is NOT the same comparison: it folds
 * Unicode case, so `"Ä".toLowerCase() === "ä"` while the host treats the two
 * as different effect kinds. The confirmation must agree with the gate itself,
 * so only ASCII letters fold here and every other code unit must match exactly.
 */
function asciiEqualsIgnoreCase(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    const ca = a.charCodeAt(i);
    const cb = b.charCodeAt(i);
    if (ca === cb) continue;
    // Folding an ASCII letter is OR-ing in bit 0x20. Anything that does not
    // land in 'a'..'z' after the fold is not an ASCII letter, so it cannot be
    // a case pair.
    const lowerA = ca | 0x20;
    const lowerB = cb | 0x20;
    if (lowerA !== lowerB || lowerA < 0x61 || lowerA > 0x7a) return false;
  }
  return true;
}

/**
 * Whether `list` still gates `target`, mirroring the host matcher
 * (`src/policy/always_approve.rs::matches`): exact or a leading dotted segment,
 * ASCII-case-insensitive, on a segment boundary.
 *
 * A reset drops the whole override, always-ask list included, so an effective
 * entry the manifest's list does not gate is a fence a reset would silently
 * take down. This is the "would the reset let something through that used to
 * ask" test, and it must agree with the gate itself or the confirmation would
 * contradict the behaviour it describes.
 */
export function gatedBy(list: string[], target: string): boolean {
  const t = target.trim();
  return list.some((entry) => {
    const e = entry.trim();
    if (e === "") return false;
    if (asciiEqualsIgnoreCase(t, e)) return true;
    return (
      t.length > e.length &&
      t[e.length] === "." &&
      asciiEqualsIgnoreCase(t.slice(0, e.length), e)
    );
  });
}

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * Whether this viewer may change the policy.
   *
   * Both writes behind this card call `require_admin`, so `false` renders the
   * tiers and the deadline as a statement of what the company's policy IS,
   * with nothing on the card that offers to change it.
   *
   * Required, and deliberately not defaulted, for the reason `GrantNamespace`
   * gives: a caller that has not worked out the viewer's role must not get an
   * enabled control by omission.
   */
  canManage: boolean;
}

/**
 * The autonomy tier and the always-ask list (issue #562).
 *
 * An operator drowning in approval cards previously had no way to stop it: the
 * tier lives in the company manifest, and nothing in the console read or wrote
 * it — so changing it meant editing a version-controlled file and redeploying,
 * or on a hosted tenant (where the manifest is a read-only boot snapshot) it
 * meant nothing at all.
 *
 * Two things this deliberately renders rather than hides:
 *
 * - **The tiers are described by consequence, not by name.** "Supervised" and
 *   "full" mean nothing to someone deciding between them; "asks before every
 *   change, including its own scratch files" does. The prose comes from the
 *   host, because it describes what that host's approval gate actually does.
 * - **When a change bites.** A tier change lands on the company's *next* turn,
 *   so a turn already running finishes under the old one. Since stopping the
 *   flood *now* is exactly why an operator is here, that gap is stated instead
 *   of being left to discover.
 * - **That version control outranks it.** The override is durable between seed
 *   edits, but editing `[policy]` in `company.toml` clears it. An operator who
 *   cannot see that would be surprised by a redeploy.
 */
export function PolicySettings({ client, company, canManage }: Props) {
  const [status, setStatus] = useState<PolicyStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  // Distinguishes "still loading" from "load finished and failed". Without it,
  // `loading || !status` renders the spinner forever on a failed load and the
  // operator has no way to retry.
  const [loadError, setLoadError] = useState<string | null>(null);
  // The always-ask list is edited as text and only committed on Save, so a
  // half-typed effect kind never reaches the gate.
  const [draftAlways, setDraftAlways] = useState("");
  const [dirty, setDirty] = useState(false);
  // The spend cap and deadline are each edited as text and only committed on
  // their own Save, so a half-typed value never reaches the gate.
  const [draftSpend, setDraftSpend] = useState("");
  const [noSpendCap, setNoSpendCap] = useState(false);
  const [draftDeadline, setDraftDeadline] = useState("");
  // A looser tier changes what teammates can do without stopping for approval.
  // Keep the target, rather than a boolean, so the dialog can compare the
  // host-provided consequences that actually apply to this deployment.
  const [tierAwaitingConfirmation, setTierAwaitingConfirmation] =
    useState<PolicyStatus["tiers"][number] | null>(null);
  // A reset restores the manifest's tier AND always-ask list, so the widening
  // check must run on it too — otherwise "Use the manifest's policy" is a
  // one-click way around the confirmation the tier buttons get, and the same
  // for always-ask gates the manifest does not carry. Kept separate from the
  // tier state so the dialog knows which action to perform on confirm.
  const [resetAwaitingConfirmation, setResetAwaitingConfirmation] =
    useState(false);
  // A direct spend-cap raise lets more payments through without asking — the
  // same widening the tier buttons and a loosening reset confirm. Remember the
  // target so the dialog's confirm button saves the value the operator typed.
  const [pendingCapRaise, setPendingCapRaise] = useState<number | null>(null);
  /**
   * The tool names this deployment can actually gate (issue #1226).
   *
   * An `always_approve` entry IS a tool name on the harness path — see
   * `src/policy/always_approve.rs`, which explains that the two were never
   * separate namespaces. So the honest set of worked examples is the set of
   * tools wired here, and this is the same read the workflow copilot grounds on
   * (issues #783 / #874) for the same reason: so nothing suggests a tool this
   * deployment does not have.
   *
   * Empty on a host predating the route, which degrades to the plain field the
   * operator had before — the suggestions are help, never a constraint. The
   * namespace stays open on purpose (a hosted brain may emit a kind this
   * repository has never seen), so nothing here validates what is typed.
   */
  const [wiredTools, setWiredTools] = useState<string[]>([]);
  // Whether the wired-tool set above was actually served. The array starts
  // empty while the request is pending and stays empty on a host predating the
  // route; an empty set is not proof that every configured entry is unwired, so
  // only a successful load lets the "is not a tool" warning speak.
  const [wiredToolsLoaded, setWiredToolsLoaded] = useState(false);

  // The scope this card's async work belongs to. A save or manual retry issued
  // for one company must stand down once the operator switches to another:
  // applying the stale response would overwrite the new company's card and
  // drafts with the old company's policy. The effect-driven read is already
  // guarded by its cleanup's `live` flag; this is the same guard for the write
  // path and the manual retry, following the `scopeRef` pattern `app-shell`
  // hands `ChatView` so sends cannot cross a company switch.
  const scopeRef = useRef({ client, company });
  useEffect(() => {
    scopeRef.current = { client, company };
  }, [client, company]);

  /** Whether an async completion still belongs to the scope on screen. */
  const isCurrentScope = (origin: {
    client: OpenCompanyClient;
    company: string | null;
  }) => {
    const current = scopeRef.current;
    return (
      current.client === origin.client && current.company === origin.company
    );
  };

  const load = useCallback(
    async (live: () => boolean) => {
      setLoading(true);
      setLoadError(null);
      // A new scoped read must not carry the previous company's values. If this
      // read fails, the card should show the error state, not the old company's
      // policy (deadline, cap, list) as if it were this one's — the drafts below
      // are the exact state an operator would otherwise save against the new
      // company. They are overwritten from `next` on success.
      setStatus(null);
      setDraftAlways("");
      setDraftSpend("");
      setNoSpendCap(false);
      setDraftDeadline("");
      try {
        const next = await getPolicy(client, company);
        // A body that is not a policy is a load FAILURE, not a policy. Left
        // unchecked it reaches `next.alwaysApprove.join(...)` two lines down,
        // and a throw there unmounts the console — there is no error boundary.
        // The `catch` below already knows how to say so.
        if (!isPolicyStatus(next)) throw new Error(NOT_A_POLICY);
        // A response for a company this `load` no longer describes must not
        // overwrite the current company's state: when the scope changes mid-
        // flight, the effect's cleanup flips `live` for the stale request, so
        // its continuation (and its `finally`) stand down here rather than
        // clobbering the read that replaced it.
        if (!live()) return;
        setStatus(next);
        setDraftAlways(next.alwaysApprove.join(", "));
        setDraftSpend(next.autoApproveUnderUsd?.toString() ?? "");
        setNoSpendCap(next.autoApproveUnderUsd === null);
        // A host that predates the deadline field omits it; `undefined` must
        // fall back to the historical 24-hour default rather than render as
        // "undefined hours".
        setDraftDeadline((next.approvalTtlHours ?? 24).toString());
        setDirty(false);
      } catch (error) {
        if (!live()) return;
        const message =
          error instanceof Error ? error.message : "Could not load the policy.";
        setLoadError(message);
        toast.error(message);
      } finally {
        if (live()) setLoading(false);
      }
    },
    [client, company],
  );

  useEffect(() => {
    let live = true;
    void load(() => live);
    return () => {
      live = false;
    };
  }, [load]);

  // The confirmation dialog holds a choice reviewed against ONE company's
  // policy. If the scope changes while it is open, that pending action no
  // longer describes what the operator looked at — confirming would loosen or
  // reset the NEW company under a dialog about the old one. Drop it on scope
  // change rather than bind it to the originating company.
  useEffect(() => {
    setTierAwaitingConfirmation(null);
    setResetAwaitingConfirmation(false);
    setPendingCapRaise(null);
  }, [client, company]);

  // Deliberately silent about its own failure, and deliberately not part of
  // `load`: these are suggestions under a free-text box. A host that cannot
  // serve them costs the operator a datalist, not the setting, and a second
  // error banner would report the policy card as broken when it is merely
  // plainer — the same reasoning `LedgersView.refreshTasks` gives.
  useEffect(() => {
    let live = true;
    setWiredTools([]);
    setWiredToolsLoaded(false);
    void listWorkflowToolSlugs(client, company)
      .then((r) => {
        if (live) {
          setWiredTools(r.slugs);
          setWiredToolsLoaded(true);
        }
      })
      .catch(() => {
        if (live) setWiredTools([]);
      });
    return () => {
      live = false;
    };
  }, [client, company]);

  /**
   * Applies a server response.
   *
   * Only the draft for the field that was just saved is resynchronised: the
   * server's value is authoritative for what the gate is enforcing, but
   * overwriting a box the operator was part-way through typing silently
   * discards their edit. `saveAlways` keeps a half-typed cap or deadline, and
   * `saveSpendCap`/`saveDeadline` keep an unsaved always-ask list — the same
   * separation the `PUT` bodies have. A reset replaces the whole override, so
   * it resynchronises everything.
   *
   * `takesEffect` overrides the host's generic timing line for a save whose
   * effect does not wait for the next turn — the deadline, whose new TTL the
   * live gate enforces immediately.
   *
   * **Returns whether the write actually landed.** It is not a formality: a
   * body this rejects is a FAILED write, and its callers hand that answer to
   * confirmation dialogs which close on success and stay open for a retry on
   * failure. Returning nothing let `saveTier`, `reset` and `commitSpendCap`
   * report `true` after showing an error, so a tier escalation, a loosening
   * reset or a spend-cap raise that the host answered with rubbish closed its
   * dialog as though the operator's change had been made — a *widening* the
   * console then claimed had happened and had not.
   */
  const apply = (
    next: PolicyStatus,
    message: string,
    resync: { alwaysAsk?: boolean; spendCap?: boolean; deadline?: boolean } = {},
    takesEffect?: string,
  ): boolean => {
    // Every write path funnels through here, so this is the one place the
    // settings page has to fence: a PUT or DELETE that answers 200 with
    // something that is not a policy must not be put on screen. Reported the
    // way a failed save is, and the previously loaded policy stands.
    if (!isPolicyStatus(next)) {
      toast.error(NOT_A_POLICY);
      return false;
    }
    setStatus(next);
    // The title row reads the same policy through `useAutonomy`, on a 30s
    // poll, and it is mounted on every view including this one. Without this
    // hand-off a change made HERE left the pill an inch above the card stating
    // the previous tier for up to half a minute — and in the direction that
    // matters most, a widening looks like the restrictive tier is still in
    // force. Same value, same scope, same fence: this is the host's own
    // response, already checked by `isPolicyStatus` above, so the row is
    // handed a value the host returned rather than an optimistic guess.
    applyAutonomy(client, company, next);
    const { alwaysAsk = true, spendCap = true, deadline = true } = resync;
    if (alwaysAsk) {
      setDraftAlways(next.alwaysApprove.join(", "));
      setDirty(false);
    }
    if (spendCap) {
      setDraftSpend(next.autoApproveUnderUsd?.toString() ?? "");
      setNoSpendCap(next.autoApproveUnderUsd === null);
    }
    if (deadline) {
      setDraftDeadline((next.approvalTtlHours ?? 24).toString());
    }
    toast.success(message, { description: takesEffect ?? next.takesEffect });
    return true;
  };

  const saveTier = async (mode: string) => {
    if (!status || saving || mode === status.mode) return false;
    setSaving(true);
    try {
      // Only `mode` is sent: an omitted field leaves the always-ask list where
      // it is, so picking a tier cannot silently discard a list the operator
      // edited earlier.
      // `dirty` means the operator has unsaved list edits; keep them. The tier
      // request touches neither the cap nor the deadline, so their drafts stay
      // too.
      const next = await setPolicy(client, company, { mode });
      // A save for a company this card no longer shows must not overwrite the
      // current company's state — the read path's `live` guard, applied to the
      // write path.
      if (!isCurrentScope({ client, company })) return false;
      // `return apply(...)`, not `apply(...); return true`. A rejected body is
      // a failed write, and the confirmation dialog behind a tier escalation
      // has to stay open for the retry rather than close on a change that did
      // not happen.
      return apply(next, "Autonomy tier updated", {
        alwaysAsk: !dirty,
        spendCap: false,
        deadline: false,
      });
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Could not change the tier.",
      );
      return false;
    } finally {
      setSaving(false);
    }
  };

  const chooseTier = (tier: PolicyStatus["tiers"][number]) => {
    if (!status || saving || tier.value === status.mode) return;
    if (widensAutonomy(status.tiers, status.mode, tier.value)) {
      confirmSource.current = "tier";
      setPendingCapRaise(null);
      setTierAwaitingConfirmation(tier);
      return;
    }
    void saveTier(tier.value);
  };

  // Only a successfully loaded tool set may flag an entry: while the request is
  // pending, and on hosts predating the route, the empty array is "unknown", not
  // "none of these are wired".
  //
  // The best set to compare against is the policy response's `knownTools` — the
  // complete gateable registry, which is broader than the workflow tool set
  // served by `/workflows/tool-slugs`: an agent may be wired a tool that cannot
  // be a workflow node (`hosting_launch_site`, `publish_artifact`), and the
  // gate matches it by name. Comparing against the workflow subset alone would
  // call such a fence a mistake. So when the host serves the complete registry
  // the note is confident; a host predating it falls back to the workflow set,
  // and the note scopes itself to what that set can prove.
  //
  // An entry counts as matching when it would gate a known tool under the
  // backend's own matcher (`SHELL` for the `shell` tool, `invoice` for a
  // `invoice.send` kind), so a fence the gate accepts is never called a mistake
  // outright.
  const knownTools = status?.knownTools ?? null;
  const gateableSet = knownTools ?? (wiredToolsLoaded ? wiredTools : null);
  const unmatchedWiredTools = gateableSet
    ? draftAlways
        .split(",")
        .map((kind) => kind.trim())
        .filter(
          (kind) =>
            kind && !gateableSet.some((tool) => alwaysApproveGates(kind, tool)),
        )
    : [];

  /**
   * The note's wording, scoped to what `gateableSet` can prove. With the
   * complete registry the claim is confident — no tool the gate recognizes —
   * and the hedge only needs the open effect namespace. On a host predating
   * the field, the note names the workflow set it actually compared against
   * and hedges that a wired agent tool outside it may still exist.
   */
  const unmatchedNote = unmatchedWiredTools.length
    ? `${unmatchedWiredTools.join(", ")} ${
        unmatchedWiredTools.length === 1 ? "doesn't" : "don't"
      } match any ${
        knownTools
          ? "tool the approval gate recognizes"
          : "of the workflow tools wired here"
      }. ${
        unmatchedWiredTools.length === 1 ? "It may" : "They may"
      } still be ${
        knownTools
          ? unmatchedWiredTools.length === 1
            ? "a hosted effect kind"
            : "hosted effect kinds"
          : unmatchedWiredTools.length === 1
            ? "a wired agent tool or a hosted effect kind"
            : "wired agent tools or hosted effect kinds"
      }.`
    : null;

  const saveAlways = async () => {
    if (!status || saving) return;
    setSaving(true);
    try {
      // An empty box means an empty list, not "leave it alone" — the host keeps
      // those apart and so must this. Saving the list resyncs the list; a
      // half-typed cap or deadline in the other fields is the operator's, not
      // the server's, so it stays.
      const kinds = draftAlways
        .split(",")
        .map((kind) => kind.trim())
        .filter(Boolean);
      const next = await setPolicy(client, company, { alwaysApprove: kinds });
      // A save for a company this card no longer shows must not overwrite the
      // current company's state: the operator may have switched companies while
      // the PUT was in flight, and applying the stale response here would
      // replace the new card's list (and later saves would send the old
      // company's values to the new company's endpoint).
      if (!isCurrentScope({ client, company })) return;
      apply(
        next,
        "Always-ask list updated",
        { spendCap: false, deadline: false },
      );
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Could not save the list.",
      );
    } finally {
      setSaving(false);
    }
  };

  /**
   * The tier buttons, in `status.tiers` order, so the radio group's arrow keys
   * can move focus between them (a roving-tabindex group: only the checked tier
   * is in the Tab order, and arrows move and select in one step).
   */
  const tierButtons = useRef<Array<HTMLButtonElement | null>>([]);
  /**
   * Which control launched the confirmation dialog, so closing it can return
   * focus somewhere sensible. A tier escalation is opened from the radio the
   * operator pressed — which may not be the tier that ends up selected — so
   * closing re-syncs focus to the checked tier; the reset flow's trigger is a
   * plain button whose own focus restore is right. A ref, not state, because
   * the dialog's close handler reads it after `onOpenChange` has cleared the
   * confirmation state.
   */
  const confirmSource = useRef<"tier" | "reset">("tier");
  /**
   * The "Use the manifest's policy" button, so a cancelled reset-driven
   * confirmation can return focus to it (the controlled `AlertDialog` has no
   * trigger of its own for Base UI to restore).
   */
  const resetButtonRef = useRef<HTMLButtonElement | null>(null);

  const reset = async () => {
    if (!status || saving) return false;
    setSaving(true);
    try {
      const next = await resetPolicy(client, company);
      // A reset for a company this card no longer shows must not overwrite the
      // current company's state.
      if (!isCurrentScope({ client, company })) return false;
      // Propagated for the same reason `saveTier` propagates it: a loosening
      // reset is confirmed, and a rejected body must keep that confirmation up.
      return apply(
        next,
        "Reverted to the manifest's policy",
        undefined,
        // A reset lands the manifest's deadline on the live gate immediately,
        // the same way a deadline save does, and a parked card's deadline is
        // re-evaluated against the current TTL whenever it is displayed or
        // resolved — so a deadline that CHANGES on a reset is immediate in
        // both directions. A shorter one lets already-parked approvals expire
        // on the next sweep before any new turn; a longer one keeps them
        // actionable past the deadline they were parked under. Either way the
        // generic "next turn" line would misstate it, so name the change the
        // way `saveDeadline` does whenever the reset moves the deadline. The
        // `!= null` guard keeps a host that predates the deadline field on
        // the generic line, since it has no deadline to have moved.
        status.approvalTtlHours != null &&
          (status.manifestApprovalTtlHours ?? 24) !== status.approvalTtlHours
          ? "takes effect immediately — parked approvals are re-checked against the manifest deadline"
          : undefined,
      );
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Could not reset the policy.",
      );
      return false;
    } finally {
      setSaving(false);
    }
  };

  // Always-ask gates an operator added that a reset would drop — entries the
  // manifest's list does not gate. The tier-widening test misses these: the
  // tiers can agree while the lists disagree, and restoring the manifest then
  // still widens what gets through, so it earns the same confirmation and the
  // dialog names it.
  const removedAlwaysAsk =
    status?.alwaysApprove.filter(
      (entry) => !gatedBy(status.manifestAlwaysApprove, entry),
    ) ?? [];

  // A reset can also loosen the spend cap when the manifest's cap is higher
  // than the override's, which is the same widening the tier buttons confirm.
  const spendCapWidens = status
    ? widensSpendCap(
        status.autoApproveUnderUsd,
        status.manifestAutoApproveUnderUsd,
      )
    : false;

  /**
   * The "Use the manifest's policy" button. A reset that gives the company
   * *more* autonomy than the override it replaces is an escalation like any
   * other tier change, so it gets the same confirmation; so does a reset that
   * drops always-ask gates the manifest does not carry, or that restores a
   * looser spend cap. A reset that tightens or holds the tier lands
   * immediately, the way a downgrade does.
   */
  const requestReset = () => {
    if (!status || saving) return;
    // The manifest's tier can be MORE autonomous than the override an operator
    // set — resetting would restore that looser tier, so it earns the same
    // widening confirmation as picking the tier directly. So does dropping
    // always-ask gates the manifest does not carry: a reset removes the whole
    // override, and an effective entry the manifest list does not gate is a
    // fence that silently comes down even when the tiers agree.
    const manifestTier = status.tiers.find(
      (tier) => tier.value === status.manifestMode,
    );
    if (
      manifestTier &&
      (widensAutonomy(status.tiers, status.mode, status.manifestMode) ||
        removedAlwaysAsk.length > 0 ||
        spendCapWidens)
    ) {
      confirmSource.current = "reset";
      setTierAwaitingConfirmation(null);
      setPendingCapRaise(null);
      setResetAwaitingConfirmation(true);
      return;
    }
    void reset();
  };

  /** Persists a spend-cap value and resyncs the cap draft. */
  const commitSpendCap = async (cap: number | null): Promise<boolean> => {
    if (!status || saving) return false;
    setSaving(true);
    try {
      const next = await setPolicy(client, company, {
        autoApproveUnderUsd: cap,
      });
      // A save for a company this card no longer shows must not overwrite the
      // current company's state.
      if (!isCurrentScope({ client, company })) return false;
      // Propagated: a cap RAISE is confirmed, and a rejected body must keep
      // that confirmation up rather than close it on a widening that did not
      // land.
      return apply(
        next,
        "Spend cap updated",
        // An unsaved always-ask edit and a half-typed deadline are the
        // operator's; the PUT only touched the cap.
        { alwaysAsk: !dirty, deadline: false },
      );
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Could not save the spend cap.",
      );
      return false;
    } finally {
      setSaving(false);
    }
  };

  const saveSpendCap = async () => {
    if (!status || saving) return;
    const cap = Number(draftSpend);
    if (noSpendCap) {
      // Choosing "no cap" tightens: with no cap every spend parks. Straight
      // through, like any other tightening save.
      await commitSpendCap(null);
      return;
    }
    if (draftSpend.trim() === "" || !Number.isFinite(cap) || cap < 0) {
      toast.error("Enter a non-negative amount, or choose no cap.");
      return;
    }
    // Raising the cap lets more payments through without asking — the same
    // widening the tier buttons and a loosening reset confirm, so it earns the
    // same dialog. A tightening save goes straight through.
    if (widensSpendCap(status.autoApproveUnderUsd, cap)) {
      setTierAwaitingConfirmation(null);
      setResetAwaitingConfirmation(false);
      setPendingCapRaise(cap);
      return;
    }
    await commitSpendCap(cap);
  };

  const saveDeadline = async () => {
    if (!status || saving) return;
    const hours = Number(draftDeadline);
    if (!Number.isSafeInteger(hours) || hours < 1) {
      toast.error("Enter a whole number of hours, at least 1.");
      return;
    }
    setSaving(true);
    try {
      const next = await setPolicy(client, company, {
        approvalTtlHours: hours,
      });
      // A save for a company this card no longer shows must not overwrite the
      // current company's state.
      if (!isCurrentScope({ client, company })) return;
      apply(
        next,
        "Approval deadline updated",
        // An unsaved always-ask edit and a half-typed cap are the operator's;
        // the PUT only touched the deadline.
        { alwaysAsk: !dirty, spendCap: false },
        // A deadline change is not "on the next turn": the live gate enforces
        // the new TTL as soon as the save lands, and already-parked approvals
        // are judged against it on the next sweep — so shortening the deadline
        // can expire an approval sitting in the queue before any new turn.
        "takes effect immediately — parked approvals are re-checked against the new deadline",
      );
    } catch (error) {
      toast.error(
        error instanceof Error
          ? error.message
          : "Could not save the deadline.",
      );
    } finally {
      setSaving(false);
    }
  };

  /**
   * Radio-group arrow keys: move focus to the neighbour and select it in the
   * same step, the way native radios behave. Without this, every tier stays in
   * the Tab order and no Arrow key moves between them — a screen reader
   * announces radio-group controls whose keyboard behavior does not exist.
   */
  const handleTierKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (!status || saving) return;
    let step = 0;
    switch (event.key) {
      case "ArrowDown":
      case "ArrowRight":
        step = 1;
        break;
      case "ArrowUp":
      case "ArrowLeft":
        step = -1;
        break;
      default:
        return;
    }
    // Navigate from the radio that has focus, not the tier that happens to be
    // selected — the two can differ. Pressing ArrowRight on Auto focuses Full
    // and, because that is an escalation, parks the choice in a confirmation
    // dialog; when the operator cancels, focus is back on Full while Auto is
    // still selected, and the next arrow must compute from Full or it skips a
    // tier. The keydown bubbles from the focused button to this container, so
    // `event.target` is that button.
    const focused = tierButtons.current.indexOf(
      event.target as HTMLButtonElement,
    );
    if (focused === -1) return;
    // Wrap at both ends, like a radio group: ArrowUp on the first tier lands
    // on the last and ArrowDown on the last lands on the first. A bare
    // `focused + step` bounds check would dead-end the group at its edges
    // instead of looping it.
    const next = (focused + step + status.tiers.length) % status.tiers.length;
    const tier = status.tiers[next];
    if (!tier) return;
    event.preventDefault();
    tierButtons.current[next]?.focus();
    chooseTier(tier);
  };

  const manifestTier = status?.tiers.find(
    (tier) => tier.value === status.manifestMode,
  );

  return (
    <Card data-testid="policy-settings">
      <CardHeader>
        <CardTitle id="approvals-heading" className="flex items-center gap-2 text-base">
          <ShieldCheck className="h-4 w-4" />
          Approvals
        </CardTitle>
        <CardDescription>
          Execution autonomy and the deadline for explicit approval requests.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-6">
        {loading ? (
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="h-4 w-4 animate-spin" />
            Loading the current policy…
          </div>
        ) : !status ? (
          <div className="space-y-3">
            <p className="text-sm text-muted-foreground">
              {loadError ?? "Could not load the policy."}
            </p>
            {/* A manual retry is not tied to an effect cleanup, so its
                liveness must still be scope-guarded: a retry that resolves
                after the operator switched companies must not paint the new
                company's card with the old company's policy. */}
            <Button
              size="sm"
              variant="outline"
              onClick={() => void load(() => isCurrentScope({ client, company }))}
            >
              Try again
            </Button>
          </div>
        ) : (
          <>
            {!canManage && (
              <AdminOnlyNotice
                testId="policy-read-only"
                title="Only an admin can change this company's approval policy"
              >
                The tier decides how much every teammate here may do without asking
                first, so it is the company&rsquo;s to set rather than any one
                member&rsquo;s. You can see which tier is in force.
              </AdminOnlyNotice>
            )}
            <div className="rounded-md border border-status-blocked/30 bg-status-blocked-soft p-3 text-xs text-muted-foreground">
              Policy-based approval prompts are disabled. Teammates ask through{" "}
              <code>request_approval</code>; read-only mode and the emergency stop still
              hard-deny applicable calls.
            </div>
            <div
              className="space-y-2"
              role="radiogroup"
              aria-labelledby="approvals-heading"
              onKeyDown={handleTierKeyDown}
            >
              <div className="flex justify-between px-1 text-xs text-muted-foreground">
                <span>More oversight</span>
                <span>More autonomy</span>
              </div>
              {status.tiers.map((tier, index) => {
                const active = tier.value === status.mode;
                const looser = tier.value === "auto" || tier.value === "full";
                return (
                  <button
                    key={tier.value}
                    ref={(el) => {
                      tierButtons.current[index] = el;
                    }}
                    type="button"
                    disabled={saving || !canManage}
                    onClick={() => chooseTier(tier)}
                    role="radio"
                    aria-checked={active}
                    tabIndex={active ? 0 : -1}
                    data-testid={`policy-tier-${tier.value}`}
                    className={cn(
                      "w-full rounded-md border p-3 text-left transition-colors",
                      "disabled:cursor-not-allowed disabled:opacity-60",
                      looser &&
                        "border-status-blocked/40 bg-status-blocked-soft hover:bg-status-blocked-soft",
                      active
                        ? looser
                          ? "ring-1 ring-status-blocked/30"
                          : "border-primary bg-primary/5"
                        : "hover:bg-muted/50",
                    )}
                  >
                    <div className="flex items-center gap-2">
                      <span className="text-sm font-medium">{tier.label}</span>
                      {active && (
                        <Badge variant="secondary" className="text-xs">
                          Current
                        </Badge>
                      )}
                    </div>
                    <p className="mt-1 text-xs text-muted-foreground">
                      {tier.description}
                    </p>
                  </button>
                );
              })}
              <p className="text-xs text-muted-foreground">
                Takes effect {status.takesEffect}.
              </p>
            </div>

            <div className="space-y-2">
              <Label htmlFor="spend-cap">Spend approval threshold (inactive)</Label>
              <p className="text-xs text-muted-foreground">
                Stored for a future policy-HITL mode. It does not create approval
                prompts while policy HITL is disabled.
              </p>
              <div className="flex flex-wrap items-center gap-2">
                <Input
                  id="spend-cap"
                  type="number"
                  min="0"
                  step="0.01"
                  inputMode="decimal"
                  value={draftSpend}
                  disabled
                  placeholder="No cap"
                  onChange={(event) => setDraftSpend(event.target.value)}
                  className="max-w-40"
                />
                <span className="text-sm text-muted-foreground">USD</span>
                <Button
                  size="sm"
                  type="button"
                  variant={noSpendCap ? "secondary" : "outline"}
                  disabled
                  onClick={() => {
                    setNoSpendCap((current) => !current);
                    if (noSpendCap) setDraftSpend("");
                  }}
                >
                  {noSpendCap ? "No cap" : "Set no cap"}
                </Button>
                <Button size="sm" disabled onClick={() => void saveSpendCap()}>
                  Save cap
                </Button>
              </div>
            </div>

            <div className="space-y-2">
              <Label htmlFor="approval-deadline">Decline anything undecided after</Label>
              <p className="text-xs text-muted-foreground">
                Each approval stays decidable for this long before it is declined.
              </p>
              <div className="flex items-center gap-2">
                <Input
                  id="approval-deadline"
                  type="number"
                  min="1"
                  step="1"
                  inputMode="numeric"
                  value={draftDeadline}
                  disabled={saving || !canManage}
                  className="max-w-32"
                  onChange={(event) => setDraftDeadline(event.target.value)}
                />
                <span className="text-sm text-muted-foreground">hours</span>
                {canManage && (
                  <Button size="sm" disabled={saving} onClick={() => void saveDeadline()}>
                    Save deadline
                  </Button>
                )}
              </div>
            </div>

            <div className="space-y-2">
              <Label htmlFor="always-approve">Always ask first (inactive)</Label>
              {/* Issue #1226: what an entry IS, said here rather than left to
                  the placeholder. `payment.send, filing.submit,
                  external.publish` used to be the only worked example this
                  field offered — the exact three strings issue #684 deleted
                  from the shipped default because, on the harness path, none of
                  them names a tool and so none of them gated anything. An
                  operator following the suggestion got a fence that was not
                  there, confirmed by a "list updated" toast.

                  A tool name and an effect kind were never two namespaces (see
                  `src/policy/always_approve.rs`), so naming the tool case first
                  is naming the case that applies to every company running the
                  openhuman toolbelt. The prefix rule is stated because it is
                  what `always_approve::matches` implements and nothing in the
                  console said it. */}
              <p className="text-xs text-muted-foreground">
                Stored for a future policy-HITL mode. These entries do not create
                prompts now; teammates use <code>request_approval</code> explicitly.
              </p>
              <Input
                id="always-approve"
                value={draftAlways}
                disabled
                list={wiredTools.length > 0 ? "always-approve-tools" : undefined}
                placeholder={alwaysAskPlaceholder(wiredTools)}
                onChange={(event) => {
                  setDraftAlways(event.target.value);
                  setDirty(true);
                }}
              />
              {/* Suggestions, never a constraint: the effect namespace is open
                  on purpose, because a hosted brain may emit a kind this
                  repository has never seen, and a `datalist` leaves free text
                  free. Rendered only when the host served the set, so a host
                  predating the route degrades to the plain box. */}
              {wiredTools.length > 0 && (
                <datalist id="always-approve-tools">
                  {wiredTools.map((slug) => (
                    <option key={slug} value={slug} />
                  ))}
                </datalist>
              )}
              {unmatchedNote && (
                <p className="text-xs text-muted-foreground">{unmatchedNote}</p>
              )}
              {dirty && (
                <Button
                  size="sm"
                  disabled
                  onClick={() => void saveAlways()}
                >
                  Save list
                </Button>
              )}
            </div>

            {status.overridden && (
              <div className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-dashed p-3">
                <p className="text-xs text-muted-foreground">
                  Set here{status.setBy ? ` by ${status.setBy}` : ""}, overriding
                  the manifest ({status.manifestMode}). Editing{" "}
                  <code>[policy]</code> in <code>company.toml</code> clears it —
                  version control wins when it speaks.
                </p>
                {canManage && (
                  <Button
                    ref={resetButtonRef}
                    size="sm"
                    variant="outline"
                    disabled={saving}
                    onClick={() => requestReset()}
                  >
                    <RotateCcw className="mr-1 h-3 w-3" />
                    Use the manifest&apos;s policy
                  </Button>
                )}
              </div>
            )}
            <AlertDialog
              open={
                tierAwaitingConfirmation !== null ||
                resetAwaitingConfirmation ||
                pendingCapRaise !== null
              }
              onOpenChange={(open) => {
                if (!open) {
                  // A PUT/DELETE is in flight — keep the dialog up. The confirm
                  // action already stops the primitive's own Close, but Escape
                  // and outside-click still reach here; dismissing now would
                  // let the request finish (or fail) under a cancelled dialog
                  // instead of the promised retry UI. The close after a save
                  // is a state change from the `.then`, not a close request,
                  // so it is unaffected.
                  if (saving) return;
                  setTierAwaitingConfirmation(null);
                  setResetAwaitingConfirmation(false);
                  setPendingCapRaise(null);
                }
              }}
            >
              <AlertDialogContent
                // A tier escalation is opened from the radio the operator
                // pressed, which may not be the one that ends up selected —
                // cancelling leaves the old tier checked with focus on the new
                // one. Return focus to the checked tier so the roving-tabindex
                // group's next arrow key computes from the right radio. The
                // reset flow returns focus to the button that opened it — this
                // controlled dialog has no trigger of its own, so without an
                // explicit target Base UI would leave focus nowhere. A reset
                // that succeeds clears the override, so that button unmounts
                // before the dialog closes; the checked tier radio is the
                // fallback then, instead of letting focus fall out.
                finalFocus={() => {
                  const checkedIndex = status.tiers.findIndex(
                    (tier) => tier.value === status.mode,
                  );
                  const checked =
                    checkedIndex === -1
                      ? null
                      : tierButtons.current[checkedIndex] ?? null;
                  if (confirmSource.current === "reset") {
                    return resetButtonRef.current ?? checked;
                  }
                  return checked;
                }}
              >
                <AlertDialogHeader>
                  <AlertDialogTitle>{AUTONOMY_CONFIRM_TITLE}</AlertDialogTitle>
                  <AlertDialogDescription>
                    {pendingCapRaise !== null ? (
                      <>
                        {status.autoApproveUnderUsd === null
                          ? "Today every spend asks first."
                          : `Today spend under ${usd(status.autoApproveUnderUsd)} asks nothing.`}{" "}
                        {`Raising the cap to ${usd(pendingCapRaise)} lets qualifying spends under the new cap pass without asking; the daily budget still stops spending after its limit.`}
                      </>
                    ) : resetAwaitingConfirmation ? (
                      <>
                        Reverting clears the override set here and returns to
                        the manifest's{" "}
                        {manifestTier?.label ?? status.manifestMode} setting
                        {manifestTier ? ` — ${manifestTier.description}` : ""}.
                        They will use that setting on their next turn.
                        {manifestTier && (
                          <>
                            {" "}
                            {manifestTier.value !== status.mode
                              ? "This also"
                              : "This"}{" "}
                            replaces the current always-ask list with the
                            manifest's list:{" "}
                            {status.manifestAlwaysApprove.length > 0
                              ? status.manifestAlwaysApprove.join(", ")
                              : "none"}
                            {removedAlwaysAsk.length > 0 &&
                              `; ${removedAlwaysAsk.join(", ")} ${
                                removedAlwaysAsk.length === 1
                                  ? "stops"
                                  : "stop"
                              } always asking for approval`}
                            {spendCapWidens && (
                              <>
                                {removedAlwaysAsk.length > 0 ||
                                widensAutonomy(
                                  status.tiers,
                                  status.mode,
                                  status.manifestMode,
                                )
                                  ? " It also"
                                  : " This"} restores the manifest's looser
                                spend cap.
                              </>
                            )}
                            .
                          </>
                        )}
                      </>
                    ) : tierAwaitingConfirmation ? (
                      tierWideningExplanation(
                        status.tiers.find((tier) => tier.value === status.mode)
                          ?.description,
                        tierAwaitingConfirmation,
                      )
                    ) : null}
                  </AlertDialogDescription>
                  <p className="text-sm text-muted-foreground">
                    {pendingCapRaise !== null
                      ? "This threshold remains inactive while policy HITL is disabled."
                      : resetAwaitingConfirmation
                        ? "Reset restores the stored policy fields; approval prompts remain explicit."
                        : AUTONOMY_PROMPTS_NOTE}
                  </p>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel disabled={saving}>
                    {AUTONOMY_CONFIRM_CANCEL}
                  </AlertDialogCancel>
                  <AlertDialogAction
                    data-testid="policy-tier-confirm"
                    disabled={saving}
                    onClick={(event) => {
                      // The primitive's `Close` would dismiss the dialog
                      // before the PUT resolves, so prevent it and close
                      // explicitly only after a successful save — a failed
                      // persistence keeps the dialog open for a retry.
                      event.preventBaseUIHandler();
                      if (pendingCapRaise !== null) {
                        void commitSpendCap(pendingCapRaise).then((saved) => {
                          if (saved) setPendingCapRaise(null);
                        });
                      } else if (tierAwaitingConfirmation) {
                        void saveTier(tierAwaitingConfirmation.value).then(
                          (saved) => {
                            if (saved) setTierAwaitingConfirmation(null);
                          },
                        );
                      } else if (resetAwaitingConfirmation) {
                        void reset().then((saved) => {
                          if (saved) setResetAwaitingConfirmation(false);
                        });
                      }
                    }}
                  >
                    {pendingCapRaise !== null
                      ? `Raise cap to ${usd(pendingCapRaise)}`
                      : resetAwaitingConfirmation
                        ? "Revert and give more autonomy"
                        : AUTONOMY_CONFIRM_ACTION}
                  </AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          </>
        )}
      </CardContent>
    </Card>
  );
}
