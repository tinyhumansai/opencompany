import { Fragment } from "react";

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
import { COPY } from "./catalogue";
import { addOptions } from "./resolve";
import type { AddOption } from "./resolve";
import type { SearchProvider } from "./types";

/**
 * Add a search provider: two categories, because they ask two different
 * questions.
 *
 * A modal rather than a section on the page, for the reason the LLM page's is:
 * listing every provider inline spends the page on the ones nobody chose and
 * buries the ones actually configured.
 *
 * ## Why two and not three
 *
 * The LLM dialog has three categories and a custom-provider escape hatch. There
 * is no third question here and no custom option, because there is no generic
 * search API to be custom against: Brave, Exa and Querit each speak a different
 * REST shape parsed by a different struct, so a custom search provider would be
 * a custom *parser*, not a custom URL. SearXNG is the self-hosted escape hatch
 * and the only one there can be. See `docs/modules/search/known-defects.md`.
 *
 * ## Two list rules that are not optional
 *
 * **Each category lists only what is not yet connected** — the page behind this
 * shows the rest, and offering to add something twice is how you get two rows
 * for one provider. Here that rule does a second job: the harness dispatches on
 * the slug, so one row per slug is what keeps the set closed.
 *
 * **The select's value stays pinned empty**: choosing an item starts a connect
 * flow and leaves nothing selected, because the connection state lives in the
 * page rather than in the control.
 *
 * ## No decisions live here
 *
 * What each list holds is `addOptions` in `resolve.ts`, with unit tests of its
 * own. What is left is layout.
 */
export function AddProviderDialog({
  open,
  onOpenChange,
  providers,
  onChoose,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  providers: readonly SearchProvider[];
  /** The chosen catalogue slug. */
  onChoose: (slug: string) => void;
}) {
  const options = addOptions(providers);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md" data-testid="search-add-provider">
        <DialogHeader>
          <DialogTitle>Add a provider</DialogTitle>
          <DialogDescription>
            Pick a provider to connect. You can add more at any time.
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-5">
          <Category
            id="account"
            label={COPY.groupAccount}
            placeholder={COPY.placeholderAccount}
            helper={COPY.helperAccount}
            options={options.account}
            onChoose={onChoose}
          />
          <Category
            id="self-hosted"
            label={COPY.groupSelfHosted}
            placeholder={COPY.placeholderSelfHosted}
            helper={COPY.helperSelfHosted}
            options={options.selfHosted}
            onChoose={onChoose}
          />
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
  onChoose: (slug: string) => void;
}) {
  const empty = options.length === 0;
  return (
    <div className="grid gap-1.5">
      <Label htmlFor={`search-add-${id}`}>{label}</Label>
      <Select
        // Pinned empty, on every render. Choosing an item starts a flow; it does
        // not put the control into a state. A select that kept the last pick
        // would claim a selection it does not own.
        value={null}
        disabled={empty}
        onValueChange={(value) => value && onChoose(String(value))}
      >
        <SelectTrigger id={`search-add-${id}`} className="w-full">
          <SelectValue placeholder={placeholder}>
            {() => placeholder}
          </SelectValue>
        </SelectTrigger>
        <SelectContent>
          {options.map((option) => (
            <SelectItem key={option.value} value={option.value}>
              <span className="flex min-w-0 items-center gap-2">
                <Monogram label={option.label} />
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
        {empty ? <Fragment>{COPY.everythingConnected}</Fragment> : helper}
      </p>
    </div>
  );
}

/**
 * A provider's mark: its initial, on a neutral swatch.
 *
 * A **monogram, not a brand logo**. Shipping four vendors' trademarks opens a
 * licensing question this feature has no need to open, and a missing logo would
 * leave a hole in the row. The swatch is a single design token rather than a
 * per-provider tint — the mark is the information and the tint is decoration,
 * and `scripts/ci/assert-design-tokens.sh` rejects the literal hex a per-slug
 * palette invites.
 *
 * `aria-hidden`, because the provider's name is right beside it and announcing
 * "B, Brave Search" helps nobody.
 */
export function Monogram({ label }: { label: string }) {
  return (
    <span
      aria-hidden
      className="flex size-6 shrink-0 items-center justify-center rounded-md bg-muted text-3xs font-semibold text-muted-foreground"
    >
      {label.trim().slice(0, 1).toUpperCase()}
    </span>
  );
}
