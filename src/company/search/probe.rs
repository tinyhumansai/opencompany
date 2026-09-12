//! Checking that a search provider answers, and classifying it when it does not.
//!
//! IO at the edge, classification pure. [`classify`] decides and [`describe`]
//! says, so the six classes are testable without a translator and the strings
//! stay where strings belong.
//!
//! # The classifier is per provider, and that is not a stylistic choice
//!
//! The inference surface this is shaped after classifies on the upstream error
//! *text* in a fixed branch order — proxy first, then `401`-or-`403`-with-
//! credential-wording, then endpoint, then timeout. That works because every
//! OpenAI-compatible endpoint signals a rejected key with `401`.
//!
//! **Brave does not.** A rejected Brave key comes back `422` with
//! `error.code == "SUBSCRIPTION_TOKEN_INVALID"`, and Brave's API reference
//! documents `200`, `404`, `422` and `429` and no `401` and no `403` at all.
//! Ported unchanged, that classifier would never fire its destructive branch for
//! Brave — leaving a key we have positive evidence is dead stored under an amber
//! "the check did not complete" — and would read Brave's only possible `403`,
//! which can only be a WAF, as a rejected key. That is the exact incident the
//! borrowed branch ordering exists to prevent, arriving through the other door.
//!
//! So [`classify`] takes the slug as well as the response. The generic branches
//! remain, in the borrowed order, for everything the per-provider rules do not
//! claim.
//!
//! # Only `auth` is destructive
//!
//! Every other class keeps the credential and shows an advisory, because the
//! credential is plausibly fine and the connection is not. A corporate proxy, a
//! WAF, a rate limit and a SearXNG instance with JSON output turned off all fail
//! a check while the credential — where there is one at all — is perfectly good.
//!
//! # The check costs a search
//!
//! No hosted search provider offers a free credential validator. All three
//! reject a bad key *before* running a search, so a failed check is free, but
//! confirming a good one costs one real query. The console says so on the button
//! it applies to. SearXNG is free, and is the only one that is.
//!
//! `reqwest` is used without a feature gate, the same assumption
//! [`crate::company::mcp_oauth`] already makes: it arrives with `oauth` and
//! `documents`, both of which are in the crate's default feature set.

use std::time::Duration;

use super::catalogue::{AuthStyle, SearchProviderInfo};

/// How long a connectivity check may take before it is abandoned.
///
/// Shorter than the harness's own 30s search timeout: a check is something an
/// operator is watching a spinner for, and one that has not answered in fifteen
/// seconds has already told them what they need to know.
const TIMEOUT: Duration = Duration::from_secs(15);

/// What a failed check means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeClass {
    /// The provider rejected the credential. **The only destructive class.**
    Auth,
    /// A SearXNG instance is reachable but will not serve JSON.
    Format,
    /// The account is out of credit, or rate limited.
    Quota,
    /// Nothing answered, or what answered was not this provider.
    Endpoint,
    /// It did not answer in time.
    Timeout,
    /// Something else. Deliberately the fallback, and never destructive.
    Unknown,
}

impl ProbeClass {
    /// The wire spelling, shared with the console mirror.
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeClass::Auth => "auth",
            ProbeClass::Format => "format",
            ProbeClass::Quota => "quota",
            ProbeClass::Endpoint => "endpoint",
            ProbeClass::Timeout => "timeout",
            ProbeClass::Unknown => "unknown",
        }
    }
}

/// Whether meeting this class should roll back the credential just written.
///
/// Exactly one class says yes, and that is the point. The naive add flow rolls
/// everything back on any failure, and the naive flow **destroys valid
/// credentials**.
pub fn destroys_credential(class: ProbeClass) -> bool {
    class == ProbeClass::Auth
}

/// What a check came back with.
#[derive(Debug, Clone)]
pub enum ProbeFailure {
    /// The request never got an answer — DNS, refused, TLS, timeout.
    Transport(String),
    /// The provider answered, and not with success.
    Status {
        /// The HTTP status.
        status: u16,
        /// The response body, read only for classification and **never shown to
        /// an operator**: it can echo request material, including fragments of
        /// the credential, and the sentence built from it lands in a banner
        /// somebody screenshots into a ticket.
        body: String,
    },
}

/// Whether `text` contains `needle` case-insensitively.
fn mentions(text: &str, needle: &str) -> bool {
    text.to_ascii_lowercase().contains(needle)
}

/// Whether `text` contains `code` as a standalone number.
///
/// Word boundaries, so `401` and `403` do not match inside an id like `1403`.
fn mentions_status(text: &str, code: u16) -> bool {
    let code = code.to_string();
    let bytes = text.as_bytes();
    text.match_indices(&code).any(|(at, _)| {
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_digit();
        let after = at + code.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_digit();
        before_ok && after_ok
    })
}

/// Whether this response is the provider saying the credential is wrong.
///
/// Per provider, because the providers do not agree — see the module header.
/// Anything not positively recognised as a credential rejection is **not**
/// `auth`, which is the safe direction: the worst outcome of a miss is an amber
/// advisory beside a stored key, while the worst outcome of a false positive is
/// a deleted working credential.
fn is_auth_rejection(slug: &str, status: u16, body: &str) -> bool {
    match slug {
        // Brave answers 422, never 401 or 403. A 422 that is *not* the token
        // code is a request-shape problem — our bug, not their key — so it is
        // deliberately not auth.
        "brave" => status == 422 && mentions(body, "subscription_token_invalid"),
        // Exa documents 401 for a bad key. Its 402 means EITHER no credential at
        // all OR out of credit, so the body decides rather than the status.
        "exa" => {
            status == 401
                || (status == 402
                    && (mentions(body, "invalid_api_key") || mentions(body, "invalid api key")))
        }
        // Querit answers 401 with `error_code` as a string.
        "querit" => status == 401,
        // SearXNG has no credential to reject. Its 403 means something else
        // entirely — see the `Format` arm below.
        "searxng" => false,
        // An unknown slug cannot be positively recognised, so it never is.
        _ => false,
    }
}

/// The class of a failed check.
pub fn classify(slug: &str, failure: &ProbeFailure) -> ProbeClass {
    match failure {
        ProbeFailure::Transport(error) => classify_transport(error),
        ProbeFailure::Status { status, body } => classify_status(slug, *status, body),
    }
}

/// [`classify`] for a request that never got an answer.
fn classify_transport(error: &str) -> ProbeClass {
    if mentions(error, "timeout") || mentions(error, "timed out") || mentions(error, "deadline") {
        return ProbeClass::Timeout;
    }
    ProbeClass::Endpoint
}

/// [`classify`] for a response the provider actually sent.
fn classify_status(slug: &str, status: u16, body: &str) -> ProbeClass {
    // The proxy branch runs FIRST, and it is not negotiable. Otherwise the word
    // "authentication" inside `407 Proxy Authentication Required` matches a
    // credential rule and a corporate proxy deletes a valid key.
    if status == 407
        || mentions_status(body, 407)
        || mentions(body, "cloudflare")
        || mentions(body, "bad gateway")
        || mentions(body, "proxy authentication")
    {
        return ProbeClass::Unknown;
    }

    if is_auth_rejection(slug, status, body) {
        return ProbeClass::Auth;
    }

    // A SearXNG 403 is neither a credential problem nor a WAF: JSON output is
    // off. SearXNG ships `search.formats: [html]` and aborts with 403 when a
    // format is not enabled, which makes this the single most likely failure
    // when connecting a perfectly healthy instance. Classifying it as `auth`
    // would try to delete a key that does not exist; classifying it as
    // `unknown` would throw away the one message that could fix it.
    if slug == "searxng" && status == 403 {
        return ProbeClass::Format;
    }

    if status == 429 || status == 402 {
        return ProbeClass::Quota;
    }

    if status == 404 || status == 502 || status == 503 {
        return ProbeClass::Endpoint;
    }

    ProbeClass::Unknown
}

/// One sentence for a class. **Never interpolates the upstream body.**
///
/// `provider` is the label to name. `auth`, `endpoint` and `timeout` are about a
/// specific thing and naming it is the difference between a dead end and a next
/// step; the others are about the account or the check, so they name neither.
pub fn describe(class: ProbeClass, provider: &str) -> String {
    match class {
        ProbeClass::Auth => {
            format!("Could not reach {provider}: the provider rejected the credential.")
        }
        ProbeClass::Format => {
            "Saved. The instance has JSON output turned off — add `json` to `search.formats` in \
             its settings.yml."
                .to_string()
        }
        ProbeClass::Quota => "Saved. The account is out of credit.".to_string(),
        ProbeClass::Endpoint => format!("Saved, but nothing answered at {provider}."),
        ProbeClass::Timeout => format!("Saved, but {provider} did not answer in time."),
        ProbeClass::Unknown => "Saved, but the check did not complete.".to_string(),
    }
}

/// Refuses an instance address this host must not fetch on an operator's behalf.
///
/// # Why this is not [`crate::server::ops::memory_ingest`]'s guard
///
/// That guard refuses **every** private address, including `.internal`
/// hostnames, which is right for fetching a link an operator pasted from the
/// internet and wrong here: a self-hosted SearXNG instance at `10.0.0.5` or
/// `search.acme.internal` is the *normal* deployment, and reusing it would
/// refuse the ordinary case. It is also `#[cfg(feature = "documents")]`, and
/// this surface is deliberately ungated.
///
/// So this is a narrower rule rather than a copy: the class that is never a
/// search instance and is always someone's cloud metadata service.
///
/// The residual reach is real — an administrator can point this at a private
/// address and learn whether something answers there — and it is why the probe
/// route requires [`AdminScopedCompany`](crate::server::ops::scope) rather than
/// the `ScopedCompany` the read routes use. It is an allowance, not an
/// oversight.
///
/// # This is half of the rule
///
/// It can only read a literal address, so `http://metadata.example/` passes it
/// and then resolves to `169.254.169.254` anyway. The other half is
/// [`pick_address`], applied to what the hostname actually resolves to at the
/// moment of the request — and the result is pinned into the client, so the
/// name cannot resolve to something else between the check and the connection.
///
/// This is the third copy of a URL-shape rule in the tree, and the second one's
/// own comment already says the lasting fix is to lift it somewhere both can
/// depend on. Doing that is a separate change: the three want three different
/// address policies, so lifting means parameterising, not just moving.
pub fn guard_instance_url(url: &str) -> Result<(), String> {
    let parsed = url
        .parse::<axum::http::Uri>()
        .map_err(|_| "not a URL".to_string())?;
    match parsed.scheme_str() {
        Some("http") | Some("https") => {}
        _ => return Err("a SearXNG instance address must be http:// or https://".to_string()),
    }
    let host = parsed
        .host()
        .ok_or_else(|| "no host in the URL".to_string())?;
    // A bracketed IPv6 literal keeps its brackets in `Uri::host`.
    let host = host.trim_start_matches('[').trim_end_matches(']');

    if let Ok(address) = host.parse::<std::net::IpAddr>()
        && is_metadata_address(address)
    {
        return Err("that address is a cloud metadata service, not a search instance".to_string());
    }
    Ok(())
}

/// AWS's IPv6 instance metadata address.
///
/// It is a **unique local** address (`fc00::/7`), not link-local, so the
/// link-local test below does not see it — and unlike `169.254.169.254` it has
/// no shape that gives it away. It is named because it is the address, and
/// because refusing every ULA would refuse an ordinary IPv6 private network,
/// which is the same mistake as refusing RFC1918.
const EC2_IPV6_METADATA: std::net::Ipv6Addr =
    std::net::Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x254);

/// Link-local and the cloud metadata services that live there.
///
/// Loopback and RFC1918 are deliberately **absent**: both are ordinary places
/// for a self-hosted instance. Link-local is not — nobody runs SearXNG on
/// `169.254.169.254`, and everybody's instance metadata service does.
fn is_metadata_address(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(v4) => v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast(),
        std::net::IpAddr::V6(v6) => {
            // The one metadata address that is not link-local, so it has to be
            // named rather than derived.
            if v6 == EC2_IPV6_METADATA {
                return true;
            }
            if v6.is_unspecified() || v6.segments()[0] & 0xffc0 == 0xfe80 {
                return true;
            }
            // `to_ipv4`, not `to_ipv4_mapped`: the deprecated IPv4-compatible
            // form (`::a.b.c.d`) carries the same address and the mapped reader
            // answers `None` for it.
            v6.to_ipv4()
                .is_some_and(|v4| is_metadata_address(std::net::IpAddr::V4(v4)))
        }
    }
}

/// The address an operator-supplied endpoint will actually be connected to.
///
/// [`guard_instance_url`] reads the URL and can only judge a literal IP. A
/// hostname is judged here instead, against what it resolves to — otherwise
/// `http://metadata.example/` walks past the guard and the request goes to
/// whatever the name points at, which is the whole of the protection the guard
/// was written to provide.
///
/// **Any** metadata address among the answers refuses the lot, rather than
/// picking a different one. A name that resolves to the metadata service is not
/// a search instance whatever else it also resolves to, and choosing around it
/// would make the outcome a coin flip on DNS ordering.
///
/// The chosen address is then pinned into the client, which is what closes the
/// rebinding window: checking a name and then letting the client resolve it
/// again leaves a gap in which the answer can change.
fn pick_address(addresses: &[std::net::SocketAddr]) -> Result<std::net::SocketAddr, String> {
    let Some(first) = addresses.first() else {
        return Err("that instance address does not resolve".to_string());
    };
    if let Some(refused) = addresses
        .iter()
        .find(|address| is_metadata_address(address.ip()))
    {
        return Err(format!(
            "that address resolves to {}, a cloud metadata service rather than a search instance",
            refused.ip()
        ));
    }
    Ok(*first)
}

/// Resolves an operator-supplied endpoint, refusing what must not be fetched.
///
/// Returns the host to pin and the address to pin it to. `None` means there is
/// nothing to pin: a literal IP is already settled by [`guard_instance_url`],
/// and letting the client handle it keeps this off the path for the three
/// account providers, whose addresses are constants in the catalogue rather
/// than anybody's input.
/// Why a name could not be pinned.
///
/// The two are not the same answer and the callers treat them differently: a
/// name this host **must not** fetch is refused wherever it appears, while a
/// name that merely did not resolve is a transient fact about DNS. Refusing to
/// *store* an address because a name server was briefly down would be a bug of
/// its own, so the distinction lives in the type rather than in a message
/// somebody has to match on.
enum PinFailure {
    /// It resolved, and to something this host must not fetch.
    Refused(String),
    /// It could not be resolved at all — no answer, a timeout, or not a URL.
    Unresolved(String),
}

impl PinFailure {
    fn message(self) -> String {
        match self {
            PinFailure::Refused(message) | PinFailure::Unresolved(message) => message,
        }
    }
}

async fn pin_for(endpoint: &str) -> Result<Option<(String, std::net::SocketAddr)>, PinFailure> {
    let parsed = endpoint
        .parse::<axum::http::Uri>()
        .map_err(|_| PinFailure::Unresolved("that instance address is not a URL".to_string()))?;
    let Some(host) = parsed.host() else {
        return Err(PinFailure::Unresolved(
            "that instance address has no host".to_string(),
        ));
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(None);
    }
    let port = parsed.port_u16().unwrap_or(match parsed.scheme_str() {
        Some("https") => 443,
        _ => 80,
    });

    // Bounded by the same clock as the request. A name server that never
    // answers must not hold this route open longer than a provider that never
    // answers does.
    let resolved = tokio::time::timeout(TIMEOUT, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| PinFailure::Unresolved("that instance address timed out in DNS".to_string()))?
        .map_err(|err| PinFailure::Unresolved(format!("dns error: {err}")))?
        .collect::<Vec<_>>();

    let address = pick_address(&resolved).map_err(PinFailure::Refused)?;
    Ok(Some((host.to_string(), address)))
}

/// The resolve-time half of [`guard_instance_url`], for a caller that is about
/// to **store** an address rather than fetch it now.
///
/// The literal-IP guard is not enough on a storing path, and the storing paths
/// are where it matters most: `PUT …/search/providers/{slug}` re-addresses a
/// connection without probing it, so nothing resolved the name before it was
/// saved — and the search tool resolves it normally at agent-turn time and
/// fetches whatever it points at.
///
/// **A name that does not resolve is not refused.** DNS being down is not
/// evidence that an address is forbidden, and refusing to save an operator's own
/// instance because a resolver blinked would be a worse failure than the one
/// this prevents. Only a name that resolves to something this host must not
/// fetch is refused.
///
/// This is a check at the door, not a guarantee at fetch time: a name can be
/// re-pointed after it is stored. Closing that needs the pinning to happen where
/// the agent's search is actually made, inside the search tool's own client,
/// which is a change to the harness rather than to this module.
pub async fn guard_resolved_host(endpoint: &str) -> Result<(), String> {
    match pin_for(endpoint).await {
        Ok(_) | Err(PinFailure::Unresolved(_)) => Ok(()),
        Err(refused @ PinFailure::Refused(_)) => Err(refused.message()),
    }
}

/// Asks a provider whether it answers for this credential.
///
/// One request, chosen per provider because there is no shape they share. For
/// the three account providers it is a one-result search — the cheapest real
/// call each offers, since none publishes a free validator. For SearXNG it is
/// the JSON search the tool itself will make, which costs the operator nothing
/// and is the only call that proves JSON output is enabled.
///
/// # Errors
///
/// Returns the failure for [`classify`] to read. Never returns the response body
/// to the caller beyond that.
pub async fn probe(
    info: &SearchProviderInfo,
    credential: Option<&str>,
    endpoint: Option<&str>,
) -> Result<(), ProbeFailure> {
    let base = endpoint.unwrap_or(info.endpoint).trim_end_matches('/');
    if base.is_empty() {
        return Err(ProbeFailure::Transport("no endpoint to check".to_string()));
    }

    let mut builder = reqwest::Client::builder()
        .timeout(TIMEOUT)
        // A redirect to somewhere else is not this provider answering, and
        // following one is how a guarded address is reached anyway.
        .redirect(reqwest::redirect::Policy::none());
    // Only for an address the operator supplied. The three account providers
    // answer at constants in the catalogue, so there is nothing about them for
    // a name to point somewhere else.
    if let Some(endpoint) = endpoint
        && let Some((host, address)) = pin_for(endpoint)
            .await
            .map_err(|failure| ProbeFailure::Transport(failure.message()))?
    {
        builder = builder.resolve(&host, address);
    }
    let client = builder
        .build()
        .map_err(|err| ProbeFailure::Transport(err.to_string()))?;

    let request = match info.slug {
        "brave" => client.get(format!("{base}/web/search?q=opencompany&count=1")),
        "exa" => client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({ "query": "opencompany", "numResults": 1 })),
        "querit" => client
            .post(format!("{base}/search"))
            .json(&serde_json::json!({ "query": "opencompany", "count": 1 })),
        "searxng" => {
            let path = if base.ends_with("/search") {
                base.to_string()
            } else {
                format!("{base}/search")
            };
            client.get(format!("{path}?q=opencompany&format=json"))
        }
        other => {
            return Err(ProbeFailure::Transport(format!(
                "no connectivity check is defined for `{other}`"
            )));
        }
    };

    let request = match (info.auth, credential) {
        (Some(AuthStyle::Header(name)), Some(key)) => request.header(name, key),
        (Some(AuthStyle::Bearer), Some(key)) => request.bearer_auth(key),
        _ => request,
    };

    let response = request
        .send()
        .await
        .map_err(|err| ProbeFailure::Transport(err.to_string()))?;

    let status = response.status().as_u16();
    if response.status().is_success() {
        return Ok(());
    }
    Err(ProbeFailure::Status {
        status,
        body: read_capped(response).await,
    })
}

/// How much of a failing response is read before the rest is dropped.
///
/// Enough to classify by — the longest phrase any of the four providers puts in
/// a rejection is well inside it — and small enough that a hostile answer costs
/// nothing.
const BODY_CAP: usize = 4096;

/// Reads at most [`BODY_CAP`] bytes of a response and abandons the rest.
///
/// **The cap is applied to the stream, not to the finished string.** The
/// previous `response.text().await` followed by `.take(4096)` buffered the
/// whole body first and then kept 4096 characters of it, which caps what is
/// *retained* and not what is *accepted*: for SearXNG the address is supplied
/// by the operator, so a malfunctioning or hostile instance could answer a
/// connect or test request with a body large enough to exhaust this host — and
/// the route is reachable by any admin of any company on it.
///
/// A read error mid-body is not an error here. Whatever arrived is enough to
/// classify from, and the status code alone usually is; failing the probe
/// because the tail of a rejection did not arrive would turn a clear answer
/// into `Unknown`.
///
/// The cap is a byte count, so the last character kept may be split. It is
/// read back lossily and only ever matched against ASCII phrases, so a
/// replacement character at the very end changes no classification.
async fn read_capped(mut response: reqwest::Response) -> String {
    let mut body: Vec<u8> = Vec::new();
    while body.len() < BODY_CAP {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = BODY_CAP - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// What a failure may be written to the log as.
///
/// **Never the body.** [`ProbeFailure::Status`] already documents that its body
/// can echo request material including fragments of the credential, and that it
/// is never shown to an operator — but a log is a second durable copy of
/// exactly the same material, kept for longer and read by more people than the
/// banner the type was worried about. The credential is write-only everywhere
/// else on this surface, so it does not get an exemption here.
///
/// What is left is what a person reading the log actually acts on: the status,
/// and how much body came back. The class is logged beside this, and the
/// classification the body fed is what the operator is shown.
pub fn log_detail(failure: &ProbeFailure) -> String {
    match failure {
        // The operator's own address and the transport error for it. No
        // credential is ever in one: the key travels in a header, and a request
        // that failed at this layer never got far enough to be echoed.
        ProbeFailure::Transport(error) => format!("transport: {error}"),
        ProbeFailure::Status { status, body } => {
            format!("status {status}, {} bytes of body withheld", body.len())
        }
    }
}

#[cfg(test)]
#[path = "probe_test.rs"]
mod probe_test;
