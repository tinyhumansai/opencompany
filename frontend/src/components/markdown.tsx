import { cloneElement, isValidElement, type ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import { cn } from "@/lib/utils";
import { mentionRegex } from "@/views/chat/mentions";

/** One mention span this document should chip, as `chat/history` returns it. */
export interface MentionSpan {
  text: string;
  /** UTF-8 byte offset in the raw Markdown message, when supplied by the host. */
  offset?: number;
  label: string;
  mine: boolean;
  quiet?: boolean;
}

/**
 * Wraps every delivered mention span in `nodes` in a chip.
 *
 * Splits string children **and recurses into element children**, so a mention
 * inside `**bold**`, emphasis, or a link still chips while the surrounding
 * markup is left to `react-markdown`.
 *
 * The pattern comes from {@link mentionRegex}, which matches nothing when the
 * list is empty. That is the rule, not an optimisation: an `@word` is
 * highlighted only when the host actually delivered a mention for it. A chip
 * claims somebody was notified, and one drawn over unresolved text is a claim
 * the reader has no way to check.
 *
 * Mentions are consumed **per occurrence**, not per literal: two `@engineer`
 * spans with different metadata (say the second is a quiet duplicate) each
 * render with their own record, and a third, unresolved `@engineer` elsewhere
 * in the text renders plain.
 */
function chipMentions(
  nodes: ReactNode,
  mentions: MentionSpan[],
  source: string,
  seen: Map<string, number>,
): ReactNode {
  if (mentions.length === 0) return nodes;
  const pattern = mentionRegex(mentions);
  // Offsets are UTF-8 bytes in the host DTO, while source is indexed in UTF-16.
  // Convert each offset to the source index and record its occurrence ordinal;
  // this prevents an unresolved earlier `@name` from consuming a later chip.
  const occurrences = new Map<string, Map<number, MentionSpan>>();
  for (const [fallbackOrdinal, m] of mentions.entries()) {
    let bytes = 0;
    let index = 0;
    while (m.offset !== undefined && index < source.length && bytes < m.offset) {
      const point = source.codePointAt(index)!;
      const ch = String.fromCodePoint(point);
      bytes += new TextEncoder().encode(ch).length;
      index += ch.length;
    }
    const ordinal =
      m.offset === undefined
        ? fallbackOrdinal
        : source.slice(0, index).split(m.text).length - 1;
    const byOrdinal = occurrences.get(m.text) ?? new Map<number, MentionSpan>();
    byOrdinal.set(ordinal, m);
    occurrences.set(m.text, byOrdinal);
  }
  const split = (node: ReactNode, key: string): ReactNode => {
    if (typeof node === "string") {
      const parts = node.split(pattern);
      if (parts.length === 1) return node;
      return parts.map((part, i) => {
        const ordinal = seen.get(part) ?? 0;
        if (part !== "") seen.set(part, ordinal + 1);
        const hit = part === "" ? undefined : occurrences.get(part)?.get(ordinal);
        if (!hit) return part;
        return (
          <span
            key={`${key}-${i}`}
            // `mine` is the reader's own mention — including `@everyone`, which is
            // addressed to them too. It reads as an alert; somebody else's mention
            // is only a reference, so it stays quiet.
            className={cn(
              "rounded px-1 py-0.5 font-medium",
              hit.mine
                ? "bg-primary/15 text-primary"
                : "bg-muted text-foreground/80",
            )}
            // A quiet mention rendered but pinged nobody — a duplicate, one past
            // the cap, or a target that has since left. Saying so is better than
            // a chip that silently means something different from its neighbour.
            title={hit.quiet ? `${hit.label} — mentioned, not notified` : hit.label}
          >
            {part}
          </span>
        );
      });
    }
    if (Array.isArray(node)) return node.map((child, i) => split(child, `${key}-${i}`));
    if (isValidElement(node)) {
      // A formatted mention arrives as an element (strong, em, a…). Descend so
      // `**@engineer**` chips too — the host notified the target, so the chip
      // belongs regardless of the markup around it.
      return cloneElement(node, undefined, split(node.props.children, key));
    }
    return node;
  };

  return split(nodes, "0");
}

/**
 * Is `href` a link that leaves the console?
 *
 * Absolute `http://`, `https://` and protocol-relative `//host/…` targets are
 * external. Everything else — `#/chat`, `/workspace`, `./note.md`, `mailto:`,
 * and an empty or missing href — is not, and must keep opening in place:
 * in-app navigation in a new tab would be worse, not better.
 *
 * Exported for the unit suite: the decision is a pure string function, so it
 * is checked there rather than through a rendered document.
 */
export function isExternalHref(href?: string): boolean {
  const target = (href ?? "").trim();
  return /^(https?:)?\/\//i.test(target);
}

/**
 * Shared markdown renderer used across chat, memory, workspace, and workflow
 * surfaces so `**bold**`, lists, links, and inline code render consistently
 * instead of leaking literal markup. Wraps `react-markdown` + `remark-gfm` in a
 * `prose` container and honors both themes via `dark:prose-invert`.
 *
 * Mirrors the recipe already used by WorkspaceView (NoteMarkdown) and
 * WorkflowsView so every surface shares one renderer and one look.
 *
 * External links open in a new tab. This renderer is shared by chat, memory,
 * workspace and workflows, and in all four the surrounding view is a working
 * context with state that is expensive to lose: following a citation used to
 * navigate the console away mid-conversation, and coming back meant a reload
 * and a scroll hunt. `rel="noreferrer noopener"` goes with it — without
 * `noopener` the opened page can reach this one through `window.opener`.
 */
export function Markdown({
  children,
  className,
  mentions,
}: {
  children: string;
  className?: string;
  /**
   * Mention spans to chip, when this document is a chat message that carries
   * any. Omitted everywhere else — memory, workspace, workflows — so those
   * surfaces render exactly as they did.
   */
  mentions?: MentionSpan[];
}) {
  const spans = mentions ?? [];
  const seen = new Map<string, number>();
  const sourceText = children;
  return (
    <div className={cn("prose prose-sm max-w-none dark:prose-invert", className)}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        components={{
          a: ({ href, children: content, ...rest }) => (
            <a
              href={href}
              {...(isExternalHref(href) ? { target: "_blank", rel: "noreferrer noopener" } : {})}
              {...rest}
            >
              {content}
            </a>
          ),
          // Chips are injected at the leaf elements that hold prose rather than
          // through a remark plugin, so the mention list stays a plain prop and
          // no AST transform has to be kept in step with it. Every Markdown
          // block that can contain phrasing content uses the same transform;
          // headings and GFM table cells must not lose resolved mentions.
          h1: ({ children: content, ...rest }) => (
            <h1 {...rest}>{chipMentions(content, spans, sourceText, seen)}</h1>
          ),
          h2: ({ children: content, ...rest }) => (
            <h2 {...rest}>{chipMentions(content, spans, sourceText, seen)}</h2>
          ),
          h3: ({ children: content, ...rest }) => (
            <h3 {...rest}>{chipMentions(content, spans, sourceText, seen)}</h3>
          ),
          h4: ({ children: content, ...rest }) => (
            <h4 {...rest}>{chipMentions(content, spans, sourceText, seen)}</h4>
          ),
          h5: ({ children: content, ...rest }) => (
            <h5 {...rest}>{chipMentions(content, spans, sourceText, seen)}</h5>
          ),
          h6: ({ children: content, ...rest }) => (
            <h6 {...rest}>{chipMentions(content, spans, sourceText, seen)}</h6>
          ),
          p: ({ children: content, ...rest }) => (
            <p {...rest}>{chipMentions(content, spans, sourceText, seen)}</p>
          ),
          td: ({ children: content, ...rest }) => (
            <td {...rest}>{chipMentions(content, spans, sourceText, seen)}</td>
          ),
          th: ({ children: content, ...rest }) => (
            <th {...rest}>{chipMentions(content, spans, sourceText, seen)}</th>
          ),
          li: ({ children: content, ...rest }) => {
            // Loose lists contain a nested paragraph. Leave that paragraph to
            // the `p` handler instead of traversing it twice; tight-list text
            // still needs to be transformed here.
            const childrenToRender = Array.isArray(content)
              ? content.map((child) =>
                  isValidElement(child) && child.type === "p"
                    ? child
                    : chipMentions(child, spans, sourceText, seen),
                )
              : isValidElement(content) && content.type === "p"
                ? content
                : chipMentions(content, spans, sourceText, seen);
            return <li {...rest}>{childrenToRender}</li>;
          },
        }}
      >
        {children}
      </ReactMarkdown>
    </div>
  );
}
