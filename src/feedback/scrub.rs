//! The **normative** privacy scrubber (`docs/spec/feedback-loop/privacy.md`).
//!
//! Feedback is filed to a *public* issue tracker, so this module is the
//! non-negotiable gate between a company's private context and the internet. It
//! removes or masks every redaction class before anything can leave the machine:
//!
//! | Class | Strategy |
//! | --- | --- |
//! | Secrets | any [`SecretStore`] value present, or a high-entropy token, **aborts** filing |
//! | Wallet material | key-shaped tokens **abort**; addresses masked (`sol:…abcd`) |
//! | Personal data | emails/phones → placeholders; roster names → `⟨redacted:name⟩` |
//! | Customer content | never included by construction (excerpts are runtime output only) |
//! | Charter specifics | prices/rules/mission → structural descriptions |
//!
//! Scrubbing **fails closed**: if a class cannot be evaluated (e.g. the secret
//! store is unreadable), filing is [`Aborted`](ScrubOutcome::Aborted), never
//! risked. The returned [`Ready`](ScrubOutcome::Ready) body is byte-exact what
//! the preview gate shows and what is posted.

use crate::Result;
use crate::ports::SecretStore;
use crate::ports::types::CompanyId;

/// A charter specific and the structural description that replaces it.
///
/// The literal (a price, a client name, a never-do rule, mission text) is
/// replaced wholesale by its structural description so the issue conveys shape
/// without leaking the specific.
#[derive(Clone, Debug)]
pub struct CharterTerm {
    /// The literal string to redact from the body.
    pub literal: String,
    /// The structural description that replaces it (e.g. "a priced skill").
    pub description: String,
}

impl CharterTerm {
    /// Builds a charter term.
    pub fn new(literal: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            literal: literal.into(),
            description: description.into(),
        }
    }
}

/// The outcome of scrubbing a candidate issue body.
#[derive(Clone, Debug, PartialEq)]
pub enum ScrubOutcome {
    /// The body is safe to file, byte-exact as returned.
    Ready(String),
    /// Filing is blocked; `reason` is safe to show the operator (it names the
    /// class, never the offending value).
    Aborted {
        /// Why filing was blocked.
        reason: String,
    },
}

/// Minimum length for the entropy scan and wallet-address masking.
const MIN_ENTROPY_LEN: usize = 32;
/// Shannon-entropy threshold (bits/char) above which a long token aborts filing.
const ENTROPY_ABORT_BITS: f64 = 3.5;
/// Length at or above which a base58 token is treated as key material (abort).
const WALLET_KEY_LEN: usize = 64;
/// Length range for base58 tokens masked as wallet addresses.
const WALLET_ADDR_MIN: usize = 32;

/// Scrubs a candidate issue `body` for one company, failing closed.
///
/// * `secret_keys` — the [`SecretStore`] keys whose values must not appear
///   (channel HMACs, GitHub/TinyHumans credentials). Any unreadable key aborts.
/// * `roster` — agent handles/names redacted to `⟨redacted:name⟩`.
/// * `charter_terms` — literals replaced by structural descriptions.
pub async fn scrub(
    body: &str,
    company: &CompanyId,
    secrets: &dyn SecretStore,
    secret_keys: &[String],
    roster: &[String],
    charter_terms: &[CharterTerm],
) -> Result<ScrubOutcome> {
    // 1. Secrets (fail closed). Read every declared key; an unreadable store
    //    aborts rather than risks. A present secret value aborts outright.
    for key in secret_keys {
        let value = match secrets.get(company, key).await {
            Ok(value) => value,
            // The class cannot be evaluated: fail closed.
            Err(_) => {
                return Ok(ScrubOutcome::Aborted {
                    reason: "secret store unreadable".to_string(),
                });
            }
        };
        if let Some(value) = value {
            let secret = value.expose();
            if !secret.trim().is_empty() && body.contains(secret) {
                return Ok(ScrubOutcome::Aborted {
                    reason: "a secret value is present in the report".to_string(),
                });
            }
        }
    }

    // 2. Wallet key material aborts (before the entropy scan, so a base58 key
    //    is reported as wallet material rather than a generic high-entropy
    //    token, and before masking addresses).
    if contains_wallet_key(body) {
        return Ok(ScrubOutcome::Aborted {
            reason: "wallet key material is present".to_string(),
        });
    }

    // High-entropy token scan: catches unregistered secrets (API keys pasted in).
    if high_entropy_token(body).is_some() {
        return Ok(ScrubOutcome::Aborted {
            reason: "a high-entropy token that may be a secret is present".to_string(),
        });
    }

    // 3+. Build the redacted body. Wallet addresses masked, charter specifics
    //     replaced, then personal data redacted.
    let mut out = mask_wallet_addresses(body);
    for term in charter_terms {
        if !term.literal.trim().is_empty() {
            out = replace_all(&out, &term.literal, &term.description);
        }
    }
    out = redact_emails(&out);
    out = redact_phones(&out);
    out = redact_names(&out, roster);

    Ok(ScrubOutcome::Ready(out))
}

/// Splits a string into candidate secret/wallet tokens, trimming surrounding
/// punctuation so `"key=ABC123,"` yields `ABC123`.
fn tokens(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '"' | '\'' | '=' | '(' | ')'))
        .map(|t| t.trim_matches(|c: char| matches!(c, '.' | ':' | '<' | '>' | '[' | ']')))
        .filter(|t| !t.is_empty())
}

/// Returns the first token that looks like a secret by Shannon entropy.
fn high_entropy_token(s: &str) -> Option<&str> {
    tokens(s).find(|token| {
        token.len() >= MIN_ENTROPY_LEN
            && token.chars().all(is_secret_char)
            && shannon_bits_per_char(token) > ENTROPY_ABORT_BITS
    })
}

/// Characters that can appear in a base64/hex/base58 secret token.
fn is_secret_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '_' | '-')
}

/// Shannon entropy of `s` in bits per character.
fn shannon_bits_per_char(s: &str) -> f64 {
    let mut counts = std::collections::HashMap::new();
    let len = s.chars().count() as f64;
    if len == 0.0 {
        return 0.0;
    }
    for ch in s.chars() {
        *counts.entry(ch).or_insert(0u32) += 1;
    }
    counts
        .values()
        .map(|&count| {
            let p = count as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// The base58 alphabet (Bitcoin/Solana): no `0`, `O`, `I`, `l`.
fn is_base58(c: char) -> bool {
    c.is_ascii_alphanumeric() && !matches!(c, '0' | 'O' | 'I' | 'l')
}

/// Whether any token is a base58 string long enough to be a private key.
fn contains_wallet_key(s: &str) -> bool {
    tokens(s).any(|token| token.len() >= WALLET_KEY_LEN && token.chars().all(is_base58))
}

/// Masks wallet addresses (`sol:<addr>` and bare base58 addresses) to
/// `sol:…<last4>`, leaving key-shaped tokens for the abort path.
fn mask_wallet_addresses(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // Split preserving delimiters so structure is retained.
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if !token.is_empty() {
            out.push_str(&mask_one_wallet(token));
            token.clear();
        }
    };
    for ch in s.chars() {
        if ch.is_whitespace() || matches!(ch, ',' | ';' | '"' | '\'' | '(' | ')') {
            flush(&mut token, &mut out);
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    flush(&mut token, &mut out);
    out
}

/// Masks a single token if it is a wallet address; otherwise returns it as-is.
fn mask_one_wallet(token: &str) -> String {
    // `sol:<addr>` prefix form.
    if let Some(addr) = token.strip_prefix("sol:")
        && addr.len() >= 4
        && addr.chars().all(is_base58)
    {
        return format!("sol:…{}", &addr[addr.len() - 4..]);
    }
    // Bare base58 address (shorter than a key).
    let core = token.trim_matches(|c: char| matches!(c, '.' | ':' | '<' | '>' | '[' | ']'));
    if (WALLET_ADDR_MIN..WALLET_KEY_LEN).contains(&core.len()) && core.chars().all(is_base58) {
        return format!("sol:…{}", &core[core.len() - 4..]);
    }
    token.to_string()
}

/// Replaces every non-overlapping occurrence of `needle` in `haystack`.
fn replace_all(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    haystack.replace(needle, replacement)
}

/// Redacts email addresses to `⟨redacted:email⟩`.
fn redact_emails(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            // Expand left over the local part.
            let mut start = i;
            while start > 0 && is_email_local(chars[start - 1]) {
                start -= 1;
            }
            // Expand right over the domain.
            let mut end = i + 1;
            while end < chars.len() && is_email_domain(chars[end]) {
                end += 1;
            }
            let domain: String = chars[i + 1..end].iter().collect();
            let has_dot = domain.contains('.') && !domain.ends_with('.');
            if start < i && has_dot {
                // Drop the already-emitted local part, then the whole address.
                let local_len = i - start;
                for _ in 0..local_len {
                    out.pop();
                }
                out.push_str("⟨redacted:email⟩");
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn is_email_local(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

fn is_email_domain(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-')
}

/// Redacts phone-number-shaped runs (10–15 digits, phone punctuation) to
/// `⟨redacted:phone⟩`.
fn redact_phones(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if is_phone_char(chars[i]) {
            let start = i;
            let mut digits = 0;
            while i < chars.len() && is_phone_char(chars[i]) {
                if chars[i].is_ascii_digit() {
                    digits += 1;
                }
                i += 1;
            }
            if (10..=15).contains(&digits) {
                out.push_str("⟨redacted:phone⟩");
            } else {
                out.extend(&chars[start..i]);
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn is_phone_char(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, ' ' | '-' | '(' | ')' | '+')
}

/// Redacts roster names (case-insensitive, whole-word) to `⟨redacted:name⟩`.
fn redact_names(s: &str, roster: &[String]) -> String {
    let mut out = s.to_string();
    for name in roster {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        out = replace_whole_word_ci(&out, name, "⟨redacted:name⟩");
    }
    out
}

/// Replaces whole-word, case-insensitive occurrences of `word`.
fn replace_whole_word_ci(haystack: &str, word: &str, replacement: &str) -> String {
    let hay_lower = haystack.to_lowercase();
    let word_lower = word.to_lowercase();
    let hay: Vec<char> = haystack.chars().collect();
    let hay_l: Vec<char> = hay_lower.chars().collect();
    let needle: Vec<char> = word_lower.chars().collect();
    if needle.is_empty() || needle.len() > hay_l.len() {
        return haystack.to_string();
    }
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < hay.len() {
        if i + needle.len() <= hay_l.len() && hay_l[i..i + needle.len()] == needle[..] {
            let before_ok = i == 0 || !is_word_char(hay[i - 1]);
            let after_idx = i + needle.len();
            let after_ok = after_idx >= hay.len() || !is_word_char(hay[after_idx]);
            if before_ok && after_ok {
                out.push_str(replacement);
                i = after_idx;
                continue;
            }
        }
        out.push(hay[i]);
        i += 1;
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::types::SecretValue;
    use async_trait::async_trait;

    /// A secret store with canned values, plus an "unreadable" mode that returns
    /// an error to exercise the fail-closed path.
    struct FakeSecrets {
        value: Option<String>,
        unreadable: bool,
    }

    #[async_trait]
    impl SecretStore for FakeSecrets {
        async fn get(&self, _company: &CompanyId, _key: &str) -> Result<Option<SecretValue>> {
            if self.unreadable {
                return Err(crate::OpenCompanyError::Store("boom".into()));
            }
            Ok(self.value.clone().map(SecretValue))
        }
        async fn set(&self, _company: &CompanyId, _key: &str, _value: SecretValue) -> Result<()> {
            Ok(())
        }
    }

    fn company() -> CompanyId {
        CompanyId::new("acme")
    }

    async fn scrub_with(secrets: FakeSecrets, body: &str) -> ScrubOutcome {
        scrub(
            body,
            &company(),
            &secrets,
            &["github_token".to_string()],
            &["Dana Roe".to_string()],
            &[CharterTerm::new("25.00", "a priced skill")],
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn present_secret_value_aborts() {
        let secrets = FakeSecrets {
            value: Some("ghp_supersecretvalue".into()),
            unreadable: false,
        };
        let out = scrub_with(secrets, "the token ghp_supersecretvalue leaked").await;
        assert!(matches!(out, ScrubOutcome::Aborted { reason } if reason.contains("secret value")));
    }

    #[tokio::test]
    async fn unreadable_secret_store_fails_closed() {
        let secrets = FakeSecrets {
            value: None,
            unreadable: true,
        };
        let out = scrub_with(secrets, "nothing sensitive here").await;
        assert!(matches!(out, ScrubOutcome::Aborted { reason } if reason.contains("unreadable")));
    }

    #[tokio::test]
    async fn high_entropy_token_aborts() {
        let secrets = FakeSecrets {
            value: None,
            unreadable: false,
        };
        // A 40-char random-looking token.
        let out = scrub_with(secrets, "key aB3xQ9zK7mN2pR5tV8wY1cE4gH6jL0oS3uD7fI2n").await;
        assert!(matches!(out, ScrubOutcome::Aborted { reason } if reason.contains("high-entropy")));
    }

    #[tokio::test]
    async fn wallet_private_key_aborts() {
        let secrets = FakeSecrets {
            value: None,
            unreadable: false,
        };
        // A 66-char base58-only token (key-shaped).
        let key =
            "5Kb8kLf9zgWQnogidDA76MzPL6TsZZY36hWXMssSzNydYXYB9KF2".to_string() + "aBcDeFgHjKmNpQ";
        let out = scrub_with(secrets, &format!("seed {key} here")).await;
        assert!(matches!(out, ScrubOutcome::Aborted { reason } if reason.contains("wallet key")));
    }

    #[tokio::test]
    async fn wallet_address_is_masked() {
        let secrets = FakeSecrets {
            value: None,
            unreadable: false,
        };
        let out = scrub_with(
            secrets,
            "pay sol:9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM now",
        )
        .await;
        match out {
            ScrubOutcome::Ready(body) => {
                assert!(body.contains("sol:…"), "got {body}");
                assert!(!body.contains("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM"));
                assert!(body.ends_with("AWWM now") || body.contains("AWWM"));
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn email_and_name_are_redacted() {
        let secrets = FakeSecrets {
            value: None,
            unreadable: false,
        };
        let out = scrub_with(secrets, "dana@acme.co said Dana Roe was unhappy").await;
        match out {
            ScrubOutcome::Ready(body) => {
                assert!(body.contains("⟨redacted:email⟩"), "got {body}");
                assert!(!body.contains("dana@acme.co"));
                assert!(body.contains("⟨redacted:name⟩"));
                assert!(!body.contains("Dana Roe"));
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn phone_is_redacted() {
        let secrets = FakeSecrets {
            value: None,
            unreadable: false,
        };
        let out = scrub_with(secrets, "call +1 (415) 555-2671 today").await;
        match out {
            ScrubOutcome::Ready(body) => {
                assert!(body.contains("⟨redacted:phone⟩"), "got {body}");
                assert!(!body.contains("555-2671"));
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn charter_term_becomes_structural() {
        let secrets = FakeSecrets {
            value: None,
            unreadable: false,
        };
        let out = scrub_with(secrets, "we charge 25.00 for audits").await;
        match out {
            ScrubOutcome::Ready(body) => {
                assert!(body.contains("a priced skill"), "got {body}");
                assert!(!body.contains("25.00"));
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn clean_body_is_ready_byte_exact() {
        let secrets = FakeSecrets {
            value: None,
            unreadable: false,
        };
        let body = "The invoice route returned the wrong total.";
        let out = scrub_with(secrets, body).await;
        assert_eq!(out, ScrubOutcome::Ready(body.to_string()));
    }
}
