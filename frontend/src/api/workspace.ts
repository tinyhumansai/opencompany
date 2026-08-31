// The live workspace API: the console reads and writes the company's real
// durable note tree through the host's `…/workspace` routes (REST, camelCase
// over the wire). Replaces the client-side `lib/workspace` localStorage stub —
// the same move `api/memory.ts` made for the Brain — so the operator and the
// agents finally look at one tree instead of two.
//
// Agents reach the same store through the `workspace_list` / `workspace_read` /
// `workspace_create` / `workspace_write` tools (issues #237, #551), which means
// a note an agent wrote or created shows up here and a note the operator writes
// here is readable by an agent on its next turn. Every node carries who created
// it and who last wrote it (issue #326) so the two are told apart on sight.

import { rosterDisplayName, type RosterNames } from "@/lib/roster-names";

import type { OpenCompanyClient } from "./client";

/** Whether a node is a folder or a file. */
export type NodeKind = "folder" | "file";

/**
 * Who authored a node — the host's `WorkspaceOrigin` (issue #326).
 *
 * `seed` is neither: it shipped with the company bundle and was typed by
 * nobody. `agentId` is present exactly when `kind` is `"agent"`.
 */
export type WorkspaceOrigin =
  { kind: "seed" } | { kind: "operator" } | { kind: "agent"; id: string };

/** The origin every node falls back to — what the host defaults a legacy node to. */
export const OPERATOR_ORIGIN: WorkspaceOrigin = { kind: "operator" };

/**
 * The lookup an `originLabel` caller passes when it has no roster to hand.
 *
 * A shared empty map rather than a fresh one per call: `rosterDisplayName`
 * only reads it, and this function is called once per rendered row.
 */
const NO_ROSTER_NAMES: RosterNames = new Map();

/**
 * A short human label for an origin, or `null` for a plain operator note.
 *
 * Mirrors `ORIGIN_LABELS` in `api/memory.ts`, but returns `null` rather than
 * "Operator" for the operator case: in the Brain every row has an interesting
 * origin, whereas here the operator is the unremarkable default and badging it
 * would put a chip on nearly every note while saying nothing.
 *
 * `names` resolves the agent case through the one shared
 * {@link rosterDisplayName} (issue #1723): the raw roster handle is engine
 * plumbing — `seo_specialist` where the operator knows "SEO Specialist" — and
 * this label sits beside names that are already resolved. Optional, and the
 * resolver falls back to the id, so a caller that has no roster read to hand
 * gets exactly the previous string rather than a blank badge.
 */
export function originLabel(
  origin: WorkspaceOrigin | undefined,
  names: RosterNames = NO_ROSTER_NAMES,
): string | null {
  switch (origin?.kind) {
    case "agent":
      return `Teammate · ${rosterDisplayName(origin.id, names)}`;
    case "seed":
      return "Seeded";
    default:
      return null;
  }
}

/**
 * One node in the workspace tree, as the host returns it.
 *
 * The tree read carries metadata only — `content` is always absent there, and a
 * body is fetched per file by {@link fetchFile}. `parentId` is normalized to
 * `null` at the workspace root (the host omits the field entirely).
 */
export interface FsNode {
  id: string;
  name: string;
  kind: NodeKind;
  /** null = workspace root. */
  parentId: string | null;
  /** Markdown body. Only ever set on a node built from a file read. */
  content?: string;
  /** Epoch-millis of the last update. */
  updatedAt: number;
  /** Who created this node. Never changes. */
  createdBy: WorkspaceOrigin;
  /** Who last wrote this node's body. A rename or move does not change it. */
  updatedBy: WorkspaceOrigin;
  /**
   * The media type of a **binary** node's payload, e.g. `image/png` (#553).
   *
   * Present only on a binary node, and the single test for one — the host omits
   * all three fields below on a prose note rather than sending nulls, so
   * `mime !== undefined` means "render or download this instead of editing it".
   */
  mime?: string;
  /** The payload's exact size in bytes. Host-computed. */
  size?: number;
  /** The payload's sha256, computed by the store from the stored bytes. */
  sha256?: string;
}

/** Whether a node holds bytes rather than prose. */
export function isBinary(node: Pick<FsNode, "mime">): boolean {
  return node.mime !== undefined && node.mime !== null;
}

/** One file's body plus the notes that link to it, from `GET …/workspace/file/{id}`. */
export interface WorkspaceFile {
  id: string;
  name: string;
  content: string;
  updatedAt: number;
  createdBy: WorkspaceOrigin;
  updatedBy: WorkspaceOrigin;
  /** Other files whose content links to this one via `[[name]]` — computed by the host. */
  backlinks: FsNode[];
}

/**
 * The wire shape: `parentId` is omitted at the root rather than sent as null,
 * and the two origins are optional purely for rollout skew — a console served
 * by a host that predates issue #326 gets neither field, and defaulting is
 * cheaper than a blank badge.
 */
interface FsNodeWire extends Omit<
  FsNode,
  "parentId" | "createdBy" | "updatedBy"
> {
  parentId?: string | null;
  createdBy?: WorkspaceOrigin;
  updatedBy?: WorkspaceOrigin;
}

/**
 * Normalizes a node off the wire. The host omits `parentId` at the workspace
 * root (`skip_serializing_if = "Option::is_none"`), and every tree query in the
 * view keys off `parentId === null`, so an absent field becomes an explicit
 * null exactly once — here — rather than at each call site. The origins get the
 * same treatment against an older host: absent means operator, which is the
 * same default the Rust port applies to a node written before the field
 * existed.
 */
function normalize(node: FsNodeWire): FsNode {
  return {
    ...node,
    parentId: node.parentId ?? null,
    createdBy: node.createdBy ?? OPERATOR_ORIGIN,
    updatedBy: node.updatedBy ?? OPERATOR_ORIGIN,
  };
}

/** Every node in the company's workspace (metadata only; no bodies). */
export async function fetchTree(
  client: OpenCompanyClient,
  company: string | null,
): Promise<FsNode[]> {
  const nodes = await client.get<FsNodeWire[]>(
    `${client.scopeFor(company)}/workspace`,
  );
  return nodes.map(normalize);
}

/** One file's content and its server-computed backlinks. 404s on a folder id. */
export async function fetchFile(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
): Promise<WorkspaceFile> {
  const file = await client.get<
    Omit<WorkspaceFile, "backlinks" | "createdBy" | "updatedBy"> & {
      backlinks: FsNodeWire[];
      createdBy?: WorkspaceOrigin;
      updatedBy?: WorkspaceOrigin;
    }
  >(`${client.scopeFor(company)}/workspace/file/${encodeURIComponent(id)}`);
  return {
    ...file,
    createdBy: file.createdBy ?? OPERATOR_ORIGIN,
    updatedBy: file.updatedBy ?? OPERATOR_ORIGIN,
    backlinks: file.backlinks.map(normalize),
  };
}

/**
 * One search hit from `GET …/workspace/search` (issue #607).
 *
 * A hit is a node plus the two things only a search knows: where it sits, and
 * why it came back. `path` matters because the tree view derives location from
 * `parentId` by walking the tree, and a flat hit list has no tree to walk.
 */
export interface SearchHit extends FsNode {
  /** The node's logical path, e.g. `standards/Engineering.md`. */
  path: string;
  /** Whether the query matched the node's name or its body. */
  matched: "name" | "content";
  /**
   * Text around the first body match.
   *
   * Absent for a name match, a folder, and a binary node — the host never
   * excerpts a payload.
   */
  excerpt?: string;
}

/** A page of search hits plus how many matched in total. */
export interface SearchResults {
  hits: SearchHit[];
  /** Matches before the limit — so the console can say "20 of 137". */
  total: number;
}

/**
 * Which notes mention `query`, matched case-insensitively as a substring of
 * note names and note bodies.
 *
 * The host refuses an empty query with a 400 rather than treating it as "match
 * everything", so callers must not send one — a cleared search box shows the
 * tree again instead of fetching every note. {@link searchWorkspace} does not
 * guard that itself: swallowing it here would hide a caller bug behind a silent
 * empty result.
 */
export async function searchWorkspace(
  client: OpenCompanyClient,
  company: string | null,
  query: string,
  options?: { prefix?: string; limit?: number },
): Promise<SearchResults> {
  const params = new URLSearchParams({ q: query });
  if (options?.prefix) params.set("prefix", options.prefix);
  if (options?.limit !== undefined) params.set("limit", String(options.limit));
  const results = await client.get<{
    hits: (FsNodeWire & {
      path: string;
      matched: "name" | "content";
      excerpt?: string;
    })[];
    total: number;
  }>(`${client.scopeFor(company)}/workspace/search?${params.toString()}`);
  return {
    total: results.total,
    // Destructured so the node half goes through the same {@link normalize} the
    // tree read uses — spreading the raw hit back over it afterwards would undo
    // the `parentId` and origin defaults it just applied.
    hits: results.hits.map(({ path, matched, excerpt, ...node }) => ({
      ...normalize(node),
      path,
      matched,
      excerpt,
    })),
  };
}

/**
 * Split `text` into alternating non-matching / matching runs for `query`,
 * so a hit's excerpt can bold what matched.
 *
 * Case-insensitive, like the host's own matching, and it returns the **original**
 * text in every run rather than the lowercased comparison copy — highlighting
 * must not silently rewrite the operator's prose to lower case.
 *
 * An empty query yields one unmatched run, which is what keeps a cleared box
 * from marking every character.
 */
export function highlightRuns(
  text: string,
  query: string,
): { text: string; hit: boolean }[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return [{ text, hit: false }];
  const haystack = text.toLowerCase();
  const runs: { text: string; hit: boolean }[] = [];
  let at = 0;
  // `indexOf` on the lowercased copy is safe here in a way it would not be in
  // Rust: JavaScript indexes strings by UTF-16 code unit and `toLowerCase` is
  // length-preserving for every character the browser will render in a note
  // title, so the two strings stay aligned.
  for (
    let found = haystack.indexOf(needle);
    found !== -1;
    found = haystack.indexOf(needle, at)
  ) {
    if (found > at) runs.push({ text: text.slice(at, found), hit: false });
    runs.push({ text: text.slice(found, found + needle.length), hit: true });
    at = found + needle.length;
  }
  if (at < text.length) runs.push({ text: text.slice(at), hit: false });
  return runs;
}

/**
 * The most matches one search can return (issue #1457).
 *
 * Mirrors the host's `MAX_SEARCH_RESULTS` (`src/company/workspace_search.rs`).
 * The host's *default* is 20 and its ceiling is 50, and `clamp_limit` clamps
 * rather than refusing — so the console naming no limit at all was what capped
 * every search at 20 while the header truthfully reported "20 of 50 matches"
 * and offered no way to reach the other 30. There is no offset on the route, so
 * 50 is a genuine hard ceiling; past it the honest remedy is a narrower query,
 * which is what the foot of the list now says.
 */
export const SEARCH_LIMIT = 50;

/**
 * Slide an excerpt so its first match is near the front (issue #1375).
 *
 * The host returns a window of context around the match, and the console
 * renders it into a `line-clamp-2` paragraph about 250px wide. When the match
 * sits past the first dozen or so words the browser clamps *before* reaching
 * it, so the operator gets two lines of arbitrary mid-file prose and no visible
 * highlight — the one thing the excerpt exists to show.
 *
 * Pure and conservative: an early match is returned untouched (no leading `…`
 * on text that was already fine), and a query that does not appear at all is
 * left exactly as the host sent it. Cutting is done at a word boundary where
 * one is near, so the result does not open mid-word.
 */
export function centerExcerpt(
  excerpt: string,
  query: string,
  budget = 24,
): string {
  const needle = query.trim().toLowerCase();
  if (!needle) return excerpt;
  const at = excerpt.toLowerCase().indexOf(needle);
  if (at === -1 || at <= budget) return excerpt;
  let cut = at - budget;
  // Prefer the next space, so the excerpt starts on a whole word — but only if
  // one is close, or a line with no spaces would be shifted arbitrarily far.
  const space = excerpt.indexOf(" ", cut);
  if (space !== -1 && space < at && space - cut < budget) cut = space + 1;
  return `…${excerpt.slice(cut)}`;
}

/** Create a folder or file. The host mints the id and the timestamp. */
export async function createNode(
  client: OpenCompanyClient,
  company: string | null,
  input: {
    name: string;
    kind: NodeKind;
    parentId?: string | null;
    content?: string;
  },
): Promise<FsNode> {
  const node = await client.post<FsNodeWire>(
    `${client.scopeFor(company)}/workspace`,
    input,
  );
  return normalize(node);
}

/** Overwrite a file's content; returns the new last-updated stamp. */
export function writeFile(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
  content: string,
): Promise<{ updatedAt: number }> {
  return client.put<{ updatedAt: number }>(
    `${client.scopeFor(company)}/workspace/file/${encodeURIComponent(id)}`,
    { content },
  );
}

/**
 * Rename and/or move a node, returning the authoritative node.
 *
 * The `parentId` contract is a double option and the distinction is load-bearing:
 * **omit** the key to leave the parent alone (a pure rename), and pass an
 * explicit `null` to move the node back to the workspace root. `JSON.stringify`
 * drops `undefined` properties but keeps `null`, so building the body
 * conditionally — as below — maps straight onto the server's
 * `Option<Option<String>>`. Spreading `{ parentId }` unconditionally would turn
 * every rename into a move-to-root.
 *
 * The move-cycle guard (a folder into its own descendant) is the store's, not
 * the console's: it answers 400 `invalid_request`.
 */
export async function renameMoveNode(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
  changes: { name?: string; parentId?: string | null },
): Promise<FsNode> {
  const body: { name?: string; parentId?: string | null } = {};
  if (changes.name !== undefined) body.name = changes.name;
  if ("parentId" in changes) body.parentId = changes.parentId ?? null;
  const node = await client.patch<FsNodeWire>(
    `${client.scopeFor(company)}/workspace/${encodeURIComponent(id)}`,
    body,
  );
  return normalize(node);
}

/** Delete a node; folders go recursively. */
export function deleteNode(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
): Promise<void> {
  return client.del<void>(
    `${client.scopeFor(company)}/workspace/${encodeURIComponent(id)}`,
  );
}

/** One folder the sweep removed, or would remove (issue #700). */
export interface SweptFolder {
  id: string;
  name: string;
}

/**
 * The sweep's answer. Exactly one list is present, and **which** one is what
 * actually happened: a preview answers `wouldRemove`, a real run answers
 * `removed`.
 */
interface SweepResult {
  wouldRemove?: SweptFolder[];
  removed?: SweptFolder[];
}

/**
 * Remove the empty `agents/<id>/` folders a pre-#570 company still carries
 * (issue #700), or — with `dryRun` — find out which ones those are without
 * touching anything.
 *
 * The two calls are the same request twice: the console previews, names every
 * folder on a confirm dialog, and only then asks for the deletion. Nothing here
 * decides emptiness; the host counts children structurally, over every node in
 * the tree, because a folder whose only child has no renderable path reads as
 * empty to anything that goes by paths while the store's recursive delete would
 * still take it (issue #671).
 *
 * Reads the field that matches what was asked for, rather than whichever list
 * turns up. If the host ever disagrees with this caller about `dryRun`, an
 * absent field yields an empty list — so a preview cannot render as "17 folders
 * deleted", and a real run cannot be mistaken for a preview.
 */
export async function sweepEmptyAgentFolders(
  client: OpenCompanyClient,
  company: string | null,
  dryRun: boolean,
): Promise<SweptFolder[]> {
  const result = await client.post<SweepResult>(
    `${client.scopeFor(company)}/workspace/sweep-empty-agent-folders?dry_run=${dryRun}`,
  );
  return (dryRun ? result.wouldRemove : result.removed) ?? [];
}

/** One node the duplicate-folder repair relocated, or would (issue #759). */
export interface MovedChild {
  id: string;
  name: string;
}

/** One duplicate folder folded into its surviving twin (issue #759). */
export interface MergedFolder {
  id: string;
  name: string;
  intoId: string;
  moved: MovedChild[];
  /** Whether the emptied folder itself went — `false` if anything is still in it. */
  removed: boolean;
}

/** Why the repair left a node exactly where it found it. */
export type ResidualCause =
  "fileSharesTheName" | "fileInTheWay" | "treeMovedOn" | "danglingParent";

/** One node the repair deliberately did not touch (issue #759). */
export interface Residual {
  id: string;
  name: string;
  parentId?: string;
  cause: ResidualCause;
}

/** What the repair did, or would do. */
export interface RepairOutcome {
  folders: MergedFolder[];
  residuals: Residual[];
}

/**
 * The repair's answer. Exactly one fold list is present — a preview answers
 * `wouldMerge`, a real run answers `merged` — while `residuals` is on both.
 */
interface RepairResult {
  wouldMerge?: MergedFolder[];
  merged?: MergedFolder[];
  residuals?: Residual[];
}

/**
 * What an operator has to do by hand, in one sentence each.
 *
 * Written as instructions rather than as the host's enum, because the residual
 * list is the part of the answer that says the tree is *not* fixed yet. A row
 * reading `fileInTheWay` tells an operator nothing about what to do with it.
 */
export function residualReason(cause: ResidualCause): string {
  switch (cause) {
    case "fileSharesTheName":
      return "A note and a folder share this name — rename or remove one of them.";
    case "fileInTheWay":
      return "Both copies hold a note with this name. Merging them would discard one, so both were kept — open them and keep what you want.";
    case "treeMovedOn":
      return "Something changed while the repair ran, so this was left alone. Run it again.";
    case "danglingParent":
      return "The folder this was filed under no longer exists, so it has no reachable path. Move it somewhere that does, or delete it if you don't need it.";
  }
}

/**
 * Merge the duplicate sibling folders a publish race left behind (issue #759),
 * or — with `dryRun` — find out what that would do without touching anything.
 *
 * The two calls are the same request twice: the console previews, names every
 * folder that gives way and every note that changes hands, and only then asks
 * for the change. Nothing here decides what merges; the host does, and it
 * refuses to decide a collision between two *files* because picking one would
 * silently discard a document.
 *
 * Reads the fold list that matches what was asked for rather than whichever one
 * turns up, so a preview can never render as "3 folders merged". `residuals` is
 * read unconditionally — it is the half of the answer that says whether the tree
 * is actually repaired, and defaulting it away would turn "two documents still
 * on one path" into silence.
 */
export async function mergeDuplicateFolders(
  client: OpenCompanyClient,
  company: string | null,
  dryRun: boolean,
): Promise<RepairOutcome> {
  const result = await client.post<RepairResult>(
    `${client.scopeFor(company)}/workspace/merge-duplicate-folders?dry_run=${dryRun}`,
  );
  return {
    folders: (dryRun ? result.wouldMerge : result.merged) ?? [],
    residuals: result.residuals ?? [],
  };
}

/**
 * Upload a file of any kind (issue #553).
 *
 * `createNode` sends a JSON body, which cannot carry bytes, so an image or a
 * PDF has to arrive as `multipart/form-data` on its own route. The host — not
 * this function — decides whether the result is a note or a binary node: a file
 * that is typed as text *and* decodes as UTF-8 becomes a prose note, so an
 * uploaded `.md` keeps its editor, its backlinks and its diffable history.
 *
 * `fetch` directly rather than `client.post`: the shared request helper sets a
 * JSON content-type and `JSON.stringify`s its body, and a multipart upload must
 * let the browser set the boundary itself.
 */
export async function uploadFile(
  client: OpenCompanyClient,
  company: string | null,
  file: File,
  parentId?: string | null,
): Promise<FsNode> {
  const form = new FormData();
  form.append("file", file, file.name);
  if (parentId) form.append("parentId", parentId);
  const node = await client.postForm<FsNodeWire>(
    `${client.scopeFor(company)}/workspace/upload`,
    form,
  );
  return normalize(node);
}

/**
 * Fetch a binary node's payload as an object URL.
 *
 * A plain `<img src="…/workspace/blob/{id}">` would not work: the route needs
 * the bearer token the client holds, and an image element cannot carry one. So
 * the bytes are fetched through the authenticated client and wrapped in an
 * object URL the element can point at.
 *
 * **The caller must revoke the returned URL** when it is done with it —
 * `URL.revokeObjectURL` — or the blob stays resident for the life of the
 * document.
 *
 * An optional `signal` cancels the transfer: a preview that scrolls out of
 * view aborts its fetch instead of downloading the whole payload only to
 * discard it (codex review finding).
 */
export async function fetchBlobUrl(
  client: OpenCompanyClient,
  company: string | null,
  id: string,
  signal?: AbortSignal,
): Promise<string> {
  const blob = await client.getBlob(
    `${client.scopeFor(company)}/workspace/blob/${encodeURIComponent(id)}`,
    signal,
  );
  return URL.createObjectURL(blob);
}

/**
 * Identifies the *bytes* a binary node currently holds (issue #669).
 *
 * The viewer fetches a payload once and holds an object URL for it, so it needs
 * to know when that URL has gone stale. The node id alone cannot say: a
 * re-publish overwrites a payload **in place** and deliberately keeps the id, so
 * an operator watching an image while an agent regenerated it kept seeing — and
 * downloading — the old bytes until they navigated away and back.
 *
 * `sha256` is the answer because it *is* "these are different bytes", which is
 * also why the blob route serves it as the `ETag`. The two obvious alternatives
 * are both wrong in a way that shows up rarely enough to be nasty: `size` misses
 * a same-length rewrite, and `updatedAtMillis` moves on a rename that changed no
 * bytes at all, forcing a needless refetch of a payload that may be 60 MB.
 *
 * This exists as a named function rather than as entries in a `useEffect`
 * dependency array because a dependency array is exactly the kind of thing a
 * later edit shortens without noticing, and because the rule above is worth
 * asserting in a test.
 *
 * A node with no digest falls back to the id, which is the pre-#553 behaviour
 * and correct: nothing here can detect a change the host did not report.
 */
export function blobCacheKey(node: Pick<FsNode, "id" | "sha256">): string {
  return node.sha256 ? `${node.id}:${node.sha256}` : node.id;
}

/** A human-readable size, for the metadata a binary node shows instead of a body. */
export function formatBytes(bytes: number | undefined): string {
  if (bytes === undefined) return "unknown size";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024)
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}
