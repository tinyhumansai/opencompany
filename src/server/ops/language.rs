//! Prosumer glossary strings for server-authored `ops` responses.
//!
//! [`docs/spec/glossary.md`](../../../docs/spec/glossary.md) is the normative
//! vocabulary and its translation table is binding: server-authored text uses
//! the right-hand ("what the Operator sees") column and never leaks runtime
//! internals. These consts are the single source for the strings the write
//! plane emits, mirroring `frontend/src/lib/language.ts`.

/// The default desk name attributed to chat turns with no explicit desk (and to
/// pre-threading history). Prosumer word for a group-chat channel.
pub const DEFAULT_DESK: &str = "General";

/// A teammate (never "agent") — the prosumer word for a roster member.
pub const TEAMMATE: &str = "teammate";

/// Error shown when a write targets a built-in that cannot be removed.
pub const BUILTIN_UNINSTALL: &str =
    "This is a built-in skill and can't be uninstalled — you can disable it instead.";

/// Error shown when a write tries to remove a teammate defined in the manifest.
pub const MANIFEST_TEAMMATE_DELETE: &str =
    "This teammate is part of your company's blueprint and can't be removed here.";

/// Error shown when a write tries to remove a desk member defined in the
/// manifest (only operator-added members can be removed at runtime).
pub const MANIFEST_DESK_MEMBER_DELETE: &str =
    "This teammate is on the desk in your company's blueprint and can't be removed here.";

/// Error shown when a workspace move would create a cycle.
pub const WORKSPACE_CYCLE: &str = "You can't move a folder into itself.";

/// Error shown when a custom skill is missing its required fields.
pub const SKILL_FIELDS_REQUIRED: &str = "A skill needs a name and a description.";
