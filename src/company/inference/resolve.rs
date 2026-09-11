//! Which provider serves which workload. Pure: no IO, no async, no fixtures
//! beyond structs.
//!
//! Everything here is a decision with a branch worth a test, which is exactly
//! why none of it lives in a handler or a component. Given a list of providers
//! and a routing map, these functions answer *which one*, *which model*, and
//! *what happened when the answer is none* — and they answer it the same way for
//! the console's status view and for the turn that is about to be sent.
//!
//! ## Four rules, each of which is a bug somebody already shipped
//!
//! **No workload inherits another's route.** Upstream shipped the opposite:
//! setting only the coding route moved chat and reasoning onto that key too, so
//! ordinary conversations were silently billed to the user's own account with no
//! settings field saying so. An unset workload resolves through the *primary*,
//! never through a sibling's configured provider. [`Resolution::Primary`] is how
//! that is said out loud.
//!
//! **Fail closed on a route naming a provider that is gone.** A route pointing
//! at a slug nobody holds is an error that names the workload and the slug —
//! [`Resolution::Missing`] — not a silent demotion to the primary. Demoting
//! quietly would attribute that workload's spend to whatever the fallback
//! happened to be, which is the same defect as resolving an unknown provider
//! kind rather than rejecting it.
//!
//! **A disabled provider is not a routing target, and a route naming one is
//! reported rather than demoted** — [`Resolution::Disabled`]. Same reasoning:
//! "stop billing this account this week" must not become "quietly bill a
//! different one".
//!
//! **Removing a provider scrubs the routes pointing at it, by three different
//! rules**, because only one of the three kinds of ref carries a slug. See
//! [`scrub_removed`]; all three cases are bugs upstream had to fix.
//!
//! ## An alias is not an inherited route
//!
//! [`Workload::Coding`] resolves through the agentic route. That is an **alias**
//! — two names for one configured route — and it is a different thing from an
//! unset route borrowing a set one. The distinction matters because the
//! inheritance bug above looked exactly like an alias from the outside.
//!
//! It is an alias rather than a fifth row for a concrete reason: OpenCompany has
//! four abstract tiers and no distinct coding tier, so a separate editable
//! coding row would write the agentic tier's route under a second name. Setting
//! one would silently change the other, which is the inheritance bug wearing a
//! different hat. The row appears when a coding tier does.

use std::collections::BTreeMap;

use super::catalogue::{self, Category};
use super::store::Provider;

/// What a request is for.
///
/// The labels, descriptions and recommendation hints that go with these live in
/// the console; this is the routing key alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Workload {
    /// Direct conversational back-and-forth.
    Chat,
    /// Deep thinking: the main chat agent and heavier answer synthesis.
    Reasoning,
    /// Sub-agent runners and tool loops.
    Agentic,
    /// Code generation and refactor passes. An **alias** of [`Workload::Agentic`].
    Coding,
    /// Image understanding.
    Vision,
}

/// Every workload that has a row of its own — one per abstract tier.
///
/// [`Workload::Coding`] is deliberately absent: it is an alias, and a row for it
/// would write the agentic tier's route under a second name.
pub const ROUTABLE_WORKLOADS: &[Workload] = &[
    Workload::Chat,
    Workload::Reasoning,
    Workload::Agentic,
    Workload::Vision,
];

impl Workload {
    /// The abstract tier this workload routes through.
    pub fn tier(self) -> &'static str {
        match self {
            Self::Chat => "chat-v1",
            Self::Reasoning => "reasoning-v1",
            // Coding shares the agentic tier — see the module header on why
            // that is an alias rather than a fifth row.
            Self::Agentic | Self::Coding => "agentic-v1",
            Self::Vision => "vision-v1",
        }
    }

    /// Whether this workload reads another's route rather than owning one.
    pub fn is_alias(self) -> bool {
        matches!(self, Self::Coding)
    }

    /// The workload for a tier name, if it is one of ours.
    pub fn from_tier(tier: &str) -> Option<Self> {
        ROUTABLE_WORKLOADS
            .iter()
            .copied()
            .find(|w| w.tier() == tier.trim())
    }

    /// The stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Reasoning => "reasoning",
            Self::Agentic => "agentic",
            Self::Coding => "coding",
            Self::Vision => "vision",
        }
    }
}

/// What one routing row points at.
///
/// [`ProviderRef::Managed`] and [`ProviderRef::Default`] are different states on
/// purpose: one is a choice, the other is an absence. Collapsing them loses the
/// ability to say "this row is deliberately managed" as distinct from "this row
/// was never set", and those two want different copy and different behaviour
/// when a provider is removed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderRef {
    /// Explicitly the managed brain.
    Managed,
    /// Unset. Falls through to the primary, never to a sibling.
    Default,
    /// A cloud or custom provider, addressed by slug.
    Cloud {
        /// The provider's slug.
        provider_slug: String,
        /// The model id to send, if pinned.
        model: Option<String>,
    },
    /// A local runtime. Carries **no slug**, which is why its scrub rule
    /// differs.
    Local {
        /// The model id to send, if pinned.
        model: Option<String>,
    },
    /// A CLI login. Carries no slug either.
    ClaudeCode {
        /// The model id to send, if pinned.
        model: Option<String>,
    },
}

impl ProviderRef {
    /// Parses the hand-editable string grammar.
    ///
    /// An operator reads and edits routes as `reasoning-v1 -> acme:gpt-5`, so
    /// the grammar has to survive a round trip through a human. An empty string
    /// is [`ProviderRef::Default`] — an absence, not a parse failure, because
    /// "nothing set here" is a legitimate value an operator writes by deleting.
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        if raw.is_empty() || raw == "default" {
            return Self::Default;
        }
        if raw == "managed" {
            return Self::Managed;
        }
        let (slug, model) = match raw.split_once(':') {
            Some((slug, model)) => (
                slug.trim(),
                Some(model.trim().to_string()).filter(|m| !m.is_empty()),
            ),
            None => (raw, None),
        };
        match slug {
            "claude-code" => Self::ClaudeCode { model },
            "local" => Self::Local { model },
            _ => Self::Cloud {
                provider_slug: slug.to_string(),
                model,
            },
        }
    }

    /// The string form, in the same grammar [`ProviderRef::parse`] reads.
    ///
    /// This is the **persisted** shape, deliberately: a route is stored as the
    /// text an operator would type, so the stored value and the value they
    /// hand-edit are the same value. Storing a tagged enum instead would make
    /// the console's grammar a presentation layer over a second representation,
    /// and the two would have to be kept in step forever.
    ///
    /// [`ProviderRef::Default`] renders as the empty string — an absence, which
    /// is why the writer drops those rather than storing `""`.
    pub fn to_route_string(&self) -> String {
        match self {
            Self::Default => String::new(),
            Self::Managed => "managed".to_string(),
            Self::Cloud {
                provider_slug,
                model,
            } => match model {
                Some(model) => format!("{provider_slug}:{model}"),
                None => provider_slug.clone(),
            },
            Self::Local { model } => match model {
                Some(model) => format!("local:{model}"),
                None => "local".to_string(),
            },
            Self::ClaudeCode { model } => match model {
                Some(model) => format!("claude-code:{model}"),
                None => "claude-code".to_string(),
            },
        }
    }

    /// The slug this ref names, when it names one at all.
    ///
    /// `None` for local and CLI refs is not an oversight — it is the fact the
    /// three scrub rules exist to work around.
    pub fn slug(&self) -> Option<&str> {
        match self {
            Self::Cloud { provider_slug, .. } => Some(provider_slug),
            _ => None,
        }
    }

    /// The pinned model id, if any.
    pub fn model(&self) -> Option<&str> {
        match self {
            Self::Cloud { model, .. } | Self::Local { model } | Self::ClaudeCode { model } => {
                model.as_deref()
            }
            Self::Managed | Self::Default => None,
        }
    }
}

/// Tier name → what that tier routes through.
pub type Routes = BTreeMap<String, ProviderRef>;

/// What resolving a workload produced.
///
/// An enum rather than an `Option` because three of these outcomes are not
/// "nothing" — they are three different things to say to an operator, and
/// collapsing any pair of them tells someone to do something that will not help.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution<'a> {
    /// A provider was named and found.
    Resolved {
        /// The provider serving this workload.
        provider: &'a Provider,
        /// The model id the route pinned, if any.
        model: Option<String>,
    },
    /// Deliberately the managed brain.
    Managed,
    /// Unset. Falls through to the primary — never to a sibling's provider.
    Primary,
    /// The route names a provider this company does not hold. Fail closed.
    Missing {
        /// The workload whose route is broken.
        workload: Workload,
        /// The slug that resolved to nothing.
        slug: String,
    },
    /// The route names a provider that is switched off.
    Disabled {
        /// The workload whose route is parked.
        workload: Workload,
        /// The provider that is off.
        slug: String,
    },
}

/// The providers a company can actually route to — enabled only, in list order.
pub fn routing_targets(providers: &[Provider]) -> Vec<&Provider> {
    providers.iter().filter(|p| p.enabled).collect()
}

/// The provider an unset workload falls through to.
///
/// **The marked default, and only then list order.** `marked` is the slug the
/// company has said is its default
/// ([`load_default_slug`](super::store::load_default_slug)); `None` is every
/// company that has never said, which is every company that existed before the
/// marker did.
///
/// The fallback is first-enabled, which is what this did unconditionally — and
/// entry zero sorts first in
/// [`list_providers`](super::store::list_providers), so a company that had one
/// provider before any of this existed keeps sending its unset workloads exactly
/// where it always did. No migration, no backfill.
///
/// ## Why the marker exists at all
///
/// First-enabled answers "which provider is my default" by **list order**. Add
/// three providers, delete the first, and the default silently becomes the
/// second — with nothing on screen having changed to say so, and the company's
/// unrouted spend moving to a different account. An explicit marker makes that a
/// thing the operator said rather than a thing that happened.
///
/// ## Two ways the marker can be stale, and one answer to both
///
/// A marked provider may be **disabled** or **gone**. Both are handled the same
/// way — fall back to first-enabled — rather than by refusing to resolve, and
/// deliberately: the routes the operator *did* set fail closed when they name a
/// provider that is missing or off ([`Resolution::Missing`],
/// [`Resolution::Disabled`]), because those are choices with a workload attached.
/// An unset workload has no such choice behind it, and the alternative to
/// falling back is a company that cannot think at all because of a marker it
/// forgot about. The write paths keep this rare rather than relying on it: both
/// disabling and deleting clear the marker in the same operation.
///
/// `None` means nothing enabled resolves, and every caller reads that as the
/// managed brain — which is always available and is the right fallback.
pub fn primary<'a>(providers: &'a [Provider], marked: Option<&str>) -> Option<&'a Provider> {
    if let Some(marked) = marked.map(str::trim).filter(|s| !s.is_empty())
        && let Some(provider) = providers.iter().find(|p| p.slug == marked && p.enabled)
    {
        return Some(provider);
    }
    providers.iter().find(|p| p.enabled)
}

/// Which provider serves `workload`.
///
/// Reads the route for the workload's **tier**, so an alias
/// ([`Workload::Coding`]) resolves through the route its tier owns rather than
/// through one of its own.
pub fn provider_for_workload<'a>(
    workload: Workload,
    routes: &Routes,
    providers: &'a [Provider],
) -> Resolution<'a> {
    let route = routes
        .get(workload.tier())
        .cloned()
        .unwrap_or(ProviderRef::Default);
    match route {
        ProviderRef::Default => Resolution::Primary,
        ProviderRef::Managed => Resolution::Managed,
        ProviderRef::Cloud {
            ref provider_slug,
            ref model,
        } => match providers.iter().find(|p| &p.slug == provider_slug) {
            None => Resolution::Missing {
                workload,
                slug: provider_slug.clone(),
            },
            Some(p) if !p.enabled => Resolution::Disabled {
                workload,
                slug: provider_slug.clone(),
            },
            Some(provider) => Resolution::Resolved {
                provider,
                model: model.clone(),
            },
        },
        // A slug-less ref names a category, not a record. It resolves to the
        // first enabled provider of that category — and to `Missing` when there
        // is none, rather than falling through to the primary, because the
        // operator did choose something here.
        ProviderRef::Local { ref model } => {
            resolve_by_category(workload, providers, Category::Local, model, "local")
        }
        ProviderRef::ClaudeCode { ref model } => {
            resolve_by_category(workload, providers, Category::Cli, model, "claude-code")
        }
    }
}

fn resolve_by_category<'a>(
    workload: Workload,
    providers: &'a [Provider],
    category: Category,
    model: &Option<String>,
    name: &str,
) -> Resolution<'a> {
    let of_category: Vec<&Provider> = providers
        .iter()
        .filter(|p| catalogue::category_of(&p.kind) == category)
        .collect();
    match of_category.iter().find(|p| p.enabled) {
        Some(provider) => Resolution::Resolved {
            provider,
            model: model.clone(),
        },
        None if of_category.is_empty() => Resolution::Missing {
            workload,
            slug: name.to_string(),
        },
        None => Resolution::Disabled {
            workload,
            slug: name.to_string(),
        },
    }
}

/// The three routing modes, inferred from the routes.
///
/// **Never stored.** A mode field would be a fifth thing that can disagree with
/// the four routes, and the routes are the truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutingMode {
    /// Every row is managed or unset.
    Managed,
    /// Every row names the same provider and model.
    Own,
    /// Anything else.
    Advanced,
}

/// Which mode the current routes describe.
pub fn infer_routing_mode(routes: &Routes) -> RoutingMode {
    let refs: Vec<ProviderRef> = ROUTABLE_WORKLOADS
        .iter()
        .map(|w| {
            routes
                .get(w.tier())
                .cloned()
                .unwrap_or(ProviderRef::Default)
        })
        .collect();
    if refs
        .iter()
        .all(|r| matches!(r, ProviderRef::Managed | ProviderRef::Default))
    {
        return RoutingMode::Managed;
    }
    let first = &refs[0];
    if refs.iter().all(|r| r == first) {
        return RoutingMode::Own;
    }
    RoutingMode::Advanced
}

/// Resets every route orphaned by removing `removed`, given what `remaining`
/// holds afterwards. Returns the tiers that were reset, so the console can say
/// which ones moved rather than leaving the operator to notice.
///
/// ## Three rules, because only one kind of ref carries a slug
///
/// * **Cloud and custom** — matched precisely by slug. The easy case.
/// * **CLI logins** — their refs carry no slug. Without special handling,
///   disconnecting one leaves workloads pinned to `claude-code:<model>`, which
///   the resolver still honours, so turns keep using a provider the operator
///   removed.
/// * **Local runtimes** — no slug either, and one more wrinkle: a `local` ref is
///   only definitively orphaned once **no** local runtime remains. Scrubbing on
///   the first removal would unpin a route that a second local runtime still
///   serves. Before this rule existed the local case was silently a no-op.
pub fn scrub_removed(
    routes: &mut Routes,
    removed: &Provider,
    remaining: &[Provider],
) -> Vec<String> {
    let category = catalogue::category_of(&removed.kind);
    let category_survives = remaining
        .iter()
        .any(|p| catalogue::category_of(&p.kind) == category);

    let mut reset = Vec::new();
    for (tier, route) in routes.iter_mut() {
        let orphaned = match route {
            // **A slug match is decisive, whatever the category.** This used to
            // also require `category == Cloud`, and the two rules then never met
            // for a local runtime: `ollama:llama3` parses as a `Cloud` ref
            // because it carries a slug, while `category_of("ollama")` is
            // `Local` — so the cloud arm refused it on category and the local
            // arm never saw it, because that arm only matches the slug-less
            // `local` ref. Removing Ollama left every row pointing at it, and
            // the routing table then refused to save at all: `put_routes` fails
            // closed on a route naming a provider nobody holds, so the operator
            // could not re-save their own routing until they had changed every
            // row by hand.
            //
            // A slug is unique per company, so naming one that is being removed
            // is orphaned by definition. The category never added anything.
            ProviderRef::Cloud { provider_slug, .. } => provider_slug == &removed.slug,
            ProviderRef::Local { .. } => category == Category::Local && !category_survives,
            ProviderRef::ClaudeCode { .. } => category == Category::Cli && !category_survives,
            ProviderRef::Managed | ProviderRef::Default => false,
        };
        if orphaned {
            *route = ProviderRef::Default;
            reset.push(tier.clone());
        }
    }
    reset
}

/// Every route naming a provider this company does not hold.
///
/// The second, independent mechanism behind the same invariant as
/// [`scrub_removed`]. Two mechanisms for one rule because the UI path can be
/// bypassed — by a hand-edited config or an older build — and an unresolvable
/// route must be reported at load rather than discovered mid-turn.
pub fn orphaned_routes(routes: &Routes, providers: &[Provider]) -> Vec<(String, String)> {
    routes
        .iter()
        .filter_map(|(tier, route)| {
            let slug = route.slug()?;
            (!providers.iter().any(|p| p.slug == slug)).then(|| (tier.clone(), slug.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::company::inference::store::{ProviderId, ProviderOrigin};

    fn provider(slug: &str, kind: &str, enabled: bool) -> Provider {
        Provider {
            id: ProviderId::new(),
            slug: slug.to_string(),
            label: slug.to_string(),
            kind: kind.to_string(),
            base_url: format!("https://{slug}.example/v1"),
            models: BTreeMap::new(),
            enabled,
            origin: ProviderOrigin::Indexed,
        }
    }

    fn routes(pairs: &[(&str, &str)]) -> Routes {
        pairs
            .iter()
            .map(|(tier, raw)| (tier.to_string(), ProviderRef::parse(raw)))
            .collect()
    }

    // ---- the no-inheritance rule -------------------------------------------

    #[test]
    fn an_unset_workload_resolves_through_the_primary_never_a_sibling() {
        // The bug this is the fix for: setting only the coding route used to
        // move chat and reasoning onto that key, silently billing ordinary
        // conversations to the operator's own account.
        let providers = vec![
            provider("openrouter", "openrouter", true),
            provider("acme", "openai_compatible", true),
        ];
        let routes = routes(&[("agentic-v1", "acme:gpt-5")]);

        match provider_for_workload(Workload::Chat, &routes, &providers) {
            Resolution::Primary => {}
            other => panic!("chat must fall through to the primary, got {other:?}"),
        }
        match provider_for_workload(Workload::Reasoning, &routes, &providers) {
            Resolution::Primary => {}
            other => panic!("reasoning must fall through to the primary, got {other:?}"),
        }
        // And the one that WAS set resolves to what it names.
        match provider_for_workload(Workload::Agentic, &routes, &providers) {
            Resolution::Resolved { provider, model } => {
                assert_eq!(provider.slug, "acme");
                assert_eq!(model.as_deref(), Some("gpt-5"));
            }
            other => panic!("agentic was set, got {other:?}"),
        }
    }

    #[test]
    fn with_no_marker_the_primary_is_the_first_enabled_provider() {
        // Today's behaviour, unchanged for every company that existed before the
        // marker did. No migration, no backfill.
        let providers = vec![
            provider("openrouter", "openrouter", false),
            provider("acme", "openai_compatible", true),
        ];
        assert_eq!(primary(&providers, None).unwrap().slug, "acme");
        assert!(primary(&[], None).is_none());
    }

    #[test]
    fn a_marked_default_wins_over_list_order() {
        // The whole point. First-enabled answers "which provider is my default"
        // by list order, so deleting the first silently moves a company's
        // unrouted spend to a different account with nothing on screen saying so.
        let providers = vec![
            provider("openrouter", "openrouter", true),
            provider("acme", "openai_compatible", true),
        ];
        assert_eq!(primary(&providers, Some("acme")).unwrap().slug, "acme");
        // And an unset workload follows the marker when it moves.
        assert_eq!(
            primary(&providers, Some("openrouter")).unwrap().slug,
            "openrouter"
        );
    }

    #[test]
    fn a_stale_marker_falls_back_rather_than_stranding_the_company() {
        // Both ways it can go stale — disabled, and gone — get the same answer.
        // A route the operator DID set fails closed when it names a provider
        // that is missing or off, because that is a choice with a workload
        // attached. An unset workload has no such choice behind it, and the
        // alternative to falling back is a company that cannot think at all
        // because of a marker it forgot about.
        let providers = vec![
            provider("openrouter", "openrouter", true),
            provider("acme", "openai_compatible", false),
        ];
        assert_eq!(
            primary(&providers, Some("acme")).unwrap().slug,
            "openrouter",
            "a disabled marked provider is not a routing target"
        );
        assert_eq!(
            primary(&providers, Some("ghost")).unwrap().slug,
            "openrouter",
            "a marker naming nothing falls back"
        );
        // Nothing enabled at all is `None`, which every caller reads as the
        // managed brain — always available, and the right fallback.
        let all_off = vec![provider("acme", "openai_compatible", false)];
        assert!(primary(&all_off, Some("acme")).is_none());
    }

    #[test]
    fn an_unset_workload_follows_the_marker_when_it_moves() {
        // The two halves together: `provider_for_workload` says "unset, use the
        // primary" and `primary` says which that is. Moving the marker moves
        // every unset workload with it, and nothing else.
        let providers = vec![
            provider("openrouter", "openrouter", true),
            provider("acme", "openai_compatible", true),
        ];
        let routes = routes(&[("reasoning-v1", "acme:gpt-5")]);

        for (marked, expected) in [(None, "openrouter"), (Some("acme"), "acme")] {
            assert!(
                matches!(
                    provider_for_workload(Workload::Chat, &routes, &providers),
                    Resolution::Primary
                ),
                "chat is unset whatever the marker says"
            );
            assert_eq!(primary(&providers, marked).unwrap().slug, expected);
        }

        // And the route that WAS set does not move.
        match provider_for_workload(Workload::Reasoning, &routes, &providers) {
            Resolution::Resolved { provider, .. } => assert_eq!(provider.slug, "acme"),
            other => panic!("reasoning was set, got {other:?}"),
        }
    }

    #[test]
    fn coding_is_an_alias_of_agentic_not_a_row_of_its_own() {
        // An alias is two names for ONE configured route. That is a different
        // thing from an unset route borrowing a set one, which is the bug above.
        let providers = vec![provider("acme", "openai_compatible", true)];
        let routes = routes(&[("agentic-v1", "acme:gpt-5")]);
        assert_eq!(Workload::Coding.tier(), Workload::Agentic.tier());
        assert!(Workload::Coding.is_alias());
        assert!(!ROUTABLE_WORKLOADS.contains(&Workload::Coding));
        match provider_for_workload(Workload::Coding, &routes, &providers) {
            Resolution::Resolved { provider, .. } => assert_eq!(provider.slug, "acme"),
            other => panic!("coding reads the agentic route, got {other:?}"),
        }
    }

    // ---- fail closed --------------------------------------------------------

    #[test]
    fn a_route_naming_a_provider_that_is_gone_fails_closed() {
        // Demoting silently would attribute this workload's spend to whatever
        // the fallback happened to be.
        let providers = vec![provider("acme", "openai_compatible", true)];
        let routes = routes(&[("reasoning-v1", "ghost:gpt-5")]);
        match provider_for_workload(Workload::Reasoning, &routes, &providers) {
            Resolution::Missing { workload, slug } => {
                assert_eq!(workload, Workload::Reasoning);
                assert_eq!(slug, "ghost");
            }
            other => panic!("expected a named failure, got {other:?}"),
        }
    }

    #[test]
    fn a_route_naming_a_disabled_provider_is_reported_not_demoted() {
        let providers = vec![
            provider("openrouter", "openrouter", true),
            provider("acme", "openai_compatible", false),
        ];
        let routes = routes(&[("reasoning-v1", "acme:gpt-5")]);
        match provider_for_workload(Workload::Reasoning, &routes, &providers) {
            Resolution::Disabled { workload, slug } => {
                assert_eq!(workload, Workload::Reasoning);
                assert_eq!(slug, "acme");
            }
            other => {
                panic!("\"off this week\" must not become \"bill another one\", got {other:?}")
            }
        }
    }

    #[test]
    fn a_disabled_provider_is_not_a_routing_target() {
        let providers = vec![
            provider("openrouter", "openrouter", true),
            provider("acme", "openai_compatible", false),
        ];
        assert_eq!(
            routing_targets(&providers)
                .iter()
                .map(|p| p.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["openrouter"]
        );
    }

    #[test]
    fn managed_and_unset_are_different_states() {
        let providers = vec![provider("acme", "openai_compatible", true)];
        let explicit = routes(&[("chat-v1", "managed")]);
        assert_eq!(
            provider_for_workload(Workload::Chat, &explicit, &providers),
            Resolution::Managed
        );
        assert_eq!(
            provider_for_workload(Workload::Chat, &Routes::new(), &providers),
            Resolution::Primary
        );
    }

    // ---- the string grammar -------------------------------------------------

    #[test]
    fn the_hand_editable_grammar_round_trips_through_a_person() {
        assert_eq!(ProviderRef::parse(""), ProviderRef::Default);
        assert_eq!(ProviderRef::parse("   "), ProviderRef::Default);
        assert_eq!(ProviderRef::parse("default"), ProviderRef::Default);
        assert_eq!(ProviderRef::parse("managed"), ProviderRef::Managed);
        assert_eq!(
            ProviderRef::parse("acme:gpt-5"),
            ProviderRef::Cloud {
                provider_slug: "acme".into(),
                model: Some("gpt-5".into())
            }
        );
        assert_eq!(
            ProviderRef::parse("acme"),
            ProviderRef::Cloud {
                provider_slug: "acme".into(),
                model: None
            }
        );
        assert_eq!(
            ProviderRef::parse("claude-code:opus"),
            ProviderRef::ClaudeCode {
                model: Some("opus".into())
            }
        );
        assert_eq!(
            ProviderRef::parse("local:llama3.1"),
            ProviderRef::Local {
                model: Some("llama3.1".into())
            }
        );
        // A trailing colon is a slug with no model, not a model named "".
        assert_eq!(
            ProviderRef::parse("acme:"),
            ProviderRef::Cloud {
                provider_slug: "acme".into(),
                model: None
            }
        );
    }

    #[test]
    fn only_a_cloud_ref_carries_a_slug() {
        // This is the fact the three scrub rules exist to work around.
        assert_eq!(ProviderRef::parse("acme:gpt-5").slug(), Some("acme"));
        assert_eq!(ProviderRef::parse("local:llama3.1").slug(), None);
        assert_eq!(ProviderRef::parse("claude-code:opus").slug(), None);
        assert_eq!(ProviderRef::parse("managed").slug(), None);
    }

    // ---- the three scrub rules ----------------------------------------------

    #[test]
    fn removing_a_cloud_provider_scrubs_routes_matched_by_slug() {
        let removed = provider("acme", "openai_compatible", true);
        let remaining = vec![provider("openrouter", "openrouter", true)];
        let mut routes = routes(&[
            ("chat-v1", "acme:gpt-5"),
            ("reasoning-v1", "openrouter:big"),
            ("agentic-v1", ""),
        ]);
        let reset = scrub_removed(&mut routes, &removed, &remaining);
        assert_eq!(reset, vec!["chat-v1".to_string()]);
        assert_eq!(routes["chat-v1"], ProviderRef::Default);
        assert_eq!(
            routes["reasoning-v1"],
            ProviderRef::Cloud {
                provider_slug: "openrouter".into(),
                model: Some("big".into())
            },
            "an unrelated route must not move"
        );
    }

    #[test]
    fn removing_a_cli_login_scrubs_its_slugless_routes() {
        // Without this, disconnecting Claude Code left workloads pinned to
        // `claude-code:<model>`, which the resolver still honours — so chats
        // kept using the CLI after the provider was removed.
        let removed = provider("claude-code", "claude-code", true);
        let remaining = vec![provider("openrouter", "openrouter", true)];
        let mut routes = routes(&[("chat-v1", "claude-code:opus")]);
        let reset = scrub_removed(&mut routes, &removed, &remaining);
        assert_eq!(reset, vec!["chat-v1".to_string()]);
        assert_eq!(routes["chat-v1"], ProviderRef::Default);
    }

    #[test]
    fn a_local_route_survives_while_any_local_runtime_remains() {
        // Scrubbing on the first removal would unpin a route a second local
        // runtime still serves.
        let removed = provider("ollama", "ollama", true);
        let remaining = vec![provider("lmstudio", "lmstudio", true)];
        let mut routes = routes(&[("chat-v1", "local:llama3.1")]);
        let reset = scrub_removed(&mut routes, &removed, &remaining);
        assert!(reset.is_empty(), "another local runtime still serves it");
        assert_eq!(
            routes["chat-v1"],
            ProviderRef::Local {
                model: Some("llama3.1".into())
            }
        );
    }

    #[test]
    fn a_local_route_is_scrubbed_once_no_local_runtime_remains() {
        // And before this rule existed, the local case was silently a no-op.
        let removed = provider("ollama", "ollama", true);
        let remaining = vec![provider("openrouter", "openrouter", true)];
        let mut routes = routes(&[("chat-v1", "local:llama3.1")]);
        let reset = scrub_removed(&mut routes, &removed, &remaining);
        assert_eq!(reset, vec!["chat-v1".to_string()]);
        assert_eq!(routes["chat-v1"], ProviderRef::Default);
    }

    #[test]
    fn a_slugless_route_with_nothing_to_serve_it_fails_closed() {
        let providers = vec![provider("openrouter", "openrouter", true)];
        let routes = routes(&[("chat-v1", "local:llama3.1")]);
        match provider_for_workload(Workload::Chat, &routes, &providers) {
            Resolution::Missing { slug, .. } => assert_eq!(slug, "local"),
            other => panic!("expected a named failure, got {other:?}"),
        }
    }

    #[test]
    fn a_slugless_route_whose_only_runtime_is_off_reports_disabled() {
        let providers = vec![provider("ollama", "ollama", false)];
        let routes = routes(&[("chat-v1", "local:llama3.1")]);
        match provider_for_workload(Workload::Chat, &routes, &providers) {
            Resolution::Disabled { slug, .. } => assert_eq!(slug, "local"),
            other => panic!("expected a disabled report, got {other:?}"),
        }
    }

    // ---- the second mechanism -----------------------------------------------

    #[test]
    fn a_route_edited_in_outside_the_ui_is_still_caught_at_load() {
        // The UI path can be bypassed by a hand-edited config or an older
        // build, so the invariant needs a second, independent check.
        let providers = vec![provider("acme", "openai_compatible", true)];
        let routes = routes(&[("chat-v1", "ghost:gpt-5"), ("reasoning-v1", "acme:gpt-5")]);
        assert_eq!(
            orphaned_routes(&routes, &providers),
            vec![("chat-v1".to_string(), "ghost".to_string())]
        );
    }

    // ---- the inferred mode --------------------------------------------------

    #[test]
    fn a_company_that_has_chosen_nothing_is_managed() {
        assert_eq!(infer_routing_mode(&Routes::new()), RoutingMode::Managed);
        assert_eq!(
            infer_routing_mode(&routes(&[("chat-v1", "managed"), ("vision-v1", "")])),
            RoutingMode::Managed
        );
    }

    #[test]
    fn one_provider_and_model_on_every_row_is_own() {
        let all = routes(&[
            ("chat-v1", "acme:gpt-5"),
            ("reasoning-v1", "acme:gpt-5"),
            ("agentic-v1", "acme:gpt-5"),
            ("vision-v1", "acme:gpt-5"),
        ]);
        assert_eq!(infer_routing_mode(&all), RoutingMode::Own);
    }

    #[test]
    fn a_single_differing_row_makes_it_advanced() {
        let mixed = routes(&[
            ("chat-v1", "acme:gpt-5"),
            ("reasoning-v1", "acme:gpt-5"),
            ("agentic-v1", "acme:gpt-5"),
            ("vision-v1", "acme:vision"),
        ]);
        assert_eq!(infer_routing_mode(&mixed), RoutingMode::Advanced);

        // Partly set is also advanced: "the same on every row" is not true of a
        // row that is unset.
        let partial = routes(&[("chat-v1", "acme:gpt-5")]);
        assert_eq!(infer_routing_mode(&partial), RoutingMode::Advanced);
    }

    #[test]
    fn the_mode_is_a_function_of_the_routes_and_nothing_else() {
        // There is no mode field, so there is nothing that can disagree with
        // the four routes. Re-deriving from the same map is stable.
        let map = routes(&[("chat-v1", "acme:gpt-5")]);
        assert_eq!(infer_routing_mode(&map), infer_routing_mode(&map.clone()));
    }

    #[test]
    fn every_routable_workload_owns_a_distinct_tier() {
        let mut tiers: Vec<&str> = ROUTABLE_WORKLOADS.iter().map(|w| w.tier()).collect();
        tiers.sort_unstable();
        let count = tiers.len();
        tiers.dedup();
        assert_eq!(tiers.len(), count, "two rows would write one tier's route");
        // And they are exactly the tiers the runtime has — no more, so a row
        // cannot address a tier nothing serves, and no fewer, so a tier cannot
        // be unreachable from the routing screen.
        let mut runtime_tiers = crate::company::types::INFERENCE_TIERS.to_vec();
        runtime_tiers.sort_unstable();
        assert_eq!(tiers, runtime_tiers);
    }

    #[test]
    fn a_tier_maps_back_to_its_workload() {
        assert_eq!(Workload::from_tier("chat-v1"), Some(Workload::Chat));
        assert_eq!(Workload::from_tier(" vision-v1 "), Some(Workload::Vision));
        assert_eq!(Workload::from_tier("nope-v1"), None);
        // Coding has no row, so no tier maps back to it.
        assert_ne!(Workload::from_tier("agentic-v1"), Some(Workload::Coding));
    }
    /// Removing a local runtime must scrub the routes naming it by slug.
    ///
    /// `ollama:llama3` parses as a `Cloud` ref — it carries a slug — while
    /// `category_of("ollama")` is `Local`. The cloud arm refused it on category
    /// and the local arm never saw it, because that arm only matches the
    /// slug-less `local` ref, so the two rules never met and removal scrubbed
    /// nothing. The consequence was the sharp part: `put_routes` fails closed on
    /// a route naming a provider nobody holds, so the table already on disk
    /// became unsaveable and the operator could not fix their own routing
    /// without rewriting every row.
    #[test]
    fn removing_a_local_runtime_scrubs_the_routes_that_name_it() {
        let ollama = provider("ollama", "ollama", true);
        let openrouter = provider("openrouter", "openrouter", true);
        let mut routes = Routes::new();
        routes.insert("chat-v1".into(), ProviderRef::parse("ollama:llama3"));
        routes.insert(
            "reasoning-v1".into(),
            ProviderRef::parse("openrouter:gpt-5"),
        );

        let reset = scrub_removed(&mut routes, &ollama, std::slice::from_ref(&openrouter));
        assert_eq!(reset, vec!["chat-v1".to_string()]);
        assert_eq!(routes.get("chat-v1"), Some(&ProviderRef::Default));
        assert_eq!(
            routes.get("reasoning-v1"),
            Some(&ProviderRef::parse("openrouter:gpt-5")),
            "another provider's row is untouched"
        );
        // And what is left is saveable, which is the property that actually
        // broke: every remaining route names something this company holds.
        assert!(orphaned_routes(&routes, &[openrouter]).is_empty());
    }

    /// The slug-less `local` ref keeps its own rule: it is orphaned only once no
    /// local runtime remains, because a second one still serves it.
    #[test]
    fn a_slug_less_local_route_survives_while_another_runtime_does() {
        let ollama = provider("ollama", "ollama", true);
        let lmstudio = provider("lmstudio", "lmstudio", true);
        let mut routes = Routes::new();
        routes.insert("chat-v1".into(), ProviderRef::parse("local:llama3"));

        let reset = scrub_removed(&mut routes, &ollama, std::slice::from_ref(&lmstudio));
        assert!(reset.is_empty(), "lmstudio still serves it");
        assert!(!scrub_removed(&mut routes, &ollama, &[]).is_empty());
    }
}
