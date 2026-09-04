import { useCallback, useEffect, useRef, useState } from "react";
import { CircleCheck, Loader2, Users } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import {
  clearComposioDefaultAccount,
  listComposioConnections,
  setComposioDefaultAccount,
  type ComposioConnectedAccount,
  type ComposioConnection,
} from "@/api/composio";
import { ApiError } from "@/api/types";
import { classifyLoadFailure } from "@/lib/section-load";
import { SectionUnreachable } from "@/views/connections/SectionUnreachable";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { toolkitLabel } from "@/lib/composio-catalog";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** Whether this viewer may change what the company acts as (issue #403). */
  canManage: boolean;
  /**
   * Bumped by the page when a connection is made or released, so the list
   * re-reads rather than describing accounts that have since changed.
   */
  generation?: number;
}

/**
 * Which account the company acts as, for a provider it holds more than one of
 * (issue #820).
 *
 * ## Why this section exists at all
 *
 * A company can hold two Composio accounts for one toolkit — `ops@` and
 * `billing@` Gmail. Until #820 nothing in the product chose between them:
 * `composio_execute` sent no connection id, so the account was resolved by
 * Composio for the entity, outside this codebase entirely, and "send from the
 * billing account, not ops" was not sayable. The only lever was to disconnect
 * the account you did not want — a blunt instrument when both are wanted for
 * different work.
 *
 * ## Why it only appears for two or more
 *
 * A single account is not a choice, and drawing a "default" control beside the
 * only account there is would invite an operator to make a decision that
 * changes nothing. One account per toolkit is the ordinary case, so for almost
 * every company this section is simply not on the page.
 *
 * ## Why nothing is marked until somebody marks it
 *
 * There is no implicit default to report. The host sends
 * `defaultConnectionId` only once a company has chosen, and this renders that
 * absence as "Composio picks" rather than pointing at the first row — a default
 * the console claimed and the harness did not honour would read as a
 * guarantee, which is worse than saying nothing. Same reason #819 states the
 * absence on the provider detail view.
 */
export function AccountChoiceSection({ client, company, canManage, generation = 0 }: Props) {
  const [rows, setRows] = useState<ComposioConnection[] | null>(null);
  // A non-404 read failure: the host could not answer which accounts exist, so
  // this section must not read that as "nothing to choose between" (issue
  // #1470). Distinct from `rows === []`, which is a host that answered with no
  // multi-account provider.
  const [failed, setFailed] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  // Only the latest read may paint: switching company re-issues this call, and
  // a slow earlier response would otherwise show another company's accounts.
  const requestGeneration = useRef(0);
  // The company these rows describe. A mutation is started against the company
  // that was on screen and must stay so — but `refresh` closes over that same
  // company, and running it after the view has moved on would answer company
  // B's page with company A's accounts.
  const shownCompany = useRef(company);

  const refresh = useCallback(async () => {
    const generation = ++requestGeneration.current;
    try {
      const list = await listComposioConnections(client, company);
      if (generation !== requestGeneration.current) return;
      setRows(list);
      setFailed(false);
    } catch (err) {
      if (generation !== requestGeneration.current) return;
      // A host with no Composio surface, and a company with no credential, both
      // have nothing to choose between — and the sections above already say
      // which of the two it is, so a second notice here would only be a second
      // voice on the same fact. Any other failure means the host could not
      // answer, and an unknown list must not render as an empty one (#1470).
      if (classifyLoadFailure(err) === "error") {
        setFailed(true);
      } else {
        setRows([]);
        setFailed(false);
      }
    }
  }, [client, company]);

  useEffect(() => {
    shownCompany.current = company;
    setRows(null);
    setFailed(false);
    void refresh();
  }, [company, refresh, generation]);

  /**
   * Re-read, unless the operator has moved to another company since the
   * mutation was sent. The write itself still lands where it was aimed — it
   * named a connection id that belongs to the company that was on screen — but
   * the read that follows it must not paint that company's accounts over the
   * one now being looked at.
   */
  async function refreshIfStillShowing(startedFor: string | null) {
    if (shownCompany.current === startedFor) await refresh();
  }

  async function choose(toolkit: string, account: ComposioConnectedAccount) {
    const startedFor = company;
    setBusy(account.id);
    try {
      const res = await setComposioDefaultAccount(client, company, account.id);
      toast.success(res.note);
      await refreshIfStillShowing(startedFor);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : `Couldn't set the ${toolkit} account.`);
    } finally {
      setBusy(null);
    }
  }

  async function clear(toolkit: string, account: ComposioConnectedAccount) {
    const startedFor = company;
    setBusy(account.id);
    try {
      const res = await clearComposioDefaultAccount(client, company, account.id);
      toast.success(res.note);
      await refreshIfStillShowing(startedFor);
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : `Couldn't clear the ${toolkit} account.`);
    } finally {
      setBusy(null);
    }
  }

  // Only providers where the choice is real. `rows === null` is still loading;
  // an empty result and a single-account result render nothing at all.
  const multi = (rows ?? []).filter((row) => (row.accounts?.length ?? 0) > 1);
  if (failed) {
    return (
      <section className="space-y-3" data-testid="account-choice">
        <div className="flex items-center gap-2">
          <Users className="size-4 text-muted-foreground" />
          <h2 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
            Which account teammates act as
          </h2>
        </div>
        <SectionUnreachable label="Couldn't read this company's connected accounts" />
      </section>
    );
  }
  if (rows === null || multi.length === 0) return null;

  return (
    <section className="space-y-3" data-testid="account-choice">
      <div className="flex items-center gap-2">
        <Users className="size-4 text-muted-foreground" />
        <h2 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          Which account teammates act as
        </h2>
      </div>
      <p className="text-sm text-muted-foreground">
        {canManage
          ? "This company holds more than one account for these providers. Choose which one your teammates act as — the choice applies to every teammate, and takes effect on their next turn. The other accounts stay connected."
          : "This company holds more than one account for these providers. Which one teammates act as is an admin's choice."}
      </p>

      <Card>
        <CardContent className="space-y-4">
          {multi.map((row) => (
            <div key={row.toolkit} className="space-y-2" data-testid={`accounts-${row.toolkit}`}>
              <div className="flex items-center gap-2">
                <span className="text-sm font-medium">{toolkitLabel(row.toolkit)}</span>
                {row.defaultConnectionId === undefined && (
                  // The honest render of "nothing is chosen". Composio's own
                  // resolution is deterministic enough that nobody has reported
                  // surprise — but it is not a decision this product makes, and
                  // the page says which of the two situations the operator is in.
                  <Badge variant="outline" className="text-3xs">
                    Composio picks
                  </Badge>
                )}
              </div>
              <ul className="space-y-1.5">
                {(row.accounts ?? []).map((account) => (
                  <li
                    key={account.id}
                    data-testid={`account-${account.id}`}
                    className={cn(
                      "flex flex-wrap items-center gap-2 rounded-md border p-2",
                      account.isDefault
                        ? "border-status-done/30 bg-status-done-soft"
                        : "border-border",
                    )}
                  >
                    <div className="min-w-0 flex-1">
                      <span className="block truncate text-xs font-medium">
                        {/* No label is a real state for plenty of toolkits, and
                            inventing one from the slug would render as a fact
                            the operator cannot check. The id is at least
                            something they can match against Composio. */}
                        {account.account ?? account.id}
                      </span>
                      <span className="block truncate text-3xs text-muted-foreground">
                        {account.account ? `${account.id} · ` : ""}
                        {account.status.toLowerCase()}
                      </span>
                    </div>

                    {account.isDefault ? (
                      <>
                        <span className="inline-flex items-center gap-1 text-xs text-status-done-text">
                          <CircleCheck className="size-3" /> teammates act as this
                        </span>
                        {canManage && (
                          <Button
                            variant="ghost"
                            size="sm"
                            disabled={busy !== null}
                            onClick={() => void clear(row.toolkit, account)}
                          >
                            {busy === account.id ? (
                              <Loader2 className="size-3.5 animate-spin" />
                            ) : null}
                            Clear
                          </Button>
                        )}
                      </>
                    ) : (
                      canManage && (
                        <Button
                          variant="outline"
                          size="sm"
                          // An account that is not usable cannot be the one
                          // agents act as — the host refuses it, and offering
                          // the button would only produce a 404 the operator
                          // has to decode. Re-authorize it first.
                          disabled={busy !== null || !account.connected}
                          title={
                            account.connected
                              ? undefined
                              : `This account is ${account.status.toLowerCase()} — re-authorize it before teammates can act as it.`
                          }
                          onClick={() => void choose(row.toolkit, account)}
                        >
                          {busy === account.id ? (
                            <Loader2 className="size-3.5 animate-spin" />
                          ) : null}
                          Act as this
                        </Button>
                      )
                    )}
                  </li>
                ))}
              </ul>
            </div>
          ))}
        </CardContent>
      </Card>
    </section>
  );
}
