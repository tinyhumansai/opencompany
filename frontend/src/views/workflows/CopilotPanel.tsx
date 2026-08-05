// Issue #303: the per-workflow copilot panel.
//
// A chat scoped to the workflow on screen. It answers questions about that
// workflow — what it does, why its last run failed, what to change — grounded in
// the graph and run history the console already holds. See
// `@/api/workflow-copilot` for why it needs no new host route and how the
// scoping is enforced server-side.
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
import type { WorkflowGraph, WorkflowRunOutcome } from "@/api/workflows";
import {
  askCopilot,
  loadCopilotHistory,
  type CopilotMessage,
} from "@/api/workflow-copilot";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Markdown } from "@/components/markdown";
import { Textarea } from "@/components/ui/textarea";

/** A local id for a message this session created (the journal supplies ids for
 * replayed ones). */
let localSeq = 0;
function localId(): string {
  localSeq += 1;
  return `local-${localSeq}`;
}

export function CopilotPanel({
  client,
  company,
  graph,
  runs,
  runsKnown,
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
  onClose: () => void;
}) {
  const [messages, setMessages] = useState<CopilotMessage[]>([]);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // The cognition path the company actually booted onto. `null` until the read
  // lands, or when the host does not serve the route.
  const [cognition, setCognition] = useState<CognitionPath | null>(null);
  const scrollRef = useRef<HTMLDivElement>(null);

  const workflowId = graph.id;
  const sourceDefined = graph.editable === false;

  // Replay this workflow's transcript. Keyed on the workflow id, so switching
  // workflow swaps transcripts rather than appending to the previous one.
  useEffect(() => {
    let live = true;
    setMessages([]);
    setError(null);
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
    (async () => {
      try {
        const status = await getInferenceStatus(client, company);
        if (live) setCognition(status.cognition);
      } catch (e) {
        // A host without the route tells us nothing either way. Staying `null`
        // lets the composer work — refusing to send because we could not
        // confirm would break the copilot on hosts where it works fine.
        console.debug("[CopilotPanel] inference status unavailable", e);
        if (live) setCognition(null);
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  // Keep the newest turn in view.
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages, sending]);

  const echoing = cognition === "echo";

  const send = useCallback(async () => {
    const question = draft.trim();
    if (!question || sending || echoing) return;
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
      setMessages((prev) => [
        ...prev,
        // An empty `responses` array is a real answer shape, not a crash: the
        // cycle ran and produced no channel reply. Say that rather than
        // dropping the turn silently, which would look like the message was
        // never sent.
        ...(replies.length === 0
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
            }))),
      ]);
    } catch (e) {
      setError(e instanceof Error ? e.message : "the copilot could not answer");
    } finally {
      setSending(false);
    }
  }, [client, company, draft, echoing, graph, runs, runsKnown, sending, workflowId]);

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
            question rather than discovered after it. */}
        <div className="rounded-lg border bg-muted/30 p-2 text-[11px] leading-snug text-muted-foreground">
          <p>
            Answers are scoped to <span className="font-medium text-foreground">{graph.name}</span>.
            It can see this workflow's steps and its recorded runs — not other
            workflows.
          </p>
          <p className="mt-1.5">
            It can explain and suggest, but{" "}
            <span className="font-medium text-foreground">it can't change the workflow</span>.
            {sourceDefined
              ? " This one is defined by a file in the company source tree, so changes belong in the company repository."
              : " Apply a change yourself with Edit — that's the path that checks the graph and refuses a stale write."}
          </p>
          {/* The copilot is a company chat turn, and a chat turn that reads as
              an instruction opens a board card. That is the host's ordinary
              behaviour, not a copilot quirk — but an operator who was not told
              would find a card they never asked for, so say it once, up front,
              and frame it as what it is: how a request gets recorded when the
              copilot itself cannot act on it. */}
          <p className="mt-1.5">
            Asking for a change may open a card on the board — that's how the
            company records work to pick up.
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
              // dialect.
              <Markdown className="prose-sm">{m.text}</Markdown>
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
          disabled={sending || echoing}
          placeholder={echoing ? "Inference isn't configured." : placeholder}
          aria-label={`Ask the copilot about ${graph.name}`}
          className="mb-2 max-h-32 min-h-16 resize-none text-xs"
          data-testid="workflow-copilot-input"
        />
        <Button
          size="sm"
          className="w-full"
          onClick={() => void send()}
          disabled={sending || echoing || !draft.trim()}
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
