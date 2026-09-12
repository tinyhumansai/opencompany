import { Info } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { ComposioSection } from "@/views/connections/ComposioSection";
import { useComposioCredential } from "@/views/connections/use-composio-credential";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/**
 * Composio: the key the app catalog runs on.
 *
 * # Why this is a page and not a tab on Apps
 *
 * It was both, in order. One column on the Apps page with the credentials on
 * top — so the page opened on a key form an operator sets once and scrolled
 * past it every time to reach the provider list they came for. Then two tabs on
 * that page, on the argument that a credential exists only to make a provider
 * connectable: one subject, two views.
 *
 * What that argument missed is what the Connections rail is a list of. The rail
 * groups its pages into things you connect *to* and the keys that authorise
 * them (issue #2259), and a key that is reachable only by opening the page
 * named after the things it unlocks, then finding a tab, is in neither group —
 * it is filed under the wrong half of the distinction the rail exists to make.
 * A row of its own puts it where an operator looks for a key.
 *
 * # What must not break, and what keeps it honest
 *
 * `OAuthView`'s header used to carry a load-bearing comment against exactly
 * this split: `ComposioSection` looks self-contained, but `ProvidersSection`
 * reads the credential's `credentialSource`, `granted`, `openMode` and catalog
 * warning to decide what every provider tile renders — the credential is the
 * engine the provider list runs on.
 *
 * That is still true, and it is a **data** dependency rather than a layout one:
 * what the grid needs is the credential's state, not the credential's form
 * sitting above it in the same scroll. So the state lifted into
 * `useComposioCredential`, which both pages mount and neither page fetches for
 * itself. Two surfaces disagreeing about whether the company has a credential
 * is the failure the old comment named (issues #582 and #586), and one shared
 * read is what prevents it — the Apps page re-probes through the same hook on
 * every mount, so a key saved here is already true of the tiles by the time the
 * operator is looking at them.
 */
export function ComposioView({ client, company }: Props) {
  const credential = useComposioCredential(client, company);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader
        title="Composio"
        width="full"
        description={
          <>
            The key your company&apos;s app catalog runs on. It is what authorises every provider
            on the Apps page.
          </>
        }
      />
      <div className="min-h-0 w-full flex-1 space-y-6 overflow-y-auto px-4 py-6">
        {!credential.canManage && (
          <Alert data-testid="connections-read-only">
            <Info className="size-4" />
            <AlertTitle>Only an admin can change what this company connects through</AlertTitle>
            <AlertDescription>
              A connection belongs to the company — it is the account your agents act
              through — so an admin manages it. You can see everything that is wired here; ask an
              admin to add, change or remove one.
            </AlertDescription>
          </Alert>
        )}

        {/* `CompanyCredentialCard` used to sit here, above the rows: the
            general answer (one TinyHumans key authorising every brokered
            surface) over the Composio-specific one. It is gone from THIS page
            and unchanged on the API Key page, which is the only place it
            renders now.

            Two surfaces for one credential is the reason. The card carried its
            own paste field and Save for the company key, and the rows below
            carry a managed route whose "Billed to this company's TinyHumans
            account" sub-line reports that same key — so the page asked for one
            credential twice, in two visual languages, and an operator reading
            it had to work out whether they were two different keys. Retiring
            that shape is what issue #2259's rework is for; leaving the card
            above it would have kept the old surface alongside the new one.

            What leaves with it: `HubAccountLinks` — "Manage API keys" and "Top
            up balance" — which the card rendered and the rows have no
            equivalent for. Both are still one click away on the API Key page,
            where the key they act on is set. */}

        {/* Remounted on a credential change so its status is re-read: the tier
            it reports (`company` vs `attested` vs `none`) is downstream of the
            key that was just set, and a stale badge would tell the operator
            their change did not land. */}
        <ComposioSection
          key={credential.generation}
          client={client}
          company={company}
          canManage={credential.canManage}
          onChanged={credential.changed}
        />
      </div>
    </div>
  );
}
