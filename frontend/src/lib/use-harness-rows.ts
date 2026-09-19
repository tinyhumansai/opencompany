// Surveying this machine about a set of harnesses, and installing the adapters
// it is missing (issue #2394).
//
// Split out of `external-harnesses.tsx` when the agent page's harness picker
// became a second surface asking the same question. Only the probe half moved:
// each caller still fetches its own `GET {scope}/harnesses`, because the two
// read it under different conditions — Settings needs to tell a 404 from an
// empty list, and the agent page already holds the list for its own picker.
//
// Everything below is a fact about *this* machine at *this* moment. Nothing is
// persisted and nothing is remembered across a page load: a stored readiness
// flag would be a second source of truth that could disagree with the CLI
// actually being there, which is the failure `docs/spec/runtime/external-harnesses-ui.md`
// exists to prevent. Concurrent asks are de-duplicated inside the transport.

import { useCallback, useEffect, useRef, useState } from "react";

import { isDesktopRuntime } from "@/api/transport";
import { acpHarnesses, confirmAcpHarness, installAcpHarness } from "@/api/transport/desktop";
import type { HarnessDto } from "@/api/types";
import {
  desktopHarnessId,
  isChecking,
  joinHarnesses,
  withReadiness,
  type HarnessRow,
} from "@/lib/harnesses";

/** What {@link useHarnessRows} gives a surface to render. */
export interface HarnessRowsState {
  /**
   * The joined rows, or `null` before the first survey has produced any —
   * which is not "no harnesses", so a caller should render a waiting state
   * rather than an empty list.
   */
  rows: HarnessRow[] | null;
  /**
   * Whether a survey is running for rows not yet on screen.
   *
   * Derived from the input rather than set by the effect, so it is already
   * true in the render that first sees a new list — a flag set inside an
   * effect would leave one frame claiming to be settled when it is not.
   */
  surveying: boolean;
  /** Fetches one row's adapter and re-settles that row. Resolves to the failure, or `null`. */
  install: (row: HarnessRow) => Promise<string | null>;
  /** Which rows are installing, by the company-side id. */
  installing: Set<string>;
  /** What each row's last install failed with, by the company-side id. */
  installErrors: Record<string, string>;
}

/**
 * Joins `declared` against what this machine can actually run, then starts
 * each local CLI to settle its readiness.
 *
 * `declared` is `null` until the caller's own fetch lands. `enabled` gates the
 * whole survey: the agent picker passes `editing`, because probing on page
 * view would spawn a subprocess per harness every time anyone opened a
 * teammate.
 */
export function useHarnessRows(
  declared: HarnessDto[] | null,
  enabled = true,
): HarnessRowsState {
  const [surveyed, setSurveyed] = useState<{ source: HarnessDto[] | null; rows: HarnessRow[] | null }>({
    source: null,
    rows: null,
  });

  /**
   * Which survey a confirmation belongs to.
   *
   * Bumped whenever a new one starts, and captured by each in-flight probe. A
   * probe from a superseded run resolves against a stale generation and is
   * dropped — so a slow CLI still starting when the list is replaced cannot
   * land its answer on rows it was never about.
   */
  const generation = useRef(0);

  const [installing, setInstalling] = useState<Set<string>>(new Set());
  const [installErrors, setInstallErrors] = useState<Record<string, string>>({});

  useEffect(() => {
    if (!enabled || !declared) {
      generation.current += 1;
      setSurveyed({ source: declared, rows: null });
      return;
    }
    const run = ++generation.current;
    let cancelled = false;
    void (async () => {
      // A browser has no local probe at all, and an older desktop shell has no
      // `oc_acp_harnesses` — both yield `null`, which the join renders as
      // "can't say from here" rather than as "not installed".
      const local = isDesktopRuntime() ? await acpHarnesses().catch(() => null) : null;
      if (cancelled || generation.current !== run) return;

      const joined = joinHarnesses(declared, local);
      setSurveyed({ source: declared, rows: joined });

      // Phase two, and the only phase with an answer in it. Every row arrives
      // `checking` — the host looks nothing up — so each one is started here
      // and settled by what it does. Fired in parallel and applied as each
      // lands, so a slow CLI delays only its own row.
      await Promise.all(
        joined.filter(isChecking).map(async (row) => {
          const confirmation = await confirmAcpHarness(desktopHarnessId(row));
          // `null` means nothing could answer; the row stays on `checking`
          // rather than being given a verdict nobody reached.
          if (!confirmation || generation.current !== run) return;
          setSurveyed((current) =>
            current.rows
              ? {
                  ...current,
                  rows: withReadiness(current.rows, row.id, confirmation.readiness, confirmation.path),
                }
              : current,
          );
        }),
      );
    })();
    return () => {
      cancelled = true;
    };
  }, [declared, enabled]);

  /**
   * Fetches one harness's adapter, then re-settles that row on its own.
   *
   * Re-confirms only the row that changed rather than re-surveying: a full
   * survey would restart every other harness's CLI for an event that cannot
   * have altered them, and would reset rows the operator is reading.
   *
   * The generation guard applies here too. An install started before the list
   * was replaced must not apply its result to the newer one — the row it was
   * about may not even be in it.
   */
  const install = useCallback(async (row: HarnessRow): Promise<string | null> => {
    // Two ids in play: `row.id` keys this state, `desktopHarnessId` addresses
    // the shell's catalogue. Conflating them is what made a declared `laptop`
    // unaddressable.
    const id = row.id;
    const desktopId = desktopHarnessId(row);
    const run = generation.current;
    setInstalling((current) => new Set(current).add(id));
    setInstallErrors(({ [id]: _dropped, ...rest }) => rest);

    const failure = await installAcpHarness(desktopId);
    const settle = () => {
      setInstalling((current) => {
        const next = new Set(current);
        next.delete(id);
        return next;
      });
      return failure;
    };

    if (generation.current !== run) return settle();
    if (failure) {
      // npm's own words, kept verbatim. A rewritten message would be this
      // layer guessing at a failure it did not diagnose.
      setInstallErrors((current) => ({ ...current, [id]: failure }));
      return settle();
    }

    // Straight back to `checking`, because that is now true again: something
    // was installed and nothing has started it yet. Leaving the old verdict up
    // while the handshake runs would show "Add-on needed" on a row that just
    // got its add-on.
    setSurveyed((current) =>
      current.rows ? { ...current, rows: withReadiness(current.rows, id, { state: "checking" }) } : current,
    );
    const confirmation = await confirmAcpHarness(desktopId);
    if (confirmation && generation.current === run) {
      setSurveyed((current) =>
        current.rows
          ? { ...current, rows: withReadiness(current.rows, id, confirmation.readiness, confirmation.path) }
          : current,
      );
    }
    return settle();
  }, []);

  return {
    rows: surveyed.rows,
    surveying: enabled && declared !== null && surveyed.source !== declared,
    install,
    installing,
    installErrors,
  };
}
