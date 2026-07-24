//! Per-tenant browser OAuth for HTTP-remote MCP servers (issue #90).
//!
//! Many remote MCP servers gate access behind OAuth 2.0 (authorization-code +
//! PKCE), advertised via a `401` challenge that points at an
//! `oauth-protected-resource` document. This module runs that flow for the
//! **console** path: the operator clicks *Sign in*, a browser tab opens the
//! authorization server, and the redirect lands on the host's unauthenticated
//! `/oauth/mcp/callback` route, which exchanges the code for a token and stores
//! it write-only under the company's per-server credential key.
//!
//! **Why a bespoke module and not `oh::mcp_registry::oauth`.** OpenHuman's full
//! flow (`vendor/openhuman/src/openhuman/mcp_registry/oauth.rs`) is coupled to
//! its SQLite `mcp_registry` store and its desktop loopback callback
//! (`http://127.0.0.1:<core_port>/…`). Our console path is multi-tenant and
//! stores credentials in a per-tenant [`SecretStore`](crate::ports::SecretStore),
//! so only the **discovery primitive** ([`oh::mcp_client::McpHttpClient`]) is
//! reused; the small private helpers (PKCE, dynamic client registration, the
//! authorize-URL builder, the token exchange + parse, and refresh) are **ported**
//! here so they stay decoupled from OpenHuman's store and single-user callback.
//!
//! **Security.** Every token this module mints is returned as an
//! [`AuthMaterial::OAuth`], whose [`AuthMaterial::secret_values`] enumerates the
//! access token, refresh token, and any confidential `client_secret` — so all of
//! them feed the existing scrubber and can never survive into an error, a health
//! record, or agent-visible output. Nothing here serializes a token into any
//! operator-visible response.
//!
//! Compiled only under `feature = "mcp"` (it needs the vendored discovery
//! primitive plus `uuid` / `base64` / `url`).

use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use openhuman_core::openhuman as oh;

use oh::mcp_client::{AuthorizationServerMetadata, McpAuthorizationContext, McpHttpClient};

use crate::Result;
use crate::company::mcp::AuthMaterial;
use crate::error::OpenCompanyError;
use crate::ports::types::CompanyId;

/// The discovery/registration/token HTTP timeout, in seconds.
const HTTP_TIMEOUT_SECS: u64 = 20;

/// base64url, no padding — the PKCE + entropy encoding.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Pending authorization parked between [`begin`] and the callback's
/// [`complete`], keyed by the opaque `state` the console round-trips through the
/// browser. Multi-tenant: unlike OpenHuman's single-user version it carries the
/// `company_id` + `server_name` the callback must resolve, since the browser
/// redirect carries no console session.
#[derive(Clone, Debug)]
pub struct PendingOAuth {
    /// The company whose per-tenant secret store the token lands in.
    pub company_id: CompanyId,
    /// The MCP server (by slug) the token authenticates.
    pub server_name: String,
    /// The PKCE verifier proving this client began the flow.
    pub code_verifier: String,
    /// The dynamically-registered client id (RFC 7591).
    pub client_id: String,
    /// The confidential client secret, when the server issued one.
    pub client_secret: Option<String>,
    /// The token endpoint the code exchange POSTs to.
    pub token_endpoint: String,
    /// The exact redirect URI registered in DCR — must match on exchange.
    pub redirect_uri: String,
}

/// The output of [`begin`]: the live `/authorize` URL for the browser plus the
/// `state`/`PendingOAuth` the route parks until the callback fires.
pub struct OAuthBegin {
    /// The authorization-server URL to open in the operator's browser.
    pub authorize_url: String,
    /// The opaque CSRF/state token the callback matches against.
    pub state: String,
    /// The pending record the caller stores keyed by `state`.
    pub pending: PendingOAuth,
}

/// Unix seconds now (best-effort; `0` if the clock is before the epoch).
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The callback redirect URI: `<base>/oauth/mcp/callback`. `base` is the host's
/// public URL (`OPENCOMPANY_PUBLIC_URL`) or `http://{bind}` — see
/// [`AppConfig::host_base_url`](crate::app::AppConfig::host_base_url). This MUST
/// be the exact string registered in DCR and replayed on the token exchange, or
/// the authorization server rejects the redirect.
pub fn callback_redirect_uri(base_url: &str) -> String {
    format!("{}/oauth/mcp/callback", base_url.trim_end_matches('/'))
}

/// `n` v4 UUIDs of entropy, base64url-encoded (no `rand` dependency — mirrors
/// the OpenHuman helper).
fn random_b64(n_uuids: usize) -> String {
    let mut bytes = Vec::with_capacity(n_uuids * 16);
    for _ in 0..n_uuids {
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    B64.encode(bytes)
}

/// A PKCE `(verifier, S256 challenge)` pair. The verifier is ~86 chars, inside
/// the RFC 7636 43..128 range.
fn gen_pkce() -> (String, String) {
    let verifier = random_b64(4);
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// Discover a server's OAuth authorization context via an unauthenticated MCP
/// `initialize` probe. `Ok(None)` means the server did not 401 (open / static
/// token). Errors surface a discovery failure.
async fn discover(endpoint: &str) -> Result<Option<McpAuthorizationContext>> {
    let client = McpHttpClient::new(endpoint.to_string(), HTTP_TIMEOUT_SECS);
    client
        .discover_authorization()
        .await
        .map_err(|e| OpenCompanyError::Harness(format!("oauth discovery failed: {e}")))
}

/// Pick the authorization server that advertises a **usable** configuration:
/// authorize + token + dynamic client registration, and (when grant types are
/// listed) `authorization_code`. Pick by capability, not position — the first
/// advertised server may be incomplete while a later one is fully usable
/// (mirrors OpenHuman's `begin`).
fn select_auth_server(ctx: McpAuthorizationContext) -> Option<AuthorizationServerMetadata> {
    ctx.authorization_server_metadata.into_iter().find(|asm| {
        asm.authorization_endpoint.is_some()
            && asm.token_endpoint.is_some()
            && asm.registration_endpoint.is_some()
            && (asm.grant_types_supported.is_empty()
                || asm
                    .grant_types_supported
                    .iter()
                    .any(|g| g == "authorization_code"))
    })
}

/// A short-lived reqwest client for DCR + token calls. Separate from the MCP
/// transport client (which speaks JSON-RPC), matching OpenHuman's split.
fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .expect("reqwest client must build")
}

/// SSRF guard for a **discovery-supplied** OAuth endpoint. The MCP server we're
/// signing into advertises its own registration/token/authorize URLs, and
/// `begin` / `complete` / `refresh` then POST to them **server-side** — so an
/// unvalidated endpoint is a server-side request-forgery primitive a hostile
/// server can aim at internal services or the cloud metadata endpoint
/// (`169.254.169.254`). Reject anything that isn't `https` or whose host
/// resolves to a loopback/private/link-local/unspecified address. Ported (kept
/// decoupled from OpenHuman's `url_guard`, which is `pub(super)` and unreachable
/// here) so the console path validates every outbound OAuth target itself.
///
/// Async because hostname resolution goes through [`tokio::net::lookup_host`],
/// which offloads the blocking `getaddrinfo` to Tokio's blocking pool — a
/// slow/hostile discovery-supplied host must not stall the executor thread.
async fn guard_endpoint(raw: &str, what: &str) -> Result<()> {
    let url = url::Url::parse(raw).map_err(|e| {
        OpenCompanyError::InvalidRequest(format!("{what} endpoint is not a valid URL: {e}"))
    })?;
    if url.scheme() != "https" {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "{what} endpoint must use https (got `{}`)",
            url.scheme()
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| OpenCompanyError::InvalidRequest(format!("{what} endpoint has no host")))?;

    // An IP-literal host is checked directly — no DNS round-trip needed.
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return if is_blocked_ip(&ip) {
            Err(OpenCompanyError::InvalidRequest(format!(
                "{what} endpoint resolves to a disallowed address"
            )))
        } else {
            Ok(())
        };
    }

    // Resolve the hostname and reject if it fails to resolve or if ANY resolved
    // address is in a blocked range (a single blocked A/AAAA record is enough —
    // this also blunts DNS-rebinding to an internal address). `lookup_host`
    // keeps the blocking resolver call off the async executor thread.
    let port = url.port_or_known_default().unwrap_or(443);
    let mut resolved = false;
    let addrs = tokio::net::lookup_host((host, port)).await.map_err(|e| {
        OpenCompanyError::InvalidRequest(format!("{what} endpoint host does not resolve: {e}"))
    })?;
    for addr in addrs {
        resolved = true;
        if is_blocked_ip(&addr.ip()) {
            return Err(OpenCompanyError::InvalidRequest(format!(
                "{what} endpoint resolves to a disallowed address"
            )));
        }
    }
    if !resolved {
        return Err(OpenCompanyError::InvalidRequest(format!(
            "{what} endpoint host does not resolve"
        )));
    }
    Ok(())
}

/// Whether an IP is in a range we refuse to POST OAuth material to (loopback,
/// RFC 1918 / unique-local, link-local incl. the `169.254.169.254` metadata
/// address, unspecified, broadcast, documentation, multicast).
fn is_blocked_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || v4.octets()[0] == 0
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_ip(&std::net::IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

/// RFC 7591 dynamic client registration. Requests `client_secret_post`; servers
/// that issue a confidential client return a `client_secret` we keep for the
/// token exchange. Returns `(client_id, client_secret?)`.
async fn register_client(
    registration_endpoint: &str,
    redirect_uri: &str,
) -> Result<(String, Option<String>)> {
    guard_endpoint(registration_endpoint, "registration").await?;
    let body = json!({
        "client_name": "OpenCompany",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "client_secret_post",
    });
    let resp = http()
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            OpenCompanyError::Harness(format!("client registration request failed: {e}"))
        })?;
    let status = resp.status();
    let json: Value = resp.json().await.map_err(|e| {
        OpenCompanyError::Harness(format!("client registration returned non-JSON: {e}"))
    })?;
    if !status.is_success() {
        // The error body carries `error`/`error_description`, never a secret (a
        // client_secret is issued only on success), so surfacing the code is safe.
        let code = json
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(OpenCompanyError::Harness(format!(
            "dynamic client registration failed (HTTP {status}, {code})"
        )));
    }
    let client_id = json
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            OpenCompanyError::Harness("registration response missing client_id".to_string())
        })?
        .to_string();
    let client_secret = json
        .get("client_secret")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((client_id, client_secret))
}

/// Build the `/authorize` redirect URL (authorization-code + PKCE S256).
/// `resource` is the MCP server URL, per the MCP authorization spec.
fn build_authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
    resource: &str,
) -> Result<String> {
    url::Url::parse_with_params(
        authorization_endpoint,
        &[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("code_challenge", code_challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            ("resource", resource),
        ],
    )
    .map(|u| u.to_string())
    .map_err(|e| OpenCompanyError::Harness(format!("failed to build authorize url: {e}")))
}

/// Begin the browser-OAuth flow for `server_name` at `endpoint`: discover →
/// dynamic client registration → PKCE, and return the live `/authorize` URL plus
/// the [`PendingOAuth`] the caller parks (keyed by the returned `state`).
///
/// Returns [`OpenCompanyError::InvalidRequest`] (a clean operator-actionable
/// error) when the server does not advertise dynamic client registration — the
/// operator should paste a static token instead.
pub async fn begin(
    endpoint: &str,
    company_id: &CompanyId,
    server_name: &str,
    redirect_uri: &str,
) -> Result<OAuthBegin> {
    let ctx = discover(endpoint)
        .await?
        .ok_or_else(|| OpenCompanyError::InvalidRequest(format!(
            "MCP server `{server_name}` does not require authorization — no OAuth sign-in is needed."
        )))?;

    // No usable auth server → the server can't do dynamic client registration.
    // Tell the operator to paste a static token instead (edge case).
    let asm = select_auth_server(ctx).ok_or_else(|| {
        OpenCompanyError::InvalidRequest(format!(
            "MCP server `{server_name}` requires OAuth but does not advertise dynamic client \
             registration — paste a static API token in its credential field instead."
        ))
    })?;
    let authorization_endpoint = asm
        .authorization_endpoint
        .ok_or_else(|| OpenCompanyError::Harness("no authorization_endpoint".to_string()))?;
    let token_endpoint = asm
        .token_endpoint
        .ok_or_else(|| OpenCompanyError::Harness("no token_endpoint".to_string()))?;
    let registration_endpoint = asm.registration_endpoint.ok_or_else(|| {
        OpenCompanyError::InvalidRequest(format!(
            "MCP server `{server_name}` requires OAuth but does not support dynamic client \
             registration — paste a static API token instead."
        ))
    })?;

    // Fail fast on unsafe discovery-supplied targets before any outbound call or
    // before parking a pending flow whose stored `token_endpoint` refresh would
    // later replay. `register_client` / `post_token_form` re-guard at call time.
    guard_endpoint(&authorization_endpoint, "authorization").await?;
    guard_endpoint(&token_endpoint, "token").await?;

    let (client_id, client_secret) = register_client(&registration_endpoint, redirect_uri).await?;
    let (code_verifier, code_challenge) = gen_pkce();
    let state = uuid::Uuid::new_v4().to_string();

    let authorize_url = build_authorize_url(
        &authorization_endpoint,
        &client_id,
        redirect_uri,
        &code_challenge,
        &state,
        endpoint,
    )?;

    log::info!(
        "[mcp-oauth] begin company={} server={server_name} client_id={client_id}",
        company_id.as_ref()
    );

    Ok(OAuthBegin {
        authorize_url,
        state,
        pending: PendingOAuth {
            company_id: company_id.clone(),
            server_name: server_name.to_string(),
            code_verifier,
            client_id,
            client_secret,
            token_endpoint,
            redirect_uri: redirect_uri.to_string(),
        },
    })
}

/// The parsed token-endpoint response.
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

/// POST a form to a token endpoint and return the JSON body (erroring on non-2xx
/// without echoing the raw body — a token endpoint's success body carries the
/// access token, so we never surface it).
async fn post_token_form(endpoint: &str, form: &[(&str, &str)]) -> Result<Value> {
    // Every token exchange + refresh POST funnels through here, so validating the
    // target at this choke point guards both `complete` and `refresh` — and
    // re-checks at call time (not just at `begin`), blunting a rebind between
    // discovery and exchange.
    guard_endpoint(endpoint, "token").await?;
    let resp = http()
        .post(endpoint)
        .form(form)
        .send()
        .await
        .map_err(|e| OpenCompanyError::Harness(format!("token request failed: {e}")))?;
    let status = resp.status();
    let json: Value = resp
        .json()
        .await
        .map_err(|e| OpenCompanyError::Harness(format!("token endpoint returned non-JSON: {e}")))?;
    if !status.is_success() {
        // Surface only the standard OAuth `error` code — never the whole body,
        // which on some servers echoes back submitted material.
        let code = json
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(OpenCompanyError::Harness(format!(
            "token request rejected (HTTP {status}, {code})"
        )));
    }
    Ok(json)
}

fn parse_token_response(json: &Value) -> Result<TokenResponse> {
    let access_token = json
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            OpenCompanyError::Harness("token response missing access_token".to_string())
        })?
        .to_string();
    Ok(TokenResponse {
        access_token,
        refresh_token: json
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string),
        expires_in: json.get("expires_in").and_then(Value::as_u64),
    })
}

/// Build an [`AuthMaterial::OAuth`] from a parsed token response + the client
/// credentials it was minted with.
fn material_from_tokens(
    tokens: TokenResponse,
    client_id: String,
    client_secret: Option<String>,
    token_endpoint: String,
) -> AuthMaterial {
    AuthMaterial::OAuth {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        client_id,
        client_secret,
        token_endpoint,
        expires_at: now_unix() + tokens.expires_in.unwrap_or(3600),
    }
}

/// Complete the flow: exchange `code` (with the PKCE verifier + client creds)
/// for a token, returning the [`AuthMaterial::OAuth`] the caller stores
/// write-only. The `pending` was parked by [`begin`] and looked up by `state`.
pub async fn complete(pending: &PendingOAuth, code: &str) -> Result<AuthMaterial> {
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", pending.redirect_uri.as_str()),
        ("client_id", pending.client_id.as_str()),
        ("code_verifier", pending.code_verifier.as_str()),
    ];
    if let Some(secret) = pending.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let tokens = parse_token_response(&post_token_form(&pending.token_endpoint, &form).await?)?;
    log::info!(
        "[mcp-oauth] complete company={} server={} — token minted",
        pending.company_id.as_ref(),
        pending.server_name
    );
    Ok(material_from_tokens(
        tokens,
        pending.client_id.clone(),
        pending.client_secret.clone(),
        pending.token_endpoint.clone(),
    ))
}

/// Whether an OAuth credential's access token is expired or within `slack_secs`
/// of expiring.
pub fn needs_refresh(material: &AuthMaterial, slack_secs: u64) -> bool {
    matches!(material, AuthMaterial::OAuth { expires_at, .. } if *expires_at <= now_unix() + slack_secs)
}

/// If `material` is an [`AuthMaterial::OAuth`] with a refresh token, mint a fresh
/// access token via the refresh-token grant and return the updated material.
///
/// Returns `Ok(None)` when the material is not OAuth, has no refresh token, or
/// the refresh request fails (the caller keeps the old material and lets the next
/// 401 re-prompt sign-in — a failed refresh must never brick the harness build).
pub async fn refresh(material: &AuthMaterial) -> Option<AuthMaterial> {
    let AuthMaterial::OAuth {
        refresh_token: Some(refresh_token),
        client_id,
        client_secret,
        token_endpoint,
        ..
    } = material
    else {
        return None;
    };
    if refresh_token.is_empty() {
        return None;
    }

    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
        ("client_id", client_id.as_str()),
    ];
    if let Some(secret) = client_secret.as_deref() {
        form.push(("client_secret", secret));
    }

    let json = match post_token_form(token_endpoint, &form).await {
        Ok(json) => json,
        Err(e) => {
            log::warn!("[mcp-oauth] refresh failed, keeping existing token: {e}");
            return None;
        }
    };
    let mut tokens = parse_token_response(&json).ok()?;
    // Some servers omit a rotated refresh token — keep the existing one.
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh_token.clone());
    }
    log::info!("[mcp-oauth] refreshed access token");
    Some(material_from_tokens(
        tokens,
        client_id.clone(),
        client_secret.clone(),
        token_endpoint.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let (verifier, challenge) = gen_pkce();
        assert!(
            (43..=128).contains(&verifier.len()),
            "verifier in PKCE range: {}",
            verifier.len()
        );
        let expected = B64.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
    }

    #[test]
    fn callback_uri_appends_route_and_trims_trailing_slash() {
        assert_eq!(
            callback_redirect_uri("http://127.0.0.1:8080"),
            "http://127.0.0.1:8080/oauth/mcp/callback"
        );
        assert_eq!(
            callback_redirect_uri("https://acme.example/"),
            "https://acme.example/oauth/mcp/callback"
        );
    }

    #[test]
    fn authorize_url_carries_pkce_and_resource() {
        let url = build_authorize_url(
            "https://as.example/authorize",
            "client-123",
            "http://127.0.0.1:8080/oauth/mcp/callback",
            "challenge-abc",
            "state-xyz",
            "https://mcp.example/mcp",
        )
        .expect("url");
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains("code_challenge=challenge-abc"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-xyz"));
        // The MCP server URL is the `resource`, percent-encoded.
        assert!(url.contains("resource=https%3A%2F%2Fmcp.example%2Fmcp"));
    }

    #[test]
    fn parse_token_response_extracts_fields() {
        let v = json!({"access_token":"a","refresh_token":"r","expires_in":3600});
        let t = parse_token_response(&v).unwrap();
        assert_eq!(t.access_token, "a");
        assert_eq!(t.refresh_token.as_deref(), Some("r"));
        assert_eq!(t.expires_in, Some(3600));
        // refresh_token / expires_in are optional; access_token is required.
        let minimal = parse_token_response(&json!({"access_token":"x"})).unwrap();
        assert_eq!(minimal.access_token, "x");
        assert!(minimal.refresh_token.is_none());
        assert!(parse_token_response(&json!({"token_type":"bearer"})).is_err());
    }

    #[test]
    fn oauth_material_secret_values_cover_every_token() {
        // The security invariant: access token, refresh token, and client secret
        // all reach the scrubber's known-secret set.
        let material = AuthMaterial::OAuth {
            access_token: "at-secret".into(),
            refresh_token: Some("rt-secret".into()),
            client_id: "cid".into(),
            client_secret: Some("cs-secret".into()),
            token_endpoint: "https://as/token".into(),
            expires_at: 0,
        };
        let secrets = material.secret_values();
        assert!(secrets.contains(&"at-secret".to_string()));
        assert!(secrets.contains(&"rt-secret".to_string()));
        assert!(secrets.contains(&"cs-secret".to_string()));
        // The client id is NOT a secret and must not be in the set.
        assert!(!secrets.contains(&"cid".to_string()));
        assert!(material.is_configured());
    }

    #[test]
    fn needs_refresh_only_fires_near_expiry() {
        let fresh = AuthMaterial::OAuth {
            access_token: "a".into(),
            refresh_token: None,
            client_id: "c".into(),
            client_secret: None,
            token_endpoint: "https://as/token".into(),
            expires_at: now_unix() + 3600,
        };
        assert!(!needs_refresh(&fresh, 60));
        let stale = AuthMaterial::OAuth {
            access_token: "a".into(),
            refresh_token: None,
            client_id: "c".into(),
            client_secret: None,
            token_endpoint: "https://as/token".into(),
            expires_at: now_unix() + 30,
        };
        assert!(needs_refresh(&stale, 60));
        // A non-OAuth material never needs refresh.
        assert!(!needs_refresh(&AuthMaterial::Bearer("t".into()), 60));
    }

    #[tokio::test]
    async fn guard_blocks_non_https_and_local_targets() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

        // Every case here short-circuits before DNS (bad scheme, bad URL, or an
        // IP-literal host), so the test never touches the network.
        // Scheme: plain http is refused outright (SSRF loves cleartext localhost).
        assert!(
            guard_endpoint("http://as.example/token", "token")
                .await
                .is_err()
        );
        // IP-literal hosts in blocked ranges are rejected without DNS.
        assert!(
            guard_endpoint("https://127.0.0.1/token", "token")
                .await
                .is_err()
        );
        assert!(
            guard_endpoint("https://10.0.0.5/register", "registration")
                .await
                .is_err()
        );
        assert!(
            guard_endpoint("https://192.168.1.1/token", "token")
                .await
                .is_err()
        );
        // The cloud metadata endpoint — the canonical SSRF target — is link-local.
        assert!(
            guard_endpoint("https://169.254.169.254/latest/meta-data", "token")
                .await
                .is_err()
        );
        assert!(
            guard_endpoint("https://[::1]/token", "token")
                .await
                .is_err()
        );
        // A syntactically bad URL is a clean rejection, not a panic.
        assert!(guard_endpoint("not a url", "token").await.is_err());

        // The IP-classification helper covers v4 + v6 ranges directly.
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_blocked_ip(&IpAddr::V6("fc00::1".parse().unwrap())));
        assert!(is_blocked_ip(&IpAddr::V6("fe80::1".parse().unwrap())));
        // A routable public address passes the classifier.
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))));
        assert!(!is_blocked_ip(&IpAddr::V6(
            "2606:2800:220:1::1".parse().unwrap()
        )));
    }

    #[tokio::test]
    async fn refresh_is_noop_without_refresh_token() {
        let material = AuthMaterial::OAuth {
            access_token: "a".into(),
            refresh_token: None,
            client_id: "c".into(),
            client_secret: None,
            token_endpoint: "https://as/token".into(),
            expires_at: 0,
        };
        assert!(refresh(&material).await.is_none());
        // A non-OAuth material is also a no-op.
        assert!(refresh(&AuthMaterial::Bearer("t".into())).await.is_none());
    }
}
