// The search modal: a field, a title, and the results under it.
//
// Everything it decides is about *presentation and keyboard*. What counts as a
// hit is `sources.ts`, how hits are ordered is `rank.ts`, what was typed is
// `query.ts`, and what has to be fetched is `useSearch.ts` — all of which are
// testable without rendering this.
//
// ## Two behaviours worth naming
//
// **Escape unwinds rather than slams.** With a scope open, the first Escape
// drops the scope and leaves the modal up; only a second closes it. Losing a
// whole query because you wanted to widen it is the thing that makes people
// stop using a palette.
//
// **Enter says what it will do before it does it.** `#engineering` navigates and
// `#engineering box` searches inside it, and the active row spells out which —
// see `SearchResult.action`.

import { useEffect, useMemo, useRef, useState } from "react";
import { Loader2, Search } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { Dialog, DialogContent, DialogTitle } from "@/components/ui/dialog";
import { parseSearchQuery, SCOPE_PREFIX, withScopeName } from "./query";
import { SearchResultRow } from "./SearchResultRow";
import type { SearchResult } from "./types";
import { useSearch } from "./useSearch";

export function SearchDialog({
  client,
  company,
  open,
  onOpenChange,
}: {
  client: OpenCompanyClient;
  company: string | null;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const [raw, setRaw] = useState("");
  const [cursor, setCursor] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);

  const query = useMemo(() => parseSearchQuery(raw), [raw]);
  const { groups, loading } = useSearch(client, company, query, open);

  /** Every result in draw order, which is what the cursor walks. */
  const flat = useMemo(() => groups.flatMap((group) => group.results), [groups]);

  // A cursor past the end of a shorter list would point at nothing and make
  // Enter do nothing, silently.
  useEffect(() => {
    setCursor((at) => (at < flat.length ? at : 0));
  }, [flat.length]);

  // Reopening starts clean. Keeping the last query would answer the previous
  // question to somebody who has just asked a new one.
  useEffect(() => {
    if (!open) {
      setRaw("");
      setCursor(0);
    }
  }, [open]);

  const activate = (result: SearchResult | undefined) => {
    if (!result) return;
    // A picked agent or channel with no term yet is a *choice*, not a
    // destination: it fills the scope in and waits for what to search for.
    if (query.scope && !query.term && (result.kind === "agent" || result.kind === "channel")) {
      // `scopeName`, not the row's label: a display name with a space in it —
      // "User Researcher" — written back as `@User Researcher ` re-parses as
      // the scope `user` and the term `researcher`, and searches somebody
      // else's DM for a word nobody typed.
      const name = result.scopeName ?? result.title.replace(/^#/, "");
      setRaw(withScopeName(query, name));
      inputRef.current?.focus();
      return;
    }
    onOpenChange(false);
    window.location.hash = result.href.replace(/^#/, "");
  };

  const onKeyDown = (event: React.KeyboardEvent<HTMLInputElement>) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      setCursor((at) => (flat.length === 0 ? 0 : (at + 1) % flat.length));
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      setCursor((at) => (flat.length === 0 ? 0 : (at - 1 + flat.length) % flat.length));
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      activate(flat[cursor]);
      return;
    }
    if (event.key === "Escape" && query.scope) {
      // Unwind one level rather than closing: the scope goes, the modal stays.
      event.preventDefault();
      event.stopPropagation();
      setRaw(query.term);
      return;
    }
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        // `sm:max-w-2xl`, never a bare `max-w-2xl`: `DialogContent`'s own base
        // carries `sm:max-w-sm`, which is a different *variant* and so survives
        // the merge and wins on every screen ≥640px. The bare class is present
        // in the DOM and simply loses, at 384px, silently
        // (`dialog-width-override.test.ts`).
        className="top-24 translate-y-0 gap-0 overflow-hidden p-0 sm:max-w-2xl"
        data-testid="search-dialog"
      >
        <div className="flex items-center gap-2.5 border-b px-3.5 py-3">
          <Search aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
          <DialogTitle className="sr-only">Search this company</DialogTitle>
          <input
            ref={inputRef}
            autoFocus
            value={raw}
            onChange={(event) => setRaw(event.target.value)}
            onKeyDown={onKeyDown}
            placeholder="Search channels, agents, messages and files"
            aria-label="Search this company"
            data-testid="search-input"
            className="min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-muted-foreground"
          />
          {loading && <Loader2 aria-hidden="true" className="size-4 shrink-0 animate-spin text-muted-foreground" />}
        </div>

        <div
          role="listbox"
          aria-label="Search results"
          className="max-h-[min(60vh,26rem)] overflow-y-auto p-1.5"
        >
          {groups.length === 0 ? (
            <Resting query={query.raw} isEmpty={query.isEmpty} loading={loading} />
          ) : (
            groups.map((group) => (
              <div key={group.kind} className="pb-1">
                <p className="px-2.5 pt-2 pb-1 text-2xs font-medium uppercase tracking-wide text-muted-foreground">
                  {group.label}
                </p>
                {group.results.map((result) => (
                  <SearchResultRow
                    key={result.id}
                    result={result}
                    active={flat[cursor]?.id === result.id}
                    onActivate={() => activate(result)}
                    onHover={() => setCursor(flat.findIndex((r) => r.id === result.id))}
                  />
                ))}
              </div>
            ))
          )}
        </div>

        <div className="flex items-center gap-3 border-t px-3.5 py-2 text-2xs text-muted-foreground">
          <span>
            <kbd className="font-sans font-medium">{SCOPE_PREFIX.person}</kbd> agents
          </span>
          <span>
            <kbd className="font-sans font-medium">{SCOPE_PREFIX.channel}</kbd> channels
          </span>
          <span>
            <kbd className="font-sans font-medium">{SCOPE_PREFIX.file}</kbd> files
          </span>
          <span className="ml-auto">↑↓ to move · ↵ to open · esc to close</span>
        </div>
      </DialogContent>
    </Dialog>
  );
}

/**
 * What the list says when it has nothing to list.
 *
 * Three different silences, and they are not interchangeable: nothing typed,
 * still looking, and genuinely nothing there. A single "No results" for all
 * three tells an operator their search failed when it has not run yet.
 */
function Resting({ query, isEmpty, loading }: { query: string; isEmpty: boolean; loading: boolean }) {
  if (isEmpty) {
    return (
      <p className="px-2.5 py-6 text-center text-sm text-muted-foreground" data-testid="search-resting">
        Search this company — or start with{" "}
        <kbd className="font-sans font-medium text-foreground">@</kbd> for an agent and{" "}
        <kbd className="font-sans font-medium text-foreground">#</kbd> for a channel.
      </p>
    );
  }
  if (loading) {
    return (
      <p className="px-2.5 py-6 text-center text-sm text-muted-foreground" data-testid="search-looking">
        Looking…
      </p>
    );
  }
  return (
    <p className="px-2.5 py-6 text-center text-sm text-muted-foreground" data-testid="search-empty">
      Nothing matched “{query.trim()}”.
    </p>
  );
}
