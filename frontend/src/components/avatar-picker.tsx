// Choosing a face: the eleven shipped mascots, an upload, and the way back to
// the default.
//
// Used for two subjects that are not the same kind of thing — a teammate, whose
// face any member may set, and yourself — and deliberately one component for
// both. What is being chosen is identical (`docs/spec/runtime/avatars.md`), and
// two pickers would be two places for the accepted formats, the size ceiling
// and the reset affordance to drift apart.

import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { ImagePlus, Loader2, RotateCcw } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import {
  AVATAR_ACCEPT,
  DEFAULT_MASCOT_COSTUME,
  MASCOT_COSTUMES,
  MASCOT_HAND_COLORS,
  MASCOT_KINDS,
  MASCOT_MODES,
  MASCOT_SKIN_COLORS,
  MAX_AVATAR_MB,
  TINY_FLAVOURS,
  avatarRef,
  isMascotRef,
  uploadAvatar,
} from "@/lib/avatar";
import { cn } from "@/lib/utils";

/** See `agent-profile-sheet.tsx` for why this is lazy-loaded rather than a static import. */
const LazyMascotAvatar = lazy(() =>
  import("@/components/mascot-avatar").then((m) => ({ default: m.MascotAvatar })),
);

interface Props {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * The chosen reference, or `undefined` for "nobody has chosen".
   *
   * Undefined is a state of its own rather than a stand-in for the hashed
   * default: it is what makes the reset row offerable only when there is a
   * choice to undo.
   */
  value?: string;
  /** The id the hashed default is drawn from — the same seed every other surface uses. */
  seed: string;
  /** The name the tile falls back to initials from while an image loads. */
  name: string;
  tone?: string;
  /** `undefined` means "back to the default", never "no face". */
  onChange: (avatar: string | undefined) => void;
  /**
   * The mascot's display mode in force (`"static"`/`"animated"`), or
   * `undefined` for the file's own default (`"animated"`). Only read while
   * `value` is a `mascot:` reference.
   */
  mascotMode?: string;
  /** The mascot costume in force, or `undefined` for the file's own default. */
  mascotCostume?: string;
  /** The mascot's skin (body) color in force, or `undefined` for the file's own default. */
  mascotSkinColor?: string;
  /** The mascot's hand/accent color in force, or `undefined` for the file's own default. */
  mascotHandColor?: string;
  /**
   * Sets the mascot's display mode; `undefined` resets to the file's own
   * default (`"animated"`).
   *
   * Omitting this (alongside the other three `onChangeMascot*` callbacks)
   * hides the mode/costume/color section entirely — the create-teammate
   * dialog and the "you" profile picker have nowhere yet to persist any of
   * these, so they pass none and this picker falls back to offering only the
   * mascot tile itself, exactly as before this section existed.
   */
  onChangeMascotMode?: (mode: string | undefined) => void;
  /** Sets the mascot costume; `undefined` resets to the file's own default. */
  onChangeMascotCostume?: (costume: string | undefined) => void;
  /** Sets the mascot's skin color; `undefined` resets to the file's own default. */
  onChangeMascotSkinColor?: (color: string | undefined) => void;
  /** Sets the mascot's hand color; `undefined` resets to the file's own default. */
  onChangeMascotHandColor?: (color: string | undefined) => void;
  /** Whether the picker is inert — a save in flight, or a teammate nobody may edit. */
  disabled?: boolean;
}

/**
 * The picker.
 *
 * It does not save: `onChange` hands the caller a reference and the caller
 * decides when that becomes a `PATCH`. That split is what lets the same
 * component sit in a create dialog, where there is nothing to patch yet, and in
 * a detail page, where every click is a save.
 */
export function AvatarPicker({
  client,
  company,
  value,
  seed,
  name,
  tone,
  onChange,
  mascotMode,
  mascotCostume,
  mascotSkinColor,
  mascotHandColor,
  onChangeMascotMode,
  onChangeMascotCostume,
  onChangeMascotSkinColor,
  onChangeMascotHandColor,
  disabled,
}: Props) {
  const [uploading, setUploading] = useState(false);
  const [previewHovering, setPreviewHovering] = useState(false);
  const fileRef = useRef<HTMLInputElement>(null);
  // False once the picker has left the tree. An upload outlives a dialog that
  // was dismissed while it was in flight, and the picker must not then hand the
  // reference to a caller that is no longer showing it — in AgentDetailView
  // that `onChange` saves the face, so a slow upload followed by Escape would
  // change the teammate's icon after the dialog was gone.
  const mounted = useRef(true);
  useEffect(() => {
    // StrictMode replays setup → cleanup → setup in development while keeping
    // refs alive. Re-arm the guard in setup so the replayed mounted instance
    // can still commit a successful upload.
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const current = avatarRef(value, seed);
  // Static is the one mode that must never wire hover up (`mascot-avatar.tsx`
  // ignores `state` in that mode too, but the point of this flag is that the
  // hover *handlers themselves* are never attached here either — a static
  // mascot does not react, full stop, not merely "reacts to nothing visibly").
  const isStaticPreview = (mascotMode ?? "animated") === "static";

  async function upload(file: File) {
    setUploading(true);
    try {
      const { avatar } = await uploadAvatar(client, company, file);
      if (!mounted.current) return;
      onChange(avatar);
    } catch (err) {
      // The host's own sentence, which names the actual problem — the wrong
      // format, or an image over the ceiling. A generic "upload failed" here
      // would replace a useful message with a useless one.
      toast.error(
        err instanceof ApiError ? err.message : "That image couldn't be uploaded.",
      );
    } finally {
      setUploading(false);
      // Cleared so re-picking the *same* file fires `change` again — a browser
      // does not re-fire it for an unchanged value, which reads as the button
      // being dead after a failed upload.
      if (fileRef.current) fileRef.current.value = "";
    }
  }

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-4">
        {isMascotRef(current) ? (
          isStaticPreview ? (
            // The `Skeleton` sits behind, not just in the `Suspense` fallback:
            // `MascotAvatar` stays transparent past the chunk load, through
            // its own `.riv` fetch (~1.7 MB, a further second or two) — the
            // operator-noticed "preview canvas takes 1-3s to paint with no
            // loading indicator" gap this closes.
            <div className="relative size-14">
              <Skeleton className="absolute inset-0 rounded-xl" />
              <Suspense fallback={null}>
                <LazyMascotAvatar
                  mode="static"
                  costume={mascotCostume}
                  skinColor={mascotSkinColor}
                  handColor={mascotHandColor}
                  className="absolute inset-0"
                  data-testid="avatar-preview"
                />
              </Suspense>
            </div>
          ) : (
            <span
              onMouseEnter={() => setPreviewHovering(true)}
              onMouseLeave={() => setPreviewHovering(false)}
            >
              <div className="relative size-14">
                <Skeleton className="absolute inset-0 rounded-xl" />
                <Suspense fallback={null}>
                  <LazyMascotAvatar
                    mode="animated"
                    state={previewHovering ? "hover" : "idle"}
                    costume={mascotCostume}
                    skinColor={mascotSkinColor}
                    handColor={mascotHandColor}
                    className="absolute inset-0"
                    data-testid="avatar-preview"
                  />
                </Suspense>
              </div>
            </span>
          )
        ) : (
          <TeammateAvatar
            name={name}
            tone={tone}
            avatar={current}
            className="size-14 rounded-xl text-base"
            data-testid="avatar-preview"
          />
        )}
        <div className="flex flex-wrap items-center gap-2">
          <input
            ref={fileRef}
            type="file"
            accept={AVATAR_ACCEPT}
            className="sr-only"
            data-testid="avatar-upload-input"
            onChange={(e) => {
              const file = e.target.files?.[0];
              if (file) void upload(file);
            }}
          />
          <Button
            type="button"
            variant="outline"
            size="sm"
            disabled={disabled || uploading}
            onClick={() => fileRef.current?.click()}
            data-testid="avatar-upload"
          >
            {uploading ? (
              <Loader2 className="size-4 animate-spin" />
            ) : (
              <ImagePlus className="size-4" />
            )}
            Upload an image
          </Button>
          {value && (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              disabled={disabled || uploading}
              onClick={() => onChange(undefined)}
              data-testid="avatar-reset"
            >
              <RotateCcw className="size-4" />
              Use the default
            </Button>
          )}
        </div>
      </div>
      {/* GIFs are named on purpose: an animated face is the case people ask for
          and the one nobody expects to be allowed. */}
      <p className="text-xs text-muted-foreground">
        PNG, JPEG, WebP or GIF, up to {MAX_AVATAR_MB}&nbsp;MB. Animated GIFs keep moving.
      </p>
      <div
        className="flex flex-wrap gap-2"
        role="radiogroup"
        aria-label="Avatar"
        data-testid="avatar-flavours"
      >
        {TINY_FLAVOURS.map((flavour) => {
          const ref = `tiny:${flavour}`;
          const selected = current === ref;
          return (
            <button
              key={flavour}
              type="button"
              role="radio"
              aria-checked={selected}
              aria-label={flavour}
              disabled={disabled || uploading}
              onClick={() => onChange(ref)}
              data-testid={`avatar-flavour-${flavour}`}
              className={cn(
                "rounded-lg p-0.5 ring-2 transition-colors disabled:opacity-50",
                // The ring is the whole selection signal, so it has to survive
                // being drawn over eleven different colours: `ring-primary` on
                // the chosen one, and transparent — not absent — on the rest, so
                // nothing shifts by two pixels as the selection moves.
                selected ? "ring-primary" : "ring-transparent hover:ring-border",
              )}
            >
              <TeammateAvatar
                name={name}
                avatar={ref}
                className="size-9 rounded-md text-xs"
              />
            </button>
          );
        })}
        {MASCOT_KINDS.map((kind) => {
          const ref = `mascot:${kind}`;
          const selected = current === ref;
          return (
            <button
              key={ref}
              type="button"
              role="radio"
              aria-checked={selected}
              aria-label={`Animated mascot (${kind})`}
              disabled={disabled || uploading}
              onClick={() => onChange(ref)}
              data-testid={`avatar-mascot-${kind}`}
              className={cn(
                "rounded-lg p-0.5 ring-2 transition-colors disabled:opacity-50",
                selected ? "ring-primary" : "ring-transparent hover:ring-border",
              )}
            >
              {/* The grid tile is a picker swatch, not a hero — it never wires
                  hover, so it is always the cheap static render regardless of
                  the teammate's own chosen mode. The `Skeleton` sits behind,
                  not just in the `Suspense` fallback, for the same reason the
                  preview above does. */}
              <div className="relative size-9 rounded-md">
                <Skeleton className="absolute inset-0 rounded-md" />
                <Suspense fallback={null}>
                  <LazyMascotAvatar
                    mode="static"
                    costume={mascotCostume}
                    skinColor={mascotSkinColor}
                    handColor={mascotHandColor}
                    className="absolute inset-0"
                  />
                </Suspense>
              </div>
            </button>
          );
        })}
      </div>
      {isMascotRef(current) &&
        (onChangeMascotMode ||
          onChangeMascotCostume ||
          onChangeMascotSkinColor ||
          onChangeMascotHandColor) && (
          <div className="space-y-3 rounded-lg border p-3" data-testid="avatar-mascot-appearance">
            {onChangeMascotMode && (
              <div className="space-y-1.5">
                <p className="text-xs font-medium text-muted-foreground">Animation</p>
                <div
                  className="inline-flex rounded-md border p-0.5"
                  role="radiogroup"
                  aria-label="Mascot animation mode"
                  data-testid="avatar-mascot-modes"
                >
                  {MASCOT_MODES.map((mode) => {
                    const selected = (mascotMode ?? "animated") === mode;
                    return (
                      <button
                        key={mode}
                        type="button"
                        role="radio"
                        aria-checked={selected}
                        disabled={disabled}
                        onClick={() => onChangeMascotMode(mode)}
                        data-testid={`avatar-mascot-mode-${mode}`}
                        className={cn(
                          "rounded px-2.5 py-1 text-xs capitalize transition-colors disabled:opacity-50",
                          selected
                            ? "bg-primary text-primary-foreground"
                            : "text-muted-foreground hover:text-foreground",
                        )}
                      >
                        {mode}
                      </button>
                    );
                  })}
                </div>
              </div>
            )}
            {onChangeMascotCostume && (
              <div className="space-y-1.5">
                <p className="text-xs font-medium text-muted-foreground">Costume</p>
                <div
                  className="flex flex-wrap gap-1.5"
                  role="radiogroup"
                  aria-label="Mascot costume"
                  data-testid="avatar-mascot-costumes"
                >
                  {MASCOT_COSTUMES.map(({ id, label }) => {
                    const selected = (mascotCostume ?? DEFAULT_MASCOT_COSTUME) === id;
                    return (
                      <button
                        key={id}
                        type="button"
                        role="radio"
                        aria-checked={selected}
                        disabled={disabled}
                        onClick={() => onChangeMascotCostume(id)}
                        data-testid={`avatar-mascot-costume-${id}`}
                        className={cn(
                          "rounded-full border px-2.5 py-1 text-xs transition-colors disabled:opacity-50",
                          selected
                            ? "border-primary bg-primary/10 text-foreground"
                            : "border-border text-muted-foreground hover:border-primary/50",
                        )}
                      >
                        {label}
                      </button>
                    );
                  })}
                </div>
              </div>
            )}
            {onChangeMascotSkinColor && (
              <div className="space-y-1.5">
                <p className="text-xs font-medium text-muted-foreground">Skin color</p>
                <div
                  className="flex flex-wrap gap-2"
                  role="radiogroup"
                  aria-label="Mascot skin color"
                  data-testid="avatar-mascot-skin-colors"
                >
                  {MASCOT_SKIN_COLORS.map(({ id, hex }) => {
                    const selected = (mascotSkinColor ?? "default") === id;
                    return (
                      <button
                        key={id}
                        type="button"
                        role="radio"
                        aria-checked={selected}
                        aria-label={id}
                        title={id}
                        disabled={disabled}
                        onClick={() => onChangeMascotSkinColor(id)}
                        data-testid={`avatar-mascot-skin-${id}`}
                        className={cn(
                          "size-6 rounded-full ring-2 ring-offset-2 ring-offset-background transition-colors disabled:opacity-50",
                          selected ? "ring-primary" : "ring-transparent hover:ring-border",
                        )}
                        style={{ background: hex }}
                      />
                    );
                  })}
                </div>
              </div>
            )}
            {onChangeMascotHandColor && (
              <div className="space-y-1.5">
                <p className="text-xs font-medium text-muted-foreground">Hand color</p>
                <div
                  className="flex flex-wrap gap-2"
                  role="radiogroup"
                  aria-label="Mascot hand color"
                  data-testid="avatar-mascot-hand-colors"
                >
                  {MASCOT_HAND_COLORS.map(({ id, hex }) => {
                    const selected = (mascotHandColor ?? "default") === id;
                    return (
                      <button
                        key={id}
                        type="button"
                        role="radio"
                        aria-checked={selected}
                        aria-label={id}
                        title={id}
                        disabled={disabled}
                        onClick={() => onChangeMascotHandColor(id)}
                        data-testid={`avatar-mascot-hand-${id}`}
                        className={cn(
                          "size-6 rounded-full ring-2 ring-offset-2 ring-offset-background transition-colors disabled:opacity-50",
                          selected ? "ring-primary" : "ring-transparent hover:ring-border",
                        )}
                        style={{ background: hex }}
                      />
                    );
                  })}
                </div>
              </div>
            )}
          </div>
        )}
    </div>
  );
}
