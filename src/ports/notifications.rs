//! The [`NotificationStore`] port: durable notifications with per-person read
//! state.
//!
//! The console has a live feed — `src/turn_stream.rs` publishes per-turn
//! progress onto a broadcast bus, the operator SSE route fans it out, and
//! `frontend/src/hooks/use-events.ts` turns it into toasts. That is a
//! *transport*: it only works while a browser tab is open and watching. Close
//! the tab and the event is gone (issue #577).
//!
//! This store holds the durable half: a notification is a stored thing with a
//! kind, a subject, a created-at, and read state. It delivers nothing on its
//! own — it is the substrate that out-of-browser delivery (#750) and the digest
//! (#751) are built on.
//!
//! **Read state is per person, not per company.** A company has a roster, and
//! "I have seen this" is per human. The record is company-wide; two operators
//! see the same notifications with independent read state. This is deliberately
//! *not* modelled as a `read` flag on the record — that is [`InboxStore`]'s
//! shape, whose `mark_read` is scoped per inbox key with no user in the
//! signature, so one admin opening it would mark it read for everyone (the trap
//! called out on #749). Read state here keys on `(company, user,
//! notification)`.
//!
//! [`InboxStore`]: crate::ports::inbox::InboxStore

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::ports::types::CompanyId;

/// What a notification is about. A closed set — the four subjects #577 names —
/// so a backend can store it as a small tag rather than an open string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubjectKind {
    Task,
    Run,
    Approval,
    Workflow,
}

impl SubjectKind {
    /// The wire/storage token for this kind. Stable — backends persist it, so a
    /// rename here is a data migration, not a cosmetic change.
    pub fn as_str(&self) -> &'static str {
        match self {
            SubjectKind::Task => "task",
            SubjectKind::Run => "run",
            SubjectKind::Approval => "approval",
            SubjectKind::Workflow => "workflow",
        }
    }

    /// Parses the storage token back, rejecting anything unknown so a corrupt or
    /// forward-versioned row surfaces rather than silently becoming a default.
    ///
    /// Named `from_token` rather than `from_str` so it is not mistaken for
    /// [`std::str::FromStr`] (which would have to return `Result`, not
    /// `Option`).
    pub fn from_token(s: &str) -> Option<Self> {
        match s {
            "task" => Some(SubjectKind::Task),
            "run" => Some(SubjectKind::Run),
            "approval" => Some(SubjectKind::Approval),
            "workflow" => Some(SubjectKind::Workflow),
            _ => None,
        }
    }
}

/// The thing a notification points at: which kind of subject, and its id in that
/// subject's own id space (a task id, a run id, an approval id, a workflow id).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subject {
    pub kind: SubjectKind,
    pub id: String,
}

/// One stored notification, company-scoped. Read state is deliberately absent
/// here — it is per person, projected onto the record by
/// [`NotificationStore::list`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notification {
    /// Stable id within the company, minted with
    /// [`generate_id`](crate::ports::generate_id) by the caller — the
    /// [`EmailRecord`](crate::ports::inbox::EmailRecord) convention.
    pub id: String,
    /// The notification type: a free-form tag. A intentionally does **not**
    /// define the vocabulary of what is "worth sending" — that judgement is the
    /// one EPIC #558 exists to make, consumed by #750, and must not be made
    /// twice (see #577 point 5).
    pub kind: String,
    /// What it is about.
    pub subject: Subject,
    /// Epoch-millis the notification was raised
    /// ([`now_millis`](crate::ports::now_millis)).
    pub created_at: u64,
    /// One-line, operator-readable summary — the line a person reads.
    pub title: String,
}

/// A notification as one person sees it: the record plus whether *they* have
/// read it. Two people list the same records with independent `read_at`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationView {
    #[serde(flatten)]
    pub notification: Notification,
    /// Epoch-millis this person first read it; `None` means unread for them.
    pub read_at: Option<u64>,
}

/// Durable per-company notifications with per-person read state. Company A's
/// notifications MUST be invisible to company B, and one person's read state
/// MUST be invisible to another.
#[async_trait]
pub trait NotificationStore: Send + Sync {
    /// Records a notification for the whole company.
    async fn append(&self, company: &CompanyId, notification: &Notification) -> Result<()>;

    /// Every notification in the company, each carrying **this** person's read
    /// state.
    ///
    /// **Newest first** (by `created_at`, descending; ties broken by `id`
    /// descending for a stable order). Part of the contract rather than an
    /// accident of each backend: insertion order differs between a document
    /// store and a table, and a caller paging a feed would see rows jump. The
    /// conformance suite asserts it, so a backend returning insertion order
    /// fails rather than passing quietly.
    async fn list(&self, company: &CompanyId, user: &str) -> Result<Vec<NotificationView>>;

    /// Marks notifications read for this person — the given `ids`, or every
    /// notification in the company when `ids` is `None`.
    ///
    /// **A latch.** Once read it stays read; re-marking an already-read
    /// notification is a no-op and preserves the original `read_at`. Ids that
    /// name no notification are ignored. Returns the count still unread for this
    /// person after the mark.
    async fn mark_read(
        &self,
        company: &CompanyId,
        user: &str,
        ids: Option<&[String]>,
    ) -> Result<u64>;

    /// Every notification in the company that has **not** yet been included in a
    /// digest delivery (issue #751), **oldest first** (by `created_at`
    /// ascending; ties broken by `id` ascending), so a caller can read the
    /// window's edges off the ends of the list.
    ///
    /// Delivery is per **company**, not per person: a notification is emailed
    /// once, to the owner set, so this axis carries no user and is independent of
    /// [`mark_read`](Self::mark_read)'s per-person read state. The ordering is
    /// part of the contract and the conformance suite asserts it.
    async fn undelivered(&self, company: &CompanyId) -> Result<Vec<Notification>>;

    /// Marks notifications **delivered** — recorded as included in a digest, so a
    /// later flush does not re-send them (issue #751).
    ///
    /// **A latch**, like [`mark_read`](Self::mark_read): once delivered it stays
    /// delivered, re-marking is a no-op, and ids that name no notification are
    /// ignored.
    async fn mark_delivered(&self, company: &CompanyId, ids: &[String]) -> Result<()>;
}
