/**
 * The Workspace header's count is notes, not files (issue #1763).
 *
 * `PageHeader`'s `count` rides inline with the title, so on Workspace it
 * answers "how many notes does this company hold" beside a description that
 * says "every note this company's teammates can read and write".
 *
 * The trap is that `NodeKind` is only `"folder" | "file"`: an uploaded image is
 * a `file` too, and is told apart from prose by `mime` alone (`isBinary`,
 * #553). A count that filtered on `kind === "file"` therefore reported a
 * workspace holding one screenshot and no prose as "1 note" — a number that is
 * wrong in the one direction nobody checks, because it is never zero and never
 * absurd. `isBinary` is already the console's single test for this everywhere
 * else in the pane, so the count asks the same question.
 */
import { describe, expect, it } from "vitest";

import { OPERATOR_ORIGIN, type FsNode } from "@/api/workspace";
import { countNotes, headerNoteCount } from "@/lib/workspace";

/** A prose note: the host omits `mime`/`size`/`sha256` on one entirely. */
function note(id: string, name: string, parentId: string | null = null): FsNode {
  return {
    id,
    name,
    kind: "file",
    parentId,
    updatedAt: 0,
    createdBy: OPERATOR_ORIGIN,
    updatedBy: OPERATOR_ORIGIN,
  };
}

/** A binary node: same `kind`, and `mime` is the only thing that says so. */
function asset(id: string, name: string, mime: string, parentId: string | null = null): FsNode {
  return { ...note(id, name, parentId), mime, size: 1024, sha256: "0".repeat(64) };
}

function folder(id: string, name: string, parentId: string | null = null): FsNode {
  return {
    id,
    name,
    kind: "folder",
    parentId,
    updatedAt: 0,
    createdBy: OPERATOR_ORIGIN,
    updatedBy: OPERATOR_ORIGIN,
  };
}

describe("the Workspace header's note count", () => {
  it("counts prose notes", () => {
    expect(countNotes([note("a", "Brief.md"), note("b", "Notes.md")])).toBe(2);
  });

  it("does not count an uploaded image as a note", () => {
    const tree = [folder("f", "Assets"), asset("i", "screenshot.png", "image/png", "f")];
    expect(countNotes(tree)).toBe(0);
  });

  it("counts only the prose in a tree holding both", () => {
    const tree = [
      folder("f", "Assets"),
      note("a", "Brief.md"),
      asset("i", "screenshot.png", "image/png", "f"),
      asset("p", "contract.pdf", "application/pdf", "f"),
    ];
    expect(countNotes(tree)).toBe(1);
  });

  it("does not count folders, which are how the tree is arranged", () => {
    expect(countNotes([folder("f", "Assets"), folder("g", "Standards")])).toBe(0);
  });

  it("is zero for an empty workspace, so a zero reads as a zero", () => {
    expect(countNotes([])).toBe(0);
  });
});

/**
 * A count is a claim, and the header must not make one it cannot support.
 *
 * `nodes` starts empty with `loading` true, so the header put an authoritative
 * `0` beside the title on every fresh visit before the tree request settled —
 * and went on reporting zero next to a load error, describing a workspace
 * nobody had managed to read. `PageHeader` omits the badge for `undefined`
 * exactly so "no notes yet" and "not counting" stay different claims.
 */
describe("the header count while the tree is unknown", () => {
  it("says nothing before a tree has ever loaded", () => {
    expect(headerNoteCount(0, false)).toBeUndefined();
    // Not even a count it happens to have: unknown is unknown.
    expect(headerNoteCount(7, false)).toBeUndefined();
  });

  it("reports an authoritative empty workspace as zero", () => {
    expect(headerNoteCount(0, true)).toBe(0);
  });

  it("keeps the last known count rather than retracting it", () => {
    // A non-silent refresh raises `loading` over a tree already on screen, and
    // a failed one leaves `error` set; neither un-knows the tree, so blanking
    // the badge in either case would be a flicker rather than honesty.
    expect(headerNoteCount(7, true)).toBe(7);
  });
});
