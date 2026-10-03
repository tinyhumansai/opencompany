//! The TinyHumans hub, scoped to the two things this host asks of it: a key
//! grant, and the billing standing of the key it was granted.
//!
//! ## What is deliberately not here
//!
//! This module once also turned a hub sign-in into a session on the company:
//! the browser was sent to the hub's OAuth start, came back carrying a platform
//! JWT, and `POST …/auth/hub` asked the hub whose it was. That is gone. A
//! company's sign-in is its own — magic link, password, wallet, or none — and
//! an ecosystem account is never a way into one. Sharing a login between two
//! products meant the weaker of the two decided the security of both, and it
//! meant a sign-in screen with three buttons that led, on every self-hosted
//! host, to a refusal on return. The hub is now only ever asked for a **key**,
//! which the person approves on the hub's own site, and never hands this host
//! a credential belonging to them.
//!
//! ## Shape
//!
//! The [`HubIdentityExchange`] trait and its offline [`MockHubIdentityExchange`]
//! compile in the default build, so every route that uses them — the key grant
//! and the billing read — is exercised without linking a network crate. Only
//! [`HttpHubIdentityExchange`] is gated behind the existing `tinyhumans`
//! feature, which is already what "this instance talks to the hub" means. It
//! does not earn a feature flag of its own.

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;

use crate::Result;

/// Builds the hub URL that starts a **key grant** and comes back to `callback_url`.
///
/// This asks the hub to mint this company a key. What the tenant ends up
/// holding is a one-time code that redeems to exactly one scoped API key, and
/// the secret that unlocks the code (`verifier`) never leaves this host — only
/// its SHA-256 goes out, as `challenge`. So a code captured anywhere along the
/// browser's path — history, a `Referer`, a shoulder — redeems nothing, and
/// this host never holds a credential belonging to the person who approved
/// the grant.
///
/// Shaped after OpenRouter's PKCE key exchange, which solves the same problem:
/// give an application a key without a human copying one between two sites.
pub fn key_grant_url(api_url: &str, callback_url: &str, challenge: &str, name: &str) -> String {
    format!(
        "{}/auth/key?{}",
        api_url.trim_end_matches('/'),
        key_grant_query(callback_url, challenge, name),
    )
}

/// The scopes a key grant asks the hub to put on the key it mints.
///
/// Only `connections` turns on anything the ask decides. Every grant carries
/// the set a human could have minted by hand whether it asks or not, and the
/// hub drops a name outside its grantable vocabulary rather than refusing the
/// grant — so this stays the one scope a console on this side would otherwise
/// never be handed, and the list is not a place to collect scopes nothing here
/// spends.
///
/// `connections` is what `/agent-integrations/composio/*` enforces. Without it
/// a key minted to a console that is not a provisioned tenant — a desktop
/// install, a developer's laptop — reaches managed Composio and is told it is
/// missing the scope, which is the whole of the third-party integrations
/// surface answering 403.
pub const KEY_GRANT_SCOPES: &[&str] = &["connections"];

/// The grant parameters as one query string, without the endpoint.
///
/// Split out because the same parameters are read by two pages: the API's
/// `GET /auth/key`, which acts on them, and the site's `/connect`, which shows
/// a person who is asking and lets them pick a provider before handing off to
/// exactly that endpoint ([`hub_account::connect_url`](crate::server::hub_account::connect_url)).
/// Building them once means the challenge cannot differ between the two — and
/// the same holds for [`KEY_GRANT_SCOPES`], which decides what the key may do
/// and is named on the consent screen the approver reads. A scope asked for on
/// one path and not the other would mint a key whose reach depended on which
/// page the browser happened to go through.
pub fn key_grant_query(callback_url: &str, challenge: &str, name: &str) -> String {
    format!(
        "callback_url={}&code_challenge={}&code_challenge_method=S256&name={}&scopes={}",
        percent_encode(callback_url),
        percent_encode(challenge),
        percent_encode(name),
        percent_encode(&KEY_GRANT_SCOPES.join(",")),
    )
}

/// Percent-encodes `value` for use as a single query-string value.
///
/// Hand-rolled rather than pulled in: the crate has no direct URL dependency in
/// the default build, and adding one to escape a handful of characters would
/// move `Cargo.lock` for no benefit. Keeps only the RFC 3986 unreserved set,
/// which is stricter than necessary and therefore cannot under-escape.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The hub, scoped to the two things this tenant may ask of it: redeem a key
/// grant, and read the standing of the key it holds.
///
/// The name predates the removal of the hub sign-in and is kept so the
/// `AppState` seam (`with_hub_identity`) and every caller stay put.
#[async_trait]
pub trait HubIdentityExchange: Send + Sync {
    /// Trades a one-time grant `code` and its `verifier` for a TinyHumans key.
    ///
    /// The other half of [`key_grant_url`]. Returns the plaintext key, which the
    /// hub emits exactly once and cannot reissue — so a caller that drops it has
    /// to send the person through the flow again, and must store it before doing
    /// anything else that can fail.
    ///
    /// Implementations must treat both arguments and the returned key as live
    /// credentials: never log them, never echo them into an error.
    async fn redeem_key_grant(&self, code: &str, verifier: &str) -> Result<String>;

    /// Reads the billing standing of the account a **key** belongs to.
    ///
    /// The one call in this trait that presents the company's own credential
    /// rather than a person's: it answers "how much is left, and on what plan",
    /// which is a property of the account the key spends from. It is a read and
    /// nothing else — topping up and changing a plan move money and stay on the
    /// hub's dashboard behind that person's own sign-in, which is why this has
    /// no counterpart that writes.
    ///
    /// Implementations must treat `key` as a live credential: never log it,
    /// never echo it into an error.
    async fn billing_summary(&self, key: &str) -> Result<BillingSummary>;
}

/// What an account's money is doing, as the console renders it.
///
/// A flattened copy of the hub's `GET /payments/summary` rather than a passthrough
/// of its JSON: the console is a different product on a different release
/// cadence, and a shape it merely forwards is one that changes under it without
/// anybody choosing to. Every field here is one the card actually draws.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingSummary {
    /// Everything spendable, promotional credit and top-up together, in USD.
    pub balance_usd: f64,
    /// The plan slug the account is on (`free`, `pro`, …).
    pub plan: String,
    /// Whether a paid subscription is live right now.
    pub active_subscription: bool,
    /// When the current plan lapses, as the hub stated it. `None` on a plan
    /// that does not expire.
    pub plan_expiry: Option<String>,
    /// Where a person tops the account up, on the hub that issued the key.
    pub top_up_url: Option<String>,
    /// Where a person changes the plan.
    pub manage_url: Option<String>,
}

/// An in-memory [`HubIdentityExchange`] for offline tests and local demos.
#[derive(Debug, Default)]
pub struct MockHubIdentityExchange {
    /// Grant codes and the `(verifier, key)` each redeems to.
    ///
    /// Single-use: a grant code really is spent on redemption at the hub, and
    /// a mock that let one be redeemed twice would make the route look safe to
    /// retry when it is not.
    grants: StdMutex<HashMap<String, (String, String)>>,
    /// A forced transport failure, standing in for "the hub is not answering".
    unreachable: bool,
    /// What [`HubIdentityExchange::billing_summary`] answers, per key.
    billing: StdMutex<HashMap<String, BillingSummary>>,
}

impl MockHubIdentityExchange {
    /// An exchange that knows no grants; every redemption is rejected.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds the billing standing one key reads back.
    pub fn with_billing(self, key: &str, summary: BillingSummary) -> Self {
        self.billing
            .lock()
            .expect("mock poisoned")
            .insert(key.to_string(), summary);
        self
    }

    /// Seeds one grant code, the verifier that unlocks it, and the key it mints.
    pub fn with_grant(self, code: &str, verifier: &str, key: &str) -> Self {
        self.grants
            .lock()
            .expect("mock poisoned")
            .insert(code.to_string(), (verifier.to_string(), key.to_string()));
        self
    }

    /// An exchange whose hub cannot be reached at all.
    ///
    /// Distinct from an unknown code on purpose: one is a dead credential the
    /// caller should re-earn by going through the grant again, the other is an
    /// outage the caller can do nothing about, and the route must not tell
    /// someone to click again when clicking again cannot work.
    pub fn unreachable() -> Self {
        Self {
            unreachable: true,
            ..Self::default()
        }
    }
}

/// The error the hub returns for a code or key that is forged, expired, or
/// revoked.
///
/// One shape for all three, mirroring the hub, which answers 401 for every one
/// of them; inventing a finer distinction here would be this tenant guessing
/// at a fact only the hub holds.
fn rejected() -> crate::error::OpenCompanyError {
    crate::error::OpenCompanyError::TinyHumans {
        code: "http_401".to_string(),
        message: "The hub did not recognize that credential".to_string(),
    }
}

#[async_trait]
impl HubIdentityExchange for MockHubIdentityExchange {
    async fn redeem_key_grant(&self, code: &str, verifier: &str) -> Result<String> {
        if self.unreachable {
            return Err(crate::error::OpenCompanyError::TinyHumans {
                code: "unreachable".to_string(),
                message: "connection refused".to_string(),
            });
        }
        // Removed before the verifier is checked, mirroring the hub: a wrong
        // verifier spends the code rather than leaving it up for another guess.
        let entry = self.grants.lock().expect("mock poisoned").remove(code);
        match entry {
            Some((expected, key)) if expected == verifier => Ok(key),
            _ => Err(rejected()),
        }
    }

    async fn billing_summary(&self, key: &str) -> Result<BillingSummary> {
        if self.unreachable {
            return Err(crate::error::OpenCompanyError::TinyHumans {
                code: "unreachable".to_string(),
                message: "connection refused".to_string(),
            });
        }
        // Non-destructive, unlike a grant code: reading a balance twice is the
        // same read twice.
        self.billing
            .lock()
            .expect("mock poisoned")
            .get(key)
            .cloned()
            .ok_or_else(rejected)
    }
}

#[cfg(test)]
#[path = "hub_identity_tests.rs"]
mod tests;

/// The real HTTP exchange, compiled only under the `tinyhumans` feature.
#[cfg(feature = "tinyhumans")]
pub use http::HttpHubIdentityExchange;

#[cfg(feature = "tinyhumans")]
mod http {
    use super::{BillingSummary, HubIdentityExchange};
    use crate::Result;
    use crate::error::OpenCompanyError;
    use async_trait::async_trait;
    use serde::Deserialize;

    /// The hub's envelope for `POST /auth/keys`.
    #[derive(Debug, Deserialize)]
    struct KeyData {
        /// The plaintext key. The hub emits it exactly once.
        key: String,
    }

    /// The `data` of the hub's `GET /payments/summary` envelope.
    #[derive(Debug, Deserialize)]
    struct SummaryData {
        #[serde(default)]
        credits: SummaryCredits,
        #[serde(default)]
        plan: SummaryPlan,
        #[serde(default)]
        links: SummaryLinks,
    }

    #[derive(Debug, Default, Deserialize)]
    struct SummaryCredits {
        #[serde(rename = "totalUsd", default)]
        total_usd: f64,
    }

    #[derive(Debug, Default, Deserialize)]
    struct SummaryPlan {
        #[serde(default)]
        plan: Option<String>,
        #[serde(rename = "hasActiveSubscription", default)]
        has_active_subscription: bool,
        #[serde(rename = "planExpiry", default)]
        plan_expiry: Option<String>,
    }

    #[derive(Debug, Default, Deserialize)]
    struct SummaryLinks {
        #[serde(rename = "topUpUrl", default)]
        top_up_url: Option<String>,
        #[serde(rename = "manageUrl", default)]
        manage_url: Option<String>,
    }

    /// A [`HubIdentityExchange`] backed by the hub's own `POST /auth/keys` and
    /// `GET /payments/summary`, through the TinyHumans SDK.
    pub struct HttpHubIdentityExchange {
        api_url: String,
    }

    impl HttpHubIdentityExchange {
        /// Builds an exchange against `api_url`.
        pub fn new(api_url: impl Into<String>) -> Self {
            Self {
                // Trailing slashes would produce `//auth/keys`.
                api_url: api_url.into().trim_end_matches('/').to_string(),
            }
        }

        /// An SDK client for one call, tagged with our product identity.
        ///
        /// `api_url` is the TinyHumans backend itself, so this is our own
        /// backend and is tagged like every other call we make to it — the SDK
        /// client is not built through `openhuman_core`'s `IntegrationClient`,
        /// so it never inherits the header `set_product_identity` attaches
        /// (see `crate::product`). Built per call because the bearer differs
        /// per call (none for a redemption, the redeemed key for a summary).
        fn sdk(&self, token: Option<&str>) -> tinyhumans_sdk::TinyHumansClient {
            let (name, value) = crate::product::product_identity_header();
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(name, reqwest::header::HeaderValue::from_static(value));
            tinyhumans_sdk::TinyHumansClient::new(&self.api_url)
                .with_token(token.map(str::to_string))
                .with_default_headers(headers)
        }

        /// Maps an SDK failure onto the crate error. A failure body is the
        /// hub's own words about the code's or key's standing; neither secret
        /// is ever in it (the key rides a header), and it is capped anyway.
        fn err(error: tinyhumans_sdk::Error) -> OpenCompanyError {
            let (code, message) = match error {
                tinyhumans_sdk::Error::Status { status, body } => {
                    (format!("http_{status}"), truncate(&body.to_string(), 200))
                }
                tinyhumans_sdk::Error::Http(e) => ("unreachable".to_string(), e.to_string()),
                other => ("decode".to_string(), other.to_string()),
            };
            OpenCompanyError::TinyHumans { code, message }
        }

        fn decode<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> Result<T> {
            serde_json::from_value(value).map_err(|e| OpenCompanyError::TinyHumans {
                code: "decode".to_string(),
                message: e.to_string(),
            })
        }
    }

    #[async_trait]
    impl HubIdentityExchange for HttpHubIdentityExchange {
        async fn redeem_key_grant(&self, code: &str, verifier: &str) -> Result<String> {
            // No bearer: the hub's redemption route is unauthenticated, and the
            // verifier is what authenticates it. That is the whole point of the
            // exchange — this host never holds a credential belonging to the
            // person who approved the grant.
            let request = tinyhumans_sdk::api::types::RedeemKeyGrantRequest {
                code: code.to_string(),
                code_verifier: verifier.to_string(),
            };
            let data = self
                .sdk(None)
                .auth()
                .redeem_key_grant(&request)
                .await
                .map_err(Self::err)?;
            Ok(Self::decode::<KeyData>(data.0)?.key)
        }

        async fn billing_summary(&self, key: &str) -> Result<BillingSummary> {
            let data = self
                .sdk(Some(key))
                .payments()
                .get_summary()
                .await
                .map_err(Self::err)?;
            let parsed: SummaryData = Self::decode(data.0)?;
            Ok(BillingSummary {
                balance_usd: parsed.credits.total_usd,
                // A hub that names no plan is on the free one — the field is
                // absent there rather than spelled out, and a card reading
                // "unknown" would be a worse answer than the true one.
                plan: parsed.plan.plan.unwrap_or_else(|| "free".to_string()),
                active_subscription: parsed.plan.has_active_subscription,
                plan_expiry: parsed.plan.plan_expiry,
                top_up_url: parsed.links.top_up_url,
                manage_url: parsed.links.manage_url,
            })
        }
    }

    /// Caps an error detail at `max` **characters**, never bytes: slicing a
    /// UTF-8 string by byte offset panics mid-codepoint, and an error path is
    /// the worst possible place to learn that.
    fn truncate(value: &str, max: usize) -> String {
        if value.chars().count() <= max {
            return value.to_string();
        }
        value.chars().take(max).collect()
    }
}
