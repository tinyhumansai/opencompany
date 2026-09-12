import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { CreditCard, EllipsisVertical, ExternalLink, KeyRound, Wallet } from "lucide-react";
import { toast } from "sonner";

import { me as fetchMe } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import {
  getCompanyBilling,
  getCompanyCredential,
  setCompanyCredential,
  type CompanyBilling,
  type CompanyCredentialStatus,
} from "@/api/credential";
import { ApiError } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";
import {
  ACCOUNT_LABEL,
  REMOVAL_CONSEQUENCE,
  REMOVAL_AND_THINKING,
  accountShape,
  accountSubline,
  balanceLine,
  canRemoveKey,
  headerAction,
  type AccountLoad,
} from "@/views/connections/account";
import { AccountKeyDialog } from "@/views/connections/AccountKeyDialog";
import { ConnectTinyHumansButton } from "@/views/connections/ConnectTinyHumansButton";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/**
 * Account — the one page about the account this company spends through.
 *
 * ## Why it is its own page rather than a card on Apps
 *
 * The key was reachable from two places and explained by neither. On
 * Connections → Apps it sat under a heading about third-party accounts, framed
 * as the thing that makes Gmail connectable; on Inference it appeared as one
 * option in a provider picker. Both are true and both are consequences. What
 * neither page could say, because neither is about it, is the plain thing: this
 * key is the company's account with TinyHumans, every teammate's thinking and
 * every connected app is billed to it, and when it runs out the company stops
 * working.
 *
 * A page whose subject is the account can say that once, show what is left on
 * it, and put the two actions — connect, top up — where somebody looking for
 * them would look.
 *
 * ## What it shows and what it refuses to
 *
 * Balance and plan, read through the host with the key it already holds
 * (`GET …/credential/billing`). Never the key itself: it is write-only on the
 * host and is not returned by any route, which is what makes "the console leaked
 * it" not a thing that can happen here.
 *
 * No checkout, either. Topping up and changing a plan move money and belong to
 * a person signed in to their own TinyHumans account — so those are links out,
 * to whichever hub this host is pointed at.
 *
 * ## The shape, and why it is the LLM page's
 *
 * Two cards: one carrying the action, one carrying the state as rows. It is the
 * same furniture `inference/ProvidersTab` uses, for the same reason — an
 * operator crossing from that page should recognise the language rather than
 * relearn it.
 *
 * It is **not** a list, and nothing here pretends otherwise. There is one
 * credential, so there is no toggle (nothing to enable against), no default
 * marker (there is one of these), and no add-a-provider modal. What the row
 * shape buys on a page with one thing on it is the sub-line: a fixed place to
 * say which tier actually answers, which is the fact this page most often gets
 * asked for and the one the old prose buried.
 *
 * The balance is the second row rather than a card of its own. It is another
 * fact about the same account, with its own two controls, and a card to hold
 * one number was a card explaining itself.
 *
 * ## Four states, and the third and fourth are the ones that were wrong
 *
 * Nothing set; this company's own key; **no company key but a live instance
 * identity** — the hosted case, where the server's account is quietly paying
 * and "not configured" is simply false; and **a store the host could not read**,
 * which `company_key::resolve` deliberately propagates rather than degrading,
 * because a connection made under a silently-borrowed identity belongs to the
 * wrong account invisibly and permanently. All four are decided in
 * `./account.ts`, with a unit test each, rather than in the JSX below.
 */
export function ApiKeyView({ client, company }: Props) {
  // Resolved here rather than taken as a prop, the same way `OAuthView` does
  // it: the section is a dispatcher and has no user plane of its own, and a
  // page that asked its parent for authority would be trusting a value nothing
  // on this rail is responsible for keeping true.
  //
  // Courtesy, not enforcement — the host refuses a non-admin's write whatever
  // this says. What it prevents is offering somebody a credential field whose
  // submit could only ever 403.
  const [canManage, setCanManage] = useState(false);
  const [status, setStatus] = useState<CompanyCredentialStatus | null>(null);
  const [billing, setBilling] = useState<CompanyBilling | null>(null);
  const [load, setLoad] = useState<AccountLoad>("loading");
  const [generation, setGeneration] = useState(0);
  const [editing, setEditing] = useState(false);
  /** Whether the Remove-key confirmation is open. */
  const [removing, setRemoving] = useState(false);
  const [busy, setBusy] = useState(false);

  // Discards the result of a request that is no longer the latest one asked
  // for — a monotonic counter rather than "is this still the wanted company",
  // because a company can stay the same while `client` is reseated to another
  // host (issue tracked alongside `CompanyCredentialCard`'s identical guard):
  // comparing only `company` would let the old host's slower response land
  // last and overwrite the new host's credential status and balance.
  const requestGeneration = useRef(0);

  const refresh = useCallback(async () => {
    setLoad("loading");
    const asked = ++requestGeneration.current;
    try {
      // Together: the page draws one story out of both, and sequencing them
      // would show a connected company an empty wallet for a frame.
      const [credential, money] = await Promise.all([
        getCompanyCredential(client, company),
        getCompanyBilling(client, company).catch(
          (err): CompanyBilling => ({
            // A rejected billing request is not the same fact as "no key". The
            // credential result (just resolved above, in the same batch) is
            // what actually says whether a key exists; a company that has one
            // must keep seeing the unavailable explanation rather than have
            // the balance row silently vanish. `configured` is corrected
            // against the credential result once both have settled below.
            configured: false,
            unavailable:
              err instanceof ApiError
                ? err.message
                : "The balance could not be read just now.",
          }),
        ),
      ]);
      if (asked !== requestGeneration.current) return;
      setStatus(credential);
      setBilling(
        money.unavailable !== undefined
          ? { ...money, configured: credential.configured }
          : money,
      );
      setLoad("ready");
    } catch {
      if (asked !== requestGeneration.current) return;
      // Not "no key". The credential route surfaces an unreadable secret store
      // as a 5xx precisely so this case exists to be told apart, and the row
      // below says so in words rather than showing an empty state that would
      // send an admin to set a key they may already have set.
      //
      // Both are dropped, not left standing. A balance from the last good read
      // under a row that has just admitted it does not know whose account this
      // is would be the most confident thing on the page and the least earned.
      setStatus(null);
      setBilling(null);
      setLoad("error");
    }
  }, [client, company]);

  useEffect(() => {
    void refresh();
  }, [refresh, generation]);

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

  /** Set, rotate or (with an empty value) clear the company's key. */
  const write = useCallback(
    async (key: string, mode: "save" | "clear") => {
      setBusy(true);
      try {
        const result = await setCompanyCredential(client, company, key);
        // Unconditional: the write really did land, and an admin who navigated
        // away mid-request is still owed that fact.
        toast.success(mode === "save" ? "Key saved." : "Key removed.", {
          description: result.note,
        });
        setEditing(false);
        setGeneration((n) => n + 1);
      } catch (err) {
        // The host's own reason where it sent one — an admin-only refusal or a
        // store failure says something specific, and a generic "couldn't save"
        // throws away the only actionable part.
        toast.error(
          err instanceof ApiError
            ? err.message
            : mode === "save"
              ? "Couldn't save the key."
              : "Couldn't remove the key.",
        );
      } finally {
        setBusy(false);
      }
    },
    [client, company],
  );

  const shape = accountShape(load, status);
  const configured = status?.configured ?? false;
  const removable = canRemoveKey(status);
  const action = headerAction(status, canManage);
  const balance = balanceLine(billing);
  const account = status?.account;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {/* The console's one page header (#1763) rather than a hand-rolled `h1`:
          a routed view that titles itself is how twelve heading styles happened
          the first time, and `page-header-adoption` is the test that says so. */}
      <PageHeader
        title="Account"
        width="full"
        description="The TinyHumans account this company acts and spends through."
      />

      <div className="min-h-0 w-full flex-1 space-y-6 overflow-y-auto px-4 py-6">
        {/* The action, and the one sentence the action does not itself say: that
            a single key covers both halves. Everything else the old page opened
            with — what write-only means, what Clear does, what happens at zero —
            described a control that was visible while it was being read. */}
        <Card>
          <CardContent className="flex flex-wrap items-center justify-between gap-3">
            <div className="grid gap-0.5">
              <h2 className="text-sm font-medium">{ACCOUNT_LABEL}</h2>
              {/* The billing consequence, on the card carrying the button it is
                  true of, and visible before anything is saved. Connecting
                  stores the identity and declares the `managed` provider, and
                  managed turns resolve through this same key (#2266) — so it
                  moves the thinking bill as well.

                  Qualified, because the managed chain has two rungs above this
                  one: a key pasted for TinyHumans on the LLM page
                  (`provider/tinyhumans/key`), and the legacy `inference/key`.
                  Where either is set it keeps answering, and connecting moves
                  the apps without moving the bill. Saying so is cheaper than
                  being wrong on a company that has one. */}
              <p className="text-xs text-muted-foreground">
                One key for the apps your agents act through and the models they think with.
                Connecting points both at this company&apos;s account — unless the LLM page
                already holds a TinyHumans key of its own, which keeps precedence.
              </p>
            </div>
            {/* Whichever action is live, never both and never a dead one. The
                grant is the short path where the host has a hub; the paste
                dialog is the only route where it does not.

                The connect button is **mounted unconditionally** and hides
                itself — `available`/`canManage` already make it render null —
                rather than being gated by `action` out here. It is not only a
                button: the effect that redeems a returning grant lives in it,
                and the grant arrives on a fresh boot with the code already
                stripped from the address bar and held in a module-local box
                that a reload empties. Gating the mount on state that is null
                while the credential read is in flight, or that stays null when
                it fails, would drop the credential on the floor with no way
                back to it. Only what is *shown* may depend on `action`. */}
            <ConnectTinyHumansButton
              client={client}
              company={company}
              available={action === "connect" && (status?.hubLink ?? false)}
              canManage={canManage}
              configured={configured}
              hint={false}
              onConnected={() => setGeneration((n) => n + 1)}
            />
            {action === "key" && (
              <Button type="button" onClick={() => setEditing(true)} data-testid="account-add-key">
                <KeyRound className="size-4" />
                {removable ? "Replace key" : "Add a key"}
              </Button>
            )}
          </CardContent>
        </Card>

        {/* The state. One row for the account, one for what is left on it. */}
        <Card>
          <CardContent className="px-0">
            {/* Named for what the card holds, not for one of the three states
                it can be in: a heading reading "Connected" over "No account
                connected yet." or over "The host could not say" contradicts the
                only line under it. */}
            <h3 className="px-4 pb-2 text-xs font-medium tracking-wide text-muted-foreground uppercase">
              {shape === "connected" ? "Connected" : "Account"}
            </h3>

            {load === "loading" ? (
              <div className="px-4 pb-2">
                <Skeleton className="h-10 rounded-md" />
              </div>
            ) : shape === "empty" ? (
              // Nothing resolves anywhere. Not a row: there is no account to
              // describe, and a row saying so would be a heading over blank space.
              <div
                className="flex flex-col items-start gap-3 px-4 py-6"
                data-testid="account-empty"
              >
                {/* Scoped to what this credential actually governs. The old page
                    said "agents cannot think and no provider can be connected"
                    here, which is false on a company whose LLM page holds a
                    provider key of its own — `inference/key` resolves without
                    this one, so such a company thinks perfectly well and would
                    be sent to fix something that is not broken. The exception is
                    named rather than denied. */}
                <p className="text-sm">
                  <span className="font-medium">No account connected yet.</span>{" "}
                  <span className="text-muted-foreground">
                    Apps cannot be connected, and there is no TinyHumans balance to think
                    against — though a provider key set on the LLM page still works.
                  </span>
                </p>
                {/* No button here, deliberately, though the list this borrows its
                    shape from has one. The page it replaces carried the warning
                    in its own source: two identical primary buttons on one screen
                    leave a reader working out whether they do the same thing. On
                    a list of providers the header action and the empty-state
                    action are inches apart in a long card; on a page with one
                    credential they are adjacent and identical, and the header
                    card's action is already in view directly above this. The
                    sentence stays — it is what the empty state is for. */}
              </div>
            ) : (
              <ul className="divide-y divide-border" data-testid="account-rows">
                <li className="flex items-center gap-3 px-4 py-3" data-testid="account-row">
                  <Mark label={ACCOUNT_LABEL} />
                  <span className="grid min-w-0 flex-1 leading-tight">
                    <span className="truncate text-sm font-medium">{ACCOUNT_LABEL}</span>
                    <span
                      className="truncate text-xs text-muted-foreground"
                      data-testid="account-row-subline"
                    >
                      {accountSubline(load, status)}
                    </span>
                  </span>

                  {/* Revoking a key is not something this console can do — it
                      ends an instance's access and lives behind that person's own
                      sign-in — so it is a link out, and it sits on the row it is
                      about rather than in a footer. */}
                  {account && (
                    <a
                      className="inline-flex items-center gap-1 text-xs font-medium underline underline-offset-4"
                      href={account.manageKeysUrl}
                      target="_blank"
                      rel="noreferrer"
                      data-testid="hub-manage-keys"
                    >
                      Manage keys <ExternalLink className="size-3" />
                    </a>
                  )}

                  <DropdownMenu>
                    <DropdownMenuTrigger
                      render={
                        <Button
                          variant="ghost"
                          size="icon"
                          disabled={!canManage || busy || shape === "unknown"}
                          aria-label={`${ACCOUNT_LABEL} actions`}
                          data-testid="account-row-menu"
                        />
                      }
                    >
                      <EllipsisVertical className="size-4" />
                    </DropdownMenuTrigger>
                    <DropdownMenuContent align="end">
                      <DropdownMenuItem onClick={() => setEditing(true)}>
                        {removable ? "Replace key" : "Add a key"}
                      </DropdownMenuItem>
                      {/* Offered only when there is a key of this row's own to
                          remove. The instance's identity is not this row's to
                          take away, and a Remove that clears nothing is the
                          control-that-cannot-act the LLM page deleted a toggle
                          over. */}
                      {/* Opens the confirmation rather than clearing on the
                          press. Clearing is destructive, irreversible from this
                          console — the hub emits a key's plaintext once — and
                          what it costs depends on state the menu item cannot
                          show. A menu item that silently revokes a company's
                          identity is the shape of mistake that cost this repo a
                          live key today. */}
                      {removable && (
                        <DropdownMenuItem
                          variant="destructive"
                          onClick={() => setRemoving(true)}
                          data-testid="account-remove-key"
                        >
                          Remove key
                        </DropdownMenuItem>
                      )}
                    </DropdownMenuContent>
                  </DropdownMenu>
                </li>

                {/* Only once there is an account of this company's own: a row
                    reading "$0.00" for a company with no wallet would be a
                    made-up fact. */}
                {balance && (
                  <li className="flex items-center gap-3 px-4 py-3" data-testid="account-balance">
                    <Mark icon={<Wallet className="size-4" />} label="Balance" />
                    <span className="grid min-w-0 flex-1 leading-tight">
                      <span
                        className={cn(
                          "truncate text-sm font-medium tabular-nums",
                          balance.low && "text-status-blocked-text",
                        )}
                        data-testid="billing-balance"
                      >
                        {balance.amount ?? "Balance unknown"}
                      </span>
                      <span className="truncate text-xs text-muted-foreground">
                        {balance.detail}
                      </span>
                    </span>

                    {billing?.summary?.manageUrl && (
                      <a
                        className="inline-flex items-center gap-1 text-xs font-medium underline underline-offset-4"
                        href={billing.summary.manageUrl}
                        target="_blank"
                        rel="noreferrer"
                        data-testid="billing-manage-plan"
                      >
                        Manage plan <ExternalLink className="size-3" />
                      </a>
                    )}

                    {/* The hub's own top-up address where it sent one, else the
                        one the host derived. Moving money is a decision made
                        signed in on the hub, so this is a link and never a
                        route here. */}
                    {(billing?.summary?.topUpUrl ?? account?.topUpUrl) && (
                      <a
                        className="inline-flex items-center gap-1 text-xs font-medium underline underline-offset-4"
                        href={billing?.summary?.topUpUrl ?? account?.topUpUrl}
                        target="_blank"
                        rel="noreferrer"
                        data-testid="billing-top-up"
                      >
                        <CreditCard className="size-3" /> Top up <ExternalLink className="size-3" />
                      </a>
                    )}
                  </li>
                )}
              </ul>
            )}
          </CardContent>
        </Card>

        <AccountKeyDialog
          open={editing}
          onOpenChange={setEditing}
          replacing={removable}
          busy={busy}
          onSubmit={(key) => void write(key, "save")}
        />

        {/* Names what actually depends on the key, and what happens next rather
            than only what is lost. The two sentences live in `account.ts` with
            a test each: they are the page's one irreversible claim, and the
            reasoning behind each half — why both fallbacks are offered rather
            than one guessed at, and why the removal is not allowed to promise
            that the billing stops — belongs next to the assertion that holds
            it. */}
        <AlertDialog open={removing} onOpenChange={setRemoving}>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>Remove this company&apos;s account key?</AlertDialogTitle>
              <AlertDialogDescription>{REMOVAL_CONSEQUENCE}</AlertDialogDescription>
              <AlertDialogDescription>{REMOVAL_AND_THINKING}</AlertDialogDescription>
              <AlertDialogDescription>
                The key itself cannot be recovered from here — TinyHumans shows a key&apos;s value
                once, when it is created. You would have to connect again or paste a new one.
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel>Keep the key</AlertDialogCancel>
              <AlertDialogAction
                onClick={() => void write("", "clear")}
                className="bg-destructive text-white hover:bg-destructive/90"
                data-testid="account-remove-key-confirm"
              >
                Remove key
              </AlertDialogAction>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      </div>
    </div>
  );
}

/**
 * The row's mark: two letters, or an icon where the row is a figure rather than
 * a name.
 *
 * Local and deliberately plain. It is here to give the rows a consistent left
 * edge — the thing that makes two rows read as one list — not to carry
 * information, so it takes no colour of its own and says nothing a screen
 * reader needs to hear.
 */
function Mark({ label, icon }: { label: string; icon?: ReactNode }) {
  return (
    <span
      aria-hidden="true"
      className="flex size-8 shrink-0 items-center justify-center rounded-md bg-muted text-xs font-medium text-muted-foreground"
    >
      {icon ?? label.slice(0, 2).toUpperCase()}
    </span>
  );
}
