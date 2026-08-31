import { useEffect, useRef } from "react";
import { Hash, Users } from "lucide-react";

import { TeammateAvatar } from "@/components/teammate-avatar";
import { cn } from "@/lib/utils";
import type { Mentionable } from "@/views/chat/mentions";

/**
 * The `@` picker, anchored above the composer.
 *
 * Positioned relative to the composer box rather than to the caret. A textarea
 * exposes no caret rectangle, so anchoring to one means mirroring the whole
 * field into a hidden clone to measure it — which has to track font, padding,
 * wrapping and scroll, and is wrong the moment any of them change. The composer
 * is a narrow box at the bottom of the pane, so "above the box" and "above the
 * caret" differ by a few pixels and only the first is reliable. The composer's
 * own formatting toolbar already sets this precedent.
 *
 * Selection is owned by the composer, not by this component: the keys that move
 * it (`ArrowUp`/`ArrowDown`/`Tab`/`Enter`) arrive at the textarea, and a picker
 * holding its own index would have to be told about every one of them anyway.
 */
export function MentionPicker({
  entries,
  active,
  onPick,
  onHover,
}: {
  entries: Mentionable[];
  /** Index into `entries` of the highlighted row. */
  active: number;
  onPick: (entry: Mentionable) => void;
  onHover: (index: number) => void;
}) {
  const list = useRef<HTMLDivElement>(null);

  // Keep the highlighted row in view as the arrows walk past the fold.
  // `block: "nearest"` so an already-visible row does not jump the list.
  useEffect(() => {
    const el = list.current?.querySelector<HTMLElement>(`#mention-option-${active}`);
    el?.scrollIntoView({ block: "nearest" });
  }, [active]);

  if (entries.length === 0) return null;

  return (
    <div
      ref={list}
      // Not a listbox/combobox: focus stays in the textarea the whole time, so
      // the aria pattern that fits is a controlled list the input points at.
      id="mention-picker"
      role="listbox"
      aria-label="Mention someone"
      data-testid="mention-picker"
      className="absolute bottom-full left-0 z-20 mb-2 max-h-64 w-72 overflow-y-auto rounded-xl border bg-popover p-1 shadow-lg"
    >
      {entries.map((entry, index) => (
        <button
          key={`${entry.target.kind}:${"id" in entry.target ? entry.target.id : "everyone"}`}
          type="button"
          role="option"
          id={`mention-option-${index}`}
          aria-selected={index === active}
          data-testid="mention-option"
          // `onMouseDown` with `preventDefault`, not `onClick`: a click would
          // blur the textarea first, and the composer would lose the selection
          // range the insert needs.
          onMouseDown={(e) => {
            e.preventDefault();
            onPick(entry);
          }}
          onMouseEnter={() => onHover(index)}
          className={cn(
            "flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left text-sm",
            index === active ? "bg-accent text-accent-foreground" : "hover:bg-accent/50",
          )}
        >
          <RowIcon entry={entry} />
          <span className="min-w-0 flex-1">
            <span className="block truncate font-medium">{entry.label}</span>
            {entry.hint && (
              <span className="block truncate text-xs text-muted-foreground">
                {entry.hint}
              </span>
            )}
          </span>
        </button>
      ))}
    </div>
  );
}

function RowIcon({ entry }: { entry: Mentionable }) {
  if (entry.target.kind === "agent" || entry.target.kind === "user") {
    return (
      <TeammateAvatar
        name={entry.label}
        avatar={entry.avatar}
        className="size-6 shrink-0"
      />
    );
  }
  const Icon = entry.target.kind === "desk" ? Hash : Users;
  return (
    <span className="flex size-6 shrink-0 items-center justify-center rounded-md bg-muted">
      <Icon className="size-3.5 text-muted-foreground" aria-hidden />
    </span>
  );
}
