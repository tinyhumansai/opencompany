//! The per-teammate email read: `Company.inboxes` over the [`InboxStore`] port.

use std::sync::Arc;

use async_graphql::{Context, ID, Object, SimpleObject};

use super::pagination::Page;
use crate::company::runtime::CompanyRuntime;
use crate::ports::inbox::{EmailRecord, InboxMeta};

/// One teammate inbox handle: metadata plus a paginated message resolver.
pub struct InboxGql {
    runtime: Arc<CompanyRuntime>,
    meta: InboxMeta,
    unread: i32,
}

impl InboxGql {
    fn new(runtime: Arc<CompanyRuntime>, meta: InboxMeta, unread: i32) -> Self {
        Self {
            runtime,
            meta,
            unread,
        }
    }
}

#[Object(name = "Inbox")]
impl InboxGql {
    /// The teammate local part / slug.
    async fn key(&self) -> ID {
        ID(self.meta.key.clone())
    }

    /// The inbox display name.
    async fn name(&self) -> String {
        self.meta.name.clone()
    }

    /// The full address (`{key}@{domain}` when a domain is configured).
    async fn address(&self) -> String {
        self.meta.address.clone()
    }

    /// The number of unread received messages.
    async fn unread(&self) -> i32 {
        self.unread
    }

    /// A page of this inbox's messages, oldest first.
    async fn messages(
        &self,
        #[graphql(default = 50)] first: i32,
        #[graphql(default = 0)] offset: i32,
    ) -> async_graphql::Result<Page<EmailMessageGql>> {
        let all = self
            .runtime
            .inbox()
            .messages(self.runtime.id(), &self.meta.key, usize::MAX, 0)
            .await?;
        let total = all.len() as i32;
        let items = all
            .into_iter()
            .skip(offset.max(0) as usize)
            .take(first.max(0) as usize)
            .map(EmailMessageGql::from)
            .collect();
        Ok(Page { items, total })
    }
}

/// One email in a teammate inbox. Mirrors [`EmailRecord`].
#[derive(SimpleObject)]
#[graphql(name = "EmailMessage")]
pub struct EmailMessageGql {
    /// The message id.
    pub id: ID,
    /// The sender's display name (may be empty).
    pub from_name: String,
    /// The sender's email address.
    pub from_email: String,
    /// The subject line.
    pub subject: String,
    /// The plain-text body.
    pub body: String,
    /// When it arrived / was sent, epoch millis.
    pub at_millis: f64,
    /// Whether the message has been read.
    pub read: bool,
    /// True for a sent message, false for a received one.
    pub outbound: bool,
}

impl From<EmailRecord> for EmailMessageGql {
    fn from(record: EmailRecord) -> Self {
        Self {
            id: ID(record.id),
            from_name: record.from_name,
            from_email: record.from_email,
            subject: record.subject,
            body: record.body,
            at_millis: record.at_millis as f64,
            read: record.read,
            outbound: record.outbound,
        }
    }
}

/// Resolves `Company.inboxes`, computing each inbox's unread count.
pub(crate) async fn resolve(
    ctx: &Context<'_>,
    runtime: &Arc<CompanyRuntime>,
) -> async_graphql::Result<Vec<InboxGql>> {
    let _ = ctx;
    let metas = runtime.inbox().inboxes(runtime.id()).await?;
    let mut out = Vec::with_capacity(metas.len());
    for meta in metas {
        let messages = runtime
            .inbox()
            .messages(runtime.id(), &meta.key, usize::MAX, 0)
            .await?;
        let unread = messages.iter().filter(|m| !m.outbound && !m.read).count() as i32;
        out.push(InboxGql::new(runtime.clone(), meta, unread));
    }
    Ok(out)
}
