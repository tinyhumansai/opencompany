use std::{path::PathBuf, process::ExitStatus};

use serde::Serialize;
use tokio::process::Command;

use crate::{OpenCompanyError, Result};

/// OpenHuman target to launch from a sibling checkout.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum LaunchMode {
    /// Launch the core Rust binary.
    Core,
    /// Launch the Tauri desktop host.
    Desktop,
}

/// Describes an OpenHuman launch request.
#[derive(Clone, Debug)]
pub struct OpenHumanLaunch {
    root: PathBuf,
    mode: LaunchMode,
    args: Vec<String>,
}

impl OpenHumanLaunch {
    /// Creates a launch request for the OpenHuman core binary.
    pub fn core(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            mode: LaunchMode::Core,
            args: Vec::new(),
        }
    }

    /// Creates a launch request for the OpenHuman desktop host.
    pub fn desktop(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            mode: LaunchMode::Desktop,
            args: Vec::new(),
        }
    }

    /// Adds passthrough arguments.
    pub fn with_args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.args = args.into_iter().collect();
        self
    }

    /// Returns the cargo command preview without spawning the process.
    pub fn command_preview(&self) -> Vec<String> {
        let mut command = vec!["cargo".to_string()];
        match self.mode {
            LaunchMode::Core => {
                command.extend([
                    "run".to_string(),
                    "--manifest-path".to_string(),
                    self.root.join("Cargo.toml").display().to_string(),
                    "--bin".to_string(),
                    "openhuman-core".to_string(),
                    "--".to_string(),
                ]);
            }
            LaunchMode::Desktop => {
                command.extend([
                    "run".to_string(),
                    "--manifest-path".to_string(),
                    self.root
                        .join("app/src-tauri/Cargo.toml")
                        .display()
                        .to_string(),
                    "--bin".to_string(),
                    "OpenHuman".to_string(),
                    "--".to_string(),
                ]);
            }
        }
        command.extend(self.args.clone());
        command
    }

    /// Starts OpenHuman and waits for it to exit.
    pub async fn run(self) -> Result<ExitStatus> {
        if !self.root.exists() {
            return Err(OpenCompanyError::MissingOpenHumanRoot(self.root));
        }

        let preview = self.command_preview();
        let status = Command::new(&preview[0])
            .args(&preview[1..])
            .status()
            .await?;
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_preview_points_to_openhuman_core() {
        let preview = OpenHumanLaunch::core("vendor/openhuman")
            .with_args(["status".to_string()])
            .command_preview();

        assert!(preview.contains(&"openhuman-core".to_string()));
        assert!(preview.contains(&"vendor/openhuman/Cargo.toml".to_string()));
        assert_eq!(preview.last(), Some(&"status".to_string()));
    }

    #[test]
    fn desktop_preview_points_to_tauri_host() {
        let preview = OpenHumanLaunch::desktop("vendor/openhuman").command_preview();

        assert!(preview.contains(&"OpenHuman".to_string()));
        assert!(
            preview
                .iter()
                .any(|part| part.ends_with("app/src-tauri/Cargo.toml"))
        );
    }
}
