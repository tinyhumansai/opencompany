//! Issue #464: the board announces its own writes.
//!
//! A card can come into being on several paths — chat intake's deterministic
//! triage (`company::task_intent::triage_message`, which opens a card only for
//! the messages it classes as work), a delegation opening a card for a desk,
//! the publish drain, the console's `POST …/tasks` — and before this nothing on the company
//! feed said so. The durable task events all describe a card that *already*
//! exists ([`TaskDispatched`](CompanyEvent::TaskDispatched) fires on the
//! `in_progress` edge, [`DeskTaskCompleted`](CompanyEvent::DeskTaskCompleted) on
//! the settle), so a console watching the board had nothing to react to and no
//! way to learn a card had appeared.
//!
//! ## Why the store, and not the callers
//!
//! [`BoardAnnouncer`] is a decorator over the company's
//! [`TaskStore`](crate::ports::tasks::TaskStore), wrapped in once by
//! [`RuntimeBuilder`](crate::runtime::RuntimeBuilder). Every writer — REST,
//! cycle, delegation, the settle mover — already goes through that port, so the
//! announcement is emitted **once, where cards are actually written**, rather
//! than once per call site that happens to write one.
//!
//! That distinction is the fix, not an implementation detail. The alternative
//! (an emit at each creation site) is the same shape as the bug: it is correct
//! only for the paths somebody remembered, and the next path added silently
//! announces nothing. Here a new writer cannot forget, because it does not get
//! a say.
//!
//! ## What it is not
//!
//! * **Not a stimulus.** The event is appended after the write and never fed
//!   into a cycle. A card that started work merely by existing would re-enter
//!   this store and announce again.
//! * **Not the board's truth.** The store is. The frame says *something on the
//!   board changed*; a console reacts by re-reading the board it already knows
//!   how to read. That is why a lost frame degrades to "one stale render" and
//!   never to wrong state.
//! * **Not a failure path.** Record-keeping never fails the work it records: an
//!   event log that refuses is logged and swallowed, exactly as the other
//!   best-effort journalling sites do. A card that was written stays written.
//!
//! The same shape fits any other store whose surface is watched and whose
//! writers are plural — the workspace tree (issue #327) is the obvious next one.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::ports::events::EventLog;
use crate::ports::tasks::{TaskRecord, TaskStore};
use crate::ports::types::{CompanyEvent, CompanyId};

/// The wire word for a write that brought a card into existence.
pub const CHANGE_OPENED: &str = "opened";
/// The wire word for a write that changed a card already on the board.
pub const CHANGE_UPDATED: &str = "updated";
/// The wire word for a card that was deleted.
pub const CHANGE_REMOVED: &str = "removed";

/// A [`TaskStore`] that appends a
/// [`TaskCardChanged`](CompanyEvent::TaskCardChanged) to the company's event log
/// after any write that actually changed the board.
///
/// Reads pass straight through. See the module docs for why this lives at the
/// store rather than at the callers.
pub struct BoardAnnouncer {
    inner: Arc<dyn TaskStore>,
    events: Arc<dyn EventLog>,
}

impl BoardAnnouncer {
    /// Wraps `inner`, announcing onto `events`.
    pub fn new(inner: Arc<dyn TaskStore>, events: Arc<dyn EventLog>) -> Self {
        Self { inner, events }
    }

    /// Appends one announcement, best-effort.
    ///
    /// A refusing event log is warned about and swallowed: the board write it
    /// describes has already succeeded and must not be undone (or reported as
    /// failed) because the *record* of it could not be filed.
    async fn announce(
        &self,
        company: &CompanyId,
        task_id: &str,
        change: &str,
        column: Option<String>,
    ) {
        let event = CompanyEvent::TaskCardChanged {
            task_id: task_id.to_string(),
            change: change.to_string(),
            column,
        };
        if let Err(err) = self.events.append(company, event).await {
            tracing::warn!(
                company = %company,
                task = %task_id,
                change = %change,
                error = %err,
                "could not announce a board write"
            );
        }
    }

    /// The card with `id` as the board holds it now, or `None`.
    ///
    /// A read that fails yields `None` rather than propagating: the caller uses
    /// it only to choose the wire word, and refusing a write because the *prior*
    /// state could not be read would fail board writes for a diagnostic.
    async fn current(&self, company: &CompanyId, id: &str) -> Option<TaskRecord> {
        self.inner
            .list(company)
            .await
            .ok()?
            .into_iter()
            .find(|t| t.id == id)
    }
}

#[async_trait]
impl TaskStore for BoardAnnouncer {
    async fn list(&self, company: &CompanyId) -> Result<Vec<TaskRecord>> {
        self.inner.list(company).await
    }

    /// Writes through, then announces — `opened` when the card was not there
    /// before, `updated` when it was and the write changed it.
    ///
    /// **A re-save that changes nothing says nothing.** Idempotent writes are
    /// ordinary here (a settle that re-persists an unchanged card, a retry), and
    /// announcing one would put a frame on the feed for a board that did not
    /// move — noise a console cannot distinguish from a real change.
    async fn upsert(&self, company: &CompanyId, task: &TaskRecord) -> Result<()> {
        // Read before the write, so `previous` is genuinely the prior state.
        // Two concurrent writers can both read "absent" and both announce
        // `opened`; that costs a duplicate re-read on the console and is
        // deliberately not locked against, because the board write itself is
        // where ordering is owned.
        let previous = self.current(company, &task.id).await;
        self.inner.upsert(company, task).await?;
        if previous.as_ref() == Some(task) {
            return Ok(());
        }
        let change = if previous.is_none() {
            CHANGE_OPENED
        } else {
            CHANGE_UPDATED
        };
        self.announce(company, &task.id, change, Some(task.column.clone()))
            .await;
        Ok(())
    }

    /// Deletes through, and announces only when a card was actually removed —
    /// a delete of an id the board never held changed nothing.
    async fn delete(&self, company: &CompanyId, id: &str) -> Result<bool> {
        let removed = self.inner.delete(company, id).await?;
        if removed {
            self.announce(company, id, CHANGE_REMOVED, None).await;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ports::types::{EventSeq, StoredEvent};
    use futures::stream::BoxStream;
    use std::sync::Mutex;

    /// An in-memory board, so the decorator is exercised against a real
    /// `TaskStore` rather than a stub that cannot tell insert from update.
    #[derive(Default)]
    struct MemTasks {
        rows: Mutex<Vec<TaskRecord>>,
    }

    #[async_trait]
    impl TaskStore for MemTasks {
        async fn list(&self, _company: &CompanyId) -> Result<Vec<TaskRecord>> {
            Ok(self.rows.lock().unwrap().clone())
        }
        async fn upsert(&self, _company: &CompanyId, task: &TaskRecord) -> Result<()> {
            let mut rows = self.rows.lock().unwrap();
            match rows.iter_mut().find(|t| t.id == task.id) {
                Some(slot) => *slot = task.clone(),
                None => rows.push(task.clone()),
            }
            Ok(())
        }
        async fn delete(&self, _company: &CompanyId, id: &str) -> Result<bool> {
            let mut rows = self.rows.lock().unwrap();
            let before = rows.len();
            rows.retain(|t| t.id != id);
            Ok(rows.len() != before)
        }
    }

    #[derive(Default)]
    struct MemLog {
        appended: Mutex<Vec<CompanyEvent>>,
    }

    #[async_trait]
    impl EventLog for MemLog {
        async fn append(&self, _id: &CompanyId, event: CompanyEvent) -> Result<EventSeq> {
            let mut got = self.appended.lock().unwrap();
            got.push(event);
            Ok(EventSeq::new(got.len() as u64))
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> Result<Vec<StoredEvent>> {
            Ok(Vec::new())
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(futures::stream::empty())
        }
    }

    /// An event log that refuses every append, so the "record-keeping never
    /// fails the work it records" contract is tested rather than asserted.
    struct BrokenLog;

    #[async_trait]
    impl EventLog for BrokenLog {
        async fn append(&self, _id: &CompanyId, _event: CompanyEvent) -> Result<EventSeq> {
            Err(crate::error::OpenCompanyError::Store(
                "log is down".to_string(),
            ))
        }
        async fn read_from(
            &self,
            _id: &CompanyId,
            _seq: EventSeq,
            _limit: usize,
        ) -> Result<Vec<StoredEvent>> {
            Ok(Vec::new())
        }
        fn subscribe(
            &self,
            _id: &CompanyId,
        ) -> BoxStream<'static, crate::ports::events::EventStreamItem> {
            Box::pin(futures::stream::empty())
        }
    }

    fn card(id: &str, column: &str) -> TaskRecord {
        TaskRecord {
            id: id.to_string(),
            title: "Draft the launch note".to_string(),
            note: None,
            column: column.to_string(),
            priority: "medium".to_string(),
            assignee: String::new(),
            updated_at_millis: 1,
            origin_chat_id: None,
            parent_task_id: None,
            output: None,
            plan: None,
            planning_attempts: Vec::new(),
            deliverable: crate::ports::tasks::TaskDeliverable::Once,
            workflow_proposal: None,
            origin_run_id: None,
            origin_workflow_id: None,
            bounced: None,
        }
    }

    fn wired() -> (Arc<BoardAnnouncer>, Arc<MemLog>, CompanyId) {
        let log = Arc::new(MemLog::default());
        let store = Arc::new(BoardAnnouncer::new(
            Arc::new(MemTasks::default()),
            log.clone(),
        ));
        (store, log, CompanyId::new("announcer-co"))
    }

    /// The headline of #464: a card written by *anything* announces itself, and
    /// the first write of an id is an `opened` rather than an `updated`.
    #[tokio::test]
    async fn a_new_card_announces_that_it_opened() {
        let (store, log, co) = wired();
        store.upsert(&co, &card("t-1", "todo")).await.unwrap();

        let got = log.appended.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "one announcement for one new card: {got:?}");
        match &got[0] {
            CompanyEvent::TaskCardChanged {
                task_id,
                change,
                column,
            } => {
                assert_eq!(task_id, "t-1");
                assert_eq!(change, CHANGE_OPENED);
                assert_eq!(column.as_deref(), Some("todo"));
            }
            other => panic!("expected a board announcement, got {other:?}"),
        }
    }

    /// A second write of the same id is an update, and carries the column it
    /// landed in — the frame a second console needs to follow a drag.
    #[tokio::test]
    async fn moving_a_card_announces_an_update_with_its_new_column() {
        let (store, log, co) = wired();
        store.upsert(&co, &card("t-1", "todo")).await.unwrap();
        store.upsert(&co, &card("t-1", "in_review")).await.unwrap();

        let got = log.appended.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "open then move: {got:?}");
        match &got[1] {
            CompanyEvent::TaskCardChanged { change, column, .. } => {
                assert_eq!(change, CHANGE_UPDATED);
                assert_eq!(column.as_deref(), Some("in_review"));
            }
            other => panic!("expected a board announcement, got {other:?}"),
        }
    }

    /// Re-saving a card exactly as it already stands announces nothing. A
    /// settle that re-persists an unchanged card is ordinary, and a frame for a
    /// board that did not move is noise a console cannot tell from a real
    /// change.
    #[tokio::test]
    async fn an_unchanged_re_save_is_silent() {
        let (store, log, co) = wired();
        store.upsert(&co, &card("t-1", "todo")).await.unwrap();
        store.upsert(&co, &card("t-1", "todo")).await.unwrap();

        assert_eq!(
            log.appended.lock().unwrap().len(),
            1,
            "the identical re-save must not announce"
        );
    }

    /// A removed card announces its removal without a column — a card that is
    /// gone is not in one, and an empty string would read as a column whose id
    /// is blank.
    #[tokio::test]
    async fn a_deleted_card_announces_removal_without_a_column() {
        let (store, log, co) = wired();
        store.upsert(&co, &card("t-1", "todo")).await.unwrap();
        assert!(store.delete(&co, "t-1").await.unwrap());

        let got = log.appended.lock().unwrap().clone();
        match got.last().expect("a removal announcement") {
            CompanyEvent::TaskCardChanged { change, column, .. } => {
                assert_eq!(change, CHANGE_REMOVED);
                assert!(column.is_none(), "a removed card is in no column");
            }
            other => panic!("expected a board announcement, got {other:?}"),
        }
    }

    /// Deleting an id the board never held changed nothing, so it announces
    /// nothing.
    #[tokio::test]
    async fn deleting_an_absent_card_is_silent() {
        let (store, log, co) = wired();
        assert!(!store.delete(&co, "never-existed").await.unwrap());
        assert!(log.appended.lock().unwrap().is_empty());
    }

    /// Record-keeping never fails the work it records: a refusing event log
    /// leaves the board write successful and the card on the board.
    #[tokio::test]
    async fn a_refusing_event_log_does_not_fail_the_board_write() {
        let store = BoardAnnouncer::new(Arc::new(MemTasks::default()), Arc::new(BrokenLog));
        let co = CompanyId::new("announcer-broken");

        store
            .upsert(&co, &card("t-1", "todo"))
            .await
            .expect("the board write succeeds even when the announcement cannot be filed");
        assert_eq!(store.list(&co).await.unwrap().len(), 1);
    }
}
