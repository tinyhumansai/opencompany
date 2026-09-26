import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  BookOpen,
  Check,
  Download,
  Info,
  Loader2,
  Plus,
  Search,
  Sparkles,
  Trash2,
  Upload,
} from "lucide-react";
import { toast } from "sonner";

import { hasNoSession, me as fetchMe } from "@/api/auth";
import {
  createSkill,
  installSkill,
  listRegistrySkills,
  listSkills,
  setSkillEnabled,
  uninstallSkill,
  type RegistrySkill,
  type Skill,
  type SkillUploadRow,
} from "@/api/skills";
import { getInferenceStatus } from "@/api/inference";
import { DraftSkillDialog } from "@/views/skills/DraftSkillDialog";
import { UploadSkillDialog } from "@/views/skills/UploadSkillDialog";
import type { OpenCompanyClient } from "@/api/client";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
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
import { PageTabPanel, PageTabs, type PageTab } from "@/components/page-tabs";
import { useHashTab } from "@/hooks/use-hash-tab";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import {
  CATEGORY_STYLES,
  registryEmptyLabel,
  SKILL_DESCRIPTION_HINT,
  SKILL_DESCRIPTION_MAX_CHARS,
  SKILL_DESCRIPTION_PLACEHOLDER,
  skillDescriptionCount,
  SKILLS_READ_ONLY_NOTE,
  type SkillCategory,
  skillReachLabel,
} from "@/lib/skills";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

const CATEGORIES: SkillCategory[] = ["Marketing", "Research", "Ops", "Content", "Finance"];

/** Category badge styling, tolerating the host's free-form category strings. */
function categoryStyle(category: string): string {
  return (
    CATEGORY_STYLES[category as SkillCategory] ??
    "border-muted-foreground/30 bg-muted text-muted-foreground"
  );
}

/**
 * The company's skills: the real effective set read from the host (`…/skills`),
 * which the operator can enable/disable, install from a registry, uninstall, or
 * extend with a custom skill. Every mutation writes through the API and updates
 * optimistically, reverting on error.
 */
/** What you already have, and what you could add. `id` is what `?tab=` carries. */
const SKILL_TABS = [
  { id: "installed", label: "Installed" },
  { id: "registry", label: "Registry" },
] as const satisfies readonly PageTab<string>[];

type SkillTab = (typeof SKILL_TABS)[number]["id"];

export function SkillsView({ client, company }: Props) {
  const [tab, setTab] = useHashTab<SkillTab>(
    SKILL_TABS.map((t) => t.id),
    "installed",
  );
  const [skills, setSkills] = useState<Skill[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  // The shared registry, live from the host. The console holds no catalog of
  // its own, so what an operator can browse is exactly what the host can serve.
  const [registry, setRegistry] = useState<RegistrySkill[]>([]);
  const [registryLoading, setRegistryLoading] = useState(true);
  const [registryError, setRegistryError] = useState<string | null>(null);
  const [addOpen, setAddOpen] = useState(false);
  const [uploadOpen, setUploadOpen] = useState(false);
  const [draftOpen, setDraftOpen] = useState(false);
  // Whether this host can draft at all. `undefined` is "it did not say" — an
  // older host omits the field — and is read as unknown rather than as `false`,
  // exactly as the Add-teammate dialog reads it. Only an explicit `false` hides
  // the control, because that is the one answer that means the route could only
  // ever return `no_model`.
  const [canDraft, setCanDraft] = useState<boolean | undefined>(undefined);
  const [query, setQuery] = useState("");
  // A generation token so a response from a previous company scope (or after
  // unmount) can't overwrite the current one.
  const gen = useRef(0);
  // Whether this viewer may enable, install, uninstall or add a skill. The four
  // writes are admin-only on the host; an unresolved role must not render an
  // enabled control, so this defaults closed the way `HostingView` does.
  const [canManage, setCanManage] = useState(false);
  const [authorityScope, setAuthorityScope] = useState({ client, company });
  // Bumped on a scope change; keys the authoring dialogs so they remount.
  const [scopeGen, setScopeGen] = useState(0);

  // Closed *during* the render that first sees a new scope, not in the effect
  // that follows it. An effect runs after commit, so the frame carrying the new
  // scope would already have painted the previous scope's `canManage` — one
  // real frame of live write controls aimed at a scope this operator may not
  // administer. Keyed by both `client` and `company`: a host reseat changes
  // `client` identity while `company` can stay the same. Resetting here is
  // React's documented adjust-state-during-render pattern: it re-renders
  // before anything reaches the screen.
  if (authorityScope.client !== client || authorityScope.company !== company) {
    setAuthorityScope({ client, company });
    setCanManage(false);
    setAddOpen(false);
    setUploadOpen(false);
    setDraftOpen(false);
    setCanDraft(undefined);
    setScopeGen((g) => g + 1);
  }

  useEffect(() => {
    let live = true;
    void (async () => {
      let admin = false;
      try {
        admin = (await fetchMe(client, company)).role === "admin";
      } catch (err) {
        // `resolve_principal` on the host tries a human session first and
        // only falls back to the platform/tenant bearer when none is
        // present — a hub console can carry both, and the write routes
        // below are AdminScopedCompany, which admits that machine principal
        // unconditionally once it has addressed this company. So a
        // *confirmed* absence of any session means the bearer is what the
        // host will actually authorize on. A network error, a timeout, or a
        // `5xx` is not that confirmation — a member's session could still be
        // live and still take precedence on the host — so those stay
        // non-admin rather than assuming the bearer wins (codeRabbit review).
        admin = client.carriesPlatformBearer && hasNoSession(err);
      }
      if (live) setCanManage(admin);
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  // Whether to offer drafting at all. A host with no drafter can only answer
  // `no_model`, and a control that answers nothing else is worse than no
  // control. A failed read leaves it unknown, which keeps the button — the
  // route's own refusal is then what says so, rather than a network blip
  // removing a working feature.
  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const status = await getInferenceStatus(client, company);
        if (live) setCanDraft(status.designsProfiles);
      } catch {
        if (live) setCanDraft(undefined);
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company]);

  // Rows an upload (or a saved draft) stored, folded into the list without a
  // re-read: the host returns the stored skill, so refetching would only be a
  // second chance to disagree with what it just said.
  const takeUploaded = useCallback((rows: SkillUploadRow[]) => {
    const stored = rows.flatMap((row) => (row.skill ? [row.skill as Skill] : []));
    if (stored.length === 0) return;
    setSkills((all) => [
      ...stored,
      ...all.filter((skill) => !stored.some((one) => one.id === skill.id)),
    ]);
    toast.success(stored.length === 1 ? `Added ${stored[0].name}.` : `Added ${stored.length} skills.`);
  }, []);

  const refresh = useCallback(async () => {
    const mine = ++gen.current;
    // Independent requests: a failing registry must not blank the installed
    // list (or the reverse), so each settles on its own.
    const [installed, shared] = await Promise.allSettled([
      listSkills(client, company),
      listRegistrySkills(client, company),
    ]);
    if (mine !== gen.current) return;

    if (installed.status === "fulfilled") {
      setSkills(installed.value);
      setError(null);
    } else {
      const e = installed.reason;
      setError(e instanceof Error ? e.message : "could not load skills");
    }
    setLoading(false);

    if (shared.status === "fulfilled") {
      setRegistry(shared.value);
      setRegistryError(null);
    } else {
      const e = shared.reason;
      setRegistryError(e instanceof Error ? e.message : "could not load the registry");
    }
    setRegistryLoading(false);
  }, [client, company]);

  useEffect(() => {
    setLoading(true);
    setRegistryLoading(true);
    setSkills([]); // drop the previous scope's skills while the new set loads
    setRegistry([]);
    void refresh();
    // Invalidate any in-flight request on scope change / unmount.
    return () => {
      gen.current++;
    };
  }, [refresh]);

  const installedIds = useMemo(() => new Set(skills.map((s) => s.id)), [skills]);
  const enabledCount = skills.filter((s) => s.enabled).length;

  async function toggle(skill: Skill) {
    const next = !skill.enabled;
    setSkills((all) => all.map((s) => (s.id === skill.id ? { ...s, enabled: next } : s)));
    try {
      const saved = await setSkillEnabled(client, company, skill.id, next);
      setSkills((all) => all.map((s) => (s.id === saved.id ? saved : s)));
    } catch (e) {
      // Revert only this skill, so a concurrent mutation isn't clobbered.
      setSkills((all) =>
        all.map((s) => (s.id === skill.id ? { ...s, enabled: skill.enabled } : s)),
      );
      toast.error(e instanceof Error ? e.message : "could not update the skill");
    }
  }

  async function uninstall(skill: Skill) {
    setSkills((all) => all.filter((s) => s.id !== skill.id));
    try {
      await uninstallSkill(client, company, skill.id);
    } catch (e) {
      // Re-insert only this skill on failure (no whole-list rollback).
      setSkills((all) => (all.some((s) => s.id === skill.id) ? all : [...all, skill]));
      toast.error(e instanceof Error ? e.message : "could not uninstall the skill");
    }
  }

  async function install(skill: RegistrySkill) {
    if (installedIds.has(skill.id)) return;
    try {
      const saved = await installSkill(client, company, skill.id, {
        name: skill.name,
        description: skill.description,
        category: skill.category,
      });
      setSkills((all) => [...all.filter((s) => s.id !== saved.id), saved]);
      toast.success(`Installed ${skill.name}.`);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not install the skill");
    }
  }

  const visibleRegistry = useMemo(() => {
    const q = query.trim().toLowerCase();
    return registry.filter(
      (s) => !q || s.name.toLowerCase().includes(q) || s.description.toLowerCase().includes(q),
    );
  }, [query, registry]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <PageHeader
        title="Skills"
        width="full"
        description={
          <>
            Playbooks your agents read. Enable, install from the registry, or add your own.
          </>
        }
        actions={
          canManage ? (
            <div className="flex flex-wrap items-center gap-2">
              <Button variant="outline" onClick={() => setUploadOpen(true)}>
                <Upload className="size-4" /> Upload
              </Button>
              {canDraft !== false && (
                <Button
                  variant="outline"
                  data-testid="skills-draft-trigger"
                  onClick={() => setDraftOpen(true)}
                >
                  <Sparkles className="size-4" /> Draft with a teammate
                </Button>
              )}
              <Button onClick={() => setAddOpen(true)}>
                <Plus className="size-4" /> Add skill
              </Button>
            </div>
          ) : undefined
        }
        tabs={
          <PageTabs
            tabs={SKILL_TABS.map((t) =>
              // The count rides the tab it describes rather than the label, so
              // "Installed (0)" cannot read as a tab named for a number.
              t.id === "installed" ? { ...t, count: skills.length } : t,
            )}
            value={tab}
            onChange={setTab}
            idBase="skills"
            aria-label="Skill views"
          />
        }
      />
      <div className="min-h-0 w-full flex-1 space-y-5 overflow-y-auto px-4 py-6">
        {!canManage && (
          <Alert data-testid="skills-admin-only">
            <Info className="size-4" />
            <AlertTitle>Only an admin can change this company&apos;s skills</AlertTitle>
            <AlertDescription>
              Enabling, installing, uninstalling and adding a skill change what every agent is
              told to do, so an admin makes those calls. You can see what is installed and browse
              the registry.
            </AlertDescription>
          </Alert>
        )}

        {/* What install / enable actually buy. A desk agent can list, describe
            and read a skill and can never run one — deliberate, and pinned by
            `dispatched_belt_excludes_every_deferred_family` — but this screen's
            vocabulary is the vocabulary of switching a capability on, so
            without saying it the operator learns the difference by asking an
            agent to do something and watching nothing happen. */}
        <Alert data-testid="skills-read-only-note">
          <BookOpen className="size-4" />
          <AlertDescription>{SKILLS_READ_ONLY_NOTE}</AlertDescription>
        </Alert>

        {error && (
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        <PageTabPanel idBase="skills" id="installed" value={tab}>
            {loading ? (
              <div className="grid gap-3 sm:grid-cols-2">
                <Skeleton className="h-32 rounded-xl" />
                <Skeleton className="h-32 rounded-xl" />
              </div>
            ) : skills.length === 0 ? (
              <Empty label="No skills installed yet." />
            ) : (
              <>
                <p className="mb-3 text-xs text-muted-foreground">{enabledCount} enabled</p>
                <div className="grid gap-3 sm:grid-cols-2">
                  {skills.map((s) => (
                    <InstalledCard
                      key={s.id}
                      skill={s}
                      canManage={canManage}
                      onToggle={() => void toggle(s)}
                      onUninstall={() => void uninstall(s)}
                    />
                  ))}
                </div>
              </>
            )}
        </PageTabPanel>

        <PageTabPanel idBase="skills" id="registry" value={tab} className="space-y-3">
            <div className="relative sm:max-w-xs">
              <Search className="absolute top-1/2 left-2.5 size-4 -translate-y-1/2 text-muted-foreground" />
              <Input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Search the registry…" className="pl-8" />
            </div>
            {registryError && (
              <Alert variant="destructive">
                <AlertDescription>{registryError}</AlertDescription>
              </Alert>
            )}
            {registryLoading ? (
              <div className="grid gap-3 sm:grid-cols-2">
                <Skeleton className="h-32 rounded-xl" />
                <Skeleton className="h-32 rounded-xl" />
              </div>
            ) : visibleRegistry.length === 0 ? (
              // A failed read leaves `registry` empty too, so the label must not
              // derive "serves no registry" from the same failure the alert above
              // already reports (issue #1467). The decider keeps the three cases
              // apart.
              <Empty label={registryEmptyLabel(registryError !== null, registry.length === 0)} />
            ) : (
              <div className="grid gap-3 sm:grid-cols-2">
                {visibleRegistry.map((s) => (
                  <RegistryCard
                    key={s.id}
                    skill={s}
                    installed={installedIds.has(s.id)}
                    canManage={canManage}
                    onInstall={() => void install(s)}
                  />
                ))}
              </div>
            )}
        </PageTabPanel>
      </div>

      <AddSkillDialog
        open={addOpen}
        onOpenChange={setAddOpen}
        onAdd={async (fields) => {
          const playbook = fields.body.trim();
          const saved = await createSkill(client, company, {
            name: fields.name.trim(),
            description: fields.description.trim(),
            category: fields.category,
            ...(playbook ? { body: playbook } : {}),
          });
          setSkills((all) => [saved, ...all.filter((s) => s.id !== saved.id)]);
          setAddOpen(false);
          toast.success(`Added ${saved.name}.`);
        }}
      />
      <UploadSkillDialog
        key={`upload-${scopeGen}`}
        client={client}
        company={company}
        open={uploadOpen}
        onOpenChange={setUploadOpen}
        onUploaded={takeUploaded}
      />
      <DraftSkillDialog
        key={`draft-${scopeGen}`}
        client={client}
        company={company}
        open={draftOpen}
        onOpenChange={setDraftOpen}
        onSaved={takeUploaded}
      />
    </div>
  );
}

function InstalledCard({
  skill,
  canManage,
  onToggle,
  onUninstall,
}: {
  skill: Skill;
  canManage: boolean;
  onToggle: () => void;
  onUninstall: () => void;
}) {
  return (
    <Card data-testid="installed-card" className={cn(!skill.enabled && "opacity-70")}>
      <CardContent className="space-y-2">
        <div className="flex items-start justify-between gap-2">
          <div className="flex items-center gap-2">
            <Sparkles className="size-4 text-muted-foreground" />
            <p className="font-medium">{skill.name}</p>
          </div>
          <Switch
            checked={skill.enabled}
            disabled={!canManage}
            onCheckedChange={canManage ? onToggle : undefined}
            aria-label="Enable skill"
          />
        </div>
        <p className="text-sm text-muted-foreground">{skill.description}</p>
        <div className="flex items-center justify-between pt-1">
          <div className="flex items-center gap-2">
            <Badge variant="outline" className={cn("capitalize", categoryStyle(skill.category))}>
              {skill.category}
            </Badge>
            <span className="text-xs text-muted-foreground capitalize">{skill.source}</span>
            {/* What the switch above decides, in the terms it actually decides
                them: reach, not capability (issue #569). */}
            <span data-testid="skill-reach" className="text-xs text-muted-foreground">
              · {skillReachLabel(skill.enabled)}
            </span>
          </div>
          {canManage && skill.source !== "company" && (
            <Button
              variant="ghost"
              size="icon"
              className="size-7 text-muted-foreground hover:text-destructive"
              onClick={onUninstall}
              aria-label="Uninstall"
            >
              <Trash2 className="size-4" />
            </Button>
          )}
        </div>
      </CardContent>
    </Card>
  );
}

function RegistryCard({
  skill,
  installed,
  canManage,
  onInstall,
}: {
  skill: RegistrySkill;
  installed: boolean;
  canManage: boolean;
  onInstall: () => void;
}) {
  return (
    <Card data-testid="registry-card">
      <CardContent className="space-y-2">
        <div className="flex items-center gap-2">
          <Sparkles className="size-4 text-muted-foreground" />
          <p className="font-medium">{skill.name}</p>
        </div>
        <p className="text-sm text-muted-foreground">{skill.description}</p>
        <div className="flex items-center justify-between pt-1">
          <div className="flex items-center gap-2">
            <Badge variant="outline" className={cn("capitalize", categoryStyle(skill.category))}>
              {skill.category}
            </Badge>
            <span className="text-xs text-muted-foreground">
              {skill.publisher}
              {skill.version ? ` · v${skill.version}` : ""}
            </span>
          </div>
          {installed ? (
            <span className="inline-flex items-center gap-1 text-xs font-medium text-status-done-text">
              <Check className="size-3.5" /> Installed
            </span>
          ) : (
            canManage && (
              <Button variant="outline" size="sm" onClick={onInstall}>
                <Download className="size-4" /> Install
              </Button>
            )
          )}
        </div>
      </CardContent>
    </Card>
  );
}

function Empty({ label }: { label: string }) {
  return (
    <div className="mt-12 flex flex-col items-center gap-2 text-center text-muted-foreground">
      <Sparkles className="size-8" />
      <p className="text-sm">{label}</p>
    </div>
  );
}

function AddSkillDialog({
  open,
  onOpenChange,
  onAdd,
}: {
  open: boolean;
  onOpenChange: (o: boolean) => void;
  onAdd: (fields: {
    name: string;
    description: string;
    category: SkillCategory;
    body: string;
  }) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [category, setCategory] = useState<SkillCategory>("Marketing");
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);
  const described = skillDescriptionCount(description);
  const tooLong = described > SKILL_DESCRIPTION_MAX_CHARS;

  function reset() {
    setName("");
    setDescription("");
    setCategory("Marketing");
    setBody("");
  }

  async function submit() {
    // The host rejects a blank description and one past the limit, so gate on
    // all three here rather than spending a round trip to be told.
    if (!name.trim() || !description.trim()) return;
    if (skillDescriptionCount(description) > SKILL_DESCRIPTION_MAX_CHARS) return;
    setBusy(true);
    try {
      await onAdd({ name, description, category, body });
      // The caller closes the dialog by flipping `open`, which never reaches
      // `onOpenChange`, so clearing on dismiss alone leaves the last skill's
      // fields sitting in the next one.
      reset();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not add the skill");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(o) => {
        onOpenChange(o);
        if (!o) reset();
      }}
    >
      <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Add a skill</DialogTitle>
          <DialogDescription>
            Describe a playbook your agents should follow — what to do, and when.
          </DialogDescription>
        </DialogHeader>
        <div className="grid gap-2">
          <Label htmlFor="skill-name">Name</Label>
          <Input id="skill-name" value={name} onChange={(e) => setName(e.target.value)} placeholder="e.g. Press Outreach" />
        </div>
        <div className="grid gap-2">
          <Label htmlFor="skill-cat">Category</Label>
          <Select
            value={category}
            onValueChange={(v) => v && setCategory(v as SkillCategory)}
            items={Object.fromEntries(CATEGORIES.map((c) => [c, c]))}
          >
            <SelectTrigger id="skill-cat" className="w-full">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {CATEGORIES.map((c) => (
                <SelectItem key={c} value={c}>
                  {c}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
        <div className="grid gap-2">
          <div className="flex items-baseline justify-between gap-2">
            <Label htmlFor="skill-desc">What it does, and when to use it</Label>
            {/* Live, and against the host's own limit rather than a second copy
                of the number: a counter the host disagrees with either stops the
                operator short of a description that would have been accepted, or
                reads green while the save is refused. */}
            <span
              data-testid="skill-desc-count"
              className={cn(
                "text-xs tabular-nums",
                described > SKILL_DESCRIPTION_MAX_CHARS
                  ? "text-destructive"
                  : "text-muted-foreground",
              )}
            >
              {described} / {SKILL_DESCRIPTION_MAX_CHARS}
            </span>
          </div>
          {/* One line, and an `Input` so it can only be one: the host collapses
              newlines out of this field, and it is what an agent reads when
              deciding whether to open the skill at all. */}
          <Input
            id="skill-desc"
            value={description}
            onChange={(e) => setDescription(e.target.value)}
            placeholder={SKILL_DESCRIPTION_PLACEHOLDER}
            aria-describedby="skill-desc-hint"
          />
          <p id="skill-desc-hint" data-testid="skill-desc-hint" className="text-xs text-muted-foreground">
            {SKILL_DESCRIPTION_HINT}
          </p>
        </div>
        <div className="grid gap-2">
          <Label htmlFor="skill-body">Playbook</Label>
          <Textarea
            id="skill-body"
            rows={8}
            value={body}
            onChange={(e) => setBody(e.target.value)}
            placeholder={
              "The steps to follow, in the order to follow them. Markdown, as long as it needs to be.\n\nLeave it empty and an agent gets the line above and nothing else."
            }
          />
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={busy}>
            Cancel
          </Button>
          <Button
            disabled={!name.trim() || !description.trim() || tooLong || busy}
            onClick={() => void submit()}
          >
            {busy && <Loader2 className="mr-1.5 size-4 animate-spin" />}
            Add skill
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
