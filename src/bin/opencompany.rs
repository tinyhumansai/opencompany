use std::path::PathBuf;

use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use opencompany::{
    AppConfig, AppState, CompanyManifest, Result,
    openhuman::{LaunchMode, OpenHumanLaunch},
    runtime::RuntimeBuilder,
};

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
) -> Result<(String, String)> {
    let manifest = CompanyManifest::from_path(dir)?;
    let name = manifest.company.name.clone();
    let runtime = RuntimeBuilder::new(home.to_path_buf(), manifest)
        .build()
        .await?;
    let id = runtime.id().as_ref().to_string();
    state
        .registry()
        .insert(runtime.id().clone(), Arc::new(runtime));
    Ok((id, name))
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
        }) => {
            let state = AppState::new(AppConfig {
                bind,
                openhuman_root,
                ..AppConfig::default()
            });
            let home = home.unwrap_or_else(default_home);
            for dir in &companies {
                let (id, name) = register_company(&state, &home, dir).await?;
                println!("registered company `{id}` ({name}) from {}", dir.display());
            }
            if companies.is_empty() {
                println!("serving with no companies; pass --company <dir> to load one");
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

        let (id, name) = register_company(&state, &home, dir).await.unwrap();

        assert_eq!(name, "Agentic Law Firm");
        assert_eq!(id, "agentic-law-firm");
        assert_eq!(state.registry().list().len(), 1);
        assert!(state.registry().sole().is_some());
        std::fs::remove_dir_all(&home).ok();
    }
}
