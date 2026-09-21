use super::*;
fn handoff(name: &str) -> Delegation {
    Delegation::DelegateToTeammate {
        teammate: name.into(),
        instruction: "Synthetic review".into(),
    }
}
#[test]
fn dispatched_card_refuses_second_handoff_but_chat_collects_both() {
    let queue = DelegationQueue::default();
    {
        let _claim = queue.claim_task();
        assert_eq!(
            queue.push_within_cap(handoff("maker"), 3, 3),
            Staged::Queued
        );
        assert_eq!(
            queue.push_within_cap(handoff("reviewer"), 3, 3),
            Staged::NoDrain(NoDrainReason::TaskHandoffAlreadyQueued)
        );
        let drained = queue.drain(3);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0], handoff("maker"));
    }
    let _claim = queue.claim();
    assert_eq!(
        queue.push_within_cap(handoff("maker"), 3, 3),
        Staged::Queued
    );
    assert_eq!(
        queue.push_within_cap(handoff("reviewer"), 3, 3),
        Staged::Queued
    );
    assert_eq!(queue.drain(3).len(), 2);
}
#[test]
fn task_handoff_keeps_unrelated_board_writes_and_redirect_reset() {
    let queue = DelegationQueue::default();
    let _claim = queue.claim_task();
    assert_eq!(
        queue.push_within_cap(handoff("maker"), 3, 3),
        Staged::Queued
    );
    assert_eq!(
        queue.push_within_cap(
            Delegation::SpawnTask {
                title: "Later work".into(),
                note: None,
                assignee: None
            },
            3,
            3
        ),
        Staged::Queued
    );
    queue.clear();
    assert_eq!(
        queue.push_within_cap(handoff("reviewer"), 3, 3),
        Staged::Queued
    );
}
