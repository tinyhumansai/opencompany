//! Provider records and their credential slots, over the [`SecretStore`] port.
//!
//! This module knows two things: where a provider record lives, and where its
//! credential lives. It knows nothing about HTTP, and nothing about which
//! provider is *right* for a request — that is [`resolve`](super::resolve)'s
//! job. Everything here is exercised against an in-memory store, with no host.
//!
//! ## Entry zero — why the flat legacy slot stays exactly where it is
//!
//! A company's stored console override and credential already live at
//! [`RUNTIME_CONFIG_KEY`](super::RUNTIME_CONFIG_KEY) and
//! [`KEY_KEY`](super::KEY_KEY), and **the store has no rename**. Namespacing
//! every provider under a new prefix would orphan the config of every company
//! already running — the one migration this design cannot afford, and the code
//! said so before this module existed.
//!
//! So the flat slot is not migrated. It *is* the first element of the list:
//!
//! ```text
//!   inference/config   ──▶  entry zero   (legacy, never moved, never indexed)
//!   inference/key      ──▶  its credential
//!
//!   inference/providers ─▶  the index: every OTHER provider, no credentials
//!   provider/<slug>/key ─▶  one credential slot per provider
//! ```
//!
//! The cost is one special case in the reader, forever. That is cheaper than a
//! migration on a store with no transaction, where a half-completed run leaves a
//! company with neither config.
//!
//! ## Why an index blob rather than a key scan
//!
//! The design sketch reads the list as `list("provider/*/config")`. **The
//! [`SecretStore`] port has no `list`** — it is `get` and `set` and nothing
//! else, and widening a port every backend must implement (including the
//! database-per-tenant MongoDB one) to enumerate secrets by prefix is a much
//! larger change than this feature justifies, and a worse one: prefix
//! enumeration over a secret store is a capability with no other caller.
//!
//! So the records live together in one indexed blob and only the *credentials*
//! are keyed per slug. This also makes an add atomic in the way that matters: a
//! provider appears in the list in one `set`, rather than a company being able
//! to observe half a record.
//!
//! ## And no delete, either
//!
//! The port cannot delete. Clearing is a write of the empty string that reads
//! back as unset, so "cleared" and "never set" are the same state. That is
//! workable, but it means a clear must actually be **issued** rather than
//! inferred — which is why [`delete_provider`] clears the credential explicitly
//! and says so loudly when the clear fails. Leaving a `provider/<slug>/key`
//! behind is not untidiness: re-adding that slug would silently inherit a
//! credential the operator thought they had removed.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::error::OpenCompanyError;
use crate::ports::SecretStore;
use crate::ports::types::{CompanyId, SecretValue};

use super::catalogue;
use super::resolve::{ProviderRef, Routes};
use super::{KEY_KEY, RuntimeInference, normalize_provider};

/// The [`SecretStore`] key holding the provider index — every provider *except*
/// entry zero, without credentials.
pub const PROVIDER_INDEX_KEY: &str = "inference/providers";

/// The credential slot for one provider.
///
/// Keyed on the **slug**, not the id, because the slug is what a routing entry
/// names and what an operator can read. An id in a secret key would be
/// unreadable in exactly the situation where someone is reading raw keys.
pub fn provider_key_key(slug: &str) -> String {
    format!("provider/{slug}/key")
}

/// The id entry zero always answers to.
///
/// Deterministic rather than generated, and that is the point: entry zero is
/// discovered by *reading* the legacy slot, and a generated id would have to be
/// written back to be stable — a write on a read path, into the one slot this
/// design promised not to touch. A fixed sentinel is stable for free.
///
/// Not in the catalogue's namespace and not generatable by [`ProviderId::new`],
/// so it cannot collide with a real one.
pub const ENTRY_ZERO_ID: &str = "prv_entry_zero";

/// Stable, opaque identity for a provider. Generated once, never shown to an
/// operator, never reused after a delete.
///
/// Separate from the slug because they answer different questions: `id` is
/// identity and has to survive a rename, `slug` is an address an operator reads
/// and hand-edits in a routing entry (`reasoning-v1 -> acme:gpt-5`). The code
/// this replaces had neither — its `provider_slug()` is telemetry-only and
/// derived *from* the kind, so two OpenRouter accounts could not coexist.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProviderId(String);

impl ProviderId {
    /// A fresh id, from OS entropy.
    ///
    /// `rand_core`'s `getrandom` feature is already a hard dependency of this
    /// crate (the tiny.place signer uses it), so this needs no new one — which
    /// matters because `uuid` here is optional and links only under the
    /// `openhuman` feature, while this module compiles in every build.
    pub fn new() -> Self {
        use rand_core::RngCore;
        let mut bytes = [0u8; 16];
        rand_core::OsRng.fill_bytes(&mut bytes);
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Self(format!("prv_{hex}"))
    }

    /// The id entry zero answers to.
    pub fn entry_zero() -> Self {
        Self(ENTRY_ZERO_ID.to_string())
    }

    /// The opaque string form.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ProviderId {
    fn default() -> Self {
        Self::new()
    }
}

/// Where a provider's record physically lives.
///
/// Not persisted — it is a fact about *which slot answered*, recovered on read.
/// Entry zero is never written into the index, so round-tripping this field
/// through the blob would let the two disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderOrigin {
    /// The flat legacy slot: `inference/config` + `inference/key`.
    EntryZero,
    /// The index blob, with a credential at `provider/<slug>/key`.
    Indexed,
}

/// One configured way for a company to reach a model.
///
/// **There is no key on this record**, and that is the single most important
/// line in this module. The credential lives in its own slot and is read per
/// request by exactly one function, which is what lets the managed tier be a
/// rotating platform token rather than a value captured at boot.
///
/// Derives no `Serialize` on purpose. The persisted shape is
/// [`StoredProvider`], the wire shape is a DTO carrying `key_configured: bool`,
/// and this is neither — it is the resolved in-memory record, and it carries
/// [`origin`](Self::origin), which is meaningless in both of the others. Four
/// independent mechanisms keep credentials off the wire in this subsystem and
/// the easiest way to break all four at once is to add a convenience derive to a
/// record like this one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    /// Stable, opaque identity.
    pub id: ProviderId,
    /// Routing key. Unique within a company. What a routing entry names.
    pub slug: String,
    /// Display label. Never used in routing.
    pub label: String,
    /// Provider kind — a catalogue slug, or one of the legacy manifest kinds.
    pub kind: String,
    /// Resolved OpenAI-compatible base URL.
    pub base_url: String,
    /// Abstract tier → concrete model id. Empty means pass tiers through.
    pub models: BTreeMap<String, String>,
    /// Whether this provider is available for routing.
    ///
    /// Distinct from deleted, and deliberately so: the design being borrowed
    /// from has a binary switch, where turning a provider off *deletes* it and
    /// loses its endpoint, its label and its routes. "Stop billing this account
    /// this week" is not expressible there. A disabled provider here keeps
    /// everything and is simply not a routing target.
    pub enabled: bool,
    /// Which slot this came out of.
    pub origin: ProviderOrigin,
}

impl Provider {
    /// Where this provider's credential is **written**: `provider/<slug>/key`,
    /// for every provider without exception.
    ///
    /// It used to branch on [`ProviderOrigin::EntryZero`] and answer the flat
    /// `inference/key`, which made "which slot" a question about where the
    /// *record* came from rather than about which provider the credential
    /// belongs to. One address rule, no exception.
    ///
    /// The legacy slot has not stopped existing — see
    /// [`legacy_key_key`](Self::legacy_key_key) and the convergence rule on
    /// [`load_provider_key`].
    pub fn key_key(&self) -> String {
        provider_key_key(&self.slug)
    }

    /// The address this provider's credential may **still** be at, from before
    /// the addresses were made uniform.
    ///
    /// Only entry zero has one: it is the company whose single credential
    /// predates the list. `None` for everything else, because nothing was ever
    /// written anywhere but `provider/<slug>/key` for an indexed provider.
    pub fn legacy_key_key(&self) -> Option<&'static str> {
        match self.origin {
            ProviderOrigin::EntryZero => Some(KEY_KEY),
            ProviderOrigin::Indexed => None,
        }
    }
}

/// The persisted shape of a non-entry-zero provider.
///
/// Separate from [`Provider`] because the blob must not carry `origin` — every
/// record in it is `Indexed` by construction, and a stored copy of that fact
/// could disagree with where it was actually read from.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredProvider {
    id: ProviderId,
    slug: String,
    label: String,
    kind: String,
    base_url: String,
    #[serde(default)]
    models: BTreeMap<String, String>,
    /// Defaults **true**, so an index written before this field existed reads as
    /// enabled rather than as a company whose providers all silently vanished.
    #[serde(default = "enabled_default")]
    enabled: bool,
}

fn enabled_default() -> bool {
    true
}

/// What a caller supplies to create or replace a provider. No credential: that
/// is a separate, write-only act ([`store_provider_key`]).
#[derive(Clone, Debug)]
pub struct ProviderDraft {
    /// The routing key. Must already be free — see [`check_slug`].
    pub slug: String,
    /// Display label.
    pub label: String,
    /// Provider kind.
    pub kind: String,
    /// Resolved OpenAI-compatible base URL.
    pub base_url: String,
    /// Abstract tier → concrete model id.
    pub models: BTreeMap<String, String>,
    /// Whether it is a routing target. New providers arrive enabled.
    pub enabled: bool,
}

/// The longest a provider name — and therefore the slug derived from it — may
/// be, in characters.
///
/// **Bounded at all** because the name is not only a label: [`slugify`] turns it
/// into the address of a secret (`provider/<slug>/key`), and a secret key is a
/// path component in the filesystem store. An unbounded name produced an
/// unbounded path, which is how a 245-character name came to 500 a credential
/// read and a 300-character one came to truncate a stored key and then fail the
/// delete that truncated it. The store no longer breaks on a long key — see
/// `legacy_secret_absent` in `src/store/fs.rs` — but a rule the store has to
/// absorb is a rule that was never stated, and the name still has to be legible
/// in a routing row an operator hand-edits.
///
/// **Eighty** because that is the bound this codebase already uses for the other
/// name a person types and then reads back in a list
/// (`MAX_DISPLAY_NAME_CHARS`, `src/server/users/mod.rs`), and because it keeps
/// the derived secret key well inside the canonical filename budget: at 80
/// characters `provider/<slug>/key` percent-encodes to 97 bytes against a
/// 200-byte budget, so a provider's credential file is never the
/// truncated-and-digested form and stays readable on disk by the person
/// debugging it.
pub const MAX_PROVIDER_NAME_CHARS: usize = 80;

/// Why a slug cannot be used.
///
/// Four named failures rather than a boolean, because they need four different
/// sentences: one is "pick another name", one is "you already have this", one
/// is "that name belongs to something we ship", and one is "that name is too
/// long".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlugError {
    /// Nothing was typed, or it normalised to nothing.
    Empty,
    /// This company already has a provider with that slug.
    Taken,
    /// The catalogue ships that name.
    Reserved,
    /// Past [`MAX_PROVIDER_NAME_CHARS`].
    TooLong,
}

impl std::fmt::Display for SlugError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "a provider needs a name"),
            Self::Taken => write!(f, "this company already has a provider with that name"),
            Self::Reserved => write!(f, "that name belongs to a built-in provider"),
            Self::TooLong => write!(
                f,
                "a provider name may be at most {MAX_PROVIDER_NAME_CHARS} characters"
            ),
        }
    }
}

/// Turns a typed label into a slug.
///
/// **The slug is derived, never typed.** An operator names the thing; the
/// address falls out. Asking for both invites them to disagree, and the one the
/// operator sees in a routing entry would then be the one they never chose.
pub fn slugify(label: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for ch in label.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Whether a typed provider **name** may be used at all, before any slug is
/// derived from it.
///
/// Separate from [`check_slug`] because the two bound different things. The slug
/// is an address; the label is text that lands in the index blob, in the
/// provider list, and in every advisory that names a provider. A name can be
/// long while its slug is short (`slugify` drops everything that is not
/// alphanumeric), so bounding only the slug leaves a page of prose in the store
/// under a three-character address.
pub fn check_provider_name(label: &str) -> std::result::Result<(), SlugError> {
    let label = label.trim();
    if label.is_empty() {
        return Err(SlugError::Empty);
    }
    if label.chars().count() > MAX_PROVIDER_NAME_CHARS {
        return Err(SlugError::TooLong);
    }
    Ok(())
}

/// Whether `slug` may be used for a **custom** provider in a company that
/// already holds `existing`.
///
/// The catalogue check applies to custom providers only. Adding the catalogue's
/// own `groq` entry *should* take the slug `groq` — that is the same provider,
/// not a collision. It is a typed name shadowing a built-in that has to be
/// refused, because a routing entry saying `groq` would then mean two things.
///
/// The length bound is checked **here** rather than only on the label, because
/// this is the function that stands between a typed name and the address of a
/// secret ([`provider_key_key`]). A console mirrors it; a console is not a
/// security boundary.
pub fn check_slug(existing: &[Provider], slug: &str) -> std::result::Result<(), SlugError> {
    let slug = slug.trim();
    if slug.is_empty() {
        return Err(SlugError::Empty);
    }
    if slug.chars().count() > MAX_PROVIDER_NAME_CHARS {
        return Err(SlugError::TooLong);
    }
    if existing.iter().any(|p| p.slug == slug) {
        return Err(SlugError::Taken);
    }
    if catalogue::is_reserved_slug(slug) {
        return Err(SlugError::Reserved);
    }
    Ok(())
}

/// Reads the provider index — everything except entry zero.
async fn load_index(company: &CompanyId, secrets: &dyn SecretStore) -> Result<Vec<StoredProvider>> {
    let Some(SecretValue(raw)) = secrets.get(company, PROVIDER_INDEX_KEY).await? else {
        return Ok(Vec::new());
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw).map_err(|e| {
        OpenCompanyError::Store(format!("inference provider index is not valid JSON: {e}"))
    })
}

/// Writes the provider index.
async fn save_index(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    index: &[StoredProvider],
) -> Result<()> {
    let raw = serde_json::to_string(index)
        .map_err(|e| OpenCompanyError::Store(format!("serializing provider index: {e}")))?;
    secrets
        .set(company, PROVIDER_INDEX_KEY, SecretValue(raw))
        .await
}

/// Entry zero, read from the flat legacy slot. `None` when the company has no
/// runtime override — which is most companies, and not an error.
///
/// The slug is the normalised kind, which is both stable and readable: a routing
/// entry naming `openrouter` says what an operator would say out loud. The label
/// comes from the catalogue where the kind is one of ours, so the row reads
/// "OpenRouter" rather than "openrouter".
async fn entry_zero(company: &CompanyId, secrets: &dyn SecretStore) -> Result<Option<Provider>> {
    let Some(config) = super::load_runtime_config(company, secrets).await? else {
        return Ok(None);
    };
    Ok(Some(provider_from_runtime(&config)))
}

/// Entry zero's record, given the runtime blob. Split out so it is testable
/// without a store, and so the slug rule has one home.
fn provider_from_runtime(config: &RuntimeInference) -> Provider {
    let kind = normalize_provider(&config.provider).to_string();
    // The **slug** is not always the kind. A managed/TinyHumans config has the
    // OpenRouter-shaped kind and the TinyHumans account, and those are two
    // different questions: the kind says what shape of API this is, the slug
    // says whose account it is. Keyed on the kind, a managed credential would
    // sit in the slot a real OpenRouter account belongs in — and a company with
    // both would have one. It is also the address the resolver reads, through
    // the same function, so the two cannot drift.
    let slug = super::credential_slug(&config.provider).to_string();
    let label = if super::is_managed_choice(&config.provider) {
        "Managed".to_string()
    } else {
        catalogue::cloud_provider(&kind)
            .map(|p| p.label.to_string())
            .or_else(|| catalogue::local_runtime(&kind).map(|r| r.label.to_string()))
            .unwrap_or_else(|| kind.clone())
    };
    Provider {
        id: ProviderId::entry_zero(),
        slug,
        label,
        base_url: super::effective_base_url(&kind, config.base_url.as_deref()),
        kind,
        models: config.models.clone(),
        // Entry zero has no stored `enabled`, and inventing a place to put one
        // would mean writing to the slot this design promised not to touch. It
        // is the company's configured provider; it is on.
        enabled: true,
        origin: ProviderOrigin::EntryZero,
    }
}

/// Every provider this company holds, entry zero first.
///
/// Order is stable and meaningful: entry zero is the one a pre-list company
/// already had, so it stays at the top of the console's Connected list rather
/// than moving when a second is added.
pub async fn list_providers(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<Vec<Provider>> {
    let mut out = Vec::new();
    if let Some(zero) = entry_zero(company, secrets).await? {
        out.push(zero);
    }
    for stored in load_index(company, secrets).await? {
        out.push(Provider {
            id: stored.id,
            slug: stored.slug,
            label: stored.label,
            kind: stored.kind,
            base_url: stored.base_url,
            models: stored.models,
            enabled: stored.enabled,
            origin: ProviderOrigin::Indexed,
        });
    }
    Ok(out)
}

/// One provider by slug, or `None`.
pub async fn get_provider(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
) -> Result<Option<Provider>> {
    let slug = slug.trim();
    Ok(list_providers(company, secrets)
        .await?
        .into_iter()
        .find(|p| p.slug == slug))
}

/// Creates a provider, or replaces one that already has this slug.
///
/// Refuses to shadow entry zero: the flat slot is written through the existing
/// runtime-config route, and a second record with the same slug would make
/// "which one does this routing entry mean" unanswerable.
pub async fn put_provider(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    draft: ProviderDraft,
) -> Result<Provider> {
    let slug = draft.slug.trim().to_string();
    if slug.is_empty() {
        return Err(OpenCompanyError::Store(
            "a provider needs a slug".to_string(),
        ));
    }
    if let Some(zero) = entry_zero(company, secrets).await?
        && zero.slug == slug
    {
        return Err(OpenCompanyError::Store(format!(
            "`{slug}` is this company's existing provider; change it through the \
             inference config rather than adding a second record for it"
        )));
    }

    let mut index = load_index(company, secrets).await?;
    let id = index
        .iter()
        .find(|s| s.slug == slug)
        .map(|s| s.id.clone())
        .unwrap_or_default();
    let record = StoredProvider {
        id: id.clone(),
        slug: slug.clone(),
        label: draft.label,
        kind: draft.kind,
        base_url: draft.base_url,
        models: draft.models,
        enabled: draft.enabled,
    };
    match index.iter_mut().find(|s| s.slug == slug) {
        // Replace in place: the row keeps its position in the console's list,
        // and its id, which every routing entry that named it still resolves
        // through.
        Some(existing) => *existing = record.clone(),
        None => index.push(record.clone()),
    }
    save_index(company, secrets, &index).await?;
    Ok(Provider {
        id,
        slug,
        label: record.label,
        kind: record.kind,
        base_url: record.base_url,
        models: record.models,
        enabled: record.enabled,
        origin: ProviderOrigin::Indexed,
    })
}

/// Turns a provider on or off. `false` when no such provider exists.
///
/// Entry zero has no stored `enabled` — see [`provider_from_runtime`] — so this
/// answers `false` for it rather than pretending to have flipped something.
pub async fn set_enabled(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
    enabled: bool,
) -> Result<bool> {
    let slug = slug.trim();
    let mut index = load_index(company, secrets).await?;
    let Some(entry) = index.iter_mut().find(|s| s.slug == slug) else {
        return Ok(false);
    };
    entry.enabled = enabled;
    save_index(company, secrets, &index).await?;
    Ok(true)
}

/// Removes a provider **and clears its credential**. `false` when no such
/// provider exists.
///
/// The credential clear is not a tidiness step. The store has no delete, so a
/// key left behind reads back as set — and re-adding that slug would silently
/// reuse a credential the operator believed they had removed. That is the
/// behaviour of the design this is ported from, and it is a defect there.
///
/// The clear is issued **before** the index write, so a failure leaves the
/// provider visible with its credential intact rather than invisible with its
/// credential orphaned. Of the two half-states, only one is discoverable by the
/// operator.
pub async fn delete_provider(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
) -> Result<bool> {
    let slug = slug.trim();
    let mut index = load_index(company, secrets).await?;
    if !index.iter().any(|s| s.slug == slug) {
        return Ok(false);
    }
    secrets
        .set(company, &provider_key_key(slug), SecretValue(String::new()))
        .await
        .map_err(|e| {
            OpenCompanyError::Store(format!(
                "could not clear the stored credential for provider `{slug}`, so it was \
                 not removed: {e}. Leaving a credential behind would let re-adding \
                 `{slug}` silently reuse it"
            ))
        })?;
    index.retain(|s| s.slug != slug);
    save_index(company, secrets, &index).await.map(|()| true)
}

/// Writes a provider's outbound credential, **and converges its address**.
///
/// Write-only intake: nothing reads this back out to a caller that is not about
/// to present it.
///
/// ## Lazy convergence rather than a migration
///
/// The credential goes to `provider/<slug>/key` and the legacy `inference/key`
/// is cleared in the same operation. There is no flag day and no half-migrated
/// state: an existing company keeps working untouched on the read fallback in
/// [`load_provider_key`], and the first save of that provider moves the key and
/// retires the old slot. When nothing is left reading the fallback it is one
/// line to delete.
///
/// The clear is a **write of the empty string**, because the port has no delete
/// — and it has to be issued rather than inferred. A key left at the old address
/// after the new one is written is an orphaned secret, which is why a failure
/// here is logged loudly rather than swallowed.
pub async fn store_provider_key(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    provider: &Provider,
    key: &str,
) -> Result<()> {
    secrets
        .set(company, &provider.key_key(), SecretValue(key.to_string()))
        .await?;
    if let Some(legacy) = provider.legacy_key_key()
        && let Err(err) = secrets
            .set(company, legacy, SecretValue(String::new()))
            .await
    {
        tracing::error!(
            company = %company,
            provider = %provider.slug,
            legacy_key = legacy,
            error = %err,
            "wrote a provider credential to its own address but could not clear the \
             legacy slot; a secret is now orphaned there",
        );
    }
    Ok(())
}

/// Reads a provider's outbound credential, or the empty string when unset.
///
/// `provider/<slug>/key` first, then the legacy flat slot for entry zero. The
/// fallback is the whole of the backward compatibility story: a company that has
/// never saved since the addresses were made uniform still resolves, and one
/// save moves it. See [`store_provider_key`].
pub async fn load_provider_key(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    provider: &Provider,
) -> Result<String> {
    if let Some(SecretValue(raw)) = secrets.get(company, &provider.key_key()).await?
        && !raw.trim().is_empty()
    {
        return Ok(raw);
    }
    if let Some(legacy) = provider.legacy_key_key()
        && let Some(SecretValue(raw)) = secrets.get(company, legacy).await?
        && !raw.trim().is_empty()
    {
        return Ok(raw);
    }
    Ok(String::new())
}

/// Whether a provider has a credential stored — the non-secret fact a read route
/// may carry.
///
/// Derived by asking the store, never by keeping a flag. A stored boolean goes
/// stale the moment a secret is cleared by any other path, and then the console
/// claims a key exists that does not.
pub async fn provider_key_configured(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    provider: &Provider,
) -> Result<bool> {
    Ok(!load_provider_key(company, secrets, provider)
        .await?
        .trim()
        .is_empty())
}

// ---- the managed tier's own switch ------------------------------------------

/// Whether the managed tier is a routing target.
///
/// Its own key because managed has **no provider record** — it resolves through
/// a chain rather than from a row, so there is no `enabled` field on anything to
/// hang this off. Absent reads as **on**: every company that existed before this
/// key did had managed available, and a missing key must not switch it off.
pub const MANAGED_ENABLED_KEY: &str = "inference/managed/enabled";

/// Whether managed may serve a workload. `true` unless explicitly switched off.
pub async fn managed_enabled(company: &CompanyId, secrets: &dyn SecretStore) -> Result<bool> {
    let Some(SecretValue(raw)) = secrets.get(company, MANAGED_ENABLED_KEY).await? else {
        return Ok(true);
    };
    // Anything but a literal "false" is on, including the empty string a clear
    // leaves behind — so a cleared key restores the default rather than
    // silently disabling the one provider a company always has.
    Ok(raw.trim() != "false")
}

/// Switches the managed tier on or off as a routing target.
///
/// **This is not the credential.** Switching managed off leaves every step of
/// its chain exactly where it was; it stops being somewhere a workload can be
/// routed, which is the same thing `enabled` means on any other provider.
pub async fn set_managed_enabled(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    enabled: bool,
) -> Result<()> {
    secrets
        .set(
            company,
            MANAGED_ENABLED_KEY,
            SecretValue(if enabled { "true" } else { "false" }.to_string()),
        )
        .await
}

// ---- the default provider ---------------------------------------------------

/// The [`SecretStore`] key naming the company's default provider.
pub const DEFAULT_PROVIDER_KEY: &str = "inference/default";

/// Which provider this company has **said** is its default, if any.
///
/// A slug in a slot of its own rather than a flag on each record, and that shape
/// is the point: **two defaults are not representable.** A boolean per record
/// can be true twice, and then the reader has to pick — which is a rule nobody
/// wrote down and everybody would have to agree on. One slot, one answer.
///
/// `None` is every company that has not said, which is every company that
/// existed before this. There is no backfill: [`resolve::primary`] falls back to
/// the first enabled provider, which is exactly what it did before.
pub async fn load_default_slug(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<Option<String>> {
    let Some(SecretValue(raw)) = secrets.get(company, DEFAULT_PROVIDER_KEY).await? else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

/// Marks `slug` as this company's default, replacing whatever was marked.
///
/// "Setting a default clears the previous one" is not an operation here — it is
/// the storage shape. One slot cannot hold two slugs.
pub async fn set_default_slug(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
) -> Result<()> {
    secrets
        .set(
            company,
            DEFAULT_PROVIDER_KEY,
            SecretValue(slug.trim().to_string()),
        )
        .await
}

/// Unmarks whatever is marked. A write of the empty string, because the port has
/// no delete.
pub async fn clear_default_slug(company: &CompanyId, secrets: &dyn SecretStore) -> Result<()> {
    secrets
        .set(company, DEFAULT_PROVIDER_KEY, SecretValue(String::new()))
        .await
}

// ---- routes -----------------------------------------------------------------

/// The [`SecretStore`] key holding the routing table: tier → route string.
///
/// Beside the providers rather than inside them on purpose. A route is a
/// statement *about* the set of providers ("reasoning goes to acme"), not a
/// property of one of them, and putting it on the record would mean a provider
/// blob that has to be rewritten whenever an unrelated row is re-pointed.
pub const ROUTES_KEY: &str = "inference/routes";

/// The persisted routing table: tier name → the route string an operator would
/// type.
///
/// **Stored as the grammar, not as a tagged enum.** The value in the store is
/// the value the console shows and an operator hand-edits (`acme:gpt-5`), so
/// there is one representation rather than a wire shape and a storage shape that
/// can disagree. [`ProviderRef::parse`](super::resolve::ProviderRef::parse) is
/// total — every string is *some* route — which is what makes that safe.
type StoredRoutes = BTreeMap<String, String>;

/// This company's routing table. Empty is the common case and not an error: a
/// company with no routes sends every workload through the primary.
pub async fn load_routes(company: &CompanyId, secrets: &dyn SecretStore) -> Result<Routes> {
    let Some(SecretValue(raw)) = secrets.get(company, ROUTES_KEY).await? else {
        return Ok(Routes::new());
    };
    if raw.trim().is_empty() {
        return Ok(Routes::new());
    }
    let stored: StoredRoutes = serde_json::from_str(&raw).map_err(|e| {
        OpenCompanyError::Store(format!("inference routes are not valid JSON: {e}"))
    })?;
    Ok(stored
        .into_iter()
        .map(|(tier, raw)| (tier, ProviderRef::parse(&raw)))
        .collect())
}

/// Writes this company's routing table.
///
/// [`ProviderRef::Default`] entries are **dropped rather than stored**. Unset is
/// an absence, and persisting it as `""` would make "never set" and "set back to
/// nothing" two states that read the same but occupy different storage — a
/// distinction with no meaning and one more thing to keep in step.
pub async fn save_routes(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    routes: &Routes,
) -> Result<()> {
    let stored: StoredRoutes = routes
        .iter()
        .filter(|(_, route)| !matches!(route, ProviderRef::Default))
        .map(|(tier, route)| (tier.clone(), route.to_route_string()))
        .collect();
    let raw = serde_json::to_string(&stored)
        .map_err(|e| OpenCompanyError::Store(format!("serializing inference routes: {e}")))?;
    secrets.set(company, ROUTES_KEY, SecretValue(raw)).await
}

// ---- health -----------------------------------------------------------------

/// The [`SecretStore`] key holding per-provider health.
pub const HEALTH_KEY: &str = "inference/health";

/// What was last learnt about reaching a provider.
///
/// Not a credential, but it lives in the same store because it is per company
/// and per provider and there is no other per-company blob store to put it in.
/// It derives `Serialize` because there is nothing secret in it — a class name
/// and a timestamp — and that is exactly the check to make before adding a
/// field here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderHealth {
    /// `ok`, or the [`ProbeClass`](super::probe::ProbeClass) of the last failure.
    pub state: String,
    /// When it was learnt, RFC 3339.
    pub at: String,
}

/// The whole health map: provider slug → what we last learnt.
///
/// **Keyed on the provider, never on the endpoint.** A 401 is an answer about
/// the credential that was presented, not a property of the address — two
/// providers can point at one gateway with different keys, and caching one's
/// rejection against the endpoint would condemn the other.
pub type HealthMap = BTreeMap<String, ProviderHealth>;

/// Every health record this company holds.
pub async fn load_health(company: &CompanyId, secrets: &dyn SecretStore) -> Result<HealthMap> {
    let Some(SecretValue(raw)) = secrets.get(company, HEALTH_KEY).await? else {
        return Ok(HealthMap::new());
    };
    if raw.trim().is_empty() {
        return Ok(HealthMap::new());
    }
    // A health blob that will not parse is not worth failing a status read over:
    // it holds no configuration and nothing depends on it being present. Report
    // "nothing learnt" and let the next probe rewrite it.
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

/// Records what was just learnt about `slug`, and says whether anything changed.
///
/// **Latched once per failure episode, not once per retry.** A write only
/// happens when the *state* differs from what is stored, so a provider failing
/// the same way on every turn keeps the timestamp of the first failure in that
/// episode rather than moving it forward on each retry. That is the rule behind
/// the ~9k events for 6 users the design this is ported from had to fix, and the
/// timestamp is more useful this way besides: "rejecting since 09:14" is a fact,
/// "rejecting as of one second ago" is a heartbeat.
///
/// Returns `true` when the record moved, so a caller can log the transition
/// rather than the repetition.
pub async fn record_health(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
    state: &str,
    at: &str,
) -> Result<bool> {
    let mut health = load_health(company, secrets).await?;
    if health.get(slug).is_some_and(|h| h.state == state) {
        return Ok(false);
    }
    health.insert(
        slug.to_string(),
        ProviderHealth {
            state: state.to_string(),
            at: at.to_string(),
        },
    );
    write_health(company, secrets, &health).await?;
    Ok(true)
}

/// Drops a provider's health record — part of deleting it.
///
/// A stale record would otherwise reappear the moment the slug is reused, and
/// claim a state nothing had established about the new provider.
pub async fn forget_health(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
) -> Result<()> {
    let mut health = load_health(company, secrets).await?;
    if health.remove(slug).is_none() {
        return Ok(());
    }
    write_health(company, secrets, &health).await
}

async fn write_health(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    health: &HealthMap,
) -> Result<()> {
    let raw = serde_json::to_string(health)
        .map_err(|e| OpenCompanyError::Store(format!("serializing inference health: {e}")))?;
    secrets.set(company, HEALTH_KEY, SecretValue(raw)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;

    #[derive(Default)]
    struct MemSecrets {
        map: Mutex<HashMap<String, String>>,
    }

    #[async_trait]
    impl SecretStore for MemSecrets {
        async fn get(&self, _c: &CompanyId, key: &str) -> Result<Option<SecretValue>> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(key)
                .map(|v| SecretValue(v.clone())))
        }
        async fn set(&self, _c: &CompanyId, key: &str, value: SecretValue) -> Result<()> {
            self.map.lock().unwrap().insert(key.to_string(), value.0);
            Ok(())
        }
    }

    /// A store whose writes to one key always fail — for the rollback rules.
    struct FailsWriting {
        inner: MemSecrets,
        failing_key: String,
    }

    #[async_trait]
    impl SecretStore for FailsWriting {
        async fn get(&self, c: &CompanyId, key: &str) -> Result<Option<SecretValue>> {
            self.inner.get(c, key).await
        }
        async fn set(&self, c: &CompanyId, key: &str, value: SecretValue) -> Result<()> {
            if key == self.failing_key {
                return Err(OpenCompanyError::Store("disk is on fire".into()));
            }
            self.inner.set(c, key, value).await
        }
    }

    fn company() -> CompanyId {
        CompanyId::new("acme")
    }

    fn draft(slug: &str) -> ProviderDraft {
        ProviderDraft {
            slug: slug.to_string(),
            label: slug.to_string(),
            kind: "openai_compatible".to_string(),
            base_url: format!("https://{slug}.example/v1"),
            models: BTreeMap::new(),
            enabled: true,
        }
    }

    async fn write_entry_zero(secrets: &dyn SecretStore, provider: &str) {
        let config = RuntimeInference {
            provider: provider.to_string(),
            base_url: None,
            models: BTreeMap::new(),
        };
        super::super::save_runtime_config(&company(), secrets, &config)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_company_with_nothing_configured_has_no_providers() {
        let secrets = MemSecrets::default();
        assert!(
            list_providers(&company(), &secrets)
                .await
                .unwrap()
                .is_empty(),
            "no config is an empty list, not an error"
        );
    }

    #[tokio::test]
    async fn the_legacy_flat_slot_reads_back_as_entry_zero() {
        let secrets = MemSecrets::default();
        write_entry_zero(&secrets, "openrouter").await;

        let providers = list_providers(&company(), &secrets).await.unwrap();
        assert_eq!(providers.len(), 1);
        let zero = &providers[0];
        assert_eq!(zero.slug, "openrouter");
        assert_eq!(zero.label, "OpenRouter", "the catalogue supplies the label");
        assert_eq!(zero.origin, ProviderOrigin::EntryZero);
        assert_eq!(zero.id.as_str(), ENTRY_ZERO_ID);
        assert!(zero.enabled);
        // Written at the uniform address; still READ from the legacy one until
        // the first save converges it. One address rule, one readable fallback.
        assert_eq!(zero.key_key(), provider_key_key("openrouter"));
        assert_eq!(zero.legacy_key_key(), Some(KEY_KEY));
    }

    #[tokio::test]
    async fn a_legacy_credential_is_read_from_the_flat_slot_and_moved_by_one_save() {
        // Lazy convergence. An existing company keeps working untouched, and the
        // first save of that provider moves the key and clears the old slot —
        // no flag day, and no half-migrated state on a store with no
        // transaction.
        let secrets = MemSecrets::default();
        write_entry_zero(&secrets, "openrouter").await;
        secrets
            .set(&company(), KEY_KEY, SecretValue("sk-not-a-real-key".into()))
            .await
            .unwrap();

        let zero = list_providers(&company(), &secrets).await.unwrap()[0].clone();
        assert_eq!(
            load_provider_key(&company(), &secrets, &zero)
                .await
                .unwrap(),
            "sk-not-a-real-key",
            "the fallback is what keeps an untouched company working"
        );

        store_provider_key(&company(), &secrets, &zero, "sk-not-a-real-key-2")
            .await
            .unwrap();
        assert_eq!(
            secrets.get(&company(), &zero.key_key()).await.unwrap(),
            Some(SecretValue("sk-not-a-real-key-2".into())),
        );
        assert_eq!(
            secrets.get(&company(), KEY_KEY).await.unwrap(),
            Some(SecretValue(String::new())),
            "the legacy slot is cleared in the same operation; a key left there \
             after the new one is written is an orphaned secret"
        );
    }

    #[tokio::test]
    async fn entry_zero_keeps_its_id_across_reads() {
        // A generated id would have to be written back to be stable, and the
        // write path into the legacy slot is the one thing this design will not
        // do on a read.
        let secrets = MemSecrets::default();
        write_entry_zero(&secrets, "openrouter").await;
        let first = list_providers(&company(), &secrets).await.unwrap()[0]
            .id
            .clone();
        let second = list_providers(&company(), &secrets).await.unwrap()[0]
            .id
            .clone();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn the_legacy_managed_alias_resolves_rather_than_failing() {
        // A stored runtime blob is data an operator cannot hand-edit, so a value
        // the console itself once wrote must not strand them.
        let secrets = MemSecrets::default();
        write_entry_zero(&secrets, "managed").await;
        let providers = list_providers(&company(), &secrets).await.unwrap();
        // The **kind** normalizes onto OpenRouter — that is the shape of API it
        // speaks. The **slug** does not: it says whose account this is, and a
        // managed config is the TinyHumans account. Keyed on the kind, the
        // managed credential would sit in the slot a real OpenRouter account
        // belongs in, and a company holding both would have one.
        assert_eq!(providers[0].kind, "openrouter");
        assert_eq!(providers[0].slug, super::super::MANAGED_SLUG);
        assert_eq!(providers[0].label, "Managed");
        assert_eq!(
            providers[0].key_key(),
            provider_key_key(super::super::MANAGED_SLUG)
        );
    }

    #[tokio::test]
    async fn adding_a_second_provider_leaves_entry_zero_first() {
        let secrets = MemSecrets::default();
        write_entry_zero(&secrets, "openrouter").await;
        put_provider(&company(), &secrets, draft("acme"))
            .await
            .unwrap();

        let providers = list_providers(&company(), &secrets).await.unwrap();
        assert_eq!(
            providers
                .iter()
                .map(|p| p.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["openrouter", "acme"]
        );
        assert_eq!(providers[1].origin, ProviderOrigin::Indexed);
        assert_eq!(providers[1].key_key(), "provider/acme/key");
    }

    #[tokio::test]
    async fn two_providers_hold_two_independent_credentials() {
        // The defect this whole change exists to fix: today one company has one
        // credential slot, so switching provider without re-entering a key
        // presents the previous vendor's credential to the new one.
        let secrets = MemSecrets::default();
        write_entry_zero(&secrets, "openrouter").await;
        let zero = list_providers(&company(), &secrets).await.unwrap()[0].clone();
        let acme = put_provider(&company(), &secrets, draft("acme"))
            .await
            .unwrap();

        store_provider_key(&company(), &secrets, &zero, "sk-not-a-real-key-zero")
            .await
            .unwrap();
        store_provider_key(&company(), &secrets, &acme, "sk-not-a-real-key-acme")
            .await
            .unwrap();

        assert_eq!(
            load_provider_key(&company(), &secrets, &zero)
                .await
                .unwrap(),
            "sk-not-a-real-key-zero"
        );
        assert_eq!(
            load_provider_key(&company(), &secrets, &acme)
                .await
                .unwrap(),
            "sk-not-a-real-key-acme"
        );
        assert!(
            provider_key_configured(&company(), &secrets, &zero)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_blank_credential_reads_as_not_configured() {
        // The store has no delete: clearing is a write of the empty string, so
        // "cleared" and "never set" are deliberately the same state.
        let secrets = MemSecrets::default();
        let acme = put_provider(&company(), &secrets, draft("acme"))
            .await
            .unwrap();
        store_provider_key(&company(), &secrets, &acme, "sk-not-a-real-key")
            .await
            .unwrap();
        assert!(
            provider_key_configured(&company(), &secrets, &acme)
                .await
                .unwrap()
        );
        store_provider_key(&company(), &secrets, &acme, "   ")
            .await
            .unwrap();
        assert!(
            !provider_key_configured(&company(), &secrets, &acme)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn deleting_a_provider_clears_its_credential() {
        let secrets = MemSecrets::default();
        let acme = put_provider(&company(), &secrets, draft("acme"))
            .await
            .unwrap();
        store_provider_key(&company(), &secrets, &acme, "sk-not-a-real-key")
            .await
            .unwrap();

        assert!(delete_provider(&company(), &secrets, "acme").await.unwrap());
        assert!(
            list_providers(&company(), &secrets)
                .await
                .unwrap()
                .is_empty()
        );

        // Re-adding the same slug must NOT inherit the old credential.
        let again = put_provider(&company(), &secrets, draft("acme"))
            .await
            .unwrap();
        assert!(
            !provider_key_configured(&company(), &secrets, &again)
                .await
                .unwrap(),
            "re-adding a deleted slug silently reused its key"
        );
    }

    #[tokio::test]
    async fn a_failed_credential_clear_keeps_the_provider_visible() {
        // Of the two half-states, "still listed, key intact" is the one the
        // operator can see and act on. "Gone from the list, key on disk" is not.
        let secrets = FailsWriting {
            inner: MemSecrets::default(),
            failing_key: provider_key_key("acme"),
        };
        put_provider(&company(), &secrets, draft("acme"))
            .await
            .unwrap();

        let err = delete_provider(&company(), &secrets, "acme")
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("could not clear the stored credential"),
            "a failed clear must be loud, got: {err}"
        );
        assert_eq!(
            list_providers(&company(), &secrets).await.unwrap().len(),
            1,
            "the provider stayed visible"
        );
    }

    #[tokio::test]
    async fn deleting_something_that_is_not_there_is_not_an_error() {
        let secrets = MemSecrets::default();
        assert!(!delete_provider(&company(), &secrets, "nope").await.unwrap());
    }

    #[tokio::test]
    async fn disabling_keeps_the_endpoint_the_label_and_the_credential() {
        let secrets = MemSecrets::default();
        let acme = put_provider(&company(), &secrets, draft("acme"))
            .await
            .unwrap();
        store_provider_key(&company(), &secrets, &acme, "sk-not-a-real-key")
            .await
            .unwrap();

        assert!(
            set_enabled(&company(), &secrets, "acme", false)
                .await
                .unwrap()
        );
        let stored = get_provider(&company(), &secrets, "acme")
            .await
            .unwrap()
            .unwrap();
        assert!(!stored.enabled);
        assert_eq!(stored.base_url, "https://acme.example/v1");
        assert_eq!(stored.label, "acme");
        assert!(
            provider_key_configured(&company(), &secrets, &stored)
                .await
                .unwrap(),
            "disabled is not deleted"
        );
    }

    #[tokio::test]
    async fn replacing_a_provider_keeps_its_id() {
        // Identity survives a rename; that is the reason id and slug are two
        // fields rather than one.
        let secrets = MemSecrets::default();
        let first = put_provider(&company(), &secrets, draft("acme"))
            .await
            .unwrap();
        let mut renamed = draft("acme");
        renamed.label = "Acme gateway".to_string();
        let second = put_provider(&company(), &secrets, renamed).await.unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.label, "Acme gateway");
        assert_eq!(list_providers(&company(), &secrets).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_generated_id_is_not_the_entry_zero_sentinel() {
        let a = ProviderId::new();
        let b = ProviderId::new();
        assert_ne!(a, b, "ids must not repeat");
        assert_ne!(a.as_str(), ENTRY_ZERO_ID);
        assert!(a.as_str().starts_with("prv_"));
    }

    #[tokio::test]
    async fn a_second_record_may_not_shadow_entry_zero() {
        let secrets = MemSecrets::default();
        write_entry_zero(&secrets, "openrouter").await;
        let err = put_provider(&company(), &secrets, draft("openrouter"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("existing provider"), "{err}");
    }

    #[test]
    fn a_slug_is_derived_from_the_name_never_typed() {
        assert_eq!(slugify("Acme Gateway"), "acme-gateway");
        assert_eq!(slugify("  My  OpenRouter!!  "), "my-openrouter");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn a_custom_slug_is_refused_for_three_named_reasons() {
        let existing = vec![Provider {
            id: ProviderId::new(),
            slug: "acme".into(),
            label: "Acme".into(),
            kind: "openai_compatible".into(),
            base_url: "https://acme.example/v1".into(),
            models: BTreeMap::new(),
            enabled: true,
            origin: ProviderOrigin::Indexed,
        }];
        assert_eq!(check_slug(&existing, "   "), Err(SlugError::Empty));
        assert_eq!(check_slug(&existing, "acme"), Err(SlugError::Taken));
        assert_eq!(check_slug(&existing, "groq"), Err(SlugError::Reserved));
        assert_eq!(check_slug(&existing, "acme-two"), Ok(()));
    }

    #[test]
    fn a_provider_name_is_bounded_at_the_limit_and_refused_past_it() {
        // The bound exists because the name becomes the address of a secret.
        // At the limit is a legal name; one character past it is not, and the
        // refusal happens here rather than at the store, where it used to
        // arrive as `ENAMETOOLONG` after a write had already landed.
        let at_limit = "a".repeat(MAX_PROVIDER_NAME_CHARS);
        let past_limit = "a".repeat(MAX_PROVIDER_NAME_CHARS + 1);

        assert_eq!(check_provider_name(&at_limit), Ok(()));
        assert_eq!(check_provider_name(&past_limit), Err(SlugError::TooLong));
        assert_eq!(check_provider_name("  "), Err(SlugError::Empty));

        assert_eq!(check_slug(&[], &at_limit), Ok(()));
        assert_eq!(check_slug(&[], &past_limit), Err(SlugError::TooLong));

        // Characters, not bytes: a name of multi-byte characters is judged by
        // what the operator typed rather than by how UTF-8 happens to store it.
        let multibyte = "é".repeat(MAX_PROVIDER_NAME_CHARS);
        assert_eq!(check_provider_name(&multibyte), Ok(()));
    }

    #[test]
    fn a_bounded_name_keeps_its_credential_key_inside_the_filename_budget() {
        // Why 80 and not some larger round number: the derived secret key has
        // to stay short enough that the canonical filename is the readable
        // `%k-` form rather than the truncated-and-digested `%l-` one. The
        // slug alphabet is `[a-z0-9-]`, one byte per character once
        // percent-encoded, and `provider/` + `/key` add 17.
        let key = provider_key_key(&"a".repeat(MAX_PROVIDER_NAME_CHARS));
        // `provider/` + `/key` is 13 characters around the slug.
        assert_eq!(key.len(), MAX_PROVIDER_NAME_CHARS + 13);
        // Percent-encoding is what the budget is measured in. The slug alphabet
        // (`[a-z0-9-]`) survives as one byte per character; the two `/`
        // separators become `%2F`, three bytes each.
        let encoded_len = key.len() + 2 * 2;
        assert!(
            encoded_len < 200,
            "a bounded name must not need a truncated secret filename: {encoded_len} bytes"
        );
    }

    #[tokio::test]
    async fn an_index_written_before_enabled_existed_reads_as_enabled() {
        // A missing field must not read as "every provider is off", which is
        // what `#[serde(default)]` on a bool would have given.
        let secrets = MemSecrets::default();
        secrets
            .set(
                &company(),
                PROVIDER_INDEX_KEY,
                SecretValue(
                    r#"[{"id":"prv_old","slug":"acme","label":"Acme","kind":"openai_compatible","base_url":"https://acme.example/v1"}]"#
                        .to_string(),
                ),
            )
            .await
            .unwrap();
        let providers = list_providers(&company(), &secrets).await.unwrap();
        assert_eq!(providers.len(), 1);
        assert!(providers[0].enabled);
    }

    #[tokio::test]
    async fn a_malformed_index_is_surfaced_not_swallowed() {
        let secrets = MemSecrets::default();
        secrets
            .set(
                &company(),
                PROVIDER_INDEX_KEY,
                SecretValue("{not json".into()),
            )
            .await
            .unwrap();
        let err = list_providers(&company(), &secrets).await.unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }

    #[tokio::test]
    async fn an_empty_index_blob_is_an_empty_list() {
        let secrets = MemSecrets::default();
        secrets
            .set(&company(), PROVIDER_INDEX_KEY, SecretValue(String::new()))
            .await
            .unwrap();
        assert!(
            list_providers(&company(), &secrets)
                .await
                .unwrap()
                .is_empty()
        );
    }

    // ---- routes -------------------------------------------------------------

    #[tokio::test]
    async fn a_company_with_no_routes_reads_an_empty_table() {
        let secrets = MemSecrets::default();
        assert!(load_routes(&company(), &secrets).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn routes_round_trip_through_the_grammar_an_operator_types() {
        let secrets = MemSecrets::default();
        let mut routes = Routes::new();
        routes.insert("reasoning-v1".to_string(), ProviderRef::parse("acme:gpt-5"));
        routes.insert("chat-v1".to_string(), ProviderRef::Managed);
        routes.insert("vision-v1".to_string(), ProviderRef::parse("local:llava"));
        save_routes(&company(), &secrets, &routes).await.unwrap();

        let read = load_routes(&company(), &secrets).await.unwrap();
        assert_eq!(read, routes);
        // And the stored form really is the text, so a person reading raw keys
        // sees what they would have typed.
        let raw = secrets
            .get(&company(), ROUTES_KEY)
            .await
            .unwrap()
            .unwrap()
            .0;
        assert!(raw.contains("acme:gpt-5"), "{raw}");
    }

    #[tokio::test]
    async fn an_unset_route_is_dropped_rather_than_stored_as_empty() {
        let secrets = MemSecrets::default();
        let mut routes = Routes::new();
        routes.insert("chat-v1".to_string(), ProviderRef::Default);
        routes.insert("agentic-v1".to_string(), ProviderRef::parse("acme"));
        save_routes(&company(), &secrets, &routes).await.unwrap();

        let read = load_routes(&company(), &secrets).await.unwrap();
        assert!(
            !read.contains_key("chat-v1"),
            "unset must not persist: {read:?}"
        );
        assert_eq!(read.get("agentic-v1"), Some(&ProviderRef::parse("acme")));
    }

    #[tokio::test]
    async fn an_unreadable_routes_blob_is_an_error_rather_than_silently_empty() {
        // Routes decide where a company's spend goes. Reading a corrupt table as
        // "no routes" would move every workload onto the primary without saying
        // so, which is the silent-demotion failure the resolver refuses.
        let secrets = MemSecrets::default();
        secrets
            .set(&company(), ROUTES_KEY, SecretValue("{oops".into()))
            .await
            .unwrap();
        let err = load_routes(&company(), &secrets).await.unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }

    // ---- health -------------------------------------------------------------

    #[tokio::test]
    async fn health_is_latched_once_per_failure_episode_not_once_per_retry() {
        let secrets = MemSecrets::default();
        assert!(
            record_health(&company(), &secrets, "acme", "auth", "2026-09-11T09:14:00Z")
                .await
                .unwrap(),
            "the first observation moves the record"
        );
        assert!(
            !record_health(&company(), &secrets, "acme", "auth", "2026-09-11T09:15:00Z")
                .await
                .unwrap(),
            "the same failure again must not move the record"
        );
        let health = load_health(&company(), &secrets).await.unwrap();
        assert_eq!(
            health.get("acme").unwrap().at,
            "2026-09-11T09:14:00Z",
            "the timestamp names when the episode began, not the latest retry"
        );
    }

    #[tokio::test]
    async fn a_state_change_moves_the_record() {
        let secrets = MemSecrets::default();
        record_health(&company(), &secrets, "acme", "auth", "2026-09-11T09:14:00Z")
            .await
            .unwrap();
        assert!(
            record_health(&company(), &secrets, "acme", "ok", "2026-09-11T10:00:00Z")
                .await
                .unwrap()
        );
        let health = load_health(&company(), &secrets).await.unwrap();
        assert_eq!(health.get("acme").unwrap().state, "ok");
        assert_eq!(health.get("acme").unwrap().at, "2026-09-11T10:00:00Z");
    }

    #[tokio::test]
    async fn health_is_per_provider_so_one_rejection_does_not_condemn_a_sibling() {
        // Two providers, one endpoint, two keys: a 401 is an answer about the
        // credential presented, never about the address.
        let secrets = MemSecrets::default();
        record_health(&company(), &secrets, "acme", "auth", "2026-09-11T09:14:00Z")
            .await
            .unwrap();
        record_health(
            &company(),
            &secrets,
            "acme-team",
            "ok",
            "2026-09-11T09:14:00Z",
        )
        .await
        .unwrap();
        let health = load_health(&company(), &secrets).await.unwrap();
        assert_eq!(health.get("acme").unwrap().state, "auth");
        assert_eq!(health.get("acme-team").unwrap().state, "ok");
    }

    #[tokio::test]
    async fn forgetting_health_stops_a_reused_slug_inheriting_a_state() {
        let secrets = MemSecrets::default();
        record_health(&company(), &secrets, "acme", "auth", "2026-09-11T09:14:00Z")
            .await
            .unwrap();
        forget_health(&company(), &secrets, "acme").await.unwrap();
        assert!(
            !load_health(&company(), &secrets)
                .await
                .unwrap()
                .contains_key("acme")
        );
        // Forgetting something that was never there is not an error.
        forget_health(&company(), &secrets, "ghost").await.unwrap();
    }

    #[tokio::test]
    async fn an_unreadable_health_blob_reads_as_nothing_learnt() {
        // The opposite call from routes, and deliberately: health holds no
        // configuration, so failing a status read over it would take the whole
        // page down to preserve a decoration.
        let secrets = MemSecrets::default();
        secrets
            .set(&company(), HEALTH_KEY, SecretValue("{oops".into()))
            .await
            .unwrap();
        assert!(load_health(&company(), &secrets).await.unwrap().is_empty());
    }
    /// What a routing write actually leaves behind, versus what was asked for.
    ///
    /// The `PUT` route used to answer with the table it built from the **request
    /// body**, which made the response a picture of the ask rather than of the
    /// state — so any divergence between the two was invisible by construction,
    /// and a save that landed nowhere still came back carrying the operator's own
    /// intent. This is the smallest concrete divergence, and it is not
    /// hypothetical: `save_routes` drops `Default` entries, because an absence is
    /// how "nothing set here" is stored. Echoing the request claimed a row had
    /// been written that the store deliberately holds nothing for.
    #[tokio::test]
    async fn a_routing_write_does_not_store_what_it_was_handed() {
        let company = CompanyId::new("acme");
        let secrets = MemSecrets::default();

        let mut asked = Routes::new();
        asked.insert("chat-v1".into(), ProviderRef::parse("acme:gpt-5"));
        // The operator put this row back to "follow the default".
        asked.insert("reasoning-v1".into(), ProviderRef::parse(""));
        save_routes(&company, &secrets, &asked).await.unwrap();

        let stored = load_routes(&company, &secrets).await.unwrap();
        assert_eq!(
            stored.get("chat-v1"),
            Some(&ProviderRef::parse("acme:gpt-5"))
        );
        assert!(
            !stored.contains_key("reasoning-v1"),
            "an unset row is stored as an absence, so a response echoing the request \
             would claim a row that is not there"
        );
        assert_ne!(
            asked, stored,
            "the ask and the stored table differ, which is why the route reads back"
        );
    }

    /// A routing write that cannot land must not read back as if it had.
    #[tokio::test]
    async fn a_dropped_routing_write_is_visible_on_the_read_back() {
        let company = CompanyId::new("acme");
        let secrets = FailsWriting {
            inner: MemSecrets::default(),
            failing_key: ROUTES_KEY.to_string(),
        };

        let mut asked = Routes::new();
        asked.insert("chat-v1".into(), ProviderRef::parse("acme:gpt-5"));
        assert!(
            save_routes(&company, &secrets, &asked).await.is_err(),
            "the write itself reports the failure"
        );
        // And the read-back agrees with the store rather than with the ask —
        // which is the property the route now answers from.
        let stored = load_routes(&company, &secrets).await.unwrap();
        assert!(stored.is_empty());
    }
}
