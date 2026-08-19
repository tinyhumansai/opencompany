// The drawer a FAILED run leaves behind (issue #1007).
//
// `RunResultPanel` next door is the surface for a run that produced something,
// and it can only mount when the settled body arrives — which on the failure
// path it never does. So a failed run had no drawer, no row and no error text:
// the console went back to its resting state behind a toast that lasted four
// seconds. This is the same slot, for the outcome that had nothing in it.
//
// Deliberately a panel and not a toast, on the same rule the version-conflict
// banner and the inference refusal already follow: an outcome the operator has
// to act on — read the message, look at the history row, fix the graph, run it
// again — must outlive the glance.

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";

import { formatDuration } from "./run-health";
import type { RunFailure } from "./run-failure";

export function RunFailurePanel({
  failure,
  onClose,
}: {
  failure: RunFailure;
  onClose: () => void;
}) {
  // How long the operator waited before this came back. The number a failure
  // most needs beside it: a run that died in 200ms was refused, and one that
  // died after four minutes got somewhere first.
  const ranFor = Math.max(0, failure.atMillis - failure.startedAtMillis);
  return (
    <div className="border-t bg-card/60" data-testid="workflow-run-failure">
      <div className="flex items-center justify-between px-4 py-2">
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-sm font-medium">Run failed</span>
          {failure.dryRun && (
            <Badge
              variant="outline"
              className="border-primary/40 bg-primary/10 text-primary"
            >
              Test run — nothing was sent
            </Badge>
          )}
          {/* Only a code the HOST gave us. See `RunFailure.code`. */}
          {failure.code && (
            <Badge
              variant="outline"
              className="h-4 px-1.5 font-mono text-3xs font-normal"
              data-testid="workflow-run-failure-code"
            >
              {failure.code}
            </Badge>
          )}
        </div>
        <Button variant="ghost" size="sm" onClick={onClose}>
          Dismiss
        </Button>
      </div>
      <div className="max-h-72 overflow-auto px-4 pb-3">
        <Alert variant="destructive" className="py-2">
          <AlertDescription
            className="text-xs"
            data-testid="workflow-run-failure-message"
          >
            {failure.message}
          </AlertDescription>
        </Alert>
        <p className="mt-2 text-2xs text-muted-foreground">
          Failed after {formatDuration(ranFor)} ·{" "}
          {new Date(failure.atMillis).toLocaleString()}
          {/* A status is worth printing only when it says something the message
              does not. `0` is the client's "the request never completed", which
              the sentence below already states in words. */}
          {failure.status != null && failure.status > 0
            ? ` · HTTP ${failure.status}`
            : ""}
        </p>
        {/* Issue #154's echo, for the same reason the result drawer carries it:
            the operator has usually typed something else by now, so the box is
            not the record of what this run was asked for. */}
        {failure.request && (
          <p className="mt-1 text-xs text-muted-foreground">
            <span className="font-medium text-foreground">Requested:</span>{" "}
            {failure.request}
          </p>
        )}
        {/* Two genuinely different next steps, and collapsing them sends the
            operator to the wrong place. A host that answered has journaled the
            run (#228), so its per-node trail is one drawer down. Something that
            gave up in between may have left no run at all — and telling someone
            to look in History for a row that does not exist is how a transport
            failure comes to read as a missing feature. */}
        <p className="mt-2 text-2xs text-muted-foreground">
          {failure.fromHost
            ? "The host records failed runs, so this one is in Run history below with the step it stopped at."
            : "The request didn't complete, so the host may or may not have started this run. Run history below is the answer either way."}
        </p>
      </div>
    </div>
  );
}
