import { useEffect, useMemo, useState } from "react";

import { OpenCompanyClient } from "./api/client";
import { ApiError, type CompanyStatus } from "./api/types";
import { CompanyPicker } from "./components/CompanyPicker";
import { resolveConfig } from "./config";
import { Console } from "./views/Console";

type Phase =
  | { kind: "loading" }
  | { kind: "error"; message: string; hint?: string }
  | { kind: "picker"; companies: CompanyStatus[] }
  | { kind: "console"; company: string; status: CompanyStatus; canGoBack: boolean };

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
          set({ kind: "console", company: config.company, status, canGoBack: false });
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
          set({ kind: "console", company: c.id, status: c, canGoBack: false });
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
          set({ kind: "console", company: status.id, status, canGoBack: false });
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

  switch (phase.kind) {
    case "loading":
      return (
        <div className="center">
          <div className="muted">Connecting…</div>
        </div>
      );
    case "error":
      return (
        <div className="center">
          <div className="card">
            <div className="banner error">{phase.message}</div>
            {phase.hint && <div className="muted mono">{phase.hint}</div>}
            <div className="row" style={{ marginTop: 16 }}>
              <button className="btn" onClick={() => location.reload()}>
                Retry
              </button>
            </div>
          </div>
        </div>
      );
    case "picker":
      return (
        <CompanyPicker
          companies={phase.companies}
          onPick={(id) => {
            const status = phase.companies.find((c) => c.id === id)!;
            setPhase({ kind: "console", company: id, status, canGoBack: true });
          }}
        />
      );
    case "console":
      return (
        <Console
          client={client}
          company={phase.company}
          initialStatus={phase.status}
          onBack={
            phase.canGoBack
              ? () => {
                  void client.listCompanies().then((companies) =>
                    setPhase({ kind: "picker", companies }),
                  );
                }
              : undefined
          }
        />
      );
  }
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
