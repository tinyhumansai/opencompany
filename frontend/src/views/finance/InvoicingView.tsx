import { useCallback, useEffect, useRef, useState } from "react";
import { ExternalLink, Loader2, RefreshCw } from "lucide-react";

import { getBilling, type BillingStatus } from "@/api/billing";
import type { OpenCompanyClient } from "@/api/client";
import { listInvoices, testChargebee, type Invoice } from "@/api/finance";
import { ApiError } from "@/api/types";
import { ChargebeeIcon } from "@/components/chargebee-icon";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import { ChargebeeForm } from "@/views/finance/ChargebeeForm";
import { ConnectionPanel } from "@/views/finance/ConnectionPanel";
import { chargebeeHealth, startsExpanded } from "@/views/finance/health";
import { grantNamespace } from "@/components/grant-namespace";
import { me as fetchMe } from "@/api/auth";
import { fromMinorUnits, invoiceStatus } from "@/views/finance/money";
import { SendInvoiceDialog } from "@/views/finance/SendInvoiceDialog";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/** The statuses Chargebee accepts as an invoice filter. */
const STATUSES = ["", "paid", "payment_due", "posted", "not_paid", "voided"] as const;

/**
 * Finance → Invoicing: the Chargebee connection and what customers owe.
 *
 * # The two halves load apart
 *
 * The credential status and the invoice list are separate reads, and a failure
 * in one must not blank the other. That is not a stylistic preference: the list
 * *cannot* load until the credential exists, so on a fresh company the list read
 * always fails first, and a page that treated that as fatal would hide the very
 * form that fixes it.
 *
 * # The list's failure is not an error toast
 *
 * `not_configured` and `not_in_build` are states, not faults, and the host
 * distinguishes them precisely so the page can say the right thing about each
 * (see `src/server/ops/finance.rs`). A `provider_error` is a real failure and
 * carries Chargebee's own message, which is shown verbatim — it names the
 * setting to change, and paraphrasing it would throw that away.
 */
export function InvoicingView({ client, company }: Props) {
  const [status, setStatus] = useState<BillingStatus | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);
  // Issue #1796: whether the `chargebee` grant is in flight. The panel renders the
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
  // Monotonic token so a stale invoice-list response (one fired for an older
  // status/customer filter) cannot overwrite the results for the visible one.
  const invoiceRequest = useRef(0);

  const [invoices, setInvoices] = useState<Invoice[] | null>(null);
  const [listError, setListError] = useState<{ code: string; message: string } | null>(null);
  const [loading, setLoading] = useState(false);
  const [filter, setFilter] = useState("");
  const [customerEmail, setCustomerEmail] = useState("");
  const [sending, setSending] = useState(false);

  const loadStatus = useCallback(async () => {
    try {
      const next = await getBilling(client, company);
      setStatus(next);
      setStatusError(null);
      if (!expandedSeeded.current) {
        expandedSeeded.current = true;
        setExpanded(startsExpanded(chargebeeHealth(next)));
      }
    } catch (err) {
      setStatusError(err instanceof Error ? err.message : String(err));
    }
  }, [client, company]);

  const loadInvoices = useCallback(async () => {
    const request = ++invoiceRequest.current;
    setLoading(true);
    try {
      const next = await listInvoices(client, company, {
        status: filter || undefined,
        customerEmail: customerEmail.trim() || undefined,
        limit: 50,
      });
      if (request !== invoiceRequest.current) return;
      setInvoices(next);
      setListError(null);
    } catch (err) {
      if (request !== invoiceRequest.current) return;
      setInvoices(null);
      setListError({
        code: err instanceof ApiError ? err.code : "unknown",
        message: err instanceof Error ? err.message : String(err),
      });
    } finally {
      if (request === invoiceRequest.current) setLoading(false);
    }
  }, [client, company, filter, customerEmail]);

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
    void loadInvoices();
  }, [loadInvoices]);

  /*
    Hoisted above the state conditionals (codex review, #1785): both early
    returns ran before the header, so this page had no `h1` while it loaded and
    none at all once the read failed — a terminal state, since nothing retries.
    The same defect and the same fix as `SearchView` and `HostingView`, which
    these two are a copy of.
  */
  const header = (
    <PageHeader
      title="Invoicing"
      width="5xl"
      description="What your customers owe and have paid, through Chargebee."
    />
  );

  if (statusError) {
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        {header}
        <div className="mx-auto w-full max-w-5xl px-4 py-6">
          <Alert variant="destructive" data-testid="invoicing-status-error">
            <AlertDescription>Could not load the Chargebee connection: {statusError}</AlertDescription>
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
          <Loader2 className="mr-2 size-4 animate-spin" /> Loading invoicing…
        </div>
      </div>
    );
  }

  const health = chargebeeHealth(status);
  const usable = health.state === "connected" || health.state === "not_granted";

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-testid="invoicing-view">
      {header}
      <div className="mx-auto min-h-0 w-full max-w-5xl flex-1 space-y-6 overflow-y-auto px-4 py-6">

        <ConnectionPanel
          title="Chargebee"
          testId="chargebee"
          logo={<ChargebeeIcon className="size-4 text-(--brand-chargebee)" />}
          health={health}
          expanded={expanded}
          onExpandedChange={setExpanded}
          onTest={usable ? () => testChargebee(client, company) : undefined}
          granting={granting}
          canManage={canManage}
          onGrant={() => {
            void (async () => {
              setGranting(true);
              try {
                // Re-read on success so the panel's own verdict moves off
                // "not granted" — a button that works and leaves the warning
                // standing reads exactly like one that did not.
                if (await grantNamespace(client, company, "chargebee")) await loadStatus();
              } finally {
                setGranting(false);
              }
            })();
          }}
        >
          <ChargebeeForm
            client={client}
            company={company}
            status={status}
            onStatus={(next) => {
              setStatus(next);
              // A saved credential is the one moment the list is worth
              // retrying without being asked: it is what the operator just
              // did it for.
              void loadInvoices();
            }}
          />
        </ConnectionPanel>

        <Card>
          <CardContent className="space-y-4">
            <div className="flex flex-wrap items-end gap-3">
              <div className="space-y-1">
                <label className="text-xs text-muted-foreground" htmlFor="inv-filter">
                  Status
                </label>
                <select
                  id="inv-filter"
                  data-testid="invoice-status-filter"
                  className="h-9 rounded-md border border-input bg-transparent px-3 text-sm"
                  value={filter}
                  onChange={(e) => setFilter(e.target.value)}
                >
                  {STATUSES.map((value) => (
                    <option key={value || "all"} value={value}>
                      {value === "" ? "All" : invoiceStatus(value).label}
                    </option>
                  ))}
                </select>
              </div>
              <div className="min-w-48 flex-1 space-y-1">
                <label className="text-xs text-muted-foreground" htmlFor="inv-customer">
                  Customer email
                </label>
                <input
                  id="inv-customer"
                  data-testid="invoice-customer-filter"
                  className="h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm"
                  placeholder="Any"
                  value={customerEmail}
                  onChange={(e) => setCustomerEmail(e.target.value)}
                />
              </div>
              <Button variant="outline" size="sm" onClick={() => void loadInvoices()} disabled={loading}>
                <RefreshCw className={cn("mr-2 size-3.5", loading && "animate-spin")} />
                Refresh
              </Button>
              <Button size="sm" onClick={() => setSending(true)} disabled={!usable} data-testid="invoice-new">
                Send invoice
              </Button>
            </div>

            {listError ? (
              <Alert
                variant={listError.code === "provider_error" ? "destructive" : "default"}
                data-testid="invoice-list-error"
              >
                <AlertDescription>
                  {listError.code === "not_configured"
                    ? "Connect Chargebee above to see this company's invoices."
                    : listError.code === "not_in_build"
                      ? "This host was built without Chargebee support, so there are no invoices to read."
                      : listError.message}
                </AlertDescription>
              </Alert>
            ) : null}

            {invoices?.length === 0 ? (
              <p className="py-6 text-center text-sm text-muted-foreground" data-testid="invoice-empty">
                No invoices match.
              </p>
            ) : null}

            {invoices?.length ? (
              <ul className="divide-y" data-testid="invoice-list">
                {invoices.map((invoice) => {
                  const state = invoiceStatus(invoice.status);
                  return (
                    <li key={invoice.id} className="flex flex-wrap items-center gap-3 py-2.5">
                      <span className="font-mono text-xs text-muted-foreground">{invoice.id}</span>
                      <span className="min-w-0 flex-1 truncate text-sm">{invoice.customer_id}</span>
                      <span className="text-sm font-medium tabular-nums">
                        {fromMinorUnits(invoice.total_in_minor_units, invoice.currency_code)}
                      </span>
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
                      {/* Chargebee reports the due date in Unix SECONDS. The
                          `* 1000` is the difference between "due 3 Sep" and
                          "due 20 Jan 1970". */}
                      <span className="w-28 shrink-0 text-right text-xs text-muted-foreground">
                        {invoice.due_date
                          ? `due ${new Date(invoice.due_date * 1000).toLocaleDateString(undefined, {
                              month: "short",
                              day: "numeric",
                            })}`
                          : ""}
                      </span>
                      {invoice.payment_url ? (
                        <a
                          href={invoice.payment_url}
                          target="_blank"
                          rel="noreferrer noopener"
                          className="text-muted-foreground hover:text-foreground"
                          aria-label={`Payment page for ${invoice.id}`}
                        >
                          <ExternalLink className="size-3.5" />
                        </a>
                      ) : null}
                    </li>
                  );
                })}
              </ul>
            ) : null}
          </CardContent>
        </Card>
      </div>

      <SendInvoiceDialog
        client={client}
        company={company}
        site={status.site}
        open={sending}
        onOpenChange={setSending}
        onSent={() => void loadInvoices()}
      />
    </div>
  );
}
