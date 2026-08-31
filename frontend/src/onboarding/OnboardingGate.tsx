import { lazy, Suspense, useState } from "react";
import { Check, Loader2, PartyPopper, Plug, UserCog, Workflow } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { type ActivationStatus, confirmCompanyName } from "@/api/activation";
import { ApiError } from "@/api/types";
import { RouteLoading } from "@/components/route-loading";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";
import { OAuthView } from "@/views/OAuthView";

// Same reason `app-shell.tsx` lazy-loads it: React Flow is heavy, and it
// should not tax a screen an operator only sees once.
// PR #1875 review finding: the name entered here is embedded verbatim into
// every agent's system prompt (`persona_prompt`, `src/company/prompt.rs`),
// so an unbounded paste can inflate every model request past its context
// limit. Mirrors the host's own limit (`COMPANY_NAME_MAX_CHARS`,
// `src/server/ops/company_profile.rs`) — the API's rejection is the real
// enforcement point regardless of client, this only avoids a round trip for
// the common case of a pasted document.
const COMPANY_NAME_MAX_CHARS = 200;

/**
 * Clamps `value` to `COMPANY_NAME_MAX_CHARS`, counting Unicode scalar values —
 * the same definition of "character" the host enforces with `chars().count()`
 * (`src/server/ops/company_profile.rs`) — rather than UTF-16 code units.
 *
 * PR #1875 review finding: the native `maxLength` attribute this field used
 * to carry counts UTF-16 code units, not scalar values. A name built from
 * 101-200 astral characters (most emoji, some scripts) passes the host's
 * `chars().count() <= 200` check, but each such character consumes two
 * UTF-16 units — so `maxLength={200}` silently refused input past 100 of
 * them, well inside what the API accepts. `Array.from` iterates by code
 * point, which is exactly `chars().count()`'s definition (same technique as
 * `titleFromMessage`, `lib/chat.ts`, and `documentSlug`, `api/memory.ts`).
 */
export function clampToCompanyNameLimit(value: string): string {
  const points = Array.from(value);
  return points.length <= COMPANY_NAME_MAX_CHARS ? value : points.slice(0, COMPANY_NAME_MAX_CHARS).join("");
}

const WorkflowsView = lazy(() =>
  import("@/views/WorkflowsView").then((m) => ({ default: m.WorkflowsView })),
);

interface GateStep {
  id: "name" | "integration" | "workflow";
  label: string;
  hint: string;
  icon: typeof UserCog;
  done: boolean;
}

/**
 * The blocking first-run gate (issue #1844): a full-screen replacement for the
 * console shell until the company has a confirmed name, at least one working
 * integration, and one successful workflow run.
 *
 * A **checklist, not a wizard.** The three steps are not sequential — an
 * operator may connect a provider before naming the company, or vice versa —
 * so all three are always visible and any incomplete one can be opened.
 * `Stepper` (`components/ui/stepper.tsx`) assumes forward-only progress
 * through ordered pages and does not fit this shape; this renders its own
 * checklist instead.
 *
 * Reuses the console's own flows for steps 2 and 3 rather than re-implementing
 * a connect or a run: [`OAuthView`] and [`WorkflowsView`] are embedded
 * whole, so this component owns none of that logic and cannot drift from the
 * page an operator would otherwise reach through the sidebar.
 */
export function OnboardingGate({
  client,
  company,
  status,
  currentName,
  onRefresh,
  onSkip,
}: {
  client: OpenCompanyClient;
  company: string | null;
  status: ActivationStatus;
  /** The company's current display name, to prefill the naming step. */
  currentName: string;
  /** Called after an in-gate action that may have moved the funnel. */
  onRefresh: () => void;
  /** "Skip for now" — de-emphasized, and always available (issue #1844). */
  onSkip: () => void;
}) {
  const steps: GateStep[] = [
    {
      id: "name",
      label: "Name your company",
      hint: "A real name beats the placeholder it launched with.",
      icon: UserCog,
      done: status.nameConfirmed,
    },
    {
      id: "integration",
      label: "Connect an integration",
      hint: "Gmail, Slack, GitHub — wherever your teammates should reach first.",
      icon: Plug,
      done: status.integrationConnected,
    },
    {
      id: "workflow",
      label: "Run a workflow",
      hint: "One real run — not a test — proves the company actually works.",
      icon: Workflow,
      done: status.workflowRunSucceeded,
    },
  ];

  const firstOpen = steps.find((s) => !s.done)?.id ?? null;
  const [active, setActive] = useState<GateStep["id"] | null>(firstOpen);

  return (
    <div className="flex min-h-screen flex-col bg-background">
      <header className="border-b px-6 py-5 sm:px-10">
        <div className="mx-auto max-w-4xl">
          <div className="flex items-center gap-2 text-sm font-medium text-muted-foreground">
            <PartyPopper className="size-4" />
            Let&apos;s get your company running
          </div>
          <p className="mt-1 text-sm text-muted-foreground">
            Three quick steps, in any order. Once all three are done, this screen never
            comes back.
          </p>
        </div>
      </header>

      <main className="mx-auto flex w-full max-w-4xl flex-1 flex-col gap-3 overflow-y-auto px-6 py-6 sm:px-10">
        {steps.map((step) => (
          <section key={step.id} className="rounded-xl border bg-card" data-testid={`gate-step-${step.id}`}>
            <button
              type="button"
              className={cn(
                "flex w-full items-center gap-3 rounded-xl px-4 py-3 text-left",
                !step.done && "hover:bg-muted/50",
              )}
              disabled={step.done}
              aria-expanded={active === step.id}
              onClick={() => setActive((cur) => (cur === step.id ? null : step.id))}
            >
              <span
                aria-hidden
                className={cn(
                  "flex size-7 shrink-0 items-center justify-center rounded-full border",
                  step.done
                    ? "border-primary bg-primary text-primary-foreground"
                    : "border-border text-muted-foreground",
                )}
              >
                {step.done ? <Check className="size-4" /> : <step.icon className="size-4" />}
              </span>
              <span className="min-w-0 flex-1">
                <span className="block text-sm font-medium">{step.label}</span>
                <span className="block text-xs text-muted-foreground">{step.hint}</span>
              </span>
              {step.done && (
                <span className="shrink-0 text-xs font-medium text-status-done-text">Done</span>
              )}
            </button>

            {!step.done && active === step.id && (
              <div className="border-t px-4 py-4">
                {step.id === "name" && (
                  <NameStep
                    client={client}
                    company={company}
                    currentName={currentName}
                    onDone={onRefresh}
                  />
                )}
                {step.id === "integration" && <OAuthView client={client} company={company} />}
                {step.id === "workflow" && (
                  <Suspense fallback={<RouteLoading title="Workflows" label="Loading canvas…" />}>
                    <WorkflowsView client={client} company={company} />
                  </Suspense>
                )}
              </div>
            )}
          </section>
        ))}
      </main>

      <footer className="border-t px-6 py-4 sm:px-10">
        <div className="mx-auto flex max-w-4xl items-center justify-end">
          <Button variant="ghost" size="sm" onClick={onSkip} data-testid="gate-skip">
            Skip for now
          </Button>
        </div>
      </footer>
    </div>
  );
}

function NameStep({
  client,
  company,
  currentName,
  onDone,
}: {
  client: OpenCompanyClient;
  company: string | null;
  currentName: string;
  onDone: () => void;
}) {
  const [name, setName] = useState(currentName);
  const [busy, setBusy] = useState(false);

  const trimmed = name.trim();

  async function confirm() {
    if (!trimmed) return;
    setBusy(true);
    try {
      await confirmCompanyName(client, company, trimmed);
      toast.success("Company name set.");
      onDone();
    } catch (err) {
      toast.error(err instanceof ApiError ? err.message : "Could not set the name.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="max-w-sm space-y-3">
      <div className="space-y-1">
        <Label htmlFor="gate-company-name" className="text-xs">
          Company name
        </Label>
        <Input
          id="gate-company-name"
          value={name}
          onChange={(e) => setName(clampToCompanyNameLimit(e.target.value))}
          onKeyDown={(e) => {
            if (e.key === "Enter") void confirm();
          }}
          placeholder="Acme Inc."
          autoFocus
        />
      </div>
      <Button disabled={busy || !trimmed} onClick={() => void confirm()}>
        {busy ? <Loader2 className="size-4 animate-spin" /> : <Check className="size-4" />}
        Confirm name
      </Button>
    </div>
  );
}
