import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ArrowLeft, Check, Loader2, RotateCcw, Sparkles, Users } from "lucide-react";

import { me } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { proposeRoster, type ProposedAgent, type RosterFallback } from "@/api/company-setup";
import { getInferenceStatus } from "@/api/inference";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button, buttonVariants } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import {
  SETUP_STEPS,
  buildOutLabel,
  draftIsSubmittable,
  emptySetupDraft,
  staffedTeam,
  stepProblem,
  type SetupDraft,
} from "@/lib/company-setup";
import { cn } from "@/lib/utils";
import { initials, toneFor } from "@/lib/team";
import { TEAM_TONES } from "@/lib/team";

/**
 * How long each created agent stays on screen before the next write starts.
 *
 * The one place in this product where slower is better. The work can finish in
 * well under a second on a warm local host, and a build-out that flashes past
 * reads as a form submitting — which is exactly the feeling this screen exists
 * to replace. Paced, it reads as a company being assembled.
 *
 * Small enough that six agents still land inside four seconds, so nobody is
 * waiting on theatre.
 */
const REVEAL_MS = 450;

type Phase =
  | { kind: "asking"; step: number }
  | { kind: "thinking" }
  | { kind: "building"; agents: ProposedAgent[]; created: number; fallback: Fallback }
  | { kind: "done"; agents: ProposedAgent[]; fallback: Fallback }
  | { kind: "failed"; reason: string };

/**
 * Why the curated team shipped, or `null` when the model designed this one.
 *
 * `"unspecified"` covers a host that reported a fallback without saying why.
 * It is not the same as `"no_model"` and must not be folded into it: the
 * credential CTA is only ever correct when the host actually said no model was
 * reachable, and guessing that on silence is how an operator gets sent to fix
 * a key that already worked.
 */
type Fallback = RosterFallback | "unspecified" | null;

/** What the host said about the model before we ask the questions it shapes. */
type InferenceReadiness =
  | "checking"
  | "ready"
  | "unavailable"
  | "restart"
  | "unknown";

/**
 * How long to wait for the readiness answer before carrying on without it.
 *
 * The browser transport's `fetch` has no timeout of its own, and this dialog
 * withholds the questions and refuses every dismissal while the answer is
 * outstanding — so a host that *stalls* rather than rejecting would lock the
 * operator out of the console for as long as it stalls. An unanswered check
 * degrades to `unknown`, which is the same honest "we could not establish this"
 * the rejection path already reports.
 */
const READINESS_TIMEOUT_MS = 6_000;

/** The cognition path that has a roster builder on it — see the effect below. */
const DESIGNING_COGNITION = "harness";

/**
 * First-run company setup: three questions, then a team built on the host
 * (docs/spec/runtime/company-setup.md).
 *
 * Owns the whole flow because the flow is one decision from the operator's point
 * of view — they answer, they watch, they land in a staffed company. Splitting
 * the questions from the build-out would put a route boundary in the middle of
 * the moment the feature exists to create.
 *
 * ## The build-out creates one agent at a time, deliberately
 *
 * Each `addTeamMember` is awaited in turn and revealed as it lands. The host has
 * no batch create and does not need one: sequential writes are what let this
 * screen narrate itself with no event plumbing, and they mean a browser closed
 * halfway leaves a company with three real teammates rather than a broken one.
 */
export function SetupDialog({
  open,
  client,
  company,
  redesign,
  fallbackIds,
  onSkip,
  onLeave,
  onDone,
  onRedesign,
  onRetry,
  onReplacementComplete,
}: {
  open: boolean;
  client: OpenCompanyClient;
  company: string | null;
  /**
   * Reopen in **redesign** mode: the company already has a team (the fallback
   * one from a pass that could not reach a model), and the next build-out must
   * replace it rather than stack a second one on it.
   */
  redesign?: boolean;
  /**
   * The host ids of the fallback team the first pass created, captured when the
   * operator left to wire a model.
   *
   * The only rows a redesign may replace. Teammates other operators added while
   * model settings were open are not in this list, and survive the replacement.
   * Empty when the first pass created nothing (no fallback team to replace).
   */
  fallbackIds?: string[];
  /** "I'll do this later" — records the skip and closes. */
  onSkip: () => void;
  /**
   * Close without recording any decision.
   *
   * Distinct from [`onSkip`] because the two mean opposite things. Skipping is
   * "I'll do this later" and is *meant* to suppress the offer on the next load.
   * Leaving to wire a model is the operator starting this flow, not declining
   * it — recording a skip there would suppress the dialog exactly when they
   * came back ready to use it, leaving an unstaffed company and no way back in
   * short of finding the Company page's separate prompt.
   */
  onLeave: () => void;
  /** Setup finished; the caller refreshes the roster and hands off to the tour. */
  onDone: () => void;
  /**
   * The completion screen's "Add a model in Settings" action: close and send
   * the operator to wire a model, recording that the fallback team — the ids of
   * the rows this run just created — is to be redesigned on their return.
   */
  onRedesign: (fallbackIds: string[]) => void;
  /**
   * The completion screen's "Try again" for a fallback a retry could fix:
   * record that the team the failed pass just created is owed a redesign, so a
   * reload before the in-place replacement completes can resume it. Unlike
   * [`onRedesign`] this does not close the dialog — the retry continues here.
   */
  onRetry?: (fallbackIds: string[]) => void;
  /**
   * Settle the persisted redesign debt when a replacement build-out changes
   * what a future replacement must sweep.
   *
   * The debt names the team the operator is owed a replacement for. Called
   * with `null` when a designed replacement just paid it, with the new
   * fallback's ids when a replacement fell back again, and with the expanded
   * boundary when a partial replacement was rolled back but a rollback removal
   * failed — in every case before the next screen shows, so a reload cannot
   * reopen redesign against a boundary that no longer exists or leaves a row
   * stranded beside the eventual team.
   */
  onReplacementComplete?: (fallbackIds: string[] | null) => void;
}) {
  const [draft, setDraft] = useState<SetupDraft>(emptySetupDraft);
  const [phase, setPhase] = useState<Phase>({ kind: "asking", step: 0 });
  const [inference, setInference] = useState<InferenceReadiness>("checking");
  /**
   * Whether this host can ever run the design path, whatever model is wired.
   *
   * `false` (an `openhuman`-less binary, or one with no pool attached) makes
   * the "Set up a model" call-to-action a dead end — the CTA is omitted rather
   * than send the operator round a redesign loop that cannot end. Defaults to
   * `true` while the status is unread, so a host we could not ask is offered
   * the CTA as before rather than having it silently taken away.
   */
  const [harnessReachable, setHarnessReachable] = useState(true);
  /**
   * Whether the operator may change where the company's model calls go.
   *
   * The actions the notice offers — wiring a model, restarting to pick one up —
   * are an admin's (the Connections form and its restart control render only
   * under management authority and the host refuses the writes for a member),
   * so a member is told to ask an admin rather than handed a dead-end link.
   * `null` while the role read is outstanding: the admin actions are withheld
   * until the host says who the operator is, so a member whose role read
   * settles after the readiness check is never offered a link that can only
   * 403 — and a read that never answers keeps the actions withheld rather
   * than defaulting to a guess that could misdirect anyone.
   */
  const [canManage, setCanManage] = useState<boolean | null>(null);
  const [touched, setTouched] = useState(false);
  /**
   * Whether this run of the flow replaces the team the company already has.
   *
   * True when the controller reopens setup in redesign mode after the operator
   * returns from wiring a model, and when the operator retries a build-out that
   * could not reach a wired model. A replacing build-out clears the existing
   * operator-authored teammates before creating the new ones — without this,
   * a second pass stacks a duplicate team on the first.
   */
  const [replacing, setReplacing] = useState(Boolean(redesign));
  /**
   * The operator roster observed before this redesign began. Only these rows
   * belong to the fallback pass; rows added by another operator while model
   * settings were open must survive the replacement.
   */
  const redesignRoster = useRef<Set<string> | null>(null);
  /**
   * The host ids of the rows this run's build-out has created so far.
   *
   * Handed to the completion screen's "Add a model in Settings" action so the
   * redesign debt can name the exact team to replace. Reset at the start of
   * each build-out, so a redesign's own creations become the next redesign's
   * boundary rather than accumulating the pass they replaced.
   */
  const createdIds = useRef<string[]>([]);
  /**
   * Guards the build-out against a second run.
   *
   * StrictMode double-invokes effects and the build-out effect creates
   * teammates, so without this a development build would staff every company
   * twice. A ref rather than state: it must be set before the first await, not
   * on the next render.
   */
  const building = useRef(false);

  useEffect(() => {
    if (!redesign) return;
    // The boundary of a redesign is the team the first pass actually created,
    // captured when the operator left — NOT a re-read of the roster now, which
    // would sweep up teammates other operators added while model settings were
    // open and then delete them below. An empty list (a pass that created
    // nothing) means there is no fallback team to replace.
    redesignRoster.current = new Set(fallbackIds ?? []);
  }, [redesign, fallbackIds]);
  // above covers the return from wiring a model. This is a belt-and-braces
  // catch for a prop flip while mounted — it cannot reopen anything on its own.
  useEffect(() => {
    if (redesign) setReplacing(true);
  }, [redesign]);

  // The actions the notice offers are an admin's, and only the host knows the
  // operator's role — ask it, rather than have the controller thread a guess
  // down. Until the read answers, `canManage` stays `null` and the admin
  // actions stay withheld: the risk worth guarding is a member misdirected by
  // an optimistic default, not an admin briefly missing a link they can reach
  // from the Connections page anyway.
  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const who = await me(client, company);
        if (live) setCanManage(who.role === "admin");
      } catch {
        // Unreadable — stay `null`: the admin actions stay withheld rather
        // than defaulting to a guess that could misdirect a member.
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  // A missing model is a supported configuration, but it changes what all
  // three answers can achieve. Learn that before asking for any of them: the
  // host's roster endpoint deliberately succeeds with a curated team on this
  // path, so waiting until its response would be an after-the-fact disclosure.
  //
  // ## Asked of the company, not the host
  //
  // The question is "will the design pass run for *this* company", and the only
  // thing that decides it is whether the company's runtime carries a roster
  // builder (`src/server/ops/setup.rs`). That builder is constructed in the same
  // branch that builds the harness brain, so the company's own cognition path
  // answers it exactly.
  //
  // The instance wizard's `/api/v1/setup` does not. It reports readiness from
  // the *host's* managed credential alone, and rejects multi-company hosts
  // outright — so a company holding a manifest or runtime BYOK config on a host
  // with no managed key reads as `unavailable` there while its design pass runs
  // perfectly well. Warning that operator about a model we are about to use is
  // the same untrustworthy disclosure this notice exists to prevent, pointing
  // the other way.
  //
  // ## The wait is bounded
  //
  // See `READINESS_TIMEOUT_MS`: an unanswered check must not hold the dialog
  // shut, because there is no way out of it while it does.
  useEffect(() => {
    let cancelled = false;
    setInference("checking");
    // A new check starts from the neutral default. The status answers the
    // *addressed* company, so the value a previous host returned must not bleed
    // into this company's notice — if this check degrades to "unknown" (below),
    // the CTA is offered as before rather than hidden by a reachability answer
    // that belonged to the company this effect used to render.
    setHarnessReachable(true);
    const settle = (value: InferenceReadiness) => {
      if (cancelled) return;
      cancelled = true;
      setInference(value);
    };
    const timer = setTimeout(() => settle("unknown"), READINESS_TIMEOUT_MS);
    // `getInferenceStatus` resolves the host-scope address synchronously, so a
    // client that cannot even build the request (no `scopeFor`, as in the
    // route-close tests' minimal mock) throws here rather than rejecting.
    // Treat it like any unreadable status — unknown, never a promise of a
    // tailored roster.
    try {
      void getInferenceStatus(client, company).then(
        (status) => {
          // The effect may have been cleaned up (company or connection switched)
          // while this request was in flight; a response from the previous host
          // must not overwrite the current company's reachability. `settle`
          // guards its own update below, but this setter runs first — hence the
          // check here rather than relying on it.
          if (cancelled) return;
          setHarnessReachable(status.harnessReachable !== false);
          // `restartRequired` outranks `cognition`: a company whose stored
          // config predates the running brain is *configured* — offering "Set
          // up a model" again would send the operator round a loop they cannot
          // close by configuring more. Name the restart the config is waiting
          // on instead (the flag is only ever set where a restart can help, so
          // this arm needs no reachability test of its own).
          settle(
            status.restartRequired
              ? "restart"
              : status.cognition === DESIGNING_COGNITION
                ? "ready"
                : "unavailable",
          );
        },
        () => {
          // Do not silently treat an unreadable status as a configured model.
          // The setup route may still work, but its result must not be promised
          // as tailored while we could not establish that.
          settle("unknown");
        },
      );
    } catch {
      settle("unknown");
    }
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [client, company]);

  const step = phase.kind === "asking" ? SETUP_STEPS[phase.step] : undefined;
  const problem = useMemo(
    () => (step && touched ? stepProblem(step, draft) : undefined),
    [step, touched, draft],
  );

  const set = useCallback((key: keyof SetupDraft, value: string) => {
    setDraft((prev) => ({ ...prev, [key]: value }));
  }, []);

  const submit = useCallback(async () => {
    setPhase({ kind: "thinking" });
    try {
      const proposal = await proposeRoster(client, company, draft);
      if (!proposal.agents.length) {
        // The host is contracted never to return an empty roster; treat it as a
        // failure rather than showing a build-out with nothing in it.
        setPhase({
          kind: "failed",
          reason: "Your company came back without a team. Try again in a moment.",
        });
        return;
      }
      setPhase({
        kind: "building",
        agents: proposal.agents,
        created: 0,
        // `?? "unspecified"` rather than `?? "no_model"`: a fallback the host
        // declined to explain must not be presented as a missing credential.
        fallback:
          proposal.source === "fallback" ? (proposal.reason ?? "unspecified") : null,
      });
    } catch {
      // A real transport or auth failure — the host answers with its reference
      // team rather than an error for anything less.
      setPhase({
        kind: "failed",
        reason: "We couldn't reach your company. Check the connection and try again.",
      });
    }
  }, [client, company, draft]);

  const next = useCallback(() => {
    if (phase.kind !== "asking") return;
    const current = SETUP_STEPS[phase.step];
    if (stepProblem(current, draft)) {
      setTouched(true);
      return;
    }
    setTouched(false);
    if (phase.step + 1 < SETUP_STEPS.length) {
      setPhase({ kind: "asking", step: phase.step + 1 });
    } else if (draftIsSubmittable(draft)) {
      void submit();
    }
  }, [phase, draft, submit]);

  const back = useCallback(() => {
    if (phase.kind !== "asking" || phase.step === 0) return;
    setTouched(false);
    setPhase({ kind: "asking", step: phase.step - 1 });
  }, [phase]);

  /**
   * Retry a build-out that shipped the standard team because a wired model
   * could not be reached. The company now has that team, so the retry runs in
   * replacing mode: the next build-out clears it before creating the designed
   * one.
   */
  const tryRedesign = useCallback(() => {
    // The build-out effect leaves the guard set once it has run; without this a
    // second submit lands on "Creating your team…" and the effect exits without
    // entering — no teammate is created and no phase follows, so the dialog
    // stalls with no way out.
    building.current = false;
    // This retry is owed a redesign, not merely an open dialog: the fallback
    // team is already on the roster, so a reload before the replacement
    // completes must be able to resume it — the ordinary gate would otherwise
    // report the company staffed and hide every path back in. Persist the debt,
    // naming exactly the rows the failed pass created.
    onRetry?.(createdIds.current);
    // The boundary captured when the dialog opened is stale: the team it named
    // was already replaced by the build-out this retry follows. Falling back to
    // `createdIds.current` — the rows the last build-out created, which are the
    // team now on the roster — bounds the next replacement to exactly the team
    // this retry replaces, instead of sweeping nothing (or everything).
    redesignRoster.current = null;
    setReplacing(true);
    setPhase({ kind: "asking", step: 0 });
  }, [onRetry]);

  // The build-out: create each proposed agent in turn, revealing as we go.
  useEffect(() => {
    if (phase.kind !== "building" || building.current) return;
    building.current = true;
    let cancelled = false;
    const agents = phase.agents;

    (async () => {
      const fallback = phase.fallback;
      // A replacing run may only remove the rows the pass it replaces created.
      // The return-from-Settings path names them from the debt captured when the
      // operator left; the in-dialog retry has no captured debt, so it falls
      // back to the rows this mount's first pass created (still in `createdIds`
      // at this point, before the reset below) — never to "everyone who is not
      // global", which would sweep up teammates added by hand in the meantime.
      const boundary = replacing
        ? redesignRoster.current ?? new Set(createdIds.current)
        : null;
      const boundaryIds = boundary ? Array.from(boundary) : [];
      // This run's creations are the boundary of the next redesign's
      // replacement; the previous run's are moot (replaced below, if this run
      // is one). Captured above, so a replacement that never fully lands can
      // still name every row it was meant to replace.
      createdIds.current = [];
      let createdCount = 0;
      const createdThisRun: string[] = [];
      for (let i = 0; i < agents.length; i++) {
        const agent = agents[i];
        try {
          const created = await client.addTeamMember(
            {
              name: agent.name,
              role: agent.role,
              description: agent.description,
              // Issue #1674: carry the job shape through, so the teammate is
              // created with the belt that shape was approved with on the review
              // screen instead of inheriting the whole company default. The host
              // derives the belt from it; the console never chooses a boundary.
              focus: agent.focus ?? undefined,
            },
            company,
          );
          createdCount += 1;
          if (created.id) {
            createdThisRun.push(created.id);
            createdIds.current.push(created.id);
          }
        } catch {
          // One refused write must not abandon the rest: a company with five of
          // six teammates is a working company, and the operator can add the
          // last by hand. Silent by design — a toast per failure would turn a
          // celebration screen into an error list.
        }
        if (cancelled) return;
        setPhase({ kind: "building", agents, created: i + 1, fallback });
        if (i + 1 < agents.length) {
          await new Promise((resolve) => setTimeout(resolve, REVEAL_MS));
          if (cancelled) return;
        }
      }
      // Clear the team a replacing run replaces only once its FULL replacement
      // is in place. Removing first would leave an unstaffed company if every
      // add above was refused; sweeping on a partial landing would trade a
      // complete fallback team for a handful of new rows — a company is worse
      // off than before the redesign. If the replacement did not fully land,
      // the old team stays and the redesign fails rather than reporting
      // completion over a roster it never finished building.
      if (boundary) {
        if (createdCount < agents.length) {
          // A replacement that did not fully land must not leave a partial new
          // team stacked beside the one it was meant to replace. Roll the rows
          // this run created back before failing, so the redesign is atomic —
          // either the whole replacement is in place or the company is exactly
          // as it was. A ref is lost on reload, so keeping the rows in a ref
          // (as this block once did) strands them for a later retry that can no
          // longer name them; skipping clears the stored debt, stranding them
          // for good. Rolling back removes the rows themselves instead.
          const kept: string[] = [];
          for (const id of createdThisRun) {
            if (cancelled) return;
            try {
              await client.removeTeamMember(id, company);
            } catch {
              // A rollback that could not land leaves its row in place. It stays
              // in the boundary rather than being stranded for the next retry to
              // miss, and the captured Set — which predates this run and cannot
              // name it — is let go so it does not shadow that expanded boundary.
              kept.push(id);
            }
          }
          if (cancelled) return;
          if (kept.length) {
            createdIds.current = [...boundaryIds, ...kept];
            redesignRoster.current = null;
            // The captured debt — which predates this run and cannot name the
            // kept row — is let go above so it does not shadow the expanded
            // boundary. Persist the expansion too: a reload before the retry
            // must still find the kept row, or the eventual replacement would
            // sweep the original boundary and leave it stacked beside the new
            // team.
            onReplacementComplete?.([...boundaryIds, ...kept]);
          } else {
            createdIds.current = boundaryIds;
          }
          building.current = false;
          setPhase({
            kind: "failed",
            reason: "We couldn't build your new team, so we kept the one you have. Try again in a moment.",
          });
          return;
        }
        try {
          const roster = await client.listTeam(company);
          for (const member of staffedTeam(roster)) {
            if (cancelled) return;
            if (!boundary.has(member.id)) continue;
            await client.removeTeamMember(member.id, company);
          }
        } catch {
          // The sweep is part of the replacement, so a sweep that cannot land
          // means the redesign is not complete: the new team would sit beside
          // the old boundary rather than replacing it. Roll the new rows back —
          // the same atomicity the partial-replacement path above applies to the
          // build — and fail, keeping the debt so a retry or reload runs the
          // whole replacement again. Reporting completion here would clear the
          // debt over a duplicated roster and leave no way back in.
          if (cancelled) return;
          const kept: string[] = [];
          for (const id of createdThisRun) {
            if (cancelled) return;
            try {
              await client.removeTeamMember(id, company);
            } catch {
              // A rollback that could not land leaves its row in place. It stays
              // in the boundary rather than being stranded for the next retry to
              // miss, and the captured Set — which predates this run and cannot
              // name it — is let go so it does not shadow that expanded boundary.
              kept.push(id);
            }
          }
          if (cancelled) return;
          if (kept.length) {
            createdIds.current = [...boundaryIds, ...kept];
            redesignRoster.current = null;
            // Persist the expansion too: a reload before the retry must still
            // find the kept row, or the eventual replacement would sweep the
            // original boundary and leave it stacked beside the new team.
            onReplacementComplete?.([...boundaryIds, ...kept]);
          } else {
            createdIds.current = boundaryIds;
          }
          building.current = false;
          setPhase({
            kind: "failed",
            reason: "We couldn't finish replacing your old team, so we kept the one you have. Try again in a moment.",
          });
          return;
        }
      }
      await new Promise((resolve) => setTimeout(resolve, REVEAL_MS * 1.5));
      if (cancelled) return;
      if (replacing) {
        // The persisted redesign debt still names the team this run replaced —
        // rows the sweep above just deleted. Settle it before the completion
        // screen is visible: a designed replacement pays the debt, while a
        // replacement that fell back again is still owed but now names the new
        // fallback's rows. A reload on the completion screen must not reopen
        // redesign against a boundary that no longer exists, or the next pass
        // would create a roster and sweep nothing — a duplicate team.
        onReplacementComplete?.(fallback ? createdIds.current : null);
      }
      setPhase({ kind: "done", agents, fallback });
    })();

    return () => {
      cancelled = true;
    };
    // Keyed on the phase *kind* rather than the whole phase: this effect sets
    // `created` on the same phase, and depending on it would re-enter the loop
    // on every reveal.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [phase.kind, client, company, replacing]);

  return (
    // Blocking: the no-op `onOpenChange` is what makes it so. Base UI drives
    // every dismissal — Esc, the backdrop, the close button — through this
    // callback, so ignoring it leaves the only way out the explicit "I'll do
    // this later" below, which records the skip. A silent dismiss would leave
    // the dialog reopening on every load.
    <Dialog open={open} onOpenChange={() => {}}>
      <DialogContent
        showCloseButton={false}
        className="sm:max-w-lg"
        data-testid="setup-dialog"
      >
        {inference === "checking" && phase.kind === "asking" && (
          <div className="flex flex-col items-center gap-3 py-10" data-testid="setup-inference-check">
            <Loader2 className="size-6 animate-spin text-primary" />
            <p className="text-sm text-muted-foreground">Checking whether this host can design your team…</p>
          </div>
        )}

        {inference !== "checking" && phase.kind === "asking" && step && (
          <>
            <DialogHeader>
              <StepDots total={SETUP_STEPS.length} at={phase.step} />
              <DialogTitle data-testid="setup-question">{step.question}</DialogTitle>
              <DialogDescription>{step.hint}</DialogDescription>
            </DialogHeader>
            {phase.step === 0 && inference !== "ready" && (
              <InferenceNotice
                inference={inference}
                harnessReachable={harnessReachable}
                canManage={canManage}
                onLeave={onLeave}
              />
            )}
            {phase.step === 0 && replacing && (
              <Alert data-testid="setup-redesign-notice">
                <AlertTitle>You already have a team</AlertTitle>
                <AlertDescription>
                  Answering these questions again will replace the standard team
                  on this company with one designed from your answers.
                </AlertDescription>
              </Alert>
            )}
            <div className="grid gap-2 py-2">
              <Label htmlFor={`setup-${step.key}`} className="sr-only">
                {step.question}
              </Label>
              {step.key === "industry" ? (
                <Input
                  id={`setup-${step.key}`}
                  autoFocus
                  value={draft[step.key]}
                  placeholder={step.placeholder}
                  data-testid={`setup-field-${step.key}`}
                  onChange={(e) => set(step.key, e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") next();
                  }}
                />
              ) : (
                <Textarea
                  id={`setup-${step.key}`}
                  autoFocus
                  rows={3}
                  value={draft[step.key]}
                  placeholder={step.placeholder}
                  data-testid={`setup-field-${step.key}`}
                  onChange={(e) => set(step.key, e.target.value)}
                />
              )}
              {problem && (
                <p className="text-sm text-destructive" data-testid="setup-problem">
                  {problem}
                </p>
              )}
            </div>
            <DialogFooter className="sm:justify-between">
              <div className="flex gap-2">
                {phase.step > 0 && (
                  <Button variant="ghost" onClick={back} data-testid="setup-back">
                    <ArrowLeft className="size-4" />
                    Back
                  </Button>
                )}
                <Button variant="ghost" onClick={onSkip} data-testid="setup-skip">
                  I'll do this later
                </Button>
              </div>
              <Button onClick={next} data-testid="setup-next">
                {phase.step + 1 === SETUP_STEPS.length ? "Build my company" : "Next"}
              </Button>
            </DialogFooter>
          </>
        )}

        {phase.kind === "thinking" && (
          <div className="flex flex-col items-center gap-3 py-10" data-testid="setup-thinking">
            <Loader2 className="size-6 animate-spin text-primary" />
            <p className="text-sm text-muted-foreground">Designing your team…</p>
          </div>
        )}

        {(phase.kind === "building" || phase.kind === "done") && (
          <BuildOut
            agents={phase.agents}
            created={phase.kind === "building" ? phase.created : phase.agents.length}
            finished={phase.kind === "done"}
            fallback={phase.fallback}
            harnessReachable={harnessReachable}
            canManage={canManage}
            onDone={onDone}
            onRedesign={() => onRedesign(createdIds.current)}
            onTryAgain={tryRedesign}
          />
        )}

        {phase.kind === "failed" && (
          <>
            <DialogHeader>
              <DialogTitle>That didn't work</DialogTitle>
              <DialogDescription data-testid="setup-failed">{phase.reason}</DialogDescription>
            </DialogHeader>
            <DialogFooter className="sm:justify-between">
              <Button variant="ghost" onClick={onSkip}>
                I'll do this later
              </Button>
              <Button onClick={() => setPhase({ kind: "asking", step: 0 })}>Try again</Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

/**
 * The no-model path is an intentional escape hatch, not a surprise on the
 * completion screen. This is kept beside the first question so every answer
 * after it is given with its consequence visible.
 *
 * The "Set up a model" call-to-action is only honest when this host could run
 * the design path at all — a binary without the `openhuman` feature, or one
 * with no harness pool attached, can never get there however the credential
 * changes. Offering the CTA there would send the operator round a redesign
 * loop that cannot end, so it is omitted and the standard team is the whole
 * story.
 */
function InferenceNotice({
  inference,
  harnessReachable,
  canManage,
  onLeave,
}: {
  /** One of the settled non-ready readouts — never "checking" or "ready". */
  inference: Exclude<InferenceReadiness, "checking" | "ready">;
  harnessReachable: boolean;
  /**
   * Whether the operator may wire or restart the company's model. The actions
   * this notice can point at are an admin's — the Connections inference form
   * and its restart control render only under management authority, and the
   * host refuses the writes for a member — so an operator who cannot land them
   * is told to ask an admin instead of being handed a link that can only 403.
   * `null` while the role read is outstanding: the action is withheld, because
   * an unread role must not be guessed either way.
   */
  canManage: boolean | null;
  onLeave: () => void;
}) {
  const restart = inference === "restart";
  const unavailable = inference === "unavailable";
  const noModel = unavailable && !harnessReachable;
  // Whether the notice has an action to offer. The restart arm is unconditional
  // — `restartRequired` is only ever set where a restart can put the config to
  // work, so the harness path is reachable by construction there.
  const offered = restart || harnessReachable;
  return (
    <Alert data-testid="setup-inference-notice">
      <AlertTitle>
        {restart
          ? "This company needs a restart"
          : noModel
            ? "This deployment can't design your team with a model"
            : unavailable
              ? "This host can't reach a model right now"
              : "We couldn't check this host's model"}
      </AlertTitle>
      <AlertDescription>
        {restart
          ? "A model is set up for this company, but the running brain predates it — teammates keep echoing until the company is restarted. "
          : noModel
            ? "Your answers will create a standard team for your industry — this deployment can't use a model to design one. "
            : unavailable
              ? "Your answers will create a standard team for your industry rather than tailor one to them. "
              : "Your answers may create a standard team rather than a tailored one. "}
        {canManage === null ? (
          "Carry on with the standard team."
        ) : offered && canManage ? (
          <>
            <a
              href="#/settings/connections"
              onClick={onLeave}
              className="font-medium underline underline-offset-4"
            >
              {restart ? "Restart the company" : "Set up a model"}
            </a>{" "}
            or carry on with the standard team.
          </>
        ) : offered ? (
          <>
            Ask an admin to {restart ? "restart the company" : "set up a model"}, or carry on
            with the standard team.
          </>
        ) : (
          "Carry on with the standard team."
        )}
      </AlertDescription>
    </Alert>
  );
}

/**
 * What to say about a curated team, and what to ask for next.
 *
 * Each arm names a different next action, which is the whole reason the host
 * distinguishes them — see `RosterFallback`.
 */
function fallbackExplanation(fallback: NonNullable<Fallback>): string {
  switch (fallback) {
    case "no_model":
      return "A general starting team for your industry — we couldn't reach a model to tailor it to your answers. Rename, retire, or add anyone from the Company page.";
    case "model_unreachable":
      return "A general starting team for your industry — a model is connected, but we couldn't reach it just now to tailor one to your answers. Rename, retire, or add anyone from the Company page, try again, or check the connection in Settings.";
    case "not_designable":
      return "A general starting team for your industry — we reached a model, but there wasn't enough in your answers to tailor one to them. Rename, retire, or add anyone from the Company page, or try again with more about what your business does.";
    case "unspecified":
      return "A general starting team for your industry, rather than one tailored to your answers. Rename, retire, or add anyone from the Company page.";
  }
}

/** The build-out: named teammates appearing one after another. */
function BuildOut({
  agents,
  created,
  finished,
  fallback,
  harnessReachable,
  canManage,
  onDone,
  onRedesign,
  onTryAgain,
}: {
  agents: ProposedAgent[];
  created: number;
  finished: boolean;
  /**
   * Why the curated team shipped instead of a designed one, or `null` when the
   * model designed this one. Said out loud below, and it decides the CTA.
   */
  fallback: Fallback;
  /**
   * Whether this host could ever run the design path. `false` makes the
   * "Add a model in Settings" action a dead end — a credential cannot put a
   * harness-less binary on the design path, so the CTA is omitted.
   */
  harnessReachable: boolean;
  /**
   * Whether the operator may wire or check the company's model. The Settings
   * links below render only under management authority — the Connections card
   * is read-only for a member — so an operator who cannot land the action is
   * told to ask an admin instead of being handed a link that can only 403.
   * `null` while the role read is outstanding: both variants are withheld,
   * because an unread role must not be guessed either way.
   */
  canManage: boolean | null;
  onDone: () => void;
  /**
   * The completion screen's "Add a model in Settings" action: close and send the
   * operator to wire a model, recording that the shipped team is to be
   * redesigned on their return rather than stacked over.
   */
  onRedesign: () => void;
  /** "Try again" for a fallback that a retry could fix — `model_unreachable`. */
  onTryAgain: () => void;
}) {
  return (
    <>
      <DialogHeader>
        <div className="mb-1 flex size-11 items-center justify-center rounded-xl bg-primary/10 text-primary">
          {finished ? <Check className="size-5" /> : <Users className="size-5" />}
        </div>
        <DialogTitle data-testid="setup-buildout-title">
          {finished
            ? fallback
              ? "A solid standard team for your industry"
              : "Your starting team is ready"
            : "Creating your team…"}
        </DialogTitle>
        <DialogDescription>
          {finished
            ? fallback
              ? fallbackExplanation(fallback)
              : "Built from your answers. A starting point — rename, retire, or add anyone from the Company page."
            : buildOutLabel(created, agents.length)}
        </DialogDescription>
      </DialogHeader>
      <ul className="grid gap-2 py-2" data-testid="setup-buildout-list">
        {agents.map((agent, i) => {
          const landed = i < created;
          const tone = TEAM_TONES[toneFor(agent.role)] ?? TEAM_TONES.sky;
          return (
            <li
              key={agent.role}
              data-testid={landed ? "setup-agent-created" : "setup-agent-pending"}
              className={cn(
                "flex items-center gap-3 rounded-lg border px-3 py-2 transition-opacity duration-300",
                landed ? "opacity-100" : "opacity-40",
              )}
            >
              <span
                className={cn(
                  "flex size-8 shrink-0 items-center justify-center rounded-full text-xs font-medium",
                  tone,
                )}
              >
                {landed ? initials(agent.name) : ""}
              </span>
              <span className="min-w-0">
                <span className="block truncate text-sm font-medium">{agent.role}</span>
                <span className="block truncate text-xs text-muted-foreground">
                  {agent.description}
                </span>
              </span>
              {landed ? (
                <Check className="ml-auto size-4 shrink-0 text-primary" />
              ) : (
                <Loader2 className="ml-auto size-4 shrink-0 animate-spin text-muted-foreground/40" />
              )}
            </li>
          );
        })}
      </ul>
      {finished && (
        <DialogFooter>
          {/*
            Only the reasons a retry or a credential could actually fix. A model
            that answered and was not designable already had a working key, so
            sending that operator to Settings is an instruction that cannot help
            them — what they need is to say more about the business, and the
            retry restarts the questions so they can. A wired but unreachable
            model is both at once: the blip may have passed (retry) or the
            credential may have been rejected (Settings), so the route is
            offered alongside the retry rather than instead of it.
          */}
          {fallback === "no_model" && harnessReachable && canManage === true && (
            <a
              href="#/settings/connections"
              onClick={onRedesign}
              data-testid="setup-add-model"
              className={buttonVariants({ variant: "outline" })}
            >
              Add a model in Settings
            </a>
          )}
          {fallback === "no_model" && harnessReachable && canManage === false && (
            <span
              className="text-sm text-muted-foreground"
              data-testid="setup-add-model-member"
            >
              Ask an admin to add a model, or carry on with the standard team.
            </span>
          )}
          {fallback === "not_designable" && (
            <Button
              variant="outline"
              onClick={onTryAgain}
              data-testid="setup-try-redesign"
            >
              <RotateCcw className="size-4" />
              Try again
            </Button>
          )}
          {fallback === "model_unreachable" && (
            <>
              <Button
                variant="outline"
                onClick={onTryAgain}
                data-testid="setup-try-redesign"
              >
                <RotateCcw className="size-4" />
                Try again
              </Button>
              {canManage === true && (
                <a
                  href="#/settings/connections"
                  onClick={onRedesign}
                  data-testid="setup-check-connection"
                  className={buttonVariants({ variant: "outline" })}
                >
                  Check connection in Settings
                </a>
              )}
              {canManage === false && (
                <span
                  className="text-sm text-muted-foreground"
                  data-testid="setup-check-connection-member"
                >
                  Ask an admin to check the connection, or carry on with the standard team.
                </span>
              )}
            </>
          )}
          <Button onClick={onDone} data-testid="setup-finish">
            <Sparkles className="size-4" />
            Show me my company
          </Button>
        </DialogFooter>
      )}
    </>
  );
}

/** Which of the three questions we are on. */
function StepDots({ total, at }: { total: number; at: number }) {
  return (
    <div className="mb-2 flex items-center gap-1.5" aria-hidden>
      {Array.from({ length: total }, (_, i) => (
        <span
          key={i}
          className={cn(
            "h-1.5 rounded-full transition-all",
            i === at ? "w-6 bg-primary" : i < at ? "w-1.5 bg-primary/60" : "w-1.5 bg-muted",
          )}
        />
      ))}
    </div>
  );
}
