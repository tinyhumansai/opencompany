import { lazy, Suspense, useCallback, useEffect, useState } from "react";
import type { Step, TourData } from "react-joyride";

import type { View } from "@/components/app-shell";
import { TOUR, waitForTarget } from "./steps";
import { TourTooltip } from "./TourTooltip";
import { WelcomeDialog } from "./WelcomeDialog";
import { RESTART_EVENT, tourForced, tourSeen, writeTourState } from "./state";

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
  setView: (view: View) => void;
}) {
  const [welcomeOpen, setWelcomeOpen] = useState(false);
  const [session, setSession] = useState(false); // mounts Joyride for the run
  const [run, setRun] = useState(false); // joyride active

  // Offer the tour once per company on first arrival (or every load under the
  // dev-force flag).
  useEffect(() => {
    // A company switch must tear down the prior company's in-flight tour first,
    // otherwise `finish` would record its completion/skip under the NEW
    // company's key (cross-company contamination).
    setRun(false);
    setSession(false);
    if (tourForced() || !tourSeen(company)) setWelcomeOpen(true);
    else setWelcomeOpen(false);
  }, [company]);

  const start = useCallback(() => {
    setWelcomeOpen(false);
    setSession(true);
    setRun(true);
  }, []);

  const finish = useCallback(
    (skipped: boolean) => {
      setRun(false);
      setSession(false);
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
      setView(stop.view);
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
