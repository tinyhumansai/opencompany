import { useEffect, useState } from "react";
import { Info } from "lucide-react";

import { me as fetchMe } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { McpServersSection } from "@/views/connections/McpServersSection";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

/**
 * Settings, MCP Servers: the company's tool servers on a page of their own.
 *
 * The body is [`McpServersSection`](./connections/McpServersSection.tsx) — the
 * same component the Connections page renders — because this page used to be a
 * second, contradictory implementation of it. That one was written against an
 * API the host does not serve: `GET .../mcp/servers` as `{ servers: [...] }`
 * where the host answers a bare array, servers keyed by `server_id` where the
 * host keys by name, and `/connect` and `/disconnect` routes that exist
 * nowhere. Opening this page therefore stored `undefined` into the server list
 * and threw `Cannot read properties of undefined (reading 'length')` on render
 * (issue #414). One surface is what keeps the two from disagreeing again.
 */
export function McpServersView({ client, company }: Props) {
  // Adding or removing a server changes what tools the company's agents can
  // call, so it is an admin's (issue #403). Courtesy only: the host answers 403
  // whatever this says. Reading the installed set stays open.
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
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-5xl space-y-6 px-4 py-6">
        <div className="space-y-1">
          <h2 className="text-2xl font-semibold tracking-tight">MCP Servers</h2>
          <p className="text-sm text-muted-foreground">
            The tool servers this company&apos;s agents can call, from its manifest and the ones you
            add here.
          </p>
        </div>

        {!canManage && (
          <Alert data-testid="mcp-read-only">
            <Info className="size-4" />
            <AlertTitle>Only an admin can change this company&apos;s tool servers</AlertTitle>
            <AlertDescription>
              A server here hands every agent a new set of tools, so an admin adds and removes them.
              You can see what is installed.
            </AlertDescription>
          </Alert>
        )}

        <McpServersSection
          client={client}
          company={company}
          canManage={canManage}
          chrome="standalone"
        />
      </div>
    </div>
  );
}
