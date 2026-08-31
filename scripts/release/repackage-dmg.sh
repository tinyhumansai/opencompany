#!/usr/bin/env bash
# Re-create and notarize a DMG after the .app has been notarized.
#
# Ported ~verbatim from openhuman. OpenCompany has no CEF assumptions, so the
# only OpenCompany-specific bits are the volume name and the absence of a DMG
# background image (Tauri did not declare one, so we rebuild without a
# background; override with DMG_BACKGROUND_PATH if one is ever added).
#
# Usage:
#   repackage-dmg.sh <app_path> <bundle_dir>
#
# Required environment variables:
#   APPLE_ID
#   APPLE_PASSWORD    (app-specific password)
#   APPLE_TEAM_ID
#
# Why a full rebuild instead of mount-and-replace:
#
# Converting the original Tauri-built UDZO DMG to UDRW, mounting it, replacing
# the .app, and converting back fails consistently on recent macOS runners with
# `hdiutil: convert failed - internal error`. Tauri's own vendored
# bundle_dmg.sh builds a fresh UDRW from a source folder via
# `hdiutil create -srcfolder` and converts to UDZO; that path works. So we reuse
# it (already on disk under `<bundle_dir>/dmg/bundle_dmg.sh` from the tauri-build
# step) and rebuild the DMG around the now-notarized .app.
set -euo pipefail

APP_PATH="${1:?Usage: repackage-dmg.sh <app_path> <bundle_dir>}"
BUNDLE_DIR="${2:?}"

# Resolve all bundle paths to absolute form — we cd into $MACOS_DIR below to
# invoke bundle_dmg.sh, and relative paths would break after the cd.
BUNDLE_DIR_ABS="$(cd "$BUNDLE_DIR" && pwd)"
DMG_DIR="$BUNDLE_DIR_ABS/dmg"
MACOS_DIR="$BUNDLE_DIR_ABS/macos"
BUNDLE_SCRIPT="$DMG_DIR/bundle_dmg.sh"
SUPPORT_DIR="$BUNDLE_DIR_ABS/share/create-dmg/support"

if [ ! -x "$BUNDLE_SCRIPT" ]; then
  echo "[dmg] ERROR: bundle_dmg.sh not found at $BUNDLE_SCRIPT" >&2
  echo "[dmg]        Did the original tauri-build step run successfully?" >&2
  exit 1
fi
if [ ! -d "$SUPPORT_DIR" ]; then
  echo "[dmg] ERROR: support dir not found at $SUPPORT_DIR" >&2
  exit 1
fi
if [ ! -d "$APP_PATH" ]; then
  echo "[dmg] ERROR: app bundle not found at $APP_PATH" >&2
  exit 1
fi
APP_PATH_ABS="$(cd "$APP_PATH/.." && pwd)/$(basename "$APP_PATH")"
APP_NAME="$(basename "$APP_PATH")"

# The .app must be inside $MACOS_DIR for the bundle_dmg.sh srcfolder arg. If the
# caller passed an .app from a different location, copy it into place so
# bundle_dmg.sh picks up the right (notarized) bundle.
if [ "$APP_PATH_ABS" != "$MACOS_DIR/$APP_NAME" ]; then
  echo "[dmg] Staging $APP_NAME into $MACOS_DIR"
  rm -rf "${MACOS_DIR:?}/${APP_NAME:?}"
  ditto "$APP_PATH_ABS" "$MACOS_DIR/$APP_NAME"
fi

# Capture the existing DMG name so the rebuild outputs to the same path.
# tauri-build always produces exactly one .dmg per target.
ORIGINAL_DMG="$(find "$DMG_DIR" -maxdepth 1 -name '*.dmg' ! -name 'rw.*.dmg' -type f 2>/dev/null | head -1 || true)"
if [ -z "$ORIGINAL_DMG" ]; then
  echo "[dmg] No DMG found in $DMG_DIR — nothing to repackage" >&2
  exit 0
fi
DMG_NAME="$(basename "$ORIGINAL_DMG")"
FINAL_DMG="$DMG_DIR/$DMG_NAME"
echo "[dmg] Rebuilding $DMG_NAME from notarized $APP_NAME"

# OpenCompany's Tauri config declares no DMG background image, so we rebuild
# without one. Override via env if a background is ever added.
BACKGROUND_PATH="${DMG_BACKGROUND_PATH:-}"
if [ -n "$BACKGROUND_PATH" ] && [ ! -f "$BACKGROUND_PATH" ]; then
  echo "[dmg] WARNING: background image not found at $BACKGROUND_PATH — building without background" >&2
  BACKGROUND_PATH=""
fi

# ── Cleanup ──────────────────────────────────────────────────────────────────
cleanup() {
  set +e
  if [ -n "${VERIFY_MOUNT:-}" ] && [ -d "$VERIFY_MOUNT" ]; then
    hdiutil detach "$VERIFY_MOUNT" -force 2>/dev/null || true
    rmdir "$VERIFY_MOUNT" 2>/dev/null || true
  fi
  # bundle_dmg.sh writes scratch files alongside the output — clean any leftovers.
  find "$DMG_DIR" -maxdepth 1 -name 'rw.*.dmg' -delete 2>/dev/null || true
  find "$MACOS_DIR" -maxdepth 1 -name 'rw.*.dmg' -delete 2>/dev/null || true
}
trap cleanup EXIT

# Pre-clean any leftover scratch DMGs from prior failed runs.
find "$DMG_DIR" -maxdepth 1 -name 'rw.*.dmg' -delete 2>/dev/null || true
find "$MACOS_DIR" -maxdepth 1 -name 'rw.*.dmg' -delete 2>/dev/null || true
rm -f "$FINAL_DMG"

BUNDLE_ARGS=(
  --volname "OpenCompany"
  --icon "$APP_NAME" 180 170
  --app-drop-link 480 170
  --window-size 660 400
  --hide-extension "$APP_NAME"
  --skip-jenkins
)
if [ -n "$BACKGROUND_PATH" ]; then
  BACKGROUND_ABS="$(cd "$(dirname "$BACKGROUND_PATH")" && pwd)/$(basename "$BACKGROUND_PATH")"
  BUNDLE_ARGS+=(--background "$BACKGROUND_ABS")
fi

echo "[dmg] Running bundle_dmg.sh..."
(
  cd "$MACOS_DIR"
  bash "$BUNDLE_SCRIPT" "${BUNDLE_ARGS[@]}" "$FINAL_DMG" "$APP_NAME"
)

if [ ! -f "$FINAL_DMG" ]; then
  echo "[dmg] ERROR: bundle_dmg.sh did not produce $FINAL_DMG" >&2
  exit 1
fi
echo "[dmg] Built fresh DMG at $FINAL_DMG ($(du -h "$FINAL_DMG" | cut -f1))"

DMG_PATH="$FINAL_DMG"

echo "[dmg] Notarizing DMG..."
DMG_SUBMIT_OUT="$(mktemp /tmp/notarize-dmg-XXXXXX.json)"
set +e
xcrun notarytool submit "$DMG_PATH" \
  --apple-id "$APPLE_ID" \
  --password "$APPLE_PASSWORD" \
  --team-id "$APPLE_TEAM_ID" \
  --output-format json \
  --wait > "$DMG_SUBMIT_OUT"
DMG_SUBMIT_RC=$?
set -e
cat "$DMG_SUBMIT_OUT"

DMG_SUBMISSION_ID="$(/usr/bin/python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("id",""))' "$DMG_SUBMIT_OUT" 2>/dev/null || true)"
DMG_SUBMISSION_STATUS="$(/usr/bin/python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("status",""))' "$DMG_SUBMIT_OUT" 2>/dev/null || true)"
rm -f "$DMG_SUBMIT_OUT"

if [ -n "$DMG_SUBMISSION_ID" ]; then
  echo "[dmg] Fetching notarytool developer log for $DMG_SUBMISSION_ID:"
  xcrun notarytool log "$DMG_SUBMISSION_ID" \
    --apple-id "$APPLE_ID" \
    --password "$APPLE_PASSWORD" \
    --team-id "$APPLE_TEAM_ID" || true
fi

if [ "$DMG_SUBMISSION_STATUS" != "Accepted" ] || [ "$DMG_SUBMIT_RC" -ne 0 ]; then
  echo "[dmg] ERROR: DMG notarization did not succeed (status=$DMG_SUBMISSION_STATUS, rc=$DMG_SUBMIT_RC)" >&2
  exit 1
fi

xcrun stapler staple "$DMG_PATH"
echo "[dmg] DMG notarization complete: $DMG_PATH"

# ── Final verification ───────────────────────────────────────────────────────
echo "[dmg] Verifying final DMG layout..."
VERIFY_MOUNT="$(mktemp -d /tmp/OpenCompany-Verify-XXXXXX)"
hdiutil attach "$DMG_PATH" -mountpoint "$VERIFY_MOUNT" -noautoopen

if [ ! -d "$VERIFY_MOUNT/$APP_NAME" ]; then
  echo "[dmg] ERROR: $APP_NAME missing in final DMG" >&2
  exit 1
fi
if [ ! -L "$VERIFY_MOUNT/Applications" ]; then
  echo "[dmg] ERROR: Applications symlink missing in final DMG" >&2
  exit 1
fi

hdiutil detach "$VERIFY_MOUNT"
rmdir "$VERIFY_MOUNT"
VERIFY_MOUNT=""
echo "[dmg] Verification successful: layout preserved."
