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

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};

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

// ---------------------------------------------------------------------------
// The per-company inference-config lock
// ---------------------------------------------------------------------------

/// One process-wide lock per company, guarding every mutation that reads an
/// in-use guard and then writes: provider delete, disable and key clear
/// (`server::ops::inference::providers`), the agent-pair PATCH
/// (`server::ops::team_agent`), set-default, and the X1 first-add
/// auto-default. [`company::company_key::fan_out`](crate::company::company_key::fan_out)'s
/// account-key save also takes it before writing [`PROVIDER_INDEX_KEY`] or the
/// default marker, so a fan-out save and a provider-page edit can never both
/// pass their checks against the same pre-write state.
///
/// Mirrors `company_key::fan_out::slot_guard`'s pattern exactly — itself
/// copied from `search::store`'s `INDEX_LOCKS` — one `tokio::sync::Mutex` per
/// company id in a process-wide map. This serialises mutations **within one
/// process**, which is the whole of a deployment (one container per tenant);
/// it is not a distributed lock and does nothing across replicas.
///
/// Never hold the guard across a network call: probe a provider's catalog
/// before taking it, or after releasing it, never while it is held. Take it,
/// re-read the state the guard check depends on, check, write, drop.
///
/// **Lock order:** a caller that also holds
/// [`slot_guard`](crate::company::company_key::fan_out::slot_guard) — today
/// only the account-key fan-out — must take `slot_guard` first and this lock
/// second. No path here takes `slot_guard` at all, so that order is the only
/// one that can ever be built; keep it that way rather than introducing a
/// second acquisition order two call sites could deadlock on.
///
/// `server::ops::team_agent::edit_agent` also holds its own per-company
/// roster write lock (`company_write_lock`) across the same span. It always
/// takes that lock first and this one second — the reverse never happens
/// anywhere in this codebase, so keep it that way rather than building a
/// second order those two locks could deadlock on.
static INDEX_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

/// Takes this company's inference-config lock, held until the returned guard
/// is dropped.
///
/// The inner `std` mutex is held only long enough to clone an `Arc` — never
/// across an `await` — so a panicking holder cannot poison anything a later
/// request needs.
pub async fn index_lock(company: &CompanyId) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut locks = INDEX_LOCKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks
            .entry(company.as_ref().to_string())
            .or_default()
            .clone()
    };
    lock.lock_owned().await
}

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
    /// crate, so this needs no new one — which
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

    /// This row's one model, read without guessing (keys rework, issue
    /// #2306, slice 2b). See [`model_on_row`].
    pub fn model(&self) -> ModelOnRow {
        model_on_row(&self.models)
    }
}

/// A provider row's single model, as read from its `models` map (keys
/// rework, issue #2306, slice 2b).
///
/// The map is a storage encoding (the same id under every tier key), not a
/// selection — `models` predates this rework and stays that shape so a
/// rollback binary still reads a model per tier. Two different ids under it
/// is a row nobody chose one model for, and it is reported, never resolved
/// by picking one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelOnRow {
    /// No non-blank value at all.
    None,
    /// Exactly one distinct non-blank id (trimmed).
    One(String),
    /// Two or more distinct ids, trimmed, sorted ascending, de-duplicated.
    Ambiguous(Vec<String>),
}

/// Collapses a tier-keyed `models` map to [`ModelOnRow`]. Entry zero uses the
/// same function, because its record carries `inference/config.models`
/// (`provider_from_runtime`, below).
pub fn model_on_row(models: &BTreeMap<String, String>) -> ModelOnRow {
    let mut distinct: Vec<String> = models
        .values()
        .map(|m| m.trim())
        .filter(|m| !m.is_empty())
        .collect::<std::collections::BTreeSet<&str>>()
        .into_iter()
        .map(str::to_string)
        .collect();
    match distinct.len() {
        0 => ModelOnRow::None,
        1 => ModelOnRow::One(distinct.remove(0)),
        _ => ModelOnRow::Ambiguous(distinct),
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

/// The longest model id accepted, counted in `char`s (not bytes) (keys
/// rework, issue #2306, slice 2c).
pub const MAX_MODEL_ID_CHARS: usize = 256;

/// The one validation every model-id write goes through: set-default, add,
/// edit (keys rework, issue #2306, slice 2c). Returns the trimmed id. Never a
/// network check: catalogues go stale, and an Azure deployment name is never
/// in `/models`.
pub fn check_model_id(raw: &str) -> Result<String> {
    let invalid = |m: String| OpenCompanyError::InvalidRequest(m);
    let id = raw.trim();
    if id.is_empty() {
        return Err(invalid(
            "Choose a model. A provider needs one model id.".into(),
        ));
    }
    if id.chars().any(char::is_control) {
        return Err(invalid(
            "A model id cannot contain control characters.".into(),
        ));
    }
    if id.chars().any(char::is_whitespace) {
        return Err(invalid("A model id cannot contain spaces.".into()));
    }
    if id.chars().count() > MAX_MODEL_ID_CHARS {
        return Err(invalid(format!(
            "A model id can be at most {MAX_MODEL_ID_CHARS} characters."
        )));
    }
    if crate::company::INFERENCE_TIERS.contains(&id) {
        return Err(invalid(format!(
            "`{id}` is a workload name, not a model. Choose a model id."
        )));
    }
    Ok(id.to_string())
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

/// Whether the legacy flat `inference/key` slot belongs to **managed**.
///
/// One address, two possible owners: entry zero's credential and managed's both
/// read through it. Which one it is depends on what entry zero's kind normalises
/// to — and a company whose original provider is a vendor account has its *BYOK*
/// key in there. The managed write path has always gated on this; the read paths
/// did not, so an upgraded BYOK company's vendor key was offered to the platform
/// URL as though it were a TinyHumans one.
///
/// `true` when there is no entry zero at all: the slot is then nobody else's,
/// and a company that predates the list and has only ever used managed is the
/// case the fallback exists for.
pub async fn legacy_slot_is_managed(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<bool> {
    Ok(list_providers(company, secrets)
        .await?
        .iter()
        .find(|p| p.origin == ProviderOrigin::EntryZero)
        .is_none_or(|zero| zero.slug == super::MANAGED_SLUG))
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
    // Kept, so the ordering is a rollback rather than a preference. Clearing
    // first is right — of the two half-states, "visible with its credential" is
    // the one an operator can see and act on — but it is only right if the
    // credential comes back when the index write fails. Without that, a DELETE
    // that reported failure had still thrown the key away irreversibly, and the
    // row it left behind could no longer answer.
    let previous = load_key(company, secrets, slug).await?;
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
    match save_index(company, secrets, &index).await {
        Ok(()) => Ok(true),
        Err(err) => {
            if !previous.trim().is_empty()
                && let Err(restore) = secrets
                    .set(company, &provider_key_key(slug), SecretValue(previous))
                    .await
            {
                tracing::error!(
                    company = %company.as_ref(),
                    provider = %slug,
                    error = %restore,
                    "a removal failed to write the provider index and then failed to put \
                     the credential back; this row is still listed and can no longer answer",
                );
            }
            Err(err)
        }
    }
}

/// This provider's credential at its own address, without the legacy fallback.
///
/// [`delete_provider`] needs the value it is about to clear so it can put it
/// back, and only that address is its to restore: `inference/key` may belong to
/// something else entirely, and clearing it is not what this function did.
async fn load_key(company: &CompanyId, secrets: &dyn SecretStore, slug: &str) -> Result<String> {
    Ok(match secrets.get(company, &provider_key_key(slug)).await? {
        Some(SecretValue(raw)) => raw,
        None => String::new(),
    })
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
    // Trimmed on the way out, because it is trimmed on the way in to decide
    // whether it is set at all: `provider_key_configured` calls `!raw.trim()
    // .is_empty()` a stored `"sk-…\n"` true, and this returning the newline
    // meant the value that answered "yes, configured" and the value put in an
    // `Authorization` header were not the same string. A pasted key keeps its
    // trailing newline far more often than anyone would like.
    if let Some(SecretValue(raw)) = secrets.get(company, &provider.key_key()).await?
        && !raw.trim().is_empty()
    {
        return Ok(raw.trim().to_string());
    }
    if let Some(legacy) = provider.legacy_key_key()
        && let Some(SecretValue(raw)) = secrets.get(company, legacy).await?
        && !raw.trim().is_empty()
    {
        return Ok(raw.trim().to_string());
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

/// A provider slug and the one model to send it (keys rework, issue #2306,
/// slice 2b): the only shape the new resolution path sends. Serialized field
/// order is `provider`, then `model`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelChoice {
    /// A slug from [`list_providers`], which includes entry zero.
    pub provider: String,
    /// The id that provider's API accepts. Non-empty after trim.
    pub model: String,
}

/// `inference/default`, parsed (keys rework, issue #2306, slice 2b).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultChoice {
    /// Missing, `""`, or only whitespace.
    Unset,
    /// A bare slug (every value stored before this rework), or JSON with a
    /// blank or absent `model`. Resolves exactly as a bare slug always has.
    ProviderOnly(String),
    /// JSON with a non-blank provider and a non-blank model.
    Full(ModelChoice),
}

impl DefaultChoice {
    /// The provider this default names, if any.
    pub fn provider(&self) -> Option<&str> {
        match self {
            Self::Unset => None,
            Self::ProviderOnly(slug) => Some(slug.as_str()),
            Self::Full(choice) => Some(choice.provider.as_str()),
        }
    }

    /// The full pair, only when both halves are present.
    pub fn full(&self) -> Option<&ModelChoice> {
        match self {
            Self::Full(choice) => Some(choice),
            _ => None,
        }
    }
}

/// The JSON read shape for [`parse_default`]. `model` is optional so a
/// provider-only JSON value is representable; unknown fields are ignored (no
/// `deny_unknown_fields`), so a future field added here never breaks an
/// older binary reading a value a newer one wrote.
#[derive(Deserialize)]
struct StoredDefault {
    provider: String,
    #[serde(default)]
    model: Option<String>,
}

/// The parse rules for `inference/default` (Q1), in order:
///
/// 1. Trim. Empty ⇒ [`DefaultChoice::Unset`].
/// 2. Does not start with `{` ⇒ the whole trimmed value is a slug ⇒
///    [`DefaultChoice::ProviderOnly`].
/// 3. Starts with `{` ⇒ deserialize as [`StoredDefault`]. Invalid JSON, a
///    missing `provider`, or a non-string `provider` ⇒ `OpenCompanyError::Store`.
/// 4. `provider` blank after trim ⇒ `OpenCompanyError::Store`.
/// 5. `model` absent, `null`, or blank after trim ⇒ `ProviderOnly(provider)`.
/// 6. Otherwise ⇒ `Full { provider: trimmed, model: trimmed }`.
pub fn parse_default(raw: &str) -> Result<DefaultChoice> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(DefaultChoice::Unset);
    }
    if !trimmed.starts_with('{') {
        return Ok(DefaultChoice::ProviderOnly(trimmed.to_string()));
    }
    let stored: StoredDefault = serde_json::from_str(trimmed).map_err(|e| {
        OpenCompanyError::Store(format!("inference default is not valid JSON: {e}"))
    })?;
    let provider = stored.provider.trim();
    if provider.is_empty() {
        return Err(OpenCompanyError::Store(
            "inference default names no provider".to_string(),
        ));
    }
    match stored
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        Some(model) => Ok(DefaultChoice::Full(ModelChoice {
            provider: provider.to_string(),
            model: model.to_string(),
        })),
        None => Ok(DefaultChoice::ProviderOnly(provider.to_string())),
    }
}

/// Reads and parses `inference/default` (keys rework, issue #2306, slice 2b).
/// Never writes: a bare slug stays a bare slug on disk (Q1) until an explicit
/// [`set_default_choice`] rewrites it.
pub async fn load_default(company: &CompanyId, secrets: &dyn SecretStore) -> Result<DefaultChoice> {
    let Some(SecretValue(raw)) = secrets.get(company, DEFAULT_PROVIDER_KEY).await? else {
        return Ok(DefaultChoice::Unset);
    };
    parse_default(&raw)
}

/// [`load_default`], but never fails (round-3a review P2-4).
///
/// A read path — a status response, or an in-use guard ahead of a delete,
/// disable or key clear — has to be able to answer even when
/// `inference/default` cannot be: a store read error or a hand-corrupted
/// value used to 500 every one of those, so the only repair left for an
/// operator was `POST …/default` from curl. Read as [`DefaultChoice::Unset`]
/// instead, with a `warn!` and `true` in the second half of the pair so the
/// caller can say so (`InferenceStatusDto::default_unreadable`) rather than
/// silently reporting "no default" as if the operator had never set one.
///
/// **Never writes.** An unreadable value is never overwritten by this call —
/// only an explicit [`set_default_choice`] ever rewrites the key — so once
/// the underlying corruption is fixed by hand, the next read recovers on its
/// own.
///
/// Turn-time resolution does not use this: a broken default there fails a
/// turn closed with a named sentence (`company::inference::copy`), which is
/// the opposite instinct from a read path degrading quietly.
pub async fn load_default_lenient(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> (DefaultChoice, bool) {
    match load_default(company, secrets).await {
        Ok(choice) => (choice, false),
        Err(err) => {
            tracing::warn!(
                company = %company,
                error = %err,
                "inference default could not be read; treating it as unset rather than \
                 failing the read",
            );
            (DefaultChoice::Unset, true)
        }
    }
}

/// Writes a full default as **one** JSON value, e.g.
/// `{"provider":"tinyhumans","model":"acme/test-model"}` (keys rework, issue
/// #2306, slice 2b). One write, so a provider can never be paired with
/// another provider's model even under a failed second write — there is no
/// second write.
pub async fn set_default_choice(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    choice: &ModelChoice,
) -> Result<()> {
    let provider = choice.provider.trim();
    let model = choice.model.trim();
    if provider.is_empty() || model.is_empty() {
        return Err(OpenCompanyError::InvalidRequest(
            "a default needs both a provider and a model".to_string(),
        ));
    }
    let raw = serde_json::to_string(&ModelChoice {
        provider: provider.to_string(),
        model: model.to_string(),
    })
    .map_err(|e| OpenCompanyError::Store(format!("serializing the inference default: {e}")))?;
    secrets
        .set(company, DEFAULT_PROVIDER_KEY, SecretValue(raw))
        .await
}

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
///
/// Keys rework (issue #2306), slice 2b: a JSON default answers its
/// `provider` too, so this stays a thin wrapper over [`load_default`] rather
/// than a second read path every caller would have to keep in sync with it.
pub async fn load_default_slug(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<Option<String>> {
    Ok(load_default(company, secrets)
        .await?
        .provider()
        .map(str::to_string))
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
#[path = "store_tests_support.rs"]
mod store_tests_support;
#[cfg(test)]
#[path = "store_tests_defaults.rs"]
mod tests_defaults;
#[cfg(test)]
#[path = "store_tests_locking.rs"]
mod tests_locking;
#[cfg(test)]
#[path = "store_tests_providers.rs"]
mod tests_providers;
#[cfg(test)]
#[path = "store_tests_routes_health.rs"]
mod tests_routes_health;
