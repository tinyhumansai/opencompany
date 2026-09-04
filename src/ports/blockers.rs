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

/// Whether `effect` is a parked blocker — its kind is `blocker.<class>` (issue
/// #1863).
///
/// The one predicate the resolve path branches on to keep a blocker away from
/// effect execution: a blocker's effect is inert, so a resolving `Approve` must
/// route to resume, never `perform_effect`. Kind-checked rather than
/// payload-checked because the kind is the part that survives redaction, the
/// same reason [`BlockerKind::effect_kind`] puts the class there.
pub fn is_blocker_effect(effect: &crate::ports::types::Effect) -> bool {
    effect
        .kind
        .starts_with(&format!("{BLOCKER_EFFECT_PREFIX}."))
}

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
    /// The root cause many parks share — a connection id, an integration name —
    /// so that ten cards stalled on one broken OAuth grant read as one question,
    /// not ten. `None` when the stop is particular to this step and groups with
    /// nothing.
    ///
    /// Provenance, not routing: it is the id the classify site already knows
    /// (which connection, which Composio account, which MCP server), copied
    /// verbatim. Nothing branches on it — the console folds parks that carry the
    /// same key into a single card, and a resolve fans its verdict to every park
    /// in the group. Additive with a serde default, so a park written before
    /// this field existed reads back as ungrouped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_key: Option<String>,
}

impl BlockerPayload {
    /// The dotted effect kind for this blocker, delegating to its
    /// [`BlockerKind`] so the two cannot describe different classes.
    pub fn effect_kind(&self) -> String {
        self.kind.effect_kind()
    }
}

/// What an operator's answer asks the stopped step to do (issue #1863).
///
/// The four-way verdict the resume path turns on. It is the durable twin of
/// [`BlockerReplyIntent`](crate::company::task_intent::BlockerReplyIntent)'s
/// four answering arms — the classifier decides it from an operator's words,
/// this carries it back into the stopped step so a restart mid-resume replays
/// the same decision rather than losing it.
///
/// Serialized `snake_case`, with [`Self::as_str`] returning the identical
/// literal, on exactly the terms [`BlockerKind`] keeps: the token is journaled,
/// so a rename is a data migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerVerdict {
    /// Run the stopped step again as it was.
    Retry,
    /// Answer or correct it — the [`BlockerResolution::answer`] carries what
    /// changed, and the step re-enters carrying it.
    Amend,
    /// Waive the blocker and let the work proceed as if it were satisfied.
    Skip,
    /// Abandon the stopped work. The one verdict that does not re-enter.
    Cancel,
}

impl BlockerVerdict {
    /// The wire/storage token.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::Amend => "amend",
            Self::Skip => "skip",
            Self::Cancel => "cancel",
        }
    }

    /// Parses a wire token back into a verdict, or `None`.
    pub fn from_wire(word: &str) -> Option<Self> {
        Some(match word {
            "retry" => Self::Retry,
            "amend" => Self::Amend,
            "skip" => Self::Skip,
            "cancel" => Self::Cancel,
            _ => return None,
        })
    }

    /// Whether this verdict **re-enters** the stopped step.
    ///
    /// Every verdict but [`Cancel`](Self::Cancel) resumes: a retry runs the step
    /// again, an amend runs it with the answer, a skip proceeds past it. Cancel
    /// abandons the work and starts nothing — the one place the resume fork
    /// short-circuits before any cycle.
    pub fn resumes(self) -> bool {
        !matches!(self, Self::Cancel)
    }

    /// The [`Verdict`](crate::ports::types::Verdict) the approval event records.
    ///
    /// The gate has only two decisions, so the four blocker verdicts lower onto
    /// them: every resuming verdict is an [`Approve`](crate::ports::types::Verdict::Approve)
    /// — the operator answered, and the durable approval says so — while
    /// [`Cancel`](Self::Cancel) is a [`Deny`](crate::ports::types::Verdict::Deny).
    /// The rich four-way answer rides the resolution beside the event, never the
    /// event itself, so the two-value audit surface is unchanged.
    pub fn event_verdict(self) -> crate::ports::types::Verdict {
        use crate::ports::types::Verdict;
        match self {
            Self::Retry | Self::Amend | Self::Skip => Verdict::Approve,
            Self::Cancel => Verdict::Deny,
        }
    }
}

/// The operator's durable answer to a parked blocker (issue #1863).
///
/// Banked before the detached resume spawns and re-armed at boot, so a restart
/// between the answer and the re-entry replays it rather than dropping the
/// operator's decision on the floor — the same restart-durable pattern
/// `ApprovalContinuation` keeps for an explicit request.
///
/// It carries what the two-value approval [`Verdict`](crate::ports::types::Verdict)
/// cannot: which of the four things the operator asked for, and — for an
/// [`Amend`](BlockerVerdict::Amend) — the words that answer the question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerResolution {
    /// What the operator asked the stopped step to do.
    pub verdict: BlockerVerdict,
    /// The operator's answer, when they gave one — the correction an
    /// [`Amend`](BlockerVerdict::Amend) re-enters carrying. Empty for a bare
    /// retry, skip or cancel, which need no words, and skipped on the wire when
    /// empty so a resolution written without one reads back the same.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answer: String,
    /// Which stopped step this answer re-enters, copied off the parked
    /// blocker's [`BlockerPayload::step`] at resolve time.
    ///
    /// Carried here rather than re-read from the approval at resume time
    /// because the journal **scrubs a parked effect's payload** once it is
    /// recorded (issue #372's redaction), so the step is gone from the durable
    /// approval by the time the detached resume runs. Banking it on the
    /// resolution makes the answer self-contained: the resume knows which card
    /// or node to re-enter without the payload, and a restart replays it whole.
    ///
    /// `None` for a bare agent question with no step behind it, where the whole
    /// of the resume is carrying the answer back into the DM it was asked in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<BlockerStep>,
    /// The seq the operator's own `OperatorMessage` was already appended
    /// under, when the answer arrived by being typed directly into the
    /// blocked conversation — `None` for one typed into the Approvals card's
    /// answer box instead (CodeRabbit review, PR #2054: "Reuse the
    /// already-journaled chat answer").
    ///
    /// `accept_chat_turn` journals the operator's chat message *before*
    /// `apply_blocker_reply` ever banks a resolution from it — the same
    /// pre-journal-then-cycle ordering [`CompanyRuntime::run_journaled_cycle`]
    /// exists for generally. A bare agent question's resume
    /// (`drive_blocker_resume`'s no-step arm) used to always synthesize a
    /// *fresh* `OperatorMessage` with the same text via `deliver_blocker_answer`
    /// regardless of that — so a DM answer landed in the transcript, and in the
    /// agent's own context, twice. This seq is how that arm tells the two
    /// origins apart: present, it re-enters the cycle on the event already on
    /// the journal instead of minting a second one; absent, nothing was
    /// journaled yet and `deliver_blocker_answer` still has to be the one that
    /// does it.
    ///
    /// [`CompanyRuntime::run_journaled_cycle`]: crate::company::runtime::CompanyRuntime::run_journaled_cycle
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journaled_answer_seq: Option<crate::ports::types::EventSeq>,
}

impl BlockerResolution {
    /// A resolution carrying a verdict, no answer text and no step.
    pub fn new(verdict: BlockerVerdict) -> Self {
        Self {
            verdict,
            answer: String::new(),
            step: None,
            journaled_answer_seq: None,
        }
    }

    /// A resolution carrying an operator's answer.
    pub fn answered(verdict: BlockerVerdict, answer: impl Into<String>) -> Self {
        Self {
            verdict,
            answer: answer.into(),
            step: None,
            journaled_answer_seq: None,
        }
    }

    /// The same resolution with its stopped step recorded.
    pub fn with_step(mut self, step: Option<BlockerStep>) -> Self {
        self.step = step;
        self
    }

    /// The same resolution, marked as answering a message already journaled
    /// at `seq` — see [`Self::journaled_answer_seq`].
    pub fn with_journaled_answer_seq(mut self, seq: Option<crate::ports::types::EventSeq>) -> Self {
        self.journaled_answer_seq = seq;
        self
    }

    /// Whether resolving with this re-enters the stopped step — a shorthand for
    /// [`BlockerVerdict::resumes`].
    pub fn resumes(&self) -> bool {
        self.verdict.resumes()
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
            group_key: None,
        };
        assert_eq!(payload.effect_kind(), "blocker.infrastructure");
    }

    /// `group_key` is additive: a payload serialized before it existed — with no
    /// such key — reads back as ungrouped rather than failing to parse, and a
    /// present key round-trips.
    #[test]
    fn group_key_is_additive_and_round_trips() {
        let legacy = serde_json::json!({
            "kind": "infrastructure",
            "source": "tool",
            "reason": "could not connect to mcp server `slack`",
            "needed": "the integration reconnected from Apps"
        });
        let parsed: BlockerPayload =
            serde_json::from_value(legacy).expect("a pre-field payload still parses");
        assert_eq!(parsed.group_key, None);

        let grouped = BlockerPayload {
            group_key: Some("connection:slack".to_string()),
            ..parsed
        };
        let json = serde_json::to_value(&grouped).expect("serializes");
        let back: BlockerPayload = serde_json::from_value(json).expect("parses");
        assert_eq!(back.group_key.as_deref(), Some("connection:slack"));
    }

    #[test]
    fn verdict_wire_round_trips() {
        for verdict in [
            BlockerVerdict::Retry,
            BlockerVerdict::Amend,
            BlockerVerdict::Skip,
            BlockerVerdict::Cancel,
        ] {
            assert_eq!(BlockerVerdict::from_wire(verdict.as_str()), Some(verdict));
            let json = serde_json::to_string(&verdict).expect("verdict serializes");
            assert_eq!(json, format!("\"{}\"", verdict.as_str()));
        }
        assert_eq!(BlockerVerdict::from_wire("nonsense"), None);
    }

    /// The mapping the sharpest risk turns on: only [`BlockerVerdict::Cancel`]
    /// denies, and only it declines to re-enter. Every answering verdict is an
    /// approve that resumes — and an approve that must never execute the inert
    /// blocker effect (that guard lives in the resolve path).
    #[test]
    fn only_cancel_denies_and_declines_to_resume() {
        use crate::ports::types::Verdict;
        for verdict in [
            BlockerVerdict::Retry,
            BlockerVerdict::Amend,
            BlockerVerdict::Skip,
        ] {
            assert_eq!(verdict.event_verdict(), Verdict::Approve, "{verdict:?}");
            assert!(verdict.resumes(), "{verdict:?} must re-enter the step");
        }
        assert_eq!(BlockerVerdict::Cancel.event_verdict(), Verdict::Deny);
        assert!(
            !BlockerVerdict::Cancel.resumes(),
            "cancel abandons the work and starts nothing"
        );
    }

    /// The answer is additive: a resolution written with no words reads back
    /// with none, and one carrying an amendment round-trips.
    #[test]
    fn resolution_answer_is_additive_and_round_trips() {
        let bare = BlockerResolution::new(BlockerVerdict::Retry);
        let json = serde_json::to_value(&bare).expect("serializes");
        assert_eq!(json, serde_json::json!({ "verdict": "retry" }));
        let back: BlockerResolution = serde_json::from_value(json).expect("parses");
        assert_eq!(back, bare);
        assert_eq!(back.answer, "");

        let amended = BlockerResolution::answered(BlockerVerdict::Amend, "use gpt-4o-mini");
        let json = serde_json::to_value(&amended).expect("serializes");
        let back: BlockerResolution = serde_json::from_value(json).expect("parses");
        assert_eq!(back, amended);
        assert!(back.resumes());
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
