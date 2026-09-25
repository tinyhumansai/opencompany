// The Installed tab's list: the filter/sort bar above it and one card per
// skill.
//
// Split out of `SkillsView` because that file is the tab shell — it owns the
// host reads, the write handlers and the three dialogs — and this is the one
// surface an operator spends time in. All of the rules the list obeys (what a
// provenance label says, which rows a filter drops, where an unedited skill
// sorts) live in `@/lib/skills-list` as pure functions under the unit runner;
// what is left here is layout and the menu.
//
// Sized for the real cap rather than a demo: a company's installed set is the
// global baseline plus a full registry install plus whatever was authored, so
// the meta row wraps and the filter bar collapses to one control per line on a
// phone.

import { MoreHorizontal, Pencil, Power, Sparkles, Trash2 } from "lucide-react";

import type { Skill } from "@/api/skills";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { cn } from "@/lib/utils";
import { CATEGORY_STYLES, skillReachLabel, type SkillCategory } from "@/lib/skills";
import {
  canEditSkill,
  canUninstallSkill,
  SKILL_BUILTIN_UNINSTALL_REASON,
  SKILL_ENABLED_FILTERS,
  SKILL_SORT_LABELS,
  SKILL_SORTS,
  SKILL_SOURCE_FILTERS,
  skillCategories,
  skillLastEditedLabel,
  skillSourceLabel,
  visibleSkills,
  type SkillEnabledFilter,
  type SkillListFilters,
  type SkillSort,
  type SkillSourceFilter,
} from "@/lib/skills-list";

/**
 * Why Edit is offered but never enabled.
 *
 * No route serves a skill's `SKILL.md`: `GET …/skills` and the registry listing
 * are both metadata only. An Edit dialog could therefore load a name, a
 * description and a category — and would save that back over a playbook body it
 * never read. Greying it with the reason says so; hiding it would leave an
 * operator hunting for an action that is not there.
 */
const EDIT_UNAVAILABLE_REASON =
  "Editing needs the skill's full text, which the host does not serve yet.";

/** Category badge styling, tolerating the host's free-form category strings. */
function categoryStyle(category: string): string {
  return (
    CATEGORY_STYLES[category as SkillCategory] ??
    "border-muted-foreground/30 bg-muted text-muted-foreground"
  );
}

export function InstalledSkillsList({
  skills,
  filters,
  onFilters,
  sort,
  onSort,
  canManage,
  now,
  onToggle,
  onUninstall,
}: {
  skills: Skill[];
  filters: SkillListFilters;
  onFilters: (next: SkillListFilters) => void;
  sort: SkillSort;
  onSort: (next: SkillSort) => void;
  canManage: boolean;
  /** Taken once per render by the caller, so every row dates itself against the
   * same instant and the list cannot report two "now"s. */
  now: number;
  onToggle: (skill: Skill) => void;
  onUninstall: (skill: Skill) => void;
}) {
  const categories = skillCategories(skills);
  const rows = visibleSkills(skills, filters, sort) as Skill[];
  const enabledCount = skills.filter((s) => s.enabled).length;

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2">
        <Input
          value={filters.query}
          onChange={(e) => onFilters({ ...filters, query: e.target.value })}
          placeholder="Search installed skills…"
          aria-label="Search installed skills"
          data-testid="skills-filter-query"
          className="w-full sm:max-w-xs"
        />
        <FilterSelect
          id="skills-filter-source"
          label="Source"
          value={filters.source}
          options={SKILL_SOURCE_FILTERS.map((v) => [v, v === "all" ? "Any source" : labelFor(v)])}
          onChange={(v) => onFilters({ ...filters, source: v as SkillSourceFilter })}
        />
        <FilterSelect
          id="skills-filter-enabled"
          label="State"
          value={filters.enabled}
          options={SKILL_ENABLED_FILTERS.map((v) => [v, v === "all" ? "Any state" : labelFor(v)])}
          onChange={(v) => onFilters({ ...filters, enabled: v as SkillEnabledFilter })}
        />
        <FilterSelect
          id="skills-filter-category"
          label="Category"
          value={filters.category}
          options={[["all", "Any category"] as const, ...categories.map((c) => [c, c] as const)]}
          onChange={(v) => onFilters({ ...filters, category: v })}
        />
        <FilterSelect
          id="skills-sort"
          label="Sort by"
          value={sort}
          options={SKILL_SORTS.map((v) => [v, SKILL_SORT_LABELS[v]])}
          onChange={(v) => onSort(v as SkillSort)}
        />
      </div>

      <p className="text-xs text-muted-foreground" data-testid="skills-count">
        {rows.length === skills.length
          ? `${skills.length} installed · ${enabledCount} enabled`
          : `${rows.length} of ${skills.length} installed · ${enabledCount} enabled`}
      </p>

      {rows.length === 0 ? (
        <p className="rounded-xl border border-dashed p-6 text-center text-sm text-muted-foreground">
          No skills match those filters.
        </p>
      ) : (
        <div className="grid gap-3 sm:grid-cols-2">
          {rows.map((s) => (
            <InstalledCard
              key={s.id}
              skill={s}
              canManage={canManage}
              now={now}
              onToggle={() => onToggle(s)}
              onUninstall={() => onUninstall(s)}
            />
          ))}
        </div>
      )}
    </div>
  );
}

function labelFor(value: string): string {
  return value[0].toUpperCase() + value.slice(1);
}

function FilterSelect({
  id,
  label,
  value,
  options,
  onChange,
}: {
  id: string;
  label: string;
  value: string;
  options: readonly (readonly [string, string])[];
  onChange: (next: string) => void;
}) {
  return (
    <Select
      value={value}
      onValueChange={(v) => v && onChange(v)}
      items={Object.fromEntries(options)}
    >
      <SelectTrigger id={id} aria-label={label} data-testid={id} className="w-full sm:w-auto">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {options.map(([v, text]) => (
          <SelectItem key={v} value={v}>
            {text}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

function InstalledCard({
  skill,
  canManage,
  now,
  onToggle,
  onUninstall,
}: {
  skill: Skill;
  canManage: boolean;
  now: number;
  onToggle: () => void;
  onUninstall: () => void;
}) {
  return (
    <Card data-testid="installed-card" className={cn(!skill.enabled && "opacity-70")}>
      <CardContent className="space-y-2">
        <div className="flex items-start justify-between gap-2">
          <div className="flex min-w-0 items-center gap-2">
            <Sparkles className="size-4 shrink-0 text-muted-foreground" />
            <p className="truncate font-medium">{skill.name}</p>
          </div>
          <div className="flex shrink-0 items-center gap-1">
            <Switch
              checked={skill.enabled}
              disabled={!canManage}
              onCheckedChange={canManage ? onToggle : undefined}
              onClick={(e) => e.stopPropagation()}
              aria-label="Enable skill"
            />
            {canManage && (
              <SkillRowMenu skill={skill} onToggle={onToggle} onUninstall={onUninstall} />
            )}
          </div>
        </div>
        <p className="text-sm text-muted-foreground">{skill.description}</p>
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1 pt-1">
          {skill.category?.trim() ? (
            <Badge
              variant="outline"
              data-testid="skill-category"
              className={cn("capitalize", categoryStyle(skill.category))}
            >
              {skill.category}
            </Badge>
          ) : null}
          <span data-testid="skill-source" className="text-xs text-muted-foreground">
            {skillSourceLabel(skill)}
          </span>
          <span data-testid="skill-last-edited" className="text-xs text-muted-foreground">
            · {skillLastEditedLabel(skill.updatedAtMillis, now)}
          </span>
          {/* What the switch above decides, in the terms it actually decides
              them: reach, not capability (issue #569). */}
          <span data-testid="skill-reach" className="text-xs text-muted-foreground">
            · {skillReachLabel(skill.enabled)}
          </span>
        </div>
      </CardContent>
    </Card>
  );
}

/**
 * The row's ⋮ menu.
 *
 * Both refusals — Edit on anything the console did not author, Uninstall on a
 * bundled skill — render as a disabled item with the reason beneath it rather
 * than as a missing one. An action that silently is not there teaches nothing,
 * and the operator who goes looking for it has no way to find out why.
 */
function SkillRowMenu({
  skill,
  onToggle,
  onUninstall,
}: {
  skill: Skill;
  onToggle: () => void;
  onUninstall: () => void;
}) {
  const removable = canUninstallSkill(skill.source);
  const editable = canEditSkill(skill.source);

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        onClick={(e) => e.stopPropagation()}
        render={
          <Button
            variant="ghost"
            size="icon"
            className="size-7 text-muted-foreground"
            aria-label={`More actions for ${skill.name}`}
            data-testid="skill-row-menu"
          />
        }
      >
        <MoreHorizontal className="size-4" />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="max-w-64">
        <DropdownMenuItem disabled data-testid="skill-menu-edit">
          <Pencil className="mr-2 size-4" />
          Edit
        </DropdownMenuItem>
        {!editable ? (
          <MenuReason testId="skill-menu-edit-reason">
            Only a skill you wrote here can be edited.
          </MenuReason>
        ) : (
          <MenuReason testId="skill-menu-edit-reason">{EDIT_UNAVAILABLE_REASON}</MenuReason>
        )}
        <DropdownMenuItem onClick={onToggle} data-testid="skill-menu-toggle">
          <Power className="mr-2 size-4" />
          {skill.enabled ? "Disable" : "Enable"}
        </DropdownMenuItem>
        <DropdownMenuSeparator />
        <DropdownMenuItem
          variant={removable ? "destructive" : undefined}
          disabled={!removable}
          onClick={removable ? onUninstall : undefined}
          data-testid="skill-menu-uninstall"
        >
          <Trash2 className="mr-2 size-4" />
          Uninstall
        </DropdownMenuItem>
        {!removable && (
          <MenuReason testId="skill-menu-uninstall-reason">
            {SKILL_BUILTIN_UNINSTALL_REASON}
          </MenuReason>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function MenuReason({ testId, children }: { testId: string; children: React.ReactNode }) {
  return (
    <p className="px-2 pt-0.5 pb-1 text-xs text-muted-foreground" data-testid={testId}>
      {children}
    </p>
  );
}
