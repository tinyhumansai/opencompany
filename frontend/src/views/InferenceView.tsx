import type { OpenCompanyClient } from "@/api/client";
import { AdminOnlyNotice } from "@/components/admin-only-notice";
import { PageHeader } from "@/components/page-header";
import { PageTabs, pageTabIds, type PageTab } from "@/components/page-tabs";
import { useHashTab } from "@/hooks/use-hash-tab";
import { useCanManage } from "@/hooks/use-can-manage";
import { ProvidersTab } from "@/inference/ProvidersTab";
import { RoutingTab } from "@/inference/RoutingTab";
import { useInference } from "@/inference/use-inference";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/**
 * The two questions this page answers: what this company can reach a model
 * through, and which provider each workload goes to.
 *
 * They were one form with a tier grid wedged between the base URL and the key,
 * so setting a key meant scrolling past six model selects and choosing models
 * meant scrolling past a credential you set once a quarter. They are now two
 * tabs over **one read** — `useInference` — because the Providers tab showing a
 * provider the Routing tab has no target for is the first thing that goes wrong
 * with two.
 *
 * The tab **ids** are unchanged (`connect`, `routing`): `#/connections/inference`
 * is linked from the chat pane's "cannot reach a model" banner and from workflow
 * run rows, and relabelling a tab is not a reason to break a link. Only the
 * labels moved.
 */
const INFERENCE_TABS = [
  { id: "connect", label: "LLM Providers" },
  { id: "routing", label: "Routing" },
] as const satisfies readonly PageTab<string>[];

type InferenceTab = (typeof INFERENCE_TABS)[number]["id"];

export function InferenceView({ client, company }: Props) {
  const [tab, setTab] = useHashTab<InferenceTab>(
    INFERENCE_TABS.map((t) => t.id),
    "connect",
  );
  // Changing the model or the key changes what every teammate's turn costs, so
  // it is an admin's.
  const canManage = useCanManage(client, company);
  const inference = useInference(client, company);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader
        title="LLM"
        width="full"
        description="Configure AI providers, local models, and the agent chat tools."
        tabs={
          <PageTabs
            tabs={INFERENCE_TABS}
            value={tab}
            onChange={setTab}
            idBase="inference"
            aria-label="Inference views"
          />
        }
      />
      <div className="min-h-0 w-full flex-1 space-y-6 overflow-y-auto px-4 py-6">
        {!canManage && (
          <AdminOnlyNotice
            testId="inference-read-only"
            title="Only an admin can change this company's model"
          >
            The model and its key decide what every agent&apos;s turn costs, so an admin sets
            them. You can see what is configured.
          </AdminOnlyNotice>
        )}

        <div
          role="tabpanel"
          id={pageTabIds("inference", tab).panel}
          aria-labelledby={pageTabIds("inference", tab).tab}
        >
          {tab === "connect" ? (
            <ProvidersTab state={inference} actions={inference} canManage={canManage} />
          ) : (
            <RoutingTab
              client={client}
              company={company}
              state={inference}
              actions={inference}
              canManage={canManage}
            />
          )}
        </div>
      </div>
    </div>
  );
}
