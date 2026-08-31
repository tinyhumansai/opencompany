import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AppWindow } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import type { PageManifestDto } from "@/api/types";
import { PageHeader } from "@/components/page-header";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";

interface Props {
  client: OpenCompanyClient;
  company: string | null;
}

type Load = "loading" | "ready" | "error";

/**
 * A bridge request from a page: `{type: "oc:graphql", id, query, variables}`
 * (docs/spec/runtime/pages.md §6, plan §6). The page's own `client.query`
 * (`frontend/pages-sdk/client.ts`) sends exactly this shape.
 */
interface GraphQLBridgeMessage {
  type: "oc:graphql";
  id: string;
  query: string;
  /** The per-document capability minted for the currently loaded iframe. */
  capability: string;
  variables?: Record<string, unknown>;
}

function isGraphQLBridgeMessage(value: unknown): value is GraphQLBridgeMessage {
  return (
    typeof value === "object" &&
    value !== null &&
    (value as { type?: unknown }).type === "oc:graphql" &&
    typeof (value as { id?: unknown }).id === "string" &&
    typeof (value as { query?: unknown }).query === "string" &&
    typeof (value as { capability?: unknown }).capability === "string"
  );
}

/**
 * Agent-authored internal dashboard pages, rendered in a sandboxed iframe.
 *
 * Each page is real React, compiled server-side and served at
 * `client.pageUrl(slug, company)` — a fixed HTML shell (not agent content)
 * that mounts the page's own compiled bundle inside a
 * `sandbox="allow-scripts"` iframe with no `allow-same-origin`. That sandbox
 * is the actual security boundary (docs/spec/runtime/pages.md §5): the
 * iframe holds no session cookie and can make no credentialed request of its
 * own, so live data reaches it only through the postMessage bridge this view
 * owns — every `oc:graphql` request the page sends is executed here, with
 * this console's own authenticated `client.graphqlRequest`, and the result is
 * posted back. Both queries and mutations are forwarded verbatim: the sandbox
 * protects the operator's *session*, not what an authorized request can *do*
 * once it crosses the bridge (see the plan's §6 for why this is deliberate).
 */
export function PagesView({ client, company }: Props) {
  const [load, setLoad] = useState<Load>("loading");
  const [error, setError] = useState<string | null>(null);
  const [pages, setPages] = useState<PageManifestDto[]>([]);
  const [activeSlug, setActiveSlug] = useState("");
  const iframeRef = useRef<HTMLIFrameElement>(null);
  // The per-document bridge capability, granted only to the initial document.
  const capabilityRef = useRef<string>("");
  // The document-bound half of the channel minted for that same document. The
  // bridge listens on this port — a navigated-to document never received the
  // other half, so it has no way to send through the bridge, which is what
  // makes the port the real credential and the capability string a backstop.
  const portRef = useRef<MessagePort | null>(null);
  const loadsRef = useRef(0);
  // The bridge handler, kept in a ref because the port it is attached to is
  // minted later (in `handleLoad`, on the iframe's `load` event) while the
  // handler needs `client` and the current page. The bridge effect below only
  // swaps what this ref points at.
  const bridgeHandlerRef = useRef<(event: MessageEvent) => void>(() => {});

  // Only nav-visible pages appear in the sidebar (`nav_visible = false` in
  // `page.toml` deliberately keeps one off the nav, reachable only by direct
  // URL). Alphabetical within, so the list order doesn't jump around as
  // pages are added.
  const visible = useMemo(
    () =>
      pages
        .filter((p) => p.navVisible !== false)
        .slice()
        .sort((a, b) => a.title.localeCompare(b.title)),
    [pages],
  );
  const active = visible.find((p) => p.slug === activeSlug) ?? visible[0];

  // Grant a capability only to the document the console asked the frame to
  // load. A self-navigation retains the same WindowProxy and sandbox flags,
  // so it cannot be distinguished by message source or origin; the second
  // load must revoke access rather than minting a token for its new occupant.
  const handleLoad = useCallback(() => {
    const frame = iframeRef.current;
    if (++loadsRef.current > 1) {
      capabilityRef.current = "";
      portRef.current?.close();
      portRef.current = null;
      return;
    }
    const cap =
      typeof globalThis.crypto?.randomUUID === "function"
        ? globalThis.crypto.randomUUID()
        : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
    // A fresh channel per document. port2 is transferred to the frame below,
    // and the bridge listens on port1 — only port2 can reach it. When the
    // document is destroyed (navigation), port2 dies with it, so a replacement
    // document's scripts have no port to post through even before this `load`
    // handler revokes the capability string.
    const channel = new MessageChannel();
    capabilityRef.current = cap;
    portRef.current = channel.port1;
    channel.port1.onmessage = (event) => bridgeHandlerRef.current(event);
    frame?.contentWindow?.postMessage({ type: "oc:init", capability: cap }, "*", [
      channel.port2,
    ]);
  }, []);

  // Changing the selected page — or switching to a different company, which
  // serves a different document at the same slug — creates a new iframe
  // document. Its first load is eligible for a capability; any later load in
  // that browsing context is a navigation and remains revoked.
  useEffect(() => {
    loadsRef.current = 0;
  }, [active?.slug, company]);

  const loadRun = useRef(0);
  const loadPages = useCallback(async () => {
    const run = ++loadRun.current;
    try {
      const rows = await client.listPages(company);
      if (run !== loadRun.current) return;
      setPages(rows);
      setActiveSlug((current) => (rows.some((p) => p.slug === current) ? current : (rows[0]?.slug ?? "")));
      setError(null);
      setLoad("ready");
    } catch (cause) {
      if (run !== loadRun.current) return;
      // No fixture fallback: a host that can't serve pages says so rather
      // than render an invented list.
      setPages([]);
      setError(cause instanceof Error ? cause.message : "Couldn't load pages.");
      setLoad("error");
    }
  }, [client, company]);

  useEffect(() => {
    setLoad("loading");
    void loadPages();
    return () => {
      loadRun.current += 1;
    };
  }, [loadPages]);

  // The bridge: forwards a page's GraphQL request to the console's own
  // authenticated endpoint and posts the answer back over the same port.
  // The handler lives in a ref ([`bridgeHandlerRef`]) because the port it is
  // attached to is minted later, in `handleLoad`, when the iframe's document
  // finishes loading.
  useEffect(() => {
    bridgeHandlerRef.current = (event: MessageEvent) => {
      // The actual authentication of "did this really come from my own
      // embedded page":
      //   * the port — only the entangled half the console transferred to
      //     exactly one iframe document can send a message here, and a
      //     document the page navigated itself to never received that half.
      //     This replaces the `event.source` / `event.origin` checks a window
      //     listener would need (both survive navigation; the port does not).
      //   * the per-document `capability` — granted only to the initial load,
      //     a redundant second layer on top of the port binding.
      if (!isGraphQLBridgeMessage(event.data)) return;
      if (!capabilityRef.current || event.data.capability !== capabilityRef.current) return;
      const { id, query, variables } = event.data;
      // Reply through the port that delivered this request, not through
      // `portRef.current` at settle time. A switch — to another page or
      // company — closes the old port and mints a fresh one while the request
      // is still in flight, and the stale response must not land on the newly
      // mounted document's port. Posting to a closed port is a silent no-op,
      // so a reply that settles after the switch simply goes nowhere.
      const replyPort = portRef.current;
      if (!replyPort) return;
      const reply = { type: "oc:graphql:result" as const, id };
      void client
        .graphqlRequest(query, variables)
        .then((result) => {
          replyPort.postMessage({ ...reply, data: result.data, errors: result.errors });
        })
        .catch((cause: unknown) => {
          replyPort.postMessage({
            ...reply,
            errors: [{ message: cause instanceof Error ? cause.message : "request failed" }],
          });
        });
    };
    return () => {
      // The selected page or company changed, or the view unmounted: the
      // iframe document this port was minted for is gone, so the port can
      // never legitimately be used again. Close it rather than leaving a live
      // channel into a document that no longer exists.
      portRef.current?.close();
      portRef.current = null;
      capabilityRef.current = "";
    };
  }, [client, active?.slug, company]);

  if (load === "loading") {
    return (
      <div className="flex flex-1 gap-2 p-4">
        <PageHeader hidden title="Pages" />
        <div className="w-64 shrink-0 space-y-2">
          {Array.from({ length: 4 }).map((_, i) => (
            <Skeleton key={i} className="h-9 rounded-lg" />
          ))}
        </div>
        <Skeleton className="flex-1 rounded-lg" />
      </div>
    );
  }

  if (load === "error") {
    return (
      <div className="flex flex-1 flex-col items-center justify-center gap-3 text-center text-muted-foreground">
        <PageHeader hidden title="Pages" />
        <AppWindow className="size-8" />
        <div className="space-y-1">
          <p className="font-medium text-foreground">Pages unavailable</p>
          <p className="max-w-sm text-sm">{error}</p>
        </div>
        <Button variant="outline" size="sm" onClick={() => void loadPages()}>
          Try again
        </Button>
      </div>
    );
  }

  if (visible.length === 0) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center gap-3 text-center text-muted-foreground">
        <PageHeader hidden title="Pages" />
        <AppWindow className="size-8" />
        <div className="space-y-1">
          <p className="font-medium text-foreground">No pages yet</p>
          <p className="max-w-sm text-sm">
            Ask the <span className="font-medium">Page Builder</span> to design one — a metrics
            view, a pipeline board, a status page — and it shows up here.
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex flex-1 overflow-hidden">
      {/* Each page's own title lives inside its sandboxed iframe, a separate
          document this console doesn't control — this names the console-side
          page for a screen reader (issue #1221). */}
      <PageHeader hidden title="Pages" />
      <section className="hidden w-64 shrink-0 flex-col overflow-y-auto border-r py-2 md:flex" data-testid="pages-list">
        {visible.map((page) => (
          <button
            key={page.slug}
            onClick={() => setActiveSlug(page.slug)}
            data-testid="pages-list-item"
            className={cn(
              "flex flex-col items-start gap-0.5 px-3 py-2 text-left text-sm transition-colors",
              page.slug === active?.slug ? "bg-accent text-accent-foreground" : "hover:bg-accent/50",
            )}
          >
            <span className="truncate font-medium">{page.title || page.slug}</span>
            {page.description && (
              <span className="truncate text-xs text-muted-foreground">{page.description}</span>
            )}
          </button>
        ))}
      </section>
      <section className="flex flex-1 flex-col overflow-hidden">
        {active ? (
          <iframe
            // The key is the iframe document's complete identity: a distinct
            // page, or the same slug under a different company, is a distinct
            // document and must remount so its first load is granted a fresh
            // bridge rather than treated as a navigation of the old one.
            key={`${company ?? ""}:${active.slug}`}
            ref={iframeRef}
            onLoad={handleLoad}
            sandbox="allow-scripts"
            src={client.pageUrl(active.slug, company)}
            title={active.title || active.slug}
            style={{ width: "100%", height: "100%", border: "none" }}
          />
        ) : (
          <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
            Select a page.
          </div>
        )}
      </section>
    </div>
  );
}
