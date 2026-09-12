//! Which connected provider a company's agents actually search through.
//!
//! Pure: no IO, no store, no network. The caller reads the records and the
//! credential-presence booleans; this module makes the decision.
//!
//! # One active provider, not a routing table
//!
//! The inference surface this is shaped after can have several providers live at
//! once, because a routing entry names one per workload. Search cannot: every
//! provider's canonical web search is aliased to the single tool name
//! `web_search` in [`crate::harness::built_in::search_byo`], because the shipped
//! research skills name that tool in their instructions and a belt where the
//! name appears and disappears with a settings change is how an agent starts
//! inventing URLs instead of searching.
//!
//! So the default marker is stored and resolved exactly as inference's is, and
//! it means something stronger: the marked provider is the **only** one used,
//! and every other connected provider is a stored credential standing ready.

use super::configuration_complete;
use super::store::SearchProvider;

/// One connected provider together with whether its credential is present.
///
/// The credential itself is never here — only the boolean. Assembled by the
/// caller from [`super::store::provider_key_configured`], which asks the store
/// rather than reading a flag that could go stale against a cleared secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The record.
    pub provider: SearchProvider,
    /// Whether a credential is stored for it. **Never the credential.**
    pub has_key: bool,
}

impl Candidate {
    /// Whether this provider has everything it needs to answer a search.
    pub fn is_complete(&self) -> bool {
        configuration_complete(
            &self.provider.slug,
            self.has_key,
            self.provider.endpoint.is_some(),
        )
    }

    /// Whether this provider could be chosen as the active one.
    fn is_usable(&self) -> bool {
        self.provider.enabled && self.is_complete()
    }
}

/// The provider this company's agents search through, or `None` for managed.
///
/// ```text
///   marked, and it is enabled and complete   ──▶  the marked provider
///   marked, enabled, but INCOMPLETE          ──▶  None  (managed)
///   marked but missing or disabled           ──▶  first usable
///   nothing marked                           ──▶  first usable
///   nothing usable                           ──▶  None  (managed)
/// ```
///
/// The second line is the one worth arguing about, and it is deliberate. A
/// marked provider whose key was removed falls back to **managed**, not to the
/// next connected provider: the operator chose which account pays for their
/// searches, and silently moving that spend to a different account because the
/// chosen one lost its credential is precisely the class of surprise this
/// feature exists to remove. The row says it needs a key; nothing guesses.
///
/// Where nothing was ever chosen there is no such choice to respect, so the
/// first usable provider answers — which is what an unmarked company resolved to
/// before any of this existed, so nothing is backfilled and nobody moves.
pub fn active<'a>(candidates: &'a [Candidate], marked: Option<&str>) -> Option<&'a Candidate> {
    // Falling out of this `if` is the disabled case, which is normally
    // unreachable: both write paths clear the marker when they disable or delete
    // the marked provider, so reaching it means the store was edited directly.
    // Fall through to the first usable provider rather than failing.
    if let Some(slug) = marked
        && let Some(found) = candidates.iter().find(|c| c.provider.slug == slug)
        && found.provider.enabled
    {
        return found.is_complete().then_some(found);
    }
    candidates.iter().find(|candidate| candidate.is_usable())
}

/// The slug that actually answers: the active provider's, or `managed`.
///
/// **The one derivation** of that answer, called by the status route, the
/// capabilities panel and the harness alike. Two surfaces mirroring each other's
/// rule would drift the first time a provider was added, leaving one page saying
/// a company searches through Exa while its agents search through the platform —
/// which is the warning the predecessor of this function already carried.
pub fn effective_slug(active: Option<&Candidate>) -> &str {
    match active {
        Some(candidate) => &candidate.provider.slug,
        None => super::MANAGED_PROVIDER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(slug: &str, enabled: bool, has_key: bool) -> Candidate {
        Candidate {
            provider: SearchProvider {
                slug: slug.to_string(),
                enabled,
                endpoint: None,
            },
            has_key,
        }
    }

    fn searxng(enabled: bool, endpoint: Option<&str>) -> Candidate {
        Candidate {
            provider: SearchProvider {
                slug: "searxng".to_string(),
                enabled,
                endpoint: endpoint.map(str::to_string),
            },
            has_key: false,
        }
    }

    #[test]
    fn nothing_connected_is_managed() {
        assert_eq!(effective_slug(active(&[], None)), "managed");
        assert_eq!(effective_slug(active(&[], Some("exa"))), "managed");
    }

    #[test]
    fn the_marked_provider_wins_when_it_is_enabled_and_complete() {
        let candidates = [candidate("exa", true, true), candidate("brave", true, true)];
        assert_eq!(
            effective_slug(active(&candidates, Some("brave"))),
            "brave",
            "the marker, not the list order"
        );
    }

    #[test]
    fn an_unmarked_company_uses_the_first_usable_provider() {
        let candidates = [
            candidate("exa", true, false),
            candidate("brave", true, true),
        ];
        assert_eq!(
            effective_slug(active(&candidates, None)),
            "brave",
            "the keyless exa entry is skipped rather than resolving to nothing"
        );
    }

    #[test]
    fn a_marked_provider_that_lost_its_key_falls_back_to_managed_not_to_a_sibling() {
        // The spend stays where the operator put it, or it goes nowhere. It does
        // not quietly move to another company account.
        let candidates = [
            candidate("exa", true, false),
            candidate("brave", true, true),
        ];
        assert_eq!(effective_slug(active(&candidates, Some("exa"))), "managed");
    }

    #[test]
    fn a_marked_provider_that_was_disabled_out_of_band_falls_through() {
        let candidates = [
            candidate("exa", false, true),
            candidate("brave", true, true),
        ];
        assert_eq!(effective_slug(active(&candidates, Some("exa"))), "brave");
    }

    #[test]
    fn a_marker_naming_nothing_falls_through() {
        let candidates = [candidate("brave", true, true)];
        assert_eq!(effective_slug(active(&candidates, Some("gone"))), "brave");
    }

    #[test]
    fn everything_disabled_is_managed() {
        let candidates = [
            candidate("exa", false, true),
            candidate("brave", false, true),
        ];
        assert_eq!(effective_slug(active(&candidates, None)), "managed");
    }

    #[test]
    fn searxng_is_complete_on_an_endpoint_and_needs_no_key() {
        assert!(searxng(true, Some("https://search.acme.internal")).is_complete());
        assert!(!searxng(true, None).is_complete());
        let candidates = [searxng(true, Some("https://search.acme.internal"))];
        assert_eq!(effective_slug(active(&candidates, None)), "searxng");
    }

    #[test]
    fn an_account_provider_is_not_complete_on_an_endpoint_alone() {
        let mut exa = candidate("exa", true, false);
        exa.provider.endpoint = Some("https://api.exa.ai".to_string());
        assert!(!exa.is_complete());
    }
}
