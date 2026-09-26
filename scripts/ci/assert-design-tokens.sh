#!/usr/bin/env bash
#
# Fail if the console reaches past the design system's token layer.
#
# Four things are checked, and each one is debt that returns a single
# convenient exception at a time:
#
#   1. Arbitrary font sizes — `text-[11px]`. The scale has a name for every
#      rung the console uses, including the dense ones (`text-2xs`, `text-3xs`).
#      A size written inline is a decision the system cannot see or change.
#
#   2. SVG labels smaller than the 10px floor — `fixedLabel(9)`. SVG styles
#      bypass Tailwind classes, but they are still typography and must stay on
#      the same scale.
#
#   3. Raw Tailwind palette colours — `text-emerald-600`, `bg-amber-500/10`.
#      Palette steps carry no meaning and are untuned for this canvas. State
#      belongs to `--status-*`, identity to `--tone-*`.
#
#   4. Raw hex — `#5865f2`. A hex cannot theme.
#
# Every one of these was cleared once (192, 87 and 26 sites respectively).
# This script is what stops them coming back, because a grep is cheaper than
# the argument.
#
# See docs/design-system/README.md.
set -uo pipefail

cd "$(dirname "$0")/../.." || exit 1
SRC=frontend/src

if [ ! -d "$SRC" ]; then
  echo "assert-design-tokens: $SRC not found (run from the repo root)" >&2
  exit 1
fi

FAILED=0

# `--include` twice: the palette leaked into plain .ts files too (the tone and
# category maps live in lib/ and api/), and an earlier sweep that only looked
# at .tsx reported a clean tree with 30 violations still in it.
INCLUDES=(--include=*.tsx --include=*.ts)

# Drop comment lines.
#
# `grep -rn` emits `file:line:content`, so a pattern anchored at `^` matches
# the *filename*, not the code — the first version of this filter did that and
# flagged a doc comment that names the anti-pattern it is warning about.
# Anchor past the two fields instead.
strip_comments() {
  grep -vE '^[^:]+:[0-9]+:[[:space:]]*(//|\*|/\*)' || true
}

report() {
  local title="$1" fix="$2" hits="$3"
  echo
  echo "✗ $title"
  echo "  $fix"
  echo
  echo "$hits" | sed 's/^/    /'
  FAILED=1
}

# ---- 1. arbitrary font sizes --------------------------------------------
# `text-[var(--x)]` is legitimate — that IS a token reference. Comments are
# skipped so a doc comment naming the anti-pattern does not trip the guard.
hits=$(grep -rn 'text-\[' "$SRC" "${INCLUDES[@]}" 2>/dev/null \
  | grep -v 'text-\[var(' \
  | strip_comments || true)
if [ -n "$hits" ]; then
  report "arbitrary font size" \
    "Use a named rung: text-3xs text-2xs text-xs text-sm text-base text-lg text-xl text-2xl" \
    "$hits"
fi

# ---- 2. SVG font sizes below the floor ----------------------------------
# `fixedLabel` is the graph's SVG font-size seam. A numeric literal below 10px
# is below the floor — one-digit, decimal, signed or leading-decimal forms all
# count; 10 itself and named constants stay reviewable, while this catches the
# literal escape hatch that Tailwind cannot.
hits=$(grep -rnE 'fixedLabel\([[:space:]]*[+-]?([0-9](\.[0-9]+)?|\.[0-9]+)[[:space:]]*[,)]' "$SRC" "${INCLUDES[@]}" 2>/dev/null \
  | strip_comments || true)
if [ -n "$hits" ]; then
  report "SVG font size below 10px" \
    "Use the 10px text-3xs floor (or a named constant set to it); below 10px is not a size." \
    "$hits"
fi

# ---- 3. raw Tailwind palette colours ------------------------------------
PALETTE='emerald|rose|amber|sky|red|green|blue|yellow|orange|violet|indigo|teal|cyan|fuchsia|purple|pink|lime|slate|stone|zinc'
PROPS='text|bg|border|from|to|via|ring|shadow|fill|stroke|divide|outline|decoration|accent|caret'
hits=$(grep -rnE "(^|[\"' ])($PROPS)-($PALETTE)-[0-9]" "$SRC" "${INCLUDES[@]}" 2>/dev/null \
  | strip_comments || true)
if [ -n "$hits" ]; then
  report "raw Tailwind palette colour" \
    "State -> --status-*, identity -> --tone-*, series -> --chart-*. See docs/design-system/color.md" \
    "$hits"
fi

# ---- 4. raw hex ----------------------------------------------------------
# connections.ts is one allowed file: eleven third-party provider brand
# colours, which identify someone else and are correct as literals. The field
# itself documents why.
#
# avatar.ts's `MASCOT_SKIN_COLORS`/`MASCOT_HAND_COLORS` are the other: these
# are not a Tailwind class, and the literals themselves live only in this one
# file — every other file reads a swatch's `hex` field as a JS value, so this
# check never sees a hex literal outside avatar.ts to begin with. Most of
# those reads (`mascot-avatar.tsx`'s `setHandColor`/`setSkinColor`, via
# `hexToRgb`) are `[r,g,b]` values written straight into the mascot's Rive
# canvas ViewModel — the same non-theming, identifies-a-specific-thing
# category as a brand colour, and canvas paint that `index.css` custom
# properties cannot reach without a read at draw time nothing here does. A
# swatch button in `avatar-picker.tsx` also paints one via `style=`, which
# *is* an ordinary CSS use — but it is still one curated, reviewed set of six
# per property, the same posture as the ten shipped `tiny:` tone tiles
# (`TeammateAvatar`'s own hash-to-hue, also exempt from this rule by being a
# runtime value rather than a literal), not a color chosen ad hoc per call
# site.
hits=$(grep -rn '#[0-9a-fA-F]\{6\}' "$SRC" "${INCLUDES[@]}" 2>/dev/null \
  | grep -v '^frontend/src/lib/connections.ts:' \
  | grep -v '^frontend/src/lib/avatar.ts:' \
  | strip_comments || true)
if [ -n "$hits" ]; then
  report "raw hex colour" \
    "Add a token to index.css. Third-party brand colours are the only exception (see --brand-discord)." \
    "$hits"
fi

if [ "$FAILED" -eq 0 ]; then
  echo "✓ design tokens: no arbitrary sizes, no palette colours, no stray hex"
fi
exit "$FAILED"
