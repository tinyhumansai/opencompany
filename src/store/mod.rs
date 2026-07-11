//! Filesystem-backed persistence for the runtime's durable ports.
//!
//! Each company owns a [`Bundle`] directory (see [`paths`]) holding its
//! manifest, event log, ledger, memory, context, and secrets. The [`fs`]
//! module implements [`CompanyStore`](crate::ports::CompanyStore),
//! [`EventLog`](crate::ports::EventLog),
//! [`MemoryStore`](crate::ports::MemoryStore),
//! [`ContextStore`](crate::ports::ContextStore), and
//! [`SecretStore`](crate::ports::SecretStore) over that layout.

pub mod fs;
pub mod paths;

pub use fs::{FsCompanyStore, FsContextStore, FsEventLog, FsMemoryStore, FsSecretStore};
pub use paths::{Bundle, default_home};
