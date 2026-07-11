//! The company kernel: assembling ports into a running company and driving the
//! cycle loop.
//!
//! - [`CompanyRuntime`] (defined in [`crate::company::runtime`]) is the wired
//!   assembly of the nine ports.
//! - [`RuntimeBuilder`] wires one from filesystem defaults.
//! - [`CycleRunner`] runs the serial drain → load → think → gate → persist loop.
//! - [`CompanyRegistry`] maps ids to running runtimes for both the single- and
//!   multi-company cases.
//! - The [`journal`] backs at-most-once effects and the durable approval queue.

pub mod builder;
pub mod channel;
pub mod cycle;
pub mod journal;
pub mod registry;
pub mod tools;
pub mod types;

pub use builder::{RuntimeBuilder, company_id_from_name};
pub use channel::{OPERATOR_CHANNEL, OperatorChannel};
pub use cycle::CycleRunner;
pub use registry::CompanyRegistry;
pub use tools::StubToolProvider;
pub use types::{ApprovalSummary, CompanyStatus, CycleReport};

// The assembly struct lives under `company/` to match the `ports.md` sketch
// (`src/company/runtime.rs`); re-export it here as the kernel's public surface.
pub use crate::company::runtime::CompanyRuntime;
