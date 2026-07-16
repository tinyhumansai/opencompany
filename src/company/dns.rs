//! Custom-domain DNS records and verification.
//!
//! Pure record generation ([`dns_records`]) mirrors
//! `frontend/src/lib/domain.ts::dnsRecords` byte-for-byte so the operator
//! console renders the same rows the host generates: a verification `TXT`, a
//! mail `CNAME`, two DKIM `CNAME`s, and an SPF `TXT`. The verification token is
//! a deterministic 32-bit fold of the domain, matching the TypeScript hash so a
//! record generated on either side verifies against the other.
//!
//! Verification is dependency-inverted through the mockable [`DnsResolver`]
//! trait: the default build ships an offline [`StaticDnsResolver`] used by tests
//! and a real recursive resolver ([`HickoryDnsResolver`]) gated behind the
//! `dns` feature so the default build links no network crate.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;

/// The platform host custom-domain records point at. Mirrors
/// `PLATFORM_TARGET` in `domain.ts`.
const PLATFORM_TARGET: &str = "mail.opencompany.host";

/// A single DNS record the operator must add for a custom domain.
///
/// Serialized with `type` for the record kind so the JSON matches the console's
/// `DnsRecord` shape exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsRecord {
    /// The record kind, `"CNAME"` or `"TXT"`.
    #[serde(rename = "type")]
    pub record_type: String,
    /// The record name (host).
    pub name: String,
    /// The record value.
    pub value: String,
    /// The suggested TTL, as a string to match the console.
    pub ttl: String,
}

/// A short, stable verification token derived from the domain.
///
/// Reproduces the TypeScript fold in `domain.ts`:
/// `hash = (hash * 31 + charCodeAt(i)) | 0`, then
/// `Math.abs(hash).toString(16).padStart(8, "0")`. The `| 0` is 32-bit signed
/// wraparound (`i32::wrapping_*`); `Math.abs` of `i32::MIN` widens past the
/// signed range in JS the same way [`i32::unsigned_abs`] widens it here.
pub fn verify_token(domain: &str) -> String {
    let mut hash: i32 = 0;
    for unit in domain.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(unit as i32);
    }
    format!("{:08x}", hash.unsigned_abs())
}

/// Normalizes a domain for record generation: trimmed and with a trailing dot
/// stripped, matching `domain.ts`.
fn normalize(domain: &str) -> String {
    domain.trim().trim_end_matches('.').to_string()
}

/// The DNS records a user must add to point a custom domain at the platform and
/// let it send email (verification + mail CNAME + two DKIM CNAMEs + SPF).
///
/// An empty (or whitespace-only) domain yields no records, matching the console.
pub fn dns_records(domain: &str) -> Vec<DnsRecord> {
    let d = normalize(domain);
    if d.is_empty() {
        return Vec::new();
    }
    let txt = |name: String, value: String| DnsRecord {
        record_type: "TXT".to_string(),
        name,
        value,
        ttl: "3600".to_string(),
    };
    let cname = |name: String, value: String| DnsRecord {
        record_type: "CNAME".to_string(),
        name,
        value,
        ttl: "3600".to_string(),
    };
    vec![
        txt(
            format!("_opencompany.{d}"),
            format!("oc-verify={}", verify_token(&d)),
        ),
        cname(d.clone(), PLATFORM_TARGET.to_string()),
        cname(
            format!("oc1._domainkey.{d}"),
            "oc1.dkim.opencompany.host".to_string(),
        ),
        cname(
            format!("oc2._domainkey.{d}"),
            "oc2.dkim.opencompany.host".to_string(),
        ),
        txt(d, "v=spf1 include:spf.opencompany.host ~all".to_string()),
    ]
}

/// The persisted, non-secret status of a company's custom domain.
///
/// Stored as JSON at `SecretStore["__domain"]`. No credential material lives
/// here — a domain is public configuration; it shares the secret store only
/// because that is the per-company durable key/value seam.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainStatus {
    /// The configured custom domain, or empty when none is set.
    pub domain: String,
    /// Whether the last verification pass found every required record.
    pub verified: bool,
    /// The records the operator must add.
    pub records: Vec<DnsRecord>,
    /// Per-record verification outcome from the last `verify` pass, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<RecordCheck>>,
}

impl DomainStatus {
    /// Builds a fresh, unverified status for `domain` with generated records.
    pub fn fresh(domain: &str) -> Self {
        Self {
            domain: normalize(domain),
            verified: false,
            records: dns_records(domain),
            checks: None,
        }
    }
}

/// The verification outcome for one required record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordCheck {
    /// The record name that was queried.
    pub name: String,
    /// The record kind (`"TXT"` or `"CNAME"`).
    #[serde(rename = "type")]
    pub record_type: String,
    /// Whether the expected value was found at that name.
    pub found: bool,
}

/// The DNS lookup seam. Mockable so verification is exercised offline; the real
/// recursive resolver lives behind the `dns` feature.
#[async_trait]
pub trait DnsResolver: Send + Sync {
    /// Returns every TXT string at `name` (empty when the name has none).
    async fn txt(&self, name: &str) -> Result<Vec<String>>;
    /// Returns the CNAME target of `name`, if any.
    async fn cname(&self, name: &str) -> Result<Option<String>>;
}

/// Verifies `domain`'s records against `resolver`, returning an updated status.
///
/// A CNAME check tolerates a trailing dot (resolvers return FQDNs). A TXT check
/// passes when any string at the name equals the expected value.
pub async fn verify(domain: &str, resolver: &dyn DnsResolver) -> Result<DomainStatus> {
    let records = dns_records(domain);
    let mut checks = Vec::with_capacity(records.len());
    for record in &records {
        let found = match record.record_type.as_str() {
            "TXT" => resolver
                .txt(&record.name)
                .await?
                .iter()
                .any(|value| value == &record.value),
            "CNAME" => resolver
                .cname(&record.name)
                .await?
                .map(|target| target.trim_end_matches('.') == record.value.trim_end_matches('.'))
                .unwrap_or(false),
            _ => false,
        };
        checks.push(RecordCheck {
            name: record.name.clone(),
            record_type: record.record_type.clone(),
            found,
        });
    }
    let verified = checks.iter().all(|check| check.found);
    Ok(DomainStatus {
        domain: normalize(domain),
        verified,
        records,
        checks: Some(checks),
    })
}

/// An offline, in-memory resolver: exact-match TXT and CNAME tables. Used by
/// tests and any deployment that pre-seeds expected answers.
#[derive(Clone, Debug, Default)]
pub struct StaticDnsResolver {
    txt: std::collections::HashMap<String, Vec<String>>,
    cname: std::collections::HashMap<String, String>,
}

impl StaticDnsResolver {
    /// An empty resolver — every lookup misses.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a TXT string at `name`.
    pub fn with_txt(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.txt.entry(name.into()).or_default().push(value.into());
        self
    }

    /// Adds a CNAME target at `name`.
    pub fn with_cname(mut self, name: impl Into<String>, target: impl Into<String>) -> Self {
        self.cname.insert(name.into(), target.into());
        self
    }

    /// Seeds every record `dns_records(domain)` expects, so a verify pass over
    /// `domain` succeeds. A convenience for the happy-path test.
    pub fn fully_verifying(domain: &str) -> Self {
        let mut resolver = Self::new();
        for record in dns_records(domain) {
            match record.record_type.as_str() {
                "TXT" => resolver = resolver.with_txt(record.name, record.value),
                "CNAME" => resolver = resolver.with_cname(record.name, record.value),
                _ => {}
            }
        }
        resolver
    }
}

#[async_trait]
impl DnsResolver for StaticDnsResolver {
    async fn txt(&self, name: &str) -> Result<Vec<String>> {
        Ok(self.txt.get(name).cloned().unwrap_or_default())
    }
    async fn cname(&self, name: &str) -> Result<Option<String>> {
        Ok(self.cname.get(name).cloned())
    }
}

/// A real recursive DNS resolver backed by `hickory-resolver`. Gated behind the
/// `dns` feature so the default build links no network crate.
#[cfg(feature = "dns")]
pub struct HickoryDnsResolver {
    resolver: hickory_resolver::TokioAsyncResolver,
}

#[cfg(feature = "dns")]
impl HickoryDnsResolver {
    /// Builds a resolver from the system configuration, falling back to Google
    /// public DNS when none is available.
    pub fn from_system() -> Result<Self> {
        use hickory_resolver::TokioAsyncResolver;
        use hickory_resolver::config::{ResolverConfig, ResolverOpts};
        let resolver = match TokioAsyncResolver::tokio_from_system_conf() {
            Ok(resolver) => resolver,
            Err(_) => TokioAsyncResolver::tokio(ResolverConfig::google(), ResolverOpts::default()),
        };
        Ok(Self { resolver })
    }
}

#[cfg(feature = "dns")]
impl HickoryDnsResolver {
    /// Whether a resolve error is a benign "the name simply has no such record"
    /// (mapped to an empty answer, not a hard failure).
    fn is_no_records(err: &hickory_resolver::error::ResolveError) -> bool {
        use hickory_resolver::error::ResolveErrorKind;
        matches!(err.kind(), ResolveErrorKind::NoRecordsFound { .. })
    }
}

#[cfg(feature = "dns")]
#[async_trait]
impl DnsResolver for HickoryDnsResolver {
    async fn txt(&self, name: &str) -> Result<Vec<String>> {
        match self.resolver.txt_lookup(name).await {
            Ok(lookup) => Ok(lookup
                .iter()
                .flat_map(|txt| {
                    txt.iter()
                        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                })
                .collect()),
            Err(err) if Self::is_no_records(&err) => Ok(Vec::new()),
            Err(err) => Err(crate::error::OpenCompanyError::Store(format!(
                "dns txt lookup failed: {err}"
            ))),
        }
    }

    async fn cname(&self, name: &str) -> Result<Option<String>> {
        // A CNAME is followed transparently; read the canonical name from the
        // record set rather than the resolved A/AAAA data.
        use hickory_resolver::proto::rr::RecordType;
        match self.resolver.lookup(name, RecordType::CNAME).await {
            Ok(lookup) => Ok(lookup.record_iter().find_map(|record| {
                record
                    .data()
                    .and_then(|data| data.as_cname())
                    .map(|cname| cname.0.to_utf8().trim_end_matches('.').to_string())
            })),
            Err(err) if Self::is_no_records(&err) => Ok(None),
            Err(err) => Err(crate::error::OpenCompanyError::Store(format!(
                "dns cname lookup failed: {err}"
            ))),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn verify_token_matches_typescript_fold() {
        // Reproduces domain.ts::verifyToken for a concrete domain. The fold of
        // "acme.com" under (hash*31 + code) | 0 is deterministic; a change here
        // would drift the console and host apart.
        let token = verify_token("acme.com");
        assert_eq!(token.len(), 8);
        let mut hash: i32 = 0;
        for unit in "acme.com".encode_utf16() {
            hash = hash.wrapping_mul(31).wrapping_add(unit as i32);
        }
        assert_eq!(token, format!("{:08x}", hash.unsigned_abs()));
    }

    #[test]
    fn records_are_deterministic_and_five() {
        let a = dns_records("acme.com");
        let b = dns_records("acme.com");
        assert_eq!(a, b);
        assert_eq!(a.len(), 5);
        assert_eq!(a[0].record_type, "TXT");
        assert_eq!(a[0].name, "_opencompany.acme.com");
        assert!(a[0].value.starts_with("oc-verify="));
        assert_eq!(a[1].record_type, "CNAME");
        assert_eq!(a[1].value, PLATFORM_TARGET);
    }

    #[test]
    fn empty_domain_yields_no_records() {
        assert!(dns_records("   ").is_empty());
        assert!(dns_records("").is_empty());
    }

    #[test]
    fn trailing_dot_is_stripped() {
        assert_eq!(dns_records("acme.com."), dns_records("acme.com"));
    }

    #[test]
    fn record_serializes_type_key() {
        let record = &dns_records("acme.com")[0];
        let json = serde_json::to_value(record).unwrap();
        assert_eq!(json["type"], "TXT");
        assert_eq!(json["ttl"], "3600");
    }

    #[tokio::test]
    async fn verify_passes_when_all_records_present() {
        let resolver = StaticDnsResolver::fully_verifying("acme.com");
        let status = verify("acme.com", &resolver).await.unwrap();
        assert!(status.verified);
        assert_eq!(status.checks.as_ref().unwrap().len(), 5);
        assert!(status.checks.unwrap().iter().all(|c| c.found));
    }

    #[tokio::test]
    async fn verify_fails_when_a_record_missing() {
        // Seed everything but the verification TXT.
        let mut resolver = StaticDnsResolver::new();
        for record in dns_records("acme.com").into_iter().skip(1) {
            match record.record_type.as_str() {
                "TXT" => resolver = resolver.with_txt(record.name, record.value),
                "CNAME" => resolver = resolver.with_cname(record.name, record.value),
                _ => {}
            }
        }
        let status = verify("acme.com", &resolver).await.unwrap();
        assert!(!status.verified);
        let checks = status.checks.unwrap();
        assert!(!checks[0].found);
    }

    #[tokio::test]
    async fn verify_tolerates_trailing_dot_on_cname() {
        let resolver = StaticDnsResolver::fully_verifying("acme.com")
            .with_cname("acme.com", "mail.opencompany.host.");
        let status = verify("acme.com", &resolver).await.unwrap();
        let cname_check = status
            .checks
            .unwrap()
            .into_iter()
            .find(|c| c.name == "acme.com" && c.record_type == "CNAME")
            .unwrap();
        assert!(cname_check.found);
    }
}
