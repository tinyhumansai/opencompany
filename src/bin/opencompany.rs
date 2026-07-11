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
            let state = AppState::new(AppConfig {
                bind,
                openhuman_root,
                tinyplace_api_url,
                public_url,
                ..AppConfig::default()
            })
            .with_home(home.clone());
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
        let dir = std::path::Path::new("examples/agentic_law_firm");

        let (id, name, _schedules) = register_company(&state, &home, dir, false).await.unwrap();

        assert_eq!(name, "Agentic Law Firm");
        assert_eq!(id, "agentic-law-firm");
        assert_eq!(state.registry().list().len(), 1);
        assert!(state.registry().sole().is_some());
        std::fs::remove_dir_all(&home).ok();
    }
}
