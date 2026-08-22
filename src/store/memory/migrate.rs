//! Engine-to-engine migration over the contract's Portability family.
//!
//! `export_page` → `import_records`, page by page, between two providers this
//! host constructed through [`open_driver`](super::driver::open_driver). The
//! records cross in the exporting driver's own vocabulary — kind, namespace
//! and taint round-trip untouched (`ExportRecord`'s own contract), so every
//! company's namespaces move together and provenance is never re-stamped.
//!
//! This is the data half of the engine-switch runbook in
//! `docs/spec/runtime/memory-engine.md`: migrate, then flip the
//! `OPENCOMPANY_MEMORY*` variables and restart.
//!
//! # Failure is a stop, never a guess
//!
//! The contract's §A4 taxonomy (13 wire-named `MemoryError` variants since
//! tinymemory v1.1.0) means a transient `Timeout`/`Unavailable`/`Unreachable`
//! is now distinguishable from a real rejection — but this code still never
//! retries, and that decision does not rest on the taxonomy: an import
//! retried into a driver that half-applied the page could double-write, and
//! no error class proves how much of a page landed. It stops at the first
//! failed page, reporting the cursor that *started* that page. `--resume-cursor` re-enters there safely because import is
//! idempotent by `(namespace, key)`: a driver that recognises a present
//! record reports it `skipped`, and one that does not simply overwrites in
//! place — either way, re-running the failed page cannot duplicate.

use std::sync::Arc;

use tinymemory_api::provider::MemoryProvider;

use super::driver::{MemoryDriverConfig, MemoryMode};
use crate::Result;
use crate::error::OpenCompanyError;

/// What a finished (or stopped) migration did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrateSummary {
    /// Pages pulled from the source.
    pub pages: u64,
    /// Records the source exported.
    pub exported: u64,
    /// Records the target wrote.
    pub imported: u64,
    /// Records the target recognised as already present (a resumed run, on a
    /// driver that detects presence — upsert-style drivers report a re-import
    /// under `imported` instead, having overwritten in place).
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
    /// The target driver's own reasons, bounded by it.
    pub errors: Vec<String>,
}

/// The outcome: complete, or stopped with a resume point.
pub type MigrateOutcome = std::result::Result<MigrateSummary, Box<MigrateStopped>>;

/// Copies every record `from` exports into `to`, `page_size` records at a time,
/// starting at `resume` (a cursor a previous stopped run printed, or `None`
/// for the beginning). `on_page` observes progress after each imported page.
///
/// # Errors
///
/// A source `export_page` failure surfaces as a crate error (nothing was
/// half-applied). A target `import_records` failure — or a page the target
/// reports `failed > 0` for — returns [`MigrateStopped`] with the resume
/// cursor, because the *next* attempt must re-enter at the failed page, not at
/// the beginning.
pub async fn migrate(
    from: &Arc<dyn MemoryProvider>,
    to: &Arc<dyn MemoryProvider>,
    page_size: usize,
    resume: Option<String>,
    mut on_page: impl FnMut(&MigrateSummary),
) -> Result<MigrateOutcome> {
    let mut summary = MigrateSummary::default();
    let mut cursor = resume;
    loop {
        // The cursor that names THIS page, kept for the stop report.
        let page_start = cursor.clone();
        let page = from
            .export_page(page_start.as_deref(), page_size)
            .await
            .map_err(|e| {
                OpenCompanyError::Store(format!(
                    "source engine `{}` failed to export a page: {e}",
                    from.driver_id()
                ))
            })?;
        summary.pages += 1;
        summary.exported += page.records.len() as u64;

        // A cursor that comes back unchanged would loop this function forever,
        // re-importing the same page (a driver bug, or a paginated API that
        // clamps an unknown cursor to the first page). Checked before the
        // import so the echoed page is not even re-written once.
        if page.next_cursor.is_some() && page.next_cursor == page_start {
            return Ok(Err(Box::new(MigrateStopped {
                summary,
                resume_cursor: page_start,
                errors: vec![format!(
                    "source engine `{}` returned a cursor that did not advance; refusing to loop",
                    from.driver_id()
                )],
            })));
        }

        if !page.records.is_empty() {
            let page_len = page.records.len() as u64;
            let outcome = match to.import_records(page.records).await {
                Ok(outcome) => outcome,
                Err(e) => {
                    return Ok(Err(Box::new(MigrateStopped {
                        summary,
                        resume_cursor: page_start,
                        errors: vec![format!(
                            "target engine `{}` failed to import: {e}",
                            to.driver_id()
                        )],
                    })));
                }
            };
            summary.imported += u64::from(outcome.imported);
            summary.skipped += u64::from(outcome.skipped);
            // A target that under-reports — imported+skipped+failed short of
            // the page it was handed — has dropped records while saying
            // nothing, the exact worst outcome this command exists to
            // prevent. Stop and say so; the operator resumes at this page.
            let accounted = u64::from(outcome.imported)
                + u64::from(outcome.skipped)
                + u64::from(outcome.failed);
            if accounted < page_len {
                return Ok(Err(Box::new(MigrateStopped {
                    summary,
                    resume_cursor: page_start,
                    errors: vec![format!(
                        "target engine `{}` accounted for {accounted} of {page_len} records in \
                         the page (imported {} + skipped {} + failed {}); refusing to continue \
                         past silently dropped records",
                        to.driver_id(),
                        outcome.imported,
                        outcome.skipped,
                        outcome.failed
                    )],
                })));
            }
            if outcome.failed > 0 {
                return Ok(Err(Box::new(MigrateStopped {
                    summary,
                    resume_cursor: page_start,
                    errors: outcome.errors,
                })));
            }
        }
        on_page(&summary);

        // `next_cursor: None` is the terminator — NOT an empty page, which is
        // legal mid-export (the contract says so explicitly).
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(Ok(summary))
}

/// Counts every record `provider` exports, paging with the same
/// non-advancing-cursor guard [`migrate`] uses — the dry-run counter and the
/// post-migration receipt both call this, so there is exactly one pager to
/// get the echo case right in (a hand-rolled twin without the guard is how a
/// stale `--resume-cursor` turns `--dry-run` into an infinite loop).
///
/// `start` counts from a resume cursor; `None` counts the whole store.
pub async fn count_records(
    provider: &Arc<dyn MemoryProvider>,
    start: Option<String>,
    page_size: usize,
) -> Result<u64> {
    let mut total: u64 = 0;
    let mut cursor = start;
    loop {
        let page_start = cursor.clone();
        let page = provider
            .export_page(page_start.as_deref(), page_size)
            .await
            .map_err(|e| {
                OpenCompanyError::Store(format!(
                    "engine `{}` failed to export a page while counting: {e}",
                    provider.driver_id()
                ))
            })?;
        total += page.records.len() as u64;
        if page.next_cursor.is_some() && page.next_cursor == page_start {
            return Err(OpenCompanyError::Store(format!(
                "engine `{}` returned a cursor that did not advance while counting; refusing to \
                 loop (driver bug, or a stale --resume-cursor this engine clamps)",
                provider.driver_id()
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
    to_data_dir: Option<std::path::PathBuf>,
) -> Result<(MemoryDriverConfig, MemoryDriverConfig)> {
    use crate::store::{MemoryBackend, StorageKind};

    // Shared-single-DB tenant mode namespaces company ids at the app layer;
    // the Portability records cross with their namespaces verbatim, so a
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
        MemoryBackend::Tinycortex
            if settings
                .memory_driver
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .is_none() =>
        {
            return Err(OpenCompanyError::Config(
                "OPENCOMPANY_MEMORY=embedded without OPENCOMPANY_MEMORY_DRIVER is the \
                 EngineCortex overlay, which is driven directly rather than through the \
                 provider seam — its data cannot migrate through this command. Use \
                 `opencompany export` for a bundle of what it holds."
                    .into(),
            ));
        }
        MemoryBackend::Tinycortex => MemoryMode::Embedded,
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
        "namespace" => {
            // The same durability refusal `open_provider` enforces at boot:
            // on a mongodb base, /data is ephemeral scratch, and a migration
            // that "succeeds" into it is data loss with a success message.
            if settings.kind == StorageKind::Mongodb && !settings.allow_ephemeral_memory {
                return Err(OpenCompanyError::Config(
                    "OPENCOMPANY_STORAGE=mongodb treats the data dir as ephemeral scratch, and \
                     the boot path refuses the namespace driver there for exactly that reason. \
                     Migrating into it would report success on data the next container \
                     replacement deletes. If the data dir is genuinely a durable volume, assert \
                     it with OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL=1."
                        .into(),
                ));
            }
            let data_dir = to_data_dir.clone().or_else(|| settings.data_dir.clone());
            // Validated HERE, in the target's own vocabulary. Letting
            // `open_driver` refuse later produces "OPENCOMPANY_MEMORY_URL is
            // required"-style messages that name the SOURCE env — advice
            // which, followed, silently repoints the source at a different
            // (likely empty) engine and reports `0 exported` as success.
            if data_dir.is_none() {
                return Err(OpenCompanyError::Config(
                    "--to namespace needs a directory for the target store: pass --to-data-dir \
                     (OPENCOMPANY_DATA_DIR also serves as its default when set)."
                        .into(),
                ));
            }
            MemoryDriverConfig {
                mode: MemoryMode::Embedded,
                driver_id: Some(to.to_string()),
                url: None,
                api_key: None,
                data_dir,
            }
        }
        "supermemory" | "mem0" | "cognee" => {
            if to_url
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
        "null" => {
            return Err(OpenCompanyError::Config(
                "--to null would discard every record; that is `null`'s contract, not a \
                 migration."
                    .into(),
            ));
        }
        other => {
            return Err(OpenCompanyError::Config(format!(
                "--to {other} names no migratable driver: namespace, supermemory, mem0, cognee."
            )));
        }
    };

    // Same engine, same target = a copy onto itself: on the embedded store it
    // is one SQLite file open twice; on a hosted store the export cursor
    // shifts under the concurrent writes and records skip or repeat. Compared
    // mode-aware and normalized — the naive field-equality version could
    // never fire remote-to-remote (the source carries the env data_dir, the
    // target None) — and dirs are canonicalised when they resolve, so
    // `--to-data-dir ../data` from `/var` cannot evade a source at
    // `/var/data` by spelling.
    let norm_id = |id: &Option<String>| id.as_deref().map(str::trim).map(str::to_owned);
    let norm_url = |url: &Option<String>| {
        url.as_deref()
            .map(str::trim)
            .map(|u| u.trim_end_matches('/').to_owned())
    };
    let norm_dir = |dir: &Option<std::path::PathBuf>| {
        dir.as_ref().map(|d| {
            // Canonical when the path exists (resolves `..`, symlinks and the
            // CWD); the component-normalized spelling when it does not (a
            // not-yet-created target dir cannot canonicalise, and refusing a
            // fresh dir over that would block every first migration).
            std::fs::canonicalize(d)
                .unwrap_or_else(|_| d.components().collect::<std::path::PathBuf>())
        })
    };
    let same_engine = from_config.mode == to_config.mode
        && norm_id(&from_config.driver_id) == norm_id(&to_config.driver_id)
        && match to_config.mode {
            MemoryMode::Remote => norm_url(&from_config.url) == norm_url(&to_config.url),
            MemoryMode::Embedded => {
                norm_dir(&from_config.data_dir) == norm_dir(&to_config.data_dir)
            }
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
mod test {
    use super::*;
    use tinymemory_api::types::{MemoryCategory, MemoryTaint};
    use tinymemory_conformance::InMemoryProvider;

    fn provider() -> Arc<dyn MemoryProvider> {
        Arc::new(InMemoryProvider::default())
    }

    async fn seed(p: &Arc<dyn MemoryProvider>, n: usize) {
        for i in 0..n {
            p.store(
                &format!("oc/team-{}", i % 3),
                &format!("key-{i}"),
                &format!("record {i}"),
                MemoryCategory::Core,
                None,
                MemoryTaint::Internal,
            )
            .await
            .expect("seed");
        }
    }

    async fn count(p: &Arc<dyn MemoryProvider>) -> u64 {
        let mut total = 0;
        let mut cursor: Option<String> = None;
        loop {
            let page = p.export_page(cursor.as_deref(), 10).await.expect("page");
            total += page.records.len() as u64;
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return total,
            }
        }
    }

    /// The whole point: every record crosses, across page boundaries, and the
    /// target's own export agrees with the source's count.
    #[tokio::test]
    async fn every_record_crosses_between_engines() {
        let (from, to) = (provider(), provider());
        seed(&from, 23).await;
        let outcome = migrate(&from, &to, 7, None, |_| {})
            .await
            .expect("no source failure")
            .expect("no stop");
        assert_eq!(outcome.exported, 23);
        assert_eq!(outcome.imported, 23);
        assert_eq!(outcome.skipped, 0);
        assert!(outcome.pages >= 4, "23 records at page size 7 is 4 pages");
        assert_eq!(
            count(&to).await,
            23,
            "the target must hold what the source held"
        );
    }

    /// A full re-run cannot duplicate: import is idempotent by
    /// `(namespace, key)`. The reference driver overwrites in place (so the
    /// re-run reports `imported` again, `skipped` stays 0 — presence
    /// detection is optional in the contract); what must hold on every
    /// driver is the count: the target ends with exactly the source's
    /// records, however the outcome buckets them.
    #[tokio::test]
    async fn a_rerun_cannot_duplicate() {
        let (from, to) = (provider(), provider());
        seed(&from, 5).await;
        let first = migrate(&from, &to, 2, None, |_| {})
            .await
            .expect("ok")
            .expect("complete");
        assert_eq!(first.imported, 5);
        let second = migrate(&from, &to, 2, None, |_| {})
            .await
            .expect("ok")
            .expect("complete");
        assert_eq!(
            second.imported + second.skipped,
            5,
            "every record accounted for, in whichever bucket the driver uses"
        );
        assert_eq!(
            count(&to).await,
            5,
            "no duplicates, however it was bucketed"
        );
    }

    /// An empty source completes with zero of everything rather than erroring —
    /// migrating before first use is legal, if pointless.
    #[tokio::test]
    async fn an_empty_source_completes_empty() {
        let (from, to) = (provider(), provider());
        let outcome = migrate(&from, &to, 10, None, |_| {})
            .await
            .expect("ok")
            .expect("complete");
        assert_eq!(outcome.exported, 0);
        assert_eq!(count(&to).await, 0);
    }
    // ── The failure doubles: one wrapper, behaviour injected per test ──────

    /// Wraps [`InMemoryProvider`] and misbehaves on demand: fail or
    /// under-report an import, echo an export cursor. Everything else
    /// delegates, so the data is real and only the defect under test is fake.
    struct FlakyProvider {
        inner: InMemoryProvider,
        /// Fail `import_records` outright on its Nth call (1-based).
        fail_import_on_call: Option<u32>,
        /// Report `failed: 1` (with a reason) on the Nth import call.
        reject_on_call: Option<u32>,
        /// Report an all-zero outcome for every import (the under-reporter).
        under_report: bool,
        /// Echo the request cursor back as `next_cursor` (the clamping pager).
        echo_cursor: bool,
        import_calls: std::sync::atomic::AtomicU32,
    }

    impl FlakyProvider {
        fn new() -> Self {
            Self {
                inner: InMemoryProvider::default(),
                fail_import_on_call: None,
                reject_on_call: None,
                under_report: false,
                echo_cursor: false,
                import_calls: std::sync::atomic::AtomicU32::new(0),
            }
        }
    }

    use tinymemory_api::error::MemoryError;
    use tinymemory_api::provider::types::SourceScope;
    use tinymemory_api::provider::types::{ExportPage, ImportOutcome};
    use tinymemory_api::provider::{ExportRecord, MemoryCore, MemoryPortability, MemoryRecall};
    use tinymemory_api::recall::OwnedRecallOpts;
    use tinymemory_api::types::{MemoryEntry, NamespaceSummary};

    #[async_trait::async_trait]
    impl MemoryCore for FlakyProvider {
        async fn store(
            &self,
            namespace: &str,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
            taint: MemoryTaint,
        ) -> std::result::Result<(), MemoryError> {
            self.inner
                .store(namespace, key, content, category, session_id, taint)
                .await
        }
        async fn get(
            &self,
            namespace: &str,
            key: &str,
        ) -> std::result::Result<Option<MemoryEntry>, MemoryError> {
            self.inner.get(namespace, key).await
        }
        async fn forget(
            &self,
            namespace: &str,
            key: &str,
        ) -> std::result::Result<bool, MemoryError> {
            self.inner.forget(namespace, key).await
        }
        async fn list(
            &self,
            namespace: Option<&str>,
            category: Option<&MemoryCategory>,
            session_id: Option<&str>,
        ) -> std::result::Result<Vec<MemoryEntry>, MemoryError> {
            self.inner.list(namespace, category, session_id).await
        }
        async fn namespaces(&self) -> std::result::Result<Vec<NamespaceSummary>, MemoryError> {
            self.inner.namespaces().await
        }
    }

    #[async_trait::async_trait]
    impl MemoryRecall for FlakyProvider {
        async fn recall(
            &self,
            query: &str,
            limit: usize,
            opts: &OwnedRecallOpts,
            scope: Option<&SourceScope>,
        ) -> std::result::Result<Vec<MemoryEntry>, MemoryError> {
            self.inner.recall(query, limit, opts, scope).await
        }
    }

    #[async_trait::async_trait]
    impl MemoryPortability for FlakyProvider {
        async fn export_page(
            &self,
            cursor: Option<&str>,
            limit: usize,
        ) -> std::result::Result<ExportPage, MemoryError> {
            let mut page = self.inner.export_page(cursor, limit).await?;
            if self.echo_cursor {
                page.next_cursor = cursor.map(str::to_owned).or(page.next_cursor);
                if cursor.is_some() {
                    page.next_cursor = cursor.map(str::to_owned);
                }
            }
            Ok(page)
        }
        async fn import_records(
            &self,
            records: Vec<ExportRecord>,
        ) -> std::result::Result<ImportOutcome, MemoryError> {
            let call = self
                .import_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if self.fail_import_on_call == Some(call) {
                return Err(MemoryError::Other(anyhow::anyhow!(
                    "injected import failure"
                )));
            }
            if self.reject_on_call == Some(call) {
                let mut outcome = ImportOutcome {
                    failed: 1,
                    errors: vec!["injected rejection".into()],
                    ..ImportOutcome::default()
                };
                outcome.imported = (records.len() as u32).saturating_sub(1);
                return Ok(outcome);
            }
            if self.under_report {
                // Swallows the page while claiming nothing happened.
                return Ok(ImportOutcome::default());
            }
            self.inner.import_records(records).await
        }
    }

    #[async_trait::async_trait]
    impl tinymemory_api::provider::MemoryProvider for FlakyProvider {
        fn driver_id(&self) -> &str {
            "flaky"
        }
        fn capabilities(&self) -> tinymemory_api::capabilities::Capabilities {
            tinymemory_api::capabilities::Capabilities::mandatory()
        }
        async fn health(&self) -> tinymemory_api::health::MemoryHealth {
            tinymemory_api::health::MemoryHealth::Ready
        }
    }

    /// A target `import_records` **error** stops with the failed page's start
    /// cursor, and resuming there with a healthy target completes without
    /// duplicating what page one already moved.
    #[tokio::test]
    async fn target_error_stops_with_resume_cursor_and_resume_completes() {
        let from = provider();
        seed(&from, 23).await;
        let flaky = FlakyProvider {
            fail_import_on_call: Some(2),
            ..FlakyProvider::new()
        };
        let to: Arc<dyn MemoryProvider> = Arc::new(flaky);
        let stopped = migrate(&from, &to, 7, None, |_| {})
            .await
            .expect("source is healthy")
            .expect_err("page two must stop the run");
        assert_eq!(
            stopped.summary.imported, 7,
            "page one landed before the stop"
        );
        let resume = stopped.resume_cursor.clone();
        assert!(resume.is_some(), "a mid-run stop must name its page");

        // Resume against the SAME target (now healthy): nothing duplicates.
        let healthy = FlakyProvider::new();
        // Rebuild target state as page one left it, then resume.
        let to2: Arc<dyn MemoryProvider> = Arc::new(healthy);
        let first_page = from.export_page(None, 7).await.unwrap();
        to2.import_records(first_page.records).await.unwrap();
        let outcome = migrate(&from, &to2, 7, resume, |_| {})
            .await
            .expect("healthy")
            .expect("completes");
        assert_eq!(
            count(&to2).await,
            23,
            "resume must deliver exactly the full set, no duplicates"
        );
        assert!(
            outcome.exported < 23,
            "a resumed run must not have re-exported the whole store"
        );
    }

    /// A page the target *rejects* (`failed > 0`) stops with the page's own
    /// errors surfaced.
    #[tokio::test]
    async fn rejected_records_stop_with_the_drivers_reasons() {
        let from = provider();
        seed(&from, 10).await;
        let to: Arc<dyn MemoryProvider> = Arc::new(FlakyProvider {
            reject_on_call: Some(1),
            ..FlakyProvider::new()
        });
        let stopped = migrate(&from, &to, 10, None, |_| {})
            .await
            .expect("source healthy")
            .expect_err("a rejected record must stop");
        assert_eq!(stopped.errors, vec!["injected rejection".to_string()]);
        assert!(stopped.resume_cursor.is_none(), "the FIRST page failed");
    }

    /// The under-reporter: a full page met with an all-zero outcome must stop
    /// as silently-dropped records, never complete as success.
    #[tokio::test]
    async fn an_under_reporting_target_stops_the_run() {
        let from = provider();
        seed(&from, 5).await;
        let to: Arc<dyn MemoryProvider> = Arc::new(FlakyProvider {
            under_report: true,
            ..FlakyProvider::new()
        });
        let stopped = migrate(&from, &to, 10, None, |_| {})
            .await
            .expect("source healthy")
            .expect_err("swallowed records must stop the run");
        assert!(
            stopped.errors[0].contains("accounted for 0 of 5"),
            "{:?}",
            stopped.errors
        );
    }

    /// The clamping pager: a source that echoes the cursor stops instead of
    /// looping, and does not import the echoed page a second time.
    #[tokio::test]
    async fn an_echoed_cursor_stops_instead_of_looping() {
        let flaky = FlakyProvider {
            echo_cursor: true,
            ..FlakyProvider::new()
        };
        // Seed through the wrapper's core delegation.
        for i in 0..9 {
            flaky
                .store(
                    "oc/team",
                    &format!("k{i}"),
                    "v",
                    MemoryCategory::Core,
                    None,
                    MemoryTaint::Internal,
                )
                .await
                .unwrap();
        }
        let from: Arc<dyn MemoryProvider> = Arc::new(flaky);
        let to = provider();
        let stopped = migrate(&from, &to, 4, None, |_| {})
            .await
            .expect("no crate error")
            .expect_err("echo must stop");
        assert!(
            stopped.errors[0].contains("did not advance"),
            "{:?}",
            stopped.errors
        );
        assert!(count(&to).await <= 8, "the echoed page must not re-import");
    }

    /// `count_records` — the dry-run/receipt pager — counts, resumes, and
    /// carries the same echo guard.
    #[tokio::test]
    async fn count_records_counts_resumes_and_refuses_echo() {
        let from = provider();
        seed(&from, 23).await;
        assert_eq!(count_records(&from, None, 7).await.unwrap(), 23);
        let first = from.export_page(None, 7).await.unwrap();
        let rest = count_records(&from, first.next_cursor.clone(), 7)
            .await
            .unwrap();
        assert_eq!(rest, 23 - first.records.len() as u64);

        let echo = FlakyProvider {
            echo_cursor: true,
            ..FlakyProvider::new()
        };
        echo.store(
            "oc/a",
            "k",
            "v",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await
        .unwrap();
        let echo: Arc<dyn MemoryProvider> = Arc::new(echo);
        let cursor = Some("stale".to_string());
        let err = count_records(&echo, cursor, 1).await;
        assert!(err.is_err(), "echoed cursor must refuse, not loop");
    }

    // ── resolve_migrate_configs: every refusal, executed ──────────────────

    use crate::store::{MemoryBackend, StorageKind, StorageSettings};

    fn base_settings() -> StorageSettings {
        StorageSettings {
            memory_backend: MemoryBackend::Remote,
            memory_driver: Some("supermemory".into()),
            memory_url: Some("https://source.example".into()),
            memory_api_key: Some("sk-src".into()),
            data_dir: Some(std::path::PathBuf::from("/data")),
            ..StorageSettings::default()
        }
    }

    fn resolve(
        settings: &StorageSettings,
        to: &str,
        to_url: Option<&str>,
        to_data_dir: Option<&str>,
    ) -> crate::Result<(MemoryDriverConfig, MemoryDriverConfig)> {
        resolve_migrate_configs(
            settings,
            to,
            to_url.map(str::to_owned),
            None,
            to_data_dir.map(std::path::PathBuf::from),
        )
    }

    #[test]
    fn source_backends_without_a_seam_refuse() {
        // Each needle is the refusal's distinctive phrase, asserted whole — a
        // first-word shortcut here reduced the Store case to "no", which any
        // refusal contains.
        for (backend, driver, needle) in [
            (MemoryBackend::Store, None, "provider seam"),
            (MemoryBackend::Null, None, "nothing to migrate"),
            (MemoryBackend::Tinycortex, None, "EngineCortex overlay"),
        ] {
            let settings = StorageSettings {
                memory_backend: backend,
                memory_driver: driver,
                ..base_settings()
            };
            let err = resolve(&settings, "mem0", Some("https://t.example"), None)
                .expect_err("must refuse")
                .to_string();
            assert!(err.contains(needle), "backend {backend:?}: {err}");
        }
    }

    #[test]
    fn to_null_and_unknown_targets_refuse() {
        let s = base_settings();
        assert!(
            resolve(&s, "null", None, None)
                .expect_err("null")
                .to_string()
                .contains("discard every record")
        );
        assert!(
            resolve(&s, "banana", None, None)
                .expect_err("unknown")
                .to_string()
                .contains("names no migratable driver")
        );
    }

    #[test]
    fn tenant_mode_refuses_before_anything_else() {
        let settings = StorageSettings {
            tenant_id: Some("acme-tenant".into()),
            ..base_settings()
        };
        let err = resolve(&settings, "mem0", Some("https://t.example"), None)
            .expect_err("tenant mode must refuse")
            .to_string();
        assert!(err.contains("OPENCOMPANY_TENANT_ID"), "{err}");
    }

    /// The review's blocker two: a missing TARGET endpoint must be named in
    /// the target's own vocabulary (`--to-url`), and must explicitly warn off
    /// the source env var whose advice would silently repoint the source.
    #[test]
    fn a_missing_target_url_names_the_flag_not_the_source_env() {
        let err = resolve(&base_settings(), "mem0", None, None)
            .expect_err("hosted target without --to-url must refuse")
            .to_string();
        assert!(err.contains("--to-url"), "{err}");
        assert!(
            err.contains("SOURCE"),
            "must warn that OPENCOMPANY_MEMORY_URL is the source's: {err}"
        );
    }

    #[test]
    fn a_namespace_target_with_no_dir_anywhere_names_the_flag() {
        let settings = StorageSettings {
            data_dir: None,
            ..base_settings()
        };
        let err = resolve(&settings, "namespace", None, None)
            .expect_err("no dir at all must refuse")
            .to_string();
        assert!(err.contains("--to-data-dir"), "{err}");
    }

    #[test]
    fn a_mongodb_base_refuses_an_ephemeral_namespace_target() {
        let settings = StorageSettings {
            kind: StorageKind::Mongodb,
            ..base_settings()
        };
        let err = resolve(&settings, "namespace", None, Some("/data/mem"))
            .expect_err("ephemeral target must refuse")
            .to_string();
        assert!(err.contains("OPENCOMPANY_MEMORY_ALLOW_EPHEMERAL"), "{err}");
        let allowed = StorageSettings {
            allow_ephemeral_memory: true,
            ..settings
        };
        resolve(&allowed, "namespace", None, Some("/data/mem"))
            .expect("the operator override must open the path");
    }

    #[test]
    fn the_same_engine_guard_fires_remote_to_remote_and_normalizes() {
        // Identical endpoint, spelled with a trailing slash and whitespace:
        // the naive field-equality version could never fire here because the
        // source carries the env data_dir and the target None.
        let err = resolve(
            &base_settings(),
            "supermemory",
            Some(" https://source.example/ "),
            None,
        )
        .expect_err("same hosted engine at the same endpoint must refuse")
        .to_string();
        assert!(err.contains("same engine"), "{err}");

        // A different endpoint proceeds.
        resolve(
            &base_settings(),
            "supermemory",
            Some("https://new.example"),
            None,
        )
        .expect("a genuinely different endpoint is a migration");
    }

    #[test]
    fn the_same_engine_guard_canonicalizes_dirs() {
        // A real directory reached by two spellings: direct, and via `..`.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("data");
        std::fs::create_dir_all(&real).unwrap();
        let dotted = dir.path().join("sub").join("..").join("data");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();

        let settings = StorageSettings {
            memory_backend: MemoryBackend::Tinycortex,
            memory_driver: Some("namespace".into()),
            data_dir: Some(real),
            ..StorageSettings::default()
        };
        let err = resolve(&settings, "namespace", None, Some(dotted.to_str().unwrap()))
            .expect_err("a dotted spelling of the same dir must refuse")
            .to_string();
        assert!(err.contains("same engine"), "{err}");
    }
}
