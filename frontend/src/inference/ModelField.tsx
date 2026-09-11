import { useEffect, useState } from "react";
import { ChevronDown } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { listProviderModels } from "@/api/inference";
import type { ProviderCatalog } from "@/api/inference";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { cn } from "@/lib/utils";
import { cappedNote, filterModels } from "./model-filter";

/** What blank means, said out loud rather than left as an absence. */
export const TIER_DEFAULT_LABEL = "Send the tier — let the endpoint resolve it";

/**
 * Whether the field shows the catalog select rather than the text input.
 *
 * A function, not an expression in the component, because it is four reasons
 * rolled into one answer and each of them is a decision somebody could get
 * wrong:
 *
 * - **`typed`** — the operator asked for the text field. Their choice wins.
 * - **`freeTextOnly`** — an Azure endpoint routes on a deployment name its own
 *   `/models` never publishes, so a closed list makes the only correct value
 *   unreachable.
 * - **an empty list** — no catalog, or a read that failed. An empty select reads
 *   as "this provider has no models", which nobody established.
 * - **a value the list does not contain** — something already typed, or an id
 *   from a stale catalog. Upgrading the field out from under it would discard
 *   it, which is the same class of bug as stripping a value mid-keystroke.
 */
export function showsCatalogSelect({
  catalog,
  typed,
  value,
}: {
  catalog: Pick<ProviderCatalog, "models" | "freeTextOnly"> | null;
  typed: boolean;
  value: string;
}): boolean {
  if (typed || !catalog || catalog.freeTextOnly) return false;
  if (catalog.models.length === 0) return false;
  return !value || catalog.models.includes(value);
}

/**
 * Choosing a model id for one provider.
 *
 * **A catalog select with a free-text escape hatch**, not one or the other. The
 * catalog is what makes this usable — an operator who has to know a vendor's id
 * scheme by heart is being asked the wrong question — and the escape hatch is
 * what keeps it correct.
 *
 * ## Why the escape hatch is always reachable, not only on failure
 *
 * Azure routes on a **deployment name** while `/models` publishes **base model
 * ids**, so at an Azure endpoint the only correct value is one the catalog will
 * never contain. The host flags those endpoints and this defaults to text there;
 * everywhere else the catalog is the default and the toggle is one click away,
 * because a catalog can be stale or incomplete anywhere.
 *
 * ## The three honest states of a catalog read
 *
 * Loading says so rather than showing an empty list that fills in underneath a
 * click. A failed or empty read falls back to text **and says why** — an empty
 * select reads as "this provider has no models", which nobody established. A
 * successful read is a select.
 *
 * ## Blank stays meaningful
 *
 * Empty is not "unset the field", it is *send the tier and let the endpoint
 * resolve it* — which is how `TierVocabulary` passthrough works and is the right
 * answer at a tier-native endpoint. So it is an item in the list with its own
 * label, never an absence.
 *
 * ## Nothing typed is ever discarded
 *
 * The catalog arrives asynchronously and the field starts as text. When the list
 * lands it becomes a select **only if nothing has been typed** — upgrading a
 * field out from under someone mid-keystroke is the same class of bug as
 * stripping a value mid-keystroke, which this module's neighbour has nine
 * recorded instances of.
 */
export function ModelField({
  client,
  company,
  slug,
  id,
  value,
  disabled,
  onChange,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /** The provider whose catalog to read, or `null` for none (no provider chosen). */
  slug: string | null;
  id: string;
  value: string;
  disabled?: boolean;
  onChange: (next: string) => void;
}) {
  const [catalog, setCatalog] = useState<ProviderCatalog | null>(null);
  const [loading, setLoading] = useState(false);
  /** Whether the operator asked for the text field explicitly. */
  const [typed, setTyped] = useState(false);

  useEffect(() => {
    setCatalog(null);
    setTyped(false);
    if (!slug) return;
    let live = true;
    setLoading(true);
    listProviderModels(client, company, slug)
      .then((next) => live && setCatalog(next))
      // A read that fails entirely is the same state as one that answered with
      // nothing usable: fall back to text and say so.
      .catch(() => live && setCatalog(null))
      .finally(() => live && setLoading(false));
    return () => {
      live = false;
    };
  }, [client, company, slug]);

  const listed = catalog?.models ?? [];
  const offersSelect = showsCatalogSelect({ catalog, typed, value });

  return (
    <div className="grid gap-1.5">
      <Label htmlFor={id}>Model id</Label>
      {offersSelect ? (
        <ModelCombobox id={id} models={listed} value={value} disabled={disabled} onChange={onChange} />
      ) : (
        <Input
          id={id}
          value={value}
          // A model id belongs to a provider. With none chosen there is nothing
          // to type one against, and an id typed here could only be discarded.
          disabled={disabled || !slug}
          placeholder={slug ? "Leave blank to send the tier" : "Choose a provider first"}
          autoComplete="off"
          spellCheck={false}
          className="font-mono text-xs"
          onChange={(e) => onChange(e.target.value)}
        />
      )}

      <ModelFieldNote
        loading={loading}
        chosen={Boolean(slug)}
        catalog={catalog}
        offersSelect={offersSelect}
        onUseCatalog={() => setTyped(false)}
        onUseText={() => setTyped(true)}
      />
    </div>
  );
}

/**
 * The one line under the field, which says whichever of four things is true.
 *
 * Split out because "which sentence" is a four-branch decision and the field
 * above should read as layout.
 */
function ModelFieldNote({
  loading,
  chosen,
  catalog,
  offersSelect,
  onUseCatalog,
  onUseText,
}: {
  loading: boolean;
  /** Whether a provider has been chosen at all. */
  chosen: boolean;
  catalog: ProviderCatalog | null;
  offersSelect: boolean;
  onUseCatalog: () => void;
  onUseText: () => void;
}) {
  // Its own state, and first, because the fallback below says "this provider
  // publishes no model list" — an assertion about a provider that does not
  // exist yet. No provider has been asked anything, so nothing is known about
  // one, and saying otherwise is a screen inventing a fact.
  if (!chosen) {
    return (
      <p className="text-xs text-muted-foreground" data-testid="inference-model-no-provider">
        Choose a provider first — the model list is read from whichever one you pick.
      </p>
    );
  }
  if (loading) {
    return <p className="text-xs text-muted-foreground">Reading this provider&apos;s models…</p>;
  }
  if (catalog?.freeTextOnly) {
    return (
      <p className="text-xs text-muted-foreground">
        This endpoint routes on a deployment name, which its model list does not publish — type
        the name you gave the deployment.
      </p>
    );
  }
  if (offersSelect) {
    return (
      <Button
        type="button"
        variant="link"
        size="sm"
        className="h-auto justify-self-start p-0 text-xs"
        data-testid="inference-model-enter-id"
        onClick={onUseText}
      >
        Enter a model id instead
      </Button>
    );
  }
  if (catalog && catalog.models.length > 0) {
    return (
      <Button
        type="button"
        variant="link"
        size="sm"
        className="h-auto justify-self-start p-0 text-xs"
        data-testid="inference-model-use-catalog"
        onClick={onUseCatalog}
      >
        Choose from this provider&apos;s models instead
      </Button>
    );
  }
  return (
    <p className="text-xs text-muted-foreground" data-testid="inference-model-no-catalog">
      {catalog?.error ?? "This provider publishes no model list, so type an id."}
    </p>
  );
}


/**
 * The model list, with a filter box.
 *
 * A combobox rather than a plain select, because OpenRouter publishes around
 * four hundred ids and a flat scrolling list of four hundred is not a control —
 * it is a haystack with a scrollbar. The ranking is
 * [`filterModels`](./model-filter.ts), which is `search/rank.ts` underneath.
 *
 * ## Three rows that are not models
 *
 * **Blank stays pinned at the top and is never filtered out.** It is not a model
 * id, and typing `claude` must not make "send the tier" disappear — that is a
 * real state of this field and the operator may be typing on their way to
 * changing their mind.
 *
 * **What you typed is offered as itself** when nothing matched. A search that
 * finds nothing and a perfectly good model id are the same string at an Azure
 * endpoint or against a stale catalog, so one control does both jobs rather than
 * dead-ending into a separate toggle.
 *
 * **The cap says what it hid.** A list that quietly stops at fifty is a list
 * that lies about what the provider serves.
 */
function ModelCombobox({
  id,
  models,
  value,
  disabled,
  onChange,
}: {
  id: string;
  models: readonly string[];
  value: string;
  disabled?: boolean;
  onChange: (next: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [term, setTerm] = useState("");
  const [active, setActive] = useState(0);

  const matched = filterModels(models, term);
  // Blank first and unfiltered; the typed id last, where a new thing belongs.
  const rows: { value: string; label: string; hint?: string }[] = [
    { value: "", label: TIER_DEFAULT_LABEL },
    ...matched.shown.map((model) => ({ value: model, label: model })),
    ...(matched.offerTyped
      ? [{ value: matched.offerTyped, label: matched.offerTyped, hint: "use as the model id" }]
      : []),
  ];
  const capped = cappedNote(matched);

  const pick = (next: string) => {
    onChange(next);
    setOpen(false);
    setTerm("");
  };

  return (
    <Popover
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) setTerm("");
        setActive(0);
      }}
    >
      <PopoverTrigger
        render={
          <button
            type="button"
            id={id}
            disabled={disabled}
            aria-haspopup="listbox"
            aria-expanded={open}
            className="flex h-9 w-full items-center justify-between gap-2 rounded-md border border-input px-3 text-left text-sm disabled:opacity-50"
          >
            <span className={cn("truncate", !value && "text-muted-foreground")}>
              {value || TIER_DEFAULT_LABEL}
            </span>
            <ChevronDown className="size-4 shrink-0 text-muted-foreground" />
          </button>
        }
      />
      <PopoverContent align="start" className="w-(--anchor-width) p-0">
        <div className="border-b border-border p-2">
          <Input
            autoFocus
            value={term}
            placeholder="Filter models…"
            autoComplete="off"
            spellCheck={false}
            aria-label="Filter models"
            className="h-8 font-mono text-xs"
            onChange={(e) => {
              setTerm(e.target.value);
              setActive(0);
            }}
            onKeyDown={(e) => {
              if (e.key === "ArrowDown") {
                e.preventDefault();
                setActive((i) => Math.min(i + 1, rows.length - 1));
              } else if (e.key === "ArrowUp") {
                e.preventDefault();
                setActive((i) => Math.max(i - 1, 0));
              } else if (e.key === "Enter") {
                e.preventDefault();
                const row = rows[active];
                if (row) pick(row.value);
              } else if (e.key === "Escape" && term) {
                // Clears the filter first and only closes on the second press —
                // the same two-stage Escape the search dialog uses, and the
                // reason is the same: one keystroke should not discard both the
                // thing you typed and the thing you opened.
                e.preventDefault();
                e.stopPropagation();
                setTerm("");
                setActive(0);
              }
            }}
          />
        </div>
        <ul role="listbox" aria-label="Models" className="max-h-64 overflow-y-auto p-1">
          {rows.map((row, index) => (
            <li key={`${row.value}-${index}`}>
              <button
                type="button"
                role="option"
                aria-selected={row.value === value}
                className={cn(
                  "flex w-full items-baseline gap-2 rounded-sm px-2 py-1.5 text-left text-sm",
                  index === active && "bg-accent",
                )}
                onMouseEnter={() => setActive(index)}
                onClick={() => pick(row.value)}
              >
                <span className={cn("truncate", row.value && "font-mono text-xs")}>
                  {row.label}
                </span>
                {row.hint && (
                  <span className="ms-auto shrink-0 text-xs text-muted-foreground">{row.hint}</span>
                )}
              </button>
            </li>
          ))}
        </ul>
        {capped && <p className="border-t border-border p-2 text-xs text-muted-foreground">{capped}</p>}
      </PopoverContent>
    </Popover>
  );
}
