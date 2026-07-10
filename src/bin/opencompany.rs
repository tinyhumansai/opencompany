use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use opencompany::{
    AppConfig, AppState, Result,
    openhuman::{LaunchMode, OpenHumanLaunch},
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
    },
    /// Print a JSON runtime specification.
    Spec {
        /// Optional OpenHuman checkout path to report.
        #[arg(long)]
        openhuman_root: Option<PathBuf>,
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    match Cli::parse().command {
        Some(Command::Serve {
            bind,
            openhuman_root,
        }) => {
            opencompany::server::serve(AppState::new(AppConfig {
                bind,
                openhuman_root,
            }))
            .await
        }
        Some(Command::Spec { openhuman_root }) => {
            let state = AppState::new(AppConfig {
                openhuman_root,
                ..AppConfig::default()
            });
            println!("{}", serde_json::to_string_pretty(&state.spec()).unwrap());
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
