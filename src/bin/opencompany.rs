use std::path::PathBuf;

use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use opencompany::company::Schedule;
use opencompany::runtime::{CompanyScheduler, MaintenanceTicker, SystemClock, WorkflowScheduler};
use opencompany::{
    AppConfig, AppState, CompanyId, CompanyManifest, Result,
    app::config::{ConfigFile, ProcessEnv, resolve},
    app::doctor,
    openhuman::{LaunchMode, OpenHumanLaunch},
    runtime::RuntimeBuilder,
};
use tokio::sync::Notify;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the Axum HTTP server.
    Serve {
        /// Address to bind. Falls back to `OPENCOMPANY_BIND`, then the
        /// `bind` key of `config.toml`, then `127.0.0.1:8080`.
        #[arg(long)]
        bind: Option<String>,
        /// Optional OpenHuman checkout path to report in `/spec`.
        #[arg(long)]
        openhuman_root: Option<PathBuf>,
        /// A company to load and register at boot (a manifest file or a
        /// directory containing one). Repeatable for multi-company hosting.
        #[arg(long = "company", value_name = "DIR")]
        companies: Vec<PathBuf>,
        /// OpenCompany home holding company bundles. Defaults to
        /// `$HOME/.opencompany`, with bundles under `companies/<slug>`.
        #[arg(long)]
        home: Option<PathBuf>,
        /// Opt every loaded company into going public on tiny.place, regardless
        /// of each manifest's `[place].discoverable`. Requires the `tinyplace`
        /// feature to actually reach the network; without it the flag only marks
        /// companies discoverable for the local A2A routes.
        #[arg(long)]
        discoverable: bool,
    },
    /// Print a JSON runtime specification.
    Spec {
        /// Optional OpenHuman checkout path to report.
        #[arg(long)]
        openhuman_root: Option<PathBuf>,
    },
    /// Validate a company manifest and print its effective configuration.
    Check {
        /// Manifest file or a directory containing `company.toml`/`agents.toml`.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Report the effective runtime configuration, which layer set each value,
    /// and what is missing per optional capability.
    Doctor {
        /// Optional company manifest whose `[brain].mode` participates in
        /// resolution. Defaults to a synthetic manifest when omitted.
        #[arg(long = "company", value_name = "DIR")]
        company: Option<PathBuf>,
        /// Print the report as JSON instead of aligned text.
        #[arg(long)]
        json: bool,
    },
    /// Export a company's bundle: read everything through the storage ports and
    /// write the canonical filesystem layout. With `--features export` the output
    /// is a single `.tar`; otherwise an unpacked bundle directory. Secrets and
    /// keys are excluded unless `--include-secrets` is set.
    Export {
        /// Company id (slug) to export.
        company: String,
        /// Output path (`<slug>.tar` under `--features export`, else a bundle
        /// directory). Defaults to the current directory.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Include the fs-only `secrets/` and `keys/` directories.
        #[arg(long)]
        include_secrets: bool,
        /// OpenCompany home holding company bundles. Defaults to
        /// `$HOME/.opencompany`, with bundles under `companies/<slug>`.
        #[arg(long)]
        home: Option<PathBuf>,
    },
    /// Import a company bundle (a `.tar` under `--features export`, else an
    /// unpacked bundle directory) into a home through the storage ports.
    Import {
        /// Bundle `.tar` or unpacked bundle directory to import.
        path: PathBuf,
        /// OpenCompany home to import into. Defaults to
        /// `$HOME/.opencompany`, with bundles under `companies/<slug>`.
        #[arg(long)]
        home: Option<PathBuf>,
    },
    /// Launch a sibling OpenHuman checkout: the core binary (`--mode core`)
    /// or the Tauri desktop host (`--mode desktop`). Desktop calls `cargo tauri`
    /// directly and performs the preflight OpenHuman's own scripts do — install
    /// the vendored CEF-aware `tauri-cli`, pin `CEF_PATH`, load `<root>/.env`,
    /// and on macOS seed the Chromium keychain + signing identity (CEF on macOS,
    /// `wry` on Linux/Windows; Tauri still drives the Vite dev server). Pass
    /// `--dry-run` to preview.
    OpenHuman {
        /// OpenHuman checkout path.
        #[arg(long, default_value = "vendor/openhuman")]
        root: PathBuf,
        /// Launch target.
        #[arg(long, value_enum, default_value_t = ModeArg::Core)]
        mode: ModeArg,
        /// Build a release bundle instead of launching a dev session
        /// (`cargo run --release` for core; `cargo tauri build` for desktop —
        /// a signed `.app`/dmg on macOS, a deb/AppImage elsewhere).
        #[arg(long)]
        release: bool,
        /// Print the command without executing it.
        #[arg(long)]
        dry_run: bool,
        /// Arguments passed after `--` to the OpenHuman core binary. Ignored
        /// (and rejected) in desktop mode, which drives fixed pnpm scripts.
        #[arg(last = true)]
        args: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ModeArg {
    Core,
    Desktop,
}

impl From<ModeArg> for LaunchMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Core => LaunchMode::Core,
            ModeArg::Desktop => LaunchMode::Desktop,
        }
    }
}

/// Resolves a `--company` argument to its source *directory*. `--company`
/// accepts either the company directory (`companies/<name>`) or the manifest
/// file inside it (`companies/<name>/company.toml`); the file form is normalized
/// to its parent so workspace seeding and the skill/workflow read resolvers look
/// under the company directory rather than under `company.toml/…`.
fn company_source_dir(path: &std::path::Path) -> std::path::PathBuf {
    if path.is_file() {
        path.parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

/// Loads the manifest under `dir`, builds a runtime over `home`, and registers
/// it in `state`. Returns the derived company id and display name.
async fn register_company(
    state: &AppState,
    home: &std::path::Path,
    dir: &std::path::Path,
    discoverable: bool,
) -> Result<(String, String, Vec<Schedule>)> {
    let mut manifest = CompanyManifest::from_path(dir)?;
    // `serve --discoverable` opts this company into going public regardless of
    // its manifest: mark it discoverable and synthesize a @handle when absent so
    // Agent Card generation and validation succeed.
    if discoverable {
        manifest.place.discoverable = true;
        if manifest.company.handle.is_none() {
            let handle = opencompany::runtime::company_id_from_name(&manifest.company.name)
                .as_ref()
                .to_string();
            manifest.company.handle = Some(handle);
        }
    }
    let name = manifest.company.name.clone();
    // Capture the schedules before the manifest is moved into the builder; boot
    // uses them to start this company's cron scheduler (lifecycle step 4).
    let schedules = manifest.schedules.clone();
    // The company's on-disk source directory (`companies/<name>`) seeds the
    // workspace tree on first boot and lets read resolvers find its committed
    // skills/workflows content.
    let source_dir = company_source_dir(dir);
    // Issue #85: this company was seeded from a template directory, so record
    // that directory's slug as the durable source-template provenance. The
    // builder stamps it only on first launch and carries it forward on rebuilds;
    // a raw-manifest `POST /api/v1/companies` provision has no template dir and
    // records no provenance.
    let provenance = source_dir.file_name().and_then(|s| s.to_str()).map(|slug| {
        opencompany::ports::types::TemplateProvenance {
            source_id: slug.to_string(),
            version: None,
            // Record only the template directory's basename, never the raw
            // absolute host path: `path` is exposed verbatim on the GraphQL and
            // REST provenance surfaces, and the absolute source dir would leak
            // the host filesystem layout + username. `slug` is already the final
            // path component (the `file_name()` guard above makes this `None`
            // when there is no basename, e.g. `serve --company .`).
            path: Some(slug.to_string()),
        }
    });
    // Shared-single-DB mode: namespace the derived id with this tenant so the
    // same boot template (`OPENCOMPANY_COMPANY`) does not collide across tenants
    // in one logical database. A no-op when `tenant_namespace` is unset.
    let derived = opencompany::runtime::company_id_from_name(&name);
    let company_id = state.config().namespaced_company_id(derived);
    let mut builder = company_builder(
        state,
        home,
        manifest,
        &company_id,
        Some(source_dir.clone()),
        discoverable,
    )?;
    if let Some(provenance) = provenance {
        builder = builder.with_template_provenance(provenance);
    }
    let runtime = builder.build().await?;
    // A company with no sign-in on a host anyone can reach is an unauthenticated
    // admin console, not a desktop app. `none` mode's whole premise is that the
    // only caller is the person at the machine, so a routable bind contradicts
    // it — and the contradiction is silent, because the host does start and does
    // serve. Refuse at boot, where somebody is looking, exactly as a
    // selected-but-unavailable storage backend does.
    if !runtime.auth_mode().has_login() && !state.config().is_local_only() {
        return Err(opencompany::error::OpenCompanyError::Config(format!(
            "company `{}` is configured with `[users].mode = \"none\"`, which has no sign-in, \
             but this host binds `{}` and would serve it to anyone who can reach that address. \
             Bind loopback, or choose `email` or `wallet`.",
            runtime.id().as_ref(),
            state.config().bind,
        )));
    }
    let company_id = runtime.id().clone();
    let id = company_id.as_ref().to_string();
    // Record boot-company ownership so a shared-DB manager can later purge by
    // tenant. Only meaningful in tenant-namespace mode; otherwise skipped so
    // db-per-tenant / self-hosted deployments keep their in-memory-only stub.
    if let Some(tenant) = state.config().tenant_namespace.clone() {
        // Canonical (bare-slug) form so the persisted `owners` row matches what
        // tenant-scoped auth compares a `tenant:acme` claim against.
        let tenant = opencompany::app::canonical_tenant(&tenant).to_string();
        state.set_owner(company_id.clone(), tenant.clone());
        if let Some(ownership) = state.stores().and_then(|s| s.ownership.clone())
            && let Err(err) = ownership.set_owner(&company_id, &tenant).await
        {
            eprintln!("failed to persist ownership for `{id}`: {err}");
        }
    }
    // Issue #290: stash what a later in-place rebuild cannot recover any other
    // way. `--discoverable` is the case that forces this to exist: it lives only
    // in the `serve` stack frame and mutates the manifest before the build.
    state.set_boot_inputs(
        company_id.clone(),
        opencompany::runtime::BootInputs {
            source_dir: Some(source_dir),
            discoverable,
        },
    );
    state.registry().insert(company_id, Arc::new(runtime));
    Ok((id, name, schedules))
}

/// Assembles the `RuntimeBuilder` for one company with this host's full boot
/// wiring: the OpenHuman RPC transport, the harness pool and its managed
/// backends, feedback routing, the opened storage backend and memory overlay,
/// the shared skill library, and the manager-injected per-tenant mailbox.
///
/// Extracted from [`register_company`] so an in-place rebuild (issue #290) runs
/// the *same* wiring boot ran. A rebuild that assembled its own would drift from
/// boot silently, and the first symptom would be a company that came back from a
/// rebuild missing a capability nobody changed.
fn company_builder(
    state: &AppState,
    home: &std::path::Path,
    manifest: CompanyManifest,
    company_id: &CompanyId,
    source_dir: Option<PathBuf>,
    discoverable: bool,
) -> Result<RuntimeBuilder> {
    let mut builder = attach_tinyhumans_feedback(
        attach_harness(attach_openhuman(RuntimeBuilder::new(
            home.to_path_buf(),
            manifest,
        ))),
        state.config(),
    )
    .with_tinyplace_api_url(state.config().tinyplace_api_url.clone())
    // Install-wide MCP defaults (issue #527): every company built on this
    // instance gets them, which is what makes a fresh install useful with no
    // per-company setup. Already normalized when the config resolved.
    .with_default_mcp_servers(state.config().default_mcp_servers.clone())
    .with_host_base_url(state.config().host_base_url())
    .with_workspace_quota(state.config().workspace_quota)
    .with_workspace_git_enabled(state.config().workspace_git_enabled)
    // Issue #752: the backend that serves this host's secrets, which the
    // repository-credential gates refuse on. Threaded through `company_builder`
    // rather than read from the environment further down, so a rebuild gets the
    // same answer boot got.
    .with_storage_kind(state.storage_kind())
    // Issue #661 / M8: the deployment's standing bootstrap admin, normalized,
    // so a fresh tenant's `owner` report reaches its creator before that first
    // sign-in mints a user record. `None` (self-hosted, no injected address) is
    // a no-op. BootRebuilder reuses this builder, so the grant survives rebuild.
    .with_bootstrap_admin(state.config().bootstrap_admin())
    // How humans sign in, when the host names it for every company it serves
    // (`OPENCOMPANY_AUTH_MODE` / `config.toml`). `None` leaves each manifest's
    // `[users].mode` to answer, which is the normal case.
    .with_auth_mode_override(state.auth_mode_override())
    .with_skills_registry(state.shared_skill_registry()?)
    .with_id(company_id.clone());
    if let Some(source_dir) = source_dir {
        builder = builder.with_seed_dir(source_dir);
    }
    if let Some(stores) = state.stores() {
        builder = builder.with_stores(stores);
    }
    if let Some(overlay) = state.memory_overlay() {
        builder = builder.with_memory_overlay(overlay);
    }
    #[cfg(feature = "smtp")]
    if let Ok(Some(cfg)) = opencompany::server::ops::mailer::TenantMailboxConfig::from_env() {
        // Same guard as `spawn_mailbox_poller`: the injected mailbox belongs to
        // exactly one company (the one whose id matches its local-part). In a
        // multi-company process, wiring it to every company would make every
        // one of them send outbound mail from the same injected address.
        let mailbox_slug = opencompany::server::ops::smtp::local_part(&cfg.address);
        if company_id.as_ref() == mailbox_slug {
            builder = builder.with_mail(opencompany::company::runtime::CompanyMail {
                sender: std::sync::Arc::new(opencompany::server::ops::smtp::LettreMailSender),
                smtp: cfg.smtp.clone(),
            });
        }
    }
    if discoverable {
        builder = builder.with_discoverable(true);
    }
    Ok(builder)
}

/// This host's in-place runtime rebuilder (issue #290).
///
/// Lives in the binary because [`company_builder`] does: the harness pool, the
/// OpenHuman transport and the managed media/search backends are assembled here
/// from the process environment and feature flags, and a rebuild that did not
/// reuse that assembly would quietly produce a differently-wired company.
struct BootRebuilder;

#[async_trait::async_trait]
impl opencompany::runtime::RuntimeRebuilder for BootRebuilder {
    async fn rebuild(
        &self,
        state: &AppState,
        request: opencompany::runtime::RebuildRequest,
    ) -> Result<opencompany::CompanyRuntime> {
        company_builder(
            state,
            state.home(),
            request.manifest,
            &request.id,
            request.boot.source_dir,
            request.boot.discoverable,
        )?
        // The whole point: the successor adopts the live journal, approval gate,
        // grant set, stores, harness pool, MCP runtime and serialising mutexes
        // rather than constructing a second copy of any of them.
        .with_handover(request.handover)
        .build()
        .await
    }
}

/// Starts a company's cron scheduler as a background task, if it has schedules.
///
/// A schedule whose cron fails to parse (which `opencompany check` does not
/// catch beyond field count) logs a warning and is skipped rather than aborting
/// boot. The returned handle is held by the caller and stops when `shutdown`
/// fires.
fn spawn_scheduler(
    state: &AppState,
    id: &str,
    schedules: &[Schedule],
    shutdown: &Arc<Notify>,
) -> Option<tokio::task::JoinHandle<()>> {
    // **This early return was issue #971.** Until the maintenance ticker
    // existed, sweeping expired approvals, expired grants and stale fire claims
    // rode this scheduler's minute loop — so a company with no `[[schedule]]`
    // returned here, spawned nothing, and swept nothing, forever, at any age.
    // The tenant that reported it drove everything through *workflow* schedules,
    // which run on a different loop that never called maintenance.
    //
    // It is correct now, and only because maintenance no longer depends on it:
    // `spawn_maintenance_ticker` is process-wide and always on. Nothing that
    // has to happen for every company may be attached below this line.
    if schedules.is_empty() {
        return None;
    }
    let runtime = state.registry().get(&CompanyId::new(id))?;
    match CompanyScheduler::new(runtime, schedules, Arc::new(SystemClock)) {
        // Follow the registry so an in-place rebuild (issue #290) reaches cron.
        // Without this the scheduler keeps driving the runtime it snapshotted —
        // which after a rebuild is the replaced, quiesced one, i.e. exactly the
        // "scheduled workflows never fire" surface #266 was reported against.
        Ok(scheduler) => Some(
            scheduler
                .following(state.registry().clone())
                .spawn(shutdown.clone()),
        ),
        Err(err) => {
            eprintln!("skipping scheduler for `{id}`: {err}");
            None
        }
    }
}

/// Starts the process-wide workflow scheduler: one task that fires every saved
/// workflow whose `trigger` node carries a cron (issue #169).
///
/// Deliberately NOT per company, unlike [`spawn_scheduler`]. Workflow schedules
/// are runtime data — creating a workflow in the console adds a cron with no
/// reboot, and a hosted tenant can be registered after boot — so the scheduler
/// re-reads the registry on every tick instead of snapshotting a company's
/// schedules at registration time.
fn spawn_workflow_scheduler(
    state: &AppState,
    shutdown: &Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    WorkflowScheduler::new(state.registry().clone(), Arc::new(SystemClock)).spawn(shutdown.clone())
}

/// Starts the process-wide maintenance ticker: one task that retires overdue
/// approvals, expired grants and stale fire claims for every registered company
/// (issue #971).
///
/// Process-wide for the same reason as [`spawn_workflow_scheduler`], and it is
/// the whole fix. The per-company [`spawn_scheduler`] above has to be reached at
/// every place a company can come into existence — a `--company` flag, adoption
/// of an existing data root, a hosted tenant registered after boot — and it
/// declines to start at all for a company with no manifest cron. Maintenance
/// must happen for every company unconditionally, so it hangs off the registry
/// and is started once, here, whether or not any company is loaded yet.
fn spawn_maintenance_ticker(
    state: &AppState,
    shutdown: &Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    MaintenanceTicker::new(state.registry().clone(), Arc::new(SystemClock)).spawn(shutdown.clone())
}

/// Starts a company's IMAP mailbox poller as a background task, if the
/// platform injected mailbox credentials for this tenant.
///
/// Mirrors [`spawn_scheduler`]: reads [`TenantMailboxConfig::from_env`]
/// (`Ok(None)` means the manager did not wire mail for this tenant — a no-op,
/// not an error; `Err` logs and skips rather than aborting boot). The actual
/// IMAP transport only exists when the crate is built with the `imap`
/// feature, so the poll itself is feature-gated; without the feature this
/// still validates the env (surfacing config typos) but starts nothing.
fn spawn_mailbox_poller(
    state: &AppState,
    id: &str,
    shutdown: &Arc<Notify>,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let cfg = match opencompany::server::ops::mailer::TenantMailboxConfig::from_env() {
        Ok(Some(cfg)) => cfg,
        Ok(None) => return,
        Err(err) => {
            eprintln!("mailbox config error: {err}");
            return;
        }
    };
    // The injected mailbox belongs to exactly one company: the one whose id
    // equals the mailbox address's local-part. In a multi-company process
    // every OTHER registered company must skip this poller entirely, or
    // inbound mail addressed to one company would be filed into another's
    // inbox.
    let mailbox_slug = opencompany::server::ops::smtp::local_part(&cfg.address);
    if id != mailbox_slug {
        return;
    }
    #[cfg(feature = "imap")]
    {
        let Some(runtime) = state.registry().get(&CompanyId::new(id)) else {
            return;
        };
        let receiver: Arc<dyn opencompany::server::ops::mailer::MailReceiver> =
            Arc::new(opencompany::server::ops::imap::AsyncImapReceiver);
        let interval = std::env::var("OPENCOMPANY_MAIL_POLL_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        let poller = opencompany::runtime::mailbox_poller::MailboxPoller::new(
            runtime,
            receiver,
            cfg.imap.clone(),
            cfg.address.clone(),
            interval,
        )
        // See `spawn_scheduler`: follow the registry so a rebuild reaches
        // inbound mail instead of stranding it on the replaced runtime.
        .following(state.registry().clone());
        handles.push(poller.spawn(shutdown.clone()));
    }
    #[cfg(not(feature = "imap"))]
    {
        let _ = (state, id, shutdown, handles, cfg);
    }
}

/// Starts a company's Telegram `getUpdates` long-polling listener as a
/// background task, whenever this host has an outbound Telegram transport wired
/// (the `telegram` feature).
///
/// Issue #203: this is what makes inbound Telegram work on a local or
/// self-hosted instance, where Telegram's servers can never reach an inbound
/// `/hooks/...` URL. It is started unconditionally rather than only when a bot
/// token is already stored — the poller idles cheaply until one appears, so an
/// operator who pastes a token in the console is receiving DMs on the next tick
/// with no restart. On a publicly reachable host that opted into the webhook
/// fast-path, the poller sees the registration and stands by.
fn spawn_telegram_poller(
    state: &AppState,
    id: &str,
    shutdown: &Arc<Notify>,
    handles: &mut Vec<tokio::task::JoinHandle<()>>,
) {
    let Some(api) = state.connections().telegram.clone() else {
        return;
    };
    let Some(runtime) = state.registry().get(&CompanyId::new(id)) else {
        return;
    };
    let poll_secs = std::env::var("OPENCOMPANY_TELEGRAM_POLL_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(opencompany::runtime::telegram_poller::DEFAULT_POLL_SECONDS);
    let webhook_capable = state.config().public_webhook_base_url().is_some();
    let poller = opencompany::runtime::telegram_poller::TelegramPoller::new(
        runtime,
        api,
        poll_secs,
        webhook_capable,
    )
    // See `spawn_scheduler`: follow the registry so a rebuild reaches inbound
    // Telegram instead of stranding it on the replaced runtime.
    .following(state.registry().clone());
    handles.push(poller.spawn(shutdown.clone()));
}

/// Attaches an OpenHuman JSON-RPC transport when the `openhuman-rpc` feature is
/// enabled and `OPENCOMPANY_OPENHUMAN_URL` is set (the attach path).
///
/// Without the feature this is the identity function, so the default build
/// stays network-free and degrades to built-in tools and the operator channel.
#[cfg(not(feature = "openhuman-rpc"))]
fn attach_openhuman(builder: RuntimeBuilder) -> RuntimeBuilder {
    builder
}

#[cfg(feature = "openhuman-rpc")]
fn attach_openhuman(builder: RuntimeBuilder) -> RuntimeBuilder {
    use opencompany::openhuman::HttpOpenHumanRpc;
    use opencompany::ports::SecretValue;

    match std::env::var("OPENCOMPANY_OPENHUMAN_URL") {
        Ok(url) if !url.trim().is_empty() => {
            let bearer =
                SecretValue(std::env::var("OPENCOMPANY_OPENHUMAN_TOKEN").unwrap_or_default());
            builder.with_openhuman_rpc(Arc::new(HttpOpenHumanRpc::attach(url, bearer)))
        }
        _ => builder,
    }
}

/// Attaches the embedded OpenHuman harness under the `openhuman` feature.
///
/// The harness pool is **always** attached, so cognition routes through a live
/// company agent whenever *any* inference source is configured — the managed
/// env default (`TINYHUMANS_API_KEY` / `OPENCOMPANY_INFERENCE_*`), a manifest
/// `[inference]` section, or a runtime console override (issue #56 — BYOK).
/// Attaching the pool unconditionally is what unblocks a BYOK-only tenant that
/// has no platform credential: the builder still constructs the harness brain
/// from its manifest/runtime config. Without any source, the runtime keeps its
/// hosted/echo brain.
///
/// Without the feature this is the identity function, so the default build is
/// unaffected.
#[cfg(not(feature = "openhuman"))]
fn attach_harness(builder: RuntimeBuilder) -> RuntimeBuilder {
    builder
}

#[cfg(feature = "openhuman")]
fn attach_harness(builder: RuntimeBuilder) -> RuntimeBuilder {
    use opencompany::app::config::ProcessEnv;
    use opencompany::harness::HarnessPool;
    use opencompany::harness::provider::{
        PlatformCredentialStatus, harness_inference_from_env, media_backend_from_env,
        search_backend_from_env,
    };

    // Issue #879: every managed surface below fails closed and says nothing at
    // boot, so a tenant provisioned without its platform token comes up looking
    // healthy and only reveals the gap when an agent is built or a workflow node
    // 500s. Say it once, here, where an operator reading the pod's first lines
    // will see it.
    if let Some(warning) = PlatformCredentialStatus::resolve(&ProcessEnv).boot_warning() {
        tracing::warn!("[boot] {warning}");
    }

    let builder = builder.with_harness(Arc::new(HarnessPool::new()));
    // Issue #109: the MANAGED media-generation backend, resolved from the
    // environment only (never a tenant secret). Absent ⇒ media tools stay unwired
    // even for a company that grants `media` (fail-closed).
    let builder = match media_backend_from_env(&ProcessEnv) {
        Some(media_backend) => builder.with_media_backend(media_backend),
        None => builder,
    };
    // Issue #238: the MANAGED web-search backend, on the same platform identity
    // as managed inference and resolved from the environment only. Absent ⇒
    // `web_search` stays unwired even for a company that grants `search`.
    let builder = match search_backend_from_env(&ProcessEnv) {
        Some(search_backend) => builder.with_search_backend(search_backend),
        None => builder,
    };
    // The managed env default is an *optional*, lowest-precedence source; a
    // BYOK-only tenant supplies none and still gets a harness brain from its
    // manifest/runtime config.
    match harness_inference_from_env(&ProcessEnv) {
        Some((config, model_override)) => builder.with_harness_inference(config, model_override),
        None => builder,
    }
}

/// Routes feedback to the TinyHumans hub when this instance is provisioned with
/// a credential, so reports are recorded on behalf of the credential's owner
/// instead of being filed as issues from here.
///
/// Without the feature this is the identity function, so the default build stays
/// network-free and keeps the local capture → GitHub/manual-link path.
#[cfg(not(feature = "tinyhumans"))]
fn attach_tinyhumans_feedback(builder: RuntimeBuilder, _config: &AppConfig) -> RuntimeBuilder {
    builder
}

/// Wires the hub identity exchange, when this build can reach the hub.
///
/// Rides the existing `tinyhumans` feature rather than earning one of its own:
/// that flag already means "this instance talks to the hub about its
/// credential's owner", and asking the hub whose sign-in token this is is the
/// same conversation about the same owner.
///
/// Unwired, `…/auth/hub` reports no providers and the console shows only the
/// magic-link form — which is the right answer for a self-hosted host that has
/// no ecosystem to sign in against.
#[cfg(not(feature = "tinyhumans"))]
fn attach_hub_identity(state: AppState) -> AppState {
    state
}

#[cfg(feature = "tinyhumans")]
fn attach_hub_identity(state: AppState) -> AppState {
    use opencompany::server::hub_identity::HttpHubIdentityExchange;

    let exchange = HttpHubIdentityExchange::new(state.config().api_url.clone());
    state.with_hub_identity(std::sync::Arc::new(exchange))
}

#[cfg(feature = "tinyhumans")]
fn attach_tinyhumans_feedback(builder: RuntimeBuilder, config: &AppConfig) -> RuntimeBuilder {
    use opencompany::feedback::HttpTinyHumansClient;

    match &config.tinyhumans_credential {
        Some(credential) => builder.with_tinyhumans_feedback(Arc::new(HttpTinyHumansClient::new(
            config.api_url.clone(),
            credential.clone(),
        ))),
        // Unprovisioned: keep the local path rather than dropping reports.
        None => builder,
    }
}

/// Parses a non-empty `usize` environment variable, ignoring unset/empty/invalid
/// values (an invalid value logs a warning and is treated as unset).
fn env_usize(key: &str) -> Option<usize> {
    match std::env::var(key) {
        Ok(value) if !value.trim().is_empty() => match value.trim().parse::<usize>() {
            Ok(parsed) => Some(parsed),
            Err(_) => {
                eprintln!("ignoring {key}=`{value}`: expected a non-negative integer");
                None
            }
        },
        _ => None,
    }
}

/// Default build: outbound webhooks require the `webhooks` feature; without it a
/// configured URL is warned and dropped.
#[cfg(not(feature = "webhooks"))]
fn webhook_config(_url: String) -> Option<opencompany::server::webhook::WebhookConfig> {
    eprintln!(
        "OPENCOMPANY_WEBHOOK_URL is set but the `webhooks` feature is not built; webhooks disabled"
    );
    None
}

/// Feature build: post to the configured URL with an HMAC-SHA256 signature.
#[cfg(feature = "webhooks")]
fn webhook_config(url: String) -> Option<opencompany::server::webhook::WebhookConfig> {
    use opencompany::server::webhook::{HmacSha256Signer, HttpWebhookSink, WebhookConfig};
    let secret = std::env::var("OPENCOMPANY_WEBHOOK_SECRET").unwrap_or_default();
    Some(WebhookConfig {
        sink: Arc::new(HttpWebhookSink::new(url)),
        signer: Arc::new(HmacSha256Signer),
        secret,
    })
}

/// Builds the injected connection seams for the credential surfaces. A real DNS
/// resolver is wired under the `dns` feature and a real SMTP sender under
/// `smtp`; the default build injects neither, so those surfaces 404 as
/// "not wired yet".
///
/// Host-level mail credentials (`OPENCOMPANY_MAIL_*`) are resolved here too. A
/// malformed or half-finished mail configuration is an error rather than a
/// silent `None`: a deployment that set those vars meant to have working mail.
fn connections_runtime() -> Result<opencompany::server::ops::ConnectionsRuntime> {
    #[allow(unused_mut)]
    let mut connections = opencompany::server::ops::ConnectionsRuntime::new();
    #[cfg(feature = "dns")]
    {
        match opencompany::company::dns::HickoryDnsResolver::from_system() {
            Ok(resolver) => connections = connections.with_dns(Arc::new(resolver)),
            Err(err) => eprintln!("dns resolver unavailable: {err}"),
        }
    }
    #[cfg(feature = "smtp")]
    {
        connections =
            connections.with_mail(Arc::new(opencompany::server::ops::smtp::LettreMailSender));
    }
    #[cfg(feature = "telegram")]
    {
        connections = connections.with_telegram(Arc::new(
            opencompany::company::telegram::HttpTelegramApi::new(),
        ));
    }
    if let Some(mail) = opencompany::server::ops::mailer::MailConfig::from_env()? {
        connections = connections.with_mail_credentials(mail.credentials);
    }
    Ok(connections)
}

/// Resolves the OpenCompany home and moves any legacy doubled install up before
/// anything reads it.
///
/// Every command that touches bundles resolves through this rather than calling
/// `store::resolve_home` directly. `serve` needs it or the operator's companies
/// vanish from the console; `export` and `import` need it because otherwise an
/// un-migrated install's first post-upgrade command is the one that fails to
/// find its bundles. See `store::migrate` for the rules and the hosted no-op.
fn resolve_home_migrated(flag: Option<PathBuf>) -> Result<PathBuf> {
    let home = opencompany::store::resolve_home(flag)?;
    opencompany::store::migrate_legacy_nest_announced(&home)?;
    Ok(home)
}

/// Builds the four fs storage ports over `home` as trait objects.
fn fs_ports(home: &std::path::Path) -> opencompany::store::export::Ports {
    use opencompany::store::{FsCompanyStore, FsContextStore, FsEventLog, FsMemoryStore};
    (
        Arc::new(FsCompanyStore::new(home.to_path_buf())),
        Arc::new(FsEventLog::new(home.to_path_buf())),
        Arc::new(FsMemoryStore::new(home.to_path_buf())),
        Arc::new(FsContextStore::new(home.to_path_buf())),
    )
}

/// A process-unique temporary path under the system temp dir. Used only by the
/// `.tar` staging paths, which are compiled under the `export` feature.
#[cfg(feature = "export")]
fn unique_temp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("opencompany-{tag}-{}-{nanos}", std::process::id()))
}

/// Exports `id`'s bundle over the fs ports into the directory `dest`.
async fn export_to_dir(
    home: &std::path::Path,
    id: &CompanyId,
    include_secrets: bool,
    dest: &std::path::Path,
) -> Result<()> {
    use opencompany::store::export::{ExportOpts, export_bundle};
    use opencompany::store::paths::Bundle;

    let (store, events, memory, context) = fs_ports(home);
    let opts = ExportOpts {
        include_secrets,
        fs_bundle: Some(Bundle::new(home.to_path_buf(), id).dir().to_path_buf()),
    };
    export_bundle(id, dest, store, events, memory, context, opts).await
}

/// Default build: export writes an unpacked bundle directory (no `.tar` support
/// without the `export` feature).
#[cfg(not(feature = "export"))]
async fn run_export(
    company: String,
    out: Option<PathBuf>,
    include_secrets: bool,
    home: Option<PathBuf>,
) -> Result<()> {
    let home = resolve_home_migrated(home)?;
    let id = CompanyId::new(company);
    let dest = out.unwrap_or_else(|| PathBuf::from(format!("{}-bundle", id.as_ref())));
    export_to_dir(&home, &id, include_secrets, &dest).await?;
    println!(
        "exported bundle for `{id}` to {} (build with --features export to produce a .tar)",
        dest.display()
    );
    Ok(())
}

/// Feature build: export writes a single-file `.tar`.
#[cfg(feature = "export")]
async fn run_export(
    company: String,
    out: Option<PathBuf>,
    include_secrets: bool,
    home: Option<PathBuf>,
) -> Result<()> {
    use opencompany::store::export::pack_tar;

    let home = resolve_home_migrated(home)?;
    let id = CompanyId::new(company);
    let out = out.unwrap_or_else(|| PathBuf::from(format!("{}.tar", id.as_ref())));

    // Stage the unpacked bundle under a slug-named dir so the tar nests cleanly.
    let staging = unique_temp("export");
    let bundle_dir = staging.join(id.as_ref());
    let result = async {
        export_to_dir(&home, &id, include_secrets, &bundle_dir).await?;
        pack_tar(&bundle_dir, &out)
    }
    .await;
    tokio::fs::remove_dir_all(&staging).await.ok();
    result?;
    println!("exported bundle for `{id}` to {}", out.display());
    Ok(())
}

/// Default build: import reads an unpacked bundle directory (no `.tar` support
/// without the `export` feature).
#[cfg(not(feature = "export"))]
async fn run_import(path: PathBuf, home: Option<PathBuf>) -> Result<()> {
    use opencompany::OpenCompanyError;

    if !path.is_dir() {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "{} is not a directory; rebuild with --features export to import a .tar",
            path.display()
        )));
    }
    import_from_dir(&path, home).await
}

/// Feature build: import a `.tar` (unpacked to a temp dir first) or a directory.
#[cfg(feature = "export")]
async fn run_import(path: PathBuf, home: Option<PathBuf>) -> Result<()> {
    use opencompany::store::export::unpack_tar;

    if path.is_dir() {
        return import_from_dir(&path, home).await;
    }
    let staging = unique_temp("import");
    let result = async {
        unpack_tar(&path, &staging)?;
        import_from_dir(&staging, home.clone()).await
    }
    .await;
    tokio::fs::remove_dir_all(&staging).await.ok();
    result
}

/// Imports the bundle rooted under `dir` into `home` through the fs ports,
/// restoring any fs-only secrets/keys the bundle carried.
async fn import_from_dir(dir: &std::path::Path, home: Option<PathBuf>) -> Result<()> {
    use opencompany::store::export::{find_bundle_root, import_bundle, restore_fs_artifacts};
    use opencompany::store::paths::Bundle;

    let home = resolve_home_migrated(home)?;
    let root = find_bundle_root(dir)?;
    let (store, events, memory, context) = fs_ports(&home);
    let id = import_bundle(&root, store, events, memory, context).await?;
    restore_fs_artifacts(&root, Bundle::new(home.clone(), &id).dir()).await?;
    println!("imported company `{id}` into {}", home.display());
    Ok(())
}

/// Handle the `openhuman` subcommand: build the launch request, reject
/// passthrough args in Desktop mode via [`OpenHumanLaunch::validate`]
/// (before the dry-run branch so `--dry-run -- --arg` reports the same error
/// as a real launch instead of printing an unlaunchable command), then either
/// print the preview or run to completion and exit with the child's code.
async fn run_openhuman(
    root: PathBuf,
    mode: ModeArg,
    release: bool,
    dry_run: bool,
    args: Vec<String>,
) -> Result<()> {
    let mut launch = match LaunchMode::from(mode) {
        LaunchMode::Core => OpenHumanLaunch::core(root),
        LaunchMode::Desktop => OpenHumanLaunch::desktop(root),
    }
    .with_args(args);
    if release {
        launch = launch.release();
    }

    // validate() rejects passthrough args in Desktop mode; run it before
    // the dry-run branch so `--dry-run -- --arg` reports the same error
    // as an actual launch instead of printing an unlaunchable command.
    launch.validate()?;

    if dry_run {
        println!("{}", launch.dry_run_preview());
        return Ok(());
    }

    let status = launch.run().await?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(feature = "openhuman")]
const WORKER_STACK_BYTES: usize = openhuman_core::core::runtime::AGENT_WORKER_STACK_BYTES;
#[cfg(not(feature = "openhuman"))]
const WORKER_STACK_BYTES: usize = 2 * 1024 * 1024;

#[cfg(feature = "openhuman")]
const MAX_BLOCKING_THREADS: usize = openhuman_core::core::runtime::MAX_BLOCKING_THREADS;
#[cfg(not(feature = "openhuman"))]
const MAX_BLOCKING_THREADS: usize = 512;

fn main() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_BYTES)
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .build()?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match Cli::parse().command {
        Some(Command::Serve {
            bind,
            openhuman_root,
            companies,
            home,
            discoverable,
        }) => {
            // `--home` > OPENCOMPANY_DATA_DIR > $HOME/.opencompany, then any
            // legacy doubled install is moved up before a single bundle is read.
            let home = resolve_home_migrated(home)?;
            // Exclusive ownership of the data root, held for the whole process.
            //
            // `docs/spec/runtime/storage.md` has always said the runtime journal
            // is single-writer and that two hosts sharing a home write over each
            // other; nothing enforced it. The common way to hit that is not
            // exotic — a `serve` left running in another terminal, then a second
            // one started against the same default `~/.opencompany`.
            //
            // Bound to a name that lives until `serve` returns. Binding it to
            // `_` would drop it immediately and release the root while this
            // process carried on writing, which is worse than not locking.
            let _home_lock = opencompany::store::lock::acquire(&home)?;
            // Materialize the canonical data-dir workspace layout and empty the
            // ephemeral `tmp/` scratch so nothing stale survives a restart. The
            // `[workspace]` section of `config.toml` (in the data dir) toggles
            // the tmp clear; absent config keeps the default (clear on startup).
            let data_root = opencompany::app::config::data_dir_from_env();
            // An explicit `--home` pointing away from the data root splits one
            // instance in two: bundles here, shared workspace there. Printed
            // rather than `warn!`d — the default `EnvFilter` drops warnings
            // unless RUST_LOG is set, so a `warn!` would be exactly as silent
            // as the bug it reports.
            if let Some(warning) = opencompany::store::home_divergence_warning(&home, &data_root) {
                eprintln!("opencompany: {warning}");
            }
            // Kept whole rather than mapped straight to `.workspace`: the
            // `bind` key is read off the same file further down (issue #425).
            let config_file = ConfigFile::load(&data_root)?;
            let workspace_cfg = config_file
                .as_ref()
                .map(|c| c.workspace.resolve())
                .unwrap_or_default();
            let layout = opencompany::store::DataLayout::new(&data_root);
            layout.ensure(workspace_cfg.clear_tmp_on_startup).await?;
            // Point the embedded OpenHuman runtime's durable agent journal at
            // the data volume and prove it is writable before a single agent
            // exists. Left alone the vendored runtime roots it under the user's
            // home directory, which in a tenant container is the read-only root
            // filesystem: every observation then fails to persist and the
            // vendored append worker reports it per event, forever, drowning
            // the container log (issue #446). An unwritable root aborts boot —
            // same precedent as a selected-but-unavailable storage backend
            // below — rather than serving an agent whose work is never
            // recorded. `println!` for the same reason as the divergence
            // warning above: the default `EnvFilter` would swallow an `info!`.
            let journal = opencompany::app::journal::prepare(&data_root).await?;
            println!("{}", journal.summary());
            // Register the same root as the vendored keyring's directory, so
            // credential storage cannot resolve to `$HOME` — or, further down
            // the same fallback chain, to `/tmp` at no log level (issue #451).
            // The export above already steers it there in practice, but only
            // because nothing has touched the keyring yet; the resolved value is
            // cached in a `OnceLock`, so the guarantee rests on startup ordering
            // nobody is checking. Registering says it outright.
            //
            // Here, and not later: this runs before any company runtime, agent
            // harness or HTTP listener exists, so the pin cannot lose a race to
            // a first keyring touch. `println!` for the same reason as the lines
            // around it — the default `EnvFilter` would swallow an `info!`, and
            // an operator needs to be able to see where the credentials live.
            #[cfg(feature = "openhuman")]
            println!(
                "{}",
                opencompany::app::journal::pin_keyring(&journal).summary()
            );
            // Tag every request the embedded openhuman_core makes to the
            // TinyHumans backend as opencompany's, not the vendored
            // runtime's own `openhuman` default (issue #376). This must run
            // here — before any company runtime, agent harness or HTTP
            // listener exists — because core's `IntegrationClient` reads the
            // identity into its default headers AT CONSTRUCTION
            // (`harness/toolbelt.rs`, `harness/composio.rs`,
            // `harness/search.rs` each build one the first time a company
            // needs it), so a call after the first client already exists
            // would not retroactively re-tag it. Same startup-ordering
            // reasoning as the keyring pin directly above: say it here, once,
            // rather than leaving it implicit in which line happens to run
            // first.
            // The call itself lives in the library so a test can reach it —
            // this arm cannot be exercised from one.
            #[cfg(feature = "openhuman")]
            opencompany::product::install_into_embedded_core();
            // Soft disk-quota alerting. Hard enforcement is the container /
            // StorageClass layer's job (EFS access point, k8s ResourceQuota);
            // here we surface an operator-visible warning when a workspace
            // exceeds its configured `[workspace]` quota.
            if let Some(limit) = workspace_cfg.storage_quota_bytes {
                let used = layout.usage_bytes().await?;
                if used > limit {
                    tracing::warn!(
                        used_bytes = used,
                        quota_bytes = limit,
                        data_dir = %data_root.display(),
                        "workspace storage over quota — enforce hard limits at the container/StorageClass layer",
                    );
                }
            }
            if let Some(limit) = workspace_cfg.tmp_quota_bytes {
                let used = layout.tmp_bytes().await?;
                if used > limit {
                    tracing::warn!(
                        used_bytes = used,
                        quota_bytes = limit,
                        "workspace tmp/ scratch over quota",
                    );
                }
            }
            // tiny.place economy + public-card configuration resolved from the
            // environment (with built-in defaults); the a2a routes and boot
            // going-public flow read these off `AppConfig`.
            let tinyplace_api_url = std::env::var("TINYPLACE_API_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| opencompany::app::config::DEFAULT_TINYPLACE_API_URL.to_string());
            let public_url = std::env::var("OPENCOMPANY_PUBLIC_URL")
                .ok()
                .filter(|value| !value.trim().is_empty());
            // A friendly name for this instance, shown by a client that holds
            // several connections at once. Cosmetic only: nothing selects,
            // authorizes, or routes on it, and `/spec` is unauthenticated, so
            // an operator naming a host "prod-eu" is publishing that.
            let instance_name = std::env::var("OPENCOMPANY_INSTANCE_NAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(|value| value.chars().take(120).collect::<String>());
            // Shared-single-DB tenant identity. When set, company ids are
            // namespaced with this value so many tenants can share one logical
            // database without colliding on the `companies` unique index. Unset
            // (db-per-tenant / single-tenant) keeps every id derivation as-is.
            let tenant_namespace = std::env::var("OPENCOMPANY_TENANT_ID")
                .ok()
                .filter(|value| !value.trim().is_empty());
            // The address the platform records as this instance's creator. A
            // provisioned company's manifest names no admin, so without this
            // nobody is eligible to log in and there is no operator token to
            // send the first invite with (issue #321). Treated exactly like a
            // manifest `[users].admins` entry — a standing invite, not an
            // account. Unset (self-hosted, local `serve`) is a full no-op.
            let admin_email = std::env::var("OPENCOMPANY_ADMIN_EMAIL")
                .ok()
                .filter(|value| !value.trim().is_empty());
            // Hosted-brain credential, resolved with the same precedence the
            // harness uses (`harness_inference_from_env`) so `/spec`'s
            // `cycles_available` reflects whether cognition can actually run.
            let tinyhumans_credential = std::env::var("OPENCOMPANY_INFERENCE_KEY")
                .or_else(|_| std::env::var("TINYHUMANS_API_KEY"))
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(opencompany::ports::types::SecretValue);
            // Honor TINYHUMANS_API_URL (e.g. staging) — the config layer reads
            // it, but this manual AppConfig build otherwise falls to the prod
            // default, so a staging credential could never reach staging.
            let api_url = std::env::var("TINYHUMANS_API_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| AppConfig::default().api_url);
            // The listener address, across every layer that may name it. Until
            // issue #425 only the flag reached this struct, so the manager's
            // injected `OPENCOMPANY_BIND` (and any `config.toml` `bind`) moved
            // `doctor`'s report while the host kept serving on the default —
            // silently, because the host does start and does serve.
            // Install-wide MCP defaults (issue #527). Normalized here, at the
            // boundary that reads the file, so a rejected entry is named at boot
            // where somebody is looking — rather than silently thinning the list
            // on every company's first agent turn. A bad entry is a warning, not
            // a boot failure: these are additive convenience, and refusing to
            // start over one malformed default would turn a cosmetic packaging
            // error into an outage.
            //
            // Read before `resolve_serve_bind` below, which consumes
            // `config_file`.
            let default_mcp_servers = {
                let raw = config_file
                    .as_ref()
                    .map(|c| c.default_mcp_servers.clone())
                    .unwrap_or_default();
                let (kept, problems) = opencompany::company::mcp::normalize_default_servers(&raw);
                for problem in &problems {
                    tracing::warn!(target: "opencompany::config", "{problem}");
                }
                kept
            };
            // A host-wide sign-in mode, when the deployment names one. Read
            // before `resolve_serve_bind` consumes `config_file`. An unparseable
            // value aborts boot rather than silently falling back to email:
            // "the login screen you asked for is not the one you got" is not a
            // failure anyone would notice from a running host.
            let auth_mode_override = {
                use std::str::FromStr as _;
                let raw = std::env::var("OPENCOMPANY_AUTH_MODE")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .or_else(|| config_file.as_ref().and_then(|c| c.auth_mode.clone()));
                match raw {
                    Some(raw) => Some(opencompany::app::config::AuthMode::from_str(&raw)?),
                    None => None,
                }
            };
            // Read before `config_file` is consumed for `bind` just below.
            // Whether this data root has been through the first-run setup flow
            // (`server::setup`); absent — a fresh root, or one predating the
            // flow — leaves it false, which is what puts the console into the
            // wizard instead of a sign-in form for a host nobody configured.
            let setup_complete = config_file
                .as_ref()
                .is_some_and(|c| c.setup_completed_at.is_some());
            let (bind, bind_source) = opencompany::app::config::resolve_serve_bind(
                bind,
                &ProcessEnv,
                config_file.and_then(|c| c.bind),
            );
            let state = AppState::new(AppConfig {
                bind,
                default_mcp_servers,
                openhuman_root,
                api_url,
                tinyplace_api_url,
                public_url,
                instance_name,
                tenant_namespace,
                admin_email,
                auth_mode_override,
                tinyhumans_credential,
                // Issue #553: the workspace's enforced byte limits, read from
                // the same `[workspace]` section as the soft disk quotas above
                // and handed to every company's builder below.
                workspace_quota: workspace_cfg.quota,
                workspace_git_enabled: workspace_cfg.git_enabled,
                ..AppConfig::default()
            })
            .with_cors(opencompany::server::cors::CorsConfig::from_env()?)
            .with_home(home.clone())
            // `setup_complete` above, and every `config.toml` read/write the
            // first-run setup flow does, resolve against `data_root` — not
            // `home`, which an explicit `--home` can point elsewhere (see
            // `home_divergence_warning`). Recording it here is what lets
            // `server::setup` resolve the same file rather than its own
            // `state.home()`.
            .with_config_root(data_root.clone())
            .with_setup_complete(setup_complete)
            .with_quota(
                env_usize("OPENCOMPANY_MAX_COMPANIES"),
                env_usize("OPENCOMPANY_MAX_COMPANIES_PER_TENANT"),
            );
            let mut state = attach_hub_identity(state);
            // Storage backend selection: fs (default) needs nothing; sqlite and
            // mongodb are opened once here and injected into every company's
            // builder. A selected-but-unavailable backend aborts boot rather
            // than silently falling back to fs.
            let storage_settings = opencompany::store::StorageSettings::from_env()?;
            if let Some(handles) =
                opencompany::store::open_storage(&storage_settings, &home).await?
            {
                // Shared-database platform mode: restore the durable company →
                // tenant map so ownership survives restarts. In shared-single-DB
                // mode the `owners` collection holds every tenant's rows; hydrate
                // only this tenant's own mappings so the in-memory map never
                // leaks other tenants' companies (which are unaddressable here
                // regardless, since the registry only holds locally-loaded ones).
                if let Some(ownership) = &handles.ownership {
                    let self_tenant = state.config().tenant_namespace.clone();
                    for (id, tenant) in ownership.owners().await? {
                        match &self_tenant {
                            // Compare in canonical (bare-slug) form so a row
                            // persisted as `tenant:acme` still hydrates under the
                            // workload's bare `acme` namespace, and vice versa.
                            Some(me)
                                if opencompany::app::canonical_tenant(&tenant)
                                    != opencompany::app::canonical_tenant(me) =>
                            {
                                continue;
                            }
                            _ => state.set_owner(id, tenant),
                        }
                    }
                }
                state = state
                    .with_stores(handles)
                    .with_storage_kind(storage_settings.kind);
                println!("storage backend: {:?}", storage_settings.kind);
            }
            // Memory engine overlay (`OPENCOMPANY_MEMORY`): swaps just the
            // memory + context ports onto a dedicated engine on top of the base
            // backend. A selected-but-unavailable engine aborts boot, same as
            // the storage backend.
            if let Some(overlay) = opencompany::store::open_memory_overlay(&storage_settings)? {
                state = state.with_memory_overlay(overlay);
                // `as_str`, not `{:?}`: the enum's Debug name is `Tinycortex`
                // while `/spec` and the docs call that engine `embedded`. An
                // operator comparing a boot log against a status response should
                // not have to work out that those are the same thing.
                println!(
                    "memory backend: {}",
                    storage_settings.memory_backend.as_str()
                );
            }
            // Platform (multi-tenant) auth: either credential enables the
            // provisioning/lifecycle surface. Without both the prosumer operator
            // path stays in force. A signing secret this build cannot verify
            // aborts boot here rather than silently degrading — same precedent
            // as a selected-but-unavailable storage backend above.
            {
                use opencompany::server::platform_auth::{
                    PLATFORM_JWT_SECRET_ENV, PLATFORM_TOKEN_ENV, configure,
                };
                if let Some((platform_auth, mode)) = configure(
                    std::env::var(PLATFORM_TOKEN_ENV).ok(),
                    std::env::var(PLATFORM_JWT_SECRET_ENV).ok(),
                )? {
                    state = state.with_platform_auth(platform_auth);
                    // The mode only — never the secret, or anything derived
                    // from it.
                    println!("platform auth: {mode}");
                }
            }
            // Outbound webhooks: a URL wires the HTTP sink under `webhooks`;
            // without the feature the request is warned and dropped.
            if let Some(url) = std::env::var("OPENCOMPANY_WEBHOOK_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
                && let Some(webhook) = webhook_config(url)
            {
                state = state.with_webhook(webhook);
            }
            // Connection seams: a real DNS resolver (feature `dns`) enables custom
            // domain verification; a real SMTP sender (feature `smtp`) enables the
            // test send and outbound mail. Absent the features these stay `None`
            // and the surfaces degrade to "not wired yet" (404).
            state = state.with_connections(connections_runtime()?);
            // The repo-level shared skill library (`skills/`) sits beside the
            // `companies/` dir; derive it from the first loaded company's source
            // dir so the `skillRegistry` query resolves the committed library.
            if let Some(skills_root) = companies.first().and_then(|path| {
                let dir = company_source_dir(path);
                dir.parent()
                    .and_then(|companies_dir| companies_dir.parent())
                    .map(|repo_root| repo_root.join("skills"))
            }) {
                state = state.with_skills_root(skills_root);
            }
            // Issue #290: with every builder input above now resolved, this host
            // can rebuild a company's runtime in place. Wired BEFORE the
            // companies register, so the very first `PUT …/inference` on a
            // freshly booted tenant already has a rebuilder to reach for.
            state = state.with_rebuilder(Arc::new(BootRebuilder));
            // Schedulers stop cleanly when this is notified (Ctrl-C below).
            let shutdown = Arc::new(Notify::new());
            let mut scheduler_handles = Vec::new();
            for dir in &companies {
                let (id, name, schedules) =
                    register_company(&state, &home, dir, discoverable).await?;
                let visibility = if discoverable {
                    " [discoverable: public]"
                } else {
                    ""
                };
                if let Some(handle) = spawn_scheduler(&state, &id, &schedules, &shutdown) {
                    scheduler_handles.push(handle);
                    println!(
                        "registered company `{id}` ({name}) from {} with {} schedule(s){visibility}",
                        dir.display(),
                        schedules.len()
                    );
                } else {
                    println!(
                        "registered company `{id}` ({name}) from {}{visibility}",
                        dir.display()
                    );
                }
                spawn_mailbox_poller(&state, &id, &shutdown, &mut scheduler_handles);
                spawn_telegram_poller(&state, &id, &shutdown, &mut scheduler_handles);
            }
            if companies.is_empty() {
                // Nothing was named on the command line, so adopt whatever this
                // data root already holds. A company can reach a root without
                // ever being named on a command line — the first-run setup flow
                // (`server::setup`) seeds one — and without this an operator who
                // completed setup, was told to restart for their settings to
                // take effect, and did, came back to an empty host with their
                // company sitting unread on disk.
                //
                // Adopting creates nothing: an empty root still starts empty,
                // rather than inventing the starter company that only the
                // packaged desktop's `bootstrap_companies` seeds.
                let adopted = opencompany::desktop::adopt_companies(&state).await?;
                for (id, manifest) in &adopted {
                    let slug = id.as_ref();
                    if let Some(handle) =
                        spawn_scheduler(&state, slug, &manifest.schedules, &shutdown)
                    {
                        scheduler_handles.push(handle);
                    }
                    spawn_mailbox_poller(&state, slug, &shutdown, &mut scheduler_handles);
                    spawn_telegram_poller(&state, slug, &shutdown, &mut scheduler_handles);
                    println!(
                        "adopted company `{slug}` ({}) from {}",
                        manifest.company.name,
                        home.display()
                    );
                }
                if adopted.is_empty() {
                    println!("serving with no companies; pass --company <dir> to load one");
                }
            }

            // One workflow scheduler for the whole process, started even with no
            // companies loaded: it re-reads the registry each minute, so a
            // company registered later is picked up without a restart.
            scheduler_handles.push(spawn_workflow_scheduler(&state, &shutdown));

            // And one maintenance ticker, for the same reason and started the
            // same way (issue #971). This is the only place approvals, grants
            // and fire claims are retired, and it covers a company registered
            // after boot — which the per-company scheduler spawn above does not.
            scheduler_handles.push(spawn_maintenance_ticker(&state, &shutdown));

            // Stop the schedulers on a termination signal so background cycle
            // work halts with the process (lifecycle shutdown).
            //
            // `SIGTERM` as well as `Ctrl-C` since issue #986: a hosted tenant is
            // only ever asked to stop by `SIGTERM`, so listening for `SIGINT`
            // alone meant the schedulers kept firing new cycles right through
            // the shutdown the server was busy draining. Both listeners resolve
            // on the same signal — tokio delivers it to every registration — so
            // this and `serve`'s drain start together rather than in sequence.
            {
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    opencompany::server::shutdown::signal().await;
                    shutdown.notify_waiters();
                });
            }

            // Name the address *and* the layer that chose it. An operator who
            // set `OPENCOMPANY_BIND` and finds the host elsewhere can read the
            // disagreement off this one line instead of inferring it from a
            // refused connection. `println!` for the same reason as the lines
            // above — the default `EnvFilter` would swallow an `info!`.
            println!("listening on {} (from {bind_source})", state.config().bind);
            opencompany::server::serve(state).await
        }
        Some(Command::Spec { openhuman_root }) => {
            let state = AppState::new(AppConfig {
                openhuman_root,
                ..AppConfig::default()
            });
            println!("{}", serde_json::to_string_pretty(&state.spec()).unwrap());
            Ok(())
        }
        Some(Command::Check { path }) => {
            if opencompany::company::run_check(&path) {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        Some(Command::Doctor { company, json }) => {
            let env = ProcessEnv;
            // Locate config.toml under the resolved data dir (env override or
            // the default `$HOME/.opencompany`).
            let config_dir = opencompany::app::config::data_dir_from_env();
            let config_toml = ConfigFile::load(&config_dir)?;
            let manifest = match &company {
                Some(dir) => CompanyManifest::from_path(dir)?,
                None => toml::from_str("[company]\nname = \"opencompany\"\n")
                    .expect("synthetic manifest is valid"),
            };
            let (cfg, prov) = resolve(&env, config_toml.as_ref(), &manifest)?;
            let report = doctor::report(&cfg, &prov);
            if json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                print!("{}", report.to_text());
            }
            Ok(())
        }
        Some(Command::Export {
            company,
            out,
            include_secrets,
            home,
        }) => run_export(company, out, include_secrets, home).await,
        Some(Command::Import { path, home }) => run_import(path, home).await,
        Some(Command::OpenHuman {
            root,
            mode,
            release,
            dry_run,
            args,
        }) => run_openhuman(root, mode, release, dry_run, args).await,
        None => {
            println!("opencompany {}", opencompany::VERSION);
            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[tokio::test]
    async fn openhuman_desktop_dry_run_rejects_passthrough_args() {
        // The handler validates before the dry-run branch, so `--dry-run`
        // with passthrough args in Desktop mode reports the same 400 as a
        // real launch instead of printing a command that could never run.
        let tmp = std::env::temp_dir().join(format!(
            "oc-bin-oh-desktop-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let err = run_openhuman(
            tmp.clone(),
            ModeArg::Desktop,
            false,
            true,
            vec!["--flag".into()],
        )
        .await
        .unwrap_err();
        assert!(matches!(
            &err,
            opencompany::OpenCompanyError::OpenHuman { code: 400, .. }
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_home_flag_is_taken_verbatim() {
        // The binary owns no home policy of its own: it delegates to
        // `store::resolve_home`, whose precedence chain (flag >
        // OPENCOMPANY_DATA_DIR > $HOME/.opencompany) is covered in
        // `src/store/paths.rs`. This only pins the wiring.
        assert_eq!(
            opencompany::store::resolve_home(Some(PathBuf::from("/flag"))).unwrap(),
            PathBuf::from("/flag")
        );
    }

    #[test]
    fn every_home_resolving_command_migrates_the_legacy_nest() {
        // `serve`, `export`, and `import` all resolve through
        // `resolve_home_migrated`, so an un-migrated install's first
        // post-upgrade command is not the one that finds no bundles.
        let home = std::env::temp_dir().join(format!(
            "oc-bin-migrate-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let nested = home.join("companies/companies/acme");
        std::fs::create_dir_all(&nested).expect("legacy bundle");
        std::fs::write(nested.join("company.toml"), "[company]\n").expect("manifest");

        let resolved = resolve_home_migrated(Some(home.clone())).expect("resolves and migrates");

        assert_eq!(resolved, home);
        assert!(home.join("companies/acme/company.toml").exists());
        assert!(!home.join("companies/companies").exists());
        // The wiring is what is under test, but the bundle path it produces is
        // the point of the whole change.
        assert_eq!(
            opencompany::store::Bundle::new(resolved, &CompanyId::new("acme"))
                .dir()
                .to_path_buf(),
            home.join("companies/acme")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn no_command_resolves_a_home_without_migrating_it() {
        // The test above pins the helper; this pins that the helper is the only
        // door. A command that called `store::resolve_home` directly would read
        // an un-migrated install and find no companies — and no runtime test
        // would catch it, because the defect is a call that never happens. The
        // needle is split so this assertion does not match its own source line.
        let needle = concat!("store::", "resolve_home(");
        let source = include_str!("opencompany.rs");
        let production = source.split("\nmod test {").next().unwrap_or(source);

        let direct: Vec<&str> = production
            .lines()
            .filter(|line| line.contains(needle))
            .collect();

        assert_eq!(
            direct.len(),
            1,
            "`resolve_home` belongs to `resolve_home_migrated` alone; found {direct:?}"
        );
        let (before_helper, _) = production
            .split_once("fn resolve_home_migrated")
            .expect("the helper is declared");
        assert!(
            !before_helper.contains(needle),
            "the one call must be the one inside `resolve_home_migrated`"
        );
    }

    #[tokio::test]
    async fn export_and_import_migrate_before_they_read() {
        // Both commands run against an install whose first post-upgrade command
        // may well be one of them, so neither may be the one that finds no
        // bundles. Their results are irrelevant here — the migration happens
        // before either touches a path, which is the whole point.
        for command in ["export", "import"] {
            let home = std::env::temp_dir().join(format!(
                "oc-bin-{command}-migrate-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&home);
            let nested = home.join("companies/companies/acme");
            std::fs::create_dir_all(&nested).expect("legacy bundle");
            std::fs::write(nested.join("company.toml"), "[company]\n").expect("manifest");

            match command {
                "export" => {
                    let out = home.join("out");
                    let _ =
                        run_export("acme".to_string(), Some(out), false, Some(home.clone())).await;
                }
                _ => {
                    let _ = import_from_dir(&home.join("nothing-here"), Some(home.clone())).await;
                }
            }

            assert!(
                home.join("companies/acme/company.toml").exists(),
                "`{command}` resolved a home without migrating it"
            );
            assert!(!home.join("companies/companies").exists());
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[tokio::test]
    async fn register_company_loads_manifest_and_registers() {
        let home = std::env::temp_dir().join(format!("oc-bin-{}", std::process::id()));
        let state = AppState::new(AppConfig::default());
        let dir = std::path::Path::new("companies/agentic_law_firm");

        let (id, name, _schedules) = register_company(&state, &home, dir, false).await.unwrap();

        assert_eq!(name, "Agentic Law Firm");
        assert_eq!(id, "agentic-law-firm");
        assert_eq!(state.registry().list().len(), 1);
        let runtime = state.registry().sole().expect("sole company");

        // The serve path records the source dir and seeds the workspace from
        // `companies/<name>/workspace/**` on first boot.
        assert_eq!(runtime.source_dir(), Some(dir));
        assert!(
            !runtime.workspace().is_empty(runtime.id()).await.unwrap(),
            "workspace seeded from the company source dir"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn company_source_dir_normalizes_manifest_file_to_its_directory() {
        let dir = std::path::Path::new("companies/agentic_law_firm");
        // A directory argument is returned unchanged.
        assert_eq!(company_source_dir(dir), dir);
        // A manifest-file argument resolves to its parent company directory, so
        // the serve-path `workspace`/`skills`/`workflows` lookups stay correct.
        assert_eq!(company_source_dir(&dir.join("company.toml")), dir);
    }

    #[tokio::test]
    async fn register_company_accepts_a_manifest_file_path() {
        let home = std::env::temp_dir().join(format!("oc-bin-file-{}", std::process::id()));
        let state = AppState::new(AppConfig::default());
        // `--company` also accepts the manifest file inside the company dir.
        let file = std::path::Path::new("companies/agentic_law_firm/company.toml");

        let (_id, name, _schedules) = register_company(&state, &home, file, false).await.unwrap();

        assert_eq!(name, "Agentic Law Firm");
        let runtime = state.registry().sole().expect("sole company");
        // The recorded source dir is the company directory, not `company.toml`.
        assert_eq!(
            runtime.source_dir(),
            Some(std::path::Path::new("companies/agentic_law_firm"))
        );
        assert!(
            !runtime.workspace().is_empty(runtime.id()).await.unwrap(),
            "workspace still seeds when --company is a manifest file"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Issue #85: launching from a template *directory* derives the provenance
    /// from that directory's basename — `source_id` and `path` both the slug,
    /// `version` faithfully `None` (the serve path exposes no template version).
    /// Distinct from the builder-injection test in `runtime::builder`: this
    /// exercises the serve-path derivation in `register_company` itself. Per
    /// 8b40fa7 the stamped `path` is the basename, never the absolute host path.
    #[tokio::test]
    async fn register_company_stamps_provenance_from_directory() {
        let home = std::env::temp_dir().join(format!("oc-prov-dir-{}", std::process::id()));
        let state = AppState::new(AppConfig::default());
        let dir = std::path::Path::new("companies/agentic_law_firm");

        register_company(&state, &home, dir, false).await.unwrap();

        let runtime = state.registry().sole().expect("sole company");
        let record = runtime
            .store()
            .load(runtime.id())
            .await
            .unwrap()
            .expect("persisted record");
        let provenance = record
            .template_provenance
            .expect("a directory launch stamps template provenance");
        assert_eq!(provenance.source_id, "agentic_law_firm");
        assert_eq!(provenance.version, None, "serve path records no version");
        assert_eq!(
            provenance.path.as_deref(),
            Some("agentic_law_firm"),
            "path is the template basename, not the absolute host path"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Issue #85: launching from a `company.toml` *file* path normalizes to its
    /// parent company directory before deriving provenance, so the stamped
    /// provenance matches the directory launch exactly (basename `source_id` +
    /// `path`, `None` version). Guards the file-path input shape of the
    /// serve-path derivation.
    #[tokio::test]
    async fn register_company_stamps_provenance_from_manifest_file() {
        let home = std::env::temp_dir().join(format!("oc-prov-file-{}", std::process::id()));
        let state = AppState::new(AppConfig::default());
        let file = std::path::Path::new("companies/agentic_law_firm/company.toml");

        register_company(&state, &home, file, false).await.unwrap();

        let runtime = state.registry().sole().expect("sole company");
        let record = runtime
            .store()
            .load(runtime.id())
            .await
            .unwrap()
            .expect("persisted record");
        let provenance = record
            .template_provenance
            .expect("a manifest-file launch stamps provenance from the parent dir");
        assert_eq!(provenance.source_id, "agentic_law_firm");
        assert_eq!(provenance.version, None);
        assert_eq!(
            provenance.path.as_deref(),
            Some("agentic_law_firm"),
            "path is the template basename, not the absolute host path"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
