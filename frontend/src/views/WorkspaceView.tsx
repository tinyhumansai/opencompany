import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import {
  ChevronDown,
  ChevronRight,
  ChevronsDownUp,
  EyeOff,
  FilePlus2,
  FileText,
  FileX,
  Folder,
  FolderOpen,
  FolderPlus,
  FolderSync,
  FolderX,
  Link2,
  Lock,
  Loader2,
  MoreHorizontal,
  PanelLeft,
  Download,
  RefreshCw,
  Search,
  Upload,
  X,
} from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { toast } from "sonner";

import { ApiError } from "@/api/types";
import type { OpenCompanyClient } from "@/api/client";
import {
  blobCacheKey,
  createNode,
  deleteNode as deleteNodeApi,
  fetchBlobUrl,
  fetchFile,
  fetchTree,
  formatBytes,
  isBinary,
  mergeDuplicateFolders,
  originLabel,
  residualReason,
  renameMoveNode,
  searchWorkspace,
  SEARCH_LIMIT,
  sweepEmptyAgentFolders,
  uploadFile,
  writeFile,
  OPERATOR_ORIGIN,
  type SearchHit,
  type SearchResults as SearchResultsPage,
  type RepairOutcome,
  type SweptFolder,
  type WorkspaceFile,
  type WorkspaceOrigin,
} from "@/api/workspace";
import { cachedAvatarNodeIds, forgetAvatarNode } from "@/lib/avatar";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription } from "@/components/ui/alert";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { Skeleton } from "@/components/ui/skeleton";
import {
  rosterDisplayName,
  rosterIdKey,
  rosterNameMap,
  type RosterNames,
} from "@/lib/roster-names";
import { fromDto } from "@/lib/team";
import { cn } from "@/lib/utils";
import { createSaveBuffer, createUnloadGuard, type SaveBuffer } from "@/lib/workspace-save-buffer";
import {
  ancestorFolderIds,
  applyRepair,
  breadcrumbOf,
  childrenOf,
  clearLegacyLocal,
  countNotes,
  declineLegacyImport,
  DERIVED_LABEL,
  DERIVED_REASON,
  ensureMdExt,
  fileByTitle,
  folderPathLabel,
  type FsNode,
  hasLegacyLocal,
  hasOperatorContent,
  isAgentsFolder,
  isRosterRoot,
  isDerivedNode,
  isSecretNode,
  legacyImportDeclined,
  type MoveAudienceChange,
  type MoveAudienceWarning,
  moveAudienceWarning,
  nodeById,
  pathOf,
  readLegacyLocalNodes,
  renameAudienceWarning,
  rosterOwnerOf,
  SECRETS_LABEL,
  SECRETS_REASON,
  sortedFolders,
  subtreeCounts,
  subtreeIds,
  titleOf, headerNoteCount } from "@/lib/workspace";
import { useLocalScope } from "@/connections/ConnectionContext";
import { MoveAudienceConfirm } from "@/views/workspace/MoveAudienceConfirm";
import { SearchResults } from "@/views/workspace/SearchResults";

/**
 * The latest workspace write off the SSE feed (issue #327), as the shell hands
 * it down.
 *
 * `tick` is what makes this a stream of *events* rather than a piece of state:
 * two frames naming the same node in one React batch would otherwise collapse
 * into one object React considers unchanged, and the second write would never
 * be reacted to.
 */
export interface WorkspaceEvent {
  /** Monotonic, bumped per frame. */
  tick: number;
  /** The node that moved. */
  nodeId: string;
  /** `opened` | `updated` | `removed`, widened for a newer host's vocabulary. */
  change: string;
}

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * The latest write anywhere in this company's tree (issue #327). `null` until
   * one arrives, and on a host with no `/events` route — where this view keeps
   * exactly its old refresh-and-refocus behaviour.
   */
  event?: WorkspaceEvent | null;
  /** Bumped when incremental event delivery cannot be trusted (#1011). */
  refreshTick?: number;
  /**
   * A node to open on arrival (issue #552), from the `#/workspace/<nodeId>`
   * hash segment the Artifacts tab's "Open in workspace" link sets.
   *
   * Unvalidated, as `useHashView` documents: an id that names nothing resolves
   * against the host and simply reports that the note could not be opened,
   * which is the same thing a stale bookmark does.
   */
  initialNodeId?: string | null;
}

/** How long typing settles before the editor pushes a save to the host. */
const AUTOSAVE_DELAY_MS = 800;

/** Browser-local width of the workspace explorer (issue #1755). */
const WORKSPACE_LIST_WIDTH_KEY = "oc.workspace.listWidth";
const DEFAULT_WORKSPACE_LIST_WIDTH = 256;
const MIN_WORKSPACE_LIST_WIDTH = 200;
const MAX_WORKSPACE_LIST_WIDTH = 560;
const WORKSPACE_LIST_KEYBOARD_STEP = 16;

function clampWorkspaceListWidth(width: number): number {
  return Math.min(MAX_WORKSPACE_LIST_WIDTH, Math.max(MIN_WORKSPACE_LIST_WIDTH, width));
}

/** Restore a usable saved width without letting blocked storage break the view. */
function initialWorkspaceListWidth(): number {
  if (typeof window === "undefined") return DEFAULT_WORKSPACE_LIST_WIDTH;
  try {
    const saved = Number(window.localStorage.getItem(WORKSPACE_LIST_WIDTH_KEY));
    return Number.isFinite(saved) && saved > 0
      ? clampWorkspaceListWidth(saved)
      : DEFAULT_WORKSPACE_LIST_WIDTH;
  } catch {
    return DEFAULT_WORKSPACE_LIST_WIDTH;
  }
}

/**
 * How long the search box waits after the last keystroke before asking the host
 * (issue #607).
 *
 * The host's search is an O(N) scan over every note in the company, so a request
 * per keystroke would put the whole tree through it several times for one word.
 * Long enough to collapse a typed word into one call, short enough that the
 * results still feel like they belong to what is on screen.
 */
const SEARCH_DEBOUNCE_MS = 250;

/** The folder created to hold notes rescued from the retired local scratchpad. */
const IMPORT_FOLDER_NAME = "imported-from-this-browser";

/**
 * The body of the import receipt, for a scratchpad of `files` notes and
 * `folders` folders (issue #500).
 *
 * Names *both* categories rather than reporting one number, because the
 * scratchpad is a flat list of two kinds and no single label is honest for
 * every mix of them: calling the whole list "notes" over-reports a mixed
 * import, and counting only files makes a folder-only import announce
 * "0 notes" — a success that reads as a failure and still never mentions the
 * folders that did arrive. The common files-only scratchpad renders exactly
 * as it always did. The `IMPORT_FOLDER_NAME` root is packaging, not imported
 * content, and is deliberately outside this tally.
 */
export function importSummary(files: number, folders: number): string {
  const parts: string[] = [];
  if (files > 0) parts.push(`${files} note${files === 1 ? "" : "s"}`);
  if (folders > 0) parts.push(`${folders} folder${folders === 1 ? "" : "s"}`);
  // Unreachable from the banner, which only renders for a non-empty
  // scratchpad — but a receipt claiming an unqualified "Imported" would be the
  // worst possible reading of an import that moved nothing.
  if (parts.length === 0) return "nothing";
  return parts.join(" and ");
}

/**
 * The migration banner's sentence, for a pending scratchpad of `files` notes
 * and `folders` folders (issue #507).
 *
 * Shares [`importSummary`] with the post-import receipt rather than counting
 * again, because the banner and the receipt describe the *same* nodes one
 * moment apart: when they counted separately they drifted, and the banner
 * offered "3 notes" that the receipt then reported as "2 notes and 1 folder"
 * — the pre-import prompt left over-reporting after #500 fixed the receipt.
 *
 * The verb agrees with the **total node count**, not with the leading number
 * of the summary. "1 note and 1 folder" is two things and takes "are"; the
 * summary's own first word is "1". Passing `files + folders` is what keeps
 * that right for a mixed scratchpad.
 */
export function migrationBannerText(files: number, folders: number): string {
  const total = files + folders;
  return (
    `${importSummary(files, folders)} from this browser's old scratchpad ` +
    `${total === 1 ? "is" : "are"} not in the company workspace yet.`
  );
}

/**
 * What Import actually does, said before it is clicked (issue #1472).
 *
 * The banner used to offer "Import" and name neither half of the bargain: not
 * where the notes land, and not that the browser's copy is **removed** on the
 * way. It is a move, not a copy — `importLegacy` clears the scratchpad key on
 * success — and an operator who expected a backup to remain in the browser had
 * no way to learn otherwise until it was gone.
 */
export const MIGRATION_CONSEQUENCE =
  `Import files them under “${IMPORT_FOLDER_NAME}” in the company workspace ` +
  `and removes this browser's copy — it moves them rather than copying them.`;

/** What the editor's status line is currently reporting. */
export type SaveState = "idle" | "dirty" | "saving" | "saved" | "error";

/**
 * What the status line says for a state, or `null` for the states with nothing
 * to report (issue #1372).
 *
 * `dirty` exists because the line used to be **silent for the only window in
 * which the operator's words are at risk**. Typing set `idle`, `idle` rendered
 * nothing, and the first thing the header ever said was "Saved" — after the
 * risk had passed. An operator who typed a sentence and reloaded inside the
 * autosave debounce lost it, having been told nothing.
 *
 * So the two silent states are now the two honest silences: `idle` is an
 * untouched note, and everything the operator has typed is announced until the
 * host has acknowledged it.
 */
export function saveStatusLabel(state: SaveState): string | null {
  switch (state) {
    case "dirty":
      return "Unsaved";
    case "saving":
      return "Saving…";
    case "saved":
      return "Saved";
    case "error":
      return "Not saved — retrying on edit";
    default:
      return null;
  }
}

/**
 * Text the operator wrote into a note that no longer exists, held out of the
 * editor so it can be handed back (issue #552 review).
 *
 * The `name` is the vanished note's, kept only so the banner can say *which*
 * note this came out of — an operator with three tabs of notes open all week
 * cannot identify a loose paragraph otherwise.
 */
interface Rescued {
  name: string;
  content: string;
}

/** What a live write means for the open note, once the refreshed tree is in. */
export type OpenNotePlan =
  /** Nothing to do: the note is untouched, or the operator is mid-edit. */
  | { kind: "leave" }
  /** Re-read the open note's body — it changed underneath a reader. */
  | { kind: "reload" }
  /**
   * The open note no longer exists. `rescue` carries the operator's unsaved
   * text, or `null` when the buffer matched what the host already had.
   */
  | { kind: "vanished"; rescue: string | null };

/**
 * Decide what a `workspace_changed` frame means for the note in the pane.
 *
 * Split out of the effect because "is the open note still there?" is the whole
 * bug and it is not a question the *frame* can answer. `WorkspaceAnnouncer`
 * emits exactly one `removed` frame naming the node the operator deleted — the
 * **folder** — and never one per descendant, so a note three folders down
 * disappears without any frame ever saying its id. The effect used to compare
 * `event.nodeId` against `openId`, decide the frame was about somebody else,
 * and return: the note left the tree, the pane stayed open on it, and the
 * debounced autosave behind it went on to 404 against a node that was gone.
 *
 * So disappearance is read off the **refreshed tree**, which knows about every
 * descendant, and the frame is consulted only for the case the tree cannot
 * settle — a refetch that failed (`tree === null`) while the frame itself said
 * the open note was removed. When both are silent the note is treated as alive,
 * which is the safe direction: a pane wrongly closed loses the operator's
 * place, and there is no reading of the evidence here that says to close it.
 */
export function planOpenNote({
  openId,
  event,
  tree,
  mode,
  draft,
  saved,
}: {
  /** The note in the pane, or `null` when none is open. */
  openId: string | null;
  /** The frame that just arrived. */
  event: WorkspaceEvent;
  /** The tree as refetched *after* the frame, or `null` if that read failed. */
  tree: FsNode[] | null;
  /** Which tab the pane is on. */
  mode: "read" | "edit";
  /** The editor's buffer, or `null` when the operator is not editing. */
  draft: string | null;
  /** The body the host last acknowledged, as the pane holds it. */
  saved: string | null | undefined;
}): OpenNotePlan {
  if (!openId) return { kind: "leave" };
  const goneFromTree = tree !== null && !tree.some((n) => n.id === openId);
  const removedOutright = event.change === "removed" && event.nodeId === openId;
  if (goneFromTree || removedOutright) {
    return { kind: "vanished", rescue: unsavedDraft(draft, saved) };
  }
  // Not the open note's frame — the tree refresh above is the whole reaction.
  if (event.nodeId !== openId) return { kind: "leave" };
  // Never clobber an in-progress edit: in edit mode the operator has a dirty
  // buffer and an autosave in flight, and replacing the body underneath them
  // would discard typing no refetch can get back. They see the change when they
  // switch back to Read, which refetches anyway.
  return mode === "read" ? { kind: "reload" } : { kind: "leave" };
}

/**
 * The words that would be lost if the open note went away right now, or `null`
 * when the buffer says nothing the host does not already have.
 *
 * Compares against the acknowledged body rather than trusting the mere presence
 * of a draft: switching to Edit seeds the buffer from a fresh read, so a note
 * merely *opened* for editing and then deleted elsewhere has nothing to rescue
 * and should close quietly instead of pushing a banner at the operator.
 */
function unsavedDraft(draft: string | null, saved: string | null | undefined): string | null {
  if (draft === null) return null;
  return draft === (saved ?? "") ? null : draft;
}

/** Formats an epoch-millis instant for the open note's header. */
function formatUpdated(ms: number): string {
  if (!ms) return "—";
  return new Date(ms).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

/** Whether a failed request was a 404 (the note is gone on the host). */
function isNotFound(e: unknown): boolean {
  return e instanceof ApiError && e.status === 404;
}

function message(e: unknown, fallback: string): string {
  return e instanceof Error ? e.message : fallback;
}

/**
 * An Obsidian-style workspace: a file-tree explorer, a markdown note pane with
 * `[[wiki links]]`, and a backlinks panel — all of it the company's **real**
 * workspace, read and written on the host over `…/workspace` (issue #177).
 *
 * This used to be a localStorage scratchpad seeded with invented marketing
 * notes, so the operator's workspace and the one the agents read and write
 * (their `workspace_*` tools, issue #237) were two unrelated trees. Now there is
 * one: a note an agent writes shows up here on the next refresh, and a note
 * typed here is readable by an agent on its next turn.
 *
 * Writes are **apply-on-ack**, never optimistic — every mutation returns the
 * authoritative node, so local state is patched from the response and a failure
 * simply leaves the tree as it was. The editor is the one exception: it keeps a
 * local dirty buffer so typing is never blocked on (or lost to) the network.
 *
 * Two gaps are deliberate and tracked rather than worked around: notes carry no
 * authorship, so an agent's note is indistinguishable from the operator's
 * (#326), and there is no live push, so a write that lands while the tab is open
 * appears on refresh/refocus rather than instantly (#327).
 */
export function WorkspaceView({ client, company, event, refreshTick = 0, initialNodeId }: Props) {
  // Which (connection, company) this subtree's browser-local state belongs to.
  const scope = useLocalScope();
  const [nodes, setNodes] = useState<FsNode[]>([]);
  // Ref mirror used by async tree refreshes: comparing the last authoritative
  // tree with the new one must not make `loadTree` depend on state and replay
  // every live-write effect. It also lets remote deletions invalidate cached
  // uploaded faces, not only deletes initiated by this view.
  const nodesRef = useRef<FsNode[]>([]);
  const [loading, setLoading] = useState(true);
  /**
   * Has a tree ever actually loaded?
   *
   * Distinct from `!loading`, which a non-silent refresh sets back to `true`
   * over a tree already on screen, and from `nodes.length`, which cannot tell
   * "not fetched yet" from "fetched, and empty" — the two states the header
   * count has to keep apart (codex review on #1785).
   */
  const [treeKnown, setTreeKnown] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // The roster names the `agents/` folders resolve against (issue #973). Best
  // effort and never blocking: a host predating the roster route 404s, and the
  // tree simply falls back to the raw ids it has always shown.
  const [rosterNames, setRosterNames] = useState<RosterNames>(() => new Map());

  const [openId, setOpenId] = useState<string | null>(null);
  const [openFile, setOpenFile] = useState<WorkspaceFile | null>(null);
  const [fileError, setFileError] = useState<string | null>(null);

  const [mode, setMode] = useState<"read" | "edit">("read");
  const [draft, setDraft] = useState<string | null>(null);
  const [saveState, setSaveState] = useState<SaveState>("idle");
  // Text salvaged from a note that was deleted while the operator was writing
  // in it. Deliberately outside the editor's own state: `draft` belongs to
  // whatever note is open, and this text belongs to one that no longer is.
  const [rescued, setRescued] = useState<Rescued | null>(null);

  // Search (issue #607). `searchInput` is what the operator is typing;
  // `searchQuery` is the debounced value the results below actually answer.
  // Keeping them apart is what lets the header say "no notes mention X" about
  // the query that ran rather than about the half-word in the box.
  const [searchInput, setSearchInput] = useState("");
  const [searchQuery, setSearchQuery] = useState("");
  const [searchPage, setSearchPage] = useState<SearchResultsPage | null>(null);
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState<string | null>(null);

  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  /**
   * The node the explorer should bring into view next (issue #1371).
   *
   * Separate from `openId` because revealing is a one-shot *event*, not a piece
   * of state: scrolling on every render that happens to have a note open would
   * yank the tree back under an operator who had deliberately scrolled away
   * from it to look at something else.
   */
  const [revealId, setRevealId] = useState<string | null>(null);
  const onRevealed = useCallback(() => setRevealId(null), []);
  const [prompt, setPrompt] = useState<PromptState | null>(null);
  const [moving, setMoving] = useState<FsNode | null>(null);
  const [showExplorer, setShowExplorer] = useState(true);
  const [listWidth, setListWidth] = useState(initialWorkspaceListWidth);
  const [resizingList, setResizingList] = useState(false);
  const listResize = useRef<{
    pointerId: number;
    startX: number;
    startWidth: number;
  } | null>(null);
  const [legacy, setLegacy] = useState<FsNode[]>([]);
  // Whether this connection already said "not now" to the migration offer.
  // Read once per company rather than per render, because the answer lives in
  // localStorage and cannot change without this component having changed it.
  const [importDeclined, setImportDeclined] = useState(false);
  const [importing, setImporting] = useState(false);
  // Which of the two irreversible "Discard" buttons is waiting on a confirm
  // (issue #1472). Both destroy the last copy of something an operator typed,
  // so neither fires from its own click any more.
  const [confirmDiscard, setConfirmDiscard] = useState<DiscardTarget | null>(null);
  // The empty-agent-folder tidy (issue #700), in its two stages: `preview` is
  // what the host says *would* go, `done` is what actually went. Both name every
  // folder — a count is not something an operator who disagrees can check.
  const [sweep, setSweep] = useState<SweepState | null>(null);
  const [sweeping, setSweeping] = useState(false);
  // The duplicate-folder repair (issue #759), in the same two stages. `preview`
  // is the plan; `done` is what the host actually did — which is not always the
  // same list, and is never the whole story: the residuals it could not decide
  // ride along on both.
  const [repair, setRepair] = useState<RepairState | null>(null);
  const [repairing, setRepairing] = useState(false);
  // The node awaiting a second click before its delete API call goes out — a
  // folder recursively takes every note nested inside it, so the dialog must
  // name what is about to go before it goes (issue #1255).
  const [confirmDelete, setConfirmDelete] = useState<FsNode | null>(null);
  // The pending scratchpad partitioned by kind, once, for every surface that
  // describes or imports it: the banner's sentence, the import loops, and the
  // receipt. #500 partitioned inside `importLegacy` so the loops and the
  // receipt could not disagree; the banner counted the flat list separately
  // and drifted anyway (#507). One partition is what makes them agree.
  const legacyFolders = useMemo(() => legacy.filter((n) => n.kind === "folder"), [legacy]);
  const legacyFiles = useMemo(() => legacy.filter((n) => n.kind === "file"), [legacy]);
  const uploadRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    try {
      window.localStorage.setItem(WORKSPACE_LIST_WIDTH_KEY, String(listWidth));
    } catch {
      // Storage can be blocked by browser policy; resizing should still work
      // for the current visit.
    }
  }, [listWidth]);

  // Generation tokens so a response from a previous company scope (or from a
  // file that has since been closed) can never overwrite the current one.
  const treeGen = useRef(0);
  const fileGen = useRef(0);
  const searchGen = useRef(0);
  const rosterGen = useRef(0);
  // Whether the explorer's initial folder expansion has happened yet, so a later
  // refresh never re-opens folders the operator collapsed.
  const expandedSeeded = useRef(false);

  /* ---- tree ---- */

  // Resolves to the tree it just installed, or `null` when this read answered
  // nothing authoritative — it failed, or a newer read has already superseded
  // it. Callers that only want the side effect ignore it; the live-writes
  // effect needs the nodes themselves, because "did the open note survive this
  // write?" is a question only the refreshed list can answer and reading it
  // back out of `nodes` would race React's own state update.
  const loadTree = useCallback(
    async (opts?: { silent?: boolean }): Promise<FsNode[] | null> => {
      const mine = ++treeGen.current;
      if (opts?.silent) setRefreshing(true);
      else setLoading(true);
      try {
        const tree = await fetchTree(client, company);
        if (mine !== treeGen.current) return null;
        const nextIds = new Set(tree.map((node) => node.id));
        for (const previous of nodesRef.current) {
          if (!nextIds.has(previous.id)) forgetAvatarNode(client, company, previous.id);
        }
        // On a fresh mount there is no previous tree to diff — the ref starts
        // empty — so a face whose node was deleted while this view was
        // unmounted would otherwise stay cached for the life of the tab. The
        // module cache is revalidated against this authoritative tree once;
        // from the next load the diff above carries the job.
        if (nodesRef.current.length === 0) {
          for (const id of cachedAvatarNodeIds(client, company)) {
            if (!nextIds.has(id)) forgetAvatarNode(client, company, id);
          }
        }
        nodesRef.current = tree;
        setNodes(tree);
        setError(null);
        if (!expandedSeeded.current) {
          expandedSeeded.current = true;
          setExpanded(
            new Set(
              childrenOf(tree, null)
                .filter((n) => n.kind === "folder")
                .map((n) => n.id),
            ),
          );
        }
        setTreeKnown(true);
        return tree;
      } catch (e) {
        if (mine !== treeGen.current) return null;
        setError(message(e, "could not load the workspace"));
        return null;
      } finally {
        if (mine === treeGen.current) {
          setLoading(false);
          setRefreshing(false);
        }
      }
    },
    [client, company],
  );

  /* ---- roster names (#973) ---- */

  // Best effort, and deliberately separate from `loadTree`: a host with no
  // roster route (or a request that simply fails) must not stop the workspace
  // itself from loading — it only means the `agents/` folders keep showing raw
  // ids, exactly as they did before this issue.
  const loadRoster = useCallback(async () => {
    const mine = ++rosterGen.current;
    try {
      const team = await client.listTeam(company);
      if (mine !== rosterGen.current) return;
      setRosterNames(rosterNameMap(team.map(fromDto)));
    } catch {
      if (mine !== rosterGen.current) return;
      setRosterNames(new Map());
    }
  }, [client, company]);

  /* ---- search (#607) ---- */

  // Resolves when the results are installed. Separated from the effect below so
  // the refocus and live-write handlers can re-run the *active* search — a hit
  // list that outlived the notes it names is worse than a stale tree, because
  // clicking one 404s.
  const runSearch = useCallback(
    async (query: string) => {
      const mine = ++searchGen.current;
      setSearching(true);
      try {
        // Ask for the host's ceiling, not its default (issue #1457). The route
        // clamps rather than refusing, so naming no limit was the console
        // silently capping itself at 20 while the header said "20 of 50".
        const page = await searchWorkspace(client, company, query, {
          limit: SEARCH_LIMIT,
        });
        if (mine !== searchGen.current) return;
        setSearchPage(page);
        setSearchError(null);
      } catch (e) {
        if (mine !== searchGen.current) return;
        // The previous page is dropped rather than left standing: results that
        // do not answer the query on screen are a lie the operator cannot see.
        setSearchPage(null);
        setSearchError(message(e, "could not search this workspace"));
      } finally {
        if (mine === searchGen.current) setSearching(false);
      }
    },
    [client, company],
  );

  // Debounce the box into `searchQuery`.
  useEffect(() => {
    const trimmed = searchInput.trim();
    if (!trimmed) {
      // Clearing the box restores the tree immediately — no debounce, and no
      // request: the host refuses an empty query with a 400 because "" is not
      // "everything", so the console must not send one.
      searchGen.current++;
      setSearchQuery("");
      setSearchPage(null);
      setSearchError(null);
      setSearching(false);
      return;
    }
    const timer = setTimeout(() => setSearchQuery(trimmed), SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
  }, [searchInput]);

  useEffect(() => {
    if (!searchQuery) return;
    void runSearch(searchQuery);
  }, [searchQuery, runSearch]);

  // Mount / company change: reset every scoped piece of state, then load.
  useEffect(() => {
    expandedSeeded.current = false;
    nodesRef.current = [];
    setNodes([]);
    setOpenId(null);
    setOpenFile(null);
    setDraft(null);
    setSaveState("idle");
    // Another company's notes are another namespace; a hit list surviving the
    // switch would offer nodes this company does not have.
    searchGen.current++;
    setSearchInput("");
    setSearchQuery("");
    setSearchPage(null);
    setSearchError(null);
    // Another company's note is another namespace, and rescued text offering to
    // be saved into the wrong workspace is worse than no offer at all.
    setRescued(null);
    setExpanded(new Set());
    // Another company's roster is another set of names; carrying the old map
    // across a switch would resolve this company's ids against that one's
    // teammates.
    setRosterNames(new Map());
    void loadTree();
    void loadRoster();
    return () => {
      treeGen.current++;
      fileGen.current++;
      rosterGen.current++;
    };
  }, [loadTree, loadRoster]);

  /* ---- the open note ---- */

  const loadFile = useCallback(
    async (id: string) => {
      const mine = ++fileGen.current;
      try {
        const file = await fetchFile(client, company, id);
        if (mine !== fileGen.current) return file;
        setOpenFile(file);
        setFileError(null);
        return file;
      } catch (e) {
        if (mine !== fileGen.current) return null;
        setOpenFile(null);
        setFileError(message(e, "could not open this note"));
        return null;
      }
    },
    [client, company],
  );

  /* ---- the editor's dirty buffer ---- */

  // The unsaved-text buffer, held in a ref so the debounce timer and the unmount
  // cleanup both see the latest value without re-subscribing on every keystroke.
  // It owns the two ordering rules the editor cannot get wrong — what counts as
  // unsaved, and which write gets to speak — and both are unit-tested away from
  // React in `lib/workspace-save-buffer.ts`.
  const bufferRef = useRef<SaveBuffer | null>(null);
  if (!bufferRef.current) bufferRef.current = createSaveBuffer();
  const buffer = bufferRef.current;
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const flush = useCallback(async () => {
    if (timer.current) {
      clearTimeout(timer.current);
      timer.current = null;
    }
    // The buffer decides whether there is anything to send and whether the
    // answer is still the newest word; a superseded write lands on none of
    // these callbacks, so it can neither claim "Saved" over text the host has
    // never seen nor overwrite an honest "Unsaved" with its own failure.
    await buffer.flush({
      write: (job) => writeFile(client, company, job.id, job.content),
      onSaving: () => setSaveState("saving"),
      onSaved: (job, ack) => {
        setSaveState("saved");
        // Patch the authoritative stamp onto both the open file and the tree
        // row, so "last updated" is the host's answer and not a guess.
        // `updatedBy` rides along: this route stamps the operator server-side,
        // and leaving the stale value would keep showing "edited by <agent>" on
        // a note the operator has just rewritten, until the next refetch.
        setOpenFile((f) =>
          f && f.id === job.id
            ? {
                ...f,
                content: job.content,
                updatedAt: ack.updatedAt,
                updatedBy: OPERATOR_ORIGIN,
              }
            : f,
        );
        setNodes((all) =>
          all.map((n) =>
            n.id === job.id ? { ...n, updatedAt: ack.updatedAt, updatedBy: OPERATOR_ORIGIN } : n,
          ),
        );
      },
      onFailed: (_job, e) => {
        // The buffer has already kept the text — the operator's words are never
        // dropped because a save failed, and the next edit retries them. A 404
        // means the note is gone on the host, which needs a decision rather than
        // a retry, so say so explicitly.
        setSaveState("error");
        if (isNotFound(e)) {
          toast.error("This note no longer exists on the host.", {
            description: "Someone deleted it. Your text is still here — save it as a new note.",
          });
        } else {
          toast.error(message(e, "could not save this note"));
        }
      },
    });
  }, [buffer, client, company]);

  // Always flush through the newest closure, including from cleanup callbacks
  // that captured an older one.
  const flushRef = useRef(flush);
  useEffect(() => {
    flushRef.current = flush;
  }, [flush]);

  // Unmounting (tab switch, sign-out) must not silently drop buffered typing.
  useEffect(() => {
    return () => {
      void flushRef.current();
    };
  }, []);

  // …and neither must unloading the page (issue #1372). The unmount cleanup
  // above covers every navigation React knows about, which is why switching
  // notes was already safe. A reload, a tab close or a link out of the console
  // is not one of those: the debounce timer dies with the document and the
  // typing goes with it, silently — and so does the request, because the
  // browser cancels an in-flight `PUT` on unload. The guard therefore asks the
  // buffer, which counts a write in flight as unsaved just as it counts a job
  // waiting on the debounce; checking a pending ref alone would leave the whole
  // round trip unguarded.
  useEffect(() => {
    const guard = createUnloadGuard(buffer);
    window.addEventListener("beforeunload", guard);
    return () => window.removeEventListener("beforeunload", guard);
  }, [buffer]);

  function onEdit(id: string, content: string) {
    setDraft(content);
    // `dirty`, not `idle`: from this keystroke until the host acknowledges the
    // write, the only copy of these words is in this tab (issue #1372).
    setSaveState("dirty");
    // Also invalidates any write already in flight: its answer is about text
    // this keystroke has just superseded, so it no longer gets to set the state.
    buffer.stage({ id, content });
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => void flushRef.current(), AUTOSAVE_DELAY_MS);
  }

  /* ---- refresh on refocus ---- */

  // The fallback half, kept: a host with no `/events` route, or a dropped
  // stream, leaves this view exactly as it behaved before #327. Coming back to
  // the tab is the moment an operator most expects to see current state.
  useEffect(() => {
    const refresh = () => {
      if (document.visibilityState !== "visible") return;
      void loadTree({ silent: true });
      // An active search is the *only* thing in the explorer pane while it
      // runs, so refreshing the tree behind it and leaving the hits alone would
      // refresh nothing the operator can see — and would leave them clicking
      // rows for notes that may since have been deleted.
      if (searchQuery) void runSearch(searchQuery);
    };
    window.addEventListener("focus", refresh);
    document.addEventListener("visibilitychange", refresh);
    return () => {
      window.removeEventListener("focus", refresh);
      document.removeEventListener("visibilitychange", refresh);
    };
  }, [loadTree, runSearch, searchQuery]);

  /* ---- live writes (#327) ---- */

  // A note written by an agent, by the publish drain, or by another browser
  // used to be invisible until the operator refreshed or refocused. Now the
  // host announces every workspace write and this reacts to it.
  //
  // Three rules, and the middle one is the one with teeth:
  //
  //  1. **Always refetch the tree, silently.** The frame carries no name and no
  //     body by design, so the tree read is where content comes from — and a
  //     silent refetch means a note appearing elsewhere never flickers the
  //     explorer or steals the operator's place.
  //  2. **Never clobber an in-progress edit.** The open note is refetched only
  //     in read mode. In edit mode the operator has a dirty buffer and an
  //     autosave in flight; replacing the body underneath them would discard
  //     typing that no refetch can get back. They see the change when they
  //     switch back to Read, which already refetches.
  //  3. **A vanished open note closes the pane** rather than being refetched —
  //     re-reading it would only 404 and leave an error where a note was. Which
  //     notes vanished is settled against the refreshed tree, not against the
  //     frame's id; [`planOpenNote`] carries the reasoning.
  useEffect(() => {
    if (!event) return;
    const frame = event;
    void (async () => {
      const tree = await loadTree({ silent: true });
      // Same reasoning as the refocus handler: a live write can add, change or
      // delete a note the hit list is naming, and the hit list is what is on
      // screen.
      if (searchQuery) void runSearch(searchQuery);
      const plan = planOpenNote({
        openId,
        event: frame,
        tree,
        mode,
        draft,
        saved: openFile?.content,
      });
      if (plan.kind === "leave") return;
      if (plan.kind === "reload") {
        if (openId) void loadFile(openId);
        return;
      }
      closeVanished(plan.rescue);
    })();
    // `event.tick` is the dependency that makes a repeat write on the same node
    // re-run this; `mode` and `openId` are read, not watched, so switching to
    // Read does not replay the last frame.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [event?.tick]);

  // A stream gap (or a stream that never reached OPEN) names no single node.
  // Re-read the canonical tree without disturbing the open editor; the normal
  // event path above remains responsible for payload-specific handling.
  useEffect(() => {
    if (refreshTick === 0) return;
    void loadTree({ silent: true });
    if (searchQuery) void runSearch(searchQuery);
  }, [refreshTick, loadTree, runSearch, searchQuery]);

  /**
   * The open note stopped existing while the pane held it.
   *
   * Cancelling the debounced save comes first and is not a tidiness step: the
   * timer is still armed on a node the host no longer has, so leaving it would
   * fire one guaranteed-404 write for a note that is already gone.
   *
   * Dropping the operator's words is the one thing this must not do. Everything
   * else in this view can be re-read from the host; unsaved typing cannot, and
   * the old handler cleared `draft` on its way out. So the text moves into a
   * banner that offers it back rather than leaving with the note it was written
   * in. A buffer with nothing unsaved in it closes silently, as it always did —
   * a banner for text the host already has is noise.
   */
  function closeVanished(rescue: string | null) {
    if (timer.current) {
      clearTimeout(timer.current);
      timer.current = null;
    }
    // Preferred over the plan's answer because it is a ref and therefore
    // current: the operator can keep typing during the tree refetch above, and
    // those keystrokes reach the buffer while the plan was computed from the
    // `draft` of the render the frame arrived in.
    const job = buffer.peek();
    buffer.clear();
    const keep = job?.content ?? rescue;
    if (keep !== null) {
      const gone = nodeById(nodes, openId);
      setRescued({ name: gone ? titleOf(gone) : "Untitled", content: keep });
    }
    setOpenId(null);
    setOpenFile(null);
    setDraft(null);
    setFileError(null);
    setSaveState("idle");
  }

  /** Land rescued text in the workspace as a note of its own. */
  async function saveRescued() {
    if (!rescued) return;
    // Cleared only on success: a failed create toasts and leaves the banner
    // standing, because dismissing it would destroy the last copy of the text.
    if (await createAndOpen(`${rescued.name} (recovered)`, rescued.content)) setRescued(null);
  }

  async function copyRescued() {
    if (!rescued) return;
    try {
      await navigator.clipboard.writeText(rescued.content);
      toast.success("Copied your unsaved text.");
    } catch {
      // No clipboard permission, or an insecure origin. The text is rendered in
      // the banner either way, so say so rather than leaving a dead button.
      toast.error("Couldn't copy — select the text above and copy it yourself.");
    }
  }

  /* ---- deep link into one note (#552) ---- */

  // The Artifacts tab's "Open in workspace" link sets `#/workspace/<nodeId>`,
  // and the shell hands that segment down. Opened once per id: `open()` sets
  // `openId`, and re-running on every render would fight an operator who then
  // clicked a different note.
  const landedOn = useRef<string | null>(null);
  useEffect(() => {
    if (!initialNodeId || landedOn.current === initialNodeId) return;
    landedOn.current = initialNodeId;
    void open(initialNodeId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [initialNodeId]);

  /* ---- reveal the open note in the tree (#1371) ---- */

  // Expanding runs off `nodes` as well as `openId`, and that is the whole
  // point: on the deep-link route the note is opened before the tree has
  // loaded, so the first pass has no ancestors to find and the second — once
  // the nodes arrive — is the one that does the work. Expanding is idempotent,
  // so re-running it costs nothing.
  useEffect(() => {
    if (!openId) return;
    const ancestors = ancestorFolderIds(nodes, openId);
    if (ancestors.length > 0) {
      setExpanded((prev) => {
        if (ancestors.every((id) => prev.has(id))) return prev;
        return new Set([...prev, ...ancestors]);
      });
    }
    setRevealId(openId);
  }, [openId, nodes]);

  /* ---- migration off the retired localStorage scratchpad ---- */

  useEffect(() => {
    const mine = readLegacyLocalNodes(scope);
    setImportDeclined(legacyImportDeclined(scope));
    if (mine.length > 0) {
      setLegacy(mine);
      return;
    }
    // A key holding nothing but the bundled seed is app-shipped bytes with zero
    // user information — sweep it silently rather than offering to import
    // invented marketing copy into a real company's workspace.
    if (hasLegacyLocal(scope)) clearLegacyLocal(scope);
    setLegacy([]);
  }, [company]);

  async function importLegacy() {
    setImporting(true);
    try {
      const root = await createNode(client, company, {
        name: IMPORT_FOLDER_NAME,
        kind: "folder",
      });
      // Folders first, so a child's remapped parent id always exists by the time
      // it is created. Old ids are local-only and meaningless to the host.
      const remap = new Map<string, string>();
      const parentFor = (node: FsNode) =>
        node.parentId ? (remap.get(node.parentId) ?? root.id) : root.id;
      for (const folder of legacyFolders) {
        const created = await createNode(client, company, {
          name: folder.name,
          kind: "folder",
          parentId: parentFor(folder),
        });
        remap.set(folder.id, created.id);
      }
      for (const file of legacyFiles) {
        await createNode(client, company, {
          name: ensureMdExt(file.name),
          kind: "file",
          parentId: parentFor(file),
          content: file.content ?? "",
        });
      }
      clearLegacyLocal(scope);
      setLegacy([]);
      toast.success(
        `Imported ${importSummary(legacyFiles.length, legacyFolders.length)} into “${IMPORT_FOLDER_NAME}”.`,
      );
      await loadTree({ silent: true });
      // Show the operator where their notes went (issue #1472). A toast naming
      // a folder is still a folder they then have to find in a tree they did
      // not build; the import is the one moment the console knows exactly which
      // row is the answer.
      setSearchInput("");
      setExpanded((prev) => new Set([...prev, root.id]));
      setRevealId(root.id);
    } catch (e) {
      // The key is left intact on failure, so the banner comes back and nothing
      // the operator wrote is lost to a half-finished import.
      toast.error(message(e, "could not import your local notes"));
    } finally {
      setImporting(false);
    }
  }

  /**
   * Destroy the browser's copy of the old scratchpad.
   *
   * Only ever reached from the confirm dialog. `clearLegacyLocal` removes both
   * the scoped key and the pre-connection origin it was adopted from, so by
   * construction there is nothing left to offer again — this is the delete, not
   * a dismissal of the offer, and `declineImport` is the button for the latter.
   */
  function discardLegacy() {
    clearLegacyLocal(scope);
    setLegacy([]);
    setConfirmDiscard(null);
  }

  /** Stop offering the import without touching the notes (issue #1472). */
  function declineImport() {
    declineLegacyImport(scope);
    setImportDeclined(true);
  }

  /** Throw away the rescued text — likewise only from the confirm. */
  function discardRescued() {
    setRescued(null);
    setConfirmDiscard(null);
  }

  /* ---- navigation ---- */

  async function open(id: string) {
    await flush();
    setOpenId(id);
    // Below `md` the two panes share one column, so opening a note has to hand
    // it over; above `md` both are shown regardless and this is inert.
    setShowExplorer(false);
    setMode("read");
    setDraft(null);
    setSaveState("idle");
    setOpenFile(null);
    setFileError(null);
    // The ancestors are expanded by the reveal effect below rather than here.
    // Doing it here read `nodes` out of this closure, and on the deep-link route
    // (`#/workspace/<id>`) that closure runs before the tree has arrived — so
    // `pathOf` walked an empty array, expanded nothing, and the note the
    // operator had just been sent to was nowhere in the explorer (issue #1371).
    // A payload has no text body to fetch, and the host refuses the text read
    // for one — asking anyway would put an error in `fileError` for a file that
    // is perfectly fine (issue #553). `BinaryNodeView` fetches the bytes it
    // needs itself.
    const node = nodeById(nodes, id);
    if (node && isBinary(node)) return;
    await loadFile(id);
  }

  /**
   * Act on a search hit.
   *
   * A file goes through the ordinary `open` flow, so a hit behaves exactly like
   * a tree click — including the binary case, which `open` already knows not to
   * fetch a text body for. The search stays up: the operator is usually working
   * a list of candidates, and clearing it on the first click would make them
   * retype the query to reach the second.
   *
   * A folder cannot be "opened" — there is no pane for one — so it exits the
   * search and reveals the folder in the tree, which is the only thing that
   * could have been meant.
   */
  async function openHit(hit: SearchHit) {
    if (hit.kind === "folder") {
      setSearchInput("");
      setExpanded((prev) => new Set([...prev, ...ancestorFolderIds(nodes, hit.id), hit.id]));
      setRevealId(hit.id);
      return;
    }
    await open(hit.id);
  }

  /**
   * Show a folder in the tree, from a breadcrumb crumb (issue #1371).
   *
   * Expands it as well as its ancestors — clicking the folder you are inside of
   * means "show me what else is in here", and revealing it collapsed would
   * answer a question nobody asked.
   */
  function revealFolder(id: string) {
    setSearchInput("");
    setExpanded((prev) => new Set([...prev, ...ancestorFolderIds(nodes, id), id]));
    setRevealId(id);
  }

  function toggle(id: string) {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  // Switching to Edit refetches first, so the operator edits what the host
  // currently holds rather than a copy that may be minutes stale — the cheapest
  // mitigation available for the no-CAS overwrite window.
  async function changeMode(next: "read" | "edit") {
    // Issue #1222: the header renders no Edit tab for a derived file, and this
    // is the same rule at the only other door into edit mode. Belt and braces
    // on purpose — a mode this function can reach is a buffer the autosave will
    // push, and the whole failure being fixed is typing the host cannot take.
    if (next === "edit" && isDerivedNode(nodes, openId)) return;
    await flush();
    if (next === "edit" && openId) {
      const fresh = await loadFile(openId);
      setDraft(fresh?.content ?? null);
    } else {
      setDraft(null);
    }
    setSaveState("idle");
    setMode(next);
  }

  /* ---- mutations (apply-on-ack) ---- */

  // Answers whether the note actually landed, which only the rescue banner
  // reads: it is holding the last copy of some text and must not clear itself
  // on a create that failed.
  async function createAndOpen(
    name: string,
    content?: string,
    parentId?: string | null,
  ): Promise<boolean> {
    try {
      const created = await createNode(client, company, {
        name: ensureMdExt(name.trim() || "Untitled"),
        kind: "file",
        parentId: parentId ?? defaultParentId,
        content: content ?? "",
      });
      setNodes((all) => [...all, created]);
      setOpenId(created.id);
      setOpenFile({
        id: created.id,
        name: created.name,
        content: content ?? "",
        updatedAt: created.updatedAt,
        // Straight off the create response rather than assumed: the host mints
        // the origins, and this route is the operator's, so they will say
        // `operator` — but reading them keeps this in step if that ever moves.
        createdBy: created.createdBy,
        updatedBy: created.updatedBy,
        backlinks: [],
      });
      setFileError(null);
      setDraft(content ?? "");
      setSaveState("idle");
      setMode("edit");
      return true;
    } catch (e) {
      toast.error(message(e, "could not create the note"));
      return false;
    }
  }

  async function createFolder(name: string, parentId?: string | null) {
    try {
      const created = await createNode(client, company, {
        name: name.trim() || "New folder",
        kind: "folder",
        parentId: parentId ?? defaultParentId,
      });
      setNodes((all) => [...all, created]);
      if (created.parentId) revealFolder(created.parentId);
    } catch (e) {
      toast.error(message(e, "could not create the folder"));
    }
  }

  async function rename(node: FsNode, name: string) {
    const next = (node.kind === "file" ? ensureMdExt(name.trim()) : name.trim()) || node.name;
    try {
      const updated = await renameMoveNode(client, company, node.id, {
        name: next,
      });
      setNodes((all) => all.map((n) => (n.id === updated.id ? updated : n)));
      setOpenFile((f) => (f && f.id === updated.id ? { ...f, name: updated.name } : f));
    } catch (e) {
      toast.error(message(e, "could not rename this item"));
    }
  }

  async function move(node: FsNode, destId: string | null) {
    try {
      // The move-cycle guard is the host's: it answers 400 for a folder moved
      // under its own descendant, which surfaces here as a toast.
      const updated = await renameMoveNode(client, company, node.id, {
        parentId: destId,
      });
      setNodes((all) => all.map((n) => (n.id === updated.id ? updated : n)));
      // Name the destination (issue #1381). The receipt for a move that cannot
      // be undone in one click said only that it happened, so an operator who
      // picked the wrong row of an unsorted, unpathed list learned nothing from
      // the confirmation either.
      toast.success(
        `Moved “${titleOf(node)}” to ${
          destId ? folderPathLabel(nodes, destId, rosterNames) : "the workspace root"
        }.`,
      );
    } catch (e) {
      toast.error(message(e, "could not move this item"));
    }
  }

  async function remove(node: FsNode) {
    const removed = subtreeIds(nodes, node.id);
    try {
      await deleteNodeApi(client, company, node.id);
      setNodes((all) => all.filter((n) => !removed.has(n.id)));
      nodesRef.current = nodesRef.current.filter((n) => !removed.has(n.id));
      // A deleted node may be somebody's chosen face (`blob:<nodeId>`); drop
      // it from the avatar cache so the next render degrades to the tone
      // tile rather than keeping a face whose file just ceased to exist.
      for (const id of removed) forgetAvatarNode(client, company, id);
      if (openId && removed.has(openId)) {
        buffer.clear();
        setOpenId(null);
        setOpenFile(null);
        setDraft(null);
      }
    } catch (e) {
      toast.error(message(e, "could not delete this item"));
    }
  }

  /**
   * Ask the host which `agents/<id>/` folders are empty, and show them
   * (issue #700).
   *
   * A preview, always — the deletion is a second call the operator makes from
   * the dialog. Nothing about emptiness is decided here: the host counts
   * children structurally, over every node in the tree, and this only renders
   * the answer.
   */
  async function previewSweep() {
    setSweeping(true);
    try {
      const folders = await sweepEmptyAgentFolders(client, company, true);
      if (folders.length === 0) {
        toast.success("No empty agent folders to tidy.");
        return;
      }
      setSweep({ stage: "preview", folders });
    } catch (e) {
      toast.error(message(e, "could not check for empty agent folders"));
    } finally {
      setSweeping(false);
    }
  }

  /**
   * Remove them, then report what actually went.
   *
   * The result list is the host's, not the preview echoed back: a folder that
   * gained a deliverable between the two calls is left standing and is absent
   * here, so the receipt describes the tree rather than the operator's intent.
   */
  async function confirmSweep() {
    setSweeping(true);
    try {
      const removed = await sweepEmptyAgentFolders(client, company, false);
      const gone = new Set(removed.map((f) => f.id));
      setNodes((all) => all.filter((n) => !gone.has(n.id)));
      setSweep({ stage: "done", folders: removed });
    } catch (e) {
      toast.error(message(e, "could not tidy the empty agent folders"));
      setSweep(null);
    } finally {
      setSweeping(false);
    }
  }

  /**
   * Ask the host what the duplicate folders in this tree would merge into
   * (issue #759).
   *
   * A preview, always. The repair *moves* notes between folders rather than
   * removing provably empty ones, so the operator sees every relocation before
   * agreeing to any of them — and sees, up front, what the host will refuse to
   * decide.
   */
  async function previewRepair() {
    setRepairing(true);
    try {
      const outcome = await mergeDuplicateFolders(client, company, true);
      if (outcome.folders.length === 0 && outcome.residuals.length === 0) {
        toast.success("No duplicate folders to repair.");
        return;
      }
      setRepair({ stage: "preview", outcome });
    } catch (e) {
      toast.error(message(e, "could not check for duplicate folders"));
    } finally {
      setRepairing(false);
    }
  }

  /**
   * Do it, then report what actually happened.
   *
   * The result is the host's, not the preview echoed back: a folder that gained
   * a note between the two calls is left standing and says so, and a relocation
   * the tree moved out from under turns into a residual. The local tree is
   * replayed from that answer rather than refetched, so the open note keeps its
   * unsaved draft.
   */
  async function confirmRepair() {
    setRepairing(true);
    try {
      const outcome = await mergeDuplicateFolders(client, company, false);
      setNodes((all) => applyRepair(all, outcome));
      setRepair({ stage: "done", outcome });
    } catch (e) {
      toast.error(message(e, "could not repair the duplicate folders"));
      setRepair(null);
    } finally {
      setRepairing(false);
    }
  }

  async function onWiki(target: string) {
    const existing = fileByTitle(nodes, target);
    if (existing) {
      await open(existing.id);
      return;
    }
    await flush();
    await createAndOpen(target, `# ${target}\n`);
  }

  /**
   * Upload files of any kind (issue #553).
   *
   * Every file now goes to the host's multipart route, including Markdown.
   * Reading the bytes here to decide would mean re-implementing the host's
   * text-versus-binary rule in a second place, where the two could disagree
   * about the same file; the host reads the bytes and answers with the node it
   * made, so there is one rule and the console just renders the result.
   */
  async function onUpload(files: FileList | null) {
    if (!files?.length) return;
    for (const file of Array.from(files)) {
      try {
        const created = await uploadFile(client, company, file, defaultParentId);
        setNodes((all) => [...all, created]);
      } catch (e) {
        toast.error(`${file.name}: ${message(e, "upload failed")}`);
      }
    }
  }

  const body = draft ?? openFile?.content ?? "";
  const openNode = nodeById(nodes, openId);
  /**
   * Where a create with no named destination lands (issue #1477).
   *
   * Every create passed `parentId: null`, so an operator standing in
   * `Standards/Engineering/` made a note and it appeared at the root,
   * unannounced — and the only way to file it was the Move dialog, which was
   * itself unusable (#1381). "Where I am" is the open note's folder, or the
   * folder itself if a folder is what is open; only a genuinely empty selection
   * falls back to the root.
   *
   * Never `derived/`: the host refuses writes there, so inheriting it from an
   * open ledger file would turn every create into an error toast.
   */
  const defaultParentId: string | null = (() => {
    if (!openNode) return null;
    const parent = openNode.kind === "folder" ? openNode.id : openNode.parentId;
    return parent && !isDerivedNode(nodes, parent) ? parent : null;
  })();
  /**
   * Whether the host will refuse a write to the open note (issue #1222).
   *
   * Read off the tree rather than off the node, because the rule is the
   * *folder* — see `isDerivedNode`. Load-bearing in two places: the header
   * offers no Edit tab, and `changeMode` refuses to enter edit mode even if
   * something else asks it to.
   */
  const readOnlyNote = isDerivedNode(nodes, openId);
  /**
   * Whether the open note is one the company's agents cannot read (#1465).
   *
   * The same folder rule the tree row asks, asked again for the header, because
   * this is the pane an operator is looking at while typing the credential —
   * and a note opened from a search hit or a `[[wiki link]]` arrives here with
   * the tree never scrolled to it.
   */
  const secretNote = isSecretNode(nodes, openId);
  /**
   * How many notes the workspace holds, for the header's count.
   *
   * Memoised rather than filtered inline: this component re-renders on every
   * keystroke in the editor (the draft is state), and `nodes` is the whole
   * tree — so an inline scan would walk every node in the workspace once per
   * character typed, to recompute a number that only changes when the tree
   * does.
   */
  const noteCount = useMemo(() => countNotes(nodes), [nodes]);

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col">
      {/*
        Issue #1763: Workspace was the one console page with no header at all.
        It opened straight into the `EXPLORER` toolbar, so the first heading an
        operator's eye landed on was a column label for the left rail, and the
        only thing naming the page was the nav row they arrived from.

        It had an `sr-only` title (issue #1221) on the reasoning that "the file
        tree and editor are the page". That is true of Chat and Inbox, where the
        content starts at the top edge and fills the frame. It is not true here:
        the pane beside the tree is empty until a note is opened, so the page
        opened on an unnamed toolbar over blank space. This was the omission,
        not the decision.

        The count is the notes, not the folders — a folder is how the tree is
        arranged rather than a thing the workspace holds.
      */}
      <PageHeader
        title="Workspace"
        count={headerNoteCount(noteCount, treeKnown)}
        /*
          Not "every note this company's teammates can read and write", which
          the tree contradicts in two places: `secrets/` is the one folder the
          agents cannot list, read, search or write (`SECRETS_REASON`, #1465),
          and `derived/` is written by a ledger and re-derived over any edit
          (`DERIVED_REASON`, #1222). A header that claims universal read/write
          is worst exactly where it matters most — over a folder holding
          credentials.

          It describes the surface and points at where the rule is stated
          rather than restating it. The per-folder rules already appear on the
          tree row, in the move dialog and on the note itself, in one wording
          each on purpose (see `SECRETS_LABEL`); a fourth phrasing up here is
          how an operator comes to believe there are four rules. The
          conditional also stays true of a workspace that has neither folder,
          which an "…and two folders are exceptions" sentence would not.
        */
        description="Every note this company holds, in one shared tree. Where a folder is read-only or hidden from the agents, the tree says so."
        data-testid="workspace-header"
      />
      {/*
        `min-w-0 max-w-full` is #1767's, kept: it is what stops the resizable
        explorer column pushing the editor past the viewport. It was on the
        one wrapper this view had; #1763 splits that into a column (header
        above, panes below), and the classes belong on the row holding the
        `aside`, which is this one rather than the element outside it.
      */}
      <div className="flex min-h-0 min-w-0 max-w-full flex-1 overflow-hidden">
      {/* Explorer */}
      <aside
        id="workspace-explorer"
        className={cn(
          "min-w-0 shrink-0 flex-col overflow-hidden bg-card/40 md:flex",
          // Full width while it owns the column; the resizable width only
          // applies once the note pane is beside it.
          "w-full md:w-(--workspace-list-width)",
          showExplorer ? "flex" : "hidden",
        )}
        style={{ "--workspace-list-width": `${listWidth}px` } as CSSProperties}
      >
        <div className="flex items-center gap-1 border-b px-2 py-2">
          <span className="flex-1 px-1 text-xs font-semibold tracking-wide text-muted-foreground uppercase">
            Explorer
          </span>
          <IconBtn
            label="Refresh"
            onClick={() => void loadTree({ silent: true })}
            data-testid="workspace-refresh"
          >
            <RefreshCw className={cn("size-4", refreshing && "animate-spin")} />
          </IconBtn>
          <IconBtn label="New file" onClick={() => setPrompt({ mode: "file" })}>
            <FilePlus2 className="size-4" />
          </IconBtn>
          <IconBtn label="New folder" onClick={() => setPrompt({ mode: "folder" })}>
            <FolderPlus className="size-4" />
          </IconBtn>
          <IconBtn label="Upload" onClick={() => uploadRef.current?.click()}>
            <Upload className="size-4" />
          </IconBtn>
          {/* Expansion persists for the session and a deep tree stays open
              behind every reveal, so there was no way back to a readable
              explorer short of collapsing each folder by hand (issue #1382).
              `setExpanded(new Set())` already existed; nothing invoked it. */}
          <IconBtn
            label="Collapse all"
            disabled={expanded.size === 0}
            onClick={() => setExpanded(new Set())}
            data-testid="workspace-collapse-all"
          >
            <ChevronsDownUp className="size-4" />
          </IconBtn>
          {/* The two controls after this line repair the tree rather than adding
              to it, and both can remove folders. Kept visually apart from the
              make-something group so the row is not six identical glyphs with
              two mines in it (issue #1378). */}
          <span aria-hidden className="mx-0.5 h-4 w-px shrink-0 self-center bg-border" />
          {/* Issue #700. A company provisioned before the tree went lazy carries
              one empty folder per teammate, and nothing else will ever remove
              them. Deliberately a button rather than something boot does: the
              operator's click is the opt-in, and the dialog names every folder
              before any of them goes. */}
          {/* Nothing to tidy or repair in a tree with nothing in it. A no-op on
              a live company, where the scaffold guarantees rows — but the
              controls claimed to apply to a state they cannot act on, which is
              the same claim the dead explorer branch was making (#1380). */}
          <IconBtn
            label="Tidy empty agent folders"
            disabled={sweeping || nodes.length === 0}
            onClick={() => void previewSweep()}
            data-testid="workspace-sweep"
          >
            <FolderX className="size-4" />
          </IconBtn>
          {/* Issue #759. Two publishes of one deliverable can race and leave two
              folders with the same name, after which every publish beneath that
              path is refused as ambiguous — for every agent, forever. Stopping
              new races does nothing for a tree already in that state, and on a
              hosted tenant this button is the only way out of it. Operator-
              triggered for the same reason the tidy beside it is: nothing
              rearranges somebody's tree unasked. */}
          <IconBtn
            label="Repair duplicate folders"
            disabled={repairing || nodes.length === 0}
            onClick={() => void previewRepair()}
            data-testid="workspace-repair"
          >
            <FolderSync className="size-4" />
          </IconBtn>
          <input
            ref={uploadRef}
            type="file"
            // Anything. The tree holds bytes now, and the host decides what
            // each file becomes — an allow-list here would be a second,
            // narrower rule that silently refuses files the store supports.
            accept="*/*"
            multiple
            hidden
            onChange={(e) => {
              void onUpload(e.target.files);
              e.target.value = "";
            }}
          />
        </div>
        {/* Search (issue #607). In the explorer header beside the refresh
            button, because it answers the same question the tree below it does
            — "which note do I want?" — and an operator who cannot find a note
            by eye reaches here next. */}
        <div className="relative border-b px-2 py-1.5">
          <Search className="pointer-events-none absolute top-1/2 left-4 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={searchInput}
            onChange={(e) => setSearchInput(e.target.value)}
            onKeyDown={(e) => {
              // Escape restores the tree, the shortcut every search box has.
              if (e.key === "Escape") setSearchInput("");
            }}
            placeholder="Search notes…"
            aria-label="Search workspace notes"
            className="h-8 pr-7 pl-7 text-sm"
            data-testid="workspace-search"
          />
          {searchInput && (
            <button
              type="button"
              onClick={() => setSearchInput("")}
              aria-label="Clear search"
              data-testid="workspace-search-clear"
              className="absolute top-1/2 right-4 -translate-y-1/2 text-muted-foreground hover:text-foreground"
            >
              <X className="size-3.5" />
            </button>
          )}
        </div>
        <div className="flex-1 overflow-y-auto py-1" data-testid="workspace-tree">
          {/* An active search replaces the tree rather than sitting beside it —
              showing both would leave the operator reading a tree that is not
              what they just asked for. */}
          {searchInput.trim() ? (
            <SearchResults
              query={searchQuery || searchInput.trim()}
              hits={searchPage?.hits ?? []}
              total={searchPage?.total ?? 0}
              loading={searching || searchQuery !== searchInput.trim()}
              error={searchError}
              onOpen={(hit) => void openHit(hit)}
              rosterNames={rosterNames}
            />
          ) : loading ? (
            <div className="space-y-2 px-2 py-2">
              <Skeleton className="h-5 w-4/5" />
              <Skeleton className="h-5 w-3/5" />
              <Skeleton className="h-5 w-2/3" />
            </div>
          ) : error ? (
            <div className="px-2 py-2">
              <Alert variant="destructive">
                <AlertDescription data-testid="workspace-error">{error}</AlertDescription>
              </Alert>
              <Button
                variant="outline"
                size="sm"
                className="mt-2 w-full"
                onClick={() => void loadTree()}
              >
                Try again
              </Button>
            </div>
          ) : (
            /* The `nodes.length === 0` branch that used to sit here said "This
               workspace is empty. Create a note to start." — dead code, since
               `ensure_workspace_scaffold` lays down `Agents/` and `secrets/` on
               every boot, and a second message contradicting the note pane's
               own (issue #1380). The pane owns what an empty workspace says. */
            <Tree
              nodes={nodes}
              parentId={null}
              depth={0}
              expanded={expanded}
              openId={openId}
              revealId={revealId}
              onRevealed={onRevealed}
              rosterNames={rosterNames}
              onToggle={toggle}
              onOpen={(id) => void open(id)}
              onRename={(node) => setPrompt({ mode: "rename", node })}
              onMove={(node) => setMoving(node)}
              onDelete={(node) => setConfirmDelete(node)}
              onNewHere={(folder, mode) => setPrompt({ mode, parentId: folder.id })}
            />
          )}
        </div>
      </aside>

      {/* On small screens the explorer and note are mutually exclusive, so a
          divider only has meaning from the two-pane breakpoint onward. */}
      <div
        role="separator"
        aria-label="Resize workspace explorer"
        aria-orientation="vertical"
        aria-controls="workspace-explorer workspace-content"
        aria-valuemin={MIN_WORKSPACE_LIST_WIDTH}
        aria-valuemax={MAX_WORKSPACE_LIST_WIDTH}
        aria-valuenow={listWidth}
        aria-valuetext={`${listWidth} pixels`}
        tabIndex={0}
        data-testid="workspace-list-resizer"
        className={cn(
          "relative w-1.5 shrink-0 touch-none cursor-col-resize bg-border outline-none transition-colors select-none motion-reduce:transition-none",
          "hover:bg-primary/50 focus-visible:bg-primary focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset",
          resizingList && "bg-primary/70",
          "hidden md:block",
        )}
        onPointerDown={(e) => {
          if (e.button !== 0) return;
          e.preventDefault();
          listResize.current = {
            pointerId: e.pointerId,
            startX: e.clientX,
            startWidth: listWidth,
          };
          e.currentTarget.setPointerCapture(e.pointerId);
          setResizingList(true);
        }}
        onPointerMove={(e) => {
          const drag = listResize.current;
          if (!drag || drag.pointerId !== e.pointerId) return;
          setListWidth(clampWorkspaceListWidth(drag.startWidth + e.clientX - drag.startX));
        }}
        onPointerUp={(e) => {
          if (listResize.current?.pointerId !== e.pointerId) return;
          listResize.current = null;
          setResizingList(false);
          if (e.currentTarget.hasPointerCapture(e.pointerId)) {
            e.currentTarget.releasePointerCapture(e.pointerId);
          }
        }}
        onPointerCancel={(e) => {
          if (listResize.current?.pointerId !== e.pointerId) return;
          listResize.current = null;
          setResizingList(false);
        }}
        onLostPointerCapture={() => {
          listResize.current = null;
          setResizingList(false);
        }}
        onKeyDown={(e) => {
          if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
          e.preventDefault();
          const delta =
            e.key === "ArrowLeft" ? -WORKSPACE_LIST_KEYBOARD_STEP : WORKSPACE_LIST_KEYBOARD_STEP;
          setListWidth((width) => clampWorkspaceListWidth(width + delta));
        }}
      />

      {/* Note pane */}
      <section
        id="workspace-content"
        className={cn(
          "min-w-0 flex-1 flex-col overflow-hidden",
          showExplorer ? "hidden md:flex" : "flex",
        )}
      >
        {legacy.length > 0 && !importDeclined && (
          <Alert className="m-3 mb-0 w-auto" data-testid="workspace-migration-banner">
            <AlertDescription className="flex flex-wrap items-center gap-3">
              <span className="flex-1 space-y-1">
                <span className="block">
                  {migrationBannerText(legacyFiles.length, legacyFolders.length)}
                </span>
                {/* What Import does, before it is clicked (issue #1472). */}
                <span className="block text-xs text-muted-foreground">{MIGRATION_CONSEQUENCE}</span>
              </span>
              <Button
                size="sm"
                disabled={importing}
                onClick={() => void importLegacy()}
                data-testid="workspace-migration-import"
              >
                {importing && <Loader2 className="size-3.5 animate-spin" />} Import
              </Button>
              {/* The non-destructive exit (issue #1472). Without it the only
                  way to stop being asked was the button that deletes the
                  notes, which is how the quiet control became the dangerous
                  one. */}
              <Button
                size="sm"
                variant="ghost"
                disabled={importing}
                onClick={declineImport}
                data-testid="workspace-migration-decline"
              >
                Not now
              </Button>
              <Button
                size="sm"
                variant="destructive"
                disabled={importing}
                onClick={() => setConfirmDiscard("legacy")}
                data-testid="workspace-migration-discard"
              >
                Discard
              </Button>
            </AlertDescription>
          </Alert>
        )}
        {/* Words rescued from a note that was deleted out from under the
            editor. Rendered above whatever is in the pane rather than inside
            it, because the note this text came from is gone and there is no
            longer a pane it belongs to — and it stays until the operator does
            something with it, since dismissing it on the next click is how the
            last copy of a paragraph gets thrown away. */}
        {rescued && (
          <Alert className="m-3 mb-0 w-auto" data-testid="workspace-rescued-banner">
            <AlertDescription className="flex flex-col items-stretch gap-2">
              <span>
                “{rescued.name}” was deleted while you were writing in it. Your unsaved text is
                below — save it as a new note, or copy it out.
              </span>
              <Textarea
                readOnly
                value={rescued.content}
                aria-label="Your unsaved text"
                data-testid="workspace-rescued-text"
                className="max-h-48 resize-none font-mono text-xs"
              />
              <span className="flex flex-wrap gap-2">
                <Button size="sm" onClick={() => void saveRescued()}>
                  Save as new note
                </Button>
                <Button size="sm" variant="ghost" onClick={() => void copyRescued()}>
                  Copy
                </Button>
                <Button
                  size="sm"
                  variant="destructive"
                  onClick={() => setConfirmDiscard("rescued")}
                  data-testid="workspace-rescued-discard"
                >
                  Discard
                </Button>
              </span>
            </AlertDescription>
          </Alert>
        )}
        {openNode && openNode.kind === "file" ? (
          <>
            <div className="flex items-center gap-2 border-b px-3 py-2">
              <IconBtn label="Toggle explorer" onClick={() => setShowExplorer((s) => !s)}>
                <PanelLeft className="size-4" />
              </IconBtn>
              <span className="flex min-w-0 items-baseline gap-1.5">
                {/* Where this note lives (issue #1371). The header used to name
                    the file and nothing else, which in a tree five deep with
                    three notes called README answered neither "which one is
                    this?" nor "what sits beside it?". Search already knew — it
                    returns a `path` on every hit — and threw the answer away at
                    exactly the moment it became useful. */}
                <Breadcrumb nodes={nodes} nodeId={openNode.id} onOpenFolder={revealFolder} />
                <span className="truncate text-sm font-medium">{titleOf(openNode)}</span>
              </span>
              {/* Said in the header and not only in the tree, because this is
                  the pane the typing happens in — and unlike the tree, it is
                  reached from a search hit and a wiki link too (issue #1465).
                  It sits after the crumb rail rather than inside it: the rail
                  says where the note is filed, this says who can read it. */}
              {secretNote && (
                <Badge
                  variant="outline"
                  className="shrink-0 gap-1 font-normal"
                  title={SECRETS_REASON}
                  data-testid="workspace-secret"
                >
                  <EyeOff className="size-3" />
                  {SECRETS_LABEL}
                </Badge>
              )}
              {/* No authorship on a derived file (#1377). `derived::publish`
                  stamps `WorkspaceOrigin::Seed` — that is how the write guard
                  tells its own derivation from a person — so `originLabel`
                  renders `Seeded` on every one of these. "Seeded" means "it
                  shipped with the company bundle and was typed by nobody",
                  which is a different and wrong story: this file was rendered
                  seconds ago and is rewritten on every `record_entry`. Two
                  console-authored badges disagreeing about the same file is
                  worse than one, so the chip that IS true says it alone. The
                  breadcrumb above stays either way: where a derived file lives
                  is the fact that explains which ledger wrote it.

                  A `secrets/` note keeps its authorship — the README the host
                  seeds there really was seeded, so both chips are true. */}
              {!readOnlyNote && (
                <Authorship
                  createdBy={openFile?.createdBy ?? openNode.createdBy}
                  updatedBy={openFile?.updatedBy ?? openNode.updatedBy}
                  rosterNames={rosterNames}
                />
              )}
              {/* Labelled (issue #1382). A bare "2 days ago" beside the title
                  read like part of it, and did not say which event it was
                  timing. */}
              <span
                className="hidden shrink-0 text-xs text-muted-foreground sm:inline"
                data-testid="workspace-updated"
              >
                Edited{" "}
                {formatUpdated(openFile?.updatedAt ?? openNode.updatedAt)}
              </span>
              {/* The backlink count, always visible (issue #1382). The rail
                  that lists them is `xl:flex`, so below about 1280px a note
                  with eleven backlinks and one with none looked identical —
                  the whole signal disappeared rather than degrading. */}
              {openFile && openFile.backlinks.length > 0 && (
                <span
                  className="hidden shrink-0 items-center gap-1 text-xs text-muted-foreground sm:inline-flex"
                  data-testid="workspace-backlink-count"
                  title="Notes that link here"
                >
                  <Link2 className="size-3" aria-hidden />
                  {openFile.backlinks.length}
                </span>
              )}
              <SaveStatus state={saveState} />
              {/* A payload has no prose to read or edit, so the mode switch is
                  hidden rather than shown-and-broken (issue #553). The host
                  refuses a text write to one, so an Edit tab here would be a
                  control whose only outcome is an error toast.

                  Issue #1222 applies the same rule to `derived/`, which the
                  host refuses for the same reason and which this pane was
                  offering anyway — the operator could type into a file whose
                  own first paragraph says "Do not edit this file", get a 400
                  per keystroke burst, and lose the text on the way out. A
                  derived file keeps its reading pane (unlike a payload, there
                  IS prose to read), so the switch is replaced by a chip that
                  says why rather than hidden. */}
              {isBinary(openNode) ? null : readOnlyNote ? (
                <Badge
                  variant="outline"
                  className="ml-auto shrink-0 gap-1 font-normal"
                  data-testid="workspace-read-only"
                >
                  <Lock className="size-3" />
                  {DERIVED_LABEL}
                </Badge>
              ) : (
                <Tabs
                  value={mode}
                  onValueChange={(v) => void changeMode(v as "read" | "edit")}
                  className="ml-auto"
                >
                  <TabsList>
                    <TabsTrigger value="read">Reading</TabsTrigger>
                    <TabsTrigger value="edit">Edit</TabsTrigger>
                  </TabsList>
                </Tabs>
              )}
            </div>
            <div className="flex flex-1 overflow-hidden">
              <div className="flex-1 overflow-y-auto">
                {isBinary(openNode) ? (
                  // Checked before `fileError` and before the skeleton: a
                  // payload is never fetched through the text route at all, so
                  // neither of those states is reachable for one and both would
                  // be wrong answers here.
                  <BinaryNodeView client={client} company={company} node={openNode} />
                ) : fileError ? (
                  <div className="p-6">
                    <Alert variant="destructive">
                      <AlertDescription>{fileError}</AlertDescription>
                    </Alert>
                  </div>
                ) : !openFile ? (
                  <div className="mx-auto max-w-3xl space-y-3 px-6 py-6">
                    <Skeleton className="h-6 w-1/3" />
                    <Skeleton className="h-4 w-full" />
                    <Skeleton className="h-4 w-5/6" />
                  </div>
                ) : mode === "edit" ? (
                  <Textarea
                    value={body}
                    onChange={(e) => onEdit(openFile.id, e.target.value)}
                    onBlur={() => void flush()}
                    placeholder="Write in Markdown… link with [[Note name]]"
                    data-testid="workspace-editor"
                    className="h-full min-h-0 resize-none rounded-none border-0 p-6 font-mono text-sm shadow-none focus-visible:ring-0"
                  />
                ) : (
                  <div className="mx-auto max-w-3xl px-6 py-6" data-testid="workspace-note">
                    {/* The reason, said in the console's own voice and in plain
                        sight (issue #1377). It used to live in a native `title`
                        on the chip — a tooltip that waits a second, never
                        appears on touch, and is never met by anyone who does
                        not think to hover a passive-looking status label. The
                        rendered body often explains itself too, but that text
                        is the ledger's, not ours: a ledger whose template omits
                        it would leave the operator with two words and no
                        reason. */}
                    {readOnlyNote && (
                      <p
                        className="mb-6 flex items-start gap-2 rounded-lg border border-border bg-muted/50 px-3 py-2 text-xs text-muted-foreground"
                        data-testid="workspace-read-only-reason"
                      >
                        <Lock className="mt-0.5 size-3.5 shrink-0" />
                        <span>{DERIVED_REASON}</span>
                      </p>
                    )}
                    <NoteMarkdown source={body} nodes={nodes} onWiki={(t) => void onWiki(t)} />
                  </div>
                )}
              </div>
              {/* Backlinks — computed by the host, not derived here: the tree
                  read carries no bodies, so the client has nothing to scan. */}
              <aside className="hidden w-56 shrink-0 flex-col border-l bg-card/30 xl:flex">
                <div className="border-b px-3 py-2 text-xs font-semibold tracking-wide text-muted-foreground uppercase">
                  Backlinks
                </div>
                <div className="flex-1 overflow-y-auto p-2">
                  {!openFile || openFile.backlinks.length === 0 ? (
                    <p className="px-1 py-2 text-xs text-muted-foreground">No backlinks yet.</p>
                  ) : (
                    openFile.backlinks.map((b) => (
                      <button
                        key={b.id}
                        onClick={() => void open(b.id)}
                        className="flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm hover:bg-accent"
                      >
                        <Link2 className="size-3.5 shrink-0 text-muted-foreground" />
                        <span className="truncate">{titleOf(b)}</span>
                      </button>
                    ))
                  )}
                </div>
              </aside>
            </div>
          </>
        ) : openId && !openNode ? (
          /* The pane used to answer on tree membership rather than on what was
             asked for (issue #1473). Everything — header, error, skeleton —
             lived inside the `openNode` branch, so a `#/workspace/<id>` link to
             a note that had since been deleted skipped the whole thing and fell
             through to "No note open / Pick a note from the explorer", blaming
             the reader for a link they had just followed. The 404 that was
             fetched, parsed and stored was never rendered at all, and the same
             fall-through swallowed the initial load of a deep-linked id. */
          <MissingNote
            loading={loading}
            error={fileError}
            onClear={() => {
              setOpenId(null);
              setFileError(null);
            }}
            onNew={() => setPrompt({ mode: "file" })}
            onToggleExplorer={() => setShowExplorer((s) => !s)}
          />
        ) : (
          <EmptyNote
            variant={hasOperatorContent(nodes) ? "no-selection" : "first-run"}
            onNew={() => setPrompt({ mode: "file" })}
            onNewFolder={() => setPrompt({ mode: "folder" })}
            onToggleExplorer={() => setShowExplorer((s) => !s)}
          />
        )}
      </section>
      </div>

      <NamePrompt
        nodes={nodes}
        state={prompt}
        rosterNames={rosterNames}
        defaultParentId={defaultParentId}
        onClose={() => setPrompt(null)}
        onSubmit={(name) => {
          if (prompt?.mode === "folder") void createFolder(name, prompt.parentId);
          else if (prompt?.mode === "file") void createAndOpen(name, undefined, prompt.parentId);
          else if (prompt?.mode === "rename" && prompt.node) void rename(prompt.node, name);
          setPrompt(null);
        }}
      />
      <MoveDialog
        nodes={nodes}
        rosterNames={rosterNames}
        moving={moving}
        onClose={() => setMoving(null)}
        onMove={(destId) => {
          if (moving) void move(moving, destId);
          setMoving(null);
        }}
      />
      <SweepDialog
        state={sweep}
        rosterNames={rosterNames}
        busy={sweeping}
        onClose={() => setSweep(null)}
        onConfirm={() => void confirmSweep()}
      />
      <RepairDialog
        state={repair}
        nodes={nodes}
        busy={repairing}
        onClose={() => setRepair(null)}
        onConfirm={() => void confirmRepair()}
        onReveal={(id) => {
          setRepair(null);
          const node = nodeById(nodes, id);
          if (node?.kind === "folder") {
            revealFolder(id);
            return;
          }
          // A residual reveal is documented reveal-only (see the doc comment
          // on RepairDialog's `onReveal` prop below). `open()` is not: it
          // flushes the editor's staged draft before it opens anything, so
          // falling through to it here for a file residual could fire a write
          // the operator never asked for (#1498 review). Expand its ancestors
          // and scroll to it, exactly like a folder — opening it is a
          // separate, explicit click.
          setExpanded((prev) => new Set([...prev, ...ancestorFolderIds(nodes, id)]));
          setRevealId(id);
        }}
      />
      <DiscardConfirm
        target={confirmDiscard}
        legacyCount={legacy.length}
        rescuedName={rescued?.name ?? ""}
        onClose={() => setConfirmDiscard(null)}
        onConfirm={confirmDiscard === "rescued" ? discardRescued : discardLegacy}
      />
      <DeleteDialog
        nodes={nodes}
        node={confirmDelete}
        onClose={() => setConfirmDelete(null)}
        onConfirm={(node) => {
          // Close FIRST: remove() can clear openId/openFile out from under a
          // still-mounted dialog (same reasoning as WorkflowsView's delete
          // confirm).
          setConfirmDelete(null);
          void remove(node);
        }}
      />
    </div>
  );
}

/**
 * Where the open note lives, above its name (issue #1371).
 *
 * Rendered as separate clickable crumbs rather than one path string, because a
 * path is only half an answer: the operator who has just discovered that this
 * note is the one under `Rust/API design` almost always wants to see what else
 * is in there, and a string cannot be clicked. Each crumb expands that folder
 * and scrolls the tree to it.
 *
 * Nothing renders at the workspace root — a note with no folders above it has no
 * location worth stating, and an empty crumb rail would be a line of chrome that
 * says "top level" in the space where a real path would go.
 */
function Breadcrumb({
  nodes,
  nodeId,
  onOpenFolder,
}: {
  nodes: FsNode[];
  nodeId: string;
  onOpenFolder: (id: string) => void;
}) {
  const crumbs = breadcrumbOf(nodes, nodeId);
  if (crumbs.length === 0) return null;
  return (
    <span
      className="hidden min-w-0 shrink items-baseline gap-1 text-xs text-muted-foreground sm:flex"
      data-testid="workspace-breadcrumb"
    >
      {crumbs.map((crumb, i) =>
        crumb === null ? (
          <span key={`gap-${i}`} aria-hidden="true">
            …/
          </span>
        ) : (
          <span key={crumb.id} className="flex min-w-0 items-baseline">
            <button
              type="button"
              onClick={() => onOpenFolder(crumb.id)}
              className="truncate rounded-sm hover:text-foreground hover:underline"
            >
              {crumb.name}
            </button>
            <span aria-hidden="true">/</span>
          </span>
        ),
      )}
    </span>
  );
}

/**
 * The editor's save indicator: quiet only on a note nobody has touched.
 *
 * `dirty` carries a filled dot as well as the word, because "Unsaved" and
 * "Saved" are one letter apart in a 12px muted line and the operator is meant
 * to be able to read this at a glance, mid-sentence, without stopping to parse
 * it (issue #1372).
 */
function SaveStatus({ state }: { state: SaveState }) {
  const label = saveStatusLabel(state);
  if (!label) return null;
  return (
    <span
      data-testid="workspace-save-state"
      data-state={state}
      className={cn(
        "flex shrink-0 items-center gap-1.5 text-xs",
        state === "error" ? "text-destructive" : "text-muted-foreground",
        state === "dirty" && "text-foreground",
      )}
    >
      {state === "dirty" && <span aria-hidden="true" className="size-1.5 rounded-full bg-tone-2" />}
      {label}
    </span>
  );
}

/* ---- explorer tree ---- */

interface TreeProps {
  nodes: FsNode[];
  parentId: string | null;
  depth: number;
  expanded: Set<string>;
  openId: string | null;
  /** The node to scroll into view once its row exists (issue #1371). */
  revealId: string | null;
  /** Called by the row that scrolled itself, so the reveal fires once. */
  onRevealed: () => void;
  /** id -> display name for roster ids (issue #973). See {@link isAgentsFolder}. */
  rosterNames: RosterNames;
  onToggle: (id: string) => void;
  onOpen: (id: string) => void;
  onRename: (node: FsNode) => void;
  onMove: (node: FsNode) => void;
  onDelete: (node: FsNode) => void;
  /** Create inside this folder (issue #1477). */
  onNewHere: (folder: FsNode, mode: "file" | "folder") => void;
}

/**
 * `agents/`'s children, sorted by display name rather than the lexical id
 * {@link childrenOf} sorts everywhere else (issue #973). The pre-#686 ULID ids
 * all sort before every readable slug under the plain id ordering, which is
 * not an order an operator can read anything into.
 *
 * Most-recently-modified still comes first here (issue #1687) — `Tree` routes
 * a roster root through this comparator instead of {@link childrenOf}'s, so
 * without its own `updatedAt` check the two visible roots that need id
 * resolution the most would be the two the MRU fix never reached, and stayed
 * alphabetical underneath it.
 */
function sortRosterFolders(items: FsNode[], names: RosterNames): FsNode[] {
  return [...items].sort((a, b) => {
    if (a.kind !== b.kind) return a.kind === "folder" ? -1 : 1;
    if (a.updatedAt != null && b.updatedAt != null) {
      const updatedAt = b.updatedAt - a.updatedAt;
      if (updatedAt !== 0) return updatedAt;
    }
    // Only a roster folder's name is an id worth resolving. A direct file
    // under `agents/` is unusual but not impossible, and its raw name could
    // coincidentally collide with a roster id — that must not reorder it by
    // a display name it was never given one for.
    return a.kind === "folder"
      ? rosterDisplayName(a.name, names).localeCompare(rosterDisplayName(b.name, names))
      : a.name.localeCompare(b.name);
  });
}

function Tree(props: TreeProps) {
  const items = childrenOf(props.nodes, props.parentId);
  const ordered = isRosterRoot(nodeById(props.nodes, props.parentId))
    ? sortRosterFolders(items, props.rosterNames)
    : items;
  return (
    <>
      {ordered.map((node) => (
        <TreeRow key={node.id} node={node} {...props} />
      ))}
    </>
  );
}

/** Badge styling per origin, mirroring `ORIGIN_STYLES` in `api/memory.ts`. */
const ORIGIN_STYLES: Record<WorkspaceOrigin["kind"], string> = {
  agent: "border-tone-3/30 bg-tone-3/10 text-tone-3-text",
  seed: "border-border bg-muted text-muted-foreground",
  operator: "border-border bg-muted text-muted-foreground",
};

/**
 * Who made this note, in the open-file header.
 *
 * Two facts, shown asymmetrically on purpose. The **creator** is the identity of
 * the note and gets a badge — but only when it is worth saying, so a plain
 * operator note stays unadorned rather than every note in the tree wearing a
 * chip. The **last writer** is shown only when it differs from the creator,
 * which is exactly the case issue #326 called out: an agent wrote this and then
 * somebody else edited it (or the reverse). When both are the same the badge
 * already says it.
 */
function Authorship({
  createdBy,
  updatedBy,
  rosterNames,
}: {
  createdBy: WorkspaceOrigin;
  updatedBy: WorkspaceOrigin;
  /** The roster read, so an agent origin reads as a name (issue #1723). */
  rosterNames: RosterNames;
}) {
  const created = originLabel(createdBy, rosterNames);
  const edited = sameOrigin(createdBy, updatedBy)
    ? null
    : originLabel(updatedBy, rosterNames);
  if (!created && !edited) return null;
  return (
    <span className="flex shrink-0 items-center gap-1.5" data-testid="workspace-authorship">
      {created && (
        <Badge variant="outline" className={cn("text-3xs", ORIGIN_STYLES[createdBy.kind])}>
          {created}
        </Badge>
      )}
      {edited && (
        <span className="hidden text-xs text-muted-foreground sm:inline">edited by {edited}</span>
      )}
    </span>
  );
}

function sameOrigin(a: WorkspaceOrigin, b: WorkspaceOrigin): boolean {
  if (a.kind !== b.kind) return false;
  return a.kind !== "agent" || a.id === (b as { id: string }).id;
}

function TreeRow({ node, ...props }: TreeProps & { node: FsNode }) {
  const { depth, expanded, openId, onToggle, onOpen, nodes, rosterNames, revealId, onRevealed } =
    props;
  const rowRef = useRef<HTMLDivElement | null>(null);
  // Scrolling belongs to the row rather than to the pane because only the row
  // knows when it exists: the ancestors expand first, and the row this reveal
  // is about is mounted by that render, not the one that asked for it
  // (issue #1371). `block: "nearest"` so a row already on screen — the ordinary
  // case, a click in the tree — does not move at all.
  useEffect(() => {
    if (revealId !== node.id) return;
    rowRef.current?.scrollIntoView({ block: "nearest" });
    onRevealed();
  }, [revealId, node.id, onRevealed]);
  const isFolder = node.kind === "folder";
  const isOpen = expanded.has(node.id);
  const active = node.id === openId;
  // A direct child of `agents/` or `artifacts/` is named by roster id — its
  // real folder path, and the identity every artifact it holds is stamped with
  // — but an operator recognizes the teammate by name, not by that id (issue
  // #973). The id stays the label everywhere else in the tree: it is only ever
  // a roster id one level below one of those two roots.
  const isRosterFolder = isFolder && isRosterRoot(nodeById(nodes, node.parentId));
  const displayName = isRosterFolder ? rosterDisplayName(node.name, rosterNames) : node.name;
  /**
   * The provenance pill for this row, resolved — or `null` when there is
   * nothing worth saying (issue #1723).
   *
   * Suppressed inside the author's own `agents/`/`artifacts/` subtree, where
   * the enclosing folder already attributes everything under it: on the
   * teammate's own folder the pill would repeat the row's own label back
   * verbatim, and on the `<task>/` folders and files beneath it, the same pill
   * four times down the pane — each one taking the width the name needs.
   * Everywhere else it is the one place the tree says who wrote a node, which
   * is the whole of #326's marker, and a node one teammate wrote inside
   * another's folder still wears it.
   */
  const owner = rosterOwnerOf(nodes, node.id);
  const agentBadge =
    node.createdBy.kind === "agent" &&
    (owner === undefined || rosterIdKey(owner) !== rosterIdKey(node.createdBy.id))
      ? { id: node.createdBy.id, name: rosterDisplayName(node.createdBy.id, rosterNames) }
      : null;
  /** What this row is actually called on screen. */
  const label = isFolder ? displayName : titleOf(node);
  /**
   * Whether the name is being cut off (issue #1459).
   *
   * A row is `truncate` inside a fixed 256px pane, indented 12px per level, so
   * by depth 5 there are about 22 characters of room — and the seeded tree
   * ellipsises six rows out of the box. There was no tooltip, no wrap, no row
   * scroll and no resize handle: the only way to read a name was to open a row
   * you could not identify.
   *
   * Measured rather than guessed, so the ordinary short name gets no hover
   * chrome at all. `title` below is the unconditional fallback — it costs
   * nothing, works on touch and for assistive tech, and means the fix does not
   * depend on this measurement having run.
   */
  const nameRef = useRef<HTMLSpanElement | null>(null);
  const [clipped, setClipped] = useState(false);
  useEffect(() => {
    const el = nameRef.current;
    if (!el) return;
    const measure = () => setClipped(el.scrollWidth > el.clientWidth);
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [label]);
  /**
   * Whether this row is the `derived/` folder or something inside it (#1377).
   *
   * The tree is where a person decides what to open, so it is where "this one
   * is not yours to edit" has to be readable. Before this, `derived/goals.md`
   * rendered identically to a hand-written note — same icon, same weight, same
   * `…` menu offering Rename and Move — and the only console-authored signal
   * was a chip in the header of a file you had already opened.
   *
   * Recomputed per row rather than threaded down through {@link Tree}, so the
   * rule stays the single {@link isDerivedNode} both this and the header ask.
   * It walks the ancestry of one node, against a tree that is a few hundred
   * nodes at most and is already re-sorted per level on every render.
   */
  const derived = isDerivedNode(nodes, node.id);
  /**
   * Whether this row is the `secrets/` folder or something inside it (#1465).
   *
   * The tree is where an operator decides where to put a credential, and until
   * this shipped it was the one place that decision got no help: `secrets`
   * rendered with the same folder icon, the same weight and the same `…` menu
   * as `Playbooks`, and sorted in among them. The only statement of the rule
   * was a README seeded inside the folder — reachable only by someone who had
   * already found it.
   *
   * Recomputed per row rather than threaded down through {@link Tree}, for the
   * same reason `derived` above is: the rule stays one function, asked by every
   * surface, against a tree of a few hundred nodes that is already re-sorted
   * per level on every render.
   *
   * Never true at the same time as `derived`: both read the first ancestor, and
   * a node has one.
   */
  const secret = isSecretNode(nodes, node.id);

  return (
    <>
      <div
        ref={rowRef}
        className={cn(
          "group flex items-center gap-1 rounded-md px-1.5 py-0 text-sm md:py-1",
          active ? "bg-accent font-medium" : "hover:bg-accent/50",
          // Muted whether or not it is the open note: neither what writes the
          // file (#1377) nor who can read it (#1465) changes when you select
          // it. The glyph after the name is what says which of the two it is.
          (derived || secret) && "text-muted-foreground",
        )}
        style={{ paddingLeft: 6 + depth * 12 }}
      >
        <button
          onClick={() => (isFolder ? onToggle(node.id) : onOpen(node.id))}
          className="flex min-h-6 min-w-0 flex-1 items-center gap-1.5 text-left md:min-h-0"
        >
          {isFolder ? (
            <>
              {isOpen ? (
                <ChevronDown className="size-3.5 shrink-0 text-muted-foreground" />
              ) : (
                <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />
              )}
              {isOpen ? (
                <FolderOpen className="size-4 shrink-0 text-tone-2" />
              ) : (
                <Folder className="size-4 shrink-0 text-tone-2" />
              )}
            </>
          ) : (
            <FileText className="ml-3.5 size-4 shrink-0 text-muted-foreground" />
          )}
          {/* The roster case keeps showing the raw id on `title`, which is
              how #973 left the folder's real name reachable; every other row
              now carries its own full name there instead of `undefined`. */}
          <Tooltip open={clipped ? undefined : false}>
            <TooltipTrigger
              render={
                <span
                  ref={nameRef}
                  className="truncate"
                  title={isRosterFolder ? node.name : label}
                  data-testid="workspace-tree-name"
                />
              }
            >
              {label}
            </TooltipTrigger>
            <TooltipContent>
              {label}
              {isRosterFolder && <span className="block text-3xs opacity-70">{node.name}</span>}
            </TooltipContent>
          </Tooltip>
          {/* The glyph is the whole of the tree-side signal: an icon, not a
              badge, because a row in a 256px pane has no width for a phrase and
              the name is the thing being scanned. The label rides along for a
              screen reader, which gets no glyph, and the full reason is on the
              `title` for a pointer.

              A lock means `derived/` — "you may not write this" (#1377). An
              eye-off means `secrets/` — the other rule: you may write it, and
              no agent reads it (#1465). Two rules, two glyphs, and no row ever
              wears both. */}
          {derived && (
            <span
              className="flex shrink-0 items-center"
              title={DERIVED_REASON}
              data-testid="workspace-tree-derived"
            >
              <Lock className="size-3" aria-hidden />
              <span className="sr-only">{DERIVED_LABEL}</span>
            </span>
          )}
          {secret && (
            <span
              className="flex shrink-0 items-center"
              title={SECRETS_REASON}
              data-testid="workspace-tree-secret"
            >
              <EyeOff className="size-3" aria-hidden />
              <span className="sr-only">{SECRETS_LABEL}</span>
            </span>
          )}
          {/* Agent-created nodes get a marker in the tree itself, so "what has
              the company been writing" is answerable by scanning rather than by
              opening each note. Only the agent case — badging the operator's
              own notes back at them says nothing.

              The pill reads the teammate's NAME, through the same
              `rosterDisplayName` the row label one line up already goes through
              (issue #1723). It used to print the raw roster handle —
              `seo_specialist` beside a row already labelled "SEO Specialist" —
              which is #1688's and #1369's leak on the one surface those fixes
              did not cover. The handle is still reachable, on `title`, because
              it is the folder's real name and the identity every artifact is
              stamped with. */}
          {agentBadge && (
            <Badge
              variant="outline"
              className={cn("shrink-0 px-1 py-0 text-3xs", ORIGIN_STYLES.agent)}
              title={`Created by teammate ${agentBadge.id}`}
              data-testid="workspace-tree-agent-badge"
            >
              {agentBadge.name}
            </Badge>
          )}
        </button>
        <DropdownMenu>
          <DropdownMenuTrigger
            render={
              <Button
                variant="ghost"
                size="icon"
                className="size-6 opacity-0 group-hover:opacity-100 data-[popup-open]:opacity-100"
                aria-label="Actions"
              />
            }
          >
            <MoreHorizontal className="size-3.5" />
          </DropdownMenuTrigger>
          {/* `w-auto` for the derived menu only: the popup is pinned to
              `w-(--anchor-width)` — the width of the tiny `…` button — with a
              128px floor, and the heading below is wider than that. Every other
              menu in the tree fits the floor. */}
          <DropdownMenuContent align="end" className={derived ? "w-auto" : undefined}>
            {/* Exactly the actions the host will accept, which is not the same
                set the issue asked for (#1377 said drop all three).
                `DerivedGuardWorkspace::rename_move` refuses both ends — moving a
                derived file out strands one the next derivation recreates, and
                moving a note *in* puts a hand-written file in the folder whose
                meaning is that nothing in it is hand-written — so Rename and
                Move are controls whose only outcome is an error toast.

                `delete` is deliberately unguarded there, and the module says
                why: nothing is silently lost, the next write re-derives the
                file, and a retired ledger's stale file has to be clearable by
                somebody. Removing it here would take away the one remedy the
                host actually offers. The heading is what explains the short
                menu — an absence on its own reads as a broken menu. */}
            {derived ? (
              // Grouped because `DropdownMenuLabel` is Base UI's
              // `Menu.GroupLabel`, which throws outside a `Menu.Group`.
              <DropdownMenuGroup>
                <DropdownMenuLabel className="flex items-center gap-1.5 font-normal whitespace-nowrap">
                  <Lock className="size-3 shrink-0" />
                  {DERIVED_LABEL}
                </DropdownMenuLabel>
                <DropdownMenuSeparator />
                <DropdownMenuItem variant="destructive" onClick={() => props.onDelete(node)}>
                  Delete
                </DropdownMenuItem>
              </DropdownMenuGroup>
            ) : (
              <>
                {/* The tree is where an operator decides where a note belongs,
                    and until now it was the one place that could not make one
                    (issue #1477). The toolbar's New note has no idea which
                    folder is on screen; this does. */}
                {isFolder && (
                  <>
                    <DropdownMenuItem onClick={() => props.onNewHere(node, "file")}>
                      New note here
                    </DropdownMenuItem>
                    <DropdownMenuItem onClick={() => props.onNewHere(node, "folder")}>
                      New folder here
                    </DropdownMenuItem>
                    <DropdownMenuSeparator />
                  </>
                )}
                <DropdownMenuItem onClick={() => props.onRename(node)}>Rename</DropdownMenuItem>
                <DropdownMenuItem onClick={() => props.onMove(node)}>Move to…</DropdownMenuItem>
                <DropdownMenuSeparator />
                <DropdownMenuItem variant="destructive" onClick={() => props.onDelete(node)}>
                  Delete
                </DropdownMenuItem>
              </>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
      {isFolder && isOpen && <Tree {...props} parentId={node.id} depth={depth + 1} />}
    </>
  );
}

/* ---- note markdown with wiki links ---- */

/**
 * A binary workspace node: rendered if it is an image, described and offered
 * for download if it is not (issue #553).
 *
 * # Why the bytes are fetched rather than linked
 *
 * The blob route needs the bearer token the API client holds, and an `<img
 * src>` cannot carry an `Authorization` header — so a direct link would 401 for
 * every operator. The bytes come through the authenticated client and become an
 * object URL the element can point at.
 *
 * The URL is revoked on unmount and whenever the node changes. An object URL is
 * a document-lifetime reference to the blob behind it, so a view that minted one
 * per opened image without revoking would hold every image the operator had
 * looked at, in memory, until the tab was closed.
 *
 * A non-image is deliberately **not** previewed. The console has no viewer for a
 * PDF or a zip, and a browser plugin rendering one inside the app frame is not
 * something this view can promise across browsers — so it shows what the file
 * is, exactly, and hands over the download.
 */
function BinaryNodeView({
  client,
  company,
  node,
}: {
  client: OpenCompanyClient;
  company: string | null;
  node: FsNode;
}) {
  const [url, setUrl] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const isImage = (node.mime ?? "").startsWith("image/");
  const blobKey = blobCacheKey(node);

  useEffect(() => {
    let revoked = false;
    let current: string | null = null;
    setUrl(null);
    setError(null);
    fetchBlobUrl(client, company, node.id)
      .then((next) => {
        // The effect may have been torn down (or the node switched) while the
        // fetch was in flight; revoking immediately is what stops that race
        // from leaking the blob it just created.
        if (revoked) {
          URL.revokeObjectURL(next);
          return;
        }
        current = next;
        setUrl(next);
      })
      .catch((e) => {
        if (!revoked) setError(message(e, "could not load this file"));
      });
    return () => {
      revoked = true;
      if (current) URL.revokeObjectURL(current);
    };
    // `blobKey`, not `node.id`, is what makes a re-publish visible: it folds in
    // the digest, so bytes replaced in place re-fetch (issue #669). The rule
    // lives in `blobCacheKey` and is asserted there.
  }, [client, company, node.id, blobKey]);

  return (
    <div className="mx-auto max-w-3xl px-6 py-6" data-testid="workspace-binary">
      {error ? (
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      ) : isImage ? (
        url ? (
          <img
            src={url}
            alt={node.name}
            data-testid="workspace-image"
            className="max-h-[70vh] w-auto max-w-full rounded-md border bg-card object-contain"
          />
        ) : (
          <Skeleton className="h-64 w-full" />
        )
      ) : null}
      <div className="mt-4 rounded-md border bg-card/40 p-4" data-testid="workspace-binary-meta">
        <p className="text-sm font-medium">{node.name}</p>
        <dl className="mt-2 grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-xs text-muted-foreground">
          <dt>Type</dt>
          <dd className="font-mono">{node.mime}</dd>
          <dt>Size</dt>
          <dd>{formatBytes(node.size)}</dd>
          {node.sha256 && (
            <>
              <dt>sha256</dt>
              {/* Wrapped, not truncated: a digest an operator cannot read in
                  full cannot be compared against anything, which is the only
                  reason to show one. */}
              <dd className="font-mono break-all">{node.sha256}</dd>
            </>
          )}
        </dl>
        <p className="mt-3 text-xs text-muted-foreground">
          This file is stored as data, so it has no text to edit here.
        </p>
        {url && (
          // A real anchor rather than a Button with an onClick: `download` on
          // an <a> is what makes the browser save the file under the node's own
          // name instead of navigating to a blob: URL.
          <a
            href={url}
            download={node.name}
            data-testid="workspace-download"
            className="mt-3 inline-flex items-center rounded-md border px-3 py-1.5 text-xs font-medium hover:bg-accent"
          >
            <Download className="mr-1 size-4" />
            Download
          </a>
        )}
      </div>
    </div>
  );
}

function NoteMarkdown({
  source,
  nodes,
  onWiki,
}: {
  source: string;
  nodes: FsNode[];
  onWiki: (target: string) => void;
}) {
  if (!source.trim()) {
    return (
      <p className="text-sm text-muted-foreground">This note is empty. Switch to Edit to write.</p>
    );
  }
  // Rewrite [[target]] / [[target|alias]] into links the renderer can style —
  // but leave fenced and inline code untouched (so `[[…]]` examples survive).
  const rewritten = source.replace(
    /(```[\s\S]*?```|~~~[\s\S]*?~~~|`[^`\n]*`)|\[\[([^\]|]+)(?:\|([^\]]+))?\]\]/g,
    (_m, code: string | undefined, target: string, alias?: string) =>
      code ? code : `[${(alias ?? target).trim()}](#wiki:${encodeURIComponent(target.trim())})`,
  );
  return (
    <div className="prose prose-sm max-w-none dark:prose-invert">
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          a({ href, children }) {
            if (href?.startsWith("#wiki:")) {
              const target = decodeURIComponent(href.slice("#wiki:".length));
              const exists = Boolean(fileByTitle(nodes, target));
              return (
                <button
                  type="button"
                  onClick={() => onWiki(target)}
                  className={cn(
                    "rounded px-0.5 font-medium no-underline",
                    exists
                      ? "text-primary hover:underline"
                      : "text-muted-foreground underline decoration-dashed underline-offset-2",
                  )}
                >
                  {children}
                </button>
              );
            }
            return (
              <a href={href} target="_blank" rel="noreferrer">
                {children}
              </a>
            );
          },
        }}
      >
        {rewritten}
      </ReactMarkdown>
    </div>
  );
}

/**
 * The pane for an id that is not in the tree (issue #1473).
 *
 * Three states an idle "No note open" used to swallow whole: the tree is still
 * arriving, the read came back with an error, or the read is done and the note
 * genuinely is not there. All three follow from `openId` — something *was*
 * asked for — which is the distinction the old branch could not make, because
 * it keyed on whether the node was in the tree.
 *
 * The copy says nothing about *why* the note is gone. A link can go stale
 * because the note was deleted, because it was in another company, or because
 * the tree read failed — and this pane cannot tell those apart. Naming a cause
 * it does not know would be worse than the fall-through it replaces.
 */
function MissingNote({
  loading,
  error,
  onClear,
  onNew,
  onToggleExplorer,
}: {
  loading: boolean;
  error: string | null;
  onClear: () => void;
  onNew: () => void;
  onToggleExplorer: () => void;
}) {
  return (
    <div className="flex flex-1 flex-col" data-testid="workspace-missing-note">
      <div className="flex items-center border-b px-3 py-2 md:hidden">
        <IconBtn label="Toggle explorer" onClick={onToggleExplorer}>
          <PanelLeft className="size-4" />
        </IconBtn>
      </div>
      {loading ? (
        // The skeleton was trapped inside the `openNode` branch, so a
        // deep-linked id showed the idle empty state for the whole of the tree
        // read and then changed its mind.
        <div
          className="mx-auto w-full max-w-3xl space-y-3 px-6 py-6"
          data-testid="workspace-missing-loading"
        >
          <Skeleton className="h-6 w-1/3" />
          <Skeleton className="h-4 w-full" />
          <Skeleton className="h-4 w-5/6" />
        </div>
      ) : (
        <div className="flex flex-1 flex-col items-center justify-center gap-3 px-6 text-center">
          <FileX className="size-8 text-muted-foreground" />
          <div className="space-y-1">
            <p className="font-medium">That note is no longer in this workspace</p>
            <p className="max-w-md text-sm text-muted-foreground">
              The link you followed points at a note this company does not have.
            </p>
            {/* The stored read error, finally rendered. It is the host's own
                sentence about this id and it was being thrown away. */}
            {error && (
              <p
                className="max-w-md text-sm text-muted-foreground"
                data-testid="workspace-missing-error"
              >
                {error}
              </p>
            )}
          </div>
          <div className="flex flex-wrap justify-center gap-2">
            <Button variant="outline" size="sm" onClick={onClear}>
              Back to the explorer
            </Button>
            <Button variant="outline" size="sm" onClick={onNew}>
              <FilePlus2 className="size-4" /> New note
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}

/**
 * The pane when no note is open — in its two genuinely different situations
 * (issues #1380, #1481).
 *
 * There used to be one. "No note open / Pick a note from the explorer, or
 * create one" is right for an operator with sixty notes who has not clicked
 * one; it is wrong on a fresh company, where the explorer holds three rows the
 * host scaffolded and the operator has no reason to open any of them. The
 * explorer meanwhile carried a second, contradicting message — "This workspace
 * is empty. Create a note to start." — on a `nodes.length === 0` branch that
 * the boot-time scaffold makes unreachable.
 *
 * So the note pane owns the messaging, and the first-run variant finally says
 * what this tree *is*. That premise — the workspace is shared with the
 * company's agents, who read what is written here and write back into it —
 * existed only in code comments in three files, none of them rendered. An
 * operator could not learn from this screen that anyone but themselves would
 * ever read it.
 */
function EmptyNote({
  variant,
  onNew,
  onNewFolder,
  onToggleExplorer,
}: {
  variant: "first-run" | "no-selection";
  onNew: () => void;
  onNewFolder: () => void;
  onToggleExplorer: () => void;
}) {
  const firstRun = variant === "first-run";
  return (
    <div
      className="flex flex-1 flex-col"
      data-testid={`workspace-empty-${variant}`}
    >
      <div className="flex items-center border-b px-3 py-2 md:hidden">
        <IconBtn label="Toggle explorer" onClick={onToggleExplorer}>
          <PanelLeft className="size-4" />
        </IconBtn>
      </div>
      <div className="flex flex-1 flex-col items-center justify-center gap-3 px-6 text-center">
        <FileText className="size-8 text-muted-foreground" />
        {firstRun ? (
          <>
            <div className="max-w-md space-y-2">
              <p className="font-medium">Your company&rsquo;s shared notes</p>
              <p className="text-sm text-muted-foreground">
                Everyone here reads this tree — your teammates and the
                company&rsquo;s agents alike. What you write is what they work
                from on their next turn, and the notes they write show up here
                beside yours.
              </p>
              <p className="text-sm text-muted-foreground">
                A <span className="font-mono text-xs">derived/</span> folder
                appears on its own once a ledger has rows. Nobody edits that one
                — it is rewritten from the ledger.
              </p>
            </div>
            <div className="flex flex-wrap justify-center gap-2">
              <Button variant="outline" size="sm" onClick={onNew}>
                <FilePlus2 className="size-4" /> New note
              </Button>
              <Button variant="outline" size="sm" onClick={onNewFolder}>
                <FolderPlus className="size-4" /> New folder
              </Button>
            </div>
          </>
        ) : (
          <>
            <div className="space-y-1">
              <p className="font-medium">No note open</p>
              <p className="text-sm text-muted-foreground">
                Pick a note from the explorer, or create one.
              </p>
            </div>
            <Button variant="outline" size="sm" onClick={onNew}>
              <FilePlus2 className="size-4" /> New note
            </Button>
          </>
        )}
      </div>
    </div>
  );
}

/**
 * An icon-only control in the explorer header, labelled on hover (issue #1378).
 *
 * The header is six of these in a row — two of which delete things — and the
 * label existed only as `aria-label`, which is to say only for a screen reader.
 * A sighted operator had six identical grey glyphs and no way to learn what any
 * of them did short of pressing it, in a row where two presses are destructive.
 *
 * The tooltip renders the same string the `aria-label` carries rather than a
 * second, longer explanation: one label, two ways of meeting it. `TooltipProvider`
 * is mounted globally (`main.tsx`), so this costs nothing at each site.
 */
function IconBtn({
  label,
  onClick,
  children,
  ...rest
}: {
  label: string;
  onClick: () => void;
  children: React.ReactNode;
} & Omit<React.ComponentProps<typeof Button>, "onClick" | "children">) {
  return (
    <Tooltip>
      <TooltipTrigger
        render={
          <Button
            variant="ghost"
            size="icon"
            className="size-7 text-muted-foreground"
            aria-label={label}
            onClick={onClick}
            {...rest}
          />
        }
      >
        {children}
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  );
}

/* ---- dialogs ---- */

interface PromptState {
  mode: "folder" | "file" | "rename";
  node?: FsNode;
  /**
   * Where a create lands (issue #1477).
   *
   * `undefined` means "wherever the view decides"; an explicit value is a
   * destination the operator named — including `null`, the workspace root,
   * which is a real answer and not the absence of one.
   */
  parentId?: string | null;
}

/**
 * Name a new node, or rename one — and stop first when the new name is what
 * decides who can read it (issue #1465).
 *
 * `Move to…` was not the only control that crosses the `secrets/` boundary.
 * The host's rule is the *first path segment*, so renaming the root `secrets/`
 * folder rewrites that segment for everything under it; the host accepts the
 * rename (`PATCH …/workspace/<id>` with a new `name` answers 200 — only
 * `derived/` is guarded there), and until this it did so on one click and a
 * toast. Same boundary, same consequence, so the same panel and the same words
 * as {@link MoveDialog}.
 */
function NamePrompt({
  nodes,
  state,
  rosterNames,
  defaultParentId,
  onClose,
  onSubmit,
}: {
  nodes: FsNode[];
  state: PromptState | null;
  rosterNames: RosterNames;
  /** Where a create with no named destination will land (issue #1477). */
  defaultParentId: string | null;
  onClose: () => void;
  onSubmit: (name: string) => void;
}) {
  const [name, setName] = useState("");
  /** The typed name, held back until the audience warning is acknowledged. */
  const [pending, setPending] = useState<{ name: string; warning: MoveAudienceWarning } | null>(
    null,
  );

  useEffect(() => {
    setName(state?.mode === "rename" ? (state.node?.name ?? "") : "");
    // A second Rename must not open onto the previous one's warning.
    setPending(null);
  }, [state]);

  /**
   * Submit, unless the name changes the audience — in which case ask first.
   *
   * Only a rename can: a *new* node is created empty, so naming one `secrets`
   * hides nothing that was not already nothing.
   */
  function submit(next: string) {
    if (state?.mode !== "rename" || !state.node) {
      onSubmit(next);
      return;
    }
    const warning = renameAudienceWarning(nodes, state.node, next);
    if (!warning) {
      onSubmit(next);
      return;
    }
    setPending({ name: next, warning });
  }

  const title =
    state?.mode === "folder" ? "New folder" : state?.mode === "file" ? "New note" : "Rename";

  return (
    <Dialog open={Boolean(state)} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>
            {pending
              ? "This changes who can read it."
              : state?.mode === "file"
                ? "Notes get a .md extension automatically."
                : "Give it a name."}
          </DialogDescription>
          {/* Where it will land, before it lands (issue #1477). Every create
              used to go to the root regardless of what the operator had open,
              and said so nowhere — the note simply appeared somewhere else. */}
          {state && state.mode !== "rename" && (
            <p className="text-xs text-muted-foreground" data-testid="workspace-prompt-dest">
              Goes in{" "}
              <span className="font-medium text-foreground">
                {(() => {
                  const parent = state.parentId === undefined ? defaultParentId : state.parentId;
                  return parent
                    ? folderPathLabel(nodes, parent, rosterNames)
                    : "the workspace root";
                })()}
              </span>
              .
            </p>
          )}
        </DialogHeader>
        {pending ? (
          <MoveAudienceConfirm
            warning={pending.warning}
            // Back to the field rather than out of the dialog: the operator who
            // reads the warning and changes their mind wanted a different name,
            // not to abandon the rename.
            onCancel={() => setPending(null)}
            onConfirm={() => onSubmit(pending.name)}
          />
        ) : (
          <>
            <div className="grid gap-2">
              <Label htmlFor="fs-name">Name</Label>
              <Input
                id="fs-name"
                autoFocus
                value={name}
                onChange={(e) => setName(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && name.trim()) submit(name);
                }}
                placeholder={state?.mode === "folder" ? "e.g. Campaigns" : "e.g. Notes"}
              />
            </div>
            <DialogFooter>
              <Button variant="ghost" onClick={onClose}>
                Cancel
              </Button>
              <Button disabled={!name.trim()} onClick={() => submit(name)}>
                {state?.mode === "rename" ? "Rename" : "Create"}
              </Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

/**
 * Pick where a note or folder goes (issues #1381, #1477, #1465).
 *
 * Four filing defects, each on its own sufficient to mis-file something. The
 * list showed bare `name`, so two `Drafts` under different parents were
 * identical rows and a roster folder was a raw ULID. It was in the host's
 * unspecified `tree()` order, beside a tree that was sorted. It offered
 * `derived/`, where the host refuses every write (#1222), so picking it could
 * only ever produce an error toast. And a single click committed the move — no
 * Move button, no Cancel, no undo, and a success toast that did not name where
 * the note went.
 *
 * So: full paths, tree order, `derived/` excluded, and select-then-confirm with
 * the footer the sibling dialogs already have. A click still *chooses*; it no
 * longer *commits*.
 *
 * On top of that, one destination is not a filing decision at all. Moving into
 * or out of `secrets/` changes who can read the note, and #1465 made that
 * legible twice over: the row is marked before it is picked, and pressing Move
 * hands off to {@link MoveAudienceConfirm} rather than moving — the confirm
 * step the ordinary destinations spend on a plain Move button is spent naming
 * the consequence instead.
 */
function MoveDialog({
  nodes,
  rosterNames,
  moving,
  onClose,
  onMove,
}: {
  nodes: FsNode[];
  rosterNames: RosterNames;
  moving: FsNode | null;
  onClose: () => void;
  onMove: (destId: string | null) => void;
}) {
  // `undefined` is "nothing picked yet"; `null` is the workspace root, which is
  // a real destination and must not read as no answer.
  const [picked, setPicked] = useState<string | null | undefined>(undefined);
  const [filter, setFilter] = useState("");
  /** The destination chosen, held back until its warning is acknowledged. */
  const [pending, setPending] = useState<{
    destId: string | null;
    warning: MoveAudienceWarning;
  } | null>(null);

  // A second `Move to…` must not open onto the previous one's selection or its
  // warning.
  useEffect(() => {
    if (moving) {
      setPicked(undefined);
      setFilter("");
      setPending(null);
    }
  }, [moving]);

  const blocked = new Set<string>(moving ? subtreeIds(nodes, moving.id) : []);
  for (const node of nodes) if (isDerivedNode(nodes, node.id)) blocked.add(node.id);

  const needle = filter.trim().toLowerCase();
  const destinations = sortedFolders(nodes, blocked)
    .map((folder) => ({
      folder,
      label: folderPathLabel(nodes, folder.id, rosterNames),
      audience: moving ? moveAudienceWarning(nodes, moving, folder.id)?.change : undefined,
    }))
    .filter(({ label }) => !needle || label.toLowerCase().includes(needle));

  const here = (id: string | null) => moving?.parentId === (id ?? null);

  /** Commit, or hand off to the warning when the audience changes. */
  function confirm() {
    if (picked === undefined || !moving) return;
    const warning = moveAudienceWarning(nodes, moving, picked);
    if (!warning) {
      onMove(picked);
      return;
    }
    setPending({ destId: picked, warning });
  }

  return (
    <Dialog open={Boolean(moving)} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Move “{moving ? titleOf(moving) : ""}”</DialogTitle>
          <DialogDescription>
            {pending
              ? "This changes who can read it."
              : "Pick a destination folder, then choose Move. Nothing moves until you do."}
          </DialogDescription>
        </DialogHeader>
        {pending ? (
          <MoveAudienceConfirm
            warning={pending.warning}
            // Back to the list rather than out of the dialog: the operator who
            // reads the warning and changes their mind wanted a different
            // folder, not to abandon the move.
            onCancel={() => setPending(null)}
            onConfirm={() => onMove(pending.destId)}
          />
        ) : (
          <>
            <Input
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              placeholder="Filter destinations…"
              aria-label="Filter destinations"
              data-testid="workspace-move-filter"
              className="h-8 text-sm"
            />
            <div className="max-h-72 space-y-1 overflow-y-auto" data-testid="workspace-move-list">
              {!needle && (
                <DestRow
                  label="Workspace root"
                  disabled={here(null)}
                  selected={picked === null}
                  // Marked from the same predicate that raises the warning, so
                  // the consequence is legible before the pick as well as
                  // after it. The root is never `secrets/`, so this row only
                  // ever marks the way out.
                  audience={moving ? moveAudienceWarning(nodes, moving, null)?.change : undefined}
                  onClick={() => setPicked(null)}
                />
              )}
              {destinations.map(({ folder, label, audience }) => (
                <DestRow
                  key={folder.id}
                  label={label}
                  disabled={here(folder.id)}
                  selected={picked === folder.id}
                  audience={audience}
                  onClick={() => setPicked(folder.id)}
                />
              ))}
              {destinations.length === 0 && (
                <p className="px-2.5 py-2 text-sm text-muted-foreground">
                  No folder matches “{filter.trim()}”.
                </p>
              )}
            </div>
            <DialogFooter>
              <Button variant="ghost" onClick={onClose}>
                Cancel
              </Button>
              <Button
                disabled={picked === undefined}
                onClick={confirm}
                data-testid="workspace-move-confirm"
              >
                Move
              </Button>
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}

/** Which last-copy-destroying "Discard" the confirm is standing in front of. */
type DiscardTarget = "legacy" | "rescued";

/**
 * The confirm both workspace "Discard" buttons now sit behind (issue #1472).
 *
 * Neither of these deletes has an undo and neither has a second copy anywhere.
 * The scratchpad discard removes the adopted key *and* its pre-connection
 * origin, so the notes can never be re-offered; the rescued-text discard drops
 * the only remaining copy of words whose note is already gone. Both were plain
 * ghost buttons — quieter than the {@link DeleteDialog} that guards deleting a
 * note the host still holds, which is strictly the more recoverable act.
 *
 * Shares {@link DeleteDialog}'s shape and its solid-destructive action rather
 * than inventing a second confirm style, so "this cannot be undone" looks the
 * same everywhere in the view. The copy names the quantity, because a confirm
 * that says only "are you sure?" is one an operator learns to click through.
 */
function DiscardConfirm({
  target,
  legacyCount,
  rescuedName,
  onClose,
  onConfirm,
}: {
  target: DiscardTarget | null;
  legacyCount: number;
  rescuedName: string;
  onClose: () => void;
  onConfirm: () => void;
}) {
  const rescued = target === "rescued";
  const title = rescued
    ? "Discard your unsaved text?"
    : `Delete ${legacyCount} note${legacyCount === 1 ? "" : "s"} kept only in this browser?`;
  const description = rescued
    ? `This is the last copy of what you wrote in “${rescuedName}”. That note is already gone, so nothing else holds this text. There is no undo.`
    : "These notes were never sent to the company workspace, so this browser holds the only copy. Deleting them cannot be undone — “Not now” leaves them alone.";

  return (
    <AlertDialog open={Boolean(target)} onOpenChange={(o) => !o && onClose()}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>{description}</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Keep it</AlertDialogCancel>
          <AlertDialogAction
            onClick={onConfirm}
            className="bg-destructive text-white hover:bg-destructive/90"
            data-testid="workspace-discard-confirm"
          >
            {rescued ? "Discard text" : "Delete notes"}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

/**
 * Delete confirmation for a note or folder (issue #1255).
 *
 * A folder's delete recursively takes every note nested inside it in the same
 * API call, so the dialog names exactly what that means — the file/folder
 * counts under it, from {@link subtreeCounts} — rather than a bare "are you
 * sure?" that reads the same for an empty folder and one holding a hundred
 * notes.
 */
function DeleteDialog({
  nodes,
  node,
  onClose,
  onConfirm,
}: {
  nodes: FsNode[];
  node: FsNode | null;
  onClose: () => void;
  onConfirm: (node: FsNode) => void;
}) {
  const counts = node ? subtreeCounts(nodes, node.id) : { files: 0, folders: 0 };
  const description =
    node?.kind === "file"
      ? "This permanently deletes this note. There is no undo."
      : counts.files === 0 && counts.folders === 0
        ? "This folder is empty. Deleting it can’t be undone."
        : `This folder and everything inside it — ${describeCounts(counts)} — will be permanently deleted. There is no undo.`;

  return (
    <AlertDialog open={Boolean(node)} onOpenChange={(o) => !o && onClose()}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>Delete “{node ? titleOf(node) : ""}”?</AlertDialogTitle>
          <AlertDialogDescription>{description}</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Keep it</AlertDialogCancel>
          <AlertDialogAction
            onClick={() => node && onConfirm(node)}
            className="bg-destructive text-white hover:bg-destructive/90"
            data-testid="workspace-delete-confirm"
          >
            Delete {node?.kind === "folder" ? "folder" : "note"}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

/** "3 notes and 1 folder" / "2 notes" / "1 folder" — for {@link DeleteDialog}. */
function describeCounts(counts: { files: number; folders: number }): string {
  const parts: string[] = [];
  if (counts.files > 0) parts.push(`${counts.files} note${counts.files === 1 ? "" : "s"}`);
  if (counts.folders > 0) parts.push(`${counts.folders} folder${counts.folders === 1 ? "" : "s"}`);
  return parts.join(" and ");
}

/**
 * The empty-agent-folder tidy, in its two stages (issue #700).
 *
 * `preview` asks; `done` reports. Both list the folders by name, which is the
 * point of the dialog rather than a nicety: an operator who disagrees with the
 * sweep can only say so if they can see what it means to take, and can only
 * check it afterwards if they are told what it took. "17 empty folders" is a
 * number nobody can verify.
 */
interface SweepState {
  stage: "preview" | "done";
  folders: SweptFolder[];
}

function SweepDialog({
  state,
  rosterNames,
  busy,
  onClose,
  onConfirm,
}: {
  state: SweepState | null;
  /** The roster the swept folder names are ids in (issue #1479). */
  rosterNames: RosterNames;
  busy: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  const done = state?.stage === "done";
  const count = state?.folders.length ?? 0;
  // Resolve, then sort by what is on screen (issue #1479). A swept folder's
  // `name` *is* a roster id — that is what `Agents/<id>/` folders are called —
  // so the dialog asking an operator to approve seven removals was showing
  // seven ULIDs, in whatever order the host's tree() happened to return, in the
  // one view that already resolves those ids in the tree behind the modal.
  const rows = (state?.folders ?? [])
    .map((folder) => {
      const display = rosterDisplayName(folder.name, rosterNames);
      // Membership, not a string compare (#1498 review): `agent_slug` derives
      // a roster id from the display name, so a name that is already legal
      // snake_case — "ops", "scout" — slugs to itself, and `display !==
      // folder.name` would then read a present, valid entry as absent just
      // because its name and id happen to be spelled the same.
      return { ...folder, display, resolved: rosterNames.has(folder.name) };
    })
    .sort((a, b) => a.display.localeCompare(b.display));

  return (
    <Dialog open={Boolean(state)} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>{done ? "Tidied" : "Tidy empty agent folders"}</DialogTitle>
          <DialogDescription>
            {done
              ? count === 0
                ? "Nothing was removed — every folder had gained something by the time the tidy ran."
                : `Removed ${count} empty folder${count === 1 ? "" : "s"} from agents/.`
              : `${count} folder${count === 1 ? "" : "s"} under agents/ hold nothing at all. Removing them cannot take anything with them — a folder holding any file, note or subfolder is left alone.`}
          </DialogDescription>
        </DialogHeader>
        <ul className="max-h-64 space-y-1 overflow-y-auto" data-testid="workspace-sweep-folders">
          {rows.map((folder) => (
            <li
              key={folder.id}
              className="flex items-center gap-2 rounded-lg px-2.5 py-1.5 text-sm"
            >
              <Folder className="size-4 shrink-0 text-tone-2" />
              <span className="truncate" title={folder.name}>
                {folder.display}
              </span>
              {/* An id the roster cannot resolve is the clearest case of all
                  for sweeping: that teammate is no longer on the roster. Said
                  plainly rather than left as a bare ULID the operator is asked
                  to recognise. */}
              {!folder.resolved && (
                <span className="ml-auto shrink-0 text-xs text-muted-foreground">
                  no longer on the roster
                </span>
              )}
            </li>
          ))}
        </ul>
        <DialogFooter>
          {done ? (
            <Button onClick={onClose}>Done</Button>
          ) : (
            <>
              <Button variant="ghost" onClick={onClose}>
                Cancel
              </Button>
              {/* The solid override, not the tinted `destructive` variant: this
                  is the codebase's confirm-and-destroy weight, the one
                  `DeleteDialog` wears (issue #1378). */}
              <Button
                variant="destructive"
                className="bg-destructive text-white hover:bg-destructive/90"
                disabled={busy}
                onClick={onConfirm}
                data-testid="workspace-sweep-confirm"
              >
                {busy && <Loader2 className="mr-1 size-4 animate-spin" />}
                Remove {count}
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/**
 * The duplicate-folder repair, in its two stages (issue #759).
 *
 * Two lists, and the second one is not optional. The folds say what the repair
 * *can* do; the residuals say what it will not, and a dialog that showed only
 * the first would tell an operator their tree is fixed when two rival documents
 * are still sitting on one path. Each residual carries its own instruction,
 * because "fileInTheWay" is the host's word for the problem and not the
 * operator's.
 */
interface RepairState {
  stage: "preview" | "done";
  outcome: RepairOutcome;
}

function RepairDialog({
  state,
  nodes,
  busy,
  onClose,
  onConfirm,
  onReveal,
}: {
  state: RepairState | null;
  /** The tree, so a residual can be given its path and its real kind (issue #1469). */
  nodes: FsNode[];
  busy: boolean;
  onClose: () => void;
  onConfirm: () => void;
  /** Show a residual in the tree. Never writes — it expands and scrolls. */
  onReveal: (id: string) => void;
}) {
  const done = state?.stage === "done";
  const folds = state?.outcome.folders ?? [];
  const residuals = state?.outcome.residuals ?? [];
  const relocations = folds.reduce((n, folder) => n + folder.moved.length, 0);
  // The outcome the host returns most often, and the one this dialog used to
  // render as a no-op (issue #1469): a group holding a *file* is left entirely
  // alone and every member comes back as a residual, so a note and a folder
  // both called `Specs` yields zero folds and two residuals. The old copy then
  // announced "0 folders share a name" above an empty list, under a permanently
  // disabled "Merge 0" — a dialog whose every element denied the thing it had
  // just been opened to report.
  const residualOnly = folds.length === 0 && residuals.length > 0;

  return (
    <Dialog open={Boolean(state)} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>
            {residualOnly
              ? "Two things share a name"
              : done
                ? "Repaired"
                : "Repair duplicate folders"}
          </DialogTitle>
          <DialogDescription>
            {residualOnly
              ? `Nothing here can be merged automatically. ${residuals.length} ${residuals.length === 1 ? "item shares its name" : "items share their names"} with something the repair must not choose between — merging would have to discard one of them. Each one below says what it needs from you.`
              : done
                ? folds.length === 0
                  ? "Nothing was merged — the tree changed before the repair ran."
                  : `Merged ${folds.length} duplicate folder${folds.length === 1 ? "" : "s"} and moved ${relocations} item${relocations === 1 ? "" : "s"}.`
                : `${folds.length} folder${folds.length === 1 ? "" : "s"} share a name with another folder beside them, which is why publishing there fails. Their contents move into the copy that was there first. Nothing is renamed, nothing is overwritten, and no folder is removed until it is empty.`}
          </DialogDescription>
        </DialogHeader>

        <ul className="max-h-64 space-y-1 overflow-y-auto" data-testid="workspace-repair-folders">
          {folds.map((folder) => (
            <li key={folder.id} className="rounded-lg px-2.5 py-1.5 text-sm">
              <div className="flex items-center gap-2">
                <Folder className="size-4 shrink-0 text-tone-2" />
                <span className="truncate">{folder.name}</span>
                <span className="ml-auto shrink-0 text-xs text-muted-foreground">
                  {folder.moved.length === 0
                    ? done && folder.removed
                      ? "removed"
                      : "empty"
                    : `${folder.moved.length} item${folder.moved.length === 1 ? "" : "s"}`}
                </span>
              </div>
              {folder.moved.length > 0 && (
                <div className="mt-0.5 truncate pl-6 text-xs text-muted-foreground">
                  {folder.moved.map((child) => child.name).join(", ")}
                </div>
              )}
            </li>
          ))}
        </ul>

        {residuals.length > 0 && (
          <div className="space-y-1" data-testid="workspace-repair-residuals">
            {!residualOnly && (
              <p className="text-xs font-medium">
                {done ? "Still needs you" : "These will be left for you"}
              </p>
            )}
            <ul className="max-h-40 space-y-1 overflow-y-auto">
              {residuals.map((residual) => {
                // The kind comes from the tree rather than the wire, which
                // carries only id/name/parentId/cause. Drawing every residual as
                // a note was actively misleading for the commonest cause of all:
                // `fileSharesTheName` means one of the pair is a *folder*, and
                // "rename or remove one of them" read under two identical
                // note-looking rows.
                const node = nodeById(nodes, residual.id);
                const Icon = node?.kind === "folder" ? Folder : FileText;
                // A root-level residual (`parentId === null`) walks zero
                // folders, so the join is empty — and `where && (...)` used to
                // read that as "nothing to say" and drop the location
                // entirely, for exactly the residuals with the shortest
                // answer to give (#1498 review). "Workspace root" is the
                // same fallback label the move dialog already uses for this
                // spot (see `DestRow` above), reused rather than reworded.
                const where =
                  pathOf(nodes, residual.parentId ?? null)
                    .map((p) => p.name)
                    .join(" / ") || "Workspace root";
                return (
                  <li key={residual.id}>
                    <button
                      type="button"
                      onClick={() => onReveal(residual.id)}
                      data-testid="workspace-repair-residual"
                      className="w-full rounded-lg px-2.5 py-1.5 text-left text-sm hover:bg-accent"
                    >
                      <span className="flex items-center gap-2">
                        <Icon className="size-4 shrink-0 text-tone-2" />
                        <span className="truncate">{residual.name}</span>
                        <span className="ml-auto shrink-0 truncate text-xs text-muted-foreground">
                          in {where}
                        </span>
                      </span>
                      <span className="mt-0.5 block pl-6 text-xs text-muted-foreground">
                        {residualReason(residual.cause)}
                      </span>
                    </button>
                  </li>
                );
              })}
            </ul>
          </div>
        )}

        <DialogFooter>
          {/* A residual-only outcome has nothing to confirm, so it gets the one
              action that is true — the same `Done` a finished repair gets —
              rather than a "Merge 0" that can never be pressed (issue #1469). */}
          {done || residualOnly ? (
            <Button onClick={onClose}>Done</Button>
          ) : (
            <>
              <Button variant="ghost" onClick={onClose}>
                Cancel
              </Button>
              <Button disabled={busy || folds.length === 0} onClick={onConfirm}>
                {busy && <Loader2 className="mr-1 size-4 animate-spin" />}
                Merge {folds.length}
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function DestRow({
  label,
  disabled,
  audience,
  selected,
  onClick,
}: {
  label: string;
  disabled?: boolean;
  /** Set when picking this destination changes who can read the note (#1465). */
  audience?: MoveAudienceChange;
  /** Whether this row is the destination currently chosen (issue #1381). */
  selected?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      disabled={disabled}
      onClick={onClick}
      aria-pressed={selected}
      data-testid="workspace-move-dest"
      data-audience-change={audience}
      className={cn(
        "flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-sm hover:bg-accent disabled:pointer-events-none disabled:opacity-40",
        selected && "bg-accent font-medium",
      )}
    >
      <Folder className="size-4 text-tone-2" />
      <span className="truncate">{label}</span>
      {disabled && <span className="ml-auto text-xs text-muted-foreground">Here</span>}
      {/* Two words, not the sentence: the row is a choice, and the sentence is
          on the panel that follows the click. `Shares it` wears the destructive
          colour because that is the direction there is no undoing — a note the
          agents have read is read. */}
      {!disabled && audience && (
        <span
          className={cn(
            "ml-auto flex shrink-0 items-center gap-1 text-xs",
            audience === "exposed" ? "text-destructive" : "text-muted-foreground",
          )}
          data-testid="workspace-move-dest-audience"
        >
          <EyeOff className="size-3" aria-hidden />
          {audience === "hidden" ? "Hides it" : "Shares it"}
        </span>
      )}
    </button>
  );
}
