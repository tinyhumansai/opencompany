//! The agent-facing bridge for hosting (TinyHosts): the tools that put a
//! company's workspace on a real hosting provider.
//!
//! The count is deliberately not written down here. [`hosting_tools`] returns
//! `Account::tools()` wholesale, so the set is whatever the vendored OpenHuman
//! ships and it moves with the pin — a number in this sentence is a number that
//! goes stale silently. `every_wired_hosting_tool_is_declared` below is what
//! holds the set to something this crate has actually classified.
//!
//! # Why the credential is per company and resolved late
//!
//! Two companies on one host deploy to two different hosting accounts, so a
//! process-wide key could only ever be wrong for one of them. The provider slug,
//! API key and team are read from **that company's**
//! [`SecretStore`](crate::ports::SecretStore), written by the console's Hosting
//! settings. No environment variable is consulted — unlike a standalone
//! OpenHuman, where `TINYHOSTS_VERCEL_TOKEN` is the ordinary source, a
//! multi-tenant host must never let one company's deployment ride on an ambient
//! credential.
//!
//! Resolution happens at roster-build time and is folded into the harness
//! fingerprint, so an operator who sets or rotates the key in the console gets
//! it on the next turn rather than after a restart — the same contract Chargebee
//! and Composio have.
//!
//! # Fail closed
//!
//! Tools are wired only when a company **explicitly** grants `hosting` *and* a
//! credential resolves. A catch-all `*` does not confer it: these tools publish
//! a company's files to the public internet and can provision a database the
//! company is billed for, so they are opted into by name rather than ridden in
//! on a wildcard set for file and shell tools.
//!
//! # What crosses this boundary
//!
//! Only the workspace directory an agent names, and only downward: OpenHuman's
//! `hosting` domain refuses an absolute path or a `..` escape before a single
//! byte is read. Nothing comes back that is secret — a provisioned database's
//! connection string is injected by the provider into the site's environment,
//! and this process learns the variable's *name* and never its value.

use std::sync::Arc;

use crate::company::hosting::{API_KEY_SECRET, DEFAULT_PROVIDER, PROVIDER_SECRET, TEAM_SECRET};
use crate::ports::SecretStore;
use crate::ports::types::CompanyId;

/// One company's resolved hosting connection.
#[derive(Clone)]
pub struct TenantHosting {
    provider: String,
    api_key: String,
    team: Option<String>,
}

/// Prints the provider and team, never the key.
impl std::fmt::Debug for TenantHosting {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TenantHosting")
            .field("provider", &self.provider)
            .field("team", &self.team)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl TenantHosting {
    /// Resolves a company's hosting credentials from its secret store.
    ///
    /// `Ok(None)` when no API key is stored: the key is the only required half.
    /// A missing provider falls back to [`DEFAULT_PROVIDER`], which is the
    /// migration value for a row written before the field existed; a missing
    /// team means the personal account.
    ///
    /// A store **read failure** is an `Err`, not `Ok(None)`. Collapsing the two
    /// would make an unhealthy secret store indistinguishable from "no
    /// credential configured", and the caller's response differs completely:
    /// absence should wire no tools, while a transient read error should keep
    /// the connection it already had. See `HarnessPool::resolve_hosting`.
    ///
    /// # Errors
    ///
    /// Returns an error when the secret store cannot be read.
    pub async fn resolve(
        secrets: &Arc<dyn SecretStore>,
        company: &CompanyId,
    ) -> crate::error::Result<Option<TenantHosting>> {
        let read = async |key: &str| -> crate::error::Result<Option<String>> {
            Ok(secrets
                .get(company, key)
                .await?
                .map(|value| value.0.trim().to_string())
                .filter(|value| !value.is_empty()))
        };

        let Some(api_key) = read(API_KEY_SECRET).await? else {
            return Ok(None);
        };

        Ok(Some(TenantHosting {
            provider: read(PROVIDER_SECRET)
                .await?
                .unwrap_or_else(|| DEFAULT_PROVIDER.to_string()),
            api_key,
            team: read(TEAM_SECRET).await?,
        }))
    }

    /// The provider this company deploys to. Never the key.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// The team scope, when the company set one.
    pub fn team(&self) -> Option<&str> {
        self.team.as_deref()
    }

    /// A stable hash of the connection, for the roster staleness check.
    ///
    /// Covers the key as well as the provider and team, so rotating a key with
    /// everything else unchanged still rebuilds the roster — otherwise a rotated
    /// credential would keep authenticating with the old one until a restart.
    pub fn fingerprint(config: &Option<TenantHosting>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match config {
            None => 0u8.hash(&mut hasher),
            Some(hosting) => {
                1u8.hash(&mut hasher);
                hosting.provider.hash(&mut hasher);
                hosting.api_key.hash(&mut hasher);
                hosting.team.hash(&mut hasher);
            }
        }
        hasher.finish()
    }
}

#[cfg(feature = "openhuman")]
pub use live::hosting_tools;

#[cfg(feature = "openhuman")]
mod live {
    use super::TenantHosting;

    use std::path::PathBuf;

    use openhuman_core::openhuman::tools::Tool;

    /// The hosting tools for one company, deploying out of `workspace`.
    ///
    /// `workspace` is the agent's own sandbox directory, the same one the file
    /// tools are pinned to, so an agent can only ever deploy what it can already
    /// read. An unusable credential — an unknown provider slug, a key the client
    /// cannot be built for — wires nothing and warns rather than failing the
    /// build: an agent that cannot deploy is a degraded agent, not a broken
    /// company.
    pub fn hosting_tools(config: &TenantHosting, workspace: PathBuf) -> Vec<Box<dyn Tool>> {
        match openhuman_core::openhuman::hosting::Account::connect(
            &config.provider,
            &config.api_key,
            config.team(),
            workspace,
        ) {
            Ok(account) => account.tools(),
            Err(error) => {
                log::warn!(
                    "[hosting] the stored hosting credential is unusable ({error}); hosting tools NOT wired"
                );
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_renders_the_api_key() {
        let hosting = TenantHosting {
            provider: "vercel".to_string(),
            api_key: "vc_live_SUPERSECRET".to_string(),
            team: Some("team_abc".to_string()),
        };

        let rendered = format!("{hosting:?}");

        assert!(!rendered.contains("SUPERSECRET"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(rendered.contains("team_abc"), "{rendered}");
    }

    #[test]
    fn the_fingerprint_moves_when_any_half_of_the_credential_does() {
        let base = TenantHosting {
            provider: "vercel".to_string(),
            api_key: "one".to_string(),
            team: None,
        };
        let rotated = TenantHosting {
            api_key: "two".to_string(),
            ..base.clone()
        };
        let scoped = TenantHosting {
            team: Some("team_abc".to_string()),
            ..base.clone()
        };

        let of = |value: TenantHosting| TenantHosting::fingerprint(&Some(value));

        assert_ne!(of(base.clone()), TenantHosting::fingerprint(&None));
        // A rotated key with the same provider must rebuild the roster.
        assert_ne!(of(base.clone()), of(rotated));
        assert_ne!(of(base.clone()), of(scoped));
        assert_eq!(of(base.clone()), of(base));
    }

    #[test]
    fn a_connection_reports_its_provider_and_team() {
        let hosting = TenantHosting {
            provider: "vercel".to_string(),
            api_key: "key".to_string(),
            team: Some("team_abc".to_string()),
        };

        assert_eq!(hosting.provider(), "vercel");
        assert_eq!(hosting.team(), Some("team_abc"));
    }

    /// **Every tool this bridge wires is classified in
    /// [`crate::policy::consequence`].** Issue #913 is the reason this exists.
    ///
    /// `every_registered_tool_is_declared` in [`crate::harness::built_in`] is
    /// the general version of this check and it cannot see these tools: it
    /// enumerates a belt built by `build_agent`, and hosting is wired only when
    /// a company has a *stored credential*, which no unit-test company has. So
    /// the hosting belt was never covered by construction, and the standing
    /// check was a hardcoded list of six names in `consequence.rs`.
    ///
    /// A hardcoded list fails when a row is deleted. It cannot fail when the
    /// vendor pin adds a tool — and [`hosting_tools`] returns
    /// `Account::tools()` wholesale, so a pin bump wires new tools onto live
    /// agents with nothing in this repository mentioning them. That is what
    /// happened: `hosting_rollback`, `hosting_list_deployments` and
    /// `hosting_domain_status` went live undeclared. Undeclared is not inert —
    /// the two reads parked (an approval for a read) and `hosting_rollback`
    /// classified as `Other` rather than `Publish`, which is issue #1079's
    /// defect returning through the pin instead of through an edit.
    ///
    /// Enumerating the belt is the fix, because it is the only form of this
    /// check a pin bump cannot outrun.
    ///
    /// `Account::connect` builds a client and does no I/O, so the dummy
    /// credential below never leaves the process.
    #[cfg(feature = "openhuman")]
    #[test]
    fn every_wired_hosting_tool_is_declared() {
        let declared: std::collections::BTreeSet<&str> =
            crate::policy::consequence::declared_tools().collect();

        let config = TenantHosting {
            provider: DEFAULT_PROVIDER.to_string(),
            api_key: "not-a-real-key".to_string(),
            team: None,
        };
        let wired: Vec<String> = hosting_tools(&config, std::path::PathBuf::from("/tmp"))
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();

        // A vacuity guard: `hosting_tools` warns and returns an empty vec when
        // the credential is unusable, and an empty belt would pass the check
        // below while proving nothing.
        assert!(
            wired.contains(&"hosting_launch_site".to_string()),
            "the hosting belt built no tools, so this check proves nothing: {wired:?}"
        );

        let undeclared: Vec<&String> = wired
            .iter()
            .filter(|name| !declared.contains(name.as_str()))
            .collect();
        assert!(
            undeclared.is_empty(),
            "these hosting tools came in with the vendor pin and are wired onto live              agents, but nobody has said what they can reach — so the gate is guessing              from their names, and a `hosting_` prefix defeats the read heuristic:              {undeclared:?}. Add them to `crate::policy::consequence::DECLARED`."
        );
    }
}
