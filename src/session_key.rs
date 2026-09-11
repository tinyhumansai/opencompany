//! The name an agent's openhuman session answers to.
//!
//! # Why this is a module and not a `format!` at the builder
//!
//! Every OpenCompany teammate already **is** an openhuman session: the pool
//! holds one [`Agent`](oh::agent::Agent) — "stateful agent session, the single
//! execution tier" in the vendored crate's own words — per `(company,
//! agent_id)`, behind a mutex, because a turn takes `&mut self` and one session
//! must serialise its own turns. What it did not have was a *name*.
//!
//! [`AgentBuilder`](oh::agent::AgentBuilder) defaults `event_session_id` to the
//! literal string `"standalone"` and `event_channel` to `"internal"`, and
//! OpenCompany never set either. Those two fields are the identity every
//! `DomainEvent` the session publishes is tagged with — `AgentTurnStarted`,
//! `AgentTurnCompleted`, `AgentError` — plus the `PromptEnforcementContext` a
//! blocked prompt is reported against and the `report_error` tags beside it. So
//! every agent of every company on the process announced itself as the same
//! session, and a bus subscriber could not tell whose turn had started, whose
//! had failed, or whose prompt had been refused.
//!
//! That cost nothing while one agent ran at a time. It stops being free now:
//! openhuman's library host runs many sessions over one core concurrently (the
//! upstream `HostKind::Library` work sharded the conversation store's
//! process-wide mutex into per-root lifecycle, per-root metadata and per-thread
//! transcript locks, and proved 100 overlapping turns on distinct session ids).
//! Concurrency is exactly the condition under which an unlabelled event stream
//! stops being readable — with one turn in flight, `session=standalone` is
//! unambiguous by luck.
//!
//! So the key is minted here, once, and the two consumers that must agree about
//! it read the same function: the builder that stamps it onto the session, and
//! the speech tools, which name the destination session when one teammate
//! leaves a DM for another. A DM is a hop from one openhuman session to
//! another, and it can only be reported as one if both ends spell the session
//! the same way.
//!
//! # Shape
//!
//! `{company}:{agent_id}` — company first, because the process is multi-tenant
//! and an `agent_id` is only unique within its own company. Two companies that
//! both call a teammate `designer` are two sessions, and sorting the bus by
//! prefix groups a tenant's traffic together.
//!
//! Ungated, and deliberately free of any openhuman type: the speech tools and
//! the operator routes are not behind `feature = "openhuman"`, and a key they
//! cannot name is a key they cannot report.

use crate::ports::CompanyId;

/// The `event_channel` every OpenCompany session declares.
///
/// openhuman's own hosts use this to say which front end a turn came from
/// (`"cli"`, `"telegram"`, `"rpc"`); the builder's default is `"internal"`,
/// which is what a session that nobody labelled looks like. Every session here
/// is driven by this product, so the honest answer is one constant rather than
/// a per-surface value: the surface an OpenCompany turn arrived on — a desk, a
/// DM, a card, a workflow — is carried by the cue the session is handed
/// (`agent_session::render_cues`), not by the bus label.
pub const SESSION_CHANNEL: &str = "opencompany";

/// The openhuman session id for one teammate of one company.
///
/// Stable across rebuilds by construction — it is a pure function of the two
/// ids, and neither moves for the life of a teammate. That matters because the
/// roster is rebuilt whenever any of its ten freshness fingerprints moves (an
/// MCP server, a skill, a budget, a persona edit…): a key derived from anything
/// that a rebuild disturbs would rename the session under a subscriber roughly
/// whenever an operator touched a setting.
pub fn openhuman_session_key(company: &CompanyId, agent_id: &str) -> String {
    format!("{company}:{agent_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_named_for_its_company_and_its_teammate() {
        let key = openhuman_session_key(&CompanyId::new("acme"), "designer");
        assert_eq!(key, "acme:designer");
    }

    #[test]
    fn two_teammates_of_one_company_are_two_sessions() {
        let company = CompanyId::new("acme");
        assert_ne!(
            openhuman_session_key(&company, "designer"),
            openhuman_session_key(&company, "engineer"),
            "one company's teammates must not share a session id — the whole \
             point is telling their turns apart on the bus"
        );
    }

    #[test]
    fn one_teammate_id_in_two_companies_is_two_sessions() {
        // The process is multi-tenant and `agent_id` is unique only within a
        // company, so the company has to be in the key or two tenants' turns
        // arrive on the bus indistinguishable.
        assert_ne!(
            openhuman_session_key(&CompanyId::new("acme"), "designer"),
            openhuman_session_key(&CompanyId::new("globex"), "designer"),
        );
    }

    #[test]
    fn the_key_is_stable_for_the_same_pair() {
        let company = CompanyId::new("acme");
        assert_eq!(
            openhuman_session_key(&company, "designer"),
            openhuman_session_key(&company, "designer"),
            "a roster rebuild must not rename a live session"
        );
    }

    #[test]
    fn the_channel_is_not_the_builders_unlabelled_default() {
        assert_ne!(
            SESSION_CHANNEL, "internal",
            "`internal` is what openhuman calls a session nobody named"
        );
    }
}
