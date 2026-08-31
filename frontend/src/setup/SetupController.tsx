import { useCallback, useEffect, useRef, useState } from "react";

import type { OpenCompanyClient } from "@/api/client";
import type { TeamMemberDto } from "@/api/types";
import { useLocalScope } from "@/connections/ConnectionContext";
import type { LocalScope } from "@/connections/types";
import { shouldOfferSetup, teamIsUnstaffed } from "@/lib/company-setup";
import { ReadTimeoutError, withReadTimeout } from "@/lib/read-timeout";
import { SetupDialog } from "./SetupDialog";
import {
  clearSetupRedesign,
  clearSetupResuming,
  clearSetupSkipped,
  markSetupRedesign,
  markSetupResuming,
  markSetupSkipped,
  setupRedesign,
  setupRedesignIds,
  setupResuming,
  setupSkipped,
} from "./state";

/** Where "Set up a model" sends the operator. */
const MODEL_SETTINGS = "#/settings/connections";

/**
 * How long the gate's roster read (`client.listTeam`, just below) is allowed
 * to sit with no response at all before it is treated as unreachable (PR
 * #1875 review finding).
 *
 * The `catch` around that call already treats any rejection as "cannot tell
 * a fresh company from a staffed one, offer nothing" and settles `checked`
 * regardless — the one thing it cannot handle is a promise that never
 * settles at all. `client.listTeam` goes through `OpenCompanyClient`, whose
 * request path has no timeout of its own (`api/transport/browser.ts` calls
 * bare `fetch`, no `AbortSignal`), so a stalled proxy or a backend that
 * accepts the connection and never answers leaves `roster = await
 * client.listTeam(company)` pending forever: `checked` never becomes `true`
 * here, `onOpenChange` never fires, `AppShell`'s `setupChecked` never flips,
 * and `shouldHoldShellPending`'s `!input.setupChecked` branch holds the shell
 * with no way out — there is no stuck counter on this axis at all, only the
 * one settlement this read never reaches. `withReadTimeout` turns that
 * silence into an ordinary rejection at this bound, which the existing
 * `catch` below already handles exactly like any other unreachable host.
 *
 * That reuse has its own edge (PR #1875 review, round 19): a read that is
 * merely slow rather than actually stuck — a cold host still building its
 * roster index, a proxy hiccup — now times out at this bound too, and the
 * `catch` cannot tell "genuinely unreachable" apart from "would have
 * answered a moment later." Unlike `getActivation` (`useActivationGate.ts`),
 * `listTeam` has no error classifier to separate a terminal answer from a
 * transient one, so `readRoster` below gives a `ReadTimeoutError` — and only
 * that error — a single retry before falling into the same "offer nothing"
 * catch. A real 404/500/network failure is still immediate, matching this
 * file's original, pre-timeout behavior for a host that plainly cannot serve
 * the roster.
 */
const SETUP_ROSTER_TIMEOUT_MS = 20000;

/**
 * Reads the roster once, retrying exactly once more if — and only if — the
 * first attempt hit `SETUP_ROSTER_TIMEOUT_MS` without settling at all.
 *
 * `withReadTimeout` cannot tell "unreachable" from "slow": it races the same
 * bound either way, so a host that would have answered at 21s looks
 * identical, from here, to one that never answers. Before this retry, that
 * meant a genuinely-slow-but-healthy read fell straight into the `catch`
 * below and reported "cannot tell a fresh company from a staffed one, offer
 * nothing" — worse than the pre-timeout behavior, which simply waited as
 * long as the read took and got the right answer. A second attempt, bounded
 * by the same timeout, gives that read one more chance before this function
 * gives up the same way the original code always did for a definite failure.
 *
 * Any error that is not a `ReadTimeoutError` — a 404, a 500, a network
 * failure — still propagates on the first attempt with no retry: those are
 * answers, not silence, and retrying an answer this file has always trusted
 * would risk masking a real backend fault as one more timeout.
 */
async function readRoster(
  client: OpenCompanyClient,
  company: string | null,
): Promise<TeamMemberDto[]> {
  try {
    return await withReadTimeout(client.listTeam(company), SETUP_ROSTER_TIMEOUT_MS);
  } catch (err) {
    if (!(err instanceof ReadTimeoutError)) throw err;
    return await withReadTimeout(client.listTeam(company), SETUP_ROSTER_TIMEOUT_MS);
  }
}

/** Whether the operator is still on the page they left setup for. */
function onModelSettings(): boolean {
  return window.location.hash.startsWith(MODEL_SETTINGS);
}

/**
 * Settle the persisted redesign debt against the roster read on return.
 *
 * A redesign names the team the first pass created. That team may have been
 * deleted or replaced by another operator while model settings were open — the
 * return re-reads the roster precisely because the answer captured at leave
 * time is a snapshot, not a fact. Reopening redesign against a boundary that no
 * longer exists would build a full roster and sweep nothing (the recorded rows
 * are gone), stacking a second team over the concurrent work.
 *
 * Returns the recorded ids that still exist, re-keying the debt to them when
 * some vanished, or `null` when none survive and the debt is cancelled. A debt
 * that recorded no ids is uncheckable and kept as it is — the empty list means
 * there was no fallback team to replace, so nothing here can prove it gone.
 */
function reconcileRedesign(scope: LocalScope, roster: TeamMemberDto[]): string[] | null {
  const recorded = setupRedesignIds(scope);
  if (!recorded.length) return recorded;
  const present = new Set(roster.map((member) => member.id));
  const survivors = recorded.filter((id) => present.has(id));
  if (!survivors.length) {
    clearSetupRedesign(scope);
    return null;
  }
  if (survivors.length !== recorded.length) {
    markSetupRedesign(scope, survivors);
  }
  return survivors;
}

/**
 * Decides whether first-run setup opens, and gets out of the way once it has
 * (docs/spec/runtime/company-setup.md).
 *
 * Mounted once inside `AppShell` beside `TourController`, so it overlays every
 * view. The two are sequenced rather than independent: setup runs **first** and
 * the tour waits, because a tour of an unstaffed company walks someone through
 * empty pages — the exact first impression this feature exists to fix. The
 * `onOpenChange` callback is how the shell tells the tour to hold.
 *
 * ## The gate
 *
 * Open when nobody has staffed this company and the operator has not skipped.
 * The test is the host's answer, not a stored flag, so it cannot drift from the
 * thing setup changes — see `shouldOfferSetup` for why a browser flag would be
 * unsafe for this and is fine for the skip.
 *
 * "Staffed" is narrower than "has a roster", and the difference is the whole of
 * issue #1404: the global baseline puts undeletable teammates on **every**
 * company, so an emptiness test never answered `true` anywhere and this dialog
 * could not open in the shipped product. `teamIsUnstaffed` reads the host's
 * per-row provenance instead.
 *
 * A company whose manifest names agents of its own therefore never sees the
 * offer. That is deliberate: it came with a team, so there is nothing to set up.
 * `force` is how the Team page's in-place prompt reopens it anyway, and
 * `routeOpen` is how the manual `#/setup` recovery address does — the only two
 * openers that work for a staffed company or after a skip.
 */
export function SetupController({
  client,
  company,
  force,
  routeOpen,
  deepLinked,
  onForceHandled,
  onOpenChange,
  onCompleted,
  onRouteDismiss,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /** Opened by hand from the Team page's prompt, regardless of the skip flag. */
  force?: boolean;
  /**
   * Whether the `#/setup` recovery address is on screen.
   *
   * The shell clears `force` the moment this controller consumes it, so a
   * dialog a Back pressed from `#/setup` would otherwise leave open over the
   * page the address bar now names. Closed on the true→false edge only — never
   * because the route is merely absent — so a dialog the first-run gate or the
   * Team prompt opened, with no `#/setup` involved, is not yanked away.
   */
  routeOpen?: boolean;
  /**
   * The operator arrived on a view they named, so do not open unprompted.
   *
   * A blocking dialog is an *offer* when someone lands on the console with
   * nowhere particular to be, and a *hijack* when they deep-linked to
   * `#/workflows/x`. They asked for that page; the Team page's in-place prompt
   * is the affordance a deliberate navigation should meet instead.
   *
   * This does not suppress `unstaffed` reporting — the tour still holds and the
   * Team prompt still shows.
   */
  deepLinked?: boolean;
  /** Clears the caller's force flag once the dialog has taken it. */
  onForceHandled?: () => void;
  /**
   * Fires whenever the tour should hold: while the dialog is open, and while the
   * company still has nobody on it.
   *
   * Emptiness and not just openness, because skipping setup would otherwise pop
   * the tour's welcome straight over an unstaffed company — a walkthrough of
   * empty pages, which is the first impression this whole feature exists to
   * replace. Held until there is a team to show.
   */
  onOpenChange?: (open: boolean) => void;
  /** Setup finished and created a team — the roster reads should refresh. */
  onCompleted?: () => void;
  /** Leaves the manual `#/setup` route after skip or completion. */
  onRouteDismiss?: () => void;
}) {
  const scope = useLocalScope();
  const [open, setOpen] = useState(false);
  /**
   * Whether the gate has been evaluated for this (connection, company).
   *
   * Without it a company switch would leave the previous company's answer in
   * place: the roster read is async, so the dialog would either linger over a
   * staffed company or fail to open over an empty one until the fetch landed.
   */
  const [checked, setChecked] = useState(false);
  /**
   * Whether the host says nobody has staffed this company, independent of the
   * skip flag. The global baseline does not count — see `teamIsUnstaffed`.
   */
  const [unstaffed, setUnstaffed] = useState(false);
  /**
   * Whether the dialog should open in **redesign** mode — the first pass shipped
   * a fallback team, the operator went to wire a model, and the next build-out
   * must replace that team rather than stack a second one on it.
   */
  const [redesigning, setRedesigning] = useState(false);
  /**
   * Whether the gate has already been evaluated once in this mount.
   *
   * Setup opens **unprompted only on the first evaluation**, never again on a
   * later company switch. A switch is navigation, not a first run: an operator
   * who deep-links into `#/workflows/x` on a company that happens to have no
   * team asked for that page, and a blocking modal over it is a hijack rather
   * than an offer. The Team page's in-place prompt still covers those companies,
   * which is the affordance a deliberate navigation should meet.
   *
   * A ref, not state: it must be true before the next evaluation's render, and
   * it must not itself trigger one.
   */
  const evaluatedOnce = useRef(false);

  // Report only once the roster read has landed.
  //
  // Reporting on mount would say "nothing to hold for" before we know, and the
  // tour would open its welcome in the gap — two dialogs stacked on the first
  // screen an operator ever sees. The shell therefore starts held and waits for
  // this, so the quiet case is a tour that opens a beat late rather than one
  // that flashes over setup.
  useEffect(() => {
    if (!checked) return;
    onOpenChange?.(open || unstaffed);
  }, [checked, open, unstaffed, onOpenChange]);

  // Re-evaluate the gate whenever the addressed company changes.
  useEffect(() => {
    let cancelled = false;
    setChecked(false);
    setOpen(false);

    (async () => {
      let roster: TeamMemberDto[] = [];
      try {
        roster = await readRoster(client, company);
      } catch {
        // A host with no roster surface, or one we cannot reach. Offer nothing:
        // a setup flow that cannot read the team cannot tell a fresh company
        // from a staffed one, and guessing risks a duplicate team.
        if (!cancelled) setChecked(true);
        return;
      }
      if (cancelled) return;
      const first = !evaluatedOnce.current;
      evaluatedOnce.current = true;
      const empty = teamIsUnstaffed(roster);
      setUnstaffed(empty);
      // A console reloaded after wiring a provider — a restart is a thing that
      // page asks for — lands here rather than on a `hashchange`, so both debts
      // are honoured on this path too. Not while still *on* that page: the
      // operator has not come back yet, and the listener below is what notices
      // when they do.
      const wasResuming = setupResuming(scope);
      const wasRedesigning = setupRedesign(scope);
      const returned = (wasResuming || wasRedesigning) && !onModelSettings();
      // A reload on the destination page is not a return, and consuming the
      // resume there would strand the later return. On a reload after navigating
      // back, the resume is paid here and the dialog opens immediately. The
      // redesign debt is *not* paid by the return that reopens it — it names
      // the team to replace, and a reload or crash between this reopen and the
      // build-out must still find it, or the fallback team would leave the gate
      // reporting staffed and the owed redesign would be unreachable.
      if (returned) {
        if (wasResuming) clearSetupResuming(scope);
      }
      // The redesign debt is only good while the team it names still exists. A
      // return that finds that team gone — another operator deleted or replaced
      // it while model settings were open — must not reopen a redesign that
      // would sweep nothing and stack a second team over the concurrent work.
      const redesignOwed =
        returned && wasRedesigning && reconcileRedesign(scope, roster) !== null;
      const resume = returned && (redesignOwed || empty);
      setRedesigning(redesignOwed);
      // Only the first evaluation may open the dialog by itself; see
      // `evaluatedOnce`. Later switches still report `unstaffed`, so the tour
      // keeps holding and the Team page keeps prompting.
      // The reset at the beginning of this effect closes the previous
      // company's dialog. Once this roster read has started, only open here
      // for the automatic first-run offer or an owed return; do not close a
      // dialog that an explicit recovery action (`#/setup` or Settings) opened
      // while the read was in flight (issue #1417).
      if (
        resume ||
        (first && !deepLinked && shouldOfferSetup({ roster, skipped: setupSkipped(scope) }))
      ) {
        setOpen(true);
      }
      setChecked(true);
    })();

    return () => {
      cancelled = true;
    };
  }, [client, company, scope, deepLinked]);

  // Bring the operator back when they return from wiring a model.
  //
  // The navigation away is a hash change and so is the navigation back, and this
  // controller sees neither through its own props — hence a listener rather than
  // an effect keyed on the route. The roster is re-read on arrival rather than
  // trusted from state captured before the navigation: another tab or colleague
  // may have staffed the company in the meantime, and a return must never open
  // setup over a team that already exists. The resume debt is cleared only when
  // the return is actually handled — a roster read that fails transiently keeps
  // it, so a later navigation or reload can retry the resume. The redesign debt
  // is cleared only when the redesign resolves or is explicitly declined, not on
  // the return: its whole point is to survive a reload between the reopen and
  // the replacement build-out.
  useEffect(() => {
    // A return handled while the company is switching is a return handled for a
    // company the controller no longer renders. The listener is removed on the
    // switch but the in-flight roster read is not, so without this guard the
    // callback would reopen setup over the *new* company — and the dialog it
    // opened would then run replacement against that company's roster. Mirror
    // the gate read above: cleanup marks the read stale, the callback checks.
    let cancelled = false;
    const arrive = () => {
      if (onModelSettings()) return;
      const wasResuming = setupResuming(scope);
      const wasRedesigning = setupRedesign(scope);
      if (!wasResuming && !wasRedesigning) return;
      void client
        .listTeam(company)
        .then((roster) => {
          if (cancelled) return;
          // The return is handled only with the roster read in hand. Consuming
          // the resume before it could not tell a transient failure from a
          // handled return: the dialog would stay shut (correctly — the roster
          // is unknown) but the resume would be gone, reachable again only
          // through the Company-page prompt. The redesign debt stays whatever
          // the read says — see `clearSetupRedesign` for why the return does
          // not pay it.
          if (wasResuming) clearSetupResuming(scope);
          const empty = teamIsUnstaffed(roster);
          setUnstaffed(empty);
          if (wasRedesigning) {
            // The first pass shipped a fallback team and the operator went to
            // wire a model. Reopen in redesign mode so the next build-out
            // replaces that team instead of stacking a second one — but only
            // while that team still exists. Another operator may have deleted
            // or replaced it while model settings were open; reopening against
            // a vanished boundary would sweep nothing and stack a second team
            // over their work, so a stale debt falls through to the ordinary
            // return (open only over an unstaffed company).
            if (reconcileRedesign(scope, roster) !== null) {
              setRedesigning(true);
              setOpen(true);
            } else if (empty) {
              setOpen(true);
            }
          } else if (empty) {
            setOpen(true);
          }
        })
        .catch(() => {
          // Cannot confirm what the roster looks like; opening setup over an
          // unknown team risks a duplicate, so stay closed. The debt is kept,
          // so a later navigation or reload can retry the resume.
        });
    };
    window.addEventListener("hashchange", arrive);
    return () => {
      cancelled = true;
      window.removeEventListener("hashchange", arrive);
    };
  }, [scope, company, client]);

  // The Team page's prompt reopens setup after a skip.
  useEffect(() => {
    if (!force) return;
    setOpen(true);
    onForceHandled?.();
  }, [force, onForceHandled]);

  // Leaving `#/setup` closes the dialog the address opened.
  //
  // The shell clears `force` once this controller has taken it, so `open` would
  // otherwise survive the route that asked for it: a Back from `#/setup` would
  // leave the blocking dialog over Settings while the address bar says Settings
  // (issue #1417 review). Closed on the true→false edge, never while the route
  // holds — and never because the route is absent, which would dismiss a dialog
  // the first-run gate or the Team prompt opened.
  const routeOpenRef = useRef(routeOpen);
  useEffect(() => {
    if (routeOpenRef.current && !routeOpen) setOpen(false);
    routeOpenRef.current = routeOpen;
  }, [routeOpen]);

  const skip = useCallback(() => {
    markSetupSkipped(scope);
    // "I'll do this later" on a reopened redesign is an explicit decline of the
    // owed replacement; the debt must not bring the dialog back after it.
    clearSetupRedesign(scope);
    setOpen(false);
    setRedesigning(false);
    if (routeOpen) onRouteDismiss?.();
  }, [routeOpen, onRouteDismiss, scope]);

  /**
   * Close for a navigation that is *part of* setup, recording nothing.
   *
   * Following "Set up a model" is the operator starting this flow, not
   * declining it. Routing that through `skip` persisted an "I'll do this later"
   * they never expressed, so on return the company was still unstaffed and the
   * dialog no longer offered itself.
   *
   * Not recording the skip is only half of it, though, and the other half is
   * why this records something of its own. This controller stays mounted across
   * hash changes, its gate re-evaluates only on `(client, company, scope,
   * deepLinked)`, and `evaluatedOnce` bars a second unprompted open — so on the
   * operator's return nothing would reopen the dialog either way, and the flow
   * they had just gone to enable would still be reachable only through the Team
   * page's separate prompt. `markSetupResuming` is the debt; the effect below
   * pays it.
   */
  const leave = useCallback(() => {
    markSetupResuming(scope);
    setOpen(false);
    setRedesigning(false);
  }, [scope]);

  const done = useCallback(() => {
    // Clear the skip so it cannot outlive what it was suppressing: an operator
    // who skipped, later ran setup, then removed every agent should be offered
    // setup again rather than left on an empty team page.
    clearSetupSkipped(scope);
    setOpen(false);
    // The route must be left before the completion handler chooses the staffed
    // company's destination (Company). Skip has no completion handler, so it
    // still lands on Overview through the same callback.
    if (routeOpen) onRouteDismiss?.();
    // The team exists now, so the tour has something to walk through.
    setUnstaffed(false);
    setRedesigning(false);
    onCompleted?.();
  }, [scope, onCompleted, routeOpen, onRouteDismiss]);

  /**
   * The completion screen's "Add a model in Settings" action.
   *
   * Distinct from [`done`] even though both close the dialog: this one leaves
   * the fallback team in place and records a redesign debt — naming the exact
   * team to replace — so when the operator returns from wiring a model the
   * dialog reopens in redesign mode and that team is replaced rather than a
   * second one stacked on it.
   */
  const redesign = useCallback((fallbackIds: string[]) => {
    markSetupRedesign(scope, fallbackIds);
    setOpen(false);
  }, [scope]);

  /**
   * The completion screen's in-place "Try again" for a fallback a retry could
   * fix. Same debt as [`redesign`] — the fallback team is owed a replacement —
   * but the dialog stays open: the retry continues here, and the debt only
   * matters if a reload or crash interrupts it before the replacement lands.
   */
  const retry = useCallback(
    (fallbackIds: string[]) => {
      markSetupRedesign(scope, fallbackIds);
    },
    [scope],
  );

  /**
   * Settle the persisted redesign debt when a replacement build-out changes
   * what a future replacement must sweep.
   *
   * `null` means the owed redesign is done — a designed replacement landed, so
   * the whole record is cleared, exactly as completing setup would. A non-empty
   * list re-keys the debt to those rows: the new fallback when a replacement
   * fell back again, or the expanded boundary when a partial replacement was
   * rolled back but a rollback removal failed. Either way the next return (or
   * reload) replaces those rows rather than the swept ones.
   */
  const onReplacementComplete = useCallback(
    (fallbackIds: string[] | null) => {
      if (fallbackIds) {
        markSetupRedesign(scope, fallbackIds);
      } else {
        clearSetupSkipped(scope);
      }
    },
    [scope],
  );

  if (!checked && !force) return null;
  if (!open) return null;

  return (
    <SetupDialog
      open={open}
      client={client}
      company={company}
      redesign={redesigning}
      fallbackIds={setupRedesignIds(scope)}
      onSkip={skip}
      onLeave={leave}
      onDone={done}
      onRedesign={redesign}
      onRetry={retry}
      onReplacementComplete={onReplacementComplete}
    />
  );
}
