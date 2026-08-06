use std::path::PathBuf;

use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use opencompany::company::Schedule;
use opencompany::runtime::{CompanyScheduler, SystemClock, WorkflowScheduler};
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
        /// Address to bind.
        #[arg(long, default_value = "127.0.0.1:8080")]
        bind: String,
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
    /// Launch a sibling OpenHuman checkout through cargo.
    OpenHuman {
        /// OpenHuman checkout path.
        #[arg(long, default_value = "vendor/openhuman")]
        root: PathBuf,
        /// Launch target.
        #[arg(long, value_enum, default_value_t = ModeArg::Core)]
        mode: ModeArg,
        /// Print the cargo command without executing it.
        #[arg(long)]
        dry_run: bool,
        /// Arguments passed after `--` to the OpenHuman binary.
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
    .with_host_base_url(state.config().host_base_url())
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
        harness_inference_from_env, media_backend_from_env, search_backend_from_env,
    };

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

#[tokio::main]
async fn main() -> Result<()> {
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
            let workspace_cfg = ConfigFile::load(&data_root)?
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
            let state = AppState::new(AppConfig {
                bind,
                openhuman_root,
                api_url,
                tinyplace_api_url,
                public_url,
                tenant_namespace,
                admin_email,
                tinyhumans_credential,
                ..AppConfig::default()
            })
            .with_cors(opencompany::server::cors::CorsConfig::from_env()?)
            .with_home(home.clone())
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
                state = state.with_stores(handles);
                println!("storage backend: {:?}", storage_settings.kind);
            }
            // Memory engine overlay (`OPENCOMPANY_MEMORY`): swaps just the
            // memory + context ports onto a dedicated engine on top of the base
            // backend. A selected-but-unavailable engine aborts boot, same as
            // the storage backend.
            if let Some(overlay) = opencompany::store::open_memory_overlay(&storage_settings)? {
                state = state.with_memory_overlay(overlay);
                println!("memory backend: {:?}", storage_settings.memory_backend);
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
                println!("serving with no companies; pass --company <dir> to load one");
            }

            // One workflow scheduler for the whole process, started even with no
            // companies loaded: it re-reads the registry each minute, so a
            // company registered later is picked up without a restart.
            scheduler_handles.push(spawn_workflow_scheduler(&state, &shutdown));

            // Stop the schedulers on Ctrl-C so background cycle work halts with
            // the process (lifecycle shutdown).
            {
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    if tokio::signal::ctrl_c().await.is_ok() {
                        shutdown.notify_waiters();
                    }
                });
            }

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
            dry_run,
            args,
        }) => {
            let launch = match LaunchMode::from(mode) {
                LaunchMode::Core => OpenHumanLaunch::core(root),
                LaunchMode::Desktop => OpenHumanLaunch::desktop(root),
            }
            .with_args(args);

            if dry_run {
                println!("{}", launch.command_preview().join(" "));
                return Ok(());
            }

            let status = launch.run().await?;
            std::process::exit(status.code().unwrap_or(1));
        }
        None => {
            println!("opencompany {}", opencompany::VERSION);
            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

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
