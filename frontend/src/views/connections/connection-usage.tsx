import { useEffect, useState } from "react";

import type { OpenCompanyClient } from "@/api/client";
import { callsForProvider } from "@/lib/connection-detail";

/** The window the usage figure covers. Matches the Usage view's own default. */
const USAGE_RANGE = "30d";

/** The usage read, as every connection surface consumes it. */
export interface Usage {
  load: "loading" | "ready" | "unavailable";
  calls: number | null;
  key: string | null;
}

/**
 * Calls counted against one connection's `byProvider` key.
 *
 * The read carries the key it was made for. A surface can change subject
 * without unmounting, and state set in an effect lands one render after the
 * subject does — so a figure kept as a bare number would paint against the new
 * connection for that frame, which is one connection's call count under
 * another's name.
 */
export function useConnectionUsage(
  client: OpenCompanyClient,
  company: string | null,
  usageKey: string | null,
): Usage {
  const [loaded, setLoaded] = useState<{
    key: string;
    load: "ready" | "unavailable";
    calls: number | null;
  } | null>(null);

  useEffect(() => {
    if (usageKey === null) return;
    let alive = true;
    client
      .usage(USAGE_RANGE, company)
      .then((usage) => {
        if (!alive) return;
        setLoaded({
          key: usageKey,
          load: "ready",
          calls: callsForProvider(usage.byProvider, usageKey),
        });
      })
      // A host without the usage route (older build) 404s. "Not recorded here"
      // is the honest render — not a zero, which claims the calls were counted
      // and there were none.
      .catch(() => {
        if (alive)
          setLoaded({ key: usageKey, load: "unavailable", calls: null });
      });
    return () => {
      alive = false;
    };
  }, [client, company, usageKey]);

  const current = loaded !== null && loaded.key === usageKey ? loaded : null;
  return {
    load: current?.load ?? ("loading" as const),
    calls: current?.calls ?? null,
    key: usageKey,
  };
}

/**
 * The Usage section.
 *
 * `perConnection` is the sentence under the figure, and it is the caller's to
 * write: what "counted per connection rather than per account" rules out
 * differs between a toolkit with two Gmail accounts and a server with one
 * endpoint, and a single generic line would be vague in the Composio case and
 * wrong in the MCP one.
 */
export function UsageSection({
  usage,
  perConnection,
}: {
  usage: Usage;
  perConnection: string;
}) {
  return (
    <section
      className="space-y-1"
      aria-label="Usage"
      data-testid="connection-detail-usage"
    >
      <h4 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
        Usage
      </h4>
      {usage.key === null ? (
        <p className="text-xs text-muted-foreground">
          This connection has no name to count calls against, so what has gone
          through it is not attributable here.
        </p>
      ) : usage.load === "loading" ? (
        <p className="text-xs text-muted-foreground">Reading usage…</p>
      ) : usage.load === "unavailable" ? (
        <p className="text-xs text-muted-foreground">
          This host does not report usage, so what has gone through this
          connection is not recorded here.
        </p>
      ) : (
        <>
          <p className="text-sm">
            <span className="font-medium">{usage.calls}</span>{" "}
            {usage.calls === 1 ? "call" : "calls"} in the last 30 days
          </p>
          <p className="text-xs text-muted-foreground">{perConnection}</p>
        </>
      )}
    </section>
  );
}
