import { useCallback, useEffect, useMemo, useState } from "react";
import { Loader2 } from "lucide-react";

import { OpenCompanyClient } from "@/api/client";
import { ApiError, type CompanyStatus } from "@/api/types";
import { AppShell } from "@/components/app-shell";
import { CompanyPicker } from "@/components/company-picker";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { resolveConfig } from "@/config";

type Phase =
  | { kind: "loading" }
  | { kind: "error"; message: string; hint?: string }
  | { kind: "picker"; companies: CompanyStatus[] }
  | {
      kind: "console";
      company: string | null;
      status: CompanyStatus;
      companies: CompanyStatus[];
      canGoBack: boolean;
    };

export function App() {
  const config = useMemo(() => resolveConfig(), []);
  const client = useMemo(() => new OpenCompanyClient(config), [config]);
  const [phase, setPhase] = useState<Phase>({ kind: "loading" });

  useEffect(() => {
    let cancelled = false;
    const set = (p: Phase) => !cancelled && setPhase(p);

    async function boot() {
      // Explicit company wins: go straight to its console.
      if (config.company) {
        try {
          const status = await client.status(config.company);
          set({ kind: "console", company: config.company, status, companies: [status], canGoBack: false });
        } catch (err) {
          set(connectionError(client, err));
        }
        return;
      }

      // Otherwise discover companies from the host.
      try {
        const companies = await client.listCompanies();
        if (companies.length === 1) {
          const c = companies[0];
          set({ kind: "console", company: c.id, status: c, companies, canGoBack: false });
        } else if (companies.length > 1) {
          set({ kind: "picker", companies });
        } else {
          set({
            kind: "error",
            message: "No companies are running on this host.",
            hint: "Start one with `opencompany serve --company <dir>`.",
          });
        }
      } catch (listErr) {
        // Fall back to the single-company alias (prosumer serve).
        try {
          const status = await client.status(null);
          set({ kind: "console", company: null, status, companies: [], canGoBack: false });
        } catch {
          set(connectionError(client, listErr));
        }
      }
    }

    void boot();
    return () => {
      cancelled = true;
    };
  }, [client, config.company]);

  const switchCompany = useCallback(
    async (id: string, companies: CompanyStatus[]) => {
      try {
        const status = await client.status(id);
        setPhase({ kind: "console", company: id, status, companies, canGoBack: true });
      } catch (err) {
        setPhase(connectionError(client, err));
      }
    },
    [client],
  );

  const backToPicker = useCallback(() => {
    void client.listCompanies().then((companies) => setPhase({ kind: "picker", companies }));
  }, [client]);

  switch (phase.kind) {
    case "loading":
      return (
        <FullScreen>
          <div className="flex items-center gap-2 text-sm text-muted-foreground">
            <Loader2 className="size-4 animate-spin" /> Connecting…
          </div>
        </FullScreen>
      );

    case "error":
      return (
        <FullScreen>
          <div className="w-full max-w-md space-y-4">
            <Alert variant="destructive">
              <AlertTitle>Can&apos;t connect</AlertTitle>
              <AlertDescription>
                {phase.message}
                {phase.hint && <span className="mt-1 block font-mono text-xs opacity-80">{phase.hint}</span>}
              </AlertDescription>
            </Alert>
            <Button className="w-full" onClick={() => location.reload()}>
              Retry
            </Button>
          </div>
        </FullScreen>
      );

    case "picker":
      return (
        <CompanyPicker
          companies={phase.companies}
          onPick={(id) => void switchCompany(id, phase.companies)}
        />
      );

    case "console":
      return (
        <AppShell
          client={client}
          company={phase.company}
          initialStatus={phase.status}
          companies={phase.companies}
          onSwitchCompany={(id) => void switchCompany(id, phase.companies)}
          onBackToPicker={phase.canGoBack ? backToPicker : undefined}
        />
      );
  }
}

function FullScreen({ children }: { children: React.ReactNode }) {
  return (
    <div className="grid min-h-svh place-items-center bg-background p-6 text-center">{children}</div>
  );
}

function connectionError(client: OpenCompanyClient, err: unknown): Phase {
  const where = client.baseUrl || "this origin";
  if (err instanceof ApiError && err.code === "unauthorized") {
    return {
      kind: "error",
      message: "This host needs an operator token.",
      hint: "Append ?token=<your-token> to the URL.",
    };
  }
  return {
    kind: "error",
    message: `Couldn't reach a company host at ${where}.`,
    hint: "Set the host with ?api=<url>, or run `opencompany serve`.",
  };
}
