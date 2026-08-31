#!/usr/bin/env bash
# Developer-ID sign a macOS .app bundle with the Hardened Runtime and submit it
# for Apple notarization.
#
# Ported from openhuman's script and SIMPLIFIED for OpenCompany: OpenCompany
# uses the system WebKit via Wry — there is no bundled Chromium/CEF framework,
# no nested Helper.app, and no external sidecar binary. So the whole inside-out
# Frameworks pass, the Helper.app loop, and the MacOS/ + Resources/ sidecar
# loops from the original are dropped. `codesign` on the single .app bundle
# recursively seals everything it contains.
#
# Usage:
#   sign-and-notarize-macos.sh <app_path> [entitlements_plist]
#
# Required environment variables:
#   APPLE_CERTIFICATE_BASE64
#   APPLE_CERTIFICATE_PASSWORD
#   APPLE_SIGNING_IDENTITY
#   APPLE_ID
#   APPLE_PASSWORD          (app-specific password)
#   APPLE_TEAM_ID
set -euo pipefail

APP_PATH="${1:?Usage: sign-and-notarize-macos.sh <app_path> [entitlements_plist]}"
ENTITLEMENTS="${2:-src-tauri/entitlements.plist}"

for var in APPLE_CERTIFICATE_BASE64 APPLE_CERTIFICATE_PASSWORD APPLE_SIGNING_IDENTITY APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID; do
  if [ -z "${!var:-}" ]; then
    echo "[sign] ERROR: Missing required env var: $var"
    exit 1
  fi
done

if [ ! -d "$APP_PATH" ]; then
  echo "[sign] ERROR: app bundle not found at $APP_PATH" >&2
  exit 1
fi
if [ ! -f "$ENTITLEMENTS" ]; then
  echo "[sign] ERROR: entitlements plist not found at $ENTITLEMENTS" >&2
  exit 1
fi

# ── Import signing certificate into a throwaway keychain ─────────────────────
KEYCHAIN="resign-$$.keychain-db"
KEYCHAIN_PW="$(openssl rand -base64 24)"
CERT_FILE="$(mktemp /tmp/cert-XXXXXX.p12)"

echo "$APPLE_CERTIFICATE_BASE64" | base64 --decode > "$CERT_FILE"
security create-keychain -p "$KEYCHAIN_PW" "$KEYCHAIN"
security set-keychain-settings -lut 21600 "$KEYCHAIN"
security unlock-keychain -p "$KEYCHAIN_PW" "$KEYCHAIN"
security import "$CERT_FILE" -k "$KEYCHAIN" \
  -P "$APPLE_CERTIFICATE_PASSWORD" \
  -T /usr/bin/codesign -T /usr/bin/security
security set-key-partition-list -S apple-tool:,apple: -k "$KEYCHAIN_PW" "$KEYCHAIN"
# Word-splitting is deliberate: the existing user keychains each become a
# separate argument so our throwaway keychain is prepended without dropping them.
# shellcheck disable=SC2046
security list-keychains -d user -s "$KEYCHAIN" $(security list-keychains -d user | tr -d '"')
rm -f "$CERT_FILE"
echo "[sign] Signing identity imported into $KEYCHAIN"

# ── Sign the .app bundle ─────────────────────────────────────────────────────
# A single Wry .app: codesign --force seals the main executable and any
# resources in one pass. Hardened runtime + entitlements + secure timestamp are
# all required for notarization to accept the bundle.
MAIN_EXE="$(defaults read "$APP_PATH/Contents/Info.plist" CFBundleExecutable 2>/dev/null || echo "OpenCompany")"
echo "[sign] Main executable (from plist): $MAIN_EXE"
echo "[sign] Bundle contents (MacOS/):"
ls -la "$APP_PATH/Contents/MacOS/"

echo "[sign] Signing .app bundle..."
codesign --force --options runtime \
  --entitlements "$ENTITLEMENTS" \
  --sign "$APPLE_SIGNING_IDENTITY" \
  --timestamp \
  "$APP_PATH"

# ── Verify ───────────────────────────────────────────────────────────────────
echo "[sign] Verifying signatures"
codesign --verify --deep --strict --verbose=2 "$APP_PATH"

# ── Notarize ─────────────────────────────────────────────────────────────────
echo "[sign] Notarizing..."
NOTARIZE_ZIP="$(mktemp /tmp/OpenCompany-notarize-XXXXXX.zip)"
ditto -c -k --keepParent "$APP_PATH" "$NOTARIZE_ZIP"

SUBMIT_OUT="$(mktemp /tmp/notarize-submit-XXXXXX.json)"
set +e
xcrun notarytool submit "$NOTARIZE_ZIP" \
  --apple-id "$APPLE_ID" \
  --password "$APPLE_PASSWORD" \
  --team-id "$APPLE_TEAM_ID" \
  --output-format json \
  --wait > "$SUBMIT_OUT"
SUBMIT_RC=$?
set -e

cat "$SUBMIT_OUT"
rm -f "$NOTARIZE_ZIP"

SUBMISSION_ID="$(/usr/bin/python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("id",""))' "$SUBMIT_OUT" 2>/dev/null || true)"
SUBMISSION_STATUS="$(/usr/bin/python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("status",""))' "$SUBMIT_OUT" 2>/dev/null || true)"
rm -f "$SUBMIT_OUT"

echo "[sign] notarytool exit=$SUBMIT_RC id=$SUBMISSION_ID status=$SUBMISSION_STATUS"

if [ -n "$SUBMISSION_ID" ]; then
  echo "[sign] Fetching notarytool developer log for $SUBMISSION_ID:"
  xcrun notarytool log "$SUBMISSION_ID" \
    --apple-id "$APPLE_ID" \
    --password "$APPLE_PASSWORD" \
    --team-id "$APPLE_TEAM_ID" || true
fi

if [ "$SUBMISSION_STATUS" != "Accepted" ] || [ "$SUBMIT_RC" -ne 0 ]; then
  echo "[sign] ERROR: notarization did not succeed (status=$SUBMISSION_STATUS, rc=$SUBMIT_RC)" >&2
  exit 1
fi

# ── Staple ───────────────────────────────────────────────────────────────────
echo "[sign] Stapling..."
xcrun stapler staple "$APP_PATH"

echo "[sign] Notarization complete"
