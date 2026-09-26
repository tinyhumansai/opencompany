//! The mascot's costume and colors: closed, curated vocabularies for what a
//! `mascot:animated` wearer additionally overrides.
//!
//! [`crate::company::avatar`] owns the reference grammar — `mascot:<kind>`
//! names *which character*, and v1 ships exactly one (`"animated"`). This
//! module owns everything a teammate who wears it can further customise:
//! whether the live canvas plays at all (`MASCOT_MODES`), which of the
//! file's costumes it lands on (`MASCOT_COSTUMES`), and its two independent
//! colors (`MASCOT_SKIN_COLORS`, `MASCOT_HAND_COLORS`). None of this lives in
//! the `mascot:` reference string itself — see the module docs on
//! [`crate::company::avatar::AvatarRef::Mascot`] for why that string stays a
//! closed, simple form. These four choices are per-agent overrides instead,
//! the same [`AgentOverride`](crate::ports::types::AgentOverride) pattern the
//! rest of that struct already uses: `None` on the override means "the
//! file's own default", never a fourth stored value.
//!
//! ## Where these numbers came from
//!
//! The `.riv` file's costume-to-number mapping is **not** documented
//! anywhere Rive ships — it is a fact about one specific authored state
//! machine, readable only by loading the file and watching it play. The
//! costume names in `docs/issue/mascot-profile-avatar/rive-parameters.md`
//! were read from the file's embedded string table before any runtime had
//! played it, and one guess there was wrong: the table names a `"face
//! mask"` animation, but nothing resembling a face-covering mask ever
//! rendered across the file's addressable states. What actually renders at
//! the number this module calls `"habibi"` — a keffiyeh-style headdress —
//! is a distinct, unlisted costume the string table's naming did not
//! predict. [`MASCOT_COSTUMES`] reflects what was actually watched play
//! (`@rive-app/canvas`, `mascotAnimationNumber` cycled 1–13, each landing
//! frame screenshotted), not the pre-runtime guess.
//!
//! Number `4` is deliberately excluded from the nine. It transitions via the
//! same `"cap dance"` clip name `1` does, but settles on a visibly different
//! frame depending on which number the state machine was previously on —
//! path-dependent rather than a stable, addressable costume. A curated
//! picker needs every entry to render the same face on every reload
//! regardless of what a teammate wore before, so an unstable slot has no
//! seat at this table. Numbers above `10` were watched too: `11` caught the
//! artboard mid-gesture (a wave) rather than at rest, and `12`/`13` echoed
//! `10`'s own resting frame rather than introducing a new one — evidence
//! there is no addressable tenth costume among them either.

use crate::Result;
use crate::error::OpenCompanyError;

/// Whether the live Rive canvas plays at all for a `mascot:animated` wearer.
///
/// `"static"` renders one frozen frame — the chosen costume, at rest, with no
/// hover or "replying" reactivity wired up at all (not merely visually
/// still: [`crate::company::avatar`]'s posture on `mascot:` applies here too
/// — a mode is a fact about behaviour, so the frontend must not attach the
/// handlers in the first place rather than attach and ignore them).
/// `"animated"` is what v1 originally shipped: the live canvas, reactive to
/// hover, cycling from the chosen costume as its baseline. `"animated"` is
/// the file's own default when nobody has overridden the mode, matching what
/// every existing `mascot:animated` wearer already saw before this override
/// existed.
pub const MASCOT_MODES: [&str; 2] = ["animated", "static"];

/// The nine costumes a `mascot:animated` wearer may land on, by id.
///
/// **Must stay in step with `MASCOT_COSTUMES` in `frontend/src/lib/
/// mascot.ts`**, the same twinning [`crate::company::avatar::TINY_FLAVOURS`]
/// needs with its frontend counterpart: an id accepted here with no frontend
/// entry has nothing to render, an id the frontend offers that this list
/// refuses is a `400` the console never explains. See the module docs for
/// where these ids and their numbers came from — screenshotted against a
/// running instance of the file, not read off its string table.
pub const MASCOT_COSTUMES: [&str; 9] = [
    "cap",
    "headphones",
    "headband",
    "glass1",
    "habibi",
    "cardboard_mask",
    "glass2",
    "glass3",
    "glass4",
];

/// The `mascotAnimationNumber` ViewModel value for each [`MASCOT_COSTUMES`]
/// entry, in the same order. A parallel array rather than a struct-valued
/// list because the Rust side only ever validates the id — the number is a
/// frontend-only fact (`frontend/src/lib/mascot.ts` carries the same pairing
/// for the one caller that actually drives the canvas), but keeping it here
/// too is what let the module doc above cite one number and mean the same
/// thing on both sides.
pub const MASCOT_COSTUME_NUMBERS: [u8; 9] = [1, 2, 3, 5, 6, 7, 8, 9, 10];

/// The mascot's own default costume — the file's resting frame before any
/// teammate had a reason to choose one, and what an unset override falls
/// back to. Matches the pre-existing `STATE_NUMBERS.idle` constant this
/// override widens rather than replaces.
pub const DEFAULT_MASCOT_COSTUME: &str = "cap";

/// The six curated skin-color swatches, by id, paired 1:1 with
/// [`MASCOT_SKIN_COLOR_HEXES`]. Curated rather than an open color picker for
/// the reason `docs/issue/mascot-profile-avatar/state-mapping.md` already
/// gives for shipping one fixed colorway in v1: the mascot's `skinColor` is
/// the character's literal body color, not an abstract UI accent, and which
/// hues read as "friendly mascot" rather than "off-model" is a call for
/// whoever looks at it rendered — six were screenshotted together and kept
/// because every one reads as an intentional, on-model variant rather than a
/// palette-generator guess.
pub const MASCOT_SKIN_COLORS: [&str; 6] = ["default", "peach", "mint", "sky", "lavender", "coral"];

/// The RGB hex (no `#`, lowercase) for each [`MASCOT_SKIN_COLORS`] entry, in
/// the same order. `"default"` is the file's own shipped `skinColor`
/// (`#F7D145`) — kept as an explicit, named entry rather than an implicit
/// absence, so resetting to it is a real choice in the swatch row rather
/// than a hidden seventh state.
pub const MASCOT_SKIN_COLOR_HEXES: [&str; 6] =
    ["f7d145", "f5b88a", "a8e6c1", "9ccdf0", "c9b6e4", "f08a8a"];

/// The six curated hand-color swatches, by id, paired 1:1 with
/// [`MASCOT_HAND_COLOR_HEXES`]. `handColor` is the mascot's second
/// independent color — a small accent visible at the hands/arms rather than
/// the body fill — screenshotted against the same costume the skin swatches
/// were, to confirm the two properties are what they claim (skin repaints
/// the whole body; hand repaints only the accent) before curating either.
pub const MASCOT_HAND_COLORS: [&str; 6] = ["default", "charcoal", "teal", "rose", "plum", "forest"];

/// The RGB hex (no `#`, lowercase) for each [`MASCOT_HAND_COLORS`] entry, in
/// the same order. `"default"` is the file's own shipped `handColor`
/// (`#B4900B`), named for the same reason the skin default is.
pub const MASCOT_HAND_COLOR_HEXES: [&str; 6] =
    ["b4900b", "3a3a3a", "2f8f86", "d86a8c", "7a4f8c", "3f7a45"];

/// Whether `mode` is one of [`MASCOT_MODES`].
pub fn is_valid_mode(mode: &str) -> bool {
    MASCOT_MODES.contains(&mode)
}

/// Whether `costume` is one of [`MASCOT_COSTUMES`].
pub fn is_valid_costume(costume: &str) -> bool {
    MASCOT_COSTUMES.contains(&costume)
}

/// Whether `color` is one of [`MASCOT_SKIN_COLORS`].
pub fn is_valid_skin_color(color: &str) -> bool {
    MASCOT_SKIN_COLORS.contains(&color)
}

/// Whether `color` is one of [`MASCOT_HAND_COLORS`].
pub fn is_valid_hand_color(color: &str) -> bool {
    MASCOT_HAND_COLORS.contains(&color)
}

/// Validates a submitted mode, refusing anything outside [`MASCOT_MODES`].
///
/// Mirrors [`crate::company::avatar::parse`]'s refusal shape: name the
/// accepted set, because the commonest way to get a closed field wrong is to
/// send yesterday's value from a client that cached an older list.
pub fn parse_mode(value: &str) -> Result<&str> {
    let value = value.trim();
    if is_valid_mode(value) {
        Ok(value)
    } else {
        Err(OpenCompanyError::InvalidRequest(format!(
            "\"{value}\" isn't a mascot display mode. Pick one of: {}.",
            MASCOT_MODES.join(", ")
        )))
    }
}

/// Validates a submitted costume, refusing anything outside
/// [`MASCOT_COSTUMES`].
pub fn parse_costume(value: &str) -> Result<&str> {
    let value = value.trim();
    if is_valid_costume(value) {
        Ok(value)
    } else {
        Err(OpenCompanyError::InvalidRequest(format!(
            "\"{value}\" isn't one of the mascot's costumes. Pick one of: {}.",
            MASCOT_COSTUMES.join(", ")
        )))
    }
}

/// Validates a submitted skin color, refusing anything outside
/// [`MASCOT_SKIN_COLORS`].
pub fn parse_skin_color(value: &str) -> Result<&str> {
    let value = value.trim();
    if is_valid_skin_color(value) {
        Ok(value)
    } else {
        Err(OpenCompanyError::InvalidRequest(format!(
            "\"{value}\" isn't one of the mascot's skin colors. Pick one of: {}.",
            MASCOT_SKIN_COLORS.join(", ")
        )))
    }
}

/// Validates a submitted hand color, refusing anything outside
/// [`MASCOT_HAND_COLORS`].
pub fn parse_hand_color(value: &str) -> Result<&str> {
    let value = value.trim();
    if is_valid_hand_color(value) {
        Ok(value)
    } else {
        Err(OpenCompanyError::InvalidRequest(format!(
            "\"{value}\" isn't one of the mascot's hand colors. Pick one of: {}.",
            MASCOT_HAND_COLORS.join(", ")
        )))
    }
}

#[cfg(test)]
#[path = "mascot_tests.rs"]
mod tests;
