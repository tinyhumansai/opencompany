import { useEffect, useMemo, useState } from "react";
import { Check, Download, Plus, Search, Sparkles, Trash2 } from "lucide-react";
import { toast } from "sonner";

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
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { cn } from "@/lib/utils";
import {
  CATEGORY_STYLES,
  fromRegistry,
  type InstalledSkill,
  loadSkills,
  newSkill,
  type RegistrySkill,
  saveSkills,
  SKILL_REGISTRY,
  type SkillCategory,
} from "@/lib/skills";

interface Props {
  company: string | null;
}

const CATEGORIES: SkillCategory[] = ["Marketing", "Research", "Ops", "Content", "Finance"];

/** The company's skills: view, enable/disable, install from a registry, or add. */
export function SkillsView({ company }: Props) {
  const [skills, setSkills] = useState<InstalledSkill[]>(() => loadSkills(company));
  const [addOpen, setAddOpen] = useState(false);
  const [query, setQuery] = useState("");

  useEffect(() => {
    saveSkills(company, skills);
  }, [company, skills]);

  const installedIds = useMemo(() => new Set(skills.map((s) => s.id)), [skills]);
  const enabledCount = skills.filter((s) => s.enabled).length;

  function toggle(id: string) {
    setSkills((all) => all.map((s) => (s.id === id ? { ...s, enabled: !s.enabled } : s)));
  }
  function uninstall(id: string) {
    setSkills((all) => all.filter((s) => s.id !== id));
  }
  function install(skill: RegistrySkill) {
    if (installedIds.has(skill.id)) return;
    setSkills((all) => [...all, fromRegistry(skill)]);
    toast.success(`Installed ${skill.name}.`);
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

        <Tabs defaultValue="installed">
          <TabsList>
            <TabsTrigger value="installed">Installed ({skills.length})</TabsTrigger>
            <TabsTrigger value="registry">Registry</TabsTrigger>
          </TabsList>

          <TabsContent value="installed" className="mt-4">
            {skills.length === 0 ? (
              <Empty label="No skills installed yet." />
            ) : (
              <>
                <p className="mb-3 text-xs text-muted-foreground">{enabledCount} enabled</p>
                <div className="grid gap-3 sm:grid-cols-2">
                  {skills.map((s) => (
                    <InstalledCard
                      key={s.id}
                      skill={s}
                      onToggle={() => toggle(s.id)}
                      onUninstall={() => uninstall(s.id)}
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
                  onInstall={() => install(s)}
                />
              ))}
            </div>
          </TabsContent>
        </Tabs>
      </div>

      <AddSkillDialog
        open={addOpen}
        onOpenChange={setAddOpen}
        onAdd={(fields) => {
          setSkills((all) => [newSkill(fields), ...all]);
          setAddOpen(false);
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
  skill: InstalledSkill;
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
            <Badge variant="outline" className={cn("capitalize", CATEGORY_STYLES[skill.category])}>
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
            <Badge variant="outline" className={cn("capitalize", CATEGORY_STYLES[skill.category])}>
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
  onAdd: (fields: { name: string; description: string; category: SkillCategory }) => void;
}) {
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [category, setCategory] = useState<SkillCategory>("Marketing");

  function reset() {
    setName("");
    setDescription("");
    setCategory("Marketing");
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
          <Button variant="ghost" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button disabled={!name.trim()} onClick={() => onAdd({ name, description, category })}>
            Add skill
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
