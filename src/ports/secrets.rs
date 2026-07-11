//! The [`SecretStore`] port: per-company secrets.

use async_trait::async_trait;

use crate::Result;
use crate::ports::types::{CompanyId, SecretValue};

/// Per-company secrets (channel credentials, GitHub token). Company A's
/// secrets MUST be invisible to company B.
#[async_trait]
pub trait SecretStore: Send + Sync {
    /// Reads a secret, or `None` if unset.
    async fn get(&self, company: &CompanyId, key: &str) -> Result<Option<SecretValue>>;
    /// Writes a secret.
    async fn set(&self, company: &CompanyId, key: &str, value: SecretValue) -> Result<()>;
}
