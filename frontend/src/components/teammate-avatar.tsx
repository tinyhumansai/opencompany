import { useEffect, useState } from "react";
import { Building2 } from "lucide-react";

import { useConsole } from "@/lib/console-context";
import { resolveAvatarSrc, staticAvatarSrc, retainAvatar, releaseAvatar, blobNodeId, subscribeAvatarNode } from "@/lib/avatar";
import { TEAM_TONES, avatarFor, initials } from "@/lib/team";
import { cn } from "@/lib/utils";

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

  return <AvatarTile name={name} tone={tone} avatar={avatar} className={className} testId={testId} />;
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
  className,
  testId,
}: {
  name: string;
  tone?: string;
  avatar?: string;
  className?: string;
  testId?: string;
}) {
  const ref = avatar ?? avatarFor(name);
  const src = useAvatarSrc(ref);

  // The tone tile stays underneath the image on purpose: it is what shows if
  // the avatar 404s or has not loaded yet, so the gutter never collapses to a
  // blank square mid-scroll.
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
      {/* Nothing is drawn until there is something to draw. An uploaded face is
          fetched through the authenticated client, so its `src` arrives a tick
          late — rendering an `img` with no source in the meantime would paint
          the browser's broken-image glyph over the initials this tile is showing
          precisely so that the gap is never empty. */}
      {src && (
        <img
          src={src}
          alt=""
          loading="lazy"
          decoding="async"
          className="relative size-full object-cover"
        />
      )}
    </span>
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
