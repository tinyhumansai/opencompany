import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import { cn } from "@/lib/utils";

/**
 * Shared markdown renderer used across chat, memory, workspace, and workflow
 * surfaces so `**bold**`, lists, links, and inline code render consistently
 * instead of leaking literal markup. Wraps `react-markdown` + `remark-gfm` in a
 * `prose` container and honors both themes via `dark:prose-invert`.
 *
 * Mirrors the recipe already used by WorkspaceView (NoteMarkdown) and
 * WorkflowsView so every surface shares one renderer and one look.
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
      <ReactMarkdown remarkPlugins={[remarkGfm]}>{children}</ReactMarkdown>
    </div>
  );
}
