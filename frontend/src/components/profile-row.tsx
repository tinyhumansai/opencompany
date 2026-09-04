// You, in the sidebar footer: the face and name you wear in this company, and
// the way to change either.
//
// It sits with the standing controls rather than in the working nav because it
// is not a destination — it is the console saying who you are signed in as, in
// the place every other app puts that, and opening the one form that changes it.

import { useCallback, useEffect, useState } from "react";
import { LogOut, UserRound } from "lucide-react";
import { toast } from "sonner";

import type { OpenCompanyClient } from "@/api/client";
import { logout, me as fetchMe, updateMe, type Me } from "@/api/auth";
import { AvatarPicker } from "@/components/avatar-picker";
import { TeammateAvatar } from "@/components/teammate-avatar";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { SidebarMenu, SidebarMenuButton, SidebarMenuItem } from "@/components/ui/sidebar";
import { uploadAvatar } from "@/lib/avatar";
import { pictureAsFile, readDeviceIdentity, type DeviceIdentity } from "@/lib/device-identity";
import { guessName, personAvatar, personName } from "@/lib/person";
import { toneFor } from "@/lib/team";

export function ProfileRow({
  client,
  company,
  variant = "sidebar",
  onSignedOut,
}: {
  client: OpenCompanyClient;
  company: string | null;
  /**
   * What to do once the host has revoked this session.
   *
   * Omitted where nothing owns the connection's state, in which case the menu
   * offers no sign-out rather than one that ends nowhere.
   */
  onSignedOut?: () => void;
  /**
   * Which chrome this is drawn in.
   *
   * `titlebar` is the home: the far right of the window's title row, opposite
   * the company switcher. `sidebar` is the footer row it used to be, kept for
   * any chrome that still gives it a column to sit at the bottom of.
   *
   * The two differ in shape and in nothing else. A sidebar footer row is a
   * full-width menu item; a title-row control sizes to its own content and
   * stops. Both open the same dialog.
   */
  variant?: "sidebar" | "titlebar";
}) {
  const [me, setMe] = useState<Me | null>(null);
  const [open, setOpen] = useState(false);
  const [signingOut, setSigningOut] = useState(false);

  useEffect(() => {
    let live = true;
    // `me` is keyed by the scope it was fetched for. When the company changes,
    // the row must not keep the previous company's identity on screen while the
    // new fetch is in flight — and, worse, a save in that window would write
    // the old name into the new company — so drop the stale record first.
    setMe(null);
    void fetchMe(client, company)
      .then((who) => {
        if (live) setMe(who);
      })
      // A company with no sign-in has no `me` to read, and a session that has
      // just expired answers 401. Neither is worth a toast on a sidebar row:
      // the row simply does not appear.
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [client, company]);

  // `me` is trusted by the type system and not by reality: a host that answers
  // this route with a different shape — an older one, a proxy, a stub that
  // returns `[]` for anything it does not recognise — must not blank the whole
  // console over a sidebar row. Same class as the `mentionables` guard in
  // AppShell; there it took out every test in chat-channel-membership.spec.ts.
  if (!me || typeof me !== "object" || !("email" in me) || !("id" in me)) return null;
  const name = personName(me);

  // 20px, not the 16px a sidebar icon slot would take: 16 is below the size a
  // face can be read at (see `MessageRow`'s facepile note), and this is the one
  // control on screen whose whole job is to show you yours. A row's icon slot
  // sizes to its content, so the extra four pixels cost the label nothing.
  const face = (
    <TeammateAvatar
      name={name}
      tone={toneFor(me.id || me.email)}
      avatar={personAvatar(me)}
      // Round, not the sidebar's `rounded-[4px]`: in the title row this sits
      // inside a circular button, and a squircle inside a circle reads as a
      // mistake at 20px — the corners clip against the border on every side.
      className="size-7 rounded-full text-2xs"
    />
  );

  const dialog = (
    <ProfileDialog
      client={client}
      company={company}
      me={me}
      open={open}
      onOpenChange={setOpen}
      onSaved={setMe}
    />
  );

  // The host is the authority on whether the session ended, so the console is
  // told nothing until it answers. A failed revocation leaves every bit of
  // state alone and says so: dropping to a login screen over a session that is
  // still live is the one outcome worse than a sign-out that visibly failed.
  async function signOut() {
    setSigningOut(true);
    try {
      await logout(client, company);
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Couldn't sign you out. You're still signed in.",
      );
      return;
    } finally {
      setSigningOut(false);
    }
    setOpen(false);
    onSignedOut?.();
  }

  const menu = (
    <DropdownMenuContent align="end" data-testid="profile-menu">
      <DropdownMenuGroup>
        {/* Which account this menu would sign out, for the shared machine the
            control exists for. */}
        <DropdownMenuLabel className="max-w-56 truncate font-normal text-muted-foreground">
          {me.email}
        </DropdownMenuLabel>
        <DropdownMenuSeparator />
        <DropdownMenuItem onClick={() => setOpen(true)} data-testid="profile-open">
          <UserRound className="mr-2 size-4" />
          Your profile
        </DropdownMenuItem>
        {onSignedOut && (
          <DropdownMenuItem
            disabled={signingOut}
            onClick={() => void signOut()}
            data-testid="profile-sign-out"
          >
            <LogOut className="mr-2 size-4" />
            {signingOut ? "Signing out…" : "Sign out"}
          </DropdownMenuItem>
        )}
      </DropdownMenuGroup>
    </DropdownMenuContent>
  );

  if (variant === "titlebar") {
    return (
      <>
        <DropdownMenu>
          <DropdownMenuTrigger
            data-testid="profile-row"
            // Native `title` rather than the sidebar's tooltip component, which
            // only renders while the rail is collapsed and needs the sidebar
            // context to know it. This control is not in the rail any more.
            title={name}
            aria-label={name}
            // The avatar alone. A title row is chrome, and the operator's own
            // name is the one label they never need read back to them — it cost
            // horizontal space at every window width to say something they
            // already know. `title` and `aria-label` keep it reachable by
            // pointer and by screen reader, so only the pixels are lost.
            // A ring at rest, not only on hover: stripped to the avatar alone
            // the control had no edge of its own, so it read as a decorative
            // mark rather than something clickable.
            className="flex items-center rounded-full border border-sidebar-border bg-sidebar/60 p-0.5 transition hover:border-sidebar-accent-foreground/30 hover:bg-sidebar-accent focus-visible:ring-2 focus-visible:ring-ring focus-visible:outline-none"
          >
            {face}
          </DropdownMenuTrigger>
          {menu}
        </DropdownMenu>
        {dialog}
      </>
    );
  }

  return (
    <SidebarMenu>
      <SidebarMenuItem>
        <DropdownMenu>
          <DropdownMenuTrigger
            render={<SidebarMenuButton tooltip={name} />}
            data-testid="profile-row"
          >
            {face}
            <span className="truncate">{name}</span>
          </DropdownMenuTrigger>
          {menu}
        </DropdownMenu>
      </SidebarMenuItem>
      {dialog}
    </SidebarMenu>
  );
}

/**
 * Your name and your face.
 *
 * Both are three-state on the wire — leave alone / back to the default / this
 * value — so this form can save one without touching the other, and can offer a
 * real "use the default" rather than only an empty field.
 */
function ProfileDialog({
  client,
  company,
  me,
  open,
  onOpenChange,
  onSaved,
}: {
  client: OpenCompanyClient;
  company: string | null;
  me: Me;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onSaved: (me: Me) => void;
}) {
  const [name, setName] = useState("");
  const [avatar, setAvatar] = useState<string | undefined>(undefined);
  const [saving, setSaving] = useState(false);
  const [device, setDevice] = useState<DeviceIdentity>({});

  // Reset from the record every time the dialog opens, not once on mount: a
  // form left half-edited and dismissed must not still be half-edited when it
  // is opened again.
  useEffect(() => {
    if (!open) return;
    setName(me.displayName ?? "");
    setAvatar(me.avatar);
  }, [open, me.displayName, me.avatar]);

  // What this machine knows, read once and only on the desktop. It is offered,
  // never applied: see `lib/device-identity.ts`.
  useEffect(() => {
    if (!open) return;
    let live = true;
    void readDeviceIdentity().then((identity) => {
      if (live) setDevice(identity);
    });
    return () => {
      live = false;
    };
  }, [open]);

  // The placeholder is the name the console would show if this field stayed
  // empty — the guess from the sign-in address — so an empty field reads as
  // "we'll call you this" rather than as a gap.
  const derived = guessName(me.email);
  // Offered only while it would actually change something: suggesting a name
  // that is already in the box is a button that does nothing.
  const suggestedName =
    device.fullName && device.fullName !== name.trim() ? device.fullName : null;

  const applyDevicePicture = useCallback(async () => {
    const file = pictureAsFile(device.pictureDataUrl);
    if (!file) return;
    setSaving(true);
    try {
      const { avatar: reference } = await uploadAvatar(client, company, file);
      setAvatar(reference);
    } catch {
      toast.error("That picture couldn't be used. Pick an image instead.");
    } finally {
      setSaving(false);
    }
  }, [client, company, device.pictureDataUrl]);

  async function save() {
    setSaving(true);
    try {
      // Three-state on the wire — omitted leaves a field alone, `null` goes
      // back to the default, a value sets it — so an untouched field is left
      // **off** the payload rather than echoed back. That matters for the
      // avatar above all: the host re-resolves a supplied reference, so a
      // name-only save that re-sent a `blob:` reference whose uploaded node
      // was deleted from Files would make the whole save fail — an unrelated
      // name edit held hostage by a face nobody asked about. (An unchanged
      // name can still be sent: `null` is the legitimate "call me the derived
      // name again", and `""` would be a name that renders as a gap.)
      const trimmed = name.trim();
      const nextName = trimmed === "" ? null : trimmed;
      const changes: { displayName?: string | null; avatar?: string | null } = {};
      // Only send the name when it would actually change something. A dialog
      // that has sat open while another client or an admin renamed us must not
      // write the stale boxed value back over the new one on an avatar-only
      // save — the same rule the avatar field already follows. `null` is still
      // sent when it means something: clearing an explicitly-set name back to
      // the derived one.
      if (nextName !== (me.displayName ?? null)) {
        changes.displayName = nextName;
      }
      if (avatar !== me.avatar) {
        changes.avatar = avatar ?? null;
      }
      const updated = await updateMe(client, company, changes);
      onSaved(updated);
      onOpenChange(false);
      toast.success("Profile updated.");
    } catch (error) {
      toast.error(error instanceof Error ? error.message : "Couldn't save your profile.");
    } finally {
      setSaving(false);
    }
  }

  const preview = name.trim() || derived || me.email;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Your profile</DialogTitle>
          <DialogDescription>
            How you appear to everyone else in this company — in chat, on approvals, and
            everywhere your name is on something.
          </DialogDescription>
        </DialogHeader>
        <div className="grid gap-4">
          <div className="grid gap-2">
            <Label htmlFor="profile-name">Name</Label>
            <Input
              id="profile-name"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={derived ?? "Your name"}
              data-testid="profile-name"
            />
            {suggestedName && (
              <button
                type="button"
                className="self-start text-xs text-muted-foreground underline underline-offset-2 hover:text-foreground"
                onClick={() => setName(suggestedName)}
                data-testid="profile-name-suggestion"
              >
                Use “{suggestedName}” from this computer
              </button>
            )}
          </div>
          <div className="grid gap-2">
            <Label>Icon</Label>
            <AvatarPicker
              client={client}
              company={company}
              value={avatar}
              seed={me.id || me.email}
              name={preview}
              tone={toneFor(me.id || me.email)}
              onChange={setAvatar}
              disabled={saving}
            />
            {device.pictureDataUrl && (
              <button
                type="button"
                className="flex items-center gap-2 self-start text-xs text-muted-foreground underline underline-offset-2 hover:text-foreground disabled:opacity-50"
                disabled={saving}
                onClick={() => void applyDevicePicture()}
                data-testid="profile-device-picture"
              >
                <img
                  src={device.pictureDataUrl}
                  alt=""
                  className="size-5 rounded-[4px] object-cover"
                />
                Use your account picture from this computer
              </button>
            )}
          </div>
        </div>
        <DialogFooter>
          <Button variant="ghost" onClick={() => onOpenChange(false)} disabled={saving}>
            Cancel
          </Button>
          <Button onClick={() => void save()} disabled={saving} data-testid="profile-save">
            Save
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
