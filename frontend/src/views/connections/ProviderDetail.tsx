import { AlertTriangle, Loader2, LogIn, Unplug } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { ComposioConnectedAccount } from "@/api/composio";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { connectedOn } from "@/lib/connection-detail";
import { toolkitSlug } from "@/lib/connections";
import { accountSummary, tallyAccounts } from "@/lib/provider-grid";
import {
  UsageSection,
  type Usage,
  useConnectionUsage,
} from "@/views/connections/connection-usage";
import type { GridProvider } from "@/lib/provider-grid";

/**
 * What this panel has been opened on — one of the connection systems
 * OpenCompany has, with everything that system's arm needs to act.
 *
 * A discriminated union rather than a bag of optional props (issue #821). The
 * two arms have genuinely different controls, and the failure this shape rules
 * out is a real one on this page: `disconnectConnection` answers 200 for a
 * Composio provider while revoking nothing (see `ConnectionsView.disconnect`).
 * Carrying each arm's handlers *inside* its own variant means an MCP subject
 * cannot be handed a Composio revoke, and a Composio subject cannot be opened
 * without one — neither is a rule a caller has to remember.
 */
export type ConnectionSubject = {
  kind: "composio";
  provider: GridProvider;
  /**
   * Nothing to authorize against — no credential of any tier resolves, so a
   * Connect could only fail. Stated here rather than only on the page behind
   * the panel: this is where the button is.
   */
  noCredential: boolean;
  onConnectAnother: (provider: GridProvider) => void;
  onDisconnectAccount: (
    provider: GridProvider,
    account: ComposioConnectedAccount,
  ) => void;
};

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /** What to show, or `null` when nothing is open. */
  subject: ConnectionSubject | null;
  /** Whether this viewer may change what the company connects through (#403). */
  canManage: boolean;
  /** A connect or disconnect is in flight somewhere on the page. */
  busy: boolean;
  onClose: () => void;
}

/**
 * A connection as an object you can open (issues #404, #821).
 *
 * Opens whether or not it is connected — "connected **or not**" is #404's
 * wording, and OpenHuman's `ComposioConnectModal` is a phase machine that opens
 * on a disconnected toolkit and connects from inside. Keeping the one-click
 * connect on the tile and the panel for connected providers only would have
 * been a nicer first click and a worse answer to "what is this provider, and
 * what is wired to it" — which is the question the issue exists to make
 * answerable.
 *
 * ## Two systems, one vocabulary
 *
 * A Composio provider and a remote MCP server open into the same panel, because
 * the alternative — a second MCP-specific panel — is how two surfaces come to
 * describe the same idea with two vocabularies and then drift (issue #821, and
 * #414 for the last time it happened to the MCP screen specifically). What
 * differs between the arms is what each system genuinely records; what does not
 * differ is the shape of the answer, or the rule about inventing one.
 *
 * ## What this is honest about, and why each line is worded the way it is
 *
 * The requirement is not "show more fields" — it is that every claim on this
 * panel is one the system can actually back. Four of them could not be made
 * naively:
 *
 *  1. **Which account an agent uses** (Composio). OpenHuman marks the first of
 *     several as the default, and inheriting that was the plan. It was not true
 *     when this panel was written: `composio_execute` posted `{tool, arguments}`
 *     and **no connection id**, so nothing on this side selected an account and
 *     a "Default" chip would have named a decision the product did not make.
 *     Issue #820 makes the decision real — `ComposioExecuteTool` sends
 *     `connectionId` for a toolkit the company has chosen for — so the panel no
 *     longer says the choice is impossible. It still marks nothing: the choice
 *     is made in one place (`AccountChoiceSection`), and a second surface
 *     reading it back is how two surfaces come to disagree. Unchosen stays the
 *     ordinary state, and Composio resolves it exactly as before.
 *  2. **When it was connected.** Only Composio records it. The native
 *     `oauth/{provider}` store keeps `{token, account}` and journals nothing on
 *     connect, and MCP has no such concept — so for those the date is not
 *     merely missing, it is unrecoverable. An empty cell would read as
 *     "never"; the panel says "not recorded".
 *  3. **What has gone through it.** Composio meters a successful
 *     `composio_execute` per toolkit and the MCP bridge meters a successful
 *     remote call under `mcp:<server>` (`src/metering/oauth.rs`), so the number
 *     here is real for both — but it is per *connection*, not per *account*, and
 *     both accounts of a two-Gmail company land on the same total. Rendering it
 *     against one account would be a number that means something else.
 *  4. **Whether an MCP server is connected.** It has no connection object, so
 *     the answer is assembled from `enabled` and the last probe — two facts one
 *     badge would collapse, with "never probed" the case that has no honest
 *     single-badge rendering at all. See `mcpStanding`.
 *
 * ## Composio and MCP only
 *
 * The native OAuth catalog gets no detail view. Its credential is written and
 * read by nothing (#396), and a page devoted to how healthy a connection is,
 * is exactly where an inert one would look most alive. A provider that also
 * holds a native credential says so as a caveat, not as a second connection.
 *
 * #822 finished the other half of that reasoning: the catalog is no longer
 * *listed* either, so the case this panel declines to open now mostly cannot
 * arise from the grid at all. What still reaches the caveat below is a provider
 * connected natively and offered by Composio — one connection object, one
 * inert secret beside it.
 */
export function ProviderDetail({
  client,
  company,
  subject,
  canManage,
  busy,
  onClose,
}: Props) {
  // The `byProvider` key this subject's calls land on. `null` for a closed
  // panel, and — unreachably, since the host rejects an unnamed server — for an
  // MCP server whose name normalizes away; the MCP arm renders that case rather
  // than reading the host's shared `unknown` bucket as this server's total.
  const usageKey = subject === null ? null : toolkitSlug(subject.provider.slug);
  // The read carries the key it was made for. The sheet changes subject without
  // unmounting, and state set in an effect lands one render *after* the subject
  // does — so a figure kept as a bare number would paint against the new
  // provider for that frame, which is one provider's call count under another
  // provider's name. Nothing is read back unless the key still matches.
  const usage = useConnectionUsage(client, company, usageKey);

  return (
    <Sheet open={subject !== null} onOpenChange={(next) => !next && onClose()}>
      <SheetContent side="right" className="w-full overflow-y-auto">
        {subject?.kind === "composio" && (
          <ComposioBody
            subject={subject}
            canManage={canManage}
            busy={busy}
            usage={usage}
          />
        )}
      </SheetContent>
    </Sheet>
  );
}

/** A Composio provider: its accounts, and what may be done with them. */
function ComposioBody({
  subject,
  canManage,
  busy,
  usage,
}: {
  subject: Extract<ConnectionSubject, { kind: "composio" }>;
  canManage: boolean;
  busy: boolean;
  usage: Usage;
}) {
  const { provider, noCredential, onConnectAnother, onDisconnectAccount } =
    subject;
  const accounts = provider.accounts ?? [];
  // Counted through the shared rule so this panel, the tile that opened it,
  // and the summary line above all mean one thing by "connected" (issue #923).
  const { live } = tallyAccounts(accounts);

  return (
    <>
      <SheetHeader className="border-b">
        <SheetTitle className="flex items-center gap-2">
          <span className="truncate">{provider.label}</span>
        </SheetTitle>
        <SheetDescription className="flex flex-wrap items-center gap-1.5 text-xs">
          <Badge variant="outline" className="font-normal">
            {/* Which of the three systems this provider is reached through —
                one of the questions the issue asks the view to answer, and
                stated rather than left to be inferred from the fact that a
                panel opened at all. */}
            Composio
          </Badge>
          {/* The same rule the tile grid states, so the panel and the tile that
              opened it cannot describe one set of accounts two ways (issue
              #923). This counted only live accounts already — correctly — but
              collapsed "holds accounts, none usable" to "not connected", which
              is the half the grid got wrong too. */}
          <span>{accountSummary(accounts) ?? "not connected"}</span>
        </SheetDescription>
      </SheetHeader>

      <div className="space-y-4 px-4 pb-6">
        <section className="space-y-2" aria-label="Connected accounts">
          {accounts.map((account) => (
            <AccountRow
              key={account.id}
              account={account}
              canManage={canManage}
              busy={busy}
              onDisconnect={() => onDisconnectAccount(provider, account)}
            />
          ))}
          {accounts.length === 0 && (
            <p className="text-xs text-muted-foreground">
              {/* An account and a *usable* account are different states, and a
                  provider that has never been connected is a third. The empty
                  case says which of them this is rather than "not connected",
                  which would cover all three. */}
              No Composio account is connected for {provider.label}, so its
              agents have none of its tools.
            </p>
          )}
        </section>

        {live > 1 && (
          // #819 wrote this paragraph to say the choice did not exist —
          // "`composio_execute` sends no connection id, so Composio resolves
          // it. Disconnect the one you do not want an agent to use." #820 is
          // what makes that false, so the two branches meeting is what forces
          // this edit: the panel must not deny a control the same page offers.
          <p className="flex items-start gap-2 rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
            <AlertTriangle className="mt-px size-3 shrink-0" />
            <span>
              Holding several accounts is fine — they are the company&apos;s,
              and every agent works through them. Which one an agent acts as is
              set under{" "}
              <span className="font-medium">Which account agents act as</span>{" "}
              on the Connections page; until one is chosen, Composio resolves it
              for the company as it always has.
            </span>
          </p>
        )}

        {provider.via.includes("native") && (
          <p className="flex items-start gap-2 rounded-md bg-muted/40 p-2 text-xs text-muted-foreground">
            <AlertTriangle className="mt-px size-3 shrink-0" />
            <span>
              This company also stores a self-hosted OAuth credential for{" "}
              {provider.label}. No agent reads it (issue #396), it is not what
              the accounts above are, and disconnecting here does not touch it.
            </span>
          </p>
        )}

        {/* Hidden for a provider that is neither connected nor has been used:
            "0 calls in the last 30 days" is true there but says nothing, and in
            open mode this panel opens over a catalog of 123 providers that have
            never been touched. A non-zero count on a disconnected provider is
            the opposite — it is the interesting case, so it stays. */}
        {(provider.connected ||
          (usage.load === "ready" && (usage.calls ?? 0) > 0)) && (
          <>
            <Separator />
            <UsageSection
              usage={usage}
              perConnection={`Successful tool calls through ${provider.label}, counted per provider rather than per account — a call through any account above lands on this one total.`}
            />
          </>
        )}

        {canManage && (
          <>
            <Separator />
            <section className="space-y-2" aria-label="Manage this connection">
              <Button
                variant="outline"
                className="w-full"
                disabled={busy || noCredential}
                onClick={() => onConnectAnother(provider)}
                data-testid="provider-detail-connect-another"
              >
                <LogIn className="size-4" />
                {accounts.length === 0
                  ? "Connect an account"
                  : "Connect another account"}
              </Button>
              {noCredential && (
                <p className="text-xs text-muted-foreground">
                  There is no credential for this company to authorize against
                  yet, so a sign-in has nothing to present. Set the
                  company&apos;s TinyHumans credential on the Connections page
                  first.
                </p>
              )}
              {accounts.length > 0 && (
                <p className="text-xs text-muted-foreground">
                  Disconnecting removes the connection at Composio, so agents
                  lose these tools on their next turn. It does not sign the
                  company out of {provider.label}, and it does not delete
                  anything there.
                </p>
              )}
            </section>
          </>
        )}

        {!canManage && (
          <p className="text-xs text-muted-foreground">
            Only an admin can connect or disconnect an account here.
          </p>
        )}
      </div>
    </>
  );
}

/** One connected account: what it is, since when, and how to release it. */
function AccountRow({
  account,
  canManage,
  busy,
  onDisconnect,
}: {
  account: ComposioConnectedAccount;
  canManage: boolean;
  busy: boolean;
  onDisconnect: () => void;
}) {
  return (
    <div
      className="flex items-start justify-between gap-2 rounded-lg border border-border px-3 py-2"
      data-testid={`provider-account-${account.id}`}
    >
      <div className="min-w-0 space-y-0.5">
        <p className="truncate text-sm font-medium">
          {/* Composio publishes no label for some providers, and guessing one
              from the toolkit makes two accounts indistinguishable at the one
              moment an operator has to tell them apart. */}
          {account.account ?? "Account name not published"}
        </p>
        <p className="text-xs text-muted-foreground">
          <Badge variant="outline" className="mr-1.5 font-mono font-normal">
            {/* Composio's own status string, not a re-spelling of it: "set up
                and since expired" and "never finished setting up" are
                different sentences and both flatten to "not connected". */}
            {account.status}
          </Badge>
          {connectedOn(account.createdAt)}
        </p>
      </div>
      {canManage && (
        <Button
          variant="destructive"
          size="sm"
          disabled={busy}
          onClick={onDisconnect}
          aria-label={`Disconnect ${account.account ?? account.id}`}
        >
          {busy ? (
            <Loader2 className="size-3.5 animate-spin" />
          ) : (
            <Unplug className="size-3.5" />
          )}
          Disconnect
        </Button>
      )}
    </div>
  );
}
