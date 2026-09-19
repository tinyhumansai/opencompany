//! Semantic handoff: who should take a message its author did not address.
//!
//! Every other way an agent hands work on in this company names its target —
//! `desk_post` with an `@id`, `desk_dm` with a `to` list, `@#desk` referral with
//! a desk. A **broadcast** names nobody: the author says what it found and what
//! should happen next, and the host works out who that is by *meaning*, through
//! one [`tinyhivemind_typesafe`] Choice over the desk's own roster.
//!
//! # Why this is not a fan-out
//!
//! The name is upstream's and it is misleading on its own. A broadcast reaches
//! the Choice's maximum **plus every candidate strictly above 20%**, bounded by
//! `round_width` — so a confident route reaches one teammate and an uncertain
//! one reaches two or three. Uncertainty widens the fan-out rather than gambling
//! on a single pick; it is not, and never becomes, a message to everybody.
//!
//! # Why it costs a second model call
//!
//! The author's turn is one call and the routing Choice is another. That is the
//! whole price of routing by meaning, and it is why `desk_post` with an explicit
//! `@id` stays the cheaper move and the right one whenever the author already
//! knows who should go next. This module exists for when it does not.
//!
//! # What a routing failure does
//!
//! Falls back to the **mechanical** routing this host did before semantic
//! routing existed — the desk's first roster agent. It never invents a recipient
//! and never silently drops the message: an unroutable broadcast that reached
//! nobody would be a finding the room never hears, which is worse than one
//! delivered to the wrong reader.
//!
//! That applies to every way routing can decline — no credential configured, a
//! transport outage, a stale roster, a malformed evaluation — so the worst case
//! of this module is the behaviour of the module it replaces.

use tinyhivemind::responder::Probability;
use tinyhivemind_embed::routing::Router;
use tinyhivemind_embed::{
    ConversationKind, ConversationRef, RouteCandidate, RoutingPlan, RoutingPolicy, RoutingRequest,
    RoutingSource, route_broadcast,
};
use tinyhivemind_typesafe::JevRouter;

/// The semantic router this host routes broadcasts through, or `None` when the
/// instance has no routing credential.
///
/// `None` is routing *off*, not a failure: a company without a key falls back to
/// explicit `@id` addressing, which is exactly how every desk worked before this
/// existed. Boot logs the absence rather than refusing to start.
///
/// # Errors
///
/// Only when a credential is present and the HTTP client cannot be built.
pub fn router_from_env()
-> Result<Option<JevRouter<super::typesafe::TypeSafeTransport>>, tinyhivemind_typesafe::Error> {
    Ok(super::typesafe::TypeSafeTransport::from_env()?
        .map(|transport| JevRouter::with_model(transport, super::typesafe::routing_model())))
}

/// Largest opening round a broadcast may assign, including the primary.
///
/// Four, not the roster size: the >20% rule already keeps a confident route
/// narrow, and this bounds the uncertain case. A desk that routes one finding to
/// five teammates has not handed work on, it has interrupted everybody.
const ROUND_WIDTH: usize = 4;

/// Maximum alternatives in one provider Choice, including `none`.
///
/// Eight covers a desk larger than any this host deliberates with today and
/// leaves the Choice small enough to stay a single decision rather than a
/// ranking exercise.
const CHOICE_OPTION_LIMIT: usize = 8;

/// Frozen thresholds for one broadcast Choice.
///
/// Deliberately permissive on confidence and clarification. A broadcast is a
/// *handoff*, not a commitment: the recipient reads the message and decides for
/// itself, so a low-confidence route costs one teammate one read, while
/// escalating or asking for clarification costs the room a turn it did not
/// budget. The bound that matters here is `round_width`, which is how many
/// people a hesitant router may interrupt.
fn policy() -> RoutingPolicy {
    RoutingPolicy {
        minimum_confidence: Probability::ZERO,
        high_impact_minimum_confidence: Probability::ZERO,
        clarification_threshold: Probability::ONE,
        high_impact_threshold: Probability::ONE,
        round_width: ROUND_WIDTH,
        choice_option_limit: CHOICE_OPTION_LIMIT,
    }
}

/// One desk member offered to the router, in deterministic desk order.
///
/// `role` is what the Choice actually matches the message against, so a desk
/// whose members carry no roles routes on their ids alone and will route badly.
/// That is a manifest problem rather than a routing one, and it is worth saying
/// out loud rather than papering over with a default.
#[must_use]
pub fn candidate(
    id: &str,
    label: &str,
    role: Option<String>,
    description: Option<String>,
) -> RouteCandidate {
    RouteCandidate {
        id: id.to_owned(),
        label: label.to_owned(),
        role,
        description,
        // Left empty deliberately. This host has no capability vocabulary a
        // router could match on, and inventing one from the tool belt would
        // route on what an agent *can* do rather than what it is *for* — which
        // is the same mistake as routing on ids.
        capabilities: Vec::new(),
        // Also empty, but for a different reason: the topics an agent has
        // actually worked on are derivable from the journal and would be a real
        // routing signal. Populating them is its own change with its own
        // evidence — a wrong learned topic routes confidently to the wrong
        // teammate, which is worse than routing on the role alone.
        learned_topics: Vec::new(),
        // Every candidate offered is one the caller has already filtered to a
        // live desk member, so availability is decided before the router sees
        // it rather than by it. A candidate this host would refuse to run
        // should not be in the list at all.
        available: true,
    }
}

/// Everything one broadcast needs routed, in one value.
///
/// A struct rather than eight parameters, because three of them are bare `&str`
/// and swapping `author` with `desk_id` at a call site would compile, route
/// nothing, and look like a provider problem. Named fields make that
/// unrepresentable.
pub struct Broadcast<'a> {
    /// Exactly what the author wrote. This is both the desk row and the text
    /// the Choice matches against; there is no separate routing hint.
    pub message: &'a str,
    /// The authoring agent, excluded from its own candidates.
    pub author: &'a str,
    /// Canonical desk id. Routing is desk-scoped and fails closed elsewhere.
    pub desk_id: &'a str,
    /// What this desk is for, when the manifest says. Sharpens the Choice.
    pub desk_purpose: Option<String>,
    /// Eligible members in deterministic desk order, author already removed.
    pub candidates: Vec<RouteCandidate>,
    /// Version of that snapshot. A mismatch is rejected as a stale roster.
    pub roster_version: u64,
    /// The **mechanical** responder an unroutable broadcast goes to.
    ///
    /// Pass
    /// [`desk_default_responder`](crate::runtime::delegation_tools::desk_default_responder)
    /// — the desk's first roster agent. That is the pre-selector behaviour and
    /// the same rung `built_in::selector` falls to, so the worst case of
    /// semantic routing is exactly the routing this host did before it existed.
    ///
    /// Deliberately the *mechanical* rung and not `MeteredSelector`: this value
    /// is resolved eagerly, before the Choice is asked, so anything costing a
    /// model call would be paid on every broadcast and thrown away on the ones
    /// that route. A roster lookup costs nothing. A caller that wants the
    /// selector's judgement on a fallback can detect
    /// [`RoutingPlan::Fallback`] from [`recipients`] and escalate then.
    pub fallback_responder: &'a str,
}

/// Route one agent-authored broadcast to the teammates best placed to take it.
///
/// The author is excluded from `candidates` by the caller **and** re-checked by
/// the library: a message routed back to its own author is a loop, and the
/// library fails it closed without spending a model call.
///
/// Returns the recipient ids in assignment order — the Choice maximum first,
/// then every option above the threshold. Empty only when the router asked to
/// clarify rather than route; every other outcome names at least the fallback.
pub async fn route(router: &(dyn Router + '_), broadcast: Broadcast<'_>) -> Vec<String> {
    let fallback_responder = broadcast.fallback_responder;
    let request = RoutingRequest {
        message: broadcast.message.to_owned(),
        source: RoutingSource::AgentBroadcast {
            author_id: broadcast.author.to_owned(),
        },
        conversation: ConversationRef {
            id: broadcast.desk_id.to_owned(),
            kind: ConversationKind::Desk,
            thread_root: None,
        },
        desk_purpose: broadcast.desk_purpose,
        thread_context: Vec::new(),
        candidates: broadcast.candidates,
        roster_version: broadcast.roster_version,
        policy: policy(),
    };
    // `None` for the reasoning router: escalation is a second provider call on
    // a decision this host has already declared cheap, and the policy above
    // never asks for one. Passing a router that cannot be reached would be the
    // more surprising choice.
    let plan = route_broadcast(Some(router), None, &request, fallback_responder).await;
    trace("broadcast", &request.message, &plan);
    recipients(&plan)
}

/// Route one **unaddressed desk message** to whoever should answer it.
///
/// The opening decision, where [`route`] is the mid-episode handoff. Same
/// router, same fallback discipline, different provenance:
/// [`RoutingSource::DeskMessage`] rather than `AgentBroadcast`.
///
/// `explicit_responder` is this host's already-resolved `@mention` and
/// short-circuits before any provider call — a named teammate outranks routing,
/// exactly as it outranks every other rung of the responder ladder. A direct or
/// workflow conversation falls back too: routing by meaning is a desk question.
///
/// Returns the ids in opening order. A [`RoutingPlan::Hive`] names a primary
/// **and** invitees, which is the router deciding this message deserves a room
/// rather than one responder — the caller opens an episode with all of them.
pub async fn route_desk_message(
    router: &(dyn Router + '_),
    opening: DeskMessage<'_>,
) -> Vec<String> {
    let explicit_responder = opening.explicit_responder;
    let fallback_responder = opening.fallback_responder;
    let request = RoutingRequest {
        message: opening.message.to_owned(),
        source: RoutingSource::DeskMessage,
        conversation: ConversationRef {
            id: opening.desk_id.to_owned(),
            kind: ConversationKind::Desk,
            thread_root: None,
        },
        desk_purpose: opening.desk_purpose,
        thread_context: Vec::new(),
        candidates: opening.candidates,
        roster_version: opening.roster_version,
        policy: policy(),
    };
    let plan = tinyhivemind_embed::route_message(
        Some(router),
        // No reasoning router, matching the reference runner: escalation is a
        // second provider call on a decision this host has priced as cheap, and
        // `policy()` never asks for one.
        None,
        &request,
        explicit_responder,
        fallback_responder,
    )
    .await;
    trace("desk_message", &request.message, &plan);
    recipients(&plan)
}

/// One unaddressed desk message needing a responder.
///
/// A struct for the reason [`Broadcast`] is one: four of its fields are bare
/// `&str` and transposing two at a call site would compile, route nothing, and
/// look like a provider fault.
pub struct DeskMessage<'a> {
    /// What was written, and what the Choice matches against.
    pub message: &'a str,
    /// Canonical desk id. Routing by meaning is a desk question.
    pub desk_id: &'a str,
    /// What this desk is for, when the manifest says.
    pub desk_purpose: Option<String>,
    /// Eligible members in deterministic desk order.
    pub candidates: Vec<RouteCandidate>,
    /// Version of that snapshot; a mismatch is rejected as a stale roster.
    pub roster_version: u64,
    /// This host's already-resolved `@mention`, which outranks routing.
    pub explicit_responder: Option<&'a str>,
    /// The mechanical responder when routing declines.
    pub fallback_responder: &'a str,
}

/// The agent ids one plan assigns, in assignment order.
///
/// [`RoutingPlan::Fallback`] names a responder too, and it is returned rather
/// than discarded: the library has already resolved the caller's deterministic
/// destination into it, so treating it as "nobody" would drop a message the
/// library considers delivered. Only [`RoutingPlan::Clarify`] yields nothing —
/// it carries an evaluation and no responder, because what it means is that the
/// router wants to ask rather than route.
#[must_use]
pub fn recipients(plan: &RoutingPlan) -> Vec<String> {
    match plan {
        RoutingPlan::One { responder_id, .. } | RoutingPlan::Fallback { responder_id, .. } => {
            vec![responder_id.clone()]
        }
        RoutingPlan::Hive {
            primary_id,
            invited_ids,
            ..
        } => std::iter::once(primary_id.clone())
            .chain(invited_ids.iter().cloned())
            .collect(),
        RoutingPlan::Clarify { .. } => Vec::new(),
    }
}

#[cfg(test)]
#[path = "broadcast_tests.rs"]
mod tests;

/// A [`Selector`](tinyhivemind::responder::Selector) backed by semantic routing.
///
/// This is the swap: the responder ladder's model rung stops being a tool-less
/// completion that names one id and becomes a Jev Choice that returns a real
/// distribution. The reference runner makes the same substitution — one semantic
/// router, no second model selector, a mechanical destination underneath.
///
/// # Why the distribution is the point
///
/// `MeteredSelector` commits to one id and reports no spread, so the host had to
/// synthesise `ONE` on the winner and `ZERO` on everyone else. Every rule that
/// reads a distribution — the `>20%` fan-out above all — is inert against a
/// synthetic one. A Jev evaluation carries the real spread, so an uncertain
/// route can widen instead of always naming exactly one teammate.
///
/// # What it does not change
///
/// An `@mention` still outranks it and never reaches here, and a declined
/// evaluation still leaves the ladder to fall to its deterministic rung. The
/// worst case of this rung remains the old rung.
#[derive(Debug)]
pub struct JevSelector<R> {
    router: R,
    desk_purpose: Option<String>,
}

impl<R> JevSelector<R> {
    /// Wrap one router as the ladder's semantic rung.
    #[must_use]
    pub const fn new(router: R, desk_purpose: Option<String>) -> Self {
        Self {
            router,
            desk_purpose,
        }
    }
}

impl<R: Router + Send + Sync> tinyhivemind::responder::Selector for JevSelector<R> {
    fn select<'a>(
        &'a self,
        request: &'a tinyhivemind::responder::SelectionRequest,
    ) -> tinyhivemind::responder::SelectorFuture<'a> {
        Box::pin(async move {
            let candidates: Vec<RouteCandidate> = request
                .candidates
                .iter()
                .map(|seat| {
                    candidate(
                        &seat.id,
                        &seat.label,
                        Some(seat.role.clone()),
                        seat.description.clone(),
                    )
                })
                .collect();
            let routing = RoutingRequest {
                message: request.message.clone(),
                source: RoutingSource::DeskMessage,
                conversation: ConversationRef {
                    id: request.desk_id.clone(),
                    kind: ConversationKind::Desk,
                    thread_root: None,
                },
                desk_purpose: self.desk_purpose.clone(),
                thread_context: Vec::new(),
                roster_version: candidates.len() as u64,
                candidates,
                policy: policy(),
            };
            let evaluation = self.router.evaluate(&routing).await?;
            // A near-total mapping: the two types describe the same answer from
            // either side of the seam. Only `none` is dropped — the ladder has
            // its own way to decline and does not want a pseudo-candidate.
            Ok(tinyhivemind::responder::SelectionEvaluation {
                choice: evaluation.primary_responder.clone(),
                probabilities: evaluation
                    .primary_probabilities
                    .iter()
                    .filter(|entry| entry.candidate_id != "none")
                    .map(|entry| tinyhivemind::responder::CandidateProbability {
                        candidate_id: entry.candidate_id.clone(),
                        probability: entry.probability,
                    })
                    .collect(),
                confidence: evaluation.confidence,
            })
        })
    }
}

/// Where a routing decision is written, under the instance data root.
///
/// One JSON object per line, appended. A routing rung nobody can audit is a
/// rung nobody can tune: a confident `0.94` and a coin-flip `0.34` produce the
/// same visible outcome, and without the distribution there is no way to tell a
/// good decision from a lucky one. The reference runner writes
/// `initial-route.json` and `routing-trace.json` for exactly this reason.
///
/// JSONL rather than one array, because a host appends across a run and a
/// truncated array is unreadable where a truncated line loses one record.
pub const TRACE_FILE: &str = "routing-trace.jsonl";

/// Record one routing decision, both as a log line and durably.
///
/// Never fails a route: a trace that cannot be written is reported and the
/// decision stands. Losing the audit trail is bad; refusing to route because the
/// disk is full would be worse.
fn trace(source: &str, message: &str, plan: &RoutingPlan) {
    let recipients = recipients(plan);
    // The log line carries the shape; the file carries everything.
    // A fallback is the interesting case, and it is the one that looks like
    // success from outside: the message still reaches somebody. Logged at
    // `warn` with its reason so a broken credential does not read as a healthy
    // route for the rest of a run.
    if let RoutingPlan::Fallback {
        responder_id,
        reason,
    } = plan
    {
        tracing::warn!(
            source,
            responder = %responder_id,
            ?reason,
            "[hive] routing declined; fell back to the mechanical responder"
        );
    } else {
        tracing::info!(
            source,
            recipients = ?recipients,
            "[hive] routed by meaning"
        );
    }

    let Some(root) = std::env::var_os("OPENCOMPANY_DATA_DIR") else {
        // No instance root configured: the log line above is the whole record.
        return;
    };
    let record = serde_json::json!({
        "source": source,
        // Bounded: a trace is for reviewing the decision, not for re-reading
        // the message, and an unbounded copy of every routed message would make
        // this file the largest thing in the data root.
        "message": message.chars().take(400).collect::<String>(),
        "recipients": recipients,
        "plan": plan,
    });
    let path = std::path::Path::new(&root).join(TRACE_FILE);
    let line = match serde_json::to_string(&record) {
        Ok(line) => line,
        Err(error) => {
            tracing::warn!(%error, "[hive] a routing decision could not be encoded for the trace");
            return;
        }
    };
    if let Err(error) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes()))
    {
        tracing::warn!(%error, path = %path.display(), "[hive] the routing trace could not be written");
    }
}
