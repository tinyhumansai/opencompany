// A skill as a page you can open: what it is, and which teammates read it.
//
// The transpose of the teammate page's picker. That one asks "which skills does
// this teammate read"; this asks "which teammates read this skill", which is the
// question an operator who thinks in skills actually has and could not answer
// without opening every teammate in turn.
//
// A page swap rather than a sheet, following `views/mcp/McpServerPage` — the
// console's other "one connected thing, in full" surface. A right-hand sheet
// gave the scope list 384px to render a face, a name, a reach verdict and a
// link in, which is narrower than the answer.
//
// ## One write path
//
// Scope is stored on the agent record. There is no skill-level scope field and
// this page adds none: every change here is `PATCH …/team/{agentId}` with that
// teammate's whole `skills` list, the same route the teammate page writes. A
// skill-side scope write would be a second store for one fact, which is what the
// owning design doc exists to prevent.
//
// That has a real cost: scoping one skill to N teammates is N requests, so
// partial failure is reachable and permanent. See `save` for how it is handled
// and why it is not hidden.

import { useEffect, useState } from "react";
import { ArrowLeft, Sparkles } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { Skill, SkillAgentScope } from "@/api/skills";
import type { TeamMemberDto } from "@/api/types";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Separator } from "@/components/ui/separator";
import { AgentAccessList } from "@/components/agent-access-list";
import { consoleHref } from "@/lib/console-paths";
import {
  SCOPE_CLEARS_TO_INHERITED_WARNING,
  SCOPE_ONLY_TAKES_AWAY,
  SCOPE_PINS_INHERITED_WARNING,
  agentsToUnpin,
  pinsAnInheritedScope,
  slugsGainedByInheriting,
  toggleSkillInScope,
} from "@/lib/skill-scope";
import { skillSourceLabel } from "@/lib/skills-list";
import { teammateName } from "@/lib/team";
import {
  partialSaveMessage,
  saveAgentAccess,
  type AgentAccessWrite,
} from "@/lib/agent-access-save";
import { SkillPlaybook } from "@/views/skills/SkillPlaybook";

/** Why the picker is read-only although the page opened. */
const MEMBER_READ_ONLY =
  "Scoping a skill changes what a teammate is told to do, so an admin makes that call. " +
  "You can see who reads it.";

/** What an absent `agents` means, said rather than rendered as an empty picker. */
const HOST_CANNOT_SAY =
  "This host does not report which teammates a skill is scoped to, so there is nothing to " +
  "show here. Each teammate's own page still carries its skill list.";

/** What a missing roster read means for the write, not just for the display. */
const NO_STORED_LISTS =
  "This host does not report each teammate's own skill list, so a change here could not be " +
  "made without overwriting the skills they already have. Use the teammate's own page.";

/** Said on a switched-off skill, where the scope is real and reaches nobody. */
const SKILL_DISABLED_NOTE =
  "This skill is switched off, so it reaches nobody right now. The scope below is still real — " +
  "it decides who gets the skill the moment it is switched back on.";

export function SkillPage({
  client,
  company,
  skill,
  team,
  canManage,
  onClose,
  onSaved,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /** The skill on screen, or `null` when nothing is open. */
  skill: Skill | null;
  /**
   * The roster read, or `null` while it is loading or after it failed.
   *
   * Load-bearing rather than decorative: each row carries that teammate's
   * **stored** skill list, and the next list is a function of it. Without the
   * roster the page can display a scope and must not offer to change one.
   */
  team: TeamMemberDto[] | null;
  /** Whether this viewer may change what the company's teammates read. */
  canManage: boolean;
  onClose: () => void;
  /** Refetch both reads — the skills list and the roster — after a write. */
  onSaved: () => void;
}) {
  // Only the agents the operator has moved. An override map rather than a full
  // draft, so a refetch that lands mid-session updates what is *stored* while a
  // change that has not been sent yet stays ticked where the operator put it.
  const [moved, setMoved] = useState<Record<string, boolean>>({});
  // Whether the operator asked for the per-teammate list. Every teammate ticked
  // reads as "All agents", and choosing "Selected agents" has to reveal the list
  // before anything is unticked or there is nothing to untick. Ticking one also
  // latches it, so tapping the last empty box cannot collapse the list underneath
  // an edit in progress.
  const [revealed, setRevealed] = useState(false);
  const [saving, setSaving] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);

  // The subject changes without this component unmounting, so a draft left over
  // from the previous skill would be saved onto this one.
  useEffect(() => {
    setMoved({});
    setRevealed(false);
    setProblem(null);
  }, [skill?.id]);

  const agents = skill?.agents;
  const ticked = (agent: SkillAgentScope) =>
    moved[agent.id] ?? agent.state !== "excluded";
  const changed = (agents ?? []).filter(
    (agent) => ticked(agent) !== (agent.state !== "excluded"),
  );
  const rosterById = new Map((team ?? []).map((member) => [member.id, member]));
  // A teammate whose stored list this host did not report cannot be written
  // safely, so it gates the save rather than being skipped silently.
  const missingStoredList =
    team === null ||
    (agents ?? []).some((agent) => !rosterById.get(agent.id)?.skills);
  const pinning = changed.filter((agent) => pinsAnInheritedScope(agent.state));
  const allTicked = (agents ?? []).length > 0 && (agents ?? []).every(ticked);
  const mode = allTicked && !revealed ? "all" : "selected";
  // "All agents" means inheriting, not a list that happens to name everybody:
  // a pinned teammate left holding today's set stops receiving whatever is
  // installed next. The work is therefore every teammate not already
  // inheriting, whether or not a checkbox moved.
  const unpinning = mode === "all" ? agentsToUnpin(agents) : [];
  const pending = mode === "all" ? unpinning : changed;
  const widening = unpinning
    .map((agent) => ({
      id: agent.id,
      gains: slugsGainedByInheriting(
        rosterById.get(agent.id)?.skills?.requested,
        rosterById.get(agent.id)?.skills?.companyAvailable ?? [],
      ),
    }))
    .filter((entry) => entry.gains.length > 0);

  /**
   * One `PATCH` per moved teammate, in order, stopping at the first failure.
   *
   * Sequential rather than parallel: every write serialises on the same
   * company-wide lock on the host anyway, so `Promise.all` buys nothing and
   * makes a partial failure non-deterministic — the operator could not be told
   * which teammates landed. Stopping at the first failure is the same reasoning
   * from the other end: a `403` fails identically for the rest, and a `5xx`
   * multiplies the damage.
   *
   * On a partial failure the page stays open with the unsent changes still
   * ticked, and both reads refetch so what is ticked beside them is what is
   * actually stored.
   */
  async function save() {
    if (!skill || missingStoredList) return;
    setSaving(true);
    setProblem(null);
    const writes: AgentAccessWrite[] = [];
    for (const agent of pending) {
      const scope = rosterById.get(agent.id)?.skills;
      if (!scope) break;
      // `null` hands the teammate back to inheriting; otherwise the whole list,
      // computed from what that teammate stores, never `[skill.id]` alone.
      const next =
        mode === "all"
          ? null
          : toggleSkillInScope(
              scope.requested,
              scope.companyAvailable,
              skill.id,
              ticked(agent),
            );
      writes.push({ agentId: agent.id, input: { skills: next } });
    }
    const outcome = await saveAgentAccess(client, company, writes, (agentId) =>
      setMoved((all) => {
        const rest = { ...all };
        delete rest[agentId];
        return rest;
      }),
    );
    setSaving(false);
    onSaved();
    const message = partialSaveMessage(outcome, (id) => teammateName(id, team), "Scoped");
    if (message === null) {
      onClose();
      return;
    }
    setProblem(message);
  }

  if (!skill) return null;

  return (
    <div className="space-y-4" data-testid="skill-page">
      <div className="flex flex-wrap items-center gap-3">
        <Button
          size="sm"
          variant="ghost"
          onClick={onClose}
          className="-ml-2 gap-1 text-muted-foreground"
          data-testid="skill-page-back"
        >
          <ArrowLeft className="size-4" />
          Skills
        </Button>
        <Sparkles className="size-5 shrink-0 text-muted-foreground" />
        <h2
          className="min-w-0 truncate text-base font-semibold"
          data-testid="skill-detail-name"
        >
          {skill.name}
        </h2>
        {skill.category?.trim() ? (
          <Badge variant="outline" className="capitalize">
            {skill.category}
          </Badge>
        ) : null}
        {/* The same label the row shows, from the same helper, so the list and
            the page it opens cannot name one skill's provenance two ways. */}
        <span
          className="text-xs text-muted-foreground"
          data-testid="skill-detail-source"
        >
          {skillSourceLabel(skill)}
        </span>
        <Badge
          variant="outline"
          data-testid="skill-detail-enabled"
          className={
            skill.enabled
              ? "bg-status-done-soft text-status-done-text"
              : "bg-muted text-muted-foreground"
          }
        >
          {skill.enabled ? "Enabled" : "Disabled"}
        </Badge>
      </div>

      <Separator />

      <div className="space-y-1">
        <p className="font-mono text-xs break-all text-muted-foreground">
          {skill.id}
        </p>
        <p className="text-sm text-muted-foreground">{skill.description}</p>
        {!skill.enabled && (
          <p
            className="text-xs text-muted-foreground"
            data-testid="skill-detail-disabled-note"
          >
            {SKILL_DISABLED_NOTE}
          </p>
        )}
      </div>

      <Separator />

      <SkillPlaybook
        client={client}
        company={company}
        slug={skill.id}
        canManage={canManage}
        onSaved={onSaved}
      />

      <Separator />

      <section className="space-y-3" aria-label="Available to">
        <h4 className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
          Available to
        </h4>

        {agents === undefined ? (
          <p
            className="text-xs text-muted-foreground"
            data-testid="skill-detail-no-scope"
          >
            {HOST_CANNOT_SAY}
          </p>
        ) : agents.length === 0 ? (
          <p
            className="text-xs text-muted-foreground"
            data-testid="skill-detail-empty-roster"
          >
            This company has no teammates yet, so there is nobody to scope this
            to.
          </p>
        ) : (
          <>
            {!canManage && (
              <p
                className="text-xs text-muted-foreground"
                data-testid="skill-detail-read-only"
              >
                {MEMBER_READ_ONLY}
              </p>
            )}
            {canManage && (
              <fieldset
                disabled={saving}
                className="flex flex-col gap-1.5 text-sm disabled:opacity-60"
                data-testid="skill-detail-mode"
              >
                <label className="flex items-center gap-2">
                  <input
                    type="radio"
                    name={`skill-scope-${skill.id}`}
                    checked={mode === "all"}
                    className="size-3.5 accent-primary"
                    data-testid="skill-detail-mode-all"
                    onChange={() => {
                      setMoved(
                        Object.fromEntries(
                          agents.map((agent) => [agent.id, true]),
                        ),
                      );
                      setRevealed(false);
                    }}
                  />
                  <span>All agents</span>
                </label>
                <label className="flex items-center gap-2">
                  <input
                    type="radio"
                    name={`skill-scope-${skill.id}`}
                    checked={mode === "selected"}
                    className="size-3.5 accent-primary"
                    data-testid="skill-detail-mode-selected"
                    onChange={() => setRevealed(true)}
                  />
                  <span>Selected agents</span>
                </label>
              </fieldset>
            )}

            {(mode === "selected" || !canManage) && (
              <AgentAccessList
                rows={agents.map((agent) => ({
                  id: agent.id,
                  ticked: ticked(agent),
                  holds: agent.holds,
                  detail: (
                    <a
                      href={consoleHref("team", agent.id)}
                      className="text-2xs text-muted-foreground transition-opacity hover:opacity-80"
                      data-testid={`skill-agent-link-${agent.id}`}
                    >
                      {stateLabel(agent)}
                    </a>
                  ),
                }))}
                team={team}
                editable={canManage}
                disabled={saving}
                listTestId="skill-detail-agents"
                rowTestIdPrefix="skill"
                onToggle={(id, on) => {
                  setRevealed(true);
                  setMoved((all) => ({ ...all, [id]: on }));
                }}
              />
            )}

            <p className="text-xs text-muted-foreground">
              {SCOPE_ONLY_TAKES_AWAY}
            </p>

            {pinning.length > 0 && (
              <p
                className="text-xs text-status-blocked-text"
                data-testid="skill-detail-pin-warning"
              >
                {SCOPE_PINS_INHERITED_WARNING}
              </p>
            )}

            {widening.length > 0 && (
              <div
                className="space-y-1 text-xs text-status-blocked-text"
                data-testid="skill-detail-widening-warning"
              >
                <p>{SCOPE_CLEARS_TO_INHERITED_WARNING}</p>
                <ul className="list-disc pl-4">
                  {widening.map((entry) => (
                    <li key={entry.id}>
                      {teammateName(entry.id, team)} gains{" "}
                      {entry.gains.length}{" "}
                      {entry.gains.length === 1 ? "skill" : "skills"}:{" "}
                      {entry.gains.join(", ")}
                    </li>
                  ))}
                </ul>
              </div>
            )}

            {missingStoredList && canManage && (
              <Alert
                variant="destructive"
                data-testid="skill-detail-no-stored-lists"
              >
                <AlertDescription>{NO_STORED_LISTS}</AlertDescription>
              </Alert>
            )}

            {problem && (
              <Alert variant="destructive" data-testid="skill-detail-problem">
                <AlertDescription>{problem}</AlertDescription>
              </Alert>
            )}

            {canManage && (
              <div className="flex justify-end gap-2">
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={onClose}
                  disabled={saving}
                >
                  Cancel
                </Button>
                <Button
                  size="sm"
                  disabled={saving || pending.length === 0 || missingStoredList}
                  onClick={() => void save()}
                  data-testid="skill-detail-save"
                >
                  Save
                </Button>
              </div>
            )}
          </>
        )}
      </section>
    </div>
  );
}

/**
 * What one agent's row says about how it got here.
 *
 * Three states, not two: a teammate that inherits and one that names this skill
 * are both ticked, and only the second stays ticked when the company enables
 * something else. Rendered as the link to that teammate's own page, which is
 * where a single teammate's scope is edited slug by slug. Handing one back to
 * inheriting is what this page's "All agents" mode does — to every teammate at
 * once, with the widening it causes named first.
 */
function stateLabel(agent: SkillAgentScope): string {
  if (agent.state === "inherited") return "inherits every skill";
  if (agent.state === "included") return "listed on its own page";
  return "not on its list";
}
