import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  FilePlus2,
  FileText,
  Folder,
  FolderOpen,
  FolderPlus,
  FolderSync,
  FolderX,
  Link2,
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
import { Alert, AlertDescription } from "@/components/ui/alert";
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
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { Skeleton } from "@/components/ui/skeleton";
import { rosterDisplayName, rosterNameMap, type RosterNames } from "@/lib/roster-names";
import { fromDto } from "@/lib/team";
import { cn } from "@/lib/utils";
import {
  applyRepair,
  childrenOf,
  clearLegacyLocal,
  ensureMdExt,
  fileByTitle,
  type FsNode,
  hasLegacyLocal,
  nodeById,
  pathOf,
  readLegacyLocalNodes,
  subtreeIds,
  titleOf,
} from "@/lib/workspace";
import { useLocalScope } from "@/connections/ConnectionContext";
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
const IMPORT_FOLDER_NAME = "Imported from this browser";

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

/** What the editor's status line is currently reporting. */
type SaveState = "idle" | "saving" | "saved" | "error";

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
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // The roster names the `Agents/` folders resolve against (issue #973). Best
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
  const [prompt, setPrompt] = useState<PromptState | null>(null);
  const [moving, setMoving] = useState<FsNode | null>(null);
  const [showExplorer, setShowExplorer] = useState(true);
  const [legacy, setLegacy] = useState<FsNode[]>([]);
  const [importing, setImporting] = useState(false);
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
  // The pending scratchpad partitioned by kind, once, for every surface that
  // describes or imports it: the banner's sentence, the import loops, and the
  // receipt. #500 partitioned inside `importLegacy` so the loops and the
  // receipt could not disagree; the banner counted the flat list separately
  // and drifted anyway (#507). One partition is what makes them agree.
  const legacyFolders = useMemo(() => legacy.filter((n) => n.kind === "folder"), [legacy]);
  const legacyFiles = useMemo(() => legacy.filter((n) => n.kind === "file"), [legacy]);
  const uploadRef = useRef<HTMLInputElement>(null);

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
  // itself from loading — it only means the `Agents/` folders keep showing raw
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
        const page = await searchWorkspace(client, company, query);
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

  // The pending save, held in a ref so the debounce timer and the unmount
  // cleanup both see the latest value without re-subscribing on every keystroke.
  const pending = useRef<{ id: string; content: string } | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const flush = useCallback(async () => {
    if (timer.current) {
      clearTimeout(timer.current);
      timer.current = null;
    }
    const job = pending.current;
    if (!job) return;
    pending.current = null;
    setSaveState("saving");
    try {
      const ack = await writeFile(client, company, job.id, job.content);
      setSaveState("saved");
      // Patch the authoritative stamp onto both the open file and the tree row,
      // so "last updated" is the host's answer and not a guess. `updatedBy`
      // rides along: this route stamps the operator server-side, and leaving
      // the stale value would keep showing "edited by <agent>" on a note the
      // operator has just rewritten, until the next refetch.
      setOpenFile((f) =>
        f && f.id === job.id
          ? { ...f, content: job.content, updatedAt: ack.updatedAt, updatedBy: OPERATOR_ORIGIN }
          : f,
      );
      setNodes((all) =>
        all.map((n) =>
          n.id === job.id ? { ...n, updatedAt: ack.updatedAt, updatedBy: OPERATOR_ORIGIN } : n,
        ),
      );
    } catch (e) {
      // Keep the buffer: the operator's text is never dropped because a save
      // failed. A 404 means the note is gone on the host, which needs a decision
      // rather than a retry, so say so explicitly.
      //
      // Only restore when nothing newer arrived. The operator can keep typing
      // during the await above, and that typing writes a fresher job into
      // `pending.current` — overwriting it with the job we just failed to save
      // would silently discard every keystroke made while the request was in
      // flight, which is the exact loss this buffer exists to prevent.
      if (!pending.current) pending.current = job;
      setSaveState("error");
      if (isNotFound(e)) {
        toast.error("This note no longer exists on the host.", {
          description: "Someone deleted it. Your text is still here — save it as a new note.",
        });
      } else {
        toast.error(message(e, "could not save this note"));
      }
    }
  }, [client, company]);

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

  function onEdit(id: string, content: string) {
    setDraft(content);
    setSaveState("idle");
    pending.current = { id, content };
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
    // those keystrokes reach `pending` while the plan was computed from the
    // `draft` of the render the frame arrived in.
    const job = pending.current;
    pending.current = null;
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

  /* ---- migration off the retired localStorage scratchpad ---- */

  useEffect(() => {
    const mine = readLegacyLocalNodes(scope);
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
      toast.success(`Imported ${importSummary(legacyFiles.length, legacyFolders.length)}.`);
      await loadTree({ silent: true });
    } catch (e) {
      // The key is left intact on failure, so the banner comes back and nothing
      // the operator wrote is lost to a half-finished import.
      toast.error(message(e, "could not import your local notes"));
    } finally {
      setImporting(false);
    }
  }

  function discardLegacy() {
    clearLegacyLocal(scope);
    setLegacy([]);
  }

  /* ---- navigation ---- */

  async function open(id: string) {
    await flush();
    setOpenId(id);
    setMode("read");
    setDraft(null);
    setSaveState("idle");
    setOpenFile(null);
    setFileError(null);
    setExpanded((prev) => {
      const next = new Set(prev);
      for (const a of pathOf(nodes, id)) if (a.kind === "folder") next.add(a.id);
      return next;
    });
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
      setExpanded((prev) => {
        const next = new Set(prev);
        for (const a of pathOf(nodes, hit.id)) if (a.kind === "folder") next.add(a.id);
        next.add(hit.id);
        return next;
      });
      return;
    }
    await open(hit.id);
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
  async function createAndOpen(name: string, content?: string): Promise<boolean> {
    try {
      const created = await createNode(client, company, {
        name: ensureMdExt(name.trim() || "Untitled"),
        kind: "file",
        parentId: null,
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

  async function createFolder(name: string) {
    try {
      const created = await createNode(client, company, {
        name: name.trim() || "New folder",
        kind: "folder",
        parentId: null,
      });
      setNodes((all) => [...all, created]);
    } catch (e) {
      toast.error(message(e, "could not create the folder"));
    }
  }

  async function rename(node: FsNode, name: string) {
    const next = (node.kind === "file" ? ensureMdExt(name.trim()) : name.trim()) || node.name;
    try {
      const updated = await renameMoveNode(client, company, node.id, { name: next });
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
      const updated = await renameMoveNode(client, company, node.id, { parentId: destId });
      setNodes((all) => all.map((n) => (n.id === updated.id ? updated : n)));
    } catch (e) {
      toast.error(message(e, "could not move this item"));
    }
  }

  async function remove(node: FsNode) {
    const removed = subtreeIds(nodes, node.id);
    try {
      await deleteNodeApi(client, company, node.id);
      setNodes((all) => all.filter((n) => !removed.has(n.id)));
      if (openId && removed.has(openId)) {
        pending.current = null;
        setOpenId(null);
        setOpenFile(null);
        setDraft(null);
      }
    } catch (e) {
      toast.error(message(e, "could not delete this item"));
    }
  }

  /**
   * Ask the host which `Agents/<id>/` folders are empty, and show them
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
        const created = await uploadFile(client, company, file, null);
        setNodes((all) => [...all, created]);
      } catch (e) {
        toast.error(`${file.name}: ${message(e, "upload failed")}`);
      }
    }
  }

  const body = draft ?? openFile?.content ?? "";
  const openNode = nodeById(nodes, openId);

  return (
    <div className="flex flex-1 overflow-hidden">
      {/* Explorer */}
      <aside
        className={cn(
          "w-64 shrink-0 flex-col border-r bg-card/40 md:flex",
          showExplorer ? "flex" : "hidden",
        )}
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
          {/* Issue #700. A company provisioned before the tree went lazy carries
              one empty folder per teammate, and nothing else will ever remove
              them. Deliberately a button rather than something boot does: the
              operator's click is the opt-in, and the dialog names every folder
              before any of them goes. */}
          <IconBtn
            label="Tidy empty agent folders"
            disabled={sweeping}
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
            disabled={repairing}
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
          ) : nodes.length === 0 ? (
            <p className="px-3 py-2 text-xs text-muted-foreground">
              This workspace is empty. Create a note to start.
            </p>
          ) : (
            <Tree
              nodes={nodes}
              parentId={null}
              depth={0}
              expanded={expanded}
              openId={openId}
              rosterNames={rosterNames}
              onToggle={toggle}
              onOpen={(id) => void open(id)}
              onRename={(node) => setPrompt({ mode: "rename", node })}
              onMove={(node) => setMoving(node)}
              onDelete={(node) => void remove(node)}
            />
          )}
        </div>
      </aside>

      {/* Note pane */}
      <section className={cn("flex-1 flex-col overflow-hidden", showExplorer ? "hidden md:flex" : "flex")}>
        {legacy.length > 0 && (
          <Alert className="m-3 mb-0 w-auto" data-testid="workspace-migration-banner">
            <AlertDescription className="flex flex-wrap items-center gap-3">
              <span className="flex-1">
                {migrationBannerText(legacyFiles.length, legacyFolders.length)}
              </span>
              <Button
                size="sm"
                disabled={importing}
                onClick={() => void importLegacy()}
                data-testid="workspace-migration-import"
              >
                {importing && <Loader2 className="size-3.5 animate-spin" />} Import
              </Button>
              <Button size="sm" variant="ghost" disabled={importing} onClick={discardLegacy}>
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
                <Button size="sm" variant="ghost" onClick={() => setRescued(null)}>
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
              <span className="truncate text-sm font-medium">{titleOf(openNode)}</span>
              <Authorship
                createdBy={openFile?.createdBy ?? openNode.createdBy}
                updatedBy={openFile?.updatedBy ?? openNode.updatedBy}
              />
              <span className="hidden shrink-0 text-xs text-muted-foreground sm:inline">
                {formatUpdated(openFile?.updatedAt ?? openNode.updatedAt)}
              </span>
              <SaveStatus state={saveState} />
              {/* A payload has no prose to read or edit, so the mode switch is
                  hidden rather than shown-and-broken (issue #553). The host
                  refuses a text write to one, so an Edit tab here would be a
                  control whose only outcome is an error toast. */}
              {!isBinary(openNode) && (
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
        ) : (
          <EmptyNote onNew={() => setPrompt({ mode: "file" })} onToggleExplorer={() => setShowExplorer((s) => !s)} />
        )}
      </section>

      <NamePrompt
        state={prompt}
        onClose={() => setPrompt(null)}
        onSubmit={(name) => {
          if (prompt?.mode === "folder") void createFolder(name);
          else if (prompt?.mode === "file") void createAndOpen(name);
          else if (prompt?.mode === "rename" && prompt.node) void rename(prompt.node, name);
          setPrompt(null);
        }}
      />
      <MoveDialog
        nodes={nodes}
        moving={moving}
        onClose={() => setMoving(null)}
        onMove={(destId) => {
          if (moving) void move(moving, destId);
          setMoving(null);
        }}
      />
      <SweepDialog
        state={sweep}
        busy={sweeping}
        onClose={() => setSweep(null)}
        onConfirm={() => void confirmSweep()}
      />
      <RepairDialog
        state={repair}
        busy={repairing}
        onClose={() => setRepair(null)}
        onConfirm={() => void confirmRepair()}
      />
    </div>
  );
}

/** The editor's save indicator: quiet when idle, explicit when it matters. */
function SaveStatus({ state }: { state: SaveState }) {
  if (state === "idle") return null;
  const label =
    state === "saving" ? "Saving…" : state === "saved" ? "Saved" : "Not saved — retrying on edit";
  return (
    <span
      data-testid="workspace-save-state"
      data-state={state}
      className={cn(
        "shrink-0 text-xs",
        state === "error" ? "text-destructive" : "text-muted-foreground",
      )}
    >
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
  /** id -> display name for roster ids (issue #973). See {@link isAgentsFolder}. */
  rosterNames: RosterNames;
  onToggle: (id: string) => void;
  onOpen: (id: string) => void;
  onRename: (node: FsNode) => void;
  onMove: (node: FsNode) => void;
  onDelete: (node: FsNode) => void;
}

/**
 * Whether `folder` is the workspace's `Agents/` root — the one folder whose
 * direct children are named by roster id rather than anything an operator
 * chose (issue #973). Root-scoped (`parentId === null`) so a note or folder an
 * operator names "Agents" somewhere else in the tree is never mistaken for it.
 */
function isAgentsFolder(folder: FsNode | undefined): boolean {
  return folder?.kind === "folder" && folder.name === "Agents" && folder.parentId === null;
}

/**
 * `Agents/`'s children, sorted by display name rather than the lexical id
 * {@link childrenOf} sorts everywhere else (issue #973). The pre-#686 ULID ids
 * all sort before every readable slug under the plain id ordering, which is
 * not an order an operator can read anything into.
 */
function sortRosterFolders(items: FsNode[], names: RosterNames): FsNode[] {
  return [...items].sort((a, b) => {
    if (a.kind !== b.kind) return a.kind === "folder" ? -1 : 1;
    // Only a roster folder's name is an id worth resolving. A direct file
    // under `Agents/` is unusual but not impossible, and its raw name could
    // coincidentally collide with a roster id — that must not reorder it by
    // a display name it was never given one for.
    return a.kind === "folder"
      ? rosterDisplayName(a.name, names).localeCompare(rosterDisplayName(b.name, names))
      : a.name.localeCompare(b.name);
  });
}

function Tree(props: TreeProps) {
  const items = childrenOf(props.nodes, props.parentId);
  const ordered = isAgentsFolder(nodeById(props.nodes, props.parentId))
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
}: {
  createdBy: WorkspaceOrigin;
  updatedBy: WorkspaceOrigin;
}) {
  const created = originLabel(createdBy);
  const edited = sameOrigin(createdBy, updatedBy) ? null : originLabel(updatedBy);
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
  const { depth, expanded, openId, onToggle, onOpen, nodes, rosterNames } = props;
  const isFolder = node.kind === "folder";
  const isOpen = expanded.has(node.id);
  const active = node.id === openId;
  // A direct child of `Agents/` is named by roster id — its real folder path,
  // and the identity every artifact it holds is stamped with — but an operator
  // recognizes the teammate by name, not by that id (issue #973). The id stays
  // the label everywhere else in the tree: it is only ever a roster id one
  // level below `Agents/`.
  const isRosterFolder = isFolder && isAgentsFolder(nodeById(nodes, node.parentId));
  const displayName = isRosterFolder ? rosterDisplayName(node.name, rosterNames) : node.name;

  return (
    <>
      <div
        className={cn(
          "group flex items-center gap-1 rounded-md px-1.5 py-1 text-sm",
          active ? "bg-accent font-medium" : "hover:bg-accent/50",
        )}
        style={{ paddingLeft: 6 + depth * 12 }}
      >
        <button
          onClick={() => (isFolder ? onToggle(node.id) : onOpen(node.id))}
          className="flex min-w-0 flex-1 items-center gap-1.5 text-left"
        >
          {isFolder ? (
            <>
              {isOpen ? <ChevronDown className="size-3.5 shrink-0 text-muted-foreground" /> : <ChevronRight className="size-3.5 shrink-0 text-muted-foreground" />}
              {isOpen ? <FolderOpen className="size-4 shrink-0 text-tone-2" /> : <Folder className="size-4 shrink-0 text-tone-2" />}
            </>
          ) : (
            <FileText className="ml-3.5 size-4 shrink-0 text-muted-foreground" />
          )}
          <span className="truncate" title={isRosterFolder ? node.name : undefined}>
            {isFolder ? displayName : titleOf(node)}
          </span>
          {/* Agent-created nodes get a marker in the tree itself, so "what has
              the company been writing" is answerable by scanning rather than by
              opening each note. Only the agent case — badging the operator's
              own notes back at them says nothing. */}
          {node.createdBy.kind === "agent" && (
            <Badge
              variant="outline"
              className={cn("shrink-0 px-1 py-0 text-3xs", ORIGIN_STYLES.agent)}
              title={`Created by agent ${node.createdBy.id}`}
              data-testid="workspace-tree-agent-badge"
            >
              {node.createdBy.id}
            </Badge>
          )}
        </button>
        <DropdownMenu>
          <DropdownMenuTrigger
            render={<Button variant="ghost" size="icon" className="size-6 opacity-0 group-hover:opacity-100 data-[popup-open]:opacity-100" aria-label="Actions" />}
          >
            <MoreHorizontal className="size-3.5" />
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuItem onClick={() => props.onRename(node)}>Rename</DropdownMenuItem>
            <DropdownMenuItem onClick={() => props.onMove(node)}>Move to…</DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem variant="destructive" onClick={() => props.onDelete(node)}>
              Delete
            </DropdownMenuItem>
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
    return <p className="text-sm text-muted-foreground">This note is empty. Switch to Edit to write.</p>;
  }
  // Rewrite [[target]] / [[target|alias]] into links the renderer can style —
  // but leave fenced and inline code untouched (so `[[…]]` examples survive).
  const rewritten = source.replace(
    /(```[\s\S]*?```|~~~[\s\S]*?~~~|`[^`\n]*`)|\[\[([^\]|]+)(?:\|([^\]]+))?\]\]/g,
    (_m, code: string | undefined, target: string, alias?: string) =>
      code
        ? code
        : `[${(alias ?? target).trim()}](#wiki:${encodeURIComponent(target.trim())})`,
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

function EmptyNote({ onNew, onToggleExplorer }: { onNew: () => void; onToggleExplorer: () => void }) {
  return (
    <div className="flex flex-1 flex-col">
      <div className="flex items-center border-b px-3 py-2 md:hidden">
        <IconBtn label="Toggle explorer" onClick={onToggleExplorer}>
          <PanelLeft className="size-4" />
        </IconBtn>
      </div>
      <div className="flex flex-1 flex-col items-center justify-center gap-3 text-center">
        <FileText className="size-8 text-muted-foreground" />
        <div className="space-y-1">
          <p className="font-medium">No note open</p>
          <p className="text-sm text-muted-foreground">Pick a note from the explorer, or create one.</p>
        </div>
        <Button variant="outline" size="sm" onClick={onNew}>
          <FilePlus2 className="size-4" /> New note
        </Button>
      </div>
    </div>
  );
}

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
    <Button
      variant="ghost"
      size="icon"
      className="size-7 text-muted-foreground"
      aria-label={label}
      onClick={onClick}
      {...rest}
    >
      {children}
    </Button>
  );
}

/* ---- dialogs ---- */

interface PromptState {
  mode: "folder" | "file" | "rename";
  node?: FsNode;
}

function NamePrompt({
  state,
  onClose,
  onSubmit,
}: {
  state: PromptState | null;
  onClose: () => void;
  onSubmit: (name: string) => void;
}) {
  const [name, setName] = useState("");

  useEffect(() => {
    setName(state?.mode === "rename" ? (state.node?.name ?? "") : "");
  }, [state]);

  const title = state?.mode === "folder" ? "New folder" : state?.mode === "file" ? "New note" : "Rename";

  return (
    <Dialog open={Boolean(state)} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          <DialogDescription>
            {state?.mode === "file" ? "Notes get a .md extension automatically." : "Give it a name."}
          </DialogDescription>
        </DialogHeader>
        <div className="grid gap-2">
          <Label htmlFor="fs-name">Name</Label>
          <Input
            id="fs-name"
            autoFocus
            value={name}
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && name.trim()) onSubmit(name);
            }}
            placeholder={state?.mode === "folder" ? "e.g. Campaigns" : "e.g. Notes"}
          />
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button disabled={!name.trim()} onClick={() => onSubmit(name)}>
            {state?.mode === "rename" ? "Rename" : "Create"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function MoveDialog({
  nodes,
  moving,
  onClose,
  onMove,
}: {
  nodes: FsNode[];
  moving: FsNode | null;
  onClose: () => void;
  onMove: (destId: string | null) => void;
}) {
  const blocked = moving ? subtreeIds(nodes, moving.id) : new Set<string>();
  const folders = nodes.filter((x) => x.kind === "folder" && !blocked.has(x.id));

  return (
    <Dialog open={Boolean(moving)} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>Move “{moving ? titleOf(moving) : ""}”</DialogTitle>
          <DialogDescription>Pick a destination folder.</DialogDescription>
        </DialogHeader>
        <div className="max-h-72 space-y-1 overflow-y-auto">
          <DestRow label="Workspace root" disabled={moving?.parentId === null} onClick={() => onMove(null)} />
          {folders.map((f) => (
            <DestRow key={f.id} label={f.name} disabled={moving?.parentId === f.id} onClick={() => onMove(f.id)} />
          ))}
        </div>
      </DialogContent>
    </Dialog>
  );
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
  busy,
  onClose,
  onConfirm,
}: {
  state: SweepState | null;
  busy: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  const done = state?.stage === "done";
  const count = state?.folders.length ?? 0;

  return (
    <Dialog open={Boolean(state)} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>{done ? "Tidied" : "Tidy empty agent folders"}</DialogTitle>
          <DialogDescription>
            {done
              ? count === 0
                ? "Nothing was removed — every folder had gained something by the time the tidy ran."
                : `Removed ${count} empty folder${count === 1 ? "" : "s"} from Agents/.`
              : `${count} folder${count === 1 ? "" : "s"} under Agents/ hold nothing at all. Removing them cannot take anything with them — a folder holding any file, note or subfolder is left alone.`}
          </DialogDescription>
        </DialogHeader>
        <ul
          className="max-h-64 space-y-1 overflow-y-auto"
          data-testid="workspace-sweep-folders"
        >
          {state?.folders.map((folder) => (
            <li
              key={folder.id}
              className="flex items-center gap-2 rounded-lg px-2.5 py-1.5 text-sm"
            >
              <Folder className="size-4 shrink-0 text-tone-2" />
              <span className="truncate">{folder.name}</span>
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
              <Button variant="destructive" disabled={busy} onClick={onConfirm}>
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
  busy,
  onClose,
  onConfirm,
}: {
  state: RepairState | null;
  busy: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  const done = state?.stage === "done";
  const folds = state?.outcome.folders ?? [];
  const residuals = state?.outcome.residuals ?? [];
  const relocations = folds.reduce((n, folder) => n + folder.moved.length, 0);

  return (
    <Dialog open={Boolean(state)} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{done ? "Repaired" : "Repair duplicate folders"}</DialogTitle>
          <DialogDescription>
            {done
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
            <p className="text-xs font-medium">
              {done ? "Still needs you" : "These will be left for you"}
            </p>
            <ul className="max-h-40 space-y-1 overflow-y-auto">
              {residuals.map((residual) => (
                <li key={residual.id} className="rounded-lg px-2.5 py-1.5 text-sm">
                  <div className="flex items-center gap-2">
                    <FileText className="size-4 shrink-0 text-tone-2" />
                    <span className="truncate">{residual.name}</span>
                  </div>
                  <p className="mt-0.5 pl-6 text-xs text-muted-foreground">
                    {residualReason(residual.cause)}
                  </p>
                </li>
              ))}
            </ul>
          </div>
        )}

        <DialogFooter>
          {done ? (
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

function DestRow({ label, disabled, onClick }: { label: string; disabled?: boolean; onClick: () => void }) {
  return (
    <button
      disabled={disabled}
      onClick={onClick}
      className="flex w-full items-center gap-2 rounded-lg px-2.5 py-2 text-left text-sm hover:bg-accent disabled:pointer-events-none disabled:opacity-40"
    >
      <Folder className="size-4 text-tone-2" />
      <span className="truncate">{label}</span>
      {disabled && <span className="ml-auto text-xs text-muted-foreground">Here</span>}
    </button>
  );
}
