import { useEffect, useState } from "react";
import { Info } from "lucide-react";

import { me as fetchMe } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { InferenceSection } from "@/views/connections/InferenceSection";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/**
 * Settings, Inference: which model this company's teammates think with, and
 * whose key pays for it.
 *
 * Its own page since the Connections split. It was a section on a page about
 * third-party accounts, which is the wrong neighbourhood twice over: an
 * inference provider is not an account the company *acts as*, and the question
 * it settles — what every teammate's turn costs and how good it is — is the one
 * an operator comes back to most. The body is
 * [`InferenceSection`](./connections/InferenceSection.tsx), unchanged: the same
 * component, given a page of its own rather than a copy.
 */
export function InferenceView({ client, company }: Props) {
  // Changing the model or the key changes what every teammate's turn costs, so
  // it is an admin's (issue #403). Courtesy only — the host answers 403 whatever
  // this says.
  const [canManage, setCanManage] = useState(false);

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

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader
        title="Inference"
        width="5xl"
        description={
          <>
            The model your teammates think with, and the key their turns are billed to.
          </>
        }
      />
      <div className="mx-auto min-h-0 w-full max-w-5xl flex-1 space-y-6 overflow-y-auto px-4 py-6">
        {!canManage && (
          <Alert data-testid="inference-read-only">
            <Info className="size-4" />
            <AlertTitle>Only an admin can change this company&apos;s model</AlertTitle>
            <AlertDescription>
              The model and its key decide what every teammate&apos;s turn costs, so an admin sets
              them. You can see what is configured.
            </AlertDescription>
          </Alert>
        )}

        <InferenceSection client={client} company={company} canManage={canManage} />
      </div>
    </div>
  );
}
