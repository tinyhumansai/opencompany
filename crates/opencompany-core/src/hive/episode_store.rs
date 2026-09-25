//! The journal as the episode store: the fold behind `GET {scope}/episodes`
//! and the driver checkpoint a resume reads back.
//!
//! There is no second store. An episode is its `EpisodeOpened` and
//! `EpisodeCompleted` rows, the `TurnStarted` rows between them -- which
//! carry the wave each turn ran in, and are what an open episode's progress
//! is read from -- and the `AgentReply` rows they bracket. (A journal
//! written before the loop moved to the library carries `RoundStarted` /
//! `RoundCommitted` rows instead, and folds the same.) The driver's own
//! resumable state is
//! one more row (`EpisodeStateSaved`) rather than a file beside the journal —
//! so a host that died after committing a round finds, on the next boot, both
//! the state it had reached and the rows it committed since, and can replay
//! the latter through `apply_committed` (an exact replay is a no-op).
//!
//! Every read here walks the journal **backwards** in bounded chunks: the
//! episodes an operator asks about are the recent ones, and a forward scan
//! from sequence zero would grow with the company's whole history.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use tinyhivemind::SharingState;

use crate::error::Result;
use crate::hive::routing::RoutingPlanDto;
use crate::ports::events::EventLog;
use crate::ports::types::{CompanyEvent, CompanyId, EpisodeReason, EventSeq, StoredEvent};

/// Raw journal entries read per underlying page.
const RAW_CHUNK: usize = 256;
/// Raw journal entries one lookup walks before giving up. Bounds the
/// pathological case — a desk busy long ago behind tens of thousands of
/// unrelated entries — at "a shorter answer", never a wrong one.
const RAW_SCAN: usize = 16_384;

/// Whether an episode is still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EpisodeStatus {
    /// Seats are still assigned.
    Open,
    /// Every seat completed, or the host closed it.
    Completed,
}

/// One episode as folded from the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EpisodeSummary {
    /// The episode id.
    pub id: String,
    /// The desk it ran on.
    pub chat_id: String,
    /// The message that opened it.
    pub opened_by_seq: u64,
    /// The thread root inside the desk, when in one.
    pub parent: Option<EventSeq>,
    /// The seats assigned at opening.
    pub participants: Vec<String>,
    /// How the opening message was routed.
    pub plan: RoutingPlanDto,
    /// The driver's revision — how many utterances have committed.
    pub revision: u64,
    /// Open or completed.
    pub status: EpisodeStatus,
    /// When it opened.
    pub opened_at_millis: u64,
    /// When it closed.
    pub completed_at_millis: Option<u64>,
    /// Who closed it.
    pub completed_by: Option<String>,
    /// Why it closed.
    pub reason: Option<EpisodeReason>,
    /// The referral hop it runs at.
    pub hop: u32,
    /// The seats parked on the operator right now, in the order they parked.
    pub waiting: Vec<String>,
}

/// `GET {scope}/episodes` row. Mirrors `EpisodeDto` in
/// `frontend/src/api/types.ts`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeDto {
    /// The episode id.
    pub id: String,
    /// The desk.
    pub chat_id: String,
    /// The message that opened it.
    pub opened_by_seq: u64,
    /// The thread root, as a console message id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// The seats assigned at opening.
    pub participants: Vec<String>,
    /// The opening plan.
    pub plan: RoutingPlanDto,
    /// The driver's revision.
    pub revision: u64,
    /// Open or completed.
    pub status: EpisodeStatus,
    /// When it opened.
    pub opened_at_millis: u64,
    /// When it closed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at_millis: Option<u64>,
    /// Who closed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_by: Option<String>,
    /// Why it closed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<EpisodeReason>,
    /// The seats parked on the operator right now.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub waiting: Vec<String>,
}

impl From<EpisodeSummary> for EpisodeDto {
    fn from(summary: EpisodeSummary) -> Self {
        Self {
            id: summary.id,
            chat_id: summary.chat_id,
            opened_by_seq: summary.opened_by_seq,
            parent_id: summary.parent.map(|seq| seq.value().to_string()),
            participants: summary.participants,
            plan: summary.plan,
            revision: summary.revision,
            status: summary.status,
            opened_at_millis: summary.opened_at_millis,
            completed_at_millis: summary.completed_at_millis,
            completed_by: summary.completed_by,
            reason: summary.reason,
            waiting: summary.waiting,
        }
    }
}

/// Folds the episode record out of journal entries given **oldest first**.
/// Returns episodes **newest first** (by opening sequence).
#[must_use]
pub fn fold_episodes(events: &[StoredEvent]) -> Vec<EpisodeSummary> {
    let mut episodes: Vec<EpisodeSummary> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for stored in events {
        match &stored.event {
            CompanyEvent::EpisodeOpened {
                chat_id,
                episode_id,
                opened_by_seq,
                parent,
                participants,
                plan,
                hop,
            } => {
                if index.contains_key(episode_id) {
                    continue;
                }
                index.insert(episode_id.clone(), episodes.len());
                episodes.push(EpisodeSummary {
                    id: episode_id.clone(),
                    chat_id: chat_id.clone(),
                    opened_by_seq: *opened_by_seq,
                    parent: *parent,
                    participants: participants.clone(),
                    plan: plan.clone(),
                    revision: 0,
                    status: EpisodeStatus::Open,
                    opened_at_millis: stored.at_millis,
                    completed_at_millis: None,
                    completed_by: None,
                    reason: None,
                    hop: *hop,
                    waiting: Vec::new(),
                });
            }
            // An episode in flight reports the wave its turns are running
            // in. The conductor announces no round -- a wave is whoever is
            // due, decided as it goes -- so the turn rows are what say how
            // far an open episode has got, and an operator watching one
            // would otherwise see it sit at round zero until it closed.
            CompanyEvent::TurnStarted {
                episode_id: Some(episode_id),
                round_revision: Some(revision),
                ..
            } => {
                if let Some(summary) = index.get(episode_id).map(|at| &mut episodes[*at]) {
                    summary.revision = summary.revision.max(*revision);
                }
            }
            // Legacy: the hand-written round loop committed a round at a
            // time. An old journal still folds to the same revision.
            CompanyEvent::RoundCommitted {
                episode_id,
                revision,
                utterances,
                ..
            } => {
                if let Some(summary) = index.get(episode_id).map(|at| &mut episodes[*at]) {
                    summary.revision = summary.revision.max(revision + utterances.len() as u64);
                }
            }
            CompanyEvent::EpisodeCompleted {
                episode_id,
                revision,
                completed_by,
                reason,
                ..
            } => {
                if let Some(summary) = index.get(episode_id).map(|at| &mut episodes[*at]) {
                    summary.revision = summary.revision.max(*revision);
                    summary.status = EpisodeStatus::Completed;
                    summary.completed_at_millis = Some(stored.at_millis);
                    summary.completed_by = completed_by.clone();
                    summary.reason = Some(*reason);
                    summary.waiting.clear();
                }
            }
            CompanyEvent::EpisodeSeatParked {
                episode_id, seat, ..
            } => {
                if let Some(summary) = index.get(episode_id).map(|at| &mut episodes[*at])
                    && summary.status == EpisodeStatus::Open
                    && !summary.waiting.contains(seat)
                {
                    summary.waiting.push(seat.clone());
                }
            }
            CompanyEvent::EpisodeSeatResumed {
                episode_id, seat, ..
            } => {
                if let Some(summary) = index.get(episode_id).map(|at| &mut episodes[*at]) {
                    summary.waiting.retain(|waiting| waiting != seat);
                }
            }
            _ => {}
        }
    }
    episodes.reverse();
    episodes
}

/// Reads the journal backwards until `stop` says enough or the scan bound is
/// hit, returning the entries **oldest first**.
async fn tail(
    events: &dyn EventLog,
    company: &CompanyId,
    mut stop: impl FnMut(&StoredEvent) -> bool,
) -> Result<Vec<StoredEvent>> {
    let mut cursor: Option<EventSeq> = None;
    let mut collected: Vec<StoredEvent> = Vec::new();
    let mut scanned = 0_usize;
    'pages: while scanned < RAW_SCAN {
        let chunk = RAW_CHUNK.min(RAW_SCAN - scanned);
        let raw = events.read_before(company, cursor, chunk).await?;
        if raw.is_empty() {
            break;
        }
        scanned += raw.len();
        let tail_page = raw.len() < chunk;
        cursor = raw.last().map(|stored| stored.seq);
        for stored in raw {
            let done = stop(&stored);
            collected.push(stored);
            if done {
                break 'pages;
            }
        }
        if tail_page {
            break;
        }
    }
    collected.reverse();
    Ok(collected)
}

/// The episodes a company ran, newest first, filtered and bounded as
/// `GET {scope}/episodes?desk&status&limit` asks.
pub async fn list_episodes(
    events: &dyn EventLog,
    company: &CompanyId,
    desk: Option<&str>,
    status: Option<EpisodeStatus>,
    limit: usize,
) -> Result<Vec<EpisodeDto>> {
    let entries = tail(events, company, |_| false).await?;
    Ok(fold_episodes(&entries)
        .into_iter()
        .filter(|episode| desk.is_none_or(|desk| episode.chat_id.eq_ignore_ascii_case(desk)))
        .filter(|episode| status.is_none_or(|status| episode.status == status))
        .take(limit)
        .map(EpisodeDto::from)
        .collect())
}

/// Where an answering episode sends its answer home — `hive::referral`'s
/// `ReturnAddress`, kept on the checkpoint so a resumed episode still knows.
pub type ReturnAddress = crate::hive::referral::ReturnAddress;

/// The driver's resumable state as the journal carries it.
#[derive(Clone, Debug, PartialEq)]
pub struct PersistedEpisode {
    /// The episode.
    pub episode_id: String,
    /// The desk.
    pub desk: String,
    /// The thread root the episode runs in.
    pub thread_root: Option<EventSeq>,
    /// The driver revision.
    pub revision: u64,
    /// The conductor's resumable snapshot
    /// (`tinyhivemind_driver::ConductorState`), as serde wrote it: the
    /// episode and every conversation open under it, each seat's watermark,
    /// the ledger of outstanding asks, who is parked, and the wave in
    /// progress. What is deliberately **not** here is the driver, the
    /// routing and the policy -- the host supplies those again on resume,
    /// because a router is a live object and a policy the operator changed
    /// between restarts should be the new one.
    pub state: serde_json::Value,
    /// Per-seat transcript delivery progress, from before the conductor
    /// kept its own watermarks. A conducted episode writes none: `state`
    /// carries them, and two records of the same thing would disagree.
    pub sharing: BTreeMap<String, SharingState>,
    /// The referral hop.
    pub hop: u32,
    /// Where a referral answer goes home to.
    pub origin: Option<ReturnAddress>,
}

impl PersistedEpisode {
    fn from_event(event: &CompanyEvent) -> Option<Self> {
        let CompanyEvent::EpisodeStateSaved {
            episode_id,
            desk,
            thread_root,
            revision,
            state,
            sharing,
            hop,
            origin,
        } = event
        else {
            return None;
        };
        let sharing = sharing
            .iter()
            .filter_map(|(agent, value)| {
                serde_json::from_value::<SharingState>(value.clone())
                    .ok()
                    .map(|state| (agent.clone(), state))
            })
            .collect();
        Some(Self {
            episode_id: episode_id.clone(),
            desk: desk.clone(),
            thread_root: *thread_root,
            revision: *revision,
            state: state.clone(),
            sharing,
            hop: *hop,
            origin: origin
                .as_ref()
                .and_then(|value| serde_json::from_value(value.clone()).ok()),
        })
    }

    /// This checkpoint as the row the journal stores.
    pub(crate) fn to_event(&self) -> CompanyEvent {
        CompanyEvent::EpisodeStateSaved {
            episode_id: self.episode_id.clone(),
            desk: self.desk.clone(),
            thread_root: self.thread_root,
            revision: self.revision,
            state: self.state.clone(),
            sharing: self
                .sharing
                .iter()
                .filter_map(|(agent, state)| {
                    serde_json::to_value(state)
                        .ok()
                        .map(|value| (agent.clone(), value))
                })
                .collect(),
            hop: self.hop,
            origin: self
                .origin
                .as_ref()
                .and_then(|origin| serde_json::to_value(origin).ok()),
        }
    }
}

/// The latest checkpoint of one episode, if any was written.
pub async fn latest_state(
    events: &dyn EventLog,
    company: &CompanyId,
    episode_id: &str,
) -> Result<Option<PersistedEpisode>> {
    let entries = tail(events, company, |stored| {
        matches!(
            &stored.event,
            CompanyEvent::EpisodeStateSaved { episode_id: id, .. } if id == episode_id
        )
    })
    .await?;
    Ok(entries
        .iter()
        .rev()
        .filter_map(|stored| PersistedEpisode::from_event(&stored.event))
        .find(|persisted| persisted.episode_id == episode_id))
}

/// Every row since `episode_id` opened, oldest first: what a resumed
/// episode's host reads its open conversations and parked seats back from.
pub async fn episode_rows(
    events: &dyn EventLog,
    company: &CompanyId,
    episode_id: &str,
) -> Result<Vec<StoredEvent>> {
    tail(events, company, |stored| {
        matches!(
            &stored.event,
            CompanyEvent::EpisodeOpened { episode_id: id, .. } if id == episode_id
        )
    })
    .await
}

#[cfg(test)]
#[path = "episode_store_tests.rs"]
mod tests;
