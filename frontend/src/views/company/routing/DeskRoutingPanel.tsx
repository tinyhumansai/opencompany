/**
 * A desk's routing block: how wide its rounds are, how many it may run, and
 * how long a seat may think.
 *
 * # Declared and effective, side by side
 *
 * `GET …/desks/{id}/routing` answers with both the block as authored and the
 * numbers the runtime will actually use, and the panel shows both because
 * they answer different questions. The effective column is what will happen
 * on the next message; the declared form is what the operator can change. A
 * single editable number could not say whether `round_width: 2` was chosen
 * or defaulted, and the two behave differently the moment the manifest moves.
 *
 * # Candidates and shared seats
 *
 * The router picks among `candidates`, and each one lists the other desks it
 * also sits on. That is worth seeing before widening a round: a shared seat
 * runs at most one turn at a time across every desk, so a round that waits on
 * it can be delayed by another desk's round. The console has no other surface
 * that says a teammate is shared.
 *
 * Reached from the org chart at `#/company/<deskId>` (the room's members pane
 * links there), and rendered from a stub on `#/styleguide`.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { Loader2, RotateCcw, Save } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { DeskRoutingDeclared, DeskRoutingDto } from "@/api/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  deskId: string;
  /** Bumped by the shell on `desk_routing_configured`, so another session's
   *  save or reset lands here without a reload. */
  refreshKey?: number;
  /** Whether the reader may write. Read-only still shows every number. */
  canManage?: boolean;
}

/** The numeric fields of the declared block, in the order the form shows them. */
const NUMBER_FIELDS: {
  key: keyof DeskRoutingDeclared;
  label: string;
  hint: string;
  step?: number;
}[] = [
  { key: "round_width", label: "Round width", hint: "seats that run together in one round" },
  { key: "max_rounds", label: "Max rounds", hint: "rounds before the host closes the episode" },
  { key: "turn_timeout_secs", label: "Turn timeout (s)", hint: "how long one seat may think, from when it gets its turn" },
  { key: "choice_option_limit", label: "Choice options", hint: "candidates offered to the router per decision" },
  { key: "minimum_confidence", label: "Min confidence", hint: "below this the router falls back to the lead", step: 0.05 },
  { key: "high_impact_minimum_confidence", label: "High-impact min confidence", hint: "the stricter bar for a high-impact message", step: 0.05 },
  { key: "clarification_threshold", label: "Clarify threshold", hint: "below this the router asks instead of routing", step: 0.05 },
  { key: "high_impact_threshold", label: "High-impact threshold", hint: "what counts as high impact", step: 0.05 },
];

const SOURCE_WORD: Record<DeskRoutingDto["source"], string> = {
  overlay: "installed here",
  manifest: "from company.toml",
  default: "library defaults",
};

export function DeskRoutingPanel({ client, company, deskId, refreshKey = 0, canManage = true }: Props) {
  const [dto, setDto] = useState<DeskRoutingDto | null>(null);
  const [form, setForm] = useState<DeskRoutingDeclared>({});
  const [load, setLoad] = useState<"loading" | "ready" | "unavailable" | "error">("loading");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<"save" | "reset" | null>(null);

  /** Monotonic ticket so only the newest read commits (a desk switch mid-read). */
  const generation = useRef(0);
  const read = useCallback(async () => {
    const ticket = ++generation.current;
    // An older host, or the lightweight room-test client, has no routing read.
    if (typeof client.getDeskRouting !== "function") {
      setLoad("unavailable");
      return;
    }
    try {
      const next = await client.getDeskRouting(deskId, company);
      if (ticket !== generation.current) return;
      setDto(next);
      setForm(next.declared);
      setLoad("ready");
      setError(null);
    } catch (e: unknown) {
      if (ticket !== generation.current) return;
      setLoad("error");
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [client, company, deskId]);

  useEffect(() => {
    setLoad("loading");
    void read();
  }, [read, refreshKey]);

  const submit = async (action: "save" | "reset") => {
    setBusy(action);
    setError(null);
    try {
      const next =
        action === "save"
          ? await client.putDeskRouting(deskId, form, company)
          : await client.resetDeskRouting(deskId, company);
      setDto(next);
      setForm(next.declared);
    } catch (e: unknown) {
      // The host's own sentence, verbatim: it is the authority on why a block
      // is invalid, and a rephrased refusal is one the operator cannot act on.
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };

  const setNumber = (key: keyof DeskRoutingDeclared, raw: string) => {
    setForm((prev) => {
      const next = { ...prev };
      if (raw.trim() === "") delete next[key];
      else {
        const value = Number(raw);
        if (Number.isFinite(value)) next[key] = value;
      }
      return next;
    });
  };

  if (load === "unavailable") {
    return (
      <p className="text-sm text-muted-foreground" data-testid="desk-routing-panel" data-state="unavailable">
        This host does not expose desk routing yet.
      </p>
    );
  }
  if (load === "error") {
    return (
      <p className="text-sm text-muted-foreground" data-testid="desk-routing-panel" data-state="error">
        {error}
      </p>
    );
  }
  if (load === "loading" || !dto) {
    return (
      <div className="flex items-center gap-2 text-sm text-muted-foreground" data-testid="desk-routing-panel" data-state="loading">
        <Loader2 className="size-4 animate-spin" aria-hidden />
        Reading routing…
      </div>
    );
  }

  const { effective } = dto;
  return (
    <section
      className="flex flex-col gap-4 rounded-lg border bg-card p-4"
      data-testid="desk-routing-panel"
      data-state="ready"
      data-source={dto.source}
      aria-label={`Routing for ${deskId}`}
    >
      <header className="flex flex-wrap items-center gap-2">
        <h3 className="text-sm font-semibold">Routing</h3>
        <Badge variant="outline" className="text-2xs font-normal" data-testid="desk-routing-source">
          {SOURCE_WORD[dto.source] ?? dto.source}
        </Badge>
        <Badge variant="outline" className="text-2xs font-normal" data-testid="desk-routing-router">
          router: {effective.router}
        </Badge>
      </header>

      <dl className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs sm:grid-cols-4" data-testid="desk-routing-effective">
        <div>
          <dt className="text-muted-foreground">round width</dt>
          <dd className="tabular-nums">{effective.roundWidth}</dd>
        </div>
        <div>
          <dt className="text-muted-foreground">max rounds</dt>
          <dd className="tabular-nums">{effective.maxRounds}</dd>
        </div>
        <div>
          <dt className="text-muted-foreground">turn timeout</dt>
          <dd className="tabular-nums">{effective.turnTimeoutSecs}s</dd>
        </div>
      </dl>

      <div>
        <p className="mb-1 text-xs font-medium text-muted-foreground">Candidates</p>
        <ul className="flex flex-wrap gap-1.5" data-testid="desk-routing-candidates">
          {dto.candidates.map((candidate) => (
            <li
              key={candidate.agentId}
              className="inline-flex items-center gap-1.5 rounded-md border px-2 py-0.5 text-xs"
              data-agent-id={candidate.agentId}
              data-shared={candidate.sharedWith.length > 0 ? "true" : "false"}
              title={candidate.role}
            >
              <span className="font-medium">{candidate.label}</span>
              {candidate.sharedWith.length > 0 && (
                <span className="text-2xs text-muted-foreground">
                  also on {candidate.sharedWith.map((desk) => `#${desk}`).join(", ")}
                </span>
              )}
            </li>
          ))}
        </ul>
        {dto.candidates.some((c) => c.sharedWith.length > 0) && (
          <p className="mt-1 text-2xs text-muted-foreground">
            A shared seat runs one turn at a time across every desk it sits on, so a round that
            waits on it can be delayed by another desk&apos;s round.
          </p>
        )}
      </div>

      <form
        className="grid gap-3 sm:grid-cols-2"
        onSubmit={(event) => {
          event.preventDefault();
          void submit("save");
        }}
      >
        {NUMBER_FIELDS.map((field) => (
          <div key={field.key} className="flex flex-col gap-1">
            <Label htmlFor={`routing-${deskId}-${field.key}`} className="text-xs">
              {field.label}
            </Label>
            <Input
              id={`routing-${deskId}-${field.key}`}
              type="number"
              step={field.step ?? 1}
              min={0}
              inputMode="decimal"
              value={form[field.key] ?? ""}
              placeholder="default"
              disabled={!canManage || busy !== null}
              onChange={(event) => setNumber(field.key, event.target.value)}
              data-testid={`desk-routing-${field.key}`}
            />
            <span className="text-2xs text-muted-foreground">{field.hint}</span>
          </div>
        ))}
        {error && (
          <p className="text-xs text-destructive sm:col-span-2" role="alert" data-testid="desk-routing-error">
            {error}
          </p>
        )}
        {canManage && (
          <div className="flex flex-wrap items-center gap-2 sm:col-span-2">
            <Button type="submit" size="sm" disabled={busy !== null} data-testid="desk-routing-save">
              {busy === "save" ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Save className="size-3.5" aria-hidden />}
              Install
            </Button>
            <Button
              type="button"
              size="sm"
              variant="ghost"
              disabled={busy !== null || dto.source !== "overlay"}
              onClick={() => void submit("reset")}
              title={dto.source === "overlay" ? "Drop the installed block and use the manifest's" : "Nothing installed here to drop"}
              data-testid="desk-routing-reset"
            >
              {busy === "reset" ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <RotateCcw className="size-3.5" aria-hidden />}
              Reset to manifest
            </Button>
          </div>
        )}
      </form>
    </section>
  );
}
