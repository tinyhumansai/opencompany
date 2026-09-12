//! Where a company's connected search providers and their credentials live.
//!
//! # One credential slot per provider, and why that is the whole point
//!
//! Before this module there was one `search/api_key` for the company and a
//! separate `search/provider` field selecting which API it was presented to.
//! Switching provider without re-pasting the key left the old key
//! authenticating against the new provider, and every layer downstream — the
//! status route, the console badge, [`crate::harness::built_in::search_byo`] —
//! agreed the company was correctly configured until an agent's first search
//! came back 401. Keying the credential on the provider it authenticates makes
//! that unrepresentable.
//!
//! ```text
//!   SearchProvider record              SecretStore (per CompanyId)
//!   ┌────────────────────┐             ┌──────────────────────────────────┐
//!   │ slug               │             │ search/providers        index    │
//!   │ enabled            │ ──names──▶  │ search/provider/<slug>/key       │
//!   │ endpoint           │             │ search/provider/<slug>/endpoint  │
//!   │                    │             │ search/default          one slug │
//!   │   NO KEY FIELD     │             │ ──────────────────────────────── │
//!   └────────────────────┘             │ search/provider   entry zero     │
//!                                      │ search/api_key    entry zero     │
//!                                      │ search/endpoint   entry zero     │
//!                                      └──────────────────────────────────┘
//! ```
//!
//! # Convergence, not migration
//!
//! The [`SecretStore`](crate::ports::SecretStore) port has **no rename and no
//! delete** — clearing is a write of the empty string that reads back as unset —
//! and a flag-day migration on a store with no transaction can leave a company
//! with neither configuration. So the flat keys stay exactly where they are and
//! *are* the first entry in the list:
//!
//! - [`list_providers`] synthesises a record from `search/provider` when that
//!   slug has no record of its own, with its credential still at the flat
//!   address.
//! - A write for **that** slug moves the credential to
//!   `search/provider/<slug>/key` and clears the flat one in the same operation.
//!   The clear must be *issued* rather than inferred, because "cleared" and
//!   "never set" are the same state here, and a key left behind at the old
//!   address after the new one is written is an orphaned secret.
//! - A write for any **other** slug leaves the flat keys alone. Clearing them
//!   would destroy the entry-zero provider's credential, which is the opposite
//!   of what convergence is for.
//!
//! Nothing existing moves, nothing is orphaned, and there is no migration to
//! half-complete. The cost is one special case in the reader, kept until nothing
//! reads the flat keys.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::SecretStore;
use crate::ports::types::{CompanyId, SecretValue};

use super::{API_KEY_SECRET, ENDPOINT_SECRET, PROVIDER_SECRET, provider_is_byo};

/// One lock per company, held across the provider index's read-modify-write.
///
/// # Why a lock and not a compare-and-swap
///
/// Every index mutation here is read-modify-write: list what is connected,
/// change one row, write the whole list back. Two of them interleaving lose one
/// of the two edits, and the loss is not cosmetic — two concurrent connects can
/// each store their credential and leave only one row in the index, orphaning a
/// secret at an address nothing reads; a remove racing a toggle can resurrect
/// the removed row. This is reachable: the console deliberately keeps every
/// other row live while one request is in flight, and the routes are a plain
/// HTTP API besides.
///
/// A compare-and-swap would be better and is not available. [`SecretStore`] is
/// `get` and `set` and nothing else — no CAS, no delete, no transaction — and
/// widening that port is a change to every backend behind it rather than a fix
/// to this surface.
///
/// # What this does and does not cover
///
/// It serialises the mutations **within one process**, which is the whole of a
/// deployment: the manager runs one container per tenant and a company's
/// requests all land in it. It is not a distributed lock and must not be read
/// as one — if this workload is ever replicated per tenant, the index needs the
/// port-level primitive rather than this.
///
/// The registry is keyed by company id and grows by one entry per company ever
/// touched, which is bounded by the tenancy.
static INDEX_LOCKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
> = std::sync::LazyLock::new(std::sync::Mutex::default);

/// Takes this company's index lock, held until the returned guard is dropped.
///
/// The inner `std` mutex is held only long enough to clone an `Arc` — never
/// across an await — so a panicking writer cannot poison anything a later
/// request needs.
async fn index_guard(company: &CompanyId) -> tokio::sync::OwnedMutexGuard<()> {
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

/// Holds the JSON index of connected providers. Carries no credential.
pub const PROVIDER_INDEX_KEY: &str = "search/providers";

/// Holds the slug of the provider this company's agents search through.
///
/// A slot rather than a flag per record: two flags can both be true and then
/// need a tie-break rule, while a slot cannot, so there is no rule to write down
/// and no state to reconcile.
pub const DEFAULT_PROVIDER_KEY: &str = "search/default";

/// The credential slot for one provider.
pub fn provider_key_key(slug: &str) -> String {
    format!("search/provider/{slug}/key")
}

/// The instance-address slot for one provider. Not a secret.
pub fn provider_endpoint_key(slug: &str) -> String {
    format!("search/provider/{slug}/endpoint")
}

/// One search provider this company has connected.
///
/// Derives no `Serialize`: there is no credential on this struct and there must
/// never be one. The wire shape is a separate DTO carrying `keyConfigured`.
///
/// The index blob it round-trips through is [`IndexEntry`], which is a private
/// storage detail rather than this type, so that adding a field to the record
/// cannot accidentally add one to something serialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchProvider {
    /// The catalogue slug. Identity and address at once.
    pub slug: String,
    /// Whether this provider is eligible to be the one agents search through.
    /// Distinct from "not connected": a disabled provider keeps its credential.
    pub enabled: bool,
    /// The instance URL. `Some` only for a self-hosted provider.
    pub endpoint: Option<String>,
}

/// The stored shape of one index row. Deliberately not [`SearchProvider`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexEntry {
    slug: String,
    #[serde(default = "yes")]
    enabled: bool,
}

fn yes() -> bool {
    true
}

/// Reads a stored value, treating empty as absent — the port has no delete, so
/// a cleared value is the empty string.
async fn read(company: &CompanyId, secrets: &dyn SecretStore, key: &str) -> Result<Option<String>> {
    Ok(secrets
        .get(company, key)
        .await?
        .map(|value| value.expose().trim().to_string())
        .filter(|value| !value.is_empty()))
}

/// Writes a value, or clears it when `value` is empty.
async fn write(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    key: &str,
    value: &str,
) -> Result<()> {
    secrets
        .set(company, key, SecretValue(value.to_string()))
        .await
}

/// The slug the legacy flat keys describe, when it names a BYO provider.
///
/// `managed` and an unknown slug both answer `None`: neither is a connection,
/// and neither should be synthesised into a row.
async fn entry_zero_slug(company: &CompanyId, secrets: &dyn SecretStore) -> Result<Option<String>> {
    Ok(read(company, secrets, PROVIDER_SECRET)
        .await?
        .map(|slug| slug.to_ascii_lowercase())
        .filter(|slug| provider_is_byo(slug)))
}

/// Every provider this company has connected, entry zero first.
///
/// Entry zero is synthesised only when its slug has **no record of its own**, so
/// a company that converges by saving its legacy provider does not then see it
/// twice.
pub async fn list_providers(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<Vec<SearchProvider>> {
    let index: Vec<IndexEntry> = match read(company, secrets, PROVIDER_INDEX_KEY).await? {
        // A blob that will not parse is reported rather than silently treated as
        // an empty list: resolving to "no providers" would quietly move every
        // agent onto managed search and bill the platform for it.
        Some(raw) => serde_json::from_str(&raw).map_err(|err| {
            crate::error::OpenCompanyError::InvalidRequest(format!(
                "stored search provider index is not readable: {err}"
            ))
        })?,
        None => Vec::new(),
    };

    let mut providers = Vec::with_capacity(index.len() + 1);

    if let Some(slug) = entry_zero_slug(company, secrets).await?
        && !index.iter().any(|entry| entry.slug == slug)
    {
        providers.push(SearchProvider {
            endpoint: read(company, secrets, ENDPOINT_SECRET).await?,
            slug,
            enabled: true,
        });
    }

    for entry in index {
        let endpoint = read(company, secrets, &provider_endpoint_key(&entry.slug)).await?;
        providers.push(SearchProvider {
            slug: entry.slug,
            enabled: entry.enabled,
            endpoint,
        });
    }

    Ok(providers)
}

/// Writes the index, preserving the order it is given.
async fn save_index(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    providers: &[SearchProvider],
) -> Result<()> {
    let index: Vec<IndexEntry> = providers
        .iter()
        .map(|provider| IndexEntry {
            slug: provider.slug.clone(),
            enabled: provider.enabled,
        })
        .collect();
    let raw = serde_json::to_string(&index).map_err(|err| {
        crate::error::OpenCompanyError::InvalidRequest(format!(
            "search provider index could not be encoded: {err}"
        ))
    })?;
    write(company, secrets, PROVIDER_INDEX_KEY, &raw).await
}

/// Adds or replaces one provider record, converging entry zero if that is what
/// it is.
///
/// Does **not** touch credentials — [`store_provider_key`] owns those, so that a
/// record flush and a credential write are separately orderable. The connect
/// flow writes the credential first, because the probe resolves it by slug.
pub async fn put_provider(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    provider: SearchProvider,
) -> Result<()> {
    let _guard = index_guard(company).await;
    put_provider_locked(company, secrets, provider).await
}

/// Adds a provider **only if its slug is not already connected**, and says
/// which happened.
///
/// # Why the check cannot live in the caller
///
/// The connect flow read the index, decided the slug was free, and wrote it in
/// three separate awaits. Two admins connecting the same provider at once both
/// got past the read — and then the loser did real damage rather than merely
/// duplicating work: the connect flow rolls back on an `Auth` probe failure by
/// deleting the row **and** the credential, so a request whose key was rejected
/// deleted the row and the working key the other request had just stored, while
/// that request still answered `saved: true` from its own request-local copy.
///
/// Test-and-set under the one lock is the only version of this check that is
/// worth having, so the check moved in here rather than the lock moving out.
///
/// `false` means somebody else got there first and **nothing was written** —
/// which is what makes it safe to call before the credential is stored, so a
/// loser cannot overwrite the winner's key on its way to being refused.
pub async fn claim_provider(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    provider: SearchProvider,
) -> Result<bool> {
    let _guard = index_guard(company).await;
    if list_providers(company, secrets)
        .await?
        .iter()
        .any(|existing| existing.slug == provider.slug)
    {
        return Ok(false);
    }
    put_provider_locked(company, secrets, provider).await?;
    Ok(true)
}

/// Re-addresses a connected provider, or says it is not connected.
///
/// The read and the write are one critical section. Split, as they were in the
/// caller, a removal landing between them made the write **recreate** the row:
/// a provider the operator had just disconnected came back enabled, with its
/// old `enabled` flag and a fresh address, and started receiving agent searches
/// again after the removal had answered 200.
///
/// `false` means it was not connected — either it never was, or it stopped being
/// while this call was waiting for the lock. Those are the same answer from the
/// caller's side and it does not need to tell them apart.
pub async fn update_endpoint_if_present(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
    endpoint: Option<String>,
) -> Result<bool> {
    let _guard = index_guard(company).await;
    let Some(existing) = list_providers(company, secrets)
        .await?
        .into_iter()
        .find(|provider| provider.slug == slug)
    else {
        return Ok(false);
    };
    put_provider_locked(
        company,
        secrets,
        SearchProvider {
            slug: slug.to_string(),
            enabled: existing.enabled,
            endpoint,
        },
    )
    .await?;
    Ok(true)
}

/// Stores a credential **only for a provider that is connected**, atomically.
///
/// Same race as [`update_endpoint_if_present`], with a worse residue: a removal
/// landing between the caller's existence check and the write left a credential
/// at an address absent from the index, which the status route never reports
/// and `DELETE …/search/key` never clears, because both walk the index.
///
/// `false` means it is not connected, and nothing was written.
pub async fn store_key_if_connected(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
    key: &str,
) -> Result<bool> {
    let _guard = index_guard(company).await;
    if !list_providers(company, secrets)
        .await?
        .iter()
        .any(|provider| provider.slug == slug)
    {
        return Ok(false);
    }
    // Takes no lock of its own — it writes credential addresses, not the index
    // — so calling it while the guard is held is safe rather than re-entrant.
    store_provider_key(company, secrets, slug, key).await?;
    Ok(true)
}

/// [`put_provider`]'s body, for a caller that already holds the index lock.
///
/// Separate because the lock is not re-entrant: [`claim_provider`] holds it
/// across a read and a write, and calling the public wrapper from inside that
/// would deadlock rather than recurse.
async fn put_provider_locked(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    provider: SearchProvider,
) -> Result<()> {
    let mut providers: Vec<SearchProvider> = list_providers(company, secrets)
        .await?
        .into_iter()
        .filter(|existing| existing.slug != provider.slug)
        .collect();

    if let Some(endpoint) = provider.endpoint.as_deref() {
        write(
            company,
            secrets,
            &provider_endpoint_key(&provider.slug),
            endpoint,
        )
        .await?;
    }

    providers.push(provider);
    save_index(company, secrets, &providers).await
}

/// Turns one provider on or off, keeping the default marker honest.
///
/// Disabling the marked provider **clears the marker** rather than moving it to
/// something the operator never chose. Resolution then falls through to the
/// first enabled provider, which is what it did before anything was marked.
pub async fn set_enabled(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
    enabled: bool,
) -> Result<()> {
    let _guard = index_guard(company).await;
    let mut providers = list_providers(company, secrets).await?;
    let Some(target) = providers.iter_mut().find(|p| p.slug == slug) else {
        return Ok(());
    };
    target.enabled = enabled;
    save_index(company, secrets, &providers).await?;

    if !enabled && load_default_slug(company, secrets).await?.as_deref() == Some(slug) {
        clear_default_slug(company, secrets).await?;
    }
    Ok(())
}

/// Removes a provider, clearing its credential in the same operation.
///
/// The borrowed design leaves the credential behind and silently reuses it when
/// the provider is re-added. Here the clear is issued, and a failed clear is
/// logged loudly: an orphaned secret on disk is the shape of an incident rather
/// than a tidiness problem.
pub async fn delete_provider(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
) -> Result<()> {
    let _guard = index_guard(company).await;
    let providers: Vec<SearchProvider> = list_providers(company, secrets)
        .await?
        .into_iter()
        .filter(|provider| provider.slug != slug)
        .collect();
    save_index(company, secrets, &providers).await?;

    for key in [provider_key_key(slug), provider_endpoint_key(slug)] {
        if let Err(err) = write(company, secrets, &key, "").await {
            tracing::error!(
                company = %company,
                key = %key,
                "[search] removing a provider could not clear its credential; a secret is now \
                 orphaned at this address: {err}"
            );
            return Err(err);
        }
    }

    // Entry zero lives at the flat keys, so removing it has to clear those too —
    // which is exactly what `DELETE …/search/key` has always done.
    if entry_zero_slug(company, secrets).await?.as_deref() == Some(slug) {
        for key in [API_KEY_SECRET, PROVIDER_SECRET, ENDPOINT_SECRET] {
            if let Err(err) = write(company, secrets, key, "").await {
                tracing::error!(
                    company = %company,
                    key = %key,
                    "[search] removing the legacy provider could not clear its flat credential; a \
                     secret is now orphaned at this address: {err}"
                );
                return Err(err);
            }
        }
    }

    if load_default_slug(company, secrets).await?.as_deref() == Some(slug) {
        clear_default_slug(company, secrets).await?;
    }
    Ok(())
}

/// Stores one provider's credential at its own address, converging entry zero.
pub async fn store_provider_key(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
    key: &str,
) -> Result<()> {
    write(company, secrets, &provider_key_key(slug), key).await?;

    // Only for the slug the flat keys describe. Clearing `search/api_key` while
    // writing some *other* provider's key would destroy the entry-zero
    // provider's credential.
    if entry_zero_slug(company, secrets).await?.as_deref() == Some(slug)
        && let Err(err) = write(company, secrets, API_KEY_SECRET, "").await
    {
        tracing::error!(
            company = %company,
            "[search] the credential moved to its per-provider address but the legacy \
             `search/api_key` could not be cleared; a secret is now orphaned there: {err}"
        );
        return Err(err);
    }
    Ok(())
}

/// One provider's credential, trying its own address and falling back to the
/// legacy flat one for entry zero.
pub async fn load_provider_key(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
) -> Result<Option<String>> {
    if let Some(key) = read(company, secrets, &provider_key_key(slug)).await? {
        return Ok(Some(key));
    }
    if entry_zero_slug(company, secrets).await?.as_deref() == Some(slug) {
        return read(company, secrets, API_KEY_SECRET).await;
    }
    Ok(None)
}

/// Whether a credential is stored for `slug`. Never the credential.
///
/// Derived by asking the store, never by storing a flag, which can go stale
/// against a cleared secret.
pub async fn provider_key_configured(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
) -> Result<bool> {
    Ok(load_provider_key(company, secrets, slug).await?.is_some())
}

/// The slug the operator marked, if any.
pub async fn load_default_slug(
    company: &CompanyId,
    secrets: &dyn SecretStore,
) -> Result<Option<String>> {
    read(company, secrets, DEFAULT_PROVIDER_KEY).await
}

/// Marks one provider as the one agents search through.
pub async fn set_default_slug(
    company: &CompanyId,
    secrets: &dyn SecretStore,
    slug: &str,
) -> Result<()> {
    write(company, secrets, DEFAULT_PROVIDER_KEY, slug).await
}

/// Unmarks whatever was marked. Resolution falls back to the first enabled.
pub async fn clear_default_slug(company: &CompanyId, secrets: &dyn SecretStore) -> Result<()> {
    write(company, secrets, DEFAULT_PROVIDER_KEY, "").await
}

#[cfg(test)]
#[path = "store_test.rs"]
mod store_test;
