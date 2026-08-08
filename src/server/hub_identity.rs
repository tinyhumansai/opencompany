//! Learning which ecosystem address is behind a platform token.
//!
//! Sign-in happens **here**, on the company's own console. The browser is sent
//! to the hub's OAuth start pointed back at this origin, the hub completes the
//! provider dance and redirects back carrying a platform JWT, and this module
//! turns that JWT into one address.
//!
//! ## Why this tenant does not verify the token
//!
//! It cannot. The signing secret belongs to the hub, and handing it to every
//! tenant so each could check a signature would also let every tenant *mint* a
//! token for any user in the ecosystem — the plainest possible way to lose the
//! isolation the hosting layer exists to provide.
//!
//! So the tenant does not verify the token; it *uses* it. It presents the token
//! to the hub's own `GET /auth/me`, exactly as the dashboard would. If the hub
//! answers with an identity, that **is** the proof: only the hub can say who a
//! token it signed belongs to, and a forged or expired one gets a 401 there.
//! No shared secret, no minted code, no second round trip to invent.
//!
//! ## What this costs, stated plainly
//!
//! The tenant briefly holds a hub credential belonging to the person signing
//! in — one that carries their ecosystem privileges, not merely their identity.
//! That is a real delegation of trust to the tenant, and it is strictly more
//! than a single-use, slug-bound code would have handed over. It is accepted
//! here because the console *is* the company: a person signing in to their own
//! company's host is already trusting that host with everything the company
//! holds. What this module owes in return is discipline — the token is used
//! once, for one request, and is never persisted, never logged, and never
//! echoed back in an error.
//!
//! Authorization is emphatically **not** delegated. The hub says who they are;
//! this company's own roster says whether they may in — the same
//! `eligibility` → `upsert_from_eligibility` → `mint_session` path a magic link
//! answers to.
//!
//! ## Shape
//!
//! The [`HubIdentityExchange`] trait and its offline [`MockHubIdentityExchange`]
//! compile in the default build, so the whole route — eligibility, session
//! minting, every refusal — is exercised without linking a network crate. Only
//! [`HttpHubIdentityExchange`] is gated behind the existing `tinyhumans`
//! feature, which is already what "this instance talks to the hub about its
//! credential's owner" means. It does not earn a feature flag of its own.

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use async_trait::async_trait;

use crate::Result;

/// An identity provider the hub can complete a sign-in with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubProvider {
    /// The hub's provider slug, as it appears in `GET /auth/{id}/login`.
    pub id: &'static str,
    /// What the console calls it on the button.
    pub label: &'static str,
}

/// The providers offered on the console's sign-in screen, in OpenHuman's order.
///
/// Deliberately the same three, in the same sequence, as OpenHuman's welcome
/// screen. Someone who signs in to the desktop app with GitHub should not have
/// to hunt for it here, and an ecosystem that offers a different identity set
/// per surface teaches people that the account is per-surface too.
///
/// Discord is registered at the hub and supports login, but OpenHuman hides it
/// on welcome (`showOnWelcome: false`), so it stays hidden here too.
pub const HUB_PROVIDERS: &[HubProvider] = &[
    HubProvider {
        id: "google",
        label: "Google",
    },
    HubProvider {
        id: "github",
        label: "GitHub",
    },
    HubProvider {
        id: "twitter",
        label: "X",
    },
];

/// Builds the hub URL that starts a sign-in and comes back to `redirect_uri`.
///
/// `redirect_uri` is round-tripped by the hub verbatim, with `token=…&key=auth`
/// appended, so it must arrive percent-encoded as a single query value —
/// unescaped it would end at the console origin's own `?` and the hub would
/// read the console's `company=` as one of its own parameters.
///
/// ## The hosted blocker
///
/// The hub accepts `redirectUri` only when it passes an RFC 8252 **loopback**
/// check (`http://` on `127.0.0.1`, `localhost`, or `[::1]`). A console on
/// `127.0.0.1:<port>` satisfies that today, which is why the whole flow can be
/// built and demonstrated locally against the route exactly as it ships.
///
/// A hosted console on an `https://<slug>.<domain>` origin satisfies neither
/// that check nor the named `redirect ∈ {app,dashboard,admin}` targets, so the
/// hub will answer `400` until it learns to allowlist tenant console origins.
/// Nothing here needs to change when it does: the origin this builds on comes
/// from [`AppConfig::host_base_url`](crate::AppConfig::host_base_url), so
/// hosted is `OPENCOMPANY_PUBLIC_URL=https://…` and no code edit.
pub fn login_start_url(api_url: &str, provider: &str, redirect_uri: &str) -> String {
    format!(
        "{}/auth/{}/login?redirectUri={}",
        api_url.trim_end_matches('/'),
        provider,
        percent_encode(redirect_uri),
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

/// Who a platform token stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubIdentity {
    /// The ecosystem address the hub resolved the token to. Not normalized
    /// here — the route does that before it goes anywhere near a user lookup.
    pub email: String,
}

/// The hub, scoped to the one question this tenant may ask it: whose token is
/// this?
#[async_trait]
pub trait HubIdentityExchange: Send + Sync {
    /// Resolves `token` to the address that holds it.
    ///
    /// Implementations must treat `token` as a live credential: never log it,
    /// never store it, and never include it in an error. It is the caller's
    /// only proof of identity and would be replayable by anyone who read it.
    async fn identify(&self, token: &str) -> Result<HubIdentity>;
}

/// An in-memory [`HubIdentityExchange`] for offline tests and local demos.
///
/// Non-destructive, unlike a single-use code: a platform token is a bearer
/// credential with a lifetime, so presenting it twice legitimately succeeds
/// twice. A mock that expired it on first use would make the route look
/// stricter than it is.
#[derive(Debug, Default)]
pub struct MockHubIdentityExchange {
    tokens: StdMutex<HashMap<String, String>>,
    /// A forced transport failure, standing in for "the hub is not answering".
    unreachable: bool,
}

impl MockHubIdentityExchange {
    /// An exchange that knows no tokens; every lookup is rejected.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds one live token and the address it resolves to.
    pub fn with_token(self, token: &str, email: &str) -> Self {
        self.tokens
            .lock()
            .expect("mock poisoned")
            .insert(token.to_string(), email.to_string());
        self
    }

    /// An exchange whose hub cannot be reached at all.
    ///
    /// Distinct from an unknown token on purpose: one is a dead credential the
    /// caller should re-earn by signing in again, the other is an outage the
    /// caller can do nothing about, and the route must not tell someone to
    /// click again when clicking again cannot work.
    pub fn unreachable() -> Self {
        Self {
            unreachable: true,
            ..Self::default()
        }
    }
}

/// The error the hub returns for a token that is forged, expired, or revoked.
///
/// One shape for all three, mirroring the hub: `GET /auth/me` answers 401 for
/// every one of them, and inventing a finer distinction here would be this
/// tenant guessing at a fact only the hub holds.
fn rejected() -> crate::error::OpenCompanyError {
    crate::error::OpenCompanyError::TinyHumans {
        code: "http_401".to_string(),
        message: "The hub did not recognize that sign-in".to_string(),
    }
}

#[async_trait]
impl HubIdentityExchange for MockHubIdentityExchange {
    async fn identify(&self, token: &str) -> Result<HubIdentity> {
        if self.unreachable {
            return Err(crate::error::OpenCompanyError::TinyHumans {
                code: "unreachable".to_string(),
                message: "connection refused".to_string(),
            });
        }
        self.tokens
            .lock()
            .expect("mock poisoned")
            .get(token)
            .map(|email| HubIdentity {
                email: email.clone(),
            })
            .ok_or_else(rejected)
    }
}

/// The real HTTP exchange, compiled only under the `tinyhumans` feature.
#[cfg(feature = "tinyhumans")]
pub use http::HttpHubIdentityExchange;

#[cfg(feature = "tinyhumans")]
mod http {
    use super::{HubIdentity, HubIdentityExchange};
    use crate::Result;
    use crate::error::OpenCompanyError;
    use async_trait::async_trait;
    use serde::Deserialize;

    /// The hub's envelope for `GET /auth/me`.
    #[derive(Debug, Deserialize)]
    struct MeResponse {
        data: MeData,
    }

    #[derive(Debug, Deserialize)]
    struct MeData {
        email: String,
    }

    /// A [`HubIdentityExchange`] backed by `GET {api_url}/auth/me`.
    ///
    /// Deliberately the hub's *existing* session route rather than anything
    /// built for tenants. There is no new endpoint to secure, no new token type
    /// to expire, and no way for this call to learn more than the person who
    /// presented the token already knows about themselves.
    pub struct HttpHubIdentityExchange {
        api_url: String,
        http: reqwest::Client,
    }

    impl HttpHubIdentityExchange {
        /// Builds an exchange against `api_url`.
        pub fn new(api_url: impl Into<String>) -> Self {
            Self {
                // Trailing slashes would produce `//auth/me`.
                api_url: api_url.into().trim_end_matches('/').to_string(),
                http: reqwest::Client::new(),
            }
        }

        fn err(context: &str, e: impl std::fmt::Display) -> OpenCompanyError {
            OpenCompanyError::TinyHumans {
                code: context.to_string(),
                message: e.to_string(),
            }
        }
    }

    #[async_trait]
    impl HubIdentityExchange for HttpHubIdentityExchange {
        async fn identify(&self, token: &str) -> Result<HubIdentity> {
            let url = format!("{}/auth/me", self.api_url);
            let (product_header_name, product_header_value) =
                crate::product::product_identity_header();
            let resp = self
                .http
                .get(&url)
                .bearer_auth(token)
                // `api_url` is the TinyHumans backend itself (`AppConfig::api_url`,
                // defaulting to `crate::app::config::DEFAULT_API_URL`), so this is
                // our own backend and is tagged like every other call we make to
                // it. A bespoke `reqwest::Client`, not one built through
                // `openhuman_core`'s `IntegrationClient`, so it never inherits the
                // header `set_product_identity` attaches — see `crate::product`.
                .header(product_header_name, product_header_value)
                .send()
                .await
                .map_err(|e| Self::err("unreachable", e))?;

            let status = resp.status();
            if !status.is_success() {
                // The hub's own message is safe to surface — it describes the
                // token's standing, never the person's. The token itself is
                // never echoed, and `reqwest`'s error Display would not carry
                // it either (the bearer lives in a header, not the URL).
                let detail = resp.text().await.unwrap_or_default();
                return Err(Self::err(
                    &format!("http_{}", status.as_u16()),
                    truncate(&detail, 200),
                ));
            }

            let parsed: MeResponse = resp.json().await.map_err(|e| Self::err("decode", e))?;
            Ok(HubIdentity {
                email: parsed.data.email,
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
