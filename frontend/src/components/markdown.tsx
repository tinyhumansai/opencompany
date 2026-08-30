import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import { cn } from "@/lib/utils";

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
}: {
  children: string;
  className?: string;
}) {
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
        }}
      >
        {children}
      </ReactMarkdown>
    </div>
  );
}
