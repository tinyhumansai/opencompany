import { ArrowRight, Building2 } from "lucide-react";

import type { CompanyStatus } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Card } from "@/components/ui/card";
import { StatusPill } from "@/components/status-pill";
import { ThemeToggle } from "@/components/theme-toggle";

interface Props {
  companies: CompanyStatus[];
  onPick: (id: string) => void;
}

/** Multi-company hosts: choose which company to operate. */
export function CompanyPicker({ companies, onPick }: Props) {
  return (
    <div className="min-h-svh bg-background">
      <header className="flex items-center justify-between border-b px-6 py-4">
        <div className="flex items-center gap-2">
          <div className="flex size-7 items-center justify-center rounded-md bg-primary text-primary-foreground">
            <Building2 className="size-4" />
          </div>
          <span className="text-sm font-semibold">OpenCompany</span>
        </div>
        <ThemeToggle />
      </header>

      <main className="mx-auto w-full max-w-4xl px-6 py-10">
        <div className="mb-6 space-y-1">
          <h1 className="text-2xl font-semibold tracking-tight">Your companies</h1>
          <p className="text-sm text-muted-foreground">Choose a company to operate.</p>
        </div>

        <div className="grid gap-4 sm:grid-cols-2">
          {companies.map((c) => (
            <Card
              key={c.id}
              role="button"
              tabIndex={0}
              onClick={() => onPick(c.id)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  onPick(c.id);
                }
              }}
              className="group cursor-pointer p-5 transition-colors hover:border-primary/40 hover:bg-accent/40"
            >
              <div className="flex items-start gap-3">
                <div className="flex size-10 shrink-0 items-center justify-center rounded-lg bg-muted">
                  <Building2 className="size-5" />
                </div>
                <div className="min-w-0 flex-1">
                  <p className="truncate font-medium">{c.name}</p>
                  <div className="mt-2 flex flex-wrap items-center gap-2">
                    <StatusPill lifecycle={c.lifecycle} />
                    {c.pending_approvals > 0 && (
                      <Badge variant="secondary">{c.pending_approvals} to approve</Badge>
                    )}
                  </div>
                </div>
                <ArrowRight className="size-4 shrink-0 text-muted-foreground transition-transform group-hover:translate-x-0.5" />
              </div>
            </Card>
          ))}
        </div>
      </main>
    </div>
  );
}
