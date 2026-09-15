//! The account-key fan-out (keys rework, issue #2306, slice 4a): one
//! `PUT …/credential` copies [`super::KEY_KEY`] into the Composio and LLM
//! TinyHumans slots (Q7), adds the `tinyhumans` provider row when a model is
//! known, sets the company default only when none is set, and checks health —
//! all under a per-company lock so two concurrent saves cannot leave one slot
//! rotated and another still on the old value.
//!
//! See `docs/key-reworks/phase-4a-account-key-fanout.md` for the full design
//! and the data carry-over matrix (§6) every test here proves a row of.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;

use crate::Result;
use crate::company::composio;
use crate::company::inference::{self, catalogue, probe, store as inference_store};
use crate::error::{OpenCompanyError, UsedBy, UsedBySurface};
use crate::ports::SecretStore;
use crate::ports::types::{CompanyId, SecretValue};

use super::KEY_KEY;
use super::types::{FanOutReport, FanOutRequest, SkipReason, Slot, SlotOutcome, SlotReport};

// ---------------------------------------------------------------------------
// The per-company lock
// ---------------------------------------------------------------------------

/// One lock per company that has ever fanned out an account-key save.
///
/// A copy of the pattern `search::store`'s `INDEX_LOCKS` already uses: it
/// serialises mutations **within one process**, which is the whole of a
/// deployment (one container per tenant), and is not a distributed lock.
static SLOT_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

/// Takes this company's slot lock, held until the returned guard is dropped.
///
/// The inner `std` mutex is held only long enough to clone an `Arc` — never
/// across an await — so a panicking writer cannot poison anything a later
/// request needs.
pub async fn slot_guard(company: &CompanyId) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut locks = SLOT_LOCKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks
            .entry(company.as_ref().to_string())
            .or_default()
            .clone()
    };
    lock.lock_owned().await
}

// ---------------------------------------------------------------------------
// decide_copy — the whole Q7 rule, pure
// ---------------------------------------------------------------------------

/// What [`decide_copy`] says to do to one derived slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyDecision {
    /// Write `new` into the slot (a fill or a rotation — the caller tells
    /// those apart by whether `current` was empty).
    Write,
    /// Clear the slot (it held the old account key).
    Clear,
    /// Leave the slot as it is, and it is worth saying it agreed already.
    Keep(SkipReason),
    /// Leave the slot as it is, and say why nothing happened.
    Skip(SkipReason),
}

/// The Q7 rule (`docs/key-reworks/phase-4a-account-key-fanout.md` §3.2's
/// table), decided from three already-trimmed values: `current` (what the
/// slot holds now), `old_account` (what `tinyhumans/key` held before this
/// request), `new` (what this request wants `tinyhumans/key` to hold — empty
/// means a clear).
///
/// A derived slot is written when it is empty or equal to the old account
/// key; any other value was set on that slot's own page and is left alone.
pub fn decide_copy(current: &str, old_account: &str, new: &str) -> CopyDecision {
    let old_account_is_current = !old_account.is_empty() && current == old_account;
    if new.is_empty() {
        if current.is_empty() {
            CopyDecision::Skip(SkipReason::AlreadyEmpty)
        } else if old_account_is_current {
            CopyDecision::Clear
        } else {
            CopyDecision::Keep(SkipReason::CustomKey)
        }
    } else if current == new {
        CopyDecision::Keep(SkipReason::AlreadyCurrent)
    } else if current.is_empty() || old_account_is_current {
        CopyDecision::Write
    } else {
        CopyDecision::Keep(SkipReason::CustomKey)
    }
}

// ---------------------------------------------------------------------------
// account_key_used_by / account_key_in_use_message — the clear guard
// ---------------------------------------------------------------------------
//
// Moved here from `ops::company_key` (keys rework #2306, P3-6 review): the
// guard used to be computed by the HTTP handler BEFORE calling into
// `fan_out`, which takes `slot_guard` itself once entered. That left an
// unlocked window between "the handler decided this clear is safe" and "the
// clear actually runs under the lock" — a concurrent save landing in that
// window could make an unconfirmed clear strand a copy the other save had
// just filled, or make the 409 this returns echo a `usedBy` that was already
// stale by the time the caller read it. Computing it here, inline in
// [`fan_out`] right after the lock is held and before anything is written,
// closes that: the read, the decision and the write are now one atomic
// sequence under one lock, exactly like every other decision `fan_out` makes.

/// The `usedBy` an account-key **clear** would carry
/// (`docs/key-reworks/in-use-guards.md` §1's account-key row, §2's `surfaces`
/// table): grounded in the exact rule [`fan_out`] itself applies
/// ([`decide_copy`]), so "what would a clear touch" can never disagree with
/// what a clear actually does.
///
/// `"llm"` appears only when a clear would actually clear
/// `provider/tinyhumans/key` (its current value equals the account key,
/// i.e. `decide_copy` would `Clear` it) **and** a `tinyhumans` row already
/// exists (indexed) **or** entry zero itself is the legacy managed config
/// (P1-1, keys rework #2306, KR-L3-01 review — a key with no row and no
/// legacy managed entry zero behind it is not "set" (D-set/X5), so it can
/// never make `"llm"` appear). `"composio"` appears whenever the same is true
/// of `composio/tinyhumans/key` **and** `composio/mode` currently selects the
/// managed slot this clear would touch (P1-1: the same rule
/// [`super::super::composio`]'s own guard already applies — a company on
/// `byok` has nothing live resolving through `composio/tinyhumans/key`, so
/// clearing the account key cannot strand a Composio workload even though the
/// copy would still be cleared).
///
/// `None` when the account key itself is unset, or when neither derived slot
/// would actually be cleared (both already hold a custom key of their own, or
/// composio is on `byok`) — the account-key clear equivalent of matrix rows
/// M6/C2.
///
/// A plain read with no lock of its own — safe for a status route to call any
/// time (it races nothing it could corrupt) — but also exactly what
/// [`fan_out`] calls **while holding [`slot_guard`]** for the guard itself, so
/// the same function serves both a best-effort display read and the atomic
/// gate without the two ever computing the answer differently.
pub async fn account_key_used_by(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<Option<UsedBy>> {
    let old_account = secrets
        .get(company, KEY_KEY)
        .await?
        .map(|SecretValue(v)| v.trim().to_string())
        .unwrap_or_default();
    if old_account.is_empty() {
        return Ok(None);
    }

    let composio_now = composio::load_tinyhumans_key(company, secrets)
        .await?
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    let inference_now =
        inference::load_managed_key(company, secrets, &inference::HarnessScope::default())
            .await?
            .trim()
            .to_string();
    let providers = inference_store::list_providers(company, secrets).await?;
    let row_exists = providers.iter().any(|p| {
        p.origin == inference_store::ProviderOrigin::Indexed && p.slug == inference::MANAGED_SLUG
    });
    // P1-1 decision (keys rework #2306, KR-L3-01 review): count a company
    // still on the legacy managed entry zero (`inference/config`,
    // `ProviderOrigin::EntryZero`) as "llm in use" too, not just an indexed
    // row — see the doc comment above.
    let entry_zero_managed = providers.iter().any(|p| {
        p.origin == inference_store::ProviderOrigin::EntryZero && p.slug == inference::MANAGED_SLUG
    });
    let llm_in_use = row_exists || entry_zero_managed;

    // P1-1: `"composio"` only when `composio/mode` currently selects the slot
    // this clear would touch — see the doc comment above.
    let composio_mode = composio::load_mode(company, secrets).await?;

    let mut surfaces = Vec::new();
    let clears =
        |current: &str| matches!(decide_copy(current, &old_account, ""), CopyDecision::Clear);
    if clears(&inference_now) && llm_in_use {
        surfaces.push(UsedBySurface::Llm);
    }
    if clears(&composio_now) && composio_mode == composio::ComposioMode::Managed {
        surfaces.push(UsedBySurface::Composio);
    }

    if surfaces.is_empty() {
        return Ok(None);
    }
    Ok(Some(UsedBy {
        surfaces,
        ..Default::default()
    }))
}

/// One plain sentence naming every surface a clear would strand, e.g. "Used
/// by TinyHumans on the LLM page and by Composio." or, with one surface,
/// "Used by Composio." Matches the operator's pattern (2026-09-15 review) —
/// the upfront copy in `CredentialStatusDto.used_by` and the 409 this fills in
/// are built from the same phrases so they can never disagree.
fn account_key_in_use_message(surfaces: &[UsedBySurface]) -> String {
    let phrases: Vec<&str> = surfaces
        .iter()
        .map(|s| match s {
            UsedBySurface::Llm => "TinyHumans on the LLM page",
            UsedBySurface::Composio => "Composio",
            UsedBySurface::Search => "Search",
        })
        .collect();
    match phrases.split_last() {
        None => String::new(),
        Some((last, [])) => format!("Used by {last}."),
        Some((last, rest)) => format!("Used by {} and by {last}.", rest.join(", ")),
    }
}

// ---------------------------------------------------------------------------
// The inference probe seam
// ---------------------------------------------------------------------------

/// What [`fan_out`] asks to learn whether the LLM copy actually works.
///
/// A trait rather than a bare function so a test can answer without a
/// network — see `FakeProber` in this module's tests.
#[async_trait]
pub trait InferenceProber: Send + Sync {
    async fn probe(
        &self,
        base_url: &str,
        key: &str,
    ) -> std::result::Result<Vec<String>, probe::ProbeFailure>;
}

/// The real probe: the same read [`inference::store`]'s 2a provider-add path
/// uses for a `tinyhumans` row — one GET, paged, against the TinyHumans
/// OpenRouter proxy.
pub struct LiveProber;

#[async_trait]
impl InferenceProber for LiveProber {
    async fn probe(
        &self,
        base_url: &str,
        key: &str,
    ) -> std::result::Result<Vec<String>, probe::ProbeFailure> {
        probe::probe_models(
            base_url,
            Some(key),
            catalogue::auth_style_for(inference::MANAGED_SLUG),
            probe::default_policy(),
            catalogue::catalog_shape_for(inference::MANAGED_SLUG, base_url),
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Small local helpers
// ---------------------------------------------------------------------------

/// One model id, pinned to every tier — the same shape
/// `ops::inference::providers::tier_overrides` builds when the console adds a
/// row. Not shared: that function is private to its module, and reaching
/// across the ops/company boundary for a four-line map is a worse coupling
/// than the duplication.
fn tier_overrides(model: &str) -> BTreeMap<String, String> {
    crate::company::INFERENCE_TIERS
        .iter()
        .map(|tier| ((*tier).to_string(), model.to_string()))
        .collect()
}

/// `now`, RFC 3339 — the crate's one dependency-free formatter
/// (`ports::iso8601`), the same one `ops::inference::providers`' own
/// `record_health` wrapper uses.
fn now_rfc3339() -> String {
    crate::ports::iso8601(crate::ports::now_millis())
}

/// Writes the Composio TinyHumans slot for the fan-out specifically: the new
/// address gets `value`, and the pre-rename legacy address
/// (`composio::LEGACY_TOKEN_KEY`) is always zeroed rather than mirrored.
///
/// This is deliberately **not** [`composio::store_token`], which mirrors the
/// *same* value to both addresses (D-mirror, for an ordinary console-driven
/// token set). The fan-out's copy is not that: once this slot is under the
/// fan-out's management the legacy address stops being a live fallback for
/// it, which is what lets a company whose Composio credential was only ever
/// read through the legacy address (M14) end up with a clean new-address-only
/// state after one save, rather than a legacy address permanently mirroring
/// whatever this path last wrote.
async fn write_composio_slot(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    value: &str,
) -> Result<()> {
    secrets
        .set(
            company,
            composio::TINYHUMANS_KEY_KEY,
            SecretValue(value.to_string()),
        )
        .await?;
    secrets
        .set(
            company,
            composio::LEGACY_TOKEN_KEY,
            SecretValue(String::new()),
        )
        .await?;
    Ok(())
}

/// The [`SkipReason`] to report on the default slot when the provider slot
/// did not fill and is not `Kept(RowExists)` (handled by its own branch in
/// [`fan_out`]) — i.e. every other [`SlotOutcome`] the provider slot can end
/// up in.
///
/// [`SlotOutcome::Failed`] has no matching [`SkipReason`] of its own — §3.1's
/// list is fixed and none of its ten reasons names "the row write itself
/// failed" — so it falls back to [`SkipReason::InferenceNotWritten`]: the LLM
/// key is fine, but nothing new exists for a default to point at, which is
/// the same practical consequence.
fn provider_gate_reason(outcome: &SlotOutcome) -> SkipReason {
    match outcome {
        SlotOutcome::Kept(reason) | SlotOutcome::Skipped(reason) => *reason,
        _ => SkipReason::InferenceNotWritten,
    }
}

// ---------------------------------------------------------------------------
// slot_facts — read-only, for the account-key dialog (keys rework #2306, 4b)
// ---------------------------------------------------------------------------

/// What the account-key dialog needs to know **before** it saves, to say which
/// slots a save would actually touch (Q9).
///
/// `docs/key-reworks/phase-4b-account-dialog.md` §3.1: `*_has_own_key` is
/// deliberately not "is set" — a copy still equal to the account key is filled
/// again on rotation, so saving does fill it. Only whether the two stored
/// values are equal is revealed here, never either value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SlotFacts {
    /// The LLM TinyHumans key slot holds a key that is not the account key.
    /// Saving leaves it alone (Q7). Never the key.
    pub inference_has_own_key: bool,
    /// The same for `composio/tinyhumans/key` (with 1a's legacy read).
    pub composio_has_own_key: bool,
    /// `inference/default` is set (`ProviderOnly` or `Full`).
    pub default_set: bool,
}

/// Read-only. The same reads [`fan_out`]'s step 2 makes, so the dialog can
/// never disagree with what a save would actually do. No lock — a read races
/// nothing it could corrupt.
pub async fn slot_facts(company: &CompanyId, secrets: &dyn SecretStore) -> Result<SlotFacts> {
    let account_key = secrets
        .get(company, KEY_KEY)
        .await?
        .map(|SecretValue(v)| v.trim().to_string())
        .unwrap_or_default();
    let composio_now = composio::load_tinyhumans_key(company, secrets)
        .await?
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    let inference_now =
        inference::load_managed_key(company, secrets, &inference::HarnessScope::default())
            .await?
            .trim()
            .to_string();
    let default_now = inference_store::load_default(company, secrets).await?;

    let has_own_key = |current: &str| !current.is_empty() && current != account_key;
    Ok(SlotFacts {
        inference_has_own_key: has_own_key(&inference_now),
        composio_has_own_key: has_own_key(&composio_now),
        default_set: !matches!(default_now, inference_store::DefaultChoice::Unset),
    })
}

// ---------------------------------------------------------------------------
// read_slots — everything fan_out needs beyond the account key's own prior
// value (P2-1, keys rework #2306 review)
// ---------------------------------------------------------------------------

/// Everything [`fan_out`] needs to decide the Composio/LLM/provider/health
/// slots, read as one batch — always AFTER the account key itself is already
/// safely stored, never before (see [`fan_out`]'s own step 3/4).
///
/// Carries no default-marker read: unlike `row`, which step 8's health probe
/// needs *before* the network round trip (for the provider's `base_url`),
/// nothing here needs the default before the probe, and reading it this early
/// only invited a second, later re-read to stay accurate. [`fan_out`] now
/// reads `inference/default` exactly once, under `index_lock`, in step 9b
/// (KR review comment 4012261302) — see [`RelockedReads`].
struct ReadSlots {
    composio_now: String,
    legacy_managed: bool,
    row: Option<inference_store::Provider>,
    inference_key_key: String,
    inference_raw_new: Option<String>,
    legacy_owned: bool,
    legacy_raw: Option<String>,
    inference_now: String,
}

/// Reads everything [`ReadSlots`] holds. Split out of [`fan_out`] itself so
/// that function can treat a failure anywhere in this batch as one thing to
/// react to (degrade every derived slot to `Failed`) rather than several
/// `?`-propagated exits that would each need the same care.
async fn read_slots(company: &CompanyId, secrets: &dyn SecretStore) -> Result<ReadSlots> {
    let composio_now = composio::load_tinyhumans_key(company, secrets)
        .await?
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    let providers = inference_store::list_providers(company, secrets).await?;
    let legacy_managed = providers.iter().any(|p| {
        p.origin == inference_store::ProviderOrigin::EntryZero && p.slug == inference::MANAGED_SLUG
    });
    let row = providers.into_iter().find(|p| {
        p.origin == inference_store::ProviderOrigin::Indexed && p.slug == inference::MANAGED_SLUG
    });
    let inference_key_key = inference_store::provider_key_key(inference::MANAGED_SLUG);
    let inference_raw_new = secrets
        .get(company, &inference_key_key)
        .await?
        .map(|SecretValue(v)| v);
    let legacy_owned = inference_store::legacy_slot_is_managed(company, secrets).await?;
    let legacy_raw = if legacy_owned {
        secrets
            .get(company, inference::KEY_KEY)
            .await?
            .map(|SecretValue(v)| v)
    } else {
        None
    };
    let inference_now =
        inference::load_managed_key(company, secrets, &inference::HarnessScope::default())
            .await?
            .trim()
            .to_string();

    Ok(ReadSlots {
        composio_now,
        legacy_managed,
        row,
        inference_key_key,
        inference_raw_new,
        legacy_owned,
        legacy_raw,
        inference_now,
    })
}

/// What [`fan_out`]'s row/default writes need, read fresh under
/// [`inference_store::index_lock`] (KR review comment 4012261302).
///
/// [`ReadSlots`]'s own `row` is captured before the health probe's network
/// round trip, so a concurrent `add_provider`, `set_default`, or another
/// fan-out call can land in that window — creating the row this call is
/// about to create, or marking a default this call is about to overwrite.
/// `still_unset` used to re-read only the default, immediately before the
/// write, which narrowed the race but could not close it: the read and the
/// write were still two separate operations, with a gap between them nothing
/// serialised. This closes it by taking `index_lock` — the same lock
/// `server::ops::inference::providers` and `set_default` already hold for
/// their own reads and writes — and re-reading both the row and the default
/// while holding it continuously through the writes that follow, so nothing
/// else touching this company's inference index or default can interleave.
///
/// **Lock order: [`slot_guard`] first (already held for the whole of
/// [`fan_out`]), then `index_lock` — never the reverse, and never held across
/// a network call.** The health probe in [`fan_out`] runs with neither held;
/// this lock is taken only after it returns.
struct RelockedReads {
    row: Option<inference_store::Provider>,
    default_now: inference_store::DefaultChoice,
}

async fn reread_under_index_lock(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<RelockedReads> {
    let row = inference_store::list_providers(company, secrets)
        .await?
        .into_iter()
        .find(|p| {
            p.origin == inference_store::ProviderOrigin::Indexed
                && p.slug == inference::MANAGED_SLUG
        });
    let default_now = inference_store::load_default(company, secrets).await?;
    Ok(RelockedReads { row, default_now })
}

// ---------------------------------------------------------------------------
// fan_out
// ---------------------------------------------------------------------------

/// Runs the whole account-key fan-out for one `PUT …/credential` (or the
/// key-grant's `finish_link`), under [`slot_guard`] for the whole call.
///
/// `Err` only when the read of the account key's own prior value, or the
/// account-key write itself, fails — nothing else has been written yet at
/// that point (P2-1, keys rework #2306 review). Every later read or write
/// failure is reported as a `Failed` slot in the returned [`FanOutReport`]
/// instead, so a transient fault — in, say, the row write, or in reading
/// `inference/default` — never costs the hub-minted, single-use key
/// `finish_link` stores here and can never reissue.
pub async fn fan_out(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    request: FanOutRequest<'_>,
    prober: &dyn InferenceProber,
) -> Result<FanOutReport> {
    let _guard = slot_guard(company).await;

    // The proxy base every TinyHumans write below carries. From the caller's
    // configured platform when it says so; the catalogue's production endpoint
    // otherwise.
    let proxy_base_url = request
        .proxy_base_url
        .map(catalogue::tinyhumans_proxy_url)
        .or_else(|| {
            catalogue::cloud_provider(inference::MANAGED_SLUG).map(|c| c.endpoint.to_string())
        })
        .unwrap_or_default();

    // 1. Validate. Nothing is written yet.
    let new = request.key.trim().to_string();
    let clearing = new.is_empty();
    let model: Option<String> = match request.model.map(str::trim).filter(|m| !m.is_empty()) {
        Some(_) if clearing => {
            return Err(OpenCompanyError::InvalidRequest(
                "A model cannot be chosen while removing the key.".to_string(),
            ));
        }
        Some(raw) => Some(inference_store::check_model_id(raw)?),
        None => None,
    };

    // 2. Read the one thing that must be read before the account key itself
    // is overwritten: its own prior value (P2-1, keys rework #2306 review).
    // Every derived-slot decision below needs "what did tinyhumans/key hold
    // before this call" to tell a copy that still matches the old account key
    // apart from a value someone set by hand — and once step 3 overwrites it,
    // that answer is gone for good. An error here returns `Err` — nothing has
    // been written yet.
    let old_account = secrets
        .get(company, KEY_KEY)
        .await?
        .map(|SecretValue(v)| v.trim().to_string())
        .unwrap_or_default();

    // 2b. The in-use guard (P3-6, keys rework #2306 review), for a clear
    // only. Computed and — if it refuses — returned HERE: after `slot_guard`
    // is held, and before anything is written. This used to be a check the
    // HTTP handler ran before ever calling into `fan_out`, which is exactly
    // the check-then-act race `slot_guard` exists to close for every other
    // decision this function makes: a concurrent save landing in that
    // unlocked window could make an unconfirmed clear strand a copy the
    // other save had just filled, or make this 409 echo a `usedBy` that was
    // already stale by the time the caller read it. Reusing
    // [`account_key_used_by`] rather than re-deriving it inline keeps this
    // gate and the status route's own display read agreeing by construction.
    let strand_check = if clearing {
        account_key_used_by(company, secrets).await?
    } else {
        None
    };
    if clearing
        && !request.confirm_in_use
        && let Some(used_by) = strand_check.clone()
    {
        return Err(OpenCompanyError::InUse {
            message: account_key_in_use_message(&used_by.surfaces),
            used_by,
        });
    }

    // 3. Write the account key itself — as early as it can land once its own
    // prior value is known. This is the point that makes `finish_link`'s
    // single-use, unreissuable hub-minted key safe: past this line, no later
    // read or write failure can lose it (see `read_slots` and step 4 below).
    // An error on THIS write is still `Err` — the read above is the last
    // chance to fail with nothing stored.
    secrets
        .set(company, KEY_KEY, SecretValue(new.clone()))
        .await?;

    // 4. Read everything else the derived slots need — best-effort from here
    // (P2-1). A failure reading any of this (a corrupt `inference/default`
    // blob, a transient store fault) can no longer cost the account key
    // itself, so it degrades to every derived slot reporting `Failed` rather
    // than losing the whole call — and the key it already safely holds — to a
    // bubbled `Err`.
    let ReadSlots {
        composio_now,
        legacy_managed,
        row,
        inference_key_key,
        inference_raw_new,
        legacy_owned,
        legacy_raw,
        inference_now,
    } = match read_slots(company, secrets).await {
        Ok(reads) => reads,
        Err(err) => {
            tracing::error!(
                company = %company,
                error = %err,
                "keys rework: fan_out could not read Composio/LLM/provider state after \
                 storing the account key; every derived slot is reported as failed",
            );
            return Ok(FanOutReport {
                slots: vec![
                    SlotReport {
                        slot: Slot::Composio,
                        outcome: SlotOutcome::Failed,
                    },
                    SlotReport {
                        slot: Slot::Inference,
                        outcome: SlotOutcome::Failed,
                    },
                    SlotReport {
                        slot: Slot::Provider,
                        outcome: SlotOutcome::Failed,
                    },
                    SlotReport {
                        slot: Slot::Default,
                        outcome: SlotOutcome::Failed,
                    },
                    SlotReport {
                        slot: Slot::Health,
                        outcome: SlotOutcome::Failed,
                    },
                ],
                needs_model: false,
                sets_default: false,
                models: Vec::new(),
                rollback_had_prior_key: false,
                used_by: strand_check.clone(),
            });
        }
    };

    // 5. Composio. Never reads or writes `composio/mode` or
    // `composio/byok/key` — see `write_composio_slot`.
    let composio_outcome = match decide_copy(&composio_now, &old_account, &new) {
        CopyDecision::Write => match write_composio_slot(company, secrets, &new).await {
            Ok(()) => {
                if composio_now.is_empty() {
                    SlotOutcome::Filled
                } else {
                    SlotOutcome::Rotated
                }
            }
            Err(_) => SlotOutcome::Failed,
        },
        CopyDecision::Clear => match write_composio_slot(company, secrets, "").await {
            Ok(()) => SlotOutcome::Cleared,
            Err(_) => SlotOutcome::Failed,
        },
        CopyDecision::Keep(reason) => SlotOutcome::Kept(reason),
        CopyDecision::Skip(reason) => SlotOutcome::Skipped(reason),
    };

    // 6. Inference key.
    let legacy_raw_is_live = legacy_owned
        && legacy_raw
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
    let mut inference_outcome = match decide_copy(&inference_now, &old_account, &new) {
        decision @ (CopyDecision::Write | CopyDecision::Clear) => {
            let is_clear = matches!(decision, CopyDecision::Clear);
            let write_value = if is_clear { String::new() } else { new.clone() };
            match secrets
                .set(company, &inference_key_key, SecretValue(write_value))
                .await
            {
                Ok(()) => {
                    let primary = if is_clear {
                        SlotOutcome::Cleared
                    } else if inference_now.is_empty() {
                        SlotOutcome::Filled
                    } else {
                        SlotOutcome::Rotated
                    };
                    if legacy_raw_is_live {
                        match secrets
                            .set(company, inference::KEY_KEY, SecretValue(String::new()))
                            .await
                        {
                            Ok(()) => primary,
                            Err(err) if is_clear => {
                                tracing::error!(
                                    company = %company,
                                    error = %err,
                                    "keys rework: could not clear the legacy inference/key slot on an account-key clear",
                                );
                                SlotOutcome::Failed
                            }
                            Err(err) => {
                                tracing::error!(
                                    company = %company,
                                    error = %err,
                                    "keys rework: could not clear the legacy inference/key slot after copying the account key",
                                );
                                primary
                            }
                        }
                    } else {
                        primary
                    }
                }
                Err(_) => SlotOutcome::Failed,
            }
        }
        CopyDecision::Keep(reason) => SlotOutcome::Kept(reason),
        CopyDecision::Skip(reason) => SlotOutcome::Skipped(reason),
    };

    let holds_new = matches!(
        inference_outcome,
        SlotOutcome::Filled | SlotOutcome::Rotated | SlotOutcome::Kept(SkipReason::AlreadyCurrent)
    );

    // 7. Clearing stops here: rows and the default are never touched by a
    // clear (§6, case C1).
    if clearing {
        let health_outcome = if matches!(inference_outcome, SlotOutcome::Cleared) {
            if let Err(err) =
                inference_store::forget_health(company, secrets, inference::MANAGED_SLUG).await
            {
                tracing::warn!(
                    company = %company,
                    error = %err,
                    "keys rework: could not forget tinyhumans health on an account-key clear",
                );
            }
            SlotOutcome::Cleared
        } else {
            SlotOutcome::Skipped(SkipReason::KeyCleared)
        };
        return Ok(FanOutReport {
            slots: vec![
                SlotReport {
                    slot: Slot::Composio,
                    outcome: composio_outcome,
                },
                SlotReport {
                    slot: Slot::Inference,
                    outcome: inference_outcome,
                },
                SlotReport {
                    slot: Slot::Provider,
                    outcome: SlotOutcome::Skipped(SkipReason::KeyCleared),
                },
                SlotReport {
                    slot: Slot::Default,
                    outcome: SlotOutcome::Skipped(SkipReason::KeyCleared),
                },
                SlotReport {
                    slot: Slot::Health,
                    outcome: health_outcome,
                },
            ],
            needs_model: false,
            sets_default: false,
            models: Vec::new(),
            rollback_had_prior_key: false,
            used_by: strand_check.clone(),
        });
    }

    // 8. Health, before any row or default write (Q6 by construction).
    let mut probe_ids: Option<Vec<String>> = None;
    let health_outcome = if legacy_managed {
        SlotOutcome::Skipped(SkipReason::LegacyManagedConfig)
    } else if !holds_new {
        match inference_outcome {
            SlotOutcome::Kept(SkipReason::CustomKey) => SlotOutcome::Skipped(SkipReason::CustomKey),
            _ => SlotOutcome::Skipped(SkipReason::InferenceNotWritten),
        }
    } else {
        let base = row
            .as_ref()
            .map(|r| r.base_url.clone())
            .unwrap_or_else(|| proxy_base_url.clone());
        match prober.probe(&base, &new).await {
            Ok(ids) => {
                if let Err(err) = inference_store::record_health(
                    company,
                    secrets,
                    inference::MANAGED_SLUG,
                    "ok",
                    &now_rfc3339(),
                )
                .await
                {
                    tracing::warn!(company = %company, error = %err, "keys rework: could not record tinyhumans health");
                }
                probe_ids = Some(ids);
                SlotOutcome::HealthOk
            }
            Err(failure) => {
                if let Err(err) = inference_store::record_health(
                    company,
                    secrets,
                    inference::MANAGED_SLUG,
                    failure.class.as_str(),
                    &now_rfc3339(),
                )
                .await
                {
                    tracing::warn!(company = %company, error = %err, "keys rework: could not record tinyhumans health");
                }
                SlotOutcome::HealthFailed(failure.class)
            }
        }
    };

    // 9. Q6 rollback: an `auth` probe undoes only what THIS request wrote to
    // the LLM slots, and never touches the account key or the Composio copy.
    let auth_rejected = matches!(
        health_outcome,
        SlotOutcome::HealthFailed(probe::ProbeClass::Auth)
    );
    // P2-2 (keys rework #2306 review): whether this specific rollback undoes
    // a genuine **rotation** (the slot held a real prior key, now restored)
    // rather than a first-time **fill** (the slot was empty, and "restoring"
    // it just puts it back to empty) — `fan_out_note` reads this to say the
    // LLM page still uses the *previous* key, rather than only that the new
    // one "was not kept". Computed from `inference_outcome` before it is
    // overwritten to `RolledBack` below: within this `if`, the only two
    // outcomes reachable are `Filled` (nothing to restore) and `Rotated`
    // (there was).
    let mut rollback_had_prior_key = false;
    if auth_rejected
        && matches!(
            inference_outcome,
            SlotOutcome::Filled | SlotOutcome::Rotated
        )
    {
        rollback_had_prior_key = matches!(inference_outcome, SlotOutcome::Rotated);
        let restore = inference_raw_new.clone().unwrap_or_default();
        if let Err(err) = secrets
            .set(company, &inference_key_key, SecretValue(restore))
            .await
        {
            tracing::error!(
                company = %company,
                slot = "inference",
                error = %err,
                "keys rework: could not restore the LLM TinyHumans key after an auth rollback",
            );
        }
        if legacy_raw_is_live {
            let restore_legacy = legacy_raw.clone().unwrap_or_default();
            if let Err(err) = secrets
                .set(company, inference::KEY_KEY, SecretValue(restore_legacy))
                .await
            {
                tracing::error!(
                    company = %company,
                    slot = "inference_legacy",
                    error = %err,
                    "keys rework: could not restore the legacy inference key after an auth rollback",
                );
            }
        }
        inference_outcome = SlotOutcome::RolledBack;
    }

    // 9b. Take `index_lock` (KR review comment 4012261302) and re-read the row
    // and the default fresh, now that the network probe is done — this lock
    // is never held across a network call, and `slot_guard` (held for the
    // whole of this function) is always taken first. Held continuously from
    // here through the row and default writes below, so nothing else
    // touching this company's inference index or default can interleave with
    // the checks steps 10 and 11 make against it.
    let _index_guard = inference_store::index_lock(company).await;
    let (row, default_now) = match reread_under_index_lock(company, secrets).await {
        Ok(RelockedReads { row, default_now }) => (row, default_now),
        Err(err) => {
            // Best-effort from here, same as step 4's own read failure (P2-1):
            // the account key and the Composio/inference/health slots are
            // already settled, so a transient fault re-reading the index
            // degrades only the two slots that depend on it rather than
            // losing the whole call.
            tracing::error!(
                company = %company,
                error = %err,
                "keys rework: fan_out could not re-read the inference index under index_lock; \
                 the provider and default slots are reported as failed",
            );
            return Ok(FanOutReport {
                slots: vec![
                    SlotReport {
                        slot: Slot::Composio,
                        outcome: composio_outcome,
                    },
                    SlotReport {
                        slot: Slot::Inference,
                        outcome: inference_outcome,
                    },
                    SlotReport {
                        slot: Slot::Provider,
                        outcome: SlotOutcome::Failed,
                    },
                    SlotReport {
                        slot: Slot::Default,
                        outcome: SlotOutcome::Failed,
                    },
                    SlotReport {
                        slot: Slot::Health,
                        outcome: health_outcome,
                    },
                ],
                needs_model: false,
                sets_default: false,
                models: Vec::new(),
                rollback_had_prior_key,
                used_by: strand_check.clone(),
            });
        }
    };

    // 10. Provider row.
    let mut needs_model = false;
    let provider_outcome = if auth_rejected {
        SlotOutcome::Skipped(SkipReason::InferenceRejected)
    } else if legacy_managed {
        SlotOutcome::Skipped(SkipReason::LegacyManagedConfig)
    } else if !holds_new {
        match inference_outcome {
            SlotOutcome::Kept(SkipReason::CustomKey) => SlotOutcome::Skipped(SkipReason::CustomKey),
            _ => SlotOutcome::Skipped(SkipReason::InferenceNotWritten),
        }
    } else if row.is_some() {
        SlotOutcome::Kept(SkipReason::RowExists)
    } else if let Some(chosen) = model.as_deref() {
        let cat = catalogue::cloud_provider(inference::MANAGED_SLUG)
            .expect("tinyhumans is a built-in cloud provider (2a)");
        match inference_store::put_provider(
            company,
            secrets,
            inference_store::ProviderDraft {
                slug: inference::MANAGED_SLUG.to_string(),
                label: cat.label.to_string(),
                kind: inference::MANAGED_SLUG.to_string(),
                base_url: proxy_base_url.clone(),
                models: tier_overrides(chosen),
                enabled: true,
            },
        )
        .await
        {
            Ok(_) => SlotOutcome::Filled,
            Err(_) => SlotOutcome::Failed,
        }
    } else {
        needs_model = true;
        SlotOutcome::Skipped(SkipReason::NeedsModel)
    };

    // 11. Default.
    //
    // `default_now` was re-read under `index_lock` in step 9b, held
    // continuously from that read through the write here — nothing else
    // touching this company's default marker can land in between, so unlike
    // the old `still_unset` re-check, no second read is needed immediately
    // before the write: the one already taken is current for the whole of
    // this critical section.
    let default_outcome = if auth_rejected {
        SlotOutcome::Skipped(SkipReason::InferenceRejected)
    } else if !matches!(default_now, inference_store::DefaultChoice::Unset) {
        SlotOutcome::Kept(SkipReason::DefaultAlreadySet)
    } else {
        match &provider_outcome {
            SlotOutcome::Filled => {
                let chosen = model.clone().expect("a filled row always had a model");
                let choice = inference_store::ModelChoice {
                    provider: inference::MANAGED_SLUG.to_string(),
                    model: chosen,
                };
                match inference_store::set_default_choice(company, secrets, &choice).await {
                    Ok(()) => SlotOutcome::Filled,
                    Err(_) => SlotOutcome::Failed,
                }
            }
            SlotOutcome::Kept(SkipReason::RowExists) => {
                // P3-8 (keys rework #2306 review): a disabled row cannot
                // currently serve anything, so it must not become the new
                // default — that would point the company's default at a
                // provider nothing can resolve through until someone
                // re-enables it, with no warning that this save was what did
                // it.
                if !row.as_ref().is_some_and(|r| r.enabled) {
                    SlotOutcome::Skipped(SkipReason::ProviderDisabled)
                } else {
                    let picked = match &model {
                        Some(sent) => Some(sent.clone()),
                        None => match row.as_ref().map(|r| r.model()) {
                            Some(inference_store::ModelOnRow::One(m)) => Some(m),
                            _ => None,
                        },
                    };
                    match picked {
                        Some(chosen) => {
                            let choice = inference_store::ModelChoice {
                                provider: inference::MANAGED_SLUG.to_string(),
                                model: chosen,
                            };
                            match inference_store::set_default_choice(company, secrets, &choice)
                                .await
                            {
                                Ok(()) => SlotOutcome::Filled,
                                Err(_) => SlotOutcome::Failed,
                            }
                        }
                        None => {
                            needs_model = true;
                            SlotOutcome::Skipped(SkipReason::NeedsModel)
                        }
                    }
                }
            }
            other => SlotOutcome::Skipped(provider_gate_reason(other)),
        }
    };

    // 12. The two flags a follow-up (or the console) needs.
    let sets_default = needs_model && matches!(default_now, inference_store::DefaultChoice::Unset);
    let models = if needs_model {
        probe_ids
            .map(|ids| inference::paged_catalog::catalogue_offer(&ids))
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    Ok(FanOutReport {
        slots: vec![
            SlotReport {
                slot: Slot::Composio,
                outcome: composio_outcome,
            },
            SlotReport {
                slot: Slot::Inference,
                outcome: inference_outcome,
            },
            SlotReport {
                slot: Slot::Provider,
                outcome: provider_outcome,
            },
            SlotReport {
                slot: Slot::Default,
                outcome: default_outcome,
            },
            SlotReport {
                slot: Slot::Health,
                outcome: health_outcome,
            },
        ],
        needs_model,
        sets_default,
        models,
        rollback_had_prior_key,
        // Never reached with `clearing` true (step 7 already returned), so
        // `strand_check` is always `None` here.
        used_by: strand_check.clone(),
    })
}

// ---------------------------------------------------------------------------
// fan_out_note — §3.4's fixed sentence table
// ---------------------------------------------------------------------------

/// The plain-words note a `PUT …/credential` answers with, built by joining
/// (with a space) exactly the sentences from §3.4's table whose condition
/// applies, in the table's order. Never contains a key.
pub fn fan_out_note(clearing: bool, report: &FanOutReport, model: Option<&str>) -> String {
    let slot = |s: Slot| report.slots.iter().find(|r| r.slot == s).map(|r| r.outcome);
    let composio = slot(Slot::Composio);
    let inference = slot(Slot::Inference);
    let provider = slot(Slot::Provider);
    let default = slot(Slot::Default);
    let health = slot(Slot::Health);

    let mut sentences: Vec<String> = Vec::new();
    sentences.push(if clearing {
        "Key removed.".to_string()
    } else {
        "Key saved.".to_string()
    });

    if matches!(
        composio,
        Some(SlotOutcome::Filled) | Some(SlotOutcome::Rotated)
    ) {
        sentences.push(
            "Composio now uses this key. A key you created by hand may lack the connections \
             permission Composio needs."
                .to_string(),
        );
    }
    if matches!(composio, Some(SlotOutcome::Kept(SkipReason::CustomKey))) {
        sentences.push("Composio keeps the key set on its own page.".to_string());
    }
    if matches!(composio, Some(SlotOutcome::Cleared)) {
        sentences.push("Composio's copy was removed too.".to_string());
    }
    if matches!(inference, Some(SlotOutcome::Kept(SkipReason::CustomKey))) {
        sentences.push("LLM keeps the TinyHumans key set on its own page.".to_string());
    }
    if let (Some(SlotOutcome::Filled), Some(m)) = (provider, model) {
        sentences.push(format!("TinyHumans is set up for LLM with {m}."));
    }
    if matches!(default, Some(SlotOutcome::Filled)) {
        sentences.push("It is now the default for new work.".to_string());
    }
    if report.needs_model {
        sentences.push("Choose a model to finish setting up TinyHumans for LLM.".to_string());
    }
    if matches!(
        provider,
        Some(SlotOutcome::Skipped(SkipReason::LegacyManagedConfig))
    ) {
        sentences.push("LLM keeps this company's existing TinyHumans setup.".to_string());
    }
    if let Some(SlotOutcome::HealthFailed(class)) = health {
        if class == probe::ProbeClass::Auth {
            // P3-1 (keys rework #2306 review): this sentence only makes sense
            // when the inference slot actually rolled back — a resave of an
            // already-current key that then fails health (`Kept(AlreadyCurrent)`)
            // never rolled anything back, so nothing here was "not kept".
            if matches!(inference, Some(SlotOutcome::RolledBack)) {
                if report.rollback_had_prior_key {
                    // P2-2: a rollback on what was a genuine rotation — the LLM
                    // slot is back on its previous key, not empty, so the
                    // generic "was not kept" sentence would undersell what
                    // actually happened.
                    sentences.push(
                        "TinyHumans on the LLM page still uses your previous key, because the \
                         new key was rejected for LLM use."
                            .to_string(),
                    );
                } else {
                    sentences.push(
                        "TinyHumans rejected this key for LLM, so the LLM copy was not kept."
                            .to_string(),
                    );
                }
            }
        } else {
            sentences.push(probe::describe(class, "TinyHumans"));
        }
    }
    if matches!(inference, Some(SlotOutcome::Cleared)) {
        sentences.push(
            "LLM's copy was removed too; TinyHumans stays on the LLM page without a key."
                .to_string(),
        );
    }
    if report
        .slots
        .iter()
        .any(|s| matches!(s.outcome, SlotOutcome::Failed))
    {
        sentences
            .push("Some copies could not be saved — check the LLM and Composio pages.".to_string());
    }

    sentences.join(" ")
}

// ---------------------------------------------------------------------------
// copy_account_key_to_composio — keys rework #2306, slice 4c (Composio half)
// ---------------------------------------------------------------------------

/// Copies [`KEY_KEY`] (the account key) into `composio/tinyhumans/key`, for
/// the reuse banner offered when Composio's own copy is gone but the account
/// key still exists (`docs/key-reworks/phase-4c-reuse-banner.md`).
///
/// A single-slot copy, not a save: the account key itself is never read as
/// "old" and "new" here in the way [`fan_out`] reads a `PUT …/credential`
/// body — there is no new value coming in, only a request to make the
/// Composio slot agree with what `tinyhumans/key` already holds. That is
/// exactly [`decide_copy`]'s rule with `old_account` and `new` both equal to
/// the current account key, so it is reused rather than reimplemented: the
/// only branches `decide_copy` can take with a non-empty `new` are `Write`
/// (the slot is empty) and `Keep` (`AlreadyCurrent` or `CustomKey`) — a
/// `Clear`/`Skip` branch would require an empty `new`, which never happens
/// here because an empty account key is refused first.
///
/// One case gets one refinement past a literal `decide_copy` read: when the
/// slot already agrees with the account key only through 1a's legacy
/// fallback (`composio/token`) — the new address itself is still empty —
/// this explicit, user-requested copy materialises the value on the new
/// address and retires the mirror via [`write_composio_slot`], the same as
/// every other write that function makes. Leaving it alone would mean a
/// company that clicks "Yes" on the reuse banner stays one legacy read away
/// from a clean state.
///
/// Refuses (`OpenCompanyError::InvalidRequest`, 400, before any write) when
/// there is no account key to copy, or when the Composio slot already holds a
/// different, non-empty key of its own. Never reads or writes `composio/mode`
/// or `composio/byok/key` — this touches the managed TinyHumans slot only.
pub async fn copy_account_key_to_composio(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<FanOutReport> {
    let _guard = slot_guard(company).await;

    // 1. Read the account key. Any error here returns `Err` — nothing has
    // been written.
    let account_key = secrets
        .get(company, KEY_KEY)
        .await?
        .map(|SecretValue(v)| v.trim().to_string())
        .unwrap_or_default();
    if account_key.is_empty() {
        return Err(OpenCompanyError::InvalidRequest(
            "There is no account key to reuse. Add one on the Account page.".to_string(),
        ));
    }

    // 2. Read the Composio slot: the new address directly, and — only when
    // that is empty — the pre-rename legacy address (1a's fallback), so a
    // value that reads correctly today purely through the legacy fallback is
    // told apart from one already sitting on the new address.
    let composio_direct = secrets
        .get(company, composio::TINYHUMANS_KEY_KEY)
        .await?
        .map(|SecretValue(v)| v.trim().to_string())
        .unwrap_or_default();
    let composio_now = if composio_direct.is_empty() {
        composio::load_tinyhumans_key(company, secrets)
            .await?
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    } else {
        composio_direct.clone()
    };

    // 3. Decide, before any write. `old_account` and `new` are both the
    // account key: this call never changes it, only copies it.
    let outcome = match decide_copy(&composio_now, &account_key, &account_key) {
        CopyDecision::Write => {
            write_composio_slot(company, secrets, &account_key).await?;
            SlotOutcome::Filled
        }
        // The slot already agrees with the account key, but only through the
        // legacy fallback — the new address itself is still empty. This
        // explicit, user-requested copy is the moment to materialise the
        // value on the new address and retire the mirror, exactly what
        // `write_composio_slot` already does on every write; leaving it
        // alone here would mean a company that clicks "Yes" stays one legacy
        // read away from a clean state.
        CopyDecision::Keep(SkipReason::AlreadyCurrent) if composio_direct.is_empty() => {
            write_composio_slot(company, secrets, &account_key).await?;
            SlotOutcome::Filled
        }
        CopyDecision::Keep(SkipReason::AlreadyCurrent) => {
            SlotOutcome::Kept(SkipReason::AlreadyCurrent)
        }
        CopyDecision::Keep(SkipReason::CustomKey) => {
            return Err(OpenCompanyError::InvalidRequest(
                "TinyHumans already has its own key here. Remove it first to reuse the account key."
                    .to_string(),
            ));
        }
        other => unreachable!(
            "decide_copy with a non-empty `new` only ever answers Write or \
             Keep(AlreadyCurrent | CustomKey): {other:?}"
        ),
    };

    Ok(FanOutReport {
        slots: vec![SlotReport {
            slot: Slot::Composio,
            outcome,
        }],
        needs_model: false,
        sets_default: false,
        models: Vec::new(),
        rollback_had_prior_key: false,
        used_by: None,
    })
}

#[cfg(test)]
mod test;
