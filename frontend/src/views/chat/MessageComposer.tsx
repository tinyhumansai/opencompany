import { useRef, useState } from "react";
import {
  AtSign,
  Bold,
  Code,
  Italic,
  Link2,
  List,
  Paperclip,
  SendHorizontal,
  Strikethrough,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

interface Props {
  placeholder: string;
  disabled?: boolean;
  onSend: (text: string) => void;
  /** Compact form, for the narrower thread panel. */
  compact?: boolean;
}

/** The markdown a toolbar button wraps the selection in. */
const WRAPS = [
  { icon: Bold, label: "Bold", mark: "**" },
  { icon: Italic, label: "Italic", mark: "_" },
  { icon: Strikethrough, label: "Strikethrough", mark: "~~" },
  { icon: Code, label: "Code", mark: "`" },
] as const;

/**
 * The composer dock.
 *
 * A bordered box that owns its own toolbar rather than a bare input: the
 * formatting buttons wrap the current selection in markdown, and the box grows
 * with the draft up to a cap before scrolling. Enter sends; Shift+Enter breaks
 * the line, which is the convention every chat client shares.
 */
export function MessageComposer({ placeholder, disabled, onSend, compact }: Props) {
  const [draft, setDraft] = useState("");
  const input = useRef<HTMLTextAreaElement>(null);

  function send() {
    const text = draft.trim();
    if (!text || disabled) return;
    setDraft("");
    onSend(text);
  }

  function onKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  }

  /** Wrap the selection (or the caret) in `mark`, keeping focus in the box. */
  function wrap(mark: string) {
    const el = input.current;
    if (!el) return;
    const { selectionStart: start, selectionEnd: end } = el;
    const next = `${draft.slice(0, start)}${mark}${draft.slice(start, end)}${mark}${draft.slice(end)}`;
    setDraft(next);
    // Restore the selection around what was wrapped, after React repaints.
    requestAnimationFrame(() => {
      el.focus();
      el.setSelectionRange(start + mark.length, end + mark.length);
    });
  }

  return (
    <div
      className={cn("shrink-0 px-4", compact ? "pb-3" : "pb-4")}
      // The guided tour spotlights the channel composer. The thread panel's
      // compact copy stays unlabelled so the tour can't anchor on the wrong one.
      data-tour={compact ? undefined : "chat-composer"}
    >
      <div className="rounded-xl border bg-card shadow-sm focus-within:ring-2 focus-within:ring-ring/40">
        {!compact && (
          <div className="flex items-center gap-0.5 border-b px-2 py-1">
            {WRAPS.map((w) => (
              <Button
                key={w.label}
                variant="ghost"
                size="icon"
                className="size-7 text-muted-foreground"
                onClick={() => wrap(w.mark)}
                aria-label={w.label}
                title={w.label}
              >
                <w.icon className="size-3.5" />
              </Button>
            ))}
            <span className="mx-1 h-4 w-px bg-border" aria-hidden />
            <Button
              variant="ghost"
              size="icon"
              className="size-7 text-muted-foreground"
              aria-label="Bulleted list"
              title="Bulleted list"
              disabled
            >
              <List className="size-3.5" />
            </Button>
            <Button
              variant="ghost"
              size="icon"
              className="size-7 text-muted-foreground"
              aria-label="Link"
              title="Link"
              disabled
            >
              <Link2 className="size-3.5" />
            </Button>
          </div>
        )}

        {/* A native textarea rather than the design-system one: the composer
            needs a ref to wrap the selection, and `Textarea` is a plain
            function component (React 18 — no ref forwarding). */}
        <textarea
          ref={input}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={onKeyDown}
          placeholder={placeholder}
          rows={1}
          className="field-sizing-content max-h-48 min-h-10 w-full resize-none bg-transparent px-3 py-2 text-sm outline-none placeholder:text-muted-foreground"
        />

        <div className="flex items-center gap-0.5 px-2 pb-1.5">
          <Button
            variant="ghost"
            size="icon"
            className="size-7 text-muted-foreground"
            aria-label="Attach a file"
            title="Attach a file"
            disabled
          >
            <Paperclip className="size-4" />
          </Button>
          <Button
            variant="ghost"
            size="icon"
            className="size-7 text-muted-foreground"
            aria-label="Mention someone"
            title="Mention someone"
            onClick={() => wrap("@")}
          >
            <AtSign className="size-4" />
          </Button>
          <Button
            size="icon"
            className="ml-auto size-7 rounded-lg"
            onClick={send}
            disabled={disabled || !draft.trim()}
            aria-label="Send"
          >
            <SendHorizontal className="size-4" />
          </Button>
        </div>
      </div>
      {!compact && (
        <p className="mt-1.5 px-1 text-[11px] text-muted-foreground">
          <kbd className="font-sans font-medium">Enter</kbd> to send ·{" "}
          <kbd className="font-sans font-medium">Shift+Enter</kbd> for a new line
        </p>
      )}
    </div>
  );
}
