import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Check, Download, Loader2, Plus, Search, Sparkles, Trash2 } from "lucide-react";
import { toast } from "sonner";

import {
  createSkill,
  installSkill,
  listSkills,
  setSkillEnabled,
  uninstallSkill,
  type Skill,
} from "@/api/skills";
import type { OpenCompanyClient } from "@/api/client";
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
  type RegistrySkill,
  SKILL_REGISTRY,
  type SkillCategory,
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
  const [addOpen, setAddOpen] = useState(false);
  const [query, setQuery] = useState("");
  const mounted = useRef(true);

  const refresh = useCallback(async () => {
    try {
      const rows = await listSkills(client, company);
      if (!mounted.current) return;
      setSkills(rows);
      setError(null);
    } catch (e) {
      if (!mounted.current) return;
      setError(e instanceof Error ? e.message : "could not load skills");
    } finally {
      if (mounted.current) setLoading(false);
    }
  }, [client, company]);

  useEffect(() => {
    mounted.current = true;
    setLoading(true);
    void refresh();
    return () => {
      mounted.current = false;
    };
  }, [refresh]);

  const installedIds = useMemo(() => new Set(skills.map((s) => s.id)), [skills]);
  const enabledCount = skills.filter((s) => s.enabled).length;

  async function toggle(skill: Skill) {
    const previous = skills;
    const next = !skill.enabled;
    setSkills((all) => all.map((s) => (s.id === skill.id ? { ...s, enabled: next } : s)));
    try {
      const saved = await setSkillEnabled(client, company, skill.id, next);
      setSkills((all) => all.map((s) => (s.id === saved.id ? saved : s)));
    } catch (e) {
      setSkills(previous);
      toast.error(e instanceof Error ? e.message : "could not update the skill");
    }
  }

  async function uninstall(skill: Skill) {
    const previous = skills;
    setSkills((all) => all.filter((s) => s.id !== skill.id));
    try {
      await uninstallSkill(client, company, skill.id);
    } catch (e) {
      setSkills(previous);
      toast.error(e instanceof Error ? e.message : "could not uninstall the skill");
    }
  }

  async function install(skill: RegistrySkill) {
    if (installedIds.has(skill.id)) return;
    try {
      const saved = await installSkill(client, company, skill.id);
      setSkills((all) => [...all.filter((s) => s.id !== saved.id), saved]);
      toast.success(`Installed ${skill.name}.`);
    } catch (e) {
      toast.error(e instanceof Error ? e.message : "could not install the skill");
    }
  }

  const registry = useMemo(() => {
    const q = query.trim().toLowerCase();
    return SKILL_REGISTRY.filter(
      (s) => !q || s.name.toLowerCase().includes(q) || s.description.toLowerCase().includes(q),
    );
  }, [query]);

  return (
    <div className="flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-5xl space-y-5 px-4 py-6">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="space-y-1">
            <h2 className="text-2xl font-semibold tracking-tight">Skills</h2>
            <p className="text-sm text-muted-foreground">
              Capabilities your company can use. Enable, install from the registry, or add your own.
            </p>
          </div>
          <Button onClick={() => setAddOpen(true)}>
            <Plus className="size-4" /> Add skill
          </Button>
        </div>

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
            <div className="grid gap-3 sm:grid-cols-2">
              {registry.map((s) => (
                <RegistryCard
                  key={s.id}
                  skill={s}
                  installed={installedIds.has(s.id)}
                  onInstall={() => void install(s)}
                />
              ))}
            </div>
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
    <Card className={cn(!skill.enabled && "opacity-70")}>
      <CardContent className="space-y-2 py-4">
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
    <Card>
      <CardContent className="space-y-2 py-4">
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
            <span className="text-xs text-muted-foreground">{skill.publisher}</span>
          </div>
          {installed ? (
            <span className="inline-flex items-center gap-1 text-xs font-medium text-emerald-600 dark:text-emerald-400">
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
    if (!name.trim()) return;
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
          <DialogDescription>Describe a capability your company should have.</DialogDescription>
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
          <Button disabled={!name.trim() || busy} onClick={() => void submit()}>
            {busy && <Loader2 className="mr-1.5 size-4 animate-spin" />}
            Add skill
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
