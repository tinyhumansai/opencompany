import { useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  Check,
  ChevronRight,
  CircleDashed,
  Copy,
  Loader2,
  Play,
  X,
} from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

/**
 * The living styleguide — `#/styleguide`.
 *
 * Every token and every component state on one page, rendered by the same CSS
 * the console ships. That is the whole point: a styleguide written as a
 * separate document drifts the moment someone edits `index.css`, whereas this
 * one cannot — it reads the variables at runtime, so a token that changes
 * changes here too, and a token that is deleted renders visibly empty.
 *
 * Not in the sidebar. It is a maintainer's tool, reachable by URL, and it
 * carries no company data — so it needs neither a client nor a company.
 */
export function StyleguideView() {
  return (
    <div className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto w-full max-w-5xl px-6 py-10">
        <Header />
        <div className="mt-10 space-y-14 pb-24">
          <ColorSection />
          <StatusSection />
          <ToneSection />
          <TypeSection />
          <ElevationSection />
          <RadiusSection />
          <MotionSection />
          <ComponentSection />
        </div>
      </div>
    </div>
  );
}

function Header() {
  return (
    <header>
      <p className="text-2xs font-medium tracking-wide text-sidebar-accent-foreground uppercase">
        OpenCompany design system
      </p>
      <h1 className="mt-2 text-2xl font-semibold tracking-tight">Styleguide</h1>
      <p className="mt-2 max-w-2xl text-sm text-muted-foreground">
        Every token and component state the console ships, rendered by the
        console's own stylesheet. Switch the theme to check both. Written
        reference lives in{" "}
        <code className="rounded-sm bg-muted px-1 py-0.5 font-mono text-2xs">
          docs/design-system/
        </code>
        .
      </p>
    </header>
  );
}

/* ---------------------------------------------------------------- sections */

function Section({
  title,
  hint,
  children,
}: {
  title: string;
  hint: string;
  children: React.ReactNode;
}) {
  return (
    <section className="space-y-5">
      <div>
        <h2 className="text-base font-semibold tracking-tight">{title}</h2>
        <p className="mt-1 text-sm text-muted-foreground">{hint}</p>
      </div>
      {children}
    </section>
  );
}

/** A resolved token, so the page shows the value the browser actually computed. */
function useResolved(vars: string[]) {
  const [values, setValues] = useState<Record<string, string>>({});
  useEffect(() => {
    const style = getComputedStyle(document.documentElement);
    const next: Record<string, string> = {};
    for (const v of vars) next[v] = style.getPropertyValue(v).trim();
    setValues(next);
    // Re-read when the theme class flips, so dark values are shown in dark.
    const observer = new MutationObserver(() => {
      const s = getComputedStyle(document.documentElement);
      const n: Record<string, string> = {};
      for (const v of vars) n[v] = s.getPropertyValue(v).trim();
      setValues(n);
    });
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["class"],
    });
    return () => observer.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [vars.join(",")]);
  return values;
}

function Swatch({
  name,
  varName,
  value,
  className,
}: {
  name: string;
  varName: string;
  value?: string;
  className: string;
}) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      onClick={() => {
        // The tick reports the copy, so it must wait for the copy. Clipboard
        // access is absent on an insecure origin and rejects when the document
        // is not focused or permission is denied — and an optional-chained call
        // that never ran still fell through to `setCopied(true)`, which is a
        // page about honest signals telling one small lie.
        navigator.clipboard
          ?.writeText(varName)
          .then(() => {
            setCopied(true);
            window.setTimeout(() => setCopied(false), 1200);
          })
          .catch(() => {
            /* nothing was copied, so nothing is reported */
          });
      }}
      className="group/swatch rounded-lg border border-border p-1 text-left transition-colors hover:bg-accent focus-visible:ring-3 focus-visible:ring-ring/50 focus-visible:outline-none"
      title={`Copy ${varName}`}
    >
      <div className={cn("h-12 w-full rounded-md border border-border", className)} />
      <div className="px-1.5 pt-1.5 pb-1">
        <div className="flex items-center gap-1">
          <span className="truncate text-2xs font-medium">{name}</span>
          {copied ? (
            <Check className="size-2.5 text-status-done" />
          ) : (
            <Copy className="size-2.5 shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover/swatch:opacity-100" />
          )}
        </div>
        <span className="block truncate font-mono text-3xs text-muted-foreground">
          {value || varName}
        </span>
      </div>
    </button>
  );
}

/*
 * Every class below is written out in full, never assembled from a template.
 * Tailwind finds classes by scanning source text: `bg-brand-${step}` is not a
 * string it can see, so the utility is never generated and the swatch renders
 * transparent. That failure is silent, which is exactly why this page — the
 * one place that must show a token honestly — spells them all out.
 */
const BRAND_STEPS = [
  { step: 50, cls: "bg-brand-50" },
  { step: 100, cls: "bg-brand-100" },
  { step: 200, cls: "bg-brand-200" },
  { step: 300, cls: "bg-brand-300" },
  { step: 400, cls: "bg-brand-400" },
  { step: 500, cls: "bg-brand-500" },
  { step: 600, cls: "bg-brand-600" },
  { step: 700, cls: "bg-brand-700" },
  { step: 800, cls: "bg-brand-800" },
  { step: 900, cls: "bg-brand-900" },
] as const;

const SURFACE_TOKENS = [
  { name: "background", cls: "bg-background" },
  { name: "card", cls: "bg-card" },
  { name: "popover", cls: "bg-popover" },
  { name: "muted", cls: "bg-muted" },
  { name: "secondary", cls: "bg-secondary" },
  { name: "accent", cls: "bg-accent" },
  { name: "sidebar", cls: "bg-sidebar" },
  { name: "sidebar-accent", cls: "bg-sidebar-accent" },
] as const;

function ColorSection() {
  const brandVars = useMemo(() => BRAND_STEPS.map((s) => `--brand-${s.step}`), []);
  const brand = useResolved(brandVars);
  const surfaceVars = useMemo(() => SURFACE_TOKENS.map((t) => `--${t.name}`), []);
  const surfaces = useResolved(surfaceVars);

  return (
    <Section
      title="Color"
      hint="Brand violet is the only hue the product owns. It marks interaction and identity — never status."
    >
      <div>
        <h3 className="mb-2 text-2xs font-medium tracking-wide text-muted-foreground uppercase">
          Brand ramp
        </h3>
        <div className="grid grid-cols-5 gap-2 sm:grid-cols-10">
          {BRAND_STEPS.map((s) => (
            <Swatch
              key={s.step}
              name={String(s.step)}
              varName={`--brand-${s.step}`}
              value={brand[`--brand-${s.step}`]}
              className={s.cls}
            />
          ))}
        </div>
        <p className="mt-2 text-2xs text-muted-foreground">
          500 is the brand. It carries interactive fills in light mode; dark mode
          steps up to 400, because 500 is too dense to read as ink on near-black.
        </p>
      </div>

      <div>
        <h3 className="mb-2 text-2xs font-medium tracking-wide text-muted-foreground uppercase">
          Surfaces
        </h3>
        <div className="grid grid-cols-4 gap-2 sm:grid-cols-8">
          {SURFACE_TOKENS.map((t) => (
            <Swatch
              key={t.name}
              name={t.name}
              varName={`--${t.name}`}
              value={surfaces[`--${t.name}`]}
              className={t.cls}
            />
          ))}
        </div>
      </div>

      <div>
        <h3 className="mb-2 text-2xs font-medium tracking-wide text-muted-foreground uppercase">
          Text hierarchy
        </h3>
        <Card>
          <CardContent className="space-y-1 py-4">
            <p className="text-sm text-ink-primary">
              ink-primary — active labels, channel headers. 16.5:1
            </p>
            <p className="text-sm text-ink-secondary">
              ink-secondary — nav items, section labels, names. 7.5:1
            </p>
            <p className="text-sm text-ink-tertiary">
              ink-tertiary — descriptions, body text. 6.4:1
            </p>
            <p className="text-sm text-ink-hint">
              ink-hint — subtitles, empty-state prompts. 5.4:1
            </p>
            <p className="text-sm text-ink-muted">
              ink-muted — member counts, metadata. 4.5:1
            </p>
            <p className="text-sm text-primary">primary — links and emphasis. 4.7:1</p>
            <p className="text-sm text-destructive">destructive — errors. 3.8:1 (marks)</p>
          </CardContent>
        </Card>
        <p className="mt-2 text-2xs text-muted-foreground">
          Five levels, from the brand guide. Ratios are worst case across both
          themes and every ground text sits on — the canvas, a card, and the
          active row. The weakest level sits on 4.5:1 and the rest step up from
          it in even increments of lightness.
        </p>
      </div>

      <div>
        <h3 className="mb-2 text-2xs font-medium tracking-wide text-muted-foreground uppercase">
          Chart series
        </h3>
        <div className="flex gap-2">
          {[1, 2, 3, 4, 5].map((n) => (
            <div key={n} className="flex-1">
              <div
                className="h-10 rounded-md"
                style={{ background: `var(--chart-${n})` }}
              />
              <span className="mt-1 block font-mono text-3xs text-muted-foreground">
                chart-{n}
              </span>
            </div>
          ))}
        </div>
        <p className="mt-2 text-2xs text-muted-foreground">
          Ordered so the two-series case gets violet and cyan — the pair that
          survives the most common colour-vision deficiencies.
        </p>
      </div>
    </Section>
  );
}

/* Spelled out, for the reason given above `BRAND_STEPS`. */
const STATUSES = [
  {
    key: "idle",
    label: "Idle",
    icon: CircleDashed,
    dot: "bg-status-idle",
    badge: "bg-status-idle-soft text-status-idle-text",
    bar: "bg-status-idle",
  },
  {
    key: "running",
    label: "Running",
    icon: Loader2,
    dot: "bg-status-running",
    badge: "bg-status-running-soft text-status-running-text",
    bar: "bg-status-running",
  },
  {
    key: "blocked",
    label: "Needs approval",
    icon: AlertTriangle,
    dot: "bg-status-blocked",
    badge: "bg-status-blocked-soft text-status-blocked-text",
    bar: "bg-status-blocked",
  },
  {
    key: "done",
    label: "Done",
    icon: Check,
    dot: "bg-status-done",
    badge: "bg-status-done-soft text-status-done-text",
    bar: "bg-status-done",
  },
  {
    key: "failed",
    label: "Failed",
    icon: X,
    dot: "bg-status-failed",
    badge: "bg-status-failed-soft text-status-failed-text",
    bar: "bg-status-failed",
  },
] as const;

/** The status vocabulary, shown in all three of the forms it is used in. */
function StatusSection() {
  return (
    <Section
      title="Run status"
      hint="Five states, five colours, used identically wherever a run appears. Each ships a mark weight, an accessible text weight, and a soft background — one value cannot do all three jobs."
    >
      <Card>
        <CardContent className="space-y-4 py-5">
          <div className="flex flex-wrap gap-2">
            {STATUSES.map((s) => (
              <span
                key={s.key}
                className={cn(
                  "inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-2xs font-medium",
                  s.badge,
                )}
              >
                <s.icon
                  className={cn("size-3", s.key === "running" && "animate-spin")}
                />
                {s.label}
              </span>
            ))}
          </div>
          <Separator />
          <div className="flex flex-wrap items-center gap-x-6 gap-y-2">
            {STATUSES.map((s) => (
              <span key={s.key} className="inline-flex items-center gap-2 text-sm">
                <span className={cn("size-2 rounded-full", s.dot)} aria-hidden />
                <span className="text-muted-foreground">{s.label}</span>
              </span>
            ))}
          </div>
          <Separator />
          <div className="space-y-1.5">
            {STATUSES.map((s) => (
              <div key={s.key} className="flex items-center gap-3">
                <span className="w-28 shrink-0 text-2xs text-muted-foreground">
                  {s.label}
                </span>
                <div
                  className={cn("h-1.5 rounded-full", s.bar)}
                  style={{ width: `${20 + STATUSES.indexOf(s) * 15}%` }}
                />
              </div>
            ))}
          </div>
        </CardContent>
      </Card>
      <p className="text-2xs text-muted-foreground">
        Never use the brand violet for a status, and never use a status hue for
        something clickable. The moment those cross, a green dot stops meaning
        "done".
      </p>
    </Section>
  );
}

/*
 * Display steps carry a shorter specimen. A type sheet exists to show the
 * letterforms, and a 24px line clipped mid-word shows the container instead.
 */
const TAGLINE = "Run an entire company with a headcount of one";
/* Spelled out, for the reason given above `BRAND_STEPS`. */
const TONES = [
  { n: 1, name: "violet", tile: "bg-tone-1/15 text-tone-1-text", chip: "bg-tone-1" },
  { n: 2, name: "blue", tile: "bg-tone-2/15 text-tone-2-text", chip: "bg-tone-2" },
  { n: 3, name: "teal", tile: "bg-tone-3/15 text-tone-3-text", chip: "bg-tone-3" },
  { n: 4, name: "fuchsia", tile: "bg-tone-4/15 text-tone-4-text", chip: "bg-tone-4" },
  { n: 5, name: "slate", tile: "bg-tone-5/15 text-tone-5-text", chip: "bg-tone-5" },
] as const;

const TONE_NAMES = ["OC", "AR", "MK", "JL", "TP"] as const;

/**
 * The identity palette, shown next to the status one on purpose — the whole
 * point of this section is that a reader can check the two do not collide.
 */
function ToneSection() {
  return (
    <Section
      title="Identity tones"
      hint="A categorical palette for who, not what state. Assigned by hash, so a name keeps its colour — and carrying no meaning beyond 'not the other one'."
    >
      <Card>
        <CardContent className="space-y-4 py-5">
          <div className="flex flex-wrap gap-2">
            {TONES.map((t, i) => (
              <span
                key={t.n}
                className={cn(
                  "grid size-9 place-items-center rounded-md text-xs font-medium",
                  t.tile,
                )}
                title={`tone-${t.n} · ${t.name}`}
              >
                {TONE_NAMES[i]}
              </span>
            ))}
          </div>
          <Separator />
          <div className="flex flex-wrap gap-x-6 gap-y-2">
            {TONES.map((t) => (
              <span key={t.n} className="inline-flex items-center gap-2 text-2xs">
                <span className={cn("size-2.5 rounded-sm", t.chip)} aria-hidden />
                <code className="font-mono text-3xs text-muted-foreground">
                  tone-{t.n}
                </code>
                <span className="text-muted-foreground">{t.name}</span>
              </span>
            ))}
          </div>
        </CardContent>
      </Card>
      <p className="text-2xs text-muted-foreground">
        No amber, no green, no red. Identity used to be drawn from the same
        palette as status, so a desk could be tinted the exact green that means
        “done”. Where the hues do come close, form separates them: identity is a{" "}
        <strong className="font-medium text-foreground">tile with initials</strong>,
        status is a{" "}
        <strong className="font-medium text-foreground">pill or dot with a label</strong>.
        They never take the same shape.
      </p>
    </Section>
  );
}

const TYPE_STEPS = [
  { cls: "text-3xs", px: "10px", sample: TAGLINE, use: "Table meta, graph labels, counters" },
  { cls: "text-2xs", px: "11px", sample: TAGLINE, use: "Captions, timestamps, key/value rows" },
  { cls: "text-xs", px: "12px", sample: TAGLINE, use: "Dense body — the workhorse" },
  { cls: "text-sm", px: "14px", sample: TAGLINE, use: "Default body, labels, buttons" },
  { cls: "text-base", px: "16px", sample: TAGLINE, use: "Long-form prose, empty states" },
  { cls: "text-lg", px: "18px", sample: "A company of one", use: "Card titles" },
  { cls: "text-xl", px: "20px", sample: "A company of one", use: "Section headings" },
  { cls: "text-2xl", px: "24px", sample: "A company of one", use: "View titles" },
] as const;

function TypeSection() {
  return (
    <Section
      title="Typography"
      hint="Geist Variable for everything the operator reads. Mono only for values that change in place — ids, durations, token counts — so digits do not reflow."
    >
      <Card>
        <CardContent className="divide-y divide-border py-0">
          {TYPE_STEPS.map((t) => (
            <div
              key={t.cls}
              className="flex items-baseline gap-4 py-2.5 first:pt-4 last:pb-4"
            >
              <code className="w-20 shrink-0 font-mono text-3xs text-muted-foreground">
                {t.cls}
              </code>
              <span className="w-10 shrink-0 font-mono text-3xs text-muted-foreground">
                {t.px}
              </span>
              <span className={cn("flex-1 truncate", t.cls)}>{t.sample}</span>
              <span className="hidden w-52 shrink-0 text-right text-2xs text-muted-foreground lg:block">
                {t.use}
              </span>
            </div>
          ))}
        </CardContent>
      </Card>
      <div className="grid gap-3 sm:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">Weights</CardTitle>
          </CardHeader>
          <CardContent className="space-y-1">
            <p className="text-sm font-normal">Normal — body text</p>
            <p className="text-sm font-medium">Medium — labels, buttons, active nav</p>
            <p className="text-sm font-semibold">Semibold — headings</p>
            <p className="text-2xs text-muted-foreground">
              Three weights, no more. Bold is not in the system.
            </p>
          </CardContent>
        </Card>
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">Mono, tabular</CardTitle>
          </CardHeader>
          <CardContent className="space-y-1 font-mono text-xs" data-numeric>
            <p>run_01J8XQ4M2K · 1,284 tok</p>
            <p>run_01J8XQ4M9F · 9,003 tok</p>
            <p>run_01J8XQ4N1A · 112 tok</p>
          </CardContent>
        </Card>
      </div>
    </Section>
  );
}

function ElevationSection() {
  return (
    <Section
      title="Elevation"
      hint="Shadows are tinted with the neutral hue, never pure black. In dark mode each step adds a 1px inset top highlight — shadow alone cannot separate two near-black surfaces."
    >
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-3 lg:grid-cols-6">
        {(
          [
            { name: "shadow-xs", cls: "shadow-xs" },
            { name: "shadow-sm", cls: "shadow-sm" },
            { name: "shadow-md", cls: "shadow-md" },
            { name: "shadow-lg", cls: "shadow-lg" },
            { name: "shadow-xl", cls: "shadow-xl" },
            { name: "shadow-brand", cls: "shadow-brand" },
          ] as const
        ).map((s) => (
          <div
            key={s.name}
            className={cn(
              "grid h-20 place-items-center rounded-lg border border-border bg-card",
              s.cls,
            )}
          >
            <span className="font-mono text-3xs text-muted-foreground">{s.name}</span>
          </div>
        ))}
      </div>
    </Section>
  );
}

function RadiusSection() {
  return (
    <Section
      title="Radius & spacing"
      hint="One --radius drives the whole scale, so a single edit re-rounds the app. Spacing is Tailwind's 4px base; the console lives between 1.5 and 6."
    >
      <div className="flex flex-wrap gap-4">
        {(
          [
            { name: "rounded-sm", cls: "rounded-sm" },
            { name: "rounded-md", cls: "rounded-md" },
            { name: "rounded-lg", cls: "rounded-lg" },
            { name: "rounded-xl", cls: "rounded-xl" },
            { name: "rounded-2xl", cls: "rounded-2xl" },
            { name: "rounded-full", cls: "rounded-full" },
          ] as const
        ).map((r) => (
          <div key={r.name} className="space-y-1.5 text-center">
            {/* Filled with the brand at low alpha rather than `bg-accent`:
                the accent tint is deliberately near-invisible against the
                canvas, which is right in the app and useless in a swatch
                whose only job is to show a corner. */}
            <div className={cn("size-16 border border-border bg-primary/20", r.cls)} />
            <span className="block font-mono text-3xs text-muted-foreground">
              {r.name}
            </span>
          </div>
        ))}
      </div>
    </Section>
  );
}

function MotionSection() {
  const [on, setOn] = useState(false);
  return (
    <Section
      title="Motion"
      hint="Three durations, two curves, and that is the whole vocabulary. Reduced-motion is honoured globally in the base layer, not per component."
    >
      <Card>
        <CardContent className="space-y-4 py-5">
          <div className="flex items-center gap-3">
            <Switch checked={on} onCheckedChange={setOn} id="sg-motion" />
            <Label htmlFor="sg-motion" className="text-sm">
              Play the durations
            </Label>
          </div>
          {(
            [
              { d: "fast", ms: "120ms", use: "Hover, press — must feel instant" },
              { d: "base", ms: "180ms", use: "Anything entering or leaving" },
              { d: "slow", ms: "260ms", use: "Sheets, dialogs, the sidebar" },
            ] as const
          ).map((m) => (
            <div key={m.d} className="flex items-center gap-3">
              <code className="w-28 shrink-0 font-mono text-3xs text-muted-foreground">
                duration-{m.d}
              </code>
              <div className="relative h-6 flex-1 rounded-md bg-muted">
                <div
                  className="absolute top-1 size-4 rounded-sm bg-primary"
                  style={{
                    left: on ? "calc(100% - 1.25rem)" : "0.25rem",
                    transitionProperty: "left",
                    transitionDuration: `var(--duration-${m.d})`,
                    transitionTimingFunction: "var(--ease-emphasized)",
                  }}
                />
              </div>
              <span className="hidden w-56 shrink-0 text-2xs text-muted-foreground sm:block">
                {m.use}
              </span>
            </div>
          ))}
        </CardContent>
      </Card>
    </Section>
  );
}

const BUTTON_VARIANTS = [
  "default",
  "secondary",
  "outline",
  "ghost",
  "destructive",
  "link",
] as const;

function ComponentSection() {
  return (
    <Section
      title="Components"
      hint="Each primitive in every state it ships. If a state is missing here, it is unspecified — build it into the primitive rather than restyling at the call site."
    >
      <Card>
        <CardHeader>
          <CardTitle className="text-sm">Button</CardTitle>
          <CardDescription className="text-2xs">
            Variants down, states across. One primary action per view.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-3">
          {BUTTON_VARIANTS.map((v) => (
            <div key={v} className="flex flex-wrap items-center gap-2">
              <code className="w-20 shrink-0 font-mono text-3xs text-muted-foreground">
                {v}
              </code>
              <Button variant={v}>Run workflow</Button>
              <Button variant={v} disabled>
                Disabled
              </Button>
              <Button variant={v}>
                <Play /> With icon
              </Button>
            </div>
          ))}
          <Separator />
          <div className="flex flex-wrap items-center gap-2">
            <code className="w-20 shrink-0 font-mono text-3xs text-muted-foreground">
              sizes
            </code>
            {(["xs", "sm", "default", "lg"] as const).map((s) => (
              <Button key={s} size={s}>
                {s}
              </Button>
            ))}
            {(["icon-xs", "icon-sm", "icon", "icon-lg"] as const).map((s) => (
              <Button key={s} size={s} variant="outline" aria-label={s}>
                <ChevronRight />
              </Button>
            ))}
          </div>
        </CardContent>
      </Card>

      <div className="grid gap-4 lg:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="text-sm">Badge</CardTitle>
          </CardHeader>
          <CardContent className="flex flex-wrap gap-2">
            <Badge>Default</Badge>
            <Badge variant="secondary">Secondary</Badge>
            <Badge variant="outline">Outline</Badge>
            <Badge variant="destructive">Destructive</Badge>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-sm">Avatar</CardTitle>
          </CardHeader>
          <CardContent className="flex items-center gap-2">
            {["OC", "AR", "MK"].map((i) => (
              <Avatar key={i}>
                <AvatarFallback>{i}</AvatarFallback>
              </Avatar>
            ))}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-sm">Form controls</CardTitle>
          </CardHeader>
          <CardContent className="space-y-3">
            <div className="space-y-1.5">
              <Label htmlFor="sg-input">Company name</Label>
              <Input id="sg-input" placeholder="acme-marketing" />
            </div>
            <div className="space-y-1.5">
              <Label htmlFor="sg-invalid">Invalid state</Label>
              <Input id="sg-invalid" aria-invalid defaultValue="not a company" />
            </div>
            <div className="space-y-1.5">
              <Label htmlFor="sg-disabled">Disabled</Label>
              <Input id="sg-disabled" disabled placeholder="Unavailable" />
            </div>
            <div className="space-y-1.5">
              <Label htmlFor="sg-textarea">Mandate</Label>
              <Textarea id="sg-textarea" placeholder="What this desk is responsible for…" />
            </div>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-sm">Feedback</CardTitle>
          </CardHeader>
          <CardContent className="space-y-3">
            <Alert>
              <AlertTitle>Heads up</AlertTitle>
              <AlertDescription>
                Two runs are waiting on your approval.
              </AlertDescription>
            </Alert>
            <Alert variant="destructive">
              <AlertTitle>Run failed</AlertTitle>
              <AlertDescription>
                The host rejected the credential. Reconnect and try again.
              </AlertDescription>
            </Alert>
            <div className="space-y-2">
              <Skeleton className="h-4 w-3/4" />
              <Skeleton className="h-4 w-1/2" />
            </div>
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-sm">Tabs & tooltip</CardTitle>
        </CardHeader>
        <CardContent>
          <Tabs defaultValue="overview">
            <TabsList>
              <TabsTrigger value="overview">Overview</TabsTrigger>
              <TabsTrigger value="runs">Runs</TabsTrigger>
              <TabsTrigger value="settings">Settings</TabsTrigger>
            </TabsList>
            <TabsContent value="overview" className="pt-3 text-sm text-muted-foreground">
              Tab panels inherit body type. They do not restate the tab label.
            </TabsContent>
            <TabsContent value="runs" className="pt-3 text-sm text-muted-foreground">
              <Tooltip>
                <TooltipTrigger render={<Button variant="outline" size="sm" />}>
                  Hover me
                </TooltipTrigger>
                <TooltipContent>Tooltips label, they never explain</TooltipContent>
              </Tooltip>
            </TabsContent>
            <TabsContent value="settings" className="pt-3 text-sm text-muted-foreground">
              Nothing here.
            </TabsContent>
          </Tabs>
        </CardContent>
      </Card>
    </Section>
  );
}
