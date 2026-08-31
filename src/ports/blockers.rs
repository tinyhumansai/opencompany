//! The blocker taxonomy: what kind of stop happened, and where it came from.
//!
//! A *blocker* is a stop that **a person can answer** — a bad model id, an
//! expired credential, a missing prerequisite, an agent's own question. Epic
//! #1860's premise is that these are conversations rather than failures: they
//! park durably, get asked, and resume from the step that stopped. This module
//! is the vocabulary that whole chain agrees on; the park itself rides
//! [`Effect`](crate::ports::types::Effect) through `CycleRunner::park`, and the
//! payload here is what that effect carries.
//!
//! # Why two axes and not one
//!
//! Issue #1861 named the axis "where the stop came from" (provider, tool,
//! prereq, an agent asking); issue #1866's sufficiency gate named it "what kind
//! of gap this is" (information, infrastructure, transient). Those are
//! different questions with overlapping answers — a provider error is
//! infrastructure *or* transient depending on which provider error it is, and a
//! missing prerequisite is always information — so a single enum could only be
//! one of them and would silently lose the other.
//!
//! Both are kept, with [`BlockerKind`] primary and [`BlockerSource`]
//! secondary. Everything that *routes* — whether to park at all, which recovery
//! rung #1866 tries first, which card shape #1862 renders — branches on the
//! kind. The source is provenance: it explains the blocker to a person and
//! groups related ones together, and nothing decides behaviour from it. Keeping
//! provenance out of the routing decision is what stops the two axes from
//! drifting back apart into two vocabularies for one concept.
//!
//! # Transient is the arm that declines to park
//!
//! [`BlockerKind::Transient`] is in the enum precisely so the classifier can
//! return one answer for the whole decision instead of an `Option` plus a
//! reason. A transient failure is *not* answerable by a person — asking someone
//! to confirm a socket timeout wastes their attention — so it routes back to
//! the ordinary retry-and-settle path and surfaces through issue #1865's
//! honest-verdict layer. [`BlockerKind::parks`] is the one place that rule is
//! written down.

use serde::{Deserialize, Serialize};

/// The dotted prefix every blocker effect kind carries.
///
/// The kind string is the *only* part of a park that reaches the console
/// without passing through redaction: `CompanyEvent::ApprovalParked` carries
/// `effect_kind` and deliberately not the payload, so that `pending_approvals`
/// stays the single place issue #372's host-side redaction runs. Putting the
/// gap class in the kind — `blocker.information`, not a bare `blocker` — lets a
/// console draw the right chip from the event alone instead of opening a second
/// surface that would have to redact.
pub const BLOCKER_EFFECT_PREFIX: &str = "blocker";

/// **What is missing.** The primary axis: it decides whether the work parks at
/// all, and which recovery is worth trying before a person is asked.
///
/// Serialized in `snake_case`, and [`Self::as_str`] returns the identical
/// literal so a stored tag and the JSON blob can never disagree — the same
/// contract [`SubjectKind`](crate::ports::notifications::SubjectKind) keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerKind {
    /// A person knows something the company does not: which environment to
    /// deploy to, which of two contradictory briefs is current, the prerequisite
    /// a plan is missing. No amount of retrying produces the answer, and no
    /// other agent has it either — though #1866 will try the roster before
    /// escalating.
    Information,
    /// A credential, connection or configuration is broken: a model id that
    /// does not exist, an expired OAuth grant, an MCP server that will not
    /// connect. Answerable, but only by the operator — no teammate knows the
    /// state of somebody else's integrations, which is why #1866 skips the
    /// ask-around rung for this kind and escalates straight to an action card.
    Infrastructure,
    /// It may simply work next time: a socket timeout, a rate limit, a blank
    /// completion. **Not a park** — see [`Self::parks`].
    Transient,
}

impl BlockerKind {
    /// The wire/storage token. Stable: journal lines persist it, so renaming a
    /// variant is a data migration rather than a cosmetic change.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Information => "information",
            Self::Infrastructure => "infrastructure",
            Self::Transient => "transient",
        }
    }

    /// Parses a wire token back into a kind, or `None`.
    ///
    /// The inverse of [`Self::as_str`] and deliberately beside it: a reader
    /// that grew its own literal table would be free to drift from the token
    /// the journal holds.
    pub fn from_wire(word: &str) -> Option<Self> {
        Some(match word {
            "information" => Self::Information,
            "infrastructure" => Self::Infrastructure,
            "transient" => Self::Transient,
            _ => return None,
        })
    }

    /// Whether a stop of this kind should **park and ask a person**.
    ///
    /// The single rule the epic turns on, written once so the three settle
    /// sites cannot each answer it differently. A park costs somebody's
    /// attention: it is worth spending when only a person can unblock the work,
    /// and wasted when the next attempt would have succeeded anyway.
    ///
    /// The match is exhaustive with no wildcard, so a future kind must state
    /// whether it is answerable rather than inheriting an answer.
    pub fn parks(self) -> bool {
        match self {
            Self::Information | Self::Infrastructure => true,
            Self::Transient => false,
        }
    }

    /// Which gap class a missing [`PrereqKind`] is (issue #1861).
    ///
    /// The planning pass already names *what* is missing; this says who can
    /// supply it, which is the axis that decides how the question is asked.
    ///
    /// A connection, a Composio account, an MCP server, a credential or a
    /// missing grant are all [`Infrastructure`](Self::Infrastructure): nobody
    /// on the roster knows the state of the operator's integrations, so #1866's
    /// bounded ask-around rung would spend turns discovering that. A missing
    /// file, an unresolved assignee, or a prerequisite this host cannot even
    /// classify are [`Information`](Self::Information) — a person knows, and so
    /// might a teammate.
    ///
    /// Never [`Transient`](Self::Transient): a prerequisite the pass could not
    /// satisfy does not satisfy itself by being retried.
    ///
    /// [`PrereqKind`]: crate::ports::tasks::PrereqKind
    pub fn for_prereq(kind: crate::ports::tasks::PrereqKind) -> Self {
        use crate::ports::tasks::PrereqKind;
        match kind {
            PrereqKind::Connection
            | PrereqKind::Composio
            | PrereqKind::Mcp
            | PrereqKind::Credential
            | PrereqKind::Permission => Self::Infrastructure,
            PrereqKind::File | PrereqKind::Assignee | PrereqKind::Other => Self::Information,
        }
    }

    /// The class for a *set* of missing prerequisites.
    ///
    /// [`Infrastructure`](Self::Infrastructure) wins whenever any member is:
    /// a plan blocked on both a missing credential and a missing brief still
    /// cannot start until the operator reconnects something, and routing the
    /// pair through the roster first would ask teammates about an integration
    /// none of them can see. The information half rides along in the reason.
    ///
    /// An empty set is [`Information`](Self::Information) — vacuous, and the
    /// caller has nothing to ask about anyway.
    pub fn for_prereqs(kinds: impl IntoIterator<Item = crate::ports::tasks::PrereqKind>) -> Self {
        kinds
            .into_iter()
            .map(Self::for_prereq)
            .fold(Self::Information, |acc, kind| {
                if acc == Self::Infrastructure || kind == Self::Infrastructure {
                    Self::Infrastructure
                } else {
                    kind
                }
            })
    }

    /// The dotted effect kind a park of this class carries, e.g.
    /// `blocker.information`.
    pub fn effect_kind(self) -> String {
        format!("{BLOCKER_EFFECT_PREFIX}.{}", self.as_str())
    }
}

/// **Where the stop came from.** Provenance — carried so a blocker can explain
/// itself and so related ones group, never branched on to decide behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerSource {
    /// The turn's own inference call: a rejected model id, an auth failure, a
    /// timeout, a terminal empty response.
    Provider,
    /// A tool, MCP server or third-party integration the turn reached for.
    Tool,
    /// The planning pass found the card cannot proceed — a missing
    /// prerequisite, no valid assignee.
    Prereq,
    /// An agent asked, through `escalate_to_human`. The one source the host did
    /// not classify: the agent judged its own ambiguity.
    AgentQuestion,
}

impl BlockerSource {
    /// The wire/storage token. Stable for the same reason as
    /// [`BlockerKind::as_str`].
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Tool => "tool",
            Self::Prereq => "prereq",
            Self::AgentQuestion => "agent_question",
        }
    }

    /// Parses a wire token back into a source, or `None`.
    pub fn from_wire(word: &str) -> Option<Self> {
        Some(match word {
            "provider" => Self::Provider,
            "tool" => Self::Tool,
            "prereq" => Self::Prereq,
            "agent_question" => Self::AgentQuestion,
            _ => return None,
        })
    }
}

/// Which stopped step a blocker is about.
///
/// Carried so the resume tiers (#1863 for tasks, #1864 for workflow nodes) can
/// find what to restart without re-deriving it from the approval's task link,
/// which only names the card and cannot name a node inside a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum BlockerStep {
    /// A board card's dispatch stopped.
    Task {
        /// The card.
        task_id: String,
    },
    /// One node inside a workflow run stopped.
    Node {
        /// The run the node belongs to.
        run_id: String,
        /// The node within that run's graph.
        node_id: String,
    },
}

/// The payload a parked blocker carries on its
/// [`Effect`](crate::ports::types::Effect).
///
/// Deliberately answerable-shaped rather than error-shaped: `reason` says what
/// stopped, `needed` says what would unstick it. A blocker whose `needed` is
/// empty is a failure wearing a question's clothes — it would reach a person
/// with nothing for them to do — so the classifier is expected to supply one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerPayload {
    /// What is missing. Routes.
    pub kind: BlockerKind,
    /// Where the stop came from. Explains.
    pub source: BlockerSource,
    /// The step that stopped, for the resume tiers — or `None` when the
    /// blocker is not attached to one.
    ///
    /// `None` is a real case, not a missing value: an agent that calls
    /// `escalate_to_human` mid-conversation is asking a question without a card
    /// or a node behind it. Where a card *does* exist, the approval record's
    /// own `TaskLink` already names it — `record_parked` stamps that from the
    /// cycle's task context — so this field is not the only path back to the
    /// work. It carries what that link cannot: a **node** inside a workflow
    /// run, which is what #1864's node-level restart needs and a task link has
    /// no way to express.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<BlockerStep>,
    /// What happened, in the words a person should read.
    pub reason: String,
    /// What would unblock it — the question being asked, or the action being
    /// requested.
    pub needed: String,
}

impl BlockerPayload {
    /// The dotted effect kind for this blocker, delegating to its
    /// [`BlockerKind`] so the two cannot describe different classes.
    pub fn effect_kind(&self) -> String {
        self.kind.effect_kind()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn kind_wire_round_trips() {
        for kind in [
            BlockerKind::Information,
            BlockerKind::Infrastructure,
            BlockerKind::Transient,
        ] {
            assert_eq!(BlockerKind::from_wire(kind.as_str()), Some(kind));
        }
        assert_eq!(BlockerKind::from_wire("nonsense"), None);
    }

    #[test]
    fn source_wire_round_trips() {
        for source in [
            BlockerSource::Provider,
            BlockerSource::Tool,
            BlockerSource::Prereq,
            BlockerSource::AgentQuestion,
        ] {
            assert_eq!(BlockerSource::from_wire(source.as_str()), Some(source));
        }
        assert_eq!(BlockerSource::from_wire("nonsense"), None);
    }

    /// The serde token and the hand-written token are the same string. They are
    /// written twice — `rename_all` and `as_str` — and a divergence would store
    /// one spelling in the journal and another in a serialized payload.
    #[test]
    fn serde_and_as_str_agree() {
        for kind in [
            BlockerKind::Information,
            BlockerKind::Infrastructure,
            BlockerKind::Transient,
        ] {
            let json = serde_json::to_string(&kind).expect("kind serializes");
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
        }
        for source in [
            BlockerSource::Provider,
            BlockerSource::Tool,
            BlockerSource::Prereq,
            BlockerSource::AgentQuestion,
        ] {
            let json = serde_json::to_string(&source).expect("source serializes");
            assert_eq!(json, format!("\"{}\"", source.as_str()));
        }
    }

    /// The rule the whole epic turns on: only the two answerable classes cost a
    /// person their attention.
    #[test]
    fn only_answerable_kinds_park() {
        assert!(BlockerKind::Information.parks());
        assert!(BlockerKind::Infrastructure.parks());
        assert!(
            !BlockerKind::Transient.parks(),
            "a transient failure is not answerable by a person and must retry, not ask"
        );
    }

    /// Only the operator can reconnect an integration, so those prerequisites
    /// must not be routed through the roster first.
    #[test]
    fn integration_prereqs_are_infrastructure_and_the_rest_are_information() {
        use crate::ports::tasks::PrereqKind;
        for kind in [
            PrereqKind::Connection,
            PrereqKind::Composio,
            PrereqKind::Mcp,
            PrereqKind::Credential,
            PrereqKind::Permission,
        ] {
            assert_eq!(BlockerKind::for_prereq(kind), BlockerKind::Infrastructure);
        }
        for kind in [PrereqKind::File, PrereqKind::Assignee, PrereqKind::Other] {
            assert_eq!(BlockerKind::for_prereq(kind), BlockerKind::Information);
        }
    }

    /// A prerequisite never resolves itself, so no prereq class may park as
    /// transient — that would settle the card and ask nobody.
    #[test]
    fn no_prereq_is_transient() {
        use crate::ports::tasks::PrereqKind;
        for kind in [
            PrereqKind::Connection,
            PrereqKind::Composio,
            PrereqKind::Mcp,
            PrereqKind::Credential,
            PrereqKind::File,
            PrereqKind::Permission,
            PrereqKind::Assignee,
            PrereqKind::Other,
        ] {
            assert!(BlockerKind::for_prereq(kind).parks());
        }
    }

    #[test]
    fn a_mixed_set_of_prereqs_takes_the_operator_route() {
        use crate::ports::tasks::PrereqKind;
        assert_eq!(
            BlockerKind::for_prereqs([PrereqKind::File, PrereqKind::Credential]),
            BlockerKind::Infrastructure,
            "a plan blocked on a credential cannot start however well the brief is answered"
        );
        assert_eq!(
            BlockerKind::for_prereqs([PrereqKind::File, PrereqKind::Assignee]),
            BlockerKind::Information
        );
        assert_eq!(
            BlockerKind::for_prereqs([]),
            BlockerKind::Information,
            "vacuous, and there is nothing to ask about"
        );
    }

    #[test]
    fn effect_kind_carries_the_gap_class() {
        assert_eq!(
            BlockerKind::Information.effect_kind(),
            "blocker.information"
        );
        assert_eq!(
            BlockerKind::Infrastructure.effect_kind(),
            "blocker.infrastructure"
        );
    }

    /// The payload's kind is the one that reaches the wire — a payload cannot
    /// announce one class on the event and carry another inside.
    #[test]
    fn payload_effect_kind_follows_its_kind() {
        let payload = BlockerPayload {
            kind: BlockerKind::Infrastructure,
            source: BlockerSource::Provider,
            step: Some(BlockerStep::Task {
                task_id: "task-1".to_string(),
            }),
            reason: "the model id `gpt-nonexistent` was rejected".to_string(),
            needed: "a model id this provider serves".to_string(),
        };
        assert_eq!(payload.effect_kind(), "blocker.infrastructure");
    }

    /// The step survives a round trip through JSON, because that is how it
    /// reaches the resume tiers: written into the effect payload at park time,
    /// read back after a restart.
    #[test]
    fn step_round_trips_through_json() {
        for step in [
            BlockerStep::Task {
                task_id: "task-1".to_string(),
            },
            BlockerStep::Node {
                run_id: "run-1".to_string(),
                node_id: "node-a".to_string(),
            },
        ] {
            let json = serde_json::to_string(&step).expect("step serializes");
            let back: BlockerStep = serde_json::from_str(&json).expect("step parses");
            assert_eq!(back, step);
        }
    }
}
