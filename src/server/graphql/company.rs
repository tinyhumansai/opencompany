//! The `Company` aggregation root and its directly-owned leaf objects.
//!
//! [`CompanyGql`] is a **handle**, not an eager projection: it carries the
//! company id and its [`CompanyRuntime`], and every field is an async resolver
//! that awaits the relevant port or parser only when selected. Nested fields
//! are safe without re-checking auth because the handle is only ever reachable
//! through an authorized `companies` / `company` query.

use std::sync::Arc;

use async_graphql::{Context, ID, Object, SimpleObject};

use super::connections::{ConnectionStateGql, DomainStatusGql, SmtpStatusGql};
use super::finances::FinancesGql;
use super::inbox::InboxGql;
use super::memory_facts::{MemoryFactGql, MemoryKindGql};
use super::pagination::Page;
use super::skills::SkillGql;
use super::tasks::TaskGql;
use super::usage::{UsageGql, UsageRangeGql};
use super::workflows::{WorkflowGql, WorkflowSummaryGql};
use super::workspace::{FsNodeGql, WorkspaceFileGql};
use super::{
    connections, finances, inbox, memory_facts, skills, tasks, usage, workflows, workspace,
};
use crate::company::runtime::CompanyRuntime;
use crate::ports::types::{CompanyEvent, CompanyId, EventSeq, StoredEvent};

/// The synthetic desk pre-threading operator messages are attributed to.
const GENERAL_DESK: &str = "General";

/// The aggregation-root handle over one company. See the module docs.
pub struct CompanyGql {
    id: CompanyId,
    runtime: Arc<CompanyRuntime>,
}

impl CompanyGql {
    /// Builds a handle over a resolved company runtime.
    pub fn new(id: CompanyId, runtime: Arc<CompanyRuntime>) -> Self {
        Self { id, runtime }
    }
}

#[Object(name = "Company")]
impl CompanyGql {
    /// The company id.
    async fn id(&self) -> ID {
        ID(self.id.as_ref().to_string())
    }

    /// The display name from the company charter.
    async fn name(&self) -> async_graphql::Result<String> {
        Ok(self.runtime.status().await?.name)
    }

    /// Lifecycle state, e.g. `running`, `paused`, `archived`.
    async fn lifecycle(&self) -> async_graphql::Result<String> {
        Ok(self.runtime.status().await?.lifecycle)
    }

    /// The number of approvals currently awaiting the operator.
    async fn pending_approvals(&self) -> i32 {
        self.runtime.pending_approvals().len() as i32
    }

    /// The approvals currently awaiting the operator for this company.
    async fn approvals(&self) -> Vec<ApprovalGql> {
        self.runtime
            .pending_approvals()
            .into_iter()
            .map(ApprovalGql::from)
            .collect()
    }

    /// The company roster: manifest teammates plus operator-added overlays.
    async fn team(&self) -> async_graphql::Result<Vec<TeamMemberGql>> {
        self.resolve_team().await
    }

    /// The company's desks (group chats).
    async fn chats(&self) -> async_graphql::Result<Vec<ChatGql>> {
        Ok(self
            .desks()
            .await?
            .into_iter()
            .map(|desk| ChatGql::new(self.runtime.clone(), desk))
            .collect())
    }

    /// One desk by id, or null when unknown.
    async fn chat(&self, id: ID) -> async_graphql::Result<Option<ChatGql>> {
        Ok(self
            .desks()
            .await?
            .into_iter()
            .find(|desk| desk.id == id.as_str())
            .map(|desk| ChatGql::new(self.runtime.clone(), desk)))
    }

    /// The per-teammate inboxes.
    async fn inboxes(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<InboxGql>> {
        inbox::resolve(ctx, &self.runtime).await
    }

    /// The task board, optionally filtered to one column.
    async fn tasks(
        &self,
        column: Option<String>,
        #[graphql(default = 100)] first: i32,
        #[graphql(default = 0)] offset: i32,
    ) -> async_graphql::Result<Page<TaskGql>> {
        tasks::resolve(&self.runtime, column, first, offset).await
    }

    /// The company's installed skills.
    async fn skills(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<SkillGql>> {
        skills::resolve_company(ctx, &self.runtime).await
    }

    /// The workspace file tree.
    async fn workspace_tree(&self) -> async_graphql::Result<Vec<FsNodeGql>> {
        workspace::resolve_tree(&self.runtime).await
    }

    /// One workspace file by id, with content and backlinks; null when absent.
    async fn workspace_file(&self, id: ID) -> async_graphql::Result<Option<WorkspaceFileGql>> {
        workspace::resolve_file(&self.runtime, id.as_str()).await
    }

    /// The company-brain memory facts.
    async fn memory(
        &self,
        query: Option<String>,
        kind: Option<MemoryKindGql>,
        #[graphql(default = 50)] first: i32,
        #[graphql(default = 0)] offset: i32,
    ) -> async_graphql::Result<Page<MemoryFactGql>> {
        memory_facts::resolve(&self.runtime, query, kind, first, offset).await
    }

    /// The enabled workflows, as one-line summaries.
    async fn workflows(&self, ctx: &Context<'_>) -> async_graphql::Result<Vec<WorkflowSummaryGql>> {
        workflows::resolve_summaries(ctx, &self.runtime).await
    }

    /// One workflow graph by id; null when unavailable.
    async fn workflow(
        &self,
        ctx: &Context<'_>,
        id: ID,
    ) -> async_graphql::Result<Option<WorkflowGql>> {
        workflows::resolve_one(ctx, &self.runtime, id.as_str()).await
    }

    /// Token/cost usage over a lookback window.
    async fn usage(
        &self,
        ctx: &Context<'_>,
        #[graphql(default)] range: UsageRangeGql,
    ) -> async_graphql::Result<UsageGql> {
        usage::resolve(ctx, &self.runtime, range).await
    }

    /// The finance surface: balance, budget vs spend, and the transaction journal.
    async fn finances(&self) -> async_graphql::Result<FinancesGql> {
        finances::resolve(&self.runtime).await
    }

    /// The third-party connections and their live status.
    async fn connections(&self) -> async_graphql::Result<Vec<ConnectionStateGql>> {
        connections::resolve_connections(&self.runtime).await
    }

    /// Custom-domain status; null when no domain is configured.
    async fn domain(&self) -> async_graphql::Result<Option<DomainStatusGql>> {
        connections::resolve_domain(&self.runtime).await
    }

    /// SMTP status — host/port/username only, never the password.
    async fn smtp(&self) -> async_graphql::Result<SmtpStatusGql> {
        connections::resolve_smtp(&self.runtime).await
    }
}

impl CompanyGql {
    /// Loads the roster from the manifest and overlays, tagging inbox state.
    async fn resolve_team(&self) -> async_graphql::Result<Vec<TeamMemberGql>> {
        let Some(record) = self.runtime.store().load(&self.id).await? else {
            return Ok(Vec::new());
        };
        let inbox_enabled: std::collections::HashMap<String, bool> = self
            .runtime
            .inbox()
            .inboxes(&self.id)
            .await?
            .into_iter()
            .map(|meta| (meta.key, meta.enabled))
            .collect();
        let enabled = |id: &str| inbox_enabled.get(id).copied().unwrap_or(false);

        let mut out: Vec<TeamMemberGql> = record
            .manifest
            .agents
            .iter()
            .map(|agent| TeamMemberGql {
                id: ID(agent.id.clone()),
                name: None,
                role: agent.role.clone(),
                description: agent.description.clone(),
                inbox_enabled: enabled(&agent.id),
            })
            .collect();
        out.extend(record.overlay_agents.iter().map(|agent| TeamMemberGql {
            id: ID(agent.id.clone()),
            name: Some(agent.name.clone()),
            role: agent.role.clone(),
            description: agent.description.clone(),
            inbox_enabled: enabled(&agent.id),
        }));
        Ok(out)
    }

    /// The company's desks from the manifest's group chats.
    async fn desks(&self) -> async_graphql::Result<Vec<Desk>> {
        let Some(record) = self.runtime.store().load(&self.id).await? else {
            return Ok(Vec::new());
        };
        Ok(record
            .manifest
            .group_chats
            .iter()
            .map(|chat| Desk {
                id: chat.id.clone(),
                name: chat.name.clone(),
                description: chat.description.clone(),
                members: chat.members.clone(),
            })
            .collect())
    }
}

/// A parked approval awaiting the operator. Mirrors
/// [`ApprovalSummary`](crate::runtime::types::ApprovalSummary).
#[derive(SimpleObject)]
#[graphql(name = "Approval")]
pub struct ApprovalGql {
    /// The approval's id.
    pub id: ID,
    /// The parked effect's dotted kind.
    pub kind: String,
    /// The USD amount involved, if any.
    pub amount_usd: Option<f64>,
    /// Epoch-millis the effect was parked. `Float` round-trips the full u64
    /// range that would overflow GraphQL's `Int`.
    pub at_millis: f64,
}

impl From<crate::runtime::types::ApprovalSummary> for ApprovalGql {
    fn from(summary: crate::runtime::types::ApprovalSummary) -> Self {
        Self {
            id: ID(summary.id.as_ref().to_string()),
            kind: summary.kind,
            amount_usd: summary.amount_usd,
            at_millis: summary.at_millis as f64,
        }
    }
}

/// One roster teammate. Mirrors `frontend/src/lib/team.ts`.
#[derive(SimpleObject)]
#[graphql(name = "TeamMember")]
pub struct TeamMemberGql {
    /// The teammate id.
    pub id: ID,
    /// The display name; null for a manifest teammate named only by role.
    pub name: Option<String>,
    /// The job title / role.
    pub role: String,
    /// An optional description.
    pub description: Option<String>,
    /// Whether this teammate has an enabled inbox.
    pub inbox_enabled: bool,
}

/// Internal desk projection shared between `chats` and `chat`.
#[derive(Clone)]
struct Desk {
    id: String,
    name: String,
    description: Option<String>,
    members: Vec<String>,
}

/// A desk (group chat): metadata plus an append-only message history resolver.
pub struct ChatGql {
    runtime: Arc<CompanyRuntime>,
    desk: Desk,
}

impl ChatGql {
    fn new(runtime: Arc<CompanyRuntime>, desk: Desk) -> Self {
        Self { runtime, desk }
    }

    /// Whether a stored event belongs to this desk. `AgentReply`s match on the
    /// desk id or name; pre-threading `OperatorMessage`s fall to the synthetic
    /// "General" desk.
    fn owns(&self, event: &CompanyEvent) -> bool {
        match event {
            CompanyEvent::AgentReply { chat_id, .. } => {
                chat_id == &self.desk.id || chat_id == &self.desk.name
            }
            CompanyEvent::OperatorMessage { .. } => {
                self.desk.id.eq_ignore_ascii_case(GENERAL_DESK)
                    || self.desk.name.eq_ignore_ascii_case(GENERAL_DESK)
            }
            _ => false,
        }
    }
}

#[Object(name = "Chat")]
impl ChatGql {
    /// The desk id.
    async fn id(&self) -> ID {
        ID(self.desk.id.clone())
    }

    /// The desk name.
    async fn name(&self) -> String {
        self.desk.name.clone()
    }

    /// An optional description.
    async fn description(&self) -> Option<String> {
        self.desk.description.clone()
    }

    /// The teammate ids on this desk.
    async fn members(&self) -> Vec<ID> {
        self.desk.members.iter().cloned().map(ID).collect()
    }

    /// The desk's message history, most-recent last. `before` is an opaque
    /// EventLog cursor (a stringified sequence position); only messages before
    /// it are returned.
    async fn history(
        &self,
        #[graphql(default = 50)] first: i32,
        before: Option<String>,
    ) -> async_graphql::Result<Page<MessageGql>> {
        let before_seq = before.as_deref().and_then(|c| c.parse::<u64>().ok());
        let stored = self
            .runtime
            .events()
            .read_from(self.runtime.id(), EventSeq::new(0), usize::MAX)
            .await?;

        let mut messages: Vec<MessageGql> = stored
            .into_iter()
            .filter(|event| self.owns(&event.event))
            .filter(|event| before_seq.is_none_or(|before| event.seq.value() < before))
            .map(MessageGql::from)
            .collect();

        let total = messages.len() as i32;
        // Keep the most recent `first`, still in chronological order.
        let first = first.max(0) as usize;
        if messages.len() > first {
            messages.drain(0..messages.len() - first);
        }
        Ok(Page {
            items: messages,
            total,
        })
    }
}

/// One message in a desk history. Mirrors `frontend/src/lib/chat.ts`.
#[derive(SimpleObject)]
#[graphql(name = "Message")]
pub struct MessageGql {
    /// The message id (its EventLog sequence position).
    pub id: ID,
    /// The channel the message came in on.
    pub channel: String,
    /// The author label.
    pub author: String,
    /// The message text.
    pub text: String,
    /// When it was journaled, epoch millis.
    pub at_millis: f64,
    /// Whether it is the operator's own message.
    pub mine: bool,
}

impl From<StoredEvent> for MessageGql {
    fn from(stored: StoredEvent) -> Self {
        let id = ID(stored.seq.value().to_string());
        let at_millis = stored.at_millis as f64;
        match stored.event {
            CompanyEvent::AgentReply { agent_id, text, .. } => MessageGql {
                id,
                channel: agent_id.clone(),
                author: agent_id,
                text,
                at_millis,
                mine: false,
            },
            CompanyEvent::OperatorMessage { text } => MessageGql {
                id,
                channel: "operator".to_string(),
                author: "operator".to_string(),
                text,
                at_millis,
                mine: true,
            },
            // `owns` never admits other variants into a history.
            other => MessageGql {
                id,
                channel: "system".to_string(),
                author: "system".to_string(),
                text: format!("{other:?}"),
                at_millis,
                mine: false,
            },
        }
    }
}
