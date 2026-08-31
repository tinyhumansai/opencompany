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

/// Error shown when removing a teammate would leave the company with nobody.
///
/// The one refusal the roster keeps. A blueprint teammate *can* be removed —
/// the runtime records a tombstone rather than rewriting `company.toml` — but a
/// company with an empty roster has nobody to answer a message, nobody to
/// delegate to and no orchestrator, and the console offers no way back from it.
pub const LAST_TEAMMATE_DELETE: &str = concat!(
    "This is your company's last teammate. ",
    "Add another one before removing this one.",
);

/// Error shown when a write tries to remove a desk member defined in the
/// manifest (only operator-added members can be removed at runtime).
pub const MANIFEST_DESK_MEMBER_DELETE: &str =
    "This teammate is on the desk in your company's blueprint and can't be removed here.";

/// Error shown when a write tries to delete a desk defined in the manifest (only
/// operator-created desks can be deleted at runtime).
pub const MANIFEST_DESK_DELETE: &str =
    "This desk is part of your company's blueprint and can't be deleted here.";

/// The name of the company-wide channel, rendered after the `#` in the console
/// (`frontend/src/lib/desks.ts` `GENERAL_CHANNEL`). Not a desk id: every
/// spelling of it folds to the General conversation through
/// [`is_general_chat`](crate::server::chat_history::is_general_chat), which is
/// what makes it addressable without anything being stored for it.
pub const GENERAL_CHANNEL: &str = "general";

/// Error shown when a write aims a desk mutation at the built-in `#general`
/// channel — a delete, a membership add or removal, or a hierarchy reorder.
///
/// `#general` is not a desk. It has no lead and no hierarchy, and its
/// membership is the whole roster computed at read time, so there is nothing
/// for any of those writes to change. The refusal says which of the three it is
/// declining rather than reporting a bare "not found": an id the host
/// deliberately reserves is a very different fact from an id nobody ever
/// created (issue #1743).
pub const GENERAL_CHANNEL_IMMUTABLE: &str = concat!(
    "#general is the company-wide channel every teammate is in. ",
    "It isn't a desk — it has no lead and no membership of its own — ",
    "so it can't be deleted, staffed, or reordered.",
);

/// Error shown when a desk create asks for an id that would shadow the built-in
/// `#general` channel.
pub const GENERAL_CHANNEL_RESERVED: &str = concat!(
    "That id is reserved for the built-in #general channel. ",
    "Give the desk another name.",
);

/// Error shown when a workspace move would create a cycle.
pub const WORKSPACE_CYCLE: &str = "You can't move a folder into itself.";

/// Error shown when a custom skill is missing its required fields.
pub const SKILL_FIELDS_REQUIRED: &str = "A skill needs a name and a description.";

/// Error shown when an install names a slug the shared skill library lacks.
pub const SKILL_NOT_IN_REGISTRY: &str = "That skill isn't in the registry.";

/// Error shown when a workflow id is not safe to use as a filename.
pub const WORKFLOW_ID_INVALID: &str =
    "A workflow id can't be empty or contain slashes or `..` — use a plain name.";
