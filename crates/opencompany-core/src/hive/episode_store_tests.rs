//! Tests for the journal-backed episode store.

use std::collections::BTreeMap;

use super::*;
use crate::hive::routing::RoutingPlanDto;
use crate::hive::test_support::{MemoryLog, operator_message};
use crate::ports::events::EventLog;
use crate::ports::types::{RoundUtteranceRecord, UtteranceKind};

fn opened(desk: &str, id: &str, by: u64, parent: Option<u64>) -> CompanyEvent {
    CompanyEvent::EpisodeOpened {
        chat_id: desk.into(),
        episode_id: id.into(),
        opened_by_seq: by,
        parent: parent.map(EventSeq::new),
        participants: vec!["engineer".into(), "ceo".into()],
        plan: RoutingPlanDto::Hive {
            primary_id: "engineer".into(),
            invited_ids: vec!["ceo".into()],
        },
        hop: 0,
    }
}

fn committed(desk: &str, id: &str, revision: u64, seqs: &[u64]) -> CompanyEvent {
    CompanyEvent::RoundCommitted {
        chat_id: desk.into(),
        episode_id: id.into(),
        revision,
        utterances: seqs
            .iter()
            .map(|seq| RoundUtteranceRecord {
                agent_id: "engineer".into(),
                sequence: *seq,
                kind: UtteranceKind::Post,
                message_seq: Some(*seq),
                to: Vec::new(),
            })
            .collect(),
        actions: Vec::new(),
    }
}

fn completed(desk: &str, id: &str, revision: u64) -> CompanyEvent {
    CompanyEvent::EpisodeCompleted {
        chat_id: desk.into(),
        episode_id: id.into(),
        revision,
        completed_by: Some("ceo".into()),
        rounds: 2,
        reason: EpisodeReason::CompleteEpisode,
        summary_seq: Some(9),
    }
}

#[tokio::test]
async fn episodes_fold_newest_first_with_their_status_and_revision() {
    let log = MemoryLog::default();
    let company = MemoryLog::company();
    log.append(&company, operator_message("engineering", "Plan it.", None))
        .await
        .unwrap();
    log.append(&company, opened("engineering", "ep-1", 1, None))
        .await
        .unwrap();
    log.append(&company, committed("engineering", "ep-1", 0, &[4, 5]))
        .await
        .unwrap();
    log.append(&company, completed("engineering", "ep-1", 2))
        .await
        .unwrap();
    log.append(&company, opened("content", "ep-2", 6, Some(3)))
        .await
        .unwrap();
    log.append(&company, committed("content", "ep-2", 0, &[8]))
        .await
        .unwrap();

    let all = list_episodes(&log, &company, None, None, 10).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].id, "ep-2");
    assert_eq!(all[0].status, EpisodeStatus::Open);
    assert_eq!(all[0].revision, 1);
    assert_eq!(all[0].parent_id.as_deref(), Some("3"));
    assert!(all[0].completed_at_millis.is_none());
    assert_eq!(all[1].id, "ep-1");
    assert_eq!(all[1].status, EpisodeStatus::Completed);
    assert_eq!(all[1].revision, 2);
    assert_eq!(all[1].completed_by.as_deref(), Some("ceo"));
    assert_eq!(all[1].reason, Some(EpisodeReason::CompleteEpisode));
    assert_eq!(all[1].opened_at_millis, 2000);
    assert_eq!(all[1].completed_at_millis, Some(4000));

    let open = list_episodes(&log, &company, None, Some(EpisodeStatus::Open), 10)
        .await
        .unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, "ep-2");
    let engineering = list_episodes(&log, &company, Some("Engineering"), None, 10)
        .await
        .unwrap();
    assert_eq!(engineering.len(), 1);
    assert_eq!(engineering[0].chat_id, "engineering");
    assert_eq!(
        list_episodes(&log, &company, None, None, 1)
            .await
            .unwrap()
            .len(),
        1
    );

    let value = serde_json::to_value(&all[0]).unwrap();
    assert_eq!(value["chatId"], "content");
    assert_eq!(value["openedBySeq"], 6);
    assert_eq!(value["parentId"], "3");
    assert_eq!(value["plan"]["kind"], "hive");
    assert_eq!(value["status"], "open");
    assert!(value.get("completedAtMillis").is_none());
    let value = serde_json::to_value(&all[1]).unwrap();
    assert_eq!(value["reason"], "complete_episode");
    assert_eq!(value["completedBy"], "ceo");
}

#[tokio::test]
async fn the_checkpoint_round_trips() {
    let log = MemoryLog::default();
    let company = MemoryLog::company();
    log.append(&company, opened("engineering", "ep-1", 1, None))
        .await
        .unwrap();
    let mut sharing = BTreeMap::new();
    sharing.insert(
        "engineer".to_string(),
        tinyhivemind::initialized_state(
            tinyhivemind::Conversation {
                desk_id: "engineering".into(),
                desk_name: "Engineering desk".into(),
                thread_root: None,
            },
            tinyhivemind::Sequence(1),
        ),
    );
    let persisted = PersistedEpisode {
        episode_id: "ep-1".into(),
        desk: "engineering".into(),
        thread_root: None,
        revision: 1,
        state: serde_json::json!({"revision": 1}),
        sharing,
        hop: 1,
        origin: Some(ReturnAddress {
            desk: "content".into(),
            thread_root: Some(EventSeq::new(4)),
            episode_id: "ep-0".into(),
            asker: "writer".into(),
            forward_seq: 3,
        }),
    };
    assert!(
        latest_state(&log, &company, "ep-1")
            .await
            .unwrap()
            .is_none()
    );
    log.append(&company, persisted.to_event()).await.unwrap();
    let older = PersistedEpisode {
        revision: 0,
        ..persisted.clone()
    };
    // A checkpoint for another episode does not shadow this one.
    log.append(
        &company,
        PersistedEpisode {
            episode_id: "ep-9".into(),
            ..older
        }
        .to_event(),
    )
    .await
    .unwrap();
    let read = latest_state(&log, &company, "ep-1")
        .await
        .unwrap()
        .expect("the checkpoint is found");
    assert_eq!(read, persisted);
}

fn seat_parked(id: &str, seat: &str) -> CompanyEvent {
    CompanyEvent::EpisodeSeatParked {
        chat_id: "engineering".into(),
        episode_id: id.into(),
        seat: seat.into(),
        thread: None,
        approval_ids: vec![crate::ports::types::ApprovalId::new("a-1")],
    }
}

#[tokio::test]
async fn a_parked_seat_is_listed_as_waiting_until_it_resumes() {
    let log = MemoryLog::default();
    let company = MemoryLog::company();
    log.append(&company, opened("engineering", "ep-1", 1, None))
        .await
        .unwrap();
    log.append(&company, seat_parked("ep-1", "engineer"))
        .await
        .unwrap();

    let waiting = list_episodes(&log, &company, None, None, 10).await.unwrap();
    assert_eq!(waiting[0].waiting, vec!["engineer".to_string()]);
    assert_eq!(
        serde_json::to_value(&waiting[0]).unwrap()["waiting"],
        serde_json::json!(["engineer"])
    );

    log.append(
        &company,
        CompanyEvent::EpisodeSeatResumed {
            chat_id: "engineering".into(),
            episode_id: "ep-1".into(),
            seat: "engineer".into(),
        },
    )
    .await
    .unwrap();
    let resumed = list_episodes(&log, &company, None, None, 10).await.unwrap();
    assert!(resumed[0].waiting.is_empty());
    assert!(
        serde_json::to_value(&resumed[0])
            .unwrap()
            .get("waiting")
            .is_none()
    );
}
