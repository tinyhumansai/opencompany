import type { OpenCompanyClient } from "@/api/client";
import type { EditAgentInput } from "@/api/types";

/** One teammate's pending write. */
export interface AgentAccessWrite {
  agentId: string;
  input: EditAgentInput;
}

export interface AgentAccessOutcome {
  done: number;
  total: number;
  /** The first write the host refused; nothing after it was sent. */
  failure: { agentId: string; message: string } | null;
}

/**
 * Sends one `PATCH …/team/{agentId}` per write, in order, stopping at the first
 * failure so the operator can be told exactly which teammates changed.
 */
export async function saveAgentAccess(
  client: OpenCompanyClient,
  company: string | null,
  writes: readonly AgentAccessWrite[],
  onWritten?: (agentId: string) => void,
): Promise<AgentAccessOutcome> {
  let done = 0;
  for (const write of writes) {
    try {
      await client.updateAgent(write.agentId, write.input, company);
      done += 1;
      onWritten?.(write.agentId);
    } catch (e) {
      return {
        done,
        total: writes.length,
        failure: {
          agentId: write.agentId,
          message: e instanceof Error ? e.message : "the host refused it",
        },
      };
    }
  }
  return { done, total: writes.length, failure: null };
}

/** The sentence a partial save leaves on screen. */
export function partialSaveMessage(
  outcome: AgentAccessOutcome,
  nameOf: (agentId: string) => string,
  verb = "Updated",
): string | null {
  if (outcome.failure === null) return null;
  const unsent = outcome.total - outcome.done - 1;
  return (
    `${verb} ${outcome.done} of ${outcome.total} teammates. Failed on ${nameOf(outcome.failure.agentId)}: ` +
    `${outcome.failure.message}.` +
    (unsent > 0
      ? ` The other ${unsent} ${unsent === 1 ? "was" : "were"} not changed — review and save again.`
      : "")
  );
}
