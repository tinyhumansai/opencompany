//! Deterministic Agent Card projection from a Company Charter.
//!
//! [`build_agent_card`] maps a [`CompanyManifest`] plus the host's base URL onto
//! the [`AgentCard`] wire shape tiny.place publishes. The projection is a pure
//! function — no clock, no randomness — so the same charter always yields the
//! same card, and the whole surface compiles and tests in the default build
//! without any crypto or network dependency.

use crate::company::CompanyManifest;
use crate::ports::types::{AgentCard, CardPayment};

/// The settlement asset advertised for priced skills.
const CARD_ASSET: &str = "USDC";
/// The settlement network advertised for priced skills.
const CARD_NETWORK: &str = "solana";
/// The single A2A interface every card advertises this phase.
const A2A_INTERFACE: &str = "a2a-jsonrpc";

/// Projects a [`CompanyManifest`] onto its published [`AgentCard`].
///
/// Mapping:
/// - `handle`  ← `[company].handle` (empty if unset)
/// - `name`    ← `[company].name`
/// - `description` ← `[company].output`, falling back to the company name
/// - `skills`  ← each `[place].skills[].id`
/// - `capabilities` / `tags` ← the same skill ids
/// - `payment_requirements` ← `{ skill_id, price = price_usd, asset, network }`
/// - `endpoint` ← `{host_base_url}/a2a/{handle}` (trailing slash trimmed)
/// - `supported_interfaces` ← `["a2a-jsonrpc"]`
/// - `actor_type` ← `"agent"`
pub fn build_agent_card(manifest: &CompanyManifest, host_base_url: &str) -> AgentCard {
    let handle = manifest.company.handle.clone().unwrap_or_default();
    let description = manifest
        .company
        .output
        .clone()
        .unwrap_or_else(|| manifest.company.name.clone());

    let skills: Vec<String> = manifest.place.skills.iter().map(|s| s.id.clone()).collect();

    let payment_requirements: Vec<CardPayment> = manifest
        .place
        .skills
        .iter()
        .map(|s| CardPayment {
            skill_id: s.id.clone(),
            price: s.price_usd.clone(),
            asset: CARD_ASSET.to_string(),
            network: CARD_NETWORK.to_string(),
        })
        .collect();

    AgentCard {
        handle: handle.clone(),
        description,
        skills: skills.clone(),
        name: manifest.company.name.clone(),
        actor_type: "agent".to_string(),
        endpoint: a2a_endpoint(host_base_url, &handle),
        supported_interfaces: vec![A2A_INTERFACE.to_string()],
        capabilities: skills.clone(),
        tags: skills,
        payment_requirements,
    }
}

/// Builds the `{base}/a2a/{handle}` endpoint, trimming a trailing slash on the
/// base so the path never doubles up.
fn a2a_endpoint(host_base_url: &str, handle: &str) -> String {
    let base = host_base_url.trim_end_matches('/');
    format!("{base}/a2a/{handle}")
}

/// Renders a card's skill catalog as human- and agent-readable `skill.md`.
///
/// Deterministic: lists every skill with its price and description in card
/// order. Used by the A2A `skill.md` route in a later batch.
pub fn render_skill_md(card: &AgentCard) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let title = if card.name.is_empty() {
        card.handle.as_str()
    } else {
        card.name.as_str()
    };
    let _ = writeln!(out, "# {title}");
    let _ = writeln!(out);
    if !card.description.is_empty() {
        let _ = writeln!(out, "{}", card.description);
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "## Skills");
    let _ = writeln!(out);
    if card.payment_requirements.is_empty() {
        let _ = writeln!(out, "_No priced skills advertised._");
    } else {
        for pay in &card.payment_requirements {
            let _ = writeln!(
                out,
                "- `{}` — {} {} ({})",
                pay.skill_id, pay.price, pay.asset, pay.network
            );
        }
    }
    out
}

#[cfg(test)]
mod test {
    use super::*;

    fn manifest_with_two_skills() -> CompanyManifest {
        let toml_src = r#"
            [company]
            name = "Acme SEO"
            output = "SEO audits and content"
            handle = "acme"

            [place]
            discoverable = true
            skills = [
                { id = "seo.audit", price_usd = "25.00", description = "Full site audit" },
                { id = "seo.brief", price_usd = "10.00" },
            ]
        "#;
        toml::from_str(toml_src).expect("valid manifest")
    }

    #[test]
    fn projects_priced_skills_deterministically() {
        let manifest = manifest_with_two_skills();
        let card = build_agent_card(&manifest, "https://host.example");

        assert_eq!(card.handle, "acme");
        assert_eq!(card.name, "Acme SEO");
        assert_eq!(card.description, "SEO audits and content");
        assert_eq!(card.actor_type, "agent");
        assert_eq!(card.endpoint, "https://host.example/a2a/acme");
        assert_eq!(card.supported_interfaces, vec!["a2a-jsonrpc"]);
        assert_eq!(card.skills, vec!["seo.audit", "seo.brief"]);
        assert_eq!(card.capabilities, card.skills);
        assert_eq!(card.tags, card.skills);

        assert_eq!(card.payment_requirements.len(), 2);
        let audit = &card.payment_requirements[0];
        assert_eq!(audit.skill_id, "seo.audit");
        assert_eq!(audit.price, "25.00");
        assert_eq!(audit.asset, "USDC");
        assert_eq!(audit.network, "solana");

        // Determinism: a second projection is byte-identical.
        assert_eq!(build_agent_card(&manifest, "https://host.example"), card);
    }

    #[test]
    fn endpoint_trims_trailing_slash_on_base() {
        let manifest = manifest_with_two_skills();
        let card = build_agent_card(&manifest, "https://host.example/");
        assert_eq!(card.endpoint, "https://host.example/a2a/acme");
    }

    #[test]
    fn description_falls_back_to_name_when_output_absent() {
        let manifest: CompanyManifest =
            toml::from_str("[company]\nname = \"Solo\"\nhandle = \"solo\"\n").expect("manifest");
        let card = build_agent_card(&manifest, "https://h");
        assert_eq!(card.description, "Solo");
        assert!(card.skills.is_empty());
        assert!(card.payment_requirements.is_empty());
    }

    #[test]
    fn skill_md_lists_every_priced_skill() {
        let manifest = manifest_with_two_skills();
        let card = build_agent_card(&manifest, "https://host.example");
        let md = render_skill_md(&card);
        assert!(md.contains("# Acme SEO"));
        assert!(md.contains("`seo.audit` — 25.00 USDC (solana)"));
        assert!(md.contains("`seo.brief` — 10.00 USDC (solana)"));
    }
}
