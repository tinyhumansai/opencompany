// Local harnesses on the LLM Providers page: the coding CLIs this company can
// put a teammate on, beside the model providers it can reach.
//
// They belong on the same page because they answer the same question — what
// can a teammate think with — but they are not providers and the section says
// so. A harness is never "connected": nothing is stored, there is no
// credential, and no company-wide state to be in. What a row reports instead is
// how many teammates individually picked it.
//
// `[+ Add]` does not grow a third group for these. There is nothing to add: a
// harness is usable exactly when its CLI is installed and signed in on this
// machine, which is not something this dialog could do.

import { useCallback, useEffect, useRef, useState } from "react";
import { Cpu } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type HarnessDto, type TeamMemberDto } from "@/api/types";
import { HarnessDetailDialog } from "@/components/harness-detail";
import { Card, CardContent } from "@/components/ui/card";
import { boundAgents, boundLabel, statusOf } from "@/lib/harnesses";
import { useHarnessRows } from "@/lib/use-harness-rows";
import { cn } from "@/lib/utils";

export function LocalHarnesses({
  client,
  company,
}: {
  client: OpenCompanyClient;
  company: string | null;
}) {
  const [declared, setDeclared] = useState<HarnessDto[] | null>(null);
  /**
   * The roster the bound counts are resolved from, or `null` when it could not
   * be read — which is not "nobody is bound". A row says nothing at all in that
   * case rather than reporting a zero it cannot stand behind.
   */
  const [roster, setRoster] = useState<TeamMemberDto[] | null>(null);

  /**
   * Which fetch an answer belongs to — the guard `external-harnesses.tsx`
   * carries for the same reason. This page is reused across company changes,
   * so a slow read for a superseded company must not land on the newer one.
   */
  const generation = useRef(0);

  const load = useCallback(async () => {
    const run = ++generation.current;
    try {
      const next = await client.listHarnesses(company);
      if (generation.current === run) setDeclared(next);
    } catch (error) {
      // A host predating `GET {scope}/harnesses` renders nothing rather than an
      // error, the way every other optional section on this page degrades.
      if (generation.current === run && error instanceof ApiError && error.status === 404) {
        setDeclared([]);
      }
    }
  }, [client, company]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const next = await client.listTeam(company);
        if (live) setRoster(next);
      } catch {
        // Inside the `try` rather than a `.catch`, so a host that cannot answer
        // this read at all fails the count instead of the page around it.
        if (live) setRoster(null);
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  const { rows, surveying, install, installing, installErrors } = useHarnessRows(declared);
  const [managingId, setManagingId] = useState<string | null>(null);

  // Which rows belong here comes from the list the host already sent, never
  // from re-deriving whether this build can run a local harness: a hosted build
  // is gated upstream and simply declares none, so an empty list after the
  // filter is the whole answer.
  const local = rows?.filter((row) => row.kind === "acp") ?? [];
  const defaultHarnessId = rows?.find((row) => row.isDefault)?.id;

  if (local.length === 0) return null;

  return (
    <>
      <Card data-testid="inference-local-harnesses">
        <CardContent className="px-0">
          <h3 className="px-4 pb-2 text-xs font-medium tracking-wide text-muted-foreground uppercase">
            Local harnesses
          </h3>
          <ul className="divide-y">
            {local.map((row) => {
              const status = statusOf(row);
              return (
                <li key={row.id}>
                  <button
                    type="button"
                    className="flex w-full items-center justify-between gap-4 px-4 py-3 text-left hover:bg-accent/50"
                    onClick={() => setManagingId(row.id)}
                    data-testid="local-harness-row"
                  >
                    <span className="flex min-w-0 items-center gap-2.5">
                      <Cpu className="size-4 shrink-0 text-muted-foreground" />
                      <span className="min-w-0">
                        <span className="block truncate text-sm font-medium">
                          {row.label}
                          <span className="ml-1.5 font-mono text-xs text-muted-foreground">
                            {row.id}
                          </span>
                        </span>
                        {roster && (
                          <span
                            className="block text-xs text-muted-foreground"
                            data-testid="local-harness-bound"
                          >
                            {boundLabel(boundAgents(roster, row.id, defaultHarnessId).length)}
                          </span>
                        )}
                      </span>
                    </span>
                    <span className="inline-flex shrink-0 items-center gap-1.5 rounded-full border bg-card px-2.5 py-0.5 text-xs font-medium">
                      <span className={cn("size-1.5 rounded-full", status.dot)} />
                      {status.label}
                    </span>
                  </button>
                </li>
              );
            })}
          </ul>
        </CardContent>
      </Card>
      <HarnessDetailDialog
        client={client}
        company={company}
        row={local.find((row) => row.id === managingId) ?? null}
        defaultHarnessId={defaultHarnessId}
        onRecheck={() => void load()}
        checking={surveying}
        install={install}
        installing={installing}
        installErrors={installErrors}
        onOpenChange={(open) => {
          if (!open) setManagingId(null);
        }}
      />
    </>
  );
}
