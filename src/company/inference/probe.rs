//! Reading a failed provider check: what class of failure it is, what to say
//! about it, and where a probe is allowed to point.
//!
//! Everything here is **pure**. The network call itself lives at the edge, in
//! the route that performs it; what this module holds is the part with branches
//! worth testing — and it is testable with a string and no host.
//!
//! ## Why classification is separate from wording
//!
//! [`classify`] decides, [`describe`] says. Splitting them is what makes the six
//! classes testable without a copy deck, and it keeps strings where strings
//! belong. It is also how the design being ported does it.
//!
//! ## Why only one class deletes the key
//!
//! Adding a provider writes the credential, then probes. If the probe fails, the
//! naive answer is "roll everything back", and the naive answer **destroys valid
//! credentials**: a corporate proxy, a WAF, a rate limit or a mistyped model id
//! all fail a probe while the key is perfectly good.
//!
//! So only [`ProbeClass::Auth`] is destructive. Everything else keeps the key and
//! the record and shows an amber advisory, because the key is plausibly fine and
//! the *connection* is not. Colouring those as errors would be a lie about what
//! happened — the save succeeded.
//!
//! ## The branch order is the whole design
//!
//! Two orderings exist because of real failures, and both are easy to
//! "simplify" back into the bug:
//!
//! **Proxy and gateway rejections are checked FIRST.** The phrase `407 Proxy
//! Authentication Required` contains the word *authentication*. Check the auth
//! branch first and a corporate proxy deletes a valid key. A WAF's bare `403
//! Forbidden` has the same shape, which is why a 403 counts as auth only when it
//! co-occurs with credential wording, and why the status-code tests use word
//! boundaries — so `401` and `403` do not match inside an id like `1403`.
//!
//! **`model` is checked BEFORE `endpoint`.** The endpoint branch matches a bare
//! "not found", which would otherwise claim every provider that phrases a
//! missing model as "model not found" and send the operator off to check their
//! base URL instead of their model id.
//!
//! ## And the raw string never reaches the copy
//!
//! [`describe`] does not interpolate the upstream text. That text can echo
//! request material — headers, key fragments — and it lands in a banner someone
//! screenshots. The raw string belongs in a detail channel, not in the sentence.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use super::catalogue;

/// What a failed probe means.
///
/// Six named classes rather than a boolean, because each one has a different
/// remedy and — more importantly — a different answer to "should the credential
/// we just wrote be deleted?".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeClass {
    /// The provider rejected the credential. **The only destructive class.**
    Auth,
    /// The endpoint answered but does not know that model id.
    Model,
    /// The account is out of credit, or rate limited.
    Quota,
    /// Nothing answered at that address.
    Endpoint,
    /// Something answered too slowly.
    Timeout,
    /// The check did not complete, and we will not guess why.
    Unknown,
}

impl ProbeClass {
    /// Whether meeting this class should roll back the credential that was just
    /// written.
    ///
    /// Exactly one class says yes. If a second ever does, re-read the module
    /// header first — every other class is a connection fact, not a key fact.
    pub fn destroys_credential(self) -> bool {
        matches!(self, Self::Auth)
    }

    /// The stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Model => "model",
            Self::Quota => "quota",
            Self::Endpoint => "endpoint",
            Self::Timeout => "timeout",
            Self::Unknown => "unknown",
        }
    }
}

/// Whether `needle` appears in `haystack` delimited by non-word characters —
/// the `\b…\b` a regex would give, without pulling in a regex.
///
/// This is what stops `403` matching inside `1403` or `4032`. It is not a
/// nicety: a model id or a request id with those digits in it would otherwise
/// be read as a status code and delete the operator's key.
fn contains_token(haystack: &str, needle: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(offset) = haystack[from..].find(needle) {
        let start = from + offset;
        let end = start + needle.len();
        let before_ok = start == 0 || !is_word(bytes[start - 1] as char);
        let after_ok = end == bytes.len() || !is_word(bytes[end] as char);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
        if from >= haystack.len() {
            break;
        }
    }
    false
}

/// Which failure a raw provider error string represents.
///
/// Ported branch for branch — including the order — from the design this work
/// follows. **Reordering these branches is a behaviour change**, not a
/// refactor; the module header names the two that matter and what each prevents.
pub fn classify(raw: &str) -> ProbeClass {
    let haystack = raw.trim().to_ascii_lowercase();

    // Network, gateway and proxy rejections are about the CONNECTION, not the
    // key. They must not reach the auth branch, or the add flow deletes a valid
    // key over a corporate proxy, a WAF, or a 407 challenge — the exact class
    // this ordering exists to preserve keys through. Checked first so
    // "authentication" inside "407 Proxy Authentication Required", and a
    // WAF/Cloudflare "403 Forbidden", classify as `unknown`.
    if contains_token(&haystack, "407")
        || haystack.contains("proxy")
        || haystack.contains("cloudflare")
        || haystack.contains("bad gateway")
        || haystack.contains("gateway timeout")
    {
        return ProbeClass::Unknown;
    }

    // A rejected credential: revoked, invalid, or without permission. A 403
    // counts only when it co-occurs with credential wording — a bare 403 from an
    // unidentified intermediary is not proof the key itself is bad.
    let is_403_credential = contains_token(&haystack, "403")
        && (haystack.contains("forbidden")
            || haystack.contains("key")
            || haystack.contains("credential")
            || haystack.contains("permission"));
    if contains_token(&haystack, "401")
        || is_403_credential
        || haystack.contains("invalid api key")
        || haystack.contains("invalid_api_key")
        || haystack.contains("incorrect api key")
        || haystack.contains("unauthorized")
        || haystack.contains("authentication")
    {
        return ProbeClass::Auth;
    }

    // Before `endpoint`, on purpose: the endpoint branch matches a bare "not
    // found", which would otherwise claim every provider that phrases a missing
    // model as "model not found" and send the operator off to check their base
    // URL instead of their model id.
    if haystack.contains("model_not_found")
        || (haystack.contains("not found") && haystack.contains("model"))
        || haystack.contains("does not exist")
        || haystack.contains("is not available")
        || haystack.contains("unknown model")
        || haystack.contains("invalid model")
    {
        return ProbeClass::Model;
    }

    if haystack.contains("quota")
        || haystack.contains("insufficient")
        || haystack.contains("billing")
        || haystack.contains("429")
        || haystack.contains("rate limit")
    {
        return ProbeClass::Quota;
    }

    // "404 / not found / DNS / refused" — all four, not the first two. A
    // connection that was refused and a name that did not resolve are the
    // clearest possible evidence that nothing is at that address, and reading
    // them as `unknown` sent the operator to look at their key instead of their
    // URL for the most common typo there is.
    if haystack.contains("404")
        || haystack.contains("not found")
        || haystack.contains("refused")
        || haystack.contains("unreachable")
        || haystack.contains("dns")
        || haystack.contains("no such host")
        || haystack.contains("could not resolve")
        || haystack.contains("name resolution")
        || haystack.contains("connection reset")
    {
        return ProbeClass::Endpoint;
    }

    if haystack.contains("timeout") || haystack.contains("timed out") {
        return ProbeClass::Timeout;
    }

    ProbeClass::Unknown
}

/// Whether meeting this class should undo the add, given what kind of provider
/// it was.
///
/// [`ProbeClass::destroys_credential`] answers the general rule: only a rejected
/// credential is evidence about the credential, so only that class rolls one
/// back. This adds the one category-specific exception, and it is in the design
/// this ports:
///
/// **A local runtime rolls back on an unreachable endpoint too.** A runtime that
/// is not running is not a connection worth creating — the operator's next move
/// is to start it and retry, not to keep a row that points at a port with
/// nothing behind it. For a cloud provider the same class means the opposite: a
/// proxy, a WAF or a slow gateway sits between a perfectly good key and an
/// endpoint that is fine, which is why that case keeps both.
///
/// The asymmetry is the point. `endpoint` against `127.0.0.1:11434` is a fact
/// about the operator's machine; `endpoint` against `api.acme.dev` is a fact
/// about the network in between.
pub fn rolls_back(class: ProbeClass, category: catalogue::Category) -> bool {
    if class.destroys_credential() {
        return true;
    }
    matches!(category, catalogue::Category::Local)
        && matches!(class, ProbeClass::Endpoint | ProbeClass::Timeout)
}

/// What to tell the operator, given a class and the provider's label.
///
/// **Never interpolates the raw upstream string.** That text can carry request
/// material — headers, fragments of a key — and this sentence lands in a banner
/// that gets screenshotted and pasted into a ticket. The raw text goes to a
/// detail or console channel instead.
///
/// Every sentence but the first begins with "Saved", because every class but
/// `auth` kept the record and the credential. The save is a fact; only
/// reachability is in question.
pub fn describe(class: ProbeClass, provider: &str) -> String {
    match class {
        ProbeClass::Auth => {
            format!("Could not reach {provider}: the provider rejected the credential.")
        }
        ProbeClass::Endpoint => format!("Saved, but nothing answered at {provider}."),
        ProbeClass::Model => "Saved. The endpoint did not recognise that model id.".to_string(),
        ProbeClass::Quota => "Saved. The account is out of credit.".to_string(),
        ProbeClass::Timeout => format!("Saved, but {provider} did not answer in time."),
        ProbeClass::Unknown => "Saved, but the check did not complete.".to_string(),
    }
}

/// What to tell the operator when the add was **undone**.
///
/// [`describe`] opens every sentence but one with "Saved", because for a cloud
/// provider every class but `auth` kept the record and the credential. Once
/// [`rolls_back`] can answer true for a second class, that wording becomes a
/// lie in exactly the case it is shown: a local runtime that is not running
/// rolls back, and telling the operator it was saved while no row appears is
/// worse than telling them nothing.
///
/// So the refusal path has its own sentences. Each names the next thing to do,
/// because in every one of these cases there is one.
pub fn describe_refusal(class: ProbeClass, subject: &str) -> String {
    match class {
        ProbeClass::Auth => {
            format!("Could not reach {subject}: the provider rejected the credential.")
        }
        ProbeClass::Endpoint => format!(
            "Nothing answered at {subject}, so it was not connected. Start it and try again."
        ),
        ProbeClass::Timeout => {
            format!("{subject} did not answer in time, so it was not connected.")
        }
        // Not reachable through `rolls_back` today. Answered rather than
        // panicked, because a future class joining the rollback set should
        // degrade to a true sentence rather than to a crash.
        ProbeClass::Model | ProbeClass::Quota | ProbeClass::Unknown => {
            format!("Could not verify {subject}, so it was not connected.")
        }
    }
}

/// Why an endpoint may not be probed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointRefusal {
    /// Not a URL this can read a host out of.
    Unparseable,
    /// Something other than `http` or `https`.
    Scheme,
    /// Loopback, and this deployment does not offer local runtimes.
    Loopback,
    /// A link-local or cloud metadata address.
    LinkLocal,
    /// A private or otherwise non-routable address.
    PrivateNetwork,
}

impl std::fmt::Display for EndpointRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparseable => write!(f, "that is not an endpoint address"),
            Self::Scheme => write!(f, "an endpoint must be http or https"),
            Self::Loopback => write!(f, "this host does not offer local model runtimes"),
            Self::LinkLocal => write!(f, "a model endpoint is never on a link-local address"),
            Self::PrivateNetwork => {
                write!(
                    f,
                    "a model endpoint is never on this host's private network"
                )
            }
        }
    }
}

/// Whether loopback is an acceptable probe target on this deployment.
///
/// It is an **explicit allowance**, made because the local-runtime category
/// exists and `ollama` needs it — not a hole left open. A server-side
/// deployment that offers no local runtimes passes `false` and loopback is
/// refused like any other non-routable address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbePolicy {
    /// Whether the local-runtime category is offered here at all.
    pub allow_loopback: bool,
}

/// Whether `url` may be probed.
///
/// Generalising "test this credential against this URL" to company scope creates
/// an authenticated *send a request to an arbitrary address* primitive, which is
/// SSRF-shaped. This is the answer, made explicitly rather than inherited:
///
/// * the scheme must be `http` or `https`;
/// * link-local and cloud metadata addresses (`169.254.0.0/16`, `fe80::/10`) are
///   refused outright — a company's model endpoint is never there, and that
///   range is where a container's credentials live;
/// * other private ranges are refused, because a model endpoint reachable only
///   from inside this host's network is this host's business, not a tenant's;
/// * loopback is allowed only where local runtimes are offered.
///
/// A hostname that is not a literal IP is allowed: resolving it here would be a
/// DNS lookup in a pure function, and a check performed before a resolve is
/// defeated by the resolve changing underneath it anyway. **Apply this to every
/// redirect target too** — a permitted host that redirects to the metadata
/// address is the whole trick.
pub fn check_endpoint(url: &str, policy: ProbePolicy) -> Result<(), EndpointRefusal> {
    let url = url.trim();
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(EndpointRefusal::Unparseable);
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return Err(EndpointRefusal::Scheme);
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host_port = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    let host = if let Some(after) = host_port.strip_prefix('[') {
        after.split_once(']').map(|(h, _)| h).unwrap_or(after)
    } else {
        host_port
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host_port)
    };
    let host = host.trim();
    if host.is_empty() {
        return Err(EndpointRefusal::Unparseable);
    }
    // A name, not a literal. See the doc comment: resolving here would make this
    // impure and would not close the window anyway.
    let Ok(ip) = host.parse::<IpAddr>() else {
        return Ok(());
    };
    check_address(ip, policy)
}

/// The address half of [`check_endpoint`], exposed so a redirect target can be
/// checked after it has been resolved.
pub fn check_address(ip: IpAddr, policy: ProbePolicy) -> Result<(), EndpointRefusal> {
    match ip {
        IpAddr::V4(v4) => check_v4(v4, policy),
        IpAddr::V6(v6) => {
            // An IPv4-mapped address is the same machine wearing a longer name,
            // so it gets the same answer. Checking only the v6 shape here is how
            // `::ffff:169.254.169.254` reaches a metadata service.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return check_v4(mapped, policy);
            }
            if v6.is_loopback() {
                return loopback(policy);
            }
            // fe80::/10 link-local, and fec0::/10 site-local.
            let first = v6.segments()[0];
            if (first & 0xffc0) == 0xfe80 || (first & 0xffc0) == 0xfec0 {
                return Err(EndpointRefusal::LinkLocal);
            }
            // fc00::/7 unique-local.
            if (v6.segments()[0] & 0xfe00) == 0xfc00 {
                return Err(EndpointRefusal::PrivateNetwork);
            }
            if v6 == Ipv6Addr::UNSPECIFIED {
                return Err(EndpointRefusal::PrivateNetwork);
            }
            Ok(())
        }
    }
}

fn check_v4(ip: Ipv4Addr, policy: ProbePolicy) -> Result<(), EndpointRefusal> {
    if ip.is_loopback() {
        return loopback(policy);
    }
    // 169.254.0.0/16 — link-local, and the address every cloud puts its
    // instance credentials behind.
    if ip.is_link_local() {
        return Err(EndpointRefusal::LinkLocal);
    }
    if ip.is_private() || ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() {
        return Err(EndpointRefusal::PrivateNetwork);
    }
    // 100.64.0.0/10, carrier-grade NAT — where a container network often lives.
    let [a, b, ..] = ip.octets();
    if a == 100 && (64..128).contains(&b) {
        return Err(EndpointRefusal::PrivateNetwork);
    }
    Ok(())
}

fn loopback(policy: ProbePolicy) -> Result<(), EndpointRefusal> {
    if policy.allow_loopback {
        Ok(())
    } else {
        Err(EndpointRefusal::Loopback)
    }
}

// ---- the IO half ------------------------------------------------------------
//
// Everything above this line is pure and testable with a string. Below it is the
// one network call this module makes, kept here rather than in a handler so that
// the guard, the caps and the classification travel together: a second caller
// that reached for `reqwest` directly would be a second, unguarded probe.

/// How long a probe may take in total, including connect, TLS and body.
///
/// The operator is watching a dialog spinner while this runs, so it is short. A
/// provider that cannot answer a catalog listing in ten seconds is a `timeout`,
/// which is a non-destructive class — nothing is lost by giving up early.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How much of a response body is read before the rest is discarded.
///
/// The body is wanted for one thing only — the wording a vendor puts in an error
/// — and 64 KiB is far more than any of them use. Without a cap, an endpoint
/// that streams indefinitely holds this connection open for the whole timeout
/// and buffers whatever it sent into this process's memory, once per probe.
const PROBE_BODY_CAP: usize = 64 * 1024;

/// How many redirects a probe will follow.
///
/// Three rather than `reqwest`'s default ten: a model catalog is a leaf
/// document, and a chain longer than a vendor's http→https plus a host move is
/// not a catalog, it is something worth refusing. **Every hop is re-checked
/// against [`check_endpoint`]** — a permitted host that redirects to the
/// metadata address is the entire SSRF trick, and a policy applied only to the
/// first URL would wave it through.
const PROBE_MAX_REDIRECTS: usize = 3;

/// Whether this deployment permits a probe at a loopback address.
///
/// **An explicit allowance, made because the local-runtime category exists.**
/// `ollama` and `lmstudio` are in the catalogue, an operator may genuinely run
/// one beside the host, and refusing loopback would make that category
/// unreachable. It is one function so that a deployment which drops the category
/// has one place to say so, rather than a boolean threaded through five call
/// sites and defaulted wrong in one of them.
pub fn default_policy() -> ProbePolicy {
    ProbePolicy {
        allow_loopback: !catalogue::LOCAL_RUNTIMES.is_empty(),
    }
}

/// A probe that did not succeed.
///
/// Carries the class *and* the raw upstream text, because they go to two
/// different places: the class decides what happens to the credential and what
/// the operator is told, while the raw text goes to a log and **never** into the
/// copy. It can echo request material — headers, fragments of a key — and the
/// sentence it would land in is one someone screenshots into a ticket.
#[derive(Clone, Debug)]
pub struct ProbeFailure {
    /// What the failure means.
    pub class: ProbeClass,
    /// The upstream text, for a detail or console channel only.
    pub raw: String,
}

impl ProbeFailure {
    /// Classifies a raw error string.
    fn from_raw(raw: String) -> Self {
        Self {
            class: classify(&raw),
            raw,
        }
    }

    /// Classifies on one string and remembers another.
    ///
    /// The two are different because **this probe's own URL ends in `/models`**.
    /// Interpolating it into the text the classifier reads makes every single
    /// probe failure contain the word "model", so a refused connection to
    /// `http://127.0.0.1:9/v1/models` classified as a missing *model id* and sent
    /// the operator off to check a model they never typed. The URL is worth
    /// having in a log and is poison in a classifier input.
    fn classified_as(classify_on: &str, raw: String) -> Self {
        Self {
            class: classify(classify_on),
            raw,
        }
    }

    /// A refusal by the SSRF guard, which is an endpoint fact rather than a
    /// credential one — so it keeps the key, like every class but `auth`.
    fn refused(refusal: EndpointRefusal) -> Self {
        Self {
            class: ProbeClass::Endpoint,
            raw: refusal.to_string(),
        }
    }
}

/// Applies a provider's credential to a request in the style that provider's
/// **native** API expects.
///
/// One helper, three callers (this probe, the catalog reader, the per-provider
/// test), because the alternative is three places to be wrong and only one of
/// them reachable from any given bug report.
///
/// The `anthropic-version` header is not optional decoration: Anthropic's native
/// API rejects a request without it as **malformed** — a `400`, not a `401`.
/// That is the diagnostic that distinguishes this from a bad key, and it is why
/// the symptom was a 400 on a key that was perfectly good.
///
/// Verified against Anthropic's own documentation
/// (`platform.claude.com/docs/en/api/models/list`), whose curl example is
/// exactly `-H 'anthropic-version: 2023-06-01' -H "X-Api-Key: …"`.
pub fn apply_auth(
    request: reqwest::RequestBuilder,
    auth: catalogue::AuthStyle,
    credential: Option<&str>,
) -> reqwest::RequestBuilder {
    let key = match credential.map(str::trim).filter(|k| !k.is_empty()) {
        Some(key) => key,
        // Nothing to present. A keyless local runtime is the ordinary case, and
        // sending an empty header would be worse than sending none.
        None => return request,
    };
    match auth {
        catalogue::AuthStyle::None => request,
        catalogue::AuthStyle::Bearer => request.bearer_auth(key),
        catalogue::AuthStyle::Anthropic => request
            .header("x-api-key", key)
            .header("anthropic-version", catalogue::ANTHROPIC_VERSION),
    }
}

/// Asks `{base_url}/models` what the endpoint serves.
///
/// This is the same cheap, read-only call the model picker needs anyway, which
/// is why it is the probe: connecting a provider and listing its models are the
/// same question asked twice, and a heavier "send a real completion" check would
/// charge the operator for the privilege of finding out their key works.
///
/// Returns the model ids on success. On failure the error is **classified**, and
/// only [`ProbeClass::Auth`] means the credential should be rolled back — see
/// the module header for why the naive "roll everything back" answer destroys
/// valid keys.
pub async fn probe_models(
    base_url: &str,
    credential: Option<&str>,
    auth: catalogue::AuthStyle,
    policy: ProbePolicy,
) -> Result<Vec<String>, ProbeFailure> {
    check_endpoint(base_url, policy).map_err(ProbeFailure::refused)?;
    let url = format!("{}/models", base_url.trim().trim_end_matches('/'));
    // What the failure text is allowed to say. `raw` reaches a host log, and a
    // log is disk — so an endpoint carrying userinfo must not be written into
    // one verbatim. The request itself still goes to `url`; only the sentence
    // about it is redacted.
    let named = catalogue::redact_endpoint(&url);

    // The redirect policy is where the guard earns its keep. `reqwest` resolves
    // and connects on our behalf, so the only place a redirect target can be
    // inspected is here, before the next request goes out.
    let redirect_policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= PROBE_MAX_REDIRECTS {
            return attempt.stop();
        }
        match check_endpoint(attempt.url().as_str(), policy) {
            Ok(()) => attempt.follow(),
            // `stop` rather than `error`: the caller then sees the redirect's
            // own status, which classifies as an endpoint problem — which is
            // what it is. Either way the request is never sent.
            Err(_) => attempt.stop(),
        }
    });

    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .redirect(redirect_policy)
        .build()
        .map_err(|e| ProbeFailure::from_raw(format!("could not build the probe client: {e}")))?;

    // The one non-bearer entry in the whole catalogue. A probe that assumed one
    // auth style would fail exactly one provider — the one people try first —
    // and would classify the result as `auth`, deleting a perfectly good key.
    let request = apply_auth(client.get(&url), auth, credential);

    let response = request.send().await.map_err(|e| {
        // Classified on the condition alone; the full error, URL and all, is
        // kept for the log. See `ProbeFailure::classified_as`.
        ProbeFailure::classified_as(transport_condition(&e), format!("{named}: {e}"))
    })?;
    let status = response.status();
    let body = read_capped(response).await;
    if !status.is_success() {
        // The body is included in the string the classifier reads, and only
        // there: vendors put "invalid api key" and "model not found" in the
        // body rather than the reason phrase, so classifying on the status
        // alone would read every one of them as `unknown`.
        let reason = format!(
            "{} {}: {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("error"),
            body.trim()
        );
        return Err(ProbeFailure::classified_as(
            &reason,
            format!("{named}: {reason}"),
        ));
    }
    Ok(parse_model_ids(&body))
}

/// The condition a transport failure classifies on.
///
/// Two problems with handing [`classify`] the error's own `Display`. It says
/// "error sending request for url (...)" and buries the cause, so a DNS failure
/// and a timeout read identically — and it **contains the URL**, which for this
/// probe always ends in `/models`, so every failure would carry the word
/// "model". Naming the condition in a short fixed phrase solves both.
fn transport_condition(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        return "timeout";
    }
    if error.is_connect() {
        // Covers DNS failure, connection refused and a TLS handshake that never
        // completed. All three are the same answer to the operator: nothing
        // usable is at that address.
        return "connection refused";
    }
    if error.is_redirect() {
        // The guard stopped the chain, or it was too long. Either way the
        // endpoint did not serve a catalog where it said it would.
        return "redirect not followed: unreachable";
    }
    "the check did not complete"
}

/// Reads at most [`PROBE_BODY_CAP`] bytes, discarding the rest.
///
/// Chunk by chunk rather than `text()`, because `text()` trusts the endpoint to
/// stop sending. A `Content-Length` header is not a promise either — it is
/// whatever the far side wrote.
async fn read_capped(mut response: reqwest::Response) -> String {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < PROBE_BODY_CAP {
        match response.chunk().await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            // A body that stops mid-stream is still worth classifying on what
            // did arrive — the status code is usually the whole signal anyway.
            Ok(None) | Err(_) => break,
        }
    }
    buf.truncate(PROBE_BODY_CAP);
    String::from_utf8_lossy(&buf).into_owned()
}

/// The model ids in an OpenAI-compatible `{ "data": [{ "id": ... }] }` body.
///
/// Deliberately forgiving: a probe asks *did this endpoint answer as a model
/// catalog*, and a body it cannot parse is a successful connection to something
/// that is not one. That is still a reachable endpoint, so it is not a failure —
/// the model field simply has nothing to offer, which the console already
/// handles for every endpoint that publishes no catalog.
fn parse_model_ids(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let entries = value
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| value.as_array());
    entries
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("id")
                        .and_then(|id| id.as_str())
                        .map(|id| id.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the four cases the branch order exists for -------------------------
    //
    // architecture.md names these four by hand, and they are the whole reason
    // the ordering is what it is. If one of them starts failing, the ordering
    // has been "simplified" back into a bug.

    #[test]
    fn a_407_proxy_challenge_is_unknown_not_auth() {
        // It contains the word "authentication". Check auth first and a
        // corporate proxy deletes a valid key.
        assert_eq!(
            classify("HTTP 407 Proxy Authentication Required"),
            ProbeClass::Unknown
        );
        assert!(!classify("HTTP 407 Proxy Authentication Required").destroys_credential());
    }

    #[test]
    fn a_bare_waf_403_is_unknown_not_auth() {
        // An unidentified intermediary saying "forbidden" is not proof the key
        // is bad. Cloudflare is named explicitly because it is the common one.
        assert_eq!(classify("error from cloudflare: 403"), ProbeClass::Unknown);
        assert_eq!(classify("502 Bad Gateway"), ProbeClass::Unknown);
    }

    #[test]
    fn a_403_with_credential_wording_is_auth() {
        assert_eq!(
            classify("403 Forbidden: your api key does not have permission"),
            ProbeClass::Auth
        );
        assert!(classify("403 Forbidden: invalid credential").destroys_credential());
    }

    #[test]
    fn a_status_code_inside_an_id_does_not_match() {
        // Word boundaries. Without them a request id or a model name carrying
        // these digits reads as a status code and deletes the operator's key.
        assert_eq!(classify("request id req_1403 failed"), ProbeClass::Unknown);
        assert_eq!(classify("trace 4032 aborted"), ProbeClass::Unknown);
        assert_eq!(classify("model gpt-4010 is odd"), ProbeClass::Unknown);
    }

    // ---- all six classes ----------------------------------------------------

    #[test]
    fn every_class_has_a_real_error_string_that_reaches_it() {
        let cases: &[(&str, ProbeClass)] = &[
            ("401 Unauthorized", ProbeClass::Auth),
            ("Incorrect API key provided", ProbeClass::Auth),
            ("invalid_api_key", ProbeClass::Auth),
            (
                "The model `gpt-5.6-sol-pro` does not exist",
                ProbeClass::Model,
            ),
            ("model_not_found", ProbeClass::Model),
            (
                "Model 'anthropic/claude-sonnet-5' is not available",
                ProbeClass::Model,
            ),
            ("You exceeded your current quota", ProbeClass::Quota),
            ("429 Too Many Requests", ProbeClass::Quota),
            ("insufficient credits", ProbeClass::Quota),
            ("404 Not Found", ProbeClass::Endpoint),
            ("dns error: not found", ProbeClass::Endpoint),
            ("operation timed out", ProbeClass::Timeout),
            ("request timeout after 10s", ProbeClass::Timeout),
            ("something nobody has seen before", ProbeClass::Unknown),
            ("", ProbeClass::Unknown),
        ];
        for (raw, expected) in cases {
            assert_eq!(classify(raw), *expected, "classifying {raw:?}");
        }
    }

    #[test]
    fn a_missing_model_is_not_read_as_a_missing_endpoint() {
        // The endpoint branch matches a bare "not found". Checked after `model`,
        // so this sends the operator to their model id rather than their URL.
        assert_eq!(
            classify("The model `acme-1` was not found"),
            ProbeClass::Model
        );
        // And a genuine endpoint miss still reaches `endpoint`.
        assert_eq!(classify("404 page not found"), ProbeClass::Endpoint);
    }

    #[test]
    fn exactly_one_class_deletes_the_credential() {
        let destructive: Vec<&str> = [
            ProbeClass::Auth,
            ProbeClass::Model,
            ProbeClass::Quota,
            ProbeClass::Endpoint,
            ProbeClass::Timeout,
            ProbeClass::Unknown,
        ]
        .into_iter()
        .filter(|c| c.destroys_credential())
        .map(|c| c.as_str())
        .collect();
        assert_eq!(destructive, vec!["auth"]);
    }

    #[test]
    fn classification_is_case_insensitive_and_ignores_surrounding_noise() {
        assert_eq!(classify("  \n401 UNAUTHORIZED\n "), ProbeClass::Auth);
    }

    // ---- the copy -----------------------------------------------------------

    #[test]
    fn the_copy_never_echoes_the_upstream_string() {
        // That text can carry headers or key fragments, and this sentence lands
        // in a screenshot-able banner.
        let raw = "401 Unauthorized: Bearer sk-not-a-real-key rejected";
        let sentence = describe(classify(raw), "Acme");
        assert!(!sentence.contains("sk-not-a-real-key"));
        assert!(!sentence.contains(raw));
    }

    #[test]
    fn only_the_destructive_class_fails_to_say_saved() {
        for class in [
            ProbeClass::Model,
            ProbeClass::Quota,
            ProbeClass::Endpoint,
            ProbeClass::Timeout,
            ProbeClass::Unknown,
        ] {
            assert!(
                describe(class, "Acme").starts_with("Saved"),
                "{class:?} kept the record, so its copy must say so"
            );
        }
        assert!(describe(ProbeClass::Auth, "Acme").starts_with("Could not reach Acme"));
    }

    // ---- the SSRF guard -----------------------------------------------------

    const LOCAL_OFFERED: ProbePolicy = ProbePolicy {
        allow_loopback: true,
    };
    const SERVER_SIDE: ProbePolicy = ProbePolicy {
        allow_loopback: false,
    };

    #[test]
    fn an_ordinary_endpoint_is_allowed() {
        assert_eq!(
            check_endpoint("https://api.openai.com/v1", SERVER_SIDE),
            Ok(())
        );
        assert_eq!(check_endpoint("https://8.8.8.8/v1", SERVER_SIDE), Ok(()));
    }

    #[test]
    fn only_http_and_https_are_probeable() {
        assert_eq!(
            check_endpoint("file:///etc/passwd", SERVER_SIDE),
            Err(EndpointRefusal::Scheme)
        );
        assert_eq!(
            check_endpoint("gopher://acme.test/v1", SERVER_SIDE),
            Err(EndpointRefusal::Scheme)
        );
        assert_eq!(
            check_endpoint("api.openai.com/v1", SERVER_SIDE),
            Err(EndpointRefusal::Unparseable)
        );
    }

    #[test]
    fn the_cloud_metadata_address_is_refused_wherever_it_is_offered() {
        // 169.254.169.254 is where a container's credentials live. There is no
        // deployment on which a company's model endpoint is there.
        for policy in [LOCAL_OFFERED, SERVER_SIDE] {
            assert_eq!(
                check_endpoint("http://169.254.169.254/latest/meta-data/", policy),
                Err(EndpointRefusal::LinkLocal)
            );
        }
    }

    #[test]
    fn link_local_is_refused_in_both_address_families() {
        assert_eq!(
            check_endpoint("http://169.254.1.1/v1", LOCAL_OFFERED),
            Err(EndpointRefusal::LinkLocal)
        );
        assert_eq!(
            check_endpoint("http://[fe80::1]/v1", LOCAL_OFFERED),
            Err(EndpointRefusal::LinkLocal)
        );
    }

    #[test]
    fn an_ipv4_mapped_ipv6_address_gets_the_ipv4_answer() {
        // Checking only the v6 shape is how `::ffff:169.254.169.254` reaches a
        // metadata service through a guard that looks like it works.
        assert_eq!(
            check_endpoint("http://[::ffff:169.254.169.254]/v1", LOCAL_OFFERED),
            Err(EndpointRefusal::LinkLocal)
        );
        assert_eq!(
            check_endpoint("http://[::ffff:127.0.0.1]:11434/v1", SERVER_SIDE),
            Err(EndpointRefusal::Loopback)
        );
    }

    #[test]
    fn loopback_is_an_explicit_allowance_not_a_hole() {
        // Allowed only where the local-runtime category is offered, because
        // that is exactly what Ollama needs.
        assert_eq!(
            check_endpoint("http://127.0.0.1:11434/v1", LOCAL_OFFERED),
            Ok(())
        );
        assert_eq!(
            check_endpoint("http://[::1]:11434/v1", LOCAL_OFFERED),
            Ok(())
        );
        assert_eq!(
            check_endpoint("http://127.0.0.1:11434/v1", SERVER_SIDE),
            Err(EndpointRefusal::Loopback)
        );
    }

    #[test]
    fn private_and_carrier_grade_ranges_are_refused() {
        for addr in [
            "http://10.0.0.5/v1",
            "http://192.168.1.10/v1",
            "http://172.16.0.1/v1",
            "http://100.64.0.1/v1",
            "http://0.0.0.0/v1",
        ] {
            assert_eq!(
                check_endpoint(addr, LOCAL_OFFERED),
                Err(EndpointRefusal::PrivateNetwork),
                "{addr}"
            );
        }
        assert_eq!(
            check_endpoint("http://[fc00::1]/v1", LOCAL_OFFERED),
            Err(EndpointRefusal::PrivateNetwork)
        );
    }

    #[test]
    fn a_redirect_target_gets_the_same_answer_as_the_first_hop() {
        // A permitted host that redirects to the metadata address is the whole
        // trick, so the address half is public for the redirect check to reuse.
        assert_eq!(check_endpoint("https://acme.test/v1", SERVER_SIDE), Ok(()));
        assert_eq!(
            check_address("169.254.169.254".parse().unwrap(), SERVER_SIDE),
            Err(EndpointRefusal::LinkLocal)
        );
    }

    #[test]
    fn userinfo_and_ports_do_not_hide_the_host() {
        assert_eq!(
            check_endpoint("http://user:pw@169.254.169.254:80/v1", SERVER_SIDE),
            Err(EndpointRefusal::LinkLocal)
        );
        assert_eq!(
            check_endpoint("http://169.254.169.254@example.test/v1", SERVER_SIDE),
            Ok(()),
            "the authority after the last @ is the real host"
        );
    }

    #[test]
    fn a_hostname_is_allowed_because_resolving_it_here_would_prove_nothing() {
        // A name resolved in a pure check is a DNS lookup in a pure function,
        // and the resolve can change underneath it anyway. The address check is
        // applied where the connection is actually made.
        assert_eq!(
            check_endpoint("https://localhost.acme.test/v1", SERVER_SIDE),
            Ok(())
        );
    }

    // ---- the IO half's pure helpers ----------------------------------------

    #[test]
    fn the_loopback_allowance_is_tied_to_the_local_runtime_category() {
        // Not a free-standing `true`. If the catalogue ever stops offering a
        // local runtime, the reason for the allowance is gone and so is the
        // allowance — one place to change rather than five call sites.
        assert_eq!(
            default_policy().allow_loopback,
            !catalogue::LOCAL_RUNTIMES.is_empty()
        );
    }

    #[test]
    fn a_guard_refusal_keeps_the_credential() {
        // The SSRF guard answers a question about the address. Treating it as
        // an auth failure would delete a key over a typo in a URL.
        let failure = ProbeFailure::refused(EndpointRefusal::LinkLocal);
        assert_eq!(failure.class, ProbeClass::Endpoint);
        assert!(!failure.class.destroys_credential());
    }

    #[test]
    fn model_ids_are_read_from_the_openai_shape_and_from_a_bare_array() {
        let wrapped = r#"{"data":[{"id":"gpt-5"},{"id":"gpt-5-mini"}]}"#;
        assert_eq!(parse_model_ids(wrapped), vec!["gpt-5", "gpt-5-mini"]);
        let bare = r#"[{"id":"llama3"}]"#;
        assert_eq!(parse_model_ids(bare), vec!["llama3"]);
    }

    #[test]
    fn a_body_that_is_not_a_catalog_is_an_empty_list_rather_than_a_failure() {
        // A 200 from something that is not a model listing is still a reachable
        // endpoint. Failing here would refuse every provider that does not
        // publish an OpenAI-shaped catalog, which the connect flow explicitly
        // supports adding.
        assert!(parse_model_ids("not json at all").is_empty());
        assert!(parse_model_ids(r#"{"models":["a"]}"#).is_empty());
        assert!(parse_model_ids(r#"{"data":[{"name":"no id here"}]}"#).is_empty());
    }

    #[test]
    fn a_transport_failure_says_which_condition_it_was() {
        // `reqwest`'s own Display buries the cause, so a DNS failure and a
        // timeout read identically and both classify as `unknown`. These fixed
        // phrases are what let `classify` tell them apart.
        assert_eq!(classify("timeout"), ProbeClass::Timeout);
        assert_eq!(classify("connection refused"), ProbeClass::Endpoint);
        assert_eq!(
            classify("redirect not followed: unreachable"),
            ProbeClass::Endpoint
        );
        assert_eq!(classify("the check did not complete"), ProbeClass::Unknown);
    }

    #[test]
    fn the_probes_own_url_never_reaches_the_classifier() {
        // This probe's URL always ends in `/models`, so interpolating it into
        // the classifier's input makes EVERY failure contain the word "model" —
        // and a refused connection classified as a missing model id, sending the
        // operator to check a model they never typed.
        let failure = ProbeFailure::classified_as(
            "connection refused",
            "http://127.0.0.1:9/v1/models: error sending request".to_string(),
        );
        assert_eq!(failure.class, ProbeClass::Endpoint);
        assert!(
            failure.raw.contains("/models"),
            "the URL is still worth having in a log"
        );
    }

    #[test]
    fn dns_and_refusal_are_endpoint_facts_not_unknowns() {
        for raw in [
            "connection refused",
            "no such host",
            "could not resolve host",
            "temporary failure in name resolution",
            "dns error",
            "network is unreachable",
            "connection reset by peer",
        ] {
            assert_eq!(
                classify(raw),
                ProbeClass::Endpoint,
                "`{raw}` is the clearest evidence there is that nothing is at that address"
            );
        }
    }

    #[test]
    fn a_local_runtime_that_is_not_running_is_not_a_connection_worth_keeping() {
        // The one category-specific exception to "only `auth` rolls back". A
        // runtime that is not listening is a fact about the operator's machine
        // and their next move is to start it — not to keep a row pointing at a
        // port with nothing behind it.
        for class in [ProbeClass::Endpoint, ProbeClass::Timeout] {
            assert!(rolls_back(class, catalogue::Category::Local), "{class:?}");
        }
    }

    #[test]
    fn the_same_class_against_a_cloud_provider_keeps_everything() {
        // And this asymmetry is the point: `endpoint` against a vendor's host
        // is a fact about the network in between — a proxy, a WAF, a slow
        // gateway — sitting between a perfectly good key and an endpoint that
        // is fine. Rolling back there is the bug the classifier exists to stop.
        for class in [ProbeClass::Endpoint, ProbeClass::Timeout, ProbeClass::Quota] {
            assert!(!rolls_back(class, catalogue::Category::Cloud), "{class:?}");
        }
    }

    #[test]
    fn auth_rolls_back_whatever_the_category() {
        for category in [
            catalogue::Category::Cloud,
            catalogue::Category::Local,
            catalogue::Category::Cli,
        ] {
            assert!(rolls_back(ProbeClass::Auth, category), "{category:?}");
        }
    }

    #[test]
    fn a_refusal_never_says_saved() {
        // `describe` opens every sentence but one with "Saved", which is true
        // when the row was kept. On the rollback path no row exists, and an
        // operator told it was saved while nothing appears has been lied to
        // about the one thing they can see.
        for class in [
            ProbeClass::Auth,
            ProbeClass::Endpoint,
            ProbeClass::Timeout,
            ProbeClass::Unknown,
        ] {
            let said = describe_refusal(class, "Ollama");
            assert!(!said.contains("Saved"), "{class:?}: {said}");
        }
    }

    #[test]
    fn a_refusal_names_the_next_thing_to_do() {
        assert!(describe_refusal(ProbeClass::Endpoint, "Ollama").contains("Start it"));
        assert!(
            describe_refusal(ProbeClass::Auth, "Groq").contains("rejected the credential"),
            "the auth sentence is unchanged — it was already right"
        );
    }

    // ---- auth style on the wire --------------------------------------------
    //
    // Asserted on the **headers actually sent**, not on a return value: this bug
    // was invisible to every test that only checked what a function returned,
    // because the function returned fine and the request was malformed.

    /// The headers one `apply_auth` call produces, as `(name, value)` pairs.
    fn headers_for(auth: catalogue::AuthStyle, key: Option<&str>) -> Vec<(String, String)> {
        let client = reqwest::Client::new();
        let request = apply_auth(client.get("https://example.test/v1/models"), auth, key);
        let built = request.build().expect("a request");
        built
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn anthropic_gets_x_api_key_and_a_version_and_no_authorization() {
        // A bearer with no `anthropic-version` is rejected by Anthropic's native
        // API as MALFORMED — a 400, not a 401 — which is the diagnostic that
        // tells a broken request from a bad key. The reported symptom was
        // exactly that 400 on a key that was fine.
        let headers = headers_for(
            catalogue::AuthStyle::Anthropic,
            Some("sk-ant-not-a-real-key"),
        );
        assert_eq!(header(&headers, "x-api-key"), Some("sk-ant-not-a-real-key"));
        assert_eq!(
            header(&headers, "anthropic-version"),
            Some(catalogue::ANTHROPIC_VERSION)
        );
        assert_eq!(
            header(&headers, "authorization"),
            None,
            "a bearer alongside x-api-key is the shape that was failing"
        );
    }

    #[test]
    fn a_bearer_provider_is_unchanged() {
        let headers = headers_for(catalogue::AuthStyle::Bearer, Some("sk-not-a-real-key"));
        assert_eq!(
            header(&headers, "authorization"),
            Some("Bearer sk-not-a-real-key")
        );
        assert_eq!(header(&headers, "x-api-key"), None);
        assert_eq!(header(&headers, "anthropic-version"), None);
    }

    #[test]
    fn a_provider_with_no_key_gets_no_auth_header_at_all() {
        // The keyless local runtime. An empty header is worse than none.
        for key in [None, Some(""), Some("   ")] {
            for auth in [
                catalogue::AuthStyle::Bearer,
                catalogue::AuthStyle::Anthropic,
                catalogue::AuthStyle::None,
            ] {
                let headers = headers_for(auth, key);
                assert_eq!(header(&headers, "authorization"), None, "{auth:?} {key:?}");
                assert_eq!(header(&headers, "x-api-key"), None, "{auth:?} {key:?}");
            }
        }
    }

    #[test]
    fn a_keyless_auth_style_sends_nothing_even_with_a_key() {
        // `AuthStyle::None` is a statement about the endpoint, not about whether
        // we happen to hold a credential.
        let headers = headers_for(catalogue::AuthStyle::None, Some("sk-not-a-real-key"));
        assert_eq!(header(&headers, "authorization"), None);
        assert_eq!(header(&headers, "x-api-key"), None);
    }
}
