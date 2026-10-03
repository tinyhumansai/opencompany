//! Engine-to-engine migration: `list` → `store`, page by page.
//!
//! Copies every record this host wrote (every item under the `oc` folder root,
//! whichever company it belongs to) from one engine to another, both built
//! through [`open_driver`](super::driver::open_driver). Items cross with their
//! metadata untouched — workspace, folder, key, tags and provenance — so every
//! company's partitions move together and nothing is re-stamped.
//!
//! Records other products wrote into the same engine are not selected: the
//! filter is this host's own folder root, not the whole store.
//!
//! This is the data half of the engine-switch runbook in
//! `docs/spec/runtime/memory-engine.md`: migrate, then flip the
//! `OPENCOMPANY_MEMORY*` variables and restart.
//!
//! # Failure is a stop, never a guess
//!
//! This never retries. It stops at the first failed write, reporting the
//! cursor that *started* that page; `--resume-cursor` re-enters there safely
//! because `store` is idempotent by content fingerprint — re-storing an item the
//! target already holds reports `replayed` and writes nothing.

use std::sync::Arc;

use tinymemory::{DocumentBody, Hit, ItemKind, ListRequest, MemoryEngine, MetaFilter, StoreItem};

use super::driver::{MemoryDriverConfig, MemoryMode};
use super::facades::ENVELOPE_MIME;
use super::namespace::ROOT;
use crate::Result;
use crate::error::OpenCompanyError;

/// What a finished (or stopped) migration did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrateSummary {
    /// Pages pulled from the source.
    pub pages: u64,
    /// Records the source listed.
    pub exported: u64,
    /// Records the target wrote.
    pub imported: u64,
    /// Records the target already held (a resumed run): `store` reported the
    /// item replayed and wrote nothing.
    pub skipped: u64,
}

/// A migration that stopped partway: what happened, and where to resume.
#[derive(Debug)]
pub struct MigrateStopped {
    /// What had already moved before the stop.
    pub summary: MigrateSummary,
    /// The cursor that started the failed page — pass to `--resume-cursor`.
    /// `None` means the very first page failed.
    pub resume_cursor: Option<String>,
    /// The target engine's own reasons.
    pub errors: Vec<String>,
}

/// The outcome: complete, or stopped with a resume point.
pub type MigrateOutcome = std::result::Result<MigrateSummary, Box<MigrateStopped>>;

/// The filter selecting every record this host wrote, and nothing else.
fn host_records() -> MetaFilter {
    MetaFilter {
        folder: Some(ROOT.to_string()),
        kinds: vec![ItemKind::Document],
        ..MetaFilter::default()
    }
}

/// Rebuilds the item a listed hit came from.
///
/// Every host record is a `Document` with no title and the envelope MIME (see
/// `facades::bound`), so the rebuild is exact — and an exact rebuild has the
/// same fingerprint, which is what makes a resumed run idempotent.
fn item_of(hit: Hit) -> StoreItem {
    StoreItem::Document {
        title: None,
        body: DocumentBody::Text(hit.text),
        mime: Some(ENVELOPE_MIME.to_string()),
        meta: hit.meta,
    }
}

/// Copies every host record `from` holds into `to`, `page_size` records at a
/// time, starting at `resume` (a cursor a previous stopped run printed, or
/// `None` for the beginning). `on_page` observes progress after each page.
///
/// # Errors
///
/// A source `list` failure surfaces as a crate error (nothing was
/// half-applied). A target `store` failure returns [`MigrateStopped`] with the
/// resume cursor, because the *next* attempt must re-enter at the failed page,
/// not at the beginning.
pub async fn migrate(
    from: &Arc<dyn MemoryEngine>,
    to: &Arc<dyn MemoryEngine>,
    page_size: usize,
    resume: Option<String>,
    mut on_page: impl FnMut(&MigrateSummary),
) -> Result<MigrateOutcome> {
    let mut summary = MigrateSummary::default();
    let mut cursor = resume;
    loop {
        // The cursor that names THIS page, kept for the stop report.
        let page_start = cursor.clone();
        let mut request = ListRequest::new(host_records(), page_size.max(1));
        request.cursor = page_start.clone();
        let page = from.list(request).await.map_err(|e| {
            OpenCompanyError::Store(format!(
                "source engine `{}` failed to list a page: {e}",
                from.descriptor().id
            ))
        })?;
        summary.pages += 1;
        summary.exported += page.items.len() as u64;

        // A cursor that comes back unchanged would loop this function forever,
        // re-copying the same page. Checked before the writes so the echoed
        // page is not even re-written once.
        if page.next_cursor.is_some() && page.next_cursor == page_start {
            return Ok(Err(Box::new(MigrateStopped {
                summary,
                resume_cursor: page_start,
                errors: vec![format!(
                    "source engine `{}` returned a cursor that did not advance; refusing to loop",
                    from.descriptor().id
                )],
            })));
        }

        for hit in page.items {
            match to.store(item_of(hit)).await {
                Ok(receipt) if receipt.replayed => summary.skipped += 1,
                Ok(_) => summary.imported += 1,
                Err(e) => {
                    return Ok(Err(Box::new(MigrateStopped {
                        summary,
                        resume_cursor: page_start,
                        errors: vec![format!(
                            "target engine `{}` failed to store: {e}",
                            to.descriptor().id
                        )],
                    })));
                }
            }
        }
        on_page(&summary);

        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(Ok(summary))
}

/// Counts every host record `engine` holds, paging with the same
/// non-advancing-cursor guard [`migrate`] uses — the dry-run counter and the
/// post-migration receipt both call this, so there is exactly one pager to get
/// the echo case right in.
///
/// `start` counts from a resume cursor; `None` counts everything.
pub async fn count_records(
    engine: &Arc<dyn MemoryEngine>,
    start: Option<String>,
    page_size: usize,
) -> Result<u64> {
    let mut total: u64 = 0;
    let mut cursor = start;
    loop {
        let page_start = cursor.clone();
        let mut request = ListRequest::new(host_records(), page_size.max(1));
        request.cursor = page_start.clone();
        let page = engine.list(request).await.map_err(|e| {
            OpenCompanyError::Store(format!(
                "engine `{}` failed to list a page while counting: {e}",
                engine.descriptor().id
            ))
        })?;
        total += page.items.len() as u64;
        if page.next_cursor.is_some() && page.next_cursor == page_start {
            return Err(OpenCompanyError::Store(format!(
                "engine `{}` returned a cursor that did not advance while counting; refusing to \
                 loop (engine bug, or a stale --resume-cursor this engine clamps)",
                engine.descriptor().id
            )));
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(total),
        }
    }
}

/// Resolves the `memory migrate` source and target configurations, with every
/// refusal the command makes — moved out of the bin so the guards execute
/// under the same feature lanes that run this module's tests, instead of
/// living in a binary no CI lane ever `cargo test`s (the review finding on
/// the first cut: all eight refusals were mutation-proof-untested).
///
/// `to_api_key` should already carry the `OPENCOMPANY_MEMORY_TARGET_API_KEY`
/// fallback (resolved by the caller, where the environment belongs).
pub fn resolve_migrate_configs(
    settings: &crate::store::StorageSettings,
    to: &str,
    to_url: Option<String>,
    to_api_key: Option<String>,
) -> Result<(MemoryDriverConfig, MemoryDriverConfig)> {
    use crate::store::MemoryBackend;

    // Shared-single-DB tenant mode namespaces company ids at the app layer;
    // records cross with their workspaces verbatim, so a
    // migration would move every tenant the source credential can see, into a
    // target that knows nothing of the `<tenant>--` scheme. The sibling
    // bundle commands refuse this; so does this one.
    if settings
        .tenant_id
        .as_deref()
        .is_some_and(|t| !t.trim().is_empty())
    {
        return Err(OpenCompanyError::Config(
            "shared-single-DB tenant mode (OPENCOMPANY_TENANT_ID) is active: a migration would \
             move every namespace the source credential can see, across tenants. Run migrations \
             from the manager path, per tenant, without tenant mode."
                .into(),
        ));
    }

    let from_mode = match settings.memory_backend {
        MemoryBackend::Store => {
            return Err(OpenCompanyError::Config(
                "OPENCOMPANY_MEMORY=store: the base backend serves memory directly and has no \
                 provider seam to export through. Use `opencompany export` for a bundle instead."
                    .into(),
            ));
        }
        MemoryBackend::Null => {
            return Err(OpenCompanyError::Config(
                "OPENCOMPANY_MEMORY=null retains nothing; there is nothing to migrate.".into(),
            ));
        }
        MemoryBackend::Remote => MemoryMode::Remote,
    };
    let from_config = MemoryDriverConfig {
        mode: from_mode,
        driver_id: settings.memory_driver.clone(),
        url: settings.memory_url.clone(),
        api_key: settings.memory_api_key.clone(),
        data_dir: settings.data_dir.clone(),
    };

    let to_config = match to {
        "null" => {
            return Err(OpenCompanyError::Config(
                "--to null would discard every record; that is `null`'s contract, not a \
                 migration."
                    .into(),
            ));
        }
        requested => {
            let to = super::driver::canonical_engine_id(requested)?;
            let needs_endpoint = tinymemory::list_engines()
                .into_iter()
                .any(|engine| engine.id == to && engine.needs_endpoint);
            if needs_endpoint
                && to_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|u| !u.is_empty())
                    .is_none()
            {
                return Err(OpenCompanyError::Config(format!(
                    "--to {to} is a hosted engine and needs --to-url (the target's endpoint). \
                     Do NOT set OPENCOMPANY_MEMORY_URL for this — that variable configures the \
                     SOURCE, and changing it repoints what you are migrating from."
                )));
            }
            MemoryDriverConfig {
                mode: MemoryMode::Remote,
                driver_id: Some(to.to_string()),
                url: to_url,
                api_key: to_api_key,
                data_dir: None,
            }
        }
    };

    // Same engine, same target = a copy onto itself: the listing cursor
    // shifts under the concurrent writes and records skip or repeat. Compared
    // mode-aware and normalized, so trailing slashes and whitespace cannot
    // evade the check.
    let norm_id = |id: &Option<String>| id.as_deref().map(str::trim).map(str::to_owned);
    let norm_url = |url: &Option<String>| {
        url.as_deref()
            .map(str::trim)
            .map(|u| u.trim_end_matches('/').to_owned())
    };
    let same_engine = from_config.mode == to_config.mode
        && norm_id(&from_config.driver_id) == norm_id(&to_config.driver_id)
        && match to_config.mode {
            MemoryMode::Remote => norm_url(&from_config.url) == norm_url(&to_config.url),
            MemoryMode::Null => true,
        };
    if same_engine {
        return Err(OpenCompanyError::Config(
            "the source and the target are the same engine at the same location; nothing to do."
                .into(),
        ));
    }
    Ok((from_config, to_config))
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
