// Choosing a face: the eleven shipped mascots, an upload, and the way back to
// the default.
//
// Used for two subjects that are not the same kind of thing — a teammate, whose
// face any member may set, and yourself — and deliberately one component for
// both. What is being chosen is identical (`docs/spec/runtime/avatars.md`), and
// two pickers would be two places for the accepted formats, the size ceiling
// and the reset affordance to drift apart.

import { useEffect, useRef, useState } from "react";
import { ImagePlus, Loader2, RotateCcw } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { ApiError } from "@/api/types";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Button } from "@/components/ui/button";
import {
  AVATAR_ACCEPT,
  MAX_AVATAR_MB,
  TINY_FLAVOURS,
  avatarRef,
  uploadAvatar,
} from "@/lib/avatar";
import { cn } from "@/lib/utils";

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
  disabled,
}: Props) {
  const [uploading, setUploading] = useState(false);
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
        <TeammateAvatar
          name={name}
          tone={tone}
          avatar={current}
          className="size-14 rounded-xl text-base"
          data-testid="avatar-preview"
        />
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
      </div>
    </div>
  );
}
