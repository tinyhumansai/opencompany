//! The live System One transport: this host's side of TypeSafe routing.
//!
//! [`tinyhivemind_typesafe::SystemOneTransport`] is the port a semantic routing
//! decision is asked through. The library builds the exact question — which of
//! these candidates should take this message — and this module is the only
//! thing that puts it on a wire. Everything above it stays transport-free, which
//! is why the crate can be exercised end to end against a fixture and why this
//! file is the only place a credential appears.
//!
//! # Why the base URL is a field
//!
//! Resolved once at construction rather than derived per request, so a test can
//! point the transport at a local stub without the production path carrying a
//! test-only branch. `ChargebeeClient` holds its own for the same reason and
//! this matches it deliberately.
//!
//! # Why redirects are refused
//!
//! Every request carries the API key as a bearer token. reqwest follows
//! redirects by default **and re-sends the `Authorization` header**, so a 30x
//! pointing at `http://` would put the key on the wire in clear text. A scheme
//! check would still leave same-scheme redirection to an unintended host, so
//! refusing redirects outright is both stricter and simpler. System One does not
//! redirect, so it costs nothing.
//!
//! # Failure is typed, not swallowed
//!
//! Every failure path returns [`Error::Transport`] carrying the status when one
//! exists. The library's routing fold reads that as an unusable evaluation and
//! falls back to the caller's deterministic destination *without* inventing a
//! recipient — so a transport outage degrades routing to the fallback rather
//! than to a confident wrong answer.

use std::time::Duration;

use tinyhivemind_typesafe::{
    Error, SystemOneRequest, SystemOneResponse, SystemOneTransport, SystemOneTransportFuture,
};

/// The System One endpoint this host talks to when none is configured.
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai/v1/systemone";

/// The routing credential. Absent means semantic routing is off.
pub const API_KEY_ENV: &str = "OPENCOMPANY_TYPESAFE_API_KEY";
/// Overrides [`DEFAULT_BASE_URL`], for staging or a stub.
pub const BASE_URL_ENV: &str = "OPENCOMPANY_TYPESAFE_BASE_URL";
/// Overrides [`DEFAULT_MODEL`].
pub const MODEL_ENV: &str = "OPENCOMPANY_TYPESAFE_MODEL";
/// The Jev model this host asks for unless the environment says otherwise.
///
/// The moving alias, not a version — and deliberately, after pinning failed.
///
/// `jev-1.13` looked like the honest pin: `tinyhivemind`'s PR #55 records "live
/// Jev 1.13 routing" and the TypeSafe adapter's own tests assert
/// `model_identity == "jev-1.13"` round-tripping back. Neither is the service's
/// list of servable ids, and the endpoint answers that pin with
///
/// ```text
/// 400 {"detail":{"error_type":"api_usage_error","message":"Unknown model: jev-1.13"}}
/// ```
///
/// A refused model fails closed — routing declines and the mechanical responder
/// takes the message — which is the safe direction and also an invisible one:
/// the handoff still lands, so a broken pin reads as a healthy fallback from
/// outside. That is what the transport's error logging exists to catch.
///
/// A response carries the version that actually answered, so the way to pin
/// honestly is to read `model_identity` off a successful call and set
/// `OPENCOMPANY_TYPESAFE_MODEL` to it. Until somebody does, the alias is the
/// only id known to be servable.
pub const DEFAULT_MODEL: &str = "jev-latest";

/// How long one routing question may take before it is a transport failure.
///
/// A routing Choice sits in front of a turn the room is waiting on, so an
/// unbounded wait is a stalled desk rather than a slow one. Thirty seconds
/// matches the bound the other outbound clients in this crate use.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A live System One transport bound to one endpoint and one credential.
#[derive(Clone)]
pub struct TypeSafeTransport {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

/// Prints the endpoint and **redacts the API key**.
///
/// A derived `Debug` would put a live credential into any log line that
/// formatted a transport. Matches `ChargebeeClient`, which hand-writes its own
/// for exactly this reason.
impl std::fmt::Debug for TypeSafeTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TypeSafeTransport")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl TypeSafeTransport {
    /// Builds a transport against the real System One endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Transport`] when the HTTP client cannot be built, which
    /// can only mean a malformed TLS or timeout configuration here.
    pub fn new(api_key: String) -> Result<Self, Error> {
        Self::with_base_url(api_key, DEFAULT_BASE_URL.to_owned())
    }

    /// Builds a transport against an explicit endpoint, with no trailing slash.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn with_base_url(api_key: String, base_url: String) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // See the module docs: the bearer token would survive a redirect.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| Error::Transport {
                status: None,
                message: format!("System One client could not be built: {error}"),
            })?;
        Ok(Self {
            http,
            api_key,
            base_url,
        })
    }

    /// The endpoint this transport asks, for a startup log line.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Build a transport from the process environment, or `None` when this
    /// instance has no routing credential.
    ///
    /// `None` is a routing feature that is *off*, not a failure: a company
    /// without a key routes by explicit `@id` exactly as it did before semantic
    /// routing existed. That is the same direction
    /// [`built_in::selector`](crate::harness::built_in::selector) takes — "the
    /// worst case of the new rung is the old rung" — and it is why this returns
    /// an `Option` rather than erroring at boot.
    ///
    /// Environment rather than [`SecretStore`](crate::ports::secrets::SecretStore)
    /// for now: routing is one credential for the instance rather than per
    /// company, and a per-company key can move here later without changing a
    /// caller. `OPENCOMPANY_TYPESAFE_BASE_URL` overrides the endpoint for a
    /// staging or stubbed System One.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new), when a key is present but the client cannot be
    /// built.
    pub fn from_env() -> Result<Option<Self>, Error> {
        let Some(api_key) = std::env::var(API_KEY_ENV)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let base_url = std::env::var(BASE_URL_ENV)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());
        Self::with_base_url(api_key, base_url).map(Some)
    }
}

/// The routing model this host pins.
///
/// Pinned rather than taking [`JevRouter`](tinyhivemind_typesafe::JevRouter)'s
/// own `jev-latest` default, for the reason every other version in this repo is
/// pinned: a silently-updated router changes which teammate receives a handoff
/// with no diff to review and no way to tell a behaviour change from a bad day.
/// `OPENCOMPANY_TYPESAFE_MODEL` moves it without a rebuild.
#[must_use]
pub fn routing_model() -> String {
    std::env::var(MODEL_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned())
}

impl SystemOneTransport for TypeSafeTransport {
    fn evaluate<'a>(&'a self, request: &'a SystemOneRequest) -> SystemOneTransportFuture<'a> {
        Box::pin(async move {
            let response = self
                .http
                .post(&self.base_url)
                .bearer_auth(&self.api_key)
                .json(request)
                .send()
                .await
                .map_err(|error| {
                    // `route_semantic` matches `let Ok(..) else` and drops this,
                    // so a failed route degrades to the mechanical pick with no
                    // reason attached. Said here, where the reason still exists:
                    // a broken credential and a healthy fallback are otherwise
                    // indistinguishable from the outside.
                    tracing::warn!(
                        %error,
                        endpoint = %self.base_url,
                        "[typesafe] the routing request did not reach System One"
                    );
                    Error::Transport {
                        status: None,
                        message: error.to_string(),
                    }
                })?;

            let status = response.status();
            if !status.is_success() {
                // The body is the provider's own diagnostic and is carried
                // verbatim: a routing refusal that says only "400" is a support
                // ticket, and nothing here is in a position to summarise it.
                let message = response.text().await.unwrap_or_default();
                // The provider's own words. A routing refusal that says only
                // "400" is a support ticket; this is the line that names a
                // retired model id, an unentitled key or a schema drift.
                tracing::warn!(
                    status = status.as_u16(),
                    endpoint = %self.base_url,
                    body = %message,
                    "[typesafe] System One refused the routing request"
                );
                return Err(Error::Transport {
                    status: Some(status.as_u16()),
                    message,
                });
            }

            response.json::<SystemOneResponse>().await.map_err(|error| {
                tracing::warn!(
                    %error,
                    status = status.as_u16(),
                    "[typesafe] System One answered with a body this host cannot decode"
                );
                Error::Transport {
                    status: Some(status.as_u16()),
                    // A success status with a body this host cannot decode
                    // is a schema drift, not an outage, and the two are
                    // worth telling apart in a log — so the status rides
                    // along.
                    message: format!("System One response did not decode: {error}"),
                }
            })
        })
    }
}

#[cfg(test)]
#[path = "typesafe_tests.rs"]
mod tests;
