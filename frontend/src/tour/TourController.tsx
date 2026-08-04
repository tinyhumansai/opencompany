import { lazy, Suspense, useCallback, useEffect, useState } from "react";
import type { Step, TourData } from "react-joyride";

import type { View } from "@/components/app-shell";
import { TOUR, waitForTarget } from "./steps";
import { TourTooltip } from "./TourTooltip";
import { WelcomeDialog } from "./WelcomeDialog";
import {
  clearTourResume,
  readTourResume,
  RESTART_EVENT,
  setActiveTourStop,
  tourForced,
  tourSeen,
  writeTourState,
} from "./state";

// react-joyride (and its floater/popper deps) is a fair chunk; only operators
// who actually run the tour download it. Types above are `import type`, so they
// erase and don't pull the module eagerly.
const Joyride = lazy(() => import("react-joyride").then((m) => ({ default: m.Joyride })));

// react-joyride status values as string literals, so we don't statically import
// the runtime `STATUS` enum (which would defeat the lazy load).
const STATUS_FINISHED = "finished";
const STATUS_SKIPPED = "skipped";

const STEPS: Step[] = TOUR.map((s) => ({
  target: s.target,
  title: s.title,
  content: s.body,
  placement: s.placement,
  // v3 renamed `disableBeacon` → `skipBeacon`; open straight to the tooltip.
  skipBeacon: true,
  // Let react-joyride wait for the target to mount after we switch views — this
  // covers both the route swap and lazy Suspense chunks (Workflows, Usage, …),
  // so we don't need to hand-roll the wait.
  targetWaitTimeout: 6000,
  // A walkthrough shouldn't trap keyboard focus inside the tooltip.
  disableFocusTrap: true,
}));

/**
 * Owns the onboarding lifecycle: the one-time welcome dialog, then the
 * react-joyride spotlight. Mounted once inside `AppShell` (a sibling of the
 * feedback dialog) so it overlays every view and can drive `setView` itself.
 *
 * We let react-joyride own the stepping. Its `before` hook fires right before
 * each step renders — we use it to switch the console to that step's view, and
 * joyride then waits (`targetWaitTimeout`) for the target to mount before
 * spotlighting. That delegates the cross-view + lazy-Suspense timing to
 * joyride's tested machinery instead of a hand-rolled controlled `stepIndex`.
 */
export function TourController({
  company,
  setView,
}: {
  company: string | null;
  /** `sub` names a section's sub-page, e.g. `#/settings/connections`. */
  setView: (view: View, sub?: string) => void;
}) {
  const [welcomeOpen, setWelcomeOpen] = useState(false);
  const [session, setSession] = useState(false); // mounts Joyride for the run
  const [run, setRun] = useState(false); // joyride active
  // Which stop this run opens on. `0` for a normal run; the resumed stop when
  // we came back from a full-page redirect mid-tour.
  const [startIndex, setStartIndex] = useState(0);

  // Offer the tour once per company on first arrival (or every load under the
  // dev-force flag) — unless we're returning mid-tour from a redirect, in which
  // case pick the tour back up where it left off instead of re-offering it.
  useEffect(() => {
    // A company switch must tear down the prior company's in-flight tour first,
    // otherwise `finish` would record its completion/skip under the NEW
    // company's key (cross-company contamination).
    setRun(false);
    setSession(false);
    setActiveTourStop(null);

    // Resume takes priority over the welcome dialog: the operator already
    // started the tour, left to authorize a connection, and came back. Showing
    // "Welcome to your company" from step 1 here is the bug (issue #300) — the
    // tour never recorded completed/skipped, so `tourSeen` is still false.
    const resumeView = readTourResume(company);
    if (resumeView !== null) {
      // Consume it either way: a marker that can't be honored must not sit
      // around waiting to fire on some later, unrelated visit.
      clearTourResume(company);
      // Match on view, not index — see `TourResume`. `findIndex` takes the
      // first stop for a view, which is unambiguous for every view that can
      // arm a marker today (only Connections navigates the page away).
      const index = TOUR.findIndex((s) => s.view === resumeView);
      if (index >= 0) {
        setStartIndex(index);
        setWelcomeOpen(false);
        setSession(true);
        setRun(true);
        return;
      }
      // -1: the stop was retired since the marker was written (as #302 retired
      // one). Drop it and fall through to today's behavior.
    }

    setStartIndex(0);
    if (tourForced() || !tourSeen(company)) setWelcomeOpen(true);
    else setWelcomeOpen(false);
  }, [company]);

  const start = useCallback(() => {
    // A replay from Settings always opens at the top, even if this mount had
    // resumed mid-tour.
    setStartIndex(0);
    setWelcomeOpen(false);
    setSession(true);
    setRun(true);
  }, []);

  const finish = useCallback(
    (skipped: boolean) => {
      setRun(false);
      setSession(false);
      setActiveTourStop(null);
      // Replaces the whole record, so any resume marker goes with it — the tour
      // is over, there is nothing left to resume.
      writeTourState(company, skipped ? { skipped: true } : { completed: true });
    },
    [company],
  );

  const handleSkip = useCallback(() => {
    setWelcomeOpen(false);
    writeTourState(company, { skipped: true });
  }, [company]);

  // "Replay product tour" from Settings clears the flag and dispatches
  // RESTART_EVENT; jump straight into the tour (no welcome dialog).
  useEffect(() => {
    window.addEventListener(RESTART_EVENT, start);
    return () => window.removeEventListener(RESTART_EVENT, start);
  }, [start]);

  // Before each step renders, switch the console to that step's view AND wait
  // for its target to actually be in the DOM before letting joyride proceed.
  // joyride awaits this hook, so a content anchor on a just-navigated view
  // (e.g. the chat composer on Conversation) is spotlighted only once mounted —
  // otherwise joyride checks too early, finds nothing, and skips the step.
  // `setView` is idempotent (no-op when already there), so this is safe on Back.
  const before = useCallback(
    async (data: TourData) => {
      const stop = TOUR[data.index];
      if (!stop) return;
      // Publish the live position so `armTourResume` can persist it if this
      // stop hands the browser off to a third party (the OAuth connect flow).
      setActiveTourStop(stop.view);
      setView(stop.view, stop.sub);
      await waitForTarget(stop.target);
    },
    [setView],
  );

  // End the tour (recording completed vs skipped) when joyride reports it done.
  const after = useCallback(
    (data: TourData) => {
      if (data.status === STATUS_FINISHED || data.status === STATUS_SKIPPED) {
        finish(data.status === STATUS_SKIPPED);
      }
    },
    [finish],
  );

  return (
    <>
      <WelcomeDialog
        open={welcomeOpen}
        onOpenChange={setWelcomeOpen}
        onStart={start}
        onSkip={handleSkip}
      />
      {session && (
        <Suspense fallback={null}>
          <Joyride
            steps={STEPS}
            run={run}
            // Resume support without giving up the uncontrolled design: this is
            // the *initial* index for an uncontrolled tour, so joyride still
            // owns the stepping from here. (A controlled `stepIndex` would make
            // us drive every transition by hand.)
            initialStepIndex={startIndex}
            continuous
            tooltipComponent={TourTooltip}
            options={{
              zIndex: 1200,
              overlayColor: "rgba(0,0,0,0.45)",
              spotlightPadding: 6,
              arrowSize: 0,
              before,
              after,
            }}
          />
        </Suspense>
      )}
    </>
  );
}
