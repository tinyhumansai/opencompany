pub mod config;
pub mod doctor;
mod types;

pub use config::{BrainMode, ConfigProvenance, RuntimeConfig, resolve};
pub use doctor::{DoctorReport, report as doctor_report};
pub use types::{AppConfig, AppSpec, AppState};
