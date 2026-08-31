import { useEffect, useState } from "react";
import { FileJson, Info, Server } from "lucide-react";

import { me as fetchMe } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { McpServersSection } from "@/views/connections/McpServersSection";
import { McpJsonEditor } from "@/views/mcp/McpJsonEditor";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/**
 * Settings, MCP Servers: the company's tool servers, said two ways.
 *
 * **Connections** is the list — a row per server with its status and its
 * controls as icons. **mcp.json** is the same configuration as one document, in
 * the shape an operator already has in a desktop config: paste a block of
 * servers, or read the whole set at once instead of expanding six rows.
 *
 * The two are tabs and not two pages because they are not two things. Both go
 * through the same host routes into the same store — `…/mcp/servers` per row,
 * `…/mcp/config` for the file — so an edit made in one is visible in the other
 * on its next read, and neither is an import format that can drift from "what is
 * actually configured". A second, parallel MCP surface written against an API
 * the host did not serve is what issue #414 was; the rule that came out of it is
 * one source of truth per question, and a tab does not break it.
 *
 * The rows live in [`McpServersSection`](./connections/McpServersSection.tsx),
 * which the Connections page also renders inline — that is the same surface, not
 * a copy.
 */
export function McpServersView({ client, company }: Props) {
  // Adding or removing a server changes what tools the company's agents can
  // call, so it is an admin's (issue #403). Courtesy only: the host answers 403
  // whatever this says. Reading the installed set stays open.
  const [canManage, setCanManage] = useState(false);
  // Bumped when the document is saved. The rows are keyed on it, so a save that
  // adds or removes servers re-reads the list instead of leaving the other tab
  // describing the configuration as it was before the file was written.
  const [written, setWritten] = useState(0);

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
        title="MCP Servers"
        width="5xl"
        description={
          <>
            The tool servers this company&apos;s teammates can call, from its manifest and the
            ones you add here.
          </>
        }
      />
      <div className="mx-auto min-h-0 w-full max-w-5xl flex-1 space-y-6 overflow-y-auto px-4 py-6">
        {!canManage && (
          <Alert data-testid="mcp-read-only">
            <Info className="size-4" />
            <AlertTitle>Only an admin can change this company&apos;s tool servers</AlertTitle>
            <AlertDescription>
              A server here hands every teammate a new set of tools, so an admin adds and removes
              them. You can see what is installed.
            </AlertDescription>
          </Alert>
        )}

        <Tabs defaultValue="connections">
          <TabsList>
            <TabsTrigger value="connections" data-testid="mcp-tab-connections">
              <Server /> Connections
            </TabsTrigger>
            <TabsTrigger value="json" data-testid="mcp-tab-json">
              <FileJson /> mcp.json
            </TabsTrigger>
          </TabsList>
          <TabsContent value="connections" className="pt-4">
            <McpServersSection
              key={written}
              client={client}
              company={company}
              canManage={canManage}
              chrome="standalone"
            />
          </TabsContent>
          <TabsContent value="json" className="pt-4">
            <McpJsonEditor
              client={client}
              company={company}
              canManage={canManage}
              onSaved={() => setWritten((n) => n + 1)}
            />
          </TabsContent>
        </Tabs>
      </div>
    </div>
  );
}
