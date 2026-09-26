import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { Building2 } from "lucide-react";

import { useConsole } from "@/lib/console-context";
import {
  resolveAvatarSrc,
  staticAvatarSrc,
  retainAvatar,
  releaseAvatar,
  blobNodeId,
  subscribeAvatarNode,
  isMascotRef,
} from "@/lib/avatar";
import {
  getMascotSnapshot,
  mascotCostumeKey,
  publishMascotSnapshot,
  subscribeMascotSnapshot,
} from "@/lib/mascot-snapshot";
import { TEAM_TONES, avatarFor, initials } from "@/lib/team";
import { cn } from "@/lib/utils";

/**
 * The Rive runtime + the mascot asset are ~1.8 MB combined — code-split so
 * every tile that is *not* a `mascot:` wearer (the overwhelming majority)
 * pays nothing for it, the same reasoning `agent-profile-sheet.tsx` and
 * `views/team/AgentDetailView.tsx` already apply to their own hero avatars.
 * This is the one place that decides *whether* a mascot renders at every
 * other surface (issue: "should be visible everywhere; wherever avatar
 * renders" — found live 2026-09-26 against the chat header, message rows,
 * the Team sidebar and the channel list, all still drawing plain initials
 * after picking a mascot) — but not, past `MascotWarmer` below, whether it
 * renders *live*. See that component's docs for why.
 */
const LazyMascotAvatar = lazy(() =>
  import("@/components/mascot-avatar").then((m) => ({ default: m.MascotAvatar })),
);

interface Props {
  name: string;
  tone?: string;
  /** The company's own voice wears the brand mark rather than initials. */
  company?: boolean;
  /**
   * Draw the tone tile without initials.
   *
   * For decorative stacks small enough that no glyph can be read at a size
   * the tile can hold: a 16px facepile fits two letters only below 10px,
   * and below 10px is not a size, it is a bug. The tile's colour still
   * distinguishes one voice from the next, which is the whole of what a
   * facepile claims to say.
   */
  markOnly?: boolean;
  /**
   * The avatar reference to draw (`lib/avatar.ts`) — `tiny:<flavour>` for a
   * shipped mascot, `blob:<nodeId>` for an image somebody uploaded.
   *
   * Optional: a caller holding a `Member` passes its resolved `avatar` so the
   * face matches everywhere that teammate appears, and a caller with only a name
   * falls back to the mascot hashed from that.
   */
  avatar?: string;
  /**
   * The mascot's chosen costume and colors — three of `AgentDetailDto`'s four
   * mascot fields (`api/types.ts`); there is no `mascotMode` here because
   * nothing drawn through this component ever wires up hover/replying
   * reactivity, so `"static"`'s resting frame and `"animated"`'s idle
   * baseline are the same picture (`mascot-avatar.tsx`'s own docs) — mode has
   * no visual effect at any surface this prop reaches. Meaningful only when
   * `avatar` is `"mascot:animated"`; ignored otherwise.
   *
   * Optional, and most callers have nothing to pass: the roster-shaped DTOs
   * behind the org chart, the member list, the channel rail and the chat
   * gutter carry only `avatar`, not per-teammate costume/color — so a mascot
   * drawn at those surfaces renders on the file's own default costume and
   * colors rather than whatever that teammate actually chose. That is a real
   * fidelity gap, not a bug in this component: closing it needs those DTOs to
   * start sending the three fields, which is a host-side change (tracked
   * separately), not one this component can paper over. A caller that *does*
   * hold them (today, none does through this prop — the two hero surfaces
   * mount `MascotAvatar` directly instead, for the hover reactivity this
   * component does not model) should pass them through so the day a roster
   * DTO gains them, every mass-render surface picks the right look for free.
   */
  mascotCostume?: string;
  /** See {@link mascotCostume}. */
  mascotSkinColor?: string;
  /** See {@link mascotCostume}. */
  mascotHandColor?: string;
  className?: string;
  /**
   * Forwarded to the tile so a spec can name one avatar among several on a page.
   *
   * Declared rather than picked up from a rest spread: a hyphenated prop passes
   * TypeScript's excess-property check on any component, so an undeclared
   * `data-testid` here would type-check happily and then be dropped at render —
   * a selector that silently matches nothing.
   */
  "data-testid"?: string;
}

/**
 * A square-ish chat avatar: initials on a tone-tinted tile.
 *
 * Rounded rather than circular, which is what distinguishes a workspace
 * avatar from a contact-list one — DM rows, message gutters, and the member
 * pane all draw the same tile at different sizes.
 */
export function TeammateAvatar({
  name,
  tone,
  company,
  markOnly,
  avatar,
  mascotCostume,
  mascotSkinColor,
  mascotHandColor,
  className,
  "data-testid": testId,
}: Props) {
  if (company) {
    return (
      <span
        className={cn(
          "flex shrink-0 items-center justify-center rounded-md bg-primary text-primary-foreground",
          className,
        )}
        aria-hidden
        data-testid={testId}
      >
        <Building2 className="size-1/2" />
      </span>
    );
  }

  // `markOnly` is the caller saying "this tile is too small to read". The
  // mascot is subject to the same limit as the initials it replaces — at 16px
  // it is a smudge — so the tone tile stays the answer there rather than a
  // detailed drawing nobody can resolve.
  if (markOnly) {
    return (
      <span
        className={cn(
          "flex shrink-0 items-center justify-center rounded-md text-xs font-semibold",
          toneClass(tone),
          className,
        )}
        aria-hidden
        data-testid={testId}
      />
    );
  }

  return (
    <AvatarTile
      name={name}
      tone={tone}
      avatar={avatar}
      mascotCostume={mascotCostume}
      mascotSkinColor={mascotSkinColor}
      mascotHandColor={mascotHandColor}
      className={className}
      testId={testId}
    />
  );
}

/**
 * The tile itself, once the company-mark and mark-only cases are out of the way.
 *
 * Split out because it holds a hook and the two cases above return before it:
 * a hook cannot sit behind an early return, and hoisting it into the component
 * would mean every 16px facepile tile subscribed to console context and ran an
 * effect to draw nothing.
 */
function AvatarTile({
  name,
  tone,
  avatar,
  mascotCostume,
  mascotSkinColor,
  mascotHandColor,
  className,
  testId,
}: {
  name: string;
  tone?: string;
  avatar?: string;
  mascotCostume?: string;
  mascotSkinColor?: string;
  mascotHandColor?: string;
  className?: string;
  testId?: string;
}) {
  const ref = avatar ?? avatarFor(name);
  const mascot = isMascotRef(ref);
  // `useAvatarSrc` is still called unconditionally for a mascot reference —
  // hooks cannot sit behind a branch — but it is cheap: `staticAvatarSrc`
  // returns `null` for `mascot:` on purpose (`lib/avatar.ts`), so this never
  // fetches anything for one.
  const src = useAvatarSrc(ref);

  // The tone tile stays underneath the image (or the mascot) on purpose: it
  // is what shows if the avatar 404s, has not loaded yet, or — for a mascot
  // with nothing cached yet — is still warming one (`MascotTile` below).
  return (
    <span
      className={cn(
        "relative flex shrink-0 items-center justify-center overflow-hidden rounded-md text-xs font-semibold",
        toneClass(tone),
        className,
      )}
      aria-hidden
      data-testid={testId}
    >
      <span className="absolute inset-0 flex items-center justify-center">{initials(name)}</span>
      {mascot ? (
        <MascotTile
          costume={mascotCostume}
          skinColor={mascotSkinColor}
          handColor={mascotHandColor}
          className="absolute inset-0 rounded-none"
        />
      ) : (
        // Nothing is drawn until there is something to draw. An uploaded face
        // is fetched through the authenticated client, so its `src` arrives a
        // tick late — rendering an `img` with no source in the meantime would
        // paint the browser's broken-image glyph over the initials this tile
        // is showing precisely so that the gap is never empty.
        src && (
          <img
            src={src}
            alt=""
            loading="lazy"
            decoding="async"
            className="relative size-full object-cover"
          />
        )
      )}
    </span>
  );
}

/**
 * A mascot tile that is a plain cached image whenever it can be, and a live
 * canvas only for the one instance, per look, that has to warm the cache.
 *
 * Fills its parent (`absolute inset-0` from the caller); paints nothing at
 * all until a look is cached, leaving `AvatarTile`'s initials span as the
 * only thing on screen in the meantime — the same "never blank" contract
 * `AvatarTile`'s own `<img>` branch keeps.
 */
function MascotTile({
  costume,
  skinColor,
  handColor,
  className,
}: {
  costume?: string;
  skinColor?: string;
  handColor?: string;
  className?: string;
}) {
  const key = mascotCostumeKey(costume, skinColor, handColor);
  const [snapshot, setSnapshot] = useState(() => getMascotSnapshot(key));

  // A snapshot published *after* this mount (by some other tile's warmer, or
  // by this tile's own — see `MascotWarmer`) still has to reach this state.
  // Already-cached looks skip this entirely: `subscribeMascotSnapshot` would
  // register a listener that a publish for this key will never call again,
  // since `publishMascotSnapshot` already fired for whoever warmed it.
  useEffect(() => {
    if (snapshot) return;
    return subscribeMascotSnapshot(key, setSnapshot);
  }, [key, snapshot]);

  if (snapshot) {
    return <img src={snapshot} alt="" className={cn("object-cover", className)} />;
  }
  return <MascotWarmer costume={costume} skinColor={skinColor} handColor={handColor} cacheKey={key} className={className} />;
}

/**
 * Mounts a live mascot only long enough to capture its resting frame, then
 * replaces itself with the same cached `<img>` every other `MascotTile` for
 * this look ends up showing.
 *
 * # Why every uncached tile races rather than one being chosen
 *
 * A single shared "warmer" would need a lock: whichever tile's render runs
 * first for a given look claims it, every other tile for that look waits.
 * Doing that claim correctly means mutating module state during render
 * itself (an effect runs too late — by the time the first tile's effect
 * fires, every other tile for the same look has already rendered and, seeing
 * nothing cached, would also have decided to warm), and React does not
 * promise a render runs exactly once: Strict Mode double-invokes it on
 * purpose, specifically to catch code that is not safe to run twice. A
 * module-level claim made inside render is exactly that code.
 *
 * So instead every tile with nothing cached becomes its own warmer, and they
 * race: each subscribes to the *other* possible winners
 * (`subscribeMascotSnapshot`, in `MascotTile`) before its own canvas is even
 * live, so the instant any of them — including one racing for the same key
 * on a completely different tile — publishes first, every loser tears its
 * own `MascotAvatar` down on the next render rather than finishing a capture
 * nobody will read. Measured live (2026-09-26, 30 identical-look message
 * rows mounted at once against `companies/vending_machine_co`): the loss is
 * bounded by how long the *fastest* racer takes, not by doing the work
 * thirty times over.
 */
function MascotWarmer({
  costume,
  skinColor,
  handColor,
  cacheKey,
  className,
}: {
  costume?: string;
  skinColor?: string;
  handColor?: string;
  cacheKey: string;
  className?: string;
}) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [dataUrl, setDataUrl] = useState<string | null>(null);

  // A snapshot for this key can land between this tile deciding to warm (its
  // parent `MascotTile` found nothing cached) and this effect subscribing —
  // another racer may have already won. Checked once more here so a losing
  // warmer still stops instead of running its live canvas to completion for
  // nothing.
  useEffect(() => {
    const already = getMascotSnapshot(cacheKey);
    if (already) {
      setDataUrl(already);
      return;
    }
    return subscribeMascotSnapshot(cacheKey, setDataUrl);
  }, [cacheKey]);

  if (dataUrl) {
    return <img src={dataUrl} alt="" className={cn("object-cover", className)} />;
  }
  return (
    <>
      {/* Nothing rendered at the caller's own slot while warming: the
          caller's initials span (`AvatarTile`) is already what shows during
          both the lazy chunk and the `.riv` file's own load, the same "never
          blank" contract its `<img>` branch keeps — this component adds
          nothing on top of it here. */}
      {/* The capture instance itself is fixed-size and invisible, not sized
          to this tile: the first tile to need an uncached look is as likely
          to be a 20px facepile dot as a 48px channel intro, and a capture
          taken at 20px looks visibly soft — genuinely broken, not just
          "smaller" — once CSS stretches it back up for a 36px message row
          (found live 2026-09-26). `size-16` (64px) is comfortably above
          every caller's own size today, so every reuse is a downscale, which
          never looks soft, rather than an upscale, which always does.
          `opacity-0` rather than positioning this off-screen: Rive's own
          `shouldUseIntersectionObserver` (default on, per `@rive-app`'s own
          types) pauses a canvas's render loop once it stops intersecting the
          viewport, and this component's first attempt — parked at
          `left: -9999px` — never painted a single frame for exactly that
          reason (confirmed live: a captured 64×64 PNG that compressed to
          under 400 bytes, i.e. fully transparent). `opacity-0` keeps this
          div geometrically inside the viewport — intersecting, in Rive's and
          `IntersectionObserver`'s terms, opacity plays no part in that check
          — while compositing nothing visible; the canvas's own bitmap buffer
          is unaffected by CSS opacity either way, which is what
          `toDataURL()` below actually reads. */}
      <div
        ref={containerRef}
        className="pointer-events-none fixed top-0 left-0 size-16 opacity-0"
        aria-hidden
      >
        {/* No `Suspense` fallback beyond `null`: nothing here is ever shown —
            see above. `mode="static"` because nothing here ever wires up
            hover — see `TeammateAvatar`'s own `mascotCostume` doc for why a
            mode prop would be meaningless in this whole cached path anyway. */}
        <Suspense fallback={null}>
          <LazyMascotAvatar
            mode="static"
            costume={costume}
            skinColor={skinColor}
            handColor={handColor}
            className="size-full"
            onReady={() => {
              const canvas = containerRef.current?.querySelector("canvas");
              if (!canvas) return;
              try {
                const url = canvas.toDataURL("image/png");
                publishMascotSnapshot(cacheKey, url);
                setDataUrl(url);
              } catch {
                // A canvas Rive draws into procedurally should never be
                // cross-origin-tainted — nothing is ever `drawImage`'d onto
                // it from another origin — but `toDataURL` is specified to
                // throw a `SecurityError` if it is, and this component would
                // rather stay live forever for one look than throw out of an
                // event handler and take the rest of the page with it.
              }
            }}
          />
        </Suspense>
      </div>
    </>
  );
}

/**
 * The `src` for an avatar reference, fetching an uploaded one if that is what it
 * names.
 *
 * A mascot resolves synchronously on the first render — which is what keeps the
 * common case free of a flash — and only a `blob:` reference goes through the
 * client. The fetch is cached module-wide (`resolveAvatarSrc`), so the same
 * uploaded face appearing forty times on a screen costs one request.
 */
function useAvatarSrc(ref: string): string | null {
  const { client, company } = useConsole();
  const immediate = staticAvatarSrc(ref);

  // The URL is stored beside the reference it was fetched for and the scope
  // it was fetched under. A mounted tile whose scope (`client` or `company`)
  // changes while `ref` stays the same — a `blob:` node id that is valid in
  // the previous company — must not keep returning the previous company's
  // object URL; the render that carries the new scope would otherwise answer
  // with the old company's face.
  // A mascot resolves synchronously, so `immediate` is always the current face
  // and the stateful path only ever holds an uploaded one.
  const [fetched, setFetched] = useState<{
    client: typeof client;
    company: typeof company;
    ref: string;
    src: string | null;
  } | null>(null);
  const src =
    fetched?.ref === ref && fetched?.client === client && fetched?.company === company
      ? fetched.src
      : immediate;

  // Revoking a face's object URL does not unpaint a tile that already decoded
  // it, so a delete from the workspace cannot redraw a mounted tile on its own.
  // Subscribing for the node makes `forgetAvatarNode` reach this tile: it bumps
  // `forgot`, which re-runs the resolve below — the deleted bytes 404 and the
  // tile falls back to the tone tile it was already drawing underneath.
  const [forgot, setForgot] = useState(0);
  useEffect(() => {
    const node = blobNodeId(ref);
    if (!node || !client) return;
    return subscribeAvatarNode(client, company, node, () => setForgot((n) => n + 1));
  }, [client, company, ref]);

  useEffect(() => {
    const node = blobNodeId(ref);
    if (!node || !client) return;
    retainAvatar(client, company, node);
    return () => releaseAvatar(client, company, node);
  }, [client, company, ref]);

  useEffect(() => {
    // No client means no authenticated fetch is possible — outside the console
    // shell, or before one is chosen. A mascot still resolves; an uploaded face
    // draws as the tone tile, which is the same thing a deleted one does.
    const resolved = client ? resolveAvatarSrc(client, company, ref) : immediate;
    // The URL is stored beside the scope it was fetched under, so the render
    // guard above can tell a same-`ref` result fetched under the previous scope
    // from one fetched under the current one.
    if (typeof resolved === "string" || resolved === null) {
      setFetched({ client, company, ref, src: resolved });
      return;
    }
    // A reference that changed while a fetch was in flight must not have the
    // stale result written over it — the tile would show the previous person's
    // face, which is worse than showing none.
    let live = true;
    setFetched({ client, company, ref, src: null });
    void resolved.then((url) => {
      if (live) setFetched({ client, company, ref, src: url });
    });
    return () => {
      live = false;
    };
  }, [client, company, ref, immediate, forgot]);

  return src;
}

/** Fall back to a hashed tone so an unnamed voice still gets a stable color. */
function toneClass(tone?: string): string {
  if (tone && TEAM_TONES[tone]) return TEAM_TONES[tone];
  const keys = Object.keys(TEAM_TONES);
  let hash = 0;
  const seed = tone ?? "";
  for (let i = 0; i < seed.length; i++) hash = (hash * 31 + seed.charCodeAt(i)) | 0;
  return TEAM_TONES[keys[Math.abs(hash) % keys.length]];
}
