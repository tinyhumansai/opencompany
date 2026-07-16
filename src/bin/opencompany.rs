use std::path::PathBuf;

use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use opencompany::company::Schedule;
use opencompany::runtime::{CompanyScheduler, SystemClock};
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
        /// `$HOME/.opencompany/companies`.
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
        /// `$HOME/.opencompany/companies`.
        #[arg(long)]
        home: Option<PathBuf>,
    },
    /// Import a company bundle (a `.tar` under `--features export`, else an
    /// unpacked bundle directory) into a home through the storage ports.
    Import {
        /// Bundle `.tar` or unpacked bundle directory to import.
        path: PathBuf,
        /// OpenCompany home to import into. Defaults to
        /// `$HOME/.opencompany/companies`.
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

/// The default OpenCompany home: `$HOME/.opencompany/companies`, falling back
/// to a relative path when `$HOME` is unset.
fn default_home() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".opencompany").join("companies"),
        None => PathBuf::from(".opencompany").join("companies"),
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
    let mut builder = attach_openhuman(RuntimeBuilder::new(home.to_path_buf(), manifest))
        .with_tinyplace_api_url(state.config().tinyplace_api_url.clone())
        .with_host_base_url(state.config().host_base_url());
    if let Some(stores) = state.stores() {
        builder = builder.with_stores(stores);
    }
    if discoverable {
        builder = builder.with_discoverable(true);
    }
    let runtime = builder.build().await?;
    let id = runtime.id().as_ref().to_string();
    state
        .registry()
        .insert(runtime.id().clone(), Arc::new(runtime));
    Ok((id, name, schedules))
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
        Ok(scheduler) => Some(scheduler.spawn(shutdown.clone())),
        Err(err) => {
            eprintln!("skipping scheduler for `{id}`: {err}");
            None
        }
    }
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
fn connections_runtime() -> opencompany::server::ops::ConnectionsRuntime {
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
    connections
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
    let home = home.unwrap_or_else(default_home);
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

    let home = home.unwrap_or_else(default_home);
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

    let home = home.unwrap_or_else(default_home);
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
            let home = home.unwrap_or_else(default_home);
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
            let mut state = AppState::new(AppConfig {
                bind,
                openhuman_root,
                tinyplace_api_url,
                public_url,
                ..AppConfig::default()
            })
            .with_home(home.clone())
            .with_quota(
                env_usize("OPENCOMPANY_MAX_COMPANIES"),
                env_usize("OPENCOMPANY_MAX_COMPANIES_PER_TENANT"),
            );
            // Storage backend selection: fs (default) needs nothing; sqlite and
            // mongodb are opened once here and injected into every company's
            // builder. A selected-but-unavailable backend aborts boot rather
            // than silently falling back to fs.
            let storage_settings = opencompany::store::StorageSettings::from_env()?;
            if let Some(handles) =
                opencompany::store::open_storage(&storage_settings, &home).await?
            {
                // Shared-database platform mode: restore the durable company →
                // tenant map so ownership survives restarts.
                if let Some(ownership) = &handles.ownership {
                    for (id, tenant) in ownership.owners().await? {
                        state.set_owner(id, tenant);
                    }
                }
                state = state.with_stores(handles);
                println!("storage backend: {:?}", storage_settings.kind);
            }
            // Platform (multi-tenant) auth: a shared platform token enables the
            // provisioning/lifecycle surface. Without it the prosumer operator
            // path stays in force. Real signed JWT is `platform-jwt`.
            if let Some(token) = std::env::var("OPENCOMPANY_PLATFORM_TOKEN")
                .ok()
                .filter(|v| !v.trim().is_empty())
            {
                use opencompany::server::platform_auth::{
                    PlatformAuthConfig, StaticPlatformVerifier,
                };
                state = state.with_platform_auth(PlatformAuthConfig::new(Arc::new(
                    StaticPlatformVerifier::new(token),
                )));
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
            state = state.with_connections(connections_runtime());
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
            }
            if companies.is_empty() {
                println!("serving with no companies; pass --company <dir> to load one");
            }

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
            let config_dir = match std::env::var_os("OPENCOMPANY_DATA_DIR") {
                Some(dir) => PathBuf::from(dir),
                None => match std::env::var_os("HOME") {
                    Some(home) => PathBuf::from(home).join(".opencompany"),
                    None => PathBuf::from(".opencompany"),
                },
            };
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
    fn default_home_lands_under_opencompany() {
        let home = default_home();
        assert!(home.ends_with("companies"));
        assert!(home.to_string_lossy().contains(".opencompany"));
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
        assert!(state.registry().sole().is_some());
        std::fs::remove_dir_all(&home).ok();
    }
}
