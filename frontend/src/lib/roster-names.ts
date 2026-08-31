// One shared way to turn a roster id into the name a human recognizes it by
// (issue #973). #931 hit this once already: an operator-added teammate's id is
// a minted internal string (a ULID for the eight teammates created before
// #686 started minting a readable slug), and printing it told a reader
// nothing. #939 fixed the two connections surfaces by having the host pair
// each id with a name server-side (`RosterAgent`, in `@/api/types`) — but that
// only works where the host is already walking the roster to answer the
// request. A surface that only has the roster listing (`GET …/team`) and a
// bare id to look up in it — the workspace tree chief among them, since the id
// is also that teammate's real folder name — needs the client-side twin: build
// one lookup from the roster read, then resolve every id through it. Route any
// new id-bearing surface through this rather than growing a fifth per-surface
// fix.

import type { RosterAgent } from "@/api/types";

/** id -> display name, built from a roster listing. */
export type RosterNames = ReadonlyMap<string, string>;

/**
 * The one spelling two forms of a roster id are compared under (issue #1723).
 *
 * A roster id is `[a-z0-9_]` by construction (#686 mints it as a slug), and
 * the folder the runtime mints for that teammate goes through `kebab_name`
 * (`src/company/workspace_names.rs`), whose only effect on such an id is
 * `_` → `-`. Collapsing that one difference is therefore the whole of the
 * mapping — a general transcription of the host's rule here would be a second
 * grammar to keep in step with it, which is the drift this module's header
 * argues against.
 */
export function rosterIdKey(id: string): string {
  return id.replaceAll("_", "-");
}

/**
 * Build the lookup {@link rosterDisplayName} needs, from a roster read.
 *
 * Each teammate is indexed under **both spellings of their id** (issue #1723).
 * The id is the roster's, `frontend_engineer`; the folder the runtime mints for
 * that teammate under `agents/`/`artifacts/` goes through `kebab_name`
 * (`src/company/workspace_names.rs`), which lands it as `frontend-engineer` on
 * any company that had not already created the folder under the verbatim id.
 * Keyed only by the raw id, this map missed every one of those folders — so the
 * surface #973 was filed about kept printing a handle on exactly the companies
 * provisioned since the naming rule shipped.
 *
 * The second key is {@link rosterIdKey}, which is the whole of the difference.
 */
export function rosterNameMap(agents: readonly Pick<RosterAgent, "id" | "name">[]): RosterNames {
  const names = new Map<string, string>();
  for (const agent of agents) {
    names.set(agent.id, agent.name);
    const kebab = rosterIdKey(agent.id);
    // `set` rather than an unconditional write, so a teammate whose real id IS
    // the kebab form can never be shadowed by another's alias.
    if (kebab !== agent.id && !names.has(kebab)) names.set(kebab, agent.name);
  }
  return names;
}

/**
 * Resolve a roster id to its display name.
 *
 * Falls back to the id itself — never a blank label — when the roster has
 * nothing to say about it: an id the roster does not carry (the roster has not
 * loaded yet, or the id names something else entirely) or an entry whose name
 * is genuinely empty.
 */
export function rosterDisplayName(id: string, names: RosterNames): string {
  return names.get(id) || id;
}
