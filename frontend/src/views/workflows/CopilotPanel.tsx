// Issue #303: the per-workflow copilot panel.
//
// A chat scoped to the workflow on screen. It answers questions about that
// workflow — what it does, why its last run failed, what to change — grounded in
// the graph and run history the console already holds. See
// `@/api/workflow-copilot` for why it needs no new host route and how the
// scoping is enforced server-side.
//
// Issue #416 made "scoped" true of the answering agent and not only of the
// question: the host reads the copilot thread id and runs the turn confined —
// no tools, no company memory, no delegation. That is what the disclosure block
// below is allowed to claim, and it claims exactly that and no more.
//
// ## It says what it cannot do
//
// Two honesty gates, and both exist because the failure they prevent is a chat
// box that LOOKS capable and is not:
//
// * **Advice only.** It cannot write the graph. Nothing here calls
//   `updateWorkflow`, and the composed prompt tells the model so too, because a
//   model that claims "done — I've added the retry" would be worse than one that
//   refuses. Applying a change is still the editor's job, and the editor already
//   carries the version token and the 409 path.
// * **A company with no inference configured is named as such.** The host
//   answers `POST …/chat` with `200 {"responses":[{"text":"You said: …"}]}` when
//   it booted onto the echo brain — there is no error status to catch. So the
//   panel reads `GET …/inference` and, on `cognition: "echo"`, refuses to send
//   and says why. Without this the copilot would parrot every question back and
//   look like a broken model rather than an unconfigured company.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Bot, Loader2, Send } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { getInferenceStatus, type CognitionPath } from "@/api/inference";
import { ApiError } from "@/api/types";
import { updateWorkflow, type WorkflowGraph, type WorkflowRunOutcome } from "@/api/workflows";
import {
  askCopilot,
  loadCopilotHistory,
  type CopilotMessage,
} from "@/api/workflow-copilot";
import {
  applyProposal,
  diffGraphs,
  isEmptyDiff,
  proposalIsStale,
  readProposal,
  type GraphDiff,
  type WorkflowProposal,
} from "@/api/workflow-proposal";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Markdown } from "@/components/markdown";
import { Textarea } from "@/components/ui/textarea";
import { ProposalCard, type ProposalState } from "@/views/workflows/ProposalCard";

/** A local id for a message this session created (the journal supplies ids for
 * replayed ones). */
let localSeq = 0;
function localId(): string {
  localSeq += 1;
  return `local-${localSeq}`;
}

/** One company reply, split into what the operator reads and what they review. */
interface Review {
  /** The reply with the proposal block stripped out. */
  prose: string;
  proposal?: WorkflowProposal;
  /** The candidate graph and its diff, computed once alongside the proposal. */
  diff?: GraphDiff;
  /** A malformed proposal block, said out loud rather than dropped. */
  problem?: string;
  state: ProposalState;
  /** The host's refusal of an apply, when one came back. */
  error?: string;
}

export function CopilotPanel({
  client,
  company,
  graph,
  runs,
  runsKnown,
  runsReady,
  onApplied,
  onConflict,
  onClose,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /** The workflow this copilot is scoped to. The panel renders nothing without
   * one — there is no such thing as an unscoped workflow copilot. */
  graph: WorkflowGraph;
  /** That workflow's OWN run history (the server-scoped read). */
  runs: WorkflowRunOutcome[];
  /** Whether the host served that history at all — see {@link CopilotContext}. */
  runsKnown: boolean;
  /**
   * Whether `runs` is the history of THIS workflow rather than the one selected
   * before it.
   *
   * The graph and the run history are two independent requests, so a workflow
   * switch can land the new graph while the previous workflow's runs are still
   * on screen. Grounding an answer on that pair is worse than not grounding it
   * at all — it would describe workflow B's steps using workflow A's failures —
   * so the panel refuses to send until the two agree.
   */
  runsReady: boolean;
  /**
   * A proposal was applied and the host stored the result (issue #415).
   *
   * The saved graph carries a fresh version token, so this is the same hand-off
   * the edit dialog makes on save: the view takes the stored graph as current,
   * the canvas re-renders, and every proposal still on screen becomes stale
   * against it — which is the behaviour we want, not a bug to work around.
   */
  onApplied: (saved: WorkflowGraph) => void;
  /** A `409` from an apply, for the view's persistent reload banner. */
  onConflict?: (message: string) => void;
  onClose: () => void;
}) {
  const [messages, setMessages] = useState<CopilotMessage[]>([]);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // The cognition path the company actually booted onto. `null` until the read
  // lands, or when the host does not serve the route.
  const [cognition, setCognition] = useState<CognitionPath | null>(null);
  // Whether that read has come back yet — either way, answer or failure.
  //
  // Separate from `cognition` because `null` is genuinely ambiguous: it is both
  // "not asked yet" and "asked, and this host does not serve the route". The
  // echo gate has to tell those apart, or it is open for the whole round trip
  // and a fast question gets exactly the parroted reply the gate exists to
  // prevent.
  const [inferenceChecked, setInferenceChecked] = useState(false);
  // Issue #415. One entry per company message that carried a proposal block,
  // parsed EXACTLY ONCE, when the message first appears.
  //
  // Once, because the parse is what stamps the graph version the proposal was
  // reasoned about. Re-parsing on a later render — after the operator edited
  // the canvas, or after another proposal was applied — would re-stamp it
  // against a graph it never saw, which is precisely the silent rebase the
  // issue rules out.
  const [reviews, setReviews] = useState<Record<string, Review>>({});
  // Message ids this session produced. A replayed proposal is never applyable:
  // the graph it was reasoned about is not knowable from the journal, and
  // "apply this to whatever is there now" is the failure mode, not the feature.
  const [thisSession, setThisSession] = useState<ReadonlySet<string>>(() => new Set());
  const scrollRef = useRef<HTMLDivElement>(null);
  // Bumped whenever the conversation this panel is holding changes identity.
  // An in-flight request captures it and drops its result if it no longer
  // matches — see `send`.
  const conversation = useRef(0);

  const workflowId = graph.id;
  const sourceDefined = graph.editable === false;

  // Replay this workflow's transcript. Keyed on the workflow id, so switching
  // workflow swaps transcripts rather than appending to the previous one.
  //
  // This is also the one place the conversation's identity changes, so it is
  // where the generation is bumped: any request still in flight for the
  // previous (workflow, company) pair is abandoned from here on.
  useEffect(() => {
    let live = true;
    conversation.current += 1;
    setMessages([]);
    setError(null);
    setSending(false);
    (async () => {
      const replayed = await loadCopilotHistory(client, company, workflowId);
      if (live) setMessages(replayed);
    })();
    return () => {
      live = false;
    };
  }, [client, company, workflowId]);

  // Whether this company can actually think. See the header — an unconfigured
  // company answers 200 with an echo, so this read is the only way to tell.
  useEffect(() => {
    let live = true;
    setInferenceChecked(false);
    (async () => {
      try {
        const status = await getInferenceStatus(client, company);
        if (live) setCognition(status.cognition);
      } catch (e) {
        // A host without the route tells us nothing either way. Leaving
        // `cognition` null lets the composer work once the check has *settled*
        // — refusing to send because we could not confirm would break the
        // copilot on hosts where it works fine.
        console.debug("[CopilotPanel] inference status unavailable", e);
        if (live) setCognition(null);
      } finally {
        // Settled either way: this is what re-opens the composer, so it must
        // run on the failure path too or an older host disables it forever.
        if (live) setInferenceChecked(true);
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  // Read each new company reply once: prose out, proposal (or the reason there
  // isn't a usable one) in. `graph` is a dependency because a reply arriving
  // after a canvas edit must be read against the graph on screen — but an
  // already-read message is skipped, so an existing proposal keeps the version
  // it was reasoned about.
  useEffect(() => {
    setReviews((prev) => {
      let next = prev;
      for (const message of messages) {
        if (message.role !== "company" || next[message.id]) continue;
        const read = readProposal(message.text, graph);
        const review: Review = {
          prose: read.prose,
          proposal: read.proposal,
          diff: read.proposal
            ? diffGraphs(graph, applyProposal(graph, read.proposal))
            : undefined,
          problem: read.problem?.reason,
          state: "pending",
        };
        if (next === prev) next = { ...prev };
        next[message.id] = review;
      }
      return next;
    });
  }, [messages, graph]);

  // Keep the newest turn in view.
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages, sending]);

  /**
   * Applies one proposal through the SAME write the canvas editor performs:
   * `updateWorkflow` with the version this panel is holding. So a proposal
   * cannot produce a graph the editor could not have produced, and cannot
   * overwrite a graph that moved while the operator was reading the diff — the
   * host refuses that with a 409, and this refuses it one step earlier with a
   * sentence naming what happened.
   */
  const apply = useCallback(
    async (messageId: string, proposal: WorkflowProposal) => {
      setReviews((prev) => ({
        ...prev,
        [messageId]: { ...prev[messageId], state: "applying", error: undefined },
      }));
      try {
        const saved = await updateWorkflow(
          client,
          company,
          graph.id,
          applyProposal(graph, proposal),
          graph.version,
        );
        setReviews((prev) => ({
          ...prev,
          [messageId]: { ...prev[messageId], state: "applied", error: undefined },
        }));
        onApplied(saved);
      } catch (e) {
        const conflict = e instanceof ApiError && e.status === 409;
        const message = conflict
          ? `${e.message} Reload the workflow and ask the copilot again.`
          : e instanceof Error
            ? e.message
            : "The change could not be applied.";
        // Back to `pending`, not to a dead end: the diff stays on screen and the
        // operator can dismiss it or retry after reloading.
        setReviews((prev) => ({
          ...prev,
          [messageId]: { ...prev[messageId], state: "pending", error: message },
        }));
        if (conflict && e instanceof ApiError) onConflict?.(e.message);
      }
    },
    [client, company, graph, onApplied, onConflict],
  );

  /** Turning a proposal down writes nothing; the card stays, greyed. */
  const dismiss = useCallback((messageId: string) => {
    setReviews((prev) => ({
      ...prev,
      [messageId]: { ...prev[messageId], state: "dismissed", error: undefined },
    }));
  }, []);

  /**
   * Why this proposal cannot be applied, or `undefined` when it can.
   *
   * Every arm here is a refusal the issue asks for by name: a graph that moved
   * under the proposal, a proposal replayed from a session whose graph we
   * cannot know, a workflow the host will not let the console write at all, and
   * a proposal that turns out to change nothing.
   */
  function blockedReason(messageId: string, review: Review): string | undefined {
    if (!review.proposal || !review.diff) return undefined;
    if (sourceDefined) {
      return "This workflow is defined by a file in the company source tree, so a change has to be made in the repository.";
    }
    if (!thisSession.has(messageId)) {
      return "This was proposed in an earlier session, so it can't be applied to the workflow as it stands now. Ask again for a fresh proposal.";
    }
    if (proposalIsStale(review.proposal, graph)) {
      return "The workflow changed after this was proposed, so it no longer describes the graph on screen. Ask again for a fresh proposal.";
    }
    if (isEmptyDiff(review.diff)) {
      return "This proposal would change nothing, so there is nothing to apply.";
    }
    return undefined;
  }

  const echoing = cognition === "echo";
  // Nothing may be asked until BOTH the echo check has settled and the run
  // history on screen is this workflow's. Either one unresolved means a
  // question would be answered on a footing we cannot vouch for.
  const ready = inferenceChecked && runsReady;

  const send = useCallback(async () => {
    const question = draft.trim();
    if (!question || sending || echoing || !ready) return;
    // Which conversation this request belongs to.
    //
    // The panel is keyed on the workflow id by its parent, so switching
    // workflow normally unmounts this instance and React drops its state
    // updates on the floor. That is not a guarantee worth relying on from in
    // here: it lives in another file, and it does NOT hold when the id is
    // stable across the change — switching company to one that happens to have
    // a workflow of the same id keeps the key identical, so the panel is
    // reused and an in-flight reply from the previous company would land in
    // the new transcript. Comparing the generation makes the guarantee local.
    const generation = conversation.current;
    const mine = () => conversation.current === generation;
    setDraft("");
    setError(null);
    setMessages((prev) => [
      ...prev,
      { id: localId(), role: "operator", text: question, atMillis: Date.now() },
    ]);
    setSending(true);
    try {
      const replies = await askCopilot(
        client,
        company,
        workflowId,
        { graph, runs, runsKnown },
        question,
      );
      if (!mine()) return;
      // An empty `responses` array is a real answer shape, not a crash: the
      // cycle ran and produced no channel reply. Say that rather than
      // dropping the turn silently, which would look like the message was
      // never sent.
      const answered: CopilotMessage[] =
        replies.length === 0
          ? [
              {
                id: localId(),
                role: "company" as const,
                text: "_The company ran the turn but produced no reply._",
                atMillis: Date.now(),
              },
            ]
          : replies.map((text) => ({
              id: localId(),
              role: "company" as const,
              text,
              atMillis: Date.now(),
            }));
      // Recorded before the messages land, so the review effect never sees a
      // reply from this session and reads it as replayed (issue #415).
      setThisSession((prev) => {
        const next = new Set(prev);
        for (const message of answered) next.add(message.id);
        return next;
      });
      setMessages((prev) => [...prev, ...answered]);
    } catch (e) {
      if (!mine()) return;
      setError(e instanceof Error ? e.message : "the copilot could not answer");
    } finally {
      // Guarded too: an abandoned request clearing this would re-enable the
      // composer for a NEWER question that is still in flight.
      if (mine()) setSending(false);
    }
  }, [client, company, draft, echoing, graph, ready, runs, runsKnown, sending, workflowId]);

  const placeholder = useMemo(
    () =>
      runs.length > 0
        ? "Ask about this workflow — what it does, or why a run failed."
        : "Ask about this workflow — what it does, or what it needs to run.",
    [runs.length],
  );

  return (
    <div
      className="absolute right-3 top-3 bottom-3 z-10 flex w-80 flex-col overflow-hidden rounded-xl border bg-card/95 shadow-lg backdrop-blur sm:w-96"
      data-testid="workflow-copilot"
    >
      <div className="flex items-start justify-between gap-2 border-b px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <Bot className="size-4 shrink-0" aria-hidden />
          <div className="min-w-0">
            <div className="truncate text-sm font-semibold">Copilot</div>
            <div className="truncate text-[11px] text-muted-foreground">{graph.name}</div>
          </div>
        </div>
        <Button variant="ghost" size="sm" className="-mr-1 h-7 px-2" onClick={onClose}>
          Close
        </Button>
      </div>

      <div ref={scrollRef} className="min-h-0 flex-1 space-y-3 overflow-auto px-3 py-3">
        {/* What it can see and what it cannot do, stated before the first
            question rather than discovered after it.

            This block is a claim about the host, so it changes only when the
            host does. #405 had to *withdraw* a confinement claim: the thread
            bought transcript isolation, not a confined responder, and the
            teammate answering held the company's whole context and tool
            surface. #416 built the confinement, so the claim is back — and it
            is now the narrower, checkable one: no tools, no company context,
            and a turn that says which part of a question it could not answer
            rather than reaching for the rest of the company. See the header of
            `@/api/workflow-copilot`, and `harness::confine` host-side. */}
        <div className="rounded-lg border bg-muted/30 p-2 text-[11px] leading-snug text-muted-foreground">
          <p>
            Answers are grounded in{" "}
            <span className="font-medium text-foreground">{graph.name}</span>: its steps and
            its recorded runs are what gets sent with your question.
          </p>
          <p className="mt-1.5">
            That is also all the answer is drawn from. This turn runs{" "}
            <span className="font-medium text-foreground">confined to this workflow</span>: no
            tools, no company memory, and no reach into the board, your teammates or another
            workflow. Ask something that needs the wider company and it will say so rather
            than guess.
          </p>
          <p className="mt-1.5">
            The conversation stays here too: it isn&apos;t in the company chat, and another
            workflow&apos;s copilot can&apos;t see it.
          </p>
          <p className="mt-1.5">
            {sourceDefined ? (
              <>
                It can explain and suggest, but{" "}
                <span className="font-medium text-foreground">
                  it can&apos;t change the workflow
                </span>
                . This one is defined by a file in the company source tree, so changes belong in
                the company repository.
              </>
            ) : (
              <>
                Ask for a change and it{" "}
                <span className="font-medium text-foreground">proposes one you review</span>: you
                see the diff and decide. Nothing is written until you press Apply, and it goes
                through the same save the editor uses, which refuses a workflow that moved while
                you were reading.
              </>
            )}
          </p>
        </div>

        {echoing && (
          <Alert variant="destructive" data-testid="workflow-copilot-echo">
            <AlertDescription className="text-[11px] leading-snug">
              This company has no inference configured, so it can't answer
              questions — it would just repeat them back. Set a provider in
              Settings → Inference, then reopen the copilot.
            </AlertDescription>
          </Alert>
        )}

        {messages.length === 0 && !echoing && (
          <p className="text-[11px] text-muted-foreground">
            No questions yet. Try “what does this workflow do?” or “why did the
            last run fail?”.
          </p>
        )}

        {messages.map((m) => (
          <div
            key={m.id}
            className={
              m.role === "operator"
                ? "ml-6 rounded-lg border bg-background/60 p-2"
                : "mr-2 rounded-lg border bg-accent/30 p-2"
            }
            data-testid={
              m.role === "operator" ? "workflow-copilot-ask" : "workflow-copilot-reply"
            }
          >
            <p className="mb-1 text-[10px] uppercase tracking-wide text-muted-foreground">
              {m.role === "operator" ? "You" : "Company"}
            </p>
            {m.role === "operator" ? (
              <p className="whitespace-pre-wrap text-xs leading-snug">{m.text}</p>
            ) : (
              // The same renderer every other markdown surface uses, so a
              // copilot answer reads like a chat reply rather than its own
              // dialect. The proposal block is stripped out of the prose and
              // rendered as the review card below, so the operator never reads
              // raw JSON (issue #415).
              <Markdown className="prose-sm">{reviews[m.id]?.prose ?? m.text}</Markdown>
            )}
            {m.role === "company" &&
              reviews[m.id]?.proposal &&
              reviews[m.id]?.diff && (
                <div className="mt-2">
                  <ProposalCard
                    summary={reviews[m.id].proposal!.summary}
                    diff={reviews[m.id].diff!}
                    state={reviews[m.id].state}
                    blocked={blockedReason(m.id, reviews[m.id])}
                    error={reviews[m.id].error}
                    onApply={() => void apply(m.id, reviews[m.id].proposal!)}
                    onDismiss={() => dismiss(m.id)}
                  />
                </div>
              )}
            {/* A block that arrived malformed is said out loud. Dropping it
                would leave the operator reading prose that describes a change
                with no way to apply it and no hint that one was attempted. */}
            {m.role === "company" && reviews[m.id]?.problem && (
              <Alert className="mt-2 py-1.5">
                <AlertDescription className="text-[11px] leading-snug">
                  The copilot tried to propose a change this console couldn&apos;t read.{" "}
                  {reviews[m.id].problem} Ask again, or make the change in the editor.
                </AlertDescription>
              </Alert>
            )}
          </div>
        ))}

        {sending && (
          <p className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
            <Loader2 className="size-3 animate-spin" />
            Thinking…
          </p>
        )}

        {error && (
          <Alert variant="destructive">
            <AlertDescription className="text-[11px]">{error}</AlertDescription>
          </Alert>
        )}
      </div>

      <div className="border-t p-2">
        <Textarea
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            // Enter sends, Shift+Enter breaks the line — the convention the
            // company chat composer already uses.
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void send();
            }
          }}
          disabled={sending || echoing || !ready}
          placeholder={
            echoing
              ? "Inference isn't configured."
              : !ready
                ? "Checking what this company can answer with…"
                : placeholder
          }
          aria-label={`Ask the copilot about ${graph.name}`}
          className="mb-2 max-h-32 min-h-16 resize-none text-xs"
          data-testid="workflow-copilot-input"
        />
        <Button
          size="sm"
          className="w-full"
          onClick={() => void send()}
          disabled={sending || echoing || !ready || !draft.trim()}
          data-testid="workflow-copilot-send"
        >
          {sending ? (
            <Loader2 className="mr-1.5 size-4 animate-spin" />
          ) : (
            <Send className="mr-1.5 size-4" />
          )}
          Ask
        </Button>
      </div>
    </div>
  );
}
