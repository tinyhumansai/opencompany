// What this machine already knows about the person using it.
//
// Read once, to prefill a profile nobody has filled in yet: someone signing in
// for the first time should not have to type a name their computer has known
// since it was set up, or go looking for a picture that is already on it.
//
// **A suggestion, never an import.** Nothing here is saved on its own. The
// profile form offers what it finds and the person accepts, edits or ignores it,
// at which point what gets stored is a decision rather than their laptop's idea
// of who they are, published to their colleagues.
//
// Desktop only — a browser cannot read any of this, and should not be able to.
// In a browser every field is absent and the form simply starts from the name
// derived from the sign-in address (`lib/person.ts`).

import { tauriCore } from "@/api/transport/bridge";

/** What `oc_device_identity` answers with. Every field optional. */
export interface DeviceIdentity {
  /** The account's login name — `enamakel`. */
  username?: string;
  /** The account's full name — "Steven Enamakel" — where the OS holds one. */
  fullName?: string;
  /** The account picture as a `data:` URL, where the OS holds one. */
  pictureDataUrl?: string;
}

/**
 * Asks this machine who is using it. `{}` in a browser, or where nothing is set.
 *
 * Never throws: a prefill that cannot happen is a form somebody fills in
 * themselves, which is exactly what happens today, so a failure here must not
 * be able to keep a profile dialog from opening.
 */
export async function readDeviceIdentity(): Promise<DeviceIdentity> {
  try {
    const core = tauriCore();
    if (!core) return {};
    return (await core.invoke<DeviceIdentity>("oc_device_identity")) ?? {};
  } catch {
    return {};
  }
}

/**
 * Turns the account picture into a `File` the avatar upload can take.
 *
 * The data URL is decoded here rather than handed to the host, because an
 * avatar is stored as bytes this host holds and a `data:` URL is not a reference
 * the grammar accepts (`lib/avatar.ts`) — so the picture goes through exactly
 * the same upload as one chosen from a file dialog, and is subject to exactly
 * the same sniffing and ceiling. There is no shortcut for it, deliberately.
 *
 * `null` when the URL is absent or malformed.
 */
export function pictureAsFile(dataUrl: string | undefined): File | null {
  if (!dataUrl) return null;
  const match = /^data:([^;,]+);base64,(.*)$/s.exec(dataUrl);
  if (!match) return null;
  const [, mime, base64] = match;
  try {
    const binary = atob(base64);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    // The extension follows the type rather than the other way round: the file
    // this came from may have had none at all (`~/.face` conventionally does).
    const extension = mime.split("/")[1]?.replace("jpeg", "jpg") ?? "png";
    return new File([bytes], `account-picture.${extension}`, { type: mime });
  } catch {
    return null;
  }
}
