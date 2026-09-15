//! The agent-facing bridge for PayPal (issue #789): two read tools over
//! [`crate::paypal::api`].
//!
//! Same shape as [`crate::harness::chargebee`] — per-company credentials from
//! that company's [`SecretStore`], resolved at roster-build time, wired only on
//! an explicit `paypal` grant and only when a credential resolves.
//!
//! # Both tools are read-only, and that is the whole surface
//!
//! #789 lists `send_payment` as optional and requires a scoping decision before
//! implementation, so nothing here moves money. Both tools are therefore
//! [`PermissionLevel::ReadOnly`] and never park: asking what the balance is
//! should not need an approval click, and there is no write to guard.

use std::sync::Arc;

use crate::company::paypal::{
    CLIENT_ID_SECRET, CLIENT_SECRET_SECRET, ENVIRONMENT_SECRET, PaypalEnvironment,
};
use crate::ports::SecretStore;
use crate::ports::types::CompanyId;

/// One company's resolved PayPal connection.
#[derive(Clone, Debug)]
pub struct TenantPaypal {
    #[cfg_attr(not(feature = "paypal"), allow(dead_code))]
    config: crate::paypal::PaypalConfig,
}

impl TenantPaypal {
    /// Resolves a company's PayPal credentials from its secret store.
    ///
    /// `Ok(None)` unless BOTH halves are present: a client id with no secret
    /// cannot obtain a token, and half a credential should wire no tools rather
    /// than tools that fail on first use.
    ///
    /// A store **read failure** is an `Err`, not `Ok(None)` — see
    /// [`crate::harness::chargebee::TenantChargebee::resolve`] for why the two
    /// must stay distinguishable.
    pub async fn resolve(
        secrets: &Arc<dyn SecretStore>,
        company: &CompanyId,
    ) -> crate::error::Result<Option<Self>> {
        let read = async |key: &str| -> crate::error::Result<Option<String>> {
            Ok(secrets
                .get(company, key)
                .await?
                .map(|value| value.0.trim().to_string())
                .filter(|value| !value.is_empty()))
        };
        let (Some(client_id), Some(client_secret)) = (
            read(CLIENT_ID_SECRET).await?,
            read(CLIENT_SECRET_SECRET).await?,
        ) else {
            return Ok(None);
        };
        // An unset environment is sandbox, matching `PaypalEnvironment::parse`:
        // the safe default is reading fake money, never moving real money.
        let environment = read(ENVIRONMENT_SECRET)
            .await?
            .map(|raw| PaypalEnvironment::parse(&raw))
            .unwrap_or_default();

        Ok(Some(Self {
            config: crate::paypal::PaypalConfig {
                client_id,
                client_secret,
                environment,
            },
        }))
    }

    /// Which PayPal environment this company is pointed at. Never the credential.
    pub fn environment(&self) -> PaypalEnvironment {
        self.config.environment
    }

    /// A stable hash of the connection, for the roster staleness check.
    ///
    /// Covers the environment as well as both halves of the credential: moving
    /// a company from sandbox to live with the same keys must rebuild, or its
    /// agents keep reading the wrong world's balance until a restart.
    pub fn fingerprint(config: &Option<TenantPaypal>) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match config {
            None => 0u8.hash(&mut hasher),
            Some(c) => {
                1u8.hash(&mut hasher);
                c.config.client_id.hash(&mut hasher);
                c.config.client_secret.hash(&mut hasher);
                c.config.environment.as_str().hash(&mut hasher);
            }
        }
        hasher.finish()
    }
}

#[cfg(feature = "paypal")]
pub use live::paypal_tools;

#[cfg(feature = "paypal")]
mod live {
    use super::*;

    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use crate::paypal::api;
    use crate::paypal::client::PaypalClient;

    use oh::tools::traits::{PermissionLevel, Tool, ToolResult};
    use openhuman_core as oh;

    /// Builds the per-company PayPal tools over a resolved connection.
    pub fn paypal_tools(config: &TenantPaypal) -> Vec<Box<dyn Tool>> {
        let config = Arc::new(config.clone());
        vec![
            Box::new(WalletBalanceTool(Arc::clone(&config))),
            Box::new(ListTransactionsTool(config)),
        ]
    }

    /// Builds the client for a call about to be made.
    fn client(config: &TenantPaypal) -> crate::error::Result<PaypalClient> {
        PaypalClient::new(config.config.clone())
    }

    /// Renders a result, or the failure as text the agent can act on.
    fn render<T: serde::Serialize>(what: &str, outcome: crate::error::Result<T>) -> ToolResult {
        match outcome {
            Ok(value) => match serde_json::to_string_pretty(&value) {
                Ok(text) => ToolResult::success(text),
                Err(e) => {
                    ToolResult::error(format!("{what} succeeded but could not be rendered: {e}"))
                }
            },
            Err(e) => ToolResult::error(format!("{what} failed: {e}")),
        }
    }

    pub struct WalletBalanceTool(Arc<TenantPaypal>);

    #[async_trait]
    impl Tool for WalletBalanceTool {
        fn name(&self) -> &str {
            "paypal_get_wallet_balance"
        }

        fn description(&self) -> &str {
            "Fetch the current PayPal account balance, per currency. Returns the available and \
             withheld amounts as exact decimal strings — report them verbatim rather than \
             rounding or recomputing."
        }

        fn parameters_schema(&self) -> Value {
            json!({"type": "object", "additionalProperties": false, "properties": {}})
        }

        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::ReadOnly
        }

        async fn execute(&self, _args: Value) -> Result<ToolResult> {
            let client = match client(&self.0) {
                Ok(client) => client,
                Err(e) => return Ok(ToolResult::error(format!("paypal client: {e}"))),
            };
            tracing::info!(
                environment = self.0.environment().as_str(),
                "[paypal] get_wallet_balance"
            );
            Ok(render(
                "paypal_get_wallet_balance",
                api::get_wallet_balance(&client).await,
            ))
        }
    }

    pub struct ListTransactionsTool(Arc<TenantPaypal>);

    #[async_trait]
    impl Tool for ListTransactionsTool {
        fn name(&self) -> &str {
            "paypal_list_transactions"
        }

        fn description(&self) -> &str {
            "List PayPal transactions between two dates. PayPal publishes on a delay of up to 3 \
             hours, so a window ENDING today is fine but one STARTING today usually has no data \
             and is rejected — start at least one day back. The window must span no more than 31 \
             days. To answer 'was I paid recently?', ask for the last 7 days rather than today."
        }

        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "required": ["start_date", "end_date"],
                "additionalProperties": false,
                "properties": {
                    "start_date": {
                        "type": "string",
                        "description": "ISO 8601, e.g. 2026-08-01T00:00:00Z. At most 31 days before end_date."
                    },
                    "end_date": {
                        "type": "string",
                        "description": "ISO 8601, e.g. 2026-08-13T23:59:59Z."
                    },
                    "page_size": {"type": "integer", "minimum": 1, "maximum": 500}
                }
            })
        }

        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::ReadOnly
        }

        async fn execute(&self, args: Value) -> Result<ToolResult> {
            let client = match client(&self.0) {
                Ok(client) => client,
                Err(e) => return Ok(ToolResult::error(format!("paypal client: {e}"))),
            };
            let start = args.get("start_date").and_then(Value::as_str).unwrap_or("");
            let end = args.get("end_date").and_then(Value::as_str).unwrap_or("");
            let page_size = args.get("page_size").and_then(Value::as_i64);
            Ok(render(
                "paypal_list_transactions",
                api::list_transactions(&client, start, end, page_size).await,
            ))
        }
    }
}

#[cfg(all(test, feature = "paypal"))]
mod tests {
    use super::*;
    use crate::ports::types::SecretValue;
    use crate::store::fs::FsSecretStore;

    async fn secrets_with(entries: &[(&str, &str)]) -> (Arc<dyn SecretStore>, CompanyId) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store: Arc<dyn SecretStore> = Arc::new(FsSecretStore::new(dir.keep()));
        let company = CompanyId::new("acme");
        for (key, value) in entries {
            store
                .set(&company, key, SecretValue(value.to_string()))
                .await
                .expect("set");
        }
        (store, company)
    }

    #[tokio::test]
    async fn both_halves_of_the_credential_are_required() {
        let (id_only, company) = secrets_with(&[(CLIENT_ID_SECRET, "AY_id")]).await;
        assert!(
            TenantPaypal::resolve(&id_only, &company)
                .await
                .expect("a readable store is not an error")
                .is_none()
        );

        let (secret_only, company) = secrets_with(&[(CLIENT_SECRET_SECRET, "EL_secret")]).await;
        assert!(
            TenantPaypal::resolve(&secret_only, &company)
                .await
                .expect("a readable store is not an error")
                .is_none()
        );

        let (neither, company) = secrets_with(&[]).await;
        assert!(
            TenantPaypal::resolve(&neither, &company)
                .await
                .expect("a readable store is not an error")
                .is_none()
        );
    }

    #[test]
    fn the_fingerprint_moves_on_the_credential_and_on_the_environment() {
        // Symmetric with the Chargebee side, plus the environment: moving a
        // company from sandbox to live with the same keys must rebuild, or its
        // agents keep reading the wrong world's balance.
        let of = |id: &str, secret: &str, env: PaypalEnvironment| {
            TenantPaypal::fingerprint(&Some(TenantPaypal {
                config: crate::paypal::PaypalConfig {
                    client_id: id.to_string(),
                    client_secret: secret.to_string(),
                    environment: env,
                },
            }))
        };

        let base = of("AY_id", "EL_secret", PaypalEnvironment::Sandbox);
        assert_eq!(
            base,
            of("AY_id", "EL_secret", PaypalEnvironment::Sandbox),
            "stable for one config"
        );
        assert_ne!(
            base,
            of("AY_other", "EL_secret", PaypalEnvironment::Sandbox)
        );
        assert_ne!(base, of("AY_id", "EL_rotated", PaypalEnvironment::Sandbox));
        assert_ne!(
            base,
            of("AY_id", "EL_secret", PaypalEnvironment::Live),
            "the environment must count on its own"
        );
        assert_ne!(base, TenantPaypal::fingerprint(&None));
    }

    #[tokio::test]
    async fn an_unset_environment_resolves_to_sandbox() {
        // The safe default, and the one that matters most: an operator who never
        // touched the environment field must not be reading a live balance.
        let (store, company) = secrets_with(&[
            (CLIENT_ID_SECRET, "AY_id"),
            (CLIENT_SECRET_SECRET, "EL_secret"),
        ])
        .await;
        let resolved = TenantPaypal::resolve(&store, &company)
            .await
            .expect("the store reads")
            .expect("both halves present");
        assert_eq!(resolved.environment(), PaypalEnvironment::Sandbox);
    }

    #[tokio::test]
    async fn live_is_reached_only_by_saying_live() {
        let (store, company) = secrets_with(&[
            (CLIENT_ID_SECRET, "AY_id"),
            (CLIENT_SECRET_SECRET, "EL_secret"),
            (ENVIRONMENT_SECRET, "live"),
        ])
        .await;
        let resolved = TenantPaypal::resolve(&store, &company)
            .await
            .expect("the store reads")
            .expect("resolves");
        assert_eq!(resolved.environment(), PaypalEnvironment::Live);

        // And a near-miss does not.
        let (typo, company) = secrets_with(&[
            (CLIENT_ID_SECRET, "AY_id"),
            (CLIENT_SECRET_SECRET, "EL_secret"),
            (ENVIRONMENT_SECRET, "Live-ish"),
        ])
        .await;
        let resolved = TenantPaypal::resolve(&typo, &company)
            .await
            .expect("the store reads")
            .expect("resolves");
        assert_eq!(resolved.environment(), PaypalEnvironment::Sandbox);
    }

    #[tokio::test]
    async fn the_credential_never_reaches_a_debug_rendering() {
        let (store, company) = secrets_with(&[
            (CLIENT_ID_SECRET, "AY_id"),
            (CLIENT_SECRET_SECRET, "EL_secret"),
        ])
        .await;
        let resolved = TenantPaypal::resolve(&store, &company)
            .await
            .expect("the store reads")
            .expect("resolves");
        let rendered = format!("{resolved:?}");
        assert!(!rendered.contains("EL_secret"), "{rendered}");
        assert!(!rendered.contains("AY_id"), "{rendered}");
    }

    #[test]
    fn both_tools_are_read_only() {
        use oh::tools::traits::PermissionLevel;
        use openhuman_core as oh;

        let config = TenantPaypal {
            config: crate::paypal::PaypalConfig {
                client_id: "AY_id".to_string(),
                client_secret: "EL_secret".to_string(),
                environment: PaypalEnvironment::Sandbox,
            },
        };
        let tools = live::paypal_tools(&config);
        assert_eq!(tools.len(), 2);
        for tool in &tools {
            // Nothing here moves money (see the module docs), so nothing parks.
            assert_eq!(
                tool.permission_level(),
                PermissionLevel::ReadOnly,
                "{}",
                tool.name()
            );
        }
    }
}
