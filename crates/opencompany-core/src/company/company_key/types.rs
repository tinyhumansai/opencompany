//! Wire-adjacent shapes for [`super::fan_out`] (keys rework, issue #2306,
//! slice 4a): what each slot the account-key save touches did, and the report
//! that adds up to.
//!
//! Kept in its own file, as the plan asks, so [`fan_out`](super::fan_out)
//! reads as pure logic over these shapes rather than logic entangled with
//! their definitions.

use serde::Serialize;

use crate::error::UsedBy;

/// One of the five things a single `PUT …/credential` can touch.
///
/// Always reported in this order — see [`FanOutReport::slots`] — because that
/// is also roughly the causal order: the account key lands first, its copies
/// follow, the row and default depend on the copies, and health is asked last
/// (Q6: before any row or default write, but after the copies exist to probe
/// with).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Slot {
    Composio,
    Inference,
    Provider,
    Default,
    Health,
}

/// Why a slot was left alone rather than written.
///
/// Named reasons rather than a string, so [`fan_out_note`](super::fan_out_note)
/// and the wire `detail` field can match on them exhaustively instead of
/// re-deriving "why" from a value that has already been thrown away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// The slot already holds the new key — nothing changed.
    AlreadyCurrent,
    /// The slot holds a key that is neither empty nor the old account key —
    /// somebody set this slot on its own page, and it is not this save's to
    /// touch.
    CustomKey,
    /// A clear, and the slot was already empty.
    AlreadyEmpty,
    /// A `tinyhumans` row already exists — the row slot only.
    RowExists,
    /// Entry zero is managed (`inference/config`); `put_provider` refuses the
    /// slug, so a `tinyhumans` row is never created underneath it.
    LegacyManagedConfig,
    /// No model was sent and none is on the row — the row or default slot
    /// cannot be written without one.
    NeedsModel,
    /// `inference/default` is already `ProviderOnly` or `Full` — never
    /// overwritten by this path (Q1: a bare slug is still "set").
    DefaultAlreadySet,
    /// The LLM key slot does not hold the new key (it stayed on a custom key,
    /// or the write to it failed) — the row/default/health slots have
    /// nothing new to act on.
    InferenceNotWritten,
    /// The health probe answered `auth` — the LLM copy was rolled back, so
    /// the row and default have nothing to build on (Q6).
    InferenceRejected,
    /// This request cleared the account key — the row and default are never
    /// touched by a clear (§6, case C1).
    KeyCleared,
    /// A `tinyhumans` row exists and no default is set yet, but the row is
    /// disabled — the default slot is left alone rather than pointing the
    /// company's default at a provider it cannot currently serve through
    /// (P3-8, keys rework #2306 review).
    ProviderDisabled,
}

/// What happened to one [`Slot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotOutcome {
    /// The slot was empty and now holds the new key.
    Filled,
    /// The slot held the old account key and now holds the new one.
    Rotated,
    /// The slot held the old account key (or, for health, a prior probe
    /// result) and now holds nothing.
    Cleared,
    /// A health rejection (Q6) undid this request's own write to this slot.
    RolledBack,
    /// Left alone, and worth saying why in a way that is not "nothing
    /// happened" — the slot already agreed with what this request wanted.
    Kept(SkipReason),
    /// Left alone because this request could not or should not touch it.
    Skipped(SkipReason),
    /// A store write failed. The wire `detail` for this is the fixed string
    /// `"store"` — see [`super::super::company_key`]'s module docs on why the
    /// value itself is never in scope for a log or a wire field, and the
    /// same discipline applies to a failure detail.
    Failed,
    /// Health slot only: the probe succeeded.
    HealthOk,
    /// Health slot only: the probe failed, classified.
    HealthFailed(crate::company::inference::probe::ProbeClass),
}

/// One slot's report line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotReport {
    pub slot: Slot,
    pub outcome: SlotOutcome,
}

/// What a `PUT …/credential` asks [`fan_out`](super::fan_out) to do.
///
/// `model` is `None` on a bare key save (or a clear); `Some` names the model a
/// `tinyhumans` row should carry if one gets created. Borrowed rather than
/// owned: the caller (a deserialized request body) already owns the strings,
/// and `fan_out` never needs to hold either past its own call.
pub struct FanOutRequest<'a> {
    pub key: &'a str,
    pub model: Option<&'a str>,
    /// Confirms a clear the in-use guard would otherwise refuse
    /// (`docs/key-reworks/in-use-guards.md` §2). Ignored on a set/rotate,
    /// which [`fan_out`](super::fan_out) never guards. `finish_link`'s grant
    /// flow passes `true` unconditionally — a grant never clears (Q10), so
    /// the flag never gates anything there.
    pub confirm_in_use: bool,
    /// The TinyHumans OpenRouter proxy base a minted `tinyhumans` row carries
    /// and the health probe reads — `catalogue::tinyhumans_proxy_url(api_url)`
    /// from the caller's `AppConfig`, so the row follows the platform this
    /// instance is configured for (`TINYHUMANS_API_URL`) rather than always
    /// production. `None` means the catalogue's production endpoint, which is
    /// what every existing test asks for.
    pub proxy_base_url: Option<&'a str>,
}

/// Everything a `PUT …/credential` needs to answer with, once the fan-out
/// completes.
///
/// **Never holds a key.** Every `String` field here is a catalog model id —
/// nothing else in this struct may ever become one, which is what
/// [`no_report_or_note_contains_a_key`](super::fan_out::test) exists to keep
/// true by construction rather than by discipline.
#[derive(Clone, Debug, Default)]
pub struct FanOutReport {
    /// Always in order: composio, inference, provider, default, health.
    pub slots: Vec<SlotReport>,
    /// Whether a `tinyhumans` row could not be created or defaulted for want
    /// of a model — the console's cue to ask for one.
    pub needs_model: bool,
    /// Whether a model sent on a follow-up request would also become the
    /// company default (i.e. no default is set yet).
    pub sets_default: bool,
    /// Catalog ids to offer, only ever populated alongside `needs_model` and
    /// only when the health probe succeeded. Sorted, deduped, capped — see
    /// `catalogue_offer`.
    pub models: Vec<String>,
    /// Whether the Q6 auth rollback on the inference slot
    /// (`SlotOutcome::RolledBack`) restored a genuine prior key — i.e. this
    /// request was a **rotation**, not a first-time fill (P2-2, keys rework
    /// #2306 review). `false` on every report where the inference slot never
    /// rolled back, and also `false` when it did but there was nothing to
    /// restore (a fill being undone, not a rotation). [`fan_out_note`] reads
    /// this to say specifically that the LLM page still uses the *previous*
    /// key, rather than only that the new one "was not kept".
    pub rollback_had_prior_key: bool,
    /// What a clear would strand, computed atomically under `slot_guard`
    /// before anything is written (P3-6, keys rework #2306 review) —
    /// `Some` only on a **confirmed** clear that had something to warn
    /// about, so a caller can echo it in a success response exactly as
    /// `docs/key-reworks/in-use-guards.md` §3 asks. `None` on every other
    /// report: a set/rotate, an unconfirmed clear (which never reaches a
    /// report — it returns `Err` instead), or a clear with nothing to warn
    /// about.
    pub used_by: Option<UsedBy>,
}
