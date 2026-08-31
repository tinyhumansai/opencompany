import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Loader2, RefreshCw } from "lucide-react";

import { getPaypal, type PaypalStatus } from "@/api/billing";
import type { OpenCompanyClient } from "@/api/client";
import { getBalance, listTransactions, testPaypal, type Balance, type Transaction } from "@/api/finance";
import { ApiError } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import { PaypalIcon } from "@/components/paypal-icon";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import { ConnectionPanel } from "@/views/finance/ConnectionPanel";
import { paypalHealth, startsExpanded } from "@/views/finance/health";
import { grantNamespace } from "@/components/grant-namespace";
import { me as fetchMe } from "@/api/auth";
import {
  defaultWindow,
  latestSelectableEnd,
  transactionStatus,
  windowProblem,
} from "@/views/finance/money";
import { PaypalForm } from "@/views/finance/PaypalForm";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/** `datetime-local` wants `YYYY-MM-DDTHH:mm` in local time, not an ISO instant. */
function toLocalInput(iso: string): string {
  const at = new Date(iso);
  const shifted = new Date(at.getTime() - at.getTimezoneOffset() * 60_000);
  return shifted.toISOString().slice(0, 16);
}

/** The inverse: a `datetime-local` value back to an ISO instant. */
function fromLocalInput(value: string): string {
  const at = new Date(value);
  return Number.isNaN(at.getTime()) ? "" : at.toISOString();
}

/**
 * Finance → Wallet: the PayPal connection, the balance, and recent transactions.
 *
 * Read-only, and it says so. `src/paypal/mod.rs` left `send_payment` out of
 * issue #789 pending a scoping decision, and nothing here changes that.
 *
 * # The three-hour lag is on the page, not discovered
 *
 * PayPal publishes transaction data up to three hours late and **rejects** a
 * window whose end is inside that gap. Its own message for the refusal says only
 * that data "is not available", which reads as "you had no transactions" — the
 * host rewrites it (`explain_unavailable_window`), and this view stops the
 * operator hitting it at all: the default range ends three hours ago, the picker
 * will not select later, and the caption says why.
 *
 * # Money stays a string
 *
 * `available`, `withheld` and `amount` are rendered exactly as PayPal sent them.
 * No parsing, no `toFixed`, no summing across currencies — `4320.50` through a
 * float is how a balance acquires a trailing `0000001`, and there is nothing
 * here that needs arithmetic.
 */
export function WalletView({ client, company }: Props) {
  const [status, setStatus] = useState<PaypalStatus | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);
  // Issue #1796: whether the `paypal` grant is in flight. The panel renders the
  // control; this page owns the write and the re-read that follows it, because
  // the panel is shared by both providers and holds neither one's status.
  const [granting, setGranting] = useState(false);
  // Whether this viewer may widen the company's tool grants (issue #1796).
  // Resolved as `OAuthView` resolves it and defaulted CLOSED: the grant write is
  // admin-only, so an unresolved role must not render an enabled button.
  const [canManage, setCanManage] = useState(false);
  const [expanded, setExpanded] = useState(false);
  // A latch, not render state: set once from the first status that arrives.
  // Re-deriving the panel's openness on every status would slam it shut the
  // moment a credential was saved, which is exactly when the operator is still
  // working in it — and a `useState` here would re-render for a value nothing
  // renders.
  const expandedSeeded = useRef(false);
  // Monotonic token so a superseded wallet read (one fired for an older date
  // range or a previous company) cannot overwrite the balance and transaction
  // data, or the spinner, of a newer one.
  const runId = useRef(0);

  const [balances, setBalances] = useState<Balance[] | null>(null);
  const [transactions, setTransactions] = useState<Transaction[] | null>(null);
  const [dataError, setDataError] = useState<{ code: string; message: string } | null>(null);
  const [loading, setLoading] = useState(false);

  // The clock is read once per mount, so the default window and the picker's
  // ceiling agree with each other. Re-reading `new Date()` on every render
  // would make the ceiling drift under a range the operator had already chosen.
  const now = useMemo(() => new Date(), []);
  const initial = useMemo(() => defaultWindow(now), [now]);
  const [since, setSince] = useState(initial.since);
  const [until, setUntil] = useState(initial.until);

  const problem = windowProblem(since, until, now);

  const loadStatus = useCallback(async () => {
    try {
      const next = await getPaypal(client, company);
      setStatus(next);
      setStatusError(null);
      if (!expandedSeeded.current) {
        expandedSeeded.current = true;
        setExpanded(startsExpanded(paypalHealth(next)));
      }
    } catch (err) {
      setStatusError(err instanceof Error ? err.message : String(err));
    }
  }, [client, company]);

  const loadData = useCallback(async () => {
    if (windowProblem(since, until, now)) return;
    const run = ++runId.current;
    setLoading(true);
    try {
      // Both reads together: they share one credential and one failure, so
      // splitting them would only mean showing half a wallet.
      const [wallet, txns] = await Promise.all([
        getBalance(client, company),
        listTransactions(client, company, since, until, 100),
      ]);
      if (run !== runId.current) return;
      setBalances(wallet);
      setTransactions(txns);
      setDataError(null);
    } catch (err) {
      if (run !== runId.current) return;
      setBalances(null);
      setTransactions(null);
      setDataError({
        code: err instanceof ApiError ? err.code : "unknown",
        message: err instanceof Error ? err.message : String(err),
      });
    } finally {
      if (run === runId.current) setLoading(false);
    }
  }, [client, company, since, until, now]);

  useEffect(() => {
    let live = true;
    void (async () => {
      let admin = false;
      try {
        admin = (await fetchMe(client, company)).role === "admin";
      } catch {
        // No user plane on this host, or not signed in — treat as non-admin.
      }
      if (live) setCanManage(admin);
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  useEffect(() => {
    void loadStatus();
  }, [loadStatus]);

  useEffect(() => {
    void loadData();
  }, [loadData]);

  /*
    Hoisted above the state conditionals (codex review, #1785): both early
    returns ran before the header, so this page had no `h1` while it loaded and
    none at all once the read failed — a terminal state, since nothing retries.
    The same defect and the same fix as `SearchView` and `HostingView`, which
    these two are a copy of.
  */
  /*
    Derived above the header rather than below the conditionals, and guarded
    for a `status` that has not arrived: the environment badge is the header's
    own, so it has to be computable in every state the header now renders in.
    `usable` is false while `status` is null, which is the correct answer —
    "this is a sandbox balance" is a claim, and a page that has read nothing
    yet is not entitled to make it.
  */
  const connection = status ? paypalHealth(status) : null;
  const usable = connection?.state === "connected" || connection?.state === "not_granted";
  const sandbox = status ? (status.environment || "sandbox") !== "live" : false;

  const header = (
    <PageHeader
      title="Wallet"
      width="5xl"
      description={
        <>
          What is in the company&rsquo;s PayPal account, and what has moved through it.
          Read-only.
        </>
      }
      /* On the page, not only in the connection panel. Reading a sandbox
         balance and believing it is real money is the failure this prevents,
         and the panel is collapsed most of the time. */
      trailing={
        usable ? (
          <Badge
            variant={sandbox ? "outline" : "secondary"}
            data-testid="wallet-environment"
            className={sandbox ? "text-status-blocked-text" : undefined}
          >
            {sandbox ? "Sandbox — not real money" : "Live"}
          </Badge>
        ) : null
      }
    />
  );

  if (statusError) {
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        {header}
        <div className="mx-auto w-full max-w-5xl px-4 py-6">
          <Alert variant="destructive" data-testid="wallet-status-error">
            <AlertDescription>Could not load the PayPal connection: {statusError}</AlertDescription>
          </Alert>
        </div>
      </div>
    );
  }

  if (!status) {
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        {header}
        <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
          <Loader2 className="mr-2 size-4 animate-spin" /> Loading wallet…
        </div>
      </div>
    );
  }

  const health = paypalHealth(status);

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="wallet-view">
      {header}
      <div className="mx-auto min-h-0 w-full max-w-5xl flex-1 space-y-6 overflow-y-auto px-4 py-6">

        <ConnectionPanel
          title="PayPal"
          testId="paypal"
          logo={<PaypalIcon className="size-4" />}
          health={health}
          expanded={expanded}
          onExpandedChange={setExpanded}
          onTest={usable ? () => testPaypal(client, company) : undefined}
          granting={granting}
          canManage={canManage}
          onGrant={() => {
            void (async () => {
              setGranting(true);
              try {
                // Re-read on success so the panel's own verdict moves off
                // "not granted" — a button that works and leaves the warning
                // standing reads exactly like one that did not.
                if (await grantNamespace(client, company, "paypal")) await loadStatus();
              } finally {
                setGranting(false);
              }
            })();
          }}
        >
          <PaypalForm
            client={client}
            company={company}
            status={status}
            onStatus={(next) => {
              setStatus(next);
              void loadData();
            }}
          />
        </ConnectionPanel>

        {dataError ? (
          <Alert
            variant={dataError.code === "provider_error" ? "destructive" : "default"}
            data-testid="wallet-data-error"
          >
            <AlertDescription>
              {dataError.code === "not_configured"
                ? "Connect PayPal above to read the wallet."
                : dataError.code === "not_in_build"
                  ? "This host was built without PayPal support, so there is no wallet to read."
                  : dataError.message}
            </AlertDescription>
          </Alert>
        ) : null}

        {balances?.length ? (
          <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3" data-testid="wallet-balances">
            {/* Primary currency first — it is the one the operator means when
                they say "the balance". */}
            {[...balances]
              .sort((a, b) => Number(b.primary) - Number(a.primary))
              .map((balance) => (
                <Card key={balance.currency_code}>
                  <CardContent className="space-y-1">
                    <div className="flex items-center justify-between">
                      <span className="text-sm font-medium text-muted-foreground">
                        {balance.currency_code}
                      </span>
                      {balance.primary ? (
                        <Badge variant="outline" className="text-xs">
                          Primary
                        </Badge>
                      ) : null}
                    </div>
                    <div className="text-2xl font-semibold tabular-nums">{balance.available}</div>
                    <p className="text-xs text-muted-foreground">
                      {balance.withheld} withheld
                    </p>
                  </CardContent>
                </Card>
              ))}
          </div>
        ) : null}

        <Card>
          <CardContent className="space-y-4">
            <div className="flex flex-wrap items-end gap-3">
              <div className="space-y-1">
                <label className="text-xs text-muted-foreground" htmlFor="tx-since">
                  From
                </label>
                <input
                  id="tx-since"
                  type="datetime-local"
                  data-testid="wallet-since"
                  className="h-9 rounded-md border border-input bg-transparent px-3 text-sm"
                  value={toLocalInput(since)}
                  onChange={(e) => setSince(fromLocalInput(e.target.value) || since)}
                />
              </div>
              <div className="space-y-1">
                <label className="text-xs text-muted-foreground" htmlFor="tx-until">
                  To
                </label>
                <input
                  id="tx-until"
                  type="datetime-local"
                  data-testid="wallet-until"
                  // The ceiling, enforced by the control rather than discovered
                  // through a rejected request.
                  max={toLocalInput(latestSelectableEnd(now).toISOString())}
                  className="h-9 rounded-md border border-input bg-transparent px-3 text-sm"
                  value={toLocalInput(until)}
                  onChange={(e) => setUntil(fromLocalInput(e.target.value) || until)}
                />
              </div>
              <Button
                variant="outline"
                size="sm"
                onClick={() => void loadData()}
                disabled={loading || !!problem}
              >
                <RefreshCw className={cn("mr-2 size-3.5", loading && "animate-spin")} />
                Refresh
              </Button>
            </div>

            <p className="text-xs text-muted-foreground" data-testid="wallet-lag-note">
              PayPal publishes transactions up to 3 hours late and allows at most 31 days per query,
              so this range ends three hours ago by default.
            </p>

            {problem ? (
              <Alert variant="destructive" data-testid="wallet-window-problem">
                <AlertDescription>{problem}</AlertDescription>
              </Alert>
            ) : null}

            {transactions?.length === 0 ? (
              <p className="py-6 text-center text-sm text-muted-foreground" data-testid="wallet-empty">
                No transactions in this range.
              </p>
            ) : null}

            {transactions?.length ? (
              <ul className="divide-y" data-testid="wallet-transactions">
                {transactions.map((txn) => {
                  const state = transactionStatus(txn.status);
                  const outgoing = txn.amount.trim().startsWith("-");
                  return (
                    <li key={txn.id} className="flex flex-wrap items-center gap-3 py-2.5">
                      <span className="w-24 shrink-0 text-xs text-muted-foreground">
                        {new Date(txn.date).toLocaleDateString(undefined, {
                          month: "short",
                          day: "numeric",
                        })}
                      </span>
                      <div className="min-w-0 flex-1">
                        <p className="truncate text-sm">{txn.counterparty ?? txn.id}</p>
                        {txn.note ? (
                          <p className="truncate text-xs text-muted-foreground">{txn.note}</p>
                        ) : null}
                      </div>
                      <Badge
                        variant="outline"
                        className={cn(
                          state.tone === "done" && "text-status-done-text",
                          state.tone === "failed" && "text-status-failed-text",
                          state.tone === "pending" && "text-status-blocked-text",
                        )}
                      >
                        {state.label}
                      </Badge>
                      {/* Rendered as PayPal sent it, sign and all. */}
                      <span
                        className={cn(
                          "w-32 shrink-0 text-right text-sm font-medium tabular-nums",
                          !outgoing && "text-status-done-text",
                        )}
                      >
                        {txn.amount} {txn.currency_code}
                      </span>
                    </li>
                  );
                })}
              </ul>
            ) : null}
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
