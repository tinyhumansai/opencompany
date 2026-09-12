import { useRef } from "react";
import { Check, KeyRound, PlugZap, ShieldCheck, Trash2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { SELECTED_BADGE } from "./rows";
import type { ComposioRow, ComposioRowId } from "./types";

/**
 * The mark at the head of a row.
 *
 * A glyph, not a brand logo. Both rows reach the same vendor, so a Composio
 * logo on each would distinguish nothing — what the eye is picking out here is
 * *whose account*, which is what a shield (somebody else holds it) and a key
 * (this company holds it) say.
 */
function Mark({ id }: { id: ComposioRowId }) {
  const Icon = id === "managed" ? ShieldCheck : KeyRound;
  return (
    <span
      aria-hidden
      className="flex size-8 shrink-0 items-center justify-center rounded-md bg-muted text-muted-foreground"
    >
      <Icon className="size-4" />
    </span>
  );
}

/**
 * The Connected card's rows: which account this company reaches Composio with.
 *
 * Each row is **a mark, a name, one sub-line and its controls**, and nothing
 * else. The page this replaces opened with a four-branch paragraph that
 * explained what each route meant, what saving would change and where the
 * providers went — every clause of it explaining a control that was visible
 * while it was being read.
 *
 * # Single-select, not a toggle per row
 *
 * `composioRows` carries the argument in full. The short version: Composio's
 * two modes are one stored scalar, so a per-row toggle would make both-on and
 * both-off reachable with nowhere to store them. The selected row is a checked
 * radio wearing an `Active` badge; the other is an unchecked radio wearing
 * `Use this`.
 *
 * # No decisions live here
 *
 * Which sub-line a row gets, which controls it may offer, whether the managed
 * route is safe to switch to — all of it is decided in `rows.ts` with a unit
 * test apiece. What is left is layout and handlers.
 */
export function ComposioRowList({
  rows,
  canManage,
  busy,
  onSelect,
  onAddKey,
  onReplaceKey,
  onRemoveKey,
  onTest,
  testingRow,
}: {
  rows: readonly ComposioRow[];
  /**
   * Whether this viewer may change what the company connects through (#403).
   *
   * Courtesy, not enforcement — the host refuses the write either way. What it
   * prevents is offering a control whose Save is refused only after the
   * operator has already pasted a live credential into it. The rows themselves
   * stay legible to a member, because what tells them why their agents can
   * reach Gmail is exactly this card.
   */
  canManage: boolean;
  busy: boolean;
  onSelect: (row: ComposioRow) => void;
  onAddKey: (row: ComposioRow) => void;
  onReplaceKey: (row: ComposioRow) => void;
  onRemoveKey: (row: ComposioRow) => void;
  /** Check this row's stored credential in place. Writes nothing. */
  onTest: (row: ComposioRow) => void;
  /**
   * The row whose check is in flight, or `null`.
   *
   * Per-row rather than folded into `busy`, because a check is the one action
   * here that changes nothing: disabling the whole card for it would tell the
   * operator a write is happening. Only the button that was pressed reports
   * itself.
   */
  testingRow: ComposioRowId | null;
}) {
  const radios = useRef<(HTMLButtonElement | null)[]>([]);

  // The rows that carry a radio, in render order — also the order the arrow
  // keys walk. A row can be in the list without one: the managed row hides its
  // `Use this` when the managed chain resolves to nothing, and a control that
  // cannot act must not be in the keyboard walk either.
  const selectable = rows.filter((row) => row.active || row.controls.select);

  /**
   * Arrow keys move between the routes and select in the same step, the way
   * native radios behave.
   *
   * Without it every radio stays in the Tab order and no Arrow key moves
   * between them — a screen reader announces radiogroup controls whose keyboard
   * behaviour does not exist. Same shape `policy-settings` uses for its
   * approval tiers. Navigates from the radio that has FOCUS: the keydown
   * bubbles from the focused button to the container, so `event.target` is that
   * button. Wraps at both ends rather than dead-ending.
   */
  function handleKeyDown(event: React.KeyboardEvent<HTMLUListElement>) {
    if (busy || !canManage) return;
    if (
      !["ArrowDown", "ArrowRight", "ArrowUp", "ArrowLeft"].includes(event.key)
    )
      return;
    const step =
      event.key === "ArrowDown" || event.key === "ArrowRight" ? 1 : -1;
    const focused = radios.current.indexOf(event.target as HTMLButtonElement);
    if (focused === -1) return;
    event.preventDefault();
    const next = (focused + step + selectable.length) % selectable.length;
    radios.current[next]?.focus();
    const row = selectable[next];
    // Moving onto the row a company is already on is not a change, so it does
    // not fire a write — the focus move is the whole effect, exactly as a
    // native radio group behaves when you arrow back onto the checked option.
    if (row && !row.active) onSelect(row);
  }

  return (
    <ul
      role="radiogroup"
      aria-label="Which Composio account this company uses"
      className="divide-y divide-border"
      data-testid="composio-rows"
      onKeyDown={handleKeyDown}
    >
      {rows.map((row) => {
        const index = selectable.indexOf(row);
        return (
          <li
            key={row.id}
            className="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-3"
            data-testid={`composio-row-${row.id}`}
          >
            <Mark id={row.id} />
            <span className="grid min-w-0 flex-1 leading-tight">
              <span className="truncate text-sm font-medium">{row.label}</span>
              <span
                className={cn(
                  "truncate text-xs",
                  row.tone === "warning"
                    ? "text-status-blocked-text"
                    : "text-muted-foreground",
                )}
                data-testid={`composio-row-${row.id}-subline`}
              >
                {row.subline}
              </span>
            </span>

            {index !== -1 && (
              <button
                ref={(el) => {
                  radios.current[index] = el;
                }}
                type="button"
                role="radio"
                aria-checked={row.active}
                // Roving tabindex: one stop for the whole group, on the checked
                // option, with the arrows moving inside it.
                tabIndex={row.active ? 0 : -1}
                disabled={!canManage || busy}
                data-testid={`composio-row-${row.id}-select`}
                onClick={() => {
                  if (!row.active) onSelect(row);
                }}
                className={cn(
                  "inline-flex shrink-0 items-center gap-1 rounded-md px-2 py-1 text-xs font-medium transition-colors",
                  "disabled:cursor-not-allowed disabled:opacity-60",
                  row.active
                    ? "bg-secondary text-secondary-foreground"
                    : "border border-border hover:bg-muted",
                )}
              >
                {/* The tick is part of the claim, not decoration, so it is
                    held to the same standard as the word beside it: a route
                    that is selected but resolves to nothing gets neither. The
                    radio is still `aria-checked` — what is withheld is the
                    assertion that it works, not the fact that it is chosen. */}
                {row.active && row.tone !== "warning" && (
                  <Check className="size-3" />
                )}
                {/* `row.badge` decides the word; this file does not. The
                    fallback used to be "Active", which quietly restored the
                    exact claim `rows.ts` had just refused to make — so if a row
                    is ever active with no badge, it reads as selected rather
                    than as working. */}
                {row.active ? (row.badge ?? SELECTED_BADGE) : "Use this"}
              </button>
            )}

            {canManage && (
              <span className="flex shrink-0 flex-wrap items-center gap-2">
                {/* Test comes first: it is the only control here that changes
                    nothing, so it sits before the two that do and well away
                    from Remove. `row.controls.test` is the decision — it is
                    false wherever the host would answer "nothing to check", so
                    this is never a button that can only fail. */}
                {row.controls.test && (
                  <Button
                    variant="outline"
                    size="sm"
                    disabled={busy || testingRow !== null}
                    data-testid={`composio-row-${row.id}-test`}
                    onClick={() => onTest(row)}
                  >
                    <PlugZap className="size-3.5" />
                    {testingRow === row.id ? "Testing…" : "Test"}
                  </Button>
                )}
                {row.controls.addKey && (
                  <Button
                    variant="outline"
                    size="sm"
                    disabled={busy}
                    data-testid={`composio-row-${row.id}-add`}
                    onClick={() => onAddKey(row)}
                  >
                    Add a {row.keyNoun}
                  </Button>
                )}
                {row.controls.replaceKey && (
                  <Button
                    variant="outline"
                    size="sm"
                    disabled={busy}
                    data-testid={`composio-row-${row.id}-replace`}
                    onClick={() => onReplaceKey(row)}
                  >
                    Replace {row.keyNoun}
                  </Button>
                )}
                {row.controls.removeKey && (
                  <Button
                    variant="destructive"
                    size="sm"
                    disabled={busy}
                    data-testid={`composio-row-${row.id}-remove`}
                    onClick={() => onRemoveKey(row)}
                  >
                    <Trash2 className="size-3.5" />
                    Remove {row.keyNoun}
                  </Button>
                )}
              </span>
            )}
          </li>
        );
      })}
    </ul>
  );
}
