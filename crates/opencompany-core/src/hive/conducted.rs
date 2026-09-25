//! One completion episode, run on `tinyhivemind`'s own loop.
//!
//! This is the whole of the episode path. It builds the door the operator's
//! message opens, seats each teammate as a session host of its own, and
//! calls [`run_episode`]. Everything between -- who speaks next, what a
//! committed row means, the private conversations a seat opens, the nudges,
//! the walls, the completion fold, parking on an approval, and the
//! checkpoint a restart resumes from -- belongs to the library.
//!
//! What stays here is what the library cannot know: the journal rows are
//! this company's, the seats are its teammates, and a turn still takes the
//! teammate's lock and writes its brackets. Those reach the loop through
//! [`DeskHost`].

use std::collections::HashMap;
use std::sync::Arc;

use tinyhivemind::SESSION_WINDOW;
use tinyhivemind_driver::{
    BoundHive, BroadcastRouting, CompletionDriver, ConductPolicy, ConductorState, Door,
};
use tinyhivemind_embed::Router;
use tinyhivemind_openhuman::{HostedRunner, Report, SeatRunner, resume_episode, run_episode};
use tinyhivemind_tools::EpisodeTools;

use crate::error::{OpenCompanyError, Result};
use crate::harness::built_in::{HarnessDeps, HarnessPool};
use crate::hive::episode_store;
use crate::hive::graph::DeskHive;
use crate::hive::host::{DeskHost, EpisodeSeatParking, SeatParking};
use crate::hive::routing::{EffectiveRouting, RoutingPlanDto, desk_routing, router_of};
use crate::ports::events::EventLog;
use crate::ports::types::CompanyEvent;
use crate::ports::types::{CompanyRecord, EventSeq, Mention, StoredEvent};

/// Everything this company brings to one episode.
///
/// A struct rather than a dozen positional arguments, most of them the same
/// shape.
pub struct Episode<'a> {
    /// The company as it effectively stands, for seating a teammate.
    pub record: Arc<CompanyRecord>,
    /// What every one of its agents is built from.
    pub deps: Arc<HarnessDeps>,
    /// The pool its teammates live in, for the lock a turn holds.
    pub pool: Arc<HarnessPool>,
    /// The company's durable journal.
    pub events: Arc<dyn EventLog>,
    /// The desk this episode runs on.
    pub desk: &'a DeskHive,
    /// The routing this desk resolved: round width, and the policy a
    /// handoff is placed under.
    pub routing: &'a EffectiveRouting,
    /// The semantic router, when a credential resolved one. `None` places a
    /// handoff by lead and mention instead of by meaning.
    pub router: Option<&'a (dyn Router + 'a)>,
    /// This episode's id, as the console names it.
    pub episode_id: String,
    /// The thread the episode's rows are parented to.
    pub thread_root: Option<EventSeq>,
    /// The operator's row: where the episode opens.
    pub opened_at: EventSeq,
    /// The seats the opening routing plan named. Empty starts the desk's
    /// first member.
    pub starters: Vec<String>,
    /// What this company does with the approvals a turn raised.
    pub parking: Option<Arc<dyn SeatParking>>,
    /// How a desk reply's mentions are resolved and notified (#2441).
    pub mentions: Option<crate::runtime::mention_seam::MentionSeam>,
}

/// Run one completion episode to quiescence.
///
/// # Errors
///
/// [`OpenCompanyError::Harness`] for a desk that seats nobody, a seat that
/// cannot be built, a journal that refuses a row, an episode that stalls or
/// runs past its wall, or one parked on the operator with nobody released.
pub async fn run(episode: Episode<'_>) -> Result<Report> {
    conduct(episode, None).await
}

/// Carry on an episode from the checkpoint `snapshot`, with `rows` the
/// journal rows it has written so far and `revision` the wave the checkpoint
/// was taken at.
///
/// # Errors
///
/// Whatever [`run`] errors on, plus a snapshot this desk cannot resume.
pub async fn resume(
    episode: Episode<'_>,
    snapshot: ConductorState,
    rows: &[StoredEvent],
    revision: u64,
) -> Result<Report> {
    conduct(episode, Some((snapshot, rows, revision))).await
}

async fn conduct(
    episode: Episode<'_>,
    resumed: Option<(ConductorState, &[StoredEvent], u64)>,
) -> Result<Report> {
    let members: Vec<String> = episode.desk.hive.members().map(str::to_owned).collect();
    if members.is_empty() {
        return Err(OpenCompanyError::Harness(format!(
            "desk `{}` seats nobody",
            episode.desk.desk_id
        )));
    }
    let starters = if episode.starters.is_empty() {
        vec![members[0].clone()]
    } else {
        episode.starters.clone()
    };

    let host = Arc::new({
        let mut host = DeskHost::new(
            episode.record.id.clone(),
            episode.desk.desk_id.clone(),
            episode.desk.desk_name.clone(),
            Arc::clone(&episode.events),
            members.clone(),
        )
        .in_thread(episode.thread_root)
        .episode(episode.episode_id.clone())
        .seating(Arc::clone(&episode.record), Arc::clone(&episode.deps))
        .locking(Arc::clone(&episode.pool));
        if let Some(parking) = episode.parking.clone() {
            host = host.parking(parking);
        }
        if let Some(mentions) = episode.mentions.clone() {
            host = host.resolving_mentions(mentions);
        }
        // The pool handle each seat runs its turns on.
        //
        // Resolved here because `EpisodeHost::build_seat` is sync and the pool
        // is not, and this is the last place that can await. A member the pool
        // does not know is left unseated, and `build_seat` says so rather than
        // inventing an agent for it.
        for member in &members {
            if let Some(agent) = episode.pool.agent(&episode.record.id, member).await {
                host = host.seat_on(member, agent);
            }
        }
        host
    });

    let releases = host.seat_releases();
    let _running = releases.as_ref().map(|releases| {
        releases.start(&episode.episode_id);
        RunningEpisode {
            releases: releases.clone(),
            episode_id: episode.episode_id.clone(),
        }
    });

    // Each seat is built once, here, and torn down with the episode: its
    // belt carries the episode's tools, which are bound to this seat of this
    // episode and to nothing else.
    let runner = HostedRunner::seat(
        Arc::clone(&host),
        Arc::new(EpisodeTools::new(members.iter().cloned())),
        &members,
        &episode.desk.desk_id,
        &episode.desk.desk_name,
        SESSION_WINDOW,
    )
    .map_err(|error| OpenCompanyError::Harness(error.to_string()))?;

    // The desk's graph is the same one the pool's hive carries; only the
    // bindings differ, because these seats are this episode's sessions
    // rather than the pool's long-lived handles.
    let hive = BoundHive::new(episode.desk.hive.graph().clone(), runner.bindings())
        .map_err(|error| OpenCompanyError::Harness(error.to_string()))?;
    let driver = CompletionDriver::new(&hive, episode.routing.round_width)
        .map_err(|error| OpenCompanyError::Harness(error.to_string()))?;
    let route_policy = episode.routing.policy();

    let routing = BroadcastRouting {
        // Threaded through deliberately: without it a handoff is still
        // placed, but by lead and mention rather than by meaning, and
        // nothing anywhere reports the difference.
        primary: episode.router,
        reasoning: None,
        policy: &route_policy,
        roster_version: episode.desk.roster_version,
        thread_context: &[],
    };
    let outcome = match resumed {
        None => {
            run_episode(
                host.as_ref(),
                &runner,
                &driver,
                routing,
                ConductPolicy::default(),
                Door {
                    chat: episode.desk.desk_id.clone(),
                    desk_name: episode.desk.desk_name.clone(),
                    members,
                    starters,
                    opened_at: tinyhivemind::Sequence(episode.opened_at.value()),
                },
            )
            .await
        }
        Some((snapshot, rows, revision)) => {
            host.recall(rows, revision);
            resume_episode(
                host.as_ref(),
                &runner,
                &driver,
                routing,
                ConductPolicy::default(),
                snapshot,
            )
            .await
        }
    };
    host.flush_deliveries();
    if let Err(tinyhivemind_openhuman::Error::Conduct(tinyhivemind_driver::Error::Parked {
        seats,
    })) = &outcome
    {
        paused(&episode, seats).await;
    }
    outcome.map_err(|error| OpenCompanyError::Harness(error.to_string()))
}

/// Says on the desk that an episode stopped with seats still waiting on the
/// operator, so the room does not simply go quiet.
async fn paused(episode: &Episode<'_>, seats: &[String]) {
    tracing::error!(
        company = %episode.record.id,
        desk = %episode.desk.desk_id,
        episode = %episode.episode_id,
        ?seats,
        "[hive] an episode stopped with seats waiting on the operator and nothing to release them"
    );
    let row = CompanyEvent::AgentReply {
        chat_id: episode.desk.desk_id.clone(),
        agent_id: crate::ports::SYSTEM_AUTHOR.to_owned(),
        text: "This conversation stopped while a teammate was waiting on a decision that could \
               not be handed back to it. Answer the pending approval and ask again to pick it \
               back up."
            .to_owned(),
        steps: Vec::new(),
        outputs: Vec::new(),
        task_id: None,
        episode: None,
        parent: episode.thread_root,
        mentions: Vec::new(),
        mention_depth: 0,
        audience: Vec::new(),
    };
    if let Err(error) = episode.events.append(&episode.record.id, row).await {
        tracing::warn!(%error, "[hive] could not record why an episode stopped");
    }
}

/// Marks an episode as running for as long as it is held.
struct RunningEpisode {
    releases: crate::runtime::episode_resume::EpisodeReleases,
    episode_id: String,
}

impl Drop for RunningEpisode {
    fn drop(&mut self) {
        self.releases.finish(&self.episode_id);
    }
}

/// What opened an episode: the operator's row and what it said.
#[derive(Clone, Debug)]
pub struct Trigger {
    /// The row the message was journaled at.
    pub seq: EventSeq,
    /// What it said.
    pub text: String,
    /// The thread it was sent in, when it was sent in one.
    pub parent: Option<EventSeq>,
    /// Who it named.
    pub mentions: Vec<Mention>,
}

/// What one episode came to, in this host's words.
#[derive(Clone, Debug)]
pub struct EpisodeReport {
    /// Which episode this is the report for.
    ///
    /// Carried so the caller can drain exactly this episode's staged
    /// takeovers. The id is minted inside `run_desk_message`, so without it
    /// `spawn_episode` had no key and drained the company's whole queue.
    pub episode_id: String,
    /// Seat turns run.
    pub turns: u64,
    /// Waves proposed.
    pub waves: u64,
    /// Conversations concluded between seats.
    pub conversations: usize,
    /// Seats that reported their work finished.
    pub settled: usize,
}

impl EpisodeReport {
    /// The driver's report, named with the episode it came from.
    #[must_use]
    pub fn of(episode_id: impl Into<String>, report: Report) -> Self {
        Self {
            episode_id: episode_id.into(),
            turns: report.turns,
            waves: report.waves,
            conversations: report.conversations,
            settled: report.settled,
        }
    }
}

/// One company's desks, and what it takes to run an episode on any of them.
pub struct HiveDispatcher {
    /// The company as it effectively stands.
    pub record: Arc<CompanyRecord>,
    /// Its durable journal.
    pub events: Arc<dyn EventLog>,
    /// Its bound desks, by id.
    pub hives: HashMap<String, Arc<DeskHive>>,
    /// The semantic router, when a credential resolved one.
    pub router: Option<Arc<dyn Router>>,
    /// What its agents are built from.
    pub deps: Arc<HarnessDeps>,
    /// The pool they live in, for the lock a turn holds.
    pub pool: Arc<HarnessPool>,
    /// How a desk reply's mentions are resolved and notified (#2441).
    ///
    /// `None` journals a reply exactly as a host without one would: the
    /// mentions are simply not resolved, which is what every construction
    /// that has no user directory to resolve against should get.
    pub mentions: Option<crate::runtime::mention_seam::MentionSeam>,
}

/// Who answers an operator DM, when the conversation is one.
///
/// **A DM's responder is not a routing question.** The hive holds the roster
/// so `ask` has somewhere to land, but a message in `dm:pm` is for the PM.
/// Routed instead it would be answered by whoever the ranker liked -- and with
/// a TinyHumans key present that ranker is a model, so the wrong teammate
/// answering your DM would be a decision nobody made and nothing recorded.
///
/// An explicit `@mention` still wins: naming someone in your own DM is an
/// instruction, not an ambiguity.
///
/// `None` for a desk, where routing is exactly the right question to ask.
pub(crate) fn dm_opening(
    desk_id: &str,
    lead: &str,
    explicit: Option<&str>,
) -> Option<(Vec<String>, RoutingPlanDto)> {
    if !desk_id.starts_with(crate::runtime::assignee::DM_PREFIX) {
        return None;
    }
    let primary = explicit.unwrap_or(lead).to_owned();
    Some((
        vec![primary.clone()],
        RoutingPlanDto::One {
            primary_id: primary,
        },
    ))
}

impl HiveDispatcher {
    /// The hive bound to `desk_id`, when that desk runs one.
    #[must_use]
    pub fn hive(&self, desk_id: &str) -> Option<Arc<DeskHive>> {
        self.hives.get(desk_id).cloned()
    }

    /// Open one episode for an operator message and run it to quiescence.
    ///
    /// # Errors
    ///
    /// A desk that runs no hive, and whatever stops the episode.
    pub async fn run_desk_message(&self, desk_id: &str, trigger: Trigger) -> Result<EpisodeReport> {
        self.run_desk_message_as(desk_id, trigger, uuid::Uuid::new_v4().simple().to_string())
            .await
    }

    /// The same, under an id the caller minted.
    ///
    /// Split out because a caller that must act on the episode *after* it
    /// ends needs its id on the failing path too, and an id minted inside is
    /// lost with the error. `spawn_episode` drains this episode's staged
    /// takeovers either way: a claim whose episode failed is still a claim
    /// the operator has been told about in a durable row, and leaving it in a
    /// queue that lives as long as the runtime strands it for good.
    ///
    /// # Errors
    ///
    /// Whatever stops the episode opening or running.
    pub async fn run_desk_message_as(
        &self,
        desk_id: &str,
        trigger: Trigger,
        episode_id: String,
    ) -> Result<EpisodeReport> {
        let desk = self.hive(desk_id).ok_or_else(|| {
            OpenCompanyError::InvalidRequest(format!("desk `{desk_id}` runs no hive"))
        })?;
        let thread_root = trigger.parent.unwrap_or(trigger.seq);
        let routing = desk_routing(&self.record, desk_id);
        let (starters, plan_dto) = self.opening(&desk, &routing, &trigger, thread_root).await?;
        self.events
            .append(
                &self.record.id,
                CompanyEvent::EpisodeOpened {
                    chat_id: desk.desk_id.clone(),
                    episode_id: episode_id.clone(),
                    opened_by_seq: trigger.seq.value(),
                    parent: Some(thread_root),
                    participants: starters.clone(),
                    plan: plan_dto.clone(),
                    hop: 0,
                },
            )
            .await?;
        let report = run(Episode {
            record: Arc::clone(&self.record),
            deps: Arc::clone(&self.deps),
            pool: Arc::clone(&self.pool),
            events: Arc::clone(&self.events),
            desk: &desk,
            routing: &routing,
            // No router in a DM. It governs `broadcast`, and handing work
            // to another seat inside a one-to-one conversation is a hand-off
            // -- a different conversation -- not something a ranker should
            // pick a recipient for. Without one, a broadcast falls back to
            // lead-and-mention, which is what a DM means anyway.
            router: if desk_id.starts_with(crate::runtime::assignee::DM_PREFIX) {
                None
            } else {
                self.router.as_deref()
            },
            episode_id: episode_id.clone(),
            thread_root: Some(thread_root),
            opened_at: trigger.seq,
            starters,
            parking: self.seat_parking(&desk.desk_id, Some(thread_root), &episode_id),
            mentions: self.mentions.clone(),
        })
        .await?;
        self.complete(&desk.desk_id, &episode_id, &report).await?;
        Ok(EpisodeReport::of(episode_id, report))
    }

    /// Carry on a parked episode from its last checkpoint, once an operator
    /// decision has come in for it and no running episode took it. `None`
    /// when it is already running here.
    ///
    /// # Errors
    ///
    /// An episode with no checkpoint, on a desk that runs no hive, and
    /// whatever stops the resumed episode.
    pub async fn resume_desk_message(&self, episode_id: &str) -> Result<Option<EpisodeReport>> {
        let releases = self.deps.approval_requests.grants().episode_releases();
        if !releases.start(episode_id) {
            return Ok(None);
        }
        let _running = RunningEpisode {
            releases,
            episode_id: episode_id.to_owned(),
        };
        let saved = episode_store::latest_state(self.events.as_ref(), &self.record.id, episode_id)
            .await?
            .ok_or_else(|| {
                OpenCompanyError::NotFound(format!("episode `{episode_id}` has no checkpoint"))
            })?;
        let desk = self.hive(&saved.desk).ok_or_else(|| {
            OpenCompanyError::InvalidRequest(format!("desk `{}` runs no hive", saved.desk))
        })?;
        let snapshot: ConductorState = serde_json::from_value(saved.state.clone())
            .map_err(|error| OpenCompanyError::Harness(format!("episode checkpoint: {error}")))?;
        let rows =
            episode_store::episode_rows(self.events.as_ref(), &self.record.id, episode_id).await?;
        let routing = desk_routing(&self.record, &desk.desk_id);
        tracing::info!(
            desk = %desk.desk_id,
            episode = %episode_id,
            revision = saved.revision,
            "[hive] resuming a parked episode from its checkpoint"
        );
        let report = resume(
            Episode {
                record: Arc::clone(&self.record),
                deps: Arc::clone(&self.deps),
                pool: Arc::clone(&self.pool),
                events: Arc::clone(&self.events),
                desk: &desk,
                routing: &routing,
                router: self.router.as_deref(),
                episode_id: episode_id.to_owned(),
                thread_root: saved.thread_root,
                opened_at: saved.thread_root.unwrap_or(EventSeq::new(0)),
                starters: Vec::new(),
                parking: self.seat_parking(&desk.desk_id, saved.thread_root, episode_id),
                mentions: self.mentions.clone(),
            },
            snapshot,
            &rows,
            saved.revision,
        )
        .await?;
        self.complete(&desk.desk_id, episode_id, &report).await?;
        Ok(Some(EpisodeReport::of(episode_id, report)))
    }

    /// Journals an episode's closing row.
    async fn complete(&self, desk_id: &str, episode_id: &str, report: &Report) -> Result<()> {
        // The episode's closing row. `run_episode` returns only once every
        // seat has recorded its part -- a wall, a stall or a fold it could
        // not explain comes back as an error instead, and is journaled by
        // whoever handles it -- so a return here *is* `complete_episode`.
        //
        // Without this the console sees an episode that opened and never
        // closed: `EpisodeOpened` was written above, `episode_store` folds
        // the pair, and the operator's list would hold it open forever.
        self.events
            .append(
                &self.record.id,
                CompanyEvent::EpisodeCompleted {
                    chat_id: desk_id.to_owned(),
                    episode_id: episode_id.to_owned(),
                    revision: report.waves,
                    // The library reports what happened, not who spoke last:
                    // every seat completed, so no one seat closed it.
                    completed_by: None,
                    rounds: u32::try_from(report.waves).unwrap_or(u32::MAX),
                    reason: crate::ports::types::EpisodeReason::CompleteEpisode,
                    summary_seq: None,
                },
            )
            .await?;
        tracing::info!(
            desk = %desk_id,
            episode = %episode_id,
            turns = report.turns,
            waves = report.waves,
            "[hive] episode finished"
        );
        Ok(())
    }

    /// Parking for the seats of one episode, through the runtime's parker.
    fn seat_parking(
        &self,
        desk_id: &str,
        thread_root: Option<EventSeq>,
        episode_id: &str,
    ) -> Option<Arc<dyn SeatParking>> {
        let parker = self.deps.approval_parker.clone()?;
        Some(Arc::new(EpisodeSeatParking::new(
            parker,
            self.record.id.clone(),
            desk_id.to_owned(),
            thread_root,
            episode_id.to_owned(),
        )))
    }

    /// Who the opening routing plan starts, or the desk in order when no
    /// router answered.
    async fn opening(
        &self,
        desk: &DeskHive,
        routing: &EffectiveRouting,
        trigger: &Trigger,
        thread_root: EventSeq,
    ) -> Result<(Vec<String>, RoutingPlanDto)> {
        let lead = desk.lead().ok_or_else(|| {
            OpenCompanyError::Harness(format!("desk `{}` has no seats", desk.desk_id))
        })?;
        let explicit = trigger.mentions.iter().find_map(|mention| {
            desk.hive
                .members()
                .find(|member| match &mention.target {
                    crate::ports::types::MentionTarget::Agent { id } => *member == id,
                    _ => false,
                })
                .map(str::to_owned)
        });
        if let Some(pinned) = dm_opening(&desk.desk_id, &lead, explicit.as_deref()) {
            return Ok(pinned);
        }
        let request = desk.hive.desk_request(
            trigger.text.clone(),
            Vec::new(),
            Some(tinyhivemind::Sequence(thread_root.value())),
            desk.roster_version,
            routing.policy(),
        );
        let plan = desk
            .hive
            .route_desk(
                self.router.as_deref(),
                None,
                &request,
                explicit.as_deref(),
                &lead,
            )
            .await
            .map_err(|error| OpenCompanyError::Harness(error.to_string()))?;
        tracing::debug!(desk = %desk.desk_id, router = ?router_of(&plan), "[hive] opening routed");
        let dto = RoutingPlanDto::from(&plan);
        let mut starters = dto.agent_ids();
        if starters.is_empty() {
            // A clarification is a routing answer the room cannot act on:
            // the lead answers, and asks if it must.
            starters.push(lead);
        }
        // Deliberately *not* padded to the desk's `round_width`. That width
        // bounds how many recipients a broadcast may be placed to and how
        // many queued handoffs a seat may hold; it does not size a wave. A
        // wave is whoever is due, and who opens is the routing plan's answer,
        // not a number this host applies to it.
        Ok((starters, dto))
    }
}
