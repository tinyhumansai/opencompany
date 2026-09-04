// The plan a card was given before it started (issue #337).
//
// A planning pass writes one `TaskPlan` onto the card and then settles the card
// itself: cleared plans hand straight on to In Progress, blocked ones come back
// to To-do. So by the time an operator reads this, the decision has already been
// made — which is exactly why the brief has to be legible. The single question
// this component exists to answer at a glance is **"why did (or didn't) this
// start?"**, and the answer is the prerequisite verdicts.
//
// Hence the ordering: the blockers come first, above the steps. A plan whose
// prerequisites are all satisfied is a plan nobody needs to read; a plan with a
// gap is one somebody has to act on, and burying that under a numbered list of
// steps would make the useful case the slow one.
//
// The plan itself is read-only. The console never posts a plan — the host's
// create body has no field for one — so a verdict cannot be forged from a
// browser, and nothing in this file needs to guard against that.
//
// One thing here does write, and it writes the *card*, not the plan: when a pass
// declined to choose an owner (issue #1106) the brief renders the candidates it
// named and hands a pick back through `onPick`, which the detail screen turns
// into the same `PATCH …/tasks/{id} {assignee}` its reassign row already sends.
// The rule the read-only note was protecting is intact — nothing here can author
// a prerequisite verdict — and the write goes through the one boundary that
// validates an assignee against the roster.

import { useState } from "react";
import {
  AlertTriangle,
  CircleHelp,
  ClipboardCheck,
  Clock,
  HelpCircle,
  Loader2,
  ShieldAlert,
  TriangleAlert,
  Users,
  XCircle,
  CheckCircle2,
  type LucideIcon,
} from "lucide-react";

import type { PrereqStatus, Prerequisite, TaskPlan } from "@/api/tasks";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { formatUsd } from "@/lib/cost";

/**
 * How each verdict reads to a person, and what it means for the card.
 *
 * The wording is deliberately about consequence rather than about state:
 * "Blocking" says why the operator is looking at this, where "Missing" would
 * leave them to work out whether it mattered. `unknown` says *we could not
 * check* rather than *it is not there*, because those are different facts and
 * the host is careful not to conflate them — an inventory that was unreachable
 * during the pass never blocks a card.
 */
const VERDICT: Record<
  PrereqStatus,
  { label: string; icon: LucideIcon; className: string }
> = {
  satisfied: {
    label: "Ready",
    icon: CheckCircle2,
    className: "border-transparent bg-status-done-soft text-status-done-text",
  },
  missing: {
    label: "Blocking",
    icon: XCircle,
    className: "border-transparent bg-destructive/10 text-destructive",
  },
  needsApproval: {
    label: "Needs approval",
    icon: ShieldAlert,
    className: "border-transparent bg-status-blocked-soft text-status-blocked-text",
  },
  unknown: {
    label: "Not checked",
    icon: CircleHelp,
    className: "border-transparent bg-muted text-muted-foreground",
  },
};

/**
 * How a plan's prerequisites actually came out, counted once.
 *
 * Three separate facts, deliberately not collapsed into two. The host is
 * careful to keep them apart — an unreachable inventory yields `unknown` rather
 * than `missing`, precisely so a provider outage cannot fabricate a blocker —
 * and any surface that folds `unknown` back into "fine" throws that care away
 * at the last step, telling an operator something the host never established.
 */
export function tallyPrerequisites(plan: TaskPlan): {
  blocking: number;
  approval: number;
  unchecked: number;
} {
  return {
    blocking: plan.prerequisites.filter((p) => p.status === "missing").length,
    approval: plan.prerequisites.filter((p) => p.status === "needsApproval").length,
    unchecked: plan.prerequisites.filter((p) => p.status === "unknown").length,
  };
}

/** The whole brief: what the plan is, what it needs, and how it will be judged. */
export function TaskPlanBrief({
  plan,
  onPick,
}: {
  plan: TaskPlan;
  /**
   * Receives a candidate's id when the operator answers the ownership question
   * (issue #1106). Omitted on any surface that cannot write the card — the
   * candidates then render as a read-only record of what the pass declined to
   * decide, which is still the runner-up that used to be lost.
   */
  onPick?: (id: string) => void | Promise<void>;
}) {
  return (
    <div className="flex flex-col gap-5" data-testid="task-plan-brief">
      <Headline plan={plan} />

      <OwnerChoice plan={plan} onPick={onPick} />

      {plan.description && (
        <p className="text-sm leading-relaxed text-foreground/90">{plan.description}</p>
      )}

      {plan.prerequisites.length > 0 && (
        <Section title="Needs first">
          <ul className="flex flex-col gap-2">
            {/* Blockers first — they are the reason anyone opened this. */}
            {[...plan.prerequisites]
              .sort((a, b) => rank(a.status) - rank(b.status))
              .map((prerequisite, i) => (
                <PrerequisiteRow key={`${prerequisite.kind}-${prerequisite.name}-${i}`} prerequisite={prerequisite} />
              ))}
          </ul>
        </Section>
      )}

      {plan.steps.length > 0 && (
        <Section title="Steps">
          <ol className="flex flex-col gap-3">
            {plan.steps.map((step, i) => (
              <li key={i} className="flex gap-3">
                <span className="mt-0.5 flex size-5 shrink-0 items-center justify-center rounded-full bg-muted text-2xs font-medium tabular-nums text-muted-foreground">
                  {i + 1}
                </span>
                <div className="min-w-0 flex-1">
                  <p className="text-sm font-medium leading-snug">{step.title}</p>
                  {step.detail && (
                    <p className="mt-0.5 text-sm leading-relaxed text-muted-foreground">
                      {step.detail}
                    </p>
                  )}
                  <Estimates step={step} />
                </div>
              </li>
            ))}
          </ol>
        </Section>
      )}

      {plan.scope && (
        <Section title="Scope">
          <p className="text-sm leading-relaxed text-muted-foreground">{plan.scope}</p>
        </Section>
      )}

      {plan.verification && (
        <Section title="Done when">
          <p className="flex items-start gap-2 text-sm leading-relaxed text-muted-foreground">
            <ClipboardCheck className="mt-0.5 size-4 shrink-0" />
            <span>{plan.verification}</span>
          </p>
        </Section>
      )}

      {plan.risks.length > 0 && (
        <Section title="Risks">
          <ul className="flex flex-col gap-1.5">
            {plan.risks.map((risk, i) => (
              <li
                key={i}
                className="flex items-start gap-2 text-sm leading-relaxed text-muted-foreground"
              >
                <TriangleAlert className="mt-0.5 size-4 shrink-0 text-status-blocked-text" />
                <span>{risk}</span>
              </li>
            ))}
          </ul>
        </Section>
      )}

      <Footnote plan={plan} />
    </div>
  );
}

/**
 * The verdict in one line, before any detail.
 *
 * **Three states, not two**, and the middle one is the point. Saying
 * "everything it needs is in place" whenever nothing is `missing` would claim a
 * verification that did not happen: an `unknown` prerequisite is one the host
 * could not check, and a `needsApproval` one *will* stop and ask a person. Both
 * are true things the operator is about to run into, and a green headline is
 * how they find out the hard way instead.
 *
 * So the all-clear is reserved for a plan where every prerequisite was actually
 * checked and actually passed. Anything less says what is unresolved and how
 * many, in words that name which kind — "couldn't be checked" and "needs
 * approval" call for different responses, and lumping them would leave the
 * operator unable to tell whether to go fix something or just expect a prompt.
 *
 * Counts rather than names: the list is right below, and a headline that tried
 * to name them would truncate exactly when there were most of them.
 */
function Headline({ plan }: { plan: TaskPlan }) {
  const { blocking, approval, unchecked } = tallyPrerequisites(plan);

  // What is unresolved but not blocking, phrased so each kind keeps its own
  // meaning.
  const caveats: string[] = [];
  if (approval > 0) {
    caveats.push(`${approval} will stop for your approval`);
  }
  if (unchecked > 0) {
    caveats.push(`${unchecked} couldn't be checked`);
  }

  const tone =
    blocking > 0 ? "blocked" : caveats.length > 0 ? "caveat" : "clear";
  const Icon =
    tone === "blocked" ? AlertTriangle : tone === "caveat" ? CircleHelp : CheckCircle2;

  const headline =
    tone === "blocked"
      ? `Planned, but it can't start yet — ${blocking} thing${blocking === 1 ? "" : "s"} missing`
      : tone === "caveat"
        ? `Planned — nothing is blocking it, but ${caveats.join(" and ")}`
        : "Planned, and everything it needs was checked and is in place";

  return (
    <div
      className={cn(
        "flex items-start gap-2 rounded-lg border px-3 py-2",
        tone === "blocked" && "border-destructive/30 bg-destructive/5",
        tone === "caveat" && "border-status-blocked/30 bg-status-blocked-soft",
        tone === "clear" && "border-status-done/30 bg-status-done-soft",
      )}
    >
      <Icon
        className={cn(
          "mt-0.5 size-4 shrink-0",
          tone === "blocked" && "text-destructive",
          tone === "caveat" && "text-status-blocked-text",
          tone === "clear" && "text-status-done-text",
        )}
      />
      <div className="min-w-0 text-sm leading-relaxed">
        <span className="font-medium">{headline}</span>
        {plan.proposedAssignee && (
          <span className="text-muted-foreground">
            {" · suggested for "}
            <span className="font-medium text-foreground/80">{plan.proposedAssignee}</span>
          </span>
        )}
      </div>
    </div>
  );
}

/**
 * The ownership question, when the pass declined to answer it (issue #1106).
 *
 * Rendered above the description and below only the headline, because a card
 * sitting in To-do with an unanswered question is not waiting on anything in the
 * plan — it is waiting on the person reading it, and burying that under the
 * brief would make the actionable case the one you have to scroll for. It is the
 * same reasoning that puts blockers above steps.
 *
 * Each row carries its reason. A bare pair of ids would hand the operator back
 * the judgement the planner already made and ask them to re-derive it; the line
 * beside each name is what lets them answer without opening two agent pages.
 *
 * Nothing is pre-selected and there is no default button. A highlighted "best"
 * option would be the silent pick this issue exists to remove, wearing a
 * suggestion's clothes.
 */
function OwnerChoice({
  plan,
  onPick,
}: {
  plan: TaskPlan;
  onPick?: (id: string) => void | Promise<void>;
}) {
  const [picking, setPicking] = useState<string | null>(null);
  const candidates = plan.assigneeCandidates ?? [];
  if (candidates.length === 0) return null;

  return (
    <div
      className="flex flex-col gap-3 rounded-lg border border-status-blocked/30 bg-status-blocked-soft px-3 py-2.5"
      data-testid="plan-owner-choice"
    >
      <div className="flex items-start gap-2">
        <Users className="mt-0.5 size-4 shrink-0 text-status-blocked-text" />
        <p className="min-w-0 text-sm leading-relaxed">
          <span className="font-medium">Who owns this?</span>{" "}
          <span className="text-muted-foreground">
            More than one teammate could take it, so it is waiting on you rather
            than being handed to whichever one the plan named first.
          </span>
        </p>
      </div>

      <ul className="flex flex-col gap-1.5">
        {candidates.map((candidate) => (
          <li
            key={candidate.id}
            className="flex items-start gap-2 rounded-md bg-background/60 px-2.5 py-2"
          >
            <div className="min-w-0 flex-1">
              <p className="truncate text-sm font-medium">{candidate.id}</p>
              {candidate.reason && (
                <p className="mt-0.5 text-sm leading-relaxed text-muted-foreground">
                  {candidate.reason}
                </p>
              )}
            </div>
            {onPick && (
              <Button
                size="sm"
                variant="outline"
                className="h-7 shrink-0"
                disabled={picking !== null}
                onClick={() => {
                  setPicking(candidate.id);
                  // The screen reloads the card on success and surfaces its own
                  // failure toast; either way this component is about to be
                  // re-rendered from fresh data, so the only state it owns is
                  // "a click is in flight".
                  void Promise.resolve(onPick(candidate.id)).finally(() =>
                    setPicking(null),
                  );
                }}
              >
                {picking === candidate.id && (
                  <Loader2 className="mr-1.5 size-3.5 animate-spin" />
                )}
                Assign
              </Button>
            )}
          </li>
        ))}
      </ul>

      {onPick && (
        <p className="text-2xs text-muted-foreground">
          Someone else can take it too — the reassign row on this card offers the
          whole roster.
        </p>
      )}
    </div>
  );
}

function PrerequisiteRow({ prerequisite }: { prerequisite: Prerequisite }) {
  const verdict = VERDICT[prerequisite.status] ?? VERDICT.unknown;
  const Icon = verdict.icon;
  return (
    <li className="flex items-start gap-2.5">
      <span
        className={cn(
          "mt-0.5 flex shrink-0 items-center gap-1 rounded-full border px-2 py-0.5 text-2xs font-medium",
          verdict.className,
        )}
      >
        <Icon className="size-3" />
        {verdict.label}
      </span>
      <div className="min-w-0 flex-1 text-sm leading-relaxed">
        <span className="font-medium">{prerequisite.name}</span>
        <span className="ml-1.5 text-2xs uppercase tracking-wide text-muted-foreground">
          {prerequisite.kind}
        </span>
        <p className="text-muted-foreground">{prerequisite.note}</p>
      </div>
    </li>
  );
}

/**
 * The model's guesses, rendered as guesses.
 *
 * Nothing anywhere budgets from these — the real caps are the teammate's daily
 * limit and the capability tier, both enforced host-side from live meter reads
 * — so they are shown quietly and hedged in words rather than presented beside
 * the actual spend, where they would read as a commitment.
 */
function Estimates({ step }: { step: TaskPlan["steps"][number] }) {
  const parts: string[] = [];
  if (typeof step.estimatedMinutes === "number") {
    parts.push(
      step.estimatedMinutes >= 60
        ? `~${(step.estimatedMinutes / 60).toFixed(1)}h`
        : `~${step.estimatedMinutes}m`,
    );
  }
  if (typeof step.estimatedCostUsd === "number") {
    parts.push(`~${formatUsd(step.estimatedCostUsd)}`);
  }
  if (parts.length === 0) return null;
  return (
    <p className="mt-1 flex items-center gap-1 text-2xs text-muted-foreground">
      <Clock className="size-3 shrink-0" />
      <span className="tabular-nums">{parts.join(" · ")}</span>
      <span className="opacity-70">estimated</span>
    </p>
  );
}

/**
 * The standing caveat, once, at the bottom.
 *
 * Two things an operator needs to know and would otherwise have to be told by a
 * colleague: the estimates are not budgets, and a "Not checked" verdict is an
 * admission rather than an all-clear.
 */
function Footnote({ plan }: { plan: TaskPlan }) {
  const unchecked = plan.prerequisites.some((p) => p.status === "unknown");
  return (
    <p className="flex items-start gap-1.5 border-t pt-3 text-2xs leading-relaxed text-muted-foreground">
      <HelpCircle className="mt-0.5 size-3 shrink-0" />
      <span>
        Written by the planner from what this company had at the time. Estimates are guesses and
        nothing is budgeted from them.
        {unchecked &&
          " Anything marked “Not checked” could not be verified during the pass — it was not treated as missing, so it did not block this card."}
      </span>
    </p>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="flex flex-col gap-2">
      <h4 className="text-2xs font-semibold uppercase tracking-wide text-muted-foreground">
        {title}
      </h4>
      {children}
    </section>
  );
}

/** Blocking first, then warnings, then the things that are fine. */
function rank(status: PrereqStatus): number {
  switch (status) {
    case "missing":
      return 0;
    case "needsApproval":
      return 1;
    case "unknown":
      return 2;
    default:
      return 3;
  }
}
