//! Talking as a tool call.
//!
//! # Why speaking was the one thing that was not a tool
//!
//! Every other thing an agent can do in this company is a tool: it writes a
//! ledger row with `record_entry`, opens a card with `spawn_task`, reads a
//! sibling thread with `read_thread`. Speaking was not. A turn's **return text**
//! was the message, journaled on the agent's behalf by
//! [`DeskChannel::send`](crate::runtime::channel) or by the chat route. So an
//! agent could not choose a recipient, could not say something to one teammate
//! rather than to the room, and could not decline to speak — the only way to
//! stay quiet was to return an empty string, which reads as a failed turn.
//!
//! These are the tools that close that. The names, the argument shapes and the
//! description text all come from
//! [`tinyhivemind::speech`](tinyhivemind_hive::speech), which states them once,
//! as data, and asks a host to render them verbatim. Nothing here invents a
//! contract: `interpret` reads the call, and this module does what the crate's
//! own rule says a host does — **"a tool call is a request to speak; the host
//! appends, the host decides."**
//!
//! # The names are prefixed
//!
//! The crate's names are bare (`post`, `dm`, `close`, `read`) and it explicitly
//! anticipates a namespacing host: *"an MCP server called `desk` serving `post`
//! presents it as `desk_post`, and the descriptions are written to read
//! correctly either way."* This belt is namespaced, because `read` and `post`
//! are far too generic to sit unqualified beside `read_thread`,
//! `read_ledger` and `pages_read` — a model reaching for "read" would have four
//! plausible answers and no way to pick.
//!
//! # A DM starts at most one bounded recipient turn
//!
//! `desk_dm` journals every recipient row, then asks TinyHiveMind's bounded
//! mention-dispatch algebra for at most one recipient turn. The existing
//! post-turn drain executes it and writes its reply into that DM. Additional
//! recipients receive the durable row through their next session delta; one
//! message never fans out into several immediate turns, and conversation alone
//! never opens a task card.
//!
//! # On by default
//!
//! Registered unless the manifest says `[speech] disabled = true`. A company
//! that opts out keeps the legacy path, and an agent that has
//! the tools but answers without calling one still has its return text
//! journaled — see [`crate::harness::built_in::speech_fallback`]. Going silent
//! because a model forgot to call a tool is not an acceptable failure mode.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use oh::tools::traits::{PermissionLevel, Tool, ToolResult};
use openhuman_core as oh;
use tinyhivemind_hive::speech::{self, CallArguments, ToolCall, Utterance, UtteranceRejection};

use crate::ports::events::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq};

/// Say one thing to the whole channel.
pub const POST_TOOL: &str = "desk_post";
/// Say one thing to named teammates only.
pub const DM_TOOL: &str = "desk_dm";
/// Say one last thing and report the work finished.
pub const CLOSE_TOOL: &str = "desk_close";
/// Read further back than the window this turn was handed.
pub const READ_TOOL: &str = "desk_read";

/// Every tool name this belt registers, for the registrar and its tests.
pub const SPEECH_TOOLS: [&str; 3] = [POST_TOOL, CLOSE_TOOL, READ_TOOL];

/// The bare crate-side name behind one of ours.
///
/// The prefix is this host's, so it is stripped before the crate is asked —
/// `interpret` is documented to take the bare name.
fn bare(name: &str) -> &str {
    name.strip_prefix("desk_").unwrap_or(name)
}

/// The crate's own description for a tool, rendered verbatim.
///
/// Verbatim is the contract: the descriptions *are* the contract text, and they
/// are the only place a seat is told that text outside a tool call reaches
/// nobody. Falls back to a plain sentence only if the crate ever stops naming a
/// tool this belt registers, which its own tests make unlikely.
fn crate_description(name: &str) -> &'static str {
    // **`desk_dm` is an ASK on this host, and the crate's text says it is not.**
    //
    // TinyHiveMind describes `dm` as "say one thing to named peers … to settle
    // a disagreement", adds that "the room is told the exchange happened and
    // not what it said", and prices it at the seat's one message for the turn.
    // Read against "find out what this teammate knows", that is a cost with no
    // return, and a seat holding a factual gap correctly picks
    // `delegate_to_teammate` instead — the only tool on the belt that promised
    // an answer. It costs a board card per call and, on a message the triage
    // read as work, three of them (live run: three `in_progress` cards for one
    // question).
    //
    // Here the recipient's turn runs inline and their reply IS this call's
    // result (see `run_recipient_turn`), so the crate's sentence is no longer
    // true of this host and describes the one behaviour that would stop the
    // tool being used. Overridden rather than patched in `vendor/`: the crate's
    // text is right for a host that only queues, which is still what this falls
    // back to when no engine is in scope.
    if bare(name) == bare(DM_TOOL) {
        return "Ask named teammates something and get their reply back as this call's result. \
                Use it when the answer is someone else's to give — their tools, their desk, their \
                call — instead of guessing or handing them the whole job. They answer now, in \
                this turn, so you can use what they say in the reply you are composing. Costs \
                your one message for the turn, and opens no board card.";
    }
    speech::tool_specs()
        .iter()
        .find(|spec| spec.name == crate_spec_name(name))
        .map(|spec| spec.description)
        .unwrap_or("Say one thing to this channel.")
}

/// The name the crate lists one of this belt's tools under.
///
/// Usually just [`bare`]. The exception is `desk_close`: upstream renamed the
/// spec to `complete_episode` — the seat is "reporting, not ending the desk",
/// and the new spelling says so — while keeping `close` a valid *input*, both
/// in `interpret` and as a serde alias on stored rows.
///
/// So the tool this host registers is unaffected at runtime and every journaled
/// row still reads, but the spec lookup silently missed and
/// [`crate_description`] fell through to its "Say one thing to this channel."
/// default. That default is not a paraphrase of `complete_episode`, it is a
/// different tool's sentence — a seat told it would have had no way to learn
/// that this is the call that reports work finished. The belt's own test caught
/// it; this mapping is the fix, and renaming the registered tool is a separate
/// change with its own prompt and manifest fallout.
fn crate_spec_name(name: &str) -> &str {
    match bare(name) {
        "close" => "complete_episode",
        other => other,
    }
}

/// What every speech tool needs: who is speaking, where, and the journal.
#[derive(Clone)]
pub struct SpeechContext {
    company: CompanyId,
    agent_id: String,
    events: Arc<dyn EventLog>,
    store: Arc<dyn crate::ports::store::CompanyStore>,
    dispatch: Option<crate::harness::orchestrator::DelegationQueue>,
}

/// What happened when a `desk_dm` tried to run its recipient's turn inline.
///
/// Three outcomes and not two, because "no reply" hides a fork that decides
/// whether the message may be queued afterwards. `None` used to cover both
/// "nobody ran" and "ran, then failed or said nothing" — so a peer whose turn
/// had already executed, with whatever tool effects that turn had, was queued
/// to execute a second time. That reintroduces the very thing this path exists
/// to remove: a recipient answering after the asking turn has closed (CodeRabbit,
/// #2368).
enum PeerTurn {
    /// Nobody ran — no engine in scope, or the hop cap was already reached.
    /// Queueing is safe and is exactly what this path did before.
    NotAttempted,
    /// The turn ran and produced a reply.
    Answered {
        reply: String,
        /// Whether the reply reached the pair thread. A reply that could not be
        /// journaled is still the peer's answer and is still worth handing
        /// back, but it must not be described as a durable exchange: the chip
        /// renders from those rows and later turns read them.
        recorded: bool,
    },
    /// The turn RAN and produced nothing usable. Must not be queued: the peer
    /// has already had its turn, and whatever it did before failing has already
    /// happened.
    Ran,
}

impl SpeechContext {
    pub fn new(
        company: CompanyId,
        agent_id: String,
        events: Arc<dyn EventLog>,
        store: Arc<dyn crate::ports::store::CompanyStore>,
    ) -> Self {
        Self {
            company,
            agent_id,
            events,
            store,
            dispatch: None,
        }
    }

    /// Attach the post-turn drain that turns a committed `desk_dm` into one
    /// bounded recipient turn. Tests and non-harness callers may omit it; the
    /// durable message still lands and is picked up by a later session delta.
    pub fn with_dispatch(
        mut self,
        dispatch: crate::harness::orchestrator::DelegationQueue,
    ) -> Self {
        self.dispatch = Some(dispatch);
        self
    }

    /// The channel this turn is answering in.
    ///
    /// `None` is a refusal, not a wildcard — the same rule `read_thread`
    /// applies. A turn with no conversation (a dispatched card, a workflow
    /// node) has no channel to speak into, and posting into a guessed one would
    /// put a line in front of people who were not in the exchange.
    fn channel(&self) -> Option<String> {
        crate::runtime::delegation::turn_conversation()
    }

    /// Which channel a `desk_post` should land in.
    ///
    /// `None` is the channel the turn is already in — the overwhelmingly common
    /// case, and the only one before a live run showed that an agent asked to
    /// say something *somewhere else* had no way to do it and answered the
    /// person who asked instead.
    ///
    /// A named desk is resolved against
    /// [`agent_channels`](crate::server::chat_history::agent_channels) — the
    /// same function that decides which channels reach this agent's own
    /// session. That is what keeps the two honest: an agent can speak exactly
    /// where it can hear, and nowhere else.
    ///
    /// A desk it does not sit on is refused rather than widened to. Reaching
    /// another desk is a *referral* — a crossing the library already models,
    /// with its own provenance chip and its own return path — and letting
    /// `desk_post` write into a room its author is not in would put a line in
    /// front of people with no record of who let it in.
    async fn resolve_desk(&self, desk: Option<&str>, ambient: &str) -> Result<String, ToolResult> {
        let Some(desk) = desk.map(str::trim).filter(|desk| !desk.is_empty()) else {
            return Ok(ambient.to_string());
        };
        let wanted = desk.trim_start_matches('#');
        let Ok(Some(record)) = self.store.load(&self.company).await else {
            // The roster could not be read, so membership cannot be checked.
            // Falling back to the ambient channel would silently say it
            // somewhere other than asked, which is the defect this argument
            // exists to fix — so it refuses instead.
            return Err(ToolResult::error(
                "The roster could not be read, so I cannot tell whether you sit on that channel.                  Say it here instead, or try again."
                    .to_string(),
            ));
        };
        let channels = crate::server::chat_history::agent_channels(&record, &self.agent_id);
        // Codex P2: an exact `id` match is resolved first and alone — desk
        // names are not unique, so a name/label match must not outrank (or be
        // outranked by iteration order against) a desk whose `id` is exactly
        // what was asked for. Only once no id matches are we in name/label
        // territory, and there an ambiguous match — more than one channel
        // answering to the same name — is refused rather than silently
        // resolved to whichever happened to iterate first.
        let found = if let Some(channel) = channels
            .iter()
            .find(|channel| channel.id.eq_ignore_ascii_case(wanted))
        {
            Some(channel)
        } else {
            let mut by_name = channels.iter().filter(|channel| {
                channel.name.eq_ignore_ascii_case(wanted)
                    || channel.label.eq_ignore_ascii_case(wanted)
            });
            match (by_name.next(), by_name.next()) {
                (Some(only), None) => Some(only),
                (Some(_), Some(_)) => {
                    return Err(ToolResult::error(format!(
                        "More than one channel you sit on answers to `{desk}`. Use its id instead."
                    )));
                }
                (None, _) => None,
            }
        };
        match found {
            Some(channel) => Ok(channel.id.clone()),
            None => {
                let reachable = channels
                    .iter()
                    .map(|channel| channel.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(ToolResult::error(format!(
                    "You do not sit on `{desk}`, so you cannot post there. You can post in:                      {reachable}. To reach another desk, refer the work across instead."
                )))
            }
        }
    }

    /// Says one thing to the whole channel.
    ///
    /// **Does not append.** The crate's rule is that a tool call is a *request*
    /// to speak and the host appends — and the host that appends here is the
    /// reply path that has always appended, because it is the one carrying the
    /// folded steps, the live SSE frame, the resolved mentions and the
    /// board-card correlation. A tool holds none of those, so a tool that
    /// appended directly would produce a bubble poorer than the one the same
    /// sentence gets today.
    ///
    /// Falls back to appending only when there is no sink — a turn run outside
    /// the harness's tracking scope, where being heard beats being well
    /// formatted.
    async fn post_to_channel(&self, chat_id: String, text: String) -> ToolResult {
        if text.trim().is_empty() {
            return ToolResult::error(
                "A message with no text reaches nobody. Say what you mean, or call no tool at all."
                    .to_string(),
            );
        }
        if crate::runtime::delegation::collect_utterance(text.clone()) {
            crate::runtime::delegation::mark_turn_spoke();
            return ToolResult::success(
                "Said to the channel. It is journaled when this turn ends.".to_string(),
            );
        }
        self.say(chat_id, text, Vec::new()).await
    }

    /// Appends one line to the journal, with the audience the caller resolved.
    ///
    /// `audience` empty is the ordinary channel-visible case. A non-empty one is
    /// a private aside **within one desk's deliberation**: every party is on
    /// that desk, and the list narrows which of them may read the row.
    ///
    /// That is why a DM does NOT use it — see [`Self::dm`]. An `audience` is a
    /// narrowing *inside* a channel, and it cannot carry a row *across* one.
    ///
    /// This is the fallback for a post with no sink, and the append every
    /// direct row goes through.
    /// Leaves one line for each named teammate, **in that teammate's own DM
    /// channel**.
    ///
    /// # Why not the ambient channel with a narrowed audience
    ///
    /// Because that delivers nowhere, and says it delivered. It was the first
    /// shape of this method and it was wrong in a way only a live run showed:
    /// a row journaled under the *current* conversation with
    /// `audience: ["motion_designer"]` sits in the operator's DM with the
    /// speaker. Which channels reach an agent is decided by
    /// [`agent_channels`](crate::server::chat_history::agent_channels), and the
    /// speaker's own DM is not one of the recipient's — so the recipient could
    /// never read it, while the operator could. The message went to exactly the
    /// wrong person, and the tool reported success.
    ///
    /// `audience` was reached for because it needed no journal migration. It is
    /// the asides field, and an aside is a narrowing *within a desk everybody
    /// named is already on*. A DM has no such guarantee, so the narrowing has to
    /// be the channel itself.
    ///
    /// So: one row per recipient, `chat_id` set to the recipient's own DM key —
    /// the bare teammate id, which is the spelling `agent_channels` registers
    /// and the console already posts under — and an empty `audience`, because
    /// the channel has done the narrowing and a non-empty one would additionally
    /// make [`fold_asides`](crate::server::chat_history) lift the row out of the
    /// transcript as a deliberation aside, which it is not.
    ///
    /// Once every row is durable, TinyHiveMind may select the first eligible
    /// recipient for one immediate bounded turn. Other recipients read the row
    /// on their next turn. The wakeup is conversation-only and opens no card.
    async fn dm(
        &self,
        peers: Vec<String>,
        text: String,
        record: &crate::ports::types::CompanyRecord,
    ) -> ToolResult {
        if text.trim().is_empty() {
            return ToolResult::error(
                "A message with no text reaches nobody. Say what you mean, or call no tool at all."
                    .to_string(),
            );
        }
        let mut left_for: Vec<String> = Vec::new();
        // coderabbit: an earlier version returned the first journal error for
        // the whole call, even though every recipient before it had already
        // been durably appended. A retry after that error re-sent the text to
        // every one of them a second time — the exact "partial write read as
        // total failure" defect this tracks both outcomes to avoid. Every
        // recipient is now attempted regardless of an earlier one's failure,
        // and the reply says which of them actually got a row so a retry (by
        // the model, or by whoever reads the result) can address only the
        // ones that still need it.
        let mut failed_for: Vec<(String, String)> = Vec::new();
        let mut first_committed: Option<(String, String, EventSeq)> = None;
        for peer in &peers {
            // Codex P1: a desk whose id happens to equal this recipient's
            // agent id also "owns" a row journaled under the bare id
            // (`chat_history::owns` matches on the stored `chat_id` alone), so
            // every member of that desk could read a supposedly private
            // `desk_dm`. `agent_channels` already registers the `dm:<id>`
            // spelling as this teammate's own line for exactly this reason —
            // reach for it whenever the bare id collides with a desk, so the
            // row is journaled somewhere only that desk's own id would match,
            // which is far less likely to collide.
            //
            // **The pair's own thread (#2368).**
            //
            // A DM used to land on the recipient's own line — the same
            // conversation the OPERATOR uses to DM that teammate. Three faults
            // followed: the operator read agent-to-agent traffic in their own
            // DM thread, authored by somebody not talking to them; the sender
            // could not read it back, since `agent_channels` gives an agent its
            // own line and not its peers'; and the same pair's referral
            // exchanges already lived in `dm:<a>+<b>`, so one relationship was
            // split across conversations by which mechanism spoke.
            //
            // Lands WITH the marker `mark_turn_dms` mints, never without it. On
            // its own this write is invisible: no console surface renders a
            // pair thread, so the exchange would go from misplaced to missing.
            // The two are one change.
            //
            // `pair_conversation` sorts the ids so direction cannot fork the
            // thread, and its `dm:` prefix cannot collide with a desk slug —
            // which retires the bare-id collision guard this line used to need.
            let key = crate::hivemind::referral::pair_conversation(&self.agent_id, peer);
            let seq = match self.append(key.clone(), text.clone(), Vec::new()).await {
                Ok(seq) => seq,
                Err(error) => {
                    failed_for.push((peer.clone(), error));
                    continue;
                }
            };
            first_committed.get_or_insert((peer.clone(), key, seq));
            // A DM is a hop from one openhuman session to another, and
            // this is the only place both ends are known at once. Both
            // teammates hold a live openhuman session named
            // `{company}:{agent_id}` (see `harness::session_key`); the
            // row just journaled leaves the sender's and is picked up
            // by the recipient's `prepare_delta` on its next turn.
            //
            // Logged rather than returned: the recipient's session id
            // is an internal name, and the agent has no use for it —
            // what the *operator* has a use for is being able to follow
            // one line between two sessions when a hundred of them are
            // live at once, which is exactly when the reply text alone
            // stops being enough to tell who heard what.
            tracing::debug!(
                from_session = %crate::harness::session_key::openhuman_session_key(
                    &self.company,
                    &self.agent_id,
                ),
                to_session = %crate::harness::session_key::openhuman_session_key(
                    &self.company,
                    peer,
                ),
                "[speech] dm left in the pair's own thread"
            );
            left_for.push(format!("@{peer}"));
        }
        if first_committed.is_some() {
            crate::runtime::delegation::mark_turn_spoke();
        }
        // Ask, then wait for the answer — and only fall back to posting the
        // letter when nobody can be run now (#2368).
        let mut answered: Option<(String, String, bool)> = None;
        let mut woke_recipient = false;
        let mut ran_without_answer = false;
        if let Some((peer, chat_id, trigger)) = first_committed {
            match self
                .run_recipient_turn(record, &peer, &chat_id, &text)
                .await
            {
                PeerTurn::Answered { reply, recorded } => {
                    answered = Some((peer, reply, recorded));
                }
                // Queue ONLY when nobody ran. A turn that ran and then failed
                // has already spent itself, and queueing it would run the peer
                // a second time — repeating whatever its first attempt did and
                // landing an answer after this turn has closed.
                PeerTurn::NotAttempted => {
                    woke_recipient =
                        self.stage_recipient_turn(record, &peer, chat_id, trigger, &text);
                }
                PeerTurn::Ran => ran_without_answer = true,
            }
        }
        if !failed_for.is_empty() {
            let failures = failed_for
                .iter()
                .map(|(peer, error)| format!("@{peer} ({error})"))
                .collect::<Vec<_>>()
                .join("; ");
            return if left_for.is_empty() {
                ToolResult::error(format!("Could not leave it for anyone: {failures}"))
            } else {
                // Not `ToolResult::error`: some of it genuinely landed, and an
                // agent that reads "error" here and retries the whole call
                // would journal a second row for everyone already in
                // `left_for`. The text says exactly who still needs it.
                ToolResult::success(format!(
                    "Left for {}. Could not reach {} — retry `desk_dm` with only the names that \
                     failed.",
                    left_for.join(", "),
                    failures,
                ))
            };
        }
        // **The conclusion, not the receipt.**
        //
        // The asking turn can now build an answer out of this, which is the
        // whole point: a receipt gives a seat nothing to say, which is why one
        // live `desk_dm`-only turn handed the operator an empty bubble — it had
        // been told its request succeeded and had learned nothing.
        if let Some((peer, reply, recorded)) = answered {
            // The caveat rides the same result rather than a second call: the
            // seat is composing an answer out of this text, and "you have their
            // reply but the thread does not" changes whether it should promise
            // the exchange is on the record.
            let caveat = match recorded {
                true => "",
                false => {
                    "\n\n(This reply could not be written to your shared \
                          thread, so neither of you will read it back later.)"
                }
            };
            return ToolResult::success(format!("{peer} replied:\n\n{reply}{caveat}"));
        }
        if ran_without_answer {
            return ToolResult::success(format!(
                "Left for {}. They took their turn and said nothing back, so there is no \
                 answer to carry — do not wait on one.",
                left_for.join(", "),
            ));
        }
        let delivery = if woke_recipient {
            " TinyHiveMind routed one bounded recipient turn now; any additional recipients read it on their next turn."
        } else {
            " No recipient turn was started; they read it on their next turn."
        };
        ToolResult::success(format!("Left for {}.{delivery}", left_for.join(", ")))
    }

    async fn say(&self, chat_id: String, text: String, audience: Vec<String>) -> ToolResult {
        if text.trim().is_empty() {
            return ToolResult::error(
                "A message with no text reaches nobody. Say what you mean, or call no tool at all."
                    .to_string(),
            );
        }
        match self.append(chat_id, text, audience).await {
            Ok(seq) => {
                crate::runtime::delegation::mark_turn_spoke();
                ToolResult::success(format!("Said. Journaled at [{seq}]."))
            }
            Err(error) => ToolResult::error(error),
        }
    }

    async fn append(
        &self,
        chat_id: String,
        text: String,
        audience: Vec<String>,
    ) -> Result<EventSeq, String> {
        self.append_as(self.agent_id.clone(), chat_id, text, audience)
            .await
    }

    /// Journal a row authored by somebody other than this belt's owner.
    ///
    /// Only the inline peer turn uses it, and it needs to: the answer belongs
    /// to the peer who gave it, and a pair thread whose replies were all
    /// attributed to the asker would be a false record of the conversation.
    async fn append_as(
        &self,
        agent_id: String,
        chat_id: String,
        text: String,
        audience: Vec<String>,
    ) -> Result<EventSeq, String> {
        let event = CompanyEvent::AgentReply {
            chat_id,
            agent_id,
            text,
            steps: Vec::new(),
            task_id: None,
            // A spoken reply produces no files: the workspace-output collector
            // runs on the turn, not on this belt.
            outputs: Vec::new(),
            parent: None,
            // Drawn as chips and read by nobody's dispatcher — see the module
            // docs. Left empty here rather than resolved: this belt does not
            // hold a company record at call time, and a half-resolved mention
            // is worse than none.
            mentions: Vec::new(),
            mention_depth: 0,
            audience,
        };
        self.events
            .append(&self.company, event)
            .await
            .map_err(|error| format!("The message could not be journaled: {error}"))
    }

    /// Run the recipient's turn **now** and hand back what they said.
    ///
    /// This is what makes `desk_dm` a question rather than a receipt. Without
    /// it the tool returns `"Left for @peer."` — a synchronous success for work
    /// that completes asynchronously — so the asking turn's completion
    /// criterion is met while its own question is still outstanding, and the
    /// answer lands in the pair thread after that turn has closed. Nothing
    /// wakes the asker: in a live run the reply sat unread until an operator
    /// message a turn later happened to sweep it up through the session delta.
    ///
    /// `ChatTarget::channel`, not `deliberating`: this is an ordinary turn in a
    /// two-person channel, so the peer SHOULD read the pair thread — that is
    /// how it knows what was already asked and answered ("i already answered
    /// above" in the same run). The three reasons a room's turn must not be
    /// seeded are reasons about a room, and none of them is about a pair.
    ///
    /// `None` — no engine, the hop cap reached, a failed turn, an empty reply —
    /// falls back to the queue, which is the behaviour this replaces rather
    /// than a degraded one.
    async fn run_recipient_turn(
        &self,
        record: &crate::ports::types::CompanyRecord,
        peer: &str,
        chat_id: &str,
        text: &str,
    ) -> PeerTurn {
        let Some(runner) = crate::runtime::delegation::peer_runner() else {
            return PeerTurn::NotAttempted;
        };
        // The same bound the queue path applies, checked before anyone runs
        // rather than at push time: this path has no queue to refuse it, and a
        // pair that can each reach for the other is exactly the shape that
        // recurses.
        let max_hops = u32::from(
            record
                .manifest
                .tools
                .max_delegation_depth
                .unwrap_or(crate::company::DEFAULT_MAX_DELEGATION_DEPTH),
        );
        let hop = crate::runtime::delegation::turn_message_hop();
        if hop >= max_hops {
            return PeerTurn::NotAttempted;
        }
        let outcome = crate::runtime::delegation::with_turn_message_hop(
            hop + 1,
            runner.run(
                &self.company,
                peer,
                text,
                crate::runtime::delegation::ChatTarget::channel(Some(chat_id)),
            ),
        )
        .await;
        // Ran-and-failed, NOT not-attempted: the turn executed, so the peer has
        // had its turn and anything it did before erroring has happened.
        let Ok(outcome) = outcome else {
            return PeerTurn::Ran;
        };
        let reply = outcome.reply.trim().to_string();
        if reply.is_empty() {
            return PeerTurn::Ran;
        }
        // **The answer belongs in the pair thread, not only in the tool
        // result.**
        //
        // `refer` deliberately journals nothing for a single-seat crossing —
        // there the seat's answer reaches the room folded into the asker's own
        // row, so a second copy would be a duplicate. A pair thread is not a
        // room: it is the durable record of this relationship, the surface the
        // chip renders from, and what the peer reads back to know it has
        // already answered ("i already answered above", live). Returning the
        // reply without writing it left the journal holding a question with no
        // answer and a chip that could only ever show one line.
        let recorded = self
            .append_as(
                peer.to_string(),
                chat_id.to_string(),
                reply.clone(),
                Vec::new(),
            )
            .await
            .is_ok();
        if !recorded {
            tracing::warn!(
                company = %self.company,
                peer = %peer,
                "[speech] a peer's inline reply could not be journaled; the asker is told so"
            );
        }
        PeerTurn::Answered { reply, recorded }
    }

    fn stage_recipient_turn(
        &self,
        record: &crate::ports::types::CompanyRecord,
        peer: &str,
        chat_id: String,
        trigger: EventSeq,
        text: &str,
    ) -> bool {
        let Some(queue) = self.dispatch.as_ref() else {
            return false;
        };
        let members = crate::runtime::delegation_tools::tinyhivemind_roster(record);
        let people = Vec::new();
        let retired = Vec::new();
        let roster = tinyhivemind_core::roster::Roster::new(&members, &people, &retired);
        let input = tinyhivemind_core::dispatch::MentionDispatchInput {
            key: tinyhivemind_core::dispatch::DispatchKey {
                trigger_sequence: trigger.value(),
            },
            conversation: tinyhivemind_core::dispatch::DispatchConversation {
                desk_id: chat_id,
                thread_root: None,
            },
            author_id: self.agent_id.clone(),
            content: text.to_string(),
            mentions: vec![tinyhivemind_core::mention::Mention {
                target: tinyhivemind_core::mention::MentionTarget::Agent {
                    id: peer.to_string(),
                },
                text: format!("@{peer}"),
                offset: 0,
                quiet: false,
            }],
            hop: crate::runtime::delegation::turn_message_hop(),
        };
        let max_hops = u32::from(
            record
                .manifest
                .tools
                .max_delegation_depth
                .unwrap_or(crate::company::DEFAULT_MAX_DELEGATION_DEPTH),
        );
        let Ok(tinyhivemind_core::dispatch::MentionDispatchDecision::One { request }) =
            tinyhivemind_core::dispatch::mention_dispatch(
                tinyhivemind_core::dispatch::MentionDispatchPolicy {
                    enabled: true,
                    max_hops,
                },
                &input,
                &roster,
            )
        else {
            return false;
        };
        matches!(
            queue.push_within_cap(
                crate::harness::orchestrator::Delegation::ConversationDispatch {
                    source: request.source_id,
                    target: request.target_id,
                    message: request.content,
                    chat_id: request.conversation.desk_id,
                    trigger_sequence: request.key.trigger_sequence,
                    child_hop: request.child_hop,
                },
                crate::harness::orchestrator::MAX_DELEGATIONS_PER_TURN,
                usize::try_from(max_hops).unwrap_or(usize::MAX),
            ),
            crate::harness::orchestrator::Staged::Queued
        )
    }
}

/// Turns a crate-side refusal into the sentence handed back to the seat.
fn refusal(rejection: UtteranceRejection) -> ToolResult {
    ToolResult::error(rejection.to_string())
}

#[cfg(test)]
fn tool_result_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            oh::tools::traits::ToolContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `desk_post` — say one thing to the whole channel.
pub struct PostTool(pub SpeechContext);

#[async_trait]
impl Tool for PostTool {
    fn name(&self) -> &str {
        POST_TOOL
    }
    fn description(&self) -> &str {
        crate_description(POST_TOOL)
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "What you established, what you did not finish, and the one \
                                    teammate you need next — that teammate named first."
                },
                "desk": {
                    "type": "string",
                    "description": "Which channel to say it in. Omit for the one you are \
                                    answering in. Only a channel you sit on — to reach a desk \
                                    you are not a member of, refer the work across instead."
                }
            },
            "required": ["message"],
            "additionalProperties": false
        })
    }
    fn permission_level(&self) -> PermissionLevel {
        // Speaking is not an effect on the world outside this company, and the
        // approval bridge classifies by tool NAME rather than by this level
        // anyway (see `built_in::policy`). Declared `None` for the same reason
        // `request_approval` is: a turn that has to ask permission to answer
        // cannot answer.
        PermissionLevel::None
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let Some(channel) = self.0.channel() else {
            return Ok(ToolResult::error(
                "`desk_post` is only available while answering in a channel; this turn is not in \
                 one."
                    .to_string(),
            ));
        };
        let message = args.get("message").and_then(Value::as_str);
        let call = speech::interpret(
            bare(POST_TOOL),
            &CallArguments {
                message,
                to: &[],
                limit: None,
            },
        );
        match call {
            Ok(ToolCall::Speak(Utterance::Post { message })) => {
                match self
                    .0
                    .resolve_desk(args.get("desk").and_then(Value::as_str), &channel)
                    .await
                {
                    Ok(target) if target == channel => {
                        // The channel this turn is already in: the ordinary
                        // path, which collects rather than appends so the reply
                        // carries its steps, its live frame and its card.
                        Ok(self.0.post_to_channel(channel, message).await)
                    }
                    // Another channel this agent sits on. It cannot ride the
                    // turn's own reply — that reply belongs to the conversation
                    // the turn is in — so it is a row of its own.
                    Ok(target) => Ok(self.0.say(target, message, Vec::new()).await),
                    Err(refusal) => Ok(refusal),
                }
            }
            Ok(_) => Ok(ToolResult::error(
                "`desk_post` says one thing to the channel; it takes no other form.".to_string(),
            )),
            Err(rejection) => Ok(refusal(rejection)),
        }
    }
}

/// `desk_dm` — say one thing to named teammates instead of the whole channel.
pub struct DmTool(pub SpeechContext);

#[async_trait]
impl Tool for DmTool {
    fn name(&self) -> &str {
        DM_TOOL
    }
    fn description(&self) -> &str {
        crate_description(DM_TOOL)
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Teammate ids, without the @."
                },
                "message": { "type": "string" }
            },
            "required": ["to", "message"],
            "additionalProperties": false
        })
    }
    fn permission_level(&self) -> PermissionLevel {
        // Speaking is not an effect on the world outside this company, and the
        // approval bridge classifies by tool NAME rather than by this level
        // anyway (see `built_in::policy`). Declared `None` for the same reason
        // `request_approval` is: a turn that has to ask permission to answer
        // cannot answer.
        PermissionLevel::None
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        // The channel's *value* is no longer used — a DM goes to the recipient's
        // own channel, not this one — but its presence still gates the tool. A
        // turn with no conversation is not a turn anybody is talking in, and
        // letting it DM would let a dispatched card message the roster.
        let Some(_channel) = self.0.channel() else {
            return Ok(ToolResult::error(
                "`desk_dm` is only available while answering in a channel; this turn is not in one."
                    .to_string(),
            ));
        };
        let to: Vec<String> = args
            .get("to")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|id| id.trim().trim_start_matches('@').to_string())
                    .filter(|id| !id.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let message = args.get("message").and_then(Value::as_str);
        let call = speech::interpret(
            bare(DM_TOOL),
            &CallArguments {
                message,
                to: &to,
                limit: None,
            },
        );
        match call {
            Ok(ToolCall::Speak(Utterance::Dm { to, message })) => {
                // Self-addressing is refused by the crate in three places and is
                // refused here too, at the one point that holds the speaker's
                // own id: a message to yourself reaches nobody else, and a row
                // whose audience is only its author is a covert channel with a
                // journal entry.
                let mut peers: Vec<String> = to
                    .iter()
                    .filter(|id| *id != &self.0.agent_id)
                    .cloned()
                    .collect();
                if peers.is_empty() {
                    return Ok(ToolResult::error(
                        "`to` names only you; a message to yourself reaches nobody else. Use \
                         `desk_post` to say it to the channel."
                            .to_string(),
                    ));
                }
                // Every named teammate must resolve to exactly one roster id.
                // `dm` below hands `peer` straight to `openhuman_session_key`,
                // and `agent_channels` registers a recipient's session under
                // its *canonical* id — so a `to` entry that was typed as a
                // display name must be replaced with that id before it ever
                // reaches `dm`, or the row is journaled under a session
                // nothing reads. `Unknown` names nobody; `Ambiguous` names more
                // than one teammate and must not silently pick either.
                let Ok(Some(record)) = self.0.store.load(&self.0.company).await else {
                    // tinysweeper: the sibling `resolve_desk` treats an
                    // unreadable roster as fatal rather than silently skipping
                    // its own membership check (same rationale, quoted there:
                    // falling through would risk exactly the row-nobody-reads
                    // outcome this check exists to prevent). A store error or
                    // `Ok(None)` here must refuse for the same reason, not
                    // fall through to `dm` with unresolved names.
                    return Ok(ToolResult::error(
                        "The roster could not be read, so I cannot tell who `to` names. Try \
                         again."
                            .to_string(),
                    ));
                };
                let mut canonical: Vec<String> = Vec::with_capacity(peers.len());
                let mut unknown: Vec<String> = Vec::new();
                let mut ambiguous: Vec<String> = Vec::new();
                for id in &peers {
                    match record.resolve_teammate_key(id) {
                        crate::ports::types::TeammateResolution::Agent(canonical_id) => {
                            canonical.push(canonical_id)
                        }
                        crate::ports::types::TeammateResolution::Unknown => {
                            unknown.push(id.clone())
                        }
                        crate::ports::types::TeammateResolution::Ambiguous(_) => {
                            ambiguous.push(id.clone())
                        }
                    }
                }
                if !unknown.is_empty() {
                    let names = unknown
                        .iter()
                        .map(|id| format!("@{id}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Ok(ToolResult::error(format!(
                        "Nobody on this company is called {names}. Check the roster and try \
                         again."
                    )));
                }
                if !ambiguous.is_empty() {
                    let names = ambiguous
                        .iter()
                        .map(|id| format!("@{id}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Ok(ToolResult::error(format!(
                        "More than one teammate answers to {names}. Use their id instead of a \
                         display name."
                    )));
                }
                peers = canonical;
                // Codex P2: canonicalization can turn a survivor of the
                // filter above back into the caller's own id — an overlay
                // teammate addressing itself by its unique DISPLAY NAME
                // (`to: ["Nova"]`, agent id `nova`) passes the raw-id
                // filter, since `"Nova" != "nova"`, and only becomes a
                // self-reference once resolved. Re-apply the same refusal
                // now that every surviving entry is a canonical id.
                peers.retain(|id| id != &self.0.agent_id);
                // Codex P2: two `to` entries can canonicalize to the same id —
                // a repeated raw id, or one spelled once by id and once by
                // display name — and without this, `dm` below loops over the
                // vector and journals the identical message once per surviving
                // entry, doubling it in the recipient's channel. Deduplicated
                // after canonicalization (not before, where the entries are
                // not yet comparable) and order-preserving, so the "Left for"
                // sentence still lists each recipient in the order they were
                // named.
                let mut seen_peers = std::collections::HashSet::with_capacity(peers.len());
                peers.retain(|id| seen_peers.insert(id.clone()));
                if peers.is_empty() {
                    return Ok(ToolResult::error(
                        "`to` names only you; a message to yourself reaches nobody else. Use \
                         `desk_post` to say it to the channel."
                            .to_string(),
                    ));
                }
                Ok(self.0.dm(peers, message, &record).await)
            }
            Ok(_) => Ok(ToolResult::error(
                "`desk_dm` says one thing to named teammates; it takes no other form.".to_string(),
            )),
            Err(rejection) => Ok(refusal(rejection)),
        }
    }
}

/// `desk_close` — say one last thing and report the work finished.
pub struct CloseTool(pub SpeechContext);

#[async_trait]
impl Tool for CloseTool {
    fn name(&self) -> &str {
        CLOSE_TOOL
    }
    fn description(&self) -> &str {
        crate_description(CLOSE_TOOL)
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The result, and why nothing is left open."
                }
            },
            "required": ["message"],
            "additionalProperties": false
        })
    }
    fn permission_level(&self) -> PermissionLevel {
        // Speaking is not an effect on the world outside this company, and the
        // approval bridge classifies by tool NAME rather than by this level
        // anyway (see `built_in::policy`). Declared `None` for the same reason
        // `request_approval` is: a turn that has to ask permission to answer
        // cannot answer.
        PermissionLevel::None
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let Some(channel) = self.0.channel() else {
            return Ok(ToolResult::error(
                "`desk_close` is only available while answering in a channel; this turn is not in \
                 one."
                    .to_string(),
            ));
        };
        let message = args.get("message").and_then(Value::as_str);
        let call = speech::interpret(
            bare(CLOSE_TOOL),
            &CallArguments {
                message,
                to: &[],
                limit: None,
            },
        );
        match call {
            // Renamed upstream from `Close` to `CompleteEpisode`: the seat is
            // "reporting, not ending the desk", and the new spelling says so.
            // The wire is unchanged — the variant keeps `alias = "close"` — so
            // `desk_close` still decodes and every stored row still reads.
            Ok(ToolCall::Speak(Utterance::CompleteEpisode { message })) => {
                Ok(self.0.post_to_channel(channel, message).await)
            }
            Ok(_) => Ok(ToolResult::error(
                "`desk_close` says one last thing and reports the work finished; it takes no other \
                 form."
                    .to_string(),
            )),
            Err(rejection) => Ok(refusal(rejection)),
        }
    }
}

/// `desk_read` — read further back in this channel than the turn was handed.
pub struct ReadTool(pub SpeechContext);

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        READ_TOOL
    }
    fn description(&self) -> &str {
        crate_description(READ_TOOL)
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": speech::READ_MAX,
                    "description": format!(
                        "How many recent messages to return. Default {}, max {}.",
                        speech::READ_DEFAULT, speech::READ_MAX
                    )
                }
            },
            "required": [],
            "additionalProperties": false
        })
    }
    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }
    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let Some(channel) = self.0.channel() else {
            return Ok(ToolResult::error(
                "`desk_read` is only available while answering in a channel; this turn is not in \
                 one."
                    .to_string(),
            ));
        };
        let call = speech::interpret(
            bare(READ_TOOL),
            &CallArguments {
                message: None,
                to: &[],
                limit: args.get("limit").and_then(Value::as_u64),
            },
        );
        // The clamp is the crate's, so two hosts cannot disagree about it and a
        // seat asking for the whole transcript gets a bounded answer rather
        // than its own context window back.
        let limit = match call {
            Ok(ToolCall::Read { limit }) => limit,
            Ok(_) => speech::READ_DEFAULT,
            Err(rejection) => return Ok(refusal(rejection)),
        };

        // Codex P1: `resolve_seed_desk` is an ALIAS resolver — built for a
        // human-typed or short-form channel key, and it will happily widen a
        // bare DM key that collides with a desk's display name (agent
        // `support`, desk `{ id: "triage", name: "support" }`) to that OTHER
        // desk's canonical id. `channel` here is not typed input: it is the
        // ambient conversation this turn is already bound to, the exact
        // string `post_to_channel`/`say` use verbatim as `chat_id` for this
        // turn's own posts (see `post_to_channel` above). Scanning under that
        // same literal string — not a resolved alias — is what keeps
        // `desk_read` scoped to "this channel", channel it was actually
        // authorized for, precisely as the tool's contract promises.
        let (desk_id, desk_name) = (channel.clone(), channel.clone());

        let mut lines: Vec<String> = Vec::new();
        let mut cursor: Option<EventSeq> = None;
        let mut scanned = 0usize;
        // Bounded for the reason every read in this area is: a read is a recent
        // window, and hunting the whole company journal for one is a defect
        // rather than thoroughness.
        const SEARCH_PAGE: usize = 256;
        const SEARCH_BUDGET: usize = 2048;
        while lines.len() < limit && scanned < SEARCH_BUDGET {
            let page = match self
                .0
                .events
                .read_before(&self.0.company, cursor, SEARCH_PAGE)
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    return Ok(ToolResult::error(format!(
                        "This channel could not be read: {error}"
                    )));
                }
            };
            if page.is_empty() {
                break;
            }
            scanned += page.len();
            cursor = page.last().map(|event| event.seq);
            for stored in page {
                if lines.len() >= limit {
                    break;
                }
                if !crate::server::chat_history::owns(&desk_id, &desk_name, &stored.event) {
                    continue;
                }
                // The same audience narrowing the session applies: a private
                // exchange this agent is not party to is not readable by asking
                // for more of the channel.
                let line = match &stored.event {
                    CompanyEvent::AgentReply {
                        agent_id,
                        text,
                        audience,
                        ..
                    } => {
                        if !audience.is_empty()
                            && agent_id != &self.0.agent_id
                            && !audience.iter().any(|member| member == &self.0.agent_id)
                        {
                            continue;
                        }
                        format!("[{}] {agent_id}: {text}", stored.seq)
                    }
                    CompanyEvent::OperatorMessage { text, .. } => {
                        format!("[{}] operator: {text}", stored.seq)
                    }
                    _ => continue,
                };
                lines.push(line);
            }
        }
        lines.reverse();
        if lines.is_empty() {
            return Ok(ToolResult::success(
                "Nothing has been said in this channel yet.".to_string(),
            ));
        }
        // A read that was cut says so — `query_company` is the cautionary case
        // this repo already names: a partial list that reads as complete
        // becomes "we have no record of that". tinysweeper: `lines.len() <
        // limit` alone missed the OTHER way a read is cut short — the scan
        // budget running out first. A matching message can exist just past
        // `SEARCH_BUDGET`; without this, that reply reads as "nothing more to
        // see" when the honest answer is "did not look far enough".
        let truncated = lines.len() >= limit || scanned >= SEARCH_BUDGET;
        let mut body = lines.join("\n");
        if truncated {
            body.push_str(&format!(
                "\n\n(Showing the most recent {limit}. Older messages are not in this reply.)"
            ));
        }
        Ok(ToolResult::success(body))
    }
}

/// The prompt brief for a company that speaks by calling a tool.
///
/// # A tool granted, unmentioned and never called
///
/// That is the failure this repo has already named twice — `shell` wired since
/// Cell A and described in no brief, `ledger` granted and never used — and
/// speech is the worst case of it, because the fallback is *silent*. An agent
/// that never learns about `desk_post` simply answers in text, the reply path
/// journals it, and nothing anywhere reports that the feature did nothing. It
/// was observed doing exactly that on the first live run of this change.
///
/// The tool descriptions cannot carry this on their own. They say "this is the
/// only way to speak", but they say it from inside a list of forty tools, and a
/// system prompt that never mentions speaking at all outranks them.
pub fn speech_brief() -> String {
    format!(
        "\n\n## Saying things\n\
         This company talks by calling a tool. Text you write outside a tool call is your own \
         thinking and reaches nobody — it is not sent, and nobody sees it.\n\
         - `{POST_TOOL}` — say one thing to a channel. Call it exactly once, at the end of your \
         turn. This is how you answer. It says it in the channel you are answering in unless you \
         pass `desk`, which may name any channel you sit on.\n\
         - `{DM_TOOL}` — leave one thing for named teammates instead of the whole channel. The \
         message is durable for every recipient; TinyHiveMind may wake exactly one of them now, \
         bounded by the hop limit, and additional recipients read it on their next turn. A DM \
         conversation opens no task card.\n\
         - `{CLOSE_TOOL}` — say one last thing AND report the work finished. Only when it \
         genuinely is: a result somebody still has to check is not finished.\n\
         - `{READ_TOOL}` — read further back in this channel than you were handed.\n"
    )
}

/// Every speech tool, built for one agent.
pub fn speech_belt(context: SpeechContext) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(PostTool(context.clone())),
        // **`desk_dm` is withheld from the belt.**
        //
        // Not because it is redundant — it works, and it is the only tool that
        // returns a teammate's answer inside the asking turn. Because it is
        // *attractive*: given a private channel a seat takes it, and the desk
        // goes dark. Observed on a live run — asked to draft an accessibility
        // section and have it verified, a seat DM'd two teammates, got its
        // answer back, and wrote nothing to the desk at all. The operator who
        // asked saw silence while the work happened out of view.
        //
        // The same prompt with this line removed produced a desk-visible
        // `!broadcast` carrying the whole finding, which a second seat then
        // picked up and completed. That is the behaviour a room is for.
        //
        // `DmTool` itself is kept: it is still the right tool on a direct
        // conversation, and re-registering it here is a one-line change if the
        // room turns out to need private pairing after all.
        // Box::new(DmTool(context.clone())),
        Box::new(CloseTool(context.clone())),
        Box::new(ReadTool(context)),
    ]
}

#[cfg(test)]
#[path = "speech_tools_test_fixtures.rs"]
mod speech_tools_test_fixtures;
/// `desk_read` coverage: scan, channel/audience filtering, and truncation.
/// See `tests_dm` above for why this is split out.
#[cfg(test)]
#[path = "speech_tools_desk_read_tests.rs"]
mod tests_desk_read;
/// `desk_dm` / `desk_post` coverage: resolution, journaling, and multi-recipient
/// delivery. Split from `desk_read` coverage (below) because the combined
/// inline module exceeded the 750-line file limit.
#[cfg(test)]
#[path = "speech_tools_dm_tests.rs"]
mod tests_dm;
