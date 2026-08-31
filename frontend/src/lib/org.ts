// The company org chart: the three-level tree the console draws over the
// structure the company already declares (issue #311).
//
// # Why this is a reader and not a schema
//
// #311 settles it: "existing desks are already durable, so this is a new reader
// over existing data, not a data change." The host's desk model is flat — a
// `[[group_chat]]` (or an operator-created overlay desk) has an id, a name, a
// description and an ordered member list, and **no parent pointer**. So the
// three levels are not a field anybody stores; they are the shape of the data:
//
//   1  the company
//   2  a desk (a team)
//   3  a seat on that desk (a teammate)
//
// That makes the three-level cap **structural**. A fourth level is not
// forbidden by a check in the console or a guard in the host — it is
// unrepresentable, because no desk can name a parent desk. Nothing here
// validates depth for the same reason nothing validates that a string is a
// string. `MAX_DEPTH` exists so a reader can see the number, and so the test
// suite can assert it against a real tree rather than against a comment.
//
// Building the tree from a parent id instead would need that pointer on both
// `GroupChat` and `OverlayDesk`, plus manifest schema, validation, export/import
// round-trip and rebuild preservation — the data change #311 rules out.

import type { DeskDto, TeamMemberDto } from "@/api/types";
import { avatarFor, fromDto, type TeamMember } from "@/lib/team";

/**
 * How deep the org chart can go: company, desk, seat.
 *
 * Not a limit that is enforced anywhere, because it cannot be exceeded — see
 * the note at the top of this file. It is here to be read and asserted.
 */
export const MAX_DEPTH = 3;

/**
 * Where a desk, or a seat on one, came from.
 *
 * The distinction has teeth rather than being decoration: the host refuses to
 * delete a `blueprint` desk or remove a `blueprint` member at runtime, because
 * both are part of the version-controlled company definition. Rendering the
 * two alike would offer controls that always fail.
 */
export type Provenance = "blueprint" | "overlay";

/** A seat: one teammate on one desk. Level 3, the leaves of the chart. */
export interface OrgSeat {
  /** The teammate id, as the desk names them. */
  id: string;
  /** Display name; falls back to the role, then to the raw id. */
  name: string;
  /** The teammate's role, or `""` when they resolve to no roster entry. */
  role: string;
  /**
   * The face to draw: this teammate's chosen avatar, else the mascot hashed
   * from the id (`lib/avatar.ts`).
   *
   * Carried on the seat rather than recomputed at the node, so a teammate whose
   * icon somebody set wears it on the chart too — a chart that hashed its own
   * face would be the one surface still showing the old one.
   */
  avatar: string;
  /**
   * Whether this seat leads the desk. True for exactly one seat per non-empty
   * desk — `DeskDto.members[0]`, which is the host's routing target.
   */
  lead: boolean;
  /** Blueprint members cannot be removed at runtime; overlay members can. */
  provenance: Provenance;
  /**
   * Whether the id resolved to a roster teammate.
   *
   * A desk can name somebody the roster no longer has — an overlay teammate
   * dropped, or a manifest edited under a running host. The chat member pane
   * drops those ids silently, which is right for a chat: you cannot message
   * a row that is nobody. It is wrong *here*. An org chart's whole job is to
   * say what the structure is, and a desk seat pointing at nobody is a fact
   * about the structure that the operator is the only one who can fix. So the
   * seat stays, flagged, rather than quietly disappearing.
   */
  known: boolean;
}

/** A desk: a team. Level 2. */
export interface OrgDesk {
  id: string;
  name: string;
  description?: string;
  /** Blueprint desks cannot be deleted at runtime; overlay desks can. */
  provenance: Provenance;
  /** Its seats, lead first — the host's own member order, never re-sorted. */
  seats: OrgSeat[];
}

/** A human who can sign in. Listed, never placed — see `OrgTree.people`. */
export interface OrgPerson {
  id: string;
  /** Their display name, or the local part of their address. */
  name: string;
  email: string;
  role: string;
}

/** The whole chart. Level 1 is the company itself. */
export interface OrgTree {
  /** The company's display name, or its id when the host names it nothing. */
  companyName: string;
  desks: OrgDesk[];
  /**
   * Roster teammates who sit on no desk at all.
   *
   * Deliberately *not* nodes of the tree: they have no place in the hierarchy,
   * and inventing one for them is exactly the mistake the Overview graph makes
   * when it keyword-matches a role into a department. They are listed beside
   * the chart so the roster still adds up, and so an unplaced teammate is
   * visible rather than absent.
   */
  unassigned: TeamMember[];
  /**
   * The humans who can sign in.
   *
   * Also not nodes. The company declares desk membership for agents only, so
   * there is no honest answer to "which desk is this person on" — and guessing
   * one would be fiction presented as structure.
   */
  people: OrgPerson[];
  /** Every teammate the roster knows, for the add-to-desk picker. */
  roster: TeamMember[];
}

/**
 * Build the chart from what the host already serves: `GET {scope}/desks`,
 * `GET {scope}/team`, and `GET {scope}/users`.
 *
 * Pure and total. Every input is optional in practice — a host can 404 `/team`
 * or `/users` — so an absent roster yields seats that are `known: false` rather
 * than an empty chart, and absent users yield no people section.
 */
export function buildOrgTree(
  companyName: string,
  desks: DeskDto[],
  roster: TeamMemberDto[],
  people: OrgPerson[] = [],
): OrgTree {
  const members = roster.map(fromDto);
  const byId = new Map(members.map((m) => [m.id, m]));

  const orgDesks: OrgDesk[] = desks.map((desk) => {
    // `overlayMembers` is omitted rather than empty when there are none, which
    // is why this reads through `?? []` instead of trusting the field.
    const overlay = new Set(desk.overlayMembers ?? []);
    return {
      id: desk.id,
      name: desk.name,
      description: desk.description,
      provenance: desk.overlayCreated ? "overlay" : "blueprint",
      seats: desk.members.map((id, index) => {
        const member = byId.get(id);
        return {
          id,
          name: member?.name ?? id,
          role: member?.role ?? "",
          // A seat the roster cannot resolve still gets a face — the hashed
          // default from its id — because a blank tile beside a flagged seat
          // reads as a rendering bug rather than as the missing teammate the
          // flag is there to report.
          avatar: member?.avatar ?? avatarFor(id),
          // The host's order carries the hierarchy: index 0 is the lead. Read
          // the position, never re-derive the lead by sorting or by name.
          // Unless the desk is an `auto` channel (issue #1835): there
          // `members[0]` carries no rank — the host's `desk_lead` is `None` by
          // definition — so no seat wears the crown.
          lead: index === 0 && desk.responder !== "auto",
          provenance: overlay.has(id) ? "overlay" : "blueprint",
          known: member !== undefined,
        };
      }),
    };
  });

  const seated = new Set(orgDesks.flatMap((d) => d.seats.map((s) => s.id)));

  return {
    companyName,
    desks: orgDesks,
    unassigned: members.filter((m) => !seated.has(m.id)),
    people,
    roster: members,
  };
}

/**
 * The deepest path in a built tree, in levels.
 *
 * Answers 1 for a company with no desks, 2 when every desk is empty, and 3 as
 * soon as one desk has a seat. It can never answer 4: there is no edge in
 * `OrgDesk` that could produce one.
 */
export function depthOf(tree: OrgTree): number {
  if (tree.desks.length === 0) return 1;
  return tree.desks.some((d) => d.seats.length > 0) ? 3 : 2;
}

/** Roster teammates not yet on this desk — what its add control may offer. */
export function addableTo(tree: OrgTree, desk: OrgDesk): TeamMember[] {
  const on = new Set(desk.seats.map((s) => s.id));
  return tree.roster.filter((m) => !on.has(m.id));
}

/**
 * Whether a seat can be dragged onto a *different* desk (issue #1227).
 *
 * Only its provenance decides this. A same-desk reorder only ever calls
 * `setDeskOrder`, which the host accepts for either provenance — but a
 * cross-desk move also has to call `removeDeskMember` on the source, and the
 * host refuses that for a blueprint seat: it is declared in the manifest, not
 * the overlay, so runtime cannot let go of it. Accepting the drop anyway
 * would only fail one step later, as a 409 the operator never asked to see;
 * this is the one predicate every drag/drop-over gate in the chart consults
 * before accepting a cross-desk drop, so the refusal is decided once and
 * shown up front — a "not allowed" cursor, or a toast — rather than
 * discovered from a failed write.
 */
export function canDragAcrossDesks(seat: Pick<OrgSeat, "provenance">): boolean {
  return seat.provenance === "overlay";
}

/**
 * The desk's member order with `index` moved one place in `direction`, or
 * `null` when that would run off either end.
 *
 * Returned rather than applied so the caller decides when to write, and so the
 * "would this move anything" question has one answer in one place. Moving the
 * seat at index 1 up is also how the lead changes — there is no separate
 * set-lead call, because `members[0]` *is* the lead.
 */
export function reorderedIds(
  desk: OrgDesk,
  index: number,
  direction: "up" | "down",
): string[] | null {
  const target = direction === "up" ? index - 1 : index + 1;
  if (index < 0 || index >= desk.seats.length) return null;
  if (target < 0 || target >= desk.seats.length) return null;
  const ids = desk.seats.map((s) => s.id);
  [ids[index], ids[target]] = [ids[target], ids[index]];
  return ids;
}

/**
 * The full desk order produced by dropping one seat at another position.
 * Returning the host's complete list keeps drag-and-drop and the keyboard
 * controls on the same write contract: position zero remains the lead.
 */
export function reorderedIdsAfterDrop(
  desk: OrgDesk,
  fromIndex: number,
  toIndex: number,
): string[] | null {
  if (
    fromIndex < 0 ||
    fromIndex >= desk.seats.length ||
    toIndex < 0 ||
    toIndex >= desk.seats.length ||
    fromIndex === toIndex
  ) {
    return null;
  }
  const ids = desk.seats.map((seat) => seat.id);
  const [moved] = ids.splice(fromIndex, 1);
  ids.splice(toIndex, 0, moved);
  return ids;
}

/** A one-line summary of the chart, for the company node. */
export function summarize(tree: OrgTree): string {
  const seats = tree.desks.reduce((n, d) => n + d.seats.length, 0);
  const desks = plural(tree.desks.length, "desk");
  return `${desks} · ${plural(seats, "seat")}`;
}

function plural(n: number, noun: string): string {
  return `${n} ${noun}${n === 1 ? "" : "s"}`;
}
