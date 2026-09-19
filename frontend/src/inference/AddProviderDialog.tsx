import { Fragment } from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Separator } from "@/components/ui/separator";
import { COPY } from "./catalogue";
import { ProviderMark, hasMark } from "./provider-icon";
import { addOptions } from "./connect";
import type { AddOption } from "./connect";
import type { Provider } from "./types";

/**
 * Add a provider: two categories, because they ask two different questions.
 *
 * A modal rather than a section on the page. There are thirty-odd providers and
 * a company connects one or two; listing them inline spends the page on the ones
 * nobody chose and buries the ones actually configured.
 *
 * ## Why two selects and not one list
 *
 * A cloud provider wants an API key and a local runtime wants an endpoint on
 * this machine. One flat list makes the operator infer that from a group
 * heading; a select per category has a label and a line of helper text to say
 * it outright.
 *
 * Custom is deliberately **not** a third select. It is one option, and a select
 * over one option is a button wearing a costume.
 *
 * ## Two list rules that are not optional
 *
 * **Each category lists only what is not yet connected** — the page behind this
 * shows the rest, and offering to add something twice is how you get two rows
 * for one provider. **The select's value stays pinned empty**: choosing an item
 * starts a connect flow and leaves nothing selected, because the connection
 * state lives in the page rather than in the control.
 *
 * ## No decisions live here
 *
 * What each list holds, what a category's detail line reads, and whether an
 * option is already connected are all functions in `connect.ts` with unit tests
 * of their own. What is left is layout.
 */
export function AddProviderDialog({
  open,
  onOpenChange,
  providers,
  onChoose,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  providers: readonly Provider[];
  /** The chosen option slug, or `custom`. */
  onChoose: (optionSlug: string) => void;
}) {
  const options = addOptions(providers);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md" data-testid="inference-add-provider">
        <DialogHeader>
          <DialogTitle>Add a provider</DialogTitle>
          <DialogDescription>
            Pick a provider to connect. You can add more at any time.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-5">
          <Category
            id="cloud"
            label={COPY.groupCloud}
            placeholder={COPY.placeholderCloud}
            helper={COPY.helperCloud}
            options={options.cloud}
            onChoose={onChoose}
          />
          <Category
            id="local"
            label={COPY.groupLocal}
            placeholder={COPY.placeholderLocal}
            helper={COPY.helperLocal}
            options={options.local}
            onChoose={onChoose}
          />

          <Separator />

          <p className="text-xs text-muted-foreground" data-testid="inference-add-harness-hint">
            Claude Code and Codex are bound per teammate, on an agent&apos;s Model tab.{" "}
            <a
              className="underline underline-offset-2 transition-opacity hover:opacity-80"
              href="#/company/agents"
              onClick={() => onOpenChange(false)}
            >
              Open the roster
            </a>
            .
          </p>

          <Separator />

          <div className="grid gap-1.5">
            <Button
              type="button"
              variant="outline"
              className="w-full"
              data-testid="inference-add-custom"
              onClick={() => onChoose("custom")}
            >
              Add a custom provider
            </Button>
            <p className="text-xs text-muted-foreground">Your own OpenAI-compatible endpoint</p>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function Category({
  id,
  label,
  placeholder,
  helper,
  options,
  onChoose,
}: {
  id: string;
  label: string;
  placeholder: string;
  helper: string;
  options: AddOption[];
  onChoose: (optionSlug: string) => void;
}) {
  const empty = options.length === 0;
  return (
    <div className="grid gap-1.5">
      <Label htmlFor={`inference-add-${id}`}>{label}</Label>
      <Select
        // Pinned empty, on every render. Choosing an item starts a flow; it does
        // not put the control into a state. A select that kept the last pick
        // would claim a selection it does not own.
        value={null}
        disabled={empty}
        onValueChange={(v) => v && onChoose(String(v))}
      >
        <SelectTrigger id={`inference-add-${id}`} className="w-full">
          <SelectValue placeholder={placeholder}>{() => placeholder}</SelectValue>
        </SelectTrigger>
        <SelectContent>
          {options.map((option) => (
            <SelectItem key={option.value} value={option.value}>
              <span className="flex min-w-0 items-center gap-2">
                <Monogram label={option.label} slug={option.value} />
                <span className="grid min-w-0 text-left leading-tight">
                  <span className="truncate">{option.label}</span>
                  {/* Monospace, because every one of these is an address or a
                      statement about where the thing runs — not prose. */}
                  <span className="truncate font-mono text-3xs text-muted-foreground">
                    {option.detail}
                  </span>
                </span>
              </span>
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
      <p className="text-xs text-muted-foreground">
        {empty ? <Fragment>Everything here is already connected.</Fragment> : helper}
      </p>
    </div>
  );
}

/**
 * A provider's mark: its brand logo where we ship one, its initial where we do
 * not.
 *
 * The swatch behind it is a **single neutral token** rather than a per-provider
 * tint. openhuman keys a colour off each slug, with literal hex in places, which
 * `scripts/ci/assert-design-tokens.sh` rejects here — and the mark is the
 * information, the tint is decoration.
 *
 * The letter is not a fallback to be embarrassed about; see `provider-icon.tsx`.
 * Both branches are `aria-hidden`, because the provider's name is right beside
 * this and announcing "O, OpenAI" helps nobody.
 */
export function Monogram({ label, slug }: { label: string; slug?: string }) {
  return (
    <span
      aria-hidden
      className="flex size-6 shrink-0 items-center justify-center rounded-md bg-muted text-3xs font-semibold text-muted-foreground"
    >
      {slug && hasMark(slug) ? (
        <ProviderMark slug={slug} className="size-3.5" />
      ) : (
        label.trim().slice(0, 1).toUpperCase()
      )}
    </span>
  );
}
