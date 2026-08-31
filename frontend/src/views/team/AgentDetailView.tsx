import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import {
  ChevronRight,
  Cpu,
  Mail,
  Pencil,
  Server,
  Sparkles,
  Users,
  Wallet,
  Wrench,
} from "lucide-react";
import { toast } from "sonner";

import { listPeople, me as fetchMe, type Person } from "@/api/auth";
import type { OpenCompanyClient } from "@/api/client";
import { setInboxEnabled } from "@/api/inbox";
import { listTasks, type Task } from "@/api/tasks";
import { isDesktopRuntime } from "@/api/transport";
import {
  cachedAcpModels,
  ensureAcpModels,
  type AcpHarnessModel,
} from "@/api/transport/desktop";
import { ApiError, type AgentDetailDto, type EditAgentInput, type HarnessDto } from "@/api/types";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Badge } from "@/components/ui/badge";
import { PageHeader } from "@/components/page-header";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { useHashFlag } from "@/hooks/use-hash-flag";
import {
  agentEdits,
  companyCovers,
  draftFrom,
  draftIsValid,
  missingRequired,
  emptyDraft,
  grantCeiling,
  harnessEdit,
  harnessOptionLabel,
  isEditable,
  modelEdit,
  parseToolGlobs,
  resolvedHarnessKind,
  summarizeGrants,
  tierLabel,
  toolGlobsDiffer,
  type AgentDraft,
  type AgentFieldKey,
} from "@/lib/agent";
import { draftAgentField } from "@/api/agent-copilot";
import { getInferenceStatus, type CognitionPath } from "@/api/inference";
import { FieldCopilot } from "@/views/team/FieldCopilot";
import { fetchBoardColumns } from "@/lib/board-columns";
import { avatarRef } from "@/lib/avatar";
import { AvatarPicker } from "@/components/avatar-picker";
import { personName } from "@/lib/person";
import { roleSubtitle, toneFor } from "@/lib/team";
import { workloadByAssignee, type Workload } from "@/lib/team-workload";
import { cn } from "@/lib/utils";
import { AgentFields } from "@/views/team/AgentFields";
import { AgentRuns } from "@/views/team/AgentRuns";

type Load = "loading" | "ready" | "missing" | "unsupported" | "error";

/**
 * The Harness select's value for "use the company default" (issue #1245's
 * harness-picker follow-up). Not `""`: an empty string is Base UI Select's own
 * placeholder/unset sentinel, so a real option needs a value of its own — the
 * boundary to `harnessEdit`'s `""`-means-default contract is translated at
 * the two points that cross it, `onEdit` and `saveHarnessAndModel`.
 */
const HARNESS_DEFAULT = "__default__";

/**
 * The model select's value for "leave it to the harness".
 *
 * A sentinel for the same reason [`HARNESS_DEFAULT`] is: `""` is Base UI
 * Select's own unset marker, so a real option needs a value of its own. It is
 * translated back to `""` — which `modelEdit` reads as "clear the override" —
 * at the single point that crosses the boundary, the select's `onValueChange`.
 */
const MODEL_HARNESS_DEFAULT = "__harness_default__";

/**
 * Why a detail read failed, in the operator's terms rather than the wire's.
 *
 * A `404` from `GET …/team/{agentId}` is **two different facts**, and the status
 * cannot tell them apart: a host that predates this route 404s the path it does
 * not serve, and a host that serves it 404s a teammate that is gone. Saying "no
 * such teammate" to the first sends an operator looking for a deletion that
 * never happened; saying "this host is too old" to the second hides a real
 * removal behind a version complaint.
 *
 * The roster settles it, but only if the right question is asked. "Did `GET
 * …/team` answer?" is not enough — the roster route is the *older* one, so an
 * out-of-date host answers it perfectly. The question that separates the two is
 * whether the roster still **contains this agent**:
 *
 * | `GET …/team` | outcome |
 * |---|---|
 * | lists this agent | the host has the roster but not the detail route → `unsupported` |
 * | omits this agent | the host serves both and the teammate is gone → `missing` |
 * | fails too | nothing is reachable; do not guess → `error` |
 *
 * Anything that is not a `404` — a transport failure, a `500` — is `error`. It
 * used to fall into `unsupported`, which told an operator their host was too old
 * when their network had simply dropped.
 */
async function classifyFailure(
  error: unknown,
  roster: () => Promise<{ id: string }[]>,
  agentId: string,
): Promise<Exclude<Load, "loading" | "ready">> {
  if (!(error instanceof ApiError) || error.status !== 404) return "error";
  const members = await roster().catch(() => null);
  if (members === null) return "error";
  return members.some((member) => member.id === agentId) ? "unsupported" : "missing";
}

/**
 * One agent, opened (issue #264).
 *
 * Before this the Team card was a dead end: a name, a role, and a destructive
 * Remove. None of what an agent *is* was reachable once it existed, so an
 * operator could not read the instructions it was defined with, could not see
 * which tools it may use or which desks it belongs to, and could not change any
 * of it. This is the screen that answers those questions, and edits the ones
 * the host says are editable.
 *
 * ## Read-only is a fact about the agent, not a state of this screen
 *
 * A **manifest** teammate is declared in the company's version-controlled
 * `company.toml` and is edited here too: the host stores the change as an
 * override on the company record rather than rewriting the blueprint, so a
 * deployed company's own roster — including the global baseline every company
 * gets — is the operator's to change without a redeploy they may not be able to
 * make. An **overlay** teammate was added here and is edited here.
 *
 * Which is which comes from the host's own `editable` list rather than from a
 * rule this file re-implements. A console that decided for itself would
 * eventually offer a field the host refuses, and the operator would meet the
 * disagreement as a failed save instead of as a field that will not take an
 * edit.
 */
export function AgentDetailView({
  client,
  company,
  agentId,
  onBack,
}: {
  client: OpenCompanyClient;
  company: string | null;
  agentId: string;
  onBack: () => void;
}) {
  const [load, setLoad] = useState<Load>("loading");
  const [agent, setAgent] = useState<AgentDetailDto | null>(null);
  /**
   * The id of the agent currently on screen, kept current even across a
   * navigation that happens while an async write is in flight. Guards the
   * reset/save response handling: a slow response for a previous agent must not
   * clobber the active detail's draft or flip its editor.
   */
  const displayedAgentIdRef = useRef<string | null>(null);
  useEffect(() => {
    displayedAgentIdRef.current = agent?.id ?? null;
  }, [agent]);
  /**
   * The edit form is an address, not a piece of local state (issue #1653).
   *
   * `#/team/<id>?edit` is what the profile panel's "Edit agent" button links
   * to, so the form has to be openable by the hash rather than only by the
   * button on this page. Deriving it from the flag rather than mirroring the
   * flag into `useState` leaves one source of truth: the browser's Back button
   * closes the editor, and a link into it lands with the form already open.
   *
   * Gated on the host's own `editable` list, so a hand-typed `?edit` on a
   * teammate this host will not edit does not open a form whose Save can only
   * fail.
   */
  const [editRequested, setEditRequested] = useHashFlag("edit");
  const setEditing = setEditRequested;
  const editing = editRequested && (agent?.editable.length ?? 0) > 0;
  const [draft, setDraft] = useState<AgentDraft>(emptyDraft());
  const [saving, setSaving] = useState(false);
  /**
   * The cognition path this company booted onto (issue #1776).
   *
   * Gates the copilot the same way `WorkflowCreateDialog` gates its Draft
   * button: on the offline `echo` brain there is no model to draft with, so the
   * control is disabled with a sentence saying why rather than failing on click.
   * `null` until the check settles, and on a host without the route — which
   * leaves it enabled, because refusing to draft on a host we could not ask
   * would break the control everywhere it actually works.
   */
  const [cognition, setCognition] = useState<CognitionPath | null>(null);
  /** An icon save is in flight — the picker is disabled until it settles, so two
      avatar PATCHes for the same teammate can never be pending at once and
      resolve out of order (the older one overwriting the newer choice). */
  const [avatarSaving, setAvatarSaving] = useState(false);
  /**
   * What this teammate is on and carrying (issue #1141), or `null` when the
   * board could not be read — in which case the header states neither rather
   * than an invented "idle · 0 open".
   */
  const [workload, setWorkload] = useState<Workload | null>(null);
  /** The open cards assigned directly to this teammate, when the board is readable. */
  const [openTasks, setOpenTasks] = useState<Task[] | null>(null);
  /** An inbox write is in flight; the switch is held until the host answers. */
  const [inboxSaving, setInboxSaving] = useState(false);
  /**
   * The Harness & Model editor (issue #1245's harness-picker follow-up). Its
   * own small state, separate from `draft`/`editing`: both fields are
   * admin-only, and neither is part of the name/role/description group the
   * Instructions card edits together. One toggle covers both — they're saved
   * together, since a model override is only ever meaningful relative to
   * whichever harness this same save leaves the teammate on.
   */
  const [editingHarness, setEditingHarness] = useState(false);
  const [harnessDraft, setHarnessDraft] = useState(HARNESS_DEFAULT);
  const [modelDraft, setModelDraft] = useState("");
  const [savingHarness, setSavingHarness] = useState(false);
  /**
   * The company's declared harnesses, for the picker's options. Best-effort
   * and silent on failure, like `PolicySettings`' own `wiredTools`: an older
   * host without `GET {scope}/harnesses` still opens a teammate, the picker
   * just has nothing to offer beyond the free-text model field it already had.
   */
  const [harnesses, setHarnesses] = useState<HarnessDto[]>([]);
  /**
   * Whether this viewer may edit the daily budget (issue #1206, ported from
   * `TeamView.tsx`). Courtesy, not enforcement — the host refuses the write
   * with a 403 regardless; hiding the control from a non-admin only spares
   * them a control they cannot use. Every agent this page can show is
   * host-backed by construction (`boot` only reaches `ready` once `getAgent`
   * answers), so there is no `fromHost` half to this check the way the roster
   * card needed.
   */
  const [isAdmin, setIsAdmin] = useState(false);
  // Who set the cap override, for the attribution line. Only an admin may read
  // the user directory, so this stays empty for a member and the attribution
  // degrades to "an admin" rather than disappearing.
  const [people, setPeople] = useState<Person[]>([]);
  /** Whether the daily-budget dialog is open. */
  const [budgetOpen, setBudgetOpen] = useState(false);
  const [avatarOpen, setAvatarOpen] = useState(false);

  useEffect(() => {
    let live = true;
    void (async () => {
      let admin = false;
      try {
        admin = (await fetchMe(client, company)).role === "admin";
      } catch {
        // No user plane on this host, or not signed in — treat as non-admin.
      }
      if (!live) return;
      setIsAdmin(admin);
      if (!admin) {
        setPeople([]);
        return;
      }
      try {
        const dir = await listPeople(client, company);
        if (live) setPeople(dir);
      } catch {
        // Attribution falls back to "an admin"; not worth a toast.
        if (live) setPeople([]);
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  /**
   * The required fields the draft leaves blank, so the form can say why Save is
   * disabled instead of just being disabled (issue #1776).
   *
   * Empty until the teammate loads — there is nothing to require a value of.
   */
  const missing = agent ? missingRequired(draft, (key) => isEditable(agent, key)) : [];

  // Issue #1776: read the cognition path while the edit form is open, so the
  // copilot can say "no model is configured" instead of offering a draft that
  // can only come back refused. Its own effect rather than a field on the boot
  // read: a slow `/inference` must not delay the teammate itself appearing.
  useEffect(() => {
    if (!editing) return;
    let live = true;
    (async () => {
      try {
        const status = await getInferenceStatus(client, company);
        if (live) setCognition(status.cognition);
      } catch {
        // A host without the route tells us nothing either way. `null` is not
        // `echo`, so the control stays enabled and a refusal (with its reason)
        // is what the operator would see instead.
        if (live) setCognition(null);
      }
    })();
    return () => {
      live = false;
    };
  }, [editing, client, company]);

  /** A human label for whoever set a cap — never a raw user id. */
  function whoSet(userId: string): string {
    const person = people.find((p) => p.id === userId);
    return person ? personName(person) : "an admin";
  }

  const boot = useCallback(async () => {
    setLoad("loading");
    try {
      const detail = await client.getAgent(agentId, company);
      setAgent(detail);
      setDraft(draftFrom(detail));
      setLoad("ready");
    } catch (error) {
      setAgent(null);
      setLoad(await classifyFailure(error, () => client.listTeam(company), agentId));
    }
  }, [client, company, agentId]);

  useEffect(() => {
    void boot();
  }, [boot]);

  // Close the harness editor whenever the displayed teammate changes.
  //
  // This view stays mounted across teammates, and these three are the only
  // pieces of state `boot` does not re-derive — it refreshes `agent` and
  // `draft` and leaves the harness editor exactly as it was. So an editor left
  // open on A stayed open on B still holding A's harness and model, and Save
  // then PATCHed A's binding onto B. The host accepts it, because the id is
  // valid for the company: nothing downstream can tell that the operator was
  // looking at someone else when they picked it.
  //
  // Reset rather than repopulated: `onEdit` seeds the drafts from whichever
  // teammate is on screen when it opens, so closing is enough and there is one
  // seeding path rather than two that can disagree.
  useEffect(() => {
    setEditingHarness(false);
    setHarnessDraft(HARNESS_DEFAULT);
    setModelDraft("");
  }, [agentId]);

  /**
   * The board, read for this one teammate — the same derivation the Company
   * cards use, from the same two reads (`lib/team-workload.ts`).
   *
   * Best-effort and never blocking: a host with no `…/tasks` route still opens
   * a teammate, it just cannot say what they are on.
   */
  useEffect(() => {
    let live = true;
    // Drop the previous teammate's board reading before the new one is read.
    // The view stays mounted across a hash change, and the agent-detail request
    // races this one — without this the ready view can render agent B beside
    // agent A's task links until (or unless) the board request lands.
    setWorkload(null);
    setOpenTasks(null);
    if (!company) return;
    void (async () => {
      const [tasks, columns] = await Promise.all([
        listTasks(client, company).catch(() => null),
        fetchBoardColumns(client, company).catch(() => null),
      ]);
      if (!live) return;
      // Empty columns is a host whose ledger list carries no board — an absence,
      // not a vocabulary. Same rule as the roster's cards.
      if (!tasks || !columns?.length) {
        setWorkload(null);
        setOpenTasks(null);
        return;
      }
      setWorkload(workloadByAssignee(tasks, columns).get(agentId) ?? { open: 0, status: "idle" });
      const closed = new Set(columns.filter((column) => column.closed).map((column) => column.id));
      setOpenTasks(
        tasks.filter((task) => task.assignee.trim() === agentId && !closed.has(task.column)),
      );
    })();
    return () => {
      live = false;
    };
  }, [client, company, agentId]);

  /**
   * The Harness picker's options (issue #1245's harness-picker follow-up).
   * Read once per (client, company) rather than per edit, so opening the
   * editor is instant. Silent on failure — see the state's own docs.
   */
  useEffect(() => {
    let live = true;
    void client
      .listHarnesses(company)
      .then((next) => {
        if (live) setHarnesses(next);
      })
      .catch(() => {
        if (live) setHarnesses([]);
      });
    return () => {
      live = false;
    };
  }, [client, company]);

  /**
   * Give this teammate an inbox, or take it away (issue #1190).
   *
   * Moved here from the roster card, where it was the only control that wrote
   * to the host and sat one mis-click away while scanning thirteen cards. This
   * page already *reported* inbox state as a badge and offered no way to change
   * it; the read and the write live together now.
   *
   * Optimistic, then reverted on failure — the switch must never be left
   * claiming a state the host refused. Keyed on the roster agent id, which is
   * the `InboxStore` key the Inbox page reads and the ingest webhook files mail
   * under; nothing is persisted client-side.
   */
  async function toggleInbox(next: boolean) {
    if (!agent || inboxSaving) return;
    // Scoped to the teammate this call is *about*. This screen does not remount
    // when the hash names a different agent — it re-reads into the same state —
    // so a slow write for A that fails after the operator has stepped to B would
    // otherwise roll back B's switch, for a request B never made.
    const apply = (enabled: boolean) =>
      setAgent((held) => (held?.id === agentId ? { ...held, inboxEnabled: enabled } : held));
    apply(next);
    // One write in flight at a time. Two quick taps otherwise race, and the
    // host's last-writer-wins can settle on the opposite of what the switch shows.
    setInboxSaving(true);
    try {
      await setInboxEnabled(client, company, agentId, next);
    } catch (error) {
      apply(!next);
      toast.error(
        error instanceof ApiError && error.status === 404
          ? "This host doesn't offer teammate inboxes yet."
          : error instanceof Error
            ? error.message
            : "Couldn't change the inbox.",
      );
    } finally {
      setInboxSaving(false);
    }
  }

  /**
   * Set, change, or remove this teammate's daily cap (issue #1206, moved here
   * from the roster card for the same reason Inbox moved in #1190: a card in
   * a grid of thirteen is for recognising a teammate, not configuring one).
   *
   * `cap` is `null` to remove the cap and a number to set one — `0` included,
   * which caps the teammate at nothing. The two are different states on the
   * host and must stay different here, which is why this takes `number | null`
   * and never an optional.
   *
   * Merges the host's answer into `agent` rather than refetching, the same way
   * `toggleInbox` does — and the same `held?.id === agentId` guard, so a slow
   * write does not clobber state after the operator has navigated elsewhere.
   */
  async function applyBudget(cap: number | null) {
    try {
      const row = await client.setTeamBudget(agentId, cap, company);
      setAgent((held) =>
        held?.id === agentId
          ? {
              ...held,
              budgetUsdDaily: row.budgetUsdDaily,
              spentTodayUsd: row.spentTodayUsd,
              budgetSetBy: row.budgetSetBy,
              budgetSetAtMillis: row.budgetSetAtMillis,
            }
          : held,
      );
      toast.success(cap === null ? "Daily cap removed." : `Daily cap set to $${cap.toFixed(2)}.`);
    } catch (error) {
      toast.error(budgetError(error, "Couldn't change the daily cap."));
    }
  }

  /** Drop the override so the company's own default applies again. */
  async function resetBudget() {
    try {
      const row = await client.clearTeamBudgetOverride(agentId, company);
      setAgent((held) =>
        held?.id === agentId
          ? {
              ...held,
              budgetUsdDaily: row.budgetUsdDaily,
              spentTodayUsd: row.spentTodayUsd,
              budgetSetBy: row.budgetSetBy,
              budgetSetAtMillis: row.budgetSetAtMillis,
            }
          : held,
      );
      toast.success("Reset to the company default.");
    } catch (error) {
      toast.error(budgetError(error, "Couldn't reset the daily cap."));
    }
  }

  /**
   * Save a chosen face, or `undefined` to go back to the hashed default.
   *
   * Its own write rather than a field of the edit form: a face is picked by
   * clicking it, and making that click wait for a Save button — in a form whose
   * other fields are text — would be the only place in the console where
   * choosing something visual is a two-step commit. Same identity guard as the
   * budget and inbox writes, for the same reason.
   */
  async function saveAvatar(avatar: string | undefined) {
    if (!agent) return;
    // The picker is disabled for the duration (see `avatarSaving`), so at most
    // one avatar PATCH can be pending at a time — an older response can never
    // land after a newer choice was saved.
    setAvatarSaving(true);
    try {
      const updated = await client.updateAgent(agentId, { avatar: avatar ?? null }, company);
      if (displayedAgentIdRef.current !== agentId) return;
      setAgent(updated);
      toast.success(avatar ? "Icon updated." : "Back to the default icon.");
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Couldn't change this teammate's icon.",
      );
    } finally {
      setAvatarSaving(false);
    }
  }

  async function save() {
    if (!agent) return;
    const edits = agentEdits(agent, draft);
    if (!edits) {
      setEditing(false);
      return;
    }
    setSaving(true);
    try {
      const updated = await client.updateAgent(agentId, edits, company);
      // A slow save must not clobber the active detail: only fold the response
      // in when the agent on screen is still the one we saved (the same guard
      // the reset and budget writes use).
      if (displayedAgentIdRef.current !== agentId) return;
      setAgent(updated);
      setDraft(draftFrom(updated));
      setEditing(false);
      toast.success("Teammate updated.");
    } catch (error) {
      toast.error(
        error instanceof ApiError && error.status === 409
          ? error.message
          : error instanceof Error
            ? error.message
            : "Couldn't save this teammate.",
      );
    } finally {
      setSaving(false);
    }
  }

  /**
   * Write this teammate's own tool-grant list — the Tools card is a report
   * AND an editor (the read-only report of a decision nobody could change was
   * the dead end this card existed to end).
   *
   * Its own write rather than a field on the shared draft, because `tools` is
   * not shaped like the others: the host gates it on admin where name, role and
   * instructions are member-open, so folding it into `save()` would make every
   * ordinary edit by a member 403 the moment a stale tools value rode along.
   * Sent alone, a member never sends the key at all.
   */
  async function saveTools(globs: string[] | null) {
    if (!agent) return;
    setSaving(true);
    try {
      // Three-state (issue #1804): `null` resets to the standard company grant,
      // `[]` is a deliberate deny-all, a non-empty list narrows. All three are
      // meaningful on the wire, so the value is passed through untouched.
      const updated = await client.updateAgent(agentId, { tools: globs }, company);
      // A slow save must not clobber the active detail: only fold the response
      // in when the agent on screen is still the one we saved (the same guard
      // the ordinary save, reset, budget and inbox writes use).
      if (displayedAgentIdRef.current !== agentId) return;
      setAgent(updated);
      toast.success("Tool grants updated.");
    } catch (error) {
      toast.error(
        error instanceof ApiError
          ? error.message
          : error instanceof Error
            ? error.message
            : "Couldn't save these tool grants.",
      );
      // Rethrow so the card keeps its editor open on a refusal — closing it
      // would read as "saved" for a write the host rejected.
      throw error;
    } finally {
      setSaving(false);
    }
  }

  /**
   * Save the harness binding, the model override, or both (issue #1245's
   * harness-picker follow-up) — one `PATCH`, so the host's cross-field check
   * (a model only means anything on the harness this same save leaves the
   * teammate on) validates against the *new* binding, not a stale one from
   * before this edit. Either field left unchanged is simply omitted, the same
   * partial-save contract `agentEdits`/`save` follow above; a blank model
   * draft still clears with `null` rather than being refused.
   */
  async function saveHarnessAndModel() {
    if (!agent) return;
    const harness = harnessEdit(agent.harness, harnessDraft === HARNESS_DEFAULT ? "" : harnessDraft);
    const model = modelEdit(agent.model, modelDraft);
    if (harness === undefined && model === undefined) {
      setEditingHarness(false);
      return;
    }
    const edits: EditAgentInput = {};
    if (harness !== undefined) edits.harness = harness;
    if (model !== undefined) edits.model = model;

    setSavingHarness(true);
    try {
      const updated = await client.updateAgent(agentId, edits, company);
      // The same guard the ordinary save, reset, budget and inbox writes use,
      // and missing here: this view stays mounted across teammates, so a slow
      // harness save for A landing after a click through to B would fold A's
      // response into B's card and show the wrong binding until a reload.
      if (displayedAgentIdRef.current !== agentId) return;
      setAgent(updated);
      setEditingHarness(false);
      toast.success("Harness updated.");
    } catch (error) {
      toast.error(
        error instanceof ApiError && (error.status === 403 || error.status === 400)
          ? error.message
          : error instanceof Error
            ? error.message
            : "Couldn't save this teammate's harness.",
      );
    } finally {
      setSavingHarness(false);
    }
  }

  /**
   * Drop this teammate's instructions override so its blueprint persona applies
   * again (issue #1530). Sends `instructions: null` — the three-state reset,
   * distinct from saving an emptied field only in that it needs no edit form
   * open. Offered only when an override is actually in force.
   */
  async function resetInstructions() {
    if (!agent) return;
    setSaving(true);
    try {
      const updated = await client.updateAgent(agentId, { instructions: null }, company);
      // A slow reset must not clobber the active detail: only fold the response
      // in when the agent on screen is still the one we asked to reset (the same
      // identity guard the budget writes and inbox toggle use).
      if (displayedAgentIdRef.current !== agentId) return;
      setAgent(updated);
      setDraft(draftFrom(updated));
      setEditing(false);
      toast.success("Instructions reset to the blueprint.");
    } catch (error) {
      toast.error(
        error instanceof ApiError && error.status === 409
          ? error.message
          : error instanceof Error
            ? error.message
            : "Couldn't reset the instructions.",
      );
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-3xl space-y-6 px-4 py-6">
        {/*
          A breadcrumb rather than a Back button (issue #1141). Back said where
          the operator had been; this says where they *are* — one teammate,
          inside the company — which is the question a linked page has to answer,
          and this page is linked from the org chart, the chat member pane and
          every "Not on a desk" chip. Arriving from any of those, "Back to team"
          named a page they had never seen.
        */}
        <nav aria-label="Breadcrumb" data-testid="agent-breadcrumb">
          <ol className="flex flex-wrap items-center gap-1 text-sm">
            <li>
              <Button
                variant="ghost"
                size="sm"
                className="-ml-2 h-7 px-2 text-muted-foreground"
                onClick={onBack}
                data-testid="agent-breadcrumb-company"
              >
                Company
              </Button>
            </li>
            <li aria-hidden className="text-muted-foreground">
              <ChevronRight className="size-3.5" />
            </li>
            <li aria-current="page" className="min-w-0 truncate font-medium">
              {/* Named as soon as there is a name, and "Teammate" until then.
                  A crumb that appeared only once the read landed would move
                  the page's controls across the row as it settled. */}
              {agent ? (agent.name?.trim() || agent.role) : "Teammate"}
            </li>
          </ol>
        </nav>

        {/*
          The page's accessible name in the four states `Identity` does not
          mount for (codex review, #1785). `Identity`'s `h1` is this page's
          only heading and it renders only once the teammate has loaded, so a
          direct `#/team/<id>` visit that was still loading — or that landed on
          a removed teammate, an older host, or a failed read — was a page a
          screen reader could not announce at all.

          `hidden`, because the breadcrumb above already says where you are and
          a title bar over a skeleton would be chrome about nothing.

          The name is gated on `load === "ready"` and not merely on `agent`
          being set (coderabbit review). `boot()` moves `load` to `"loading"`
          on an `agentId` change but keeps the previous `agent` until the new
          request settles, so keying off `agent` alone announced the teammate
          you just navigated *away from* as the name of the page you navigated
          *to* — a wrong name, which is worse than a generic one. The crumb has
          the same shape and can afford it: it is visible text next to the
          controls, changing in place, rather than the one string a screen
          reader announces on arrival.
        */}
        {load !== "ready" || !agent ? (
          <PageHeader title="Teammate" hidden />
        ) : null}

        {load === "loading" && <Skeleton className="h-64 rounded-xl" />}

        {load === "missing" && (
          <EmptyState
            title="This teammate is no longer on the roster."
            body="It may have been removed. Go back to the team to see who is here now."
          />
        )}

        {load === "unsupported" && (
          <EmptyState
            title="This host can't open a teammate yet."
            body="Opening a teammate needs a newer host. The roster still works."
          />
        )}

        {load === "error" && (
          <EmptyState
            title="Couldn't load this teammate."
            body="The company host didn't answer. Try again in a moment."
          />
        )}

        {load === "ready" && agent && (
          <>
            <Identity
              agent={agent}
              // The host's own `editable` list decides, never this file: a host
              // predating the field lists no `avatar`, and offering the picker
              // there would be a click whose save is a 400. Same rule the edit
              // form's fields follow.
              onPickAvatar={
                agent.editable.includes("avatar")
                  ? () => setAvatarOpen(true)
                  : undefined
              }
              avatarBusy={avatarSaving}
              action={
                !editing ? (
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setEditing(true)}
                    // Disabled with the reason, never absent — an operator
                    // looking for the edit needs to find out *why* there isn't
                    // one, not to conclude the console forgot to build it. What
                    // makes a teammate uneditable is the host's own `editable`
                    // list and nothing this file decides: a current host offers
                    // at least name, role and instructions on every teammate,
                    // manifest ones included, so an empty list now means a host
                    // that does not support the edit rather than a blueprint row
                    // this console must refuse.
                    disabled={agent.editable.length === 0}
                    title={
                      agent.editable.length === 0
                        ? "This teammate can't be edited from here."
                        : undefined
                    }
                    data-testid="agent-edit"
                  >
                    <Pencil className="size-4" /> Edit
                  </Button>
                ) : undefined
              }
            />
            <FactLine agent={agent} workload={workload} />
            <OpenTasks tasks={openTasks} />

            {/* What this teammate has actually done (issue #1573), directly
                under what it is doing now. Everything below this point defines
                the teammate — instructions, tools, inbox, budget — and the
                record of its work reads before its definition, not after four
                cards of configuration. */}
            <AgentRuns
              client={client}
              company={company}
              agentId={agent.id}
              agentName={agent.name?.trim() || agent.role}
            />

            {/* The Edit action sits on the teammate's name row (issue #1434) —
                one editing action, in the place a page's actions live, rather
                than halfway down inside one of its cards. */}
            <Section
              title="Instructions"
              subtitle="What this teammate was defined to do. It frames every turn they take."
              action={
                // Reset is offered only when an override is actually masking the
                // blueprint, and only to a viewer the host will let write
                // instructions — otherwise it is a control that can only 409.
                isEditable(agent, "instructions") && agent.instructionsOverridden ? (
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => void resetInstructions()}
                    disabled={saving}
                    data-testid="agent-instructions-reset"
                  >
                    Reset to blueprint
                  </Button>
                ) : undefined
              }
            >
              {editing ? (
                <div className="grid gap-4">
                  <AgentFields
                    idPrefix="agent-edit"
                    draft={draft}
                    onChange={(key: AgentFieldKey, value) =>
                      setDraft((d) => ({ ...d, [key]: value }))
                    }
                    readOnly={(key) => !isEditable(agent, key)}
                    copilot={(key) =>
                      key === "description" || key === "instructions" ? (
                        <FieldCopilot
                          field={key}
                          // Addressed by id: this teammate exists, so the host
                          // grounds the draft in its own record rather than in
                          // anything this console sends.
                          onTurn={(conversation) =>
                            draftAgentField(client, company, agentId, key, conversation, {
                              // The form's own values, not the host's. An
                              // operator who took a draft and has not saved is
                              // looking at something the record does not have,
                              // and a copilot grounded in the record would
                              // refine a version that is no longer on screen.
                              description: draft.description,
                              instructions: draft.instructions,
                              // Identity too: both prompts are written FROM the
                              // role, so a teammate repurposed on this form and
                              // drafted for before Save would otherwise get a
                              // mandate for the job it used to do.
                              role: draft.role,
                              name: draft.name,
                            })
                          }
                          // Fills the form draft and nothing else. The Save
                          // below is still what writes, which is what makes a
                          // drafted persona no different from a typed one.
                          onAccept={(text) => setDraft((d) => ({ ...d, [key]: text }))}
                          // A blank role is refused here for the reason the
                          // Add form refuses it: both briefs are written FROM
                          // the role. The wire drops a blank one rather than
                          // sending it, so the host would fall back to the
                          // STORED role and draft for the job this teammate is
                          // being moved off — the one thing the operator is
                          // mid-way through changing.
                          disabled={saving || cognition === "echo" || !draft.role.trim()}
                          disabledNotice={
                            cognition === "echo"
                              ? "No model is configured, so the copilot can't draft yet."
                              : !draft.role.trim()
                                ? "Give this teammate a role first — the copilot drafts from it."
                                : undefined
                          }
                        />
                      ) : null
                    }
                  />
                  {agent.instructionsOverridden && agent.blueprintInstructions?.trim() && (
                    <p
                      className="whitespace-pre-wrap text-xs text-muted-foreground"
                      data-testid="agent-blueprint-hint"
                    >
                      Overriding the blueprint. Clearing this field, or “Reset to blueprint”,
                      restores: {agent.blueprintInstructions.trim()}
                    </p>
                  )}
                  <div className="flex items-center justify-end gap-2">
                    {/* Why Save is dead, next to Save (issue #1776). A manifest
                        teammate carries no name of its own, so this form opens
                        with Name blank and the button already disabled — and
                        until this line the only way to find that out was to
                        guess. The fields themselves are marked too; this says
                        it where the operator is looking when they wonder. */}
                    {missing.length > 0 && (
                      <p
                        className="mr-auto text-2xs text-muted-foreground"
                        data-testid="agent-save-blocked"
                      >
                        {missing.map((field) => field.label).join(" and ")}{" "}
                        {missing.length > 1 ? "are" : "is"} required to save.
                      </p>
                    )}
                    <Button
                      variant="ghost"
                      onClick={() => {
                        setDraft(draftFrom(agent));
                        setEditing(false);
                      }}
                    >
                      Cancel
                    </Button>
                    <Button
                      onClick={() => void save()}
                      disabled={saving || !draftIsValid(agent, draft)}
                      data-testid="agent-save"
                    >
                      Save
                    </Button>
                  </div>
                </div>
              ) : (
                <>
                  <p
                    className="whitespace-pre-wrap text-sm text-muted-foreground"
                    data-testid="agent-description"
                  >
                    {agent.description?.trim() ||
                      "No description was written for this teammate."}
                  </p>
                  {agent.instructions?.trim() && (
                    <div className="space-y-1">
                      <p className="text-xs font-medium">
                        Persona instructions
                        {agent.instructionsOverridden ? " · overriding the blueprint" : ""}
                      </p>
                      <p
                        className="whitespace-pre-wrap text-sm text-muted-foreground"
                        data-testid="agent-instructions"
                      >
                        {agent.instructions.trim()}
                      </p>
                    </div>
                  )}
                  {agent.editable.length === 0 && (
                    <p className="text-xs text-muted-foreground" data-testid="agent-readonly-note">
                      This teammate can't be edited from here. Its daily budget can still be changed
                      below.
                    </p>
                  )}
                </>
              )}
            </Section>

            <Tools agent={agent} saving={saving} onSave={(globs) => saveTools(globs)} />
            <HarnessAndModel
              agent={agent}
              harnesses={harnesses}
              editing={editingHarness}
              harnessDraft={harnessDraft}
              modelDraft={modelDraft}
              saving={savingHarness}
              onEdit={() => {
                setHarnessDraft(agent.harness ?? HARNESS_DEFAULT);
                setModelDraft(agent.model ?? "");
                setEditingHarness(true);
              }}
              onHarnessChange={(next) => {
                setHarnessDraft(next);
                // A model override only means anything against a harness that
                // can be told which model to run. Switching to a `built_in`
                // one — the host's own engine, whose model is the host's to
                // choose — must drop the override rather than save a value
                // that will silently never apply. Leaving it also made the
                // form claim a binding it was not going to honour.
                // The sentinel has to be resolved first, not excluded. "Company
                // default" is a *binding*, not a kind — when the company
                // default is `built_in`, picking it lands the teammate on a
                // managed harness exactly as naming one explicitly would. The
                // earlier version skipped the sentinel, so that route kept the
                // model, hid the control, and then sent the model anyway: the
                // host refused with a 400 against a field the operator could
                // no longer see or clear.
                const resolve = (id: string) =>
                  id === HARNESS_DEFAULT
                    ? harnesses.find((h) => h.default)
                    : harnesses.find((h) => h.id === id);
                const before = resolve(harnessDraft);
                const bound = resolve(next);

                // Two ways a model stops meaning anything, and both have to
                // clear it:
                //
                //   - the target is managed, whose model is the host's choice;
                //   - the target is a *different* ACP agent. Model ids are the
                //     agent's own vocabulary, so a Claude model handed to
                //     Codex is not refused — `model_config_id` simply fails to
                //     find it and the session stays on its default, while the
                //     page goes on claiming an override that is not applied.
                //
                // Compared on `agent` rather than harness id, since two
                // harnesses can drive the same CLI and a model is valid across
                // those.
                if (!bound || bound.kind !== "acp" || bound.agent !== before?.agent) {
                  setModelDraft("");
                }
              }}
              onModelChange={setModelDraft}
              onCancel={() => setEditingHarness(false)}
              onSave={() => void saveHarnessAndModel()}
            />
            <Inbox
              agent={agent}
              busy={inboxSaving}
              onToggle={(next) => void toggleInbox(next)}
            />
            <Budget
              agent={agent}
              canEdit={isAdmin}
              setByLabel={agent.budgetSetBy ? whoSet(agent.budgetSetBy) : undefined}
              onEdit={() => setBudgetOpen(true)}
              onRemoveCap={() => void applyBudget(null)}
              onResetBudget={() => void resetBudget()}
            />
          </>
        )}
      </div>
      <AvatarDialog
        client={client}
        company={company}
        agent={avatarOpen ? agent : null}
        busy={avatarSaving}
        onOpenChange={setAvatarOpen}
        onPick={(avatar) => {
          setAvatarOpen(false);
          void saveAvatar(avatar);
        }}
      />
      <BudgetDialog
        agent={budgetOpen ? agent : null}
        onOpenChange={setBudgetOpen}
        onSave={(cap) => {
          setBudgetOpen(false);
          void applyBudget(cap);
        }}
      />
    </div>
  );
}

/** Name, role, id, desks, and the two facts that classify an agent. */
function Identity({
  agent,
  action,
  onPickAvatar,
  avatarBusy,
}: {
  agent: AgentDetailDto;
  action?: ReactNode;
  /** Opens the icon picker. Absent leaves the tile inert — a read-only header. */
  onPickAvatar?: () => void;
  /** An icon save is in flight — the tile must not start another one. */
  avatarBusy?: boolean;
}) {
  const display = agent.name?.trim() || agent.role;
  const seed = agent.id || display;
  const tone = toneFor(seed);
  // What this teammate wears: the chosen face, else the mascot hashed from the
  // same seed as `tone` — the id where there is one — so a rename doesn't
  // change this teammate's face on the one screen that should never be
  // showing letters (issue #1181, and issue #1185 for the seed itself).
  const avatar = avatarRef(agent.avatar, seed);
  // #1208, on the page a teammate *is*. `display` already falls back to the
  // role, and a manifest-declared agent has no `name` at all, so the line under
  // the title was the title again on every teammate in every shipped company.
  const subtitle = roleSubtitle(display, agent.role);
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="flex items-start gap-4 min-w-0">
        {/* The header of the page a teammate *is* — the one screen that should
            never be the one showing letters (issue #1181). 56px. */}
        {/* The tile is the control. A face is a visual thing, so the way to
            change it is to click the one on screen rather than to hunt for a
            field named after it — and the hover ring is what says so, since an
            avatar that looks identical to an inert one is a button nobody
            finds. Falls back to a plain tile where there is no handler. */}
        {onPickAvatar ? (
          <button
            type="button"
            onClick={onPickAvatar}
            disabled={avatarBusy}
            aria-label="Change this teammate's icon"
            title="Change icon"
            className="rounded-xl ring-2 ring-transparent transition-colors hover:ring-primary focus-visible:ring-primary focus-visible:outline-none disabled:cursor-wait"
            data-testid="agent-avatar-pick"
          >
            <TeammateAvatar
              name={display}
              tone={tone}
              avatar={avatar}
              className="size-14 rounded-xl text-base"
              data-testid="agent-avatar"
            />
          </button>
        ) : (
          <TeammateAvatar
            name={display}
            tone={tone}
            avatar={avatar}
            className="size-14 rounded-xl text-base"
            data-testid="agent-avatar"
          />
        )}
        <div className="min-w-0 flex-1 space-y-2">
          <div>
            <h1 className="truncate text-2xl font-semibold tracking-tight" data-testid="agent-name">
              {display}
            </h1>
            {subtitle && (
              <p className="truncate text-sm text-muted-foreground" data-testid="agent-role">
                {subtitle}
              </p>
            )}
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <Badge variant="secondary" className="gap-1" data-testid="agent-tier">
              <Sparkles className="size-3" /> {tierLabel(agent)}
            </Badge>
            <Badge variant="outline" data-testid="agent-source">
              {agent.source === "manifest" ? "Company blueprint" : "Added here"}
            </Badge>
            {agent.desks.map((desk) => (
              <a
                key={desk.id}
                href={`#/company/${encodeURIComponent(desk.id)}`}
                className="inline-flex"
                data-testid={`agent-desk-${desk.id}`}
              >
                <Badge variant="secondary" className="gap-1">
                  <Users className="size-3" aria-hidden /> {desk.name}
                  {desk.lead && <span className="text-xs opacity-70">(lead)</span>}
                </Badge>
              </a>
            ))}
            {agent.inboxEnabled && (
              <Badge variant="outline" className="gap-1">
                <Mail className="size-3" /> Inbox
              </Badge>
            )}
          </div>
        </div>
      </div>
      {action && (
        <div className="flex shrink-0 flex-wrap justify-end gap-2">{action}</div>
      )}
    </div>
  );
}

/**
 * The three running facts about a teammate, on one line (issue #1141): what
 * they are on, how much is on them, and what today has cost.
 *
 * Every part is omitted independently when its source is silent, and none is
 * defaulted. A host that cannot answer the board draws no status and no count —
 * not "idle · 0 open", which is a claim — and an uncapped teammate draws no
 * spend line, because absence *is* the uncapped signal on the wire and `$0.00`
 * would read as a teammate capped at nothing.
 */
function FactLine({
  agent,
  workload,
}: {
  agent: AgentDetailDto;
  workload: Workload | null;
}) {
  const capped = agent.budgetUsdDaily !== undefined;
  if (!workload && !capped) return null;
  const working = workload?.status === "working";
  return (
    <div
      className="flex flex-wrap items-center gap-x-2 gap-y-2 text-sm text-muted-foreground"
      data-testid="agent-facts"
    >
      {workload && (
        <>
          <span className="flex items-center gap-1.5">
            <span
              className={cn(
                "size-2 shrink-0 rounded-full",
                working ? "bg-status-running" : "bg-status-idle",
              )}
              aria-hidden
            />
            <span
              className={cn(
                "font-medium",
                working ? "text-status-running-text" : "text-status-idle-text",
              )}
              data-testid="agent-status"
            >
              {working ? "Working" : "Idle"}
            </span>
          </span>
          <span aria-hidden>·</span>
          <span data-testid="agent-tasks">
            {workload.open === 1 ? "1 open task" : `${workload.open} open tasks`}
          </span>
        </>
      )}
      {capped && (
        <span data-testid="agent-spend">
          Today ${(agent.spentTodayUsd ?? 0).toFixed(2)} of $
          {(agent.budgetUsdDaily ?? 0).toFixed(2)}
        </span>
      )}
    </div>
  );
}

/** The open cards assigned to this teammate, linked to the work behind the count. */
function OpenTasks({ tasks }: { tasks: Task[] | null }) {
  if (!tasks?.length) return null;
  return (
    <div className="space-y-1" data-testid="agent-open-tasks">
      <p className="text-xs font-medium text-muted-foreground">Open tasks</p>
      <div className="flex flex-wrap gap-x-3 gap-y-1">
        {tasks.map((task) => (
          <a
            key={task.id}
            href={`#/tasks/${encodeURIComponent(task.id)}`}
            className="text-sm text-primary underline-offset-4 hover:underline"
            data-testid={`agent-open-task-${task.id}`}
          >
            {task.title}
          </a>
        ))}
      </div>
    </div>
  );
}

/**
 * The tool grants, resolved — and, for an admin, editable.
 *
 * Three facts, because the difference between them is the whole reason this
 * section exists. What the agent holds. Whether it holds it because it asked or
 * because it asked for nothing and inherited the company's grant. And what it
 * asked for and did not get, which is the line an operator checking a tool
 * change is actually looking for and which no surface showed before — and
 * which, as of this card becoming an editor, has a way to act on it.
 *
 * The edit surface is deliberately live: the preview of what will be stored is
 * computed against the same ceilings the host applies when it re-derives
 * `effective`, so a glob that would land struck-through is flagged while the
 * operator types, not after the write.
 */
function Tools({
  agent,
  saving,
  onSave,
}: {
  agent: AgentDetailDto;
  saving: boolean;
  onSave: (globs: string[] | null) => Promise<void>;
}) {
  const summary = summarizeGrants(agent.tools);
  const canEdit = isEditable(agent, "tools");
  const [editing, setEditing] = useState(false);
  // `requested` is three-state since #1804 (`null` = standard, `[]` = deny-all,
  // list = narrow); the text field only ever renders the concrete globs, so a
  // `null`/`[]` grant both start from an empty box.
  const requestedGlobs = agent.tools.requested ?? [];
  const [field, setField] = useState(requestedGlobs.join(", "));

  // The teammate on screen can change under this card (a slow detail load, a
  // sibling route swap), and a draft left over from the previous one would be
  // saved onto the new teammate. Re-seed whenever the stored list changes.
  useEffect(() => {
    setField((agent.tools.requested ?? []).join(", "));
    setEditing(false);
  }, [agent.id, agent.tools.requested]);

  const draft = parseToolGlobs(field);
  const dirty = toolGlobsDiffer(requestedGlobs, draft);
  // Live, before the save rather than after it: the intersection is the thing
  // operators get wrong, and a glob the desk-and-company ceiling does not allow
  // is stored happily and then confers nothing. Saying so while they type is the
  // whole reason this card knows the ceilings. The desk level is the gate when
  // a desk states one — `grantCeiling` is `deskAllow` when a ceiling is active,
  // else the company allow-list, matching the host's `agent_scoped_grants`
  // two-level application — because a desk that omits a company-allowed
  // namespace drops it immediately after saving. `deskCeilingActive` (not
  // `deskAllow`'s emptiness) is the sentinel: a ceiling whose narrowed list is
  // empty still narrows everything away.
  const deskCeilingActive = agent.tools.deskCeilingActive;
  const willNotApply = draft.filter((glob) => !companyCovers(grantCeiling(agent.tools), glob));

  return (
    <Section
      title="Tools"
      subtitle={
        summary.standardGrant
          ? deskCeilingActive
            ? "This teammate lists no tools of its own, so it holds what its desk allows, narrowed by the company."
            : "This teammate lists no tools of its own, so it holds everything the company allows."
          : summary.deniedAll
            ? "This teammate has been given an explicit empty grant, so it holds no tools at all."
            : "What this teammate asked for, narrowed by what its desk and the company allow."
      }
      action={
        canEdit && !editing ? (
          <Button
            variant="ghost"
            size="sm"
            onClick={() => setEditing(true)}
            data-testid="agent-tools-edit"
          >
            <Pencil className="size-4" /> Edit
          </Button>
        ) : undefined
      }
    >
      {editing && (
        <div className="grid gap-2" data-testid="agent-tools-editor">
          <Label htmlFor="agent-tools-field">Tool grants</Label>
          <Input
            id="agent-tools-field"
            value={field}
            onChange={(event) => setField(event.target.value)}
            placeholder="workspace.read, docs.*, files.*"
            className="font-mono text-xs"
            data-testid="agent-tools-field"
          />
          <p className="text-xs text-muted-foreground">
            One glob per grant, separated by commas or spaces. Each is narrowed by the
            company tool list below
            {deskCeilingActive ? " and by this teammate's desk ceiling" : ""}, so this
            can only ever take capability away — never add to it.
          </p>
          {draft.length === 0 && (
            // Since #1804 the inversion runs the other way: an empty list is a
            // deliberate deny-all, NOT the standard grant. An operator who
            // wants the standard grant back must use "Reset to standard" below.
            <p className="text-xs text-status-blocked-text" data-testid="agent-tools-empty-warning">
              Saving an empty list is a deny-all — this teammate would hold no tools at all. To
              give it the standard company grant instead, use “Reset to standard grant”.
            </p>
          )}
          {willNotApply.length > 0 && (
            <p className="text-xs text-status-blocked-text" data-testid="agent-tools-uncovered">
              {deskCeilingActive
                ? `The desk and company tool lists do not cover ${willNotApply.join(", ")}, so it will be stored and confer nothing.`
                : `The company tool list does not cover ${willNotApply.join(", ")}, so it will be stored and confer nothing.`}
            </p>
          )}
          <div className="flex justify-end gap-2">
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                setField(requestedGlobs.join(", "));
                setEditing(false);
              }}
            >
              Cancel
            </Button>
            {/* Reset to the standard grant (`null`) — a distinct action from
                saving an empty list (`[]`, a deny-all) since #1804. Only shown
                when the teammate is not already on the standard grant. */}
            {!summary.standardGrant && (
              <Button
                variant="ghost"
                size="sm"
                disabled={saving}
                onClick={() => {
                  void onSave(null).then(
                    () => setEditing(false),
                    () => undefined,
                  );
                }}
                data-testid="agent-tools-reset"
              >
                Reset to standard grant
              </Button>
            )}
            <Button
              size="sm"
              disabled={saving || !dirty}
              onClick={() => {
                // The editor closes on success and stays open on a refusal;
                // the toast is raised by the caller, so the rejection is
                // swallowed here rather than left unhandled. An empty `draft`
                // is a deliberate deny-all (`[]`), not a reset — that is the
                // separate "Reset to standard grant" button above.
                void onSave(draft).then(
                  () => setEditing(false),
                  () => undefined,
                );
              }}
              data-testid="agent-tools-save"
            >
              Save
            </Button>
          </div>
        </div>
      )}
      {summary.effective.length === 0 ? (
        <p className="text-sm text-muted-foreground" data-testid="agent-tools-empty">
          {/* The ways of holding nothing land here, and they are not the same
              fact. An agent on the standard grant under a company that allows
              nothing has been refused nothing; a deny-all agent asked to hold
              nothing; a narrowed agent asked for tools none of which are
              covered. */}
          {summary.standardGrant
            ? "This teammate has no tools, because the company allows none."
            : summary.deniedAll
              ? "This teammate has no tools: it was given an explicit empty (deny-all) grant."
              : "This teammate has no tools. Nothing it asked for is covered by the company tool list."}
        </p>
      ) : (
        <div className="flex flex-wrap gap-2" data-testid="agent-tools">
          {summary.effective.map((glob) => (
            <Badge key={glob} variant="secondary" className="gap-1 font-mono text-xs">
              <Wrench className="size-3" /> {glob}
            </Badge>
          ))}
        </div>
      )}
      {summary.dropped.length > 0 && (
        <div className="space-y-1" data-testid="agent-tools-dropped">
          <p className="text-xs text-muted-foreground">
            Asked for but not granted, because the company tool list does not cover it:
          </p>
          <div className="flex flex-wrap gap-2">
            {summary.dropped.map((glob) => (
              <Badge key={glob} variant="outline" className="font-mono text-xs line-through">
                {glob}
              </Badge>
            ))}
          </div>
        </div>
      )}
      {!summary.standardGrant && (
        <p className="text-xs text-muted-foreground">
          Company tool list: {agent.tools.companyAllow.join(", ") || "nothing allowed"}
          {deskCeilingActive && (
            <>
              {" · "}
              Desk tool list: {agent.tools.deskAllow.join(", ") || "nothing allowed"}
            </>
          )}
        </p>
      )}
    </Section>
  );
}

/**
 * This teammate's harness binding and its own model override (issue #1245's
 * harness-picker follow-up).
 *
 * One section, one edit control, for both: they are saved together (see
 * `saveHarnessAndModel`'s own docs on why), and the model field's very
 * relevance depends on which harness is selected — showing it in a card of
 * its own would let an operator set a model with no harness in view to judge
 * whether it does anything.
 *
 * Admin-only, same as `tools` (the "cost/scope decision" reasoning), and
 * `agent.editable` says so per actor — a member sees the values with no edit
 * affordance, the same shape `agent.editable.length === 0` already gives a
 * manifest teammate above.
 */
function HarnessAndModel({
  agent,
  harnesses,
  editing,
  harnessDraft,
  modelDraft,
  saving,
  onEdit,
  onHarnessChange,
  onModelChange,
  onCancel,
  onSave,
}: {
  agent: AgentDetailDto;
  harnesses: HarnessDto[];
  editing: boolean;
  harnessDraft: string;
  modelDraft: string;
  saving: boolean;
  onEdit: () => void;
  onHarnessChange: (value: string) => void;
  onModelChange: (value: string) => void;
  onCancel: () => void;
  onSave: () => void;
}) {
  const editable = agent.editable.includes("harness") || agent.editable.includes("model");
  const declaredKind = resolvedHarnessKind(harnesses, agent.harness);
  const draftKind = resolvedHarnessKind(
    harnesses,
    harnessDraft === HARNESS_DEFAULT ? undefined : harnessDraft,
  );
  const defaultHarness = harnesses.find((h) => h.default);

  /**
   * The models the drafted harness advertises.
   *
   * Fetched when the editor opens on an ACP harness, and again whenever the
   * operator picks a different one — the lists are per harness and share no
   * ids, so carrying claude's over to codex would offer models it will
   * silently refuse. Cached in the transport, so switching back and forth
   * spawns nothing after the first look.
   */
  const [models, setModels] = useState<AcpHarnessModel[]>([]);
  const draftHarnessId = harnessDraft === HARNESS_DEFAULT ? defaultHarness?.id : harnessDraft;
  // What the *desktop* calls this harness. A manifest binding and the shell's
  // catalogue key are different things — `id = "laptop", agent = "claude"` is
  // a supported shape — and asking the shell about `laptop` returns no models
  // at all rather than claude's.
  const draftAgentId = harnesses.find((h) => h.id === draftHarnessId)?.agent ?? draftHarnessId;

  useEffect(() => {
    if (!editing || draftKind !== "acp" || !draftAgentId) {
      setModels([]);
      return;
    }
    let live = true;
    setModels(cachedAcpModels(draftAgentId));
    void ensureAcpModels(draftAgentId).then((found) => {
      if (live) setModels(found);
    });
    return () => {
      live = false;
    };
  }, [editing, draftKind, draftAgentId]);

  const unlistedModel =
    modelDraft && !models.some((m) => m.value === modelDraft) ? modelDraft : undefined;
  /**
   * What the harness would use if this teammate pins nothing — the entry the
   * adapter itself reports as current, not a guess. Absent when the adapter
   * names none (`claude-agent-acp` leads its list with a synthetic `default`
   * instead), in which case the option stays unqualified rather than
   * inventing an answer.
   */
  const currentModel = models.find((m) => m.current);
  const modelLabel = () => {
    if (!modelDraft) return "Whatever the harness defaults to";
    const found = models.find((m) => m.value === modelDraft);
    return found ? (found.name ?? found.value) : modelDraft;
  };

  // `Select.Value` cannot read a label off its matching `SelectItem` here —
  // `SelectContent` (and every item in it) is portal-rendered only while the
  // popup is open, so a trigger that has never been opened has nothing to
  // read from and falls back to the raw value (issue #1245's harness-picker
  // follow-up shipped with exactly that: the trigger read the literal
  // `__default__` sentinel until this closed-form label was added). Passing
  // the render-function form sidesteps the mount-order dependency entirely.
  const harnessLabel = (value: string) =>
    value === HARNESS_DEFAULT
      ? `Company default${defaultHarness ? ` (${harnessOptionLabel(defaultHarness)})` : ""}`
      : (() => {
          const found = harnesses.find((h) => h.id === value);
          return found ? harnessOptionLabel(found) : value;
        })();

  return (
    <Section
      title="Harness & model"
      subtitle="Which coding engine this teammate runs on, and — on an ACP harness (an operator's own coding CLI) — which model to pin it to."
      action={
        editable && !editing ? (
          <Button variant="ghost" size="sm" onClick={onEdit} data-testid="agent-harness-edit">
            <Pencil className="size-4" />
          </Button>
        ) : undefined
      }
    >
      {editing ? (
        <div className="space-y-3">
          <Select value={harnessDraft} onValueChange={(value) => onHarnessChange(value ?? HARNESS_DEFAULT)}>
            <SelectTrigger className="w-full" data-testid="agent-harness-select">
              <SelectValue>{harnessLabel}</SelectValue>
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={HARNESS_DEFAULT}>
                Company default{defaultHarness ? ` (${harnessOptionLabel(defaultHarness)})` : ""}
              </SelectItem>
              {harnesses.map((harness) => (
                <SelectItem key={harness.id} value={harness.id}>
                  {harnessOptionLabel(harness)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {draftKind === "acp" ? (
            models.length > 0 ? (
              <>
                <Select
                  value={modelDraft === "" ? MODEL_HARNESS_DEFAULT : modelDraft}
                  onValueChange={(value) =>
                    onModelChange(!value || value === MODEL_HARNESS_DEFAULT ? "" : value)
                  }
                >
                  <SelectTrigger className="w-full" data-testid="agent-model-select">
                    <SelectValue>{modelLabel}</SelectValue>
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value={MODEL_HARNESS_DEFAULT}>
                      Whatever the harness defaults to
                      {/* Named rather than left abstract: leaving this alone
                          is the common choice, and an operator should not
                          have to guess what they are choosing. The adapter
                          reports which entry is current, so this tracks the
                          CLI's own default instead of asserting one. */}
                      {currentModel && (
                        <span className="text-muted-foreground">
                          {" "}
                          — {currentModel.name ?? currentModel.value}
                        </span>
                      )}
                    </SelectItem>
                    {models.map((model) => (
                      <SelectItem key={model.value} value={model.value}>
                        {model.name ?? model.value}
                        {model.description && (
                          <span className="text-muted-foreground"> — {model.description}</span>
                        )}
                      </SelectItem>
                    ))}
                    {/* A value the harness no longer advertises is still
                        offered, so opening the editor cannot silently drop a
                        pin somebody set deliberately. The list moves when the
                        CLI updates; the teammate's setting should not. */}
                    {unlistedModel && (
                      <SelectItem value={unlistedModel}>
                        {unlistedModel}
                        <span className="text-muted-foreground"> — no longer offered</span>
                      </SelectItem>
                    )}
                  </SelectContent>
                </Select>
                <p className="text-xs text-muted-foreground">
                  Read from the harness itself, so these are the models it will actually
                  accept.
                </p>
              </>
            ) : (
              // No list to offer. Free text rather than an empty dropdown:
              // "nothing cached yet" (a browser, or a harness never probed)
              // is not the same as "this harness has no models", and an empty
              // picker would assert the second.
              <>
                <Input
                  value={modelDraft}
                  onChange={(event) => onModelChange(event.target.value)}
                  placeholder="Leave blank to use the harness's own default"
                  data-testid="agent-model-input"
                />
                <p className="text-xs text-muted-foreground">
                  {isDesktopRuntime()
                    ? "This harness hasn't reported its models yet — open Settings › External harnesses to check it."
                    : "Open the desktop app to pick from the models this harness offers."}
                </p>
              </>
            )
          ) : (
            <p className="text-xs text-muted-foreground">
              A model override only applies on an ACP harness — pick one above to set one.
            </p>
          )}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" onClick={onCancel} disabled={saving}>
              Cancel
            </Button>
            <Button onClick={onSave} disabled={saving} data-testid="agent-harness-save">
              Save
            </Button>
          </div>
        </div>
      ) : (
        <div className="flex flex-wrap items-center gap-2">
          <Badge variant="secondary" className="gap-1 font-mono text-xs" data-testid="agent-harness">
            <Server className="size-3" />
            {agent.harness ?? (defaultHarness ? `${defaultHarness.id} (default)` : "default harness")}
          </Badge>
          {agent.model ? (
            <Badge variant="secondary" className="gap-1 font-mono text-xs" data-testid="agent-model">
              <Cpu className="size-3" /> {agent.model}
            </Badge>
          ) : (
            <span className="text-sm text-muted-foreground" data-testid="agent-model-empty">
              {declaredKind === "acp"
                ? "No model override set — uses the harness's own default."
                : "No model override (this harness has no ACP transport to steer)."}
            </span>
          )}
        </div>
      )}
    </Section>
  );
}

/**
 * Whether mail addressed to this teammate lands anywhere (issue #1190).
 *
 * A per-teammate setting, on the teammate's own page — not a switch in a grid
 * of cards, which is what it was. The subtitle says what turning it on actually
 * does, because "Inbox" alone does not: an inbox is an address the outside
 * world can reach, which is a different kind of decision from the rest of this
 * screen and worth one sentence.
 */
function Inbox({
  agent,
  busy,
  onToggle,
}: {
  agent: AgentDetailDto;
  /** A write is in flight — the switch is held rather than allowed to race. */
  busy: boolean;
  onToggle: (next: boolean) => void;
}) {
  return (
    <Section
      title="Inbox"
      subtitle="Give this teammate an address of its own, so mail routed to it arrives here rather than nowhere."
    >
      <label className="flex cursor-pointer items-center justify-between gap-3">
        <span className="flex items-center gap-2 text-sm">
          <Mail className="size-4 text-muted-foreground" />
          {agent.inboxEnabled ? "This teammate has an inbox." : "This teammate has no inbox."}
        </span>
        <Switch
          checked={agent.inboxEnabled}
          disabled={busy}
          onCheckedChange={onToggle}
          aria-label="Give this teammate an inbox"
          data-testid="agent-inbox-toggle"
        />
      </label>
    </Section>
  );
}

/**
 * Turns a failed budget write into something worth reading.
 *
 * The 403 is the one an operator will actually hit, and it needs to say *why* —
 * "only an admin can change a spend limit" is the answer, not "request failed".
 */
function budgetError(error: unknown, fallback: string): string {
  if (error instanceof ApiError) {
    if (error.status === 403) return "Only an admin can change a teammate's daily cap.";
    if (error.status === 404) return "This host doesn't support console budgets yet.";
    return error.message;
  }
  return error instanceof Error ? error.message : fallback;
}

/**
 * The teammate's daily spend cap, editable (issue #1206).
 *
 * Moved here from the roster card's `⋯` menu, for the same reason Inbox moved
 * in #1190: a card in a grid of thirteen is for recognising a teammate, not
 * configuring one. The card still shows the cap and today's spend — this is
 * where an operator now sets, changes, removes or resets it, beside Inbox.
 *
 * Renders the cap and attribution to everyone (the roster card does too), but
 * only offers the writing controls to an admin — same courtesy-not-enforcement
 * gate `TeamView.tsx` used, so a member sees the same facts without a control
 * that would only 403.
 */
function Budget({
  agent,
  canEdit,
  setByLabel,
  onEdit,
  onRemoveCap,
  onResetBudget,
}: {
  agent: AgentDetailDto;
  /** Whether to offer the writing controls at all (admins only). */
  canEdit: boolean;
  /** Who set the current override, already resolved to something readable. */
  setByLabel?: string;
  onEdit: () => void;
  onRemoveCap: () => void;
  onResetBudget: () => void;
}) {
  const cap = agent.budgetUsdDaily;
  const capped = cap !== undefined;
  // An override exists (someone set this deliberately), as opposed to the cap
  // simply coming from the company's own definition.
  const overridden = agent.budgetSetBy !== undefined;
  const usd = (n: number) => `$${n.toFixed(2)}`;
  return (
    <Section
      title="Budget"
      subtitle="The most this teammate may spend per day. It takes effect on their next task — no restart needed."
      action={
        canEdit ? (
          <Button variant="outline" size="sm" onClick={onEdit} data-testid="team-budget-edit">
            <Wallet className="size-4" />
            {capped ? "Change…" : "Set…"}
          </Button>
        ) : undefined
      }
    >
      <div className="space-y-1 text-sm" data-testid="agent-budget">
        {capped ? (
          <p className="text-muted-foreground">
            {usd(cap)}/day · {usd(agent.spentTodayUsd ?? 0)} spent today
          </p>
        ) : (
          <p className="text-muted-foreground">No daily cap — this teammate spends freely.</p>
        )}
        {setByLabel && agent.budgetSetAtMillis !== undefined && (
          <p className="text-xs text-muted-foreground" data-testid="agent-budget-attribution">
            {capped ? "Set by" : "Uncapped by"} {setByLabel} ·{" "}
            {new Date(agent.budgetSetAtMillis).toLocaleDateString()}
          </p>
        )}
      </div>
      {canEdit && (capped || overridden) && (
        <div className="flex flex-wrap gap-2">
          {capped && (
            <Button
              variant="ghost"
              size="sm"
              onClick={onRemoveCap}
              data-testid="team-budget-remove"
            >
              Remove cap
            </Button>
          )}
          {overridden && (
            <Button
              variant="ghost"
              size="sm"
              onClick={onResetBudget}
              data-testid="team-budget-reset"
            >
              Reset to company default
            </Button>
          )}
        </div>
      )}
    </Section>
  );
}

/**
 * Pick a teammate's icon.
 *
 * Picking **is** the save — there is no Save button — because a face is chosen
 * by clicking it and a confirm step over a visual choice only adds a way to
 * lose it. The dialog closes on the click, and the write reports itself with a
 * toast like every other one-click write on this page.
 */
function AvatarDialog({
  client,
  company,
  agent,
  busy,
  onOpenChange,
  onPick,
}: {
  client: OpenCompanyClient;
  company: string | null;
  agent: AgentDetailDto | null;
  /** An avatar save is in flight — the picker is inert until it settles. */
  busy: boolean;
  onOpenChange: (open: boolean) => void;
  onPick: (avatar: string | undefined) => void;
}) {
  const name = agent?.name?.trim() || agent?.role || "this teammate";
  return (
    <Dialog open={agent !== null} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Icon</DialogTitle>
          <DialogDescription>
            The face {name} wears everywhere in this console — chat, the org chart, every list
            they appear in.
          </DialogDescription>
        </DialogHeader>
        {agent && (
          <AvatarPicker
            client={client}
            company={company}
            value={agent.avatar}
            seed={agent.id || name}
            name={name}
            tone={toneFor(agent.id || name)}
            disabled={busy}
            onChange={onPick}
          />
        )}
      </DialogContent>
    </Dialog>
  );
}

/**
 * Enter a daily cap for one teammate.
 *
 * Empty input is **not** submittable: "no cap" is the explicit "Remove cap"
 * action, not a blank field, so an operator clearing the box and saving can
 * never silently uncap a teammate. `0` is allowed and means exactly what it
 * says — this teammate may not spend.
 */
function BudgetDialog({
  agent,
  onOpenChange,
  onSave,
}: {
  agent: AgentDetailDto | null;
  onOpenChange: (open: boolean) => void;
  onSave: (cap: number) => void;
}) {
  const [value, setValue] = useState("");

  useEffect(() => {
    setValue(agent?.budgetUsdDaily !== undefined ? String(agent.budgetUsdDaily) : "");
  }, [agent]);

  const parsed = Number(value);
  const valid = value.trim() !== "" && Number.isFinite(parsed) && parsed >= 0;
  const name = agent?.name?.trim() || agent?.role || "this teammate";

  return (
    <Dialog open={agent !== null} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Daily budget</DialogTitle>
          <DialogDescription>
            The most {name} may spend per day. It takes effect on their next task — no restart
            needed.
          </DialogDescription>
        </DialogHeader>
        <div className="grid gap-2">
          <Label htmlFor="agent-budget">US dollars per day</Label>
          <Input
            id="agent-budget"
            type="number"
            min={0}
            step="0.01"
            inputMode="decimal"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            placeholder="e.g. 5.00"
            data-testid="team-budget-input"
          />
          <p className="text-xs text-muted-foreground">
            $0 stops them spending entirely. To let them spend freely, use “Remove cap”.
          </p>
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button
            onClick={() => valid && onSave(parsed)}
            disabled={!valid}
            data-testid="team-budget-save"
          >
            Save
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function Section({
  title,
  subtitle,
  action,
  children,
}: {
  title: string;
  subtitle: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <Card>
      <CardContent className="space-y-3">
        <div className="flex items-start justify-between gap-3">
          <div className="space-y-1">
            <h3 className="font-medium">{title}</h3>
            <p className="text-xs text-muted-foreground">{subtitle}</p>
          </div>
          {action}
        </div>
        {children}
      </CardContent>
    </Card>
  );
}

function EmptyState({ title, body }: { title: string; body: string }) {
  return (
    <Card className="[--card-spacing:--spacing(8)]">
      <CardContent className="space-y-1 text-center">
        <p className="font-medium">{title}</p>
        <p className="text-sm text-muted-foreground">{body}</p>
      </CardContent>
    </Card>
  );
}
