import type { OpenCompanyClient } from "@/api/client";
import { AdminOnlyNotice } from "@/components/admin-only-notice";
import { PageHeader } from "@/components/page-header";
import { useCanManage } from "@/hooks/use-can-manage";
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
  // it is an admin's.
  const canManage = useCanManage(client, company);

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
          <AdminOnlyNotice
            testId="inference-read-only"
            title="Only an admin can change this company's model"
          >
            The model and its key decide what every teammate&apos;s turn costs, so an admin sets
            them. You can see what is configured.
          </AdminOnlyNotice>
        )}

        <InferenceSection client={client} company={company} canManage={canManage} />
      </div>
    </div>
  );
}
