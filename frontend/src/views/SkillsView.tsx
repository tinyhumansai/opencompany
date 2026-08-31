import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { BookOpen, Check, Download, Loader2, Plus, Search, Sparkles, Trash2 } from "lucide-react";
import { toast } from "sonner";

import {
  createSkill,
  installSkill,
  listRegistrySkills,
  listSkills,
  setSkillEnabled,
  uninstallSkill,
  type RegistrySkill,
  type Skill,
} from "@/api/skills";
import type { OpenCompanyClient } from "@/api/client";
import { PageHeader } from "@/components/page-header";
import { Alert, AlertDescription } from "@/components/ui/alert";
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import {
  CATEGORY_STYLES,
  registryEmptyLabel,
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
export function SkillsView({ client, company }: Props) {
  const [skills, setSkills] = useState<Skill[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  // The shared registry, live from the host. The console holds no catalog of
  // its own, so what an operator can browse is exactly what the host can serve.
  const [registry, setRegistry] = useState<RegistrySkill[]>([]);
  const [registryLoading, setRegistryLoading] = useState(true);
  const [registryError, setRegistryError] = useState<string | null>(null);
  const [addOpen, setAddOpen] = useState(false);
  const [query, setQuery] = useState("");
  // A generation token so a response from a previous company scope (or after
  // unmount) can't overwrite the current one.
  const gen = useRef(0);

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
        width="5xl"
        description={
          <>
            Playbooks your teammates read. Enable, install from the registry, or add your own.
          </>
        }
        actions={
          <>
          <Button onClick={() => setAddOpen(true)}>
            <Plus className="size-4" /> Add skill
          </Button>
          </>
        }
      />
      <div className="mx-auto min-h-0 w-full max-w-5xl flex-1 space-y-5 overflow-y-auto px-4 py-6">
        {/* Issue #569: what install / enable actually buy. A desk agent can list,
            describe and read a skill and can never run one — deliberate, and
            pinned by `dispatched_belt_excludes_every_deferred_family` — but this
            screen's vocabulary is the vocabulary of switching a capability on,
            so without saying it the operator learns the difference by asking a
            teammate to do something and watching nothing happen. */}
        <Alert data-testid="skills-read-only-note">
          <BookOpen className="size-4" />
          <AlertDescription>{SKILLS_READ_ONLY_NOTE}</AlertDescription>
        </Alert>

        {error && (
          <Alert variant="destructive">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}

        <Tabs defaultValue="installed">
          <TabsList>
            <TabsTrigger value="installed">Installed ({skills.length})</TabsTrigger>
            <TabsTrigger value="registry">Registry</TabsTrigger>
          </TabsList>

          <TabsContent value="installed" className="mt-4">
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
                      onToggle={() => void toggle(s)}
                      onUninstall={() => void uninstall(s)}
                    />
                  ))}
                </div>
              </>
            )}
          </TabsContent>

          <TabsContent value="registry" className="mt-4 space-y-3">
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
                    onInstall={() => void install(s)}
                  />
                ))}
              </div>
            )}
          </TabsContent>
        </Tabs>
      </div>

      <AddSkillDialog
        open={addOpen}
        onOpenChange={setAddOpen}
        onAdd={async (fields) => {
          const saved = await createSkill(client, company, {
            name: fields.name.trim(),
            description: fields.description.trim(),
            category: fields.category,
          });
          setSkills((all) => [saved, ...all.filter((s) => s.id !== saved.id)]);
          setAddOpen(false);
          toast.success(`Added ${saved.name}.`);
        }}
      />
    </div>
  );
}

function InstalledCard({
  skill,
  onToggle,
  onUninstall,
}: {
  skill: Skill;
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
          <Switch checked={skill.enabled} onCheckedChange={onToggle} aria-label="Enable skill" />
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
          {skill.source !== "company" && (
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
  onInstall,
}: {
  skill: RegistrySkill;
  installed: boolean;
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
            <Button variant="outline" size="sm" onClick={onInstall}>
              <Download className="size-4" /> Install
            </Button>
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
  onAdd: (fields: { name: string; description: string; category: SkillCategory }) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [category, setCategory] = useState<SkillCategory>("Marketing");
  const [busy, setBusy] = useState(false);

  function reset() {
    setName("");
    setDescription("");
    setCategory("Marketing");
  }

  async function submit() {
    // The host rejects a blank description, so gate on both here.
    if (!name.trim() || !description.trim()) return;
    setBusy(true);
    try {
      await onAdd({ name, description, category });
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
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Add a skill</DialogTitle>
          {/* Not "a capability your company should have" (issue #569): this is
              where an operator authors one, so it is the earliest point the
              console can frame a skill as the playbook a teammate reads rather
              than as something the company will carry out. */}
          <DialogDescription>
            Describe a playbook your teammates should follow — what to do, and when.
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
          <Label htmlFor="skill-desc">What it does</Label>
          <Textarea id="skill-desc" rows={3} value={description} onChange={(e) => setDescription(e.target.value)} placeholder="One line on when to use it and what it delivers." />
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={busy}>
            Cancel
          </Button>
          <Button disabled={!name.trim() || !description.trim() || busy} onClick={() => void submit()}>
            {busy && <Loader2 className="mr-1.5 size-4 animate-spin" />}
            Add skill
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
