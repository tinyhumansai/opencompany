//! How this instance obtains its TinyHumans credential.
//!
//! A hosted tenant holds **no** static secret. The platform gives its pod a
//! short-lived, audience-bound token in a file that the kubelet rewrites **in
//! place** (roughly every 8 minutes), so the credential must be *obtained* per
//! request rather than read once at boot into a `String`.
//!
//! [`TinyhumansTokenSource`] is that seam. It has exactly two tiers, in strict
//! precedence order:
//!
//! 1. **[`Tier::ProjectedFile`]** — the path named by [`TOKEN_FILE_ENV`]. The
//!    file is re-read whenever the cached copy is past its window, so a rotation
//!    is picked up without a restart. A permanent cache here would be a latent
//!    outage: the pod would keep presenting a token the cluster already rotated
//!    away from. The platform mounts it read-only as
//!    `/var/run/secrets/tinyhumans.ai/token` with a 600-second expiry; the env var
//!    carries the **path**, never a token value.
//! 2. **[`Tier::Static`]** — the long-lived [`API_KEY_ENV`] value. This keeps
//!    `docker compose` development alive and serves the (explicitly
//!    **unsupported**) self-host case. It is not going away.
//!
//! ## Expiry is read, never trusted
//!
//! The cache window is derived from the token's own `exp` claim, parsed
//! **without verifying the signature** — this process needs the expiry only, and
//! the backend is the party that verifies. A token whose `exp` cannot be read
//! (not a JWT, malformed payload, no `exp`) is never cached: correctness beats a
//! saved file read. An already-expired token is still returned — refusing to
//! send it would turn a recoverable 401 into a local outage — but likewise never
//! cached.
//!
//! ## Degrading to the static tier
//!
//! The projected tier is only selected when the named path actually exists. The
//! docker runtime injects nothing, so a leftover [`TOKEN_FILE_ENV`] pointing at
//! a path that was never mounted must fall through to the static tier rather
//! than failing every request. Once the tier *is* selected, a later read failure
//! is a hard error: silently swapping to a different identity mid-life is worse
//! than a loud failure.
//!
//! ## Rejected tokens
//!
//! Callers invalidate the cache with [`TinyhumansTokenSource::invalidate`] when
//! the backend rejects a bearer (401), so the very next request re-reads the file
//! instead of waiting out the window with a token the cluster has moved past.
//!
//! ## Redaction
//!
//! Nothing here renders the token. [`Debug`] shows the tier (and, for a
//! projected file, the path — not a secret); [`TinyhumansTokenSource::describe`]
//! is the operator-facing one-liner the doctor prints.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::Result;
use crate::app::config::EnvSource;
use crate::error::OpenCompanyError;

/// Environment variable naming the platform-projected token file. Set by the
/// platform (a kubelet-projected, audience-bound ServiceAccount token volume) —
/// never by an operator, and never present in `docker compose`.
pub const TOKEN_FILE_ENV: &str = "TINYHUMANS_TOKEN_FILE";

/// Environment variable holding a long-lived static TinyHumans API key. The
/// docker-development credential, and the unsupported self-host escape hatch.
pub const API_KEY_ENV: &str = "TINYHUMANS_API_KEY";

/// Hard ceiling on how long a projected token is cached, regardless of how much
/// TTL it claims to have left. A projected file rotates on the platform's
/// schedule, not ours, so the ceiling bounds how stale a bearer can get even if
/// a token advertises hours of validity.
pub const MAX_CACHE_WINDOW: Duration = Duration::from_secs(60);

/// Fraction of a token's remaining TTL we are willing to cache for, so a
/// re-read always happens with time left on the clock.
const TTL_FRACTION: f64 = 0.8;

/// Which tier a token was obtained from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenTier {
    /// A platform-projected file that rotates in place ([`TOKEN_FILE_ENV`]).
    ProjectedFile,
    /// A long-lived static key ([`API_KEY_ENV`]).
    Static,
}

impl TokenTier {
    /// The stable wire/diagnostic spelling of this tier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProjectedFile => "projected_file",
            Self::Static => "static",
        }
    }

    /// How this tier is named on the console read plane.
    pub fn credential_source(self) -> CredentialSource {
        match self {
            Self::ProjectedFile => CredentialSource::Attested,
            Self::Static => CredentialSource::Static,
        }
    }
}

impl std::fmt::Display for TokenTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a company's outbound credential comes from, as the console renders it.
/// Carries no secret and no path — just the shape of the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialSource {
    /// The pod presents a platform-minted, audience-bound identity. Nothing is
    /// stored on this instance.
    Attested,
    /// A static key/token is configured — the docker-development path, or a
    /// per-company token an operator pasted in.
    Static,
    /// No credential can be obtained at all.
    None,
}

impl CredentialSource {
    /// The stable wire spelling (`attested` / `static` / `none`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attested => "attested",
            Self::Static => "static",
            Self::None => "none",
        }
    }
}

impl std::fmt::Display for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A projected token cached until `good_until`.
struct Cached {
    token: String,
    good_until: SystemTime,
}

/// The resolved tier, with whatever state it needs to keep answering.
enum Tier {
    /// A rotating file plus the currently-cached read of it.
    ProjectedFile {
        path: PathBuf,
        cache: Mutex<Option<Cached>>,
    },
    /// A static value held in memory for the life of the process.
    Static { token: String },
}

/// How this instance obtains its TinyHumans credential — see the module docs.
///
/// Construct once and share behind an `Arc`: the projected-file tier keeps a
/// small cache, so cloning the source would clone the cache and defeat it.
pub struct TinyhumansTokenSource {
    tier: Tier,
}

impl TinyhumansTokenSource {
    /// A source that re-reads `path` as it rotates.
    pub fn projected_file(path: impl Into<PathBuf>) -> Self {
        Self {
            tier: Tier::ProjectedFile {
                path: path.into(),
                cache: Mutex::new(None),
            },
        }
    }

    /// A source over a static, long-lived key.
    pub fn static_key(token: impl Into<String>) -> Self {
        Self {
            tier: Tier::Static {
                token: token.into(),
            },
        }
    }

    /// Resolves the source from the environment, honouring the documented
    /// precedence **[`TOKEN_FILE_ENV`] > [`API_KEY_ENV`]**, or `None` when
    /// neither is configured (fail closed — the caller keeps its offline brain).
    ///
    /// The projected file wins deliberately: a pod that has been handed a cluster
    /// identity must present *that*, even if a stale static key is still lying
    /// around in its environment. It is chosen only when the named path **exists**
    /// — the docker runtime mounts nothing, so a leftover variable degrades to the
    /// static tier (with a warning) instead of failing every request.
    pub fn from_env(env: &dyn EnvSource) -> Option<Self> {
        Self::from_parts(
            env.get(TOKEN_FILE_ENV).as_deref().map(Path::new),
            env.get(API_KEY_ENV).as_deref(),
        )
    }

    /// The precedence rule itself, over already-resolved parts.
    ///
    /// [`Self::from_env`] is this function plus an environment read. Callers that
    /// already hold the resolved values — `RuntimeConfig`, which parsed them at
    /// load — must route through here rather than re-deriving the rule, because a
    /// second copy is a second chance to forget the existence check and answer
    /// `attested` for a path the runtime never mounted.
    ///
    /// Both inputs are trimmed; blank is treated as absent.
    pub fn from_parts(token_file: Option<&Path>, api_key: Option<&str>) -> Option<Self> {
        if let Some(path) = token_file {
            let path = Path::new(path.as_os_str().to_str().map(str::trim).unwrap_or_default());
            if !path.as_os_str().is_empty() {
                if path.exists() {
                    return Some(Self::projected_file(path));
                }
                tracing::warn!(
                    token_file = %path.display(),
                    "{TOKEN_FILE_ENV} names a path that does not exist; falling back to the static credential tier"
                );
            }
        }
        let key = api_key?.trim();
        if key.is_empty() {
            return None;
        }
        Some(Self::static_key(key))
    }

    /// The tier a set of resolved parts selects, without building a source.
    ///
    /// `has_static_credential` stands in for a held secret the caller must not
    /// hand over (`RuntimeConfig` keeps its key behind a redacting wrapper), so
    /// the static tier is reported from its presence rather than its value.
    pub fn source_of_parts(
        token_file: Option<&Path>,
        has_static_credential: bool,
    ) -> CredentialSource {
        match Self::from_parts(token_file, None) {
            Some(source) => source.credential_source(),
            None if has_static_credential => CredentialSource::Static,
            None => CredentialSource::None,
        }
    }

    /// Which tier this source resolved to.
    pub fn tier(&self) -> TokenTier {
        match &self.tier {
            Tier::ProjectedFile { .. } => TokenTier::ProjectedFile,
            Tier::Static { .. } => TokenTier::Static,
        }
    }

    /// How the console names this source.
    pub fn credential_source(&self) -> CredentialSource {
        self.tier().credential_source()
    }

    /// The projected token file, when this is the projected-file tier.
    pub fn token_file(&self) -> Option<&Path> {
        match &self.tier {
            Tier::ProjectedFile { path, .. } => Some(path.as_path()),
            Tier::Static { .. } => None,
        }
    }

    /// An operator-facing, credential-free one-liner for the doctor: the active
    /// tier, plus the file path when there is one. Never the token.
    pub fn describe(&self) -> String {
        match &self.tier {
            Tier::ProjectedFile { path, .. } => {
                format!("projected_file ({})", path.display())
            }
            Tier::Static { .. } => "static (set)".to_string(),
        }
    }

    /// Folds this source's *identity* into `hasher` for a roster fingerprint.
    ///
    /// The projected-file tier contributes its **tier + path**, never the token
    /// value: the whole point is that the token rotates every few minutes while
    /// the identity behind it does not. Hashing the value there would rebuild
    /// every agent's tool roster on the platform's rotation schedule. The static
    /// tier contributes its **value**, because for a static key a changed value
    /// *is* a changed identity (an operator rotated it) and must rebuild.
    pub fn hash_identity<H: std::hash::Hasher>(&self, hasher: &mut H) {
        use std::hash::Hash;
        match &self.tier {
            Tier::ProjectedFile { path, .. } => {
                0u8.hash(hasher);
                path.hash(hasher);
            }
            Tier::Static { token } => {
                1u8.hash(hasher);
                token.hash(hasher);
            }
        }
    }

    /// Drops any cached read, so the next [`Self::current`] goes back to the
    /// file. Called when the backend rejects a bearer (401): the cluster may have
    /// rotated the token early, and waiting out the cache window would keep
    /// presenting the rejected one. A no-op for the static tier.
    pub fn invalidate(&self) {
        if let Tier::ProjectedFile { cache, .. } = &self.tier {
            *cache.lock().expect("token cache poisoned") = None;
        }
    }

    /// The credential to present on the next outbound request.
    ///
    /// For the static tier this is the configured value. For the projected-file
    /// tier it is the cached read while the cache window holds, and otherwise a
    /// fresh read of the file — which is how an in-place rotation is picked up.
    pub async fn current(&self) -> Result<String> {
        self.current_at(SystemTime::now()).await
    }

    /// [`Self::current`] against an explicit clock, so the cache window can be
    /// exercised deterministically in tests.
    pub async fn current_at(&self, now: SystemTime) -> Result<String> {
        let (path, cache) = match &self.tier {
            Tier::Static { token } => return Ok(token.clone()),
            Tier::ProjectedFile { path, cache } => (path, cache),
        };

        // Scoped so the guard is dropped before the await below.
        {
            let guard = cache.lock().expect("token cache poisoned");
            if let Some(cached) = guard.as_ref()
                && cached.good_until > now
            {
                return Ok(cached.token.clone());
            }
        }

        let raw = tokio::fs::read_to_string(path).await.map_err(|e| {
            OpenCompanyError::Config(format!(
                "could not read the projected TinyHumans token file {}: {e}",
                path.display()
            ))
        })?;
        let token = raw.trim().to_string();
        if token.is_empty() {
            return Err(OpenCompanyError::Config(format!(
                "the projected TinyHumans token file {} is empty",
                path.display()
            )));
        }

        // A token we cannot date, or one already past `exp`, is never cached.
        let window = cache_window(now, unverified_jwt_exp(&token));
        *cache.lock().expect("token cache poisoned") = window.map(|window| Cached {
            token: token.clone(),
            good_until: now + window,
        });
        Ok(token)
    }
}

/// One resolved outbound credential: nothing, a value someone gave us, or a
/// [`TinyhumansTokenSource`] that must be read per request.
///
/// This is the seam that keeps a rotating token from being flattened into a
/// `String` at build time. Anything holding a credential holds *this*, resolves
/// it with [`Self::current`] on the request path, and asks
/// [`Self::configured`]/[`Self::source`] for status — so a rotation never needs a
/// rebuild and status never needs the value.
#[derive(Clone, Default)]
pub enum Credential {
    /// No credential — omit the bearer entirely (e.g. a keyless local Ollama).
    #[default]
    None,
    /// A value held in memory: a per-company token an operator pasted into the
    /// secret store, or a BYOK key. A changed value is a changed identity.
    Value(String),
    /// A platform token source. Read per request; the value it yields rotates
    /// while the identity behind it does not.
    Source(std::sync::Arc<TinyhumansTokenSource>),
}

impl Credential {
    /// A credential from a stored value, treating blank as "not configured".
    pub fn from_value(value: impl Into<String>) -> Self {
        let value = value.into();
        if value.trim().is_empty() {
            Self::None
        } else {
            Self::Value(value)
        }
    }

    /// A credential over a shared token source.
    pub fn from_source(source: std::sync::Arc<TinyhumansTokenSource>) -> Self {
        Self::Source(source)
    }

    /// Whether a credential can be obtained — the non-secret status the read
    /// planes surface. Never returns the value.
    pub fn configured(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Which tier this credential comes from, as the console names it.
    pub fn source(&self) -> CredentialSource {
        match self {
            Self::None => CredentialSource::None,
            Self::Value(_) => CredentialSource::Static,
            Self::Source(source) => source.credential_source(),
        }
    }

    /// The bearer to present on the next request, or `None` to omit the header.
    /// A [`Self::Source`] is read here — that is what makes a rotation take
    /// effect without a rebuild.
    pub async fn current(&self) -> Result<Option<String>> {
        let token = match self {
            Self::None => return Ok(None),
            Self::Value(value) => value.clone(),
            Self::Source(source) => source.current().await?,
        };
        let token = token.trim().to_string();
        Ok(if token.is_empty() { None } else { Some(token) })
    }

    /// Forwards a rejection (401) to the underlying source so the next request
    /// re-reads it. A no-op for a value we were handed.
    pub fn invalidate(&self) {
        if let Self::Source(source) = self {
            source.invalidate();
        }
    }

    /// Folds this credential's *identity* into `hasher` for a roster
    /// fingerprint — see [`TinyhumansTokenSource::hash_identity`] for why a
    /// projected source contributes its path rather than its rotating value.
    pub fn hash_identity<H: std::hash::Hasher>(&self, hasher: &mut H) {
        use std::hash::Hash;
        match self {
            Self::None => 0u8.hash(hasher),
            Self::Value(value) => {
                1u8.hash(hasher);
                value.hash(hasher);
            }
            Self::Source(source) => {
                2u8.hash(hasher);
                source.hash_identity(hasher);
            }
        }
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("Credential(<unset>)"),
            Self::Value(_) => f.write_str("Credential(<redacted>)"),
            Self::Source(source) => write!(f, "Credential({})", source.describe()),
        }
    }
}

impl std::fmt::Debug for TinyhumansTokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.tier {
            Tier::ProjectedFile { path, .. } => f
                .debug_struct("TinyhumansTokenSource")
                .field("tier", &"projected_file")
                .field("path", path)
                .finish(),
            Tier::Static { token } => f
                .debug_struct("TinyhumansTokenSource")
                .field("tier", &"static")
                .field(
                    "token",
                    &if token.is_empty() {
                        "<unset>"
                    } else {
                        "<redacted>"
                    },
                )
                .finish(),
        }
    }
}

/// How long a token read at `now` may be cached: 80% of its remaining TTL,
/// capped at [`MAX_CACHE_WINDOW`]. `None` — meaning "do not cache" — when the
/// expiry is unknown, already passed, or so close that no window is left.
fn cache_window(now: SystemTime, expiry: Option<SystemTime>) -> Option<Duration> {
    let remaining = expiry?.duration_since(now).ok()?;
    let window = remaining.mul_f64(TTL_FRACTION).min(MAX_CACHE_WINDOW);
    if window.is_zero() { None } else { Some(window) }
}

/// The `exp` claim of a JWT, read **without verifying the signature**.
///
/// This process needs the expiry to schedule a re-read; the backend is the party
/// that verifies the token, so validating here would buy nothing and would need
/// the cluster's public keys. `None` for anything that is not a three-segment
/// JWT with a numeric `exp` — the caller then declines to cache.
pub fn unverified_jwt_exp(token: &str) -> Option<SystemTime> {
    let mut segments = token.trim().split('.');
    let _header = segments.next()?;
    let payload = segments.next()?;
    // A JWS has exactly three segments; anything else is not a token we can read.
    segments.next()?;
    if segments.next().is_some() {
        return None;
    }
    let claims: serde_json::Value = serde_json::from_slice(&base64url_decode(payload)?).ok()?;
    let exp = claims.get("exp")?.as_u64()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(exp))
}

/// Decodes unpadded base64url (RFC 4648 §5) — the JWT segment encoding.
/// `None` on any character outside the alphabet.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for byte in input.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            // Tolerate the padded form even though JWT segments omit it.
            b'=' => break,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::config::MapEnv;

    /// Builds an unsigned-but-well-shaped JWT carrying `claims` as its payload.
    fn jwt(claims: serde_json::Value) -> String {
        let encode = |bytes: &[u8]| {
            const ALPHA: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let b = [
                    chunk[0],
                    chunk.get(1).copied().unwrap_or(0),
                    chunk.get(2).copied().unwrap_or(0),
                ];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                let take = chunk.len() + 1;
                for i in 0..take {
                    out.push(ALPHA[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
                }
            }
            out
        };
        format!(
            "{}.{}.{}",
            encode(br#"{"alg":"RS256"}"#),
            encode(claims.to_string().as_bytes()),
            "c2lnbmF0dXJl"
        )
    }

    fn unix(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// The caller must hold the returned handle: it owns the enclosing
    /// directory and removes it on drop.
    fn temp_file(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("oc-tokensrc-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join(name);
        (dir, path)
    }

    // ---- tier precedence ---------------------------------------------------

    #[test]
    fn projected_file_beats_static_key() {
        let (_path_dir, path) = temp_file("token");
        std::fs::write(&path, "projected").unwrap();
        let env = MapEnv::new([
            (TOKEN_FILE_ENV, path.display().to_string()),
            (API_KEY_ENV, "th_static".to_string()),
        ]);
        let source = TinyhumansTokenSource::from_env(&env).expect("configured");
        assert_eq!(source.tier(), TokenTier::ProjectedFile);
        assert_eq!(source.token_file(), Some(path.as_path()));
        assert_eq!(source.credential_source(), CredentialSource::Attested);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// The docker runtime mounts nothing. A leftover `TINYHUMANS_TOKEN_FILE`
    /// pointing at a path that was never projected must degrade to the static
    /// tier — not select a tier that can only ever fail.
    #[test]
    fn a_token_file_that_does_not_exist_degrades_to_the_static_tier() {
        let env = MapEnv::new([
            (TOKEN_FILE_ENV, "/nonexistent/oc/token"),
            (API_KEY_ENV, "th_static"),
        ]);
        let source = TinyhumansTokenSource::from_env(&env).expect("configured");
        assert_eq!(source.tier(), TokenTier::Static);

        // With nothing to fall back to, there is simply no source.
        let alone = MapEnv::new([(TOKEN_FILE_ENV, "/nonexistent/oc/token")]);
        assert!(TinyhumansTokenSource::from_env(&alone).is_none());
    }

    #[test]
    fn static_key_is_the_fallback_tier() {
        let env = MapEnv::new([(API_KEY_ENV, "th_static")]);
        let source = TinyhumansTokenSource::from_env(&env).expect("configured");
        assert_eq!(source.tier(), TokenTier::Static);
        assert!(source.token_file().is_none());
        assert_eq!(source.credential_source(), CredentialSource::Static);
    }

    #[test]
    fn neither_configured_resolves_to_none() {
        assert!(TinyhumansTokenSource::from_env(&MapEnv::default()).is_none());
        // Blank values are not configuration.
        let blank = MapEnv::new([(TOKEN_FILE_ENV, "   "), (API_KEY_ENV, "  ")]);
        assert!(TinyhumansTokenSource::from_env(&blank).is_none());
        // A blank token file falls through to the static key rather than
        // resolving to a source that can never read anything.
        let fallthrough = MapEnv::new([(TOKEN_FILE_ENV, " "), (API_KEY_ENV, "th_static")]);
        assert_eq!(
            TinyhumansTokenSource::from_env(&fallthrough)
                .expect("configured")
                .tier(),
            TokenTier::Static
        );
    }

    #[tokio::test]
    async fn static_tier_returns_its_configured_value() {
        let source = TinyhumansTokenSource::static_key("th_static");
        assert_eq!(source.current().await.unwrap(), "th_static");
    }

    // ---- projected-file rotation + cache window ----------------------------

    /// The kubelet rewrites the projected file **in place**. Once the cache
    /// window closes, the next call must present the new token — a permanent
    /// cache here is the latent outage this tier exists to avoid.
    #[tokio::test]
    async fn projected_file_picks_up_an_in_place_rotation() {
        let (_path_dir, path) = temp_file("token");
        // exp 1000s out at t=0 → window = min(0.8 * 1000, 60) = 60s.
        std::fs::write(&path, jwt(serde_json::json!({ "exp": 1000 }))).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        let source = TinyhumansTokenSource::projected_file(&path);

        assert_eq!(source.current_at(unix(0)).await.unwrap(), first);

        // Rotation lands. Inside the window the cached copy is still served —
        // that is what keeps the hot path off the filesystem.
        std::fs::write(&path, jwt(serde_json::json!({ "exp": 2000 }))).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert_ne!(first, second, "the rotation must change the file");
        assert_eq!(
            source.current_at(unix(59)).await.unwrap(),
            first,
            "inside the cache window the previous read is reused"
        );

        // Past the window the file is re-read and the rotated token is served.
        assert_eq!(
            source.current_at(unix(61)).await.unwrap(),
            second,
            "past the cache window the rotated token must be picked up"
        );

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// The window is driven by the token's own expiry: 80% of the remaining TTL,
    /// capped at [`MAX_CACHE_WINDOW`], and never for a token we cannot date.
    #[test]
    fn cache_window_is_eighty_percent_of_remaining_ttl_capped_at_a_minute() {
        // 10s left → 8s window (well under the cap).
        assert_eq!(
            cache_window(unix(0), Some(unix(10))),
            Some(Duration::from_secs(8))
        );
        // 8 minutes left (the projected-token rotation period) → capped at 60s.
        assert_eq!(
            cache_window(unix(0), Some(unix(480))),
            Some(MAX_CACHE_WINDOW)
        );
        // Already expired, or expiring exactly now → never cache.
        assert_eq!(cache_window(unix(100), Some(unix(90))), None);
        assert_eq!(cache_window(unix(100), Some(unix(100))), None);
        // Unknown expiry → never cache.
        assert_eq!(cache_window(unix(0), None), None);
    }

    /// A rejected bearer (401) must not wait out the cache window: invalidating
    /// sends the next call straight back to the file.
    #[tokio::test]
    async fn invalidate_forces_a_re_read_inside_the_cache_window() {
        let (_path_dir, path) = temp_file("token");
        std::fs::write(&path, jwt(serde_json::json!({ "exp": 1000 }))).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        let source = TinyhumansTokenSource::projected_file(&path);
        assert_eq!(source.current_at(unix(0)).await.unwrap(), first);

        std::fs::write(&path, jwt(serde_json::json!({ "exp": 1001 }))).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        // Same instant, so the window is wide open — but the cache is gone.
        source.invalidate();
        assert_eq!(source.current_at(unix(0)).await.unwrap(), second);

        // Harmless on the static tier.
        TinyhumansTokenSource::static_key("k").invalidate();
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// A token whose expiry cannot be read is re-read on **every** call. Slower,
    /// but it can never serve a credential the platform already rotated away.
    #[tokio::test]
    async fn an_undatable_token_is_never_cached() {
        let (_path_dir, path) = temp_file("token");
        std::fs::write(&path, "opaque-not-a-jwt").unwrap();
        let source = TinyhumansTokenSource::projected_file(&path);
        assert_eq!(
            source.current_at(unix(0)).await.unwrap(),
            "opaque-not-a-jwt"
        );

        std::fs::write(&path, "opaque-rotated").unwrap();
        assert_eq!(
            source.current_at(unix(0)).await.unwrap(),
            "opaque-rotated",
            "with no readable expiry every call re-reads the file"
        );

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// An expired token is still presented — the backend decides, and refusing
    /// locally would turn a recoverable 401 into an outage — but never cached.
    #[tokio::test]
    async fn an_expired_token_is_served_but_not_cached() {
        let (_path_dir, path) = temp_file("token");
        std::fs::write(&path, jwt(serde_json::json!({ "exp": 50 }))).unwrap();
        let stale = std::fs::read_to_string(&path).unwrap();
        let source = TinyhumansTokenSource::projected_file(&path);
        assert_eq!(source.current_at(unix(100)).await.unwrap(), stale);

        std::fs::write(&path, jwt(serde_json::json!({ "exp": 9000 }))).unwrap();
        let fresh = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            source.current_at(unix(100)).await.unwrap(),
            fresh,
            "an expired token must not have been cached"
        );

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn a_missing_or_empty_token_file_is_a_config_error() {
        let missing = TinyhumansTokenSource::projected_file("/nonexistent/oc/token");
        let err = missing.current().await.expect_err("unreadable");
        assert_eq!(err.code(), "config_error");

        let (_path_dir, path) = temp_file("token");
        std::fs::write(&path, "   \n").unwrap();
        let empty = TinyhumansTokenSource::projected_file(&path);
        assert_eq!(
            empty.current().await.expect_err("empty").code(),
            "config_error"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    // ---- fingerprint identity ---------------------------------------------

    fn identity_of(source: &TinyhumansTokenSource) -> u64 {
        use std::hash::Hasher;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        source.hash_identity(&mut hasher);
        hasher.finish()
    }

    /// The load-bearing fingerprint contract: a kubelet rotation changes the
    /// token but NOT the identity, so the agents' tool roster is not rebuilt
    /// every few minutes. A tier change or a static-value change does move it.
    #[test]
    fn identity_is_stable_across_rotation_and_moves_on_tier_or_value_change() {
        let projected = TinyhumansTokenSource::projected_file("/var/run/token");
        let same_path = TinyhumansTokenSource::projected_file("/var/run/token");
        assert_eq!(
            identity_of(&projected),
            identity_of(&same_path),
            "the same projected path is the same identity whatever the file holds"
        );

        let other_path = TinyhumansTokenSource::projected_file("/var/run/other");
        assert_ne!(identity_of(&projected), identity_of(&other_path));

        let static_a = TinyhumansTokenSource::static_key("key-a");
        let static_b = TinyhumansTokenSource::static_key("key-b");
        assert_ne!(
            identity_of(&static_a),
            identity_of(&static_b),
            "a rotated static key IS a new identity"
        );
        assert_ne!(
            identity_of(&projected),
            identity_of(&static_a),
            "a tier change must move the identity"
        );
    }

    // ---- redaction ---------------------------------------------------------

    #[test]
    fn debug_and_describe_never_render_the_token() {
        let source = TinyhumansTokenSource::static_key("th_super_secret_value");
        for rendered in [format!("{source:?}"), source.describe()] {
            assert!(!rendered.contains("th_super_secret_value"), "{rendered}");
        }
        assert!(format!("{source:?}").contains("<redacted>"));

        let projected = TinyhumansTokenSource::projected_file("/var/run/secrets/token");
        assert!(projected.describe().contains("/var/run/secrets/token"));
        assert!(projected.describe().contains("projected_file"));
    }

    // ---- jwt / base64url ---------------------------------------------------

    #[test]
    fn unverified_jwt_exp_reads_the_claim_without_verifying() {
        assert_eq!(
            unverified_jwt_exp(&jwt(serde_json::json!({ "exp": 1893456000u64 }))),
            Some(unix(1_893_456_000))
        );
        // No exp, not a JWT, wrong segment count, non-numeric exp → unreadable.
        assert!(unverified_jwt_exp(&jwt(serde_json::json!({ "aud": "x" }))).is_none());
        assert!(unverified_jwt_exp("not-a-jwt").is_none());
        assert!(unverified_jwt_exp("a.b").is_none());
        assert!(unverified_jwt_exp("a.b.c.d").is_none());
        assert!(unverified_jwt_exp(&jwt(serde_json::json!({ "exp": "soon" }))).is_none());
        // A payload that is not base64url at all.
        assert!(unverified_jwt_exp("aaa.!!!!.bbb").is_none());
    }

    #[test]
    fn base64url_decodes_the_unpadded_alphabet() {
        assert_eq!(base64url_decode("aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64url_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64url_decode("").unwrap(), Vec::<u8>::new());
        // `-` and `_` are the url-safe substitutions for `+` and `/`.
        assert_eq!(base64url_decode("--__").unwrap(), vec![0xFB, 0xEF, 0xFF]);
        assert!(base64url_decode("a+b/c").is_none());
    }

    // ---- Credential --------------------------------------------------------

    #[tokio::test]
    async fn credential_variants_report_status_and_resolve() {
        use std::sync::Arc;

        let none = Credential::None;
        assert!(!none.configured());
        assert_eq!(none.source(), CredentialSource::None);
        assert_eq!(none.current().await.unwrap(), None);

        // Blank is not configuration.
        assert!(!Credential::from_value("   ").configured());

        let value = Credential::from_value("pasted-token");
        assert!(value.configured());
        assert_eq!(value.source(), CredentialSource::Static);
        assert_eq!(
            value.current().await.unwrap().as_deref(),
            Some("pasted-token")
        );

        let (_path_dir, path) = temp_file("token");
        std::fs::write(&path, "projected-token").unwrap();
        let source =
            Credential::from_source(Arc::new(TinyhumansTokenSource::projected_file(&path)));
        assert!(source.configured());
        assert_eq!(source.source(), CredentialSource::Attested);
        assert_eq!(
            source.current().await.unwrap().as_deref(),
            Some("projected-token")
        );
        // The value it yields follows the file, per call.
        std::fs::write(&path, "rotated-token").unwrap();
        assert_eq!(
            source.current().await.unwrap().as_deref(),
            Some("rotated-token")
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn credential_debug_never_renders_a_value() {
        let rendered = format!("{:?}", Credential::from_value("super-secret"));
        assert!(!rendered.contains("super-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"));
        assert!(format!("{:?}", Credential::None).contains("<unset>"));
    }

    /// The tier's DTO spelling is a wire contract the console switches on.
    #[test]
    fn credential_source_wire_spellings_are_stable() {
        assert_eq!(CredentialSource::Attested.as_str(), "attested");
        assert_eq!(CredentialSource::Static.as_str(), "static");
        assert_eq!(CredentialSource::None.as_str(), "none");
        assert_eq!(
            serde_json::to_string(&CredentialSource::Attested).unwrap(),
            "\"attested\""
        );
        assert_eq!(TokenTier::ProjectedFile.as_str(), "projected_file");
        assert_eq!(TokenTier::Static.as_str(), "static");
    }
}
