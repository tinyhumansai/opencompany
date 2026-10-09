import type { McpAgentAccess } from "@/api/types";

/** Whether the teammate's box starts ticked: it reaches the server through its grants. */
export function grantedNow(agent: McpAgentAccess): boolean {
  return agent.state === "inherited" || agent.state === "included";
}

/**
 * The `tools` list to send when the box moves to `on`, or `undefined` when
 * this page cannot make that change.
 */
export function nextTools(agent: McpAgentAccess, on: boolean): string[] | undefined {
  if (on === grantedNow(agent)) return undefined;
  return on ? agent.grantTools : agent.revokeTools;
}

/** Whether the box can move at all. */
export function canToggle(agent: McpAgentAccess): boolean {
  return grantedNow(agent) ? agent.revokeTools !== undefined : agent.grantTools !== undefined;
}

/** Why a box is locked, in the operator's terms. */
export function lockedReason(agent: McpAgentAccess, grant: string): string | null {
  if (agent.state === "blocked") {
    return `The company or desk tool grants leave out ${grant}, so this teammate can't be given it here.`;
  }
  if (grantedNow(agent) && agent.revokeTools === undefined) {
    return "A broad tool grant gives this teammate access; narrow it on the teammate's Tools tab.";
  }
  return null;
}

/** Said before saving a removal from a teammate that inherits the company grants. */
export const MCP_PINS_INHERITED_WARNING =
  "Removing a teammate that inherits the company's tool grants writes out its own list, " +
  "so tools the company grants later will not reach it. Hand it back to inheriting on its Tools tab.";

/** How one row describes its standing. */
export function accessStateLabel(agent: McpAgentAccess): string {
  switch (agent.state) {
    case "inherited":
      return "inherits company grants";
    case "included":
      return "granted on its own list";
    case "excluded":
      return "not on its list";
    case "blocked":
      return "not granted by the company";
  }
}
