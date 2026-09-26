import {
  createContext,
  lazy,
  Suspense,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import { Pencil, Sparkles, Users, Wrench } from "lucide-react";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError, type AgentDetailDto } from "@/api/types";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { Skeleton } from "@/components/ui/skeleton";
import { agentHref, agentProfile } from "@/lib/agent-profile";
import { isMascotRef } from "@/lib/avatar";
import { cn } from "@/lib/utils";

/**
 * The Rive runtime + the mascot asset are ~1.8 MB combined and load only for
 * the rare teammate wearing `mascot:animated` — code-split the same way this
 * codebase already isolates `recharts`/`@xyflow/react`/`react-joyride`, so
 * every other profile sheet pays nothing for it.
 */
const LazyMascotAvatar = lazy(() =>
  import("@/components/mascot-avatar").then((m) => ({ default: m.MascotAvatar })),
);

/**
 * The profile sheet's header face: the live mascot for a teammate who chose
 * it, the ordinary static tile for everyone else.
 *
 * `TeammateAvatar` already falls back to the tone tile while an image loads,
 * so the `Suspense` fallback here matches that same tile-shaped `Skeleton`
 * rather than a generic spinner — the header must not jump size while the
 * mascot's chunk is in flight.
 */
function AgentAvatar({
  name,
  tone,
  avatar,
  mascotMode,
  mascotCostume,
  mascotSkinColor,
  mascotHandColor,
}: {
  name: string;
  tone: string;
  avatar: string;
  mascotMode?: string;
  mascotCostume?: string;
  mascotSkinColor?: string;
  mascotHandColor?: string;
}) {
  // Only the mascot branch needs this — a static tile has no state to track,
  // and hooks cannot sit behind the early return below, so it is declared
  // unconditionally like `AvatarTile`'s own hook in `teammate-avatar.tsx`.
  const [hovering, setHovering] = useState(false);
  if (isMascotRef(avatar)) {
    // Static means static: the handlers that make the header react to a
    // pointer are never attached in the first place, not merely fed a state
    // `MascotAvatar` then ignores — see that component's own module docs.
    if ((mascotMode ?? "animated") === "static") {
      return (
        // The `Skeleton` sits behind, not just in the `Suspense` fallback:
        // `MascotAvatar` stays transparent past the chunk load, through its
        // own `.riv` fetch (~1.7 MB, a further second or two), so a fallback
        // that only covers the chunk would still hand off to a blank header
        // for that whole gap — which is exactly what read as broken (issue
        // found live 2026-09-26, "no loading indicator").
        <div className="relative size-12">
          <Skeleton className="absolute inset-0 rounded-xl" />
          <Suspense fallback={null}>
            <LazyMascotAvatar
              mode="static"
              costume={mascotCostume}
              skinColor={mascotSkinColor}
              handColor={mascotHandColor}
              className="absolute inset-0"
              data-testid="agent-profile-avatar"
            />
          </Suspense>
        </div>
      );
    }
    return (
      <span
        onMouseEnter={() => setHovering(true)}
        onMouseLeave={() => setHovering(false)}
      >
        <div className="relative size-12">
          <Skeleton className="absolute inset-0 rounded-xl" />
          <Suspense fallback={null}>
            <LazyMascotAvatar
              mode="animated"
              state={hovering ? "hover" : "idle"}
              costume={mascotCostume}
              skinColor={mascotSkinColor}
              handColor={mascotHandColor}
              className="absolute inset-0"
              data-testid="agent-profile-avatar"
            />
          </Suspense>
        </div>
      </span>
    );
  }
  return (
    <TeammateAvatar
      name={name}
      tone={tone}
      avatar={avatar}
      className="size-12 rounded-xl text-sm"
      data-testid="agent-profile-avatar"
    />
  );
}

/** What a click on a teammate's face can do, from anywhere under the provider. */
interface AgentProfileApi {
  /** Open the panel on this roster agent id. */
  open: (agentId: string) => void;
  close: () => void;
}

const AgentProfileContext = createContext<AgentProfileApi | null>(null);

/**
 * The opener, or `null` where no provider is mounted.
 *
 * Null rather than a no-op on purpose: a caller is expected to render a plain
 * avatar when this returns null, not a button that looks clickable and does
 * nothing. The styleguide and the unit-rendered pieces of chat both mount
 * outside the shell, and a dead control there would be a lie about the surface
 * rather than a missing convenience.
 */
export function useAgentProfileOpener(): ((agentId: string) => void) | null {
  return useContext(AgentProfileContext)?.open ?? null;
}

/**
 * A teammate's face, clickable where there is a profile behind it.
 *
 * Falls back to the bare avatar — not a dead button — when the voice has no
 * roster id (a desk, the company, "you") or when no provider is mounted. That
 * distinction is the whole reason this wrapper exists rather than every surface
 * writing its own `<button>`: a face that looks pressable and is not is worse
 * than one that never claimed to be.
 */
export function AgentAvatarButton({
  agentId,
  name,
  className,
  children,
}: {
  agentId?: string;
  /** For the control's accessible name — the face itself is `aria-hidden`. */
  name: string;
  className?: string;
  /** The avatar to draw, so each surface keeps its own sizing and variant. */
  children: ReactNode;
}) {
  const open = useAgentProfileOpener();
  if (!agentId || !open) return <>{children}</>;
  return (
    <button
      type="button"
      onClick={() => open(agentId)}
      // `rounded-md` matches the tile it wraps, so the focus ring follows the
      // avatar's own corners rather than boxing it.
      className={cn(
        "rounded-md transition-opacity hover:opacity-80 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring",
        className,
      )}
      aria-label={`Open ${name}'s profile`}
      data-testid="agent-profile-open-avatar"
    >
      {children}
    </button>
  );
}

/**
 * Clicking a teammate opens who they are, beside what you were reading.
 *
 * Before this, a face in a transcript or a member list was inert: the only way
 * to find out what an agent is — its persona, its tools, the desks it sits on —
 * was to leave the conversation for `#/team/<id>` and find your way back. The
 * panel answers the question in place and keeps the full page one button away,
 * which is the right split: a summary is what a click on an avatar is asking
 * for, and editing is a page's worth of controls.
 *
 * Mounted once, near the root, rather than per surface. Every avatar in the
 * console would otherwise need the client, the company scope and a piece of
 * open state threaded to it, and each surface would answer the same question in
 * its own slightly different words.
 */
export function AgentProfileProvider({
  client,
  company,
  children,
}: {
  client: OpenCompanyClient;
  company: string | null;
  children: ReactNode;
}) {
  const [agentId, setAgentId] = useState<string | null>(null);

  const api = useMemo<AgentProfileApi>(
    () => ({ open: (id: string) => setAgentId(id), close: () => setAgentId(null) }),
    [],
  );

  // The panel's own actions are hash links, and so are the desk badges inside
  // it. A navigation is the operator saying "take me there" — leaving the panel
  // floating over the page they asked for would make them close it first.
  useEffect(() => {
    const onHashChange = () => setAgentId(null);
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  return (
    <AgentProfileContext.Provider value={api}>
      {children}
      <AgentProfileSheet
        client={client}
        company={company}
        agentId={agentId}
        onClose={() => setAgentId(null)}
      />
    </AgentProfileContext.Provider>
  );
}

type Load = "loading" | "ready" | "unavailable" | "error";

/**
 * The panel itself.
 *
 * Exported for the styleguide and for a surface that wants one without the
 * context; ordinary callers open it through {@link useAgentProfileOpener}.
 */
export function AgentProfileSheet({
  client,
  company,
  agentId,
  onClose,
}: {
  client: OpenCompanyClient;
  company: string | null;
  agentId: string | null;
  onClose: () => void;
}) {
  const [load, setLoad] = useState<Load>("loading");
  const [agent, setAgent] = useState<AgentDetailDto | null>(null);

  useEffect(() => {
    if (!agentId) return;
    let live = true;
    setLoad("loading");
    setAgent(null);
    void (async () => {
      try {
        const detail = await client.getAgent(agentId, company);
        if (!live) return;
        setAgent(detail);
        setLoad("ready");
      } catch (error) {
        if (!live) return;
        // A `404` here is two facts at once — a host with no detail route, or a
        // teammate that is gone — and this panel has no roster to tell them
        // apart the way the detail page does. So it says both, rather than
        // picking one and sending the operator after a deletion that may never
        // have happened.
        setLoad(error instanceof ApiError && error.status === 404 ? "unavailable" : "error");
      }
    })();
    return () => {
      live = false;
    };
  }, [client, company, agentId]);

  return (
    <Sheet open={agentId !== null} onOpenChange={(next) => !next && onClose()}>
      <SheetContent
        side="right"
        className="w-full overflow-y-auto sm:max-w-sm"
        data-testid="agent-profile-panel"
      >
        {load === "loading" && <ProfileSkeleton />}
        {load === "unavailable" && (
          <Message
            title="Can't open this agent."
            body="Either they've been removed from the roster, or this company host is too old to serve an agent's profile."
          />
        )}
        {load === "error" && (
          <Message
            title="Couldn't load this agent."
            body="The company host didn't answer. Try again in a moment."
          />
        )}
        {load === "ready" && agent && <ProfileBody agent={agent} />}
      </SheetContent>
    </Sheet>
  );
}

function ProfileBody({ agent }: { agent: AgentDetailDto }) {
  const profile = agentProfile(agent);
  // The host's own `editable` list decides whether editing is offered, exactly
  // as it does on the detail page. An empty list means this host does not
  // support the edit at all — a current one offers name, role and instructions
  // on every teammate, blueprint ones included.
  const editable = agent.editable.length > 0;

  return (
    <>
      <SheetHeader className="gap-3 pr-10">
        <div className="flex items-start gap-3">
          <AgentAvatar
            name={profile.display}
            tone={profile.tone}
            avatar={profile.avatar}
            mascotMode={agent.mascotMode}
            mascotCostume={agent.mascotCostume}
            mascotSkinColor={agent.mascotSkinColor}
            mascotHandColor={agent.mascotHandColor}
          />
          <div className="min-w-0 flex-1">
            <SheetTitle className="truncate text-lg" data-testid="agent-profile-name">
              {profile.display}
            </SheetTitle>
            {profile.subtitle && (
              <SheetDescription className="truncate" data-testid="agent-profile-role">
                {profile.subtitle}
              </SheetDescription>
            )}
          </div>
        </div>
        <div className="flex flex-wrap items-center gap-1.5">
          <Badge variant="secondary" className="gap-1" data-testid="agent-profile-tier">
            <Sparkles className="size-3" aria-hidden /> {profile.tier}
          </Badge>
          <Badge variant="outline">{profile.origin}</Badge>
        </div>
      </SheetHeader>

      <div className="space-y-5 px-4">
        <Section title="What they do">
          {profile.about ? (
            <p
              className="whitespace-pre-wrap text-sm text-muted-foreground"
              data-testid="agent-profile-about"
            >
              {profile.about}
            </p>
          ) : (
            <p className="text-sm text-muted-foreground">
              No instructions have been written for this agent yet.
            </p>
          )}
          {profile.aboutTruncated && (
            <p className="text-xs text-muted-foreground">
              Shortened here — the full text is on their page.
            </p>
          )}
        </Section>

        {agent.desks.length > 0 && (
          <Section title="Desks">
            <div className="flex flex-wrap gap-1.5">
              {agent.desks.map((desk) => (
                <a
                  key={desk.id}
                  href={`#/company/${encodeURIComponent(desk.id)}`}
                  className="inline-flex"
                  data-testid={`agent-profile-desk-${desk.id}`}
                >
                  <Badge variant="secondary" className="gap-1">
                    <Users className="size-3" aria-hidden /> {desk.name}
                    {desk.lead && <span className="text-xs opacity-70">(lead)</span>}
                  </Badge>
                </a>
              ))}
            </div>
          </Section>
        )}

        <Section title="Tools">
          {/* An empty `requested` is the company's standard grant, not "no
              tools" — saying "none" for exactly those agents would report the
              opposite of what they hold. */}
          {profile.tools.standardGrant ? (
            <p className="text-sm text-muted-foreground" data-testid="agent-profile-tools">
              Everything this company allows — {profile.tools.effective.length}{" "}
              {profile.tools.effective.length === 1 ? "grant" : "grants"}.
            </p>
          ) : profile.tools.effective.length === 0 ? (
            <p className="text-sm text-muted-foreground" data-testid="agent-profile-tools">
              No tools. Nothing this agent asked for is on the company's allow-list.
            </p>
          ) : (
            <div className="flex flex-wrap gap-1.5" data-testid="agent-profile-tools">
              {profile.tools.effective.map((glob) => (
                <Badge key={glob} variant="outline" className="gap-1 font-mono text-xs">
                  <Wrench className="size-3" aria-hidden /> {glob}
                </Badge>
              ))}
            </div>
          )}
          {profile.tools.dropped.length > 0 && (
            <p className="text-xs text-muted-foreground" data-testid="agent-profile-tools-dropped">
              Asked for but not allowed: {profile.tools.dropped.join(", ")}.
            </p>
          )}
        </Section>

        <p className="font-mono text-xs text-muted-foreground" data-testid="agent-profile-id">
          {agent.id}
        </p>
      </div>

      {/* Anchors rather than handlers: these are addresses (`#/team/<id>`), so
          they open in a new tab, copy, and land in history like every other
          link in the console. */}
      <div className="mt-auto flex gap-2 p-4">
        <Button
          className="flex-1"
          render={<a href={agentHref(agent.id, { edit: true })} />}
          disabled={!editable}
          title={editable ? undefined : "This agent can't be edited from here."}
          data-testid="agent-profile-edit"
        >
          <Pencil className="size-4" aria-hidden /> Edit agent
        </Button>
        <Button
          variant="outline"
          className="flex-1"
          render={<a href={agentHref(agent.id)} />}
          data-testid="agent-profile-open"
        >
          Full profile
        </Button>
      </div>
    </>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="space-y-1.5" aria-label={title}>
      <h4 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">{title}</h4>
      {children}
    </section>
  );
}

function Message({ title, body }: { title: string; body: string }) {
  return (
    <SheetHeader className="gap-1 pr-10">
      <SheetTitle>{title}</SheetTitle>
      <SheetDescription>{body}</SheetDescription>
    </SheetHeader>
  );
}

function ProfileSkeleton() {
  return (
    <div className="space-y-4 p-4" data-testid="agent-profile-loading">
      <div className="flex items-center gap-3">
        <Skeleton className="size-12 rounded-xl" />
        <div className="flex-1 space-y-2">
          <Skeleton className="h-4 w-32" />
          <Skeleton className="h-3 w-20" />
        </div>
      </div>
      <Skeleton className="h-16 w-full" />
      <Skeleton className="h-8 w-2/3" />
    </div>
  );
}
