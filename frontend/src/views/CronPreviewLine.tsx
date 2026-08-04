// Echoes back what a trigger's cron schedule actually means (issue #262).
//
// A schedule's failure modes split in two. The malformed one was already
// handled — the host rejects it with the parser's message. The one that was
// not is the SUCCESSFUL save: `0 9 * * *` and `9 0 * * *` are both valid and
// nine hours apart, and the dialect is UTC, so an author in IST who wants a 9am
// report writes `0 9 * * *` and gets one at 14:30 local. Neither mistake is
// invalid, so no amount of validation catches either. Reading the schedule back
// does.
//
// The parsing lives on the host: `CronExpr` is already the one dialect the
// scheduler and the graph validator both speak, so a second parser here would
// be exactly the duplicated-rule problem issue #260 is about. (OpenHuman
// humanises cron client-side in `app/src/lib/flows/cron.ts` — a deliberate
// divergence: it has no server round trip to spend, and no host-side cron
// describer to reuse.)

import { useEffect, useState } from "react";

import { previewCron, type CronPreview } from "@/api/workflows";
import type { OpenCompanyClient } from "@/api/client";

/** How long the author must pause before a preview is fetched.
 *
 * Long enough that typing a 5-field expression is one request rather than one
 * per character, short enough to feel live. The gate at the call site (a
 * 5-field shape check) already keeps a half-written expression off the wire. */
const PREVIEW_DEBOUNCE_MS = 350;

/**
 * The host's reading of `expr`, refreshed as the author types.
 *
 * Returns `null` while there is nothing to show — disabled, not yet fetched, or
 * the request failed. A failure is deliberately indistinguishable from "no
 * preview": this is a convenience, and a host that cannot answer must not turn
 * into an error message on a field the author is still filling in.
 */
function useCronPreview(
  client: OpenCompanyClient,
  company: string | null,
  expr: string,
  enabled: boolean,
): CronPreview | null {
  const [preview, setPreview] = useState<CronPreview | null>(null);

  useEffect(() => {
    if (!enabled || !expr.trim()) {
      setPreview(null);
      return;
    }
    // `live` guards against an out-of-order response overwriting a newer one:
    // the author keeps typing, so several previews can be in flight, and the
    // slowest must not win. Same shape as the dialog's roster load.
    let live = true;
    const timer = window.setTimeout(() => {
      void (async () => {
        try {
          const result = await previewCron(client, company, expr);
          if (live) setPreview(result);
        } catch {
          // Network failure, or a host predating this route. Show nothing —
          // never block authoring on a preview.
          if (live) setPreview(null);
        }
      })();
    }, PREVIEW_DEBOUNCE_MS);

    return () => {
      live = false;
      window.clearTimeout(timer);
    };
  }, [client, company, expr, enabled]);

  return preview;
}

/** `Mon 3 Aug, 09:00` in UTC. */
function formatUtc(ms: number): string {
  return new Date(ms).toLocaleString(undefined, {
    timeZone: "UTC",
    weekday: "short",
    day: "numeric",
    month: "short",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

/**
 * The same instant in the viewer's own zone — `14:30`, or `Tue 14:30` when the
 * offset pushes it onto a different day.
 *
 * Rendered from the SAME epoch millis as {@link formatUtc}, which is why the
 * two readings cannot drift: they are one number formatted twice, not two
 * computations that have to agree. The day is included only when it differs,
 * because that is exactly when omitting it would mislead.
 */
function formatLocal(ms: number): string {
  const at = new Date(ms);
  const time = at.toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
  const sameDay =
    at.toLocaleDateString(undefined, { timeZone: "UTC" }) === at.toLocaleDateString();
  if (sameDay) return time;
  return `${at.toLocaleDateString(undefined, { weekday: "short" })} ${time}`;
}

/**
 * A one-line reading of `schedule`, under the cron field.
 *
 * Four states, all of them quiet:
 *
 * - the host describes the schedule → the description plus the next run in UTC
 *   and in the viewer's zone;
 * - the host declines to describe it → the next three fire times, which say the
 *   same thing without paraphrasing;
 * - the expression does not parse → the parser's message, in *informational*
 *   styling and not a destructive alert, because a half-written expression is
 *   the normal state of a field being typed into, not a failure;
 * - nothing to show → nothing rendered.
 *
 * `suppressError` hides the third state while the field already carries a
 * blur-time error (issue #261), so one mistake never produces two complaints
 * stacked under one input.
 */
export function CronPreviewLine({
  client,
  company,
  schedule,
  suppressError = false,
}: {
  client: OpenCompanyClient;
  company: string | null;
  schedule: string;
  suppressError?: boolean;
}) {
  const preview = useCronPreview(client, company, schedule, true);
  if (!preview) return null;

  if (preview.error) {
    if (suppressError) return null;
    return (
      <p className="text-[10px] leading-snug text-amber-600 dark:text-amber-400">
        {preview.error}
      </p>
    );
  }

  const next = preview.next ?? [];
  if (next.length === 0) return null;

  return (
    <p className="text-[10px] leading-snug text-muted-foreground">
      {preview.description ? (
        <>
          <span className="text-foreground">{preview.description}</span> — next run{" "}
          {formatUtc(next[0])} UTC ({formatLocal(next[0])} your time)
        </>
      ) : (
        <>Next runs: {next.map((ms) => `${formatUtc(ms)} UTC`).join(" · ")}</>
      )}
    </p>
  );
}
